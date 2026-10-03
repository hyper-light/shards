//! The image store: blobs by digest, exactly as registries serve them, and each image's
//! root filesystem as EROFS, by ChainID (docs/design/architecture.md D18).
//!
//! Blobs keep the registry's bytes, as containerd's content store and the OCI image layout
//! keep them, so a later push or save can reproduce their digests. Nothing enters the
//! store unverified: a blob is committed only when its size and digest match its
//! descriptor, and a layer only counts once its decompressed bytes match its DiffID
//! (docs/research/registry-pull.md §5, rows 4 and 6).

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use sha2::{Digest as _, Sha256, Sha384, Sha512};

use crate::erofs;
use crate::layer::{self, Archives};
use crate::oci::{self, Descriptor};
use crate::reference::{Algorithm, Digest};
use crate::{Error, bad};

/// Bumped whenever the EROFS writer's output changes, so older root filesystems are rebuilt.
/// 2: regular files are plain, for DAX (erofs.rs).
pub const ROOTFS_VERSION: u32 = 2;
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

/// A hold on a store's content ([`Store::lease`]), let go when dropped.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

/// A store held whole by a collection ([`Store::collect`]): no lease is given meanwhile.
#[derive(Debug)]
pub struct Whole {
    _file: File,
}

/// What a collection removed ([`Store::collect`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub blobs: u64,
    pub rootfs: u64,
    /// Files left in `ingest/`.
    pub ingest: u64,
    pub bytes: u64,
}

/// What the store holds of a small blob ([`Store::held`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Held {
    /// Nothing under its digest.
    Missing,
    /// A descriptor nothing can be held by, and why: its digest or size cannot be read,
    /// its size is past the limit, or its blob is not of the size it says. The same at
    /// every look, unlike a read that fails: a listing passes it by, as a collection
    /// does, rather than fail every image for one record.
    Invalid(String),
    /// A copy that is not what its digest names any more, and why: a pull fetches it
    /// again in its place.
    Changed(String),
    /// Its bytes, checked.
    Whole(Vec<u8>),
}

/// Whether `file` is still the file at `path`: not moved, by a download that finished
/// with it, or replaced. Where files have no identity to compare (Windows offers none in
/// std), it is taken to be.
fn still_at(file: &File, path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let ours = file.metadata()?;
        match fs::symlink_metadata(path) {
            Ok(m) => Ok(m.dev() == ours.dev() && m.ino() == ours.ino()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Ok(true)
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
        // never trusted; `create_new` refuses one that exists, which one a crashed
        // process of the same ID left may: the next is tried, as os.CreateTemp tries
        // up to 10,000.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut tries = 0;
        let (path, file) = loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = dir.join(format!("{}-{n}", std::process::id()));
            match File::options().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < 10_000 => tries += 1,
                Err(e) => return Err(e.into()),
            }
        };
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
/// chosen by, so that finding the image again checks what pulling it checked; and what
/// the reference resolved to, an index or the manifest itself, as Docker reports it.
/// Records written before `resolved` was kept read as resolving to their manifest.
#[derive(serde::Serialize, serde::Deserialize)]
struct Tag {
    reference: String,
    manifest: Descriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    /// The repository it was pulled from, if pulled: what dockerd shows as the image's
    /// pull identity, kept when another name is given it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    /// What it resolved to, described as it was when the record was made: by the
    /// registry, for a pull; by an archive's index, annotations and all, for a load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<Descriptor>,
}

/// An image the store's references name: what they resolved to (its ID, as dockerd's
/// containerd store gives it), those references, when it was made (its config's
/// `created`), its manifests, and the bytes of it here: its content, and its root
/// filesystems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: Digest,
    pub references: Vec<String>,
    /// What its references resolved to, described: an index, or its manifest.
    pub target: Descriptor,
    /// The manifest our platform's runs use, and its config's bytes, if here.
    pub manifest: Digest,
    pub config: Option<Vec<u8>>,
    /// When a record of it was last written, and the repositories it was pulled from.
    pub tagged_at: Option<std::time::SystemTime>,
    pub sources: Vec<String>,
    /// What each reference resolved to as its record describes it, where it does.
    pub targets: BTreeMap<String, Descriptor>,
    pub created: Option<String>,
    pub manifests: Vec<ImageManifest>,
    pub content: u64,
    pub unpacked: u64,
}

/// A manifest of an image: its platform (`os/arch[/variant]`), whether it is an
/// attestation, whether all of it is here, and the bytes of it that are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageManifest {
    pub digest: Digest,
    pub platform: Option<String>,
    pub attestation: bool,
    pub available: bool,
    pub content: u64,
    pub unpacked: u64,
}

/// A platform as containerd's platforms.Format writes it.
fn platform_string(p: &oci::Platform) -> String {
    let mut s = format!("{}/{}", p.os, p.architecture);
    if let Some(v) = p.variant.as_deref().filter(|v| !v.is_empty()) {
        s.push('/');
        s.push_str(v);
    }
    s
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
    /// Whether it replaces a stored copy that has changed.
    replace: bool,
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
            replace,
            room: _,
        } = self;
        if offset != size {
            return bad(format!("{digest}: {offset} of its {size} bytes"));
        }
        let actual = hasher.finish();
        let file = file.into_inner().map_err(|e| Error(e.to_string()))?;
        if actual != digest {
            // Gone before the lock is, so that no download waiting on it hashes it.
            let _ = fs::remove_file(&path);
            drop(file);
            return bad(format!("{digest}: the content hashes to {actual}"));
        }
        file.sync_all()?;
        // Moved while still locked: a download waiting on the lock must find the blob.
        if target.is_file() && !replace {
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

/// A layer's archive, decompressed and checked against its DiffID, in a file under
/// `ingest/` that goes when this is dropped.
pub struct Unpacked(Tar);

impl std::fmt::Debug for Unpacked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unpacked({})", self.0.path().display())
    }
}

impl Unpacked {
    /// The archive, opened for reading.
    pub fn open(&self) -> Result<File, Error> {
        Ok(File::open(self.0.path())?)
    }

    /// The archive's bytes, as [`Store::rootfs`] counts them against its limit.
    pub fn size(&self) -> Result<u64, Error> {
        Ok(fs::metadata(self.0.path())?.len())
    }
}

/// A layer's archive, checked against its DiffID: a blob that is its own archive, read
/// where it is, or a decompressed copy, which goes when this does. A blob is only read:
/// copying one that needs no decompressing wrote it to disk again for nothing (PM M78).
enum Tar {
    Blob(PathBuf),
    Temp(Partial),
}

impl Tar {
    fn path(&self) -> &Path {
        match self {
            Tar::Blob(p) => p,
            Tar::Temp(p) => &p.path,
        }
    }
}

/// A directory under `ingest/` private to one build, removed with what it holds when
/// dropped; one a crashed build leaves is collected.
#[derive(Debug)]
pub struct Stage(PathBuf);

impl Stage {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A blob being written, hashed as it goes; [`BlobWriter::commit`] stores it under its
/// SHA-256 digest. Dropped uncommitted, it is removed.
///
/// A thread of its own hashes and writes what it is given, a piece at a time, so whoever
/// makes the blob waits for neither: a layer's SHA-256 alone took a sixth of the time a
/// build spent making a layer of a million entries (PM M78).
pub struct BlobWriter {
    piece: Vec<u8>,
    to: Option<mpsc::SyncSender<Vec<u8>>>,
    /// Pieces the thread is done with, to be filled again.
    back: mpsc::Receiver<Vec<u8>>,
    worker: Option<std::thread::JoinHandle<io::Result<(Partial, Hasher)>>>,
    size: u64,
    blobs: PathBuf,
}

/// How many pieces may wait for a blob's writer: what the two hold between them.
const PIECES: usize = 2;

impl std::fmt::Debug for BlobWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlobWriter({} bytes)", self.size)
    }
}

