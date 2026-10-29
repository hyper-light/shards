//! A share's DAX window (D189) on Windows: virtio-fs's cache, the range of the guest's
//! physical address space that its virtiofs maps a file's ranges into (fs/fuse/dax.c, 2 MiB
//! each), each a view of a section of the file the share's server answers the range's ask
//! with: one slates' daemon makes, which did not spawn this process and duplicates each
//! section into it (PM M343), or one the window makes of the file shards' own share process
//! gives it (D197's slice 4).
//!
//! The window is host address space reserved as placeholders, one a range (VirtualAlloc2,
//! MEM_RESERVE_PLACEHOLDER, split at each range). A view replaces a range's placeholder
//! (MapViewOfFile3, MEM_REPLACE_PLACEHOLDER); committed zeros do where its server's section
//! cannot be had, a restore's, as on the other hosts; and either is freed back to its
//! placeholder (UnmapViewOfFile2 or VirtualFree, MEM_PRESERVE_PLACEHOLDER). A view fills its
//! placeholder exactly, so a mapping of several ranges is a view a range; and a range a
//! file ends within is its placeholder split at the page that holds the file's end, a view
//! as long as the file's bytes there and committed zeros after it ([`Part`]), coalesced
//! again as it is freed (PM M412: a placeholder splits at any page, and a view asked past
//! its section's bytes is refused).
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
//! A slates server's section reaches past every range it answers an ask for (it refuses an
//! ask past it), its pages committed before it answers, zeros past the file's end: nothing
//! of it is followed, and a fault in its range is only a vCPU's access while another thread
//! replaced the range, run again once. A section of a file reaches no further than the
//! file, so a share's own file is followed as the Unix window follows one: before the guest
//! is told anything of its size, its ranges are made what it is then
//! ([`Window::reconcile`]), its views as long as its bytes there, zeros past them. No other
//! process cuts it short while a view of it is mapped: NTFS refuses to (PM M412,
//! ERROR_USER_MAPPED_FILE), and its guest's own cut finds it let go of first
//! ([`Window::evict`]), each vCPU that reaches it meanwhile waiting until it is made again.
//!
//! Its held record (kernel patch 0009) says every page of a range mapped to be written
//! held: the server's section is memory committed before its answer, so no page of it is
//! read from a disk that the guest's write of whole pages would overwrite. A range mapped
//! to be read alone, or zeros, has none held, as on the other hosts.

/// Whether a file a guest maps shared through its window is opened to write too (window.rs's
/// rule): not on WHP, where a view of a section its server makes read-only is mapped
/// read-only.
pub const SHARED_NEEDS_WRITE: bool = false;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use windows_sys::Win32::Storage::FileSystem::GetFileSizeEx;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS,
    PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::SystemServices::MEM_COALESCE_PLACEHOLDERS;
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
    /// Where it maps a share's own file: how many of its bytes the file's views hold, the
    /// file's bytes in it as the window last looked, zeros after them. None where a
    /// server's section fills every range.
    file_to: Option<u64>,
    /// Its views let go of before its guest cuts its file short ([`Window::evict`]), its
    /// ranges placeholders until they are made what the file is again.
    evicted: bool,
}

/// What a range of a mapping is made of: a view of `n` bytes of its file's section, the
/// whole range's or fewer, committed zeros after them; or zeros alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    View(u64),
    Zeros,
}

/// What range `i` of `e`'s ranges is made of.
fn part(e: &Entry, i: u64) -> Part {
    match e.file_to {
        _ if e.zeros => Part::Zeros,
        None => Part::View(RANGE),
        Some(to) => part_to(to, i),
    }
}

/// What range `i` of a mapping whose file's views hold `to` of its bytes is made of.
fn part_to(to: u64, i: u64) -> Part {
    match to.saturating_sub(i.saturating_mul(RANGE)).min(RANGE) {
        0 => Part::Zeros,
        n => Part::View(n),
    }
}

