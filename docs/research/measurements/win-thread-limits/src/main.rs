//! What limits how many threads a Windows process may have, and what each thread costs it
//! (D181's admission on Windows, PM M529). Prints, for this host and build:
//!
//! - the host's limits: its page size and allocation granularity, the process's user address
//!   space (MEMORYSTATUSEX's ullTotalVirtual), its commit limit (ullTotalPageFile, "for the
//!   system or the current process, whichever is smaller") and the system's
//!   (PERFORMANCE_INFORMATION's CommitLimit), and the limits of the job it is in, if any;
//! - its image's stack reserve and commit (the PE optional header's SizeOfStackReserve and
//!   SizeOfStackCommit), and those of an image named with `--image`;
//! - a thread as std spawns one by default: its stack's reservation
//!   (GetCurrentThreadStackLimits) and each region of it (VirtualQuery), its stack guarantee
//!   (SetThreadStackGuarantee), and its TEB's allocation (NtCurrentTeb, winnt.h);
//! - bursts of `--burst` threads, held parked: the process's commit (PrivateUsage), pool
//!   quota and handles, and the system's commit (CommitTotal), each per thread;
//! - a copy of itself in a job holding its process's commit to `--job-limit` MiB, which starts
//!   parked threads, each before the next, until Windows refuses one: how many, the commit
//!   each was charged, and how many the limit holds of a thread as shards' `thread_limit`
//!   counts one (its stack's guard pages, the image's SizeOfStackCommit and winternl.h's
//!   TEB, whole pages).
//!
//! - `--suspended N`: what Windows charges for each of N threads made suspended, their stacks
//!   reserved as std's, before any runs (no heap of theirs, no stack guarantee), beside a std
//!   thread as thread_limit counts one;
//! - `--priority`: the priority limits of the job this process is in, and what each of
//!   SetThreadPriority's levels does to the calling thread under them (its return, its error,
//!   and the priority read back);
//! - `--priority-class CLASS`: the same, once SetPriorityClass has set this process at CLASS
//!   (hex, as winbase.h's `*_PRIORITY_CLASS`), in no job of its own making;
//! - `--in-priority-job CLASS -- CMD [ARG]...`: CMD run in a job of this process's making that
//!   holds its processes to CLASS (JOB_OBJECT_LIMIT_PRIORITY_CLASS, which needs
//!   SeIncreaseBasePriorityPrivilege enabled), nested in any job this process is in; exits
//!   as CMD does.
//!
//! Usage: win-thread-limits [--image PATH] [--burst N] [--rounds R] [--job-limit MIB]
//!        win-thread-limits --suspended N | --priority | --priority-class CLASS
//!        win-thread-limits --in-priority-job CLASS -- CMD [ARG]...

use std::io::{BufRead as _, Read as _, Write as _};
use std::os::windows::io::AsRawHandle as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::IMAGE_NT_HEADERS64;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualQuery};
use windows_sys::Win32::System::ProcessStatus::{
    GetPerformanceInfo, GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO};