impl BlobWriter {
    fn new(mut partial: Partial, blobs: PathBuf) -> Result<BlobWriter, Error> {
        let (to, from) = mpsc::sync_channel::<Vec<u8>>(PIECES);
        let (give_back, back) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("blob-writer".into())
            .spawn(move || {
                let mut hasher = Hasher::new(Algorithm::Sha256);
                for mut piece in from {
                    hasher.update(&piece);
                    partial.write_all(&piece)?;
                    piece.clear();
                    // The writer may be gone, done with its last piece.
                    let _ = give_back.send(piece);
                }
                Ok((partial, hasher))
            })?;
        Ok(BlobWriter {
            piece: Vec::with_capacity(CHUNK),
            to: Some(to),
            back,
            worker: Some(worker),
            size: 0,
            blobs,
        })
    }

    /// Hands the full piece to the thread. If the thread has stopped, its error.
    fn send(&mut self) -> io::Result<()> {
        let fresh = self.back.try_recv().unwrap_or_else(|_| Vec::with_capacity(CHUNK));
        let piece = std::mem::replace(&mut self.piece, fresh);
        let sent = self.to.as_ref().map(|to| to.send(piece));
        match sent {
            Some(Ok(())) => Ok(()),
            _ => Err(match self.finish() {
                Err(e) => io::Error::other(e.0),
                Ok(_) => io::Error::other("the blob's writer stopped"),
            }),
        }
    }

    /// Ends the thread once it has everything: what it wrote, and the hash, or its error.
    fn finish(&mut self) -> Result<(Partial, Hasher), Error> {
        drop(self.to.take());
        let worker = self
            .worker
            .take()
            .ok_or_else(|| Error("the blob's writer is already done".into()))?;
        match worker.join() {
            Ok(r) => Ok(r?),
            Err(_) => bad("the blob's writer failed"),
        }
    }

    /// Moves the blob into place (fsync, then rename), returning its digest and size.
    pub fn commit(mut self) -> Result<(Digest, u64), Error> {
        if !self.piece.is_empty() {
            self.send()?;
        }
        let (partial, hasher) = self.finish()?;
        let digest = hasher.finish();
        let to = self.blobs.join(digest.algorithm().name()).join(digest.hex());
        partial.commit(&to)?;
        Ok((digest, self.size))
    }
}

impl Write for BlobWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(CHUNK - self.piece.len());
        self.piece.extend_from_slice(buf.get(..n).unwrap_or_default());
        self.size += n as u64;
        if self.piece.len() == CHUNK {
            self.send()?;
        }
        Ok(n)
    }

    /// Everything given so far goes to the thread; [`BlobWriter::commit`] waits for it.
    fn flush(&mut self) -> io::Result<()> {
        if self.piece.is_empty() {
            return Ok(());
        }
        self.send()
    }
}

impl Drop for BlobWriter {
    /// Uncommitted, the thread ends and the partial file with it.
    fn drop(&mut self) {
        let _ = self.finish();
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
        // One level at a time, inside `root`: a store whose root was removed meanwhile is
        // not made again, with the directories above it.
        let refs = format!("refs/v{REFS_VERSION}");
        let rootfs = format!("rootfs/v{ROOTFS_VERSION}");
        for dir in [
            "blobs",
            "blobs/sha256",
            "blobs/sha384",
            "blobs/sha512",
            "ingest",
            "refs",
            &refs,
            "rootfs",
            &rootfs,
        ] {
            match fs::create_dir(root.join(dir)) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                _ => {}
            }
        }
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
    /// committed only if exactly `size` bytes arrived and they hash to `digest`. A copy
    /// there is kept.
    pub fn ingest(&self, digest: &Digest, size: u64, src: &mut dyn Read) -> Result<PathBuf, Error> {
        self.ingest_as(digest, size, src, false)
    }

    /// [`ingest`](Self::ingest), in place of a stored copy that has changed
    /// ([`Held::Changed`]).
    pub fn ingest_again(&self, digest: &Digest, size: u64, src: &mut dyn Read) -> Result<PathBuf, Error> {
        self.ingest_as(digest, size, src, true)
    }

    fn ingest_as(
        &self,
        digest: &Digest,
        size: u64,
        src: &mut dyn Read,
        replace: bool,
    ) -> Result<PathBuf, Error> {
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
        if replace {
            partial.replace(&path)?;
        } else {
            partial.commit(&path)?;
        }
        Ok(path)
    }

    /// Starts or resumes downloading the `size`-byte blob `digest`. The bytes an earlier
    /// attempt left are hashed again, so the blob is verified whole when it is committed.
    /// Waits while another process downloads the same blob, and gives `None` if that left
    /// it stored.
    /// It is refused if what is left of it would leave the store's filesystem less free
    /// than `limits` keep, and stops once it would as it goes (audit A10).
    pub fn download(&self, digest: &Digest, size: u64, limits: &Limits) -> Result<Option<Download>, Error> {
        self.download_as(digest, size, limits, false)
    }

    /// [`download`](Self::download), in place of a stored copy that has changed
    /// ([`Held::Changed`]).
    pub fn download_again(
        &self,
        digest: &Digest,
        size: u64,
        limits: &Limits,
    ) -> Result<Option<Download>, Error> {
        self.download_as(digest, size, limits, true)
    }

    fn download_as(
        &self,
        digest: &Digest,
        size: u64,
        limits: &Limits,
        replace: bool,
    ) -> Result<Option<Download>, Error> {
        let path =
            self.root
                .join("ingest")
                .join(format!("{}-{}.partial", digest.algorithm().name(), digest.hex()));
        // A download that held the lock before this one may have finished, its file moved
        // into place while locked: only the file still at `path` is this download's to
        // go on with, or to remove.
        let mut file = loop {
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            file.lock()?;
            if still_at(&file, &path)? {
                break file;
            }
        };
        if self.has(digest) && !replace {
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
        let left = size.saturating_sub(offset);
        // With nothing to leave free, a full disk fails the write, as it fails containerd's.
        let free = match limits.keep_free {
            0 => u64::MAX,
            _ => (limits.available)(&ingest)?,
        };
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
            replace,
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
    pub fn tag(
        &self,
        reference: &str,
        manifest: &Descriptor,
        resolved: &Digest,
        contents: &[Digest],
    ) -> Result<(), Error> {
        self.record(reference, manifest, resolved, None, contents, None)
    }

    /// [`tag`](Self::tag), with what `reference` resolved to described as `target` says
    /// (`resolved` its digest), and, for what was pulled, repository `source`.
    pub fn tag_from(
        &self,
        reference: &str,
        manifest: &Descriptor,
        target: &Descriptor,
        contents: &[Digest],
        source: Option<&str>,
    ) -> Result<(), Error> {
        let resolved = target.digest()?;
        self.record(reference, manifest, &resolved, Some(target), contents, source)
    }

    fn record(
        &self,
        reference: &str,
        manifest: &Descriptor,
        resolved: &Digest,
        target: Option<&Descriptor>,
        contents: &[Digest],
        source: Option<&str>,
    ) -> Result<(), Error> {
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
            resolved: Some(resolved.to_string()),
            source: source.map(String::from),
            target: target.cloned(),
        })
        .map_err(|e| Error(e.to_string()))?;
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        partial.write_all(&record)?;
        partial.replace(&self.tag_path(reference))?;
        sync_dir(&self.root.join(format!("refs/v{REFS_VERSION}")))
    }

    /// The descriptor of the manifest `reference` names, if it has been pulled.
    pub fn tagged(&self, reference: &str) -> Result<Option<Descriptor>, Error> {
        Ok(self.tag_record(reference)?.map(|t| t.manifest))
    }

