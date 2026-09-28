//! Run benchmark: how fast a command runs in an image, from the request to its exit
//! status. The command is `/bin/testguest exit 0`, in the minimal image the E2E tests use
//! (tests/common, `workload_image`).
//!
//! - cold: `shards vm run --rootfs IMAGE -- COMMAND` boots the kernel into the image for
//!   every command.
//!   - `cold_spawn_exit`: host wall clock around the whole process.
//! - warm: a template, saved once the image is mounted (`vm run --rootfs --snapshot-dir`),
//!   is restored with `--hold` for every command.
//!   - `warm_request`: release → the VM stops, which it does once shards has read the
//!     command's exit status (the VMM's clock): a start request to a warm VM, answered.
//!   - `warm_spawn_exit`: host wall clock around the whole process.
//!
//! Phases, from shards-init's markers (the VMM's clock):
//! - `*_resume` (warm): release → the guest runs again; `cold_boot`: first guest entry →
//!   init starts.
//! - `*_connect`: init running → connected to the host (cold: the image mounted first).
//! - `*_spawn`: connected → the command executing: the workload received, its user
//!   resolved, fork and exec.
//! - `*_command`: the command's own run.
//! - `*_report`: the command exited → init powers off: output drained, the status sent
//!   and read.
//! - `*_power_off`: → the VM stops.
//!
//! Peak RSS includes the guest memory the process touched.
//!
//! `cargo bench -p shards --bench run [-- --runs N]`

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
    let _ = writeln!(std::io::stderr(), "SKIP: the run benchmark needs a Unix host");
}

#[cfg(unix)]
fn main() {
    use support::{report, rss_mib, run, stats, us, wall_us};

    const WARMUP: usize = 3;
    let runs: usize = support::option("--runs").map_or(50, |v| v.parse().expect("--runs N"));
    if common::cannot_run_vms() {
        return;
    }
    let dir = common::workspace().join(format!("target/bench/run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let image = common::workload_image(&dir);
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (kernel, init, image) = (
        common::kernel().to_str().unwrap(),
        common::guest_init().to_str().unwrap(),
        image.to_str().unwrap(),
    );
    let command = ["--", "/bin/testguest", "exit", "0"];
    let cold_args = strings(
        &[
            &["vm", "run", "--kernel", kernel, "--init", init, "--rootfs", image][..],
            &command,
        ]
        .concat(),
    );
    let snapshots = !common::cannot_snapshot();
    let template = dir.join("template").display().to_string();
    if snapshots {
        run(
            &strings(&[
                "vm",
                "run",
                "--kernel",
                kernel,
                "--init",
                init,
                "--rootfs",
                image,
                "--snapshot-dir",
                &template,
                "--no-console",
            ]),
            false,
        );
    }
    let warm_args = strings(&[&["vm", "restore", &template, "--hold"][..], &command].concat());

    for _ in 0..WARMUP {
        run(&cold_args, false);
        if snapshots {
            run(&warm_args, true);
        }
    }
    let mut cold = Vec::with_capacity(runs);
    let mut warm = Vec::with_capacity(runs);
    for _ in 0..runs {
        cold.push(run(&cold_args, false));
        if snapshots {
            warm.push(run(&warm_args, true));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    use shards_abi::marker::{
        CONNECTED, INIT_STARTED, POWERING_OFF, RESUMED, WORKLOAD_EXITED, WORKLOAD_STARTED,
    };
    let between = |samples: &[support::Sample], from: Option<u32>, to: Option<u32>| {
        us(samples, |r| {
            let at = |m: Option<u32>| match m {
                Some(m) => r.marker_us(m),
                None => r.exit_us(),
            };
            at(to)?.checked_sub(at(from)?)
        })
    };
    let phases = |prefix: &str, samples: &[support::Sample], first: u32| {
        [
            (format!("{prefix}_connect"), Some(first), Some(CONNECTED)),
            (format!("{prefix}_spawn"), Some(CONNECTED), Some(WORKLOAD_STARTED)),
            (
                format!("{prefix}_command"),
                Some(WORKLOAD_STARTED),
                Some(WORKLOAD_EXITED),
            ),
            (
                format!("{prefix}_report"),
                Some(WORKLOAD_EXITED),
                Some(POWERING_OFF),
            ),
            (format!("{prefix}_power_off"), Some(POWERING_OFF), None),
        ]
        .into_iter()
        .map(|(name, from, to)| stats(&name, "us", between(samples, from, to)))
        .collect::<Vec<_>>()
    };
    let mut rows = vec![
        stats("cold_spawn_exit", "us", wall_us(&cold)),
        stats(
            "cold_boot",
            "us",
            us(&cold, |r| r.marker_us(INIT_STARTED)?.checked_sub(r.entry_us()?)),
        ),
    ];
    rows.extend(phases("cold", &cold, INIT_STARTED));
    if snapshots {
        rows.extend([
            stats(
                "warm_request",
                "us",
                us(&warm, |r| r.exit_us()?.checked_sub(r.released_us()?)),
            ),
            stats("warm_spawn_exit", "us", wall_us(&warm)),
            stats(
                "warm_resume",
                "us",
                us(&warm, |r| r.marker_us(RESUMED)?.checked_sub(r.released_us()?)),
            ),
        ]);
        rows.extend(phases("warm", &warm, RESUMED));
        rows.push(stats("warm_peak_rss", "MiB", rss_mib(&warm)));
    }
    report(
        "run",
        &[
            ("n", runs.to_string()),
            ("command", "/bin/testguest exit 0".into()),
            ("kernel", common::kernel_artifact().name.to_string()),
        ],
        &rows,
    );
}
