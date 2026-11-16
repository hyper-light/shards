//! The host's memory, as its kernel says it: all of it, what it has available now, and so
//! the most a reader may hold. The daemon's and a VM process's alike: a VM process that
//! reads a snapshot's state reads it by them (`snapshot::StateRefused`), as the daemon reads
//! a build's or a call's.

/// The memory this host has available, in bytes: what its kernel says a new program may
/// have without taking from what runs. Linux's `MemAvailable` ("an estimate of how much
/// memory is available for starting new applications, without swapping", proc_meminfo(5)).
#[cfg(target_os = "linux")]
pub fn host_available() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}

/// The memory this host has available, in bytes: macOS's `kern.memorystatus_level`, the
/// percentage of its pages its pressure thread counts available
/// (`memorystatus_get_available_page_count`, osfmk/vm/vm_pageout.c `vm_pressure_response`;
/// the sysctl in bsd/kern/kern_memorystatus.c), of its memory.
#[cfg(target_os = "macos")]
pub fn host_available() -> Option<u64> {
    let mut level: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>();
    // SAFETY: sysctlbyname(3) of a NUL-terminated name into an int of the length given.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_level".as_ptr(),
            (&raw mut level).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != size_of::<libc::c_int>() {
        return None;
    }
    let level = u64::try_from(level).ok()?.min(100);
    Some(host_memory()? / 100 * level)
}

/// The memory this host has available, in bytes: none known on this Unix.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn host_available() -> Option<u64> {
    None
}

/// The memory this host has available, in bytes: `ullAvailPhys` (GlobalMemoryStatusEx),
/// "the amount of physical memory that can be immediately reused without having to write
/// its contents to disk first" (MEMORYSTATUSEX).
#[cfg(windows)]
pub fn host_available() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a structure of its own length for the call to fill.
    (unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0).then_some(status.ullAvailPhys)
}

/// The most a read whose length the reader cannot know before it ends may hold, where what
/// it guards is the reader's memory: what this host has available now ([`host_available`]),
/// else its memory ([`host_memory`]); past that it could not be held without taking from
/// what runs. Unbounded where the host says neither, as dockerd reads a call's JSON.
pub fn holdable() -> u64 {
    holdable_known().unwrap_or(u64::MAX)
}

/// [`holdable`], `None` where the host says neither: for a reader that refuses what it
/// cannot bound (a snapshot's state, `snapshot::StateRefused::HostMemoryUnknown`).
pub fn holdable_known() -> Option<u64> {
    host_available().or_else(host_memory)
}

/// The host's physical memory, in bytes.
#[cfg(unix)]
pub fn host_memory() -> Option<u64> {
    // SAFETY: sysconf(3) with constant arguments.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    u64::try_from(pages).ok()?.checked_mul(u64::try_from(size).ok()?)
}

/// The host's physical memory, in bytes (GlobalMemoryStatusEx).
#[cfg(windows)]
pub fn host_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a structure of its own length for the call to fill.
    (unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0).then_some(status.ullTotalPhys)
}
