//! Guest physical memory.
//!
//! Guest RAM is host memory that running vCPUs write concurrently, so the VMM never
//! forms Rust references into it. Every access is a bounds-checked raw copy or an
//! atomic operation.

use std::fmt;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU16;

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

/// Guest RAM regions, each backed by a private anonymous host mapping.
#[derive(Debug)]
pub struct GuestMemory {
    regions: Vec<Region>,
}

// SAFETY: the mappings are plain memory owned by this value; all access is via raw
// copies/atomics that tolerate concurrent mutation by guest vCPUs.
unsafe impl Send for GuestMemory {}
// SAFETY: as above.
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Reserves lazily-populated anonymous memory for each `(gpa, len)` range. Pages are
    /// materialized on first touch, so untouched guest RAM costs no host memory.
    pub fn anonymous(ranges: &[(u64, usize)]) -> io::Result<GuestMemory> {
        let page = page_size();
        let mut regions: Vec<Region> = Vec::with_capacity(ranges.len());
        for &(gpa, len) in ranges {
            if len == 0 || len % page != 0 || gpa % page as u64 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guest RAM {gpa:#x}+{len:#x} is not host-page ({page:#x}) aligned"),
                ));
            }
            if regions.iter().any(|r| gpa < r.gpa + r.len as u64 && r.gpa < gpa + len as u64) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "overlapping guest RAM regions"));
            }
            // SAFETY: fresh private anonymous mapping; ownership moves into `regions`.
            let host = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            if host == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            regions.push(Region { gpa, len, host: NonNull::new(host.cast()).expect("mmap returned NULL") });
        }
        regions.sort_by_key(|r| r.gpa);
        Ok(GuestMemory { regions })
    }

    /// `(gpa, host pointer, len)` for each region, for stage-2 mapping.
    pub fn regions(&self) -> impl Iterator<Item = (u64, *mut u8, usize)> + '_ {
        self.regions.iter().map(|r| (r.gpa, r.host.as_ptr(), r.len))
    }

    /// Host address of `gpa..gpa+len`, which must lie within one region.
    pub fn host_ptr(&self, gpa: u64, len: usize) -> Result<*mut u8, OutOfBounds> {
        let oob = OutOfBounds { gpa, len: len as u64 };
        let end = gpa.checked_add(len as u64).ok_or(oob)?;
        let r = self.regions.iter().find(|r| gpa >= r.gpa && end <= r.gpa + r.len as u64).ok_or(oob)?;
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
        if gpa % 2 != 0 {
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
            // SAFETY: each region is a mapping we created and still own.
            unsafe { libc::munmap(r.host.as_ptr().cast(), r.len) };
        }
    }
}

pub fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> GuestMemory {
        let p = page_size();
        GuestMemory::anonymous(&[(0x8000_0000, 4 * p), (0x1_0000_0000, p)]).unwrap()
    }

    #[test]
    fn roundtrip_and_bounds() {
        let m = mem();
        let p = page_size() as u64;
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
        let p = page_size();
        assert!(GuestMemory::anonymous(&[(0x8000_0000, p + 1)]).is_err());
        assert!(GuestMemory::anonymous(&[(0x8000_0001, p)]).is_err());
        assert!(GuestMemory::anonymous(&[(0x8000_0000, 2 * p), (0x8000_0000 + p as u64, p)]).is_err());
    }

    #[test]
    fn atomic_view_is_aligned_only() {
        let m = mem();
        use std::sync::atomic::Ordering;
        m.atomic_u16(0x8000_0010).unwrap().store(0xabcd, Ordering::Release);
        assert_eq!(m.read_obj::<u16>(0x8000_0010).unwrap(), 0xabcd);
        assert!(m.atomic_u16(0x8000_0011).is_err());
    }
}
