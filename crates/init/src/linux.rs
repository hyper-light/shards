//! PID 1's start: announce arrival to the VMM, then run a workload in an image (run.rs),
//! or report the guest's uptime and power off (the boot benchmark).

use std::ffi::CStr;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use shards_abi::{CONTROL_PAGE, control, marker};

pub fn main() {
    if let Err(e) = mount(c"devtmpfs", c"/dev", c"devtmpfs") {
        let _ = writeln!(io::stderr(), "shards-init: mounting /dev: {e}");
    } else if let Err(e) = crate::devices::keep_devtmpfs() {
        let _ = writeln!(io::stderr(), "shards-init: keeping /dev: {e}");
    }
    if let Err(e) = crate::devices::keep_sysfs() {
        let _ = writeln!(io::stderr(), "shards-init: keeping a sysfs for VFIO: {e}");
    }
    crate::plug::settle_reporting();
    if let Err(e) = mark(marker::INIT_STARTED) {
        let _ = writeln!(io::stderr(), "shards-init: control page: {e}");
    }
    // `shards_online=1`: the kernel brought up its boot CPU alone, and this brings the rest
    // online before anything runs (vm::late_cpus_cmdline, PM M379).
    if std::env::var_os("shards_online").is_some()
        && let Err(e) = bring_cpus_online()
    {
        let _ = writeln!(io::stderr(), "shards-init: bringing its CPUs online: {e}");
    }
    // Which contract this init speaks, for the host to check before it hands over a
    // workload. A snapshot keeps it.
    if let Err(e) = control_write64(control::ABI, shards_abi::IDENTITY) {
        let _ = writeln!(io::stderr(), "shards-init: announcing the protocol: {e}");
    }
    if let Err(e) = sync_clock() {
        let _ = writeln!(io::stderr(), "shards-init: setting the clock: {e}");
    }
    // `shards_root=<device>` on the kernel command line: boot into the image on that
    // device and run the host's workload in it; with `shards_template=1`, snapshot first.
    // `shards_build=1`: a builder for `shards build`'s steps (build.rs).
    if std::env::var_os("shards_build").is_some() {
        crate::build::main()
    }
    if let Some(device) = std::env::var_os("shards_root") {
        let template = std::env::var_os("shards_template").is_some();
        crate::run::main(&device.to_string_lossy(), template)
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

/// Brings each CPU the kernel has and has not brought up online, one after another, through
/// a sysfs of its own attached nowhere: on CI's Windows runners 3 CPUs in 67 to 109 ms (PM
/// M379). A CPU that will not come online is said, and the rest still brought up.
fn bring_cpus_online() -> io::Result<()> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let sysfs = crate::domains::detached(c"sysfs", &[])?;
    let open = |path: &str, flags: libc::c_int| -> io::Result<std::fs::File> {
        let c = std::ffi::CString::new(path).map_err(io::Error::other)?;
        // SAFETY: openat(2) of a NUL-terminated name beneath a descriptor this holds.
        let fd = unsafe { libc::openat(sysfs.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return Err(io::Error::new(e.kind(), format!("{path}: {e}")));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        Ok(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) })
    };
    let list = |name: &str| -> io::Result<Vec<u32>> {
        let mut text = String::new();
        open(&format!("devices/system/cpu/{name}"), libc::O_RDONLY)?.read_to_string(&mut text)?;
        cpu_list(&text).ok_or_else(|| io::Error::other(format!("CPUs {name}: {text:?}")))
    };
    let online = list("online")?;
    for cpu in list("present")?.into_iter().filter(|c| !online.contains(c)) {
        let path = format!("devices/system/cpu/cpu{cpu}/online");
        if let Err(e) = open(&path, libc::O_WRONLY).and_then(|mut f| f.write_all(b"1")) {
            let _ = writeln!(io::stderr(), "shards-init: CPU {cpu} not brought online: {e}");
        }
    }
    Ok(())
}

/// The CPUs of a sysfs CPU list (`0-3,5`), in order; none where it is not one.
fn cpu_list(text: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        let (from, to) = part.split_once('-').unwrap_or((part, part));
        let (from, to) = (from.parse::<u32>().ok()?, to.parse::<u32>().ok()?);
        if from > to {
            return None;
        }
        cpus.extend(from..=to);
    }
    Some(cpus)
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
    control_write(control::MARKER, value)
}

