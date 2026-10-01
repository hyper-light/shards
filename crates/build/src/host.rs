//! The host's files as fsutil's sender reads them (stat.go, stat_unix.go, stat_windows.go
//! at 83cac42c1c52): Go's FileMode bits, the link and device numbers it sends, and
//! extended attributes, those under `com.apple.` aside. The OS's own calls are here
//! and nowhere else in the crate.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::copy::fm;

/// What fsutil's `mkstat` records of a path, before the sender resets its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// Go's FileMode bits.
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, u32),
    /// The inode and its link count, where hard links are told apart; none on Windows.
    pub inode: Option<(u64, u64)>,
    pub devmajor: u32,
    pub devminor: u32,
    pub link: Vec<u8>,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// A socket, which is sent as the empty file archive/tar can hold.
    pub socket: bool,
}

fn since_epoch(m: &fs::Metadata) -> (i64, u32) {
    match m.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
                match d.subsec_nanos() {
                    0 => (-secs, 0),
                    n => (-secs - 1, 1_000_000_000 - n),
                }
            }
        },
        Err(_) => (0, 0),
    }
}

/// A directory's names, sorted, as `os.ReadDir` gives them.
pub fn read_dir(dir: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir)? {
        names.push(name_bytes(&e?.file_name()));
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    n.as_bytes().to_vec()
}

#[cfg(windows)]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    n.to_string_lossy().into_owned().into_bytes()
}

/// A path below `root` from its slash-separated relative name.
#[cfg(unix)]
pub fn path(root: &Path, rel: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    root.join(std::ffi::OsStr::from_bytes(rel))
}

#[cfg(windows)]
pub fn path(root: &Path, rel: &[u8]) -> std::path::PathBuf {
    let mut p = root.to_path_buf();
    for part in String::from_utf8_lossy(rel).split('/') {
        p.push(part);
    }
    p
}

/// `os.Lstat` and fsutil's mkstat.
#[cfg(unix)]
pub fn lstat(p: &Path) -> io::Result<Stat> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(p)?;
    let st = m.mode();
    let mut mode = st & 0o777;
    mode |= match st & 0o170_000 {
        0o040_000 => fm::DIR,
        0o120_000 => fm::SYMLINK,
        0o060_000 => fm::DEVICE,
        0o020_000 => fm::DEVICE | fm::CHAR_DEVICE,
        0o010_000 => fm::NAMED_PIPE,
        0o140_000 => fm::SOCKET,
        _ => 0,
    };
    if st & 0o4000 != 0 {
        mode |= fm::SETUID;
    }
    if st & 0o2000 != 0 {
        mode |= fm::SETGID;
    }
    if st & 0o1000 != 0 {
        mode |= fm::STICKY;
    }
    // archive/tar cannot hold a socket, so fsutil sends it as what it then is: a file.
    let socket = mode & fm::SOCKET != 0;
    mode &= !fm::SOCKET;
    let dir = mode & fm::DIR != 0;
    let rdev = m.rdev();
    let device = st & 0o170_000 == 0o060_000 || st & 0o170_000 == 0o020_000;
    let link = if mode & fm::SYMLINK != 0 {
        use std::os::unix::ffi::OsStrExt;
        fs::read_link(p)?.as_os_str().as_bytes().to_vec()
    } else {
        Vec::new()
    };
    Ok(Stat {
        mode,
        size: if dir { 0 } else { m.size() },
        mtime: since_epoch(&m),
        inode: (!dir).then(|| (m.ino(), m.nlink())),
        // fsutil's major and minor, Linux's encoding whatever the host's.
        devmajor: if device { ((rdev >> 8) & 0xfff) as u32 } else { 0 },
        devminor: if device {
            ((rdev & 0xff) | ((rdev >> 12) & 0xf_ff00)) as u32
        } else {
            0
        },
        link,
        xattrs: xattrs(p)?,
        socket,
    })
}

/// `os.Lstat` on Windows and fsutil's mkstat: Go's 0666, or 0444 read-only, made
/// executable and capped at 0755, as fsutil sends Windows files.
#[cfg(windows)]
pub fn lstat(p: &Path) -> io::Result<Stat> {
    let m = fs::symlink_metadata(p)?;
    let mut mode = if m.permissions().readonly() { 0o444 } else { 0o666 };
    if m.is_dir() {
        mode |= fm::DIR | 0o111;
    }
    if m.file_type().is_symlink() {
        mode |= fm::SYMLINK;
    }
    let perm = ((mode & fm::PERM) | 0o111) & 0o755;
    mode = (mode & !fm::PERM) | perm;
    let link = if m.file_type().is_symlink() {
        fs::read_link(p)?
            .to_string_lossy()
            .replace('\\', "/")
            .into_bytes()
    } else {
        Vec::new()
    };
    Ok(Stat {
        mode,
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: since_epoch(&m),
        inode: None,
        devmajor: 0,
        devminor: 0,
        link,
        xattrs: BTreeMap::new(),
        socket: false,
    })
}

