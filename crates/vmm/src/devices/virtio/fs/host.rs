//! What a Linux guest's FUSE requests mean on this host, and back: its errno values, open
//! flags, `lseek` whences and device numbers (Linux include/uapi/asm-generic/errno*.h,
//! fcntl.h and arch/x86/include/uapi/asm, the guest's architecture being the host's;
//! linux/kdev_t.h new_encode_dev). On a Linux host each is itself.

/// Linux's errno for this host's `errno`. Every name is matched, for macOS numbers some
/// differently from Linux (its 11 is EDEADLK, Linux's EAGAIN).
#[cfg(target_os = "macos")]
pub fn linux_errno(errno: i32) -> i32 {
    match errno {
        libc::EPERM => 1,
        libc::ENOENT => 2,
        libc::ESRCH => 3,
        libc::EINTR => 4,
        libc::EIO => 5,
        libc::ENXIO => 6,
        libc::E2BIG => 7,
        libc::ENOEXEC => 8,
        libc::EBADF => 9,
        libc::ECHILD => 10,
        libc::EAGAIN => 11,
        libc::ENOMEM => 12,
        libc::EACCES => 13,
        libc::EFAULT => 14,
        libc::ENOTBLK => 15,
        libc::EBUSY => 16,
        libc::EEXIST => 17,
        libc::EXDEV => 18,
        libc::ENODEV => 19,
        libc::ENOTDIR => 20,
        libc::EISDIR => 21,
        libc::EINVAL => 22,
        libc::ENFILE => 23,
        libc::EMFILE => 24,
        libc::ENOTTY => 25,
        libc::ETXTBSY => 26,
        libc::EFBIG => 27,
        libc::ENOSPC => 28,
        libc::ESPIPE => 29,
        libc::EROFS => 30,
        libc::EMLINK => 31,
        libc::EPIPE => 32,
        libc::EDOM => 33,
        libc::ERANGE => 34,
        libc::EDEADLK => 35,
        libc::ENAMETOOLONG => 36,
        libc::ENOLCK => 37,
        libc::ENOSYS => 38,
        libc::ENOTEMPTY => 39,
        libc::ELOOP => 40,
        // macOS's missing attribute is Linux's ENODATA.
        libc::ENOATTR => 61,
        libc::EOVERFLOW => 75,
        libc::EILSEQ => 84,
        libc::ENOTSOCK => 88,
        libc::EDESTADDRREQ => 89,
        libc::EMSGSIZE => 90,
        libc::EPROTOTYPE => 91,
        libc::ENOPROTOOPT => 92,
        libc::EPROTONOSUPPORT => 93,
        libc::ESOCKTNOSUPPORT => 94,
        libc::EOPNOTSUPP => 95,
        libc::EPFNOSUPPORT => 96,
        libc::EAFNOSUPPORT => 97,
        libc::EADDRINUSE => 98,
        libc::EADDRNOTAVAIL => 99,
        libc::ENETDOWN => 100,
        libc::ENETUNREACH => 101,
        libc::ENETRESET => 102,
        libc::ECONNABORTED => 103,
        libc::ECONNRESET => 104,
        libc::ENOBUFS => 105,
        libc::EISCONN => 106,
        libc::ENOTCONN => 107,
        libc::ESHUTDOWN => 108,
        libc::ETIMEDOUT => 110,
        libc::ECONNREFUSED => 111,
        libc::EHOSTDOWN => 112,
        libc::EHOSTUNREACH => 113,
        libc::EALREADY => 114,
        libc::EINPROGRESS => 115,
        libc::ESTALE => 116,
        libc::EDQUOT => 122,
        libc::ECANCELED => 125,
        // ENOTSUP is EOPNOTSUPP on Linux.
        libc::ENOTSUP => 95,
        _ => 5,
    }
}

#[cfg(target_os = "linux")]
pub fn linux_errno(errno: i32) -> i32 {
    errno
}

/// Linux's open(2) flags, as the guest sends them (its architecture's values).
pub mod linux {
    pub const O_ACCMODE: u32 = 0o3;
    pub const O_CREAT: u32 = 0o100;
    pub const O_EXCL: u32 = 0o200;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
    pub const O_NONBLOCK: u32 = 0o4000;
    pub const O_DSYNC: u32 = 0o10000;
    pub const O_SYNC: u32 = 0o4010000;
    #[cfg(target_arch = "x86_64")]
    pub const O_DIRECT: u32 = 0o40000;
    #[cfg(not(target_arch = "x86_64"))]
    pub const O_DIRECT: u32 = 0o200000;
    /// `lseek` whences: SEEK_DATA and SEEK_HOLE.
    pub const SEEK_DATA: u32 = 3;
    pub const SEEK_HOLE: u32 = 4;
    /// renameat2(2)'s flags.
    pub const RENAME_NOREPLACE: u32 = 1;
    pub const RENAME_EXCHANGE: u32 = 2;
    pub const RENAME_WHITEOUT: u32 = 4;
    /// fallocate(2)'s KEEP_SIZE.
    pub const FALLOC_FL_KEEP_SIZE: u32 = 1;
    /// setxattr(2)'s flags.
    pub const XATTR_CREATE: u32 = 1;
    pub const XATTR_REPLACE: u32 = 2;
}

