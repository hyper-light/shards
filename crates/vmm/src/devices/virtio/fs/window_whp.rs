//! A share's DAX window (D189) on Windows: virtio-fs's cache, the range of the guest's
//! physical address space that its virtiofs maps a file's ranges into (fs/fuse/dax.c, 2 MiB
//! each), each a view of the section that the share's server answers the range's ask with:
//! slates' daemon, which did not spawn this process and duplicates each section into it
//! (PM M343). Shards' own shares have no server on Windows yet, so only a share another
//! server serves (D38) has a window here.
//!
//! The window is host address space reserved as placeholders, one a range (VirtualAlloc2,
//! MEM_RESERVE_PLACEHOLDER, split at each range). A view replaces a range's placeholder
//! (MapViewOfFile3, MEM_REPLACE_PLACEHOLDER); committed zeros do where its server's section
//! cannot be had, a restore's, as on the other hosts; and either is freed back to its
//! placeholder (UnmapViewOfFile2 or VirtualFree, MEM_PRESERVE_PLACEHOLDER). A view fills its
//! placeholder exactly, so a mapping of several ranges is a view a range.
//!
//! WHP's stage 2 follows the host's view (PM M341): a range freed under a standing stage-2
//! mapping exits the guest's access to it to the VMM, at its address, and a view placed
//! there is the guest's at once, with no further call; M193, HVF's range replaced in the
//! host alone keeping its old pages, does not hold on WHP. So each range is mapped at stage
//! 2 as it is first placed and left standing: a replace is a free and a place, 82.1 µs at
//! p50 on windows-2025, where unmapping and mapping it at stage 2 too took 122.1 (M341,
//! "standing" against "remap"). A range never placed is not mapped at stage 2: a 64 GiB
//! window's 32,768 ranges would cost its boot as many maps, 33.5 µs each at p50 there.
//!
//! What the Unix window follows of a host file (its size, a cut under the guest, a fault
//! past its end) has nothing to follow here. A range is a view of the server's section,
//! which reaches past every range the server answers an ask for (it refuses an ask past
//! it), whose pages the server commits before it answers, zeros past the file's end. So
//! [`Window::reconcile`] and [`Window::cut`] change nothing, and a fault in a mapped range
//! is only a vCPU's access while another thread replaced the range, run again once.
//!
//! Its held record (kernel patch 0009) says every page of a range mapped to be written
//! held: the server's section is memory committed before its answer, so no page of it is
//! read from a disk that the guest's write of whole pages would overwrite. A range mapped
//! to be read alone, or zeros, has none held, as on the other hosts.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::proto;

/// Maps a window's ranges into the guest at the hypervisor's stage 2 and unmaps them: WHP's
/// (`hv::Stage2`), or a test's.
pub trait Stage2: Send + Sync {
    /// Maps `len` bytes of host memory at `host` at `gpa`: writable by the guest only if
    /// `writable`, and executable, as a guest runs programs from its volumes.
    ///
    /// # Safety
    /// `host..host+len` must stay reserved in this process until unmapped here or the VM is
    /// destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String>;
    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String>;
}

/// The ranges the guest maps files in (FUSE_DAX_SZ), and what a mapping's offsets and
/// lengths must be whole multiples of.
pub const RANGE: u64 = 2 << 20;

/// A range of a file mapped into the window: what a save keeps of it (D176).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapped {
    pub len: u64,
    /// The node of the share's server whose file it maps, and where in the file.
    pub node: u64,
    pub foffset: u64,
    pub writable: bool,
}

/// A mapping, and what it maps.
struct Entry {
    mapped: Mapped,
    /// Whether committed zeros stand in for its server's section ([`Window::map_zeros`]);
    /// else each of its ranges is a view of it.
    zeros: bool,
}

/// A window's mappings, by window offset; how many of them map each node; and which of
/// its ranges are mapped at stage 2, standing, one bit a range.
#[derive(Default)]
struct Table {
    ranges: BTreeMap<u64, Entry>,
    nodes: BTreeMap<u64, usize>,
    standing: Vec<u64>,
}

impl Table {
    /// The mappings that meet `len` bytes at window offset `moffset`, `(window offset,
    /// length)` each, whole.
    fn meeting(&self, moffset: u64, len: u64) -> Vec<(u64, u64)> {
        let end = moffset.saturating_add(len);
        self.ranges
            .range(..end)
            .filter(|(at, e)| at.saturating_add(e.mapped.len) > moffset)
            .map(|(at, e)| (*at, e.mapped.len))
            .collect()
    }

    fn insert(&mut self, moffset: u64, entry: Entry) {
        *self.nodes.entry(entry.mapped.node).or_default() += 1;
        self.ranges.insert(moffset, entry);
    }

