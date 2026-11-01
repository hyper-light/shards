//! A group of VM processes in a job of its own, for `--cgroup-parent` on Windows (PM M359,
//! D183 slice 3), before it is built:
//!
//! - whether a child already in a job of its own, as a VM process is (its last handle's close
//!   ending it, one active process), can be put in a group's job afterwards, the group's
//!   empty and the group's holding another such child already: as a warm VM taken by a run
//!   under a group would have to be;
//! - whether children made in the group's job and their own at once (PROC_THREAD_ATTRIBUTE_
//!   JOB_LIST, the group's first) both run, each its own job's;
//! - what the group's job memory limit (JOB_OBJECT_LIMIT_JOB_MEMORY) does to its children's
//!   commits: one alone past it, and two each within it whose sum is past it;
//! - what the group's CPU rate's hard cap (JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP) leaves a
//!   child that spins on as many threads as the host has CPUs.
//!
//! The children are this program again, `child-commit MIB` (commits MIB, touches it, says
//! what it got, then waits for its stdin to close) and `child-spin MS` (spins every CPU
//! for MS milliseconds, then says its CPU time).

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::io::{Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetProcessTimes, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A job: ending its processes as its last handle closes and letting one run, as a VM
/// process's own is (`own`); or a group's, with neither.
fn job(own: bool) -> Result<OwnedHandle, String> {
    // SAFETY: an anonymous job, owned below.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(format!("CreateJobObjectW: {}", last()));
    }
    // SAFETY: a handle just made, which nothing else owns.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    if own {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        set_limits(&job, &limits)?;
    }
    Ok(job)
}

fn set_limits(job: &OwnedHandle, limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> Result<(), String> {
    // SAFETY: limits of their own size, on a job of ours.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(format!("SetInformationJobObject: {}", last()));
    }
    Ok(())
}

/// A child of this program running `args`, in `jobs` as it is made (the first the outermost),
/// its stdin and stdout pipes: suspended, then resumed.
struct Child {
    process: OwnedHandle,
    stdin: Option<std::fs::File>,
    stdout: std::fs::File,
}

fn spawn(args: &str, jobs: &[HANDLE]) -> Result<Child, String> {
    let (stdin_child, stdin_ours) = std::io::pipe().map_err(|e| e.to_string())?;
    let (stdout_ours, stdout_child) = std::io::pipe().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut line = wide(&format!("\"{}\" {args}", exe.display()));
    let mut size = 0usize;
    // SAFETY: a size query, as InitializeProcThreadAttributeList's documentation says.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    let mut buffer = vec![0u8; size];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();
    // SAFETY: a buffer of the size asked for, one attribute.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(format!("InitializeProcThreadAttributeList: {}", last()));
    }
    // SAFETY: the list just made; `jobs` outlives it.
    if !jobs.is_empty()
        && unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast::<c_void>(),
                std::mem::size_of_val(jobs),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
    {
        let e = format!("UpdateProcThreadAttribute(JOB_LIST): {}", last());
        // SAFETY: the list made above.
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err(e);
    }
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin_child.as_raw_handle();
    si.StartupInfo.hStdOutput = stdout_child.as_raw_handle();
    si.StartupInfo.hStdError = std::io::stderr().as_raw_handle();
    si.lpAttributeList = list;
    // The pipes' child ends inheritable for this one spawn.
    for h in [stdin_child.as_raw_handle(), stdout_child.as_raw_handle()] {
        // SAFETY: handles of ours.
        unsafe { windows_sys::Win32::Foundation::SetHandleInformation(h, 1, 1) };
    }
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: a command line of our own, the attribute list above, a process info to fill.
    let made = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    let made_err = (made == 0).then(last);
    // SAFETY: the list made above, no longer needed.
    unsafe { DeleteProcThreadAttributeList(list) };
    drop((stdin_child, stdout_child));
    if let Some(e) = made_err {
        return Err(format!("CreateProcessW: {e}"));
    }
    // SAFETY: handles CreateProcessW made, owned from here on.
    let (process, thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess), OwnedHandle::from_raw_handle(pi.hThread)) };
    // SAFETY: the child's suspended thread.
    unsafe { ResumeThread(thread.as_raw_handle()) };
    Ok(Child {
        process,
        stdin: Some(std::fs::File::from(OwnedHandle::from(stdin_ours))),
        stdout: std::fs::File::from(OwnedHandle::from(stdout_ours)),
    })
}