use windows_sys::Win32::System::SystemServices::IMAGE_DOS_HEADER;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadStackLimits, GetProcessHandleCount, SetThreadStackGuarantee,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    if args.first().map(String::as_str) == Some("--fill") {
        return fill();
    }
    if args.first().map(String::as_str) == Some("--priority") {
        return priority();
    }
    if args.first().map(String::as_str) == Some("--priority-class") {
        let class = args.get(1).and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        let Some(class) = class else {
            eprintln!("--priority-class CLASS, in hex");
            std::process::exit(2);
        };
        // SAFETY: this process's pseudo-handle.
        let set = unsafe { windows_sys::Win32::System::Threading::SetPriorityClass(GetCurrentProcess(), class) };
        // SAFETY: no arguments.
        println!("SetPriorityClass({class:#x}) -> {set} (last error {})", unsafe { GetLastError() });
        return priority();
    }
    if args.first().map(String::as_str) == Some("--in-priority-job") {
        let class = args.get(1).and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        let command = args.iter().position(|a| a == "--").map(|i| &args[i + 1..]);
        let (Some(class), Some([cmd, rest @ ..])) = (class, command) else {
            eprintln!("--in-priority-job CLASS -- CMD [ARG]...");
            std::process::exit(2);
        };
        std::process::exit(in_priority_job(class, cmd, rest));
    }
    if args.first().map(String::as_str) == Some("--regions") {
        return regions();
    }
    if args.first().map(String::as_str) == Some("--suspended") {
        let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(256);
        return suspended(n);
    }
    let burst: usize = value("--burst").and_then(|v| v.parse().ok()).unwrap_or(2000);
    let rounds: usize = value("--rounds").and_then(|v| v.parse().ok()).unwrap_or(5);
    host();
    if let Some((reserve, commit)) = image_stack() {
        println!("image (this harness): SizeOfStackReserve {reserve} SizeOfStackCommit {commit}");
    }
    if let Some(path) = value("--image") {
        match file_stack(&path) {
            Ok((reserve, commit)) => println!("image {path}: SizeOfStackReserve {reserve} SizeOfStackCommit {commit}"),
            Err(e) => println!("image {path}: {e}"),
        }
    }
    println!("main thread:");
    probe();
    println!("a thread std spawns by default:");
    match std::thread::spawn(probe).join() {
        Ok(()) => {}
        Err(_) => println!("  the probe thread failed"),
    }
    for round in 0..rounds {
        burst_round(round, burst);
    }
    if let Some(mib) = value("--job-limit").and_then(|v| v.parse::<u64>().ok()) {
        in_job(mib << 20);
    }
}

fn host() {
    let mut si = SYSTEM_INFO::default();
    // SAFETY: a SYSTEM_INFO to fill.
    unsafe { GetSystemInfo(&mut si) };
    println!(
        "system: page {} allocation granularity {} application addresses {:#x}..{:#x} processors {}",
        si.dwPageSize,
        si.dwAllocationGranularity,
        si.lpMinimumApplicationAddress as usize,
        si.lpMaximumApplicationAddress as usize,
        si.dwNumberOfProcessors
    );
    let mut ms = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a MEMORYSTATUSEX of its own length.
    if unsafe { GlobalMemoryStatusEx(&mut ms) } != 0 {
        println!(
            "MEMORYSTATUSEX: TotalPhys {} AvailPhys {} TotalPageFile {} AvailPageFile {} TotalVirtual {} AvailVirtual {}",
            ms.ullTotalPhys, ms.ullAvailPhys, ms.ullTotalPageFile, ms.ullAvailPageFile, ms.ullTotalVirtual, ms.ullAvailVirtual
        );
    }
    let pi = performance();
    println!(
        "PERFORMANCE_INFORMATION (bytes): CommitTotal {} CommitLimit {} CommitPeak {} PhysicalTotal {} KernelNonpaged {} KernelPaged {}; HandleCount {} ProcessCount {} ThreadCount {}",
        pi.CommitTotal * pi.PageSize,
        pi.CommitLimit * pi.PageSize,
        pi.CommitPeak * pi.PageSize,
        pi.PhysicalTotal * pi.PageSize,
        pi.KernelNonpaged * pi.PageSize,
        pi.KernelPaged * pi.PageSize,
        pi.HandleCount,
        pi.ProcessCount,
        pi.ThreadCount
    );
    let mut in_job = 0;
    // SAFETY: this process's pseudo-handle, no job named: whether it is in any.
    unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    println!("in a job: {}", in_job != 0);
    if in_job != 0 {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the limits of this process's own job, into a structure of their size.
        let ok = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            let flags = limits.BasicLimitInformation.LimitFlags;
            println!(
                "job: LimitFlags {flags:#x} process memory {} job memory {} active processes {}",
                if flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0 { limits.ProcessMemoryLimit.to_string() } else { "-".into() },
                if flags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0 { limits.JobMemoryLimit.to_string() } else { "-".into() },
                limits.BasicLimitInformation.ActiveProcessLimit
            );
        } else {
            // SAFETY: no arguments.
            println!("job: QueryInformationJobObject error {}", unsafe { GetLastError() });
        }
    }
}

