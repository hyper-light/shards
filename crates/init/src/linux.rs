//! PID 1's start: announce arrival to the VMM, then run a workload in an image (run.rs),
//! or report the guest's uptime and power off (the boot benchmark).

use std::ffi::CStr;
use std::io::{self, Write};

use shards_abi::{CONTROL_PAGE, marker};

pub fn main() {
    if let Err(e) = mount(c"devtmpfs", c"/dev", c"devtmpfs") {
        let _ = writeln!(io::stderr(), "shards-init: mounting /dev: {e}");
    }
    if let Err(e) = mark(marker::INIT_STARTED) {
        let _ = writeln!(io::stderr(), "shards-init: control page: {e}");
    }
    // `shards_root=<device>` on the kernel command line: boot into the image on that
    // device and run the host's workload in it.
    if let Some(device) = std::env::var_os("shards_root") {
        crate::run::main(&device.to_string_lossy())
    }
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    // Uptime starts at the kernel's timekeeping init, after early boot; the VMM's marker
    // timestamps cover everything from kernel entry.
    let _ = writeln!(
        io::stdout(),
        "shards-init: pid {} running at uptime {}.{:06}s",
        std::process::id(),
        ts.tv_sec,
        ts.tv_nsec / 1000
    );
    // `shards_dmesg=1` on the kernel command line reaches PID 1 as an environment
    // variable: dump the kernel log (boot analysis with `quiet initcall_debug`).
    if std::env::var_os("shards_dmesg").is_some() {
        dump_kernel_log();
    }
    power_off();
}

fn dump_kernel_log() {
    const SYSLOG_ACTION_READ_ALL: libc::c_int = 3;
    const SYSLOG_ACTION_SIZE_BUFFER: libc::c_int = 10;
    // SAFETY: klogctl writes at most `len` bytes into the buffer.
    let log = unsafe {
        let len = libc::klogctl(SYSLOG_ACTION_SIZE_BUFFER, std::ptr::null_mut(), 0);
        let mut buf = vec![0u8; len.max(0) as usize];
        let n = libc::klogctl(SYSLOG_ACTION_READ_ALL, buf.as_mut_ptr().cast(), len);
        buf.truncate(n.max(0) as usize);
        buf
    };
    let _ = io::stdout().write_all(&log);
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
            CONTROL_PAGE as libc::off_t,
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

pub(crate) fn power_off() -> ! {
    // SAFETY: PID 1 flushing filesystems and asking the kernel to power off (PSCI SYSTEM_OFF).
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // reboot() only returns on failure; PID 1 must never exit.
    let _ = writeln!(
        io::stderr(),
        "shards-init: power off failed: {}",
        io::Error::last_os_error()
    );
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}
