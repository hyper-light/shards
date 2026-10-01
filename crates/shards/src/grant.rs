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
//! macOS alone: Linux confines VMs with Landlock.
//!
//! CoreFoundation's bookmark calls (CFURL.h: `CFURLCreateBookmarkData`,
//! `CFURLCreateByResolvingBookmarkData`, `CFURLStartAccessingSecurityScopedResource`) are
//! current API, available since macOS 10.6 and 10.7 and marked deprecated nowhere.

use std::path::PathBuf;

/// The most paths one request names: a VM's kernel, initrd, init, disks, pmem files,
/// snapshot and the files it records, and the directories it writes in, with room.
pub const MAX_GRANTS: usize = 256;
/// The longest path a request names (PATH_MAX on macOS).
pub const MAX_PATH: usize = 1024;

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
    /// bookmark. A directory cannot be granted read-only (PM M70).
    MakeDir,
    /// Its vsock device's socket path (`--vsock`): a socket the spawner binds there and
    /// hands it, and from then on, connections the spawner dials for it to the host ports
    /// beside it (`kind::DIAL`). App Sandbox lets it do neither (PM M67).
    Listen,
}

/// A path and how it is granted.
pub type Wanted = (Access, PathBuf);
