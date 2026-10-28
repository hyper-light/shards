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
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_OVERWRITE_IF, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, NTSTATUS, OBJ_CASE_INSENSITIVE,
    RtlNtStatusToDosError, STATUS_OBJECT_NAME_COLLISION, UNICODE_STRING,
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

/// What sets a version's number off from the name it is a version of ([`write_in`]):
/// `name.~N`.
const VERSION: &str = ".~";

/// Creates `name` in the directory `dir` holds open, durably, as a version of its own:
/// written to a temporary sibling, flushed, renamed by the directory's handle to `name.~N`,
/// N one past the highest version listed there, by a rename that replaces nothing, and the
/// directory flushed; then the versions before it, and a file at `name` itself, are removed
/// where Windows lets them go. Windows refused renames over a file it had lately been given
/// (STATUS_ACCESS_DENIED) for a minute and more at a time, Restart Manager naming no holder,
/// and in those holds refused the links and moves that might have stepped around one, while
/// a rename to a name no file had was never refused (PM M345): so no write here replaces a
/// file, and readers take the newest version ([`newest_in`]). Of writers racing to the same
/// name each lands a version whole, the highest the newest; one that finds its version's
/// name taken lists again (STATUS_OBJECT_NAME_COLLISION). Versions count up from those
/// listed, never from a clock, which may step back. A version Windows will not yet let go of
/// stays for the next write, or the directory's owner ([`remove_in`]), to remove. It lands
/// even if the directory was renamed since it was opened.
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
    let landed = file
        .write_all(bytes)
        .and_then(|()| super::sync_durable(&file))
        .and_then(|()| land(&file, dir, name))
        // The rename is the file's own metadata: flushed with it.
        .and_then(|version| super::sync_durable(&file).map(|()| version));
    let version = match landed {
        Ok(version) => version,
        Err(e) => {
            // Ours alone, under its own name, unless the rename went: then the version is
            // whole, and stays.
            if attributes(&file).is_ok() && file_name_is(&file, &tmp) {
                delete_on_close(&file);
            }
            return Err(e);
        }
    };
    drop(file);
    sync_entries(dir)?;
    // What it supersedes, where Windows lets it go: what it keeps, a later write removes.
    if let Ok(names) = names_in(dir) {
        let before = versions(&names, name)
            .filter(|(n, _)| *n < version)
            .map(|(_, entry)| entry);
        for entry in before.chain(names.iter().filter(|entry| entry.as_os_str() == OsStr::new(name))) {
            if let Some(entry) = entry.to_str() {
                let _ = unlink_in(dir, entry);
            }
        }
    }
    Ok(())
}

/// Renames `file` to `name`'s next version in the directory `dir` holds open, `name.~N`, N
/// one past the highest listed there, by a rename that replaces nothing; listed again where
/// another writer landed that version first. The version it landed.
fn land(file: &File, dir: &File, name: &str) -> io::Result<u64> {
    loop {
        let next = versions(&names_in(dir)?, name)
            .map(|(n, _)| n)
            .max()
            .map_or(Some(1), |n| n.checked_add(1))
            .ok_or_else(|| io::Error::other(format!("{name}: no version past the last")))?;
        let status = rename_to(file, dir, &format!("{name}{VERSION}{next}"))?;
        if status >= 0 {
            return Ok(next);
        }
        if status != STATUS_OBJECT_NAME_COLLISION {
            return Err(nt_error(status));
        }
    }
}

/// The versions of `name` among `names`, a directory's listing ([`write_in`]): each
/// `name.~N`'s N, with its entry.
fn versions<'a>(names: &'a [OsString], name: &'a str) -> impl Iterator<Item = (u64, &'a OsString)> + 'a {
    names.iter().filter_map(move |entry| {
        let n = entry.to_str()?.strip_prefix(name)?.strip_prefix(VERSION)?;
        version_number(n).map(|n| (n, entry))
    })
}

