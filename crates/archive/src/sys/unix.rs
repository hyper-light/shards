//! The files beneath, on Linux and macOS: stat, directory listings and the
//! security.capability attribute for packing, and the destination as Go's os.Root holds
//! it for unpacking: a directory descriptor, and every operation an `*at` call on a
//! descriptor the walk opened without following symlinks (go1.26.1 src/os/root_unix.go).

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{FileKind, Stat};
use crate::gopath::Os;
use crate::root::{self, Step, WalkError};
use crate::tar::Time;

pub(crate) fn os_path(p: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(p))
}

pub(crate) fn os_path_buf(p: &[u8]) -> PathBuf {
    os_path(p).to_path_buf()
}

/// A `mode_t` widened: it is u16 on macOS and u32 on Linux.
#[allow(clippy::unnecessary_cast)]
pub(crate) const fn mode32(m: libc::mode_t) -> u32 {
    m as u32
}

pub(crate) const S_IFBLK: u32 = mode32(libc::S_IFBLK);
pub(crate) const S_IFCHR: u32 = mode32(libc::S_IFCHR);
pub(crate) const S_IFIFO: u32 = mode32(libc::S_IFIFO);
pub(crate) const ENOTSUP: i32 = libc::ENOTSUP;
pub(crate) const EPERM: i32 = libc::EPERM;

/// An implied directory's mode set again, with no umask: go-archive opens it and chmods.
pub(crate) fn fix_implied_dir(root: &Root, name: &[u8], mode: u32) -> Result<(), crate::Error> {
    let dir = root
        .open_file(name, libc::O_RDONLY, 0)
        .map_err(|e| e.error("openat", name))?;
    // SAFETY: dir is an open descriptor.
    let r = check(unsafe { libc::fchmod(dir.as_raw_fd(), mode as libc::mode_t) });
    r.map_err(|e| {
        let full = [root.name(), b"/", name].concat();
        crate::Error::path("chmod", &full, &e)
    })
}

pub(crate) fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

fn cstring(p: &[u8]) -> io::Result<CString> {
    CString::new(p).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn kind_of(mode: u32) -> FileKind {
    match mode & mode32(libc::S_IFMT) {
        m if m == mode32(libc::S_IFREG) => FileKind::File,
        m if m == mode32(libc::S_IFDIR) => FileKind::Dir,
        m if m == mode32(libc::S_IFLNK) => FileKind::Symlink,
        m if m == mode32(libc::S_IFCHR) => FileKind::Char,
        m if m == mode32(libc::S_IFBLK) => FileKind::Block,
        m if m == mode32(libc::S_IFIFO) => FileKind::Fifo,
        m if m == mode32(libc::S_IFSOCK) => FileKind::Socket,
        _ => FileKind::Other,
    }
}

/// os.Lstat.
pub(crate) fn lstat(p: &[u8]) -> io::Result<Stat> {
    let md = std::fs::symlink_metadata(os_path(p))?;
    Ok(Stat {
        kind: kind_of(md.mode()),
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
        size: md.size(),
        mtime: Time::unix(md.mtime(), md.mtime_nsec()),
        ino: md.ino(),
        dev: md.dev(),
        nlink: md.nlink(),
        rdev: md.rdev(),
    })
}

/// os.Stat.
pub(crate) fn stat(p: &[u8]) -> io::Result<Stat> {
    let md = std::fs::metadata(os_path(p))?;
    Ok(Stat {
        kind: kind_of(md.mode()),
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
        size: md.size(),
        mtime: Time::unix(md.mtime(), md.mtime_nsec()),
        ino: md.ino(),
        dev: md.dev(),
        nlink: md.nlink(),
        rdev: md.rdev(),
    })
}

/// os.Readlink.
pub(crate) fn readlink(p: &[u8]) -> io::Result<Vec<u8>> {
    Ok(path_bytes(&std::fs::read_link(os_path(p))?))
}

/// The permission, set-id and sticky bits Go's FileMode carries of `st_mode`.
pub(crate) fn perm(st: &Stat) -> i64 {
    i64::from(st.mode & 0o7777)
}

/// unix.Major and unix.Minor (golang.org/x/sys dev_linux.go, dev_darwin.go).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn major_minor(dev: u64) -> (i64, i64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major as i64, minor as i64)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn major_minor(dev: u64) -> (i64, i64) {
    (((dev >> 24) & 0xff) as i64, (dev & 0xff_ffff) as i64)
}

