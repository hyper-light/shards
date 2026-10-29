//! Guest physical memory.
//!
//! Guest RAM is shared. The guest's vCPUs write it while they run, the host's kernel reads
//! and writes it in system calls given guest addresses, and the VMM's own threads (vCPU
//! threads handling exits, device workers, the snapshot coordinator) read and write it for
//! the devices. Rust makes a race between two of its own threads undefined unless both
//! accesses are atomic and of one size, and a volatile access counts as non-atomic
//! (std::sync::atomic, "Memory model for atomic accesses"; std::ptr::read_volatile). So:
//!
//! - The VMM's threads reach guest memory through an [`Access`], which one of them holds
//!   at a time. Their accesses are ordered, never racing, whatever addresses a guest gives
//!   its devices, overlapping or not (audit A01).
//! - Within an access, reads and writes are volatile, or `asm!` the compiler cannot see
//!   into ([`platform::copy_in`]), since the guest and the kernel change guest memory under
//!   them, and the virtqueue indices that order the host against the guest are atomic
//!   ([`Access::load_u16`], [`Access::store_u16`]).
//! - Nothing forms a Rust reference or slice into guest memory.
//! - Bulk data goes by system calls given guest addresses ([`GuestMemory::host_ptr`]),
//!   without an access held, as the guest's own accesses go: the kernel, like the guest,
//!   is outside Rust's abstract machine.
//! - Device memory, a pmem device's file or a DAX window, which a guest maps its files from
//!   and names to its devices as any of its pages (D189), is a file's mapping whose pages
//!   may be gone: system calls reach it ([`GuestMemory::io_ptr`]), whose kernel answers a
//!   fault EFAULT, and an access's copies by [`platform::copy_checked`], which a fault
//!   fails, not the VMM's process.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::platform;
use crate::sync::lock;

/// `len` bytes of guest memory from `gpa`, which a file fills from `offset`: a piece of a
/// kernel or an initrd as a boot plans its copy ([`GuestMemory::read_file`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSpan {
    pub gpa: u64,
    pub len: usize,
    pub offset: u64,
}

/// A guest-physical range that is not backed by guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfBounds {
    pub gpa: u64,
    pub len: u64,
}

impl fmt::Display for OutOfBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "guest range {:#x}+{:#x} is not guest RAM", self.gpa, self.len)
    }
}

impl std::error::Error for OutOfBounds {}

/// An [`Access`] asked for by the thread that holds one already, which would wait for
/// itself forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reentered;

impl fmt::Display for Reentered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("guest memory accessed again by the thread that holds it")
    }
}

impl std::error::Error for Reentered {}

/// Plain data: every bit pattern is a valid value and there is no padding.
///
/// # Safety
/// Implementors must be `repr(C)`/primitive types satisfying the above.
pub unsafe trait Pod: Copy + 'static {}

// SAFETY: primitive integers accept every bit pattern and have no padding.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u16 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}

/// The width of the volatile reads that look for a zero page.
const WORD: usize = size_of::<u64>();

/// The blocks a snapshot's memory file says it holds data in ([`GuestMemory::save`]): the
/// least a guest reports free, Linux's pageblock of 4 KiB pages (D151), so that a restore
/// maps from its template's file only the blocks its template's guest used, and gives the
/// rest memory of its own, zeros, which a block its guest reports free gives back whole
/// (Windows: [`platform::map_file_views`]).
pub const DATA_BLOCK: usize = 2 << 20;

#[derive(Debug)]
struct Region {
    gpa: u64,
    len: usize,
    host: NonNull<u8>,
    /// Where it is a file's, copy-on-write ([`GuestMemory::from_file`]): what gives its
    /// pages back to the file's ([`platform::FileViews::give_back`]).
    views: Option<platform::FileViews>,
}

impl Region {
    fn end(&self) -> u64 {
        self.gpa + self.len as u64 // cannot wrap: checked on creation
    }
}

/// Memory the guest has only where its virtio-mem device has plugged it (D133): reserved
/// whole, as RAM is, and withheld, its pages given back and the range inaccessible, but for
/// the blocks plugged.
#[derive(Debug)]
struct Hotplug {
    gpa: u64,
    len: usize,
    host: NonNull<u8>,
    block: u64,
    /// A bit for each block, set while it is plugged: what [`GuestMemory::host_ptr`] lets
    /// the host's devices reach.
    plugged: Box<[AtomicU64]>,
    /// A bit for each block that has been plugged, never cleared: the blocks whose pages
    /// the host may hold, asked about by [`GuestMemory::hotplug_held`] (D148).
    ever: Box<[AtomicU64]>,
    /// Whether withholding an unplugged block's pages has failed and been said: once, as a
    /// guest unplugging over and over could otherwise fill the host's log.
    said_kept: AtomicBool,
}

impl Hotplug {
    fn end(&self) -> u64 {
        self.gpa + self.len as u64 // cannot wrap: checked on creation
    }

    /// The blocks `gpa..gpa+len` covers, where it lies within the region.
    fn blocks(&self, gpa: u64, len: u64) -> Option<Range<u64>> {
        let end = gpa.checked_add(len)?;
        if gpa < self.gpa || end > self.end() {
            return None;
        }
        let first = (gpa - self.gpa) / self.block;
        let last = (end.max(gpa + 1) - 1 - self.gpa) / self.block;
        Some(first..last + 1)
    }

    fn is_plugged(&self, block: u64) -> bool {
        let word = usize::try_from(block / 64).ok().and_then(|w| self.plugged.get(w));
        word.is_some_and(|w| w.load(Ordering::Acquire) & (1 << (block % 64)) != 0)
    }

    fn set(&self, blocks: Range<u64>, plugged: bool) {
        for block in blocks {
            let word = usize::try_from(block / 64).ok();
            let bit = 1 << (block % 64);
            if let Some(w) = word.and_then(|w| self.plugged.get(w)) {
                if plugged {
                    w.fetch_or(bit, Ordering::Release);
                } else {
                    w.fetch_and(!bit, Ordering::Release);
                }
            }
            if plugged && let Some(w) = word.and_then(|w| self.ever.get(w)) {
                w.fetch_or(bit, Ordering::Release);
            }
        }
    }

    /// Each run of the blocks whose bits are set in `bits`, `(gpa, len)`, in order.
    fn runs(&self, bits: &[AtomicU64]) -> Vec<(u64, u64)> {
        let set = |block: u64| {
            let word = usize::try_from(block / 64).ok().and_then(|w| bits.get(w));
            word.is_some_and(|w| w.load(Ordering::Acquire) & (1 << (block % 64)) != 0)
        };
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for block in (0..self.len as u64 / self.block).filter(|&b| set(b)) {
            let gpa = self.gpa + block * self.block;
            match runs.last_mut() {
                Some((start, len)) if *start + *len == gpa => *len += self.block,
                _ => runs.push((gpa, self.block)),
            }
        }
        runs
    }

    /// The host's address of `gpa`, within the region.
    fn host(&self, gpa: u64) -> *mut u8 {
        // SAFETY: callers pass an address within the region (`blocks` checked it).
        unsafe { self.host.as_ptr().add((gpa - self.gpa) as usize) }
    }
}

/// Whole blocks of the memory to plug ([`Access::whole_blocks`]): which, and where the
/// host has them.
struct Blocks<'a> {
    h: &'a Hotplug,
    blocks: Range<u64>,
    at: NonNull<u8>,
    size: usize,
}

/// Guest-physical ranges a device's DMA holds pinned through the host's IOMMU: what a
/// function passed through behind the guest's own IOMMU has mapped (D154). The host's
/// IOMMU keeps the pages it pinned, so a page given back to the host and replaced (a
/// report of free memory, an unplug) would leave the device the old one: a range a pin
/// meets is kept. Each pinned range by its first address, as many lengths as are pinned
/// there; the longest ever, so that a look for those meeting a range starts no further
/// back.
#[derive(Debug, Default)]
struct Pins {
    ranges: BTreeMap<u64, Vec<u64>>,
    longest: u64,
}

impl Pins {
    fn meets(&self, gpa: u64, len: u64) -> bool {
        let end = gpa.saturating_add(len);
        self.ranges
            .range(gpa.saturating_sub(self.longest)..end)
            .any(|(&start, lens)| lens.iter().any(|&n| start.saturating_add(n) > gpa))
    }
}

/// What the host held of the RAM a guest gave back while a working set was recorded from
/// what its host maps ([`GuestMemory::keep_given_back`]), which a page given back leaves:
/// for each host page of each region, two bits, whether the host held it and whether as a
/// private copy, written.
#[derive(Debug)]
pub struct GivenBack {
    /// Each region's, as [`GuestMemory::regions`] gives them: its pages held, its pages
    /// copied.
    regions: Vec<(Vec<u64>, Vec<u64>)>,
    /// The first range given back whose pages the host could not say: the recording lost
    /// them.
    pub unseen: Option<String>,
}

impl GivenBack {
    /// The pages of region `region` the host held as the guest gave them back, each as its
    /// index and whether it was a private copy.
    pub fn pages(&self, region: usize) -> Vec<(usize, bool)> {
        let Some((held, copied)) = self.regions.get(region) else {
            return Vec::new();
        };
        let mut pages = Vec::new();
        for (w, (&held, &copied)) in held.iter().zip(copied).enumerate() {
            let mut left = held;
            while left != 0 {
                let bit = left.trailing_zeros() as usize;
                left &= left - 1;
                pages.push((w * 64 + bit, copied & (1 << bit) != 0));
            }
        }
        pages
    }

    /// Keeps the pages the host maps of `len` bytes at `host`, from page `first` of region
    /// `region`, as the guest gives them back.
    fn keep(&mut self, region: usize, first: usize, host: *const u8, len: usize) {
        #[cfg(any(target_os = "linux", windows))]
        let mapped = platform::mapped_pages(host, len);
        #[cfg(not(any(target_os = "linux", windows)))]
        let mapped: io::Result<Vec<(usize, bool)>> = {
            let _ = (host, len);
            Err(io::Error::new(io::ErrorKind::Unsupported, "no pagemap here"))
        };
        let (mapped, Some((held, copied))) = (mapped, self.regions.get_mut(region)) else {
            return;
        };
        let mapped = match mapped {
            Ok(mapped) => mapped,
            Err(e) => {
                self.unseen.get_or_insert_with(|| {
                    format!("{len:#x} bytes of region {region} from its page {first}: {e}")
                });
                return;
            }
        };
        for (i, private) in mapped {
            let page = first + i;
            let (word, bit) = (page / 64, 1u64 << (page % 64));
            if let (Some(h), Some(c)) = (held.get_mut(word), copied.get_mut(word)) {
                *h |= bit;
                if private {
                    *c |= bit;
                }
            }
        }
    }
}

/// Guest RAM regions, each backed by its own host reservation, and the memory a virtio-mem
/// device plugs, where the guest has one.
#[derive(Debug)]
pub struct GuestMemory {
    regions: Vec<Region>,
    hotplug: Option<Hotplug>,
    /// Mapped from a file ([`from_file`](Self::from_file)): a page it has not touched holds
    /// the file's bytes, not zeros.
    file_backed: bool,
    /// The bytes the guest has reported free, given back or not (D151).
    reported: AtomicU64,
    /// While a working set is recorded: what the host held of the RAM the guest gave back
    /// since ([`keep_given_back`](Self::keep_given_back)).
    given_back: Mutex<Option<GivenBack>>,
    /// What a device's DMA holds pinned ([`GuestMemory::pin`]).
    pins: Mutex<Pins>,
    /// Device memory its devices reach as they reach RAM ([`give_devices`](Self::give_devices)):
    /// its devices' own mappings, which this memory never releases.
    devices: OnceLock<Box<[Region]>>,
    /// Held by the thread with the [`Access`].
    host: Mutex<()>,
    /// That thread's [`token`], or 0.
    holder: AtomicUsize,
}

// SAFETY: the reservations are memory this value owns. The host reaches it only through an
// `Access`, which one thread holds at a time, or through raw pointers handed to the
// kernel and the hypervisor; volatile and atomic accesses tolerate the guest's and the
// kernel's concurrent ones.
unsafe impl Send for GuestMemory {}
// SAFETY: as above.
unsafe impl Sync for GuestMemory {}

thread_local! {
    /// Its address tells this thread from every other live thread.
    static TOKEN: u8 = const { 0 };
}

