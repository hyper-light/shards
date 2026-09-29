//! Guest physical memory.
//!
//! Guest RAM is host memory that running vCPUs write concurrently, so the VMM never
//! forms Rust references into it. Every access is a bounds-checked raw copy or an
//! atomic operation.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU16;

use crate::platform;

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
}

// SAFETY: the reservations are plain memory owned by this value; all access is via raw
// copies/atomics that tolerate concurrent mutation by guest vCPUs.
unsafe impl Send for GuestMemory {}
// SAFETY: as above.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Reserves zero-filled memory for each `(gpa, len)` range. Pages are materialized on
    /// first touch, so untouched guest RAM costs no host memory.
    pub fn anonymous(ranges: &[(u64, usize)]) -> io::Result<GuestMemory> {
        let page = platform::page_size()?;
        let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidInput, msg);
        // Built incrementally so that Drop unmaps whatever was mapped if a later range fails.
        let mut mem = GuestMemory {
            regions: Vec::with_capacity(ranges.len()),
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

    /// Guest RAM backed copy-on-write by `file`, which holds each region in turn (as
    /// [`save`](Self::save) writes them). Clones share every page none of them writes.
    /// Where the platform cannot map a file into reserved memory, the file is read in.
    ///
    /// The file must not change while the VM runs: pages the guest has not yet touched
    /// come from it.
    pub fn from_file(ranges: &[(u64, usize)], file: &std::fs::File) -> io::Result<GuestMemory> {
        let mem = GuestMemory::anonymous(ranges)?;
        let mut offset = 0u64;
        for &(gpa, len) in ranges {
            let host = mem
                .host_ptr(gpa, len)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let host = NonNull::new(host).ok_or_else(|| io::Error::other("null guest pointer"))?;
            // SAFETY: a whole region we just reserved: page-aligned, unmapped by any
            // hypervisor, unreferenced; region offsets in the file are page-aligned sums.
            match unsafe { platform::map_file_private(file, offset, len, host) } {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                    // SAFETY: the region is ours and nothing else references it yet.
                    let region = unsafe { std::slice::from_raw_parts_mut(host.as_ptr(), len) };
                    platform::read_exact_at(file, region, offset)?;
                }
                Err(e) => return Err(e),
            }
            offset += len as u64;
        }
        Ok(mem)
    }

    /// Maps every region afresh from `file`, copy-on-write, as [`from_file`](Self::from_file)
    /// does: the guest reads what it did, but the host maps each page again only once
    /// it is touched, so what is mapped later is what was touched since.
    ///
    /// # Safety
    /// Every vCPU and device must be paused, nothing may reference the regions, `file`
    /// must hold what [`save`](Self::save) wrote of them, and the hypervisor must follow
    /// mapping changes (KVM, api.rst 4.35: "changes in the backing of the memory region
    /// are automatically reflected into the guest").
    pub unsafe fn remap(&self, file: &std::fs::File) -> io::Result<()> {
        let mut offset = 0u64;
        for r in &self.regions {
            // SAFETY: forwarded caller contract; the region is page-aligned (checked on
            // creation), and region offsets in the file are page-aligned sums.
            unsafe { platform::map_file_private(file, offset, r.len, r.host) }?;
            offset += r.len as u64;
        }
        Ok(())
    }

    /// Writes every region in turn to `file`, leaving all-zero pages as holes so the file
    /// stays as small as the memory the guest used. Every vCPU and device must be paused.
    pub fn save(&self, file: &std::fs::File) -> io::Result<()> {
        let page = platform::page_size()?;
        let mut offset = 0u64;
        for r in &self.regions {
            // SAFETY: the VM is paused (caller contract), so nothing writes this region
            // while the slice lives.
            let region = unsafe { std::slice::from_raw_parts(r.host.as_ptr(), r.len) };
            for (i, chunk) in region.chunks(page).enumerate() {
                if chunk.iter().all(|&b| b == 0) {
                    continue;
                }
                let at = offset + (i * page) as u64;
                let mut done = 0;
                while let Some(rest) = chunk.get(done..).filter(|c| !c.is_empty()) {
                    // SAFETY: `rest` is a live slice of `rest.len()` bytes.
                    let n = unsafe { platform::write_at(file, rest.as_ptr(), rest.len(), at + done as u64)? };
                    if n == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    done += n;
                }
            }
            offset += r.len as u64;
        }
        file.set_len(offset)
    }

    /// `(gpa, host pointer, len)` for each region, for stage-2 mapping.
    pub fn regions(&self) -> impl Iterator<Item = (u64, *mut u8, usize)> + '_ {
        self.regions.iter().map(|r| (r.gpa, r.host.as_ptr(), r.len))
    }

    /// Host address of `gpa..gpa+len`, which must lie within one region.
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

    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), OutOfBounds> {
        let src = self.host_ptr(gpa, buf.len())?;
        // SAFETY: `src` is valid for `buf.len()` bytes; guest memory never aliases `buf`.
        unsafe { std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    pub fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), OutOfBounds> {
        let dst = self.host_ptr(gpa, buf.len())?;
        // SAFETY: `dst` is valid for `buf.len()` bytes; guest memory never aliases `buf`.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len()) };
        Ok(())
    }

    pub fn read_obj<T: Pod>(&self, gpa: u64) -> Result<T, OutOfBounds> {
        let src = self.host_ptr(gpa, size_of::<T>())?;
        // SAFETY: in bounds; `T: Pod` accepts any bytes; unaligned read tolerated.
        Ok(unsafe { src.cast::<T>().read_unaligned() })
    }

    pub fn write_obj<T: Pod>(&self, gpa: u64, value: T) -> Result<(), OutOfBounds> {
        let dst = self.host_ptr(gpa, size_of::<T>())?;
        // SAFETY: in bounds; unaligned write tolerated.
        unsafe { dst.cast::<T>().write_unaligned(value) };
        Ok(())
    }

    /// An atomic view of the naturally aligned `u16` at `gpa` (virtqueue indices).
    pub fn atomic_u16(&self, gpa: u64) -> Result<&AtomicU16, OutOfBounds> {
        if !gpa.is_multiple_of(2) {
            return Err(OutOfBounds { gpa, len: 2 });
        }
        let p = self.host_ptr(gpa, 2)?;
        // SAFETY: aligned, in bounds, lives as long as `self`; atomics permit the guest's
        // concurrent accesses.
        Ok(unsafe { AtomicU16::from_ptr(p.cast()) })
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

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::platform::page_size;

    fn mem() -> GuestMemory {
        let p = page_size().unwrap();
        GuestMemory::anonymous(&[(0x8000_0000, 4 * p), (0x1_0000_0000, p)]).unwrap()
    }

    #[test]
    fn roundtrip_and_bounds() {
        let m = mem();
        let p = page_size().unwrap() as u64;
        m.write_obj(0x8000_0003u64, 0xdead_beef_u32).unwrap();
        assert_eq!(m.read_obj::<u32>(0x8000_0003).unwrap(), 0xdead_beef);
        // Last byte of a region is fine; one past is not; ranges may not straddle holes.
        m.write(0x8000_0000 + 4 * p - 1, &[7]).unwrap();
        assert!(m.write(0x8000_0000 + 4 * p, &[7]).is_err());
        assert!(m.read(0x8000_0000 + 4 * p - 2, &mut [0; 4]).is_err());
        assert!(m.read(0x7fff_ffff, &mut [0; 2]).is_err());
        assert!(m.host_ptr(u64::MAX - 1, 4).is_err());
        assert_eq!(m.read_obj::<u8>(0x1_0000_0000 + p - 1).unwrap(), 0);
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
        m.write(0x8000_0000 + p as u64 + 5, b"hello").unwrap();
        m.write(0x1_0000_0000 + 2 * p as u64 - 1, &[0xee]).unwrap();
        let path = std::env::temp_dir().join(format!("shards-mem-{}", std::process::id()));
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
        a.read(0x8000_0000 + p as u64 + 5, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(a.read_obj::<u8>(0x1_0000_0000 + 2 * p as u64 - 1).unwrap(), 0xee);
        assert_eq!(a.read_obj::<u64>(0x8000_0000).unwrap(), 0);
        // Writes stay private to each copy and never reach the file.
        a.write(0x8000_0000 + p as u64 + 5, b"HELLO").unwrap();
        b.read(0x8000_0000 + p as u64 + 5, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        drop((a, b));
        let c = GuestMemory::from_file(&ranges, &file).unwrap();
        c.read(0x8000_0000 + p as u64 + 5, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    /// What working sets record on Linux (vm::x86_64::record): once remapped, memory maps
    /// only what is touched from then on, and a page written is a private copy.
    #[cfg(target_os = "linux")]
    #[test]
    fn remapped_memory_maps_only_what_is_touched_after() {
        let p = page_size().unwrap();
        let base = 0x8000_0000u64;
        let at = |page: usize| base + (page * p) as u64;
        let m = GuestMemory::anonymous(&[(base, 64 * p)]).unwrap();
        for page in 0..64 {
            m.write(at(page), &[page as u8 + 1]).unwrap();
        }
        let path = std::env::temp_dir().join(format!("shards-remap-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        m.save(&file).unwrap();
        // SAFETY: nothing else uses `m`, and `file` holds what it saved.
        unsafe { m.remap(&file) }.unwrap();
        let (_, host, len) = m.regions().next().unwrap();
        assert_eq!(platform::mapped_pages(host, len).unwrap(), []);

        // A read maps its page and pages around it (fault-around, 16 by default: 64 KiB):
        // from the start of its 16-page block of addresses, or of the mapping if that is
        // later, 16 pages on, within its page table and the mapping (mm/memory.c
        // do_fault_around). A write maps its page alone, a private copy.
        assert_eq!(m.read_obj::<u8>(at(5)).unwrap(), 6);
        m.write(at(40), &[0xff]).unwrap();
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
        assert_eq!(m.read_obj::<u8>(at(51)).unwrap(), 52);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn atomic_view_is_aligned_only() {
        let m = mem();
        use std::sync::atomic::Ordering;
        m.atomic_u16(0x8000_0010)
            .unwrap()
            .store(0xabcd, Ordering::Release);
        assert_eq!(m.read_obj::<u16>(0x8000_0010).unwrap(), 0xabcd);
        assert!(m.atomic_u16(0x8000_0011).is_err());
    }
}
