//! A Linux tree's files on NTFS, kept as WSL keeps them (D197, PM M362): the NT calls made
//! of them, each relative to a handle of the tree and following no reparse point, and what
//! they mean in Linux's terms. A guest's files a share keeps on a Windows host are kept so
//! (shards-vmm's share server), and a client's build context its Windows machine keeps a
//! copy of (D180, shards' `synced`), through shards-archive and shards-build. And a host's
//! own file opened through no link at its name, as Unix's O_NOFOLLOW opens one
//! ([`open_unfollowed`]), and a host's own tree walked, opened, made and removed a name at a
//! time from a directory's handle, through no link anywhere below it, as Unix's openat(2)
//! walks ([`create_at`], [`open_unfollowed_at`]), for both. Nothing on other hosts.

#![cfg(windows)]

use std::os::windows::io::{
    AsHandle as _, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};

use windows_sys::Wdk::Foundation::{NtQueryObject, OBJECT_ATTRIBUTES, ObjectBasicInformation};
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CASE_SENSITIVE_INFORMATION, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
    FILE_STAT_LX_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileCaseSensitiveInformation,
    FileFsFullSizeInformation, FileLinkInformation, FileRenameInformationEx, FileStatLxInformation,
    LX_FILE_METADATA_HAS_DEVICE_ID, LX_FILE_METADATA_HAS_MODE, LX_FILE_METADATA_HAS_UID, NtCreateFile,
    NtQueryEaFile, NtQueryInformationFile, NtQueryVolumeInformationFile, NtSetEaFile, NtSetInformationFile,
    SYMLINK_FLAG_RELATIVE,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_DIRECTORY, ERROR_DISK_FULL,
    ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_FILENAME_EXCED_RANGE, ERROR_INVALID_NAME,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_A_REPARSE_POINT, ERROR_NOT_SAME_DEVICE,
    ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, ERROR_WRITE_PROTECT, NTSTATUS,
    STATUS_ACCESS_DENIED, STATUS_CANNOT_DELETE, STATUS_DELETE_PENDING, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_DISK_FULL, STATUS_EAS_NOT_SUPPORTED, STATUS_FILE_DELETED, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_PARAMETER, STATUS_IO_REPARSE_TAG_NOT_HANDLED,
    STATUS_MEDIA_WRITE_PROTECTED, STATUS_NAME_TOO_LONG, STATUS_NO_EAS_ON_FILE, STATUS_NO_MORE_EAS,
    STATUS_NONEXISTENT_EA_ENTRY, STATUS_NOT_A_DIRECTORY, STATUS_NOT_A_REPARSE_POINT, STATUS_NOT_SAME_DEVICE,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_REPARSE_POINT_NOT_RESOLVED,
    STATUS_SHARING_VIOLATION, STATUS_STOPPED_ON_SYMLINK, STATUS_UNTRUSTED_MOUNT_POINT, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_EA, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA, FileBasicInfo,
    FileDispositionInfoEx, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::{
    FILE_CS_FLAG_CASE_SENSITIVE_DIR, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::Win32::System::WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION;

/// A file's attributes in Linux's terms ([`stat`]): its volume's and its own key (`dev`,
/// `ino`), its type and permissions (Linux's `mode_t`'s bits), its links, owner, size,
/// device number and times, seconds and nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    pub rdev: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
}

/// A Linux errno.
pub type Errno = i32;

pub const ENOENT: Errno = 2;
pub const E2BIG: Errno = 7;
pub const EIO: Errno = 5;
pub const ENOMEM: Errno = 12;
pub const EACCES: Errno = 13;
pub const EBUSY: Errno = 16;
pub const EEXIST: Errno = 17;
pub const EXDEV: Errno = 18;
pub const ENOTDIR: Errno = 20;
pub const EISDIR: Errno = 21;
pub const EINVAL: Errno = 22;
pub const ENOSPC: Errno = 28;
pub const EROFS: Errno = 30;
pub const ENAMETOOLONG: Errno = 36;
pub const ELOOP: Errno = 40;
pub const ENOTEMPTY: Errno = 39;
pub const ENODATA: Errno = 61;
pub const EILSEQ: Errno = 84;
pub const EOPNOTSUPP: Errno = 95;
pub const ERANGE: Errno = 34;

/// The LX reparse tags (ntifs.h, PM M362): a guest's symlink's, FIFO's, character and block
/// devices' and Unix socket's, each kept as WSL keeps them.
pub const LX_SYMLINK: u32 = 0xA000_001D;
pub const LX_FIFO: u32 = 0x8000_0024;
pub const LX_CHR: u32 = 0x8000_0025;
pub const LX_BLK: u32 = 0x8000_0026;
pub const AF_UNIX: u32 = 0x8000_0023;

/// The first character of the Private Use Area a name's character no Windows name may hold
/// is kept at, plus that character's own value, as WSL keeps one (`\`, `:`, `*`, `?`, `"`,
/// `<`, `>`, `|` and the control characters).
const ESCAPED: u32 = 0xF000;

/// Whether `c` is one no NTFS name may hold, which [`nt_name`] keeps escaped.
fn unholdable(c: char) -> bool {
    matches!(
        c,
        '\u{1}'..='\u{1f}' | '"' | '*' | ':' | '<' | '>' | '?' | '\\' | '|'
    )
}

/// A guest's name (one component, Linux's bytes) as NTFS keeps it: its UTF-8 as UTF-16,
/// each character no Windows name may hold escaped into the Private Use Area as WSL
/// escapes it. NTFS keeps UTF-16 names: one not UTF-8 is refused (EILSEQ), as is one
/// holding a `/` or a NUL, which no component holds.
pub fn nt_name(guest: &[u8]) -> Result<Vec<u16>, Errno> {
    let text = std::str::from_utf8(guest).map_err(|_| EILSEQ)?;
    if text.is_empty() || text.contains(['/', '\0']) {
        return Err(EINVAL);
    }
    let mut out = Vec::with_capacity(text.len());
    let mut units = [0u16; 2];
    for c in text.chars() {
        let kept = if unholdable(c) {
            char::from_u32(ESCAPED + u32::from(c)).ok_or(EINVAL)?
        } else {
            c
        };
        out.extend_from_slice(kept.encode_utf16(&mut units));
    }
    Ok(out)
}

/// The guest's name for an NTFS name: [`nt_name`]'s turned back, an escaped character its
/// own again; a name not UTF-16 kept with each unpaired surrogate as U+FFFD.
pub fn guest_name(nt: &[u16]) -> Vec<u8> {
    let mut out = String::with_capacity(nt.len());
    for c in char::decode_utf16(nt.iter().copied()) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let back = u32::from(c)
            .checked_sub(ESCAPED)
            .and_then(char::from_u32)
            .filter(|b| unholdable(*b));
        out.push(back.unwrap_or(c));
    }
    out.into_bytes()
}

/// Linux's errno for an NTSTATUS an NT call answered with, the ones a file system's calls
/// answer; EIO for any other.
pub fn errno(status: NTSTATUS) -> Errno {
    match status {
        STATUS_OBJECT_NAME_NOT_FOUND
        | STATUS_OBJECT_PATH_NOT_FOUND
        | STATUS_DELETE_PENDING
        | STATUS_FILE_DELETED => ENOENT,
        STATUS_OBJECT_NAME_COLLISION => EEXIST,
        STATUS_ACCESS_DENIED | STATUS_CANNOT_DELETE => EACCES,
        STATUS_SHARING_VIOLATION => EBUSY,
        STATUS_NOT_A_DIRECTORY => ENOTDIR,
        STATUS_FILE_IS_A_DIRECTORY => EISDIR,
        STATUS_DIRECTORY_NOT_EMPTY => ENOTEMPTY,
        STATUS_DISK_FULL => ENOSPC,
        STATUS_MEDIA_WRITE_PROTECTED => EROFS,
        STATUS_NAME_TOO_LONG => ENAMETOOLONG,
        STATUS_OBJECT_NAME_INVALID | STATUS_INVALID_PARAMETER => EINVAL,
        STATUS_NOT_SAME_DEVICE => EXDEV,
        STATUS_INSUFFICIENT_RESOURCES => ENOMEM,
        STATUS_NOT_SUPPORTED | STATUS_EAS_NOT_SUPPORTED => EOPNOTSUPP,
        STATUS_NO_EAS_ON_FILE | STATUS_NONEXISTENT_EA_ENTRY | STATUS_NOT_A_REPARSE_POINT => ENODATA,
        // A name beneath a reparse point opened as itself, which NT resolves through none
        // (PM M362): as Linux's openat2(2) answers a symlink met with RESOLVE_NO_SYMLINKS.
        // A mount point Windows' Redirection Guard will not traverse is one more such link
        // (STATUS_UNTRUSTED_MOUNT_POINT, ERROR_UNTRUSTED_MOUNT_POINT 448: Windows 11 26H2,
        // 10.0.26300, answers it for a junction its user made, measured on the step-10
        // arm64 host).
        STATUS_REPARSE_POINT_NOT_RESOLVED
        | STATUS_STOPPED_ON_SYMLINK
        | STATUS_IO_REPARSE_TAG_NOT_HANDLED
        | STATUS_UNTRUSTED_MOUNT_POINT => ELOOP,
        _ => EIO,
    }
}

