//! Files beneath a root, and in a directory held open, on Windows: unix.rs's
//! `open_beneath`, `open_dir_beneath`, `open_in`, `write_in`, `names_in`, `sync_entries`
//! and `punch_hole`, each by Windows' own means.
//!
//! - **Beneath a root**, as unix.rs reads a snapshot's files (PM M165): each directory below
//!   `root` checked to be one and no reparse point, which is what a symbolic link, a
//!   junction and a mount point all are on NTFS (FILE_ATTRIBUTE_REPARSE_POINT); the file
//!   opened without following one (FILE_FLAG_OPEN_REPARSE_POINT), then checked again by
//!   its handle once open.
//! - **In a directory held open**, the file there even if the directory was renamed or
//!   replaced since: NtCreateFile relative to the directory's handle (OBJECT_ATTRIBUTES'
//!   RootDirectory), Windows' openat; a rename into it by its handle too (FILE_RENAME_INFO's
//!   RootDirectory); its names read through a handle of its own (FileFullDirectoryInfo), as
//!   unix.rs reads them through a descriptor of their own.
//! - **Names** are plain ones, which Windows reads as nothing else. Beside unix.rs's `/`,
//!   `.`, `..` and NUL: `\`, `:` (an alternate data stream), the characters Win32 refuses
//!   in a name, a trailing dot or space, which Win32 strips, and the reserved device names,
//!   which Win32 opens as devices, an extension or none (Microsoft, "Naming Files, Paths,
//!   and Namespaces").

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_INFORMATION_CLASS, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_OVERWRITE_IF, FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
    FILE_RENAME_REPLACE_IF_EXISTS, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, NTSTATUS, OBJ_CASE_INSENSITIVE,
    RtlNtStatusToDosError, STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
    FileAttributeTagInfo, FileDispositionInfo, FileDispositionInfoEx, FileFullDirectoryInfo,
    FlushFileBuffers, GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::{
    FILE_SET_SPARSE_BUFFER, FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
};

/// Every sharing a handle allows, as std's opens and unix.rs's: another may read, write,
/// rename or delete what this holds open.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// `name`, if it names an entry of a directory and no more, and nothing else to Windows (the
/// module's **Names**).
pub(super) fn plain_name(name: &str) -> io::Result<()> {
    let refused = || io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?}: not a plain name"));
    if name.is_empty() || name == "." || name == ".." || name.ends_with('.') || name.ends_with(' ') {
        return Err(refused());
    }
    if name
        .chars()
        .any(|c| c < ' ' || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(refused());
    }
    if reserved(name) {
        return Err(refused());
    }
    Ok(())
}

/// Whether Win32 opens `name` as a device: CON, PRN, AUX, NUL, and COM and LPT with a
/// digit, or a superscript one, two or three, in any case, alone or with an extension
/// (Microsoft, "Naming Files, Paths, and Namespaces").
fn reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    ["COM", "LPT"].iter().any(|device| {
        stem.strip_prefix(device).is_some_and(|rest| {
            let mut chars = rest.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        })
    })
}

/// `root` joined with `rel`'s directories, each checked to be one and no reparse point, and
/// its names plain (unix.rs `dirs_beneath`, which says why a check of each name holds for
/// the open that follows). `root`, its reader's own, is followed as given.
fn dirs_beneath(root: &Path, rel: &[&str]) -> io::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for name in rel {
        plain_name(name)?;
        path.push(name);
        let found = std::fs::symlink_metadata(&path)?;
        if !found.is_dir() || found.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a directory"));
        }
    }
    Ok(path)
}

/// What `file`'s handle says it is: its attributes (FileAttributeTagInfo).
fn attributes(file: &File) -> io::Result<u32> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    let size = u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).map_err(io::Error::other)?;
    // SAFETY: a handle `file` holds open, and a FILE_ATTRIBUTE_TAG_INFO of `size` bytes.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info.FileAttributes)
}

/// The directory `rel` names beneath the directory at `root`, opened: no component below
/// `root` a reparse point, the last one's handle checked too.
pub fn open_dir_beneath(root: &Path, rel: &[&str]) -> io::Result<File> {
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dirs_beneath(root, rel)?)?;
    let found = attributes(&dir)?;
    if found & FILE_ATTRIBUTE_DIRECTORY == 0 || found & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a directory"));
    }
    Ok(dir)
}

