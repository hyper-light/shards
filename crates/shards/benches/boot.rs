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
#[cfg(unix)]
mod support;

#[cfg(not(unix))]
fn main() {
    use std::io::Write;
    // Peak RSS comes from wait4(2); a Windows port reads the job's peak working set.
    let _ = writeln!(std::io::stderr(), "SKIP: the boot benchmark needs a Unix host");
}

#[cfg(unix)]
fn main() {
    use shards_abi::marker;
    use support::{peak_rss_mib, report, run, stats, us, wall_us};

    const WARMUP: usize = 3;
    let runs: usize = support::option("--runs").map_or(50, |v| v.parse().expect("--runs N"));
    let cpus = support::option("--cpus").unwrap_or_else(|| "1".into());
    let memory = support::option("--memory").unwrap_or_else(|| "256".into());
    if common::cannot_run_vms() {
        return;
    }
    let args: Vec<String> = [
        "vm",
        "run",
        "--kernel",
        common::kernel().to_str().unwrap(),
        "--init",
        common::guest_init().to_str().unwrap(),
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
        run(&args, false);
    }
    let samples: Vec<_> = (0..runs).map(|_| run(&args, false)).collect();
    let init = |r: &common::Run| r.marker_us(marker::INIT_STARTED);
    report(
        "boot",
        &[
            ("n", runs.to_string()),
            ("cpus", cpus),
            ("memory_mib", memory),
            ("kernel", common::kernel_artifact().name.to_string()),
        ],
        &[
            stats("vmm_setup", "us", us(&samples, |r| r.entry_us())),
            stats(
                "kernel",
                "us",
                us(&samples, |r| init(r)?.checked_sub(r.entry_us()?)),
            ),
            stats("to_init", "us", us(&samples, init)),
            stats("to_exit", "us", us(&samples, |r| r.exit_us())),
            stats("spawn_to_exit", "us", wall_us(&samples)),
            stats("peak_rss", "MiB", peak_rss_mib(&samples)),
        ],
    );
}