#[cfg(test)]
thread_local! {
    /// How many times this thread has taken guest memory: how a test counts a device's
    /// takes of it.
    pub static TAKES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A nonzero number no other live thread has.
fn token() -> usize {
    TOKEN.with(|t| std::ptr::from_ref(t).addr())
}

impl GuestMemory {
    /// Reserves zero-filled memory for each `(gpa, len)` range. Pages are materialized on
    /// first touch, so untouched guest RAM costs no host memory.
    pub fn anonymous(ranges: &[(u64, usize)]) -> io::Result<GuestMemory> {
        let page = platform::page_size()?;
        let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidInput, msg);
        // Built incrementally so that Drop unmaps whatever was mapped if a later range fails.
        let mut mem = GuestMemory {
            regions: Vec::with_capacity(ranges.len()),
            hotplug: None,
            file_backed: false,
            host: Mutex::new(()),
            reported: AtomicU64::new(0),
            given_back: Mutex::new(None),
            pins: Mutex::new(Pins::default()),
            devices: OnceLock::new(),
            holder: AtomicUsize::new(0),
        };
        for &(gpa, len) in ranges {
            if len == 0 || !len.is_multiple_of(page) || !gpa.is_multiple_of(page as u64) {
                return Err(invalid(format!(
                    "guest RAM {gpa:#x}+{len:#x} is not host-page ({page:#x}) aligned"
                )));
            }
            let end = gpa
                .checked_add(len as u64)
                .ok_or_else(|| invalid(format!("guest RAM {gpa:#x}+{len:#x} wraps")))?;
            if mem.regions.iter().any(|r| gpa < r.end() && r.gpa < end) {
                return Err(invalid("overlapping guest RAM regions".into()));
            }
            // Ownership moves into `mem`, whose Drop releases it.
            let host = platform::reserve_ram(len)?;
            mem.regions.push(Region {
                gpa,
                len,
                host,
                views: None,
            });
        }
        mem.regions.sort_by_key(|r| r.gpa);
        Ok(mem)
    }

    /// Guest RAM of `ranges`, as [`anonymous`](Self::anonymous) makes it, with memory to
    /// plug ([`with_plug_region`](Self::with_plug_region)).
    pub fn with_hotplug(
        ranges: &[(u64, usize)],
        gpa: u64,
        len: usize,
        block: u64,
    ) -> io::Result<GuestMemory> {
        GuestMemory::anonymous(ranges)?.with_plug_region(gpa, len, block)
    }

    /// This memory, and `len` bytes at `gpa` that are the guest's only as its virtio-mem
    /// device plugs them, in blocks of `block` bytes (D133): reserved now, and withheld
    /// until plugged ([`Access::plug`]). A booted guest's RAM, or a restored one's (D148).
    pub fn with_plug_region(mut self, gpa: u64, len: usize, block: u64) -> io::Result<GuestMemory> {
        let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidInput, msg);
        let page = platform::page_size()? as u64;
        if !block.is_power_of_two() || !block.is_multiple_of(page) {
            return Err(invalid(format!(
                "a block of {block:#x} bytes is no power of two of host pages ({page:#x})"
            )));
        }
        let end = gpa
            .checked_add(len as u64)
            .ok_or_else(|| invalid(format!("memory to plug at {gpa:#x}+{len:#x} wraps")))?;
        if len == 0 || !gpa.is_multiple_of(block) || !(len as u64).is_multiple_of(block) {
            return Err(invalid(format!(
                "memory to plug at {gpa:#x}+{len:#x} is not in whole blocks of {block:#x}"
            )));
        }
        if self.hotplug.is_some() {
            return Err(invalid("the memory has memory to plug already".into()));
        }
        if self.regions.iter().any(|r| gpa < r.end() && r.gpa < end) {
            return Err(invalid("memory to plug overlaps guest RAM".into()));
        }
        let host = platform::reserve_withheld(len)?;
        let words = (len as u64 / block).div_ceil(64);
        // Ownership moves into `self`, whose Drop releases it.
        self.hotplug = Some(Hotplug {
            gpa,
            len,
            host,
            block,
            plugged: (0..words).map(|_| AtomicU64::new(0)).collect(),
            ever: (0..words).map(|_| AtomicU64::new(0)).collect(),
            said_kept: AtomicBool::new(false),
        });
        Ok(self)
    }

    /// `(gpa, host pointer, len)` of the memory a virtio-mem device plugs, if the guest has
    /// any: for a hypervisor whose stage 2 follows the host's mappings, mapped whole.
    pub fn hotplug(&self) -> Option<(u64, *mut u8, usize)> {
        self.hotplug.as_ref().map(|h| (h.gpa, h.host.as_ptr(), h.len))
    }

    /// Of the memory the guest gave back, the bytes whose pages the host holds still: of
    /// each block plugged once and unplugged since, its pages touched, by the guest or a
    /// device, and not given back ([`platform::untouched`]). Unplugging gives them back
    /// (`platform::withhold`), where a hypervisor's unmapping of them took them out of the
    /// process's footprint and resident set but left them its own (PM M193). A block plugged
    /// now is the guest's, its pages held as they should be, and none of what it gave back:
    /// a guest that ends asking for memory has its driver plugging as it ends. None where
    /// the guest has no memory to plug, or the host cannot say.
    pub fn hotplug_held(&self) -> io::Result<Option<u64>> {
        let Some(h) = &self.hotplug else {
            return Ok(None);
        };
        let page = platform::page_size()?;
        // The blocks plugged once and not now alone are asked about, where the pages a block
        // given back still held would show: asking of all of a region the size of the
        // host's memory, a run's (D148), took 7.7 ms a run where none of it was ever plugged.
        let given_back: Vec<AtomicU64> = h
            .ever
            .iter()
            .zip(h.plugged.iter())
            .map(|(ever, now)| AtomicU64::new(ever.load(Ordering::Acquire) & !now.load(Ordering::Acquire)))
            .collect();
        let mut held = 0u64;
        for (gpa, len) in h.runs(&given_back) {
            let (Some(offset), Ok(len)) = (
                gpa.checked_sub(h.gpa).and_then(|o| usize::try_from(o).ok()),
                usize::try_from(len),
            ) else {
                return Ok(None);
            };
            if offset.checked_add(len).is_none_or(|end| end > h.len) {
                return Ok(None);
            }
            // SAFETY: a range within the reservation, which this value owns.
            let untouched = unsafe { platform::untouched(h.host.as_ptr().add(offset), len, page) }?;
            match untouched {
                Some(u) => held += u.iter().filter(|&&never| !never).count() as u64 * page as u64,
                None => return Ok(None),
            }
        }
        Ok(Some(held))
    }

    /// How many of the blocks `gpa..gpa+len` covers are plugged, and how many it covers;
    /// none where it is not within the memory to plug.
    pub fn plugged(&self, gpa: u64, len: u64) -> Option<(u64, u64)> {
        let h = self.hotplug.as_ref()?;
        let blocks = h.blocks(gpa, len)?;
        let total = blocks.end - blocks.start;
        Some((blocks.filter(|&b| h.is_plugged(b)).count() as u64, total))
    }

    /// Each run of plugged blocks, `(gpa, len)`, in order.
    pub fn plugged_runs(&self) -> Vec<(u64, u64)> {
        self.hotplug
            .as_ref()
            .map_or_else(Vec::new, |h| h.runs(&h.plugged))
    }

    /// Guest RAM backed copy-on-write by `file`, which holds each region in turn by guest
    /// address, as [`save`](Self::save) writes them, whatever order `ranges` lists them in
    /// (audit A21). Clones share every page none of them writes. On the host's base pages
    /// ([`platform::small_pages`]): the file's pages are the page cache's folios, up to 2 MiB,
    /// which a mapping without the advice maps whole at a fault, 14 MiB of a restored test
    /// guest's where Firecracker's mapped none, and a lone restore held 19.4 MiB of the file
    /// where on base pages it holds 9.0; its first beat comes 1.8 ms later, and a pooled
    /// run, whose working set is prefetched before its request, takes as long (PM M287).
    /// Where the platform cannot map a file into reserved memory, the file is read in.
    /// `data`, the runs of the file's RAM that hold data as [`save`](Self::save) said them,
    /// `(offset, len)`: where the platform maps those alone from the file, the rest is
    /// memory of the guest's own, zeros, as the file holds there (Windows,
    /// [`platform::map_file_views`]); none says all of it may.
    ///
    /// The file must hold all of them, and must not change while the VM runs: pages the
    /// guest has not yet touched come from it.
    pub fn from_file(
        ranges: &[(u64, usize)],
        file: &File,
        data: Option<&[(u64, u64)]>,
    ) -> io::Result<GuestMemory> {
        let mut mem = GuestMemory::anonymous(ranges)?;
        let total = mem.regions.iter().map(|r| r.len as u64).sum::<u64>();
        let have = file.metadata()?.len();
        if have < total {
            // A mapping past the file's end would fault the guest's access with SIGBUS.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a memory file of {have} bytes for {total} bytes of guest RAM"),
            ));
        }
        let mut offset = 0u64;
        for i in 0..mem.regions.len() {
            let Some((gpa, len, host)) = mem.regions.get(i).map(|r| (r.gpa, r.len, r.host)) else {
                break;
            };
            let runs = region_data(data, offset, len);
            // SAFETY: a whole region we just reserved: page-aligned, unmapped by any
            // hypervisor, unreferenced; region offsets in the file are page-aligned sums.
            match unsafe { platform::map_file_views(file, offset, len, host, &runs) } {
                Ok(views) => {
                    platform::small_pages(host, len);
                    if let Some(r) = mem.regions.get_mut(i) {
                        r.views = Some(views);
                    }
                    mem.file_backed = true;
                }
                // Its data read in, the rest zeros already.
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                    for run in &runs {
                        mem.read_file(gpa + run.start as u64, run.len(), file, offset + run.start as u64)?;
                    }
                }
                Err(e) => return Err(e),
            }
            offset += len as u64;
        }
        Ok(mem)
    }

    /// Exclusive access to guest memory among the host's threads, until it is dropped.
    /// The thread that holds one already gets [`Reentered`] instead of waiting for itself.
    pub fn access(&self) -> Result<Access<'_>, Reentered> {
        let me = token();
        // Only this thread stores its own token, so this sees whether it holds `host`.
        if self.holder.load(Ordering::Relaxed) == me {
            return Err(Reentered);
        }
        let held = lock(&self.host);
        self.holder.store(me, Ordering::Relaxed);
        #[cfg(test)]
        TAKES.with(|t| t.set(t.get() + 1));
        Ok(Access {
            mem: self,
            _held: held,
        })
    }

    /// Fills `len` bytes of guest memory at `gpa` from `file` at `offset`, by system calls
    /// given the guest address, as the block device transfers. Fails on a short file.
    pub fn read_file(&self, gpa: u64, len: usize, file: &File, offset: u64) -> io::Result<()> {
        let dst = self
            .host_ptr(gpa, len)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut done = 0;
        while done < len {
            let at = offset
                .checked_add(done as u64)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: `dst + done .. dst + len` is guest memory (`host_ptr` checked it all);
            // the kernel writes it.
            let n = unsafe { platform::read_at(file, dst.add(done), len - done, at)? };
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }

    /// Writes every region in turn, by guest address, to `file`, leaving all-zero pages as
    /// holes so the file stays as small as the memory the guest used. The file is emptied
    /// first, so no page keeps bytes it held before (audit A22); it must not be one that
    /// backs guest memory. Each run of pages the guest used is one write: a write's length
    /// sets the order of the page-cache folios it fills (Linux 6.17, ext4
    /// `write_begin_get_folio`), and a restore maps those folios. It holds an [`Access`]
    /// throughout, so no host thread writes meanwhile; a snapshot's is the memory of a
    /// machine whose vCPUs and devices are paused. After the RAM, the memory a virtio-mem
    /// device has plugged, run by run in the order of their addresses (D176): a restore
    /// plugs the same runs and reads them back ([`restore_plugged`](Self::restore_plugged)).
    /// The runs of the file's RAM that hold data, `(offset, len)` in order, each of whole
    /// [`DATA_BLOCK`]s but at a region's end: what a restore maps from the file
    /// ([`from_file`](Self::from_file)).
    pub fn save(&self, file: &File) -> io::Result<Vec<(u64, u64)>> {
        let page = platform::page_size()?;
        let _held = self.access().map_err(io::Error::other)?;
        file.set_len(0)?;
        // Its holes holes on every host (NTFS keeps none it is not told to).
        platform::sparse(file)?;
        let mut offset = 0u64;
        let mut data = Vec::new();
        for r in &self.regions {
            // Pages of anonymous RAM the guest never touched hold zeros, and reading them
            // would make the host give each one a page (audit D01): the OS says which, where
            // it can. Not so for a file's pages, which hold the file's bytes.
            let untouched = if self.file_backed {
                None
            } else {
                // SAFETY: the whole region, which this value owns.
                unsafe { platform::untouched(r.host.as_ptr(), r.len, page) }?
            };
            let used = write_used(file, r, untouched.as_deref(), offset, page)?;
            data.extend(
                blocks_of(&used, r.len)
                    .into_iter()
                    .map(|b| (offset + b.start as u64, b.len() as u64)),
            );
            offset += r.len as u64;
        }
        if let Some(h) = &self.hotplug {
            for (gpa, len) in h.runs(&h.plugged) {
                let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                let host = NonNull::new(h.host(gpa)).ok_or(io::ErrorKind::InvalidData)?;
                // A view of the run, which owns nothing: the reservation is the region's.
                let run = Region {
                    gpa,
                    len,
                    host,
                    views: None,
                };
                // SAFETY: whole plugged blocks of the reservation, which the host was granted.
                let untouched = unsafe { platform::untouched(host.as_ptr(), len, page) }?;
                write_used(file, &run, untouched.as_deref(), offset, page)?;
                offset += len as u64;
            }
        }
        file.set_len(offset)?;
        Ok(data)
    }

    /// Plugs `runs` again, the memory a snapshot's machine had plugged, and reads each back
    /// from `file`, which holds them in turn after the RAM ([`save`](Self::save)); `map`
    /// gives each to the guest, as a plug does ([`Access::plug`]). Before any vCPU runs:
    /// a guest resumed without what it held there would find zeros in its place.
    pub fn restore_plugged(
        &self,
        runs: &[(u64, u64)],
        file: &File,
        mut map: impl FnMut(*mut u8, u64, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut offset: u64 = self.regions.iter().map(|r| r.len as u64).sum();
        let access = self.access().map_err(|e| e.to_string())?;
        for &(gpa, len) in runs {
            access.plug(gpa, len, &mut map)?;
            let size = usize::try_from(len).map_err(|e| e.to_string())?;
            self.read_file(gpa, size, file, offset)
                .map_err(|e| format!("the memory plugged at {gpa:#x}+{len:#x}: {e}"))?;
            offset = offset
                .checked_add(len)
                .ok_or("memory plugged past what a file can hold")?;
        }
        Ok(())
    }

    /// `(gpa, host pointer, len)` for each region of RAM, for stage-2 mapping: not the
    /// memory to plug ([`hotplug`](Self::hotplug)).
    pub fn regions(&self) -> impl Iterator<Item = (u64, *mut u8, usize)> + '_ {
        self.regions.iter().map(|r| (r.gpa, r.host.as_ptr(), r.len))
    }

    /// Host address of `gpa..gpa+len`, which must lie within one region, or within the
    /// memory to plug and its blocks all plugged: for the kernel and the hypervisor, and
    /// for an [`Access`]. A device the guest points at memory it has not plugged, or has
    /// unplugged, reaches nothing there, as its own accesses do not (VIRTIO 1.3 §5.15.6.1).
    pub fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8, OutOfBounds> {
        let oob = OutOfBounds { gpa, len: len as u64 };
        let end = gpa.checked_add(len as u64).ok_or(oob)?;
        if let Some(r) = self.regions.iter().find(|r| gpa >= r.gpa && end <= r.end()) {
            // SAFETY: offset is within the region's mapping (checked above).
            return Ok(unsafe { r.host.as_ptr().add((gpa - r.gpa) as usize) });
        }
        let h = self.hotplug.as_ref().ok_or(oob)?;
        let mut blocks = h.blocks(gpa, len as u64).ok_or(oob)?;
        if !blocks.all(|b| h.is_plugged(b)) {
            return Err(oob);
        }
        Ok(h.host(gpa))
    }

    /// Has the host's devices reach `regions` as they reach RAM, each `(gpa, host, len)`
    /// of device memory (D189): memory a device gives its guest that is not RAM, a pmem
    /// device's file or a DAX window, which the guest maps its files from and so names to
    /// its other devices as any of its pages, as a write from a mapped file's pages does.
    /// Once, before the guest runs: a region that overlaps RAM, the memory to plug or
    /// another is refused, as is a second call.
    ///
    /// # Safety
    /// Each region is host memory that stays mapped while a device may reach this memory:
    /// until its machine's devices are quiet for good (`Running::teardown`), or it drops.
    pub unsafe fn give_devices(
        &self,
        regions: impl IntoIterator<Item = (u64, *mut u8, usize)>,
    ) -> Result<(), String> {
        let mut given: Vec<Region> = Vec::new();
        for (gpa, host, len) in regions {
            let named = || format!("device memory {gpa:#x}+{len:#x}");
            let end = gpa
                .checked_add(len as u64)
                .filter(|_| len > 0)
                .ok_or_else(named)?;
            let host = NonNull::new(host).ok_or_else(|| format!("{} at no host address", named()))?;
            let meets = |at: u64, to: u64| gpa < to && at < end;
            if self.regions.iter().any(|r| meets(r.gpa, r.end()))
                || self.hotplug.as_ref().is_some_and(|h| meets(h.gpa, h.end()))
                || given.iter().any(|r| meets(r.gpa, r.end()))
            {
                return Err(format!("{} overlaps other guest memory", named()));
            }
            given.push(Region {
                gpa,
                len,
                host,
                views: None,
            });
        }
        self.devices
            .set(given.into_boxed_slice())
            .map_err(|_| "its device memory was given already".to_string())
    }

    /// Host address of `gpa..gpa+len` for a system call: within RAM or the memory plugged,
    /// as [`host_ptr`](Self::host_ptr) gives it, or within one region of device memory
    /// ([`give_devices`](Self::give_devices)). Device memory is a file's mapping, whose pages
    /// may be gone (its file cut short by another process, a page its disk could not give
    /// back): the host's kernel answers a system call's access there with EFAULT, where an
    /// access of the VMM's own would end its process. So system calls alone are given it,
    /// and an [`Access`] copies it by [`platform::copy_checked`].
    pub fn io_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8, OutOfBounds> {
        self.host_ptr(gpa, len)
            .or_else(|oob| self.device_ptr(gpa, len).ok_or(oob))
    }

    /// Host address of `gpa..gpa+len` within one region of device memory.
    fn device_ptr(&self, gpa: u64, len: usize) -> Option<*mut u8> {
        let end = gpa.checked_add(len as u64)?;
        let r = self
            .devices
            .get()?
            .iter()
            .find(|r| gpa >= r.gpa && end <= r.end())?;
        // SAFETY: offset is within the region's mapping (found above).
        Some(unsafe { r.host.as_ptr().add((gpa - r.gpa) as usize) })
    }

    /// Pins `gpa..gpa+len` for a device's DMA, as the host's IOMMU has mapped it ([`Pins`]),
    /// until [`unpin`](Self::unpin)ned as many times.
    pub fn pin(&self, gpa: u64, len: u64) {
        let mut pins = lock(&self.pins);
        pins.ranges.entry(gpa).or_default().push(len);
        pins.longest = pins.longest.max(len);
    }

    /// One pin of `gpa..gpa+len` let go.
    pub fn unpin(&self, gpa: u64, len: u64) {
        let mut pins = lock(&self.pins);
        if let Some(lens) = pins.ranges.get_mut(&gpa) {
            if let Some(i) = lens.iter().position(|&n| n == len) {
                lens.swap_remove(i);
            }
            if lens.is_empty() {
                pins.ranges.remove(&gpa);
            }
        }
    }

    /// Whether a pin meets `gpa..gpa+len`.
    fn pinned(&self, gpa: u64, len: u64) -> bool {
        lock(&self.pins).meets(gpa, len)
    }

    /// The bytes the guest has reported free since this memory was made, given back or
    /// not ([`Access::discard`], D151).
    pub fn reported(&self) -> u64 {
        self.reported.load(Ordering::Relaxed)
    }

    /// Keeps, from now until [`given_back`](Self::given_back), which pages of RAM the host
    /// held as the guest gave them back ([`Access::discard`]): for a working set recorded
    /// from the pages its host maps when it ends (`vm::recorded`), which a page given back
    /// in between leaves, and which then left out of one what its run's guest had given
    /// back of its reads' page cache as its host ran short (PM M320).
    pub fn keep_given_back(&self) -> Result<(), String> {
        let page = platform::page_size().map_err(|e| e.to_string())?;
        let regions = self
            .regions
            .iter()
            .map(|r| {
                let words = (r.len / page).div_ceil(64);
                (vec![0; words], vec![0; words])
            })
            .collect();
        *lock(&self.given_back) = Some(GivenBack {
            regions,
            unseen: None,
        });
        Ok(())
    }

    /// What was kept since [`keep_given_back`](Self::keep_given_back), and keeps no more.
    pub fn given_back(&self) -> Option<GivenBack> {
        lock(&self.given_back).take()
    }

    /// Keeps `gpa..gpa+len`, within one region, on the host's base pages
    /// ([`platform::small_pages`]): guest RAM the guest touches sparsely. Before it does.
    pub fn small_pages(&self, gpa: u64, len: usize) -> Result<(), OutOfBounds> {
        let at = self.host_ptr(gpa, len)?;
        if let Some(at) = std::ptr::NonNull::new(at) {
            platform::small_pages(at, len);
        }
        Ok(())
    }

    /// Puts `gpa..gpa+len`, within one region, on huge pages as the guest next touches it
    /// ([`platform::huge_pages`]): undoes [`small_pages`](Self::small_pages).
    pub fn huge_pages(&self, gpa: u64, len: usize) -> Result<(), OutOfBounds> {
        let at = self.host_ptr(gpa, len)?;
        if let Some(at) = std::ptr::NonNull::new(at) {
            platform::huge_pages(at, len);
        }
        Ok(())
    }

    /// Puts its RAM on huge pages for what its guest goes on to touch, once its boot is
    /// done, where the boot kept it on small pages (D29, PM M279): each huge page's span
    /// the guest has not touched at all, or has touched whole; a span it touched in part
    /// stays on small pages, where a huge page would hold the rest resident for nothing,
    /// and the host's khugepaged would make one of it. Advice only, with the spans the
    /// host says were touched (`platform::untouched`); an error where it cannot say.
    pub fn huge_pages_past_boot(&self) -> io::Result<()> {
        let Some(huge) = platform::advised_huge_page() else {
            return Ok(());
        };
        let page = platform::page_size()?;
        for r in &self.regions {
            // SAFETY: a region this memory owns, page-aligned.
            let Some(untouched) = (unsafe { platform::untouched(r.host.as_ptr(), r.len, page) })? else {
                return Ok(());
            };
            for (first, pages) in huge_runs(&untouched, huge / page) {
                if let Some(at) = std::ptr::NonNull::new(r.host.as_ptr().wrapping_add(first * page)) {
                    platform::huge_pages(at, pages * page);
                }
            }
        }
        Ok(())
    }
}