/// `digits` as a version's number: digits alone, none a leading zero, so that each version
/// has one name.
fn version_number(digits: &str) -> Option<u64> {
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !(digits.len() > 1 && digits.starts_with('0'));
    canonical.then(|| digits.parse().ok()).flatten()
}

/// What `entry`, a name a directory lists, is a version of ([`write_in`]): `name` for
/// `name.~N`, else `entry` itself.
pub fn version_of(entry: &str) -> &str {
    match entry.rsplit_once(VERSION) {
        Some((name, n)) if !name.is_empty() && version_number(n).is_some() => name,
        _ => entry,
    }
}

/// The name `name`'s newest version goes by in the directory `dir` holds open
/// ([`write_in`]): the highest `name.~N` listed there, else `name` itself.
pub fn newest_in(dir: &File, name: &str) -> io::Result<String> {
    plain_name(name)?;
    let names = names_in(dir)?;
    Ok(versions(&names, name)
        .max_by_key(|(n, _)| *n)
        .and_then(|(_, entry)| entry.to_str())
        .map_or_else(|| name.to_string(), str::to_string))
}

/// Opens `name`'s newest version for reading in the directory `dir` holds open
/// ([`newest_in`]).
pub fn open_newest_in(dir: &File, name: &str) -> io::Result<File> {
    newest_opened(dir, name, |entry| open_in(dir, entry))
}

/// The regular file `rel` names beneath the directory at `root`, its newest version
/// ([`newest_in`]), opened for reading as [`open_beneath`] opens one: no component below
/// `root` a reparse point, nor the version, which its handle says.
pub fn open_newest_beneath(root: &Path, rel: &[&str]) -> io::Result<File> {
    let (name, dirs) = rel
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file named"))?;
    let dir = open_dir_beneath(root, dirs)?;
    newest_opened(&dir, name, |entry| {
        let file = open_relative(
            &dir,
            entry,
            FILE_GENERIC_READ,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        )?;
        regular(attributes(&file)?)?;
        Ok(file)
    })
}

/// `name`'s newest version in `dir`, as `open` opens an entry: listed again where it went
/// between its listing and its open, which a writer does only once it has landed a newer.
fn newest_opened(dir: &File, name: &str, open: impl Fn(&str) -> io::Result<File>) -> io::Result<File> {
    loop {
        let entry = newest_in(dir, name)?;
        match open(&entry) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && entry != name => {}
            opened => return opened,
        }
    }
}

/// Removes `name` from the directory `dir` holds open, every version of it and a file at
/// `name` itself ([`write_in`]), each where Windows lets it go: the first it refused, once
/// it has asked for them all.
pub fn remove_in(dir: &File, name: &str) -> io::Result<()> {
    plain_name(name)?;
    let names = names_in(dir)?;
    let mut refused = None;
    let all = versions(&names, name)
        .map(|(_, entry)| entry)
        .chain(names.iter().filter(|entry| entry.as_os_str() == OsStr::new(name)));
    for entry in all {
        let Some(entry) = entry.to_str() else { continue };
        match unlink_in(dir, entry) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                refused.get_or_insert(e);
            }
            _ => {}
        }
    }
    refused.map_or(Ok(()), Err)
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

/// Renames `file` to `name` in the directory `dir` holds open, replacing nothing
/// (NtSetInformationFile, FileRenameInformation, ReplaceIfExists false, which every file
/// system takes): the call's status, STATUS_OBJECT_NAME_COLLISION where `name` is taken.
fn rename_to(file: &File, dir: &File, name: &str) -> io::Result<NTSTATUS> {
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
    let name_length = u32::try_from(name_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a name too long"))?;
    // Eight-byte words: aligned as the structure's handle is.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `buf` holds `size` bytes, aligned for FILE_RENAME_INFORMATION: its head, and
    // the name's `wide.len()` units at its FileName, which `size` makes room for; then a
    // handle `file` holds open with DELETE, and the structure of `size32` bytes `buf` holds.
    // Its first word zero: ReplaceIfExists false.
    Ok(unsafe {
        (*info).RootDirectory = dir.as_raw_handle();
        (*info).FileNameLength = name_length;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            wide.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            info.cast(),
            size32,
            FileRenameInformation,
        )
    })
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
    make_sparse(file)?;
    let zero = FILE_ZERO_DATA_INFORMATION {
        FileOffset: at(offset)?,
        BeyondFinalZero: at(beyond)?,
    };
    control(file, FSCTL_SET_ZERO_DATA, &zero)
}

