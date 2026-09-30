//! The image store: blobs by digest, exactly as registries serve them, and each image's
//! root filesystem as EROFS, by ChainID (docs/design/architecture.md D18).
//!
//! Blobs keep the registry's bytes, as containerd's content store and the OCI image layout
//! keep them, so a later push or save can reproduce their digests. Nothing enters the
//! store unverified: a blob is committed only when its size and digest match its
//! descriptor, and a layer only counts once its decompressed bytes match its DiffID
//! (docs/research/registry-pull.md §5, rows 4 and 6).

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256, Sha384, Sha512};

use crate::erofs;
use crate::layer::{self, Archives};
use crate::oci::{self, Descriptor};
use crate::reference::{Algorithm, Digest};
use crate::{Error, bad};

/// Bumped whenever the EROFS writer's output changes, so older root filesystems are rebuilt.
const ROOTFS_VERSION: u32 = 1;
/// Bumped whenever a reference's record changes shape: records of an older shape are not
/// read, and the images they name are pulled again.
const REFS_VERSION: u32 = 1;
const CHUNK: usize = 1 << 20;
/// The largest zstd window decoded: klauspost/compress v1.20.0's `MaxWindowSize`, in the
/// decoder containerd uses.
const ZSTD_MAX_WINDOW: u64 = 1 << 29;

/// A layer: its blob, the blob's media type, and the digest of its uncompressed tar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub blob: Digest,
    pub media_type: String,
    pub diff_id: Digest,
}

#[derive(Debug)]
pub struct Store {
    root: PathBuf,
}

enum Hasher {
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl Hasher {
    fn new(algorithm: Algorithm) -> Hasher {
        match algorithm {
            Algorithm::Sha256 => Hasher::Sha256(Sha256::new()),
            Algorithm::Sha384 => Hasher::Sha384(Sha384::new()),
            Algorithm::Sha512 => Hasher::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(bytes),
            Hasher::Sha384(h) => h.update(bytes),
            Hasher::Sha512(h) => h.update(bytes),
        }
    }

    fn finish(self) -> Digest {
        match self {
            Hasher::Sha256(h) => Digest::from_hash(Algorithm::Sha256, &h.finalize()),
            Hasher::Sha384(h) => Digest::from_hash(Algorithm::Sha384, &h.finalize()),
            Hasher::Sha512(h) => Digest::from_hash(Algorithm::Sha512, &h.finalize()),
        }
    }
}

/// A file being written under `ingest/`, removed unless committed.
struct Partial {
    path: PathBuf,
    file: Option<BufWriter<File>>,
}

impl Partial {
    fn create(dir: &Path) -> Result<Partial, Error> {
        // Unique among this store's writers: the process, then a counter. The name is
        // never trusted; `create_new` refuses one that exists.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("{}-{n}", std::process::id()));
        let file = File::options().write(true).create_new(true).open(&path)?;
        Ok(Partial {
            path,
            file: Some(BufWriter::with_capacity(CHUNK, file)),
        })
    }

    /// Flushes and syncs the file, then moves it to the content-addressed name `to`, so a
    /// crash never leaves a torn file under a verified name. A file already at `to` holds
    /// the same bytes and is kept: it may be open, or mapped by a running VM.
    fn commit(mut self, to: &Path) -> Result<(), Error> {
        if to.is_file() {
            return Ok(());
        }
        let Some(f) = self.file.take() else {
            return Ok(());
        };
        let moved = f
            .into_inner()
            .map_err(|e| Error(e.to_string()))
            .and_then(|file| Ok(file.sync_all()?))
            .and_then(|()| Ok(fs::rename(&self.path, to)?));
        if moved.is_err() {
            let _ = fs::remove_file(&self.path);
            // Another writer may have committed the same bytes first.
            if to.is_file() {
                return Ok(());
            }
        }
        moved
    }
}

impl Partial {
    /// Flushes and syncs the file, then moves it to `to`, replacing what is there.
    fn replace(mut self, to: &Path) -> Result<(), Error> {
        let Some(f) = self.file.take() else {
            return Ok(());
        };
        let moved = f
            .into_inner()
            .map_err(|e| Error(e.to_string()))
            .and_then(|file| Ok(file.sync_all()?))
            .and_then(|()| Ok(fs::rename(&self.path, to)?));
        if moved.is_err() {
            let _ = fs::remove_file(&self.path);
        }
        moved
    }
}

/// What `refs/` records for a reference: the manifest it names, by the descriptor it was
/// chosen by, so that finding the image again checks what pulling it checked.
#[derive(serde::Serialize, serde::Deserialize)]
struct Tag {
    reference: String,
    manifest: Descriptor,
}

/// Makes the entries of `dir` durable: the renames into it outlast a power loss.
fn sync_dir(dir: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// A blob being downloaded (`Store::download`). Its file under `ingest/` stays locked for
/// the download's life and outlives a failed attempt, so the next one resumes, as
/// containerd keeps ingests (docs/research/registry-pull.md §2).
pub struct Download {
    path: PathBuf,
    file: BufWriter<File>,
    hasher: Hasher,
    offset: u64,
    digest: Digest,
    size: u64,
    target: PathBuf,
    /// The room the store's filesystem has for it (audit A10).
    room: Room,
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Download({}, {} of {} bytes)",
            self.digest, self.offset, self.size
        )
    }
}

