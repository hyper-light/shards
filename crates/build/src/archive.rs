//! ADD's archives, as BuildKit unpacks a local one (dockerfile/1.27.1's
//! solver/llbsolver/file/unpack.go, over moby/go-archive's compression.DecompressStream
//! and chrootarchive.Untar): a regular file that decompresses to a readable tar is unpacked
//! into the destination, its paths resolved as if the destination were the root, so
//! nothing in it, `../` names and absolute symlinks included, reaches past it.
//!
//! Where moby runs `xz` and `unpigz` as programs, and fails on a host without them,
//! shards decodes gzip, bzip2, xz and zstd in-process (PM M77).

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc;

use shards_dockerfile::go;
use shards_image::erofs::{DataRef, Kind, Source};
use shards_image::store::{Limits, Room};
use shards_image::tar::{self, Type};

use crate::Error;
use crate::copy::{self, Chown, User};
use crate::data::Sources;
use crate::vfs::{self, Errno, Fs, PathError, Place};

/// moby's ImpliedDirectoryMode.
const IMPLIED_DIR_MODE: u32 = 0o755;

/// How much of an archive's stream is decompressed to tell whether it is one: its first
/// header, with the largest PAX header Go reads (1 MiB) before it.
const DETECT: usize = 2 << 20;

/// What the archives a build's ADDs unpack may take, together: the limits an image pull
/// holds to (audit A10), so a small archive that decompresses without end, or holds
/// millions of entries, stops the build before it fills the disk or the memory.
#[derive(Debug)]
pub struct Budget {
    limits: Limits,
    /// Decompressed: counted where the archive is decompressed.
    bytes: u64,
    /// Entries and their metadata: counted where they are unpacked.
    held: Held,
}

/// What the entries the build's ADDs unpacked hold.
#[derive(Debug, Default)]
struct Held {
    entries: u64,
    metadata: u64,
}

impl Budget {
    pub fn new(limits: Limits) -> Budget {
        Budget {
            limits,
            bytes: 0,
            held: Held::default(),
        }
    }
}

impl Held {
    fn entry(&mut self, limits: &Limits, e: &tar::Entry) -> Result<(), Error> {
        self.entries += 1;
        let held = e
            .xattrs
            .iter()
            .fold(e.path.len() + e.link.len(), |n, (k, v)| n + k.len() + v.len());
        self.metadata = self.metadata.saturating_add(held as u64);
        if self.entries > limits.entries {
            return Err(Error(format!(
                "the archives ADD unpacks hold more than {} entries (SHARDS_MAX_IMAGE_ENTRIES)",
                limits.entries
            )));
        }
        if self.metadata > limits.metadata {
            return Err(Error(format!(
                "the archives ADD unpacks have names, links and xattrs past {} bytes (SHARDS_MAX_IMAGE_METADATA)",
                limits.metadata
            )));
        }
        Ok(())
    }
}

/// What unpacking needs beside the snapshots: where file bytes are read and decompressed
/// archives kept, and the build's budget.
#[derive(Debug)]
pub struct Unpack<'a> {
    pub sources: &'a mut Sources,
    pub stage: &'a Path,
    pub budget: &'a mut Budget,
}

/// How much of a decompressed archive goes over to the unpacker at once, and how many such
/// pieces may wait for it: what the two hold between them.
const PIECE: usize = 256 << 10;
const PIECES: usize = 4;

/// Where an archive is decompressed to: pieces sent to the unpacker as they fill, the
/// bytes held to the build's budget.
struct Pieces<'a> {
    to: mpsc::SyncSender<Vec<u8>>,
    piece: Vec<u8>,
    bytes: &'a mut u64,
    limit: u64,
}

impl Pieces<'_> {
    fn send(&mut self) -> io::Result<()> {
        let piece = std::mem::replace(&mut self.piece, Vec::with_capacity(PIECE));
        self.to
            .send(piece)
            .map_err(|_| io::Error::other("the archive's unpacker stopped"))
    }
}