    fn remove(&mut self, moffset: u64) -> Option<Entry> {
        let e = self.ranges.remove(&moffset)?;
        if let Some(count) = self.nodes.get_mut(&e.mapped.node) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.nodes.remove(&e.mapped.node);
            }
        }
        Some(e)
    }

    fn is_standing(&self, range: u64) -> bool {
        let (word, bit) = (range / 64, range % 64);
        usize::try_from(word)
            .ok()
            .and_then(|w| self.standing.get(w))
            .is_some_and(|w| w & (1 << bit) != 0)
    }

    fn stand(&mut self, range: u64) {
        let (word, bit) = (range / 64, range % 64);
        if let Some(w) = usize::try_from(word).ok().and_then(|w| self.standing.get_mut(w)) {
            *w |= 1 << bit;
        }
    }
}

/// The window's address space: `len` bytes at `at`, a placeholder for each of its ranges
/// where no view or zeros replace it, each released as it drops (the window frees its
/// views and zeros first).
struct Placeholders {
    at: *mut u8,
    len: usize,
}

// SAFETY: an address range this process owns; what is placed into it is serialized by the
// window's lock.
unsafe impl Send for Placeholders {}
// SAFETY: as above.
unsafe impl Sync for Placeholders {}

impl Drop for Placeholders {
    fn drop(&mut self) {
        // Each range's placeholder, released whole: a placeholder not yet split from the
        // rest, where making the window failed part way, is released whole at its range's
        // address, and the release of each range within it then finds nothing there.
        for at in (0..self.len).step_by(RANGE as usize) {
            // SAFETY: a placeholder of this window's, which nothing references any more.
            unsafe { VirtualFree(self.at.wrapping_add(at).cast(), 0, MEM_RELEASE) };
        }
    }
}

/// Committed memory of the window's own: its held record.
struct Committed {
    at: *mut u8,
    len: usize,
}

// SAFETY: memory this process owns, read and written atomically alone (`held_words`).
unsafe impl Send for Committed {}
// SAFETY: as above.
unsafe impl Sync for Committed {}

impl Drop for Committed {
    fn drop(&mut self) {
        // SAFETY: the allocation this value made, released once.
        unsafe { VirtualFree(self.at.cast(), 0, MEM_RELEASE) };
    }
}

pub struct Window {
    gpa: u64,
    len: u64,
    host: Placeholders,
    /// Which of its pages its host holds, one bit a guest's page, for its guest to read
    /// (shared memory region `HELD`, kernel patch 0009): set for a range mapped to be
    /// written, cleared as it is unmapped or maps zeros. Memory of the window's own, which
    /// its guest writes too.
    held: Committed,
    stage2: Box<dyn Stage2>,
    mapped: Mutex<Table>,
    /// Its changes to what its ranges hold, counted: a fault that came while one was under
    /// way is run again once it is done ([`Window::memory_fault`]).
    changes: AtomicU64,
    /// What a section's view must begin at a multiple of: the system's allocation
    /// granularity (GetSystemInfo), 64 KiB on every Windows shards runs on.
    granularity: u64,
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
    /// and the window's count of changes then.
    static UNCHANGED: Cell<Option<(usize, Option<u64>, u64)>> = const { Cell::new(None) };
}

/// The page protection a range mapped as `writable` is given.
fn protection(writable: bool) -> u32 {
    if writable { PAGE_READWRITE } else { PAGE_READONLY }
}

