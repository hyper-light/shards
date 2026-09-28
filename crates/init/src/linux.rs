//! Boot-path init: announce arrival to the VMM, report guest boot time, power off.

use std::ffi::CStr;
use std::io;

use shards_abi::{CONTROL_PAGE_AARCH64, marker};

pub fn main() {
    if let Err(e) = mount(c"devtmpfs", c"/dev", c"devtmpfs") {
        eprintln!("shards-init: mounting /dev: {e}");
    }
    if let Err(e) = mark(marker::INIT_STARTED) {
        eprintln!("shards-init: control page: {e}");
    }
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    println!(
        "shards-init: pid {} running {}.{:06}s after kernel entry",
        std::process::id(),
        ts.tv_sec,
        ts.tv_nsec / 1000
    );
    power_off();
}

fn mount(src: &CStr, target: &CStr, fstype: &CStr) -> io::Result<()> {
    // SAFETY: NUL-terminated strings; no mount data.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Writes `value` to the VMM's control page through /dev/mem.
fn mark(value: u32) -> io::Result<()> {
    // SAFETY: plain syscalls; the mapping is used for a single aligned volatile store and
    // unmapped before returning.
    unsafe {
        let fd = libc::open(
            c"/dev/mem".as_ptr(),
            libc::O_RDWR | libc::O_SYNC | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let page = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            CONTROL_PAGE_AARCH64 as libc::off_t,
        );
        libc::close(fd);
        if page == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        std::ptr::write_volatile(page.cast::<u32>(), value);
        libc::munmap(page, 4096);
    }
    Ok(())
}

fn power_off() -> ! {
    // SAFETY: PID 1 flushing filesystems and asking the kernel to power off (PSCI SYSTEM_OFF).
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // reboot() only returns on failure; PID 1 must never exit.
    eprintln!("shards-init: power off failed: {}", io::Error::last_os_error());
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}