/// unix.Mkdev.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mkdev(major: u32, minor: u32) -> u64 {
    let (major, minor) = (u64::from(major), u64::from(minor));
    ((major & 0x0000_0fff) << 8)
        | ((major & 0xffff_f000) << 32)
        | (minor & 0x0000_00ff)
        | ((minor & 0xffff_ff00) << 12)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mkdev(major: u32, minor: u32) -> u64 {
    (u64::from(major) << 24) | u64::from(minor)
}

/// os.ReadDir: the names in a directory, sorted, and whether each is a directory (never
/// following symlinks). What could be read before an error is kept, as Go keeps it.
pub(crate) fn read_dir(p: &[u8]) -> io::Result<Vec<(Vec<u8>, bool)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(os_path(p))? {
        let Ok(entry) = entry else {
            break;
        };
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push((entry.file_name().as_bytes().to_vec(), is_dir));
    }
    out.sort();
    Ok(out)
}

/// go-archive's lgetxattr (xattr_supported.go): an attribute's value, None where it is
/// unset.
pub(crate) fn lgetxattr(p: &[u8], name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let path = cstring(p)?;
    let name = cstring(name)?;
    let mut buf = vec![0u8; 128];
    loop {
        // SAFETY: path and name are NUL-terminated; buf is valid for buf.len() bytes.
        let n = unsafe { get_xattr(&path, &name, buf.as_mut_ptr(), buf.len()) };
        if n >= 0 {
            buf.truncate(usize::try_from(n).unwrap_or(0));
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(NOATTR) {
            return Ok(None);
        }
        if e.raw_os_error() != Some(libc::ERANGE) {
            return Err(e);
        }
        // SAFETY: a null buffer of size 0 asks for the size.
        let size = unsafe { get_xattr(&path, &name, std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        buf = vec![0u8; usize::try_from(size).unwrap_or(0)];
    }
}

/// What an unset attribute's error is.
#[cfg(any(target_os = "linux", target_os = "android"))]
const NOATTR: i32 = libc::ENODATA;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const NOATTR: i32 = libc::ENOATTR;

/// security.capability, None where it is unset or unreadable, as go-archive ignores the
/// error (ReadSecurityXattrToTarHeader).
pub(crate) fn capability(p: &[u8]) -> Option<Vec<u8>> {
    lgetxattr(p, b"security.capability").ok().flatten()
}

/// fsetxattr.
///
/// # Safety
/// `fd` must be open.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn fsetxattr(fd: RawFd, key: &CStr, value: &[u8]) -> libc::c_int {
    // SAFETY: the caller's contract; key is NUL-terminated, value valid for its length.
    unsafe { libc::fsetxattr(fd, key.as_ptr(), value.as_ptr().cast(), value.len(), 0) }
}

/// fsetxattr.
///
/// # Safety
/// `fd` must be open.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
unsafe fn fsetxattr(fd: RawFd, key: &CStr, value: &[u8]) -> libc::c_int {
    // SAFETY: the caller's contract; key is NUL-terminated, value valid for its length.
    unsafe { libc::fsetxattr(fd, key.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) }
}

/// lgetxattr.
///
/// # Safety
/// `buf` must be valid for `size` bytes, or null with `size` 0.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn get_xattr(path: &CStr, name: &CStr, buf: *mut u8, size: usize) -> isize {
    // SAFETY: the caller's contract, and NUL-terminated strings.
    unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), buf.cast(), size) }
}

