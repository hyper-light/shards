//! A run's swap (D159): the microVM's swap window, a virtio-pmem device its host maps a
//! run's swap file into, which the guest's CPUs write as memory. Its kernel completes each
//! swap write before reclaim goes on (pmem is BLK_FEAT_SYNCHRONOUS), so reclaim under the
//! run's memory limit never waits on the host.
//!
//! The window is hidden as the kernel boots (its transport reads as no device): probed
//! then, libnvdimm would give it page structures, a 64th of it in memory
//! (drivers/nvdimm/pmem.c, pmem_should_map_pages). Init shows it before the root is
//! mounted, with libnvdimm's drivers kept from binding of themselves, and has its
//! namespace probed raw (`force_raw`, Documentation/ABI/testing/sysfs-bus-nd): a block
//! device whose pages are mapped and never described, kept open by its node in the
//! VM's devtmpfs. Each run swaps to it as its host has mapped the run's file there.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::OnceLock;

use shards_abi::control;

/// The window's block device, its devtmpfs node held `O_PATH`, once shown and probed, or
/// why it could not be: what each run's swap is made on, in init's memory and descriptors,
/// which a template keeps.
static WINDOW: OnceLock<Result<OwnedFd, String>> = OnceLock::new();

/// Where sysfs is mounted while the window is shown: by init, before anything else
/// mounts it, so that it is writable (a network namespace's sysfs is one superblock,
/// read-only where its first mount was: kernfs, sysfs_init_fs_context).
const SYS: &str = "/swapsys";

/// Shows the microVM its swap window, where it has one, and has its kernel probe it raw:
/// the window's virtio-mmio transport bound, then its region and its namespace, raw. A
/// microVM without one has nothing to do. What failed is kept, for the runs that would
/// swap to it to say.
pub fn show_window() -> Result<(), String> {
    let shown = show();
    if let Err(e) = &shown {
        let _ = WINDOW.set(Err(e.clone()));
    }
    shown
}