/// This host's open(2) flags for a guest's: its access mode, appending, truncation,
/// synchronous writes, and, for a create, exclusion. Never following a final symlink.
pub fn open_flags(guest: u32) -> libc::c_int {
    let mut f = match guest & linux::O_ACCMODE {
        1 => libc::O_WRONLY,
        2 => libc::O_RDWR,
        _ => libc::O_RDONLY,
    };
    for (g, h) in [
        (linux::O_CREAT, libc::O_CREAT),
        (linux::O_EXCL, libc::O_EXCL),
        (linux::O_TRUNC, libc::O_TRUNC),
        (linux::O_APPEND, libc::O_APPEND),
        (linux::O_NONBLOCK, libc::O_NONBLOCK),
        (linux::O_DSYNC, libc::O_DSYNC),
    ] {
        if guest & g == g {
            f |= h;
        }
    }
    if guest & linux::O_SYNC == linux::O_SYNC {
        f |= libc::O_SYNC;
    }
    f | libc::O_NOFOLLOW | libc::O_CLOEXEC
}

/// This host's `lseek` whence for a guest's.
pub fn whence(guest: u32) -> Option<libc::c_int> {
    Some(match guest {
        0 => libc::SEEK_SET,
        1 => libc::SEEK_CUR,
        2 => libc::SEEK_END,
        linux::SEEK_DATA => libc::SEEK_DATA,
        linux::SEEK_HOLE => libc::SEEK_HOLE,
        _ => return None,
    })
}

/// A host device number as the guest's FUSE takes one (new_encode_dev).
pub fn encode_dev(rdev: u64) -> u32 {
    #[cfg(target_os = "macos")]
    let (major, minor) = ((rdev >> 24) as u32 & 0xff, rdev as u32 & 0xff_ffff);
    #[cfg(target_os = "linux")]
    let (major, minor) = (libc::major(rdev as libc::dev_t), libc::minor(rdev as libc::dev_t));
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// A guest's device number (new_encode_dev's) as this host's.
pub fn decode_dev(rdev: u32) -> libc::dev_t {
    let major = (rdev & 0xfff00) >> 8;
    let minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00);
    #[cfg(target_os = "macos")]
    {
        libc::makedev(major as i32, minor as i32)
    }
    #[cfg(target_os = "linux")]
    {
        libc::makedev(major, minor)
    }
}

/// What the server holds a file by, and passes its host's calls: a descriptor on Unix, a
/// handle on Windows (D197).
#[cfg(unix)]
pub use std::os::fd::{OwnedFd as Owned, RawFd as Raw};

/// A held file's raw descriptor or handle, as its host's calls take it.
pub trait AsRaw {
    fn raw(&self) -> Raw;
}

#[cfg(unix)]
impl AsRaw for Owned {
    fn raw(&self) -> Raw {
        std::os::fd::AsRawFd::as_raw_fd(self)
    }
}

/// This host's names for what the server asks of its file system, its flags, modes and
/// kinds: on Unix libc's own; on Windows Linux's, which its layer of NT calls answers in
/// (D197).
#[cfg(unix)]
pub use libc::{
    AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, DT_BLK, DT_CHR, DT_FIFO, DT_REG, DT_SOCK, O_ACCMODE,
    O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_RDONLY, O_RDWR, O_TRUNC,
    O_WRONLY, PATH_MAX, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, S_IROTH,
    S_IRUSR, S_ISGID, S_ISUID, S_IWOTH, S_IWUSR, SEEK_SET, c_int, c_long, c_uint, mode_t, off_t,
};

/// A file's attributes as the server answers its guest about them: its host's own, in
/// Linux's terms, and the key it is known by (`dev`, `ino`). On Unix its stat(2), the mode's
/// type bits POSIX's own on every host; on Windows its file system's (D197).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    /// Its type and permissions, Linux's `mode_t`'s bits.
    pub mode: u32,
    pub nlink: u32,
    /// Its host's owner, which a guest's owner kept beside it overrides (server.rs `Owner`).
    pub uid: u32,
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    /// Its host's device number, a device file's.
    pub rdev: u64,
    /// Seconds and nanoseconds since the epoch.
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
}

#[cfg(unix)]
impl From<&libc::stat> for Stat {
    #[allow(
        clippy::useless_conversion,
        clippy::unnecessary_cast,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    fn from(st: &libc::stat) -> Stat {
        Stat {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            mode: st.st_mode as u32,
            nlink: st.st_nlink as u32,
            uid: st.st_uid as u32,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            blksize: st.st_blksize as u32,
            rdev: st.st_rdev as u64,
            atime: (st.st_atime as u64, st.st_atime_nsec as u32),
            mtime: (st.st_mtime as u64, st.st_mtime_nsec as u32),
            ctime: (st.st_ctime as u64, st.st_ctime_nsec as u32),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_numbers_round_trip_as_linux_encodes_them() {
        // /dev/sda1 (8, 1) and a large minor: Linux's new_encode_dev.
        let sda1 = decode_dev(0x801);
        assert_eq!(encode_dev(sda1 as u64), 0x801);
        // A major macOS holds (8 bits), and a minor past 8 bits.
        let big = decode_dev((0x23 << 8) | 0x45 | (0x678 << 20));
        assert_eq!(encode_dev(big as u64), (0x23 << 8) | 0x45 | (0x678 << 20));
    }

    #[test]
    fn errnos_and_flags_are_linuxs() {
        assert_eq!(linux_errno(libc::EAGAIN), 11);
        assert_eq!(linux_errno(libc::ENOTEMPTY), 39);
        assert_eq!(linux_errno(libc::ENAMETOOLONG), 36);
        assert_eq!(linux_errno(libc::ELOOP), 40);
        let f = open_flags(linux::O_CREAT | linux::O_EXCL | 2);
        assert_eq!(f & libc::O_ACCMODE, libc::O_RDWR);
        assert_ne!(f & libc::O_CREAT, 0);
        assert_ne!(f & libc::O_EXCL, 0);
        assert_ne!(f & libc::O_NOFOLLOW, 0);
    }
}
