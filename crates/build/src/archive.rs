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

    /// Counts `n` bytes a download wrote.
    pub fn fetched(&mut self, n: u64) -> Result<(), Error> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.limits.bytes {
            return Err(Error(over_budget(self.limits.bytes)));
        }
        Ok(())
    }
}

/// What a build's ADDs write past `limit`, their downloads and decompressed archives
/// together.
pub fn over_budget(limit: u64) -> String {
    format!("what ADD fetches and unpacks comes to more than {limit} bytes (SHARDS_MAX_IMAGE_BYTES)")
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
            return Err(io::Error::other(over_budget(self.limit)));
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
    /// Whether it was read to where the decompressor stopped.
    ended: bool,
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.piece.len() {
            match self.from.recv() {
                Ok(piece) => {
                    self.piece = piece;
                    self.at = 0;
                }
                Err(_) => {
                    self.ended = true;
                    return Ok(0);
                }
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

/// The compressions moby's Detect knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Bzip2,
    Gzip,
    Xz,
    Zstd,
}

/// go-archive's compression.Detect, over the first bytes of a stream: bzip2's and xz's
/// magic, or gzip's and zstd's as containerd matches them, which is as Detect does.
pub fn detect(head: &[u8]) -> Compression {
    use shards_image::store::{self, compression};
    if head.starts_with(&[0x42, 0x5A, 0x68]) {
        Compression::Bzip2
    } else if head.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        Compression::Xz
    } else {
        match compression(head) {
            store::Compression::Gzip => Compression::Gzip,
            store::Compression::Zstd => Compression::Zstd,
            store::Compression::None => Compression::None,
        }
    }
}

/// A stream with its first bytes read ahead, then buffered.
pub type Peeked<R> = BufReader<io::Chain<io::Cursor<Vec<u8>>, R>>;

/// A stream as DecompressStream gives it: decoded as its first bytes said, as it is read.
pub enum Decoder<R: BufRead> {
    None(R),
    Bzip2(bzip2::bufread::MultiBzDecoder<R>),
    Gzip(flate2::bufread::MultiGzDecoder<R>),
    Xz(Box<lzma_rust2::XzReader<Full<R>>>),
    Zstd(Box<shards_image::store::Zstd<R>>),
}

/// A stream whose reads fill all they are given, short only at its end, as lzma-rust2's
/// XzReader needs its input: 0.21.0 reads a block's padding with one read and refuses
/// fewer bytes (reader.rs consume_padding), which a buffered stream returns wherever the
/// padding straddles its buffer's end.
#[derive(Debug)]
pub struct Full<R> {
    inner: R,
    /// An error met after some bytes were read, for the next read to return.
    error: Option<io::Error>,
}

impl<R: Read> Read for Full<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        let mut n = 0;
        while let Some(rest) = buf.get_mut(n..)
            && !rest.is_empty()
        {
            match self.inner.read(rest) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if n > 0 => {
                    self.error = Some(e);
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }
}

impl<R: BufRead> std::fmt::Debug for Decoder<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Decoder::None(_) => "Decoder::None",
            Decoder::Bzip2(_) => "Decoder::Bzip2",
            Decoder::Gzip(_) => "Decoder::Gzip",
            Decoder::Xz(_) => "Decoder::Xz",
            Decoder::Zstd(_) => "Decoder::Zstd",
        })
    }
}

impl<R: BufRead> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Decoder::None(r) => r.read(buf),
            Decoder::Bzip2(r) => r.read(buf),
            Decoder::Gzip(r) => r.read(buf),
            Decoder::Xz(r) => r.read(buf),
            Decoder::Zstd(r) => r.read(buf),
        }
    }
}