/// fsutil's loadXattr: every attribute but `com.apple.*`; none where the file system
/// has none.
#[cfg(unix)]
fn xattrs(p: &Path) -> io::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))?;
    let mut out = BTreeMap::new();
    let names = match list(&c) {
        Ok(n) => n,
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return Ok(out),
        Err(e) => return Err(io::Error::other(format!("failed to xattr {}: {e}", p.display()))),
    };
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if name.starts_with(b"com.apple.") {
            continue;
        }
        let Ok(cname) = CString::new(name) else { continue };
        if let Ok(v) = get(&c, &cname) {
            out.insert(name.to_vec(), v);
        }
    }
    Ok(out)
}

/// Calls `f` for the size, then for the bytes, as long as the size keeps growing.
#[cfg(unix)]
fn sized(f: &dyn Fn(*mut libc::c_char, usize) -> isize) -> io::Result<Vec<u8>> {
    loop {
        let n = f(std::ptr::null_mut(), 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; n as usize];
        let got = f(buf.as_mut_ptr().cast(), buf.len());
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(e);
        }
        buf.truncate(got as usize);
        return Ok(buf);
    }
}

#[cfg(target_os = "linux")]
fn list(c: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: `c` is NUL-terminated and outlives the call; `buf` holds `len` bytes, or is
    // null with `len` 0, as llistxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::llistxattr(c.as_ptr(), buf, len) })
}

#[cfg(target_os = "linux")]
fn get(c: &std::ffi::CStr, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: both names are NUL-terminated and outlive the call; `buf` holds `len` bytes,
    // or is null with `len` 0, as lgetxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::lgetxattr(c.as_ptr(), name.as_ptr(), buf.cast(), len) })
}

#[cfg(target_os = "macos")]
fn list(c: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: `c` is NUL-terminated and outlives the call; `buf` holds `len` bytes, or is
    // null with `len` 0, as listxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::listxattr(c.as_ptr(), buf, len, libc::XATTR_NOFOLLOW) })
}

#[cfg(target_os = "macos")]
fn get(c: &std::ffi::CStr, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: both names are NUL-terminated and outlive the call; `buf` holds `len` bytes,
    // or is null with `len` 0, as getxattr(2) takes them.
    sized(&|buf, len| unsafe {
        libc::getxattr(
            c.as_ptr(),
            name.as_ptr(),
            buf.cast(),
            len,
            0,
            libc::XATTR_NOFOLLOW,
        )
    })
}

/// What a snapshot of a regular file holds: the mode, size and time its bytes had when
/// taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Go's FileMode bits.
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, u32),
}

/// The smallest file a stage clones rather than packs. Measured on APFS
/// (docs/research/measurements/build-context/clone.py, platform-measurements.md M76): a
/// clone and its removal cost about 200 µs whatever the size, reading a file into the pack
/// 24 µs at 1 KiB and 98 µs at 256 KiB; at 1 MiB they meet, and at 4 MiB the clone is ten
/// times faster.
pub const CLONE_MIN: u64 = 1 << 20;

/// Where a stage put a file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// A clone, or copy, of its own.
    File(PathBuf),
    /// At this offset of the stage's pack.
    Pack(u64),
}

/// A directory private to one build that holds one version of each context file, which
/// no later edit of the file changes. Files under [`CLONE_MIN`] are appended to one pack
/// file, so taking one makes and removes no file; larger ones are copy-on-write clones
/// where the file system makes them (APFS's fclonefileat, Btrfs's and XFS's FICLONE),
/// else copies.
///
/// A source is held open while it is taken, never through a symlink put in its place nor
/// blocking on a FIFO. One that changes while it is taken has no one version to take and
/// is refused, as GNU tar reports "file changed as we read it": its identity, size, mtime
/// and ctime are read before and after, and Linux and macOS move ctime on every write,
/// which nothing can set back. On Windows the source is opened sharing reads only, so no
/// one writes it meanwhile.
#[derive(Debug)]
pub struct Stage {
    dir: PathBuf,
    pack: io::BufWriter<fs::File>,
    len: u64,
    next: u64,
    buf: Vec<u8>,
}

impl Stage {
    /// A stage in `dir`, which exists and is the build's own.
    pub fn new(dir: &Path) -> io::Result<Stage> {
        let pack = fs::File::options()
            .write(true)
            .create_new(true)
            .open(dir.join("pack"))?;
        Ok(Stage {
            dir: dir.to_path_buf(),
            pack: io::BufWriter::with_capacity(1 << 20, pack),
            len: 0,
            next: 0,
            buf: Vec::new(),
        })
    }

    /// The pack's path, to read it from.
    pub fn pack_path(&self) -> PathBuf {
        self.dir.join("pack")
    }