    /// Has `reference` name what `existing` names, as `docker tag` does, replacing what it
    /// named. What it names is durable already, as `existing`'s record is.
    pub fn alias(&self, reference: &str, existing: &str) -> Result<(), Error> {
        // Its record is written through `ingest/`, which a collection empties: none runs
        // meanwhile.
        let _lease = self.lease()?;
        let Some(tag) = self.tag_record(existing)? else {
            return bad(format!("{existing}: no such reference"));
        };
        let record = serde_json::to_vec(&Tag {
            reference: reference.to_string(),
            ..tag
        })
        .map_err(|e| Error(e.to_string()))?;
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        partial.write_all(&record)?;
        partial.replace(&self.tag_path(reference))?;
        sync_dir(&self.root.join(format!("refs/v{REFS_VERSION}")))
    }

    /// Every reference, and what it resolved to. A record that cannot be read is left out.
    pub fn references(&self) -> Result<Vec<(String, Digest)>, Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(format!("refs/v{REFS_VERSION}")))? {
            let Ok(bytes) = fs::read(entry?.path()) else {
                continue;
            };
            let Ok(tag) = serde_json::from_slice::<Tag>(&bytes) else {
                continue;
            };
            if let Ok(id) = Digest::parse(tag.resolved.as_deref().unwrap_or(&tag.manifest.digest)) {
                out.push((tag.reference, id));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Removes `reference`'s record, durably; what it named stays until a collection
    /// finds nothing else names it.
    pub fn untag(&self, reference: &str) -> Result<(), Error> {
        fs::remove_file(self.tag_path(reference))?;
        sync_dir(&self.root.join(format!("refs/v{REFS_VERSION}")))
    }

    /// What `reference` resolved to when it was tagged: an index, or its manifest.
    pub fn resolved(&self, reference: &str) -> Result<Option<Digest>, Error> {
        let Some(tag) = self.tag_record(reference)? else {
            return Ok(None);
        };
        let digest = tag.resolved.unwrap_or(tag.manifest.digest.clone());
        Digest::parse(&digest)
            .map(Some)
            .map_err(|e| Error(format!("{reference}: {e}")))
    }

    /// The images the references name, by what each resolved to, in its digest's order:
    /// each one's references, manifests and what of it is here. A record that cannot be
    /// read is left out.
    pub fn images(&self) -> Result<Vec<Image>, Error> {
        let mut images: Vec<Image> = Vec::new();
        for entry in fs::read_dir(self.root.join(format!("refs/v{REFS_VERSION}")))? {
            let path = entry?.path();
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(tag) = serde_json::from_slice::<Tag>(&bytes) else {
                continue;
            };
            let Ok(id) = Digest::parse(tag.resolved.as_deref().unwrap_or(&tag.manifest.digest)) else {
                continue;
            };
            let tagged_at = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let image = match images.iter_mut().position(|i| i.id == id) {
                Some(at) => {
                    let image = images.get_mut(at).ok_or_else(|| Error("an image gone".into()))?;
                    image.references.push(tag.reference);
                    image
                }
                None => {
                    images.push(self.image(id, tag.reference, &tag.manifest)?);
                    images.last_mut().ok_or_else(|| Error("an image gone".into()))?
                }
            };
            image.tagged_at = image.tagged_at.max(tagged_at);
            if let Some(target) = tag.target {
                image
                    .targets
                    .insert(image.references.last().cloned().unwrap_or_default(), target);
            }
            if let Some(source) = tag.source
                && !image.sources.contains(&source)
            {
                image.sources.push(source);
            }
        }
        for image in &mut images {
            image.references.sort();
            image.sources.sort();
        }
        images.sort_by_key(|i| i.id.to_string());
        Ok(images)
    }

    /// Image `id`, named by `reference`, whose manifest for our platform `ours` describes.
    fn image(&self, id: Digest, reference: String, ours: &Descriptor) -> Result<Image, Error> {
        let mut present: HashSet<Digest> = HashSet::new();
        let mut size_of = |d: &Digest| -> u64 {
            if !present.insert(d.clone()) {
                return 0;
            }
            fs::metadata(self.blob_path(d)).map_or(0, |m| m.len())
        };
        // The index's manifests, checked against its digest, or the one manifest it is.
        let mut index_size = 0;
        let mut descriptors = vec![ours.clone()];
        let mut target = Descriptor {
            platform: None,
            annotations: Default::default(),
            ..ours.clone()
        };
        let mut image_config = None;
        if id.to_string() != ours.digest
            && let Ok(meta) = fs::metadata(self.blob_path(&id))
        {
            let desc = Descriptor {
                media_type: String::new(),
                digest: id.to_string(),
                size: i64::try_from(meta.len()).unwrap_or(i64::MAX),
                platform: None,
                annotations: Default::default(),
            };
            if let Ok(Held::Whole(bytes)) = self.held(&desc, oci::MAX_MANIFEST)
                && let Ok(index) = serde_json::from_slice::<oci::Index>(&bytes)
            {
                index_size = size_of(&id);
                target = Descriptor {
                    media_type: index.media_type.unwrap_or_else(|| oci::media::OCI_INDEX.into()),
                    ..desc
                };
                descriptors = index.manifests;
            }
        }
        let mut manifests = Vec::with_capacity(descriptors.len());
        let mut created = None;
        for desc in descriptors {
            let Ok(digest) = desc.digest() else { continue };
            let attestation = desc
                .annotations
                .get("vnd.docker.reference.type")
                .map(String::as_str)
                == Some("attestation-manifest");
            let mut listed = ImageManifest {
                digest: digest.clone(),
                platform: desc.platform.as_ref().map(platform_string),
                attestation,
                available: false,
                content: 0,
                unpacked: 0,
            };
            // One that cannot be held, for whatever reason, is listed as not here, as
            // dockerd lists a manifest it cannot read (moby image_list.go).
            if let Ok(Held::Whole(bytes)) = self.held(&desc, oci::MAX_MANIFEST) {
                listed.content = size_of(&digest);
                if let Ok(oci::Document::Manifest(manifest)) = oci::parse_document(&bytes, &desc.media_type) {
                    let mut whole = true;
                    for part in std::iter::once(&manifest.config).chain(&manifest.layers) {
                        match part.digest() {
                            Ok(d) if self.has(&d) => listed.content += size_of(&d),
                            _ => whole = false,
                        }
                    }
                    listed.available = whole;
                    if let Ok(Held::Whole(bytes)) = self.held(&manifest.config, oci::MAX_CONFIG)
                        && let Ok(config) = oci::parse_config(&bytes)
                    {
                        if desc.digest == ours.digest {
                            image_config = Some(bytes.clone());
                        }
                        if listed.platform.is_none() && !attestation {
                            listed.platform = Some(platform_string(&oci::Platform {
                                architecture: config.architecture.clone(),
                                os: config.os.clone(),
                                variant: config.variant.clone(),
                                os_features: Vec::new(),
                            }));
                        }
                        let diff_ids: Result<Vec<Digest>, _> =
                            config.rootfs.diff_ids.iter().map(|d| Digest::parse(d)).collect();
                        if let Ok(path) = diff_ids
                            .map_err(|e| Error(e.to_string()))
                            .and_then(|d| self.rootfs_path(&d))
                        {
                            listed.unpacked = fs::metadata(path).map_or(0, |m| m.len());
                        }
                        if desc.digest == ours.digest || created.is_none() {
                            created = config.created.clone().or(created);
                        }
                    }
                }
            }
            manifests.push(listed);
        }
        Ok(Image {
            id,
            references: vec![reference],
            target,
            manifest: ours.digest().map_err(|e| Error(e.to_string()))?,
            config: image_config,
            tagged_at: None,
            sources: Vec::new(),
            targets: BTreeMap::new(),
            created,
            content: index_size + manifests.iter().map(|m| m.content).sum::<u64>(),
            unpacked: manifests.iter().map(|m| m.unpacked).sum(),
            manifests,
        })
    }

    fn tag_record(&self, reference: &str) -> Result<Option<Tag>, Error> {
        let bytes = match fs::read(self.tag_path(reference)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let tag: Tag = serde_json::from_slice(&bytes).map_err(|e| Error(format!("{reference}: {e}")))?;
        if tag.reference != reference {
            return bad(format!("{reference}: its record names {}", tag.reference));
        }
        Ok(Some(tag))
    }

    /// What wrote the root filesystem at `rootfs`, and the version of its rules
    /// ([`Store::rootfs_written`]): `<image>.from`, beside it.
    pub fn rootfs_producer(&self, rootfs: &Path) -> Option<String> {
        fs::read_to_string(from_path(rootfs)).ok()
    }

    /// Where the root filesystem of the layers with `diff_ids` is kept: by their ChainID.
    fn rootfs_path(&self, diff_ids: &[Digest]) -> Result<PathBuf, Error> {
        let chain = oci::chain_id(diff_ids).ok_or_else(|| Error("an image with no layers".into()))?;
        Ok(self.root.join(format!("rootfs/v{ROOTFS_VERSION}")).join(format!(
            "{}-{}.erofs",
            chain.algorithm().name(),
            chain.hex()
        )))
    }

    /// A hold on the store's content for as long as it is kept, by whoever writes content
    /// not yet recorded by a reference, or reads what a run is about to use: a collection
    /// ([`collect`](Self::collect)) never runs while one is held, in any process.
    pub fn lease(&self) -> Result<Lease, Error> {
        let file = self.lease_file()?;
        file.lock_shared()?;
        Ok(Lease { _file: file })
    }

    fn lease_file(&self) -> Result<File, Error> {
        Ok(File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lease"))?)
    }

    /// Removes what no reference needs (audit A13): the blobs its manifest, config and
    /// layers are not, the root filesystems of no reference's layers, records and root
    /// filesystems of older versions, and what `ingest/` holds. The roots are the
    /// references alone; content being written or read for a run is under a
    /// [`lease`](Self::lease), and while any is held nothing is collected: `None`.
    /// Files a running VM holds open stay its own until it closes them. The store stays
    /// held whole until the [`Whole`] returned is dropped, for its caller to collect what
    /// depends on it.
    pub fn collect(&self) -> Result<Option<(Collected, Whole)>, Error> {
        let whole = self.lease_file()?;
        match whole.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let (blobs, rootfs) = self.roots()?;
        let mut collected = Collected::default();
        let mut remove = |path: &Path, count: &mut u64| {
            let meta = fs::symlink_metadata(path);
            if meta.as_ref().is_ok_and(fs::Metadata::is_dir) {
                // A build's stage (`Store::stage`) its process left: its files, then it.
                if let Ok(entries) = fs::read_dir(path) {
                    for e in entries.flatten() {
                        let len = e.metadata().map_or(0, |m| m.len());
                        if fs::remove_file(e.path()).is_ok() {
                            *count += 1;
                            collected.bytes = collected.bytes.saturating_add(len);
                        }
                    }
                }
                let _ = fs::remove_dir(path);
                return;
            }
            let len = meta.map_or(0, |m| m.len());
            if fs::remove_file(path).is_ok() {
                *count += 1;
                collected.bytes = collected.bytes.saturating_add(len);
            }
        };
        for algorithm in ["sha256", "sha384", "sha512"] {
            for entry in fs::read_dir(self.root.join("blobs").join(algorithm))? {
                let path = entry?.path();
                if !blobs.contains(&path) {
                    remove(&path, &mut collected.blobs);
                }
            }
        }
        let current = format!("v{ROOTFS_VERSION}");
        for entry in fs::read_dir(self.root.join("rootfs"))? {
            let entry = entry?;
            if entry.file_name() != current.as_str() {
                let _ = fs::remove_dir_all(entry.path());
                continue;
            }
            for built in fs::read_dir(entry.path())? {
                let path = built?.path();
                if path.extension().is_some_and(|e| e == "erofs") && !rootfs.contains(&path) {
                    remove(&path, &mut collected.rootfs);
                }
                // What wrote an image goes with it.
                if path.extension().is_some_and(|e| e == "from") && !rootfs.contains(&path.with_extension(""))
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        let current = format!("v{REFS_VERSION}");
        for entry in fs::read_dir(self.root.join("refs"))? {
            let entry = entry?;
            if entry.file_name() != current.as_str() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        // Nothing writes there while the store is held whole: what is there was left.
        for entry in fs::read_dir(self.root.join("ingest"))? {
            remove(&entry?.path(), &mut collected.ingest);
        }
        Ok(Some((collected, Whole { _file: whole })))
    }

    /// The blobs and root filesystems the references need: each one's manifest, config
    /// and layers, and the root filesystem of its layers. A record that cannot be read,
    /// or names what is not whole, holds only what it names that is.
    fn roots(&self) -> Result<(HashSet<PathBuf>, HashSet<PathBuf>), Error> {
        let (mut blobs, mut rootfs) = (HashSet::new(), HashSet::new());
        for entry in fs::read_dir(self.root.join(format!("refs/v{REFS_VERSION}")))? {
            // One removed since it was listed (an rmi meanwhile) holds nothing.
            let bytes = match fs::read(entry?.path()) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let Ok(tag) = serde_json::from_slice::<Tag>(&bytes) else {
                continue;
            };
            // What it resolved to, an index, is kept as containerd keeps it, with what it
            // names that is here: the image's platforms (`images --tree`), and the
            // attestations a pull kept.
            if let Some(resolved) = tag.resolved.as_deref().and_then(|d| Digest::parse(d).ok()) {
                blobs.insert(self.blob_path(&resolved));
                if let Ok(index) = fs::read(self.blob_path(&resolved))
                    && let Ok(index) = serde_json::from_slice::<oci::Index>(&index)
                {
                    for desc in &index.manifests {
                        let Ok(Held::Whole(bytes)) = self.held(desc, oci::MAX_MANIFEST) else {
                            continue;
                        };
                        if let Ok(digest) = desc.digest() {
                            blobs.insert(self.blob_path(&digest));
                        }
                        if let Ok(oci::Document::Manifest(m)) = oci::parse_document(&bytes, &desc.media_type)
                        {
                            for part in std::iter::once(&m.config).chain(&m.layers) {
                                if let Ok(d) = part.digest() {
                                    blobs.insert(self.blob_path(&d));
                                }
                            }
                        }
                    }
                }
            }
            let Ok(digest) = tag.manifest.digest() else {
                continue;
            };
            blobs.insert(self.blob_path(&digest));
            let Held::Whole(bytes) = self.held(&tag.manifest, oci::MAX_MANIFEST)? else {
                continue;
            };
            let Ok(oci::Document::Manifest(manifest)) = oci::parse_document(&bytes, &tag.manifest.media_type)
            else {
                continue;
            };
            for desc in std::iter::once(&manifest.config).chain(&manifest.layers) {
                if let Ok(digest) = desc.digest() {
                    blobs.insert(self.blob_path(&digest));
                }
            }
            let Held::Whole(config) = self.held(&manifest.config, oci::MAX_CONFIG)? else {
                continue;
            };
            let Ok(config) = oci::parse_config(&config) else {
                continue;
            };
            let diff_ids: Result<Vec<Digest>, _> =
                config.rootfs.diff_ids.iter().map(|d| Digest::parse(d)).collect();
            if let Ok(path) = diff_ids
                .map_err(|e| Error(e.to_string()))
                .and_then(|d| self.rootfs_path(&d))
            {
                rootfs.insert(path);
            }
        }
        Ok((blobs, rootfs))
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
        match self.held(desc, max)? {
            Held::Missing => Ok(None),
            Held::Changed(why) | Held::Invalid(why) => Err(Error(why)),
            Held::Whole(bytes) => Ok(Some(bytes)),
        }
    }

    /// What the store holds of the small blob `desc` describes, read as
    /// [`content`](Self::content) reads it.
    pub fn held(&self, desc: &Descriptor, max: u64) -> Result<Held, Error> {
        let digest = match desc.digest() {
            Ok(digest) => digest,
            Err(e) => return Ok(Held::Invalid(e.to_string())),
        };
        let size = match desc.size() {
            Ok(size) => size,
            Err(e) => return Ok(Held::Invalid(e.to_string())),
        };
        if size > max {
            return Ok(Held::Invalid(format!(
                "{digest}: {size} bytes is over the {max}-byte limit"
            )));
        }
        let path = self.blob_path(&digest);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Held::Missing),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
        // Past the limit its descriptor is within, it is not what it was stored as.
        let mut hasher = Hasher::new(digest.algorithm());
        hasher.update(&bytes);
        if bytes.len() as u64 > max || hasher.finish() != digest {
            return Ok(Held::Changed(format!(
                "{}: the stored copy has changed",
                path.display()
            )));
        }
        if bytes.len() as u64 != size {
            return Ok(Held::Invalid(format!(
                "{digest}: {} bytes, where its descriptor says {size}",
                bytes.len()
            )));
        }
        Ok(Held::Whole(bytes))
    }

    /// Decompresses a layer's blob into a file under `ingest/`, removed when dropped. Its
    /// media type says whether the compression is sniffed (`oci::layer_compression`). The
    /// whole decompressed stream, bytes after the tar's end included, must match the
    /// DiffID, as containerd's applier checks it (`core/diff/apply/apply.go`), and must
    /// not pass `max` bytes.
    fn unpack(&self, layer: &Layer, bytes: &mut u64, limits: &Limits, room: &mut Room) -> Result<Tar, Error> {
        let how = oci::layer_compression(&layer.media_type)?;
        let blob = self.blob_path(&layer.blob);
        let mut file = File::open(&blob)?;
        let mut head = Vec::with_capacity(8);
        if how == oci::LayerCompression::Sniffed {
            (&mut file).take(8).read_to_end(&mut head)?;
            file.rewind()?;
        }
        let mut src = BufReader::with_capacity(CHUNK, file);
        let check = |hasher: Hasher| {
            let actual = hasher.finish();
            if actual != layer.diff_id {
                return bad(format!(
                    "layer {}: its content hashes to {actual}, not its DiffID {}",
                    layer.blob, layer.diff_id
                ));
            }
            Ok(())
        };
        let how = compression(&head);
        if matches!(how, Compression::None) {
            let mut sink = Sink {
                hasher: Hasher::new(layer.diff_id.algorithm()),
                out: None,
                written: bytes,
                max: limits.bytes,
            };
            io::copy(&mut src, &mut sink)?;
            check(sink.hasher)?;
            return Ok(Tar::Blob(blob));
        }
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        let mut sink = Sink {
            hasher: Hasher::new(layer.diff_id.algorithm()),
            out: Some(Checked {
                out: &mut partial,
                room,
            }),
            written: bytes,
            max: limits.bytes,
        };
        match how {
            Compression::Gzip => {
                io::copy(&mut flate2::bufread::MultiGzDecoder::new(src), &mut sink)?;
            }
            Compression::Zstd => zstd(&mut src, &mut sink)?,
            Compression::None => {}
        }
        check(sink.hasher)?;
        partial.flush()?;
        Ok(Tar::Temp(partial))
    }

    /// A new stage: on the store's file system, so files cloned into it share their
    /// blocks where the context is on the same one.
    pub fn stage(&self) -> Result<Stage, Error> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self
            .root
            .join("ingest")
            .join(format!("{}-stage-{n}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Stage(path))
    }

    /// A new blob, written by the caller.
    pub fn writer(&self) -> Result<BlobWriter, Error> {
        BlobWriter::new(
            Partial::create(&self.root.join("ingest"))?,
            self.root.join("blobs"),
        )
    }

    /// `layers`' archives, each decompressed and checked, together taking no more than
    /// `limits` allow, as [`Store::rootfs`] takes them.
    pub fn unpack_layers(&self, layers: &[Layer], limits: &Limits) -> Result<Vec<Unpacked>, Error> {
        let mut room = Room::new(&self.root.join("ingest"), limits)?;
        let mut bytes = 0u64;
        layers
            .iter()
            .map(|l| Ok(Unpacked(self.unpack(l, &mut bytes, limits, &mut room)?)))
            .collect()
    }

    /// The EROFS root filesystem of `layers`, built on first use: each layer is unpacked
    /// and checked, the layers are stacked (layer.rs), and the tree is written once. It is
    /// kept by ChainID; the unpacked tars go when it is done. Building it takes no more
    /// than `limits` allow (audit A10), and one build at a time goes on in a store,
    /// whichever process asks: a second of the same image finds the first's.
    pub fn rootfs(&self, layers: &[Layer], limits: &Limits) -> Result<PathBuf, Error> {
        let diff_ids: Vec<Digest> = layers.iter().map(|l| l.diff_id.clone()).collect();
        let from = from_path(&self.rootfs_path(&diff_ids)?);
        self.rootfs_by(layers, limits, |room, ingest| {
            // Stacked from the layers: nothing else wrote it, whatever was named before.
            match fs::remove_file(&from) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            let (mut bytes, mut entries, mut metadata) = (0u64, 0u64, 0u64);
            let mut tree = layer::root();
            let mut tars = Vec::with_capacity(layers.len());
            for (i, l) in layers.iter().enumerate() {
                let tar = self.unpack(l, &mut bytes, limits, room)?;
                let source = u32::try_from(i).map_err(|_| Error("too many layers".into()))?;
                let file = File::open(tar.path())?;
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
                // What this layer replaced or whited out goes before the next is read, so
                // the tree holds the image, not its history (audit D11).
                tree.compact();
                tars.push(tar);
            }
            let files = tars
                .iter()
                .map(|t| File::open(t.path()))
                .collect::<io::Result<Vec<_>>>()?;
            let mut partial = Partial::create(ingest)?;
            erofs::write(
                &tree,
                &mut Archives(files),
                &mut Checked {
                    out: &mut partial,
                    room,
                },
            )?;
            Ok(partial)
        })
    }

    /// The root filesystem of `layers`, kept as [`Store::rootfs`] keeps it, written by
    /// `write` when it is not built yet: an image of the tree the layers stack to, which
    /// the caller holds, such as a build's last snapshot (shards_build::stack). `write`
    /// writes within the room the store's limits leave, as `rootfs` does.
    ///
    /// `producer` names what wrote it and the version of its rules, kept beside it in
    /// `<image>.from` before the image itself is in place, so that a crash never leaves it
    /// unnamed: should a producer's rules prove wrong, what they wrote is found and removed,
    /// and nothing else, as REAPI's action salt disowns a set of results.
    pub fn rootfs_written(
        &self,
        layers: &[Layer],
        limits: &Limits,
        producer: &str,
        write: impl FnOnce(&mut dyn Write) -> Result<(), Error>,
    ) -> Result<PathBuf, Error> {
        let diff_ids: Vec<Digest> = layers.iter().map(|l| l.diff_id.clone()).collect();
        let from = from_path(&self.rootfs_path(&diff_ids)?);
        self.rootfs_by(layers, limits, |room, ingest| {
            let mut marker = Partial::create(ingest)?;
            marker.write_all(producer.as_bytes())?;
            marker.replace(&from)?;
            let mut partial = Partial::create(ingest)?;
            write(&mut Checked {
                out: &mut partial,
                room,
            })?;
            Ok(partial)
        })
    }

    /// The root filesystem of `layers` where it is kept, made by `make` unless it is
    /// there, while this process holds the store's lock on building them: `make` writes
    /// it into a file under `ingest`, within `room`.
    fn rootfs_by(
        &self,
        layers: &[Layer],
        limits: &Limits,
        make: impl FnOnce(&mut Room, &Path) -> Result<Partial, Error>,
    ) -> Result<PathBuf, Error> {
        let diff_ids: Vec<Digest> = layers.iter().map(|l| l.diff_id.clone()).collect();
        let path = self.rootfs_path(&diff_ids)?;
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
        let partial = make(&mut room, &ingest)?;
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

/// The room a file system has for what is written to it: past `Limits::keep_free`, the
/// writing stops (audit A10). Free space is looked at as it starts, and again every
/// `LOOK_EVERY` bytes.
#[derive(Debug)]
pub struct Room {
    dir: PathBuf,
    keep_free: u64,
    available: fn(&Path) -> io::Result<u64>,
    since: u64,
}

impl Room {
    pub fn new(dir: &Path, limits: &Limits) -> io::Result<Room> {
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
        // Nothing to leave free: nothing to look at, and a full disk fails the write.
        if self.keep_free == 0 {
            return Ok(());
        }
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

    pub fn wrote(&mut self, n: usize) -> io::Result<()> {
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

/// Where a layer's archive goes as it is checked: hashed, counted against the image's
/// limit, and written to `out` when it is kept apart from its blob.
struct Sink<'a> {
    hasher: Hasher,
    out: Option<Checked<'a>>,
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
        if let Some(out) = &mut self.out {
            out.write_all(buf)?;
        }
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
    decode_zstd(src, out)
}

/// What gzip `src` holds, read as it is decoded, every member of it as Go's gzip reader
/// reads them.
pub fn gunzip<R: BufRead>(src: R) -> impl Read {
    flate2::bufread::MultiGzDecoder::new(src)
}

/// Decodes every zstd frame of `src` to `out`, skipping skippable frames, with each
/// frame's checksum checked and windows no larger than klauspost/compress decodes, as
/// containerd and moby decode zstd.
pub fn decode_zstd(src: &mut dyn BufRead, out: &mut dyn Write) -> Result<(), Error> {
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

/// Where what wrote the root filesystem at `rootfs` is named.
fn from_path(rootfs: &Path) -> PathBuf {
    let mut name = rootfs.as_os_str().to_os_string();
    name.push(".from");
    PathBuf::from(name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::tar::tests::{Member, Writer};

    fn sha256(bytes: &[u8]) -> Digest {
        Digest::from_hash(Algorithm::Sha256, &Sha256::digest(bytes))
    }

    /// A directory of its own, removed when dropped, whether its test passes or panics.
    struct Temp(std::path::PathBuf);

    impl std::ops::Deref for Temp {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for Temp {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(name: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("shards-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// A descriptor of `size` bytes named `digest`.
    /// A name given while collections run is never lost: its record goes through
    /// `ingest/`, which a collection empties, so the alias holds a lease.
    #[test]
    fn names_given_during_collections_are_kept() {
        let root = temp("alias-collect");
        let store = Store::open(&root).unwrap();
        let blob = b"manifest".to_vec();
        let d = sha256(&blob);
        store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
        store
            .tag("docker.io/library/a:1", &described(&d, blob.len()), &d, &[])
            .unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = store.collect().unwrap();
                }
            });
            for n in 0..40 {
                let name = format!("docker.io/library/b:{n}");
                let aliased = store.alias(&name, "docker.io/library/a:1");
                if aliased.is_err() {
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                aliased.unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(store.references().unwrap().len(), 41);
    }

    /// Images are listed by what their references resolved to, newest first: each one's
    /// references, its index's manifests (the attestation told apart, the one not here
    /// not whole), and the bytes of it here, the index's among them.
    #[test]
    fn images_are_listed_by_what_their_references_resolved_to() {
        let root = temp("images");
        let store = Store::open(&root).unwrap();
        let put = |blob: &[u8]| {
            let d = sha256(blob);
            store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
            d
        };
        let layer = b"layer bytes".to_vec();
        let config = |created: &str| {
            format!(
                r#"{{"architecture":"arm64","os":"linux","created":"{created}","rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
                sha256(&layer)
            )
            .into_bytes()
        };
        let manifest = |config: &[u8]| {
            format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
                sha256(config),
                config.len(),
                sha256(&layer),
                layer.len()
            )
            .into_bytes()
        };
        put(&layer);
        let (old_config, new_config) = (config("2024-01-02T03:04:05Z"), config("2025-01-02T03:04:05Z"));
        put(&old_config);
        put(&new_config);
        let (old, new) = (manifest(&old_config), manifest(&new_config));
        let (old_digest, new_digest) = (put(&old), put(&new));
        let absent = format!("sha256:{}", "1".repeat(64));
        let attestation = format!("sha256:{}", "2".repeat(64));
        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{old_digest}","size":{},"platform":{{"architecture":"arm64","os":"linux","variant":"v8"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{absent}","size":9,"platform":{{"architecture":"amd64","os":"linux"}}}},{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{attestation}","size":9,"annotations":{{"vnd.docker.reference.type":"attestation-manifest"}},"platform":{{"architecture":"unknown","os":"unknown"}}}}]}}"#,
            old.len()
        )
        .into_bytes();
        let index_digest = put(&index);
        let desc = |d: &Digest, len: usize| Descriptor {
            platform: None,
            ..described(d, len)
        };
        store
            .tag(
                "docker.io/library/a:2",
                &desc(&old_digest, old.len()),
                &index_digest,
                &[],
            )
            .unwrap();
        store
            .tag(
                "docker.io/library/a:1",
                &desc(&old_digest, old.len()),
                &index_digest,
                &[],
            )
            .unwrap();
        store
            .tag(
                "docker.io/library/b:1",
                &desc(&new_digest, new.len()),
                &new_digest,
                &[],
            )
            .unwrap();
        let images = store.images().unwrap();
        let by_id = |d: &Digest| images.iter().find(|i| i.id == *d).unwrap();
        assert_eq!(images.len(), 2);
        let a = by_id(&index_digest);
        assert_eq!(a.references, ["docker.io/library/a:1", "docker.io/library/a:2"]);
        assert_eq!(a.created.as_deref(), Some("2024-01-02T03:04:05Z"));
        let here = (index.len() + old.len() + old_config.len() + layer.len()) as u64;
        assert_eq!((a.content, a.unpacked), (here, 0));
        let shown: Vec<(Option<&str>, bool, bool, u64)> = a
            .manifests
            .iter()
            .map(|m| (m.platform.as_deref(), m.attestation, m.available, m.content))
            .collect();
        assert_eq!(
            shown,
            [
                (Some("linux/arm64/v8"), false, true, here - index.len() as u64),
                (Some("linux/amd64"), false, false, 0),
                (Some("unknown/unknown"), true, false, 0),
            ]
        );
        let b = by_id(&new_digest);
        assert_eq!(
            (b.references.as_slice(), b.content),
            (
                &["docker.io/library/b:1".to_string()][..],
                (new.len() + new_config.len() + layer.len()) as u64
            )
        );
        assert_eq!(b.manifests.len(), 1);
        assert_eq!(
            b.manifests[0].platform.as_deref(),
            Some("linux/arm64"),
            "from its config"
        );
        // A layer the manifest names that has gone: the manifest is not whole.
        fs::remove_file(store.blob_path(&sha256(&layer))).unwrap();
        let images = store.images().unwrap();
        assert!(!images.iter().find(|i| i.id == new_digest).unwrap().manifests[0].available);
    }

    /// A record whose descriptor nothing can be held by, its size not its blob's,
    /// breaks neither the listing nor a collection: the listing shows its manifest as not
    /// here, as dockerd lists one it cannot read, and the rest as ever; a collection runs,
    /// and keeps what the other image needs.
    #[test]
    fn a_record_nothing_can_be_held_by_breaks_nothing_else() {
        let root = temp("invalid-record");
        let store = Store::open(&root).unwrap();
        let put = |blob: &[u8]| {
            let d = sha256(blob);
            store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
            d
        };
        let layer = b"layer bytes".to_vec();
        put(&layer);
        let manifest = |arch: &str| {
            let config = format!(
                r#"{{"architecture":"{arch}","os":"linux","rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
                sha256(&layer)
            )
            .into_bytes();
            put(&config);
            format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
                sha256(&config),
                config.len(),
                sha256(&layer),
                layer.len()
            )
            .into_bytes()
        };
        let (good, bad) = (manifest("arm64"), manifest("amd64"));
        let (good_digest, bad_digest) = (put(&good), put(&bad));
        store
            .tag(
                "docker.io/library/good:1",
                &described(&good_digest, good.len()),
                &good_digest,
                &[],
            )
            .unwrap();
        store
            .tag(
                "docker.io/library/bad:1",
                &described(&bad_digest, bad.len() + 1),
                &bad_digest,
                &[],
            )
            .unwrap();
        let images = store.images().unwrap();
        assert_eq!(images.len(), 2);
        let listed = |d: &Digest| images.iter().find(|i| i.id == *d).unwrap();
        assert!(listed(&good_digest).manifests.iter().all(|m| m.available));
        assert!(listed(&bad_digest).manifests.iter().all(|m| !m.available));
        assert!(store.collect().is_ok());
        assert!(store.has(&good_digest) && store.has(&sha256(&layer)));
    }

    /// A download that opened the partial file before another moved it into place, and
    /// then had the lock, finds it no longer at its path: it neither goes on with the
    /// stored blob nor removes another's partial there, but starts over with a file of
    /// its own (download_as).
    #[cfg(unix)]
    #[test]
    fn a_download_waiting_on_a_finished_one_finds_its_file_moved() {
        let root = temp("download-moved");
        let store = Store::open(&root).unwrap();
        let blob = b"the blob".to_vec();
        let digest = sha256(&blob);
        let limits = Limits::none();
        let path = root
            .join("ingest")
            .join(format!("sha256-{}.partial", digest.hex()));
        let mut first = store
            .download(&digest, blob.len() as u64, &limits)
            .unwrap()
            .unwrap();
        let waiting = File::options().read(true).write(true).open(&path).unwrap();
        first.write(&blob).unwrap();
        first.commit().unwrap();
        waiting.lock().unwrap();
        assert!(!still_at(&waiting, &path).unwrap());
        // A third download's partial at the path is not the one waited on either.
        let third = File::create(&path).unwrap();
        assert!(!still_at(&waiting, &path).unwrap());
        assert!(still_at(&third, &path).unwrap());
    }

    fn described(digest: &Digest, size: usize) -> Descriptor {
        Descriptor {
            media_type: oci::media::OCI_MANIFEST.into(),
            digest: digest.to_string(),
            size: i64::try_from(size).unwrap(),
            platform: None,
            annotations: Default::default(),
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
            assert_eq!(fs::read(out.path()).unwrap(), tar);
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
        assert_eq!(fs::read(out.path()).unwrap(), gz);
        drop(out);
        // A plain blob is read where it is, and still checked: one changed on disk after
        // it was stored is refused.
        let plain = Layer {
            blob: sha256(&tar),
            media_type: oci::media::OCI_LAYER.into(),
            diff_id: diff_id.clone(),
        };
        assert!(unpack_within(&store, &plain, 1 << 20).is_ok());
        let path = store.blob_path(&plain.blob);
        let mut perms = fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&path, perms).unwrap();
        let mut changed = tar.clone();
        changed[600] ^= 1;
        fs::write(&path, &changed).unwrap();
        match unpack_within(&store, &plain, 1 << 20) {
            Err(e) => assert!(e.to_string().contains("not its DiffID"), "{e}"),
            Ok(_) => panic!("a changed blob was taken"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// A blob written in writes of any size, across its thread's pieces, is stored under
    /// the digest of exactly its bytes; one dropped uncommitted leaves nothing behind.
    #[test]
    fn blobs_written_in_any_pieces_are_stored_whole() {
        let root = temp("blob-writer");
        let store = Store::open(&root).unwrap();
        let blob: Vec<u8> = (0..CHUNK * 3 + 777).map(|i| (i % 253) as u8).collect();
        for step in [1, 4096, CHUNK - 1, CHUNK, CHUNK + 1, blob.len()] {
            let mut w = store.writer().unwrap();
            for c in blob.chunks(step) {
                w.write_all(c).unwrap();
            }
            let (digest, size) = w.commit().unwrap();
            assert_eq!(
                (digest.clone(), size),
                (sha256(&blob), blob.len() as u64),
                "writes of {step}"
            );
            assert_eq!(fs::read(store.blob_path(&digest)).unwrap(), blob);
        }
        let empty = store.writer().unwrap().commit().unwrap();
        assert_eq!(empty, (sha256(b""), 0));
        let mut w = store.writer().unwrap();
        w.write_all(&blob).unwrap();
        drop(w);
        assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0);
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
        assert_eq!(store.resolved(name).unwrap(), None);
        let a_digest = Digest::parse(&a.digest).unwrap();
        store.tag(name, &a, &a_digest, &[]).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(a.clone()));
        assert_eq!(store.resolved(name).unwrap(), Some(a_digest.clone()));
        // Through an index: the manifest chosen, and the index the reference named.
        let index = sha256(b"index");
        store.tag(name, &b, &index, &[]).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(b.clone()));
        assert_eq!(store.resolved(name).unwrap(), Some(index));
        // A record from before `resolved` was kept resolves to its manifest.
        let path = store.tag_path(name);
        let record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut old = record.clone();
        old.as_object_mut().unwrap().remove("resolved");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(b.clone()));
        assert_eq!(
            store.resolved(name).unwrap(),
            Some(Digest::parse(&b.digest).unwrap())
        );
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
        let resolved = Digest::parse(&manifest.digest).unwrap();
        assert!(store.tag(name, &manifest, &resolved, &[sha512]).is_err());
        assert_eq!(store.tagged(name).unwrap(), None, "nothing recorded");
        fs::remove_dir_all(root.join(format!("rootfs/v{ROOTFS_VERSION}"))).unwrap();
        assert!(store.tag(name, &manifest, &resolved, &[]).is_err());
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
        // truncated, grown, or grown past the limit its descriptor is within. Each is a
        // changed copy, which a pull fetches again.
        let path = store.blob_path(&d);
        let same_length = String::from_utf8(blob.to_vec()).unwrap().replace('7', "9");
        let cases = [
            same_length.into_bytes(),
            blob[..blob.len() - 1].to_vec(),
            [&blob[..], b" "].concat(),
            vec![b' '; 2048],
        ];
        for bytes in cases {
            fs::write(&path, &bytes).unwrap();
            let e = store.content(&desc, 1024).unwrap_err().to_string();
            assert!(
                e.contains("the stored copy has changed"),
                "{} bytes: {e}",
                bytes.len()
            );
            assert!(
                matches!(store.held(&desc, 1024).unwrap(), Held::Changed(_)),
                "{} bytes",
                bytes.len()
            );
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
        assert_eq!(fs::read(out.path()).unwrap(), tar);
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
    fn unpack_within(store: &Store, layer: &Layer, max: u64) -> Result<Tar, Error> {
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

    /// An image of `tars`, one layer each, stored and tagged `reference` as a pull leaves
    /// it, its root filesystem built; its manifest's digest, its blobs and its root
    /// filesystem.
    fn tagged(store: &Store, reference: &str, tars: &[&[u8]]) -> (Vec<PathBuf>, PathBuf) {
        let layers: Vec<Layer> = tars.iter().map(|t| stored_layer(store, t)).collect();
        let diff_ids: Vec<String> = layers.iter().map(|l| l.diff_id.to_string()).collect();
        let config = serde_json::json!({
            "architecture": "arm64", "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": diff_ids},
        })
        .to_string()
        .into_bytes();
        let config_digest = sha256(&config);
        store
            .ingest(&config_digest, config.len() as u64, &mut &config[..])
            .unwrap();
        let layer_descs: Vec<serde_json::Value> = layers
            .iter()
            .map(|l| {
                let size = fs::metadata(store.blob_path(&l.blob)).unwrap().len();
                serde_json::json!({"mediaType": l.media_type, "digest": l.blob.to_string(), "size": size})
            })
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": oci::media::OCI_MANIFEST,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": config_digest.to_string(), "size": config.len()},
            "layers": layer_descs,
        })
        .to_string()
        .into_bytes();
        let manifest_digest = sha256(&manifest);
        store
            .ingest(&manifest_digest, manifest.len() as u64, &mut &manifest[..])
            .unwrap();
        let rootfs = store.rootfs(&layers, &Limits::none()).unwrap();
        let mut contents = vec![manifest_digest.clone(), config_digest.clone()];
        contents.extend(layers.iter().map(|l| l.blob.clone()));
        store
            .tag(
                reference,
                &described(&manifest_digest, manifest.len()),
                &manifest_digest,
                &contents,
            )
            .unwrap();
        (contents.iter().map(|d| store.blob_path(d)).collect(), rootfs)
    }

    fn tar_of(name: &[u8], data: &[u8]) -> Vec<u8> {
        Writer::default()
            .member(Member {
                name,
                data,
                mode: 0o644,
                ..Member::default()
            })
            .finish()
    }

    /// A collection keeps what the references need, their manifests, configs, layers and
    /// root filesystems, shared ones once; removes the rest, blobs, root filesystems,
    /// older versions' directories and what `ingest/` holds; and does nothing while any
    /// process holds the store's lease (audit A13).
    #[test]
    fn a_collection_keeps_what_references_need_and_nothing_else() {
        let root = temp("collect");
        let store = Store::open(&root).unwrap();
        let (a, b, c) = (tar_of(b"a", b"a"), tar_of(b"b", b"b"), tar_of(b"c", b"c"));
        // What `one` named before it was tagged anew.
        let (old_blobs, old_rootfs) = tagged(&store, "one:v1", &[&c]);
        let (first_blobs, first_rootfs) = tagged(&store, "one:v1", &[&a, &b]);
        let (second_blobs, second_rootfs) = tagged(&store, "two:v1", &[&a, &c]);
        let orphan = b"nobody's";
        let orphan_digest = sha256(orphan);
        store
            .ingest(&orphan_digest, orphan.len() as u64, &mut &orphan[..])
            .unwrap();
        fs::write(root.join("ingest/left-behind"), b"x").unwrap();
        fs::create_dir_all(root.join("rootfs/v0")).unwrap();
        fs::write(root.join("rootfs/v0/old.erofs"), b"x").unwrap();
        fs::create_dir_all(root.join("refs/v0")).unwrap();

        let lease = store.lease().unwrap();
        assert!(store.collect().unwrap().is_none(), "collected under a lease");
        assert!(store.blob_path(&orphan_digest).is_file());
        drop(lease);
        let (collected, whole) = store.collect().unwrap().unwrap();
        // Held whole until let go: a lease waits, and another collection gets nothing.
        assert!(store.collect().unwrap().is_none());
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = {
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                let _lease = Store::open(&root).unwrap().lease().unwrap();
                tx.send(()).unwrap();
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(),
            "leased while held whole"
        );
        drop(whole);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        waiting.join().unwrap();

        let kept: Vec<&PathBuf> = first_blobs.iter().chain(&second_blobs).collect();
        for path in &kept {
            assert!(path.is_file(), "{} collected", path.display());
        }
        assert!(first_rootfs.is_file() && second_rootfs.is_file());
        // The old manifest and config of `one` go; its layer `c` is `two`'s too.
        let gone: Vec<&PathBuf> = old_blobs.iter().filter(|p| !kept.contains(p)).collect();
        assert_eq!(gone.len(), 2, "{gone:?}");
        for path in &gone {
            assert!(!path.exists(), "{} kept", path.display());
        }
        assert!(!old_rootfs.exists(), "an old root filesystem kept");
        assert!(!store.blob_path(&orphan_digest).exists());
        assert!(!root.join("rootfs/v0").exists() && !root.join("refs/v0").exists());
        assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        assert_eq!(
            (collected.blobs, collected.rootfs, collected.ingest),
            (3, 1, 1),
            "{collected:?}"
        );
        // What is kept is whole: the images are found again as they were.
        assert!(store.tagged("one:v1").unwrap().is_some() && store.tagged("two:v1").unwrap().is_some());
        assert_eq!(
            store.collect().unwrap().map(|(c, _)| c),
            Some(Collected::default())
        );
        let _ = fs::remove_dir_all(&root);
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

    /// With nothing to leave free, the default, a build and a download never look at the
    /// room their filesystem has, however little it says it has: as containerd's
    /// unpacking and BuildKit's builds, they write until done or until the disk fails the
    /// write.
    #[test]
    fn nothing_kept_free_never_looks_at_the_room() {
        let root = temp("no-room-kept");
        let store = Store::open(&root).unwrap();
        let blob = vec![9u8; 96 << 20];
        let none = Limits {
            available: |_| Err(io::Error::other("looked at the room")),
            ..Limits::none()
        };
        let layer = stored_layer(
            &store,
            &Writer::default()
                .member(Member {
                    name: b"f",
                    data: &blob,
                    ..Member::default()
                })
                .finish(),
        );
        store.rootfs(std::slice::from_ref(&layer), &none).unwrap();
        let d = sha256(&blob);
        let mut w = store.download(&d, blob.len() as u64, &none).unwrap().unwrap();
        w.write(&blob).unwrap();
        w.commit().unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// An image written by a producer is named beside it as that producer, before it is
    /// in place; one stacked from the layers is named by nothing, whatever was named before;
    /// and what names an image goes when a collection takes the image.
    #[test]
    fn what_wrote_an_image_is_kept_beside_it() {
        let root = temp("producer");
        let store = Store::open(&root).unwrap();
        let tar = Writer::default()
            .member(Member {
                name: b"f",
                data: b"x",
                ..Member::default()
            })
            .finish();
        let layer = stored_layer(&store, &tar);
        let layers = std::slice::from_ref(&layer);
        let written = store
            .rootfs_written(layers, &Limits::none(), "snapshot 1", |out| {
                let mut tree = layer::root();
                layer::apply(&mut tree, 0, io::Cursor::new(&tar), &mut |_| Ok(())).unwrap();
                erofs::write(
                    &tree,
                    &mut layer::Archives(vec![io::Cursor::new(tar.clone())]),
                    out,
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(store.rootfs_producer(&written).as_deref(), Some("snapshot 1"));
        fs::remove_file(&written).unwrap();
        let stacked = store.rootfs(layers, &Limits::none()).unwrap();
        assert_eq!(stacked, written);
        assert_eq!(
            store.rootfs_producer(&stacked),
            None,
            "stacked, so named by nothing"
        );
        store
            .rootfs_written(layers, &Limits::none(), "snapshot 1", |_| Ok(()))
            .unwrap();
        fs::remove_file(&stacked).unwrap();
        fs::write(from_path(&stacked), b"snapshot 1").unwrap();
        store.collect().unwrap();
        assert!(!from_path(&stacked).exists(), "named with no image, and kept");
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
        // A build that keeps room free looks at it as it starts: the number of looks is the
        // number of builds.
        let counting = Limits {
            keep_free: 1,
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
