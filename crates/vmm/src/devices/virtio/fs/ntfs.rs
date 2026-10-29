//! A Linux guest's files on NTFS, kept as WSL keeps them (D197, PM M362): the NT calls the
//! share server's Windows layer makes, each relative to a handle of the shared tree and
//! following no reparse point, and what they mean in Linux's terms.

use std::os::windows::io::{
    AsHandle as _, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CASE_SENSITIVE_INFORMATION, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
    FILE_STAT_LX_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileCaseSensitiveInformation,
    FileLinkInformation, FileRenameInformationEx, FileStatLxInformation, LX_FILE_METADATA_HAS_DEVICE_ID,
    LX_FILE_METADATA_HAS_MODE, LX_FILE_METADATA_HAS_UID, NtCreateFile, NtQueryEaFile, NtQueryInformationFile,
    NtSetEaFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DISK_FULL, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_PARAMETER, ERROR_NOT_A_REPARSE_POINT, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND,
    ERROR_SHARING_VIOLATION, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_CANNOT_DELETE, STATUS_DELETE_PENDING,
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_DISK_FULL, STATUS_EAS_NOT_SUPPORTED, STATUS_FILE_DELETED,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_PARAMETER,
    STATUS_MEDIA_WRITE_PROTECTED, STATUS_NAME_TOO_LONG, STATUS_NO_EAS_ON_FILE, STATUS_NO_MORE_EAS,
    STATUS_NONEXISTENT_EA_ENTRY, STATUS_NOT_A_DIRECTORY, STATUS_NOT_A_REPARSE_POINT, STATUS_NOT_SAME_DEVICE,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SHARING_VIOLATION, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_READ_EA, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_EA, FileDispositionInfoEx, FileIdInfo,
    GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;

use super::host::Stat;

/// A Linux errno.
pub type Errno = i32;

const ENOENT: Errno = 2;
const E2BIG: Errno = 7;
const EIO: Errno = 5;
const ENOMEM: Errno = 12;
const EACCES: Errno = 13;
const EBUSY: Errno = 16;
const EEXIST: Errno = 17;
const EXDEV: Errno = 18;
const ENOTDIR: Errno = 20;
const EISDIR: Errno = 21;
const EINVAL: Errno = 22;
const ENOSPC: Errno = 28;
const EROFS: Errno = 30;
const ENAMETOOLONG: Errno = 36;
const ENOTEMPTY: Errno = 39;
const ENODATA: Errno = 61;
const EILSEQ: Errno = 84;
const EOPNOTSUPP: Errno = 95;

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
        LX_SYMLINK => 0o120000,
        LX_FIFO => 0o010000,
        LX_CHR => 0o020000,
        LX_BLK => 0o060000,
        AF_UNIX => 0o140000,
        _ if lx.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 => 0o040000,
        _ => 0o100000,
    };
    let mode = if lx.LxFlags & LX_FILE_METADATA_HAS_MODE != 0 {
        kind | (lx.LxMode & 0o7777)
    } else {
        kind | match kind {
            0o040000 => 0o755,
            0o120000 => 0o777,
            _ if lx.FileAttributes & FILE_ATTRIBUTE_READONLY != 0 => 0o444,
            _ => 0o644,
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
    let mut buf = Vec::with_capacity(8 + data.len());
    buf.extend(tag.to_le_bytes());
    buf.extend(u16::try_from(data.len()).map_err(|_| ENAMETOOLONG)?.to_le_bytes());
    buf.extend(0u16.to_le_bytes());
    buf.extend_from_slice(data);
    let mut returned = 0u32;
    // SAFETY: a buffer of this frame's, its length, on a handle the caller holds.
    let done = unsafe {
        DeviceIoControl(
            h.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr().cast(),
            u32::try_from(buf.len()).map_err(|_| ENAMETOOLONG)?,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(last_error());
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

/// What the LX symlink `h` is to, its target's bytes; EINVAL for anything else, as
/// readlink(2) answers of what is no symlink.
pub fn readlink(h: BorrowedHandle<'_>) -> Result<Vec<u8>, Errno> {
    match reparse(h)? {
        Some((LX_SYMLINK, data)) if data.get(..4) == Some(&LX_SYMLINK_VERSION.to_le_bytes()[..]) => {
            Ok(data.get(4..).unwrap_or_default().to_vec())
        }
        _ => Err(EINVAL),
    }
}

/// A FIFO, character or block device or Unix socket `name` in `dir`, by its mode's kind
/// (`S_IFIFO`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK`), as WSL makes one: a new empty file made a
/// reparse point of its LX tag, a device's major and minor in `$LXDEV` (PM M362).
pub fn special_at(
    dir: BorrowedHandle<'_>,
    name: &[u16],
    kind: u32,
    dev: (u32, u32),
) -> Result<OwnedHandle, Errno> {
    let tag = match kind {
        0o010000 => LX_FIFO,
        0o020000 => LX_CHR,
        0o060000 => LX_BLK,
        0o140000 => AF_UNIX,
        _ => return Err(EINVAL),
    };
    let made = open_at(
        dir,
        name,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    set_reparse(made.as_handle(), tag, &[])?;
    if matches!(tag, LX_CHR | LX_BLK) {
        set_lx(made.as_handle(), None, None, None, Some(dev))?;
    }
    Ok(made)
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
    let info = FILE_CASE_SENSITIVE_INFORMATION {
        Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR,
    };
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: an info of its own size on a handle this holds.
    let status = unsafe {
        NtSetInformationFile(
            made.as_raw_handle(),
            &mut io,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_CASE_SENSITIVE_INFORMATION>()).unwrap_or(0),
            FileCaseSensitiveInformation,
        )
    };
    if status < 0 {
        return Err(errno(status));
    }
    Ok(made)
}

/// Linux's errno for a Win32 error a call answered with, by its NTSTATUS's.
fn win32_errno(e: &std::io::Error) -> Errno {
    match e.raw_os_error().and_then(|c| u32::try_from(c).ok()) {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => ENOENT,
        Some(ERROR_ACCESS_DENIED) => EACCES,
        Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => EEXIST,
        Some(ERROR_SHARING_VIOLATION) => EBUSY,
        Some(ERROR_INVALID_PARAMETER) => EINVAL,
        Some(ERROR_NOT_SUPPORTED) => EOPNOTSUPP,
        Some(ERROR_DISK_FULL) => ENOSPC,
        _ => EIO,
    }
}

/// [`win32_errno`] of the calling thread's last error.
fn last_error() -> Errno {
    win32_errno(&std::io::Error::last_os_error())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
    };

    use super::*;

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
        assert_eq!((st.mode, st.uid, st.nlink, st.size), (0o100644, 0, 1, 0));
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
}