/// The runs of pages, `(first, count)`, to put on huge pages past a boot, of a region whose
/// pages `untouched` says the boot never touched: each huge page's span of `per` pages
/// untouched whole, or touched whole, coalesced with its neighbours; a span touched in part,
/// and a region's last span short of a huge page, left out.
fn huge_runs(untouched: &[bool], per: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    if per == 0 {
        return runs;
    }
    for (i, span) in untouched.chunks(per).enumerate() {
        let whole = span.len() == per && (span.iter().all(|&u| u) || span.iter().all(|&u| !u));
        if !whole {
            continue;
        }
        match runs.last_mut() {
            Some((first, count)) if *first + *count == i * per => *count += per,
            _ => runs.push((i * per, per)),
        }
    }
    runs
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        for r in &self.regions {
            // SAFETY: each region is a reservation we made and still own.
            unsafe { platform::release(r.host, r.len) };
        }
        if let Some(h) = &self.hotplug {
            // SAFETY: the memory to plug is a reservation we made and still own.
            unsafe { platform::release(h.host, h.len) };
        }
    }
}

/// One host thread's access to guest memory: while it lives, no other host thread has
/// one ([`GuestMemory::access`]).
#[derive(Debug)]
pub struct Access<'a> {
    mem: &'a GuestMemory,
    _held: MutexGuard<'a, ()>,
}

impl Drop for Access<'_> {
    fn drop(&mut self) {
        // Before `_held` unlocks, so the next holder's token is never overwritten.
        self.mem.holder.store(0, Ordering::Relaxed);
    }
}

