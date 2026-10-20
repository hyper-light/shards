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

/// Mounts the run's writable layer at `target` (D190), where the microVM has one: the
/// ext4 file system on the pmem device the control page names by its address
/// (`control::LAYER`), with DAX, so that what the run writes is the host's memory of its
/// file, never this guest's. Its kernel made the device's page structures in the device as
/// it probed it, from the pfn info block the host wrote there (drivers/nvdimm/pfn_devs.c).
/// Returns whether it has one; an error where it has one that does not mount.
pub fn mount_layer(target: &str) -> Result<bool, String> {
    let gpa = crate::linux::control_read64(shards_abi::control::LAYER)
        .map_err(|e| format!("the control page: {e}"))?;
    if gpa == 0 {
        return Ok(false);
    }
    let mut found = Err(format!("no pmem region at {gpa:#x}"));
    with_sysfs(|sys| {
        await_probe(sys);
        found = layer_disk(sys, gpa);
    })
    .map_err(|e| format!("sysfs: {e}"))?;
    let disk = found?;
    let c = |s: &str| CString::new(s).map_err(|_| format!("{s:?}: a NUL in it"));
    let (device, target_c) = (c(&format!("/dev/{disk}"))?, c(target)?);
    // noinit_itable: its host made every inode table zeros as it made the file system
    // (shards-ext4, EXT4_BG_INODE_ZEROED), which nothing need write again.
    // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
    let mounted = unsafe {
        libc::mount(
            device.as_ptr(),
            target_c.as_ptr(),
            c"ext4".as_ptr(),
            0,
            c"dax=always,noinit_itable".as_ptr().cast(),
        )
    };
    if mounted != 0 {
        return Err(format!(
            "mounting /dev/{disk} on {target}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(true)
}

/// The block device of the pmem region at `gpa`: its pfn device's, which claims its
/// namespace once libnvdimm finds the info block.
fn layer_disk(sys: &Path, gpa: u64) -> Result<String, String> {
    let devices = sys.join("bus/nd/devices");
    let names = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    };
    let region = names(&devices)
        .into_iter()
        .filter(|n| n.starts_with("region"))
        .find(|n| {
            std::fs::read_to_string(devices.join(n).join("resource"))
                .ok()
                .and_then(|r| u64::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
                == Some(gpa)
        })
        .ok_or_else(|| format!("no pmem region at {gpa:#x}"))?;
    names(&devices.join(&region))
        .into_iter()
        .filter(|n| n.starts_with("pfn"))
        .find_map(|pfn| names(&devices.join(&pfn).join("block")).into_iter().next())
        .ok_or_else(|| format!("{region}, at {gpa:#x}: no disk of page structures in its device"))
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