/// getxattr without following a final symlink.
///
/// # Safety
/// `buf` must be valid for `size` bytes, or null with `size` 0.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
unsafe fn get_xattr(path: &CStr, name: &CStr, buf: *mut u8, size: usize) -> isize {
    // SAFETY: the caller's contract, and NUL-terminated strings.
    unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.cast(),
            size,
            0,
            libc::XATTR_NOFOLLOW,
        )
    }
}

/// Retries a call EINTR interrupted (Go's ignoringEINTR).
fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            r => return r,
        }
    }
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn openat(dir: RawFd, name: &CStr, flags: libc::c_int, mode: u32) -> io::Result<OwnedFd> {
    retry(|| {
        // SAFETY: name is NUL-terminated; dir is a descriptor the caller holds.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    })
}

fn readlinkat(dir: RawFd, name: &CStr) -> io::Result<Vec<u8>> {
    let mut len = 128;
    loop {
        let mut buf = vec![0u8; len];
        let n = retry(|| {
            // SAFETY: buf is valid for len bytes; name is NUL-terminated.
            let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), len) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n)
        })?;
        let n = usize::try_from(n).unwrap_or(0);
        if n < len {
            buf.truncate(n);
            return Ok(buf);
        }
        len = len
            .checked_mul(2)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENAMETOOLONG))?;
    }
}

fn fstatat(dir: RawFd, name: &CStr) -> io::Result<Stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
    retry(|| {
        // SAFETY: st is valid for a stat; name is NUL-terminated.
        check(unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) })
    })?;
    // SAFETY: fstatat succeeded and filled it; it was zeroed before.
    let st = unsafe { st.assume_init() };
    let mode = mode32(st.st_mode);
    #[allow(clippy::unnecessary_cast)]
    Ok(Stat {
        kind: kind_of(mode),
        mode,
        uid: st.st_uid,
        gid: st.st_gid,
        size: u64::try_from(st.st_size).unwrap_or(0),
        mtime: Time::unix(st.st_mtime as i64, st.st_mtime_nsec as i64),
        ino: st.st_ino as u64,
        dev: st.st_dev as u64,
        nlink: st.st_nlink as u64,
        rdev: st.st_rdev as u64,
    })
}

/// checkSymlink: a symlink becomes a step to its target; anything else keeps `orig`.
fn check_symlink<T>(dir: RawFd, name: &CStr, orig: io::Error) -> io::Result<Step<T>> {
    match readlinkat(dir, name) {
        Ok(target) => Ok(Step::Link(target)),
        Err(_) => Err(orig),
    }
}

/// isNoFollowErr: what openat gives for a symlink under O_NOFOLLOW.
fn no_follow(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

/// A directory in the walk: the root's own descriptor, or one the walk opened.
#[derive(Debug)]
pub(crate) enum Handle {
    Root(RawFd),
    Owned(OwnedFd),
}

impl Handle {
    fn fd(&self) -> RawFd {
        match self {
            Handle::Root(fd) => *fd,
            Handle::Owned(fd) => fd.as_raw_fd(),
        }
    }
}

/// rootOpenDir.
fn open_dir(dir: &Handle, name: &[u8]) -> io::Result<Step<Handle>> {
    let c = cstring(name)?;
    match openat(
        dir.fd(),
        &c,
        libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_RDONLY,
        0,
    ) {
        Ok(fd) => Ok(Step::Done(Handle::Owned(fd))),
        Err(e) if no_follow(&e) || e.raw_os_error() == Some(libc::ENOTDIR) => check_symlink(dir.fd(), &c, e),
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) || e.raw_os_error() == Some(libc::EOPNOTSUPP) => {
            Err(io::Error::from_raw_os_error(libc::ENOTDIR))
        }
        Err(e) => Err(e),
    }
}

