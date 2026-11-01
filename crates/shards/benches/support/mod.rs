//! What the benchmarks share: running `shards` or `shardsd` with per-process resource usage, order
//! statistics, and the host/OS/revision stamp every result carries (CLAUDE.md).
#![allow(dead_code)]

#[cfg(any(unix, windows))]
use std::io::{BufRead, BufReader, Write};
use std::process::Command;
#[cfg(any(unix, windows))]
use std::process::Stdio;
#[cfg(any(unix, windows))]
use std::sync::mpsc;
#[cfg(any(unix, windows))]
use std::time::{Duration, Instant};

use crate::common;

#[cfg(any(unix, windows))]
const TIMEOUT: Duration = Duration::from_secs(60);

pub struct Sample {
    pub run: common::Run,
    /// When it began and ended, for [`frozen`].
    pub began: Instant,
    pub ended: Instant,
}

/// The least gap between two ticks of a 1 ms sleeper that says its process was frozen: on
/// jupiter, kiln's headroom guard froze every process of a run for 1.5 to 29 s at a time
/// while a sleeper running woke within 51 ms (PM M418), so a sample taken across such a gap
/// times the freeze, not what it measures.
const FROZEN: Duration = Duration::from_millis(250);

/// The spans this process's ticker saw it frozen ([`FROZEN`]), and its last tick, in
/// microseconds since [`base`].
static FREEZES: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());
static LAST_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn base() -> Instant {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn since_base(t: Instant) -> u64 {
    u64::try_from(t.saturating_duration_since(base()).as_micros()).unwrap_or(u64::MAX)
}

/// Starts this process's ticker once: a thread that sleeps a millisecond at a time and
/// records each gap past [`FROZEN`] between its ticks.
fn tick() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = base();
        std::thread::Builder::new()
            .name("frozen".into())
            .spawn(|| {
                let mut last = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(1));
                    let now = Instant::now();
                    if now.duration_since(last) > FROZEN {
                        FREEZES
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((since_base(last), since_base(now)));
                    }
                    LAST_TICK.store(since_base(now), std::sync::atomic::Ordering::Release);
                    last = now;
                }
            })
            .expect("the ticker");
    });
}

/// Whether this process was frozen while `sample` was taken, as its ticker saw it once it
/// had ticked past the sample's end: a sample to take again, its time the freeze's.
pub fn frozen(sample: &Sample) -> bool {
    let (began, ended) = (since_base(sample.began), since_base(sample.ended));
    while LAST_TICK.load(std::sync::atomic::Ordering::Acquire) <= ended {
        std::thread::sleep(Duration::from_millis(1));
    }
    FREEZES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|&(from, to)| from < ended && to > began)
}

/// What `take` takes, taken again until no freeze of this process spans it ([`frozen`]),
/// each taken again counted in `retaken`.
pub fn unfrozen(retaken: &mut usize, mut take: impl FnMut() -> Sample) -> Sample {
    loop {
        let sample = take();
        if !frozen(&sample) {
            return sample;
        }
        *retaken += 1;
    }
}

/// How long this process could run since `start`: the wall's time less each freeze its
/// ticker saw ([`FROZEN`]), and the one it has not yet ticked past. A wait timed so outlasts
/// a guard that freezes this process and its children together: under kiln's headroom guard
/// on jupiter a sample's process had 0.2 to 0.6 s to run between freezes of 25 s (PM M418),
/// and a minute of the wall's time was no minute of theirs.
pub fn runnable_since(start: Instant) -> Duration {
    let (from, now) = (since_base(start), since_base(Instant::now()));
    let seen: u64 = FREEZES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|&(a, b)| b.min(now).saturating_sub(a.max(from)))
        .sum();
    let last = LAST_TICK.load(std::sync::atomic::Ordering::Acquire);
    let frozen_us = u64::try_from(FROZEN.as_micros()).unwrap_or(u64::MAX);
    let unseen = if now.saturating_sub(last) > frozen_us {
        now.saturating_sub(last.max(from))
    } else {
        0
    };
    Duration::from_micros(
        now.saturating_sub(from)
            .saturating_sub(seen.saturating_add(unseen)),
    )
}

