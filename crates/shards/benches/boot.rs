//! Boot benchmark: sequential boots of a real guest, from process spawn through PID 1
//! to power-off. Reports n, p50, p90, p99 and max for each phase and the VMM's peak RSS,
//! with host, OS and revision (CLAUDE.md).
//!
//! `cargo bench -p shards --bench boot [-- --runs N --cpus N --memory MIB]`
//!
//! Each boot is a fresh `shards vm run` process with a warm host page cache (three
//! discarded warm-up boots first). Phases, from the VMM's own clock (µs since `main`),
//! except spawn→exit:
//!
//! - `vmm_setup`: `main` → the boot vCPU enters the guest
//! - `kernel`: guest entry → PID 1 starts (the init marker)
//! - `to_init`: `main` → PID 1 starts
//! - `to_exit`: `main` → the guest powered off
//! - `spawn_to_exit`: the host's wall clock around the whole process, including exec and
//!   teardown

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

#[cfg(unix)]
#[path = "../tests/common/mod.rs"]
mod common;

#[cfg(not(unix))]
fn main() {
    use std::io::Write;
    // Peak RSS comes from wait4(2); a Windows port reads the job's peak working set.
    let _ = writeln!(std::io::stderr(), "SKIP: the boot benchmark needs a Unix host");
}

#[cfg(unix)]
fn main() {
    imp::main();
}

#[cfg(unix)]
mod imp {
    use super::common;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use shards_abi::marker;

    const WARMUP: usize = 3;
    const TIMEOUT: Duration = Duration::from_secs(60);

    struct Sample {
        run: common::Run,
        max_rss_bytes: u64,
    }

    /// Runs one boot, reaping the child with wait4(2) for its own resource usage.
    #[allow(clippy::zombie_processes)] // reaped by wait4, not Child::wait
    fn boot(args: &[String]) -> Sample {
        let start = Instant::now();
        let mut child = Command::new(common::shards())
            .args(["vm", "run"])
            .args(args)
            .env("SHARDS_TIMING", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning shards");
        let mut stderr = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let pid = child.id() as libc::pid_t;
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
        let exited = libc::WIFEXITED(status);
        let run = common::Run {
            status: exited.then(|| libc::WEXITSTATUS(status)),
            stdout: String::new(),
            stderr: reader.join().unwrap(),
            elapsed,
        };
        assert_eq!(run.status, Some(0), "boot failed: {run}");
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

    fn stats(name: &str, unit: &str, mut v: Vec<f64>) -> (String, String) {
        v.sort_by(f64::total_cmp);
        let (p50, p90, p99, max) = (
            percentile(&v, 50.0),
            percentile(&v, 90.0),
            percentile(&v, 99.0),
            v[v.len() - 1],
        );
        (
            format!("{name:<14} {p50:>10.1} {p90:>10.1} {p99:>10.1} {max:>10.1}  {unit}"),
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

    pub fn main() {
        let mut runs = 50usize;
        let mut cpus = "1".to_string();
        let mut memory = "256".to_string();
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--runs" => runs = args.next().and_then(|v| v.parse().ok()).expect("--runs N"),
                "--cpus" => cpus = args.next().expect("--cpus N"),
                "--memory" => memory = args.next().expect("--memory MIB"),
                _ => {} // cargo passes --bench
            }
        }
        if common::cannot_run_vms() {
            return;
        }
        let kernel = common::kernel().to_str().unwrap().to_string();
        let init = common::guest_init().to_str().unwrap().to_string();
        let vm_args: Vec<String> = [
            "--kernel",
            &kernel,
            "--init",
            &init,
            "--cpus",
            &cpus,
            "--memory",
            &memory,
            "--cmdline",
            "quiet panic=-1",
            "--no-console",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        for _ in 0..WARMUP {
            boot(&vm_args);
        }
        let samples: Vec<Sample> = (0..runs).map(|_| boot(&vm_args)).collect();

        let us = |f: &dyn Fn(&Sample) -> Option<u128>| -> Vec<f64> {
            samples
                .iter()
                .map(|s| f(s).expect("timing field") as f64)
                .collect()
        };
        let init_us = |s: &Sample| s.run.marker_us(marker::INIT_STARTED);
        let rows = [
            stats("vmm_setup", "us", us(&|s| s.run.entry_us())),
            stats(
                "kernel",
                "us",
                us(&|s| init_us(s)?.checked_sub(s.run.entry_us()?)),
            ),
            stats("to_init", "us", us(&|s| init_us(s))),
            stats("to_exit", "us", us(&|s| s.run.exit_us())),
            stats(
                "spawn_to_exit",
                "us",
                samples
                    .iter()
                    .map(|s| s.run.elapsed.as_secs_f64() * 1e6)
                    .collect(),
            ),
            stats(
                "peak_rss",
                "MiB",
                samples
                    .iter()
                    .map(|s| s.max_rss_bytes as f64 / (1024.0 * 1024.0))
                    .collect(),
            ),
        ];
        let (host, os, rev) = (host(), os(), revision());
        let kernel_name = common::kernel_artifact().name;
        println!(
            "boot: n={runs} cpus={cpus} memory={memory}MiB kernel={kernel_name}\nhost: {host}\nos: {os}\nrevision: {rev}\n"
        );
        println!(
            "{:<14} {:>10} {:>10} {:>10} {:>10}",
            "phase", "p50", "p90", "p99", "max"
        );
        for (row, _) in &rows {
            println!("{row}");
        }
        let json: Vec<&str> = rows.iter().map(|(_, j)| j.as_str()).collect();
        println!(
            "\nshards-bench {{\"bench\":\"boot\",\"n\":{runs},\"cpus\":{cpus},\"memory_mib\":{memory},\"kernel\":\"{kernel_name}\",\"host\":\"{host}\",\"os\":\"{os}\",\"revision\":\"{rev}\",{}}}",
            json.join(",")
        );
    }
}