/// The destination of an unpack: Go's os.Root.
#[derive(Debug)]
pub(crate) struct Root {
    name: Vec<u8>,
    fd: OwnedFd,
}

impl Root {
    /// os.OpenRoot.
    pub(crate) fn open(name: &[u8]) -> Result<Root, crate::Error> {
        let c = cstring(name).map_err(|e| crate::Error::path("open", name, &e))?;
        let fd = openat(libc::AT_FDCWD, &c, libc::O_RDONLY, 0)
            .map_err(|e| crate::Error::path("open", name, &e))?;
        let st = fstatat(fd.as_raw_fd(), c".").map_err(|e| crate::Error::path("open", name, &e))?;
        if st.kind != FileKind::Dir {
            return Err(crate::Error::other(format!(
                "open {}: not a directory",
                String::from_utf8_lossy(name)
            )));
        }
        Ok(Root {
            name: name.to_vec(),
            fd,
        })
    }

    pub(crate) fn name(&self) -> &[u8] {
        &self.name
    }

    fn walk<T>(
        &self,
        name: &[u8],
        mut f: impl FnMut(RawFd, &CStr) -> io::Result<Step<T>>,
    ) -> Result<T, WalkError> {
        let root = self.fd.as_raw_fd();
        root::walk(
            Os::Unix,
            || Handle::Root(root),
            name,
            open_dir,
            |dir, last| f(dir.fd(), &cstring(last)?),
        )
    }

    /// Opens `dir` as root.OpenFile(dir, O_RDONLY) does, following a final symlink inside
    /// the root, then runs `f` on it and `base`: go-archive's operations on a parent.
    fn in_dir<T>(&self, dir: &[u8], flags: libc::c_int, f: impl FnOnce(RawFd) -> T) -> Result<T, WalkError> {
        let fd = self.walk(dir, |parent, last| {
            match openat(parent, last, libc::O_NOFOLLOW | flags, 0) {
                Ok(fd) => Ok(Step::Done(fd)),
                Err(e) if no_follow(&e) || e.raw_os_error() == Some(libc::ENOTDIR) => {
                    check_symlink(parent, last, e)
                }
                Err(e) => Err(e),
            }
        })?;
        Ok(f(fd.as_raw_fd()))
    }