impl Download {
    /// How many bytes are already here: where the next request should start.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Appends bytes that arrived; more than the blob's size is refused.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let offset = self.offset.saturating_add(bytes.len() as u64);
        if offset > self.size {
            return bad(format!("{}: more than its {} bytes", self.digest, self.size));
        }
        self.room.wrote(bytes.len())?;
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.offset = offset;
        Ok(())
    }

    /// Starts again from the first byte, for a server that ignored a range.
    pub fn restart(&mut self) -> Result<(), Error> {
        self.file.flush()?;
        let file = self.file.get_mut();
        file.set_len(0)?;
        file.seek(io::SeekFrom::Start(0))?;
        self.hasher = Hasher::new(self.digest.algorithm());
        self.offset = 0;
        Ok(())
    }

    /// Checks the size and then the digest, and moves the blob into place (fsync, then
    /// rename). A download that fails the check is thrown away, so the next starts clean.
    pub fn commit(self) -> Result<PathBuf, Error> {
        let Download {
            path,
            file,
            hasher,
            offset,
            digest,
            size,
            target,
            room: _,
        } = self;
        if offset != size {
            return bad(format!("{digest}: {offset} of its {size} bytes"));
        }
        let actual = hasher.finish();
        let file = file.into_inner().map_err(|e| Error(e.to_string()))?;
        if actual != digest {
            drop(file);
            let _ = fs::remove_file(&path);
            return bad(format!("{digest}: the content hashes to {actual}"));
        }
        file.sync_all()?;
        // Moved while still locked: a download waiting on the lock must find the blob.
        if target.is_file() {
            let _ = fs::remove_file(&path);
        } else {
            fs::rename(&path, &target)?;
        }
        drop(file);
        Ok(target)
    }
}

