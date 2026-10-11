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

use std::fmt;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::platform;
use crate::sync::lock;

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

#[derive(Debug)]
struct Region {
    gpa: u64,
    len: usize,
    host: NonNull<u8>,
}

impl Region {
    fn end(&self) -> u64 {
        self.gpa + self.len as u64 // cannot wrap: checked on creation
    }
}

/// Guest RAM regions, each backed by its own host reservation.
#[derive(Debug)]
pub struct GuestMemory {
    regions: Vec<Region>,
    /// Mapped from a file ([`from_file`](Self::from_file)): a page it has not touched holds
    /// the file's bytes, not zeros.
    file_backed: bool,
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
            file_backed: false,
            host: Mutex::new(()),
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
            mem.regions.push(Region { gpa, len, host });
        }
        mem.regions.sort_by_key(|r| r.gpa);
        Ok(mem)
    }

    /// Guest RAM backed copy-on-write by `file`, which holds each region in turn by guest
    /// address, as [`save`](Self::save) writes them, whatever order `ranges` lists them in
    /// (audit A21). Clones share every page none of them writes. Where the platform cannot
    /// map a file into reserved memory, the file is read in.
    ///
    /// The file must hold all of them, and must not change while the VM runs: pages the
    /// guest has not yet touched come from it.
    pub fn from_file(ranges: &[(u64, usize)], file: &File) -> io::Result<GuestMemory> {
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
        for r in &mem.regions {
            // SAFETY: a whole region we just reserved: page-aligned, unmapped by any
            // hypervisor, unreferenced; region offsets in the file are page-aligned sums.
            match unsafe { platform::map_file_private(file, offset, r.len, r.host) } {
                Ok(()) => mem.file_backed = true,
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                    mem.read_file(r.gpa, r.len, file, offset)?;
                }
                Err(e) => return Err(e),
            }
            offset += r.len as u64;
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
    /// machine whose vCPUs and devices are paused.
    pub fn save(&self, file: &File) -> io::Result<()> {
        let page = platform::page_size()?;
        let _held = self.access().map_err(io::Error::other)?;
        file.set_len(0)?;
        let mut offset = 0u64;
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
            // Where the run of used pages being gathered starts.
            let mut run = None;
            for (i, start) in (0..r.len).step_by(page).enumerate() {
                let skipped = untouched
                    .as_ref()
                    .is_some_and(|u| u.get(i).copied().unwrap_or(false));
                // SAFETY: the page at `start` lies in this region, which is page-aligned.
                let used = !skipped && unsafe { !is_zero(r.host.as_ptr().add(start), page) };
                match (run, used) {
                    (None, true) => run = Some(start),
                    (Some(first), false) => {
                        write_run(file, r, first..start, offset)?;
                        run = None;
                    }
                    _ => {}
                }
            }
            if let Some(first) = run {
                write_run(file, r, first..r.len, offset)?;
            }
            offset += r.len as u64;
        }
        file.set_len(offset)
    }

    /// `(gpa, host pointer, len)` for each region, for stage-2 mapping.
    pub fn regions(&self) -> impl Iterator<Item = (u64, *mut u8, usize)> + '_ {
        self.regions.iter().map(|r| (r.gpa, r.host.as_ptr(), r.len))
    }

    /// Host address of `gpa..gpa+len`, which must lie within one region: for the kernel
    /// and the hypervisor, and for an [`Access`].
    pub fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8, OutOfBounds> {
        let oob = OutOfBounds { gpa, len: len as u64 };
        let end = gpa.checked_add(len as u64).ok_or(oob)?;
        let r = self
            .regions
            .iter()
            .find(|r| gpa >= r.gpa && end <= r.end())
            .ok_or(oob)?;
        // SAFETY: offset is within the region's mapping (checked above).
        Ok(unsafe { r.host.as_ptr().add((gpa - r.gpa) as usize) })
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
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        for r in &self.regions {
            // SAFETY: each region is a reservation we made and still own.
            unsafe { platform::release(r.host, r.len) };
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

    /// Copies guest memory at `gpa` into `buf`.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), OutOfBounds> {
        // SAFETY: `buf` is valid for writes of its length, and nothing else accesses it.
        unsafe { self.read_raw(gpa, buf.as_mut_ptr(), buf.len()) }
    }

    /// Copies `buf` into guest memory at `gpa`.
    pub fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), OutOfBounds> {
        // SAFETY: `buf` is valid for reads of its length.
        unsafe { self.write_raw(gpa, buf.as_ptr(), buf.len()) }
    }

    /// Copies `len` bytes of guest memory at `gpa` to `dst`, memory no Rust reference
    /// covers: a record of a ring another process shares, written before it is published.
    ///
    /// # Safety
    /// `dst` must be valid for writes of `len` bytes, of host memory that nothing else
    /// accesses meanwhile.
    pub unsafe fn read_raw(&self, gpa: u64, dst: *mut u8, len: usize) -> Result<(), OutOfBounds> {
        let src = self.mem.host_ptr(gpa, len)?;
        // SAFETY: `src` is valid for `len` bytes of guest memory, which host memory like
        // `dst` never overlaps; `dst` as the caller promises.
        unsafe { platform::copy_in(src, dst, len) };
        Ok(())
    }

    /// Copies `len` bytes at `src`, memory no Rust reference covers, into guest memory at
    /// `gpa`.
    ///
    /// # Safety
    /// `src` must be valid for reads of `len` bytes of host memory.
    pub unsafe fn write_raw(&self, gpa: u64, src: *const u8, len: usize) -> Result<(), OutOfBounds> {
        let dst = self.mem.host_ptr(gpa, len)?;
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
fn write_run(file: &File, r: &Region, run: Range<usize>, offset: u64) -> io::Result<()> {
    if run.start > run.end || run.end > r.len {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let at = offset + run.start as u64;
    let mut done = 0;
    while done < run.len() {
        // SAFETY: `run.start + done .. run.end` lies in the region (checked above); the
        // kernel reads it.
        let n = unsafe {
            platform::write_at(
                file,
                r.host.as_ptr().add(run.start + done),
                run.len() - done,
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

        let a = GuestMemory::from_file(&ranges, &file).unwrap();
        let b = GuestMemory::from_file(&ranges, &file).unwrap();
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
        let c = GuestMemory::from_file(&ranges, &file).unwrap();
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

        let r = GuestMemory::from_file(&ranges, &file).unwrap();
        for page in 0..19 {
            let first = r.access().unwrap().read_obj::<u8>(gpa(page)).unwrap();
            let last = r
                .access()
                .unwrap()
                .read_obj::<u8>(gpa(page) + p as u64 - 1)
                .unwrap();
            assert_eq!((first, last), marks(page), "page {page}");
        }

        // The file holds data exactly where pages were used (lseek(2) SEEK_DATA, SEEK_HOLE).
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
        drop((m, r, file));
        let _ = std::fs::remove_file(path);
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
        let m = GuestMemory::from_file(&ranges, &file).unwrap();
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
            let r = GuestMemory::from_file(order, &file).unwrap();
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
        assert!(GuestMemory::from_file(&ranges, &file).is_err());
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
            let r = GuestMemory::from_file(&ranges, &file).unwrap();
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
        let r = GuestMemory::from_file(&[(0, len)], &file).unwrap();
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
        let r = GuestMemory::from_file(&[(0, len)], &file).unwrap();
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
