//! The guest's processes as `top` shows them: what the kernel says of each in `/proc`,
//! as it says it, for the host to lay out as procps's `ps` would (shards
//! `daemon/top.rs`). The guest only reads; every column is the host's to make.
//!
//! The dump is records separated by RS (0x1e), fields by US (0x1f). The first record is
//! the system's: `shards-processes/1`, then `/proc/stat`'s `btime`, the clock tick,
//! the page size, `/proc/meminfo`'s `MemTotal` in KiB, `/proc/uptime`'s first field,
//! the time now in seconds since the epoch, and the container's `/etc/passwd` and
//! `/etc/group` (names are the container's own). Each process is then its
//! `/proc/PID/stat`, `status`, `cmdline` (its NULs kept) and `wchan`. RS and US within
//! any of them become `?`, as ps shows any control character.
//!
//! Left out: this process, kernel threads, and processes of this program not yet
//! anything else (shards-init's standby forks), which are not the container's, but its
//! reaper under `--init`, docker-init's part, which `docker top` lists (D115).

use std::io;

/// Separators.
const RS: u8 = 0x1e;
const US: u8 = 0x1f;
/// `PF_KTHREAD` in `/proc/PID/stat`'s flags (include/linux/sched.h).
const PF_KTHREAD: u64 = 0x0020_0000;

/// The dump, with the run's `reaper`, where it has one.
pub fn dump(reaper: Option<libc::pid_t>) -> Vec<u8> {
    let mut out = Vec::new();
    let read = |path: &str| std::fs::read(path).unwrap_or_default();
    let stat = String::from_utf8_lossy(&read("/proc/stat")).into_owned();
    let btime = stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .unwrap_or("0")
        .trim()
        .to_string();
    let meminfo = String::from_utf8_lossy(&read("/proc/meminfo")).into_owned();
    let total = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next())
        .unwrap_or("0")
        .to_string();
    let uptime = String::from_utf8_lossy(&read("/proc/uptime"))
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .to_string();
    // SAFETY: sysconf(3) reads constants.
    let (hz, page) = unsafe {
        (
            libc::sysconf(libc::_SC_CLK_TCK),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
    let (hz, page, now) = (hz.to_string(), page.to_string(), now.tv_sec.to_string());
    let fields: [&[u8]; 7] = [
        b"shards-processes/1",
        btime.as_bytes(),
        hz.as_bytes(),
        page.as_bytes(),
        total.as_bytes(),
        uptime.as_bytes(),
        now.as_bytes(),
    ];
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(US);
        }
        put(&mut out, f);
    }
    for f in [read("/etc/passwd"), read("/etc/group")] {
        out.push(US);
        put(&mut out, &f);
    }
    let me = std::process::id();
    let mine = std::fs::read_link("/proc/self/exe").ok();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    let mut pids: Vec<u32> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&pid| pid != me)
        .collect();
    pids.sort_unstable();
    for pid in pids {
        let base = format!("/proc/{pid}");
        // A process may end as it is read: what is gone is left out.
        let Ok(stat) = std::fs::read(format!("{base}/stat")) else {
            continue;
        };
        if flags(&stat).is_none_or(|f| f & PF_KTHREAD != 0) {
            continue;
        }
        let reaped = reaper.is_some_and(|r| u32::try_from(r) == Ok(pid));
        if !reaped && mine.is_some() && std::fs::read_link(format!("{base}/exe")).ok() == mine {
            continue;
        }
        let Ok(status) = std::fs::read(format!("{base}/status")) else {
            continue;
        };
        out.push(RS);
        put(&mut out, &stat);
        out.push(US);
        put(&mut out, &status);
        out.push(US);
        put(
            &mut out,
            &std::fs::read(format!("{base}/cmdline")).unwrap_or_default(),
        );
        out.push(US);
        put(
            &mut out,
            &std::fs::read(format!("{base}/wchan")).unwrap_or_default(),
        );
    }
    out
}

/// `bytes`, with RS and US as `?`.
fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend(bytes.iter().map(|&b| if b == RS || b == US { b'?' } else { b }));
}

/// The flags field of a `/proc/PID/stat` line: its ninth, the seventh after the
/// command, which is in parentheses and may hold anything.
fn flags(stat: &[u8]) -> Option<u64> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    rest.split_whitespace().nth(6)?.parse().ok()
}

/// `shards-init processes`, run as any process but PID 1: the dump on stdout, for the
/// tests that hold the host's layout to procps's own (scripts/top/generate).
pub fn print() -> io::Result<()> {
    use std::io::Write as _;
    io::stdout().write_all(&dump(None))
}
