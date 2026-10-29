//! The share server's calls of a Unix host's file system (D197): each a function of the
//! server's own, so that the server above them is its host's alike everywhere.

use std::os::fd::FromRawFd as _;

use super::*;

/// The flags a node is opened with for its metadata, never following it: on macOS a
/// symlink opened itself (O_SYMLINK).
#[cfg(target_os = "macos")]
pub(super) const META_FLAGS: host::c_int =
    host::O_RDONLY | host::O_NOFOLLOW | host::O_NONBLOCK | host::O_NOCTTY | libc::O_SYMLINK;
#[cfg(target_os = "linux")]
pub(super) const META_FLAGS: host::c_int =
    host::O_RDONLY | host::O_NOFOLLOW | host::O_NONBLOCK | host::O_NOCTTY;

/// Whether an open of a device node runs its driver's: on Unix it does, a serial line's
/// raising DTR, so that the server opens none (`open_meta`).
pub(super) const DEVICE_OPENS_DRIVER: bool = true;

/// Whether a file's mode refuses opens of it: on Unix it does, the share process its files'
/// owner and unprivileged, so that a descriptor is kept against a mode the guest sets
/// (`refuses`).
pub(super) const MODES_REFUSE_OPENS: bool = true;

/// Whether a directory is renamed only where no handle beneath it is open: on Unix none is
/// needed.
pub(super) const RENAME_NEEDS_NONE_BENEATH: bool = false;

/// A file holding `bytes`, made in directory `dir` and removed from it at once: the
/// descriptor of a file no path reaches, as its guest held one before its save (D176).
pub(super) fn ghost(dir: host::Raw, bytes: &[u8]) -> Result<host::Owned, String> {
    let mut random = [0u8; 16];
    crate::platform::fill_random(&mut random).map_err(|e| format!("a removed file's name: {e}"))?;
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let name = CString::new(format!("{GHOST_PREFIX}{name}")).map_err(|_| "a removed file's name")?;
    let fd = open_at(
        dir,
        &name,
        host::O_RDWR | host::O_CREAT | host::O_EXCL | host::O_CLOEXEC,
        0o600,
    )
    .map_err(|e| format!("a removed file, made again: errno {e}"))?;
    // SAFETY: unlinkat(2) of the name just made in a directory we hold.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
        return Err(format!(
            "a removed file, made again: {}",
            std::io::Error::last_os_error()
        ));
    }
    let file = std::fs::File::from(fd);
    crate::platform::write_all_at(&file, bytes, 0).map_err(|e| format!("a removed file's bytes: {e}"))?;
    Ok(file.into())
}

pub(super) fn last() -> Errno {
    host::linux_errno(
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

/// `fd`'s attribute `name`, or none.
pub(super) fn fget_xattr(fd: host::Raw, name: &CStr) -> Result<Option<Vec<u8>>, Errno> {
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: fgetxattr(2) into a buffer of its length.
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::fgetxattr(fd, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf.truncate(usize::try_from(n).unwrap_or(0));
            return Ok(Some(buf));
        }
        match last() {
            ENODATA => return Ok(None),
            ERANGE if buf.len() < 1 << 16 => buf.resize(buf.len() * 4, 0),
            e => return Err(e),
        }
    }
}

pub(super) fn fset_xattr(fd: host::Raw, name: &CStr, value: &[u8], flags: u32) -> Result<(), Errno> {
    let mut host_flags = 0;
    if flags & linux::XATTR_CREATE != 0 {
        host_flags |= libc::XATTR_CREATE;
    }
    if flags & linux::XATTR_REPLACE != 0 {
        host_flags |= libc::XATTR_REPLACE;
    }
    // SAFETY: fsetxattr(2) of a buffer of its length.
    #[cfg(target_os = "macos")]
    let r = unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            host_flags,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let r = unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), host_flags) };
    if r != 0 {
        return Err(last());
    }
    Ok(())
}