/// `name` in `dir` opened by NtCreateFile with `access`, `disposition` and `options`, as a
/// handle of its own: relative to `dir`, its name one component, never following a
/// reparse point at it (FILE_OPEN_REPARSE_POINT), every kind of sharing allowed, as a Unix
/// file's opens share; its I/O synchronous; its attributes and EAs readable, for [`stat`]. A name NTFS matches case-sensitively where its
/// directory is (D197).
pub fn open_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    access: u32,
    disposition: u32,
    options: u32,
) -> Result<OwnedHandle, Errno> {
    let bytes = u16::try_from(name.len().checked_mul(2).ok_or(ENAMETOOLONG)?).map_err(|_| ENAMETOOLONG)?;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &object,
        // Not OBJ_CASE_INSENSITIVE: a directory made case-sensitive matches its names' own
        // case alone.
        Attributes: 0,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: an object name and attributes of this frame's, valid for the call; plain
    // arguments; no EA buffer.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            // Its attributes and EAs read too, which stat reads WSL's metadata by.
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_READ_EA,
            &attributes,
            &mut io,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    // SAFETY: a handle NtCreateFile just made, owned here alone.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Seconds and nanoseconds since the Unix epoch of an NT time, 100 ns since 1601.
fn unix_time(nt: i64) -> (u64, u32) {
    const SINCE_1601: i64 = 116_444_736_000_000_000;
    let ticks = u64::try_from(nt.saturating_sub(SINCE_1601)).unwrap_or(0);
    (
        ticks / 10_000_000,
        u32::try_from((ticks % 10_000_000) * 100).unwrap_or(0),
    )
}

/// What `h`'s file is, as a Linux guest is told: NTFS's stat with WSL's metadata
/// (FileStatLxInformation, its `$LX` attributes read), its key its volume's serial number
/// and its file's ID (FileIdInfo, whose upper 8 bytes NTFS leaves unused, PM M362). One with
/// no owner WSL keeps is root's, and one with no mode its kind's: a directory 755, an LX
/// symlink 777, a file 644, or 444 where NTFS keeps it read-only.
pub fn stat(h: BorrowedHandle<'_>) -> Result<Stat, Errno> {
    let mut lx = FILE_STAT_LX_INFORMATION::default();
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a buffer of its own size for the information class asked.
    let status = unsafe {
        NtQueryInformationFile(
            h.as_raw_handle(),
            &mut io,
            (&raw mut lx).cast(),
            u32::try_from(size_of::<FILE_STAT_LX_INFORMATION>()).unwrap_or(0),
            FileStatLxInformation,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    let mut id = FILE_ID_INFO::default();
    // SAFETY: a buffer of its own size for the class asked, of a handle the caller holds.
    let got = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            FileIdInfo,
            (&raw mut id).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).unwrap_or(0),
        )
    };
    if got == 0 {
        return Err(EIO);
    }
    let kind = match lx.ReparseTag {
        LX_SYMLINK | IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT => 0o120000,
        LX_FIFO => 0o010000,
        LX_CHR => 0o020000,
        LX_BLK => 0o060000,
        AF_UNIX => 0o140000,
        _ if lx.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 => 0o040000,
        _ => 0o100000,
    };
    // A file with no mode of WSL's is executable, as Windows has no bit to say otherwise
    // (WSL shows its user's rights, Docker Desktop 0777): 0755, or 0555 read-only, as a
    // Windows file goes to a build (fsutil; shards-build's host.rs), its owner's to write.
    let mode = if lx.LxFlags & LX_FILE_METADATA_HAS_MODE != 0 {
        kind | (lx.LxMode & 0o7777)
    } else {
        kind | match kind {
            0o040000 => 0o755,
            0o120000 => 0o777,
            _ if lx.FileAttributes & FILE_ATTRIBUTE_READONLY != 0 => 0o555,
            _ => 0o755,
        }
    };
    let rdev = if lx.LxFlags & LX_FILE_METADATA_HAS_DEVICE_ID != 0 {
        let (major, minor) = (lx.LxDeviceIdMajor, lx.LxDeviceIdMinor);
        // Linux's new_encode_dev.
        u64::from((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
    } else {
        0
    };
    let file = u128::from_le_bytes(id.FileId.Identifier);
    Ok(Stat {
        dev: id.VolumeSerialNumber,
        ino: u64::try_from(file & u128::from(u64::MAX)).unwrap_or(0),
        mode,
        nlink: lx.NumberOfLinks,
        uid: if lx.LxFlags & LX_FILE_METADATA_HAS_UID != 0 {
            lx.LxUid
        } else {
            0
        },
        size: u64::try_from(lx.EndOfFile).unwrap_or(0),
        blocks: u64::try_from(lx.AllocationSize).unwrap_or(0) / 512,
        blksize: 4096,
        rdev,
        atime: unix_time(lx.LastAccessTime),
        mtime: unix_time(lx.LastWriteTime),
        ctime: unix_time(lx.ChangeTime),
    })
}

/// Which file of several names a host's file is ([`links`]): how many names it has, its
/// volume's serial number and its file's ID, as a Unix file's link count, device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Links {
    pub count: u64,
    pub volume: u64,
    pub file: u128,
}

/// Of `h`'s file, a host's own, which file it is where it has more than one name, as Unix
/// packers tell hard links apart by their device and inode: its link count
/// (GetFileInformationByHandle's `nNumberOfLinks`), and where that is above 1, its volume's
/// 64-bit serial number and its 128-bit file ID (FileIdInfo), ReFS's whole, where the 64-bit
/// index GetFileInformationByHandle gives is not unique on ReFS, and NTFS's index in its low
/// 8 bytes. A file of one name asks nothing more: None. A handle opened for no access may
/// ask both, as `std::fs::metadata`'s is.
pub fn links(h: BorrowedHandle<'_>) -> std::io::Result<Option<Links>> {
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: a buffer of its own type, for a handle the caller holds.
    if unsafe { GetFileInformationByHandle(h.as_raw_handle(), &raw mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.nNumberOfLinks <= 1 {
        return Ok(None);
    }
    let (volume, file) = file_id(h)?;
    Ok(Some(Links {
        count: u64::from(info.nNumberOfLinks),
        volume,
        file,
    }))
}

/// `h`'s file's volume serial number and 128-bit file ID (FileIdInfo, FILE_ID_INFO), which
/// "uniquely identify a file on a single computer".
fn file_id(h: BorrowedHandle<'_>) -> std::io::Result<(u64, u128)> {
    let mut id = FILE_ID_INFO::default();
    // SAFETY: a buffer of its own size for the class asked, of a handle the caller holds.
    let got = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            FileIdInfo,
            (&raw mut id).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).unwrap_or(0),
        )
    };
    if got == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((id.VolumeSerialNumber, u128::from_le_bytes(id.FileId.Identifier)))
}

/// The error [`open_unfollowed`] refuses a link with: ERROR_STOPPED_ON_SYMLINK, "The create
/// operation stopped after reaching a symbolic link" (Microsoft, System Error Codes
/// 500-999), as Unix's open(2) refuses one ELOOP under O_NOFOLLOW.
pub const STOPPED_ON_LINK: i32 = windows_sys::Win32::Foundation::ERROR_STOPPED_ON_SYMLINK as i32;

/// A reparse tag's name surrogate bit (winnt.h's IsReparseTagNameSurrogate): set where "the
/// file or directory represents another named entity in the system" (Microsoft, Reparse
/// Point Tags), as a symbolic link, a junction or mount point and WSL's symlink do. std's
/// `FileType::is_symlink` is a reparse point of this bit (library/std/src/sys/fs/windows.rs),
/// so a walk's `symlink_metadata` calls the same files links.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// The file at `p` opened as `options` and `flags` (CreateFileW's flags, std's
/// `custom_flags`) ask, never through a link at its last name, as Unix opens one with
/// O_NOFOLLOW:
/// - It is opened first as itself, a reparse point not processed (FILE_FLAG_OPEN_REPARSE_POINT:
///   "If an existing file is opened and it is a symbolic link, the handle returned is a
///   handle to the symbolic link", CreateFileW), and its handle asked its attributes and
///   reparse tag (FileAttributeTagInfo, FILE_ATTRIBUTE_TAG_INFO).
/// - A file no reparse point is that handle.
/// - A name surrogate (a symbolic link, a junction, WSL's symlink) is refused,
///   [`STOPPED_ON_LINK`].
/// - Any other reparse point (a cloud file's placeholder, a deduplicated or WOF-compressed
///   file, a container's WCI file) is opened again as `options` ask, its filter at work, for
///   what it holds and not its reparse data; and is that file only where the second handle's
///   volume serial number and file ID are the first's (FILE_ID_INFO: "To determine whether
///   two open handles represent the same file, combine the identifier and the volume serial
///   number for each file and compare them"). Another file, which a link put in its place
///   between the two opens or a filter that redirects it gives, is refused, [`STOPPED_ON_LINK`].
///
/// `options` must not truncate: FILE_FLAG_OPEN_REPARSE_POINT "cannot be used with the
/// CREATE_ALWAYS flag", and with TRUNCATE_EXISTING "the file affected is a symbolic link"
/// (CreateFileW). Truncate the file it gives, as Go's Openat does
/// (internal/syscall/windows/at_windows.go, go1.26.1). A directory opens only where `flags`
/// hold FILE_FLAG_BACKUP_SEMANTICS ("You must set this flag to obtain a handle to a
/// directory"): without it a junction put in a file's place is refused as Windows refuses a
/// directory opened as a file.
pub fn open_unfollowed(
    options: &std::fs::OpenOptions,
    flags: u32,
    p: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let first = options
        .clone()
        .custom_flags(flags | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p)?;
    match reparse_of(first.as_handle())? {
        Reparse::None => return Ok(first),
        Reparse::Link => return Err(std::io::Error::from_raw_os_error(STOPPED_ON_LINK)),
        Reparse::Other => {}
    }
    let again = options.clone().custom_flags(flags).open(p)?;
    same_file(&first, again)
}

/// `again`, where it is the file `first` is (FILE_ID_INFO's volume serial number and file
/// ID, [`open_unfollowed`]); refused [`STOPPED_ON_LINK`] where it is another.
fn same_file(first: &std::fs::File, again: std::fs::File) -> std::io::Result<std::fs::File> {
    if file_id(first.as_handle())? != file_id(again.as_handle())? {
        return Err(std::io::Error::from_raw_os_error(STOPPED_ON_LINK));
    }
    Ok(again)
}

/// What a file is as a reparse point ([`reparse_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reparse {
    /// No reparse point.
    None,
    /// A name surrogate (`NAME_SURROGATE`): a symbolic link, a junction or mount point,
    /// WSL's symlink.
    Link,
    /// Any other: a cloud file's placeholder, a deduplicated or WOF-compressed file.
    Other,
}