fn performance() -> PERFORMANCE_INFORMATION {
    let mut pi = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: a PERFORMANCE_INFORMATION of its own size.
    unsafe { GetPerformanceInfo(&mut pi, pi.cb) };
    pi
}

/// This process's commit charge (PrivateUsage), nonpaged and paged pool quota, in bytes.
fn process_counters() -> (u64, u64, u64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: counters of their own size, of this process.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    (c.PrivateUsage as u64, c.QuotaNonPagedPoolUsage as u64, c.QuotaPagedPoolUsage as u64)
}

fn handles() -> u64 {
    let mut n = 0u32;
    // SAFETY: this process's pseudo-handle and a count to fill.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
    u64::from(n)
}

/// The SizeOfStackReserve and SizeOfStackCommit of this process's image, as loaded.
fn image_stack() -> Option<(u64, u64)> {
    // SAFETY: this process's own image's base, which stays loaded.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) }.cast::<u8>().cast_const();
    if base.is_null() {
        return None;
    }
    // SAFETY: a loaded image begins with its DOS header, whose e_lfanew is its NT headers'
    // offset; both read unaligned, as copies.
    let nt = unsafe {
        let dos: IMAGE_DOS_HEADER = std::ptr::read_unaligned(base.cast());
        let at = base.offset(dos.e_lfanew as isize);
        std::ptr::read_unaligned(at.cast::<IMAGE_NT_HEADERS64>())
    };
    let opt = nt.OptionalHeader;
    Some((opt.SizeOfStackReserve, opt.SizeOfStackCommit))
}

/// The same of an image file at `path`, read from its headers (PE Format: e_lfanew at 0x3c,
/// the signature, the 20-byte COFF header, then the optional header, PE32+ magic 0x20b, its
/// SizeOfStackReserve at 72 and SizeOfStackCommit at 80).
fn file_stack(path: &str) -> Result<(u64, u64), String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).map_err(|e| e.to_string())?;
    head.truncate(n);
    let u32_at = |i: usize| head.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u64_at = |i: usize| head.get(i..i + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes);
    let pe = u32_at(0x3c).ok_or("no e_lfanew")? as usize;
    if head.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("no PE signature".into());
    }
    let opt = pe + 24;
    let magic = head.get(opt..opt + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or("short")?;
    if magic != 0x20b {
        return Err(format!("magic {magic:#x}, not PE32+"));
    }
    Ok((u64_at(opt + 72).ok_or("short")?, u64_at(opt + 80).ok_or("short")?))
}

/// What the calling thread holds as it is asked, read before it prints anything: its stack's
/// reservation and committed bytes, the guard pages among them, its stack guarantee, each
/// region (offset, size, state, protection), and its TEB's address and region.
#[derive(Debug, Default)]
struct Held {
    reserved: usize,
    committed: usize,
    guard: usize,
    guarantee: u32,
    regions: Vec<(usize, usize, u32, u32)>,
    teb: usize,
    teb_region: usize,
    teb_allocation_committed: usize,
}

