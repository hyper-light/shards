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
use crate::oci;
use crate::reference::{Algorithm, Digest};
use crate::{Error, bad};

/// Bumped whenever the EROFS writer's output changes, so older root filesystems are rebuilt.
const ROOTFS_VERSION: u32 = 1;
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

/// What `refs/` records for a reference.
#[derive(serde::Serialize, serde::Deserialize)]
struct Tag {
    reference: String,
    manifest: String,
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
        for dir in ["blobs/sha256", "blobs/sha384", "blobs/sha512", "ingest", "refs"] {
            fs::create_dir_all(root.join(dir))?;
        }
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
    pub fn download(&self, digest: &Digest, size: u64) -> Result<Option<Download>, Error> {
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
        let mut download = Download {
            path,
            file: BufWriter::with_capacity(CHUNK, file),
            hasher,
            offset,
            digest: digest.clone(),
            size,
            target: self.blob_path(digest),
        };
        // More than the blob holds can only be wrong.
        if offset > size {
            download.restart()?;
        }
        Ok(Some(download))
    }

    /// Records that `reference` names the manifest `manifest`, replacing what it named.
    pub fn tag(&self, reference: &str, manifest: &Digest) -> Result<(), Error> {
        let record = serde_json::to_vec(&Tag {
            reference: reference.to_string(),
            manifest: manifest.to_string(),
        })
        .map_err(|e| Error(e.to_string()))?;
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        partial.write_all(&record)?;
        partial.replace(&self.tag_path(reference))
    }

    /// The manifest `reference` names, if it has been pulled.
    pub fn tagged(&self, reference: &str) -> Result<Option<Digest>, Error> {
        let bytes = match fs::read(self.tag_path(reference)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let tag: Tag = serde_json::from_slice(&bytes).map_err(|e| Error(format!("{reference}: {e}")))?;
        if tag.reference != reference {
            return bad(format!("{reference}: its record names {}", tag.reference));
        }
        Digest::parse(&tag.manifest).map(Some)
    }

    /// References can be long and hold `/` and `:`, so records are named by their hash.
    fn tag_path(&self, reference: &str) -> PathBuf {
        let name = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(reference.as_bytes()));
        self.root.join("refs").join(name.hex())
    }

    /// A small blob's bytes, at most `max`, checked against its digest again.
    pub fn read(&self, digest: &Digest, max: u64) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        File::open(self.blob_path(digest))?
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return bad(format!("{digest}: over {max} bytes"));
        }
        let mut hasher = Hasher::new(digest.algorithm());
        hasher.update(&bytes);
        if hasher.finish() != *digest {
            return bad(format!("{digest}: the stored bytes changed"));
        }
        Ok(bytes)
    }

    /// Decompresses a layer's blob into a file under `ingest/`, removed when dropped. Its
    /// media type says whether the compression is sniffed (`oci::layer_compression`). The
    /// whole decompressed stream, bytes after the tar's end included, must match the
    /// DiffID, as containerd's applier checks it (`core/diff/apply/apply.go`), and must
    /// not pass `max` bytes.
    fn unpack(&self, layer: &Layer, max: u64) -> Result<Partial, Error> {
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
            out: &mut partial,
            written: 0,
            max,
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
    /// kept by ChainID; the unpacked tars go when it is done.
    pub fn rootfs(&self, layers: &[Layer], max: u64) -> Result<PathBuf, Error> {
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
        let mut tree = layer::root();
        let mut tars = Vec::with_capacity(layers.len());
        for (i, l) in layers.iter().enumerate() {
            let tar = self.unpack(l, max)?;
            let source = u32::try_from(i).map_err(|_| Error("too many layers".into()))?;
            let file = File::open(&tar.path)?;
            layer::apply(&mut tree, source, BufReader::with_capacity(CHUNK, file))?;
            tars.push(tar);
        }
        let files = tars
            .iter()
            .map(|t| File::open(&t.path))
            .collect::<io::Result<Vec<_>>>()?;
        let mut out = Partial::create(&self.root.join("ingest"))?;
        erofs::write(&tree, &mut Archives(files), &mut out)?;
        out.commit(&path)?;
        Ok(path)
    }
}