impl Write for Pieces<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // What fits in this piece is taken, and counted; the rest comes again.
        let n = buf.len().min(PIECE - self.piece.len());
        *self.bytes = self.bytes.saturating_add(n as u64);
        if *self.bytes > self.limit {
            return Err(io::Error::other(format!(
                "the archives ADD unpacks decompress to more than {} bytes (SHARDS_MAX_IMAGE_BYTES)",
                self.limit
            )));
        }
        self.piece.extend_from_slice(buf.get(..n).unwrap_or_default());
        if self.piece.len() == PIECE {
            self.send()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.piece.is_empty() {
            return Ok(());
        }
        self.send()
    }
}

/// The decompressed archive, as the unpacker reads it: the pieces, in order, until the
/// decompressor is done.
struct Stream {
    from: mpsc::Receiver<Vec<u8>>,
    piece: Vec<u8>,
    at: usize,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.piece.len() {
            match self.from.recv() {
                Ok(piece) => {
                    self.piece = piece;
                    self.at = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let rest = self.piece.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.at += n;
        Ok(n)
    }
}

impl Stream {
    /// Reads what is left, so the decompressor runs to its end and says whether the
    /// whole stream was sound, as it did when it ran before anything was unpacked.
    fn drain(&mut self) {
        while self.from.recv().is_ok() {}
    }
}

/// The contents of an archive's regular files, written to the stage as the archive is
/// unpacked, held to the room its file system has: the one thing of the archive kept.
struct Contents {
    out: BufWriter<File>,
    room: Room,
    at: u64,
}

impl Write for Contents {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.room.wrote(buf.len())?;
        self.out.write_all(buf)?;
        self.at += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The compressions moby's Detect knows, in the order it tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Bzip2,
    Gzip,
    Xz,
    Zstd,
}

/// compression.Detect, over the first bytes of a stream.
fn detect(head: &[u8]) -> Compression {
    if head.starts_with(&[0x42, 0x5A, 0x68]) {
        Compression::Bzip2
    } else if head.starts_with(&[0x1F, 0x8B, 0x08]) {
        Compression::Gzip
    } else if head.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        Compression::Xz
    } else if head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD])
        || (head.len() >= 8
            && head
                .get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .is_some_and(|b| u32::from_le_bytes(b) & 0xFFFF_FFF0 == 0x184D_2A50))
    {
        Compression::Zstd
    } else {
        Compression::None
    }
}

/// A file's bytes in a snapshot, read in order.
struct DataReader<'a> {
    src: &'a mut dyn Source,
    data: DataRef,
    size: u64,
    at: u64,
}

impl Read for DataReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = usize::try_from((self.size - self.at).min(buf.len() as u64)).unwrap_or(0);
        let chunk = buf.get_mut(..n).unwrap_or_default();
        if n > 0 {
            self.src.read_at(self.data, self.at, chunk)?;
        }
        self.at += n as u64;
        Ok(n)
    }
}

/// Decompresses `input`, as DecompressStream detects it, into `out`.
fn decompress(input: impl Read, out: &mut dyn Write) -> Result<(), Error> {
    let mut input = BufReader::with_capacity(1 << 16, input);
    let head = input.fill_buf().map_err(|e| Error(e.to_string()))?;
    let head = head.get(..head.len().min(10)).unwrap_or_default().to_vec();
    let io_err = |e: io::Error| Error(e.to_string());
    match detect(&head) {
        Compression::None => io::copy(&mut input, out).map(drop).map_err(io_err),
        Compression::Gzip => io::copy(&mut flate2::bufread::MultiGzDecoder::new(input), out)
            .map(drop)
            .map_err(io_err),
        Compression::Bzip2 => io::copy(&mut bzip2::bufread::MultiBzDecoder::new(input), out)
            .map(drop)
            .map_err(io_err),
        Compression::Xz => io::copy(&mut lzma_rust2::XzReader::new(input, true), out)
            .map(drop)
            .map_err(io_err),
        Compression::Zstd => {
            shards_image::store::decode_zstd(&mut input, out).map_err(|e| Error(e.to_string()))
        }
    }
}

