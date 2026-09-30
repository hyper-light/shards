//! A daemon's fleet of warm VMs, measured (audit A13; docs/research/platform-measurements.md
//! M49): traffic through 1, 10, 100 and 1,000 distinct templates, bursts past a pool's
//! ready count, and the daemon's stop. Real VMs, restored from real templates of distinct
//! test images, each served from a loopback registry.
//!
//! For each fleet size N, in a home of its own with the daemon's default settings:
//!
//! - every image is run once (pulled, booted, its template saved), then once again
//!   (restored), each run's client wall clock recorded, and whether it succeeded;
//! - with the fleet settled, the warm VMs are counted, and their memory, the daemon's,
//!   and the disk the home takes are measured: each process's physical footprint on
//!   macOS (`footprint`: its private dirty and compressed pages, page tables included),
//!   its proportional set size on Linux (`/proc/PID/smaps_rollup`, `Pss`); CPU time of
//!   each warm VM, which is what its restore took, since it has done nothing else;
//! - one image's runs one at a time, then in bursts of `FLEET_BURST` at once, each run's
//!   wall clock;
//! - `shards daemon stop`'s wall clock, and the VMs left after it.
//!
//! Crates/shards/tests/fleet.rs includes it as an ignored test, for the helpers E2E tests
//! use:
//!
//!     FLEET_SIZES=1,10,100,1000 cargo test --release -p shards --test fleet -- \
//!         --ignored --nocapture
//!
//! `FLEET_SIZES` (default `1,10,100`), `FLEET_BURST` (8) and `FLEET_ROUNDS` (20) say
//! what it runs.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::common::{
    TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, registry, run_shards_env, test_image_with,
};

const TIMEOUT: Duration = Duration::from_secs(120);

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// n, p50, p90, p99 and max of `samples`, in milliseconds.
fn summary(samples: &mut [f64]) -> String {
    if samples.is_empty() {
        return "n 0".into();
    }
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    format!(
        "n {} | p50 {:.1} | p90 {:.1} | p99 {:.1} | max {:.1}",
        samples.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        samples[samples.len() - 1]
    )
}

/// The pids of processes whose command line holds `needle`.
fn processes_with(needle: &str) -> Vec<u32> {
    let out = Command::new("ps").args(["-axo", "pid=,args="]).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(needle))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