/// The regular file `rel` names beneath the directory at `root`, opened for reading: no
/// component below `root` a reparse point, and the file neither a directory nor a reparse
/// point, checked before it is opened and again by its handle (unix.rs `open_beneath`).
pub fn open_beneath(root: &Path, rel: &[&str]) -> io::Result<File> {
    let (name, dirs) = rel
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file named"))?;
    plain_name(name)?;
    let path = dirs_beneath(root, dirs)?.join(name);
    regular(std::fs::symlink_metadata(&path)?.file_attributes())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    regular(attributes(&file)?)?;
    Ok(file)
}

/// Whether `attributes` are a regular file's: no directory, no reparse point.
fn regular(attributes: u32) -> io::Result<()> {
    if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"));
    }
    Ok(())
}

/// `name` in the directory `dir` holds open, opened relative to its handle by NtCreateFile
/// (OBJECT_ATTRIBUTES' RootDirectory): with `access`, `disposition` (FILE_OPEN, FILE_CREATE)
/// and `options`, beside synchronous I/O, which std's reads and writes take, and as Win32
/// opens names, without regard to case.
fn open_relative(dir: &File, name: &str, access: u32, disposition: u32, options: u32) -> io::Result<File> {
    plain_name(name)?;
    nt_open(
        dir,
        name,
        access | SYNCHRONIZE,
        disposition,
        options | FILE_SYNCHRONOUS_IO_NONALERT,
    )
}

/// A handle of its own to the directory `dir` holds open, with `access` and `options`: an
/// open of no name relative to its handle, which is that directory itself, wherever it has
/// gone, as unix.rs opens `.` relative to it. ReOpenFile refuses a directory's handle:
/// access denied, every way it was asked, on jupiter (Windows 10.0.26200).
pub(super) fn reopen_dir(dir: &File, access: u32, options: u32) -> io::Result<File> {
    nt_open(dir, "", access, FILE_OPEN, options | FILE_DIRECTORY_FILE)
}

/// A handle of its own to the file `file` holds open, with `access` and `options`, as
/// [`reopen_dir`] opens a directory: wherever the file has gone.
pub(super) fn reopen_file(file: &File, access: u32, options: u32) -> io::Result<File> {
    nt_open(file, "", access, FILE_OPEN, options | FILE_NON_DIRECTORY_FILE)
}

/// NtCreateFile of `name` relative to the handle `dir` holds: none for that directory itself.
fn nt_open(dir: &File, name: &str, access: u32, disposition: u32, options: u32) -> io::Result<File> {
    let mut wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let bytes = wide
        .len()
        .checked_mul(2)
        .and_then(|b| u16::try_from(b).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a name too long"))?;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(io::Error::other)?,
        RootDirectory: dir.as_raw_handle(),
        ObjectName: &object,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle = std::ptr::null_mut();
    // SAFETY: NtCreateFile writes the handle it opens into `handle` and its status into
    // `status_block`; `attributes` names `object`, whose buffer `wide` holds (none read where
    // its length is 0), all alive for the call, and a directory handle `dir` holds open. No
    // allocation size, no extended attributes.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            SHARE_ALL,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: the handle NtCreateFile just opened, owned by nothing else.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// The Win32 error an NTSTATUS stands for, as an io::Error.
fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: RtlNtStatusToDosError maps a status to the Win32 error it stands for.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// Opens `name` for reading in the directory `dir` holds open: the file there, even if the
/// directory was renamed or replaced since (unix.rs `open_in`).
pub fn open_in(dir: &File, name: &str) -> io::Result<File> {
    open_relative(dir, name, FILE_GENERIC_READ, FILE_OPEN, FILE_NON_DIRECTORY_FILE)
}

/// Makes `name` in the directory `dir` holds open, to write anew: one there is emptied
/// (unix.rs `create_in`). This user's alone as its directory's files are: NTFS gives it the
/// DACL its directory passes on, where unix.rs makes it 0600.
pub fn create_in(dir: &File, name: &str) -> io::Result<File> {
    open_relative(
        dir,
        name,
        FILE_GENERIC_WRITE,
        FILE_OVERWRITE_IF,
        FILE_NON_DIRECTORY_FILE,
    )
}

/// Makes `name` in the directory `dir` holds open, to append to; refused where it is there
/// (unix.rs `append_new_in`). Opened to append alone, as std opens a file to append: every
/// write goes to its end.
pub fn append_new_in(dir: &File, name: &str) -> io::Result<File> {
    open_relative(
        dir,
        name,
        (FILE_GENERIC_WRITE & !FILE_WRITE_DATA) | FILE_APPEND_DATA,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )
}

/// Removes `name` from the directory `dir` holds open (unix.rs `unlink_in`): its name at
/// once, whoever holds it open, as unlink(2) removes it (FileDispositionInfoEx, POSIX
/// semantics); on a file system without them (FAT), as its last handle closes. A link
/// itself is removed, not what it names.
pub fn unlink_in(dir: &File, name: &str) -> io::Result<()> {
    let file = open_relative(
        dir,
        name,
        DELETE,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
    )?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).map_err(io::Error::other)?;
    // SAFETY: a handle `file` holds open with DELETE, and a FILE_DISPOSITION_INFO_EX of
    // `size` bytes.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            size,
        )
    } != 0
    {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if !matches!(
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)
    ) {
        return Err(e);
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).map_err(io::Error::other)?;
    // SAFETY: as above, with a FILE_DISPOSITION_INFO of `size` bytes.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Creates `name` in the directory `dir` holds open, durably: written to a temporary
