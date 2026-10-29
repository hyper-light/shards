//! A Linux guest's files on NTFS, kept as WSL keeps them (D197, PM M362): the NT calls the
//! share server's Windows layer makes, each relative to a handle of the shared tree and
//! following no reparse point, and what they mean in Linux's terms.

use std::os::windows::io::{AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN_REPARSE_POINT, FILE_STAT_LX_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileStatLxInformation,
    LX_FILE_METADATA_HAS_DEVICE_ID, LX_FILE_METADATA_HAS_MODE, LX_FILE_METADATA_HAS_UID, NtCreateFile,
    NtQueryInformationFile,
};
use windows_sys::Win32::Foundation::{
    NTSTATUS, STATUS_ACCESS_DENIED, STATUS_CANNOT_DELETE, STATUS_DELETE_PENDING, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_DISK_FULL, STATUS_EAS_NOT_SUPPORTED, STATUS_FILE_DELETED, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_PARAMETER, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_NAME_TOO_LONG, STATUS_NO_EAS_ON_FILE, STATUS_NONEXISTENT_EA_ENTRY, STATUS_NOT_A_DIRECTORY,
    STATUS_NOT_A_REPARSE_POINT, STATUS_NOT_SAME_DEVICE, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    STATUS_SHARING_VIOLATION, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_READ_EA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::host::Stat;

/// A Linux errno.
pub type Errno = i32;

const ENOENT: Errno = 2;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::os::windows::io::AsHandle as _;

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
}