impl Write for Partial {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.file.as_mut() {
            Some(f) => f.write(buf),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Store {
    /// Opens the store at `root`, making its directories inside. The caller makes `root`
    /// private to its user, as containerd keeps its root 0700: blobs can come from private
    /// registries.
    pub fn open(root: &Path) -> Result<Store, Error> {
        if !root.is_dir() {
            return bad(format!("{}: not a directory", root.display()));
        }
        for dir in ["blobs/sha256", "blobs/sha384", "blobs/sha512", "ingest"] {
            fs::create_dir_all(root.join(dir))?;
        }
        fs::create_dir_all(root.join(format!("refs/v{REFS_VERSION}")))?;
        fs::create_dir_all(root.join(format!("rootfs/v{ROOTFS_VERSION}")))?;
        Ok(Store {
            root: root.to_path_buf(),
        })
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join("blobs")
            .join(digest.algorithm().name())
            .join(digest.hex())
    }

    pub fn has(&self, digest: &Digest) -> bool {
        self.blob_path(digest).is_file()
    }

    /// Stores the `size`-byte blob `digest` from `src`: hashed as it is written, and
    /// committed only if exactly `size` bytes arrived and they hash to `digest`.
    pub fn ingest(&self, digest: &Digest, size: u64, src: &mut dyn Read) -> Result<PathBuf, Error> {
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        let mut hasher = Hasher::new(digest.algorithm());
        let mut taken = src.take(size.saturating_add(1));
        let mut buf = vec![0u8; CHUNK];
        let mut got: u64 = 0;
        loop {
            let n = match taken.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            let chunk = buf.get(..n).unwrap_or_default();
            got = got.saturating_add(n as u64);
            if got > size {
                return bad(format!("{digest}: more than its {size} bytes"));
            }
            hasher.update(chunk);
            partial.write_all(chunk)?;
        }
        if got != size {
            return bad(format!("{digest}: {got} of its {size} bytes"));
        }
        let actual = hasher.finish();
        if actual != *digest {
            return bad(format!("{digest}: the content hashes to {actual}"));
        }
        let path = self.blob_path(digest);
        partial.commit(&path)?;
        Ok(path)
    }

    /// Starts or resumes downloading the `size`-byte blob `digest`. The bytes an earlier
    /// attempt left are hashed again, so the blob is verified whole when it is committed.
    /// Waits while another process downloads the same blob, and gives `None` if that left
    /// it stored.
    /// It is refused if what is left of it would leave the store's filesystem less free
    /// than `limits` keep, and stops once it would as it goes (audit A10).
    pub fn download(&self, digest: &Digest, size: u64, limits: &Limits) -> Result<Option<Download>, Error> {
        let path =
            self.root
                .join("ingest")
                .join(format!("{}-{}.partial", digest.algorithm().name(), digest.hex()));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.lock()?;
        // The lock may have been released by a download that finished. Its file has moved,
        // and what is at `path` now is an empty file this call made.
        if self.has(digest) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
        let mut hasher = Hasher::new(digest.algorithm());
        let mut offset: u64 = 0;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            hasher.update(buf.get(..n).unwrap_or_default());
            offset = offset.saturating_add(n as u64);
        }
        let ingest = self.root.join("ingest");
        let free = (limits.available)(&ingest)?;
        let left = size.saturating_sub(offset);
        if free < limits.keep_free.saturating_add(left) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "{digest}: {left} more bytes, with {free} free and {} to be left (SHARDS_KEEP_FREE)",
                    limits.keep_free
                ),
            )
            .into());
        }
        let mut download = Download {
            path,
            file: BufWriter::with_capacity(CHUNK, file),
            hasher,
            offset,
            digest: digest.clone(),
            size,
            target: self.blob_path(digest),
            room: Room::new(&ingest, limits)?,
        };
        // More than the blob holds can only be wrong.
        if offset > size {
            download.restart()?;
        }
        Ok(Some(download))
    }

    /// Records that `reference` names the manifest `manifest` describes, replacing what it
    /// named. What the record names, the blobs `contents` and the root filesystems, is made
    /// durable first, and the record after: no power loss leaves a record naming what it
    /// lost (audit A15).
    pub fn tag(&self, reference: &str, manifest: &Descriptor, contents: &[Digest]) -> Result<(), Error> {
        let mut dirs: Vec<PathBuf> = contents
            .iter()
            .map(|d| self.root.join("blobs").join(d.algorithm().name()))
            .collect();
        dirs.push(self.root.join(format!("rootfs/v{ROOTFS_VERSION}")));
        dirs.sort();
        dirs.dedup();
        for dir in &dirs {
            sync_dir(dir)?;
        }
        let record = serde_json::to_vec(&Tag {
            reference: reference.to_string(),
            manifest: manifest.clone(),
        })
        .map_err(|e| Error(e.to_string()))?;
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        partial.write_all(&record)?;
        partial.replace(&self.tag_path(reference))?;
        sync_dir(&self.root.join(format!("refs/v{REFS_VERSION}")))
    }

    /// The descriptor of the manifest `reference` names, if it has been pulled.
    pub fn tagged(&self, reference: &str) -> Result<Option<Descriptor>, Error> {
        let bytes = match fs::read(self.tag_path(reference)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let tag: Tag = serde_json::from_slice(&bytes).map_err(|e| Error(format!("{reference}: {e}")))?;
        if tag.reference != reference {
            return bad(format!("{reference}: its record names {}", tag.reference));
        }
        Ok(Some(tag.manifest))
    }

    /// References can be long and hold `/` and `:`, so records are named by their hash.
    fn tag_path(&self, reference: &str) -> PathBuf {
        let name = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(reference.as_bytes()));
        self.root.join(format!("refs/v{REFS_VERSION}")).join(name.hex())
    }

    /// The bytes of the small blob `desc` describes, if it is stored: at most `max`, and
    /// checked again against the digest and then the size. Content whose length is not its
    /// descriptor's is not trusted (image-spec descriptor.md).
    pub fn content(&self, desc: &Descriptor, max: u64) -> Result<Option<Vec<u8>>, Error> {
        let digest = desc.digest()?;
        let size = desc.size()?;
        if size > max {
            return bad(format!("{digest}: {size} bytes is over the {max}-byte limit"));
        }
        let path = self.blob_path(&digest);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return bad(format!("{}: over the {max}-byte limit", path.display()));
        }
        let mut hasher = Hasher::new(digest.algorithm());
        hasher.update(&bytes);
        if hasher.finish() != digest {
            return bad(format!(
                "{}: the stored copy has changed; remove it to fetch it again",
                path.display()
            ));
        }
        if bytes.len() as u64 != size {
            return bad(format!(
                "{digest}: {} bytes, where its descriptor says {size}",
                bytes.len()
            ));
        }
        Ok(Some(bytes))
    }

    /// Decompresses a layer's blob into a file under `ingest/`, removed when dropped. Its
    /// media type says whether the compression is sniffed (`oci::layer_compression`). The
    /// whole decompressed stream, bytes after the tar's end included, must match the
    /// DiffID, as containerd's applier checks it (`core/diff/apply/apply.go`), and must
    /// not pass `max` bytes.
    fn unpack(
        &self,
        layer: &Layer,
        bytes: &mut u64,
        limits: &Limits,
        room: &mut Room,
    ) -> Result<Partial, Error> {
        let how = oci::layer_compression(&layer.media_type)?;
        let mut file = File::open(self.blob_path(&layer.blob))?;
        let mut head = Vec::with_capacity(8);
        if how == oci::LayerCompression::Sniffed {
            (&mut file).take(8).read_to_end(&mut head)?;
            file.rewind()?;
        }
        let mut src = BufReader::with_capacity(CHUNK, file);
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        let mut sink = Sink {
            hasher: Hasher::new(layer.diff_id.algorithm()),
            out: Checked {
                out: &mut partial,
                room,
            },
            written: bytes,
            max: limits.bytes,
        };
        match compression(&head) {
            Compression::None => {
                io::copy(&mut src, &mut sink)?;
            }
            Compression::Gzip => {
                io::copy(&mut flate2::bufread::MultiGzDecoder::new(src), &mut sink)?;
            }
            Compression::Zstd => zstd(&mut src, &mut sink)?,
        }
        let actual = sink.hasher.finish();
        if actual != layer.diff_id {
            return bad(format!(
                "layer {}: its content hashes to {actual}, not its DiffID {}",
                layer.blob, layer.diff_id
            ));
        }
        partial.flush()?;
        Ok(partial)
    }

    /// The EROFS root filesystem of `layers`, built on first use: each layer is unpacked
    /// and checked, the layers are stacked (layer.rs), and the tree is written once. It is
    /// kept by ChainID; the unpacked tars go when it is done. Building it takes no more
    /// than `limits` allow (audit A10), and one build at a time goes on in a store,
    /// whichever process asks: a second of the same image finds the first's.
    pub fn rootfs(&self, layers: &[Layer], limits: &Limits) -> Result<PathBuf, Error> {
        let diff_ids: Vec<Digest> = layers.iter().map(|l| l.diff_id.clone()).collect();
        let chain = oci::chain_id(&diff_ids).ok_or_else(|| Error("an image with no layers".into()))?;
        let path = self.root.join(format!("rootfs/v{ROOTFS_VERSION}")).join(format!(
            "{}-{}.erofs",
            chain.algorithm().name(),
            chain.hex()
        ));
        if path.is_file() {
            return Ok(path);
        }
        let building = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(format!("rootfs/v{ROOTFS_VERSION}/.building")))?;
        building.lock()?;
        if path.is_file() {
            return Ok(path);
        }
        let ingest = self.root.join("ingest");
        let mut room = Room::new(&ingest, limits)?;
        let (mut bytes, mut entries, mut metadata) = (0u64, 0u64, 0u64);
        let mut tree = layer::root();
        let mut tars = Vec::with_capacity(layers.len());
        for (i, l) in layers.iter().enumerate() {
            let tar = self.unpack(l, &mut bytes, limits, &mut room)?;
            let source = u32::try_from(i).map_err(|_| Error("too many layers".into()))?;
            let file = File::open(&tar.path)?;
            let mut count = |e: &crate::tar::Entry| {
                entries += 1;
                let held = e.path.len() + e.link.len();
                let held = e.xattrs.iter().fold(held, |n, (k, v)| n + k.len() + v.len());
                metadata = metadata.saturating_add(held as u64);
                if entries > limits.entries {
                    return bad(format!(
                        "the image has more than {} entries (SHARDS_MAX_IMAGE_ENTRIES)",
                        limits.entries
                    ));
                }
                if metadata > limits.metadata {
                    return bad(format!(
                        "the image's names, links and xattrs pass {} bytes (SHARDS_MAX_IMAGE_METADATA)",
                        limits.metadata
                    ));
                }
                Ok(())
            };
            layer::apply(
                &mut tree,
                source,
                BufReader::with_capacity(CHUNK, file),
                &mut count,
            )?;
            tars.push(tar);
        }
        let files = tars
            .iter()
            .map(|t| File::open(&t.path))
            .collect::<io::Result<Vec<_>>>()?;
        let mut partial = Partial::create(&ingest)?;
        let mut out = Checked {
            out: &mut partial,
            room: &mut room,
        };
        erofs::write(&tree, &mut Archives(files), &mut out)?;
        partial.commit(&path)?;
        drop(building);
        Ok(path)
    }
}