    /// Root.Lstat.
    pub(crate) fn lstat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| fstatat(dir, last).map(Step::Done))
    }

    /// Root.Stat: a final symlink followed inside the root.
    pub(crate) fn stat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| {
            let st = fstatat(dir, last)?;
            if st.kind == FileKind::Symlink {
                return check_symlink(dir, last, io::Error::from_raw_os_error(libc::ELOOP));
            }
            Ok(Step::Done(st))
        })
    }

    /// Root.Mkdir.
    pub(crate) fn mkdir(&self, name: &[u8], perm: u32) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            // SAFETY: last is NUL-terminated.
            retry(|| check(unsafe { libc::mkdirat(dir, last.as_ptr(), perm as libc::mode_t) }))
                .map(Step::Done)
        })
    }

    /// Root.OpenFile: a final symlink followed inside the root unless creating
    /// exclusively.
    pub(crate) fn open_file(&self, name: &[u8], flags: libc::c_int, perm: u32) -> Result<File, WalkError> {
        let excl = flags & (libc::O_CREAT | libc::O_EXCL) == (libc::O_CREAT | libc::O_EXCL);
        let fd = self.walk(name, |dir, last| {
            match openat(dir, last, libc::O_NOFOLLOW | flags, perm) {
                Ok(fd) => Ok(Step::Done(fd)),
                Err(e) if !excl && (no_follow(&e) || e.raw_os_error() == Some(libc::ENOTDIR)) => {
                    check_symlink(dir, last, e)
                }
                Err(e) if excl && no_follow(&e) => Err(io::Error::from_raw_os_error(libc::EEXIST)),
                Err(e) => Err(e),
            }
        })?;
        Ok(File::from(fd))
    }

    /// Root.OpenFile(name, O_CREATE|O_WRONLY|O_TRUNC, perm).
    pub(crate) fn create(&self, name: &[u8], perm: u32) -> Result<File, WalkError> {
        self.open_file(name, libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC, perm)
    }

    /// Root.Lchown. Ids are passed as Go passes an int to the call: truncated to 32 bits,
    /// so -1 leaves one unchanged.
    pub(crate) fn lchown(&self, name: &[u8], uid: i64, gid: i64) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            retry(|| {
                // SAFETY: last is NUL-terminated.
                check(unsafe {
                    libc::fchownat(
                        dir,
                        last.as_ptr(),
                        uid as u32,
                        gid as u32,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })
            })
            .map(Step::Done)
        })
    }

    /// Root.Chtimes: a final symlink followed inside the root.
    pub(crate) fn chtimes(&self, name: &[u8], atime: Time, mtime: Time) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            if let Ok(target) = readlinkat(dir, last) {
                return Ok(Step::Link(target));
            }
            utimensat(dir, last, atime, mtime).map(Step::Done)
        })
    }

    /// go-archive's lchtimes: the times of a symlink itself, through its parent.
    pub(crate) fn lchtimes(
        &self,
        dir: &[u8],
        base: &[u8],
        atime: Time,
        mtime: Time,
    ) -> Result<io::Result<()>, WalkError> {
        let base = cstring(base)?;
        self.in_dir(dir, libc::O_RDONLY, |fd| {
            match utimensat(fd, &base, atime, mtime) {
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => Ok(()),
                r => r,
            }
        })
    }

    /// Root.Symlink: the target stored as it is.
    pub(crate) fn symlink(&self, target: &[u8], name: &[u8]) -> Result<(), WalkError> {
        let t = cstring(target)?;
        self.walk(name, |dir, last| {
            // SAFETY: both strings are NUL-terminated.
            retry(|| check(unsafe { libc::symlinkat(t.as_ptr(), dir, last.as_ptr()) })).map(Step::Done)
        })
    }

    /// Root.Link: both names walked inside the root; a final symlink of `old` is linked
    /// itself.
    pub(crate) fn link(&self, old: &[u8], new: &[u8]) -> Result<(), WalkError> {
        self.walk(old, |odir, olast| {
            let r = self.walk(new, |ndir, nlast| {
                // SAFETY: both names are NUL-terminated.
                retry(|| check(unsafe { libc::linkat(odir, olast.as_ptr(), ndir, nlast.as_ptr(), 0) }))
                    .map(Step::Done)
            });
            match r {
                Ok(()) => Ok(Step::Done(Ok(()))),
                Err(e) => Ok(Step::Done(Err(e))),
            }
        })?
    }

    /// Root.RemoveAll: nothing followed; a missing name is no error.
    pub(crate) fn remove_all(&self, name: &[u8]) -> Result<(), WalkError> {
        let mut name = name;
        while let Some(n) = name.strip_suffix(b"/") {
            name = n;
        }
        match self.walk(name, |dir, last| remove_all_at(dir, last).map(Step::Done)) {
            Err(e) if e.is_not_found() => Ok(()),
            r => r,
        }
    }

    /// mknodInRoot: on Linux mknodat in the parent; on macOS, which lacks it before 13, a
    /// path to the parent's descriptor, as go-archive's dev_darwin.go makes a path.
    pub(crate) fn mknod(
        &self,
        dir: &[u8],
        base: &[u8],
        mode: u32,
        major: u32,
        minor: u32,
    ) -> Result<io::Result<()>, WalkError> {
        let base = cstring(base)?;
        let dev = mkdev(major, minor);
        self.in_dir(dir, libc::O_RDONLY | libc::O_DIRECTORY, |fd| {
            mknod_in(fd, &base, mode, dev)
        })
    }

    /// chmodNoSymlink: fchmodat in the parent without following the name; where the
    /// system cannot, through a descriptor of the name itself.
    pub(crate) fn chmod_nofollow(
        &self,
        dir: &[u8],
        base: &[u8],
        perm: u32,
    ) -> Result<io::Result<()>, WalkError> {
        let base = cstring(base)?;
        self.in_dir(dir, libc::O_RDONLY, |fd| {
            let r = retry(|| {
                // SAFETY: base is NUL-terminated.
                check(unsafe {
                    libc::fchmodat(fd, base.as_ptr(), perm as libc::mode_t, libc::AT_SYMLINK_NOFOLLOW)
                })
            });
            match r {
                Err(e)
                    if e.raw_os_error() == Some(libc::EOPNOTSUPP)
                        || e.raw_os_error() == Some(libc::ENOTSUP) =>
                {
                    chmod_fallback(fd, &base, perm)
                }
                r => r,
            }
        })
    }

    /// An attribute of the directory `dir` itself, set through a descriptor of it
    /// (overlayWhiteoutConverter.ConvertRead's fsetxattr).
    pub(crate) fn set_xattr_dir(
        &self,
        dir: &[u8],
        key: &[u8],
        value: &[u8],
    ) -> Result<io::Result<()>, WalkError> {
        let key = cstring(key)?;
        self.in_dir(dir, libc::O_RDONLY, |fd| {
            retry(|| {
                // SAFETY: key is NUL-terminated; value is valid for its length.
                check(unsafe { fsetxattr(fd, &key, value) })
            })
        })
    }

    /// fchownat in the parent, never following the name.
    pub(crate) fn lchown_in(
        &self,
        dir: &[u8],
        base: &[u8],
        uid: i64,
        gid: i64,
    ) -> Result<io::Result<()>, WalkError> {
        let base = cstring(base)?;
        self.in_dir(dir, libc::O_RDONLY, |fd| {
            retry(|| {
                // SAFETY: base is NUL-terminated. Ids are truncated as Go passes an int.
                check(unsafe {
                    libc::fchownat(
                        fd,
                        base.as_ptr(),
                        uid as u32,
                        gid as u32,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })
            })
        })
    }

    /// lsetxattr of the entry `base` in `dir`, never following it.
    pub(crate) fn set_xattr(
        &self,
        dir: &[u8],
        base: &[u8],
        key: &[u8],
        value: &[u8],
    ) -> Result<io::Result<()>, WalkError> {
        let key = cstring(key)?;
        let base = base.to_vec();
        self.in_dir(dir, libc::O_RDONLY, |fd| set_xattr_in(fd, &base, &key, value))
    }
}