    /// Writes out what the pack holds, for it to be read.
    pub fn finish(mut self) -> io::Result<()> {
        io::Write::flush(&mut self.pack)
    }

    fn changed(src: &Path) -> io::Error {
        io::Error::other(format!("{}: file changed as the build read it", src.display()))
    }

    /// Takes one version of the regular file `src`.
    pub fn take(&mut self, src: &Path) -> io::Result<(Snapshot, Taken)> {
        let (f, before) = open(src)?;
        if !before.is_file() {
            return Err(io::Error::other(format!("{}: not a regular file", src.display())));
        }
        let size = before.len();
        let taken = if size >= CLONE_MIN {
            let dst = self.dir.join(self.next.to_string());
            self.next += 1;
            if !clone(&f, &dst)? {
                copy(&f, &dst)?;
            }
            if !unchanged(&f, &before)? {
                fs::remove_file(&dst)?;
                return Err(Self::changed(src));
            }
            Taken::File(dst)
        } else {
            self.buf.clear();
            io::Read::read_to_end(&mut io::Read::take(&f, size + 1), &mut self.buf)?;
            if self.buf.len() as u64 != size || !unchanged(&f, &before)? {
                return Err(Self::changed(src));
            }
            io::Write::write_all(&mut self.pack, &self.buf)?;
            let at = self.len;
            self.len += size;
            Taken::Pack(at)
        };
        Ok((
            Snapshot {
                mode: mode_of(&before),
                size,
                mtime: since_epoch(&before),
            },
            taken,
        ))
    }
}

#[cfg(unix)]
fn open(src: &Path) -> io::Result<(fs::File, fs::Metadata)> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)?;
    let m = f.metadata()?;
    Ok((f, m))
}

#[cfg(windows)]
fn open(src: &Path) -> io::Result<(fs::File, fs::Metadata)> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ: others may read, not write or delete, while it is held.
    let f = fs::File::options().read(true).share_mode(0x1).open(src)?;
    let m = f.metadata()?;
    Ok((f, m))
}

/// Whether `f` is as `before` was: its identity, size, mtime and ctime.
#[cfg(unix)]
fn unchanged(f: &fs::File, before: &fs::Metadata) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let v = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.size(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    Ok(v(before) == v(&f.metadata()?))
}

/// On Windows no one could write it meanwhile.
#[cfg(windows)]
fn unchanged(_: &fs::File, _: &fs::Metadata) -> io::Result<bool> {
    Ok(true)
}

/// Go's FileMode permission and set-ID bits of a regular file.
#[cfg(unix)]
fn mode_of(m: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    let st = m.mode();
    let mut mode = st & 0o777;
    if st & 0o4000 != 0 {
        mode |= fm::SETUID;
    }
    if st & 0o2000 != 0 {
        mode |= fm::SETGID;
    }
    if st & 0o1000 != 0 {
        mode |= fm::STICKY;
    }
    mode
}

/// Go's 0666, or 0444 read-only, made executable and capped at 0755, as fsutil sends
/// Windows files.
#[cfg(windows)]
fn mode_of(m: &fs::Metadata) -> u32 {
    let mode = if m.permissions().readonly() { 0o444 } else { 0o666 };
    ((mode & fm::PERM) | 0o111) & 0o755
}

/// Copies what `f` holds to a new file `dst`, from its start.
fn copy(f: &fs::File, dst: &Path) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut src = f;
    src.seek(SeekFrom::Start(0))?;
    let mut out = fs::File::options().write(true).create_new(true).open(dst)?;
    io::copy(&mut src, &mut out)?;
    Ok(())
}

/// A copy-on-write clone of `f` at `dst`; false where the file system makes none.
#[cfg(target_os = "macos")]
fn clone(f: &fs::File, dst: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))?;
    // SAFETY: the descriptor is open for the call and `c` is NUL-terminated.
    let r = unsafe { libc::fclonefileat(f.as_raw_fd(), libc::AT_FDCWD, c.as_ptr(), 0) };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(target_os = "linux")]
fn clone(f: &fs::File, dst: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let out = fs::File::options().write(true).create_new(true).open(dst)?;
    // SAFETY: both descriptors are open for the call; FICLONE reads the source's.
    let r = unsafe { libc::ioctl(out.as_raw_fd(), libc::FICLONE, f.as_raw_fd()) };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    drop(out);
    fs::remove_file(dst)?;
    match e.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY) => Ok(false),
        _ => Err(e),
    }
}

/// Windows has no clone shards makes yet: a copy.
#[cfg(windows)]
fn clone(_: &fs::File, _: &Path) -> io::Result<bool> {
    Ok(false)
}

/// `filepath.EvalSymlinks`, as fsutil.NewFS resolves the context's directory.
pub fn eval_symlinks(p: &Path) -> io::Result<std::path::PathBuf> {
    fs::canonicalize(p)
}