/// sibling of its own, flushed, renamed over `name` by the directory's handle, and the
/// directory flushed (unix.rs `write_in`). It lands there even if the directory was renamed
/// since it was opened, and of writers racing to the same name, one replaces the file
/// whole: a POSIX rename (FILE_RENAME_POSIX_SEMANTICS) replaces a file another holds
/// open, as rename(2) does, where without it NTFS refuses (STATUS_ACCESS_DENIED, PM M344).
/// A holder that shares no delete keeps the file from being replaced either way
/// (STATUS_SHARING_VIOLATION): Windows' own limit, which std's opens, and so shards' own
/// readers, never set.
pub fn write_in(dir: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    plain_name(name)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = format!(
        ".{name}.{nanos:x}-{:x}-{:x}.tmp",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    );
    let mut file = open_relative(
        dir,
        &tmp,
        FILE_GENERIC_WRITE | DELETE,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
    )?;
    let written = file
        .write_all(bytes)
        .and_then(|()| super::sync_durable(&file))
        .and_then(|()| rename_into(&file, dir, name))
        // The rename is the file's own metadata: flushed with it.
        .and_then(|()| super::sync_durable(&file));
    if let Err(e) = written {
        // Ours alone, under its own name, unless the rename went: then the file at `name`
        // is whole, and stays.
        if attributes(&file).is_ok() && file_name_is(&file, &tmp) {
            delete_on_close(&file);
        }
        return Err(e);
    }
    drop(file);
    sync_entries(dir)
}

/// Whether `file`'s own name, as its handle says it, ends with `name`.
fn file_name_is(file: &File, name: &str) -> bool {
    super::final_path(file).is_ok_and(|path| path.file_name() == Some(OsStr::new(name)))
}

/// Has `file`, opened with DELETE, removed as its last handle closes.
fn delete_on_close(file: &File) {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(0);
    // SAFETY: a handle `file` holds open, and a FILE_DISPOSITION_INFO of `size` bytes.
    unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size,
        )
    };
}

/// Renames `file` to `name` in the directory `dir` holds open, replacing what is there,
/// whoever holds it open (NtSetInformationFile, FileRenameInformationEx, POSIX semantics);
/// on a file system without POSIX renames (FAT), by the plain rename, which replaces a file
/// no one holds open.
fn rename_into(file: &File, dir: &File, name: &str) -> io::Result<()> {
    let wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let name_bytes = wide
        .len()
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a name too long"))?;
    // The structure, whose one unit of name is room for the NUL after it, and the name, as
    // Microsoft's samples size it: past the structure's head and the name alone, the rename
    // was refused (ERROR_INVALID_PARAMETER on jupiter, Windows 10.0.26200).
    let size = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a name too long"))?;
    let size32 =
        u32::try_from(size).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name too long"))?;
    // Eight-byte words: aligned as the structure's handle is.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let set = |flags: u32| {
        // SAFETY: `buf` holds `size` bytes, aligned for FILE_RENAME_INFORMATION: its head, and
        // the name's `wide.len()` units at its FileName, which `size` makes room for.
        unsafe {
            (*info).Anonymous.Flags = flags;
            (*info).RootDirectory = dir.as_raw_handle();
            (*info).FileNameLength = u32::try_from(name_bytes).unwrap_or(u32::MAX);
            std::ptr::copy_nonoverlapping(
                wide.as_ptr(),
                (&raw mut (*info).FileName).cast::<u16>(),
                wide.len(),
            );
        }
    };
    let rename = |class: FILE_INFORMATION_CLASS| {
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: a handle `file` holds open with DELETE, and the FILE_RENAME_INFORMATION
        // of `size32` bytes `buf` holds.
        unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut status_block,
                info.cast(),
                size32,
                class,
            )
        }
    };
    set(FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS);
    let status = rename(FileRenameInformationEx);
    if status >= 0 {
        return Ok(());
    }
    if ![
        STATUS_INVALID_PARAMETER,
        STATUS_INVALID_INFO_CLASS,
        STATUS_NOT_SUPPORTED,
    ]
    .contains(&status)
    {
        return Err(nt_error(status));
    }
    // FileRenameInformation's ReplaceIfExists is the first byte of the same word.
    set(1);
    let status = rename(FileRenameInformation);
    if status < 0 {
        return Err(nt_error(status));
    }
    Ok(())
}