/// The name the host keeps a guest's ownership under: Docker Desktop's on macOS, which
/// keeps no namespaces; in Linux's `user.` namespace on Linux.
pub(super) fn owner_xattr() -> Result<CString, Errno> {
    #[cfg(target_os = "macos")]
    {
        cstring(OWNER_XATTR)
    }
    #[cfg(target_os = "linux")]
    {
        cstring(&format!("user.{OWNER_XATTR}"))
    }
}

pub(super) fn stat_fd(fd: host::Raw) -> Result<Stat, Errno> {
    // SAFETY: an all-zero stat is a valid out-parameter for fstat(2).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor we hold.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(last());
    }
    Ok(Stat::from(&st))
}

pub(super) fn stat_at(dir: host::Raw, name: &CStr) -> Result<Stat, Errno> {
    // SAFETY: as stat_fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstatat(2) of a NUL-terminated name in a directory we hold, not following it.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, host::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(last());
    }
    Ok(Stat::from(&st))
}

/// What the symlink `name` in `dir` holds (readlinkat(2)).
pub(super) fn readlink_at(dir: host::Raw, name: &CStr) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: readlinkat(2) into a buffer of its length.
    let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(last());
    }
    buf.truncate(usize::try_from(n).unwrap_or(0));
    Ok(buf)
}