/// DecompressStream: `input`, buffered by `capacity` bytes, read through the decoder its
/// first ten bytes call for, as Peek(10) gives them to Detect: fewer only where the stream
/// ends sooner, however few each read of it returns.
pub fn decompressed<R: Read>(mut input: R, capacity: usize) -> io::Result<Decoder<Peeked<R>>> {
    let mut head = Vec::with_capacity(10);
    (&mut input).take(10).read_to_end(&mut head)?;
    let compression = detect(&head);
    let input = BufReader::with_capacity(capacity, io::Cursor::new(head).chain(input));
    Ok(match compression {
        Compression::None => Decoder::None(input),
        Compression::Bzip2 => Decoder::Bzip2(bzip2::bufread::MultiBzDecoder::new(input)),
        Compression::Gzip => Decoder::Gzip(flate2::bufread::MultiGzDecoder::new(input)),
        Compression::Xz => Decoder::Xz(Box::new(lzma_rust2::XzReader::new(
            Full {
                inner: input,
                error: None,
            },
            true,
        ))),
        Compression::Zstd => Decoder::Zstd(Box::new(shards_image::store::Zstd::new(input))),
    })
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
    let io_err = |e: io::Error| Error(e.to_string());
    io::copy(&mut decompressed(input, 1 << 16).map_err(io_err)?, out)
        .map(drop)
        .map_err(io_err)
}

/// A regular file at `p` in `fs`, its symlinks followed: its size and data.
fn regular(fs: &Fs, p: &[u8]) -> Option<(u64, DataRef)> {
    let id = fs.lstat(p).ok()?;
    match fs.node(id).map(|n| &n.kind) {
        Some(Kind::File { size, data }) => Some((*size, *data)),
        _ => None,
    }
}