impl Child {
    /// Its first line.
    fn line(&mut self) -> String {
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while let Ok(1) = self.stdout.read(&mut byte) {
            if byte[0] == b'\n' {
                break;
            }
            out.push(byte[0]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Its end: its stdin closed, then waited for; killed past 30 s.
    fn end(mut self) {
        drop(self.stdin.take());
        // SAFETY: our child's process handle.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 30_000) } != 0 {
            // SAFETY: as above.
            unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
        }
    }
}

fn assign(job: &OwnedHandle, child: &Child) -> String {
    // SAFETY: a job and a process of ours.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.process.as_raw_handle()) } == 0 {
        format!("refused: {}", last())
    } else {
        "assigned".into()
    }
}

fn cpu_ns(process: HANDLE) -> u64 {
    let [mut c, mut e, mut k, mut u] = [FILETIME::default(); 4];
    // SAFETY: a process handle and four FILETIMEs.
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) };
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    (t(k) + t(u)) * 100
}

fn child_commit(mib: usize) {
    // SAFETY: a fresh allocation of our own, touched below only where it succeeded.
    let p = unsafe { VirtualAlloc(std::ptr::null(), mib << 20, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    if p.is_null() {
        let _ = writeln!(std::io::stdout(), "commit of {mib} MiB refused: {}", last());
    } else {
        for page in (0..mib << 20).step_by(4096) {
            // SAFETY: within the allocation just committed.
            unsafe { p.cast::<u8>().add(page).write_volatile(1) };
        }
        let _ = writeln!(std::io::stdout(), "committed and touched {mib} MiB");
    }
    let _ = std::io::stdout().flush();
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
}

fn child_spin(ms: u64) {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let until = Instant::now() + Duration::from_millis(ms);
    std::thread::scope(|s| {
        for _ in 0..cpus {
            s.spawn(|| {
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            });
        }
    });
    // SAFETY: this process's own pseudo-handle.
    let used = cpu_ns(unsafe { GetCurrentProcess() });
    let _ = writeln!(std::io::stdout(), "spun {cpus} threads {ms} ms: {used} ns of CPU");
    let _ = std::io::stdout().flush();
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
}

fn probe() -> Result<(), String> {
    let say = |s: String| {
        let _ = writeln!(std::io::stdout(), "m359 {s}");
    };
    // 1. Already in a job of its own, then put in a group's.
    let group = job(false)?;
    let own_a = job(true)?;
    let mut a = spawn("child-commit 1", &[own_a.as_raw_handle()])?;
    say(format!("child A: {}", a.line()));
    say(format!("child A, in its own job, put in an empty group's after: {}", assign(&group, &a)));
    let own_b = job(true)?;
    let mut b = spawn("child-commit 1", &[own_b.as_raw_handle()])?;
    say(format!("child B: {}", b.line()));
    say(format!("child B, in its own job, put in a group's holding A after: {}", assign(&group, &b)));
    a.end();
    b.end();
    // 2. Made in a group's job and its own at once.
    let limit_mib = 256u64;
    let group = job(false)?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
    limits.JobMemoryLimit = (limit_mib << 20) as usize;
    set_limits(&group, &limits)?;
    let (own_c, own_d, own_e) = (job(true)?, job(true)?, job(true)?);
    let made = |own: &OwnedHandle, mib: u64| {
        spawn(
            &format!("child-commit {mib}"),
            &[group.as_raw_handle(), own.as_raw_handle()],
        )
    };
    match made(&own_c, 512) {
        Ok(mut c) => {
            say(format!("child C, made in a group of a {limit_mib} MiB job memory limit and its own, asks 512 MiB: {}", c.line()));
            c.end();
        }
        Err(e) => say(format!("child C, made in a group's job and its own: {e}")),
    }
    let mut d = made(&own_d, 160)?;
    say(format!("child D, the same group, asks 160 MiB: {}", d.line()));
    let mut e = made(&own_e, 160)?;
    say(format!("child E, beside D in the group, asks 160 MiB more: {}", e.line()));
    d.end();
    e.end();
    // 3. The group's CPU rate, hard capped at a quarter of the host's CPUs.
    let group = job(false)?;
    let rate = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
        ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 2500 },
    };
    // SAFETY: a rate of its own size, on a job of ours.
    if unsafe {
        SetInformationJobObject(
            group.as_raw_handle(),
            JobObjectCpuRateControlInformation,
            (&raw const rate).cast(),
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        )
    } == 0
    {
        say(format!("a group's CPU rate: {}", last()));
    } else {
        let own = job(true)?;
        let t0 = Instant::now();
        let mut f = spawn("child-spin 2000", &[group.as_raw_handle(), own.as_raw_handle()])?;
        let said = f.line();
        let wall = t0.elapsed();
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        say(format!(
            "child F, in a group capped at 25% of the host's {cpus} CPUs: {said}; {:.2} s of wall time",
            wall.as_secs_f64()
        ));
        f.end();
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2).and_then(|v| v.parse::<u64>().ok())) {
        (Some("child-commit"), Some(mib)) => child_commit(mib as usize),
        (Some("child-spin"), Some(ms)) => child_spin(ms),
        _ => {
            if let Err(e) = probe() {
                let _ = writeln!(std::io::stdout(), "m359 failed: {e}");
            }
        }
    }
}
