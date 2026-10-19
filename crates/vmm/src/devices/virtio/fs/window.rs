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
//! A file the host cuts short under a mapping leaves pages of it past the file's end, which
//! the host cannot give: the guest's access there, before it learns the file's size, ends
//! its vCPU's run (KVM: EFAULT; HVF: an abort the VMM cannot complete, PM M271). The window
//! gives zeros there instead ([`Window::fill_past_end`]), and the file again once the file
//! has grown back past them ([`Window::refill`]).

use std::collections::BTreeMap;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Where in the range zeros stand in for the file, its pages from there on wholly past
    /// the file's end as the host cut it short ([`Window::fill_past_end`]).
    zeros_from: Option<u64>,
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
    /// Whether a range may hold zeros: [`Window::refill`] has something to look at.
    zeroed: AtomicBool,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("gpa", &self.gpa)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
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

/// Where in `e`'s range its pages lie wholly past its file's end, from a page boundary;
/// none where the file reaches its last page.
fn past_end(e: &Entry, page: u64) -> Option<u64> {
    let end = size_of(e.file.as_ref()?)?
        .saturating_sub(e.mapped.foffset)
        .checked_next_multiple_of(page)?;
    (end < e.mapped.len).then_some(end)
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
            zeroed: AtomicBool::new(false),
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

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Entry>> {
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
    /// the guest only if `writable`, replacing what was there: a FUSE_SETUPMAPPING's. An
    /// errno where the range lies outside the window or the host refuses the mapping.
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
        let at = std::ptr::NonNull::new(host).ok_or(libc::EINVAL)?;
        let mut mapped = self.lock();
        self.forget(&mut mapped, moffset, len);
        // SAFETY: within the window's reservation, which nothing else uses: what was mapped
        // there is replaced, and was unmapped at stage 2 first.
        if let Err(e) = unsafe { crate::platform::map_file_range(file.as_fd(), foffset, size, at, writable) }
        {
            let errno = super::host::linux_errno(e.raw_os_error().unwrap_or(libc::EIO));
            // Inaccessible again, as before.
            let _ = reserve(host, size);
            return Err(errno);
        }
        let gpa = self.gpa.checked_add(moffset).ok_or(libc::EINVAL)?;
        // SAFETY: the file's mapping stays until this window replaces it, or the VM, which
        // goes before its devices, is destroyed.
        if let Err(e) = unsafe { self.stage2.map(host, gpa, size, writable) } {
            crate::debug!("virtio-fs: mapping a DAX range at {gpa:#x}: {e}");
            let _ = reserve(host, size);
            return Err(libc::EIO);
        }
        let mapped_now = Mapped {
            len,
            node,
            foffset,
            writable,
        };
        mapped.insert(
            moffset,
            Entry {
                mapped: mapped_now,
                file: Some(file),
                zeros_from: None,
            },
        );
        Ok(())
    }

    /// Zeros over `len` bytes at window offset `moffset`, which the guest holds as node
    /// `node`'s from `foffset`, where its file can be given no more: a restore whose server
    /// reaches the file a save mapped no longer (D176), as past a file's end (M271), so
    /// that the guest's access there finds memory, never a fault no window answers.
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
        self.forget(&mut mapped, moffset, len);
        if let Err(e) = zeros(host, size, writable) {
            let _ = reserve(host, size);
            return Err(super::host::linux_errno(e.raw_os_error().unwrap_or(libc::ENOMEM)));
        }
        // SAFETY: anonymous memory the window holds until it replaces it.
        if let Err(e) = unsafe { self.stage2.map(host, gpa, size, writable) } {
            crate::debug!("virtio-fs: mapping zeros at {gpa:#x}: {e}");
            let _ = reserve(host, size);
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
                zeros_from: Some(0),
            },
        );
        Ok(())
    }

    /// Unmaps `len` bytes at window offset `moffset`: a FUSE_REMOVEMAPPING's. An errno where
    /// the range lies outside the window.
    pub fn unmap(&self, moffset: u64, len: u64) -> Result<(), i32> {
        let (host, size) = self.place(moffset, len).ok_or(libc::EINVAL)?;
        let mut mapped = self.lock();
        self.forget(&mut mapped, moffset, len);
        reserve(host, size)
            .map(|_| ())
            .map_err(|e| super::host::linux_errno(e.raw_os_error().unwrap_or(libc::EIO)))
    }

    /// Unmaps, at stage 2 and in the host, and forgets, every mapping that meets `len`
    /// bytes at `moffset`, each whole: one a new mapping overlaps in part leaves nothing of
    /// it reachable (Linux maps whole 2 MiB ranges, which never overlap; a guest of another
    /// kind may).
    fn forget(&self, mapped: &mut BTreeMap<u64, Entry>, moffset: u64, len: u64) {
        let end = moffset.saturating_add(len);
        let meeting: Vec<(u64, u64)> = mapped
            .range(..end)
            .filter(|(at, e)| at.saturating_add(e.mapped.len) > moffset)
            .map(|(at, e)| (*at, e.mapped.len))
            .collect();
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
            mapped.remove(&at);
        }
    }

    /// Gives zeros in place of the file where a mapped range's pages lie wholly past its
    /// file's end, the host having cut the file short under the guest's mapping: the
    /// guest's access there, which faulted, runs again, and reads zeros until the guest
    /// learns the file's size. `gpa` is where it faulted, where the hypervisor says (HVF;
    /// KVM's EFAULT says nothing, and every range is looked at). True where zeros were
    /// given; false where no range lay past its file's end, the fault no window's.
    pub fn fill_past_end(&self, gpa: Option<u64>) -> bool {
        let Some(page) = crate::platform::page_size()
            .ok()
            .and_then(|p| u64::try_from(p).ok())
        else {
            return false;
        };
        let mut mapped = self.lock();
        let mut filled = false;
        for (&at, e) in mapped.iter_mut() {
            let start = self.gpa.saturating_add(at);
            if gpa.is_some_and(|g| g < start || g >= start.saturating_add(e.mapped.len)) {
                continue;
            }
            let Some(from) = past_end(e, page) else {
                continue;
            };
            if e.zeros_from.is_some_and(|z| z <= from) {
                continue;
            }
            let Some((host, size)) = at
                .checked_add(from)
                .and_then(|off| self.host_at(off, e.mapped.len.saturating_sub(from)))
            else {
                continue;
            };
            let gpa = start.saturating_add(from);
            if let Err(err) = self.stage2.unmap(gpa, size) {
                crate::debug!("virtio-fs: unmapping a DAX range's end at {gpa:#x}: {err}");
            }
            if let Err(err) = zeros(host, size, e.mapped.writable) {
                crate::debug!("virtio-fs: zeros past a file's end at {gpa:#x}: {err}");
                continue;
            }
            // SAFETY: anonymous memory the window holds until it maps the range again.
            if let Err(err) = unsafe { self.stage2.map(host, gpa, size, e.mapped.writable) } {
                crate::debug!("virtio-fs: mapping zeros at {gpa:#x}: {err}");
                continue;
            }
            e.zeros_from = Some(from);
            filled = true;
        }
        if filled {
            self.zeroed.store(true, Ordering::Release);
        }
        filled
    }

    /// Maps each range given zeros ([`fill_past_end`](Self::fill_past_end)) from its file
    /// again where the file has grown back past them: called before the guest is told
    /// anything of its files (each reply), so that once it may read there, it reads the
    /// file.
    pub fn refill(&self) {
        if !self.zeroed.load(Ordering::Acquire) {
            return;
        }
        let mut mapped = self.lock();
        let mut left = false;
        for (&at, e) in mapped.iter_mut() {
            // Zeros for no file have none to grow back.
            let (Some(from), Some(file)) = (e.zeros_from, e.file.as_ref()) else {
                continue;
            };
            let grown = size_of(file).is_some_and(|n| n.saturating_sub(e.mapped.foffset) > from);
            let place = self.host_at(at, e.mapped.len);
            let (true, Some((host, size)), Some(nn)) =
                (grown, place, place.and_then(|(h, _)| std::ptr::NonNull::new(h)))
            else {
                left = true;
                continue;
            };
            let gpa = self.gpa.saturating_add(at);
            if let Err(err) = self.stage2.unmap(gpa, size) {
                crate::debug!("virtio-fs: unmapping a DAX range at {gpa:#x}: {err}");
            }
            // SAFETY: within the window's reservation, unmapped at stage 2 above.
            let again = unsafe {
                crate::platform::map_file_range(file.as_fd(), e.mapped.foffset, size, nn, e.mapped.writable)
            };
            // SAFETY: the file's mapping, or the zeros left where it failed, stay until the
            // window replaces them.
            let staged = unsafe { self.stage2.map(host, gpa, size, e.mapped.writable) };
            match (again, staged) {
                (Ok(()), Ok(())) => e.zeros_from = None,
                (again, staged) => {
                    crate::debug!("virtio-fs: mapping a grown file at {gpa:#x}: {again:?} {staged:?}");
                    left = true;
                }
            }
        }
        self.zeroed.store(left, Ordering::Release);
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
    /// in part replaces it whole; and unmapped, it is forgotten.
    #[test]
    fn a_files_ranges_are_mapped_into_the_window_and_out() {
        let dir = shards_testdir::TempDir::new("dax-window").unwrap();
        let path = dir.join("file");
        let bytes: Vec<u8> = (0..8 * MIB).map(pattern).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let fd = || OwnedFd::from(file.try_clone().unwrap());
        let told = Arc::new(Told::default());
        let w = window(&told);
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
        w.unmap(2 * MIB, 4 * MIB).unwrap();
        assert!(w.mappings().is_empty());
        assert_eq!(w.unmap(15 * MIB, 2 * MIB), Err(libc::EINVAL));
    }

    /// A file cut short on the host under a mapping: a fault in the range gives zeros where
    /// its pages lie wholly past the file's end, the rest the file's still, and once only;
    /// a fault in no range of a file cut short is none of the window's; and grown back,
    /// the range is the file's again, its new bytes and all.
    #[test]
    fn zeros_stand_in_past_a_cut_files_end_until_it_grows_back() {
        let dir = shards_testdir::TempDir::new("dax-cut").unwrap();
        let path = dir.join("file");
        let bytes: Vec<u8> = (0..8 * MIB).map(pattern).collect();
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
            false,
        )
        .unwrap();
        assert!(!w.fill_past_end(None), "nothing is past the file's end");
        // Cut to 3 MiB and a byte: the range's pages from the next page past its first MiB
        // and a byte lie past the end.
        file.set_len(3 * MIB + 1).unwrap();
        let page = crate::platform::page_size().unwrap() as u64;
        let from = (MIB + 1).next_multiple_of(page);
        told.0.lock().unwrap().clear();
        assert!(!w.fill_past_end(Some(GPA)), "no range holds the fault");
        assert!(w.fill_past_end(Some(GPA + 4 * MIB + from)));
        let rest = (2 * MIB - from) as usize;
        assert_eq!(
            *told.0.lock().unwrap(),
            [
                (false, GPA + 4 * MIB + from, rest),
                (true, GPA + 4 * MIB + from, rest)
            ]
        );
        assert!(!w.fill_past_end(None), "given zeros once");
        // SAFETY: the window's own range: the file up to `from`, zeros after it.
        let seen = unsafe { std::slice::from_raw_parts(w.host().add(4 << 20), 2 << 20) };
        let from = from as usize;
        assert!(
            seen[..=MIB as usize]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == pattern(2 * MIB + i as u64))
        );
        assert!(seen[from..].iter().all(|&b| b == 0));
        // Not grown past the zeros: they stay.
        w.refill();
        assert!(seen[from..].iter().all(|&b| b == 0));
        // Grown back with new bytes past them: the file's again.
        let grown: Vec<u8> = (0..MIB).map(|i| pattern(i) ^ 0x5a).collect();
        crate::platform::write_all_at(&file, &grown, 3 * MIB).unwrap();
        file.set_len(8 * MIB).unwrap();
        w.refill();
        assert_eq!(&seen[MIB as usize..2 * MIB as usize], &grown[..]);
        assert!(!w.fill_past_end(None));
    }
}
