//! A builder guest's trees and the host's change streams (shards_abi::changes): a step's
//! overlay upper layer sent as changes, and changes the host sends written as a layer for
//! overlayfs to stack. Both use overlayfs's own markers on disk (Documentation/filesystems/
//! overlayfs.rst): a whiteout is a character device numbered 0:0, and an opaque directory
//! has `trusted.overlay.opaque` set to `y`.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use shards_abi::changes::{self, Decoder, Entry, Event, flag, kind};

/// How much of a stream is gathered before it is handed on: as much as one frame holds.
const CHUNK: usize = shards_abi::run::MAX_PAYLOAD as usize;

const OPAQUE: &[u8] = b"trusted.overlay.opaque";

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::other("a path holds NUL"))
}

/// The xattrs of `path`, its last symlink not followed: every one but overlayfs's own.
fn xattrs(path: &CString) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut names = vec![0u8; 1024];
    let len = loop {
        // SAFETY: a NUL-terminated path, and a buffer of the length given.
        let n = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ERANGE) => names.resize(names.len() * 2, 0),
            Some(libc::ENOTSUP) => return Ok(Vec::new()),
            _ => return Err(e),
        }
    };
    let mut out = Vec::new();
    for name in names.get(..len).unwrap_or_default().split(|&b| b == 0) {
        if name.is_empty() || name.starts_with(b"trusted.overlay.") {
            continue;
        }
        let value = getxattr(path, name)?;
        if let Some(value) = value {
            out.push((name.to_vec(), value));
        }
    }
    Ok(out)
}

/// One xattr of `path`, its last symlink not followed; `None` if it is not there.
fn getxattr(path: &CString, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let cname = CString::new(name).map_err(|_| io::Error::other("an xattr name holds NUL"))?;
    let mut value = vec![0u8; 256];
    loop {
        // SAFETY: NUL-terminated strings, and a buffer of the length given.
        let n = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                cname.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if n >= 0 {
            value.truncate(n as usize);
            return Ok(Some(value));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ERANGE) => {
                let more = value.len() * 2;
                if more > changes::MAX_XATTR_VALUE * 2 {
                    return Err(e);
                }
                value.resize(more, 0);
            }
            Some(libc::ENODATA) => return Ok(None),
            _ => return Err(e),
        }
    }
}

/// Gathers a stream into frames' worth before handing it on.
struct Sink<'a> {
    buf: Vec<u8>,
    out: &'a mut dyn FnMut(&[u8]) -> io::Result<()>,
}

