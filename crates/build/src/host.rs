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
/// blocking on a FIFO, and what is taken is one version it had, or it is refused, as GNU
/// tar reports "file changed as we read it"; BuildKit's copy takes whatever it reads.
/// Reads and writes interleave on ext4 and APFS alike, so a read can take part of two
/// versions (PM M100): writers are kept off where the system lets a process do so, and
/// any write while the file is taken is seen where it does not ([`settle`]):
/// - Linux keeps them off with a read lease. Without one, the file's ctime shows any
///   write begun since it was looked at, once its tick has passed and a write under way
///   has finished.
/// - macOS lets no process but Apple's take a lease. APFS stamps a write's times as it
///   ends, which misses only a write that spans the whole read, one far longer than the
///   files read here (PM M100), and its clones of larger files take one version.
/// - Windows: the source is opened sharing reads only, so no one writes it meanwhile.
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
        let f = open(src)?;
        let before = settle(&f, src)?;
        let meta = &before.meta;
        if !meta.is_file() {
            return Err(io::Error::other(format!("{}: not a regular file", src.display())));
        }
        let size = meta.len();
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
                mode: mode_of(meta),
                size,
                mtime: since_epoch(meta),
            },
            taken,
        ))
    }
}

#[cfg(unix)]
fn open(src: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
}

#[cfg(windows)]
fn open(src: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ: others may read, not write or delete, while it is held.
    fs::File::options().read(true).share_mode(0x1).open(src)
}

/// What a source was as its taking began, which it must still be once taken.
struct Before {
    meta: fs::Metadata,
}

/// Readies `f` to be taken, and says what it was. A read lease, which Linux grants while
/// no one has the file open for writing (fs/locks.c, lease_open_conflict), makes one who
/// opens it for writing, or truncates it, wait until the lease goes with `f` (fs/open.c,
/// break_lease): what is read is one version.
///
/// Without one, a write while `f` is taken must show in its ctime, which Linux stamps as
/// a write begins, holding the inode's lock until its last byte is in place (fs/ext4/
/// file.c, ext4_buffered_write_iter and ext4_write_checks). A ctime still in the tick of
/// the coarse clock it is stamped from (fs/inode.c, current_time) is waited past, as a
/// write stamped in that tick too would leave it as it was; and lseek(2) SEEK_DATA, which
/// takes the inode's lock shared on ext4, Btrfs and tmpfs (and overlayfs's, theirs),
/// waits out a write under way, as XFS's reads take its own. A file written again while
/// its tick is waited past is refused.
#[cfg(target_os = "linux")]
fn settle(f: &fs::File, src: &Path) -> io::Result<Before> {
    use std::os::unix::fs::MetadataExt;
    if lease(f) {
        return Ok(Before { meta: f.metadata()? });
    }
    for _ in 0..2 {
        let now = coarse_now()?;
        let meta = f.metadata()?;
        let (ctime, grain) = (nanos(meta.ctime(), meta.ctime_nsec()), grain(meta.ctime_nsec()));
        match unseen_until(ctime, grain, now) {
            None => {
                barrier(f)?;
                return Ok(Before { meta });
            }
            Some(after) => {
                while coarse_now()? < after {
                    std::thread::sleep(std::time::Duration::from_nanos(
                        u64::try_from(after - coarse_now()?).unwrap_or(0),
                    ));
                }
            }
        }
    }
    Err(Stage::changed(src))
}

/// F_SETSIG, the signal a lease's break is told with (include/uapi/asm-generic/fcntl.h,
/// which x86_64 and arm64 take): the libc crate has it for none of shards' targets.
#[cfg(target_os = "linux")]
const F_SETSIG: libc::c_int = 10;

/// Takes a read lease on `f`, if Linux grants one. Its break is told with SIGURG, which
/// a process ignores unless it handles it, rather than SIGIO, which ends one that does
/// not: the lease goes as soon as the file is taken, and the writer waiting with it.
#[cfg(target_os = "linux")]
fn lease(f: &fs::File) -> bool {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: fcntl(2) on a descriptor `f` holds open.
    unsafe {
        libc::fcntl(fd, F_SETSIG, libc::SIGURG) == 0 && libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) == 0
    }
}