/// A symlink `name` in `dir` holding `target` (symlinkat(2)).
pub(super) fn symlink_at(target: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: symlinkat(2) of NUL-terminated strings in a directory we hold.
    if unsafe { libc::symlinkat(target.as_ptr(), dir, name.as_ptr()) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// A node `name` in `dir` of `mode` (its kind and permissions) and the guest's device
/// number `rdev` (mknodat(2)).
pub(super) fn mknod_at(dir: host::Raw, name: &CStr, mode: u32, rdev: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: mknodat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::mknodat(dir, name.as_ptr(), mode as host::mode_t, host::decode_dev(rdev)) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// A directory `name` in `dir` of the host's permissions `perm` (mkdirat(2)).
pub(super) fn mkdir_at(dir: host::Raw, name: &CStr, perm: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: mkdirat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), perm as host::mode_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// Removes `name` from `dir`, a directory if `rmdir` (unlinkat(2)).
pub(super) fn unlink_at(dir: host::Raw, name: &CStr, rmdir: bool) -> Result<(), Errno> {
    let flags = if rmdir { host::AT_REMOVEDIR } else { 0 };
    // SAFETY: unlinkat(2) of a NUL-terminated name in a directory we hold.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// Another name, `name` in `dir`, of `oname` in `odir` (linkat(2)).
pub(super) fn link_at(odir: host::Raw, oname: &CStr, dir: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: linkat(2) of NUL-terminated names in directories we hold.
    if unsafe { libc::linkat(odir, oname.as_ptr(), dir, name.as_ptr(), 0) } != 0 {
        return Err(last());
    }
    Ok(())
}

pub(super) fn open_at(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    mode: host::c_uint,
) -> Result<host::Owned, Errno> {
    #[cfg(test)]
    OPENED.with(|o| o.borrow_mut().push(name.to_owned()));
    // SAFETY: openat(2) of a NUL-terminated name in a directory we hold.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | host::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(last());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { host::Owned::from_raw_fd(fd) })
}

/// `name` in `dir` opened for a guest's OPEN or CREATE, with `flags` (`host::open_flags`'),
/// if it is a regular file. A guest kernel opens FIFOs, sockets and device nodes itself and
/// sends no FUSE request to; one that asks would have this process wait on a FIFO's other
/// end for good, or open a host device, so they are refused (EBADF), as QEMU's virtiofsd
/// refuses them (`lo_inode_open`, CVE-2020-35517; audit V02). The name is looked at first,
/// so no special file is opened, then opened without waiting and looked at again, for one
/// put there meanwhile.
pub(super) fn open_regular(
    dir: host::Raw,
    name: &CStr,
    flags: host::c_int,
    mode: host::c_uint,
) -> Result<(host::Owned, Stat), Errno> {
    let exclusive = flags & (host::O_CREAT | host::O_EXCL) == host::O_CREAT | host::O_EXCL;
    match stat_at(dir, name) {
        // An exclusive create of a name that exists fails at the open, opening nothing.
        Ok(st) if !exclusive && mode_of(&st) & S_IFMT != S_IFREG => return Err(EBADF),
        Ok(_) => {}
        Err(ENOENT) if flags & host::O_CREAT != 0 => {}
        Err(e) => return Err(e),
    }
    let fd = open_at(dir, name, flags | host::O_NONBLOCK | host::O_NOCTTY, mode)?;
    let st = stat_fd(fd.raw())?;
    if mode_of(&st) & S_IFMT != S_IFREG {
        return Err(EBADF);
    }
    // The status flags the guest asked for, without O_NONBLOCK: F_SETFL takes those of
    // `flags` and ignores its access mode and creation flags (POSIX fcntl(2)).
    // SAFETY: fcntl(2) of a descriptor we hold.
    if flags & host::O_NONBLOCK == 0 && unsafe { libc::fcntl(fd.raw(), libc::F_SETFL, flags) } != 0 {
        return Err(last());
    }
    Ok((fd, st))
}

/// Changes the mode of `name` in `dir` without following it. fchmodat(2) without
/// AT_SYMLINK_NOFOLLOW follows a final symlink: a guest's link to a host file outside the
/// share would have that file's mode changed (audit V01). A symlink's own mode is not
/// changed, as Linux changes none (EOPNOTSUPP: fs/attr.c `notify_change` since 6.6, and
/// glibc's and musl's fchmodat with AT_SYMLINK_NOFOLLOW before it).
pub(super) fn chmod_at(dir: host::Raw, name: &CStr, mode: u32) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        // Linux's fchmodat(2) takes no AT_SYMLINK_NOFOLLOW before fchmodat2 (6.6). The
        // name is opened as a path, never followed, and the file changed through its
        // /proc/self/fd link, which reaches that file whatever is at the name by then
        // (musl's src/stat/fchmodat.c, virtiofsd's setattr).
        let fd = open_at(dir, name, libc::O_PATH | host::O_NOFOLLOW, 0)?;
        if mode_of(&stat_fd(fd.raw())?) & S_IFMT == S_IFLNK {
            return Err(EOPNOTSUPP);
        }
        let link = CString::new(format!("/proc/self/fd/{}", fd.raw())).map_err(|_| EINVAL)?;
        // SAFETY: fchmodat(2) of a NUL-terminated path.
        if unsafe { libc::fchmodat(host::AT_FDCWD, link.as_ptr(), mode as host::mode_t, 0) } != 0 {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        if mode_of(&stat_at(dir, name)?) & S_IFMT == S_IFLNK {
            return Err(EOPNOTSUPP);
        }
        // A symlink put at the name since has its own mode changed, not its target's.
        // SAFETY: fchmodat(2) of a NUL-terminated name in a directory we hold.
        if unsafe {
            libc::fchmodat(
                dir,
                name.as_ptr(),
                mode as host::mode_t,
                host::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
}

/// The user this process serves as, which owns what it makes (geteuid(2)), read once.
pub(super) fn served_by() -> u32 {
    static EUID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: geteuid(2) takes nothing and cannot fail.
    *EUID.get_or_init(|| unsafe { libc::geteuid() })
}

/// Another descriptor of what `fd` is open to (fcntl(2) F_DUPFD_CLOEXEC).
pub(super) fn dup_fd(fd: host::Raw) -> Result<host::Owned, Errno> {
    // SAFETY: fcntl(2) F_DUPFD_CLOEXEC of a descriptor the server holds open.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(last());
    }
    // SAFETY: a descriptor fcntl(2) just made, owned here alone.
    Ok(unsafe { host::Owned::from_raw_fd(copy) })
}

/// The file system `fd` is on's [`FsStats`] (fstatvfs(2)).
pub(super) fn fs_stats(fd: host::Raw) -> Result<FsStats, Errno> {
    // SAFETY: an all-zero statvfs is a valid out-parameter.
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: fstatvfs(2) of a descriptor we hold.
    if unsafe { libc::fstatvfs(fd, &mut v) } != 0 {
        return Err(last());
    }
    #[allow(
        clippy::useless_conversion,
        clippy::unnecessary_cast,
        clippy::cast_possible_truncation
    )]
    Ok(FsStats {
        blocks: v.f_blocks as u64,
        free: v.f_bfree as u64,
        available: v.f_bavail as u64,
        files: v.f_files as u64,
        files_free: v.f_ffree as u64,
        block_size: v.f_bsize as u32,
        fragment_size: v.f_frsize as u32,
    })
}

/// Removes `fd`'s attribute `name` (fremovexattr(2)).
pub(super) fn fremove_xattr(fd: host::Raw, name: &CStr) -> Result<(), Errno> {
    // SAFETY: fremovexattr(2) of a NUL-terminated name.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::fremovexattr(fd, name.as_ptr()) };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// `fd`'s offset moved to `offset` from `whence` (this host's, `host::whence`): where it is
/// (lseek(2)).
pub(super) fn seek(fd: host::Raw, offset: u64, whence: host::c_int) -> Result<u64, Errno> {
    #[allow(clippy::cast_possible_wrap)]
    // SAFETY: lseek(2) of a descriptor we hold.
    let at = unsafe { libc::lseek(fd, offset as host::off_t, whence) };
    u64::try_from(at).map_err(|_| last())
}

/// `fd`'s file cut or grown to `size` bytes (ftruncate(2)).
pub(super) fn truncate_fd(fd: host::Raw, size: u64) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_wrap)]
    // SAFETY: ftruncate(2) of a descriptor we hold.
    if unsafe { libc::ftruncate(fd, size as host::off_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// `fd`'s file given the host's permissions `perm` (fchmod(2)).
pub(super) fn chmod_fd(fd: host::Raw, perm: u32) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY: fchmod(2) of a descriptor we hold.
    if unsafe { libc::fchmod(fd, perm as host::mode_t) } != 0 {
        return Err(last());
    }
    Ok(())
}

/// The access and modification times of `own`'s file, or else of `name` in `dir` without
/// following it (futimens(2), utimensat(2)).
pub(super) fn set_times(
    own: Option<host::Raw>,
    dir: host::Raw,
    name: &CStr,
    atime: Time,
    mtime: Time,
) -> Result<(), Errno> {
    let spec = |t: Time| match t {
        Time::Now => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        #[allow(clippy::cast_possible_wrap)]
        Time::At(at, ns) => libc::timespec {
            tv_sec: at as _,
            tv_nsec: libc::c_long::from(ns as i32),
        },
        Time::Kept => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    };
    let times = [spec(atime), spec(mtime)];
    let rc = match own {
        // SAFETY: futimens(2) of a descriptor we hold.
        Some(fd) => unsafe { libc::futimens(fd, times.as_ptr()) },
        // SAFETY: utimensat(2) of a NUL-terminated name in a directory we hold.
        None => unsafe { libc::utimensat(dir, name.as_ptr(), times.as_ptr(), host::AT_SYMLINK_NOFOLLOW) },
    };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// The directory `names` lead to under `dir`, a component after another, none followed
/// if a symlink: on Linux in one openat2(2) refusing symlinks and any way out of `dir`
/// (RESOLVE_NO_SYMLINKS, RESOLVE_BENEATH; Linux 5.6), and a component at a time with
/// O_NOFOLLOW where the kernel has no openat2, or the path is too long for one call, and on
/// macOS. A path that no longer leads to a directory is stale.
pub(super) fn open_beneath(dir: host::Raw, names: &[CString]) -> Result<host::Owned, Errno> {
    #[cfg(target_os = "linux")]
    if let Some(opened) = openat2_beneath(dir, names) {
        return opened.map_err(stale);
    }
    let mut held: Option<host::Owned> = None;
    for name in names {
        let at = held.as_ref().map_or(dir, std::os::fd::AsRawFd::as_raw_fd);
        let next = open_at(at, name, host::O_RDONLY | host::O_DIRECTORY | host::O_NOFOLLOW, 0);
        held = Some(next.map_err(stale)?);
    }
    held.ok_or(EINVAL)
}

/// [`open_beneath`] in one openat2(2); none where this kernel has no openat2 (ENOSYS, or a
/// seccomp filter's EPERM) or the path is longer than one call takes.
#[cfg(target_os = "linux")]
pub(super) fn openat2_beneath(dir: host::Raw, names: &[CString]) -> Option<Result<host::Owned, Errno>> {
    /// struct open_how (include/uapi/linux/openat2.h), which libc's leaves unbuilt.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let mut path = Vec::new();
    for name in names {
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name.to_bytes());
    }
    let path = CString::new(path).ok()?;
    let how = OpenHow {
        flags: u64::try_from(host::O_RDONLY | host::O_DIRECTORY | host::O_CLOEXEC).ok()?,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: openat2(2) of a NUL-terminated path, with an open_how of the size given.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            path.as_ptr(),
            &raw const how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if let Ok(fd) = host::Raw::try_from(fd)
        && fd >= 0
    {
        // SAFETY: a fresh descriptor nothing else owns.
        return Some(Ok(unsafe { host::Owned::from_raw_fd(fd) }));
    }
    match last() {
        ENOSYS | EPERM | ENAMETOOLONG => None,
        e => Some(Err(e)),
    }
}

/// A directory the guest opened, read as it asks from a descriptor of its own. A handle
/// holds none of the listing: a copy of it each, taken at OPENDIR, made a guest that opened
/// a large directory again and again, never releasing it, cost the host memory without
/// bound (audit V05). Handles are as many as the descriptors the process may have.
///
/// Linux: read by getdents64(2) at the offset each request carries, the kernel's cookie
/// for what follows an entry (`d_off`), as virtiofsd reads one; nothing but the
/// descriptor is kept between requests (glibc's own stream keeps 32 KiB, PM M130).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(super) struct DirStream(host::Owned);

#[cfg(target_os = "linux")]
impl DirStream {
    pub(super) fn open(dir: host::Raw) -> Result<DirStream, Errno> {
        open_at(dir, c".", host::O_RDONLY | host::O_DIRECTORY, 0).map(DirStream)
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends; `want` bytes of them are read at a time.
    pub(super) fn read(
        &mut self,
        offset: u64,
        want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        let fd = self.0.raw();
        // SAFETY: lseek(2) of a descriptor we hold.
        if unsafe { libc::lseek(fd, offset as host::off_t, host::SEEK_SET) } < 0 {
            return Err(last());
        }
        // Room for a record of the longest name, 280 bytes, whatever the guest wants.
        let mut buf = vec![0u8; want.clamp(1024, MAX_WRITE as usize)];
        loop {
            // SAFETY: getdents64(2) into a buffer of its length.
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
            let got = usize::try_from(n).map_err(|_| last())?;
            if got == 0 {
                return Ok(());
            }
            let mut records = buf.get(..got).unwrap_or_default();
            // struct linux_dirent64: d_ino, d_off, d_reclen, d_type, then the name.
            while !records.is_empty() {
                let u64_at = |at: usize| {
                    let bytes = records.get(at..at + 8)?;
                    Some(u64::from_ne_bytes(bytes.try_into().ok()?))
                };
                let (Some(ino), Some(next)) = (u64_at(0), u64_at(8)) else {
                    return Err(EIO);
                };
                let reclen = records
                    .get(16..18)
                    .and_then(|b| b.try_into().ok())
                    .map(u16::from_ne_bytes)
                    .ok_or(EIO)?;
                let record = records
                    .get(..usize::from(reclen))
                    .filter(|r| r.len() > 19)
                    .ok_or(EIO)?;
                let name = record.get(19..).unwrap_or_default();
                let entry = Entry {
                    name: CStr::from_bytes_until_nul(name).map_err(|_| EIO)?.to_owned(),
                    ino,
                    kind: u32::from(record.get(18).copied().unwrap_or(0)),
                };
                if !each(&entry, next) {
                    return Ok(());
                }
                records = records.get(usize::from(reclen)..).unwrap_or_default();
            }
        }
    }
}

/// macOS: a directory stream of the handle's own (2.2 KiB, PM M130), its position counted
/// in entries, the offsets requests carry, with the entry the last reply had no room for;
/// a request at any other offset reads again from the start.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(super) struct DirStream {
    stream: std::ptr::NonNull<libc::DIR>,
    /// Entries read since the stream's start.
    at: u64,
    /// The entry at `at`, read already, which the last reply had no room for.
    held: Option<Entry>,
}

#[cfg(target_os = "macos")]
impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: closedir(3) of the stream this value owns, which closes its descriptor.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
impl DirStream {
    pub(super) fn open(dir: host::Raw) -> Result<DirStream, Errno> {
        use std::os::fd::IntoRawFd as _;
        let fd = open_at(dir, c".", host::O_RDONLY | host::O_DIRECTORY, 0)?;
        // SAFETY: fdopendir(3) of a descriptor we own; the stream owns it once it exists.
        let stream = std::ptr::NonNull::new(unsafe { libc::fdopendir(fd.raw()) }).ok_or_else(last)?;
        let _ = fd.into_raw_fd();
        Ok(DirStream {
            stream,
            at: 0,
            held: None,
        })
    }

    pub(super) fn next(&mut self) -> Option<Entry> {
        // SAFETY: readdir(3) of our open stream; the entry is copied before the next call.
        let entry = unsafe { libc::readdir(self.stream.as_ptr()).as_ref() }?;
        Some(Entry {
            // SAFETY: readdir's entry, its name NUL-terminated.
            name: unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }.to_owned(),
            ino: entry.d_ino,
            kind: u32::from(entry.d_type),
        })
    }

    /// Gives `each` the entries from `offset` on, with the offset that follows each, until
    /// it says to stop or the directory ends.
    pub(super) fn read(
        &mut self,
        offset: u64,
        _want: usize,
        mut each: impl FnMut(&Entry, u64) -> bool,
    ) -> Result<(), Errno> {
        if offset != self.at {
            // SAFETY: rewinddir(3) of our open stream.
            unsafe { libc::rewinddir(self.stream.as_ptr()) };
            self.at = 0;
            self.held = None;
            while self.at < offset {
                if self.next().is_none() {
                    return Ok(());
                }
                self.at += 1;
            }
        }
        loop {
            let Some(entry) = self.held.take().or_else(|| self.next()) else {
                return Ok(());
            };
            if !each(&entry, self.at.saturating_add(1)) {
                self.held = Some(entry);
                return Ok(());
            }
            self.at += 1;
        }
    }
}

/// `renameat2(2)` of `old` in `od` to `new` in `nd` with Linux's `flags`, as the host makes
/// it: `renameat`, macOS's `renameatx_np`, or Linux's own system call.
pub(super) fn renamed(od: host::Raw, old: &CStr, nd: host::Raw, new: &CStr, flags: u32) -> Result<(), Errno> {
    let rc = if flags == 0 {
        // SAFETY: renameat(2) of NUL-terminated names in directories we hold.
        unsafe { libc::renameat(od, old.as_ptr(), nd, new.as_ptr()) }
    } else {
        #[cfg(target_os = "macos")]
        {
            let mut host = 0;
            if flags & linux::RENAME_NOREPLACE != 0 {
                host |= libc::RENAME_EXCL;
            }
            if flags & linux::RENAME_EXCHANGE != 0 {
                host |= libc::RENAME_SWAP;
            }
            if flags & !(linux::RENAME_NOREPLACE | linux::RENAME_EXCHANGE) != 0 {
                return Err(EINVAL);
            }
            // SAFETY: renameatx_np(2) of NUL-terminated names in directories we hold.
            unsafe { libc::renameatx_np(od, old.as_ptr(), nd, new.as_ptr(), host) }
        }
        #[cfg(target_os = "linux")]
        {
            // The system call itself: musl 1.2.5, which Rust's musl targets link, has no
            // renameat2 wrapper, and glibc's is the same call.
            // SAFETY: renameat2(2) of NUL-terminated names in directories we hold.
            unsafe {
                libc::syscall(libc::SYS_renameat2, od, old.as_ptr(), nd, new.as_ptr(), flags) as host::c_int
            }
        }
    };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// The names of `fd`'s attributes the guest may see: those in a Linux namespace, but for
/// the server's own.
pub(super) fn list_xattr(fd: host::Raw) -> Result<Vec<u8>, Errno> {
    let mut buf = vec![0u8; 4096];
    let n = loop {
        // SAFETY: flistxattr(2) into a buffer of its length.
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::flistxattr(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::flistxattr(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            break usize::try_from(n).unwrap_or(0);
        }
        match last() {
            ERANGE if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            e => return Err(e),
        }
    };
    let mut out = Vec::new();
    for name in buf
        .get(..n)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
    {
        let ours =
            name == OWNER_XATTR.as_bytes() || name.strip_prefix(b"user.") == Some(OWNER_XATTR.as_bytes());
        let Some(name) = guest_xattr(name).filter(|_| !ours) else {
            continue;
        };
        let linux = [&b"user."[..], b"trusted.", b"security.", b"system."]
            .iter()
            .any(|p| name.starts_with(p));
        if linux {
            out.extend_from_slice(name);
            out.push(0);
        }
    }
    Ok(out)
}

pub(super) fn pread(fd: host::Raw, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get_mut(done..).ok_or(EIO)?;
        // SAFETY: pread(2) into the rest of our buffer.
        let n = unsafe {
            libc::pread(
                fd,
                rest.as_mut_ptr().cast(),
                rest.len(),
                (offset + done as u64) as host::off_t,
            )
        };
        match n {
            0 => break,
            n if n > 0 => done += n as usize,
            _ if last() == 4 => {}
            _ => return Err(last()),
        }
    }
    Ok(done)
}

/// Drops the host's cached pages of `len` bytes of `fd` at `offset`, clean ones that no
/// process maps (posix_fadvise POSIX_FADV_DONTNEED), as the guest took them: a hint, which
/// a host without it ignores.
pub(super) fn drop_cached(fd: host::Raw, offset: u64, len: usize) {
    #[cfg(target_os = "linux")]
    {
        let (Ok(offset), Ok(len)) = (host::off_t::try_from(offset), host::off_t::try_from(len)) else {
            return;
        };
        // SAFETY: advice on a range of a descriptor the server holds.
        unsafe { libc::posix_fadvise(fd, offset, len, libc::POSIX_FADV_DONTNEED) };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (fd, offset, len);
}

/// `buf` written to `fd`'s file at `offset`, or at its end where none: through a descriptor
/// opened O_APPEND by write(2), which puts each write at the end its file then has (POSIX),
/// where macOS's pwrite(2) writes at the offset named; through another (a pin), at the end
/// its file has as it is written.
pub(super) fn pwrite(fd: host::Raw, buf: &[u8], offset: Option<u64>) -> Result<usize, Errno> {
    let appends = offset.is_none() && {
        // SAFETY: F_GETFL of a descriptor the server holds.
        let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        status >= 0 && status & libc::O_APPEND != 0
    };
    let at = match offset {
        Some(at) => at,
        None if appends => 0,
        None => stat_fd(fd)?.size,
    };
    let mut done = 0;
    while done < buf.len() {
        let rest = buf.get(done..).ok_or(EIO)?;
        let n = if appends {
            // SAFETY: write(2) of the rest of the guest's data, at its file's end.
            unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) }
        } else {
            // SAFETY: pwrite(2) of the rest of the guest's data.
            unsafe {
                libc::pwrite(
                    fd,
                    rest.as_ptr().cast(),
                    rest.len(),
                    (at + done as u64) as host::off_t,
                )
            }
        };
        match n {
            n if n > 0 => done += n as usize,
            0 => break,
            _ if last() == 4 => {}
            _ => return Err(last()),
        }
    }
    Ok(done)
}

/// Durable on stable storage: F_FULLFSYNC on macOS, whose fsync leaves the drive's cache.
pub(super) fn durable(fd: host::Raw) -> Result<(), Errno> {
    #[cfg(target_os = "macos")]
    // SAFETY: fcntl(2) of a descriptor we hold.
    let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    #[cfg(target_os = "linux")]
    // SAFETY: fsync(2) of a descriptor we hold.
    let rc = unsafe { libc::fsync(fd) };
    if rc != 0 {
        return Err(last());
    }
    Ok(())
}

/// fallocate(2)'s allocation, with or without KEEP_SIZE; nothing else Linux's takes.
pub(super) fn fallocate(fd: host::Raw, mode: u32, offset: u64, length: u64) -> Result<(), Errno> {
    if mode & !linux::FALLOC_FL_KEEP_SIZE != 0 {
        return Err(EOPNOTSUPP);
    }
    let end = offset.checked_add(length).ok_or(EINVAL)?;
    #[cfg(target_os = "linux")]
    {
        let _ = end;
        // SAFETY: fallocate(2) of a descriptor we hold.
        if unsafe {
            libc::fallocate(
                fd,
                mode as host::c_int,
                offset as host::off_t,
                length as host::off_t,
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let size = stat_fd(fd)?.size;
        if end > size {
            let mut store = libc::fstore_t {
                fst_flags: libc::F_ALLOCATEALL,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: (end - size) as host::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: fcntl(2) F_PREALLOCATE with a store of ours.
            if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) } != 0 {
                return Err(last());
            }
            if mode & linux::FALLOC_FL_KEEP_SIZE == 0 {
                // SAFETY: ftruncate(2) of a descriptor we hold.
                if unsafe { libc::ftruncate(fd, end as host::off_t) } != 0 {
                    return Err(last());
                }
            }
        }
        Ok(())
    }
}

// SAFETY: the stream is this value's alone, used by one thread at a time (under the
// server's state lock); a DIR holds nothing tied to the thread that opened it.
#[cfg(target_os = "macos")]
unsafe impl Send for DirStream {}

/// Where the host keeps a guest's extended attribute `name`: under its own name on macOS,
/// which keeps no namespaces; and on Linux, where only a privileged process sets one in
/// `trusted.` or `security.` (xattr(7)), those under `user.shards.` (overlayfs keeps its
/// `trusted.overlay.` ones on its upper directory). The guest's names in `user.` are its
/// own; `user.shards.` is the server's, as the owner's attribute is.
pub(super) fn host_xattr(name: &CStr) -> Result<CString, Errno> {
    let n = name.to_bytes();
    if n.starts_with(PRIVATE_XATTR) {
        return Err(EOPNOTSUPP);
    }
    #[cfg(target_os = "linux")]
    if n.starts_with(b"trusted.") || n.starts_with(b"security.") {
        let mut host = PRIVATE_XATTR.to_vec();
        host.extend_from_slice(n);
        return CString::new(host).map_err(|_| EINVAL);
    }
    Ok(name.to_owned())
}

/// The prefix the host keeps the guest's `trusted.` and `security.` attributes under on
/// Linux ([`host_xattr`]), which a guest's own names may not take.
pub(super) const PRIVATE_XATTR: &[u8] = b"user.shards.";

/// The guest's name for a host attribute `host`: [`host_xattr`]'s, turned back; none for
/// one the guest does not see: the server's own, another of `user.shards.`'s, and on Linux
/// the host's own `trusted.` and `security.` attributes, which its kernel keeps (an SELinux
/// label, file capabilities or an IMA hash the host set), where the guest's of those names
/// are under `user.shards.`, and reading them by those names reaches the guest's alone.
pub(super) fn guest_xattr(host: &[u8]) -> Option<&[u8]> {
    match host.strip_prefix(PRIVATE_XATTR) {
        #[cfg(target_os = "linux")]
        Some(rest) if rest.starts_with(b"trusted.") || rest.starts_with(b"security.") => Some(rest),
        Some(_) => None,
        #[cfg(target_os = "linux")]
        None if host.starts_with(b"trusted.") || host.starts_with(b"security.") => None,
        None => Some(host),
    }
}