fn held() -> Held {
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: two words to fill with this thread's stack's bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut h = Held { reserved: high - low, regions: Vec::with_capacity(8), ..Default::default() };
    let mut at = low;
    while at < high {
        let Some(m) = query(at) else { break };
        let end = (m.BaseAddress as usize).saturating_add(m.RegionSize).min(high);
        h.regions.push((at - low, end - at, m.State, m.Protect));
        if m.State == MEM_COMMIT {
            h.committed += end - at;
            if m.Protect & PAGE_GUARD != 0 {
                h.guard += end - at;
            }
        }
        at = end;
    }
    // SAFETY: 0 asks the current guarantee, which it writes back.
    unsafe { SetThreadStackGuarantee(&mut h.guarantee) };
    h.teb = teb() as usize;
    if let Some(m) = query(h.teb) {
        h.teb_region = m.RegionSize;
        let base = m.AllocationBase as usize;
        let mut at = base;
        while let Some(r) = query(at) {
            if r.AllocationBase as usize != base || r.RegionSize == 0 {
                break;
            }
            if r.State == MEM_COMMIT {
                h.teb_allocation_committed += r.RegionSize;
            }
            at += r.RegionSize;
        }
    }
    h
}

/// The calling thread's stack and TEB, region by region.
fn probe() {
    let h = held();
    println!("  stack reserved {} bytes; guarantee {}", h.reserved, h.guarantee);
    for (offset, size, state, protect) in &h.regions {
        println!("    +{offset:#x} {size} bytes state {state:#x} protect {protect:#x}");
    }
    println!("  stack committed {} bytes, of which guard {}", h.committed, h.guard);
    println!(
        "  TEB {:#x}: its region {} bytes; its allocation's committed bytes {}",
        h.teb, h.teb_region, h.teb_allocation_committed
    );
    println!(
        "  winternl.h's TEB: {} bytes, {} pages",
        size_of::<windows_sys::Win32::System::Threading::TEB>(),
        size_of::<windows_sys::Win32::System::Threading::TEB>().div_ceil(4096)
    );
}

fn query(at: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut m = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query of an address of this process into a structure of its size.
    let n = unsafe { VirtualQuery(at as *const _, &mut m, size_of::<MEMORY_BASIC_INFORMATION>()) };
    (n != 0).then_some(m)
}

/// NtCurrentTeb (winnt.h): NT_TIB's Self, at gs:0x30 on x64, x18 on ARM64.
#[cfg(target_arch = "x86_64")]
fn teb() -> *const u8 {
    let teb: *const u8;
    // SAFETY: reads the current thread's TEB pointer, as winnt.h's NtCurrentTeb does.
    unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
    teb
}

#[cfg(target_arch = "aarch64")]
fn teb() -> *const u8 {
    let teb: *const u8;
    // SAFETY: reads x18, the current thread's TEB on ARM64 Windows, as NtCurrentTeb does.
    unsafe { std::arch::asm!("mov {}, x18", out(reg) teb, options(nostack, nomem, preserves_flags)) };
    teb
}

/// `n` threads started and held parked: what the process and the system were charged.
fn burst_round(round: usize, n: usize) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let parked = Arc::new((Mutex::new(0usize), Condvar::new()));
    let (p0, np0, pp0) = process_counters();
    let h0 = handles();
    let c0 = performance();
    let mut threads = Vec::with_capacity(n);
    for _ in 0..n {
        let (gate, parked) = (gate.clone(), parked.clone());
        match std::thread::Builder::new().spawn(move || {
            *parked.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            parked.1.notify_one();
            let mut open = gate.0.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = gate.1.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }) {
            Ok(t) => threads.push(t),
            Err(e) => {
                println!("burst {round}: spawn {} failed: {e}", threads.len());
                break;
            }
        }
    }
    let started = threads.len();
    {
        let mut count = parked.0.lock().unwrap_or_else(|e| e.into_inner());
        while *count < started {
            count = parked.1.wait(count).unwrap_or_else(|e| e.into_inner());
        }
    }
    let (p1, np1, pp1) = process_counters();
    let h1 = handles();
    let c1 = performance();
    *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    gate.1.notify_all();
    for t in threads {
        let _ = t.join();
    }
    let (p2, _, _) = process_counters();
    let c2 = performance();
    let per = |a: u64, b: u64| (b as f64 - a as f64) / started.max(1) as f64;
    let page = c0.PageSize as u64;
    println!(
        "burst {round}: {started} threads parked: per thread process commit {:.0} B, nonpaged quota {:.0} B, paged quota {:.0} B, handles {:.3}, system commit {:.0} B; after join process commit {:+} B system commit {:+} B; system threads {} -> {}",
        per(p0, p1),
        per(np0, np1),
        per(pp0, pp1),
        per(h0, h1),
        per(c0.CommitTotal as u64 * page, c1.CommitTotal as u64 * page),
        p2 as i64 - p0 as i64,
        (c2.CommitTotal as i64 - c0.CommitTotal as i64) * page as i64,
        c0.ThreadCount,
        c1.ThreadCount
    );
}

