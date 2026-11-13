//! A microVM's members as an exec finds them (the harness plan's step 6, G24), on real VMs
//! booted from SHARDS_KERNEL and SHARDS_INIT: an image from a loopback registry (tests/common,
//! `served`), whose Agentfile runs the test guest as an agent; what it sees of itself is what
//! its `confined` mode says, on the run's stderr, each line prefixed with the agent's name.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// A member's process starts with its signals as an exec's does (shards-init's
/// `defaults::signals`): none ignored, though init ignores SIGPIPE (Rust's runtime does) and
/// execve keeps an ignored signal ignored, and none blocked. So a process of the member's
/// writing to a pipe whose reader has gone dies of SIGPIPE, as one a shell starts does,
/// where it saw EPIPE: the probe's child, which keeps the disposition the probe was given.
#[test]
fn a_member_starts_with_every_signal_at_its_default() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("mx-signals-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let ctx = TempDir::new("mx-signals-ctx");
    std::fs::create_dir_all(ctx.join("agent/bin")).unwrap();
    std::fs::copy(common::test_guest(), ctx.join("agent/bin/testguest")).unwrap();
    std::fs::write(
        ctx.join("agent/agent.json"),
        r#"{"entrypoint":"bin/testguest","options":["confined","signals","pipe"]}"#,
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\n"),
    )
    .unwrap();
    let built = shards(&["build", "image", "-t", "mx-signals:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{built}");
    let ran = common::run_awaiting(
        &env,
        &[
            "run",
            "vm",
            "--rm",
            "--name",
            "mx-signals",
            "mx-signals:1",
            "await",
            "confined-ready",
            "1",
        ],
        "mx-signals",
        ("confined-ready", 1),
        TIMEOUT,
    );
    assert_eq!(ran.status, Some(0), "{ran}");
    let said: Vec<&str> = ran
        .stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[agent main] "))
        .collect();
    for want in [
        "confined signals ignored=0000000000000000 blocked=0000000000000000",
        "confined pipe: signal 13",
    ] {
        assert!(said.contains(&want), "no {want:?} in:\n{}", ran.stderr);
    }
}
