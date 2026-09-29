//! Restore benchmark: how fast a snapshot of a real guest runs again. One snapshot is
//! taken (the `resume` test guest, which marks RESUMED as its first act after the
//! snapshot point and powers off), then restored many times:
//!
//! - cold: a fresh `shards vm restore` process per sample
//!   - `restore`: VMM `main` → guest running again (the RESUMED marker)
//!   - `spawn_to_exit`: host wall clock around the whole process
//! - warm: a `--hold` process prepares everything, then gets its start request
//!   - `request`: release → guest running again: the cost of a start request to a warm VM
//!
//! Peak RSS includes the guest memory the process touched.
//!
//! `cargo bench -p shards --bench restore [-- --runs N --cpus N --memory MIB]`

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
    let _ = writeln!(std::io::stderr(), "SKIP: the restore benchmark needs a Unix host");
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
    let dir = common::workspace().join(format!("target/bench/restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let snapshot = dir.join("snapshot").display().to_string();
    let boot: Vec<String> = [
        "vm",
        "run",
        "--kernel",
        common::kernel().to_str().unwrap(),
        "--init",
        common::test_guest().to_str().unwrap(),
        "--cpus",
        &cpus,
        "--memory",
        &memory,
        "--cmdline",
        "quiet panic=-1 shards_test=resume",
        "--no-console",
        "--snapshot-dir",
        &snapshot,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    run(&boot, false);

    let resumed = |r: &common::Run| r.marker_us(marker::RESUMED);
    let cold_args: Vec<String> = ["vm", "restore", &snapshot, "--no-console"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let warm_args: Vec<String> = cold_args.iter().cloned().chain(["--hold".to_string()]).collect();
    for _ in 0..WARMUP {
        run(&cold_args, false);
        run(&warm_args, true);
    }
    let mut cold = Vec::with_capacity(runs);
    let mut warm = Vec::with_capacity(runs);
    for _ in 0..runs {
        cold.push(run(&cold_args, false));
        warm.push(run(&warm_args, true));
    }
    let _ = std::fs::remove_dir_all(&dir);
    report(
        "restore",
        &[
            ("n", runs.to_string()),
            ("cpus", cpus),
            ("memory_mib", memory),
            ("kernel", common::kernel_artifact().name.to_string()),
        ],
        &[
            stats("cold_restore", "us", us(&cold, resumed)),
            stats("cold_spawn_exit", "us", wall_us(&cold)),
            stats(
                "warm_request",
                "us",
                us(&warm, |r| resumed(r)?.checked_sub(r.released_us()?)),
            ),
            stats("warm_peak_rss", "MiB", peak_rss_mib(&warm)),
        ],
    );
}
