//! Other processes' CPU and memory, as each host counts them, and this host's description.
//!
//! - CPU: a process's whole CPU time, its ended threads' included, at the host's finest grain:
//!   Linux's process CPU-time clock (`clock_getcpuclockid(3)`, sum_exec_runtime, ns), which
//!   counts a KVM vCPU thread's guest time; macOS's `proc_pid_rusage` user and system time
//!   (Mach time, converted); Windows' `GetProcessTimes` (100 ns).
//! - Memory: what the process alone holds, as each host says it: Linux's proportional set
//!   size (`smaps_rollup` Pss), which splits a page shared by n processes n ways, else its
//!   resident set; macOS's physical footprint (`ri_phys_footprint`: private dirty and
//!   compressed pages, page tables included), as the fleet measurement (M49) reads it;
//!   Windows' working set. `alt` is the resident set (Linux, macOS) or private bytes
//!   (Windows).

/// A process, its parent and its name.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

/// What a process has used and holds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub cpu_ns: u64,
    pub mem: u64,
    pub alt: u64,
}

/// What [`Usage::mem`] is on this host.
pub const MEM: &str = if cfg!(target_os = "linux") {
    "pss"
} else if cfg!(target_os = "macos") {
    "phys_footprint"
} else {
    "working_set"
};

/// What [`Usage::alt`] is on this host.
pub const ALT: &str = if cfg!(windows) { "private_bytes" } else { "rss" };

/// `roots` and every process below them, from `table`.
pub fn tree(table: &[Proc], roots: &[u32]) -> Vec<Proc> {
    let mut out: Vec<Proc> = table.iter().filter(|p| roots.contains(&p.pid)).cloned().collect();
    let mut at = 0;
    while let Some(parent) = out.get(at).map(|p| p.pid) {
        for p in table {
            if p.ppid == parent && p.pid != parent && !out.iter().any(|o| o.pid == p.pid) {
                out.push(p.clone());
            }
        }
        at = at.saturating_add(1);
    }
    out
}

/// What a process of Shards' is to this measurement: a microVM's VM process, its network
/// process, or another of Shards' (its daemon, its spawner, a `shards` that waits for its VM).
pub fn class(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    if name.starts_with("shards-vm") {
        "vm"
    } else if name.starts_with("shards-net") {
        "net"
    } else if name.starts_with("control-end") {
        "endpoint"
    } else {
        "shards"
    }
}

/// The load the host was under: its load averages, or on Windows its CPUs' busy share since
/// `since` ([`SystemTimes`]).
pub fn load(since: Option<SystemTimes>) -> String {
    #[cfg(unix)]
    {
        let _ = since;
        let mut avg = [0f64; 3];
        // SAFETY: a writable array of three doubles, as getloadavg(3) asks.
        let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
        if n == 3 {
            let [a, b, c] = avg;
            return format!("loadavg {a:.2} {b:.2} {c:.2}");
        }
        "loadavg unknown".into()
    }
    #[cfg(windows)]
    {
        match (since, SystemTimes::now()) {
            (Some(a), Some(b)) => {
                let total = b.total.saturating_sub(a.total);
                let idle = b.idle.saturating_sub(a.idle);
                if total == 0 {
                    return "busy unknown".into();
                }
                format!(
                    "busy {:.1}% of all CPUs",
                    total.saturating_sub(idle) as f64 * 100.0 / total as f64
                )
            }
            _ => "busy unknown".into(),
        }
    }
}

/// The host's CPU time so far, idle and in all, for [`load`] on Windows.
#[derive(Debug, Clone, Copy)]
pub struct SystemTimes {
    #[cfg_attr(unix, allow(dead_code))]
    idle: u64,
    #[cfg_attr(unix, allow(dead_code))]
    total: u64,
}

impl SystemTimes {
    pub fn now() -> Option<SystemTimes> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::FILETIME;
            use windows_sys::Win32::System::Threading::GetSystemTimes;
            let mut idle = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            // SAFETY: three writable FILETIMEs.
            if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
                return None;
            }
            let t = |f: FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
            // The kernel's time includes its idle time (GetSystemTimes).
            Some(SystemTimes {
                idle: t(idle),
                total: t(kernel).saturating_add(t(user)),
            })
        }
        #[cfg(unix)]
        {
            Some(SystemTimes { idle: 0, total: 0 })
        }
    }
}