/// What `rx` gets before this process has run `limit` since `start` ([`runnable_since`]).
pub fn recv_within<T>(
    rx: &mpsc::Receiver<T>,
    start: Instant,
    limit: Duration,
) -> Result<T, mpsc::RecvTimeoutError> {
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) if runnable_since(start) < limit => {}
            got => return got,
        }
    }
}

/// Runs `shards-vm <args>`, the VMM's own process without the `shards` command in front,
/// to completion. With `hold`, waits for `shards-ready` on stderr, then sends the start
/// line.
#[cfg(target_os = "linux")]
pub fn run(args: &[String], hold: bool) -> Sample {
    measure(common::shards_vm(), args, hold, &[])
}

/// [`run`] where a VM process opens only what its spawner grants it: on macOS, in App
/// Sandbox (D30), and on Windows, in the AppContainer `shards` starts it in (D141): `shards
/// <args>`, which grants it its files, its VM process's own timing line passed through. A
/// VM process started alone there refuses to start ("nothing was granted to this VM").
#[cfg(not(target_os = "linux"))]
pub fn run(args: &[String], hold: bool) -> Sample {
    measure(common::shards(), args, hold, &[])
}

/// Runs the `shards` command as a user runs it, with `env` added to its environment.
#[cfg(any(unix, windows))]
pub fn run_env(args: &[String], hold: bool, env: &[(&str, &std::ffi::OsStr)]) -> Sample {
    measure(common::shards(), args, hold, env)
}

