//! What a process uses of the host, and how far it may grow, on Windows: unix.rs's `Usage`,
//! `process_usage`, `launched`, `thread_limit`, `descriptor_limit`,
//! `raise_descriptor_limit` and `mapped_pages`, each by Windows' own means.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Diagnostics::Debug::{IMAGE_NT_HEADERS64, IMAGE_NT_OPTIONAL_HDR64_MAGIC};
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualQuery};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
    PSAPI_WORKING_SET_EX_INFORMATION, QueryWorkingSetEx,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::SystemServices::{IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_NT_SIGNATURE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadStackLimits, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, TEB,
};

/// The kernel handles a process may hold: "The per-process limit on kernel handles is 2^24"
/// (Microsoft, "Kernel Objects"). Windows has no descriptor table of its own to raise.
const HANDLES: u64 = 1 << 24;

/// What a process uses of the host (unix.rs `Usage`): its working set, its own memory, in
/// bytes, the CPU time it has had, user and kernel together, in nanoseconds, and the page
/// faults it has taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub resident: u64,
    /// Its own memory, as memory per VM is counted (PM M30): its private working set, the
    /// pages it alone holds resident (PROCESS_MEMORY_COUNTERS_EX2's PrivateWorkingSetSize),
    /// as macOS's physical footprint and Linux's `RssAnon` count what is its own; not its
    /// private commit (PrivateUsage), which counts what it committed and never touched, a
    /// guest's whole RAM. A Windows without EX2 (its documentation: Windows 10 and 11 22H2
    /// with the September 2023 update), or one that takes EX2's size and leaves its private
    /// working set 0 (Windows Server 2022, build 20348, for a process that runs), answers its
    /// whole working set instead, its own pages and those it shares: never less than its own.
    pub footprint: u64,
    pub cpu_ns: u64,
    /// Its page faults, soft and hard (PROCESS_MEMORY_COUNTERS' PageFaultCount).
    pub faults: u64,
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
    let (resident, footprint, faults) = memory(&process)?;
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
        faults,
    })
}

/// `process`'s working set, its private working set ([`Usage::footprint`]) and its page
/// faults: EX2's, else EX's working set for both.
fn memory(process: &OwnedHandle) -> io::Result<(u64, u64, u64)> {
    let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX2>()).map_err(io::Error::other)?,
        ..Default::default()
    };
    // SAFETY: counters of the size their `cb` says, of a process handle `process` holds.
    if unsafe { GetProcessMemoryInfo(process.as_raw_handle(), (&raw mut ex2).cast(), ex2.cb) } != 0
        && ex2.PrivateWorkingSetSize != 0
    {
        return Ok((
            ex2.WorkingSetSize as u64,
            ex2.PrivateWorkingSetSize as u64,
            u64::from(ex2.PageFaultCount),
        ));
    }
    let mut ex = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).map_err(io::Error::other)?,
        ..Default::default()
    };
    // SAFETY: as above.
    if unsafe { GetProcessMemoryInfo(process.as_raw_handle(), (&raw mut ex).cast(), ex.cb) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        ex.WorkingSetSize as u64,
        ex.WorkingSetSize as u64,
        u64::from(ex.PageFaultCount),
    ))
}

/// How many threads this process may have, as far as the system says (unix.rs
/// `thread_limit`). Windows keeps no count of them: "The number of threads a process can
/// create is limited by the available virtual memory" (CreateThread). Each takes its stack's
/// reservation of the process's address space, and "the initially committed pages ... remove
/// pages from the system total commit limit" (Thread Stack Size), its TEB's with them; one
/// whose handle is kept holds one of the [`HANDLES`] a process may. So the fewest threads any
/// of the three holds ([`Room::threads`]), each limit as the host and this process's job set
/// it now ([`Room::now`]), and each cost read off a thread started as std starts every thread
/// of shards' ([`Thread::here`]; std's default stack, RUST_MIN_STACK, else 2 MiB). `None`
/// where no thread could be started to read them.
pub fn thread_limit() -> Option<u64> {
    let thread = std::thread::Builder::new()
        .spawn(Thread::here)
        .ok()?
        .join()
        .ok()
        .flatten()?;
    Room::now()?.threads(&thread)
}