/// A node the window maps: how many of its mappings do, and its file, where its share's own
/// server gave it, of which the window makes its sections and reads its size.
#[derive(Default)]
struct Node {
    count: usize,
    file: Option<OwnedHandle>,
}

/// A window's mappings, by window offset; the nodes they map; and which of its ranges are
/// mapped at stage 2, standing, one bit a range.
#[derive(Default)]
struct Table {
    ranges: BTreeMap<u64, Entry>,
    nodes: BTreeMap<u64, Node>,
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

    /// Keeps `entry` at `moffset`, and its node's `file` where the node has none yet.
    fn insert(&mut self, moffset: u64, entry: Entry, file: Option<OwnedHandle>) {
        let node = self.nodes.entry(entry.mapped.node).or_default();
        node.count += 1;
        if node.file.is_none() {
            node.file = file;
        }
        self.ranges.insert(moffset, entry);
    }

    fn remove(&mut self, moffset: u64) -> Option<Entry> {
        let e = self.ranges.remove(&moffset)?;
        if let Some(node) = self.nodes.get_mut(&e.mapped.node) {
            node.count = node.count.saturating_sub(1);
            if node.count == 0 {
                self.nodes.remove(&e.mapped.node);
            }
        }
        Some(e)
    }

    /// The window offsets of node `node`'s mappings.
    fn of(&self, node: u64) -> Vec<u64> {
        self.ranges
            .iter()
            .filter(|(_, e)| e.mapped.node == node)
            .map(|(at, _)| *at)
            .collect()
    }