#[cfg(unix)]
#[allow(clippy::zombie_processes)] // reaped by wait4, not Child::wait
fn measure(bin: &std::path::Path, args: &[String], hold: bool, env: &[(&str, &std::ffi::OsStr)]) -> Sample {
    tick();
    let start = Instant::now();
    let mut child = Command::new(bin)
        .args(args)
        .envs(env.iter().copied())
        .env("SHARDS_TIMING", "1")
        .stdin(if hold { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards");
    let stderr = child.stderr.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line == "shards-ready" {
                let _ = ready_tx.send(());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let pid = child.id() as libc::pid_t;
    if hold {
        recv_within(&ready_rx, start, TIMEOUT).expect("shards never reported shards-ready");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"go\n").unwrap();
    }
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if recv_within(&done_rx, start, TIMEOUT).is_err() {
            // SAFETY: the child is not yet reaped, so `pid` still names it.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let mut status = 0;
    // SAFETY: waits for our own child; `status` is a valid out-parameter.
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    let elapsed = start.elapsed();
    let _ = done_tx.send(());
    watchdog.join().unwrap();
    assert_eq!(reaped, pid, "waitpid: {}", std::io::Error::last_os_error());
    let run = common::Run {
        status: libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
        signal: libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
        stdout: String::new(),
        stderr: reader.join().unwrap(),
        elapsed,
    };
    assert_eq!(run.status, Some(0), "shards failed: {run}");
    Sample {
        run,
        began: start,
        ended: Instant::now(),
    }
}

/// [`measure`] on Windows: the child waited for, and ended where it outlives [`TIMEOUT`],
/// by a watchdog holding a handle of its own to the child's process.
#[cfg(windows)]
fn measure(bin: &std::path::Path, args: &[String], hold: bool, env: &[(&str, &std::ffi::OsStr)]) -> Sample {
    use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
    tick();
    let start = Instant::now();
    let mut child = Command::new(bin)
        .args(args)
        .envs(env.iter().copied())
        .env("SHARDS_TIMING", "1")
        .stdin(if hold { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning shards");
    let stderr = child.stderr.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line == "shards-ready" {
                let _ = ready_tx.send(());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    if hold {
        recv_within(&ready_rx, start, TIMEOUT).expect("shards never reported shards-ready");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"go\n").unwrap();
    }
    let process = child.as_handle().try_clone_to_owned().unwrap();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if recv_within(&done_rx, start, TIMEOUT).is_err() {
            // SAFETY: a handle of the child's process, which this thread holds open.
            unsafe { windows_sys::Win32::System::Threading::TerminateProcess(process.as_raw_handle(), 1) };
        }
    });
    let status = child.wait().unwrap();
    let elapsed = start.elapsed();
    let _ = done_tx.send(());
    watchdog.join().unwrap();
    let run = common::Run {
        status: status.code(),
        signal: None,
        stdout: String::new(),
        stderr: reader.join().unwrap(),
        elapsed,
    };
    assert_eq!(run.status, Some(0), "shards failed: {run}");
    Sample {
        run,
        began: start,
        ended: Instant::now(),
    }
}

/// Nearest-rank percentile of sorted `v`.
fn percentile(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

/// Every row's samples, in the order they were taken, for [`report`] to write when
/// `SHARDS_BENCH_SAMPLES` names a directory: runs interleaved with another VMM's pair up by
/// their index.
static SAMPLES: std::sync::Mutex<Vec<(String, String, Vec<f64>)>> = std::sync::Mutex::new(Vec::new());

/// One result row: `(table line, JSON member)`.
pub fn stats(name: &str, unit: &str, v: Vec<f64>) -> (String, String) {
    SAMPLES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((name.to_string(), unit.to_string(), v.clone()));
    row(name, unit, v)
}

/// The table line and JSON member of `v`: its p50, p90, p99 and max.
fn row(name: &str, unit: &str, mut v: Vec<f64>) -> (String, String) {
    v.sort_by(f64::total_cmp);
    let (p50, p90, p99, max) = (
        percentile(&v, 50.0),
        percentile(&v, 90.0),
        percentile(&v, 99.0),
        v[v.len() - 1],
    );
    (
        format!("{name:<16} {p50:>10.1} {p90:>10.1} {p99:>10.1} {max:>10.1}  {unit}"),
        format!(
            "\"{name}\":{{\"unit\":\"{unit}\",\"p50\":{p50:.1},\"p90\":{p90:.1},\"p99\":{p99:.1},\"max\":{max:.1}}}"
        ),
    )
}

/// A result row the envelope does not check (scripts/bench-envelope), its samples kept
/// from it: what explains a checked row, such as the parts of a time. NaN samples are
/// left out, and a row of none says so.
pub fn diagnostic(name: &str, unit: &str, mut v: Vec<f64>) -> (String, String) {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return (format!("{name:<16} none  {unit}"), format!("\"{name}\":null"));
    }
    row(name, unit, v)
}

fn command_output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn host() -> String {
    if cfg!(target_os = "macos") {
        format!(
            "{} ({})",
            command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
            command_output("sysctl", &["-n", "hw.model"])
        )
    } else if cfg!(windows) {
        // `ProcessorNameString    REG_SZ    NAME`: the brand string, as CPUID gives it.
        let out = command_output(
            "reg",
            &[
                "query",
                r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "/v",
                "ProcessorNameString",
            ],
        );
        out.split_once("REG_SZ")
            .map_or(out.clone(), |(_, name)| name.trim().to_string())
    } else {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|c| {
                c.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split(':').nth(1))
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "unknown".into())
    }
}

fn os() -> String {
    if cfg!(target_os = "macos") {
        format!(
            "macOS {} ({})",
            command_output("sw_vers", &["-productVersion"]),
            command_output("sw_vers", &["-buildVersion"])
        )
    } else if cfg!(windows) {
        command_output("cmd", &["/c", "ver"])
    } else {
        format!(
            "{} {}",
            command_output("uname", &["-s"]),
            command_output("uname", &["-r"])
        )
    }
}

/// The host's 1, 5 and 15 minute load averages: how busy it was with other work. Windows
/// keeps none.
fn load() -> String {
    if cfg!(windows) {
        "none kept".into()
    } else if cfg!(target_os = "macos") {
        command_output("sysctl", &["-n", "vm.loadavg"])
            .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
            .to_string()
    } else {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "unknown".into())
    }
}

fn revision() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let rev = command_output("git", &["-C", dir, "rev-parse", "--short", "HEAD"]);
    let status = Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--untracked-files=no"])
        .output();
    match status {
        Ok(o) if !o.stdout.is_empty() => format!("{rev}-dirty"),
        _ => rev,
    }
}

