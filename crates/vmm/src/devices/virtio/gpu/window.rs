//! The GPU's window on KVM (D201): its host side reserved and inaccessible, one memslot
//! over it, as a share's DAX window's (D189). A blob is mapped into it by its descriptor at
//! the place its guest chose, and unmapped by reserving that range again; KVM's stage 2
//! follows the host's mapping through its MMU notifiers (Documentation/virt/kvm/mmu.rst), a
//! dma-buf's pages (PFN-mapped, as NVIDIA's are) as well as memory of the host's own.

use std::os::fd::BorrowedFd;
use std::ptr::NonNull;

use super::{Pages, Window};

/// The window's host side: `len` bytes at `host`, seen by the guest at `gpa`.
#[derive(Debug)]
pub struct Reserved {
    gpa: u64,
    host: NonNull<u8>,
    len: usize,
}

// SAFETY: an address range this value alone maps into and out of; nothing of it is read
// or written through it.
unsafe impl Send for Reserved {}
// SAFETY: as for Send; each mapping is one mmap(2), which the kernel serializes.
unsafe impl Sync for Reserved {}

impl Reserved {
    /// `len` bytes of address space for a window at `gpa`, inaccessible until a blob is
    /// mapped there, given to the VM by `slot` (its memslot over them), which must hold
    /// them until this value drops.
    pub fn new(
        gpa: u64,
        len: u64,
        slot: impl FnOnce(*mut u8, usize) -> Result<(), String>,
    ) -> Result<Reserved, String> {
        let len = usize::try_from(len).map_err(|_| format!("a GPU window of {len} bytes"))?;
        let host = crate::platform::reserve(len).map_err(|e| format!("reserving the GPU's window: {e}"))?;
        let reserved = Reserved { gpa, host, len };
        // SAFETY: the range just reserved, which nothing else uses.
        unsafe { crate::platform::withhold(host, len) }.map_err(|e| format!("the GPU's window: {e}"))?;
        slot(host.as_ptr(), len)?;
        Ok(reserved)
    }

    /// Where `len` bytes at `gpa` are in the window's host side, if all of them are in it.
    fn at(&self, gpa: u64, len: usize) -> Result<NonNull<u8>, String> {
        let offset = gpa
            .checked_sub(self.gpa)
            .and_then(|o| usize::try_from(o).ok())
            .filter(|o| o.checked_add(len).is_some_and(|end| end <= self.len))
            .ok_or_else(|| format!("{len} bytes at {gpa:#x} are not within the GPU's window"))?;
        // SAFETY: within the reservation, as just checked.
        NonNull::new(unsafe { self.host.as_ptr().add(offset) }).ok_or_else(|| "a null window".to_string())
    }
}

impl Window for Reserved {
    unsafe fn map(&self, pages: Pages, gpa: u64, len: usize) -> Result<(), String> {
        let Pages::Fd(fd) = pages else {
            return Err("KVM's GPU window maps a blob by its descriptor".into());
        };
        let at = self.at(gpa, len)?;
        // SAFETY: the caller's descriptor, open for the call.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        // SAFETY: MAP_FIXED within the window's reservation, where the device mapped
        // nothing else (`Served::free`).
        unsafe { crate::platform::map_file_range(fd, 0, len, at, true) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        let at = self.at(gpa, len)?;
        // SAFETY: within the window's reservation: the blob's mapping there replaced.
        unsafe { crate::platform::withhold(at, len) }.map_err(|e| e.to_string())
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        // SAFETY: the reservation this value made, after the VM whose memslot it was.
        unsafe { crate::platform::release(self.host, self.len) };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// A blob's shared memory mapped into the window is seen there, both ways; unmapped,
    /// its range is inaccessible again, and the blob's memory keeps what was written; a
    /// range past the window, or a pointer, is refused.
    #[test]
    fn a_blob_is_mapped_into_the_window_by_its_descriptor() {
        let page = crate::platform::page_size().unwrap();
        let mut slotted = None;
        let window = Reserved::new(1 << 32, (4 * page) as u64, |host, len| {
            slotted = Some((host as usize, len));
            Ok(())
        })
        .unwrap();
        assert_eq!(slotted, Some((window.host.as_ptr() as usize, 4 * page)));
        let blob = tempfile();
        blob.set_len((2 * page) as u64).unwrap();
        let gpa = (1 << 32) + page as u64;
        // SAFETY: the blob's descriptor, open for the call.
        unsafe { window.map(Pages::Fd(blob.as_raw_fd()), gpa, 2 * page) }.unwrap();
        let at = window.at(gpa, 2 * page).unwrap().as_ptr();
        // SAFETY: the blob's mapping, just made.
        unsafe { at.add(page + 7).write(0x5a) };
        let mut read = [0u8; 1];
        std::os::unix::fs::FileExt::read_exact_at(&blob, &mut read, (page + 7) as u64).unwrap();
        assert_eq!(read, [0x5a]);
        std::os::unix::fs::FileExt::write_all_at(&blob, &[0xa5], 3).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { at.add(3).read() }, 0xa5);
        // SAFETY: a read of the mapping, which holds the blob's pages.
        unsafe { crate::platform::copy_checked(at.add(3), read.as_mut_ptr(), 1) }.unwrap();
        window.unmap(gpa, 2 * page).unwrap();
        // Inaccessible again, a read of it refused, while the blob keeps what was written.
        for offset in [3, page + 7] {
            // SAFETY: a read the fault handler returns from.
            let read = unsafe { crate::platform::copy_checked(at.add(offset), read.as_mut_ptr(), 1) };
            assert_eq!(read.unwrap_err().raw_os_error(), Some(libc::EFAULT));
        }
        std::os::unix::fs::FileExt::read_exact_at(&blob, &mut read, (page + 7) as u64).unwrap();
        assert_eq!(read, [0x5a]);
        assert!(window.at(gpa, 4 * page).is_err());
        // SAFETY: refused before anything is mapped.
        assert!(unsafe { window.map(Pages::At(at), gpa, page) }.is_err());
        // SAFETY: refused before anything is mapped.
        assert!(unsafe { window.map(Pages::Fd(blob.as_raw_fd()), 1 << 31, page) }.is_err());
    }

    fn tempfile() -> std::fs::File {
        let dir = shards_testdir::TempDir::new("gpu-window").unwrap();
        let path = dir.join("blob");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        file
    }
}
