//! Workloads benchmark: `shards run IMAGE COMMAND` as a user runs it, for commands that
//! do different work, each run's output checked byte for byte (docs/audit/2026-09-29_audit.md,
//! "Required benchmark matrix": workloads, exact status and output). The image is the E2E
//! tests' registry image, its entrypoint the test guest; runs are served from the
//! template's warm VMs where this build snapshots, as a user's are (D26), else booted.
//!
//! - `true`: `exit 0`, nothing written.
//! - `stderr`: a line to stderr, read back exactly.
//! - `stdout`: `bulk 16 MiB`, a pattern (shards_testguest) checked byte for byte.
//! - `stdin`: `cat` with `-i`, 16 MiB of a pattern in, the same 16 MiB out.
//! - `cpu`: `hash 64 MiB`, FNV-1a over the pattern in the guest, the answer checked.
//!
//! The host's wall clock from the client's spawn to its exit, and for the streams the
//! bytes a second. A run whose status or output is wrong stops the benchmark. The
//! workloads take turns, after two runs of each that save and warm the template.
//!
//! `cargo bench -p shards --bench workloads [-- --runs N]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

#[cfg(unix)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(unix)]
mod support;

#[cfg(not(unix))]
fn main() {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "SKIP: the workloads benchmark needs a Unix host"
    );
}

#[cfg(unix)]
fn main() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const STREAM: u64 = 16 << 20;
    const CPU: u64 = 64 << 20;
    const WARMUP: usize = 2;

    let runs: usize = support::option("--runs").map_or(20, |v| v.parse().expect("--runs N"));
    if common::cannot_run_vms() {
        return;
    }
    let home = common::workspace().join(format!("target/bench/workloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let (image, _) = common::served();
    let (kernel, init) = (common::kernel(), common::guest_init());
    let shards = |args: &[&str], extra: &[(&str, &std::ffi::OsStr)]| {
        let mut command = Command::new(common::shards());
        command.args(args).env("SHARDS_HOME", &home);
        for (k, v) in extra {
            command.env(k, v);
        }
        command
    };
    let status = |mut c: Command| c.stdin(Stdio::null()).output().unwrap();
    let pulled = status(shards(&["pull", "-q", &image], &[]));
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    // Where this build cannot snapshot, every run boots SHARDS_KERNEL and SHARDS_INIT.
    let booted: Vec<(&str, &std::ffi::OsStr)> = if shards_vmm::vm::SNAPSHOTS {
        let recorded = status(shards(
            &[
                "guest",
                "use",
                "--kernel",
                kernel.to_str().unwrap(),
                "--init",
                init.to_str().unwrap(),
            ],
            &[],
        ));
        assert!(recorded.status.success());
        Vec::new()
    } else {
        vec![
            ("SHARDS_KERNEL", kernel.as_os_str()),
            ("SHARDS_INIT", init.as_os_str()),
        ]
    };

    let mut expected_stdout = vec![0u8; STREAM as usize];
    shards_testguest::fill(7, 0, &mut expected_stdout);
    let mut stdin_bytes = vec![0u8; STREAM as usize];
    shards_testguest::fill(9, 0, &mut stdin_bytes);
    let hash = format!("{:016x}\n", shards_testguest::pattern_hash(3, CPU));
    let (stream, cpu) = (STREAM.to_string(), CPU.to_string());

    // One run of `workload`: its wall clock, its output checked.
    let run = |workload: &str| -> f64 {
        let (args, input): (Vec<&str>, Option<&[u8]>) = match workload {
            "true" => (vec!["exit", "0"], None),
            "stderr" => (vec!["stderr", "to stderr"], None),
            "stdout" => (vec!["bulk", &stream, "7"], None),
            "stdin" => (vec!["cat"], Some(&stdin_bytes)),
            "cpu" => (vec!["hash", &cpu, "3"], None),
            other => panic!("no workload {other}"),
        };
        let mut full = vec!["run", "--pull", "never"];
        if input.is_some() {
            full.push("-i");
        }
        full.push(&image);
        full.extend(args);
        let t0 = Instant::now();
        let mut child = shards(&full, &booted)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().unwrap();
            let bytes = bytes.to_vec();
            std::thread::spawn(move || stdin.write_all(&bytes).unwrap())
        });
        let mut stderr = child.stderr.take().unwrap();
        let errs = std::thread::spawn(move || {
            let mut e = Vec::new();
            stderr.read_to_end(&mut e).unwrap();
            e
        });
        let mut out = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        let done = child.wait().unwrap();
        let elapsed = t0.elapsed().as_secs_f64() * 1e6;
        if let Some(w) = writer {
            w.join().unwrap();
        }
        let err = errs.join().unwrap();
        let err_text = String::from_utf8_lossy(&err);
        assert!(done.success(), "{workload}: {done:?}: {err_text}");
        match workload {
            "true" => assert!(out.is_empty(), "true wrote {} bytes", out.len()),
            "stderr" => assert_eq!(err_text, "to stderr", "stderr"),
            "stdout" => assert!(
                out == expected_stdout,
                "stdout: {} bytes, first wrong at {:?}",
                out.len(),
                shards_testguest::first_mismatch(7, 0, &out)
            ),
            "stdin" => assert!(
                out == stdin_bytes,
                "stdin: {} bytes back, first wrong at {:?}",
                out.len(),
                shards_testguest::first_mismatch(9, 0, &out)
            ),
            "cpu" => assert_eq!(String::from_utf8_lossy(&out), hash, "cpu"),
            _ => {}
        }
        elapsed
    };

    let workloads = ["true", "stderr", "stdout", "stdin", "cpu"];
    for w in workloads {
        for _ in 0..WARMUP {
            run(w);
        }
    }
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(runs); workloads.len()];
    for _ in 0..runs {
        for (i, w) in workloads.iter().enumerate() {
            samples[i].push(run(w));
        }
    }
    let _ = status(shards(&["daemon", "stop"], &[]));
    let _ = std::fs::remove_dir_all(&home);
    let mib_per_s = |v: &[f64]| -> Vec<f64> {
        v.iter()
            .map(|us| STREAM as f64 / (1 << 20) as f64 / (us / 1e6))
            .collect()
    };
    let mut rows: Vec<(String, String)> = workloads
        .iter()
        .zip(&samples)
        .map(|(w, v)| support::stats(w, "us", v.clone()))
        .collect();
    rows.push(support::stats("stdout_rate", "MiB/s", mib_per_s(&samples[2])));
    rows.push(support::stats("stdin_rate", "MiB/s", mib_per_s(&samples[3])));
    support::report(
        "workloads",
        &[
            ("n", runs.to_string()),
            (
                "served",
                if shards_vmm::vm::SNAPSHOTS {
                    "template"
                } else {
                    "boot"
                }
                .to_string(),
            ),
            ("stream_mib", (STREAM >> 20).to_string()),
            ("cpu_mib", (CPU >> 20).to_string()),
        ],
        &rows,
    );
}
