//! Real-VM ordinary-child cleanup with an independently owned joiner observing pidfds.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]
mod common;

use common::{Run, TempDir, guest_init, kernel};
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(90);
fn call(home: &Path, args: &[&str]) -> Run {
    common::run_shards_env(
        &[],
        args,
        &[
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ],
        PATIENCE,
    )
}
struct Client {
    child: Child,
    output: BufReader<ChildStdout>,
}
fn ready(home: &Path, args: &[&str]) -> Client {
    let mut child = common::interruptible(
        common::command()
            .args(args)
            .env("SHARDS_HOME", home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()),
    )
    .spawn()
    .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n", "{args:?}: {line:?}");
    Client { child, output }
}
fn finished(client: &mut Client) -> String {
    let mut rest = String::new();
    client.output.read_to_string(&mut rest).unwrap();
    let status = client.child.wait().unwrap();
    assert!(status.success(), "{status}: {rest}");
    rest
}
struct Daemon<'a>(&'a Path);
impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = call(self.0, &["daemon", "stop"]);
    }
}

#[test]
#[ignore = "explicit bounded real-VM cleanup campaign"]
fn main_cleanup_reaps_owned_children_and_preserves_independent_joiner() {
    assert!(
        !common::cannot_run_vms(),
        "actual cleanup campaign requires a working hypervisor"
    );
    let home = TempDir::new("cleanup-owned");
    let _daemon = Daemon(&home);
    let (image, _) = common::served();
    println!(
        "CLEANUP_BINARIES shards={} init={} guest={} kernel={} helpers={}",
        common::shards().display(),
        guest_init().display(),
        common::test_guest().display(),
        kernel().display(),
        env!("SHARDS_HELPERS_CARRIED")
    );
    let pulled = call(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Counts bound this synthetic fixture. 72 children exceeds the unrelated 64-pass
    // source literal, but is not a claim that process count requires that many passes.
    for (mode, n) in [("own", 8), ("init", 8), ("host", 8), ("host", 72)] {
        let token = format!("cleanup-{}-{mode}-{n}", std::process::id());
        let count = n.to_string();
        let mut args = vec!["run", "--pull", "never", "--name", "source"];
        match mode {
            "host" => args.extend(["--pid", "host"]),
            "init" => args.push("--init"),
            _ => {}
        }
        args.extend([&image, "cleanup", "/tmp/cleanup-control", &token, &count, "90000"]);
        let mut main = ready(&home, &args);
        let join = call(
            &home,
            &[
                "run",
                "--pull",
                "never",
                "-d",
                "--name",
                "observer",
                "--network",
                "container:source",
                "--pid",
                "host",
                &image,
                "sleep",
            ],
        );
        assert_eq!(join.status, Some(0), "{join}");
        let pid = call(&home, &["inspect", "-f", "{{.State.Pid}}", "source"]);
        assert_eq!(pid.status, Some(0), "{pid}");
        let mut watcher = ready(
            &home,
            &[
                "exec",
                "observer",
                "/bin/testguest",
                "cleanup-control",
                "watch",
                &token,
                &count,
                "20000",
            ],
        );
        if mode == "own" {
            // A held-live control proves the observer cannot report cleanup merely
            // because main acknowledged readiness. This is not a product failure.
            let mut negative = ready(
                &home,
                &[
                    "exec",
                    "observer",
                    "/bin/testguest",
                    "cleanup-control",
                    "watch",
                    &token,
                    &count,
                    "100",
                ],
            );
            let mut rest = String::new();
            negative.output.read_to_string(&mut rest).unwrap();
            assert_eq!(
                negative.child.wait().unwrap().code(),
                Some(1),
                "known-live control passed: {rest}"
            );
            println!(
                "CLEANUP_CONTROL deliberately_unreleased=true children={n} expected_observer_timeout=true"
            );
        }
        let began = Instant::now();
        let release = call(
            &home,
            &[
                "exec",
                "source",
                "/bin/testguest",
                "cleanup-control",
                "release",
                "/tmp/cleanup-control",
            ],
        );
        assert_eq!(release.status, Some(0), "{release}");
        let ended = finished(&mut main);
        assert_eq!(ended, format!("released {n}\n"));
        let observed = finished(&mut watcher);
        assert!(
            observed.contains(&format!("cleanup children {n} reaped {n} remaining 0")),
            "{observed}"
        );
        let proof = format!("write:/work/cleanup-proof={token}");
        let progress = call(
            &home,
            &[
                "exec",
                "observer",
                "/bin/testguest",
                "fs",
                &proof,
                "print:/work/cleanup-proof",
            ],
        );
        assert_eq!(
            (progress.status, progress.stdout.as_str()),
            (Some(0), token.as_str()),
            "{progress}"
        );
        println!(
            "CLEANUP_RESULT mode={mode} children={n} vm_pid={} {} surviving_joiner_progress=true total_us={}",
            pid.stdout.trim(),
            observed.trim(),
            began.elapsed().as_micros()
        );
        let removed = call(&home, &["rm", "-f", "source", "observer"]);
        assert_eq!(removed.status, Some(0), "{removed}");
    }
}