/// The VMM's control page, mapped once from /dev/mem and kept: init's markers are stores,
/// not syscalls, and a child can write them before it execs.
static CONTROL: AtomicUsize = AtomicUsize::new(0);

fn control_page() -> io::Result<*mut u8> {
    let mapped = CONTROL.load(Ordering::Relaxed);
    if mapped != 0 {
        return Ok(mapped as *mut u8);
    }
    // SAFETY: plain syscalls; the mapping is never unmapped.
    let page = unsafe {
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
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            CONTROL_PAGE as libc::off_t,
        );
        libc::close(fd);
        page
    };
    if page == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    CONTROL.store(page as usize, Ordering::Relaxed);
    Ok(page.cast())
}

/// Writes one register of the VMM's control page.
pub(crate) fn control_write(register: u64, value: u32) -> io::Result<()> {
    let page = control_page()?;
    let offset = usize::try_from(register)
        .ok()
        .filter(|&o| o % 4 == 0 && o < 4096)
        .ok_or_else(|| io::Error::other("no such control register"))?;
    // SAFETY: an aligned register inside the mapped page.
    unsafe { std::ptr::write_volatile(page.add(offset).cast::<u32>(), value) };
    Ok(())
}

/// Writes a 64-bit register of the VMM's control page, in one access.
pub(crate) fn control_write64(register: u64, value: u64) -> io::Result<()> {
    let page = control_page()?;
    let offset = usize::try_from(register)
        .ok()
        .filter(|&o| o % 8 == 0 && o < 4096)
        .ok_or_else(|| io::Error::other("no such control register"))?;
    // SAFETY: an aligned register inside the mapped page.
    unsafe { std::ptr::write_volatile(page.add(offset).cast::<u64>(), value) };
    Ok(())
}

/// Reads a 64-bit register of the VMM's control page, in one access.
pub(crate) fn control_read64(register: u64) -> io::Result<u64> {
    let page = control_page()?;
    let offset = usize::try_from(register)
        .ok()
        .filter(|&o| o % 8 == 0 && o < 4096)
        .ok_or_else(|| io::Error::other("no such control register"))?;
    // SAFETY: an aligned register inside the mapped page.
    Ok(unsafe { std::ptr::read_volatile(page.add(offset).cast::<u64>()) })
}

/// Sets CLOCK_REALTIME to the host's time (`control::HOST_TIME`): at boot, where the RTC
/// gave whole seconds, and after a restore, where the clock is still the snapshot's.
pub(crate) fn sync_clock() -> io::Result<()> {
    let ns = control_read64(control::HOST_TIME)?;
    if ns == 0 {
        return Err(io::Error::other("the host gave no time"));
    }
    let ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000).try_into().map_err(io::Error::other)?,
        tv_nsec: (ns % 1_000_000_000).try_into().map_err(io::Error::other)?,
    };
    // SAFETY: a valid timespec; PID 1 holds CAP_SYS_TIME.
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn power_off() -> ! {
    // SAFETY: PID 1 waiting for its console to send what it wrote, flushing filesystems and
    // asking the kernel to power off (PSCI SYSTEM_OFF). The console's tty holds a write
    // until its UART has sent it, and the kernel's own lines as it powers off go past it:
    // powering off at once can drop PID 1's last words.
    unsafe {
        libc::tcdrain(1);
        libc::tcdrain(2);
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

#[cfg(test)]
mod tests {
    /// A sysfs CPU list reads as its CPUs in order, and what is not one as none.
    #[test]
    fn cpu_lists_read_as_sysfs_writes_them() {
        assert_eq!(super::cpu_list("0\n"), Some(vec![0]));
        assert_eq!(super::cpu_list("0-3,5,7-8\n"), Some(vec![0, 1, 2, 3, 5, 7, 8]));
        assert_eq!(super::cpu_list("\n"), Some(vec![]));
        assert_eq!(super::cpu_list("3-1"), None);
        assert_eq!(super::cpu_list("a-b"), None);
    }
}
