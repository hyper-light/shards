//! A share's DAX window (D189): virtio-fs's cache, a range of the guest's physical address
//! space that its virtiofs maps a file's ranges into (fs/fuse/dax.c, 2 MiB each), each
//! mapped from the file itself in the host (MAP_SHARED), so that the file's data is the
//! host's page cache alone and the guest reads and writes it as memory.
//!
//! The window is host address space reserved for it, nothing mapped there but what the
//! guest asked for: on KVM one memslot covers it (`add_hotplug`), each range placed in the
//! reservation (MAP_FIXED), which KVM's MMU notifiers see; elsewhere each range is mapped
//! at stage 2 as it is placed, and unmapped before it is replaced (M193: a range replaced
//! in the host alone keeps the old pages at stage 2).
//!
//! A range is its file's pages up to the page that holds the file's end, and zeros past it,
//! the window's own: a page of a shared mapping wholly past its file's end is one the host
//! cannot give. HVF ends the vCPU's run at an access there with an abort the VMM cannot
//! complete (PM M271); x86 KVM retries the access without end, its vCPU spinning in KVM_RUN
//! and never returning (PM M273: an access it cannot fault in, asynchronously or not, is one
//! it takes to need I/O). So the window follows each file: before the guest is told
//! anything of a file's size, its ranges are made what the file is then ([`reconcile`]),
//! the file where it has grown and zeros where it was cut short; and where another process
//! cuts it short under the guest's mapping, which the guest reads past before it learns
//! the file's size, the window's [`Watch`] says so ([`cut`]), or the hypervisor's fault
//! does ([`memory_fault`]), and the guest reads zeros there until it learns.
//!
//! [`reconcile`]: Window::reconcile
//! [`cut`]: Window::cut
//! [`memory_fault`]: Window::memory_fault