/// Prints the results: a readable table, then one `shards-bench` JSON line. The load is
/// the host's as the run ends.
pub fn report(bench: &str, params: &[(&str, String)], rows: &[(String, String)]) {
    let (host, os, rev, load) = (host(), os(), revision(), load());
    let described: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!(
        "{bench}: {}\nhost: {host}\nos: {os}\nrevision: {rev}\nload (1, 5, 15 min): {load}\n",
        described.join(" ")
    );
    println!(
        "{:<16} {:>10} {:>10} {:>10} {:>10}",
        "phase", "p50", "p90", "p99", "max"
    );
    for (row, _) in rows {
        println!("{row}");
    }
    let json_params: Vec<String> = params.iter().map(|(k, v)| format!("\"{k}\":\"{v}\"")).collect();
    let json_rows: Vec<&str> = rows.iter().map(|(_, j)| j.as_str()).collect();
    println!(
        "\nshards-bench {{\"bench\":\"{bench}\",{},\"host\":\"{host}\",\"os\":\"{os}\",\"revision\":\"{rev}\",\"load\":\"{load}\",{}}}",
        json_params.join(","),
        json_rows.join(",")
    );
    let samples = std::mem::take(&mut *SAMPLES.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    if let Some(dir) = std::env::var_os("SHARDS_BENCH_SAMPLES") {
        let rows: Vec<String> = samples
            .iter()
            .map(|(name, unit, values)| {
                let values: Vec<String> = values.iter().map(|v| format!("{v:.1}")).collect();
                format!(
                    "\"{name}\":{{\"unit\":\"{unit}\",\"values\":[{}]}}",
                    values.join(",")
                )
            })
            .collect();
        let json = format!(
            "{{\"bench\":\"{bench}\",{},\"host\":\"{host}\",\"os\":\"{os}\",\"revision\":\"{rev}\",\"load\":\"{load}\",\"samples\":{{{}}}}}\n",
            json_params.join(","),
            rows.join(",")
        );
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        // One file per report: a bench may report more than once (boots, then restores).
        let mut n = 0;
        let path = loop {
            let path = dir.join(format!("{bench}-{n}.json"));
            if !path.exists() {
                break path;
            }
            n += 1;
        };
        std::fs::write(&path, json).unwrap();
        println!("samples: {}", path.display());
    }
}

/// Microsecond samples of one field of every run.
pub fn us(samples: &[Sample], f: impl Fn(&common::Run) -> Option<u128>) -> Vec<f64> {
    samples
        .iter()
        .map(|s| f(&s.run).expect("timing field") as f64)
        .collect()
}

/// Each run's own peak resident set, in MiB, as its process reports it on its timing line
/// (`rss_kib`: `VmHWM` on Linux, `ru_maxrss` on macOS). Not `wait4`'s `ru_maxrss`, which
/// on Linux starts from this harness's own peak: exec keeps the peak of the address space
/// it replaces, the spawner's under `posix_spawn` (fs/exec.c `exec_mmap`).
pub fn peak_rss_mib(samples: &[Sample]) -> Vec<f64> {
    us(samples, |r| r.rss_kib())
        .into_iter()
        .map(|kib| kib / 1024.0)
        .collect()
}

pub fn wall_us(samples: &[Sample]) -> Vec<f64> {
    samples
        .iter()
        .map(|s| s.run.elapsed.as_secs_f64() * 1e6)
        .collect()
}

/// `--runs N` and other `--name value` options; cargo's `--bench` is ignored.
pub fn option(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}