/// Makes the entries of the directory `dir` holds open durable: the renames into it survive
/// a crash once this returns (unix.rs `sync_entries`). FlushFileBuffers wants a handle that
/// may write ("must have the GENERIC_WRITE access right", its documentation), which `dir`,
/// opened to read, is not: one of its own.
pub fn sync_entries(dir: &File) -> io::Result<()> {
    let writable = reopen_dir(
        dir,
        FILE_GENERIC_WRITE | SYNCHRONIZE,
        FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    // SAFETY: flushing a handle `writable` holds open.
    if unsafe { FlushFileBuffers(writable.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The bytes a directory's listing is read in at once.
const LISTING: usize = 64 << 10;

/// The names in the directory `dir` holds open, but for `.` and `..`, wherever it has gone:
/// read through a handle of its own, whose listing starts at the directory's start, as
/// unix.rs reads through a descriptor of its own (GetFileInformationByHandleEx,
/// FileFullDirectoryInfo, until ERROR_NO_MORE_FILES).
pub fn names_in(dir: &File) -> io::Result<Vec<OsString>> {
    let listing = reopen_dir(
        dir,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    // Eight-byte words: each entry is aligned on eight bytes (FILE_FULL_DIR_INFO's
    // documentation).
    let mut buf = vec![0u64; LISTING / 8];
    let len = u32::try_from(LISTING).map_err(io::Error::other)?;
    let head = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
    let mut names = Vec::new();
    loop {
        // SAFETY: a directory handle `listing` holds open, and `buf`'s `len` bytes.
        if unsafe {
            GetFileInformationByHandleEx(
                listing.as_raw_handle(),
                FileFullDirectoryInfo,
                buf.as_mut_ptr().cast(),
                len,
            )
        } == 0
        {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
                return Ok(names);
            }
            return Err(e);
        }
        let bytes = buf.as_ptr().cast::<u8>();
        let mut at = 0usize;
        loop {
            // Each entry's head, then its name, within what was read.
            let name_at = at.checked_add(head).filter(|&end| end <= LISTING);
            let Some(name_at) = name_at else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a listing's entry past its end",
                ));
            };
            // SAFETY: `at..name_at` lies within `buf`'s bytes, just checked; read unaligned,
            // whatever the file system wrote.
            let entry = unsafe { std::ptr::read_unaligned(bytes.add(at).cast::<FILE_FULL_DIR_INFO>()) };
            let units = usize::try_from(entry.FileNameLength).unwrap_or(usize::MAX) / 2;
            let end = units
                .checked_mul(2)
                .and_then(|n| name_at.checked_add(n))
                .filter(|&end| end <= LISTING)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a listing's name past its end"))?;
            let mut name = vec![0u16; units];
            // SAFETY: `name_at..end` lies within `buf`'s bytes, just checked, and `name` has
            // room for its `units` units.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.add(name_at),
                    name.as_mut_ptr().cast::<u8>(),
                    end - name_at,
                );
            }
            let name = OsString::from_wide(&name);
            if name != "." && name != ".." {
                names.push(name);
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            at = at
                .checked_add(usize::try_from(entry.NextEntryOffset).unwrap_or(usize::MAX))
                .filter(|&next| next < LISTING)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "a listing's entry past its end")
                })?;
        }
    }
}

