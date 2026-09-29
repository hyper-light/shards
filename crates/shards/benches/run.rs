//! Run benchmark: how fast a command runs in an image, from the request to its exit
//! status. The command is `/bin/testguest exit 0`, in the minimal image the E2E tests use
//! (tests/common, `workload_image`).
//!
//! - cold: `shards vm run --rootfs IMAGE -- COMMAND` boots the kernel into the image for
//!   every command.
//!   - `cold_spawn_exit`: host wall clock around the whole process.
//! - warm: a template, saved once the image is mounted (`vm run --rootfs --snapshot-dir`),
//!   is restored with `--hold` for every command. The restored VM resumes and connects;
//!   then the request (a line on stdin) sends it the command.
//!   - `warm_request`: the request → shards has read the command's exit status (the VMM's
//!     clock): a request to a warm VM, answered.
//!   - `warm_spawn_exit`: host wall clock around the whole process.
//!
//! Phases, from shards-init's markers (the VMM's clock):
//! - `warm_resume`: release → the guest runs again; `cold_boot`: first guest entry → init
//!   starts.
//! - `*_connect`: init running → connected to the host (cold: the image mounted first).
//!   Warm VMs resume and connect before their request.
//! - `*_spawn`: connected (warm: the request) → the command executing: the workload
//!   received, its user resolved, fork and exec.
//! - `*_command`: the command's own run.
//! - `*_report`: the command exited → init powers off: output drained, the status sent
//!   and read.
//! - `*_power_off`: → the VM stops.
//!
//! Peak RSS includes the guest memory the process touched.
//!
//! Guest state differs from template to template, and so can a restore's cost, so warm
//! samples come from `--templates` templates (default 5), restored in turn.
//!
//! `cargo bench -p shards --bench run [-- --runs N --templates T]`

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
    use support::{peak_rss_mib, report, run, stats, us, wall_us};

    const WARMUP: usize = 3;
    let runs: usize = support::option("--runs").map_or(50, |v| v.parse().expect("--runs N"));
    let templates: usize = support::option("--templates").map_or(5, |v| v.parse().expect("--templates T"));
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
            &["run", "--kernel", kernel, "--init", init, "--rootfs", image][..],
            &command,
        ]
        .concat(),
    );
    let snapshots = !common::cannot_snapshot();
    let mut warm_args = Vec::new();
    for t in 0..if snapshots { templates.max(1) } else { 0 } {
        let template = dir.join(format!("template-{t}")).display().to_string();
        run(
            &strings(&[
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
        warm_args.push(strings(
            &[&["restore", &template, "--hold"][..], &command].concat(),
        ));
    }

    for i in 0..WARMUP {
        run(&cold_args, false);
        if let Some(args) = warm_args.get(i % warm_args.len().max(1)) {
            run(args, true);
        }
    }
    let mut cold = Vec::with_capacity(runs);
    let mut warm = Vec::with_capacity(runs);
    for i in 0..runs {
        cold.push(run(&cold_args, false));
        if let Some(args) = warm_args.get(i % warm_args.len().max(1)) {
            warm.push(run(args, true));
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
    // Connect, spawn (from `spawn_from`), command, report and power-off rows.
    let phases = |prefix: &str,
                  samples: &[support::Sample],
                  first: u32,
                  spawn_from: &dyn Fn(&common::Run) -> Option<u128>| {
        vec![
            stats(
                &format!("{prefix}_connect"),
                "us",
                between(samples, Some(first), Some(CONNECTED)),
            ),
            stats(
                &format!("{prefix}_spawn"),
                "us",
                us(samples, |r| {
                    r.marker_us(WORKLOAD_STARTED)?.checked_sub(spawn_from(r)?)
                }),
            ),
            stats(
                &format!("{prefix}_command"),
                "us",
                between(samples, Some(WORKLOAD_STARTED), Some(WORKLOAD_EXITED)),
            ),
            stats(
                &format!("{prefix}_report"),
                "us",
                between(samples, Some(WORKLOAD_EXITED), Some(POWERING_OFF)),
            ),
            stats(
                &format!("{prefix}_power_off"),
                "us",
                between(samples, Some(POWERING_OFF), None),
            ),
        ]
    };
    let mut rows = vec![
        stats("cold_spawn_exit", "us", wall_us(&cold)),
        stats(
            "cold_boot",
            "us",
            us(&cold, |r| r.marker_us(INIT_STARTED)?.checked_sub(r.entry_us()?)),
        ),
    ];
    rows.extend(phases("cold", &cold, INIT_STARTED, &|r| r.marker_us(CONNECTED)));
    if snapshots {
        rows.extend([
            stats(
                "warm_request",
                "us",
                us(&warm, |r| r.answered_us()?.checked_sub(r.request_us()?)),
            ),
            stats("warm_spawn_exit", "us", wall_us(&warm)),
            stats(
                "warm_resume",
                "us",
                us(&warm, |r| r.marker_us(RESUMED)?.checked_sub(r.released_us()?)),
            ),
        ]);
        rows.extend(phases("warm", &warm, RESUMED, &|r| r.request_us()));
        rows.push(stats("warm_peak_rss", "MiB", peak_rss_mib(&warm)));
    }
    report(
        "run",
        &[
            ("n", runs.to_string()),
            ("templates", warm_args.len().to_string()),
            ("command", "/bin/testguest exit 0".into()),
            ("kernel", common::kernel_artifact().name.to_string()),
        ],
        &rows,
    );
}