/// A copy of this harness in a job of its own that holds its process's commit to `limit`
/// bytes, filling it with parked threads: what it says, line by line.
fn in_job(limit: u64) {
    // SAFETY: an anonymous job, closed below.
    let job: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        // SAFETY: no arguments.
        return println!("job: CreateJobObjectW error {}", unsafe { GetLastError() });
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.ProcessMemoryLimit = limit as usize;
    // SAFETY: limits of their own size, on the job just made.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        // SAFETY: no arguments.
        return println!("job: SetInformationJobObject error {}", unsafe { GetLastError() });
    }
    let Ok(exe) = std::env::current_exe() else { return println!("job: no current_exe") };
    let mut child = match Command::new(exe).arg("--fill").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return println!("job: spawn: {e}"),
    };
    // SAFETY: the job and the child's process handle, both held.
    if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
        // SAFETY: no arguments.
        println!("job: AssignProcessToJobObject error {}", unsafe { GetLastError() });
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"g");
    }
    if let Some(out) = child.stdout.take() {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            println!("child: {line}");
        }
    }
    println!("child: exit {:?}", child.wait());
    // SAFETY: the job made above, closed once.
    unsafe { CloseHandle(job) };
}

/// In a job holding its commit: starts parked threads, each before the next, until Windows
/// refuses one; lets them all go before it says anything, so that nothing it formats needs
/// commit the threads hold.
fn fill() {
    let mut go = [0u8; 1];
    let _ = std::io::stdin().read_exact(&mut go);
    host();
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the limits of this process's own job, into a structure of their size.
    unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    let limit = limits.ProcessMemoryLimit;
    // A thread as thread_limit counts one (crates/vmm platform/windows_process.rs): its
    // stack's guard pages, the image's SizeOfStackCommit and winternl.h's TEB, whole pages.
    let probe = std::thread::spawn(held).join().unwrap_or_default();
    let whole = |bytes: usize| bytes.div_ceil(4096) * 4096;
    let teb = whole(size_of::<windows_sys::Win32::System::Threading::TEB>());
    let initial = whole(image_stack().map_or(0, |(_, commit)| commit as usize));
    let counted = probe.guard + initial + teb;
    let derived = limit / counted.max(1);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let parked = Arc::new((Mutex::new(0usize), Condvar::new()));
    // Room for a thread a page of the limit, the least a thread commits, made before the
    // limit is near.
    let mut threads = Vec::with_capacity(limit / 4096);
    let (start, _, _) = process_counters();
    let refused = loop {
        let (its_gate, its_count) = (gate.clone(), parked.clone());
        match std::thread::Builder::new().spawn(move || {
            *its_count.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            its_count.1.notify_one();
            let mut open = its_gate.0.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = its_gate.1.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }) {
            Ok(t) => {
                threads.push(t);
                let mut count = parked.0.lock().unwrap_or_else(|e| e.into_inner());
                while *count < threads.len() {
                    count = parked.1.wait(count).unwrap_or_else(|e| e.into_inner());
                }
            }
            Err(e) => break e,
        }
    };
    let (at_refusal, _, _) = process_counters();
    let started = threads.len();
    *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    gate.1.notify_all();
    for t in threads {
        let _ = t.join();
    }
    println!(
        "limit {limit}: commit {start} before the first thread; refused thread {} ({:?}) at commit {at_refusal}, {} short of the limit; charged {:.0} B a thread",
        started + 1,
        refused.raw_os_error(),
        (limit as u64).saturating_sub(at_refusal),
        (at_refusal - start) as f64 / started.max(1) as f64
    );
    println!(
        "counted a thread {counted} B (guard {}, initial commit {initial}, TEB {teb}; {} of its stack committed as it began): the limit holds {derived}; Windows started {started}",
        probe.guard, probe.committed
    );
}