/// Gives back the file system's blocks under `len` bytes of `file` from `offset`, which
/// read as zeros after (unix.rs `punch_hole`). NTFS holds every cluster of a file up to its
/// end unless the file is sparse, and deallocates a zeroed range of a sparse one ("If the
/// file is sparse or compressed, the NTFS file system may deallocate disk space in the
/// file", FSCTL_SET_ZERO_DATA's documentation): so the file is made sparse, then the range
/// zeroed.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let beyond = offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a hole past the largest offset"))?;
    let at = |n: u64| {
        i64::try_from(n).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an offset past i64"))
    };
    let sparse = FILE_SET_SPARSE_BUFFER { SetSparse: true };
    control(file, FSCTL_SET_SPARSE, &sparse)?;
    let zero = FILE_ZERO_DATA_INFORMATION {
        FileOffset: at(offset)?,
        BeyondFinalZero: at(beyond)?,
    };
    control(file, FSCTL_SET_ZERO_DATA, &zero)
}

/// DeviceIoControl of `code` on `file`, with `input` and no output.
fn control<T>(file: &File, code: u32, input: &T) -> io::Result<()> {
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    let mut returned = 0u32;
    // SAFETY: a handle `file` holds open for synchronous I/O, `input`'s `size` bytes, no
    // output buffer, and a count to write.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            std::ptr::from_ref(input).cast(),
            size,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// A junction at `link` to the directory `to`, as `mklink /J` makes one: no privilege
    /// needed, unlike a symbolic link's.
    fn junction(link: &Path, to: &Path) {
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(to)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    }

    fn read(mut file: File) -> String {
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    /// What shards' containers' removal relies on (D195): NTFS renames no directory while a
    /// file in it is open, even one std opened (every sharing allowed), by MoveFileEx's
    /// rename or a POSIX one through the directory's handle; once nothing in it is open, it
    /// does. Both refusals were "Access is denied" on jupiter.
    #[test]
    fn a_directory_holding_an_open_file_is_renamed_by_no_rename() {
        let base = shards_testdir::TempDir::new("dir-rename").unwrap();
        let dir = base.join("c");
        std::fs::create_dir(&dir).unwrap();
        let held = std::fs::File::create(dir.join("log")).unwrap();
        let plain = std::fs::rename(&dir, base.join("c.plain"));
        let parent = super::super::open_dir(&base).unwrap();
        let handle = std::fs::OpenOptions::new()
            .access_mode(DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&dir)
            .unwrap();
        let posix = rename_into(&handle, &parent, "c.posix");
        assert_eq!(plain.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        assert_eq!(posix.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        drop((handle, held));
        std::fs::rename(&dir, base.join("c.plain")).unwrap();
    }

    /// Names Windows reads as something else are refused: a stream, a separator, a device
    /// with or without an extension, a trailing dot or space; plain ones pass.
    #[test]
    fn only_plain_names_pass() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "f:stream",
            "CON",
            "con.txt",
            "NUL",
            "nul.tar.gz",
            "COM1",
            "lpt9.log",
            "COM\u{b9}",
            "end.",
            "end ",
            "a*",
            "a?",
            "a|b",
            "a\u{1}",
        ] {
            assert!(plain_name(name).is_err(), "{name:?} passed");
        }
        for name in ["state", ".hidden", "COM10", "console", "a.b.c", "LPT", "nul-not"] {
            plain_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        }
    }

    /// A file beneath a root opens; a directory, a junction anywhere below the root, and a
    /// name that is not plain are refused, each before anything is read.
    #[test]
    fn beneath_a_root_no_reparse_point_is_followed() {
        let root = shards_testdir::TempDir::new("beneath").unwrap();
        let elsewhere = shards_testdir::TempDir::new("elsewhere").unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::write(root.join("a").join("f"), b"inside").unwrap();
        std::fs::write(elsewhere.join("f"), b"outside").unwrap();
        junction(&root.join("link"), &elsewhere);
        junction(&root.join("a").join("j"), &elsewhere);
        assert_eq!(read(open_beneath(root.path(), &["a", "f"]).unwrap()), "inside");
        assert!(
            open_beneath(root.path(), &["link", "f"]).is_err(),
            "through a junction"
        );
        assert!(open_beneath(root.path(), &["a"]).is_err(), "a directory");
        assert!(
            open_beneath(root.path(), &["a", "j"]).is_err(),
            "a junction itself"
        );
        assert!(open_beneath(root.path(), &["a", "..", "a", "f"]).is_err(), "..");
        assert!(open_beneath(root.path(), &[]).is_err(), "no name");
        assert!(open_dir_beneath(root.path(), &["a"]).is_ok());
        assert!(open_dir_beneath(root.path(), &["link"]).is_err(), "a junction");
        assert!(
            open_dir_beneath(root.path(), &["a", "j"]).is_err(),
            "a junction below"
        );
    }

    /// A file in a directory held open is the one there after the directory moved, by its
    /// handle; one not there is NotFound, as callers take it.
    #[test]
    fn in_an_open_directory_by_its_handle() {
        let base = shards_testdir::TempDir::new("open-in").unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        std::fs::write(base.join("d").join("f"), b"held").unwrap();
        let dir = super::super::open_dir(&base.join("d")).unwrap();
        std::fs::rename(base.join("d"), base.join("moved")).unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        std::fs::write(base.join("d").join("f"), b"new").unwrap();
        assert_eq!(read(open_in(&dir, "f").unwrap()), "held");
        assert_eq!(open_in(&dir, "none").unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(open_in(&dir, "..").is_err());
    }

    /// A write replaces a file whole, lands in its directory wherever it moved, leaves no
    /// temporary file, and replaces one another reader holds open; the listing shows each
    /// name but `.` and `..`, and the directory's entries flush.
    #[test]
    fn written_whole_in_an_open_directory_and_listed() {
        let base = shards_testdir::TempDir::new("write-in").unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        let dir = super::super::open_dir(&base.join("d")).unwrap();
        write_in(&dir, "state", b"one").unwrap();
        let held = File::open(base.join("d").join("state")).unwrap();
        write_in(&dir, "state", b"two").unwrap();
        assert_eq!(read(held), "one", "the reader keeps what it opened");
        std::fs::rename(base.join("d"), base.join("moved")).unwrap();
        write_in(&dir, "other", b"three").unwrap();
        std::fs::create_dir(base.join("moved").join("sub")).unwrap();
        assert_eq!(std::fs::read(base.join("moved").join("state")).unwrap(), b"two");
        assert_eq!(std::fs::read(base.join("moved").join("other")).unwrap(), b"three");
        let mut names = names_in(&dir).unwrap();
        names.sort();
        assert_eq!(names, ["other", "state", "sub"]);
        sync_entries(&dir).unwrap();
        assert!(write_in(&dir, "a:b", b"x").is_err(), "a stream's name");
    }

    /// A listing longer than one read's buffer is whole.
    #[test]
    fn a_long_listing_is_whole() {
        let base = shards_testdir::TempDir::new("listing").unwrap();
        let many = 2_000;
        for n in 0..many {
            std::fs::write(base.join(format!("file-with-a-long-name-{n:05}")), b"").unwrap();
        }
        let dir = super::super::open_dir(base.path()).unwrap();
        assert_eq!(names_in(&dir).unwrap().len(), many);
    }

    /// A hole punched reads as zeros and holds fewer of the volume's bytes, the rest as it
    /// was.
    #[test]
    fn a_punched_hole_reads_zeros_and_is_given_back() {
        use std::os::windows::ffi::OsStrExt as _;
        const MIB: usize = 1 << 20;
        let base = shards_testdir::TempDir::new("hole").unwrap();
        let path = base.join("f");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![0xaa; 4 * MIB]).unwrap();
        file.sync_all().unwrap();
        let allocated = || {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut high = 0u32;
            // SAFETY: a NUL-terminated wide path and a local for the high word.
            let low = unsafe {
                windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(wide.as_ptr(), &mut high)
            };
            (u64::from(high) << 32) | u64::from(low)
        };
        let before = allocated();
        punch_hole(&file, MIB as u64, 2 * MIB as u64).unwrap();
        punch_hole(&file, 0, 0).unwrap();
        assert!(allocated() + MIB as u64 <= before, "{} of {before}", allocated());
        let all = std::fs::read(&path).unwrap();
        assert!(all[..MIB].iter().all(|&b| b == 0xaa));
        assert!(all[MIB..3 * MIB].iter().all(|&b| b == 0));
        assert!(all[3 * MIB..].iter().all(|&b| b == 0xaa));
    }
}