/// What preparing one image's root filesystem may take, its layers together (audit A10).
/// The work is linear in what it reads, so these bound its time too.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes its layers decompress to, which `ingest/` holds while it is built.
    pub bytes: u64,
    /// Entries in its layers, each held in memory while it is built.
    pub entries: u64,
    /// Bytes of its entries' names, link targets and xattrs, held in memory too.
    pub metadata: u64,
    /// Bytes to leave free on the store's filesystem: a build stops short of them.
    pub keep_free: u64,
    /// The bytes free now on the filesystem holding a path.
    pub available: fn(&Path) -> io::Result<u64>,
}

impl Limits {
    /// No limits, and room for anything.
    pub fn none() -> Limits {
        Limits {
            bytes: u64::MAX,
            entries: u64::MAX,
            metadata: u64::MAX,
            keep_free: 0,
            available: |_| Ok(u64::MAX),
        }
    }
}

/// How much a build writes before it looks again at what is left free.
const LOOK_EVERY: u64 = 64 << 20;

/// The room a build has on its filesystem: looked at as it starts, and again every
/// `LOOK_EVERY` bytes written, so it stops before it would leave less than `keep_free`,
/// give or take what it wrote since.
struct Room {
    dir: PathBuf,
    keep_free: u64,
    available: fn(&Path) -> io::Result<u64>,
    since: u64,
}

impl Room {
    fn new(dir: &Path, limits: &Limits) -> io::Result<Room> {
        let mut room = Room {
            dir: dir.to_path_buf(),
            keep_free: limits.keep_free,
            available: limits.available,
            since: 0,
        };
        room.look()?;
        Ok(room)
    }

    fn look(&mut self) -> io::Result<()> {
        self.since = 0;
        let free = (self.available)(&self.dir)?;
        if free < self.keep_free.saturating_add(LOOK_EVERY) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "{}: {free} bytes free, and {} are to be left (SHARDS_KEEP_FREE)",
                    self.dir.display(),
                    self.keep_free
                ),
            ));
        }
        Ok(())
    }

    fn wrote(&mut self, n: usize) -> io::Result<()> {
        self.since = self.since.saturating_add(n as u64);
        if self.since >= LOOK_EVERY {
            self.look()?;
        }
        Ok(())
    }
}

/// A file being written, within the room its build has.
struct Checked<'a> {
    out: &'a mut Partial,
    room: &'a mut Room,
}

impl Write for Checked<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.room.wrote(buf.len())?;
        self.out.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Where decompressed layer bytes go: hashed, counted against the image's budget, and
/// written.
struct Sink<'a> {
    hasher: Hasher,
    out: Checked<'a>,
    written: &'a mut u64,
    max: u64,
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        *self.written = self.written.saturating_add(buf.len() as u64);
        if *self.written > self.max {
            return Err(io::Error::other(format!(
                "the image decompresses to more than {} bytes (SHARDS_MAX_IMAGE_BYTES)",
                self.max
            )));
        }
        self.hasher.update(buf);
        self.out.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Compression {
    None,
    Gzip,
    Zstd,
}

/// containerd's `DetectCompression` (`pkg/archive/compression/compression.go`), given a
/// blob's first 8 bytes: gzip's magic and method, a zstd frame's magic, or a skippable
/// frame's, which counts only once its 8-byte header is whole.
fn compression(head: &[u8]) -> Compression {
    match head {
        [0x1f, 0x8b, 0x08, ..] => Compression::Gzip,
        [0x28, 0xb5, 0x2f, 0xfd, ..] | [0x50..=0x5f, 0x2a, 0x4d, 0x18, _, _, _, _, ..] => Compression::Zstd,
        _ => Compression::None,
    }
}