/// Waits out a write of `f` under way: lseek(2) SEEK_DATA, under the inode's lock, which
/// the write holds; the offset is put back.
#[cfg(target_os = "linux")]
fn barrier(f: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: lseek(2) on a descriptor `f` holds open. What SEEK_DATA finds, an offset or
    // ENXIO in a file of holes alone, is not wanted.
    unsafe {
        libc::lseek(fd, 0, libc::SEEK_DATA);
        if libc::lseek(fd, 0, libc::SEEK_SET) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// CLOCK_REALTIME_COARSE, the clock Linux stamps file times from, in nanoseconds.
#[cfg(target_os = "linux")]
fn coarse_now() -> io::Result<i128> {
    // SAFETY: an all-zero timespec is valid; clock_gettime(2) fills it.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: as above, into a timespec of ours.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(nanos(t.tv_sec, t.tv_nsec))
}

#[cfg(target_os = "linux")]
fn nanos(sec: i64, nsec: i64) -> i128 {
    i128::from(sec) * 1_000_000_000 + i128::from(nsec)
}

/// The coarsest stamp a file system could have given a time of `nsec`: the largest power
/// of ten that divides it, a second where it is 0. ext4's 128-byte inodes keep seconds
/// alone, FAT's ctime hundredths (fs/ext4/super.c, fs/fat/inode.c, s_time_gran).
#[cfg(target_os = "linux")]
fn grain(nsec: i64) -> i128 {
    if nsec == 0 {
        return 1_000_000_000;
    }
    let (mut n, mut g) = (nsec, 1i128);
    while n % 10 == 0 {
        n /= 10;
        g *= 10;
    }
    g
}

/// When a write could leave a ctime of `ctime`, stamped in steps of `grain`, as it was,
/// the coarse clock being at `now`: until the clock leaves its step, if it is in it or
/// within a second ahead of it (a step back of the clock leaves times further ahead,
/// which a write now would change); `None` where a write now changes it.
#[cfg(target_os = "linux")]
fn unseen_until(ctime: i128, grain: i128, now: i128) -> Option<i128> {
    let step = now - now.rem_euclid(grain);
    (ctime >= step && ctime - step <= 1_000_000_000).then(|| ctime - ctime.rem_euclid(grain) + grain)
}

/// macOS keeps leases for Apple's own processes (bsd/vfs/vfs_subr.c, vnode_setlease: the
/// private entitlement com.apple.private.vfs.file-leases): what the file was is all.
#[cfg(target_os = "macos")]
fn settle(f: &fs::File, _: &Path) -> io::Result<Before> {
    Ok(Before { meta: f.metadata()? })
}

/// On Windows no one writes it while it is open (`open`).
#[cfg(windows)]
fn settle(f: &fs::File, _: &Path) -> io::Result<Before> {
    Ok(Before { meta: f.metadata()? })
}

/// Whether `f` is as `before` was: its identity, size, mtime and ctime.
#[cfg(unix)]
fn unchanged(f: &fs::File, before: &Before) -> io::Result<bool> {
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
    Ok(v(&before.meta) == v(&f.metadata()?))
}

/// On Windows no one could write it meanwhile.
#[cfg(windows)]
fn unchanged(_: &fs::File, _: &Before) -> io::Result<bool> {
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

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    /// A file of `bytes` in a directory of the test's own.
    fn file(name: &str, bytes: &[u8]) -> (shards_testdir::TempDir, PathBuf) {
        let dir = shards_testdir::TempDir::new(&format!("host-{name}")).unwrap();
        let path = dir.join("f");
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn a_times_grain_is_the_power_of_ten_it_could_have_been_stamped_in() {
        assert_eq!(grain(0), 1_000_000_000);
        assert_eq!(grain(123_456_789), 1);
        assert_eq!(grain(120_000_000), 10_000_000);
        assert_eq!(grain(500_000_000), 100_000_000);
    }

    /// A ctime is waited past while a write now could leave it as it is: in the coarse
    /// clock's step, or a little ahead of it; not once the clock is past its step, nor
    /// far ahead of the clock, as after the clock was set back.
    #[test]
    fn a_ctime_is_waited_past_only_while_a_write_could_leave_it() {
        let s = 1_000_000_000i128;
        // Nanosecond stamps: unseen until the clock passes it.
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 5), Some(100 * s + 6));
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 4), Some(100 * s + 6));
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 6), None);
        // Second stamps: unseen until the clock's second is past it.
        assert_eq!(unseen_until(100 * s, s, 100 * s + 999), Some(101 * s));
        assert_eq!(unseen_until(100 * s, s, 101 * s), None);
        // Far ahead of the clock: a write now stamps an earlier time.
        assert_eq!(unseen_until(200 * s, 1, 100 * s), None);
    }

    /// A read lease is granted where no one has the file open for writing, and one who
    /// opens it for writing meanwhile waits until it goes; none is granted while one has.
    #[test]
    fn a_lease_keeps_writers_off_while_it_is_held() {
        let (_dir, path) = file("lease", b"one");
        let f = open(&path).unwrap();
        assert!(lease(&f), "{}", io::Error::last_os_error());
        let held = std::time::Duration::from_millis(200);
        // Timed from before the writer starts: the lease goes no sooner than `held` after
        // it, where the writer's own start may come later.
        let start = std::time::Instant::now();
        let opened = std::thread::scope(|s| {
            let writer = s.spawn(|| {
                let w = fs::File::options().write(true).open(&path).unwrap();
                (start.elapsed(), w)
            });
            std::thread::sleep(held);
            drop(f);
            writer.join().unwrap()
        });
        assert!(opened.0 >= held, "the writer opened it in {:?}", opened.0);
        let f = open(&path).unwrap();
        assert!(!lease(&f), "granted while a writer has it open");
        drop(opened.1);
        assert!(lease(&f), "{}", io::Error::last_os_error());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