/// What a thread std started holds of its process however far its code goes: its stack's
/// reservation of the address space, and the commit Windows charges every thread of it, its
/// stack's initial commit and guard pages (std's stack guarantee's among them) and its TEB's.
/// Not the pages its code has touched: a process's first thread runs its one-time
/// initialization on its own stack, and a test's every thread its output's capture, a page
/// more each (PM M529). No call reports what else a thread is charged: its share of the heap
/// (7 KB of the 48 KB Windows charged each in a job, PM M529), nor the kernel's own commit for
/// it, its kernel stack's, which "can vary among different hardware platforms and different
/// versions of the operating system" (Using the Kernel Stack). None of these is counted, so
/// that the threads a commit is said to hold are never fewer than Windows starts in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Thread {
    reserved: u64,
    committed: u64,
}

impl Thread {
    /// The calling thread's: the bounds GetCurrentThreadStackLimits gives; the image's initial
    /// commit ([`stack_commit`]) and the guard pages VirtualQuery finds between them
    /// (PAGE_GUARD); and winternl.h's TEB, the least a TEB is ("Do not assume a maximum size
    /// for this structure"; two pages, as measured, PM M529). Each in whole pages.
    fn here() -> Option<Thread> {
        let (mut low, mut high) = (0usize, 0usize);
        // SAFETY: two words to fill with this thread's stack's bounds.
        unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        let mut guard = 0usize;
        let mut at = low;
        while at < high {
            let mut region = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: a query of a page of this thread's own stack, into a structure of its size.
            let said = unsafe {
                VirtualQuery(
                    at as *const core::ffi::c_void,
                    &mut region,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if said == 0 {
                return None;
            }
            let end = (region.BaseAddress as usize)
                .checked_add(region.RegionSize)?
                .min(high);
            if end <= at {
                return None;
            }
            if region.State == MEM_COMMIT && region.Protect & PAGE_GUARD != 0 {
                guard = guard.checked_add(end - at)?;
            }
            at = end;
        }
        let page = super::page_size().ok()?;
        let whole = |bytes: usize| bytes.div_ceil(page).checked_mul(page);
        let committed = guard
            .checked_add(whole(stack_commit()?)?)?
            .checked_add(whole(size_of::<TEB>())?)?;
        Some(Thread {
            reserved: u64::try_from(high.checked_sub(low)?).ok()?,
            committed: u64::try_from(committed).ok()?,
        })
    }
}

/// What Windows commits of a thread's stack as it starts it, where its stack is reserved as
/// std reserves it (STACK_SIZE_PARAM_IS_A_RESERVATION): "the initially committed size is the
/// default size specified in the executable header" (Thread Stack Size), this process's
/// image's SizeOfStackCommit (PE Format; the linker's default, 4 KB: /STACK).
fn stack_commit() -> Option<usize> {
    // SAFETY: this process's own image's base, which stays loaded while it runs.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) }
        .cast::<u8>()
        .cast_const();
    if base.is_null() {
        return None;
    }
    // SAFETY: a loaded image begins with its DOS header, read whole as a copy, unaligned.
    let dos: IMAGE_DOS_HEADER = unsafe { std::ptr::read_unaligned(base.cast()) };
    if dos.e_magic != IMAGE_DOS_SIGNATURE {
        return None;
    }
    let at = usize::try_from(dos.e_lfanew).ok()?;
    // SAFETY: e_lfanew is the offset of the image's NT headers, within the headers loaded with
    // it (PE Format: "the file offset of the PE signature"), read whole as a copy, unaligned.
    let nt: IMAGE_NT_HEADERS64 = unsafe { std::ptr::read_unaligned(base.add(at).cast()) };
    let optional = nt.OptionalHeader;
    if nt.Signature != IMAGE_NT_SIGNATURE || optional.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC {
        return None;
    }
    usize::try_from(optional.SizeOfStackCommit).ok()
}

/// What this process may take of its host: its address space, and the commit it may be
/// charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Room {
    space: u64,
    commit: u64,
}

impl Room {
    /// MEMORYSTATUSEX's ullTotalVirtual, "the size of the user-mode portion of the virtual
    /// address space of the calling process", and the fewer of its ullTotalPageFile, "the
    /// current committed memory limit for the system or the current process, whichever is
    /// smaller", and its job's ([`job_commit`]), which that leaves out (PM M529).
    fn now() -> Option<Room> {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
            ..Default::default()
        };
        // SAFETY: a MEMORYSTATUSEX whose dwLength says its size.
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return None;
        }
        Some(Room {
            space: status.ullTotalVirtual,
            commit: job_commit().map_or(status.ullTotalPageFile, |job| job.min(status.ullTotalPageFile)),
        })
    }

    /// The threads it holds of threads like `thread`: the fewest its address space holds of
    /// their stacks' reservations, its commit of what each is charged, and a process's
    /// [`HANDLES`] of one each.
    fn threads(&self, thread: &Thread) -> Option<u64> {
        let by_space = self.space.checked_div(thread.reserved)?;
        let by_commit = self.commit.checked_div(thread.committed)?;
        Some(by_space.min(by_commit).min(HANDLES))
    }
}