/// The job's priority limits as this process reads them, and what each of SetThreadPriority's
/// levels does to the calling thread under them: its return, its error, and the priority
/// GetThreadPriority reads back.
fn priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetPriorityClass, GetThreadPriority, SetThreadPriority};
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the limits of this process's own job, into a structure of their size.
    let read = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: this process's pseudo-handle.
    let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
    println!(
        "job read {read}: LimitFlags {:#x}, PriorityClass {:#x}, SchedulingClass {}; this process's priority class {class:#x}",
        limits.BasicLimitInformation.LimitFlags,
        limits.BasicLimitInformation.PriorityClass,
        limits.BasicLimitInformation.SchedulingClass
    );
    for (name, level) in [
        ("ABOVE_NORMAL", 1),
        ("HIGHEST", 2),
        ("TIME_CRITICAL", 15),
        ("NORMAL", 0),
        ("BELOW_NORMAL", -1),
        ("LOWEST", -2),
        ("IDLE", -15),
        ("NORMAL", 0),
    ] {
        // SAFETY: the calling thread's pseudo-handle.
        let set = unsafe { SetThreadPriority(GetCurrentThread(), level) };
        // SAFETY: no arguments.
        let error = unsafe { GetLastError() };
        // SAFETY: the calling thread's pseudo-handle.
        let now = unsafe { GetThreadPriority(GetCurrentThread()) };
        println!("SetThreadPriority({name}, {level}) -> {set} (last error {error}); GetThreadPriority -> {now}");
    }
}

/// What Windows charges this process for each of `n` threads it makes suspended, their stacks
/// reserved as std reserves its threads' (a std thread's reservation): before any runs a line,
/// no heap of theirs and no stack guarantee; beside a std thread's guard, initial commit, TEB
/// and stack guarantee, as thread_limit counts a thread.
fn suspended(n: usize) {
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateThread, INFINITE, ResumeThread, STACK_SIZE_PARAM_IS_A_RESERVATION, WaitForSingleObject,
    };
    unsafe extern "system" fn nothing(_: *mut core::ffi::c_void) -> u32 {
        0
    }
    let probe = std::thread::spawn(held).join().unwrap_or_default();
    let initial = image_stack().map_or(0, |(_, commit)| commit as usize).div_ceil(4096) * 4096;
    let teb = size_of::<windows_sys::Win32::System::Threading::TEB>().div_ceil(4096) * 4096;
    println!(
        "a std thread: reserved {}, guard {}, guarantee {}, initial commit {initial}, TEB {teb}: counted {}",
        probe.reserved,
        probe.guard,
        probe.guarantee,
        probe.guard + initial + teb
    );
    let mut handles = Vec::with_capacity(n);
    let (before, _, _) = process_counters();
    for _ in 0..n {
        // SAFETY: a thread of this process at a function that returns at once, made suspended,
        // its stack reserved as a std thread's.
        let h = unsafe {
            CreateThread(
                std::ptr::null(),
                probe.reserved,
                Some(nothing),
                std::ptr::null(),
                CREATE_SUSPENDED | STACK_SIZE_PARAM_IS_A_RESERVATION,
                std::ptr::null_mut(),
            )
        };
        if h.is_null() {
            // SAFETY: no arguments.
            println!("CreateThread failed after {}: {}", handles.len(), unsafe { GetLastError() });
            break;
        }
        handles.push(h);
    }
    let (after, _, _) = process_counters();
    let made = handles.len().max(1) as u64;
    println!(
        "{} suspended threads: charged {} B, {} B each, remainder {}; counted less the guarantee: {}",
        handles.len(),
        after - before,
        (after - before) / made,
        (after - before) % made,
        probe.guard + initial + teb - probe.guarantee as usize
    );
    for h in handles {
        // SAFETY: each thread made above, resumed to return at once, waited for, closed once.
        unsafe {
            ResumeThread(h);
            WaitForSingleObject(h, INFINITE);
            CloseHandle(h);
        }
    }
    let (end, _, _) = process_counters();
    println!("after they ended: {end} (before {before})");
}