    /// Whether the mapping that holds window offset `at` was let go of for a cut.
    fn evicted_at(&self, at: u64) -> bool {
        self.ranges
            .range(..=at)
            .next_back()
            .is_some_and(|(start, e)| at < start.saturating_add(e.mapped.len) && e.evicted)
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
    /// Told as a mapping its guest's cut let go of is made again: each vCPU that reached it
    /// meanwhile waits on it ([`Window::evict`]).
    restored: Condvar,
    /// What a section's view must begin at a multiple of: the system's allocation
    /// granularity (GetSystemInfo), 64 KiB on every Windows shards runs on.
    granularity: u64,
    /// The host's page (GetSystemInfo): where a range a file ends within is split.
    page: u64,
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
        let page = u64::from(info.dwPageSize);
        if page == 0 || !RANGE.is_multiple_of(page) {
            return Err(format!(
                "a host page of {page} bytes, which no 2 MiB range is a multiple of"
            ));
        }
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
            restored: Condvar::new(),
            granularity,
            page,
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

    /// Where a range a file's `n` bytes are a view of is split: the page after them, none
    /// where they reach its last page and the view fills it whole.
    fn split_at(&self, n: u64) -> Option<u64> {
        let head = n.checked_next_multiple_of(self.page)?;
        (head < RANGE).then_some(head)
    }

    /// Unmaps a view this window placed at `at`, its placeholder there again: whether it did.
    fn unmap_view(at: *mut u8) -> bool {
        // SAFETY: a view this window placed in its own range, replaced with its placeholder
        // again: the guest's access there exits, as stage 2 follows the host's view (M341).
        let unmapped = unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS { Value: at.cast() },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        unmapped != 0
    }

    /// Frees `len` bytes of zeros this window committed at `at` to their placeholder.
    fn release_zeros(at: *mut u8, len: u64) -> bool {
        // SAFETY: zeros this window committed in its own range, of exactly `len` bytes,
        // replaced with their placeholder again.
        let freed = unsafe { VirtualFree(at.cast(), len as usize, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
        freed != 0
    }

    /// Makes range `range`'s two placeholders, split at the page after a view, one again.
    fn coalesce(&self, range: u64) -> bool {
        let at = self.range_at(range);
        // SAFETY: the two placeholders of this window's own range, which together are it.
        let made = unsafe { VirtualFree(at.cast(), RANGE as usize, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS) };
        made != 0
    }

    /// Frees range `range` back to its placeholder, one again: its view, its zeros, or a
    /// split range's view and zeros, coalesced (`part`).
    fn free(&self, range: u64, part: Part) {
        let at = self.range_at(range);
        let freed = match part {
            Part::Zeros => Self::release_zeros(at, RANGE),
            Part::View(n) => match self.split_at(n) {
                None => Self::unmap_view(at),
                Some(head) => {
                    Self::unmap_view(at)
                        && Self::release_zeros(at.wrapping_add(head as usize), RANGE - head)
                        && self.coalesce(range)
                }
            },
        };
        if !freed {
            crate::debug!(
                "virtio-fs: freeing a DAX range at {:#x}: {}",
                self.gpa.saturating_add(range.saturating_mul(RANGE)),
                std::io::Error::last_os_error()
            );
        }
    }

    /// Makes range `range` `part` of a mapping, writable by the guest only if `writable`: a
    /// view of `section` from `foffset`, of the whole range, or of its file's `n` bytes in
    /// a placeholder split after the page that holds them, zeros committed in the rest; or
    /// zeros alone. Whether it did: where it did not, the range is its placeholder again.
    fn place(
        &self,
        range: u64,
        section: Option<&OwnedHandle>,
        foffset: u64,
        part: Part,
        writable: bool,
    ) -> bool {
        let at = self.range_at(range);
        let zeros = |at: *mut u8, len: u64| {
            // SAFETY: committed zeros replace a placeholder of this window's own, at its
            // base and of its size.
            let made = unsafe {
                VirtualAlloc2(
                    GetCurrentProcess(),
                    at.cast(),
                    len as usize,
                    MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                    protection(writable),
                    std::ptr::null_mut(),
                    0,
                )
            };
            made == at.cast()
        };
        let view = |section: &OwnedHandle, len: u64| {
            // SAFETY: a view of `len` bytes of the section from `foffset` replaces the
            // range's placeholder, which its pages fill (checked by its caller: the whole
            // range, or the placeholder split after the page that holds them).
            let made = unsafe {
                MapViewOfFile3(
                    section.as_raw_handle(),
                    GetCurrentProcess(),
                    at.cast(),
                    foffset,
                    len as usize,
                    MEM_REPLACE_PLACEHOLDER,
                    protection(writable),
                    std::ptr::null_mut(),
                    0,
                )
            };
            made.Value == at.cast()
        };
        let placed = match (part, section) {
            (Part::Zeros, _) => zeros(at, RANGE),
            (Part::View(_), None) => false,
            (Part::View(n), Some(section)) => match self.split_at(n) {
                None => view(section, n),
                Some(head) => {
                    // SAFETY: the range's placeholder, split after `head` bytes.
                    let split = unsafe {
                        VirtualFree(at.cast(), head as usize, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)
                    } != 0;
                    if !split {
                        false
                    } else if !view(section, n) {
                        self.coalesce(range);
                        false
                    } else if !zeros(at.wrapping_add(head as usize), RANGE - head) {
                        Self::unmap_view(at);
                        self.coalesce(range);
                        false
                    } else {
                        true
                    }
                }
            },
        };
        if !placed {
            crate::debug!(
                "virtio-fs: placing a DAX range at {:#x}: {}",
                self.gpa.saturating_add(range.saturating_mul(RANGE)),
                std::io::Error::last_os_error()
            );
        }
        placed
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

    /// Maps `len` bytes of node `node`'s file from `foffset` at window offset `moffset`,
    /// writable by the guest only if `writable`, replacing what was there: a
    /// FUSE_SETUPMAPPING's, of what its server gave, a section of its own or the file
    /// itself, of which the window makes a section as long as the file. Its bytes, which its
    /// microVM's I/O limits charge the mapping (D178): the file's in it, and the whole of it
    /// where a server's section holds it, where the file ends being that server's to know.
    /// An errno where the range lies outside the window, in other than whole ranges, begins
    /// its file where no view can, or the host refuses the view.
    pub fn map(
        &self,
        object: OwnedHandle,
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
        let (section, file, file_to) = match super::made_of(object.as_raw_handle()) {
            Some(super::Made::Section) => (Some(object), None, None),
            Some(super::Made::File) => {
                let size = file_size(&object).ok_or(proto::EIO)?;
                let to = len.min(size.saturating_sub(foffset));
                let section = match to {
                    0 => None,
                    _ => Some(section_of(&object, writable).ok_or(proto::EIO)?),
                };
                (section, Some(object), Some(to))
            }
            None => return Err(proto::EIO),
        };
        let parts = |i: u64| file_to.map_or(Part::View(RANGE), |to| part_to(to, i));
        let mut table = self.lock();
        self.forget(&mut table, moffset, len);
        for i in 0..count {
            if !self.place(
                first + i,
                section.as_ref(),
                foffset + i * RANGE,
                parts(i),
                writable,
            ) {
                (0..i).for_each(|j| self.free(first + j, parts(j)));
                return Err(proto::EIO);
            }
        }
        if let Err(errno) = self.stand(&mut table, first, count) {
            (0..count).for_each(|j| self.free(first + j, parts(j)));
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
                file_to,
                evicted: false,
            },
            file,
        );
        self.record_held(moffset, len, writable);
        self.changes.fetch_add(1, Ordering::AcqRel);
        Ok(file_to.unwrap_or(len))
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
            if !self.place(first + i, None, 0, Part::Zeros, writable) {
                (0..i).for_each(|j| self.free(first + j, Part::Zeros));
                return Err(proto::EIO);
            }
        }
        if let Err(errno) = self.stand(&mut table, first, count) {
            (0..count).for_each(|j| self.free(first + j, Part::Zeros));
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
                file_to: None,
                evicted: false,
            },
            None,
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
    /// 2 MiB ranges, which never overlap; a guest of another kind may). Stage 2 stands. One
    /// let go of for a cut is its placeholders already.
    fn forget(&self, table: &mut Table, moffset: u64, len: u64) {
        for (at, n) in table.meeting(moffset, len) {
            if let Some(e) = table.remove(at).filter(|e| !e.evicted) {
                for i in 0..n / RANGE {
                    self.free(at / RANGE + i, part(&e, i));
                }
            }
            self.record_held(at, n, false);
        }
        self.restored.notify_all();
    }

    /// Records again which pages its host holds of the ranges that map node `node`'s file
    /// written there: nothing to record, as every page of a range mapped to be written is
    /// held from its mapping on (the server's section is memory), and a write leaves it so.
    pub fn rerecord(&self, _node: u64, _offset: u64, _len: u64) {}

    /// Makes the ranges of the files of `nodes` what each file is now, where the window
    /// made them of a share's own file: called before the guest is told anything of their
    /// sizes, by the reply to a request that may tell it, so that what it may read of a
    /// file grown is the file's, and what one cut short has no more, zeros; and a mapping
    /// let go of for its guest's cut is made again, its waiting vCPUs told. A server's
    /// section, which reaches past every range asked of it, changes not.
    pub fn reconcile(&self, nodes: &[u64]) {
        let mut table = self.lock();
        let mut changed = false;
        for &node in nodes {
            let size = table
                .nodes
                .get(&node)
                .and_then(|n| n.file.as_ref())
                .and_then(file_size);
            for at in table.of(node) {
                let Some(e) = table.ranges.get(&at) else {
                    continue;
                };
                let (
                    Some(was),
                    Mapped {
                        len,
                        foffset,
                        writable,
                        ..
                    },
                ) = (e.file_to, e.mapped)
                else {
                    continue;
                };
                // A file whose size cannot be read is left as it was, but one let go of,
                // which is made zeros, as past its end.
                let to = size.map_or(0, |size| len.min(size.saturating_sub(foffset)));
                if !e.evicted && (size.is_none() || to == was) {
                    continue;
                }
                let (first, evicted) = (at / RANGE, e.evicted);
                if !evicted {
                    (0..len / RANGE).for_each(|i| self.free(first + i, part_to(was, i)));
                }
                let section = table
                    .nodes
                    .get(&node)
                    .and_then(|n| n.file.as_ref())
                    .filter(|_| to > 0)
                    .and_then(|f| section_of(f, writable));
                let to = if section.is_some() { to } else { 0 };
                let mut zeros = false;
                for i in 0..len / RANGE {
                    if !self.place(
                        first + i,
                        section.as_ref(),
                        foffset + i * RANGE,
                        part_to(to, i),
                        writable,
                    ) {
                        // What a placement that failed leaves is its placeholder: the mapping
                        // zeros, as past its file's end.
                        (0..i).for_each(|j| self.free(first + j, part_to(to, j)));
                        (0..len / RANGE).for_each(|j| {
                            let _ = self.place(first + j, None, 0, Part::Zeros, writable);
                        });
                        zeros = true;
                        break;
                    }
                }
                if let Some(e) = table.ranges.get_mut(&at) {
                    (e.file_to, e.zeros, e.evicted) = (Some(to), zeros, false);
                }
                changed = true;
            }
        }
        drop(table);
        if changed {
            self.changes.fetch_add(1, Ordering::AcqRel);
            self.restored.notify_all();
        }
    }

    /// Another process cut node `node`'s file short: its ranges made what it is now, as
    /// [`Window::reconcile`] makes them; on NTFS none does while a view of it is mapped.
    pub fn cut(&self, node: u64) {
        self.reconcile(&[node]);
    }

    /// Lets go of node `node`'s views before its guest cuts its file short, which NTFS
    /// refuses while a view of it is mapped (PM M412): each range of its file's mappings its
    /// placeholder again, until [`Window::reconcile`] makes it what the file is then; a vCPU
    /// that reaches one meanwhile waits ([`Window::memory_fault`]).
    pub fn evict(&self, node: u64) {
        let mut table = self.lock();
        let mut changed = false;
        for at in table.of(node) {
            let Some(e) = table.ranges.get_mut(&at) else {
                continue;
            };
            if e.evicted || e.zeros || e.file_to.is_none() {
                continue;
            }
            for i in 0..e.mapped.len / RANGE {
                self.free(at / RANGE + i, part(e, i));
            }
            e.evicted = true;
            changed = true;
        }
        drop(table);
        if changed {
            self.changes.fetch_add(1, Ordering::AcqRel);
        }
    }

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
        // A mapping let go of for its guest's cut: run again once it is made again.
        if table.evicted_at(at) {
            drop(
                self.restored
                    .wait_while(table, |t| t.evicted_at(at))
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            return true;
        }
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

/// `file`'s size as Windows has it now: none where it cannot be read.
fn file_size(file: &OwnedHandle) -> Option<u64> {
    let mut size = 0i64;
    // SAFETY: a file's handle the window holds, its size read into a local.
    let read = unsafe { GetFileSizeEx(file.as_raw_handle(), &mut size) };
    (read != 0).then(|| u64::try_from(size).ok()).flatten()
}

/// A section of `file` as long as it is now, to write too where `writable`: none where
/// Windows makes none (a file of no bytes, or one opened to read alone asked to write).
fn section_of(file: &OwnedHandle, writable: bool) -> Option<OwnedHandle> {
    // SAFETY: a file's handle the window holds; an unnamed section as long as the file
    // (its sizes 0).
    let made = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            protection(writable),
            0,
            0,
            std::ptr::null(),
        )
    };
    // SAFETY: the section just made, owned from here on.
    (!made.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(made) })
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
    /// A share's own file (D197's slice 4): its ranges its bytes, a range it ends within a
    /// view as long as them and zeros after, each way between the guest and the file; grown,
    /// its new bytes the guest's once the window follows it; refused a cut while a view of
    /// it is mapped (PM M412), and let go of first, a vCPU that faults on it meanwhile
    /// waiting until it is made what the file is, zeros past its new end; unmapped, its
    /// ranges whole placeholders again, which a server's section of a whole range fills.
    #[test]
    fn a_files_ranges_are_its_bytes_then_zeros_and_follow_it() {
        const SIZE: u64 = MIB + 100;
        let dir = shards_testdir::TempDir::new("whp-file").unwrap();
        let path = dir.join("f");
        let bytes: Vec<u8> = (0..SIZE as usize).map(pattern).collect();
        std::fs::write(&path, &bytes).unwrap();
        let open = || {
            OwnedHandle::from(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap(),
            )
        };
        let told = Arc::new(Told::default());
        let w = window(&told, 8 * MIB);
        assert_eq!(w.map(open(), 9, 0, 0, 4 * MIB, true), Ok(SIZE));
        for at in (0..SIZE).step_by(4096).chain([SIZE - 1]) {
            assert_eq!(read(&w, at), pattern(at as usize), "{at:#x}");
        }
        for at in [SIZE, SIZE + 4096, 2 * MIB - 1, 2 * MIB, 4 * MIB - 1] {
            assert_eq!(read(&w, at), 0, "zeros past its end, {at:#x}");
        }
        write(&w, 10, 0xa5);
        assert_eq!(
            std::fs::read(&path).unwrap()[10],
            0xa5,
            "the guest's store is the file's"
        );
        // Grown past its range: the guest's once the window follows it.
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::os::windows::fs::FileExt::seek_write(&file, &[0x5a; 300], 2 * MIB).unwrap();
        // Past the page that held its end, which its view held whole already.
        std::os::windows::fs::FileExt::seek_write(&file, &[0x33; 8192], MIB + 4096).unwrap();
        assert_eq!(read(&w, MIB + 8192), 0, "not yet followed");
        w.reconcile(&[9]);
        assert_eq!((read(&w, MIB + 8192), read(&w, 2 * MIB + 299)), (0x33, 0x5a));
        assert_eq!(read(&w, 2 * MIB + 300), 0);
        assert_eq!(read(&w, 3 * MIB), 0);
        // Cut short: refused while a view of it is mapped; let go of first, it is cut, and a
        // vCPU that reached it meanwhile runs again once it is made what the file is.
        assert!(
            file.set_len(4096).is_err(),
            "NTFS cuts no file a view of which is mapped"
        );
        w.evict(9);
        std::thread::scope(|scope| {
            let (done, ran) = std::sync::mpsc::channel();
            let w = &w;
            scope.spawn(move || done.send(w.memory_fault(Some(GPA + 8192))).unwrap());
            assert!(
                ran.recv_timeout(std::time::Duration::from_millis(200)).is_err(),
                "waits while its mapping is let go of"
            );
            file.set_len(4096).unwrap();
            w.reconcile(&[9]);
            assert!(ran.recv().unwrap(), "run again");
        });
        assert_eq!(
            (read(&w, 4095), read(&w, 4096), read(&w, 2 * MIB)),
            (pattern(4095), 0, 0)
        );
        assert_eq!(told.0.lock().unwrap().len(), 2, "stage 2 stands throughout");
        // Unmapped: whole placeholders again, the file let go of.
        w.unmap(0, 4 * MIB).unwrap();
        assert!(!w.maps(9));
        file.set_len(0).unwrap();
        let whole = Section::new((2 * MIB) as usize, |_| 0x77);
        assert_eq!(w.map(whole.give(), 7, 0, 0, 2 * MIB, false), Ok(2 * MIB));
        assert_eq!(read(&w, 2 * MIB - 1), 0x77);
    }

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
