//! Grants: what a VM process in App Sandbox may reach, given by the process that started
//! it (docs/research/macos-confinement.md §3; D30). Before it opens anything, the VM asks
//! its spawner over a socket for the paths its arguments name (`kind::GRANT`). Each file
//! is answered with a descriptor the spawner opened, read-only for what the VM only reads,
//! which the VM opens as its own (vmm `platform::open_input`); each directory it writes in
//! with a read-write bookmark, which the VM resolves, extending its sandbox to it ("Share
//! file access between processes with URL bookmarks", Apple: Accessing files from the
//! macOS App Sandbox). A bookmark passed between processes grants read and write or
//! nothing (PM M70), so no file it only reads is granted one.
//!
//! On Windows the VM process runs in an AppContainer of its own (D141, PM M201), which
//! reaches no file or socket of its user's; it asks the same way, over two pipes, and is
//! answered with handles its spawner duplicated into it, each file with the path the
//! spawner found it at (the VM may not ask, PM M203), and with sockets shared with it as
//! Winsock documents (WSADuplicateSocketW, PM M203). It writes in no directory of its user's:
//! the directory its snapshots go to its spawner makes, and stages each generation there for
//! it, handing it the generation's memory and state files to fill and committing the
//! generation on its word (`kind::STAGE`, `kind::COMMIT`, D196).
//!
//! Linux confines VMs with Landlock.
//!
//! CoreFoundation's bookmark calls (CFURL.h: `CFURLCreateBookmarkData`,
//! `CFURLCreateByResolvingBookmarkData`, `CFURLStartAccessingSecurityScopedResource`) are
//! current API, available since macOS 10.6 and 10.7 and marked deprecated nowhere.

use std::path::{Path, PathBuf};

/// The most paths one request names: a VM's kernel, initrd, init, disks, pmem files,
/// snapshot and the files it records, and the directories it writes in, with room.
pub const MAX_GRANTS: usize = 256;
/// The longest path a request names, in bytes: PATH_MAX on macOS; on Windows, its longest
/// path, 32,767 UTF-16 units ("Maximum Path Length Limitation", Microsoft Learn).
#[cfg(unix)]
pub const MAX_PATH: usize = 1024;
#[cfg(windows)]
pub const MAX_PATH: usize = 2 * 32_767;

/// `path` as a request carries it: its bytes on Unix; on Windows its UTF-16 units,
/// little-endian, as a Windows name need not be UTF-8.
#[cfg_attr(
    target_os = "macos",
    allow(
        dead_code,
        reason = "the VM's, which asks; macOS's spawner answers with no path"
    )
)]
pub fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
}

/// The path [`path_bytes`] carried, if they are one.
#[cfg_attr(
    target_os = "macos",
    allow(
        dead_code,
        reason = "the spawner's, which reads requests; macOS's VM reads no path"
    )
)]
pub fn path_from(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt as _;
        let (units, rest) = bytes.as_chunks::<2>();
        if !rest.is_empty() {
            return None;
        }
        let units: Vec<u16> = units.iter().map(|u| u16::from_le_bytes(*u)).collect();
        Some(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }
}

/// How a path is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// A file it reads: a descriptor opened read-only.
    Read,
    /// A file it reads if it is there: a descriptor, or word that it is not.
    ReadIfThere,
    /// A file it reads and writes: a descriptor opened read-write.
    Write,
    /// A directory it writes in, its own: made first if it is not there, then a read-write
    /// bookmark on macOS (a directory cannot be granted read-only, PM M70); on Windows,
    /// where its snapshots' generations are staged for it (D196).
    MakeDir,
    /// Its vsock device's socket path (`--vsock`): a socket the spawner binds there and
    /// hands it, and from then on, connections the spawner dials for it to the host ports
    /// beside it (`kind::DIAL`). App Sandbox lets it do neither (PM M67).
    Listen,
}

/// A path and how it is granted.
pub type Wanted = (Access, PathBuf);

/// The link a VM asks its spawner over on Windows: a pipe each way, as a Windows pipe
/// runs one way.
#[cfg(windows)]
#[derive(Debug)]
pub struct Link {
    /// What this end reads: the VM's requests at the spawner, the answers at the VM.
    pub from: std::fs::File,
    /// What this end writes.
    pub to: std::fs::File,
}