fn timespec(t: Time) -> libc::timespec {
    libc::timespec {
        tv_sec: t.sec as _,
        tv_nsec: libc::c_long::from(i32::try_from(t.nsec).unwrap_or(0)),
    }
}

fn utimensat(dir: RawFd, name: &CStr, atime: Time, mtime: Time) -> io::Result<()> {
    let times = [timespec(atime), timespec(mtime)];
    retry(|| {
        // SAFETY: times holds two timespecs; name is NUL-terminated.
        check(unsafe { libc::utimensat(dir, name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) })
    })
}

/// removeAllFrom (go1.26.1 src/os/removeall_at.go): unlink, or empty the directory and
/// remove it, never following a symlink.
fn remove_all_at(dir: RawFd, name: &CStr) -> io::Result<()> {
    if name.to_bytes() == b"." || name.to_bytes() == b".." {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: name is NUL-terminated.
    let unlinked = retry(|| check(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) }));
    let err = match unlinked {
        Ok(()) => return Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
        Err(e) => e,
    };
    if !matches!(
        err.raw_os_error(),
        Some(libc::EISDIR | libc::EPERM | libc::EACCES)
    ) {
        return Err(err);
    }
    let sub = match openat(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => return Err(err),
        Err(e) => return Err(e),
    };
    for entry in names(&sub)? {
        remove_all_at(sub.as_raw_fd(), &entry)?;
    }
    // SAFETY: name is NUL-terminated.
    match retry(|| check(unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) })) {
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        r => r,
    }
}