/// `h`'s file's attributes and reparse tag (FileAttributeTagInfo, FILE_ATTRIBUTE_TAG_INFO;
/// the tag is 0 where the attributes hold no FILE_ATTRIBUTE_REPARSE_POINT).
pub fn attribute_tag(h: BorrowedHandle<'_>) -> std::io::Result<(u32, u32)> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo};
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: a buffer of its own size for the class asked, of a handle the caller holds.
    let got = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0),
        )
    };
    if got == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.FileAttributes, info.ReparseTag))
}

/// What `h`'s file is as a reparse point, by its [`attribute_tag`].
pub fn reparse_of(h: BorrowedHandle<'_>) -> std::io::Result<Reparse> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    let (attributes, tag) = attribute_tag(h)?;
    Ok(if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Reparse::None
    } else if tag & NAME_SURROGATE != 0 {
        Reparse::Link
    } else {
        Reparse::Other
    })
}

/// Every sharing a handle may allow, as std's opens allow it: another may read, write,
/// rename or delete what it holds.
pub const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// The Win32 error CreateFileW and its kin give for an NTSTATUS (RtlNtStatusToDosError), as
/// an [`std::io::Error`]: its kind and message Windows' own, as std's calls give them.
fn win32_error(status: NTSTATUS) -> std::io::Error {
    // SAFETY: a lookup of a status's Win32 code, with no other effect.
    let code = unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) };
    std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// `name` in the directory `dir` holds, opened by NtCreateFile relative to its handle
/// (OBJECT_ATTRIBUTES' RootDirectory), as a host's own names are opened rather than a Linux
/// tree's ([`open_at`]): without regard to case (OBJ_CASE_INSENSITIVE, as CreateFileW opens
/// a name and Go's Openat, go1.26.1 internal/syscall/windows/at_windows.go), sharing `share`,
/// with `access`, `disposition` (FILE_OPEN, FILE_CREATE, FILE_OPEN_IF) and `options`, its
/// I/O synchronous and its attributes readable, a file it makes FILE_ATTRIBUTE_NORMAL. An
/// empty `name` opens `dir`'s own file again, wherever it has gone. Its error is the Win32
/// one CreateFileW gives for the same status, which std's calls and Go's give too.
pub fn create_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
) -> std::io::Result<std::fs::File> {
    use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    let too_long =
        || std::io::Error::from_raw_os_error(i32::try_from(ERROR_FILENAME_EXCED_RANGE).unwrap_or(0));
    let bytes = name
        .len()
        .checked_mul(2)
        .and_then(|b| u16::try_from(b).ok())
        .ok_or_else(too_long)?;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &object,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: an object name and attributes of this frame's, valid for the call (no name read
    // where its length is 0); a directory handle the caller holds; plain arguments; no EA
    // buffer.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            &attributes,
            &mut io,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(win32_error(status));
    }
    // SAFETY: a handle NtCreateFile just made, owned here alone.
    Ok(std::fs::File::from(unsafe {
        OwnedHandle::from_raw_handle(handle)
    }))
}

/// `name` in `dir` opened as [`create_at`] opens it, never through a link there, as
/// [`open_unfollowed`] opens a path's last name: opened as itself first (FILE_OPEN_REPARSE_POINT),
/// a name surrogate refused [`STOPPED_ON_LINK`], any other reparse point opened again for
/// what it holds and kept only where it is the same file. Walked a name at a time from a
/// directory's handle, each directory so opened, no link anywhere below it is followed, as
/// Unix's openat(2) walks with O_NOFOLLOW (Go's os.Root on Windows walks so too, go1.26.1
/// os/root_windows.go). `options` must not truncate ([`open_unfollowed`]).
pub fn open_unfollowed_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
) -> std::io::Result<std::fs::File> {
    let first = create_at(
        dir,
        name,
        access,
        share,
        disposition,
        options | FILE_OPEN_REPARSE_POINT,
    )?;
    match reparse_of(first.as_handle())? {
        Reparse::None => return Ok(first),
        Reparse::Link => return Err(std::io::Error::from_raw_os_error(STOPPED_ON_LINK)),
        Reparse::Other => {}
    }
    let again = create_at(
        dir,
        name,
        access,
        share,
        FILE_OPEN,
        options & !FILE_OPEN_REPARSE_POINT,
    )?;
    same_file(&first, again)
}

/// The names in the directory `h` holds open, `.` and `..` aside, as NTFS keeps them,
/// read through `h` (FileFullDirectoryRestartInfo, then FileFullDirectoryInfo until
/// ERROR_NO_MORE_FILES): that directory's, wherever it has gone.
pub fn names(h: BorrowedHandle<'_>) -> std::io::Result<Vec<Vec<u16>>> {
    use std::mem::offset_of;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FULL_DIR_INFO, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    };
    let bad = || std::io::Error::from(std::io::ErrorKind::InvalidData);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 << 10];
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: a buffer of its length for the class asked, on a handle the caller holds.
        let done = unsafe {
            GetFileInformationByHandleEx(
                h.as_raw_handle(),
                class,
                buf.as_mut_ptr().cast(),
                u32::try_from(buf.len()).unwrap_or(0),
            )
        };
        if done == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(i32::try_from(ERROR_NO_MORE_FILES).unwrap_or(0)) {
                return Ok(out);
            }
            return Err(e);
        }
        class = FileFullDirectoryInfo;
        let word = |at: usize| -> Option<usize> {
            let b = buf.get(at..at.checked_add(4)?)?;
            usize::try_from(u32::from_le_bytes(<[u8; 4]>::try_from(b).ok()?)).ok()
        };
        let mut at = 0usize;
        loop {
            let next = word(at).ok_or_else(bad)?;
            let len = word(at + offset_of!(FILE_FULL_DIR_INFO, FileNameLength)).ok_or_else(bad)?;
            let name_at = at + offset_of!(FILE_FULL_DIR_INFO, FileName);
            let name: Vec<u16> = buf
                .get(name_at..name_at.checked_add(len).ok_or_else(bad)?)
                .ok_or_else(bad)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            if !matches!(name.as_slice(), [0x2e] | [0x2e, 0x2e]) {
                out.push(name);
            }
            if next == 0 {
                break;
            }
            at = at.checked_add(next).ok_or_else(bad)?;
        }
    }
}

/// `h`'s file removed as Go's Deleteat removes one (go1.26.1 at_windows.go): its name gone
/// at once though another handle holds it open (FILE_DISPOSITION_FLAG_POSIX_SEMANTICS), a
/// read-only one too (FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE); where the system or
/// its file system takes no such flag (ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED), as
/// DeleteFileW removes one (FileDispositionInfo). `h` was opened with DELETE access; a link
/// opened as itself (FILE_OPEN_REPARSE_POINT) is removed itself.
pub fn delete(h: BorrowedHandle<'_>) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_DISPOSITION_INFO, FileDispositionInfo};
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: an info of its own size on a handle the caller holds.
    let done = unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).unwrap_or(0),
        )
    };
    if done != 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if !matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)
    ) {
        return Err(e);
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: as above.
    let done = unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(0),
        )
    };
    if done == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Another name, `new` in `ndir`, of the file `old` in `odir` names, never over one
/// (FileLinkInformation, its RootDirectory `ndir`): the link itself where `old` is one
/// (FILE_OPEN_REPARSE_POINT), as Go's Linkat makes one (go1.26.1 at_windows.go). Names as
/// [`create_at`] opens them.
pub fn hard_link_at(
    odir: BorrowedHandle<'_>,
    old: &[u16],
    ndir: BorrowedHandle<'_>,
    new: &[u16],
) -> std::io::Result<()> {
    let linked = create_at(
        odir,
        old,
        FILE_WRITE_ATTRIBUTES,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )?;
    let info = named(0, ndir, new).map_err(io_error)?;
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a FILE_LINK_INFORMATION laid out by `named`, its length, on a handle this holds.
    let status = unsafe {
        NtSetInformationFile(
            linked.as_raw_handle(),
            &mut io,
            info.as_ptr().cast(),
            u32::try_from(info.len()).unwrap_or(u32::MAX),
            FileLinkInformation,
        )
    };
    if status < 0 {
        return Err(win32_error(status));
    }
    Ok(())
}

/// An NTFS symbolic link `name` in `dir` to `substitute` (the name Windows follows), shown
/// as `print`: a directory's link where `directory`, a file's otherwise, its target relative
/// to the link's directory where `relative` (SYMLINK_FLAG_RELATIVE). Made as Go's Symlinkat
/// makes one in a directory's handle (go1.26.1 internal/syscall/windows/at_windows.go), as
/// CreateSymbolicLinkW cannot: a new file opened as itself (FILE_CREATE,
/// FILE_OPEN_REPARSE_POINT), given a symbolic link's reparse point (FSCTL_SET_REPARSE_POINT,
/// a SymbolicLinkReparseBuffer, MS-FSCC 2.1.2.4), and removed again where that fails; with
/// SeCreateSymbolicLinkPrivilege enabled while it is made, where this thread may enable it
/// (`with_symlink_privilege`). Where it may not (no Developer Mode, not elevated) it fails
/// as CreateSymbolicLinkW does, ERROR_PRIVILEGE_NOT_HELD.
pub fn host_symlink_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    substitute: &[u16],
    print: &[u16],
    directory: bool,
    relative: bool,
) -> std::io::Result<()> {
    let too_long =
        || std::io::Error::from_raw_os_error(i32::try_from(ERROR_FILENAME_EXCED_RANGE).unwrap_or(0));
    let len = |n: &[u16]| n.len().checked_mul(2).and_then(|b| u16::try_from(b).ok());
    let (sub_len, print_len) = (
        len(substitute).ok_or_else(too_long)?,
        len(print).ok_or_else(too_long)?,
    );
    // Its substitute name's offset and length, its print name's, its flags, then the names.
    let mut data = Vec::new();
    for word in [0, sub_len, sub_len, print_len] {
        data.extend(word.to_le_bytes());
    }
    data.extend(u32::from(relative).to_le_bytes());
    for c in substitute.iter().chain(print) {
        data.extend(c.to_le_bytes());
    }
    let kind = if directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    with_symlink_privilege(|| {
        let made = create_at(
            dir,
            name,
            FILE_WRITE_ATTRIBUTES | DELETE,
            0,
            FILE_CREATE,
            FILE_OPEN_REPARSE_POINT | kind,
        )?;
        if let Err(e) = set_reparse_io(made.as_handle(), IO_REPARSE_TAG_SYMLINK, &data) {
            let _ = delete(made.as_handle());
            return Err(e);
        }
        Ok(())
    })
}