fn show() -> Result<(), String> {
    let len = crate::linux::control_read64(control::SWAP).map_err(|e| format!("the control page: {e}"))?;
    if len == 0 {
        return Ok(());
    }
    std::fs::create_dir_all(SYS).map_err(|e| format!("mkdir {SYS}: {e}"))?;
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // SAFETY: mount(2) of NUL-terminated literals.
    if unsafe {
        libc::mount(
            c"sysfs".as_ptr(),
            c"/swapsys".as_ptr(),
            c"sysfs".as_ptr(),
            flags,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(format!("mounting sysfs: {}", io::Error::last_os_error()));
    }
    let sys = Path::new(SYS);
    let shown = probe_raw(sys, len);
    // libnvdimm's drivers bind of themselves again, whatever came of it.
    let _ = write(sys, "bus/nd/drivers_autoprobe", "1");
    // SAFETY: umount2(2) of the mount above, by a NUL-terminated path.
    unsafe { libc::umount2(c"/swapsys".as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir(SYS);
    let disk = shown?;
    let node = crate::devices::device_handle(&disk).map_err(|e| format!("/dev/{disk}: {e}"))?;
    let _ = WINDOW.set(Ok(node));
    Ok(())
}

/// Shows the window of `len` bytes and probes it raw, through sysfs at `sys`; returns its
/// block device's name.
fn probe_raw(sys: &Path, len: u64) -> Result<String, String> {
    write(sys, "bus/nd/drivers_autoprobe", "0")?;
    crate::linux::control_write(control::SWAP, control::SHOW_SWAP)
        .map_err(|e| format!("the control page: {e}"))?;
    // Its transport is the one the kernel passed over: a platform device no driver has.
    // Every such device is offered to virtio-mmio, which takes the window alone (bus.c,
    // bind_store: a driver that does not match refuses).
    for name in names(sys, "bus/platform/devices")? {
        if !sys
            .join("bus/platform/devices")
            .join(&name)
            .join("driver")
            .exists()
        {
            let _ = write(sys, "bus/platform/drivers/virtio-mmio/bind", &name);
        }
    }
    // virtio_pmem registers the window's nd bus and region as it binds, the region
    // asynchronously (drivers/nvdimm/bus.c, nd_device_register).
    await_probe(sys);
    let region = names(sys, "bus/nd/devices")?
        .into_iter()
        .filter(|n| n.starts_with("region"))
        .find(|n| {
            let dir = sys.join("bus/nd/devices").join(n);
            !dir.join("driver").exists()
                && std::fs::read_to_string(dir.join("size")).is_ok_and(|s| s.trim() == len.to_string())
        })
        .ok_or("the window's region did not come")?;
    write(sys, "bus/nd/drivers/nd_region/bind", &region)?;
    await_probe(sys);
    // Its namespace, `namespaceR.N`, beside the region's attributes (`namespace_seed`).
    let namespace = names(sys, &format!("bus/nd/devices/{region}"))?
        .into_iter()
        .find(|n| n.strip_prefix("namespace").is_some_and(|r| r.contains('.')))
        .ok_or("the window's namespace did not come")?;
    write(sys, &format!("bus/nd/devices/{namespace}/force_raw"), "1")?;
    write(sys, "bus/nd/drivers/nd_pmem/bind", &namespace)?;
    names(sys, &format!("bus/nd/devices/{namespace}/block"))?
        .into_iter()
        .next()
        .ok_or_else(|| "the window's disk did not come".to_string())
}

/// Waits for each nd bus's probing to end, as ndctl waits for it (drivers/nvdimm/core.c,
/// wait_probe_show).
fn await_probe(sys: &Path) {
    for bus in names(sys, "bus/nd/devices").unwrap_or_default() {
        if bus.starts_with("ndbus") {
            let _ = std::fs::read(sys.join("bus/nd/devices").join(bus).join("wait_probe"));
        }
    }
}

/// The names in the directory `path` beneath `sys`.
fn names(sys: &Path, path: &str) -> Result<Vec<String>, String> {
    let dir = std::fs::read_dir(sys.join(path)).map_err(|e| format!("{path}: {e}"))?;
    Ok(dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect())
}

/// Writes `value` to the attribute `path` beneath `sys`, in one write, as an attribute
/// takes a value.
fn write(sys: &Path, path: &str, value: &str) -> Result<(), String> {
    std::fs::write(sys.join(path), value).map_err(|e| format!("{path} {value}: {e}"))
}

/// Makes the window's first `len` bytes, the run's swap file its host mapped there, the
/// microVM's swap: a swap header on the first page (include/linux/swap.h, union
/// swap_header: version 1, its last page, no bad pages, "SWAPSPACE2" at the page's end),
/// written past the page cache, which may hold the page as the template's kernel read it;
/// then swapon(2). Both through the node init holds, by its descriptor's path: no node is
/// made, and none is the workload's.
pub fn on(len: u64) -> Result<(), String> {
    let node = match WINDOW.get() {
        Some(Ok(node)) => node,
        Some(Err(e)) => return Err(format!("its swap window: {e}")),
        None => return Err("its guest was given no swap window".into()),
    };
    let path = format!("/proc/self/fd/{}", node.as_raw_fd());
    // SAFETY: sysconf(3) of a constant.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).map_err(|_| "the page size")?;
    let last_page = u32::try_from((len / page as u64).saturating_sub(1)).unwrap_or(u32::MAX);
    if last_page == 0 {
        return Err(format!("{len} bytes of swap hold no page past its header"));
    }
    let mut header = Page::new(page)?;
    let bytes = header.bytes_mut();
    for (at, value) in [(1024, 1u32), (1028, last_page), (1032, 0)] {
        bytes
            .get_mut(at..at + 4)
            .ok_or("a page too small for a swap header")?
            .copy_from_slice(&value.to_ne_bytes());
    }
    bytes
        .get_mut(page - 10..)
        .ok_or("a page too small for a swap header")?
        .copy_from_slice(b"SWAPSPACE2");
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| format!("its device: {e}"))?;
    write_page(&device, header.bytes_mut()).map_err(|e| format!("its header: {e}"))?;
    drop(device);
    let c = CString::new(path).map_err(|_| "its device's path")?;
    // SAFETY: swapon(2) of a NUL-terminated path, with no flags.
    if unsafe { libc::swapon(c.as_ptr(), 0) } != 0 {
        return Err(format!("swapon: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Writes `page` at the start of `file`, whole.
fn write_page(file: &File, page: &[u8]) -> io::Result<()> {
    // SAFETY: pwrite(2) of a buffer that outlives the call, its length given.
    let n = unsafe { libc::pwrite(file.as_raw_fd(), page.as_ptr().cast(), page.len(), 0) };
    match usize::try_from(n) {
        Ok(n) if n == page.len() => Ok(()),
        Ok(n) => Err(io::Error::other(format!("{n} of {} bytes written", page.len()))),
        Err(_) => Err(io::Error::last_os_error()),
    }
}

/// One page of zeros, aligned to a page, as O_DIRECT writes from (open(2), O_DIRECT).
struct Page {
    ptr: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}

impl Page {
    fn new(size: usize) -> Result<Page, String> {
        let layout = std::alloc::Layout::from_size_align(size, size).map_err(|e| format!("a page: {e}"))?;
        // SAFETY: a layout of non-zero size (a page).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(ptr).ok_or("no memory for a page")?;
        Ok(Page { ptr, layout })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the allocation `new` made, `layout.size()` bytes, owned here alone.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: the allocation `new` made with this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}
