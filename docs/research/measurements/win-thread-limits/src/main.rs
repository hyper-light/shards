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
//! Usage: win-thread-limits [--image PATH] [--burst N] [--rounds R] [--job-limit MIB]

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
