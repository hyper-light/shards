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
    memory_status().map(|status| status.ullAvailPhys)
}

/// What this host's memory is now (GlobalMemoryStatusEx).
#[cfg(windows)]
fn memory_status() -> Option<windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a structure of its own length for the call to fill.
    (unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0).then_some(status)
}

/// The memory a new VM's guest may have of this host, in bytes, its RAM committed as its VM
/// process starts: what the host has available ([`host_available`]), and on Windows no more
/// than it can still commit ([`committable`]), which a commit past fails whatever is free
/// (ERROR_COMMITMENT_LIMIT, 1455). A cold builder sized by free physical memory alone was
/// refused its RAM so ("the paging file is too small", CI 44227612442, windows-2025; D133).
pub fn host_committable() -> Option<u64> {
    #[cfg(windows)]
    return memory_status().map(|status| committable(&status));
    #[cfg(not(windows))]
    host_available()
}

/// What `status` says a new commit may take: its physical memory available (`ullAvailPhys`),
/// at most what may still be committed (`ullAvailPageFile`, "the maximum amount of memory the
/// current process can commit", MEMORYSTATUSEX): the system's commit limit, its physical
/// memory and its paging files, less the commit charge of everything that runs, which a
/// commit is charged against whether or not it is ever touched (VirtualAlloc's MEM_COMMIT).
#[cfg(windows)]
fn committable(status: &windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX) -> u64 {
    status.ullAvailPhys.min(status.ullAvailPageFile)
}

/// What this host's memory is now, said where a VM's memory was refused it: its physical
/// memory available and what it can still commit, in MiB, as a refusal's account; none
/// unread or for any other error. Windows says a refusal so as ERROR_NO_SYSTEM_RESOURCES
/// (1450, as WHvMapGpaRange's HRESULT 0x800705aa) or ERROR_COMMITMENT_LIMIT (1455).
#[cfg(windows)]
pub fn memory_refused_said(code: i32) -> Option<String> {
    if !matches!(code, 1450 | 1455) {
        return None;
    }
    let status = memory_status()?;
    Some(format!(
        "the host had {} MiB of physical memory available and could commit {} MiB more",
        status.ullAvailPhys >> 20,
        status.ullAvailPageFile >> 20
    ))
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
    memory_status().map(|status| status.ullTotalPhys)
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX;

    /// A new commit takes no more than its host can still commit, whatever is free of its
    /// physical memory, and no more than is free where it could commit more: the cold builder
    /// of CI 44227612442 (windows-2025) was sized by free memory alone and refused its RAM.
    #[test]
    fn a_new_commit_takes_no_more_than_its_host_can_commit() {
        const GIB: u64 = 1 << 30;
        let status = |phys: u64, commit: u64| MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ullAvailPhys: phys,
            ullAvailPageFile: commit,
            ..Default::default()
        };
        assert_eq!(super::committable(&status(9 * GIB, 3 * GIB)), 3 * GIB);
        assert_eq!(super::committable(&status(3 * GIB, 9 * GIB)), 3 * GIB);
        assert_eq!(super::committable(&status(4 * GIB, 4 * GIB)), 4 * GIB);
    }
}