/// The commit this process's job holds it to, where it is in one that holds it to any: the
/// fewer of its limit for a process (JOB_OBJECT_LIMIT_PROCESS_MEMORY) and for its processes
/// together (JOB_OBJECT_LIMIT_JOB_MEMORY), as QueryInformationJobObject reads them of "the
/// immediate job of the calling process". A job it nests in that holds it to less, where
/// "the effective memory limit is the most restrictive memory limit in the job chain", is not
/// for it to read.
fn job_commit() -> Option<u64> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the limits of this process's own job, if it is in one, into a structure of their
    // size.
    let read = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).ok()?,
            std::ptr::null_mut(),
        )
    };
    if read == 0 {
        return None;
    }
    let flags = limits.BasicLimitInformation.LimitFlags;
    let process = (flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0).then_some(limits.ProcessMemoryLimit);
    let job = (flags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0).then_some(limits.JobMemoryLimit);
    let fewer = match (process, job) {
        (Some(p), Some(j)) => Some(p.min(j)),
        (p, j) => p.or(j),
    };
    fewer.and_then(|bytes| u64::try_from(bytes).ok())
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

/// How many handles this process has open (unix.rs `open_descriptors`): GetProcessHandleCount.
pub fn open_descriptors() -> Option<u64> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0u32;
    // SAFETY: this process's pseudo-handle, and a count to fill.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    (ok != 0).then_some(u64::from(count))
}

/// The system's table of open files (unix.rs `file_table`): Windows keeps none apart from
/// its processes' handles, each process held to [`HANDLES`].
pub fn file_table() -> Option<(u64, u64)> {
    None
}