/// A regular file at `p` in `fs`, its symlinks followed: its size and data.
fn regular(fs: &Fs, p: &[u8]) -> Option<(u64, DataRef)> {
    let id = fs.lstat(p).ok()?;
    match fs.node(id).map(|n| &n.kind) {
        Some(Kind::File { size, data }) => Some((*size, *data)),
        _ => None,
    }
}

/// Writes nowhere, failing once more than a header's worth has been asked of it.
struct Head(Vec<u8>);

impl Write for Head {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        if self.0.len() >= DETECT {
            return Err(io::Error::other("enough"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// unpack.go isArchivePath: a regular file whose stream decompresses to a tar with a
/// first entry Go reads.
pub fn is_archive(src: &Fs, p: &[u8], sources: &mut Sources) -> Result<bool, Error> {
    let p = copy::root_path(src, p)?;
    let Some((size, data)) = regular(src, &p) else {
        return Ok(false);
    };
    let mut head = Head(Vec::new());
    let reader = DataReader {
        src: sources,
        data,
        size,
        at: 0,
    };
    // A stream that fails to decompress past its start is no archive, as Go's Next fails.
    let _ = decompress(reader, &mut head);
    Ok(matches!(tar::Reader::raw(&head.0[..]).next_entry(), Ok(Some(_))))
}

fn os(e: PathError) -> Error {
    Error(e.to_string())
}

/// moby's boundTime: a time before 1970 or past Go's range is 1970.
fn bound(sec: i64, nsec: u32) -> (i64, u32) {
    if !(0..=9_223_372_036).contains(&sec) || (sec == 9_223_372_036 && nsec > 854_775_807) {
        (0, 0)
    } else {
        (sec, nsec)
    }
}

/// Go's FileMode of a tar header, as syscall bits: permission, set-ID and sticky.
fn header_mode(mode: u32) -> u32 {
    mode & 0o7777
}

/// unpack.go unpack, for one source already known to be an archive: decompressed and
/// unpacked at once, with chrootarchive.Untar's Unpack into `dest_path`, and `owner` for
/// every entry when the action names one.
///
/// moby decompresses into the unpacker as a stream too. Here a thread decompresses, within
/// the build's budget, while this one unpacks, and only regular files' contents are kept,
/// in the stage: an archive is never written out whole (PM M78). A stream the decompressor
/// cannot read to its end fails the step with the decompressor's error, whatever the
/// unpacker made of what came before, as when the whole was decompressed first.
#[allow(clippy::too_many_arguments)]
pub fn unpack(
    src: &Fs,
    s: &[u8],
    dest: &mut Fs,
    dest_path: &[u8],
    ch: Chown,
    owner: Option<User>,
    tm: Option<(i64, u32)>,
    io: &mut Unpack<'_>,
) -> Result<(), Error> {
    let p = copy::root_path(src, s)?;
    let (size, data) = regular(src, &p)
        .ok_or_else(|| Error(format!("{}: not a regular file", String::from_utf8_lossy(s))))?;
    let dest_dir = copy::root_path(dest, dest_path)?;
    copy::mkdir_all(dest, &dest_dir, IMPLIED_DIR_MODE, ch, tm)?;

    let e = |e: io::Error| Error(e.to_string());
    let path = io.stage.join(format!("unpack-{}", next_id()));
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(e)?;
    // Read once everything is written: its files' contents, where the snapshot finds them.
    let source = io.sources.archive(File::open(&path).map_err(e)?).map_err(e)?;
    let mut contents = Contents {
        out: BufWriter::with_capacity(PIECE, file),
        room: Room::new(io.stage, &io.budget.limits).map_err(e)?,
        at: 0,
    };
    let limits = io.budget.limits;
    let Budget { bytes, held, .. } = &mut *io.budget;
    let sources = &mut *io.sources;

    dest.chroot(&dest_dir).map_err(os)?;
    let r = std::thread::scope(|scope| {
        let (to, from) = mpsc::sync_channel(PIECES);
        let decompressor = std::thread::Builder::new()
            .name("add-decompress".into())
            .spawn_scoped(scope, move || {
                let mut out = Pieces {
                    to,
                    piece: Vec::with_capacity(PIECE),
                    bytes,
                    limit: limits.bytes,
                };
                let reader = DataReader {
                    src: sources,
                    data,
                    size,
                    at: 0,
                };
                decompress(reader, &mut out)?;
                out.flush().map_err(|e| Error(e.to_string()))
            })
            .map_err(e)?;
        let mut stream = Stream {
            from,
            piece: Vec::new(),
            at: 0,
        };
        let unpacked = untar(dest, &mut stream, &mut contents, source, owner, &limits, held)
            .and_then(|()| contents.flush().map_err(e));
        stream.drain();
        let decompressed = decompressor
            .join()
            .map_err(|_| Error("decompressing the archive failed".into()))?;
        decompressed.and(unpacked)
    });
    dest.unchroot();
    r
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// moby's Unpack and createTarFile, inside the chroot.
fn untar(
    dest: &mut Fs,
    archive: &mut dyn Read,
    contents: &mut Contents,
    source: u32,
    owner: Option<User>,
    limits: &Limits,
    held: &mut Held,
) -> Result<(), Error> {
    let mut reader = tar::Reader::raw(archive);
    let mut dirs: Vec<(Vec<u8>, (i64, u32))> = Vec::new();
    loop {
        let mut entry = match reader.next_entry() {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return Err(Error(e.to_string())),
        };
        held.entry(limits, &entry)?;
        // A regular file's contents go to the stage, read from there once it is written:
        // where its offset now says.
        if entry.kind == Type::File {
            entry.offset = contents.at;
            reader.copy_data(contents).map_err(|e| Error(e.to_string()))?;
        }
        // filepath.Clean keeps a leading `..`, which joining to the root then removes.
        let name = go::clean(&entry.path);
        if name != b"."
            && entry.kind != Type::HardLink
            && placed(dest, &name, &entry, source, owner, &mut dirs)?
        {
            continue;
        }
        if name != b"/" {
            let parent = copy::dir(&name);
            let parent_path = vfs::join(b"/", &parent);
            if let Err(e) = dest.lstat(&parent_path)
                && e.errno == Errno::NoEnt
            {
                implied_dirs(dest, &parent_path)?;
            }
        }
        let path = vfs::join(b"/", &name);
        if let Ok(id) = dest.lstat(&path) {
            let is_dir = dest.is_dir(id);
            if is_dir && name == b"." {
                continue;
            }
            if !is_dir || entry.kind != Type::Dir {
                dest.remove_all(&path).map_err(os)?;
            }
        }
        let mode = header_mode(entry.mode);
        match entry.kind {
            Type::Dir => {
                let exists = dest.lstat(&path).ok().is_some_and(|id| dest.is_dir(id));
                if !exists {
                    dest.mkdir(&path, mode).map_err(os)?;
                }
            }
            Type::File => {
                let id = dest.create(&path, mode).map_err(os)?;
                dest.set_data(
                    id,
                    entry.size,
                    DataRef {
                        source,
                        offset: entry.offset,
                    },
                );
            }
            Type::BlockDevice | Type::CharDevice | Type::Fifo => {
                let kind = match entry.kind {
                    Type::BlockDevice => Kind::BlockDevice {
                        major: entry.devmajor,
                        minor: entry.devminor,
                    },
                    Type::CharDevice => Kind::CharDevice {
                        major: entry.devmajor,
                        minor: entry.devminor,
                    },
                    _ => Kind::Fifo,
                };
                dest.mknod(&path, kind, mode).map_err(os)?;
            }
            Type::HardLink => {
                let target = vfs::join(b"/", &entry.link);
                dest.link(&target, &path).map_err(os)?;
            }
            Type::Symlink => {
                dest.symlink(&entry.link, &path).map_err(os)?;
            }
        }
        let (uid, gid) = owner.map_or((entry.uid, entry.gid), |u| (u.uid, u.gid));
        dest.lchown(&path, uid, gid).map_err(|e| {
            Error(format!(
                "failed to Lchown {:?} for UID {}, GID {}: {e}",
                String::from_utf8_lossy(&path),
                entry.uid,
                entry.gid
            ))
        })?;
        for (k, v) in &entry.xattrs {
            if let Err(e) = dest.setxattr(&path, k, v, false) {
                // BestEffortXattrs: what the file system refuses is left out.
                if !matches!(e.errno, Errno::NotSup | Errno::Perm) {
                    return Err(Error(e.errno.text().to_string()));
                }
            }
        }
        let times = bound(entry.mtime, entry.mtime_nsec);
        // A hard link's attributes go to its file, unless it names a symlink.
        let links_to_file = |dest: &Fs| {
            dest.lstat(&vfs::join(b"/", &entry.link))
                .ok()
                .and_then(|id| dest.node(id))
                .is_some_and(|n| !matches!(n.kind, Kind::Symlink(_)))
        };
        match entry.kind {
            Type::HardLink => {
                if links_to_file(dest) {
                    dest.chmod(&path, mode).map_err(os)?;
                    dest.utimes(&path, times).map_err(os)?;
                }
            }
            Type::Symlink => dest.utimes(&path, times).map_err(os)?,
            _ => {
                dest.chmod(&path, mode).map_err(os)?;
                dest.utimes(&path, times).map_err(os)?;
            }
        }
        if entry.kind == Type::Dir {
            dirs.push((path, times));
        }
    }
    for (path, times) in dirs {
        dest.utimes(&path, times).map_err(os)?;
    }
    Ok(())
}

/// One entry of [`untar`] where its path leads somewhere already: to a free name in a
/// directory that exists, or to a directory it names again. The path is resolved once and
/// what follows acts on what it led to, as each call on the path would find it again;
/// true when done. Anything else (an entry in the way, a directory missing on the way, a
/// path that leads nowhere) is false, with nothing changed, and goes the way of every
/// call on the path, which says why as Linux and Go would.
fn placed(
    dest: &mut Fs,
    name: &[u8],
    entry: &tar::Entry,
    source: u32,
    owner: Option<User>,
    dirs: &mut Vec<(Vec<u8>, (i64, u32))>,
) -> Result<bool, Error> {
    let path = vfs::join(b"/", name);
    let mode = header_mode(entry.mode);
    let made = match dest.place(&path) {
        Ok(Place::Free(dir, last)) => {
            let (op, made) = match entry.kind {
                Type::Dir => ("mkdir", dest.mkdir_in(dir, &last, mode)),
                Type::File => ("open", dest.create_in(dir, &last, mode)),
                Type::BlockDevice | Type::CharDevice | Type::Fifo => {
                    let kind = match entry.kind {
                        Type::BlockDevice => Kind::BlockDevice {
                            major: entry.devmajor,
                            minor: entry.devminor,
                        },
                        Type::CharDevice => Kind::CharDevice {
                            major: entry.devmajor,
                            minor: entry.devminor,
                        },
                        _ => Kind::Fifo,
                    };
                    ("mknod", dest.mknod_in(dir, &last, kind, mode))
                }
                Type::Symlink => ("symlink", dest.symlink_in(&entry.link, dir, &last)),
                Type::HardLink => return Ok(false),
            };
            let (id, at) = made.map_err(|errno| {
                os(PathError {
                    op,
                    path: if entry.kind == Type::Symlink {
                        [&entry.link[..], b" ", &path].concat()
                    } else {
                        path.clone()
                    },
                    errno,
                })
            })?;
            if entry.kind == Type::File {
                dest.set_data(
                    id,
                    entry.size,
                    DataRef {
                        source,
                        offset: entry.offset,
                    },
                );
            }
            (id, at)
        }
        Ok(Place::Is(id, at)) if entry.kind == Type::Dir && dest.is_dir(id) => (id, at),
        _ => return Ok(false),
    };
    let (id, at) = made;
    let (uid, gid) = owner.map_or((entry.uid, entry.gid), |u| (u.uid, u.gid));
    dest.lchown_node(id, at, uid, gid);
    for (k, v) in &entry.xattrs {
        if let Err(errno) = dest.setxattr_node(id, at, k, v) {
            // BestEffortXattrs: what the file system refuses is left out.
            if !matches!(errno, Errno::NotSup | Errno::Perm) {
                return Err(Error(errno.text().to_string()));
            }
        }
    }
    let times = bound(entry.mtime, entry.mtime_nsec);
    if entry.kind != Type::Symlink {
        dest.chmod_node(id, at, mode);
    }
    dest.utimes_node(id, at, times);
    if entry.kind == Type::Dir {
        dirs.push((path, times));
    }
    Ok(true)
}

/// user.MkdirAllAndChown(path, 0755, 0, 0, WithOnlyNew): the directories made, and only
/// they, owned by root.
fn implied_dirs(dest: &mut Fs, path: &[u8]) -> Result<(), Error> {
    let mut missing = Vec::new();
    let mut p = path.to_vec();
    while p != b"/" {
        if matches!(dest.stat(&p), Err(ref e) if e.errno == Errno::NoEnt) {
            missing.push(p.clone());
        }
        p = copy::dir(&p);
    }
    for d in missing.iter().rev() {
        match dest.mkdir(d, IMPLIED_DIR_MODE) {
            Ok(_) => {}
            Err(e) if e.errno == Errno::Exist => {}
            Err(e) => return Err(os(e)),
        }
    }
    for d in &missing {
        dest.lchown(d, 0, 0).map_err(os)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Writes `total` patterned bytes in writes of `step` through pieces held to `limit`,
    /// reading them back on another thread: whether the writes were taken, and what came
    /// through.
    fn through(total: usize, step: usize, limit: u64) -> (io::Result<()>, Vec<u8>, u64) {
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let (to, from) = mpsc::sync_channel(PIECES);
        let mut bytes = 0u64;
        let got = std::thread::scope(|scope| {
            let reader = scope.spawn(move || {
                let mut s = Stream {
                    from,
                    piece: Vec::new(),
                    at: 0,
                };
                let mut got = Vec::new();
                s.read_to_end(&mut got).unwrap();
                got
            });
            let mut out = Pieces {
                to,
                piece: Vec::with_capacity(PIECE),
                bytes: &mut bytes,
                limit,
            };
            let r = data
                .chunks(step)
                .try_for_each(|c| out.write_all(c))
                .and_then(|()| out.flush());
            drop(out);
            (r, reader.join().unwrap())
        });
        (got.0, got.1, bytes)
    }

    /// Every byte is counted once, whatever the writes' sizes against the pieces', and
    /// arrives in order: a stream exactly at the limit passes, one byte more does not.
    #[test]
    fn pieces_count_each_byte_once_and_keep_its_order() {
        let total = PIECE * 3 + 1234;
        for step in [1000, PIECE - 1, PIECE, PIECE * 2 + 7] {
            let (r, got, bytes) = through(total, step, total as u64);
            r.unwrap();
            assert_eq!(bytes, total as u64, "writes of {step}");
            assert_eq!(got, (0..total).map(|i| (i % 251) as u8).collect::<Vec<_>>());
            let (r, _, _) = through(total, step, total as u64 - 1);
            let e = r.unwrap_err().to_string();
            assert!(e.contains("decompress to more than"), "{e}");
        }
    }
}
