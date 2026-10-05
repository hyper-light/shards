//! What packing and unpacking ask of the system, on Unix and on Windows.

use crate::tar::Time;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

/// A file's type, as Go's FileMode tells it. Windows has no devices or FIFOs to pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) enum FileKind {
    File,
    Dir,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
    /// Go's ModeIrregular, or a type Go names none of.
    Other,
}

/// What lstat says of a file. Windows has no owners, links or device numbers to pack.
#[derive(Debug, Clone)]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) struct Stat {
    pub(crate) kind: FileKind,
    /// `st_mode` on Unix; Go's permission bits on Windows.
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) size: u64,
    pub(crate) mtime: Time,
    pub(crate) ino: u64,
    pub(crate) dev: u64,
    pub(crate) nlink: u64,
    pub(crate) rdev: u64,
}