/// Decompresses every zstd frame in `src` and skips skippable ones (RFC 8878 §3.1.2), as
/// klauspost/compress v1.20.0 does for containerd: each frame's checksum is verified, and
/// its window may not pass 512 MiB. klauspost exempts single-segment frames up to 64 GiB,
/// which it buffers whole; we cap those too.
fn zstd(src: &mut BufReader<File>, out: &mut Sink<'_>) -> Result<(), Error> {
    use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
    let mut frame = FrameDecoder::new();
    frame.set_max_window_size(ZSTD_MAX_WINDOW);
    while !src.fill_buf()?.is_empty() {
        match StreamingDecoder::new_with_decoder(&mut *src, &mut frame) {
            Ok(mut decoder) => {
                io::copy(&mut decoder, out)?;
                let stored = frame.get_checksum_from_data();
                if stored.is_some() && stored != frame.get_calculated_checksum() {
                    return bad("zstd: a frame's checksum does not match its content");
                }
            }
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length, ..
            })) => {
                let skipped = io::copy(&mut (&mut *src).take(u64::from(length)), &mut io::sink())?;
                if skipped != u64::from(length) {
                    return bad("a zstd skippable frame is cut short");
                }
            }
            Err(e) => return bad(format!("zstd: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::tar::tests::{Member, Writer};

    fn sha256(bytes: &[u8]) -> Digest {
        Digest::from_hash(Algorithm::Sha256, &Sha256::digest(bytes))
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// A descriptor of `size` bytes named `digest`.
    fn described(digest: &Digest, size: usize) -> Descriptor {
        Descriptor {
            media_type: oci::media::OCI_MANIFEST.into(),
            digest: digest.to_string(),
            size: i64::try_from(size).unwrap(),
            platform: None,
        }
    }

    #[test]
    fn blobs_are_committed_only_when_they_match() {
        let root = temp("ingest");
        assert!(Store::open(&root.join("missing")).is_err());
        let store = Store::open(&root).unwrap();
        let blob = b"hello, registry";
        let d = sha256(blob);
        let path = store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), blob);
        assert_eq!(
            store.content(&described(&d, blob.len()), 100).unwrap(),
            Some(blob.to_vec())
        );
        // Fetched again, the stored copy stays.
        assert_eq!(store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap(), path);
        let other = sha256(b"other");
        for (digest, size, why) in [
            (&other, blob.len() as u64, "a wrong digest"),
            (&d, blob.len() as u64 - 1, "more bytes than the descriptor says"),
            (&d, blob.len() as u64 + 1, "fewer bytes than the descriptor says"),
        ] {
            assert!(store.ingest(digest, size, &mut &blob[..]).is_err(), "{why}");
        }
        assert!(!store.has(&other));
        assert_eq!(
            fs::read_dir(root.join("ingest")).unwrap().count(),
            0,
            "no partial left behind"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn layers_unpack_by_sniffed_compression_and_check_their_diff_id() {
        let root = temp("unpack");
        let store = Store::open(&root).unwrap();
        let tar = Writer::default()
            .member(Member {
                name: b"etc/hostname",
                data: b"box\n",
                ..Member::default()
            })
            .finish();
        let diff_id = sha256(&tar);
        // Two gzip members, as some writers concatenate them, and a plain tar labelled
        // gzip, as some writers mislabel them.
        let (half, rest) = tar.split_at(700);
        let mut gz = gzip(half);
        gz.extend(gzip(rest));
        for blob in [gz.clone(), tar.clone()] {
            let d = sha256(&blob);
            store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
            let layer = Layer {
                blob: d,
                media_type: oci::media::DOCKER_LAYER_GZIP.into(),
                diff_id: diff_id.clone(),
            };
            let out = unpack_within(&store, &layer, 1 << 20).unwrap();
            assert_eq!(fs::read(&out.path).unwrap(), tar);
            drop(out);
            // A wrong DiffID, or a cap below the size, and nothing is kept.
            let wrong = Layer {
                diff_id: sha256(b"x"),
                ..layer.clone()
            };
            assert!(unpack_within(&store, &wrong, 1 << 20).is_err());
            assert!(unpack_within(&store, &layer, 100).is_err());
            assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        }
        // OCI's plain tar type is read as it is, whatever its bytes look like.
        let raw = |diff_id: Digest| Layer {
            blob: sha256(&gz),
            media_type: oci::media::OCI_LAYER.into(),
            diff_id,
        };
        assert!(unpack_within(&store, &raw(diff_id.clone()), 1 << 20).is_err());
        let out = unpack_within(&store, &raw(sha256(&gz)), 1 << 20).unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), gz);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn downloads_resume_where_they_stopped_and_are_verified_whole() {
        let root = temp("download");
        let store = Store::open(&root).unwrap();
        let blob: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let d = sha256(&blob);
        let size = blob.len() as u64;
        let mut first = store.download(&d, size, &Limits::none()).unwrap().unwrap();
        first.write(&blob[..40_000]).unwrap();
        drop(first);
        let mut second = store.download(&d, size, &Limits::none()).unwrap().unwrap();
        assert_eq!(second.offset(), 40_000, "the first attempt's bytes are kept");
        assert!(
            second.write(&vec![0u8; 60_001]).is_err(),
            "more than the blob holds"
        );
        second.write(&blob[40_000..]).unwrap();
        let path = second.commit().unwrap();
        assert_eq!(fs::read(path).unwrap(), blob);
        assert!(
            store.download(&d, size, &Limits::none()).unwrap().is_none(),
            "already stored"
        );
        // Wrong bytes are thrown away, so the next attempt starts clean.
        let other = sha256(b"other");
        let mut wrong = store.download(&other, 5, &Limits::none()).unwrap().unwrap();
        wrong.write(b"wrong").unwrap();
        assert!(wrong.commit().is_err());
        assert_eq!(
            store
                .download(&other, 5, &Limits::none())
                .unwrap()
                .unwrap()
                .offset(),
            0
        );
        let mut again = store.download(&other, 5, &Limits::none()).unwrap().unwrap();
        again.write(b"wro").unwrap();
        again.restart().unwrap();
        assert_eq!(again.offset(), 0);
        drop(again);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn one_process_at_a_time_downloads_a_blob() {
        let root = temp("download-lock");
        let store = Store::open(&root).unwrap();
        let blob = b"shared blob".to_vec();
        let d = sha256(&blob);
        let mut holder = store
            .download(&d, blob.len() as u64, &Limits::none())
            .unwrap()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (store2, d2, n) = (Store::open(&root).unwrap(), d.clone(), blob.len() as u64);
        let waiter = std::thread::spawn(move || {
            let _ = tx.send(store2.download(&d2, n, &Limits::none()).map(|d| d.is_none()));
        });
        // While the holder has the blob's lock, the other download waits.
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err());
        holder.write(&blob).unwrap();
        holder.commit().unwrap();
        let stored = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(stored, "it finds the blob stored");
        waiter.join().unwrap();
        assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn references_record_their_manifest() {
        let root = temp("tags");
        let store = Store::open(&root).unwrap();
        let a = described(&sha256(b"a"), 1);
        let b = Descriptor {
            platform: Some(oci::Platform {
                architecture: "arm64".into(),
                os: "linux".into(),
                ..oci::Platform::default()
            }),
            ..described(&sha256(b"b"), 1)
        };
        let name = "docker.io/library/alpine:latest";
        assert_eq!(store.tagged(name).unwrap(), None);
        store.tag(name, &a, &[]).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(a.clone()));
        store.tag(name, &b, &[]).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(b));
        assert_eq!(store.tagged("docker.io/library/alpine:3").unwrap(), None);
        // A record of the shape before descriptors is not read: the image is pulled again.
        let old = root
            .join("refs")
            .join(Digest::from_hash(Algorithm::Sha256, &Sha256::digest(b"docker.io/library/alpine:3")).hex());
        fs::write(
            &old,
            format!(
                r#"{{"reference":"docker.io/library/alpine:3","manifest":"{}"}}"#,
                a.digest
            ),
        )
        .unwrap();
        assert_eq!(store.tagged("docker.io/library/alpine:3").unwrap(), None);
        let _ = fs::remove_dir_all(&root);
    }

    /// A reference is recorded only once what it names is durable: a directory of what
    /// it names that cannot be synced fails the record, and nothing is recorded (audit
    /// A15). Windows has no directory to sync.
    #[cfg(unix)]
    #[test]
    fn a_reference_is_recorded_once_what_it_names_is_durable() {
        let root = temp("tag-order");
        let store = Store::open(&root).unwrap();
        let name = "docker.io/library/alpine:latest";
        let manifest = described(&sha256(b"a"), 1);
        let sha512 = Digest::from_hash(Algorithm::Sha512, &Sha512::digest(b"a"));
        fs::remove_dir_all(root.join("blobs/sha512")).unwrap();
        assert!(store.tag(name, &manifest, &[sha512]).is_err());
        assert_eq!(store.tagged(name).unwrap(), None, "nothing recorded");
        fs::remove_dir_all(root.join(format!("rootfs/v{ROOTFS_VERSION}"))).unwrap();
        assert!(store.tag(name, &manifest, &[]).is_err());
        assert_eq!(store.tagged(name).unwrap(), None, "nothing recorded");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn small_blobs_are_checked_again_whenever_they_are_read() {
        let root = temp("content");
        let store = Store::open(&root).unwrap();
        let blob = br#"{"config":{"Cmd":["exit","7"]}}"#;
        let d = sha256(blob);
        let desc = described(&d, blob.len());
        assert_eq!(store.content(&desc, 1024).unwrap(), None, "not stored");
        store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
        assert_eq!(store.content(&desc, 1024).unwrap(), Some(blob.to_vec()));

        // The stored copy changed under its name: as valid JSON of the same length,
        // truncated, or grown.
        let path = store.blob_path(&d);
        let same_length = String::from_utf8(blob.to_vec()).unwrap().replace('7', "9");
        let cases = [
            (same_length.into_bytes(), "has changed"),
            (blob[..blob.len() - 1].to_vec(), "has changed"),
            ([&blob[..], b" "].concat(), "has changed"),
            (vec![b' '; 2048], "over the 1024-byte limit"),
        ];
        for (bytes, why) in cases {
            fs::write(&path, &bytes).unwrap();
            let e = store.content(&desc, 1024).unwrap_err().to_string();
            assert!(e.contains(why), "{why}: {e}");
        }
        fs::write(&path, blob).unwrap();

        // The descriptor is wrong about the content: its size, or a size past the limit.
        let e = store
            .content(&described(&d, blob.len() + 1), 1024)
            .unwrap_err()
            .to_string();
        assert!(e.contains("where its descriptor says"), "{e}");
        let e = store.content(&desc, 8).unwrap_err().to_string();
        assert!(e.contains("over the 8-byte limit"), "{e}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compression_is_detected_as_containerd_detects_it() {
        let skippable = [0x5f, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
        for (head, gzip, zstd) in [
            (&[0x1f, 0x8b, 0x08][..], true, false),
            (&[0x1f, 0x8b, 0x07, 0][..], false, false),
            (&[0x28, 0xb5, 0x2f, 0xfd][..], false, true),
            (&skippable[..], false, true),
            // The magic of a skippable frame without the rest of its header.
            (&skippable[..7], false, false),
            (&[0x60, 0x2a, 0x4d, 0x18, 0, 0, 0, 0][..], false, false),
            (&[][..], false, false),
        ] {
            let found = compression(head);
            assert_eq!(matches!(found, Compression::Gzip), gzip, "{head:x?}");
            assert_eq!(matches!(found, Compression::Zstd), zstd, "{head:x?}");
        }
    }

    /// Two zstd frames with a skippable frame between them (RFC 8878 §3.1.2), as seekable
    /// and chunked zstd writers emit, and a frame whose checksum is wrong.
    #[test]
    fn zstd_layers_unpack_across_frames() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};
        let root = temp("zstd");
        let store = Store::open(&root).unwrap();
        let tar = Writer::default()
            .member(Member {
                name: b"data",
                data: &[7u8; 3000],
                ..Member::default()
            })
            .finish();
        let (a, b) = tar.split_at(1500);
        let mut blob = compress_to_vec(a, CompressionLevel::Fastest);
        blob.extend_from_slice(&[0x5a, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3]);
        blob.extend(compress_to_vec(b, CompressionLevel::Fastest));
        let layer = Layer {
            blob: sha256(&blob),
            media_type: format!("{}+zstd", oci::media::OCI_LAYER),
            diff_id: sha256(&tar),
        };
        store
            .ingest(&layer.blob, blob.len() as u64, &mut &blob[..])
            .unwrap();
        let out = unpack_within(&store, &layer, 1 << 20).unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), tar);
        // The frame decodes, but its last 4 bytes, the checksum, disagree.
        let mut blob = compress_to_vec(&tar[..], CompressionLevel::Fastest);
        if let Some(last) = blob.last_mut() {
            *last ^= 1;
        }
        let layer = Layer {
            blob: sha256(&blob),
            media_type: format!("{}+zstd", oci::media::OCI_LAYER),
            diff_id: sha256(&tar),
        };
        store
            .ingest(&layer.blob, blob.len() as u64, &mut &blob[..])
            .unwrap();
        assert!(unpack_within(&store, &layer, 1 << 20).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn root_filesystems_are_built_once_per_chain() {
        let root = temp("rootfs");
        let store = Store::open(&root).unwrap();
        let tar = Writer::default()
            .member(Member {
                name: b"bin/tool",
                data: b"#!",
                mode: 0o755,
                ..Member::default()
            })
            .finish();
        let blob = gzip(&tar);
        let layer = Layer {
            blob: sha256(&blob),
            media_type: format!("{}+gzip", oci::media::OCI_LAYER),
            diff_id: sha256(&tar),
        };
        store
            .ingest(&layer.blob, blob.len() as u64, &mut &blob[..])
            .unwrap();
        let path = store
            .rootfs(std::slice::from_ref(&layer), &Limits::none())
            .unwrap();
        let image = fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(image[1024..1028].try_into().unwrap()),
            0xE0F5_E1E2
        );
        let again = store.rootfs(&[layer], &Limits::none()).unwrap();
        assert_eq!(again, path);
        assert_eq!(
            fs::read_dir(root.join("ingest")).unwrap().count(),
            0,
            "unpacked tars are gone"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Unpacks `layer` alone, within `max` bytes.
    fn unpack_within(store: &Store, layer: &Layer, max: u64) -> Result<Partial, Error> {
        let limits = Limits {
            bytes: max,
            ..Limits::none()
        };
        let mut room = Room::new(&store.root.join("ingest"), &limits)?;
        store.unpack(layer, &mut 0, &limits, &mut room)
    }

    /// Stores `tar`, gzipped, as a layer.
    fn stored_layer(store: &Store, tar: &[u8]) -> Layer {
        let blob = gzip(tar);
        let layer = Layer {
            blob: sha256(&blob),
            media_type: format!("{}+gzip", oci::media::OCI_LAYER),
            diff_id: sha256(tar),
        };
        store
            .ingest(&layer.blob, blob.len() as u64, &mut &blob[..])
            .unwrap();
        layer
    }

    /// A build refused leaves nothing: no root filesystem, nothing in `ingest/`, and the
    /// store free to build again.
    fn refused(store: &Store, root: &Path, layer: &Layer, limits: &Limits, why: &str) {
        let e = store
            .rootfs(std::slice::from_ref(layer), limits)
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{e}");
        let built = fs::read_dir(root.join(format!("rootfs/v{ROOTFS_VERSION}")))
            .unwrap()
            .filter(|e| !e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(built, 0, "{why}: a root filesystem was left");
        assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0, "{why}");
    }

    /// Building a root filesystem takes no more than its limits allow (audit A10): a
    /// layer that decompresses past the image's bytes, as a compression bomb does; more
    /// entries, or bytes of names and xattrs, than the image may hold in memory; or a
    /// filesystem whose room would fall below what is to be left free. Within them it is
    /// built, after every refusal.
    #[test]
    fn a_build_takes_no_more_than_its_limits() {
        let root = temp("limits");
        let store = Store::open(&root).unwrap();
        // 64 MiB of zeroes, which gzip to 64 KiB.
        let zeroes = vec![0u8; 64 << 20];
        let bomb = stored_layer(
            &store,
            &Writer::default()
                .member(Member {
                    name: b"zeroes",
                    data: &zeroes,
                    ..Member::default()
                })
                .finish(),
        );
        let bytes = Limits {
            bytes: 32 << 20,
            ..Limits::none()
        };
        refused(
            &store,
            &root,
            &bomb,
            &bytes,
            "decompresses to more than 33554432 bytes",
        );

        let mut many = Writer::default();
        for i in 0..100u32 {
            let name = format!("f{i}");
            many.member(Member {
                name: name.as_bytes(),
                ..Member::default()
            });
        }
        let many = stored_layer(&store, &many.finish());
        let entries = Limits {
            entries: 99,
            ..Limits::none()
        };
        refused(&store, &root, &many, &entries, "more than 99 entries");

        let big = vec![b'x'; 60_000];
        let xattr = stored_layer(
            &store,
            &Writer::default()
                .pax(&[("SCHILY.xattr.user.big", &big)])
                .member(Member {
                    name: b"f",
                    ..Member::default()
                })
                .finish(),
        );
        let metadata = Limits {
            metadata: 50_000,
            ..Limits::none()
        };
        refused(&store, &root, &xattr, &metadata, "pass 50000 bytes");

        let full = Limits {
            keep_free: 1 << 30,
            available: |_| Ok((1 << 30) + (32 << 20)),
            ..Limits::none()
        };
        refused(&store, &root, &bomb, &full, "are to be left (SHARDS_KEEP_FREE)");
        // Room enough as it starts, and none once the layer's 64 MiB are written.
        static LOOKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let filling = Limits {
            keep_free: 1 << 30,
            available: |_| match LOOKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Ok(u64::MAX),
                _ => Ok(0),
            },
            ..Limits::none()
        };
        refused(&store, &root, &bomb, &filling, "0 bytes free");
        // Room for the unpacked layer, as the build starts and once it is written, and
        // none as the image is written.
        static LATER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let late = Limits {
            keep_free: 1 << 30,
            available: |_| match LATER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 | 1 => Ok(u64::MAX),
                _ => Ok(0),
            },
            ..Limits::none()
        };
        refused(&store, &root, &bomb, &late, "0 bytes free");
        assert!(
            LATER.load(std::sync::atomic::Ordering::SeqCst) > 2,
            "refused as it unpacked"
        );

        for layer in [&bomb, &many, &xattr] {
            store
                .rootfs(std::slice::from_ref(layer), &Limits::none())
                .unwrap();
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Builds of one image at once, from two stores on one directory, as two processes
    /// would have: one builds it, and the other finds it built (audit A10).
    #[test]
    fn an_image_is_built_once_however_many_ask_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOOKS: AtomicUsize = AtomicUsize::new(0);
        let root = temp("singleflight");
        let store = Store::open(&root).unwrap();
        let layer = stored_layer(
            &store,
            &Writer::default()
                .member(Member {
                    name: b"f",
                    data: b"x",
                    ..Member::default()
                })
                .finish(),
        );
        // A build looks at its room as it starts: the number of looks is the number of
        // builds.
        let counting = Limits {
            available: |_| {
                LOOKS.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(u64::MAX)
            },
            ..Limits::none()
        };
        let paths: Vec<PathBuf> = std::thread::scope(|s| {
            let builds: Vec<_> = (0..4)
                .map(|_| {
                    let (root, layer) = (&root, &layer);
                    s.spawn(move || {
                        let store = Store::open(root).unwrap();
                        store.rootfs(std::slice::from_ref(layer), &counting).unwrap()
                    })
                })
                .collect();
            builds.into_iter().map(|b| b.join().unwrap()).collect()
        });
        assert!(paths.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(LOOKS.load(Ordering::SeqCst), 1, "built more than once");
        let _ = fs::remove_dir_all(&root);
    }

    /// A download is refused if what is left of it would leave the store's filesystem
    /// less free than its limits keep, and stops as it goes once it would (audit A10).
    #[test]
    fn a_download_leaves_the_room_it_must() {
        let root = temp("download-room");
        let store = Store::open(&root).unwrap();
        let blob = vec![7u8; 96 << 20];
        let d = sha256(&blob);
        let tight = Limits {
            keep_free: 1 << 30,
            available: |_| Ok((1 << 30) + (64 << 20)),
            ..Limits::none()
        };
        let e = store.download(&d, blob.len() as u64, &tight).unwrap_err();
        assert!(e.to_string().contains("to be left (SHARDS_KEEP_FREE)"), "{e}");
        static LOOKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let shrinking = Limits {
            keep_free: 1 << 30,
            available: |_| match LOOKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 | 1 => Ok(u64::MAX),
                _ => Ok(0),
            },
            ..Limits::none()
        };
        let mut download = store
            .download(&d, blob.len() as u64, &shrinking)
            .unwrap()
            .unwrap();
        let written = blob.chunks(1 << 20).try_for_each(|c| download.write(c));
        let e = written.unwrap_err();
        assert!(e.to_string().contains("0 bytes free"), "{e}");
        assert!(download.offset() < blob.len() as u64, "it stopped");
        drop(download);
        assert!(!store.has(&d));
        let _ = fs::remove_dir_all(&root);
    }
}
