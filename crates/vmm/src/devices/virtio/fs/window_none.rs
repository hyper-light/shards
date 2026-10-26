//! A share's DAX window (D189) where none is built yet: Windows. There each range's view will be placed in a
//! placeholder reservation that WHP's stage 2 follows (PM M341), and a share another server serves will answer a
//! mapping with a section of the file (PM M343). Until then no device has a window. [`Window`] holds an uninhabited
//! value, so none is ever made and every path through one is a path no Windows device takes. Its methods are total
//! without a panic, and [`Window::new`] says why there is none. The other hosts' window is `window.rs`; this module
//! keeps its API, so the device is the same code on every host.

use std::os::windows::io::OwnedHandle;

/// Maps a window's ranges into the guest at the hypervisor's stage 2 and unmaps them, as on the other hosts.
pub trait Stage2: Send + Sync {
    /// Maps `len` bytes of host memory at `host` at `gpa`: writable by the guest only if `writable`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// The ranges the guest maps files in (FUSE_DAX_SZ), as on the other hosts.
pub const RANGE: u64 = 2 << 20;

/// A range of a file mapped into the window: what a save keeps of it (D176), as on the other hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub len: u64,
    /// The node of the share's server whose file it maps, and where in the file.
    pub node: u64,
    pub foffset: u64,
    pub writable: bool,
}

/// A DAX window, which no Windows device has yet: it cannot be made.
#[derive(Debug)]
pub struct Window {
    never: std::convert::Infallible,
}

impl Window {
    /// Refused: no DAX window is built on Windows yet.
    pub fn new(
        _gpa: u64,
        _len: u64,
        _stage2: impl FnOnce(*mut u8) -> Result<Box<dyn Stage2>, String>,
    ) -> Result<Window, String> {
        Err("a DAX window is not built on Windows yet (D189)".into())
    }

    pub fn host(&self) -> *mut u8 {
        match self.never {}
    }

    pub fn gpa(&self) -> u64 {
        match self.never {}
    }

    pub fn held(&self) -> (*mut u8, usize) {
        match self.never {}
    }

    pub fn rerecord(&self, _node: u64, _offset: u64, _len: u64) {
        match self.never {}
    }

    pub fn size(&self) -> u64 {
        match self.never {}
    }

    pub fn map(
        &self,
        _file: OwnedHandle,
        _node: u64,
        _foffset: u64,
        _moffset: u64,
        _len: u64,
        _writable: bool,
    ) -> Result<u64, i32> {
        match self.never {}
    }

    pub fn map_zeros(
        &self,
        _node: u64,
        _foffset: u64,
        _moffset: u64,
        _len: u64,
        _writable: bool,
    ) -> Result<(), i32> {
        match self.never {}
    }

    pub fn unmap(&self, _moffset: u64, _len: u64) -> Result<(), i32> {
        match self.never {}
    }

    pub fn reconcile(&self, _nodes: &[u64]) {
        match self.never {}
    }

    pub fn cut(&self, _node: u64) {
        match self.never {}
    }

    pub fn memory_fault(&self, _gpa: Option<u64>) -> bool {
        match self.never {}
    }

    pub fn maps(&self, _node: u64) -> bool {
        match self.never {}
    }

    pub fn mappings(&self) -> Vec<(u64, Mapped)> {
        match self.never {}
    }

    pub fn clear(&self) {
        match self.never {}
    }
}