impl Window {
    /// A window of `len` bytes at guest address `gpa`, whole [`RANGE`]s, its host side a
    /// placeholder a range and nothing mapped; `stage2`, given where the reservation
    /// begins, gives what maps each range into the guest as it is first placed.
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
        let mut info = SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo fills the struct it is given.
        unsafe { GetSystemInfo(&mut info) };
        let granularity = u64::from(info.dwAllocationGranularity);
        if granularity == 0 || !RANGE.is_multiple_of(granularity) {
            return Err(format!(
                "an allocation granularity of {granularity} bytes, which no 2 MiB range is a multiple of"
            ));
        }
        // SAFETY: a fresh placeholder of this process's, which only this window references.
        let at = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                size,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        }
        .cast::<u8>();
        if at.is_null() {
            return Err(format!(
                "reserving a DAX window: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Released as it drops, whatever comes next.
        let host = Placeholders { at, len: size };
        let ranges = len / RANGE;
        for range in 0..ranges.saturating_sub(1) {
            let off = usize::try_from(range * RANGE).map_err(|_| format!("a DAX window of {len} bytes"))?;
            // SAFETY: the first RANGE bytes of the placeholder at this range, split from the
            // rest of it, all of it this window's.
            if unsafe {
                VirtualFree(
                    at.wrapping_add(off).cast(),
                    RANGE as usize,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )
            } == 0
            {
                return Err(format!(
                    "splitting a DAX window's placeholder: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        let held_len = crate::devices::virtio::head::window_held_len(len)?;
        let held_size = usize::try_from(held_len).map_err(|_| format!("a DAX window of {len} bytes"))?;
        // SAFETY: fresh committed memory, zeros, which only this window references.
        let held_at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                held_size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        }
        .cast::<u8>();
        if held_at.is_null() {
            return Err(format!(
                "a DAX window's record: {}",
                std::io::Error::last_os_error()
            ));
        }
        let held = Committed {
            at: held_at,
            len: held_size,
        };
        let stage2 = stage2(at)?;
        let words =
            usize::try_from(ranges.div_ceil(64)).map_err(|_| format!("a DAX window of {len} bytes"))?;
        Ok(Window {
            gpa,
            len,
            host,
            held,
            stage2,
            mapped: Mutex::new(Table {
                standing: vec![0; words],
                ..Table::default()
            }),
            changes: AtomicU64::new(0),
            granularity,
        })
    }

    /// Where the window's host side begins.
    pub fn host(&self) -> *mut u8 {
        self.host.at
    }

    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    /// Its held record's memory, `(host, len)`, which its VM maps for its guest, writable,
    /// right after the window (`HELD`).
    pub fn held(&self) -> (*mut u8, usize) {
        (self.held.at, self.held.len)
    }

    pub fn size(&self) -> u64 {
        self.len
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.mapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ranges `len` bytes at window offset `moffset` are, `(first, count)`: none where
    /// they are not whole ranges within the window.
    fn ranges_of(&self, moffset: u64, len: u64) -> Option<(u64, u64)> {
        let end = moffset.checked_add(len)?;
        (len > 0 && moffset.is_multiple_of(RANGE) && len.is_multiple_of(RANGE) && end <= self.len)
            .then_some((moffset / RANGE, len / RANGE))
    }

    /// Where range `range` begins in the host.
    fn range_at(&self, range: u64) -> *mut u8 {
        let off = usize::try_from(range.saturating_mul(RANGE)).unwrap_or(usize::MAX);
        self.host.at.wrapping_add(off)
    }

    /// The words of its held record for `size` bytes at window offset `moffset`, whole
    /// ranges: 64 of the guest's pages a word, and a range eight of them.
    fn held_words(&self, moffset: u64, size: u64) -> &[AtomicU64] {
        let page = crate::devices::virtio::head::WINDOW_PAGE;
        let first = usize::try_from(moffset / page / 64).unwrap_or(usize::MAX);
        let count = usize::try_from(size / page / 64).unwrap_or(usize::MAX);
        let words = self.held.len / std::mem::size_of::<AtomicU64>();
        let (first, count) = if first.checked_add(count).is_some_and(|end| end <= words) {
            (first, count)
        } else {
            (0, 0)
        };
        // SAFETY: the record's own memory, committed for the window's life and aligned to
        // the host's pages; its words are only ever read and written atomically, here and by
        // the guest's kernel (set_bit, test_bit), and lie within it (checked above).
        unsafe { std::slice::from_raw_parts(self.held.at.cast::<AtomicU64>().add(first), count) }
    }

    /// Says every page of `size` bytes at window offset `moffset` held where `held`, else
    /// none.
    fn record_held(&self, moffset: u64, size: u64, held: bool) {
        let bits = if held { u64::MAX } else { 0 };
        for word in self.held_words(moffset, size) {
            word.store(bits, Ordering::Release);
        }
    }

    /// Frees range `range` back to its placeholder: its view, or its zeros where `zeros`.
    fn free(&self, range: u64, zeros: bool) {
        let at = self.range_at(range);
        // SAFETY: a view or zeros this window placed in its own range, which it replaces
        // with the range's placeholder again: the guest's access there exits, as stage 2
        // follows the host's view (M341).
        let freed = unsafe {
            if zeros {
                VirtualFree(at.cast(), RANGE as usize, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)
            } else {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            }
        };
        if freed == 0 {
            crate::debug!(
                "virtio-fs: freeing a DAX range at {:#x}: {}",
                self.gpa.saturating_add(range.saturating_mul(RANGE)),
                std::io::Error::last_os_error()
            );
        }
    }

    /// Maps each of `count` ranges from `first` at stage 2 that is not mapped there yet, and
    /// leaves it standing: an errno where the hypervisor refuses one.
    fn stand(&self, table: &mut Table, first: u64, count: u64) -> Result<(), i32> {
        for range in first..first.saturating_add(count) {
            if table.is_standing(range) {
                continue;
            }
            let gpa = self.gpa.saturating_add(range.saturating_mul(RANGE));
            // SAFETY: the range's placeholder, view or zeros stay reserved until the window
            // drops, which unmaps every standing range first.
            unsafe { self.stage2.map(self.range_at(range), gpa, RANGE as usize, true) }.map_err(|e| {
                crate::debug!("virtio-fs: mapping a DAX range at {gpa:#x}: {e}");
                proto::EIO
            })?;
            table.stand(range);
        }
        Ok(())
    }

    /// Maps `len` bytes of `section` from `foffset` at window offset `moffset`, writable by
    /// the guest only if `writable`, replacing what was there: a FUSE_SETUPMAPPING's. Its
    /// bytes, which its microVM's I/O limits charge the mapping (D178): the whole of it, as
    /// where a file ends within a server's section is the server's to know. An errno where
    /// the range lies outside the window, in other than whole ranges, begins its section
    /// where no view can, or the host refuses the view.
    pub fn map(
        &self,
        section: OwnedHandle,
        node: u64,
        foffset: u64,
        moffset: u64,
        len: u64,
        writable: bool,
    ) -> Result<u64, i32> {
        let (first, count) = self.ranges_of(moffset, len).ok_or(proto::EINVAL)?;
        if !foffset.is_multiple_of(self.granularity) || foffset.checked_add(len).is_none() {
            return Err(proto::EINVAL);
        }
        let mut table = self.lock();
        self.forget(&mut table, moffset, len);
        for i in 0..count {
            let at = self.range_at(first + i);
            // SAFETY: the range's placeholder, of exactly RANGE bytes at `at`, replaced by a
            // view of the section within it (checked: no overflow above).
            let view = unsafe {
                MapViewOfFile3(
                    section.as_raw_handle(),
                    GetCurrentProcess(),
                    at.cast(),
                    foffset + i * RANGE,
                    RANGE as usize,
                    MEM_REPLACE_PLACEHOLDER,
                    protection(writable),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if view.Value != at.cast() {
                crate::debug!(
                    "virtio-fs: placing a DAX range at {:#x}: {}",
                    self.gpa.saturating_add(moffset),
                    std::io::Error::last_os_error()
                );
                (first..first + i).for_each(|range| self.free(range, false));
                return Err(proto::EIO);
            }
        }
        if let Err(errno) = self.stand(&mut table, first, count) {
            (first..first + count).for_each(|range| self.free(range, false));
            return Err(errno);
        }
        table.insert(
            moffset,
            Entry {
                mapped: Mapped {
                    len,
                    node,
                    foffset,
                    writable,
                },
                zeros: false,
            },
        );
        self.record_held(moffset, len, writable);
        self.changes.fetch_add(1, Ordering::AcqRel);
        Ok(len)
    }

    /// Zeros over `len` bytes at window offset `moffset`, which the guest holds as node
    /// `node`'s from `foffset`, where its server's section can be given no more: a restore
    /// whose server reaches the file a save mapped no longer (D176), so that the guest's
    /// access there finds memory, never a fault no window answers.
    pub fn map_zeros(
        &self,
        node: u64,
        foffset: u64,
        moffset: u64,
        len: u64,
        writable: bool,
    ) -> Result<(), i32> {
        let (first, count) = self.ranges_of(moffset, len).ok_or(proto::EINVAL)?;
        let mut table = self.lock();
        self.forget(&mut table, moffset, len);
        for i in 0..count {
            let at = self.range_at(first + i);
            // SAFETY: committed zeros replace the range's placeholder, at its base and size.
            let zeros = unsafe {
                VirtualAlloc2(
                    GetCurrentProcess(),
                    at.cast(),
                    RANGE as usize,
                    MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                    protection(writable),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if zeros != at.cast() {
                crate::debug!(
                    "virtio-fs: zeros for a DAX range at {:#x}: {}",
                    self.gpa.saturating_add(moffset),
                    std::io::Error::last_os_error()
                );
                (first..first + i).for_each(|range| self.free(range, true));
                return Err(proto::EIO);
            }
        }
        if let Err(errno) = self.stand(&mut table, first, count) {
            (first..first + count).for_each(|range| self.free(range, true));
            return Err(errno);
        }
        table.insert(
            moffset,
            Entry {
                mapped: Mapped {
                    len,
                    node,
                    foffset,
                    writable,
                },
                zeros: true,
            },
        );
        self.record_held(moffset, len, false);
        self.changes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Unmaps `len` bytes at window offset `moffset`: a FUSE_REMOVEMAPPING's. An errno where
    /// the range lies outside the window or in other than whole ranges.
    pub fn unmap(&self, moffset: u64, len: u64) -> Result<(), i32> {
        self.ranges_of(moffset, len).ok_or(proto::EINVAL)?;
        let mut table = self.lock();
        self.forget(&mut table, moffset, len);
        self.changes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Frees, and forgets, every mapping that meets `len` bytes at `moffset`, each whole:
    /// one a new mapping overlaps in part leaves nothing of it reachable (Linux maps whole
    /// 2 MiB ranges, which never overlap; a guest of another kind may). Stage 2 stands.
    fn forget(&self, table: &mut Table, moffset: u64, len: u64) {
        for (at, n) in table.meeting(moffset, len) {
            if let Some(e) = table.remove(at) {
                for range in at / RANGE..(at.saturating_add(n)) / RANGE {
                    self.free(range, e.zeros);
                }
            }
            self.record_held(at, n, false);
        }
    }

    /// Records again which pages its host holds of the ranges that map node `node`'s file
    /// written there: nothing to record, as every page of a range mapped to be written is
    /// held from its mapping on (the server's section is memory), and a write leaves it so.
    pub fn rerecord(&self, _node: u64, _offset: u64, _len: u64) {}

    /// Makes the ranges of the files of `nodes` what each file is now: nothing changes, as a
    /// range is a view of its server's section, which reaches past every range asked of it,
    /// zeros past the file's end.
    pub fn reconcile(&self, _nodes: &[u64]) {}

    /// Another process cut node `node`'s file short: as [`Window::reconcile`], nothing to
    /// change.
    pub fn cut(&self, _node: u64) {}

    /// A guest's access to memory the host could not give, at `gpa`, which WHP's memory
    /// access exit names: in a mapped range, only a vCPU's access while another thread
    /// replaced the range (stage 2 follows the host's view, M341), which may run again now
    /// that the lock it waited for is free; told once, as the same fault again with nothing
    /// changed between is the guest's (a write to a range mapped to be read alone), and
    /// false. Elsewhere false: the window gives nothing there.
    pub fn memory_fault(&self, gpa: Option<u64>) -> bool {
        let Some(at) = gpa
            .and_then(|g| g.checked_sub(self.gpa))
            .filter(|&at| at < self.len)
        else {
            return false;
        };
        let table = self.lock();
        let held = table
            .ranges
            .range(..=at)
            .next_back()
            .is_some_and(|(start, e)| at < start.saturating_add(e.mapped.len));
        drop(table);
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
        self.lock().nodes.contains_key(&node)
    }

    /// Every mapping, by window offset: what a save keeps (D176).
    pub fn mappings(&self) -> Vec<(u64, Mapped)> {
        self.lock().ranges.iter().map(|(at, e)| (*at, e.mapped)).collect()
    }

    /// Unmaps everything: the device reset, its guest's mappings gone with its driver.
    pub fn clear(&self) {
        let _ = self.unmap(0, self.len);
    }
}

impl Drop for Window {
    /// Frees every range's view or zeros, and unmaps every standing range at stage 2, before
    /// the placeholders are released: stage 2 must not map address space this process has
    /// given back.
    fn drop(&mut self) {
        let mut table = self.lock();
        self.forget(&mut table, 0, self.len);
        let ranges = self.len / RANGE;
        for range in (0..ranges).filter(|&r| table.is_standing(r)) {
            let gpa = self.gpa.saturating_add(range.saturating_mul(RANGE));
            if let Err(e) = self.stage2.unmap(gpa, RANGE as usize) {
                crate::debug!("virtio-fs: unmapping a DAX range at {gpa:#x}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The window over real pagefile-backed sections, as slates' daemon makes them (PM M343),
    //! with a stage 2 that records what it is told: what a WHP guest reaches through it is
    //! M341's, and the guest test in tests/slates_fs.rs.
    use std::os::windows::io::FromRawHandle as _;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    use super::*;

    const MIB: u64 = 1 << 20;
    const GPA: u64 = 1 << 32;

    /// What a test's stage 2 was told: each map's (true) and unmap's guest address and
    /// length, and whether a map was writable.
    #[derive(Default)]
    struct Told(Mutex<Vec<(bool, u64, usize, bool)>>);

    struct Recorded(Arc<Told>);

    impl Stage2 for Recorded {
        unsafe fn map(&self, _host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String> {
            self.0.0.lock().unwrap().push((true, gpa, len, writable));
            Ok(())
        }
        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            self.0.0.lock().unwrap().push((false, gpa, len, false));
            Ok(())
        }
    }

    fn window(told: &Arc<Told>, len: u64) -> Window {
        let stage2 = Recorded(told.clone());
        Window::new(GPA, len, |_| Ok(Box::new(stage2) as Box<dyn Stage2>)).unwrap()
    }

    /// A pagefile-backed section of `len` bytes, as slates' daemon makes one, and its
    /// server's own view, unmapped as it drops, each byte `fill`'s.
    struct Section {
        handle: OwnedHandle,
        view: *mut u8,
        len: usize,
    }

    impl Section {
        fn new(len: usize, fill: impl Fn(usize) -> u8) -> Section {
            let size = len as u64;
            // SAFETY: a fresh section of the pagefile's, checked below.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    std::ptr::null(),
                )
            };
            assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
            // SAFETY: the section just made, owned from here on.
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            // SAFETY: a view of the whole section, checked below, unmapped as it drops.
            let view = unsafe { MapViewOfFile(handle.as_raw_handle(), FILE_MAP_ALL_ACCESS, 0, 0, len) }
                .Value
                .cast::<u8>();
            assert!(!view.is_null(), "{}", std::io::Error::last_os_error());
            for at in 0..len {
                // SAFETY: within the view.
                unsafe { view.add(at).write(fill(at)) };
            }
            Section { handle, view, len }
        }

        /// A handle of it for the window, as a server duplicates one.
        fn give(&self) -> OwnedHandle {
            self.handle.try_clone().unwrap()
        }

        fn byte(&self, at: usize) -> u8 {
            assert!(at < self.len);
            // SAFETY: within the view.
            unsafe { self.view.add(at).read_volatile() }
        }

        fn store(&self, at: usize, value: u8) {
            assert!(at < self.len);
            // SAFETY: within the view.
            unsafe { self.view.add(at).write_volatile(value) };
        }
    }

    impl Drop for Section {
        fn drop(&mut self) {
            // SAFETY: the view this value made.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view.cast(),
                })
            };
        }
    }

    fn pattern(at: usize) -> u8 {
        (at / 4096 % 251) as u8
    }

    /// The byte at window offset `at`, through the window's host side.
    fn read(w: &Window, at: u64) -> u8 {
        assert!(at < w.size());
        // SAFETY: within the window, at a range a test has placed.
        unsafe { w.host().add(at as usize).read_volatile() }
    }

    fn write(w: &Window, at: u64, value: u8) {
        assert!(at < w.size());
        // SAFETY: within the window, at a range a test has placed writable.
        unsafe { w.host().add(at as usize).write_volatile(value) };
    }

    /// Do: map 4 MiB of a section from 2 MiB at window offset 4 MiB, writable. Expect the
    /// window to hold the section's bytes there; a store through either view to be seen
    /// through the other; stage 2 told to map each of the two ranges once, writable; and a
    /// mapping of another section over them to be its bytes with stage 2 told nothing more,
    /// as it follows the host's view (M341).
    #[test]
    fn a_range_is_its_sections_bytes_each_way_and_stage_2_is_told_once() {
        let told = Arc::new(Told::default());
        let w = window(&told, 16 * MIB);
        let a = Section::new((8 * MIB) as usize, pattern);
        assert_eq!(w.map(a.give(), 7, 2 * MIB, 4 * MIB, 4 * MIB, true), Ok(4 * MIB));
        for at in (0..4 * MIB).step_by(4096) {
            assert_eq!(
                read(&w, 4 * MIB + at),
                pattern((2 * MIB + at) as usize),
                "{at:#x}"
            );
        }
        write(&w, 4 * MIB + 100, 0xa5);
        assert_eq!(
            a.byte((2 * MIB + 100) as usize),
            0xa5,
            "the guest's store is the server's"
        );
        a.store((3 * MIB) as usize, 0x5a);
        assert_eq!(read(&w, 5 * MIB), 0x5a, "the server's store is the guest's");
        assert_eq!(
            *told.0.lock().unwrap(),
            [
                (true, GPA + 4 * MIB, RANGE as usize, true),
                (true, GPA + 6 * MIB, RANGE as usize, true)
            ]
        );
        let b = Section::new((4 * MIB) as usize, |_| 0x77);
        assert_eq!(w.map(b.give(), 8, 0, 4 * MIB, 4 * MIB, false), Ok(4 * MIB));
        assert_eq!(read(&w, 4 * MIB), 0x77);
        assert_eq!(read(&w, 8 * MIB - 1), 0x77);
        assert_eq!(told.0.lock().unwrap().len(), 2, "stage 2 stands");
        assert!(w.maps(8) && !w.maps(7));
        assert_eq!(
            w.mappings(),
            [(
                4 * MIB,
                Mapped {
                    len: 4 * MIB,
                    node: 8,
                    foffset: 0,
                    writable: false
                }
            )]
        );
    }

    /// Do: unmap a mapping, then map again where it was; and map one range over the second
    /// of a two-range mapping. Expect each unmapped range to take a view again (its
    /// placeholder kept), and the mapping met in part to be forgotten whole, its first range
    /// a placeholder again too.
    #[test]
    fn an_unmapped_range_takes_a_view_again_and_one_met_in_part_goes_whole() {
        let told = Arc::new(Told::default());
        let w = window(&told, 8 * MIB);
        let s = Section::new((4 * MIB) as usize, pattern);
        assert_eq!(w.map(s.give(), 1, 0, 0, 4 * MIB, true), Ok(4 * MIB));
        assert_eq!(w.unmap(0, 4 * MIB), Ok(()));
        assert!(w.mappings().is_empty() && !w.maps(1));
        assert_eq!(w.map(s.give(), 1, 0, 0, 4 * MIB, true), Ok(4 * MIB));
        assert_eq!(read(&w, 3 * MIB), pattern((3 * MIB) as usize));
        assert_eq!(w.map(s.give(), 2, 2 * MIB, 2 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(w.mappings().len(), 1, "the mapping met in part is gone whole");
        assert!(!w.maps(1) && w.maps(2));
        assert_eq!(w.map(s.give(), 3, 0, 0, 2 * MIB, false), Ok(2 * MIB));
        assert_eq!(read(&w, 4096), pattern(4096));
    }

    /// Do: map zeros where a server cannot answer (a restore's), store into them, then map a
    /// section over them. Expect zeros, the store kept in the window's own memory alone, and
    /// then the section's bytes.
    #[test]
    fn zeros_stand_in_where_the_server_cannot_answer() {
        let told = Arc::new(Told::default());
        let w = window(&told, 4 * MIB);
        assert_eq!(w.map_zeros(5, 0, 2 * MIB, 2 * MIB, true), Ok(()));
        assert_eq!(read(&w, 2 * MIB + 4096), 0);
        write(&w, 2 * MIB + 4096, 9);
        assert_eq!(read(&w, 2 * MIB + 4096), 9);
        assert!(w.maps(5));
        let s = Section::new((2 * MIB) as usize, |_| 0x33);
        assert_eq!(w.map(s.give(), 5, 0, 2 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(read(&w, 2 * MIB + 4096), 0x33);
        assert_eq!(s.byte(4096), 0x33, "the store into zeros reached no section");
    }

    /// Do: map outside the window, in parts of ranges, at a section offset no view can
    /// begin at, and from a handle that is no section. Expect EINVAL, EINVAL, EINVAL and EIO,
    /// and the range to take a view after each.
    #[test]
    fn what_no_view_can_be_is_refused_and_leaves_the_range_as_it_was() {
        let told = Arc::new(Told::default());
        let w = window(&told, 4 * MIB);
        let s = Section::new((4 * MIB) as usize, pattern);
        assert_eq!(w.map(s.give(), 1, 0, 4 * MIB, 2 * MIB, true), Err(proto::EINVAL));
        assert_eq!(w.map(s.give(), 1, 0, 0, 0, true), Err(proto::EINVAL));
        assert_eq!(w.map(s.give(), 1, 0, MIB, 2 * MIB, true), Err(proto::EINVAL));
        assert_eq!(w.map(s.give(), 1, 0, 0, MIB, true), Err(proto::EINVAL));
        assert_eq!(w.map(s.give(), 1, 4096, 0, 2 * MIB, true), Err(proto::EINVAL));
        assert_eq!(w.unmap(MIB, 2 * MIB), Err(proto::EINVAL));
        // SAFETY: an unnamed event, which is no section, owned from here on.
        let event =
            unsafe { OwnedHandle::from_raw_handle(CreateEventW(std::ptr::null(), 1, 0, std::ptr::null())) };
        assert_eq!(w.map(event, 1, 0, 0, 2 * MIB, true), Err(proto::EIO));
        assert!(w.mappings().is_empty());
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(read(&w, 4096), pattern(4096));
    }

    /// Do: map a range to be written, then to be read, then zeros, then unmap. Expect the
    /// held record (patch 0009) to say every page of it held for the first alone.
    #[test]
    fn the_held_record_says_a_range_mapped_to_be_written_held() {
        let told = Arc::new(Told::default());
        let w = window(&told, 4 * MIB);
        let s = Section::new((2 * MIB) as usize, pattern);
        let words = |w: &Window| {
            w.held_words(2 * MIB, RANGE)
                .iter()
                .map(|b| b.load(Ordering::Acquire))
                .collect::<Vec<_>>()
        };
        let first = |w: &Window| {
            w.held_words(0, RANGE)
                .iter()
                .all(|b| b.load(Ordering::Acquire) == 0)
        };
        assert_eq!(w.map(s.give(), 1, 0, 2 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert!(!words(&w).is_empty() && words(&w).iter().all(|&b| b == u64::MAX));
        assert!(first(&w), "no other range's held");
        assert_eq!(w.map(s.give(), 1, 0, 2 * MIB, 2 * MIB, false), Ok(2 * MIB));
        assert!(words(&w).iter().all(|&b| b == 0));
        assert_eq!(w.map(s.give(), 1, 0, 2 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(w.map_zeros(1, 0, 2 * MIB, 2 * MIB, true), Ok(()));
        assert!(words(&w).iter().all(|&b| b == 0));
        assert_eq!(w.map(s.give(), 1, 0, 2 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(w.unmap(2 * MIB, 2 * MIB), Ok(()));
        assert!(words(&w).iter().all(|&b| b == 0));
    }

    /// Do: fault in a mapped range twice, then again after the window changed; and fault
    /// outside the window, in a range nothing maps, and with no address. Expect the first
    /// fault run again, the same fault with nothing changed the guest's (false), the one
    /// after a change run again, and the rest the guest's.
    #[test]
    fn a_fault_in_a_mapped_range_runs_again_once() {
        let told = Arc::new(Told::default());
        let w = window(&told, 4 * MIB);
        let s = Section::new((2 * MIB) as usize, pattern);
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, false), Ok(2 * MIB));
        let at = Some(GPA + 4096);
        assert!(w.memory_fault(at), "a replace may have been under way");
        assert!(!w.memory_fault(at), "nothing changed: the guest's");
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, false), Ok(2 * MIB));
        assert!(w.memory_fault(at), "the window changed since");
        assert!(
            !w.memory_fault(Some(GPA + 2 * MIB + 4096)),
            "a range nothing maps"
        );
        assert!(!w.memory_fault(Some(GPA - 4096)), "outside the window");
        assert!(!w.memory_fault(Some(GPA + 4 * MIB)), "past the window");
        assert!(!w.memory_fault(None));
    }

    /// Do: map two ranges, then drop the window. Expect stage 2 told to unmap each standing
    /// range before the window's address space goes.
    #[test]
    fn a_window_unmaps_its_standing_ranges_as_it_goes() {
        let told = Arc::new(Told::default());
        let w = window(&told, 8 * MIB);
        let s = Section::new((2 * MIB) as usize, pattern);
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(w.map(s.give(), 1, 0, 6 * MIB, 2 * MIB, true), Ok(2 * MIB));
        drop(w);
        let unmapped: Vec<_> = told
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|t| !t.0)
            .map(|t| (t.1, t.2))
            .collect();
        assert_eq!(unmapped, [(GPA, RANGE as usize), (GPA + 6 * MIB, RANGE as usize)]);
    }

    /// What WHP's stage 2 answered each call through the window: a map's (true) or an
    /// unmap's guest address, and whether it took.
    #[cfg(hv = "whp")]
    struct Answered(crate::hv::Stage2, Arc<Mutex<Vec<(bool, u64, bool)>>>);

    #[cfg(hv = "whp")]
    impl Stage2 for Answered {
        unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<(), String> {
            // SAFETY: forwarded caller contract.
            let took = unsafe { self.0.map_file(host, gpa, len, writable) }.map_err(|e| e.to_string());
            self.1.lock().unwrap().push((true, gpa, took.is_ok()));
            took
        }
        fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
            let took = self.0.unmap(gpa, len).map_err(|e| e.to_string());
            self.1.lock().unwrap().push((false, gpa, took.is_ok()));
            took
        }
    }

    /// Do, on a real WHP partition: place a view to be read alone first, so that its range's
    /// standing stage-2 mapping, writable for what replaces it later, lies over a read-only
    /// view; then zeros, and a view to be written; then drop the window. Expect WHP to take
    /// every map and unmap: a stage-2 map is taken over any view or placeholder (M341), and
    /// the view's protection, not stage 2's, keeps a range read alone. Skips where the host
    /// offers no WHP.
    #[cfg(hv = "whp")]
    #[test]
    fn whp_takes_each_standing_range_over_any_view() {
        if let Err(why) = crate::hv::check_host() {
            eprintln!("SKIP: no WHP here ({why})");
            return;
        }
        let vm = crate::hv::Vm::new(crate::hv::VmConfig { vcpus: 1 }).unwrap();
        let answered = Arc::new(Mutex::new(Vec::new()));
        let len = 8 * MIB;
        let w = Window::new(GPA, len, |host| {
            // SAFETY: the window's reservation stays until the window drops, before the VM.
            let stage2 = unsafe { vm.add_hotplug(host, GPA, len as usize) }.map_err(|e| e.to_string())?;
            Ok(Box::new(Answered(stage2, answered.clone())) as Box<dyn Stage2>)
        })
        .unwrap();
        let s = Section::new((4 * MIB) as usize, pattern);
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, false), Ok(2 * MIB));
        assert_eq!(read(&w, 4096), pattern(4096));
        assert_eq!(w.map_zeros(2, 0, 2 * MIB, 2 * MIB, true), Ok(()));
        assert_eq!(w.map(s.give(), 3, 2 * MIB, 4 * MIB, 2 * MIB, true), Ok(2 * MIB));
        assert_eq!(w.map(s.give(), 1, 0, 0, 2 * MIB, true), Ok(2 * MIB));
        drop(w);
        let answered = answered.lock().unwrap().clone();
        assert_eq!(
            answered,
            [
                (true, GPA, true),
                (true, GPA + 2 * MIB, true),
                (true, GPA + 4 * MIB, true),
                (false, GPA, true),
                (false, GPA + 2 * MIB, true),
                (false, GPA + 4 * MIB, true),
            ]
        );
        drop(vm);
    }

    /// Do: make a window of 64 GiB, a placeholder for each of its 32,768 ranges, and let it
    /// go. Expect it made and released, and print how long its making took.
    #[test]
    fn a_window_of_64_gib_is_made_and_let_go() {
        let told = Arc::new(Told::default());
        let started = std::time::Instant::now();
        let w = window(&told, 64 << 30);
        eprintln!(
            "a 64 GiB DAX window, {} ranges split: made in {:?}",
            (64u64 << 30) / RANGE,
            started.elapsed()
        );
        drop(w);
        assert!(
            told.0.lock().unwrap().is_empty(),
            "no range placed, none mapped at stage 2"
        );
    }
}
