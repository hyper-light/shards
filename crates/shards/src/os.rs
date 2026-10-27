//! What the host side of shards takes of its OS beside `shards_ipc` and the VMM's
//! `platform`, written once for every host: the null device, a file of its user's alone,
//! and what says a process is out of descriptors (D195).

use std::fs::{File, OpenOptions};
use std::io;

/// The null device's path: what reads nothing and takes all that is written to it.
pub(crate) const NULL: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The null device, open to read and to write.
pub(crate) fn null() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(NULL)
}

/// `options`, to make a file its user's alone: mode 0600 on Unix; on Windows the ACL the
/// home's directories give it, which admit their user alone (`platform::create_private_dir`).
pub(crate) fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600)
    }
    #[cfg(windows)]
    options
}

/// `builder`, to make a directory its user's alone: mode 0700 on Unix; on Windows the ACL
/// the home's directories give it, as [`owner_only`]'s files have.
pub(crate) fn owner_only_dir(builder: &mut std::fs::DirBuilder) -> &mut std::fs::DirBuilder {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700)
    }
    #[cfg(windows)]
    builder
}

/// The OS errors that say a process, or the system, has no descriptor left to open: EMFILE
/// and ENFILE on Unix.
#[cfg(unix)]
pub(crate) const OUT_OF_DESCRIPTORS: [i32; 2] = [libc::EMFILE, libc::ENFILE];

/// The OS errors that say a process, or the system, has no handle left to open: on Windows
/// ERROR_TOO_MANY_OPEN_FILES and, where its handle table or kernel memory is spent,
/// ERROR_NO_SYSTEM_RESOURCES.
#[cfg(windows)]
pub(crate) const OUT_OF_DESCRIPTORS: [i32; 2] = [
    windows_sys::Win32::Foundation::ERROR_TOO_MANY_OPEN_FILES as i32,
    windows_sys::Win32::Foundation::ERROR_NO_SYSTEM_RESOURCES as i32,
];

/// Whether `e` says this process or the system was out of descriptors.
pub(crate) fn out_of_descriptors(e: &io::Error) -> bool {
    e.raw_os_error()
        .is_some_and(|code| OUT_OF_DESCRIPTORS.contains(&code))
}

/// Whether `file`, held open, has lost its name: unlinked on Unix (no link left); on
/// Windows deleted, its name gone at once or its delete pending until its last handle
/// closes (FILE_STANDARD_INFO).
pub(crate) fn unlinked(file: &File) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        file.metadata().is_ok_and(|m| m.nlink() == 0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
        };
        let mut info = FILE_STANDARD_INFO::default();
        let size = u32::try_from(size_of::<FILE_STANDARD_INFO>()).unwrap_or(0);
        // SAFETY: a file the caller holds open, and a structure of `size` bytes to fill.
        let read = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&raw mut info).cast(),
                size,
            )
        };
        read != 0 && (info.NumberOfLinks == 0 || info.DeletePending)
    }
}

/// What tells the directory or file at `path` from another made there later, its link not
/// followed: its device and inode on Unix, its volume's serial and file index on Windows.
pub(crate) fn path_id(path: &std::path::Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        shards_ipc::Identity::of(path).ok().map(|i| (i.dev, i.ino))
    }
}
