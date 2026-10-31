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
//! Peak RSS includes the guest memory the process touched. On Windows each sample is
//! `shards restore`, whose VM process runs only confined, in the AppContainer `shards`
//! starts it in (D141): `restore` and `request` are its VM process's own clock's, and
//! `spawn_to_exit` holds the confinement's start too.
//!
//! Where restores record a working set (`vm::RESTORES_RECORD`), the first warm-up restore,
//! through `shards restore`, records the snapshot's, which each restore after it maps
//! ahead. `--working-set` picks the arms: `with` (the default), as restores run; `without`,
//! each sample's working set set aside first, so that its guest takes each page as it
//! first touches it, its rows `_no_ws`; `both`, the two interleaved, each pair's order
//! alternating.
//!
//! `cargo bench -p shards --bench restore [-- --runs N --cpus N --memory MIB --working-set with|without|both]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

#[path = "../tests/common/mod.rs"]
mod common;
mod support;

use std::path::Path;

fn main() {
    use shards_abi::marker;
    use support::{peak_rss_mib, report, run, stats, us, wall_us};

    const WARMUP: usize = 3;
    let runs: usize = support::option("--runs").map_or(50, |v| v.parse().expect("--runs N"));
    let cpus = support::option("--cpus").unwrap_or_else(|| "1".into());
    let memory = support::option("--memory").unwrap_or_else(|| "256".into());
    let arms = support::option("--working-set").unwrap_or_else(|| "with".into());
    let (with, without) = match arms.as_str() {
        "with" => (true, false),
        "without" => (false, true),
        "both" => (true, true),
        other => panic!("--working-set {other}: with, without or both"),
    };
    if common::cannot_run_vms() {
        return;
    }
    let dir = common::workspace().join(format!("target/bench/restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let snapshot = dir.join("snapshot").display().to_string();
    let boot: Vec<String> = [
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
    let cold_args: Vec<String> = ["restore", &snapshot, "--no-console"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let warm_args: Vec<String> = cold_args.iter().cloned().chain(["--hold".to_string()]).collect();
    // The command a user runs, which records the working set where restores record one,
    // kept once the VM has handed it over as it ends: on Linux by a process of its own.
    support::run_env(&cold_args, false, &[]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while shards_vmm::vm::RESTORES_RECORD && !shards_vmm::snapshot::has_working_set(Path::new(&snapshot)) {
        assert!(std::time::Instant::now() < deadline, "no working set kept");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for _ in 0..WARMUP {
        run(&cold_args, false);
        run(&warm_args, true);
    }
    let kept = working_set(Path::new(&snapshot));
    let aside = dir.join("aside");
    let bare = |args: &[String], hold: bool| {
        let moved = set_aside(Path::new(&snapshot), &aside);
        let sample = run(args, hold);
        put_back(Path::new(&snapshot), &aside, &moved);
        sample
    };
    let (mut cold, mut warm) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let (mut cold_bare, mut warm_bare) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for i in 0..runs {
        for bare_first in [i % 2 == 1, i % 2 == 0] {
            if bare_first && without {
                cold_bare.push(bare(&cold_args, false));
                warm_bare.push(bare(&warm_args, true));
            } else if !bare_first && with {
                cold.push(run(&cold_args, false));
                warm.push(run(&warm_args, true));
            }
        }
    }
    assert_eq!(
        working_set(Path::new(&snapshot)),
        kept,
        "the working set put back"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let (pages, written) = kept.unwrap_or((0, 0));
    let rows = |cold: &[support::Sample], warm: &[support::Sample], suffix: &str| {
        [
            stats(&format!("cold_restore{suffix}"), "us", us(cold, resumed)),
            stats(&format!("cold_spawn_exit{suffix}"), "us", wall_us(cold)),
            stats(
                &format!("warm_request{suffix}"),
                "us",
                us(warm, |r| resumed(r)?.checked_sub(r.released_us()?)),
            ),
            stats(&format!("warm_peak_rss{suffix}"), "MiB", peak_rss_mib(warm)),
        ]
    };
    let mut table = Vec::new();
    if with {
        table.extend(rows(&cold, &warm, ""));
    }
    if without {
        table.extend(rows(&cold_bare, &warm_bare, "_no_ws"));
    }
    report(
        "restore",
        &[
            ("n", runs.to_string()),
            ("cpus", cpus),
            ("memory_mib", memory),
            ("kernel", common::kernel_artifact().name.to_string()),
            ("working_set", arms),
            ("working_set_pages", pages.to_string()),
            ("working_set_written", written.to_string()),
        ],
        &table,
    );
}

/// The pages of the working set the snapshot in `dir` keeps for the generation it restores,
/// and of them those its guest wrote; none where it keeps none.
fn working_set(dir: &Path) -> Option<(usize, usize)> {
    let generation = shards_vmm::snapshot::generation(dir).unwrap();
    let touched = shards_vmm::snapshot::read_working_set(&generation, 4096, 1 << 24).unwrap()?;
    Some((touched.len(), touched.iter().filter(|t| t.written).count()))
}

/// The files of a generation's working set (`working-set`, and on Windows its versions).
fn working_set_files(generation: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(generation)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("working-set"))
        .collect()
}

/// Moves the working set of the generation the snapshot in `dir` restores into `aside`,
/// where a restore does not map it ahead: the names moved.
fn set_aside(dir: &Path, aside: &Path) -> Vec<std::ffi::OsString> {
    let generation = shards_vmm::snapshot::generation(dir).unwrap();
    std::fs::create_dir_all(aside).unwrap();
    let moved = working_set_files(&generation);
    for name in &moved {
        std::fs::rename(generation.join(name), aside.join(name)).unwrap();
    }
    assert!(!shards_vmm::snapshot::has_working_set(dir));
    moved
}

/// Puts back what [`set_aside`] moved, once the working set the restore meanwhile recorded
/// in its place is removed. Windows may hold a file a while past its last handle, as its
/// renames found (PM M345): a removal it refuses is asked again for up to 10 s.
fn put_back(dir: &Path, aside: &Path, moved: &[std::ffi::OsString]) {
    let generation = shards_vmm::snapshot::generation(dir).unwrap();
    for name in working_set_files(&generation) {
        let path = generation.join(&name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while let Err(e) = std::fs::remove_file(&path) {
            assert!(std::time::Instant::now() < deadline, "{}: {e}", path.display());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    for name in moved {
        std::fs::rename(aside.join(name), generation.join(name)).unwrap();
    }
}