/// `name: value` lines of this host: its name, OS and kernel, CPU and CPU count.
pub fn host() -> Vec<(String, String)> {
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    let mut out = vec![
        ("hostname".to_string(), hostname()),
        (
            "platform".to_string(),
            format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        ("cpus".to_string(), cpus.to_string()),
    ];
    out.extend(os_lines());
    out
}

fn command(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
    #[cfg(unix)]
    {
        command("hostname", &[])
    }
}

#[cfg(target_os = "linux")]
fn os_lines() -> Vec<(String, String)> {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let os = read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default();
    let cpu = read("/proc/cpuinfo")
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .and_then(|v| v.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    vec![
        ("os".into(), os),
        ("kernel".into(), read("/proc/sys/kernel/osrelease").trim().to_string()),
        ("cpu".into(), cpu),
    ]
}

#[cfg(target_os = "macos")]
fn os_lines() -> Vec<(String, String)> {
    vec![
        ("os".into(), format!("macOS {}", command("sw_vers", &["-productVersion"]))),
        ("kernel".into(), command("uname", &["-r"])),
        ("cpu".into(), command("sysctl", &["-n", "machdep.cpu.brand_string"])),
    ]
}

#[cfg(windows)]
fn os_lines() -> Vec<(String, String)> {
    vec![
        ("os".into(), command("cmd", &["/c", "ver"])),
        (
            "cpu".into(),
            std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default(),
        ),
    ]
}

/// This thread's CPU time in nanoseconds.
pub fn thread_cpu_ns() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero timespec is a valid value to fill.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: a writable timespec; the calling thread's own clock.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) } != 0 {
            return 0;
        }
        nanos(ts)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: the calling thread's pseudo-handle and four writable FILETIMEs.
        let ok = unsafe { GetThreadTimes(GetCurrentThread(), created, exited, kernel, user) };
        if ok == 0 {
            return 0;
        }
        hundreds(*kernel).saturating_add(hundreds(*user)).saturating_mul(100)
    }
}

#[cfg(unix)]
fn nanos(ts: libc::timespec) -> u64 {
    u64::try_from(ts.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
}

#[cfg(windows)]
fn hundreds(f: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)
}

/// Lets this process hold as many descriptors as its hard limit allows: each connection of
/// the controller's holds a UDP socket and a waker, and each relay of the host endpoint's
/// two vsock descriptors and two wakers.
pub fn raise_descriptor_limit() {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero rlimit is a valid value to fill.
        let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
        // SAFETY: a writable rlimit.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
            return;
        }
        // macOS refuses a soft limit past OPEN_MAX (sys/syslimits.h, 10240) however high its
        // hard limit is (setrlimit(2), COMPATIBILITY).
        for want in [lim.rlim_max, lim.rlim_max.min(10240)] {
            let raised = libc::rlimit {
                rlim_cur: want,
                rlim_max: lim.rlim_max,
            };
            // SAFETY: a valid rlimit no higher than the hard limit.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } == 0 {
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn table() -> Result<Vec<Proc>, String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("/proc: {e}"))?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid ...", comm in parentheses that may hold any byte but NUL.
        let Some((head, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let name = head.split_once('(').map_or("", |(_, n)| n).to_string();
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        out.push(Proc { pid, ppid, name });
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
pub fn usage(pid: u32) -> Option<Usage> {
    let pid_t = libc::pid_t::try_from(pid).ok()?;
    let mut clock: libc::clockid_t = 0;
    // SAFETY: a writable clockid_t; the process's CPU-time clock, as clock_getcpuclockid(3)
    // gives any process's.
    if unsafe { libc::clock_getcpuclockid(pid_t, &raw mut clock) } != 0 {
        return None;
    }
    // SAFETY: an all-zero timespec is a valid value to fill.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: a writable timespec and the clock just given.
    if unsafe { libc::clock_gettime(clock, &raw mut ts) } != 0 {
        return None;
    }
    let kib = |text: &str, name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|k| k.saturating_mul(1024))
    };
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let rss = kib(&status, "VmRSS:").unwrap_or(0);
    let pss = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|r| kib(&r, "Pss:"));
    Some(Usage {
        cpu_ns: nanos(ts),
        mem: pss.unwrap_or(rss),
        alt: rss,
    })
}

