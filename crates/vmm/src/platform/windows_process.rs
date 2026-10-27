//! What a process uses of the host, and how far it may grow, on Windows: unix.rs's `Usage`,
//! `process_usage`, `launched`, `thread_limit`, `descriptor_limit`,
//! `raise_descriptor_limit` and `mapped_pages`, each by Windows' own means.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
    PSAPI_WORKING_SET_EX_INFORMATION, QueryWorkingSetEx,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The kernel handles a process may hold: "The per-process limit on kernel handles is 2^24"
/// (Microsoft, "Kernel Objects"). Windows has no descriptor table of its own to raise.
const HANDLES: u64 = 1 << 24;

/// What a process uses of the host (unix.rs `Usage`): its working set, its own memory, in
/// bytes, and the CPU time it has had, user and kernel together, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub resident: u64,
    /// Its own memory, as memory per VM is counted (PM M30): its private working set, the
    /// pages it alone holds resident (PROCESS_MEMORY_COUNTERS_EX2's PrivateWorkingSetSize),
    /// as macOS's physical footprint and Linux's `RssAnon` count what is its own; not its
    /// private commit (PrivateUsage), which counts what it committed and never touched, a
    /// guest's whole RAM. A Windows without EX2 (its documentation: Windows 10 and 11 22H2
    /// with the September 2023 update) answers its whole working set instead, its own pages
    /// and those it shares: never less than its own.
    pub footprint: u64,
    pub cpu_ns: u64,
}

/// Whether process `pid` has run anything of its own yet (unix.rs `launched`): Windows makes
/// no process wait to be assessed before it runs, as macOS does (PM M166), that this could
/// tell: `None`.
pub fn launched(pid: u32) -> Option<bool> {
    let _ = pid;
    None
}

/// [`Usage`] of process `pid`, as Windows keeps it: GetProcessMemoryInfo's counters, and
/// GetProcessTimes' kernel and user times, in units of 100 ns.
pub fn process_usage(pid: u32) -> io::Result<Usage> {
    // SAFETY: OpenProcess of a process ID for query access: a handle, or null.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle just opened, owned by nothing else.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let (resident, footprint) = memory(&process)?;
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: a process handle `process` holds, and four FILETIMEs to fill.
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let hundreds = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok(Usage {
        resident,
        footprint,
        cpu_ns: hundreds(kernel)
            .saturating_add(hundreds(user))
            .saturating_mul(100),
    })
}

/// `process`'s working set and its private working set ([`Usage::footprint`]): EX2's, else
/// EX's working set for both.
fn memory(process: &OwnedHandle) -> io::Result<(u64, u64)> {
    let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX2>()).map_err(io::Error::other)?,
        ..Default::default()
    };
    // SAFETY: counters of the size their `cb` says, of a process handle `process` holds.
    if unsafe { GetProcessMemoryInfo(process.as_raw_handle(), (&raw mut ex2).cast(), ex2.cb) } != 0 {
        return Ok((ex2.WorkingSetSize as u64, ex2.PrivateWorkingSetSize as u64));
    }
    let mut ex = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).map_err(io::Error::other)?,
        ..Default::default()
    };
    // SAFETY: as above.
    if unsafe { GetProcessMemoryInfo(process.as_raw_handle(), (&raw mut ex).cast(), ex.cb) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((ex.WorkingSetSize as u64, ex.WorkingSetSize as u64))
}

/// How many threads this process may have, as far as the system says (unix.rs
/// `thread_limit`): none, where "the number of threads a process can create is limited by
/// the available virtual memory" (CreateThread's documentation).
pub fn thread_limit() -> Option<u64> {
    None
}

/// How many handles this process may hold open (unix.rs `descriptor_limit`): the kernel's
/// limit for every process ([`HANDLES`]).
pub fn descriptor_limit() -> Option<u64> {
    Some(HANDLES)
}

/// unix.rs `raise_descriptor_limit`: Windows holds every process to [`HANDLES`] alike, and
/// there is nothing to raise. Returns the limit it has.
pub fn raise_descriptor_limit() -> io::Result<u64> {
    Ok(HANDLES)
}

/// PSAPI_WORKING_SET_EX_BLOCK's Valid bit, its first: the page is in the working set.
const VALID: usize = 1;
/// Its Shared bit, the sixteenth, after Valid, ShareCount's three and Win32Protection's
/// eleven: the page can be shared, as a mapped file's page is until a write copies it.
const SHARED: usize = 1 << 15;

/// The pages QueryWorkingSetEx is asked of at once: 512 KiB of entries.
const QUERIED: usize = 1 << 16;