/// Makes `file` sparse (FSCTL_SET_SPARSE), so that what it holds past what is written takes
/// none of its disk, as a Unix file's holes take no blocks: NTFS takes every cluster up to a
/// file's end as it is extended unless the file is sparse (PM M349: a file set to 10 GiB
/// took 10 GiB of free space; made sparse first, none).
pub fn make_sparse(file: &File) -> io::Result<()> {
    let sparse = FILE_SET_SPARSE_BUFFER { SetSparse: true };
    control(file, FSCTL_SET_SPARSE, &sparse)
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
    /// rename or one through the directory's handle (a POSIX one too, PM M347); once nothing
    /// in it is open, it does. Both refusals were "Access is denied" on jupiter.
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
        let by_handle = rename_to(&handle, &parent, "c.handle").unwrap();
        assert_eq!(plain.map_err(|e| e.kind()), Err(io::ErrorKind::PermissionDenied));
        assert!(by_handle < 0, "a rename through its handle went: {by_handle:#x}");
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

    /// A write lands whole as its name's next version, in its directory wherever that moved,
    /// leaving no temporary file and no version before it; a reader keeps what it opened; the
    /// listing shows each name but `.` and `..`, and the directory's entries flush.
    #[test]
    fn written_whole_in_an_open_directory_and_listed() {
        let base = shards_testdir::TempDir::new("write-in").unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        let dir = super::super::open_dir(&base.join("d")).unwrap();
        write_in(&dir, "state", b"one").unwrap();
        let held = open_newest_in(&dir, "state").unwrap();
        write_in(&dir, "state", b"two").unwrap();
        assert_eq!(read(held), "one", "the reader keeps what it opened");
        std::fs::rename(base.join("d"), base.join("moved")).unwrap();
        write_in(&dir, "other", b"three").unwrap();
        std::fs::create_dir(base.join("moved").join("sub")).unwrap();
        assert_eq!(read(open_newest_in(&dir, "state").unwrap()), "two");
        assert_eq!(read(open_newest_in(&dir, "other").unwrap()), "three");
        let mut names = names_in(&dir).unwrap();
        names.sort();
        assert_eq!(names, ["other.~1", "state.~2", "sub"]);
        sync_entries(&dir).unwrap();
        assert!(write_in(&dir, "a:b", b"x").is_err(), "a stream's name");
    }

    /// A version another holds open sharing no delete, which no rename could replace (PM
    /// M344), as Windows' own holds kept files it refused to replace (PM M345), keeps no write
    /// from landing: the next version is the newest, and the one held goes with a later write
    /// once it is let go.
    #[test]
    fn a_held_version_keeps_no_write_from_landing() {
        let base = shards_testdir::TempDir::new("held").unwrap();
        let d = base.join("d");
        std::fs::create_dir(&d).unwrap();
        let dir = super::super::open_dir(&d).unwrap();
        write_in(&dir, "current", b"one").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(d.join("current.~1"))
            .unwrap();
        write_in(&dir, "current", b"two").unwrap();
        assert_eq!(read(open_newest_in(&dir, "current").unwrap()), "two");
        assert!(d.join("current.~1").exists(), "a version held was let go of");
        drop(held);
        write_in(&dir, "current", b"three").unwrap();
        assert_eq!(names_in(&dir).unwrap(), ["current.~3"]);
        assert_eq!(
            read(open_newest_beneath(&base, &["d", "current"]).unwrap()),
            "three"
        );
    }

    /// Writers racing to one name each land a version of their own, whole, none replacing
    /// another's: what is left is versions alone, the newest one of theirs.
    #[test]
    fn writers_racing_to_one_name_each_land_whole() {
        let base = shards_testdir::TempDir::new("race").unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        let dir = super::super::open_dir(&base.join("d")).unwrap();
        let sets: Vec<String> = (1..=8).map(|w| format!("writer {w} ").repeat(64 * w)).collect();
        std::thread::scope(|scope| {
            for set in &sets {
                let dir = &dir;
                scope.spawn(move || {
                    for _ in 0..50 {
                        write_in(dir, "set", set.as_bytes()).unwrap();
                    }
                });
            }
        });
        let newest = read(open_newest_in(&dir, "set").unwrap());
        assert!(sets.contains(&newest), "{} bytes", newest.len());
        for name in names_in(&dir).unwrap() {
            let name = name.to_str().unwrap();
            assert_eq!(version_of(name), "set", "{name} left");
        }
    }

    /// A write's version is one past the highest listed, whatever wrote it, and the versions
    /// before it go, with a file at the name itself; a name with no version's digits is no
    /// version, and stays; a removal takes every version.
    #[test]
    fn a_version_is_one_past_the_highest_listed() {
        let base = shards_testdir::TempDir::new("versions").unwrap();
        let d = base.join("d");
        std::fs::create_dir(&d).unwrap();
        let dir = super::super::open_dir(&d).unwrap();
        for (name, bytes) in [
            ("set", "plain"),
            ("set.~41", "old"),
            ("set.~041", "a leading zero"),
            ("set.~x", "no digits"),
        ] {
            std::fs::write(d.join(name), bytes).unwrap();
        }
        assert_eq!(newest_in(&dir, "set").unwrap(), "set.~41");
        write_in(&dir, "set", b"new").unwrap();
        let mut names = names_in(&dir).unwrap();
        names.sort();
        assert_eq!(names, ["set.~041", "set.~42", "set.~x"]);
        for (entry, of) in [
            ("set.~42", "set"),
            ("set.~041", "set.~041"),
            ("set.~x", "set.~x"),
            (".~1", ".~1"),
            ("set", "set"),
        ] {
            assert_eq!(version_of(entry), of, "{entry}");
        }
        remove_in(&dir, "set").unwrap();
        let mut names = names_in(&dir).unwrap();
        names.sort();
        assert_eq!(names, ["set.~041", "set.~x"]);
        assert_eq!(
            open_newest_in(&dir, "set").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// A measurement (PM M345): [`write_in`] as the rename probe's writers write
    /// (docs/research/measurements/whp-run/src/bin/m345_rename.rs, its second part), 8 KiB of
    /// their own bytes a write: one writer 2,000 times, eight side by side 250 times each, then
    /// eight beside eight threads writing and removing files of their own; each pass's writes
    /// timed, and every error said.
    #[test]
    #[ignore = "a measurement: docs/research/measurements/write-in-versions/run.sh"]
    fn write_in_as_the_rename_probes_writers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let base = shards_testdir::TempDir::new("write-in-m345").unwrap();
        std::fs::create_dir(base.join("d")).unwrap();
        let dir = super::super::open_dir(&base.join("d")).unwrap();
        let bytes =
            |w: usize, n: usize| -> Vec<u8> { (0..8 << 10).map(|i| (i * 31 + w * 7 + n) as u8).collect() };
        let writes = |w: usize, rounds: usize| -> (Vec<u128>, Vec<String>) {
            let (mut took, mut errors) = (Vec::new(), Vec::new());
            for n in 0..rounds {
                let content = bytes(w, n);
                let t0 = std::time::Instant::now();
                match write_in(&dir, "target", &content) {
                    Ok(()) => took.push(t0.elapsed().as_micros()),
                    Err(e) => errors.push(format!("writer {w} write {n}: {e}")),
                }
            }
            (took, errors)
        };
        let report = |pass: &str, mut took: Vec<u128>, errors: Vec<String>| {
            took.sort_unstable();
            let at = |q: usize| {
                took.get((took.len() * q / 100).min(took.len().saturating_sub(1)))
                    .copied()
            };
            println!(
                "{pass}: n={} errors={} p50={:?} p90={:?} p99={:?} max={:?} (us)",
                took.len() + errors.len(),
                errors.len(),
                at(50),
                at(90),
                at(99),
                took.last()
            );
            for e in errors.iter().take(10) {
                println!("  {e}");
            }
        };
        let (took, errors) = writes(0, 2000);
        report("one writer", took, errors);
        for churn in [false, true] {
            let stop = AtomicBool::new(false);
            let (took, errors) = std::thread::scope(|scope| {
                let churners: Vec<_> = (0..if churn { 8 } else { 0 })
                    .map(|c| {
                        let (stop, base) = (&stop, &base);
                        scope.spawn(move || {
                            let mine = base.join(format!("churn-{c}"));
                            while !stop.load(Ordering::Relaxed) {
                                std::fs::write(&mine, bytes(c, 0)).unwrap();
                                std::fs::remove_file(&mine).unwrap();
                            }
                        })
                    })
                    .collect();
                let each: Vec<_> = (1..=8).map(|w| scope.spawn(move || writes(w, 250))).collect();
                let all = each.into_iter().map(|h| h.join().unwrap()).fold(
                    (Vec::new(), Vec::new()),
                    |(mut t, mut e), (took, errors)| {
                        t.extend(took);
                        e.extend(errors);
                        (t, e)
                    },
                );
                stop.store(true, Ordering::Relaxed);
                for c in churners {
                    c.join().unwrap();
                }
                all
            });
            report(
                if churn {
                    "eight writers beside churn"
                } else {
                    "eight writers"
                },
                took,
                errors,
            );
        }
        let newest = newest_in(&dir, "target").unwrap();
        let left: Vec<_> = names_in(&dir)
            .unwrap()
            .into_iter()
            .filter(|n| n != newest.as_str())
            .collect();
        println!("newest {newest}; left beside it: {left:?}");
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

    /// The clusters the file at `path` takes of its volume (GetCompressedFileSizeW).
    fn allocated(path: &std::path::Path) -> u64 {
        use std::os::windows::ffi::OsStrExt as _;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut high = 0u32;
        // SAFETY: a NUL-terminated wide path and a local for the high word.
        let low = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW(wide.as_ptr(), &mut high)
        };
        (u64::from(high) << 32) | u64::from(low)
    }

    /// A copy takes no more of the volume than what it copies, into a file not made sparse
    /// before it too (`copy_ranges`, PM M349): 1 MiB of 64 MiB, the rest zeros, holds a few
    /// MiB at most where it held all 64; and reads back as it was.
    #[test]
    fn a_copy_takes_its_data_alone_of_the_disk() {
        const MIB: u64 = 1 << 20;
        let base = shards_testdir::TempDir::new("copy-sparse").unwrap();
        let (from_path, to_path) = (base.join("from"), base.join("to"));
        let from = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&from_path)
            .unwrap();
        from.set_len(64 * MIB).unwrap();
        std::os::windows::fs::FileExt::seek_write(&from, &vec![0x5a; MIB as usize], 32 * MIB).unwrap();
        let to = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&to_path)
            .unwrap();
        super::super::copy_used(&from, &to, 64 * MIB).unwrap();
        drop((from, to));
        assert!(
            allocated(&to_path) <= 4 * MIB,
            "{} bytes taken",
            allocated(&to_path)
        );
        assert_eq!(
            std::fs::read(&to_path).unwrap(),
            std::fs::read(&from_path).unwrap()
        );
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
