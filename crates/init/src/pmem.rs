//! The VM's pmem devices, mounted once libnvdimm has made them.
//!
//! libnvdimm registers a pmem device's region and namespace asynchronously, in a domain of
//! its own that the kernel leaves out when it waits for its asynchronous work before init
//! runs (drivers/nvdimm/bus.c, `nd_async_domain`, an `ASYNC_DOMAIN_EXCLUSIVE`, which
//! `async_synchronize_full` does not wait for). An image of gigabytes, whose region takes
//! longest to map, was still probing as init mounted it: its runs found no `/dev/pmem0`
//! (ENOENT), or a node whose disk was not there yet (ENXIO). Reading a bus's `wait_probe`
//! returns once its probing is done (drivers/nvdimm/core.c, wait_probe_show), as ndctl
//! waits for it.

use std::ffi::CString;
use std::io;
use std::path::Path;

/// Mounts the EROFS on pmem `device` at `target`, read-only, with `data`. Where libnvdimm
/// has not made its node yet (ENOENT), or the disk behind it (ENXIO), waits for its
/// probing to end and mounts once more. `sys` is where sysfs is mounted; none, before the
/// root is, where it is mounted for the wait alone.
pub fn mount_erofs(device: &str, target: &str, data: &str, sys: Option<&Path>) -> io::Result<()> {
    match mount(device, target, data) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENXIO)) => {
            match sys {
                Some(sys) => await_probe(sys),
                None => with_sysfs(await_probe)?,
            }
            mount(device, target, data)
        }
        done => done,
    }
}

fn mount(device: &str, target: &str, data: &str) -> io::Result<()> {
    let c = |s: &str| CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    let (device, target, data) = (c(device)?, c(target)?, c(data)?);
    // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
    let mounted = unsafe {
        libc::mount(
            device.as_ptr(),
            target.as_ptr(),
            c"erofs".as_ptr(),
            libc::MS_RDONLY,
            data.as_ptr().cast(),
        )
    };
    if mounted != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits for each nd bus's probing to end, as ndctl waits for it.
fn await_probe(sys: &Path) {
    let Ok(buses) = std::fs::read_dir(sys.join("bus/nd/devices")) else {
        return;
    };
    for bus in buses.flatten() {
        if bus.file_name().as_encoded_bytes().starts_with(b"ndbus") {
            let _ = std::fs::read(bus.path().join("wait_probe"));
        }
    }
}

/// Runs `then` with sysfs mounted for it alone, at `/nd`.
fn with_sysfs(then: impl FnOnce(&Path)) -> io::Result<()> {
    const AT: &str = "/nd";
    std::fs::create_dir_all(AT)?;
    let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
    if unsafe {
        libc::mount(
            c"sysfs".as_ptr(),
            c"/nd".as_ptr(),
            c"sysfs".as_ptr(),
            flags,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    then(Path::new(AT));
    // SAFETY: umount2(2) of the mount above, by a NUL-terminated path.
    unsafe { libc::umount2(c"/nd".as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir(AT);
    Ok(())
}