/// `f` run with SeCreateSymbolicLinkPrivilege enabled in this thread's own copy of its
/// process's token (ImpersonateSelf, AdjustTokenPrivileges), reverted as it ends
/// (RevertToSelf), as CreateSymbolicLinkW enables the privilege it holds and Go's
/// withPrivilege does (go1.26.1 at_windows.go): an elevated administrator holds it disabled.
/// Where any step fails, `f` is run anyway, and fails as the privilege's absence makes it.
fn with_symlink_privilege<T>(f: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, ImpersonateSelf, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, RevertToSelf,
        SE_PRIVILEGE_ENABLED, SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
    /// Reverts this thread to its process's token as it drops.
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // SAFETY: ends the impersonation ImpersonateSelf began on this thread.
            unsafe { RevertToSelf() };
        }
    }
    // SAFETY: a copy of the process's token for this thread alone, at a level of its own.
    if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
        return f();
    }
    let _revert = Revert;
    let mut token = std::ptr::null_mut();
    // SAFETY: this thread's pseudo-handle, the token's handle written into `token`.
    if unsafe {
        OpenThreadToken(
            GetCurrentThread(),
            TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
            0,
            &mut token,
        )
    } == 0
    {
        return f();
    }
    // SAFETY: a handle OpenThreadToken just opened, owned here alone.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let name: Vec<u16> = "SeCreateSymbolicLinkPrivilege\0".encode_utf16().collect();
    let mut luid = LUID::default();
    // SAFETY: a NUL-terminated name, and a LUID of this frame's written.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) } == 0 {
        return f();
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: a token this frame holds, opened to adjust its privileges, and a
    // TOKEN_PRIVILEGES of this frame's; no previous state asked. A privilege not held is
    // left as it is (ERROR_NOT_ALL_ASSIGNED), and `f` fails as it would.
    unsafe {
        AdjustTokenPrivileges(
            token.as_raw_handle(),
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    f()
}

/// The EA a Linux extended attribute `name` is kept in: its bytes as hexadecimal after
/// `SH.`, as NTFS keeps an EA's name in upper case, ASCII, and at most 255 bytes long, where
/// Linux's names are any bytes, case kept (so at most 126 bytes of one, ERANGE past it). Its
/// value is the attribute's after a byte of 0 ([`XATTR_KEPT`]), as NTFS keeps no EA of no
/// value, where Linux keeps an attribute of one (overlayfs's `metacopy`).
pub fn xattr_ea(name: &[u8]) -> Result<Vec<u8>, Errno> {
    if name.len() > 126 {
        return Err(ERANGE);
    }
    let mut out = b"SH.".to_vec();
    for b in name {
        out.extend(format!("{b:02X}").bytes());
    }
    Ok(out)
}

/// The Linux extended attribute's name an EA [`xattr_ea`] made is of: none for any other EA.
pub fn ea_xattr(ea: &[u8]) -> Option<Vec<u8>> {
    let hex = ea.strip_prefix(b"SH.")?;
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// What a Linux extended attribute's value is kept after in its EA ([`xattr_ea`]).
pub const XATTR_KEPT: &[u8] = &[0];

/// The most a file's extended attributes NTFS keeps take together.
const EA_ROOM: usize = 64 << 10;

/// A file's extended attributes, each its name and its value.
pub type Eas = Vec<(Vec<u8>, Vec<u8>)>;

/// `h`'s extended attributes, each name and value (NtQueryEaFile, all at once from the
/// first: NTFS keeps at most 64 KiB of a file's), none where it has none.
pub fn eas(h: BorrowedHandle<'_>) -> Result<Eas, Errno> {
    let mut buf = vec![0u8; EA_ROOM];
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a buffer of its length; every EA, from the first, no list of names.
    let status = unsafe {
        NtQueryEaFile(
            h.as_raw_handle(),
            &mut io,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).unwrap_or(0),
            false,
            std::ptr::null(),
            0,
            std::ptr::null(),
            true,
        )
    };
    match status {
        STATUS_NO_EAS_ON_FILE | STATUS_NO_MORE_EAS => return Ok(Vec::new()),
        s if s < 0 => return Err(errno(s)),
        _ => {}
    }
    let mut out = Vec::new();
    let mut at = 0usize;
    // FILE_FULL_EA_INFORMATION: its next's offset, flags, name's and value's lengths, its
    // name and a NUL, its value.
    while let Some(head) = buf.get(at..at + 8).and_then(|h| <[u8; 8]>::try_from(h).ok()) {
        let next = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let name_len = usize::from(head[5]);
        let value_len = usize::from(u16::from_le_bytes([head[6], head[7]]));
        let name = buf.get(at + 8..at + 8 + name_len).ok_or(EIO)?;
        let value = buf
            .get(at + 9 + name_len..at + 9 + name_len + value_len)
            .ok_or(EIO)?;
        out.push((name.to_vec(), value.to_vec()));
        if next == 0 {
            break;
        }
        at = at.checked_add(next).ok_or(EIO)?;
    }
    Ok(out)
}

/// `h`'s extended attributes set, each `(name, value)`, an empty value removing its name
/// (NtSetEaFile, FILE_FULL_EA_INFORMATION entries four-byte aligned).
pub fn set_eas(h: BorrowedHandle<'_>, eas: &[(&[u8], &[u8])]) -> Result<(), Errno> {
    let mut list = Vec::new();
    for (i, (name, value)) in eas.iter().enumerate() {
        let name_len = u8::try_from(name.len()).map_err(|_| ENAMETOOLONG)?;
        let value_len = u16::try_from(value.len()).map_err(|_| E2BIG)?;
        let start = list.len();
        list.extend([0u8; 4]);
        list.push(0);
        list.push(name_len);
        list.extend(value_len.to_le_bytes());
        list.extend_from_slice(name);
        list.push(0);
        list.extend_from_slice(value);
        if i + 1 < eas.len() {
            while (list.len() - start) % 4 != 0 {
                list.push(0);
            }
            let next = u32::try_from(list.len() - start).map_err(|_| E2BIG)?;
            if let Some(slot) = list.get_mut(start..start + 4) {
                slot.copy_from_slice(&next.to_le_bytes());
            }
        }
    }
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a list of this frame's, its length.
    let status = unsafe {
        NtSetEaFile(
            h.as_raw_handle(),
            &mut io,
            list.as_ptr().cast(),
            u32::try_from(list.len()).map_err(|_| E2BIG)?,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(())
}

/// WSL's metadata of `h`'s file set, each where given: its owner, group and mode (`$LXUID`,
/// `$LXGID`, `$LXMOD`, each a little-endian u32) and a device file's major and minor
/// (`$LXDEV`, two), which FileStatLxInformation reads back (PM M362).
pub fn set_lx(
    h: BorrowedHandle<'_>,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
    dev: Option<(u32, u32)>,
) -> Result<(), Errno> {
    let (uid, gid, mode) = (
        uid.map(u32::to_le_bytes),
        gid.map(u32::to_le_bytes),
        mode.map(u32::to_le_bytes),
    );
    let dev = dev.map(|(major, minor)| {
        let mut both = [0u8; 8];
        both[..4].copy_from_slice(&major.to_le_bytes());
        both[4..].copy_from_slice(&minor.to_le_bytes());
        both
    });
    let mut list: Vec<(&[u8], &[u8])> = Vec::new();
    if let Some(v) = &uid {
        list.push((b"$LXUID", v));
    }
    if let Some(v) = &gid {
        list.push((b"$LXGID", v));
    }
    if let Some(v) = &mode {
        list.push((b"$LXMOD", v));
    }
    if let Some(v) = &dev {
        list.push((b"$LXDEV", v));
    }
    if list.is_empty() {
        return Ok(());
    }
    set_eas(h, &list)
}

/// `h`'s file made a reparse point of `tag` holding `data` (FSCTL_SET_REPARSE_POINT): a
/// REPARSE_DATA_BUFFER, its tag, its data's length and a reserved word, then the data.
pub fn set_reparse(h: BorrowedHandle<'_>, tag: u32, data: &[u8]) -> Result<(), Errno> {
    set_reparse_io(h, tag, data).map_err(|e| win32_errno(&e))
}

/// [`set_reparse`], its error Windows' own: ERROR_FILENAME_EXCED_RANGE where `data` is
/// longer than a reparse point holds, which [`win32_errno`] makes ENAMETOOLONG.
fn set_reparse_io(h: BorrowedHandle<'_>, tag: u32, data: &[u8]) -> std::io::Result<()> {
    let too_long =
        || std::io::Error::from_raw_os_error(i32::try_from(ERROR_FILENAME_EXCED_RANGE).unwrap_or(0));
    let mut buf = Vec::with_capacity(8 + data.len());
    buf.extend(tag.to_le_bytes());
    buf.extend(u16::try_from(data.len()).map_err(|_| too_long())?.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend_from_slice(data);
    let mut returned = 0u32;
    // SAFETY: a buffer of this frame's, its length, on a handle the caller holds.
    let done = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr().cast(),
            u32::try_from(buf.len()).map_err(|_| too_long())?,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The tag and data of the reparse point `h`'s file is (FSCTL_GET_REPARSE_POINT), none
/// where it is none.
pub fn reparse(h: BorrowedHandle<'_>) -> Result<Option<(u32, Vec<u8>)>, Errno> {
    // MAXIMUM_REPARSE_DATA_BUFFER_SIZE.
    let mut buf = vec![0u8; 16 << 10];
    let mut returned = 0u32;
    // SAFETY: a buffer of this frame's, its length, on a handle the caller holds.
    let done = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).unwrap_or(0),
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(i32::try_from(ERROR_NOT_A_REPARSE_POINT).unwrap_or(0)) {
            return Ok(None);
        }
        return Err(win32_errno(&e));
    }
    let got = buf.get(..usize::try_from(returned).unwrap_or(0)).ok_or(EIO)?;
    let head = got
        .get(..8)
        .and_then(|h| <[u8; 8]>::try_from(h).ok())
        .ok_or(EIO)?;
    let tag = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let len = usize::from(u16::from_le_bytes([head[4], head[5]]));
    Ok(Some((tag, got.get(8..8 + len).ok_or(EIO)?.to_vec())))
}

/// The version an LX symlink's data starts with (MS-FSCC 2.1.2.7), before its target's
/// bytes.
const LX_SYMLINK_VERSION: u32 = 2;

/// A symlink `name` in `dir` to `target`, as WSL makes one: a new empty file made an LX
/// symlink reparse point holding its target's bytes (MS-FSCC 2.1.2.7), never an NTFS
/// symlink, which an unelevated process makes only in Developer Mode (PM M362).
pub fn symlink_at(dir: BorrowedHandle<'_>, name: &[u16], target: &[u8]) -> Result<OwnedHandle, Errno> {
    let made = open_at(
        dir,
        name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    let mut data = LX_SYMLINK_VERSION.to_le_bytes().to_vec();
    data.extend_from_slice(target);
    set_reparse(made.as_handle(), LX_SYMLINK, &data)?;
    Ok(made)
}

/// What the symlink `h` is to, its target's bytes: an LX symlink's as it holds them; an
/// NTFS symlink's or a junction's, which the host made, as [`host_target`] turns it; EINVAL
/// for anything else, as readlink(2) answers of what is no symlink.
pub fn readlink(h: BorrowedHandle<'_>) -> Result<Vec<u8>, Errno> {
    match reparse(h)? {
        Some((LX_SYMLINK, data)) if data.get(..4) == Some(&LX_SYMLINK_VERSION.to_le_bytes()[..]) => {
            Ok(data.get(4..).unwrap_or_default().to_vec())
        }
        Some((IO_REPARSE_TAG_SYMLINK, data)) => host_target(h, &data, true),
        Some((IO_REPARSE_TAG_MOUNT_POINT, data)) => host_target(h, &data, false),
        _ => Err(EINVAL),
    }
}

/// The target of the host's NTFS symlink (`symlink`) or junction `h`, of reparse data
/// `data` (MS-FSCC 2.1.2.4, 2.1.2.5: its substitute name's offset and length, its print
/// name's, a symlink's flags, then the names), as a Linux symlink holds one, as Linux's
/// symlinks show to a guest on Unix: a relative symlink's target as it is; an absolute
/// one's, or a junction's (pnpm's node_modules on Windows), on the link's own volume, the
/// path to it from the link's directory, so that one within a shared tree leads there in
/// the guest as on the host; any other as NTFS holds it. Each with Linux's separators, its
/// names' characters the guest's ([`guest_name`]).
fn host_target(h: BorrowedHandle<'_>, data: &[u8], symlink: bool) -> Result<Vec<u8>, Errno> {
    let word = |at: usize| -> Result<usize, Errno> {
        let b: [u8; 2] = data
            .get(at..at + 2)
            .and_then(|b| b.try_into().ok())
            .ok_or(EINVAL)?;
        Ok(usize::from(u16::from_le_bytes(b)))
    };
    let (offset, length) = (word(0)?, word(2)?);
    let (relative, names): (bool, usize) = if symlink {
        let flags: [u8; 4] = data.get(8..12).and_then(|b| b.try_into().ok()).ok_or(EINVAL)?;
        (u32::from_le_bytes(flags) & SYMLINK_FLAG_RELATIVE != 0, 12)
    } else {
        (false, 8)
    };
    let at = names.checked_add(offset).ok_or(EINVAL)?;
    let name: Vec<u16> = data
        .get(at..at.checked_add(length).ok_or(EINVAL)?)
        .ok_or(EINVAL)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let backslash = u16::from(b'\\');
    let parts = |path: &[u16]| -> Vec<Vec<u16>> {
        path.split(|&c| c == backslash)
            .filter(|p| !p.is_empty())
            .map(<[u16]>::to_vec)
            .collect()
    };
    let joined = |parts: &[Vec<u16>]| -> Vec<u8> {
        let mut out = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend(guest_name(p));
        }
        out
    };
    if relative {
        return Ok(joined(&parts(&name)));
    }
    let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    let target = name.strip_prefix(prefix.as_slice()).unwrap_or(&name);
    let link = final_path(h)?;
    let (to, from) = (parts(target), parts(&link));
    // A name as NTFS compares it where its directory does: its case aside.
    let same = |a: &[u16], b: &[u16]| {
        String::from_utf16_lossy(a).to_lowercase() == String::from_utf16_lossy(b).to_lowercase()
    };
    // Both on one drive (`X:`), the link's directory its components but its own last.
    let drive = |p: &[Vec<u16>]| {
        p.first()
            .filter(|d| d.len() == 2 && d.get(1) == Some(&u16::from(b':')))
            .cloned()
    };
    let (Some(a), Some(b)) = (drive(&to), drive(&from)) else {
        return Ok(joined(&to));
    };
    let Some((_, dir)) = from.split_last().filter(|_| same(&a, &b)) else {
        return Ok(joined(&to));
    };
    let common = dir.iter().zip(&to).take_while(|(x, y)| same(x, y)).count();
    let up = dir.len().saturating_sub(common);
    let mut out: Vec<Vec<u16>> = vec![vec![u16::from(b'.'), u16::from(b'.')]; up];
    out.extend(to.iter().skip(common).cloned());
    if out.is_empty() {
        return Ok(b".".to_vec());
    }
    Ok(joined(&out))
}

/// The path of `h`'s file, as the host names it (GetFinalPathNameByHandleW, its DOS name),
/// without `\\?\`.
fn final_path(h: BorrowedHandle<'_>) -> Result<Vec<u16>, Errno> {
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: a buffer of the length given, of a handle the caller holds.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h.as_raw_handle(),
                buf.as_mut_ptr(),
                u32::try_from(buf.len()).unwrap_or(u32::MAX),
                0,
            )
        };
        let n = usize::try_from(n).map_err(|_| EIO)?;
        if n == 0 {
            return Err(last_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        // Too short: n is the length it needs, its NUL counted.
        buf = vec![0u16; n];
    }
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    Ok(buf
        .strip_prefix(prefix.as_slice())
        .map_or(buf.clone(), <[u16]>::to_vec))
}

/// A FIFO, character or block device or Unix socket `name` in `dir`, by its mode's kind
/// (`S_IFIFO`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK`), as WSL makes one ([`make_special`]).
pub fn special_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    kind: u32,
    dev: (u32, u32),
) -> Result<OwnedHandle, Errno> {
    let made = open_at(
        dir,
        name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    make_special(made.as_handle(), kind, dev)?;
    Ok(made)
}

/// `h`'s file, an empty one, made the FIFO, character or block device or Unix socket of
/// `kind` as WSL makes one: a reparse point of its LX tag, a device's major and minor in
/// `$LXDEV` (PM M362). `h` may write its data and attributes.
pub fn make_special(h: BorrowedHandle<'_>, kind: u32, dev: (u32, u32)) -> Result<(), Errno> {
    let tag = match kind {
        0o010000 => LX_FIFO,
        0o020000 => LX_CHR,
        0o060000 => LX_BLK,
        0o140000 => AF_UNIX,
        _ => return Err(EINVAL),
    };
    set_reparse(h, tag, &[])?;
    if matches!(tag, LX_CHR | LX_BLK) {
        set_lx(h, None, None, None, Some(dev))?;
    }
    Ok(())
}

/// The access `h` was opened with (NtQueryObject's ObjectBasicInformation), which NT checks
/// each call on it against, where Unix checks its file's.
pub fn granted(h: BorrowedHandle<'_>) -> Result<u32, Errno> {
    let mut info = PUBLIC_OBJECT_BASIC_INFORMATION::default();
    // SAFETY: a buffer of its own size for the class asked, of a handle the caller holds.
    let status = unsafe {
        NtQueryObject(
            h.as_raw_handle(),
            ObjectBasicInformation,
            (&raw mut info).cast(),
            u32::try_from(size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>()).unwrap_or(0),
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(info.GrantedAccess)
}

/// A FILE_RENAME_INFORMATION or FILE_LINK_INFORMATION, laid out as Win32's FILE_RENAME_INFO: `flags`, its root
/// directory `root` and the name `name` in it, as one buffer, the name past the struct's
/// own `FileName`.
fn named(flags: u32, root: BorrowedHandle<'_>, name: &[u16]) -> Result<Vec<u8>, Errno> {
    use std::mem::offset_of;
    let bytes = name.len().checked_mul(2).ok_or(ENAMETOOLONG)?;
    let at = offset_of!(FILE_RENAME_INFO, FileName);
    let mut buf = vec![
        0u8;
        at.checked_add(bytes)
            .ok_or(ENAMETOOLONG)?
            .max(size_of::<FILE_RENAME_INFO>())
    ];
    let field = |buf: &mut [u8], at: usize, value: &[u8]| -> Result<(), Errno> {
        buf.get_mut(at..at + value.len())
            .ok_or(EINVAL)?
            .copy_from_slice(value);
        Ok(())
    };
    field(&mut buf, 0, &flags.to_le_bytes())?;
    field(
        &mut buf,
        offset_of!(FILE_RENAME_INFO, RootDirectory),
        &(root.as_raw_handle() as usize).to_le_bytes(),
    )?;
    field(
        &mut buf,
        offset_of!(FILE_RENAME_INFO, FileNameLength),
        &u32::try_from(bytes).map_err(|_| ENAMETOOLONG)?.to_le_bytes(),
    )?;
    for (i, c) in name.iter().enumerate() {
        field(&mut buf, at + i * 2, &c.to_le_bytes())?;
    }
    Ok(buf)
}

/// `name` in `dir` removed as Linux removes one (FILE_DISPOSITION_FLAG_POSIX_SEMANTICS): its
/// name gone at once though a handle holds the file open, its read-only attribute ignored;
/// a directory where `rmdir`, else anything else.
pub fn unlink_at(dir: BorrowedHandle<'_>, name: &[u16], rmdir: bool) -> Result<(), Errno> {
    let kind = if rmdir {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    let gone = open_at(dir, name, DELETE, FILE_OPEN, kind)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: an info of its own size on a handle this holds.
    let done = unsafe {
        SetFileInformationByHandle(
            gone.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).unwrap_or(0),
        )
    };
    if done == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// `old` in `odir` renamed `new` in `ndir` as Linux renames (FILE_RENAME_FLAG_POSIX_SEMANTICS):
/// over a file a handle holds open, its holder keeping what it held; over nothing where
/// not `replace` (EEXIST).
pub fn rename_at(
    odir: BorrowedHandle<'_>,
    old: &[u16],
    ndir: BorrowedHandle<'_>,
    new: &[u16],
    replace: bool,
) -> Result<(), Errno> {
    let moving = open_at(odir, old, DELETE, FILE_OPEN, 0)?;
    // POSIX semantics where it replaces, a held file then replaced (M362).
    let flags = if replace {
        FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS
    } else {
        0
    };
    let info = named(flags, ndir, new)?;
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a FILE_RENAME_INFORMATION laid out above, its length, on a handle this holds.
    let status = unsafe {
        NtSetInformationFile(
            moving.as_raw_handle(),
            &mut io,
            info.as_ptr().cast(),
            u32::try_from(info.len()).map_err(|_| ENAMETOOLONG)?,
            FileRenameInformationEx,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(())
}

/// Another name, `new` in `ndir`, of `old` in `odir` (FileLinkInformation), never over one.
pub fn link_at(
    odir: BorrowedHandle<'_>,
    old: &[u16],
    ndir: BorrowedHandle<'_>,
    new: &[u16],
) -> Result<(), Errno> {
    let linked = open_at(
        odir,
        old,
        FILE_WRITE_ATTRIBUTES,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )?;
    let info = named(0, ndir, new)?;
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a FILE_LINK_INFORMATION laid out above, its length, on a handle this holds.
    let status = unsafe {
        NtSetInformationFile(
            linked.as_raw_handle(),
            &mut io,
            info.as_ptr().cast(),
            u32::try_from(info.len()).map_err(|_| ENAMETOOLONG)?,
            FileLinkInformation,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(())
}

/// A directory `name` in `dir`, case-sensitive as WSL makes its own
/// (FileCaseSensitiveInformation), its handle.
pub fn mkdir_at(dir: BorrowedHandle<'_>, name: &[u16]) -> Result<OwnedHandle, Errno> {
    let made = open_at(
        dir,
        name,
        FILE_LIST_DIRECTORY | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
    )?;
    case_sensitive(made.as_handle())?;
    Ok(made)
}

/// The directory `h` made case-sensitive, as WSL makes its own (FileCaseSensitiveInformation):
/// its names matched in their own case alone. `h` may write its attributes.
pub fn case_sensitive(h: BorrowedHandle<'_>) -> Result<(), Errno> {
    let info = FILE_CASE_SENSITIVE_INFORMATION {
        Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR,
    };
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: an info of its own size on a handle the caller holds.
    let status = unsafe {
        NtSetInformationFile(
            h.as_raw_handle(),
            &mut io,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_CASE_SENSITIVE_INFORMATION>()).unwrap_or(0),
            FileCaseSensitiveInformation,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(())
}

/// The EA that marks a directory the root of a Linux tree kept as this crate keeps one,
/// whole, by shards (shards-archive's `apply_layer_lx`): one whose every name, mode, link
/// and device is Linux's, which a walk of it reads as Linux's (shards-build's host walk). A
/// tree of the host's own carries none, though WSL kept some of its files' metadata.
pub const LINUX_TREE: &[u8] = b"SHARDS.LINUX";

/// Whether the directory `h` is the root of a Linux tree ([`LINUX_TREE`]).
pub fn is_linux_tree(h: BorrowedHandle<'_>) -> Result<bool, Errno> {
    Ok(eas(h)?
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(LINUX_TREE)))
}

/// The directory `h` made the root of a Linux tree ([`LINUX_TREE`]): case-sensitive, and
/// marked so. `h` may write its attributes and EAs.
pub fn make_linux_tree(h: BorrowedHandle<'_>) -> Result<(), Errno> {
    case_sensitive(h)?;
    set_eas(h, &[(LINUX_TREE, &[1])])
}

/// `e` as an [`std::io::Error`] of its kind, its message the errno's own.
pub fn io_error(e: Errno) -> std::io::Error {
    use std::io::ErrorKind as K;
    let kind = match e {
        ENOENT => K::NotFound,
        EEXIST => K::AlreadyExists,
        EACCES => K::PermissionDenied,
        ENOTDIR => K::NotADirectory,
        EISDIR => K::IsADirectory,
        ENOTEMPTY => K::DirectoryNotEmpty,
        EINVAL | EILSEQ => K::InvalidInput,
        ENOSPC => K::StorageFull,
        EROFS => K::ReadOnlyFilesystem,
        ENAMETOOLONG => K::InvalidFilename,
        EBUSY => K::ResourceBusy,
        EXDEV => K::CrossesDevices,
        EOPNOTSUPP => K::Unsupported,
        _ => K::Other,
    };
    std::io::Error::new(kind, format!("errno {e} ({})", errno_name(e)))
}

/// The name of a Linux errno this crate answers with.
fn errno_name(e: Errno) -> &'static str {
    match e {
        ENOENT => "ENOENT",
        E2BIG => "E2BIG",
        EIO => "EIO",
        ENOMEM => "ENOMEM",
        EACCES => "EACCES",
        EBUSY => "EBUSY",
        EEXIST => "EEXIST",
        EXDEV => "EXDEV",
        ENOTDIR => "ENOTDIR",
        EISDIR => "EISDIR",
        EINVAL => "EINVAL",
        ENOSPC => "ENOSPC",
        EROFS => "EROFS",
        ERANGE => "ERANGE",
        ENAMETOOLONG => "ENAMETOOLONG",
        ENOTEMPTY => "ENOTEMPTY",
        ELOOP => "ELOOP",
        ENODATA => "ENODATA",
        EILSEQ => "EILSEQ",
        EOPNOTSUPP => "EOPNOTSUPP",
        _ => "an errno",
    }
}

/// `h` as a [`File`](std::fs::File) that never closes it, for std's calls on Windows
/// handles: what `h` borrows outlives it.
fn file_of(h: BorrowedHandle<'_>) -> std::mem::ManuallyDrop<std::fs::File> {
    // SAFETY: a handle the caller holds open while the File lives, which never closes it.
    std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(h.as_raw_handle()) })
}

/// `h`'s file read at `offset` into `buf`: the bytes read, none at its end.
pub fn read_at(h: BorrowedHandle<'_>, buf: &mut [u8], offset: u64) -> Result<usize, Errno> {
    std::os::windows::fs::FileExt::seek_read(&*file_of(h), buf, offset).map_err(|e| win32_errno(&e))
}

/// `buf` written to `h`'s file at `offset`: the bytes written.
pub fn write_at(h: BorrowedHandle<'_>, buf: &[u8], offset: u64) -> Result<usize, Errno> {
    std::os::windows::fs::FileExt::seek_write(&*file_of(h), buf, offset).map_err(|e| win32_errno(&e))
}

/// `h`'s file cut or grown to `size` bytes, its end-of-file set.
pub fn truncate(h: BorrowedHandle<'_>, size: u64) -> Result<(), Errno> {
    file_of(h).set_len(size).map_err(|e| win32_errno(&e))
}

/// `h`'s file's data and metadata written through to its disk (FlushFileBuffers).
pub fn sync(h: BorrowedHandle<'_>) -> Result<(), Errno> {
    file_of(h).sync_all().map_err(|e| win32_errno(&e))
}

/// An NT time, 100 ns since 1601, of seconds and nanoseconds since the Unix epoch.
fn nt_time((secs, nanos): (u64, u32)) -> i64 {
    const SINCE_1601: i64 = 116_444_736_000_000_000;
    i64::try_from(secs)
        .unwrap_or(i64::MAX / 10_000_000)
        .saturating_mul(10_000_000)
        .saturating_add(i64::from(nanos / 100))
        .saturating_add(SINCE_1601)
}

/// `h`'s file's access and modification times set, each where given, as seconds and
/// nanoseconds since the Unix epoch (FileBasicInfo, whose zeros change nothing).
pub fn set_times(
    h: BorrowedHandle<'_>,
    atime: Option<(u64, u32)>,
    mtime: Option<(u64, u32)>,
) -> Result<(), Errno> {
    let info = FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: atime.map_or(0, nt_time),
        LastWriteTime: mtime.map_or(0, nt_time),
        ChangeTime: 0,
        FileAttributes: 0,
    };
    // SAFETY: an info of its own size on a handle the caller holds.
    let done = unsafe {
        SetFileInformationByHandle(
            h.as_raw_handle(),
            FileBasicInfo,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0),
        )
    };
    if done == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// What a volume holds and has room for, in its allocation units of `block_size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    pub block_size: u32,
    pub blocks: u64,
    /// Free, and free to this process (its quota's).
    pub free: u64,
    pub available: u64,
}

/// FILE_FS_FULL_SIZE_INFORMATION (ntifs.h).
#[repr(C)]
#[derive(Default)]
struct FullSize {
    total: i64,
    caller_available: i64,
    actual_available: i64,
    sectors_per_unit: u32,
    bytes_per_sector: u32,
}

/// The [`Volume`] `h`'s file is on (NtQueryVolumeInformationFile, FileFsFullSizeInformation).
pub fn volume(h: BorrowedHandle<'_>) -> Result<Volume, Errno> {
    let mut size = FullSize::default();
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: a buffer of its own size for the class asked, on a handle the caller holds.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            h.as_raw_handle(),
            &mut io,
            (&raw mut size).cast(),
            u32::try_from(size_of::<FullSize>()).unwrap_or(0),
            FileFsFullSizeInformation,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    let units = |n: i64| u64::try_from(n).unwrap_or(0);
    Ok(Volume {
        block_size: size.sectors_per_unit.saturating_mul(size.bytes_per_sector),
        blocks: units(size.total),
        free: units(size.actual_available),
        available: units(size.caller_available),
    })
}

/// An entry of a directory, as a guest's READDIR tells it: its name, its key's file ID
/// and its kind (a mode's type bits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: Vec<u8>,
    pub ino: u64,
    pub kind: u32,
}

/// The next of `h`'s directory's entries, from its first where `restart`, as many as one
/// call gives (GetFileInformationByHandleEx, FileIdExtdDirectoryInfo, whose entries name
/// their reparse tags): none once it has given them all. `.` and `..` among them, as NTFS
/// lists them.
pub fn list(h: BorrowedHandle<'_>, restart: bool) -> Result<Vec<Listed>, Errno> {
    list_within(h, restart, 64 << 10)
}

/// The room one entry of the longest name NTFS keeps (255 UTF-16 units) takes in a
/// [`list_within`] call's buffer: FILE_ID_EXTD_DIR_INFO's fields before its name, then it.
pub const LISTED_ROOM: usize = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName) + 255 * 2;

/// [`list`]'s next entries, as many as `room` bytes of them hold (never less than
/// [`LISTED_ROOM`], so that each call gives one at least). The enumeration's place is the
/// handle's, past the last entry given, and a call that does not restart goes on from
/// there (Microsoft Learn, FILE_INFO_BY_HANDLE_CLASS: the Restart classes "force the
/// enumeration operation to start again from the beginning"; ZwQueryDirectoryFile: a scan
/// not restarted resumes after the entries its handle's last call returned).
pub fn list_within(h: BorrowedHandle<'_>, restart: bool, room: usize) -> Result<Vec<Listed>, Errno> {
    use std::mem::offset_of;
    let mut buf = vec![0u8; room.max(LISTED_ROOM)];
    let class = if restart {
        FileIdExtdDirectoryRestartInfo
    } else {
        FileIdExtdDirectoryInfo
    };
    // SAFETY: a buffer of its length for the class asked, on a handle the caller holds.
    let done = unsafe {
        GetFileInformationByHandleEx(
            h.as_raw_handle(),
            class,
            buf.as_mut_ptr().cast(),
            u32::try_from(buf.len()).unwrap_or(0),
        )
    };
    if done == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(i32::try_from(ERROR_NO_MORE_FILES).unwrap_or(0)) {
            return Ok(Vec::new());
        }
        return Err(win32_errno(&e));
    }
    let word = |at: usize| -> Option<u32> {
        let b = buf.get(at..at + 4)?;
        Some(u32::from_le_bytes([
            *b.first()?,
            *b.get(1)?,
            *b.get(2)?,
            *b.get(3)?,
        ]))
    };
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let next = word(at).ok_or(EIO)? as usize;
        let attributes = word(at + offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes)).ok_or(EIO)?;
        let name_len = word(at + offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength)).ok_or(EIO)? as usize;
        let tag = word(at + offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag)).ok_or(EIO)?;
        let id_at = at + offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
        let id = buf.get(id_at..id_at + 8).ok_or(EIO)?;
        let ino = u64::from_le_bytes(<[u8; 8]>::try_from(id).map_err(|_| EIO)?);
        let name_at = at + offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let name: Vec<u16> = buf
            .get(name_at..name_at + name_len)
            .ok_or(EIO)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let kind = match tag {
            LX_SYMLINK | IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT => 0o120000,
            LX_FIFO => 0o010000,
            LX_CHR => 0o020000,
            LX_BLK => 0o060000,
            AF_UNIX => 0o140000,
            _ if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 => 0o040000,
            _ => 0o100000,
        };
        out.push(Listed {
            name: guest_name(&name),
            ino,
            kind,
        });
        if next == 0 {
            break;
        }
        at = at.checked_add(next).ok_or(EIO)?;
    }
    Ok(out)
}