use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Maps a window's ranges into the guest at the hypervisor's stage 2 and unmaps them: the
/// hypervisor's (`hv::Stage2`), or a test's.
pub trait Stage2: Send + Sync {
    /// Maps `len` bytes of host memory at `host` at `gpa`: writable by the guest only if
    /// `writable`, and executable, as a guest runs programs from its volumes.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// Watches the files a window maps for other processes cutting them short, and says so
/// with [`Window::cut`]: on Linux the daemon's watch, for its microVMs' windows (D189).
pub trait Watch: Send + Sync {
    /// Watches `file`, node `node`'s, which the window found `size` bytes long as it mapped
    /// it: from the window's first range of the node on.
    fn watch(&self, node: u64, file: BorrowedFd<'_>, size: u64);
    /// Node `node`'s file watched no more: the window maps none of it.
    fn unwatch(&self, node: u64);
}

/// The ranges the guest maps files in (FUSE_DAX_SZ), and what a mapping's offsets and
/// lengths must be whole multiples of.
pub const RANGE: u64 = 2 << 20;

/// Whether every range's descriptor is opened to write too, the guest's reads' as well:
/// Hypervisor.framework maps a shared file mapping only of such a descriptor (PM M17), and
/// a private mapping goes on showing the guest the bytes it first read after the host
/// writes the file (PM M270). Such a share maps a file only where its server may write it.
pub const SHARED_NEEDS_WRITE: bool = cfg!(hv = "hvf");

/// A range of a file mapped into the window: what a save keeps of it (D176).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub len: u64,
    /// The node of the share's server whose file it maps, and where in the file.
    pub node: u64,
    pub foffset: u64,
    pub writable: bool,
}

/// A mapped range, and what it maps.
struct Entry {
    mapped: Mapped,
    /// The descriptor it maps, kept to look at its file's size: none where zeros stand in
    /// for a file a restore could not reach ([`Window::map_zeros`]).
    file: Option<OwnedFd>,
    /// How far into the range the file's pages go: whole pages, to the one that holds the
    /// file's end as the window last looked, or the range's end; zeros past it.
    file_to: u64,
}

/// Host address space reserved for a window: inaccessible where nothing is mapped,
/// unmapped as it drops.
struct Reserved {
    at: *mut u8,
    len: usize,
}

// SAFETY: an address range this process owns; what maps into it is serialized by the
// window's lock.
unsafe impl Send for Reserved {}
// SAFETY: as above.
unsafe impl Sync for Reserved {}

impl Drop for Reserved {
    fn drop(&mut self) {
        // SAFETY: the range this reserved, unmapped once.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

pub struct Window {
    gpa: u64,
    len: u64,
    host: Reserved,
    stage2: Box<dyn Stage2>,
    mapped: Mutex<BTreeMap<u64, Entry>>,
    watch: OnceLock<Box<dyn Watch>>,
    /// Its changes to what its ranges hold, counted: a fault that came while one was under
    /// way is run again once it is done ([`Window::memory_fault`]).
    changes: AtomicU64,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("gpa", &self.gpa)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

thread_local! {
    /// The fault this thread was last told nothing changed for: by its window, its address
    /// (none on KVM) and the window's count of changes then.
    static UNCHANGED: Cell<Option<(usize, Option<u64>, u64)>> = const { Cell::new(None) };
}

/// Inaccessible, unbacked address space over `len` bytes at `at` (where `at` is not null,
/// in its place).
fn reserve(at: *mut u8, len: usize) -> std::io::Result<*mut u8> {
    let fixed = if at.is_null() { 0 } else { libc::MAP_FIXED };
    // SAFETY: an anonymous mapping, at a range this process reserved where `at` is given.
    let p = unsafe {
        libc::mmap(
            at.cast(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | fixed,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(p.cast())
}

/// Zeros over `len` bytes at `at`, in a range a window reserved: anonymous memory, the
/// guest's to write where `writable`.
fn zeros(at: *mut u8, len: usize, writable: bool) -> std::io::Result<()> {
    let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
    // SAFETY: MAP_FIXED within a range the window reserved and holds.
    let p = unsafe {
        libc::mmap(
            at.cast(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `len` bytes of `file` from `foffset` over `at`, in a range a window reserved.
fn file_range(
    at: *mut u8,
    len: usize,
    file: BorrowedFd<'_>,
    foffset: u64,
    writable: bool,
) -> std::io::Result<()> {
    let at = std::ptr::NonNull::new(at).ok_or(std::io::ErrorKind::InvalidInput)?;
    // SAFETY: within the window's reservation, which nothing else uses; what was there is
    // replaced, unmapped at stage 2 first.
    unsafe { crate::platform::map_file_range(file, foffset, len, at, writable) }
}

/// The size of the file `fd` holds open.
fn size_of(fd: &OwnedFd) -> Option<u64> {
    // SAFETY: a zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor held open, into `st`.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return None;
    }
    u64::try_from(st.st_size).ok()
}

/// The Linux errno the guest is told for the host's failure `e`.
fn errno(e: std::io::Error) -> i32 {
    super::host::linux_errno(e.raw_os_error().unwrap_or(libc::EIO))
}

/// The host's page size.
fn page() -> Option<u64> {
    crate::platform::page_size()
        .ok()
        .and_then(|p| u64::try_from(p).ok())
}

/// How far into a range of `len` bytes from `foffset` the pages of a file of `size` bytes
/// go: to the page that holds its end, at most the range's end.
fn file_end(size: u64, foffset: u64, len: u64, page: u64) -> u64 {
    size.saturating_sub(foffset)
        .checked_next_multiple_of(page)
        .unwrap_or(u64::MAX)
        .min(len)
}

/// Whether any range maps node `node`.
fn maps(mapped: &BTreeMap<u64, Entry>, node: u64) -> bool {
    mapped.values().any(|e| e.mapped.node == node)
}

impl Window {
    /// A window of `len` bytes at guest address `gpa`, whole [`RANGE`]s, its host side
    /// reserved and nothing mapped; `stage2`, given where the reservation begins, gives what
    /// maps each range placed in it into the guest (on KVM, having made the memslot that
    /// covers it).
    pub fn new(
        gpa: u64,
        len: u64,
        stage2: impl FnOnce(*mut u8) -> Result<Box<dyn Stage2>, String>,
    ) -> Result<Window, String> {
        if len == 0 || !len.is_multiple_of(RANGE) || !gpa.is_multiple_of(RANGE) {
            return Err(format!(
                "a DAX window of {len} bytes at {gpa:#x}: whole 2 MiB ranges"
            ));
        }
        let size = usize::try_from(len).map_err(|_| format!("a DAX window of {len} bytes"))?;
        let at = reserve(std::ptr::null_mut(), size).map_err(|e| format!("reserving a DAX window: {e}"))?;
        // Unmapped as it drops, whatever comes next.
        let host = Reserved { at, len: size };
        let stage2 = stage2(at)?;
        Ok(Window {
            gpa,
            len,
            host,
            stage2,
            mapped: Mutex::new(BTreeMap::new()),
            watch: OnceLock::new(),
            changes: AtomicU64::new(0),
        })
    }

    /// Where the window's host side begins: what KVM's memslot covers.
    pub fn host(&self) -> *mut u8 {
        self.host.at
    }

    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    pub fn size(&self) -> u64 {
        self.len
    }

    /// Has `watch` watch the files the window maps, from those it maps now on; a window
    /// watched already keeps its watch.
    pub fn watch_with(&self, watch: Box<dyn Watch>) {
        // Set under the lock, as a mapping looks for the watch under it ([`settle`]): each
        // node is watched once, here or by its mapping.
        //
        // [`settle`]: Self::settle
        let mapped = self.lock();
        if self.watch.set(watch).is_err() {
            return;
        }
        let mut now: Vec<(u64, OwnedFd, u64)> = Vec::new();
        for e in mapped.values() {
            if now.iter().any(|(node, ..)| *node == e.mapped.node) {
                continue;
            }
            let Some(file) = e.file.as_ref() else {
                continue;
            };
            if let (Ok(copy), Some(size)) = (file.try_clone(), size_of(file)) {
                now.push((e.mapped.node, copy, size));
            }
        }
        drop(mapped);
        if let Some(w) = self.watch.get() {
            for (node, file, size) in now {
                w.watch(node, file.as_fd(), size);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, Entry>> {
        self.mapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The host address of window offset `at` for `len` bytes, where they lie within it.
    fn host_at(&self, at: u64, len: u64) -> Option<(*mut u8, usize)> {
        if at.checked_add(len)? > self.len {
            return None;
        }
        let off = usize::try_from(at).ok()?;
        // SAFETY: within the reservation, which the check above bounds.
        Some((unsafe { self.host.at.add(off) }, usize::try_from(len).ok()?))
    }

    /// As [`host_at`](Self::host_at), for whole ranges alone: what the guest maps.
    fn place(&self, moffset: u64, len: u64) -> Option<(*mut u8, usize)> {
        if len == 0 || !moffset.is_multiple_of(RANGE) || !len.is_multiple_of(RANGE) {
            return None;
        }
        self.host_at(moffset, len)
    }

    /// Maps `len` bytes of `file` from `foffset` at window offset `moffset`, writable by
    /// the guest only if `writable`, replacing what was there: a FUSE_SETUPMAPPING's. The
    /// file's pages go to the one that holds its end, zeros past it. An errno where the
    /// range lies outside the window or the host refuses the mapping.
    pub fn map(
        &self,
        file: OwnedFd,
        node: u64,
        foffset: u64,
        moffset: u64,
        len: u64,
        writable: bool,
    ) -> Result<(), i32> {
        let (host, size) = self.place(moffset, len).ok_or(libc::EINVAL)?;
        let gpa = self.gpa.checked_add(moffset).ok_or(libc::EINVAL)?;
        let page = page().ok_or(libc::EINVAL)?;
        let file_size = size_of(&file).ok_or(libc::EIO)?;
        let file_to = file_end(file_size, foffset, len, page);
        let mut mapped = self.lock();
        let watched = maps(&mapped, node);
        let gone = self.forget(&mut mapped, moffset, len);
        let backed = (|| {
            let n = usize::try_from(file_to).map_err(|_| libc::EINVAL)?;
            if n > 0 {
                file_range(host, n, file.as_fd(), foffset, writable).map_err(errno)?;
            }
            if n < size {
                // SAFETY: `n` is short of the range's length, `size`.
                zeros(unsafe { host.add(n) }, size - n, writable).map_err(errno)?;
            }
            Ok(())
        })();
        let staged = backed.and_then(|()| {
            // SAFETY: the file's mapping and the zeros stay until this window replaces them,
            // or the VM, which goes before its devices, is destroyed.
            unsafe { self.stage2.map(host, gpa, size, writable) }.map_err(|e| {
                crate::debug!("virtio-fs: mapping a DAX range at {gpa:#x}: {e}");
                libc::EIO
            })
        });
        if let Err(errno) = staged {
            // Inaccessible again, as before.
            let _ = reserve(host, size);
            self.settle(mapped, gone, None);
            return Err(errno);
        }
        let watch = (!watched)
            .then(|| file.try_clone().ok())
            .flatten()
            .map(|copy| (node, copy, file_size));
        mapped.insert(
            moffset,
            Entry {
                mapped: Mapped {
                    len,
                    node,
                    foffset,
                    writable,
                },
                file: Some(file),
                file_to,
            },
        );
        self.settle(mapped, gone, watch);
        Ok(())
    }

    /// Zeros over `len` bytes at window offset `moffset`, which the guest holds as node
    /// `node`'s from `foffset`, where its file can be given no more: a restore whose server
    /// reaches the file a save mapped no longer (D176), so that the guest's access there
    /// finds memory, never a fault no window answers.
    pub fn map_zeros(
        &self,
        node: u64,
        foffset: u64,
        moffset: u64,
        len: u64,
        writable: bool,
    ) -> Result<(), i32> {
        let (host, size) = self.place(moffset, len).ok_or(libc::EINVAL)?;
        let gpa = self.gpa.checked_add(moffset).ok_or(libc::EINVAL)?;
        let mut mapped = self.lock();
        let gone = self.forget(&mut mapped, moffset, len);
        if let Err(e) = zeros(host, size, writable) {
            let _ = reserve(host, size);
            self.settle(mapped, gone, None);
            return Err(errno(e));
        }
        // SAFETY: anonymous memory the window holds until it replaces it.
        if let Err(e) = unsafe { self.stage2.map(host, gpa, size, writable) } {
            crate::debug!("virtio-fs: mapping zeros at {gpa:#x}: {e}");
            let _ = reserve(host, size);
            self.settle(mapped, gone, None);
            return Err(libc::EIO);
        }
        mapped.insert(
            moffset,
            Entry {
                mapped: Mapped {
                    len,
                    node,
                    foffset,
                    writable,
                },
                file: None,
                file_to: 0,
            },
        );
        self.settle(mapped, gone, None);
        Ok(())
    }

    /// Unmaps `len` bytes at window offset `moffset`: a FUSE_REMOVEMAPPING's. An errno where
    /// the range lies outside the window.
    pub fn unmap(&self, moffset: u64, len: u64) -> Result<(), i32> {
        let (host, size) = self.place(moffset, len).ok_or(libc::EINVAL)?;
        let mut mapped = self.lock();
        let gone = self.forget(&mut mapped, moffset, len);
        let reserved = reserve(host, size);
        self.settle(mapped, gone, None);
        reserved.map(|_| ()).map_err(errno)
    }

    /// Unmaps, at stage 2 and in the host, and forgets, every mapping that meets `len`
    /// bytes at `moffset`, each whole: one a new mapping overlaps in part leaves nothing of
    /// it reachable (Linux maps whole 2 MiB ranges, which never overlap; a guest of another
    /// kind may). The nodes of those forgotten.
    fn forget(&self, mapped: &mut BTreeMap<u64, Entry>, moffset: u64, len: u64) -> Vec<u64> {
        let end = moffset.saturating_add(len);
        let meeting: Vec<(u64, u64)> = mapped
            .range(..end)
            .filter(|(at, e)| at.saturating_add(e.mapped.len) > moffset)
            .map(|(at, e)| (*at, e.mapped.len))
            .collect();
        let mut gone = Vec::with_capacity(meeting.len());
        for (at, len) in meeting {
            let gpa = self.gpa.saturating_add(at);
            if let Err(e) = self.stage2.unmap(gpa, usize::try_from(len).unwrap_or(usize::MAX)) {
                crate::debug!("virtio-fs: unmapping a DAX range at {gpa:#x}: {e}");
            }
            if let Some((host, size)) = self.place(at, len)
                && let Err(e) = reserve(host, size)
            {
                crate::debug!("virtio-fs: clearing a DAX range at {gpa:#x}: {e}");
            }
            if let Some(e) = mapped.remove(&at) {
                gone.push(e.mapped.node);
            }
        }
        gone
    }

    /// Lets go of the lock, then tells the window's watch of the nodes of `gone` it maps
    /// none of now, and of `watch`, a node it has begun to map: never under the lock, as
    /// the watch may wait on another process.
    fn settle(
        &self,
        mapped: MutexGuard<'_, BTreeMap<u64, Entry>>,
        mut gone: Vec<u64>,
        watch: Option<(u64, OwnedFd, u64)>,
    ) {
        gone.sort_unstable();
        gone.dedup();
        gone.retain(|node| !maps(&mapped, *node));
        let watching = self.watch.get();
        drop(mapped);
        let Some(w) = watching else {
            return;
        };
        for node in gone {
            w.unwatch(node);
        }
        if let Some((node, file, size)) = watch {
            w.watch(node, file.as_fd(), size);
        }
    }

    /// Makes `e`'s range, at window offset `at`, what its file is now: its pages to the one
    /// that holds its end, where it has grown since, and zeros past that, where it was cut
    /// short. Whether it changed.
    fn follow(&self, at: u64, e: &mut Entry, page: u64) -> bool {
        let Some(size) = e.file.as_ref().and_then(size_of) else {
            return false;
        };
        let file_to = file_end(size, e.mapped.foffset, e.mapped.len, page);
        if file_to == e.file_to {
            return false;
        }
        let (from, to) = (file_to.min(e.file_to), file_to.max(e.file_to));
        let Some((host, size)) = at
            .checked_add(from)
            .and_then(|off| self.host_at(off, to.saturating_sub(from)))
        else {
            return false;
        };
        let (gpa, writable) = (
            self.gpa.saturating_add(at).saturating_add(from),
            e.mapped.writable,
        );
        if let Err(err) = self.stage2.unmap(gpa, size) {
            crate::debug!("virtio-fs: unmapping a DAX range's end at {gpa:#x}: {err}");
        }
        let backed = match e.file.as_ref() {
            Some(file) if file_to > e.file_to => file_range(
                host,
                size,
                file.as_fd(),
                e.mapped.foffset.saturating_add(from),
                writable,
            ),
            _ => zeros(host, size, writable),
        };
        let changed = match backed {
            Ok(()) => {
                let start = self.gpa.saturating_add(at);
                crate::debug!("virtio-fs: the DAX range at {start:#x} is its file's to {file_to:#x}");
                e.file_to = file_to;
                true
            }
            Err(err) => {
                // What a failed MAP_FIXED leaves there is unknown: zeros, as past the end.
                crate::debug!("virtio-fs: following a file at {gpa:#x}: {err}");
                let _ = zeros(host, size, writable);
                false
            }
        };
        // SAFETY: the file's mapping or zeros, which the window holds until it replaces them.
        if let Err(err) = unsafe { self.stage2.map(host, gpa, size, writable) } {
            crate::debug!("virtio-fs: mapping a DAX range's end at {gpa:#x}: {err}");
        }
        changed
    }

    /// Makes the ranges of the files of `nodes` what each file is now ([`follow`]): called
    /// before the guest is told anything of their sizes, by the reply to a request that may
    /// tell it, so that what it may read of a file grown is the file's, and what a file cut
    /// short has no more, zeros.
    ///
    /// [`follow`]: Self::follow
    pub fn reconcile(&self, nodes: &[u64]) {
        if nodes.is_empty() {
            return;
        }
        let Some(page) = page() else {
            return;
        };
        let mut mapped = self.lock();
        if mapped.is_empty() {
            return;
        }
        let mut nodes = nodes.to_vec();
        nodes.sort_unstable();
        let mut changed = false;
        for (&at, e) in mapped.iter_mut() {
            if nodes.binary_search(&e.mapped.node).is_ok() {
                changed |= self.follow(at, e, page);
            }
        }
        if changed {
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Node `node`'s file was cut short by another process, its watch says: its ranges are
    /// what it is now, zeros where it has no pages, and a vCPU that spun on one of those
    /// pages runs on (PM M273).
    pub fn cut(&self, node: u64) {
        self.reconcile(&[node]);
    }

    /// A guest's access to memory the host could not give, at `gpa` where the hypervisor
    /// says (HVF; KVM's EFAULT says nothing, and every range is looked at): a range whose
    /// file another process cut short, which is made what the file is now. True where the
    /// access may run again: something changed, or nothing did here but the access fell in
    /// a range, which another thread may have been changing as it faulted (HVF unmaps a
    /// range at stage 2 before it maps it again; two vCPUs may fault on one page cut
    /// short), told once: the same fault again, with nothing changed between, is the
    /// guest's, and false.
    pub fn memory_fault(&self, gpa: Option<u64>) -> bool {
        let Some(page) = page() else {
            return false;
        };
        let mut mapped = self.lock();
        let (mut held, mut changed) = (false, false);
        for (&at, e) in mapped.iter_mut() {
            let start = self.gpa.saturating_add(at);
            if gpa.is_some_and(|g| g < start || g >= start.saturating_add(e.mapped.len)) {
                continue;
            }
            held = true;
            changed |= self.follow(at, e, page);
        }
        drop(mapped);
        if changed {
            self.changes.fetch_add(1, Ordering::AcqRel);
            UNCHANGED.set(None);
            return true;
        }
        if !held {
            return false;
        }
        let now = (
            std::ptr::from_ref(self) as usize,
            gpa,
            self.changes.load(Ordering::Acquire),
        );
        if UNCHANGED.get() == Some(now) {
            return false;
        }
        UNCHANGED.set(Some(now));
        true
    }

    /// Whether any range maps node `node`'s file.
    pub fn maps(&self, node: u64) -> bool {
        maps(&self.lock(), node)
    }

    /// Every mapping, by window offset: what a save keeps (D176).
    pub fn mappings(&self) -> Vec<(u64, Mapped)> {
        self.lock().iter().map(|(at, e)| (*at, e.mapped)).collect()
    }

    /// Unmaps everything: the device reset, its guest's mappings gone with its driver.
    pub fn clear(&self) {
        let _ = self.unmap(0, self.len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// What a test's stage 2 was told: each map's and unmap's guest address and length.
    #[derive(Default)]
    struct Told(Mutex<Vec<(bool, u64, usize)>>);

    struct Recorded(Arc<Told>);

    impl Stage2 for Recorded {
        unsafe fn map(&self, _host: *mut u8, gpa: u64, len: usize, _: bool) -> Result<(), String> {
            self.0.0.lock().unwrap().push((true, gpa, len));
            Ok(())
        }
        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            self.0.0.lock().unwrap().push((false, gpa, len));
            Ok(())
        }
    }

    /// What a test's watch was told: each node watched, with its file's size then, and
    /// each let go of.
    #[derive(Default)]
    struct Watched(Mutex<Vec<(bool, u64, u64)>>);

    struct Watching(Arc<Watched>);

    impl Watch for Watching {
        fn watch(&self, node: u64, _: BorrowedFd<'_>, size: u64) {
            self.0.0.lock().unwrap().push((true, node, size));
        }
        fn unwatch(&self, node: u64) {
            self.0.0.lock().unwrap().push((false, node, 0));
        }
    }

    const MIB: u64 = 1 << 20;
    const GPA: u64 = 1 << 32;

    fn pattern(at: u64) -> u8 {
        (at / 4096 % 251) as u8
    }

    fn window(told: &Arc<Told>) -> Window {
        let stage2 = Recorded(told.clone());
        Window::new(GPA, 16 * MIB, |_| Ok(Box::new(stage2) as Box<dyn Stage2>)).unwrap()
    }

    /// A file's range is mapped into the window from the file itself, where the guest
    /// asks, and told to stage 2; one past the window or not in whole ranges is refused,
    /// as is a writable mapping of a descriptor that may not write; one that meets another
    /// in part replaces it whole; and unmapped, it is forgotten. Its file is watched from
    /// its first range on, and let go of with its last.
    #[test]
    fn a_files_ranges_are_mapped_into_the_window_and_out() {
        let dir = shards_testdir::TempDir::new("dax-window").unwrap();
        let path = dir.join("file");
        let bytes: Vec<u8> = (0..8 * MIB).map(pattern).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let fd = || OwnedFd::from(file.try_clone().unwrap());
        let told = Arc::new(Told::default());
        let watched = Arc::new(Watched::default());
        let w = window(&told);
        w.watch_with(Box::new(Watching(watched.clone())));
        w.map(fd(), 7, 2 * MIB, 4 * MIB, 2 * MIB, false).unwrap();
        // SAFETY: the window's own range, the file mapped there, for as long as `w` is.
        let seen = unsafe { std::slice::from_raw_parts(w.host().add(4 << 20), 2 << 20) };
        assert!(
            seen.iter()
                .enumerate()
                .all(|(i, &b)| b == pattern(2 * MIB + i as u64))
        );
        assert_eq!(
            w.mappings(),
            [(
                4 * MIB,
                Mapped {
                    len: 2 * MIB,
                    node: 7,
                    foffset: 2 * MIB,
                    writable: false
                }
            )]
        );
        assert_eq!(*told.0.lock().unwrap(), [(true, GPA + 4 * MIB, 2 << 20)]);
        assert_eq!(*watched.0.lock().unwrap(), [(true, 7, 8 * MIB)]);
        assert_eq!(w.map(fd(), 7, 0, 16 * MIB, 2 * MIB, false), Err(libc::EINVAL));
        assert_eq!(w.map(fd(), 7, 0, MIB, 2 * MIB, false), Err(libc::EINVAL));
        assert_eq!(w.map(fd(), 7, 0, 0, 3 * MIB, false), Err(libc::EINVAL));
        assert_eq!(w.map(fd(), 7, 0, 0, 2 * MIB, true), Err(libc::EACCES));
        // Over it in part: the old one is unmapped whole, and only the new one is left.
        told.0.lock().unwrap().clear();
        w.map(fd(), 8, 0, 2 * MIB, 4 * MIB, false).unwrap();
        assert_eq!(
            *told.0.lock().unwrap(),
            [(false, GPA + 4 * MIB, 2 << 20), (true, GPA + 2 * MIB, 4 << 20)]
        );
        assert_eq!(w.mappings().len(), 1);
        assert_eq!(w.mappings()[0].0, 2 * MIB);
        assert_eq!(
            *watched.0.lock().unwrap(),
            [(true, 7, 8 * MIB), (false, 7, 0), (true, 8, 8 * MIB)]
        );
        w.unmap(2 * MIB, 4 * MIB).unwrap();
        assert!(w.mappings().is_empty());
        assert_eq!(watched.0.lock().unwrap().last(), Some(&(false, 8, 0)));
        assert_eq!(w.unmap(15 * MIB, 2 * MIB), Err(libc::EINVAL));
    }

    /// A range past its file's end holds the file's pages to the one that holds its end,
    /// zeros past it; grown, the file's again once the window follows it, and cut short,
    /// zeros where the file has no pages: before the guest is told the file's size
    /// (`reconcile`), as its watch says it was cut (`cut`), or as the guest faults there
    /// (`memory_fault`), which is the window's only within a range, and told once where
    /// nothing changed.
    #[test]
    fn a_range_follows_its_file_as_it_grows_and_is_cut_short() {
        let dir = shards_testdir::TempDir::new("dax-cut").unwrap();
        let path = dir.join("file");
        let page = crate::platform::page_size().unwrap() as u64;
        // 3 MiB and a byte: its range from 2 MiB holds the file to the page past its first
        // MiB and a byte, zeros after.
        let bytes: Vec<u8> = (0..3 * MIB + 1).map(pattern).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let told = Arc::new(Told::default());
        let w = window(&told);
        w.map(
            OwnedFd::from(file.try_clone().unwrap()),
            7,
            2 * MIB,
            4 * MIB,
            2 * MIB,
            true,
        )
        .unwrap();
        // SAFETY: the window's own range, for as long as `w` is.
        let seen = unsafe { std::slice::from_raw_parts_mut(w.host().add(4 << 20), 2 << 20) };
        let from = (MIB + 1).next_multiple_of(page) as usize;
        assert!(
            seen[..=MIB as usize]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == pattern(2 * MIB + i as u64))
        );
        assert!(seen[from..].iter().all(|&b| b == 0), "zeros past the end");
        // Nothing to follow: nothing changes, and a fault in the range is told once.
        told.0.lock().unwrap().clear();
        w.reconcile(&[7]);
        assert!(told.0.lock().unwrap().is_empty());
        assert!(!w.memory_fault(Some(GPA)), "no range holds the fault");
        assert!(w.memory_fault(Some(GPA + 4 * MIB + 5)), "told once");
        assert!(!w.memory_fault(Some(GPA + 4 * MIB + 5)), "and the guest's after");
        // Grown by another process with new bytes: the guest is told the size only after
        // the window maps them.
        let grown: Vec<u8> = (0..MIB).map(|i| pattern(i) ^ 0x5a).collect();
        crate::platform::write_all_at(&file, &grown, 3 * MIB).unwrap();
        assert!(seen[from..].iter().all(|&b| b == 0), "not followed yet");
        w.reconcile(&[8, 9]);
        assert!(seen[from..].iter().all(|&b| b == 0), "another node's");
        w.reconcile(&[9, 7]);
        assert_eq!(&seen[MIB as usize..2 * MIB as usize], &grown[..]);
        let rest = 2 * MIB as usize - from;
        assert_eq!(
            *told.0.lock().unwrap(),
            [
                (false, GPA + 4 * MIB + from as u64, rest),
                (true, GPA + 4 * MIB + from as u64, rest)
            ]
        );
        // Written through the window, the file has it.
        seen[MIB as usize + 3] = 0xa5;
        let mut back = [0u8; 1];
        use std::os::unix::fs::FileExt as _;
        file.read_exact_at(&mut back, 3 * MIB + 3).unwrap();
        assert_eq!(back[0], 0xa5);
        // Cut short under the mapping: its watch says so, and zeros stand where the file
        // has no pages; the rest is the file's still.
        file.set_len(2 * MIB + 10).unwrap();
        w.cut(7);
        let kept = 10usize.next_multiple_of(page as usize);
        assert!(seen[kept..].iter().all(|&b| b == 0));
        assert!(
            seen[..10]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == pattern(2 * MIB + i as u64))
        );
        // Grown and cut again, with no reply nor watch between: the guest's fault there
        // has the window follow it, and runs again.
        file.set_len(4 * MIB).unwrap();
        assert!(w.memory_fault(None));
        assert!(seen[kept..].iter().all(|&b| b == 0), "the file's holes");
        file.set_len(MIB).unwrap();
        assert!(w.memory_fault(Some(GPA + 4 * MIB)));
        assert!(
            seen.iter().all(|&b| b == 0),
            "nothing of the file is left in the range"
        );
    }
}