impl<'a> Access<'a> {
    /// The memory this accesses: for the bounds and addresses of ranges the kernel is
    /// given.
    pub fn memory(&self) -> &'a GuestMemory {
        self.mem
    }

    /// Plugs `gpa..gpa+len`, whole blocks of the memory to plug, none of them plugged:
    /// grants the host's pages ([`platform::grant`]), has `map` give them to the guest where
    /// the hypervisor's stage 2 holds what it maps (`hv::Stage2`), then lets the host's
    /// devices reach them. Under this access, so no host thread's access to guest memory
    /// sees part of it. What fails is undone: the blocks stay unplugged.
    pub fn plug(
        &self,
        gpa: u64,
        len: u64,
        map: impl FnOnce(*mut u8, u64, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let Blocks { h, blocks, at, size } = self.whole_blocks(gpa, len)?;
        if blocks.clone().any(|b| h.is_plugged(b)) {
            return Err(format!("{gpa:#x}+{len:#x} is plugged already"));
        }
        // SAFETY: whole blocks of the reservation, which no host thread accesses: none is
        // plugged, so `host_ptr` gives no one a pointer into them.
        unsafe { platform::grant(at, size) }.map_err(|e| format!("granting {gpa:#x}+{len:#x}: {e}"))?;
        if let Err(e) = map(at.as_ptr(), gpa, size) {
            // SAFETY: as above; the hypervisor did not map them.
            let _ = unsafe { platform::withhold(at, size) };
            return Err(e);
        }
        h.set(blocks, true);
        Ok(())
    }

    /// Unplugs `gpa..gpa+len`, whole blocks of the memory to plug, all of them plugged and
    /// none of them pinned for a device's DMA ([`GuestMemory::pin`]):
    /// keeps the host's devices from them, has `unmap` take them from the guest where the
    /// hypervisor's stage 2 holds what it maps, then gives their pages back to the host
    /// ([`platform::withhold`]). Under this access, so no host thread is between finding
    /// them plugged and touching them. Where `unmap` fails they stay plugged; where the
    /// host keeps their pages, the guest has them no more, and the first such failure is
    /// said.
    pub fn unplug(
        &self,
        gpa: u64,
        len: u64,
        unmap: impl FnOnce(u64, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let Blocks { h, blocks, at, size } = self.whole_blocks(gpa, len)?;
        if !blocks.clone().all(|b| h.is_plugged(b)) {
            return Err(format!("{gpa:#x}+{len:#x} is not plugged"));
        }
        if self.mem.pinned(gpa, len) {
            return Err(format!("{gpa:#x}+{len:#x} is pinned for a device's DMA"));
        }
        h.set(blocks.clone(), false);
        if let Err(e) = unmap(gpa, size) {
            h.set(blocks, true);
            return Err(e);
        }
        // SAFETY: whole blocks of the reservation, which no host thread holds a pointer into:
        // `host_ptr` gives none for unplugged blocks, and none was taken under another
        // access; the system calls of a device given one before are refused the range from
        // here on (EFAULT).
        if let Err(e) = unsafe { platform::withhold(at, size) }
            && !h.said_kept.swap(true, Ordering::Relaxed)
        {
            crate::warn!("memory unplugged at {gpa:#x}+{len:#x} keeps its host pages: {e}");
        }
        Ok(())
    }

    /// Gives the host back the pages of `gpa..gpa+len`, guest RAM the guest holds free and
    /// has reported (D151): the host's whole pages within it, of its RAM or of memory
    /// plugged, unless a device's DMA holds some of them pinned ([`GuestMemory::pin`]). As unplug and plug in turn, but the range stays the guest's: `unmap` takes
    /// it from the guest where the hypervisor's stage 2 holds what it maps, its pages go
    /// back to the host and fresh ones, zero-filled as first touched, take their place
    /// ([`platform::discard`]), and `map` gives it to the guest again, whether its pages
    /// went or not. A range whose pages the host holds none of is left as it is: the
    /// guest's free memory a restored VM never touched is its template's file's, and its
    /// reports came at once after each restore (D151). Under this access, so no host thread
    /// is between. The bytes given back.
    pub fn discard(
        &self,
        gpa: u64,
        len: u64,
        unmap: impl FnOnce(u64, usize) -> Result<(), String>,
        map: impl FnOnce(*mut u8, u64, usize) -> Result<(), String>,
    ) -> Result<u64, String> {
        self.mem.reported.fetch_add(len, Ordering::Relaxed);
        let page = platform::page_size().map_err(|e| e.to_string())? as u64;
        let end = gpa
            .checked_add(len)
            .ok_or_else(|| format!("{gpa:#x}+{len:#x} overflows"))?;
        let (start, end) = (
            gpa.checked_next_multiple_of(page).unwrap_or(u64::MAX),
            end - end % page,
        );
        if end <= start {
            return Ok(0);
        }
        if self.mem.pinned(start, end - start) {
            return Err(format!(
                "{start:#x}+{:#x} is pinned for a device's DMA",
                end - start
            ));
        }
        let size = usize::try_from(end - start).map_err(|e| e.to_string())?;
        let host = self.mem.host_ptr(start, size).map_err(|e| e.to_string())?;
        let at = NonNull::new(host).ok_or("a null host address")?;
        // SAFETY: guest RAM of this memory, whole host pages of it.
        let held = unsafe { platform::untouched(host, size, page as usize) };
        if let Ok(Some(untouched)) = held
            && untouched.iter().all(|u| *u)
        {
            return Ok(0);
        }
        // A working set being recorded keeps what the host held of it.
        if let Some(kept) = lock(&self.mem.given_back).as_mut()
            && let Some((region, r)) =
                (self.mem.regions.iter().enumerate()).find(|(_, r)| start >= r.gpa && end <= r.end())
        {
            kept.keep(region, ((start - r.gpa) / page) as usize, host, size);
        }
        // A file's RAM, a restore's, gives its pages back through its views: Windows gives
        // a copy-on-write view's written pages back only as the view goes (PM M371, M375).
        let views = (self.mem.regions.iter())
            .find(|r| start >= r.gpa && end <= r.end())
            .and_then(|r| r.views.as_ref());
        unmap(start, size)?;
        // SAFETY: whole host pages of guest RAM, which the guest holds free and no host
        // thread accesses under this access: a device's buffers are the guest's in use.
        let discarded = unsafe {
            match views {
                Some(views) => views.give_back(at, size),
                None => platform::discard(at, size).map(|()| size),
            }
        };
        map(host, start, size)?;
        let given = discarded.map_err(|e| e.to_string())?;
        Ok(given as u64)
    }

    /// The blocks `gpa..gpa+len` is, where it is whole blocks of the memory to plug.
    fn whole_blocks(&self, gpa: u64, len: u64) -> Result<Blocks<'a>, String> {
        let h = self
            .mem
            .hotplug
            .as_ref()
            .ok_or("the guest has no memory to plug")?;
        let blocks = h
            .blocks(gpa, len)
            .filter(|_| len > 0 && gpa.is_multiple_of(h.block) && len.is_multiple_of(h.block))
            .ok_or_else(|| format!("{gpa:#x}+{len:#x} is not whole blocks of the memory to plug"))?;
        let size = usize::try_from(len).map_err(|e| e.to_string())?;
        let at = NonNull::new(h.host(gpa)).ok_or("a null host address")?;
        Ok(Blocks { h, blocks, at, size })
    }

    /// Copies guest memory at `gpa` into `buf`: RAM, the memory plugged, or device memory,
    /// whose copy fails where its pages are gone ([`GuestMemory::io_ptr`]).
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), OutOfBounds> {
        // SAFETY: `buf` is valid for writes of its length, and nothing else accesses it.
        unsafe { self.read_raw(gpa, buf.as_mut_ptr(), buf.len()) }
    }

    /// Copies `buf` into guest memory at `gpa`, as [`read`](Self::read) reaches it.
    pub fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), OutOfBounds> {
        // SAFETY: `buf` is valid for reads of its length.
        unsafe { self.write_raw(gpa, buf.as_ptr(), buf.len()) }
    }

    /// Copies `len` bytes of guest memory at `gpa` to `dst`, memory no Rust reference
    /// covers: a record of a ring another process shares, written before it is published.
    /// As [`read`](Self::read) reaches it.
    ///
    /// # Safety
    /// `dst` must be valid for writes of `len` bytes, of host memory that nothing else
    /// accesses meanwhile.
    pub unsafe fn read_raw(&self, gpa: u64, dst: *mut u8, len: usize) -> Result<(), OutOfBounds> {
        let src = match self.mem.host_ptr(gpa, len) {
            Ok(src) => src,
            Err(oob) => {
                let src = self.mem.device_ptr(gpa, len).ok_or(oob)?;
                // SAFETY: `len` bytes of device memory, mapped while a device reaches it
                // (`give_devices`), which host memory like `dst` never overlaps, and whose
                // faults fail the copy; `dst` as the caller promises.
                return unsafe { platform::copy_checked(src, dst, len) }.map_err(|_| oob);
            }
        };
        // SAFETY: `src` is valid for `len` bytes of guest memory, which host memory like
        // `dst` never overlaps; `dst` as the caller promises.
        unsafe { platform::copy_in(src, dst, len) };
        Ok(())
    }

    /// Copies `len` bytes at `src`, memory no Rust reference covers, into guest memory at
    /// `gpa`, as [`read`](Self::read) reaches it.
    ///
    /// # Safety
    /// `src` must be valid for reads of `len` bytes of host memory.
    pub unsafe fn write_raw(&self, gpa: u64, src: *const u8, len: usize) -> Result<(), OutOfBounds> {
        let dst = match self.mem.host_ptr(gpa, len) {
            Ok(dst) => dst,
            Err(oob) => {
                let dst = self.mem.device_ptr(gpa, len).ok_or(oob)?;
                // SAFETY: as in `read_raw`, the other way.
                return unsafe { platform::copy_checked(src, dst, len) }.map_err(|_| oob);
            }
        };
        // SAFETY: `dst` is valid for `len` bytes of guest memory, which host memory like
        // `src` never overlaps; `src` as the caller promises.
        unsafe { platform::copy_out(src, dst, len) };
        Ok(())
    }

    /// The `T` at `gpa`, which need not be aligned.
    pub fn read_obj<T: Pod>(&self, gpa: u64) -> Result<T, OutOfBounds> {
        let src = self.mem.host_ptr(gpa, size_of::<T>())?;
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        // SAFETY: `src` is valid for `size_of::<T>()` bytes of guest memory, `value` for as
        // many bytes of this thread's, and every byte of it is written; `T: Pod` accepts
        // whatever bytes arrive.
        unsafe {
            platform::copy_in(src, value.as_mut_ptr().cast(), size_of::<T>());
            Ok(value.assume_init())
        }
    }

    /// Stores `value` at `gpa`, which need not be aligned.
    pub fn write_obj<T: Pod>(&self, gpa: u64, value: T) -> Result<(), OutOfBounds> {
        let dst = self.mem.host_ptr(gpa, size_of::<T>())?;
        // SAFETY: `dst` is valid for `size_of::<T>()` bytes of guest memory, and `value`'s
        // bytes are all initialized (`T: Pod` has no padding).
        unsafe { platform::copy_out(std::ptr::from_ref(&value).cast(), dst, size_of::<T>()) };
        Ok(())
    }

    /// Loads the naturally aligned `u16` at `gpa` in one access: a virtqueue index, which
    /// the guest stores whole.
    pub fn load_u16(&self, gpa: u64, order: Ordering) -> Result<u16, OutOfBounds> {
        let p = self.aligned_u16(gpa)?;
        // SAFETY: aligned, in bounds, and valid for the call; host accesses are ordered by
        // this access, the guest's are the hardware's.
        Ok(unsafe { AtomicU16::from_ptr(p) }.load(order))
    }

    /// Stores the naturally aligned `u16` at `gpa` in one access: a virtqueue index, which
    /// the guest loads whole.
    pub fn store_u16(&self, gpa: u64, value: u16, order: Ordering) -> Result<(), OutOfBounds> {
        let p = self.aligned_u16(gpa)?;
        // SAFETY: as in `load_u16`.
        unsafe { AtomicU16::from_ptr(p) }.store(value, order);
        Ok(())
    }

    fn aligned_u16(&self, gpa: u64) -> Result<*mut u16, OutOfBounds> {
        if !gpa.is_multiple_of(2) {
            return Err(OutOfBounds { gpa, len: 2 });
        }
        // Regions are page-aligned, so an even address is aligned in the host too.
        Ok(self.mem.host_ptr(gpa, 2)?.cast())
    }
}