/// The names in an open directory, but `.` and `..`.
fn names(dir: &OwnedFd) -> io::Result<Vec<CString>> {
    let dup = openat(dir.as_raw_fd(), c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    // SAFETY: fdopendir takes the descriptor, which closedir closes.
    let d = unsafe { libc::fdopendir(dup.as_raw_fd()) };
    if d.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(dup);
    let mut out = Vec::new();
    loop {
        // SAFETY: d is an open directory stream.
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            break;
        }
        // SAFETY: readdir returned an entry whose d_name is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            out.push(name.to_owned());
        }
    }
    // SAFETY: d is open, and closed once.
    unsafe { libc::closedir(d) };
    Ok(out)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn mknod_in(dir: RawFd, name: &CStr, mode: u32, dev: u64) -> io::Result<()> {
    // SAFETY: name is NUL-terminated.
    retry(|| check(unsafe { libc::mknodat(dir, name.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) }))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mknod_in(dir: RawFd, name: &CStr, mode: u32, dev: u64) -> io::Result<()> {
    let path = cstring(&fd_path(dir, name.to_bytes())?)?;
    // SAFETY: path is NUL-terminated.
    retry(|| check(unsafe { libc::mknod(path.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) }))
}

/// chmodNoSymlinkFallback (chmod_linux.go): the name opened as a path, never followed,
/// and changed through /proc/self/fd.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn chmod_fallback(dir: RawFd, name: &CStr, perm: u32) -> io::Result<()> {
    let fd = openat(dir, name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
    let proc = cstring(format!("/proc/self/fd/{}", fd.as_raw_fd()).as_bytes())?;
    // SAFETY: proc is NUL-terminated.
    retry(|| check(unsafe { libc::chmod(proc.as_ptr(), perm as libc::mode_t) }))
}

/// chmodNoSymlinkFallback (chmod_unix_nolinux.go): the name opened, never followed, and
/// changed through its descriptor.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn chmod_fallback(dir: RawFd, name: &CStr, perm: u32) -> io::Result<()> {
    let fd = openat(dir, name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK, 0)?;
    // SAFETY: fd is open.
    retry(|| check(unsafe { libc::fchmod(fd.as_raw_fd(), perm as libc::mode_t) }))
}

/// lsetxattr through /proc/self/fd/DIR/NAME: the parent is the descriptor the walk
/// opened, so no rename can move the name elsewhere.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_xattr_in(dir: RawFd, name: &[u8], key: &CStr, value: &[u8]) -> io::Result<()> {
    let path = cstring(&[format!("/proc/self/fd/{dir}/").as_bytes(), name].concat())?;
    retry(|| {
        // SAFETY: strings are NUL-terminated; value is valid for its length.
        check(unsafe { libc::lsetxattr(path.as_ptr(), key.as_ptr(), value.as_ptr().cast(), value.len(), 0) })
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_xattr_in(dir: RawFd, name: &[u8], key: &CStr, value: &[u8]) -> io::Result<()> {
    let path = cstring(&fd_path(dir, name)?)?;
    retry(|| {
        // SAFETY: strings are NUL-terminated; value is valid for its length.
        check(unsafe {
            libc::setxattr(
                path.as_ptr(),
                key.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        })
    })
}

/// The path of `name` in the directory `dir` holds, by F_GETPATH.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn fd_path(dir: RawFd, name: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated.
    check(unsafe { libc::fcntl(dir, libc::F_GETPATH, buf.as_mut_ptr()) })?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf.truncate(end);
    buf.push(b'/');
    buf.extend_from_slice(name);
    Ok(buf)
}
