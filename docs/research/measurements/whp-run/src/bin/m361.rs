//! What a job's CPU rate hard cap (JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP) holds its
//! processes to over time, for `cpu.max` of a group on Windows (PM M361, D183 slice 3): M359
//! saw a 2 s spin of every CPU in a job capped at 25% use 1.21 and 1.36 CPUs on average, past
//! the cap's 1. Here a child spins every CPU for 30 s in a group's job and its own, the group
//! capped at 25% and at 50% of the host's CPUs, and with no cap; its CPU time is read from
//! outside each second (GetProcessTimes), and each second's CPUs used told as n, p50, p90,
//! p99 and max, with the whole run's mean beside the cap's CPUs.
//!
//! The child is this program again, `child-spin MS` (spins every CPU for MS milliseconds,
//! then waits for its stdin to close).

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::io::{Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetProcessTimes, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
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

fn cpu_ns(process: HANDLE) -> u64 {
    let [mut c, mut e, mut k, mut u] = [FILETIME::default(); 4];
    // SAFETY: a process handle and four FILETIMEs.
    unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) };
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    (t(k) + t(u)) * 100
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
    let _ = writeln!(std::io::stdout(), "spun {cpus} threads {ms} ms");
    let _ = std::io::stdout().flush();
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
}

/// The `q`th quantile of `sorted`, nearest rank.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

/// A group's job capped at `rate` ten-thousandths of the host's CPUs, or none.
fn group(rate: Option<u32>) -> Result<OwnedHandle, String> {
    let job = job(false)?;
    if let Some(rate) = rate {
        let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: rate },
        };
        // SAFETY: a rate of its own size, on a job of ours.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectCpuRateControlInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(format!("a group's CPU rate: {}", last()));
        }
    }
    Ok(job)
}

fn probe() -> Result<(), String> {
    const SECONDS: u64 = 30;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let say = |s: String| {
        let _ = writeln!(std::io::stdout(), "m361 {s}");
        let _ = std::io::stdout().flush();
    };
    say(format!("{cpus} CPUs"));
    for rate in [Some(2500u32), Some(5000), None] {
        let group = group(rate)?;
        let own = job(true)?;
        let mut child = spawn(
            &format!("child-spin {}", SECONDS * 1000),
            &[group.as_raw_handle(), own.as_raw_handle()],
        )?;
        let mut per_second = Vec::new();
        let t0 = Instant::now();
        let mut before = cpu_ns(child.process.as_raw_handle());
        for second in 1..=SECONDS {
            let at = t0 + Duration::from_secs(second);
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
            let now = cpu_ns(child.process.as_raw_handle());
            per_second.push(now.saturating_sub(before) as f64 / 1e9);
            before = now;
        }
        let said = child.line();
        let total = cpu_ns(child.process.as_raw_handle()) as f64 / 1e9;
        child.end();
        let mean: f64 = per_second.iter().sum::<f64>() / per_second.len() as f64;
        let mut sorted = per_second.clone();
        sorted.sort_by(f64::total_cmp);
        let cap = rate.map_or_else(|| "no cap".to_string(), |r| format!("capped at {:.2} CPUs", f64::from(r) / 10_000.0 * cpus as f64));
        say(format!(
            "{cap}: {said}; CPUs used each second: n {} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}, mean {mean:.2}; {total:.1} s of CPU in all",
            sorted.len(),
            quantile(&sorted, 0.5),
            quantile(&sorted, 0.9),
            quantile(&sorted, 0.99),
            sorted.last().copied().unwrap_or(0.0),
        ));
        say(format!(
            "{cap}: each second's CPUs: {}",
            per_second.iter().map(|c| format!("{c:.2}")).collect::<Vec<_>>().join(" ")
        ));
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2).and_then(|v| v.parse::<u64>().ok())) {
        (Some("child-spin"), Some(ms)) => child_spin(ms),
        _ => {
            if let Err(e) = probe() {
                let _ = writeln!(std::io::stdout(), "m361 failed: {e}");
            }
        }
    }
}