/// Linux's errno for a Win32 error a call answered with, by its NTSTATUS's.
fn win32_errno(e: &std::io::Error) -> Errno {
    match e.raw_os_error().and_then(|c| u32::try_from(c).ok()) {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => ENOENT,
        Some(ERROR_ACCESS_DENIED) => EACCES,
        Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => EEXIST,
        Some(ERROR_SHARING_VIOLATION) => EBUSY,
        Some(ERROR_INVALID_PARAMETER | ERROR_INVALID_NAME) => EINVAL,
        Some(ERROR_NOT_SUPPORTED) => EOPNOTSUPP,
        Some(ERROR_DISK_FULL) => ENOSPC,
        Some(ERROR_DIR_NOT_EMPTY) => ENOTEMPTY,
        Some(ERROR_DIRECTORY) => ENOTDIR,
        Some(ERROR_NOT_SAME_DEVICE) => EXDEV,
        Some(ERROR_WRITE_PROTECT) => EROFS,
        Some(ERROR_FILENAME_EXCED_RANGE) => ENAMETOOLONG,
        _ => EIO,
    }
}

/// [`win32_errno`] of the calling thread's last error.
fn last_error() -> Errno {
    win32_errno(&std::io::Error::last_os_error())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
    };

    use super::*;

    /// Linux's attribute names are EAs of names of their own, NTFS's upper-case ASCII,
    /// every byte and its case kept and turned back; one too long for an EA's name refused
    /// (ERANGE); WSL's EAs and any other none of Linux's.
    #[test]
    fn attribute_names_are_eas_of_their_own() {
        for name in [
            &b"user.x"[..],
            b"user.Mixed.Case",
            b"trusted.overlay.opaque",
            b"user.\xff?",
        ] {
            let ea = xattr_ea(name).unwrap();
            assert!(
                ea.iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'.'),
                "{ea:?}"
            );
            assert_eq!(ea_xattr(&ea).as_deref(), Some(name));
        }
        assert_ne!(xattr_ea(b"user.a").unwrap(), xattr_ea(b"user.A").unwrap());
        assert_eq!(xattr_ea(&[b'a'; 126]).map(|e| e.len()), Ok(255));
        assert_eq!(xattr_ea(&[b'a'; 127]), Err(ERANGE));
        for theirs in [&b"$LXUID"[..], b"$LXMOD", b"SH.4", b"SH.ZZ", b"OTHER"] {
            assert_eq!(ea_xattr(theirs), None, "{theirs:?}");
        }
    }

    /// A guest's names keep their characters on NTFS: each a Windows name may not hold
    /// escaped as WSL escapes it, and turned back as listed; one not UTF-8, or holding a
    /// `/`, refused.
    #[test]
    fn a_guests_names_are_kept_as_wsl_keeps_them() {
        for guest in ["plain", "a:b", "what?", "<x>|y*\"z\\", "tab\there", "naïve ☃"] {
            let nt = nt_name(guest.as_bytes()).unwrap();
            assert!(
                !String::from_utf16_lossy(&nt).contains([':', '?', '<', '>', '|', '*', '"', '\\', '\t']),
                "{guest}"
            );
            assert_eq!(guest_name(&nt), guest.as_bytes(), "{guest}");
        }
        assert_eq!(nt_name(&[0xff, 0x41]), Err(EILSEQ));
        assert_eq!(nt_name(b"a/b"), Err(EINVAL));
        assert_eq!(nt_name(b""), Err(EINVAL));
    }

    /// A file made relative to a directory's handle is that directory's, and reads as a
    /// file of no owner WSL keeps: root's, 644, of the key NTFS gives it; a directory made
    /// so 755.
    #[test]
    fn files_made_beneath_a_handle_read_as_wsl_reads_them() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = shards_testdir::TempDir::new("ntfs-open").unwrap();
        let root = std::fs::File::options()
            .read(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
            .open(&*dir)
            .unwrap();
        let made = open_at(
            root.as_handle(),
            &nt_name(b"made").unwrap(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .unwrap();
        let st = stat(made.as_handle()).unwrap();
        assert_eq!((st.mode, st.uid, st.nlink, st.size), (0o100755, 0, 1, 0));
        assert!(st.ino != 0);
        assert!(dir.join("made").is_file());
        assert_eq!(
            open_at(
                root.as_handle(),
                &nt_name(b"made").unwrap(),
                FILE_GENERIC_READ,
                FILE_CREATE,
                0,
            )
            .unwrap_err(),
            EEXIST
        );
        let sub = open_at(
            root.as_handle(),
            &nt_name(b"sub").unwrap(),
            FILE_LIST_DIRECTORY,
            FILE_CREATE,
            FILE_DIRECTORY_FILE,
        )
        .unwrap();
        assert_eq!(stat(sub.as_handle()).unwrap().mode, 0o040755);
        assert_eq!(
            open_at(
                root.as_handle(),
                &nt_name(b"none").unwrap(),
                FILE_GENERIC_READ,
                FILE_OPEN,
                0,
            )
            .unwrap_err(),
            ENOENT
        );
    }

    /// A directory of this test's own, its handle as a share's root is opened.
    fn tree(label: &str) -> (shards_testdir::TempDir, std::fs::File) {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = shards_testdir::TempDir::new(label).unwrap();
        let root = std::fs::File::options()
            .read(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
            .open(&*dir)
            .unwrap();
        (dir, root)
    }

    /// WSL's metadata set reads back as a file's owner and mode, a guest's extended
    /// attributes beside it as set, and one removed gone.
    #[test]
    fn a_files_owner_mode_and_attributes_are_kept_in_its_eas() {
        let (_dir, root) = tree("ntfs-eas");
        let f = open_at(
            root.as_handle(),
            &nt_name(b"f").unwrap(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .unwrap();
        set_lx(f.as_handle(), Some(1750), Some(1751), Some(0o100600), None).unwrap();
        let st = stat(f.as_handle()).unwrap();
        assert_eq!((st.uid, st.mode), (1750, 0o100600));
        set_eas(f.as_handle(), &[(b"GUEST.A", b"one"), (b"GUEST.B", b"two")]).unwrap();
        let kept = eas(f.as_handle()).unwrap();
        assert!(
            kept.iter().any(|(n, v)| n == b"GUEST.A" && v == b"one"),
            "{kept:?}"
        );
        assert!(
            kept.iter().any(|(n, v)| n == b"GUEST.B" && v == b"two"),
            "{kept:?}"
        );
        set_eas(f.as_handle(), &[(b"GUEST.A", b"")]).unwrap();
        assert!(!eas(f.as_handle()).unwrap().iter().any(|(n, _)| n == b"GUEST.A"));
    }

    /// A guest's symlink is an LX symlink, reading back its target and stat'ing as a
    /// symlink; a FIFO and a character device each their kind, the device its number.
    #[test]
    fn symlinks_and_special_files_are_wsls_reparse_points() {
        let (_dir, root) = tree("ntfs-reparse");
        let link = symlink_at(root.as_handle(), &nt_name(b"link").unwrap(), b"../there").unwrap();
        assert_eq!(readlink(link.as_handle()).unwrap(), b"../there");
        assert_eq!(stat(link.as_handle()).unwrap().mode & 0o170000, 0o120000);
        let fifo = special_at(root.as_handle(), &nt_name(b"fifo").unwrap(), 0o010000, (0, 0)).unwrap();
        assert_eq!(stat(fifo.as_handle()).unwrap().mode & 0o170000, 0o010000);
        assert_eq!(readlink(fifo.as_handle()), Err(EINVAL));
        let chr = special_at(root.as_handle(), &nt_name(b"null").unwrap(), 0o020000, (1, 3)).unwrap();
        let st = stat(chr.as_handle()).unwrap();
        assert_eq!((st.mode & 0o170000, st.rdev), (0o020000, (1 << 8) | 3));
        let plain = open_at(
            root.as_handle(),
            &nt_name(b"plain").unwrap(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .unwrap();
        assert_eq!(reparse(plain.as_handle()), Ok(None));
    }

    /// A name removed while held is gone at once and made again; a rename goes over a held
    /// file, and over nothing where told not to replace; a link is a second name of one
    /// file; and a directory made holds two names differing in case.
    /// NTFS renames no directory a handle beneath is open in, POSIX semantics or not, nor
    /// does Win32's MoveFileEx: refused EACCES; renamed once let go. The share server lets go
    /// of what it holds beneath a directory it renames (`RENAME_NEEDS_NONE_BENEATH`).
    #[test]
    fn a_directory_a_handle_beneath_is_open_in_is_renamed_by_none() {
        let (dir, root) = tree("ntfs-beneath");
        let a = mkdir_at(root.as_handle(), &nt_name(b"a").unwrap()).unwrap();
        let b = mkdir_at(a.as_handle(), &nt_name(b"b").unwrap()).unwrap();
        drop(a);
        let (from, to) = (nt_name(b"a").unwrap(), nt_name(b"z").unwrap());
        for replace in [false, true] {
            assert_eq!(
                rename_at(root.as_handle(), &from, root.as_handle(), &to, replace),
                Err(EACCES),
                "replace {replace}"
            );
        }
        assert!(std::fs::rename(dir.join("a"), dir.join("y")).is_err());
        drop(b);
        rename_at(root.as_handle(), &from, root.as_handle(), &to, false).unwrap();
        assert!(dir.join("z").join("b").is_dir());
    }

    #[test]
    fn removal_rename_links_and_case_are_linuxs() {
        let (_dir, root) = tree("ntfs-names");
        let made_in = |dir: BorrowedHandle<'_>, name: &[u8]| {
            open_at(
                dir,
                &nt_name(name).unwrap(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
            )
            .unwrap()
        };
        let named = |name: &[u8]| nt_name(name).unwrap();
        let held = made_in(root.as_handle(), b"held");
        unlink_at(root.as_handle(), &named(b"held"), false).unwrap();
        assert_eq!(
            open_at(root.as_handle(), &named(b"held"), FILE_GENERIC_READ, FILE_OPEN, 0).unwrap_err(),
            ENOENT
        );
        drop(made_in(root.as_handle(), b"held"));
        drop(held);
        let (a, b) = (made_in(root.as_handle(), b"a"), made_in(root.as_handle(), b"b"));
        let b_ino = stat(b.as_handle()).unwrap().ino;
        assert_ne!(stat(a.as_handle()).unwrap().ino, b_ino);
        assert_eq!(
            rename_at(
                root.as_handle(),
                &named(b"b"),
                root.as_handle(),
                &named(b"a"),
                false
            ),
            Err(EEXIST)
        );
        rename_at(
            root.as_handle(),
            &named(b"b"),
            root.as_handle(),
            &named(b"a"),
            true,
        )
        .unwrap();
        let now = open_at(root.as_handle(), &named(b"a"), FILE_GENERIC_READ, FILE_OPEN, 0).unwrap();
        assert_eq!(stat(now.as_handle()).unwrap().ino, b_ino);
        link_at(root.as_handle(), &named(b"a"), root.as_handle(), &named(b"c")).unwrap();
        assert_eq!(stat(now.as_handle()).unwrap().nlink, 2);
        let d = mkdir_at(root.as_handle(), &named(b"d")).unwrap();
        let (upper, lower) = (made_in(d.as_handle(), b"A"), made_in(d.as_handle(), b"a"));
        assert_ne!(
            stat(upper.as_handle()).unwrap().ino,
            stat(lower.as_handle()).unwrap().ino
        );
        drop((upper, lower, a, b, now));
        unlink_at(d.as_handle(), &named(b"A"), false).unwrap();
        unlink_at(d.as_handle(), &named(b"a"), false).unwrap();
        drop(d);
        unlink_at(root.as_handle(), &named(b"d"), true).unwrap();
    }

    /// A file's bytes written at offsets read back, past its end none, cut short, synced
    /// and its times set; a directory lists its entries with their kinds, an escaped name
    /// its own again; and its volume says its sizes.
    #[test]
    fn data_times_listing_and_volume_are_read_as_linux_reads_them() {
        let (_dir, root) = tree("ntfs-data");
        let f = open_at(
            root.as_handle(),
            &nt_name(b"f").unwrap(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
        )
        .unwrap();
        assert_eq!(write_at(f.as_handle(), b"hello", 0).unwrap(), 5);
        assert_eq!(write_at(f.as_handle(), b"!", 9).unwrap(), 1);
        let mut buf = [0u8; 16];
        let n = read_at(f.as_handle(), &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello\0\0\0\0!");
        truncate(f.as_handle(), 2).unwrap();
        let n = read_at(f.as_handle(), &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"he");
        assert_eq!(read_at(f.as_handle(), &mut buf, 2).unwrap(), 0);
        sync(f.as_handle()).unwrap();
        set_times(
            f.as_handle(),
            Some((1_000_000_000, 500)),
            Some((1_700_000_000, 123_456_700)),
        )
        .unwrap();
        let st = stat(f.as_handle()).unwrap();
        assert_eq!(st.mtime, (1_700_000_000, 123_456_700));
        assert_eq!(st.atime.0, 1_000_000_000);
        drop(mkdir_at(root.as_handle(), &nt_name(b"sub").unwrap()).unwrap());
        drop(symlink_at(root.as_handle(), &nt_name(b"to:it").unwrap(), b"f").unwrap());
        let mut seen = list(root.as_handle(), true).unwrap();
        loop {
            let more = list(root.as_handle(), false).unwrap();
            if more.is_empty() {
                break;
            }
            seen.extend(more);
        }
        let mut kinds: Vec<(Vec<u8>, u32)> = seen
            .into_iter()
            .filter(|l| l.name != b"." && l.name != b"..")
            .map(|l| (l.name, l.kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                (b"f".to_vec(), 0o100000),
                (b"sub".to_vec(), 0o040000),
                (b"to:it".to_vec(), 0o120000)
            ]
        );
        let v = volume(root.as_handle()).unwrap();
        assert!(
            v.block_size >= 512 && v.blocks > 0 && v.available <= v.blocks,
            "{v:?}"
        );
    }

    /// A file [`open_unfollowed`] opens is the file at its name; a link there, an LX symlink
    /// or a junction to the directory it is in, is refused [`STOPPED_ON_LINK`] and never
    /// opened through; a junction opened as a file, without backup semantics, refused as a
    /// directory is.
    #[test]
    fn a_link_at_a_name_is_never_opened_through() {
        use std::os::windows::ffi::OsStrExt as _;
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let (dir, root) = tree("ntfs-unfollowed");
        std::fs::write(dir.join("plain"), b"plain").unwrap();
        let mut read = std::fs::File::options();
        read.read(true);
        let mut got = Vec::new();
        let mut plain = open_unfollowed(&read, 0, &dir.join("plain")).unwrap();
        std::io::Read::read_to_end(&mut plain, &mut got).unwrap();
        assert_eq!(got, b"plain");

        drop(symlink_at(root.as_handle(), &nt_name(b"link").unwrap(), b"plain").unwrap());
        let e = open_unfollowed(&read, 0, &dir.join("link")).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(STOPPED_ON_LINK), "{e}");

        // A junction to this test's directory: a MountPointReparseBuffer, its NT and print
        // names' offsets and lengths, then each name NUL-terminated.
        std::fs::create_dir(dir.join("junction")).unwrap();
        let at = std::fs::File::options()
            .access_mode(FILE_GENERIC_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir.join("junction"))
            .unwrap();
        let full = std::fs::canonicalize(&*dir).unwrap();
        // Its verbatim prefix, \\?\, left off.
        let print: Vec<u16> = full.as_os_str().encode_wide().skip(4).collect();
        let nt: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
        let bytes = |n: &[u16]| u16::try_from(n.len() * 2).unwrap();
        let mut data = Vec::new();
        for word in [0, bytes(&nt), bytes(&nt) + 2, bytes(&print)] {
            data.extend(word.to_le_bytes());
        }
        for c in nt.iter().chain([&0]).chain(&print).chain([&0]) {
            data.extend(c.to_le_bytes());
        }
        set_reparse(at.as_handle(), IO_REPARSE_TAG_MOUNT_POINT, &data).unwrap();
        drop(at);
        assert_eq!(
            std::fs::read(dir.join("junction").join("plain")).unwrap(),
            b"plain",
            "a junction Windows follows"
        );
        let e = open_unfollowed(&read, FILE_FLAG_BACKUP_SEMANTICS, &dir.join("junction")).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(STOPPED_ON_LINK), "{e}");
        assert!(open_unfollowed(&read, 0, &dir.join("junction")).is_err());
    }
}