/// How many processes this process's user may have (unix.rs `user_processes`): Windows
/// holds a user to no count of them.
pub fn user_processes() -> Option<u64> {
    None
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
        // Where Windows fills EX2, its own pages are fewer than all it holds: its DLLs' are
        // shared.
        let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX2>()).unwrap(),
            ..Default::default()
        };
        // SAFETY: counters of the size their `cb` says, of this process.
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut ex2).cast(), ex2.cb) } != 0
            && ex2.PrivateWorkingSetSize != 0
        {
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
    }

    /// The threads a host holds are the fewest its address space, its commit and a
    /// process's handles each hold: each binds in turn here, the first two with jupiter's
    /// readings and a thread as it measured one (PM M529: 2 MiB reserved, its stack's 4 KiB
    /// initial commit and 24 KiB of guard pages, and 8 KiB of TEB); and a thread that reserves
    /// nothing is none to count by.
    #[test]
    fn the_threads_held_are_the_fewest_each_limit_holds() {
        let thread = Thread {
            reserved: 2 << 20,
            committed: 36 << 10,
        };
        let room = |space, commit| Room { space, commit };
        assert_eq!(
            room(140_737_488_224_256, 141_087_805_440).threads(&thread),
            Some(3_827_251)
        );
        assert_eq!(room(140_737_488_224_256, 64 << 20).threads(&thread), Some(1_820));
        assert_eq!(room(4 << 30, u64::MAX).threads(&thread), Some(2_048));
        assert_eq!(room(u64::MAX, u64::MAX).threads(&thread), Some(HANDLES));
        assert_eq!(room(1, 1).threads(&thread), Some(0));
        let none = Thread {
            reserved: 0,
            committed: 1,
        };
        assert_eq!(room(1, 1).threads(&none), None);
    }

    /// How many threads this process may have is what its host holds of a thread std
    /// starts: its address space of the thread's stack's reservation, its commit limit of the
    /// commit the thread is charged, and a process's handles of one, none a count Windows
    /// keeps. Before, Windows said none, and the daemon took POSIX's least, 64
    /// (daemon.rs `most_clients`): 27 clients, 13 connections a microVM's engine.
    #[test]
    fn a_thread_limit_is_what_the_host_holds_of_a_thread() {
        let before = Room::now().unwrap();
        let limit = thread_limit();
        let after = Room::now().unwrap();
        let thread = std::thread::spawn(Thread::here).join().unwrap().unwrap();
        // However far its code has gone: a thread that has touched 256 KiB of its stack is
        // counted as one that has touched none.
        let deep = std::thread::spawn(|| {
            let touched = std::hint::black_box([1u8; 256 << 10]);
            (Thread::here(), touched.iter().map(|&b| u64::from(b)).sum::<u64>())
        })
        .join()
        .unwrap();
        assert_eq!(deep, (Some(thread), 256 << 10));
        // A thread std starts reserves its stack (RUST_MIN_STACK, else 2 MiB), and has
        // committed at least its initial page and the guard page below it (Thread Stack
        // Size), and its TEB, of what it reserves.
        let page = super::super::page_size().unwrap() as u64;
        assert!(thread.reserved >= 2 * page, "{thread:?}");
        assert!(
            thread.committed >= 2 * page + size_of::<TEB>() as u64,
            "{thread:?}"
        );
        assert!(thread.committed < thread.reserved, "{thread:?}");
        assert!(before.space > 0 && before.commit > 0, "{before:?}");
        // The host's commit limit may move between the readings (a page file grown).
        let (a, b) = (before.threads(&thread).unwrap(), after.threads(&thread).unwrap());
        assert!(
            limit.is_some_and(|n| n >= a.min(b) && n <= a.max(b)),
            "{limit:?}: not what {before:?} then {after:?} hold of {thread:?}; none, and the \
             daemon takes POSIX's least, 64 threads"
        );
        assert!(limit.is_some_and(|n| n > 64), "{limit:?}");
        println!("thread limit: {limit:?} threads, of {after:?} and {thread:?}");
    }

    /// Set in the child `a_jobs_commit_limit_holds_the_threads_it_says` runs.
    const IN_JOB: &str = "SHARDS_VMM_THREADS_IN_JOB";

    /// Where its job holds a process's commit to less than its host does, the threads it may
    /// have are that commit's over what a thread is charged; and Windows charges each thread
    /// std starts at least what thread_limit counts of it (its stack's committed pages, as
    /// VirtualQuery says them, and its TEB's, as winternl.h declares it), so that the limit
    /// is never below the threads the commit holds. Run in a child of this test binary, which
    /// holds itself, in a job of its own, to its commit now and room for twice the threads it
    /// starts: alone in its process, so that its commit's growth is its threads'.
    #[test]
    fn a_jobs_commit_limit_holds_the_threads_it_says() {
        use std::sync::{Arc, Condvar, Mutex, mpsc};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        };
        const NAME: &str = "platform::windows_process::tests::a_jobs_commit_limit_holds_the_threads_it_says";
        const SAID: &str = "thread limit child: ";
        const THREADS: u64 = 64;
        if std::env::var_os(IN_JOB).is_none() {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
                .env(IN_JOB, "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&child.stdout);
            assert!(
                child.status.success() && said.contains(SAID),
                "{:?}\n{said}{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            if let Some(line) = said.lines().find(|l| l.contains(SAID)) {
                println!("{line}");
            }
            return;
        }
        let commit = || {
            let mut c = PROCESS_MEMORY_COUNTERS_EX {
                cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap(),
                ..Default::default()
            };
            // SAFETY: counters of the size their `cb` says, of this process.
            let read = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
            assert_ne!(read, 0);
            c.PrivateUsage as u64
        };
        let thread = std::thread::spawn(Thread::here).join().unwrap().unwrap();
        let limit = commit() + 2 * THREADS * thread.committed;
        // SAFETY: an anonymous job, which this process stays in until it ends.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.ProcessMemoryLimit = usize::try_from(limit).unwrap();
        // SAFETY: limits of their own size, on the job just made.
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
            )
        };
        assert_ne!(set, 0);
        // SAFETY: this process's pseudo-handle, into the job, nested in any it is in already.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert_ne!(assigned, 0);
        let room = Room::now().unwrap();
        assert_eq!(room.commit, limit, "{room:?}: not the job's limit");
        assert_eq!(thread_limit(), room.threads(&thread));
        assert_eq!(thread_limit(), Some(limit / thread.committed));
        // Threads std starts, each reading itself as `thread` was read before it parks.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let before = commit();
        let parked: Vec<_> = (0..THREADS)
            .map(|_| {
                let (gate, tx) = (gate.clone(), tx.clone());
                std::thread::spawn(move || {
                    tx.send(Thread::here()).unwrap();
                    let mut open = gate.0.lock().unwrap();
                    while !*open {
                        open = gate.1.wait(open).unwrap();
                    }
                })
            })
            .collect();
        let each: Vec<Thread> = (0..THREADS).map(|_| rx.recv().unwrap().unwrap()).collect();
        let charged = commit() - before;
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for t in parked {
            t.join().unwrap();
        }
        assert!(
            each.iter().all(|t| t.reserved == thread.reserved),
            "{thread:?}, then {each:?}"
        );
        let counted: u64 = each.iter().map(|t| t.committed).sum();
        assert!(
            charged >= counted,
            "{THREADS} threads charged {charged} B, fewer than the {counted} B counted of them"
        );
        println!(
            "{SAID}{THREADS} threads charged {charged} B, {} B each, counted {} B each ({thread:?} first); \
             the job's {limit} B hold {:?}",
            charged / THREADS,
            counted / THREADS,
            thread_limit()
        );
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
        // The pages touched alone, the file's others in the cache as they are: a fault maps
        // no page around it, as Linux's page cache does (unix.rs `FaultAroundOff`).
        assert_eq!(mapped_pages(at, len).unwrap(), [(5, false), (40, true)]);
        // SAFETY: the view and mapping made above, each closed once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
            windows_sys::Win32::Foundation::CloseHandle(mapping);
        }
    }

    /// A page read in from disk is the one page of its view a fault maps: the pages read
    /// in around it go to the cache alone, so a working set read of a restore's mapped pages
    /// (vm::mapped) holds what its guest touched and no neighbour (windows.rs
    /// `FaultAroundOff`).
    #[test]
    fn a_fault_from_disk_maps_the_page_it_touches_alone() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
            MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile,
            VirtualAlloc, VirtualFree,
        };
        let page = super::super::page_size().unwrap();
        let len = 256 * page;
        let dir = shards_testdir::TempDir::new("cold").unwrap();
        // Written past the cache, which holds none of it: each page a view reads is read
        // from disk, with those around it.
        // SAFETY: a fresh reservation, committed, as an unbuffered write's page-aligned
        // buffer, freed below.
        let buf = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
            .cast::<u8>();
        assert!(!buf.is_null());
        // SAFETY: the `len` bytes committed above, filled before they are read.
        let bytes = unsafe {
            buf.write_bytes(7, len);
            std::slice::from_raw_parts(buf, len)
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(dir.join("f"))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        // SAFETY: the reservation made above, released whole.
        unsafe { VirtualFree(buf.cast(), 0, MEM_RELEASE) };
        let file = std::fs::File::open(dir.join("f")).unwrap();
        // SAFETY: a read-only mapping of the whole file, and a view of it, both closed below.
        let mapping = unsafe {
            CreateFileMappingW(
                std::os::windows::io::AsRawHandle::as_raw_handle(&file),
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(!mapping.is_null());
        // SAFETY: as above.
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len) };
        let at = view.Value.cast::<u8>();
        assert!(!at.is_null());
        assert_eq!(mapped_pages(at, len).unwrap(), []);
        // SAFETY: pages 5 and 100 read, of the view's 256.
        unsafe {
            assert_eq!(at.add(5 * page).read_volatile(), 7);
            assert_eq!(at.add(100 * page).read_volatile(), 7);
        }
        assert_eq!(mapped_pages(at, len).unwrap(), [(5, false), (100, false)]);
        // SAFETY: the view and mapping made above, each closed once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view.Value });
            windows_sys::Win32::Foundation::CloseHandle(mapping);
        }
    }
}