/// A process's own memory, in bytes: its physical footprint on macOS, its PSS on Linux.
fn memory(pid: u32) -> u64 {
    if cfg!(target_os = "linux") {
        let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
        return text
            .lines()
            .find_map(|l| l.strip_prefix("Pss:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024);
    }
    let out = Command::new("footprint")
        .args(["-f", "bytes", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.split_once("Footprint: ").map(|(_, v)| v.to_string()))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// A process's CPU time, in milliseconds, to the nanosecond: `/proc/PID/schedstat`'s time
/// on the CPU on Linux (sched-stats.rst), `proc_pid_rusage`'s user and system times in
/// Mach time units on macOS, scaled by the timebase.
// libc's Mach timebase is deprecated for the mach2 crate, a dependency this alone would add.
#[allow(deprecated)]
fn cpu_ms(pid: u32) -> f64 {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string(format!("/proc/{pid}/schedstat")).unwrap_or_default();
        text.split_whitespace()
            .next()
            .and_then(|ns| ns.parse::<u64>().ok())
            .map_or(0.0, |ns| ns as f64 / 1e6)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain data, for which all zeroes is a value.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(2) filling a v2 record we own.
        let r = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                (&raw mut info).cast::<libc::rusage_info_t>(),
            )
        };
        if r != 0 {
            return 0.0;
        }
        let mut base = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info filling a record we own.
        unsafe { libc::mach_timebase_info(&mut base) };
        let ticks = (info.ri_user_time + info.ri_system_time) as f64;
        ticks * f64::from(base.numer) / f64::from(base.denom.max(1)) / 1e6
    }
}

fn disk_kib(path: &Path) -> u64 {
    let out = Command::new("du").args(["-sk"]).arg(path).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

#[test]
#[ignore = "a measurement: FLEET_SIZES=… cargo test --test fleet -- --ignored --nocapture"]
fn fleet() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let revision = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .unwrap();
    let host = Command::new("uname").args(["-srm"]).output().unwrap();
    println!(
        "revision {}, host {}",
        String::from_utf8_lossy(&revision.stdout).trim(),
        String::from_utf8_lossy(&host.stdout).trim()
    );
    let burst: usize = setting("FLEET_BURST", "8").parse().unwrap();
    let rounds: usize = setting("FLEET_ROUNDS", "20").parse().unwrap();
    for n in setting("FLEET_SIZES", "1,10,100")
        .split(',')
        .map(|s| s.trim().parse::<usize>().unwrap())
    {
        let images: Vec<String> = (0..n)
            .map(|i| {
                let (manifest, blobs) = test_image_with(Some(format!("fleet-{i}").as_bytes()));
                let (port, _) = registry(manifest, blobs);
                format!("127.0.0.1:{port}/test/image:v1")
            })
            .collect();
        let home = TempDir::new(&format!("fleet-{n}"));
        let env = [("SHARDS_HOME", home.as_os_str())];
        let args = [
            "use".as_ref(),
            "--kernel".as_ref(),
            kernel().as_os_str(),
            "--init".as_ref(),
            guest_init().as_os_str(),
        ];
        let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
        assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
        let run = |image: &str| run_shards_env(&["run"], &[image, "exit", "0"], &env, TIMEOUT);
        println!("\n== {n} templates");
        for pass in ["first (pull, boot, save)", "second (restore)"] {
            let t0 = Instant::now();
            let (mut wall, mut failed) = (Vec::new(), 0);
            for image in &images {
                let r = run(image);
                if r.status == Some(0) {
                    wall.push(r.elapsed.as_secs_f64() * 1000.0);
                } else {
                    failed += 1;
                    println!("failed: {}", r.stderr.trim());
                }
            }
            println!(
                "{pass}: {} ms | failed {failed} | {:.1} s",
                summary(&mut wall),
                t0.elapsed().as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_secs(2));
        let templates = home.join("templates");
        let warm = processes_with(&templates.to_string_lossy());
        let warm_memory: u64 = warm.iter().map(|&p| memory(p)).sum();
        let mut refill_cpu: Vec<f64> = warm.iter().map(|&p| cpu_ms(p)).collect();
        let daemon: u32 = std::fs::read_to_string(home.join("daemon.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        println!(
            "warm VMs {} | their memory {:.1} MiB | daemon {:.1} MiB, cpu {:.0} ms",
            warm.len(),
            mib(warm_memory),
            mib(memory(daemon)),
            cpu_ms(daemon)
        );
        println!("warm VM cpu (its restore): {} ms", summary(&mut refill_cpu));
        println!(
            "disk: templates {:.1} MiB, images {:.1} MiB",
            disk_kib(&templates) as f64 / 1024.0,
            disk_kib(&home.join("images")) as f64 / 1024.0
        );

        let image = images[0].as_str();
        let mut one = Vec::new();
        for _ in 0..rounds {
            let r = run(image);
            assert_eq!(r.status, Some(0), "{}", r.stderr);
            one.push(r.elapsed.as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_millis(200));
        }
        println!("one at a time, 200 ms apart: {} ms", summary(&mut one));
        let (mut bursts, mut failed) = (Vec::new(), 0);
        for _ in 0..rounds {
            let handles: Vec<_> = (0..burst)
                .map(|_| {
                    let (image, home) = (image.to_string(), home.to_path_buf());
                    std::thread::spawn(move || {
                        let env: [(&str, &OsStr); 1] = [("SHARDS_HOME", home.as_os_str())];
                        run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env, TIMEOUT)
                    })
                })
                .collect();
            for h in handles {
                let r = h.join().unwrap();
                if r.status == Some(0) {
                    bursts.push(r.elapsed.as_secs_f64() * 1000.0);
                } else {
                    failed += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        println!("bursts of {burst}: {} ms | failed {failed}", summary(&mut bursts));
        let warm_after = processes_with(&templates.to_string_lossy()).len();
        let t0 = Instant::now();
        let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
        assert_eq!(stopped.status, Some(0));
        let stop_ms = t0.elapsed().as_secs_f64() * 1000.0;
        std::thread::sleep(Duration::from_millis(500));
        println!(
            "warm VMs after the bursts {warm_after} | daemon stop {stop_ms:.0} ms | VMs left {}",
            processes_with(&templates.to_string_lossy()).len()
        );
    }
}