/// unpack.go isArchivePath: a regular file whose stream decompresses to a tar with a
/// first entry Go reads.
pub fn is_archive(src: &Fs, p: &[u8], sources: &mut Sources) -> Result<bool, Error> {
    let p = copy::root_path(src, p)?;
    let Some((size, data)) = regular(src, &p) else {
        return Ok(false);
    };
    let reader = DataReader {
        src: sources,
        data,
        size,
        at: 0,
    };
    // As much as its first header can take; a stream that fails to decompress past its
    // start is no archive, as Go's Next fails.
    let mut head = Vec::new();
    if let Ok(decoder) = decompressed(reader, 1 << 16) {
        let _ = decoder.take(DETECT as u64).read_to_end(&mut head);
    }
    Ok(matches!(tar::Reader::raw(&head[..]).next_entry(), Ok(Some(_))))
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
            ended: false,
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

/// When the newest regular member of the archive in `file` was modified, if it is one, as
/// BuildKit takes a source's time from what it fetches (dockerfile/1.27.1 epoch.go,
/// archiveMaxTimeFromRef, which allows what is no archive): decompressed as ADD
/// decompresses an archive, and read as Go's archive/tar reads it, as far as its end. What
/// cannot be read as one, or stops decompressing before its end, is none; what
/// decompresses to more than `limits` allow fails.
pub fn newest_file(file: File, limits: &Limits) -> Result<Option<(i64, u32)>, Error> {
    let limit = limits.bytes;
    std::thread::scope(|scope| {
        let (to, from) = mpsc::sync_channel(PIECES);
        let decompressor = std::thread::Builder::new()
            .name("epoch-decompress".into())
            .spawn_scoped(scope, move || {
                let mut bytes = 0;
                let mut out = Pieces {
                    to,
                    piece: Vec::with_capacity(PIECE),
                    bytes: &mut bytes,
                    limit,
                };
                // What came out before any error goes to the reader, as Go's reads it
                // before it reads the error.
                let decompressed = decompress(file, &mut out);
                let flushed = out.flush().map_err(|e| Error(e.to_string()));
                drop(out);
                (decompressed.and(flushed), bytes)
            })
            .map_err(|e| Error(e.to_string()))?;
        let mut stream = Stream {
            from,
            piece: Vec::new(),
            at: 0,
            ended: false,
        };
        let newest = tar::Reader::new(&mut stream).newest_regular();
        let ended = stream.ended;
        // Whatever the archive holds past its end is not read: the decompressor stops.
        drop(stream);
        let (decompressed, bytes) = decompressor
            .join()
            .map_err(|_| Error("decompressing the archive failed".into()))?;
        if bytes > limit {
            return Err(Error(over_budget(limit)));
        }
        Ok(match (newest, decompressed) {
            // Go's reader would have read the decompressor's error where this one found
            // the stream's end.
            (Ok(_), Err(_)) if ended => None,
            (Ok(newest), _) => newest,
            (Err(_), _) => None,
        })
    })
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
                    ended: false,
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

    fn limits(bytes: u64) -> Limits {
        Limits {
            bytes,
            entries: u64::MAX,
            metadata: u64::MAX,
            keep_free: 0,
            available: |_| Ok(u64::MAX),
        }
    }

    /// When the newest regular member of `data` was modified, as `newest_file` finds it.
    fn newest(data: &[u8], limit: u64) -> Result<Option<(i64, u32)>, Error> {
        let path = std::env::temp_dir().join(format!("shards-newest-{}-{}", std::process::id(), next_id()));
        std::fs::write(&path, data).unwrap();
        let found = newest_file(File::open(&path).unwrap(), &limits(limit));
        std::fs::remove_file(&path).unwrap();
        found
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    /// An archive's newest regular member's time comes through its compression, to the
    /// nanosecond; what is no archive has none; what decompresses past the limit fails; and
    /// a decompressor's error where Go's reader would read it (here, a gzip checksum after
    /// an archive with no end marker) leaves none, as Go's reader fails there, while one
    /// past the archive's end changes nothing.
    /// DecompressStream: the first ten bytes tell the compression however few each read
    /// returns (Peek(10)), and every compression Detect knows decodes, zstd after a
    /// skippable frame too, whose magic tells only with its whole 8-byte header. The
    /// streams are bzip2 1.0.8's, Apple gzip 479's, XZ Utils 5.8.4's and zstd 1.5.7's.
    #[test]
    fn streams_decompress_as_decompressstream_reads_them() {
        /// Hands over one byte a read.
        struct Dribble<'a>(&'a [u8]);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let (Some((first, rest)), Some(to)) = (self.0.split_first(), buf.first_mut()) else {
                    return Ok(0);
                };
                *to = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        const BZIP2: [u8; 62] = [
            0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x48, 0x63, 0x7b, 0xef, 0x00, 0x00,
            0x05, 0xd1, 0x80, 0x00, 0x10, 0x40, 0x00, 0x2e, 0x22, 0xdc, 0x80, 0x20, 0x00, 0x21, 0xa9, 0xea,
            0x34, 0xc4, 0xc6, 0xa1, 0x00, 0x00, 0x18, 0x58, 0xc5, 0xd9, 0x24, 0x3d, 0xc4, 0x14, 0xed, 0x43,
            0x16, 0x09, 0xa5, 0x3f, 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, 0x48, 0x63, 0x7b, 0xef,
        ];
        const GZIP: [u8; 47] = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x4b, 0x49, 0x4d, 0xce, 0x4f, 0x49,
            0x4d, 0x51, 0x48, 0x2c, 0x56, 0xc8, 0x2c, 0x51, 0x28, 0x07, 0x52, 0xc9, 0xf9, 0xb9, 0x05, 0x45,
            0xa9, 0xc5, 0xc5, 0xa9, 0x29, 0x5c, 0x00, 0x26, 0x1a, 0x96, 0x11, 0x1d, 0x00, 0x00, 0x00,
        ];
        const XZ: [u8; 96] = [
            0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46, 0x04, 0xc0, 0x21, 0x1d,
            0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe6, 0x6a, 0x1c, 0x77,
            0x01, 0x00, 0x1c, 0x64, 0x65, 0x63, 0x6f, 0x64, 0x65, 0x64, 0x20, 0x61, 0x73, 0x20, 0x69, 0x74,
            0x20, 0x77, 0x61, 0x73, 0x20, 0x63, 0x6f, 0x6d, 0x70, 0x72, 0x65, 0x73, 0x73, 0x65, 0x64, 0x0a,
            0x00, 0x00, 0x00, 0x00, 0x8f, 0xf1, 0xab, 0x12, 0x9a, 0x91, 0xa5, 0xd8, 0x00, 0x01, 0x3d, 0x1d,
            0x4c, 0x91, 0x68, 0x29, 0x1f, 0xb6, 0xf3, 0x7d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
        ];
        const ZSTD: [u8; 42] = [
            0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x1d, 0xe9, 0x00, 0x00, 0x64, 0x65, 0x63, 0x6f, 0x64, 0x65, 0x64,
            0x20, 0x61, 0x73, 0x20, 0x69, 0x74, 0x20, 0x77, 0x61, 0x73, 0x20, 0x63, 0x6f, 0x6d, 0x70, 0x72,
            0x65, 0x73, 0x73, 0x65, 0x64, 0x0a, 0xba, 0xe8, 0xe6, 0x6e,
        ];
        let plain = b"decoded as it was compressed\n";
        let mut skipped = vec![0x5f, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
        skipped.extend_from_slice(&ZSTD);
        for (stream, want) in [
            (&plain[..], Compression::None),
            (&b"short"[..], Compression::None),
            (&BZIP2[..], Compression::Bzip2),
            (&GZIP[..], Compression::Gzip),
            (&XZ[..], Compression::Xz),
            (&ZSTD[..], Compression::Zstd),
            (&skipped[..], Compression::Zstd),
        ] {
            let mut decoder = decompressed(Dribble(stream), 64).unwrap();
            let found = match decoder {
                Decoder::None(_) => Compression::None,
                Decoder::Bzip2(_) => Compression::Bzip2,
                Decoder::Gzip(_) => Compression::Gzip,
                Decoder::Xz(_) => Compression::Xz,
                Decoder::Zstd(_) => Compression::Zstd,
            };
            assert_eq!(found, want);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).unwrap();
            let expected: &[u8] = if stream == b"short" { b"short" } else { plain };
            assert_eq!(out, expected, "{want:?}");
        }
    }

    /// Full's reads fill what they are given past interruptions, and an error met after
    /// some bytes comes with the next read.
    #[test]
    fn full_reads_fill_and_keep_their_errors() {
        struct Steps(Vec<io::Result<u8>>);
        impl Read for Steps {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let byte = self.0.remove(0)?;
                if let Some(to) = buf.first_mut() {
                    *to = byte;
                }
                Ok(1)
            }
        }
        let steps = vec![
            Ok(1),
            Err(io::ErrorKind::Interrupted.into()),
            Ok(2),
            Err(io::Error::other("broken")),
            Ok(3),
        ];
        let mut full = Full {
            inner: Steps(steps),
            error: None,
        };
        let mut buf = [0u8; 4];
        assert_eq!(full.read(&mut buf).unwrap(), 2);
        assert_eq!(buf.get(..2), Some(&[1, 2][..]));
        assert_eq!(full.read(&mut buf).unwrap_err().to_string(), "broken");
        assert_eq!(full.read(&mut buf).unwrap(), 1);
        assert_eq!(buf.first(), Some(&3));
        assert_eq!(full.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn an_archives_newest_file_is_found_through_its_compression() {
        let tar = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../image/testdata/tar-newest/pax-nsec.tar"),
        )
        .unwrap();
        let time = Some((1_700_000_000, 500_000_000));
        assert_eq!(newest(&tar, u64::MAX).unwrap(), time);
        assert_eq!(newest(&gzip(&tar), u64::MAX).unwrap(), time);
        assert_eq!(newest(b"no archive at all", u64::MAX).unwrap(), None);
        let e = newest(&gzip(&tar), 1024).unwrap_err();
        assert!(e.0.contains("comes to more than 1024 bytes"), "{e:?}");
        // The end marker's two blocks dropped: the archive ends where the stream does.
        let unmarked = tar.get(..tar.len() - 1024).unwrap();
        let mut crc = gzip(unmarked);
        assert_eq!(newest(&crc, u64::MAX).unwrap(), time);
        let at = crc.len() - 8;
        crc[at] ^= 0xff;
        assert_eq!(newest(&crc, u64::MAX).unwrap(), None);
        let mut past = gzip(&tar);
        let at = past.len() - 8;
        past[at] ^= 0xff;
        assert_eq!(newest(&past, u64::MAX).unwrap(), time);
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
            assert!(e.contains("fetches and unpacks comes to more than"), "{e}");
        }
    }
}