/// The pages of the `len` bytes at `ptr` that are mapped now, each as its index and whether
/// it is a private copy rather than a page of the mapped file (unix.rs `mapped_pages`): those
/// QueryWorkingSetEx finds in this process's working set (Valid), a private copy where not
/// shareable (Shared), as an anonymous page and a mapped file's page a write has copied are
/// not.
pub fn mapped_pages(ptr: *const u8, len: usize) -> io::Result<Vec<(usize, bool)>> {
    let page = super::page_size()?;
    if !(ptr as usize).is_multiple_of(page) || !len.is_multiple_of(page) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{ptr:p}+{len:#x} is not page-aligned"),
        ));
    }
    let pages = len / page;
    let mut entries = vec![PSAPI_WORKING_SET_EX_INFORMATION::default(); pages.min(QUERIED)];
    let mut mapped = Vec::new();
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(entries.len());
        let asked = entries
            .get_mut(..n)
            .ok_or_else(|| io::Error::other("working set entries"))?;
        for (i, entry) in asked.iter_mut().enumerate() {
            entry.VirtualAddress = ((ptr as usize).wrapping_add((done + i) * page)) as *mut core::ffi::c_void;
        }
        let bytes = u32::try_from(std::mem::size_of_val(asked)).map_err(io::Error::other)?;
        // SAFETY: `asked`'s entries, each naming an address to ask of, which QueryWorkingSetEx
        // fills; this process's own pseudo-handle.
        if unsafe { QueryWorkingSetEx(GetCurrentProcess(), asked.as_mut_ptr().cast(), bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        for (i, entry) in asked.iter().enumerate() {
            // SAFETY: the union's word of flags, which QueryWorkingSetEx filled.
            let flags = unsafe { entry.VirtualAttributes.Flags };
            if flags & VALID != 0 {
                mapped.push((done + i, flags & SHARED == 0));
            }
        }
        done += n;
    }
    Ok(mapped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// This process's own usage is there: some memory resident, its own no more than all
    /// of it, CPU time that grows as it works.
    #[test]
    fn a_process_usage_is_read() {
        let before = process_usage(std::process::id()).unwrap();
        assert!(before.resident > 0 && before.footprint > 0, "{before:?}");
        assert!(before.footprint <= before.resident, "{before:?}");
        // Where Windows has EX2, its own pages are fewer than all it holds: its DLLs' are
        // shared.
        let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX2>()).unwrap(),
            ..Default::default()
        };
        // SAFETY: counters of the size their `cb` says, of this process.
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut ex2).cast(), ex2.cb) } != 0 {
            assert!(before.footprint < before.resident, "{before:?}");
        }
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(200) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let after = process_usage(std::process::id()).unwrap();
        assert!(after.cpu_ns > before.cpu_ns, "{before:?} then {after:?}");
        assert!(process_usage(u32::MAX - 3).is_err(), "no such process");
        assert_eq!(launched(std::process::id()), None);
        assert_eq!(descriptor_limit(), Some(HANDLES));
        assert_eq!(raise_descriptor_limit().unwrap(), HANDLES);
        assert_eq!(thread_limit(), None);
    }

    /// Anonymous memory: nothing mapped until touched, then each page written, a private
    /// page. A file mapped copy-on-write: a page read is the file's, a page written a
    /// private copy, and nothing else a copy.
    #[test]
    fn mapped_pages_are_the_working_sets() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_COPY, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
            MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, PAGE_WRITECOPY, UnmapViewOfFile,
            VirtualAlloc, VirtualFree,
        };
        let page = super::super::page_size().unwrap();
        let len = 64 * page;
        // SAFETY: a fresh reservation, committed, freed below.
        let anon = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
            .cast::<u8>();
        assert!(!anon.is_null());
        assert_eq!(mapped_pages(anon, len).unwrap(), []);
        // SAFETY: pages 3 and 7 of the 64 committed above.
        unsafe {
            anon.add(3 * page).write_volatile(1);
            anon.add(7 * page).write_volatile(1);
        }
        assert_eq!(mapped_pages(anon, len).unwrap(), [(3, true), (7, true)]);
        assert!(mapped_pages(anon.wrapping_add(1), page).is_err(), "unaligned");
        // SAFETY: the reservation made above, released whole.
        unsafe { VirtualFree(anon.cast(), 0, MEM_RELEASE) };

        let dir = shards_testdir::TempDir::new("mapped").unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("f"))
            .unwrap();
        file.write_all(&vec![7u8; len]).unwrap();
        file.sync_all().unwrap();
        // SAFETY: a copy-on-write mapping of the whole file, and a view of it, both closed
        // below.
        let mapping = unsafe {
            CreateFileMappingW(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file),
                std::ptr::null(),
                PAGE_WRITECOPY,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(!mapping.is_null());
        // SAFETY: as above.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_COPY, 0, 0, len) };
        let at = view.Value.cast::<u8>();
        assert!(!at.is_null());
        // SAFETY: page 5 read and page 40 written, of the view's 64.
        unsafe {
            assert_eq!(at.add(5 * page).read_volatile(), 7);
            at.add(40 * page).write_volatile(9);
        }
        let mapped = mapped_pages(at, len).unwrap();
        assert!(
            mapped.contains(&(5, false)) && mapped.contains(&(40, true)),
            "{mapped:?}"
        );
        assert!(mapped.iter().all(|&(p, copy)| !copy || p == 40), "{mapped:?}");
        // SAFETY: the view and mapping made above, each closed once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
            windows_sys::Win32::Foundation::CloseHandle(mapping);
        }
    }
}