/// Every region of this process's address space: base, size, state, protection, type and
/// allocation base, as VirtualQuery says them.
fn map() -> Vec<(usize, usize, u32, u32, u32, usize)> {
    let mut regions = Vec::with_capacity(4096);
    let mut at = 0usize;
    while let Some(m) = query(at) {
        regions.push((m.BaseAddress as usize, m.RegionSize, m.State, m.Protect, m.Type, m.AllocationBase as usize));
        let Some(next) = (m.BaseAddress as usize).checked_add(m.RegionSize) else { break };
        if next <= at {
            break;
        }
        at = next;
    }
    regions
}

/// The committed regions one thread made suspended adds or changes, its stack reserved as a
/// std thread's: where each of the pages Windows charges for it lies. And whether user-mode
/// shadow stacks (CET) are on for this process (GetProcessMitigationPolicy).
fn regions() {
    use windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_USER_SHADOW_STACK_POLICY;
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateThread, GetProcessMitigationPolicy, INFINITE, ProcessUserShadowStackPolicy,
        ResumeThread, STACK_SIZE_PARAM_IS_A_RESERVATION, WaitForSingleObject,
    };
    unsafe extern "system" fn nothing(_: *mut core::ffi::c_void) -> u32 {
        0
    }
    let mut policy = PROCESS_MITIGATION_USER_SHADOW_STACK_POLICY::default();
    // SAFETY: this process's pseudo-handle and a policy structure of its size.
    let read = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessUserShadowStackPolicy,
            (&raw mut policy).cast(),
            size_of::<PROCESS_MITIGATION_USER_SHADOW_STACK_POLICY>(),
        )
    };
    // SAFETY: the union's flags, which the call filled.
    println!("user shadow stack policy read {read}: flags {:#x}", unsafe { policy.Anonymous.Flags });
    let reserved = std::thread::spawn(held).join().unwrap_or_default().reserved;
    let rounds: usize = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(2);
    let quiet = rounds > 2;
    let mut usual = 0usize;
    // Each kept, suspended, until the last is made: as the batch kept its 256.
    let mut kept = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let before = map();
        let (c0, _, _) = process_counters();
        // SAFETY: a thread of this process at a function that returns at once, made suspended.
        let h = unsafe {
            CreateThread(
                std::ptr::null(),
                reserved,
                Some(nothing),
                std::ptr::null(),
                CREATE_SUSPENDED | STACK_SIZE_PARAM_IS_A_RESERVATION,
                std::ptr::null_mut(),
            )
        };
        let (c1, _, _) = process_counters();
        let after = map();
        let charged = c1 as i64 - c0 as i64;
        let changed: Vec<_> = after.iter().filter(|r| r.2 == MEM_COMMIT && !before.contains(r)).collect();
        let gone: Vec<_> = before.iter().filter(|r| r.2 == MEM_COMMIT && !after.contains(r)).collect();
        // Committed bytes by allocation, before and after: the new stack's allocation (new, with
        // guard pages), and every other that grew.
        let by_allocation = |m: &[(usize, usize, u32, u32, u32, usize)]| {
            let mut total = std::collections::BTreeMap::<usize, i64>::new();
            for r in m.iter().filter(|r| r.2 == MEM_COMMIT) {
                *total.entry(r.5).or_default() += r.1 as i64;
            }
            total
        };
        let (b, a) = (by_allocation(&before), by_allocation(&after));
        let mut grew: Vec<(usize, i64)> = a
            .iter()
            .map(|(base, t)| (*base, t - b.get(base).copied().unwrap_or(0)))
            .filter(|(_, d)| *d != 0)
            .collect();
        grew.extend(b.iter().filter(|(base, _)| !a.contains_key(base)).map(|(base, t)| (*base, -t)));
        let stack = changed.iter().find(|r| r.3 & PAGE_GUARD != 0).map(|r| r.5);
        let past_stack: i64 = grew.iter().filter(|(base, _)| Some(*base) != stack).map(|(_, d)| d).sum();
        let others = grew.iter().filter(|(base, _)| Some(*base) != stack).count();
        // The usual: 28,672 B charged; its stack's allocation new, and one other grown by 8 KiB.
        if quiet && charged == 28672 && stack.is_some() && others == 1 && past_stack == 8192 {
            usual += 1;
        } else {
            println!("round {round}: allocations changed {grew:x?}; past the stack {past_stack} B");
            println!("round {round}: one suspended thread charged {charged} B");
            for r in &changed {
                println!(
                    "  new or changed committed region {:#x} {} B protect {:#x} type {:#x} allocation {:#x}",
                    r.0, r.1, r.3, r.4, r.5
                );
            }
            for r in &gone {
                println!("  committed before, not now: {:#x} {} B protect {:#x}", r.0, r.1, r.3);
            }
        }
        kept.push(h);
    }
    for h in kept {
        // SAFETY: each thread made above, resumed to return at once, waited for, closed once.
        unsafe {
            ResumeThread(h);
            WaitForSingleObject(h, INFINITE);
            CloseHandle(h);
        }
    }
    if quiet {
        println!("{usual} of {rounds} rounds the usual: 28672 B charged, the stack's allocation new, one other grown 8192 B");
    }
}