#[cfg(target_os = "macos")]
pub fn table() -> Result<Vec<Proc>, String> {
    let size = std::mem::size_of::<libc::pid_t>();
    // SAFETY: a null buffer asks proc_listallpids(3) only for the count of processes.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let room = usize::try_from(count).map_err(|_| "proc_listallpids failed".to_string())?;
    // Room for processes started meanwhile.
    let mut pids: Vec<libc::pid_t> = vec![0; room.saturating_add(64)];
    let bytes = i32::try_from(pids.len().saturating_mul(size)).map_err(|e| e.to_string())?;
    // SAFETY: a writable buffer of `bytes` bytes.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let got = usize::try_from(got).map_err(|_| "proc_listallpids failed".to_string())?;
    let mut out = Vec::new();
    for &pid in pids.iter().take(got) {
        // SAFETY: an all-zero proc_bsdinfo is a valid value to fill.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let want = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).map_err(|e| e.to_string())?;
        // SAFETY: a writable proc_bsdinfo of `want` bytes, as PROC_PIDTBSDINFO fills.
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), want) };
        if n != want {
            continue;
        }
        let text = |chars: &[libc::c_char]| -> String {
            let bytes: Vec<u8> = chars.iter().map(|&c| c as u8).take_while(|&c| c != 0).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        // Its longer name where it has one, else its command's.
        let name = match text(&info.pbi_name) {
            n if n.is_empty() => text(&info.pbi_comm),
            n => n,
        };
        out.push(Proc {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            name,
        });
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
pub fn usage(pid: u32) -> Option<Usage> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: an all-zero rusage_info_v4 is a valid value to fill.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: a writable rusage_info_v4, as flavor RUSAGE_INFO_V4 fills.
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    if rc != 0 {
        return None;
    }
    // Its times are Mach absolute time on Apple silicon: converted by the timebase
    // (mach_timebase_info(3)), bound here as Shards' VMM binds it (platform/unix.rs
    // `mach_ns`): libc's binding is deprecated in favour of a crate, the call itself is not.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    let mut base = Timebase { numer: 0, denom: 0 };
    // SAFETY: mach_timebase_info(3) fills `base`.
    if unsafe { mach_timebase_info(&raw mut base) } != 0 || base.denom == 0 {
        return None;
    }
    let ticks = u128::from(info.ri_user_time.saturating_add(info.ri_system_time));
    let ns = ticks.saturating_mul(u128::from(base.numer)) / u128::from(base.denom);
    Some(Usage {
        cpu_ns: u64::try_from(ns).unwrap_or(u64::MAX),
        mem: info.ri_phys_footprint,
        alt: info.ri_resident_size,
    })
}

#[cfg(windows)]
pub fn table() -> Result<Vec<Proc>, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot of every process, no module list asked.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(format!("CreateToolhelp32Snapshot: {}", std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: the snapshot just made, and an entry whose size field is set.
    let mut more = unsafe { Process32FirstW(snap, &raw mut entry) } != 0;
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(entry.szExeFile.get(..len).unwrap_or_default());
        out.push(Proc {
            pid: entry.th32ProcessID,
            ppid: entry.th32ParentProcessID,
            name,
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &raw mut entry) } != 0;
    }
    // SAFETY: the snapshot's own handle, closed once.
    unsafe { CloseHandle(snap) };
    Ok(out)
}

#[cfg(windows)]
pub fn usage(pid: u32) -> Option<Usage> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    // SAFETY: OpenProcess with query rights; the handle is closed below.
    let mut handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        // SAFETY: as above, without the right to read its memory's counters.
        handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    }
    if handle.is_null() {
        return None;
    }
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: an open process handle and four writable FILETIMEs.
    let ok = unsafe { GetProcessTimes(handle, created, exited, kernel, user) } != 0;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: an open process handle and a writable PROCESS_MEMORY_COUNTERS_EX whose cb is
    // its size, as K32GetProcessMemoryInfo takes the EX form.
    let mem = unsafe { K32GetProcessMemoryInfo(handle, (&raw mut counters).cast(), counters.cb) } != 0;
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(handle) };
    if !ok {
        return None;
    }
    Some(Usage {
        cpu_ns: hundreds(*kernel).saturating_add(hundreds(*user)).saturating_mul(100),
        mem: if mem { counters.WorkingSetSize as u64 } else { 0 },
        alt: if mem { counters.PrivateUsage as u64 } else { 0 },
    })
}
