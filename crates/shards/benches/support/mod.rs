//! What the benchmarks share: running `shards` with per-process resource usage, order
//! statistics, and the host/OS/revision stamp every result carries (CLAUDE.md).
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::common;

const TIMEOUT: Duration = Duration::from_secs(60);

pub struct Sample {
    pub run: common::Run,
    pub max_rss_bytes: u64,
}

/// Runs `shards <args>` to completion, reaping it with wait4(2) for its own resource
/// usage. With `hold`, waits for `shards-ready` on stderr, then sends the start line.
pub fn run(args: &[String], hold: bool) -> Sample {
    run_env(args, hold, &[])
}

/// [`run`], with `env` added to shards' environment.
#[allow(clippy::zombie_processes)] // reaped by wait4, not Child::wait
pub fn run_env(args: &[String], hold: bool, env: &[(&str, &std::ffi::OsStr)]) -> Sample {
    let start = Instant::now();
    let mut child = Command::new(common::shards())
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
        ready_rx
            .recv_timeout(TIMEOUT)
            .expect("shards never reported shards-ready");
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"go\n").unwrap();
    }
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(TIMEOUT).is_err() {
            // SAFETY: the child is not yet reaped, so `pid` still names it.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let mut status = 0;
    // SAFETY: zeroed rusage is a valid out-parameter.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: waits for our own child; both out-parameters are valid.
    let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
    let elapsed = start.elapsed();
    let _ = done_tx.send(());
    watchdog.join().unwrap();
    assert_eq!(reaped, pid, "wait4: {}", std::io::Error::last_os_error());
    let run = common::Run {
        status: libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
        stdout: String::new(),
        stderr: reader.join().unwrap(),
        elapsed,
    };
    assert_eq!(run.status, Some(0), "shards failed: {run}");
    // ru_maxrss is bytes on macOS and KiB on Linux (getrusage(2) on each).
    let rss_unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    Sample {
        run,
        max_rss_bytes: usage.ru_maxrss as u64 * rss_unit,
    }
}

/// Nearest-rank percentile of sorted `v`.
fn percentile(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

/// One result row: `(table line, JSON member)`.
pub fn stats(name: &str, unit: &str, mut v: Vec<f64>) -> (String, String) {
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
    } else {
        format!(
            "{} {}",
            command_output("uname", &["-s"]),
            command_output("uname", &["-r"])
        )
    }
}

/// The host's 1, 5 and 15 minute load averages: how busy it was with other work.
fn load() -> String {
    if cfg!(target_os = "macos") {
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
}

/// Microsecond samples of one field of every run.
pub fn us(samples: &[Sample], f: impl Fn(&common::Run) -> Option<u128>) -> Vec<f64> {
    samples
        .iter()
        .map(|s| f(&s.run).expect("timing field") as f64)
        .collect()
}

pub fn rss_mib(samples: &[Sample]) -> Vec<f64> {
    samples
        .iter()
        .map(|s| s.max_rss_bytes as f64 / (1024.0 * 1024.0))
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