/// Runs `cmd` with `args` in a job of this process's making that holds its processes to
/// priority class `class` (JOB_OBJECT_LIMIT_PRIORITY_CLASS), nested in any job this process is
/// in: this process joins it first, and `cmd`, its child, is in it from its start. Its exit
/// code, or 1 where the job could not be made so.
fn in_priority_job(class: u32, cmd: &str, args: &[String]) -> i32 {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOBOBJECT_BASIC_LIMIT_INFORMATION, JobObjectBasicLimitInformation,
    };
    // SAFETY: an anonymous job, which this process stays in until it ends.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        // SAFETY: no arguments.
        eprintln!("CreateJobObjectW: {}", unsafe { GetLastError() });
        return 1;
    }
    let limits = JOBOBJECT_BASIC_LIMIT_INFORMATION {
        LimitFlags: JOB_OBJECT_LIMIT_PRIORITY_CLASS,
        PriorityClass: class,
        ..Default::default()
    };
    // SAFETY: basic limits of their own size, on the job just made.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectBasicLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        // SAFETY: no arguments.
        eprintln!("SetInformationJobObject(PRIORITY_CLASS {class:#x}): {}", unsafe { GetLastError() });
        return 1;
    }
    // SAFETY: this process's pseudo-handle, into the job, nested in any it is in already.
    if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
        // SAFETY: no arguments.
        eprintln!("AssignProcessToJobObject: {}", unsafe { GetLastError() });
        return 1;
    }
    println!("in a job holding its processes to priority class {class:#x}: {cmd} {}", args.join(" "));
    let _ = std::io::stdout().flush();
    match Command::new(cmd).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("{cmd}: {e}");
            1
        }
    }
}