/// Where decompressed layer bytes go: hashed, counted against the cap, and written.
struct Sink<'a> {
    hasher: Hasher,
    out: &'a mut Partial,
    written: u64,
    max: u64,
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written = self.written.saturating_add(buf.len() as u64);
        if self.written > self.max {
            return Err(io::Error::other(format!(
                "a layer decompresses to more than {} bytes",
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

    #[test]
    fn blobs_are_committed_only_when_they_match() {
        let root = temp("ingest");
        assert!(Store::open(&root.join("missing")).is_err());
        let store = Store::open(&root).unwrap();
        let blob = b"hello, registry";
        let d = sha256(blob);
        let path = store.ingest(&d, blob.len() as u64, &mut &blob[..]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), blob);
        assert_eq!(store.read(&d, 100).unwrap(), blob);
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
            let out = store.unpack(&layer, 1 << 20).unwrap();
            assert_eq!(fs::read(&out.path).unwrap(), tar);
            drop(out);
            // A wrong DiffID, or a cap below the size, and nothing is kept.
            let wrong = Layer {
                diff_id: sha256(b"x"),
                ..layer.clone()
            };
            assert!(store.unpack(&wrong, 1 << 20).is_err());
            assert!(store.unpack(&layer, 100).is_err());
            assert_eq!(fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        }
        // OCI's plain tar type is read as it is, whatever its bytes look like.
        let raw = |diff_id: Digest| Layer {
            blob: sha256(&gz),
            media_type: oci::media::OCI_LAYER.into(),
            diff_id,
        };
        assert!(store.unpack(&raw(diff_id.clone()), 1 << 20).is_err());
        let out = store.unpack(&raw(sha256(&gz)), 1 << 20).unwrap();
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
        let mut first = store.download(&d, size).unwrap().unwrap();
        first.write(&blob[..40_000]).unwrap();
        drop(first);
        let mut second = store.download(&d, size).unwrap().unwrap();
        assert_eq!(second.offset(), 40_000, "the first attempt's bytes are kept");
        assert!(
            second.write(&vec![0u8; 60_001]).is_err(),
            "more than the blob holds"
        );
        second.write(&blob[40_000..]).unwrap();
        let path = second.commit().unwrap();
        assert_eq!(fs::read(path).unwrap(), blob);
        assert!(store.download(&d, size).unwrap().is_none(), "already stored");
        // Wrong bytes are thrown away, so the next attempt starts clean.
        let other = sha256(b"other");
        let mut wrong = store.download(&other, 5).unwrap().unwrap();
        wrong.write(b"wrong").unwrap();
        assert!(wrong.commit().is_err());
        assert_eq!(store.download(&other, 5).unwrap().unwrap().offset(), 0);
        let mut again = store.download(&other, 5).unwrap().unwrap();
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
        let mut holder = store.download(&d, blob.len() as u64).unwrap().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (store2, d2, n) = (Store::open(&root).unwrap(), d.clone(), blob.len() as u64);
        let waiter = std::thread::spawn(move || {
            let _ = tx.send(store2.download(&d2, n).map(|d| d.is_none()));
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
        let (a, b) = (sha256(b"a"), sha256(b"b"));
        let name = "docker.io/library/alpine:latest";
        assert_eq!(store.tagged(name).unwrap(), None);
        store.tag(name, &a).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(a));
        store.tag(name, &b).unwrap();
        assert_eq!(store.tagged(name).unwrap(), Some(b));
        assert_eq!(store.tagged("docker.io/library/alpine:3").unwrap(), None);
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
        let out = store.unpack(&layer, 1 << 20).unwrap();
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
        assert!(store.unpack(&layer, 1 << 20).is_err());
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
        let path = store.rootfs(std::slice::from_ref(&layer), 1 << 20).unwrap();
        let image = fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(image[1024..1028].try_into().unwrap()),
            0xE0F5_E1E2
        );
        let again = store.rootfs(&[layer], 1 << 20).unwrap();
        assert_eq!(again, path);
        assert_eq!(
            fs::read_dir(root.join("ingest")).unwrap().count(),
            0,
            "unpacked tars are gone"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