impl Sink<'_> {
    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let room = CHUNK - self.buf.len();
            let (now, later) = bytes.split_at(room.min(bytes.len()));
            self.buf.extend_from_slice(now);
            bytes = later;
            if self.buf.len() == CHUNK {
                (self.out)(&self.buf)?;
                self.buf.clear();
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            (self.out)(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// Sends the overlay upper layer at `upper` as changes, through `out`, a frame's worth at
/// a time: each directory before what it holds, each hard-linked file once and its other
/// names as links to it.
pub fn send_upper(upper: &Path, out: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    let mut sink = Sink {
        buf: Vec::with_capacity(CHUNK),
        out,
    };
    let mut seen: HashMap<(u64, u64), Vec<u8>> = HashMap::new();
    let mut header = Vec::with_capacity(256);
    let mut data = vec![0u8; CHUNK];
    // Directories to walk: their path on disk, and their path in the stream.
    let mut todo: Vec<(PathBuf, Vec<u8>)> = vec![(upper.to_path_buf(), Vec::new())];
    while let Some((dir, rel)) = todo.pop() {
        for child in std::fs::read_dir(&dir)? {
            let child = child?;
            let name = child.file_name();
            let path = child.path();
            let mut rel_child = rel.clone();
            if !rel_child.is_empty() {
                rel_child.push(b'/');
            }
            rel_child.extend_from_slice(name.as_bytes());
            let m = std::fs::symlink_metadata(&path)?;
            let c = cstr(&path)?;
            let t = m.file_type();
            let mut e = Entry {
                mode: m.mode() & 0o7777,
                uid: m.uid(),
                gid: m.gid(),
                mtime: m.mtime(),
                mtime_nsec: u32::try_from(m.mtime_nsec()).unwrap_or(0),
                path: rel_child.clone(),
                ..Entry::default()
            };
            let rdev = m.rdev();
            if t.is_char_device() && rdev == 0 {
                e.kind = kind::WHITEOUT;
                e.mode = 0;
            } else {
                e.xattrs = xattrs(&c)?;
                if t.is_dir() {
                    e.kind = kind::DIR;
                    if getxattr(&c, OPAQUE)?.as_deref() == Some(b"y") {
                        e.flags = flag::OPAQUE;
                    }
                } else if !t.is_dir()
                    && m.nlink() > 1
                    && let Some(first) = seen.get(&(m.dev(), m.ino()))
                {
                    e = Entry {
                        kind: kind::LINK,
                        path: rel_child.clone(),
                        target: first.clone(),
                        ..Entry::default()
                    };
                } else {
                    if m.nlink() > 1 {
                        seen.insert((m.dev(), m.ino()), rel_child.clone());
                    }
                    if t.is_file() {
                        e.kind = kind::FILE;
                        e.size = m.size();
                    } else if t.is_symlink() {
                        e.kind = kind::SYMLINK;
                        e.target = std::fs::read_link(&path)?.as_os_str().as_bytes().to_vec();
                    } else if t.is_char_device() || t.is_block_device() {
                        e.kind = if t.is_char_device() {
                            kind::CHAR
                        } else {
                            kind::BLOCK
                        };
                        e.major = libc::major(rdev);
                        e.minor = libc::minor(rdev);
                    } else if t.is_fifo() {
                        e.kind = kind::FIFO;
                    } else {
                        e.kind = kind::SOCKET;
                    }
                }
            }
            header.clear();
            e.encode_into(&mut header);
            sink.push(&header)?;
            if e.kind == kind::FILE {
                send_file(&c, e.size, &mut data, &mut sink)?;
            }
            if e.kind == kind::DIR {
                todo.push((path, rel_child));
            }
        }
    }
    sink.push(&[changes::END])?;
    sink.flush()
}

/// Sends exactly `size` bytes of the file at `path`, never through a symlink.
fn send_file(path: &CString, size: u64, buf: &mut [u8], sink: &mut Sink<'_>) -> io::Result<()> {
    // SAFETY: a NUL-terminated path; the descriptor is owned from here on.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just opened.
    let mut f = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(buf.len(), |l| l.min(buf.len()));
        let chunk = buf.get_mut(..want).unwrap_or_default();
        let n = f.read(chunk)?;
        if n == 0 {
            return Err(io::Error::other("a file grew shorter while it was sent"));
        }
        sink.push(chunk.get(..n).unwrap_or_default())?;
        left -= n as u64;
    }
    Ok(())
}

/// Writes a stream of changes as a layer under `root`, for overlayfs to stack: each entry
/// as the file it names, whiteouts and opaque directories as overlayfs marks them.
pub struct LayerWriter {
    root: PathBuf,
    decoder: Decoder,
    /// The file whose bytes are arriving, its entry, and how many are still to come.
    file: Option<(File, Entry, u64)>,
    /// Directories made, and their entries: their times are set last, deepest first.
    dirs: Vec<(CString, Entry)>,
}

impl LayerWriter {
    /// A layer at `root`, an empty directory.
    pub fn new(root: PathBuf) -> LayerWriter {
        LayerWriter {
            root,
            decoder: Decoder::new(),
            file: None,
            dirs: Vec::new(),
        }
    }

    pub fn feed(&mut self, mut input: &[u8]) -> io::Result<()> {
        let bad = |e: changes::Error| io::Error::other(format!("the host's changes: {e:?}"));
        loop {
            let (event, n) = self.decoder.next(input).map_err(bad)?;
            match event {
                Event::Entry(e) => self.entry(e)?,
                Event::Data(bytes) => {
                    if let Some((f, _, left)) = &mut self.file {
                        f.write_all(bytes)?;
                        *left = left.saturating_sub(bytes.len() as u64);
                    }
                    if self.file.as_ref().is_some_and(|(_, _, left)| *left == 0)
                        && let Some((f, e, _)) = self.file.take()
                    {
                        drop(f);
                        finish(&cstr(&self.root.join(OsStr::from_bytes(&e.path)))?, &e)?;
                    }
                }
                Event::More => return Ok(()),
                Event::End => {
                    let rest = input.get(n..).unwrap_or_default();
                    return if rest.is_empty() {
                        Ok(())
                    } else {
                        self.decoder.next(rest).map(|_| ()).map_err(bad)
                    };
                }
            }
            input = input.get(n..).unwrap_or_default();
        }
    }

    /// Whether the stream has ended.
    pub fn ended(&self) -> bool {
        self.decoder.ended()
    }

    /// The stream must have ended whole; directories then take their times, deepest first.
    pub fn finish(self) -> io::Result<()> {
        if !self.decoder.ended() || self.file.is_some() {
            return Err(io::Error::other("the host's changes ended early"));
        }
        for (path, e) in self.dirs.iter().rev() {
            times(path, e)?;
        }
        Ok(())
    }

    fn entry(&mut self, e: Entry) -> io::Result<()> {
        let path = self.root.join(OsStr::from_bytes(&e.path));
        let c = cstr(&path)?;
        let node = |mode: u32, dev: libc::dev_t| {
            // SAFETY: a NUL-terminated path.
            if unsafe { libc::mknod(c.as_ptr(), mode, dev) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        match e.kind {
            kind::DIR => {
                // SAFETY: a NUL-terminated path.
                if unsafe { libc::mkdir(c.as_ptr(), 0o700) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                if e.flags & flag::OPAQUE != 0 {
                    setxattr(&c, OPAQUE, b"y")?;
                }
                owner_mode_xattrs(&c, &e)?;
                self.dirs.push((c, e));
                return Ok(());
            }
            kind::FILE => {
                let f = File::options().write(true).create_new(true).open(&path)?;
                if e.size == 0 {
                    drop(f);
                    return finish(&c, &e);
                }
                let size = e.size;
                self.file = Some((f, e, size));
                return Ok(());
            }
            kind::SYMLINK => {
                let t = CString::new(e.target.clone()).map_err(|_| io::Error::other("a target holds NUL"))?;
                // SAFETY: NUL-terminated strings.
                if unsafe { libc::symlink(t.as_ptr(), c.as_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            kind::LINK => {
                let t = cstr(&self.root.join(OsStr::from_bytes(&e.target)))?;
                // SAFETY: NUL-terminated paths.
                if unsafe { libc::link(t.as_ptr(), c.as_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            kind::CHAR => node(libc::S_IFCHR, libc::makedev(e.major, e.minor))?,
            kind::BLOCK => node(libc::S_IFBLK, libc::makedev(e.major, e.minor))?,
            kind::FIFO => node(libc::S_IFIFO, 0)?,
            kind::SOCKET => node(libc::S_IFSOCK, 0)?,
            kind::WHITEOUT => return node(libc::S_IFCHR, 0),
            other => return Err(io::Error::other(format!("an entry of kind {other}"))),
        }
        finish(&c, &e)
    }
}

fn setxattr(path: &CString, name: &[u8], value: &[u8]) -> io::Result<()> {
    let n = CString::new(name).map_err(|_| io::Error::other("an xattr name holds NUL"))?;
    // SAFETY: NUL-terminated strings and a value of the length given.
    if unsafe { libc::lsetxattr(path.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Owner, then mode (a change of owner clears set-ID bits), then xattrs (it clears file
/// capabilities too).
fn owner_mode_xattrs(path: &CString, e: &Entry) -> io::Result<()> {
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::lchown(path.as_ptr(), e.uid, e.gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if e.kind != kind::SYMLINK {
        // SAFETY: a NUL-terminated path, not a symlink.
        if unsafe { libc::chmod(path.as_ptr(), e.mode & 0o7777) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    for (name, value) in &e.xattrs {
        setxattr(path, name, value)?;
    }
    Ok(())
}

/// What an entry that is no directory takes once it is whole: owner, mode, xattrs, times.
fn finish(path: &CString, e: &Entry) -> io::Result<()> {
    owner_mode_xattrs(path, e)?;
    times(path, e)
}

fn times(path: &CString, e: &Entry) -> io::Result<()> {
    let t = libc::timespec {
        tv_sec: e.mtime,
        tv_nsec: libc::c_long::from(e.mtime_nsec as i32),
    };
    let ts = [t, t];
    // SAFETY: a NUL-terminated path and two timespecs.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            ts.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