/// Whether the `len` bytes of guest memory at `p`, word-aligned and a whole number of
/// words, are all zero, by volatile reads.
///
/// # Safety
/// `p` must be word-aligned and valid for reads of `len` bytes of guest memory.
unsafe fn is_zero(p: *const u8, len: usize) -> bool {
    let words = p.cast::<u64>();
    // SAFETY: `k < len / WORD` keeps each aligned word within the range.
    (0..len / WORD).all(|k| unsafe { words.add(k).read_volatile() } == 0)
}

/// Writes `run` of region `r` to `file`, which holds the region from `offset` on, by
/// system calls given the guest address.
/// Writes the pages of `r` the guest used to `file` at `offset`, one write for each run of
/// them, all-zero pages and those `untouched` marks left as holes: the runs written, from
/// the region's start, in order.
fn write_used(
    file: &File,
    r: &Region,
    untouched: Option<&[bool]>,
    offset: u64,
    page: usize,
) -> io::Result<Vec<Range<usize>>> {
    // Where the run of used pages being gathered starts.
    let mut run = None;
    let mut written = Vec::new();
    for (i, start) in (0..r.len).step_by(page).enumerate() {
        let skipped = untouched.is_some_and(|u| u.get(i).copied().unwrap_or(false));
        // SAFETY: the page at `start` lies in `r`, which is page-aligned.
        let used = !skipped && unsafe { !is_zero(r.host.as_ptr().add(start), page) };
        match (run, used) {
            (None, true) => run = Some(start),
            (Some(first), false) => {
                write_run(file, r, first..start, offset)?;
                written.push(first..start);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(first) = run {
        write_run(file, r, first..r.len, offset)?;
        written.push(first..r.len);
    }
    Ok(written)
}

/// The runs of whole [`DATA_BLOCK`]s, the last cut at `len`, that `used`'s runs, in order,
/// of a region `len` long lie in, in order.
fn blocks_of(used: &[Range<usize>], len: usize) -> Vec<Range<usize>> {
    let mut blocks: Vec<Range<usize>> = Vec::new();
    for run in used.iter().filter(|r| r.start < r.end) {
        let start = run.start - run.start % DATA_BLOCK;
        let end = run.end.div_ceil(DATA_BLOCK).saturating_mul(DATA_BLOCK).min(len);
        match blocks.last_mut() {
            Some(last) if last.end >= start => last.end = last.end.max(end),
            _ => blocks.push(start..end),
        }
    }
    blocks
}

/// The runs of a region at `offset` of its memory file, `len` long, that `data`'s runs of
/// the file say hold data, from the region's start: all of it where none are said.
fn region_data(data: Option<&[(u64, u64)]>, offset: u64, len: usize) -> Vec<Range<usize>> {
    let Some(data) = data else {
        return std::iter::once(0..len).collect();
    };
    let end = offset.saturating_add(len as u64);
    data.iter()
        .filter_map(|&(at, n)| {
            let (start, stop) = (at.max(offset), at.saturating_add(n).min(end));
            (start < stop).then(|| (start - offset) as usize..(stop - offset) as usize)
        })
        .collect()
}

fn write_run(file: &File, r: &Region, run: Range<usize>, offset: u64) -> io::Result<()> {
    if run.start > run.end || run.end > r.len {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // On a Linux host the page cache holds a file written in large contiguous runs as
    // large folios (ext4 makes them, 6.16+), and a restore's read fault then maps a whole
    // folio of the file at once (filemap_map_folio_range, 6.18+), whatever the mapping's
    // advice: a restored guest's memory file held 26 MiB to Firecracker's 14 (PM M287,
    // M291). Writing a page at a time keeps the folios to a page, so a fault maps no more
    // than fault-around's (9.1 to 9.2 MiB, beating Firecracker). Every Linux host writes so,
    // one that makes no large folios too (what it costs a save: M290); macOS writes the run
    // whole.
    let most = if cfg!(target_os = "linux") {
        platform::page_size()?
    } else {
        usize::MAX
    };
    let at = offset + run.start as u64;
    let mut done = 0;
    while done < run.len() {
        // SAFETY: `run.start + done .. run.end` lies in the region (checked above); the
        // kernel reads it.
        let n = unsafe {
            platform::write_at(
                file,
                r.host.as_ptr().add(run.start + done),
                (run.len() - done).min(most),
                at + done as u64,
            )?
        };
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        done += n;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::platform::page_size;

    fn mem() -> GuestMemory {
        let p = page_size().unwrap();
        GuestMemory::anonymous(&[(0x8000_0000, 4 * p), (0x1_0000_0000, p)]).unwrap()
    }

    /// Past a boot, a huge page's span goes on huge pages where the boot touched none of it
    /// or all of it, never where it touched part (which would hold the rest resident),
    /// neighbours coalesced, a short last span left out.
    #[test]
    fn huge_pages_go_where_a_boot_touched_none_or_all() {
        // '.' never touched, '#' touched.
        let t = |s: &str| s.chars().map(|c| c == '.').collect::<Vec<bool>>();
        // Spans of 4: touched whole, in part, untouched, untouched, in part, a short last.
        let untouched = t("#### #..# .... .... ..#. ..".replace(' ', "").as_str());
        assert_eq!(huge_runs(&untouched, 4), vec![(0, 4), (8, 8)]);
        assert_eq!(huge_runs(&t("........"), 4), vec![(0, 8)]);
        assert_eq!(huge_runs(&t("#......#"), 4), Vec::<(usize, usize)>::new());
        assert_eq!(huge_runs(&t("####....####"), 4), vec![(0, 12)]);
        assert_eq!(huge_runs(&t("###"), 4), Vec::<(usize, usize)>::new());
        assert_eq!(huge_runs(&t("...."), 0), Vec::<(usize, usize)>::new());
    }

    /// Guest RAM kept on small pages has its own mapping, advised so (smaps' `nh`), and
    /// the rest keeps its huge pages' advice (`hg`), where the kernel has THP.
    #[cfg(target_os = "linux")]
    #[test]
    fn small_pages_are_advised_apart_from_the_rest() {
        if !std::path::Path::new("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").exists() {
            return;
        }
        let m = GuestMemory::anonymous(&[(0, 64 << 20)]).unwrap();
        m.small_pages(0, 2 << 20).unwrap();
        let flags = |gpa: u64| {
            let at = m.host_ptr(gpa, 1).unwrap() as u64;
            let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
            let mut ours = false;
            for line in smaps.lines() {
                if let Some((range, _)) = line.split_once(' ')
                    && let Some((s, _)) = range.split_once('-')
                    && let Ok(s) = u64::from_str_radix(s, 16)
                {
                    ours = s == at;
                } else if ours && let Some(flags) = line.strip_prefix("VmFlags:") {
                    return flags.split_whitespace().map(String::from).collect::<Vec<_>>();
                }
            }
            Vec::new()
        };
        let (small, rest) = (flags(0), flags(2 << 20));
        assert!(small.iter().any(|f| f == "nh"), "{small:?}");
        assert!(rest.iter().any(|f| f == "hg"), "{rest:?}");
        assert!(m.small_pages(63 << 20, 2 << 20).is_err());
    }

    /// Past a boot on small pages, the huge pages' spans the boot left untouched, or
    /// touched whole, are advised huge again (smaps' `hg`), and one it touched in part
    /// stays on small pages (`nh`), where the kernel has THP.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_boots_untouched_and_whole_spans_go_back_on_huge_pages() {
        let Some(huge) = platform::advised_huge_page() else {
            return;
        };
        let page = page_size().unwrap();
        let m = GuestMemory::anonymous(&[(0, 4 * huge)]).unwrap();
        m.small_pages(0, 4 * huge).unwrap();
        {
            let access = m.access().unwrap();
            // The first span whole, one page of the second; the rest untouched.
            for at in (0..huge).step_by(page) {
                access.write_obj(at as u64, 1u8).unwrap();
            }
            access.write_obj(huge as u64, 1u8).unwrap();
        }
        m.huge_pages_past_boot().unwrap();
        let flags = |gpa: u64| {
            let at = m.host_ptr(gpa, 1).unwrap() as u64;
            let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
            let mut ours = false;
            for line in smaps.lines() {
                if let Some((range, _)) = line.split_once(' ')
                    && let Some((s, e)) = range.split_once('-')
                    && let (Ok(s), Ok(e)) = (u64::from_str_radix(s, 16), u64::from_str_radix(e, 16))
                {
                    ours = (s..e).contains(&at);
                } else if ours && let Some(flags) = line.strip_prefix("VmFlags:") {
                    return flags.split_whitespace().map(String::from).collect::<Vec<_>>();
                }
            }
            Vec::new()
        };
        let huge = huge as u64;
        let (whole, part, untouched) = (flags(0), flags(huge), flags(2 * huge));
        assert!(whole.iter().any(|f| f == "hg"), "{whole:?}");
        assert!(part.iter().any(|f| f == "nh"), "{part:?}");
        assert!(untouched.iter().any(|f| f == "hg"), "{untouched:?}");
        assert!(flags(3 * huge).iter().any(|f| f == "hg"));
    }

    #[test]
    fn roundtrip_and_bounds() {
        let m = mem();
        let p = page_size().unwrap() as u64;
        m.access()
            .unwrap()
            .write_obj(0x8000_0003u64, 0xdead_beef_u32)
            .unwrap();
        assert_eq!(
            m.access().unwrap().read_obj::<u32>(0x8000_0003).unwrap(),
            0xdead_beef
        );
        // Last byte of a region is fine; one past is not; ranges may not straddle holes.
        m.access().unwrap().write(0x8000_0000 + 4 * p - 1, &[7]).unwrap();
        assert!(m.access().unwrap().write(0x8000_0000 + 4 * p, &[7]).is_err());
        assert!(
            m.access()
                .unwrap()
                .read(0x8000_0000 + 4 * p - 2, &mut [0; 4])
                .is_err()
        );
        assert!(m.access().unwrap().read(0x7fff_ffff, &mut [0; 2]).is_err());
        assert!(m.host_ptr(u64::MAX - 1, 4).is_err());
        assert_eq!(
            m.access().unwrap().read_obj::<u8>(0x1_0000_0000 + p - 1).unwrap(),
            0
        );
    }

    #[test]
    fn rejects_misaligned_and_overlapping() {
        let p = page_size().unwrap();
        assert!(GuestMemory::anonymous(&[(0x8000_0000, p + 1)]).is_err());
        assert!(GuestMemory::anonymous(&[(0x8000_0001, p)]).is_err());
        assert!(GuestMemory::anonymous(&[(0x8000_0000, 2 * p), (0x8000_0000 + p as u64, p)]).is_err());
    }

    /// Memory to plug is whole blocks, of whole host pages, apart from RAM; none of it is
    /// the host's devices' until plugged, nor once unplugged.
    #[test]
    fn memory_to_plug_is_whole_blocks_and_withheld_until_plugged() {
        let p = page_size().unwrap();
        let block = (2 << 20).max(p as u64);
        let ram = [(0x8000_0000u64, 4 * p)];
        let hot = 0x1_0000_0000u64;
        assert!(GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, block + p as u64).is_err());
        assert!(GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, p as u64 / 2).is_err());
        assert!(GuestMemory::with_hotplug(&ram, hot + p as u64, 4 * block as usize, block).is_err());
        assert!(GuestMemory::with_hotplug(&ram, hot, 4 * block as usize - p, block).is_err());
        assert!(GuestMemory::with_hotplug(&ram, hot, 0, block).is_err());
        assert!(GuestMemory::with_hotplug(&ram, u64::MAX - block + 1, 2 * block as usize, block).is_err());
        assert!(GuestMemory::with_hotplug(&[(0, 4 * block as usize)], 0, block as usize, block).is_err());

        let m = GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, block).unwrap();
        assert_eq!(
            m.hotplug().map(|(g, _, l)| (g, l)),
            Some((hot, 4 * block as usize))
        );
        assert_eq!(m.regions().count(), 1, "the memory to plug is no region of RAM");
        assert!(m.host_ptr(hot, 1).is_err());
        assert_eq!(m.plugged(hot, 4 * block), Some((0, 4)));
        assert_eq!(m.plugged(hot + 4 * block, 1), None);
        let a = m.access().unwrap();
        assert!(
            a.plug(hot + 1, block, |_, _, _| Ok(())).is_err(),
            "not whole blocks"
        );
        assert!(a.unplug(hot, block, |_, _| Ok(())).is_err(), "not plugged");
        a.plug(hot + block, 2 * block, |_, _, _| Ok(())).unwrap();
        assert!(
            a.plug(hot + 2 * block, block, |_, _, _| Ok(())).is_err(),
            "plugged already"
        );
        a.write_obj(hot + 3 * block - 8, 7u64).unwrap();
        assert!(
            a.read_obj::<u64>(hot + 3 * block - 4).is_err(),
            "across into an unplugged block"
        );
        assert_eq!(m.plugged_runs(), [(hot + block, 2 * block)]);
        // An unmap that fails leaves the blocks plugged.
        assert!(a.unplug(hot + block, block, |_, _| Err("no".into())).is_err());
        assert_eq!(a.read_obj::<u64>(hot + 3 * block - 8).unwrap(), 7);
        a.unplug(hot + 2 * block, block, |_, _| Ok(())).unwrap();
        assert!(a.read_obj::<u64>(hot + 3 * block - 8).is_err());
        drop(a);
        // What is plugged is saved with the RAM (`memory_plugged_is_saved_and_plugged_back`).
        let dir = shards_testdir::TempDir::new("plug-save").unwrap();
        let file = std::fs::File::create(dir.join("mem")).unwrap();
        m.save(&file).unwrap();
    }

    /// What the host holds of the memory the guest gave back: none of a block unplugged,
    /// whose pages its unplugging gave back; and a block plugged still, its pages the
    /// guest's, is none of what it gave back, where a guest that ends asking for memory has
    /// one being plugged as it ends.
    #[test]
    fn the_memory_given_back_holds_nothing() {
        let p = page_size().unwrap();
        let block = (2 << 20).max(p as u64);
        let ram = [(0x8000_0000u64, 4 * p)];
        let hot = 0x1_0000_0000u64;
        let m = GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, block).unwrap();
        let a = m.access().unwrap();
        a.plug(hot, 2 * block, |_, _, _| Ok(())).unwrap();
        for at in (0..2 * block).step_by(p) {
            a.write_obj(hot + at, 1u8).unwrap();
        }
        a.unplug(hot, block, |_, _| Ok(())).unwrap();
        drop(a);
        // Where the host can say which pages it holds.
        if let Some(held) = m.hotplug_held().unwrap() {
            assert_eq!(held, 0, "of the block given back, or counting the one plugged");
        }
    }

    /// A range a device's DMA holds pinned is kept: a report of it free gives nothing back
    /// and says why, and an unplug of a block it meets is refused, the block plugged still;
    /// let go as many times as pinned, both go through.
    #[test]
    fn what_a_devices_dma_holds_pinned_is_kept() {
        let p = page_size().unwrap();
        let block = (2 << 20).max(p as u64);
        let ram = [(0x8000_0000u64, 16 * p)];
        let hot = 0x1_0000_0000u64;
        let m = GuestMemory::with_hotplug(&ram, hot, 2 * block as usize, block).unwrap();
        let a = m.access().unwrap();
        a.plug(hot, 2 * block, |_, _, _| Ok(())).unwrap();
        a.write_obj(0x8000_0000 + 4 * p as u64, 7u64).unwrap();
        a.write_obj(hot + block, 9u64).unwrap();
        let (two, pages) = (2 * p as u64, 0x8000_0000 + 4 * p as u64);
        m.pin(pages + p as u64, p as u64);
        m.pin(pages + p as u64, p as u64);
        m.pin(hot + block + 8, 8);
        let kept = a.discard(pages, two, |_, _| Ok(()), |_, _, _| Ok(()));
        assert!(kept.unwrap_err().contains("pinned"), "a pin within it");
        assert_eq!(a.read_obj::<u64>(pages).unwrap(), 7, "nothing went");
        assert!(
            a.unplug(hot + block, block, |_, _| Ok(()))
                .unwrap_err()
                .contains("pinned")
        );
        assert_eq!(a.read_obj::<u64>(hot + block).unwrap(), 9, "plugged still");
        a.unplug(hot, block, |_, _| Ok(())).unwrap();
        m.unpin(pages + p as u64, p as u64);
        assert!(
            a.discard(pages, two, |_, _| Ok(()), |_, _, _| Ok(())).is_err(),
            "pinned twice"
        );
        m.unpin(pages + p as u64, p as u64);
        assert_eq!(
            a.discard(pages, two, |_, _| Ok(()), |_, _, _| Ok(())).unwrap(),
            two
        );
        m.unpin(hot + block + 8, 8);
        a.unplug(hot + block, block, |_, _| Ok(())).unwrap();
    }

    /// A snapshot keeps memory to plug where none of it is plugged (D148): its RAM is
    /// saved, and a restore remakes the region as boot made it, withheld.
    #[test]
    fn memory_to_plug_with_none_plugged_is_saved_and_remade() {
        let p = page_size().unwrap();
        let block = (2 << 20).max(p as u64);
        let ram = [(0x8000_0000u64, 4 * p)];
        let hot = 0x1_0000_0000u64;
        let m = GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, block).unwrap();
        m.access().unwrap().write_obj(0x8000_0000u64, 0x77u64).unwrap();
        let dir = shards_testdir::TempDir::new("plug-none").unwrap();
        let file = std::fs::File::create(dir.join("mem")).unwrap();
        m.save(&file).unwrap();
        let file = std::fs::File::open(dir.join("mem")).unwrap();
        let back = GuestMemory::from_file(&ram, &file, None)
            .unwrap()
            .with_plug_region(hot, 4 * block as usize, block)
            .unwrap();
        let a = back.access().unwrap();
        assert_eq!(a.read_obj::<u64>(0x8000_0000).unwrap(), 0x77);
        assert!(
            a.read_obj::<u64>(hot).is_err(),
            "the region is withheld until plugged"
        );
        drop(a);
        assert_eq!(
            back.hotplug().map(|(gpa, _, len)| (gpa, len)),
            Some((hot, 4 * block as usize))
        );
        assert!(
            back.with_plug_region(hot, block as usize, block).is_err(),
            "one region"
        );
    }

    /// Memory plugged is saved after the RAM, run by run (D176): a restore that plugs the
    /// same runs again reads back what each held, its zero pages holes in the file, and the
    /// rest stays withheld. A run the file does not hold is refused, not read as zeros.
    #[test]
    fn memory_plugged_is_saved_and_plugged_back() {
        let p = page_size().unwrap();
        let block = (2 << 20).max(p as u64);
        let ram = [(0x8000_0000u64, 4 * p)];
        let hot = 0x1_0000_0000u64;
        let m = GuestMemory::with_hotplug(&ram, hot, 4 * block as usize, block).unwrap();
        let a = m.access().unwrap();
        a.write_obj(0x8000_0000u64, 0x77u64).unwrap();
        // Two runs: block 0, and blocks 2 and 3.
        a.plug(hot, block, |_, _, _| Ok(())).unwrap();
        a.plug(hot + 2 * block, 2 * block, |_, _, _| Ok(())).unwrap();
        a.write_obj(hot + 8, 0x1111u64).unwrap();
        a.write_obj(hot + 3 * block + 16, 0x3333u64).unwrap();
        drop(a);
        let runs = m.plugged_runs();
        assert_eq!(runs, [(hot, block), (hot + 2 * block, 2 * block)]);
        let dir = shards_testdir::TempDir::new("plug-some").unwrap();
        let file = std::fs::File::create(dir.join("mem")).unwrap();
        m.save(&file).unwrap();
        assert_eq!(
            file.metadata().unwrap().len(),
            4 * p as u64 + 3 * block,
            "the RAM, then each run"
        );
        let file = std::fs::File::open(dir.join("mem")).unwrap();
        let back = GuestMemory::from_file(&ram, &file, None)
            .unwrap()
            .with_plug_region(hot, 4 * block as usize, block)
            .unwrap();
        let mut mapped = Vec::new();
        back.restore_plugged(&runs, &file, |_, gpa, len| {
            mapped.push((gpa, len));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            mapped,
            [(hot, block as usize), (hot + 2 * block, 2 * block as usize)]
        );
        assert_eq!(back.plugged_runs(), runs);
        let a = back.access().unwrap();
        assert_eq!(a.read_obj::<u64>(0x8000_0000).unwrap(), 0x77);
        assert_eq!(a.read_obj::<u64>(hot + 8).unwrap(), 0x1111);
        assert_eq!(a.read_obj::<u64>(hot + 3 * block + 16).unwrap(), 0x3333);
        assert_eq!(
            a.read_obj::<u64>(hot + 2 * block).unwrap(),
            0,
            "a hole, read as zeros"
        );
        assert!(
            a.read_obj::<u64>(hot + block).is_err(),
            "block 1 was never plugged"
        );
        drop(a);
        let other = GuestMemory::from_file(&ram, &file, None)
            .unwrap()
            .with_plug_region(hot, 4 * block as usize, block)
            .unwrap();
        let past = [(hot, block), (hot + 2 * block, 2 * block), (hot + block, block)];
        assert!(other.restore_plugged(&past, &file, |_, _, _| Ok(())).is_err());
    }

    /// Guest RAM restored from a file is on the host's base pages (MADV_NOHUGEPAGE, which
    /// /proc/self/smaps shows as VmFlags `nh`): the page cache's folios of the file, up to
    /// 2 MiB, are mapped a page at a time, not whole (PM M287).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_restored_guests_memory_is_on_base_pages() {
        let huge = 2usize << 20;
        let ranges = [(0x8000_0000u64, 4 * huge)];
        let m = GuestMemory::anonymous(&ranges).unwrap();
        m.access().unwrap().write(0x8000_0000, b"hello").unwrap();
        let dir = shards_testdir::TempDir::new("mem-base").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("mem"))
            .unwrap();
        m.save(&file).unwrap();
        let restored = GuestMemory::from_file(&ranges, &file, None).unwrap();
        let (_, host, len) = restored.regions().next().unwrap();
        let (start, end) = (host as usize, host as usize + len);
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        // Each mapping's header line, then its fields; VmFlags last.
        let mut covering = false;
        let mut flags = Vec::new();
        for line in smaps.lines() {
            if let Some((range, _)) = line.split_once(' ')
                && let Some((a, b)) = range.split_once('-')
                && let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16))
            {
                covering = a < end && start < b;
            } else if covering && let Some(vm) = line.strip_prefix("VmFlags:") {
                flags.push(vm.split_whitespace().any(|f| f == "nh"));
            }
        }
        assert!(!flags.is_empty() && flags.iter().all(|nh| *nh), "{flags:?}");
    }

    #[test]
    fn saves_sparsely_and_restores_copy_on_write() {
        let p = page_size().unwrap();
        let ranges = [(0x8000_0000u64, 4 * p), (0x1_0000_0000, 2 * p)];
        let m = GuestMemory::anonymous(&ranges).unwrap();
        m.access()
            .unwrap()
            .write(0x8000_0000 + p as u64 + 5, b"hello")
            .unwrap();
        m.access()
            .unwrap()
            .write(0x1_0000_0000 + 2 * p as u64 - 1, &[0xee])
            .unwrap();
        let path_dir = shards_testdir::TempDir::new("mem").unwrap();
        let path = path_dir.join("mem");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        m.save(&file).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 6 * p as u64);

        let a = GuestMemory::from_file(&ranges, &file, None).unwrap();
        let b = GuestMemory::from_file(&ranges, &file, None).unwrap();
        let mut buf = [0u8; 5];
        a.access()
            .unwrap()
            .read(0x8000_0000 + p as u64 + 5, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(
            a.access()
                .unwrap()
                .read_obj::<u8>(0x1_0000_0000 + 2 * p as u64 - 1)
                .unwrap(),
            0xee
        );
        assert_eq!(a.access().unwrap().read_obj::<u64>(0x8000_0000).unwrap(), 0);
        // Writes stay private to each copy and never reach the file.
        a.access()
            .unwrap()
            .write(0x8000_0000 + p as u64 + 5, b"HELLO")
            .unwrap();
        b.access()
            .unwrap()
            .read(0x8000_0000 + p as u64 + 5, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"hello");
        drop((a, b));
        let c = GuestMemory::from_file(&ranges, &file, None).unwrap();
        c.access()
            .unwrap()
            .read(0x8000_0000 + p as u64 + 5, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"hello");
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    /// Runs of used pages are written whole, across their edges and to their regions' ends,
    /// and the pages between them stay holes.
    #[cfg(unix)]
    #[test]
    fn saves_runs_of_used_pages_and_holes_between() {
        use std::os::fd::AsRawFd;
        let p = page_size().unwrap();
        let ranges = [(0x8000_0000u64, 16 * p), (0x1_0000_0000, 3 * p)];
        let gpa = |page: usize| match page {
            0..16 => 0x8000_0000 + (page * p) as u64,
            _ => 0x1_0000_0000 + ((page - 16) * p) as u64,
        };
        let used = |page: usize| matches!(page, 0..=2 | 4 | 11..=16 | 18);
        // Each used page's first and last bytes, so a run cut short or moved shows.
        let marks = |page: usize| match used(page) {
            true => (page as u8 + 1, page as u8 + 0x81),
            false => (0, 0),
        };
        let m = GuestMemory::anonymous(&ranges).unwrap();
        for page in (0..19).filter(|&page| used(page)) {
            let (first, last) = marks(page);
            m.access().unwrap().write(gpa(page), &[first]).unwrap();
            m.access()
                .unwrap()
                .write(gpa(page) + p as u64 - 1, &[last])
                .unwrap();
        }
        let path_dir = shards_testdir::TempDir::new("runs").unwrap();
        let path = path_dir.join("runs");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        m.save(&file).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 19 * p as u64);

        // The file holds data exactly where pages were used (lseek(2) SEEK_DATA, SEEK_HOLE),
        // looked at before a restore reads it: a fault on a hole of a tmpfs file fills it
        // (shmem_fault allocates, where read(2) gives the zero page), as on Fedora's /tmp.
        let mut data = Vec::new();
        let mut at = 0;
        loop {
            // SAFETY: lseek on an open descriptor this test owns.
            let start = unsafe { libc::lseek(file.as_raw_fd(), at, libc::SEEK_DATA) };
            if start < 0 {
                // ENXIO: no data from `at` on.
                assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENXIO));
                break;
            }
            // SAFETY: as above.
            let end = unsafe { libc::lseek(file.as_raw_fd(), start, libc::SEEK_HOLE) };
            assert!(end > start, "SEEK_HOLE from {start}: {end}");
            data.extend(start as usize / p..(end as usize).div_ceil(p));
            at = end;
        }
        assert_eq!(data, (0..19).filter(|&page| used(page)).collect::<Vec<_>>());

        let r = GuestMemory::from_file(&ranges, &file, None).unwrap();
        for page in 0..19 {
            let first = r.access().unwrap().read_obj::<u8>(gpa(page)).unwrap();
            let last = r
                .access()
                .unwrap()
                .read_obj::<u8>(gpa(page) + p as u64 - 1)
                .unwrap();
            assert_eq!((first, last), marks(page), "page {page}");
        }
        drop((m, r, file));
        let _ = std::fs::remove_file(path);
    }

    /// A restore's RAM, its runs of data a file's copy-on-write and the rest zeros, gives
    /// what its guest reports free back (D151 on Windows, PM M375): its save says the runs
    /// of whole blocks that hold data; restored, a stretch between them its guest wrote
    /// reads zeros again, and a run its guest wrote whole reads the file's bytes again, or
    /// zeros where the host maps fresh memory over it (macOS); and on Windows, where each
    /// run is a view of its own, a run the range only partly covers keeps what its guest
    /// wrote, and is not said given back.
    #[test]
    fn a_files_ram_gives_back_what_its_guest_frees() {
        let block = DATA_BLOCK;
        let base = 0x8000_0000u64;
        let ranges = [(base, 6 * block)];
        let saved = GuestMemory::anonymous(&ranges).unwrap();
        // The file's bytes: 0x5a in each page's first byte of blocks 0, 1 and 4.
        let p = page_size().unwrap();
        for page in (0..2 * block).chain(4 * block..5 * block).step_by(p) {
            saved
                .access()
                .unwrap()
                .write(base + page as u64, &[0x5a])
                .unwrap();
        }
        let dir = shards_testdir::TempDir::new("give-back").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("memory"))
            .unwrap();
        let data = saved.save(&file).unwrap();
        let b = block as u64;
        assert_eq!(data, vec![(0, 2 * b), (4 * b, b)], "its runs of data");
        let m = GuestMemory::from_file(&ranges, &file, Some(&data)).unwrap();
        assert!(m.file_backed);
        let first = |at: usize| m.access().unwrap().read_obj::<u8>(base + at as u64).unwrap();
        assert!(
            (0..6 * block)
                .step_by(p)
                .all(|at| first(at) == saved_byte(at, block))
        );
        // The guest writes every page of blocks 1 to 4.
        for page in (block..5 * block).step_by(p) {
            m.access().unwrap().write(base + page as u64, &[0xa5]).unwrap();
        }
        let none = |_: u64, _: usize| Ok(());
        let none_map = |_: *mut u8, _: u64, _: usize| Ok(());
        let given = (m.access().unwrap())
            .discard(base + b, 4 * b, none, none_map)
            .unwrap();
        // Blocks 2 and 3, between the runs, zeros again.
        assert!(
            (2 * block..4 * block).step_by(p).all(|at| first(at) == 0),
            "a page of memory of its own given back still holds what the guest wrote"
        );
        // Block 4, a run whole, the file's again.
        let filed = |at: usize| first(at) == 0x5a || (cfg!(target_os = "macos") && first(at) == 0);
        assert!(
            (4 * block..5 * block).step_by(p).all(filed),
            "a page of the run given back still holds what the guest wrote"
        );
        if cfg!(windows) {
            assert_eq!(given, 3 * b, "the run only partly given back said given back");
            assert!(
                (block..2 * block).step_by(p).all(|at| first(at) == 0xa5),
                "the run the range only partly covers lost what the guest wrote"
            );
        } else {
            assert_eq!(given, 4 * b);
        }
        // Block 0, never written, still reads the file's, and block 5 zeros.
        assert!((0..block).step_by(p).all(|at| first(at) == 0x5a));
        assert!((5 * block..6 * block).step_by(p).all(|at| first(at) == 0));
    }

    /// The byte a page at `at` of `a_files_ram_gives_back_what_its_guest_frees`'s file
    /// starts with.
    fn saved_byte(at: usize, block: usize) -> u8 {
        if at < 2 * block || (4 * block..5 * block).contains(&at) {
            0x5a
        } else {
            0
        }
    }

    /// A save's runs of data are runs of whole blocks, the last cut at its region's end,
    /// and a restore takes from them its region's own, from its start; a state that says
    /// none maps a region from its file whole.
    #[test]
    fn runs_of_data_are_whole_blocks_and_each_region_takes_its_own() {
        let mib = 1usize << 20;
        let used = [
            4096..8192,
            3 * mib..3 * mib + 4096,
            9 * mib..10 * mib,
            10 * mib + 4096..10 * mib + 8192,
        ];
        assert_eq!(blocks_of(&used, 11 * mib), vec![0..4 * mib, 8 * mib..11 * mib]);
        assert_eq!(
            blocks_of(&used[..3], 11 * mib),
            vec![0..4 * mib, 8 * mib..10 * mib]
        );
        assert!(blocks_of(&[], 11 * mib).is_empty());
        let m = mib as u64;
        let data = [(0, 4 * m), (8 * m, 2 * m), (12 * m, 2 * m)];
        assert_eq!(
            region_data(Some(&data), 2 * m, 8 * mib),
            vec![0..2 * mib, 6 * mib..8 * mib]
        );
        assert_eq!(
            region_data(Some(&data), 14 * m, 2 * mib),
            Vec::<Range<usize>>::new()
        );
        assert_eq!(region_data(None, 2 * m, 8 * mib), vec![0..8 * mib]);
    }

    /// What working sets record on Linux (vm::x86_64::recorder): memory mapped from a
    /// file maps only what is touched, and a page written is a private copy.
    #[cfg(target_os = "linux")]
    #[test]
    fn file_backed_memory_maps_only_what_is_touched() {
        let p = page_size().unwrap();
        let base = 0x8000_0000u64;
        let at = |page: usize| base + (page * p) as u64;
        let ranges = [(base, 64 * p)];
        let saved = GuestMemory::anonymous(&ranges).unwrap();
        for page in 0..64 {
            saved
                .access()
                .unwrap()
                .write(at(page), &[page as u8 + 1])
                .unwrap();
        }
        let path_dir = shards_testdir::TempDir::new("touched").unwrap();
        let path = path_dir.join("touched");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        saved.save(&file).unwrap();
        let m = GuestMemory::from_file(&ranges, &file, None).unwrap();
        let (_, host, len) = m.regions().next().unwrap();
        assert_eq!(platform::mapped_pages(host, len).unwrap(), []);

        // A read maps its page and pages around it (fault-around, 16 by default: 64 KiB):
        // from the start of its 16-page block of addresses, or of the mapping if that is
        // later, 16 pages on, within its page table and the mapping (mm/memory.c
        // do_fault_around). A write maps its page alone, a private copy.
        assert_eq!(m.access().unwrap().read_obj::<u8>(at(5)).unwrap(), 6);
        m.access().unwrap().write(at(40), &[0xff]).unwrap();
        let mapped = platform::mapped_pages(host, len).unwrap();
        assert!(
            mapped.contains(&(5, false)) && mapped.contains(&(40, true)),
            "{mapped:?}"
        );
        let pte = (host as usize / p + 5) % 512;
        let from = (pte & !15).max(pte - pte.min(5));
        let to = (from + 16).min(512).min(pte + 64 - 5);
        let around = from + 5 - pte..to + 5 - pte;
        assert!(
            mapped
                .iter()
                .all(|&(page, copy)| (around.contains(&page) && !copy) || (page, copy) == (40, true)),
            "{mapped:?} around {around:?}"
        );

        // Populating writable makes the copies a write would, keeping what they hold.
        platform::populate_writable(m.host_ptr(at(50), 2 * p).unwrap(), 2 * p).unwrap();
        let mapped = platform::mapped_pages(host, len).unwrap();
        assert!(
            mapped.contains(&(50, true)) && mapped.contains(&(51, true)),
            "{mapped:?}"
        );
        assert!(
            !mapped.iter().any(|&(page, _)| page == 49 || page == 52),
            "{mapped:?}"
        );
        assert_eq!(m.access().unwrap().read_obj::<u8>(at(51)).unwrap(), 52);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn index_accesses_are_aligned_only() {
        let m = mem();
        let a = m.access().unwrap();
        a.store_u16(0x8000_0010, 0xabcd, Ordering::Release).unwrap();
        assert_eq!(a.read_obj::<u16>(0x8000_0010).unwrap(), 0xabcd);
        assert_eq!(a.load_u16(0x8000_0010, Ordering::Acquire).unwrap(), 0xabcd);
        assert!(a.store_u16(0x8000_0011, 1, Ordering::Release).is_err());
        assert!(a.load_u16(0x8000_0011, Ordering::Acquire).is_err());
    }

    /// Copies land byte for byte at every alignment of guest address and host address (to 64
    /// bytes, which long copies align their stores to; under Miri, which runs short copies
    /// alone, to 16) and length, whatever mix of bytes, words, 16-byte accesses and 32- and
    /// 64-byte pieces carries them: each way checked against bytes read one at a time.
    #[test]
    fn copies_are_exact_at_every_alignment() {
        let m = mem();
        let a = m.access().unwrap();
        let pattern: Vec<u8> = (0..272u32)
            .map(|b| (b as u8).wrapping_mul(37).wrapping_add(11))
            .collect();
        let lens = (0..=40).chain([
            63, 64, 65, 79, 80, 88, 127, 128, 129, 143, 152, 191, 192, 193, 200,
        ]);
        for at in 0..if cfg!(miri) { 16 } else { 64u64 } {
            for len in lens.clone() {
                let gpa = 0x8000_0100 + at;
                let sent = &pattern[(at as usize * 5) % 16..][..len];
                a.write(0x8000_0100 - 8, &[0xee; 256 + 32]).unwrap();
                a.write(gpa, sent).unwrap();
                let got: Vec<u8> = (gpa - 8..gpa + len as u64 + 8)
                    .map(|k| a.read_obj::<u8>(k).unwrap())
                    .collect();
                assert_eq!(&got[..8], &[0xee; 8], "before {at}+{len}");
                assert_eq!(&got[8..8 + len], sent, "{at}+{len}");
                assert_eq!(&got[8 + len..], &[0xee; 8], "after {at}+{len}");
                let mut back = vec![0x11u8; len + 64];
                let to = (at as usize * 7) % 64;
                a.read(gpa, &mut back[to..to + len]).unwrap();
                assert_eq!(&back[to..to + len], sent, "read {at}+{len}");
                let (before, rest) = back.split_at(to);
                assert!(
                    before.iter().chain(&rest[len..]).all(|&b| b == 0x11),
                    "read past {at}+{len}"
                );
            }
        }
        a.write_obj(0x8000_0203u64, 0x0102_0304_0506_0708u64).unwrap();
        assert_eq!(a.read_obj::<u64>(0x8000_0203).unwrap(), 0x0102_0304_0506_0708);
        let mut bytes = [0u8; 8];
        a.read(0x8000_0203, &mut bytes).unwrap();
        assert_eq!(bytes, 0x0102_0304_0506_0708u64.to_ne_bytes());
    }

    /// Copies of 16 KiB and more, which macOS's memcpy, and so these, store past the caches,
    /// land byte for byte too, each way, either side of that length and at odd ends.
    #[test]
    fn long_copies_are_exact() {
        let m = GuestMemory::anonymous(&[(0x8000_0000, 1 << 20)]).unwrap();
        let a = m.access().unwrap();
        let pattern: Vec<u8> = (0..(66u32 << 10)).map(|i| (i % 251) as u8).collect();
        let k16 = 16usize << 10;
        for len in [
            k16 - 1,
            k16,
            k16 + 1,
            k16 + 64 + 16 + 8 + 7,
            64 << 10,
            (64 << 10) + 13,
        ] {
            for at in [0u64, 1, 8, 15, 16, 31, 32, 63] {
                let gpa = 0x8000_1000 + at;
                let sent = &pattern[(at as usize * 3) % 16..][..len];
                a.write(gpa - 8, &vec![0xee; len + 16]).unwrap();
                a.write(gpa, sent).unwrap();
                let got: Vec<u8> = (gpa - 8..gpa + len as u64 + 8)
                    .map(|k| a.read_obj::<u8>(k).unwrap())
                    .collect();
                assert_eq!(&got[..8], &[0xee; 8], "before {at}+{len}");
                assert!(got[8..8 + len] == *sent, "{at}+{len}");
                assert_eq!(&got[8 + len..], &[0xee; 8], "after {at}+{len}");
                let mut back = vec![0x11u8; len + 64];
                let to = (at as usize * 7) % 64;
                a.read(gpa, &mut back[to..to + len]).unwrap();
                assert!(back[to..to + len] == *sent, "read {at}+{len}");
                let (before, rest) = back.split_at(to);
                assert!(
                    before.iter().chain(&rest[len..]).all(|&b| b == 0x11),
                    "read past {at}+{len}"
                );
            }
        }
    }

    /// One host thread at a time holds an access: threads writing and reading back the same
    /// guest bytes never see one another's writes midway (audit A01).
    #[test]
    fn host_threads_take_turns() {
        let m = mem();
        let inside = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for t in 0..8u8 {
                let (m, inside) = (&m, &inside);
                s.spawn(move || {
                    let mine = [t.wrapping_mul(29).wrapping_add(1); 48];
                    for _ in 0..2000 {
                        let a = m.access().unwrap();
                        assert!(!inside.swap(true, Ordering::Relaxed), "two accesses at once");
                        // Overlapping every other thread's bytes, at an odd address.
                        a.write(0x8000_0007, &mine).unwrap();
                        let mut back = [0u8; 48];
                        a.read(0x8000_0007, &mut back).unwrap();
                        assert_eq!(back, mine);
                        inside.store(false, Ordering::Relaxed);
                    }
                });
            }
        });
    }

    /// A thread that asks again for the access it holds is refused, not left waiting for
    /// itself; once it lets go, it and other threads get it.
    #[test]
    fn a_nested_access_is_refused() {
        let m = mem();
        let a = m.access().unwrap();
        assert_eq!(m.access().unwrap_err(), Reentered);
        assert!(m.save(&tempfile("nested")).is_err());
        std::thread::scope(|s| {
            let other = s.spawn(|| m.access().map(|a| a.read_obj::<u8>(0x8000_0000).unwrap()));
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(
                !other.is_finished(),
                "another thread got in while this one held it"
            );
            drop(a);
            assert_eq!(other.join().unwrap(), Ok(0));
        });
        drop(m.access().unwrap());
    }

    /// Ranges given out of address order, of different sizes and with gaps, come back from
    /// a save in the same places, whichever order the restore lists them in (audit A21).
    #[test]
    fn regions_round_trip_whatever_order_their_ranges_come_in() {
        let p = page_size().unwrap();
        let ranges = [(0x1_0000_0000u64, 2 * p), (0x8000_0000, 4 * p), (0x4000_0000, p)];
        let m = GuestMemory::anonymous(&ranges).unwrap();
        let mark = |gpa: u64| (gpa >> 24) as u8 ^ 0x5a;
        for &(gpa, len) in &ranges {
            let a = m.access().unwrap();
            a.write(gpa, &[mark(gpa)]).unwrap();
            a.write(gpa + len as u64 - 1, &[mark(gpa) ^ 0xff]).unwrap();
        }
        let file = tempfile("order");
        m.save(&file).unwrap();
        let mut sorted = ranges;
        sorted.sort_unstable();
        let mut reversed = sorted;
        reversed.reverse();
        for order in [&ranges[..], &sorted, &reversed] {
            let r = GuestMemory::from_file(order, &file, None).unwrap();
            let a = r.access().unwrap();
            for &(gpa, len) in &ranges {
                assert_eq!(
                    a.read_obj::<u8>(gpa).unwrap(),
                    mark(gpa),
                    "{gpa:#x} in {order:x?}"
                );
                assert_eq!(
                    a.read_obj::<u8>(gpa + len as u64 - 1).unwrap(),
                    mark(gpa) ^ 0xff,
                    "{gpa:#x} in {order:x?}"
                );
            }
        }
        // A file that cannot hold the guest's RAM is refused, not mapped past its end.
        file.set_len(6 * p as u64).unwrap();
        assert!(GuestMemory::from_file(&ranges, &file, None).is_err());
    }

    /// A save into a file that held other bytes leaves none of them, in zero pages or past
    /// the end (audit A22).
    #[test]
    fn a_save_leaves_nothing_of_the_file_before() {
        use std::io::Write as _;
        let p = page_size().unwrap();
        let ranges = [(0x8000_0000u64, 4 * p), (0x1_0000_0000, p)];
        let m = GuestMemory::anonymous(&ranges).unwrap();
        m.access()
            .unwrap()
            .write(0x8000_0000 + p as u64, b"kept")
            .unwrap();
        for garbage in [5 * p, 9 * p] {
            let mut file = tempfile("garbage");
            file.write_all(&vec![0xa5; garbage]).unwrap();
            m.save(&file).unwrap();
            assert_eq!(file.metadata().unwrap().len(), 5 * p as u64);
            let r = GuestMemory::from_file(&ranges, &file, None).unwrap();
            let a = r.access().unwrap();
            let mut page = vec![0u8; p];
            for &(gpa, len) in &ranges {
                for at in (gpa..gpa + len as u64).step_by(p) {
                    a.read(at, &mut page).unwrap();
                    let used = at == 0x8000_0000 + p as u64;
                    assert_eq!(page.iter().any(|&b| b != 0), used, "{at:#x} after {garbage}");
                }
            }
            let mut kept = [0u8; 4];
            a.read(0x8000_0000 + p as u64, &mut kept).unwrap();
            assert_eq!(&kept, b"kept");
        }
    }

    /// A save skips the pages the guest never touched, and does not touch them itself; a
    /// restore reads back every page as it was: written, written with zeros, or never
    /// touched (audit D01).
    #[test]
    fn a_save_leaves_untouched_ram_untouched() {
        let p = page_size().unwrap();
        let len = 64 * p;
        let m = GuestMemory::anonymous(&[(0, len)]).unwrap();
        let written = [3usize, 10, 63];
        let zeroed = 20usize;
        {
            let a = m.access().unwrap();
            for &i in &written {
                a.write((i * p + 7) as u64, &[i as u8 + 1]).unwrap();
            }
            a.write((zeroed * p) as u64, &[0]).unwrap();
        }
        let host = m.regions().next().unwrap().1;
        // SAFETY: the region this test's memory owns.
        let before = unsafe { crate::platform::untouched(host, len, p) }.unwrap();
        let file = tempfile("untouched");
        m.save(&file).unwrap();
        // SAFETY: as above.
        let after = unsafe { crate::platform::untouched(host, len, p) }.unwrap();
        if let (Some(before), Some(after)) = (before, after) {
            for i in 0..64 {
                let touched = written.contains(&i) || i == zeroed;
                assert_eq!(before[i], !touched, "page {i} before the save");
                assert_eq!(after[i], !touched, "page {i} after the save: the save touched it");
            }
        }
        let r = GuestMemory::from_file(&[(0, len)], &file, None).unwrap();
        let a = r.access().unwrap();
        let mut page = vec![0u8; p];
        for i in 0..64 {
            a.read((i * p) as u64, &mut page).unwrap();
            let mut want = vec![0u8; p];
            if written.contains(&i) {
                want[7] = i as u8 + 1;
            }
            assert!(page == want, "page {i} restored");
        }
    }

    /// Restored RAM reserves no commit, as booted RAM reserves none: Linux would charge a
    /// private writable mapping its whole size (PM M56). Strict overcommit, mode 2,
    /// charges it all the same.
    #[cfg(target_os = "linux")]
    #[test]
    fn restored_ram_reserves_no_commit() {
        let len = 64 << 20;
        let file = tempfile("noreserve");
        file.set_len(len as u64).unwrap();
        let r = GuestMemory::from_file(&[(0, len)], &file, None).unwrap();
        assert!(r.file_backed);
        let start = format!("{:x}-", r.regions[0].host.as_ptr() as usize);
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let entry = smaps.split_inclusive('\n').skip_while(|l| !l.starts_with(&start));
        let flags = entry
            .filter_map(|l| l.strip_prefix("VmFlags:"))
            .next()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>();
        let strict = std::fs::read_to_string("/proc/sys/vm/overcommit_memory")
            .unwrap()
            .trim()
            == "2";
        if !strict {
            assert!(flags.contains(&"nr") && !flags.contains(&"ac"), "{flags:?}");
        }
    }

    /// Device memory a device gives its guest (D189) is reached by the host's devices as RAM
    /// is: by system calls (`io_ptr`) and by an access's copies; never by `host_ptr`, which
    /// the kernel and the hypervisor are given RAM by. A page of it whose file was cut short
    /// fails a copy, and a system call EFAULT, the process going on. It may overlap no other
    /// guest memory, and is given once.
    #[cfg(unix)]
    #[test]
    fn device_memory_is_reached_as_ram_is_and_its_lost_pages_fail() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::FileExt as _;
        let m = mem();
        let p = page_size().unwrap();
        let file = tempfile("device");
        file.set_len(4 * p as u64).unwrap();
        // SAFETY: a shared mapping of the file's four pages, unmapped at the test's end.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4 * p,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        let host = host.cast::<u8>();
        let at = 0x2_0000_0000u64;
        // SAFETY: the mapping above, which outlives every use of `m`'s.
        unsafe {
            assert!(m.give_devices([(0x8000_0000 + p as u64, host, p)]).is_err());
            assert!(
                m.give_devices([(at, host, 2 * p), (at + p as u64, host, p)])
                    .is_err()
            );
            m.give_devices([(at, host, 4 * p)]).unwrap();
            assert!(m.give_devices([(at + 8 * p as u64, host, p)]).is_err());
        }
        assert!(m.host_ptr(at, 8).is_err());
        assert_eq!(m.io_ptr(at + 8, 8).unwrap(), host.wrapping_add(8));
        assert!(m.io_ptr(at + 4 * p as u64 - 4, 8).is_err());
        m.access().unwrap().write(at + 8, b"device").unwrap();
        let mut back = [0u8; 6];
        m.access().unwrap().read(at + 8, &mut back).unwrap();
        assert_eq!(&back, b"device");
        let mut held = [0u8; 6];
        file.read_exact_at(&mut held, 8).unwrap();
        assert_eq!(&held, b"device");
        // Cut short under its mapping: copies from and to its lost pages fail, a system call
        // given one fails EFAULT, and the rest of guest memory is reached as before.
        file.set_len(p as u64).unwrap();
        assert!(m.access().unwrap().read(at + 2 * p as u64, &mut back).is_err());
        assert!(m.access().unwrap().write(at + 3 * p as u64, b"x").is_err());
        m.access().unwrap().read(at + 8, &mut back).unwrap();
        m.access().unwrap().write(0x8000_0000, b"ram").unwrap();
        let lost = m.io_ptr(at + 2 * p as u64, 16).unwrap();
        let mut fds = [0; 2];
        // SAFETY: pipe(2) into a two-element array; a write(2) of the lost page into it.
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            assert_eq!(libc::write(fds[1], lost.cast(), 16), -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EFAULT));
            libc::close(fds[0]);
            libc::close(fds[1]);
            libc::munmap(host.cast(), 4 * p);
        }
    }

    fn tempfile(tag: &str) -> std::fs::File {
        let path_dir = shards_testdir::TempDir::new(&format!("mem-{tag}")).unwrap();
        let path = path_dir.join("it");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let _ = std::fs::remove_file(path);
        file
    }
}
