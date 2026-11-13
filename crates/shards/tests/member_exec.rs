//! A microVM's members as a command run in them finds them (the harness plan's step 6,
//! G24), on real VMs booted from SHARDS_KERNEL and SHARDS_INIT: an image from a loopback
//! registry (tests/common, `served`) whose Agentfile's harness is the test guest, booted for
//! an orchestrator through shards-api's `ImageShards` and this build's `shards api-run`
//! (D231). What the harness sees of itself is what its `confined` mode says on its stdout,
//! read as its owner reads it (D180): a daemon's run hands a member's output to its owner's
//! endpoint alone, never to the run's own stderr.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader};
use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use shards_api::image::{
    Channel, ImageGuest, ImageShards, ImageSpec, Images as _, MemberName, MemberSpec, Narrowing, RunEnd,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The variables a test's run host is given: its home, this build's guest, and the tests'
/// own docker config and local store (`common::as_tests_run`'s).
fn host_env(home: &TempDir) -> Vec<(OsString, OsString)> {
    vec![
        ("SHARDS_HOME".into(), home.as_os_str().into()),
        ("SHARDS_KERNEL".into(), kernel().as_os_str().into()),
        ("SHARDS_INIT".into(), guest_init().as_os_str().into()),
        ("SHARDS_LOCAL_STORE".into(), "none".into()),
        ("DOCKER_CONFIG".into(), common::docker_config().as_os_str().into()),
    ]
}

fn codex() -> MemberName {
    MemberName::harness("codex")
}

/// Builds an image whose harness `codex` is the test guest started with `options`, tagged
/// `tag` in `home`.
fn build(home: &TempDir, tag: &str, options: &[&str]) {
    let (image, _) = served();
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let ctx = TempDir::new("mx-ctx");
    let binary = std::fs::read(common::test_guest()).unwrap();
    let options: Vec<String> = options.iter().map(|o| format!("{o:?}")).collect();
    let descriptor = format!(r#"{{"entrypoint":"codex","options":[{}]}}"#, options.join(","));
    std::fs::write(
        ctx.join("harness.tar"),
        common::tar(&[
            ("bin", 0o755, 0, None),
            ("bin/codex", 0o755, 0, Some(&binary)),
            ("harness.json", 0o644, 0, Some(descriptor.as_bytes())),
        ]),
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nHARNESS codex FROM ./harness.tar\n"),
    )
    .unwrap();
    let built = run_shards_env(
        &["build", "image"],
        &["-t", tag, ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
}

/// `image` booted from `home` with its harness running as the image declares it.
fn boot(home: &TempDir, image: &str) -> Box<dyn ImageGuest> {
    let spec = ImageSpec {
        image: image.into(),
        vcpus: 1,
        memory_mib: 512,
        command: None,
        members: vec![MemberSpec {
            member: codex(),
            env: Vec::new(),
            files: Vec::new(),
            workdir: None,
            stdin: false,
            terminal: false,
        }],
        publish: Vec::new(),
        egress: Narrowing::default(),
        credentials: Vec::new(),
        shares: Vec::new(),
        nesting: false,
        ready_by: None,
        binary: Some(common::shards().to_path_buf()),
        env: host_env(home),
    };
    ImageShards::new().boot(&spec, TIMEOUT).unwrap()
}

/// The lines the harness writes on its stdout up to the first that begins with `last`,
/// that one included: `confined` says it all at once, then waits to be ended.
fn lines_until(guest: &dyn ImageGuest, last: &str) -> Vec<String> {
    let pipe = guest.take_output(&codex(), Channel::Stdout).unwrap();
    let last = last.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut said = Vec::new();
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let done = line.starts_with(&last);
            said.push(line);
            if done {
                break;
            }
        }
        let _ = tx.send(said);
    });
    rx.recv_timeout(TIMEOUT)
        .expect("the harness said nothing in time")
}

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
    let home = TempDir::new("mx-signals-home");
    build(&home, "mx-signals:1", &["confined", "signals", "pipe"]);
    let guest = boot(&home, "mx-signals:1");
    let said = lines_until(&*guest, "confined net=");
    for want in [
        "confined signals ignored=0000000000000000 blocked=0000000000000000",
        "confined pipe: signal 13",
    ] {
        assert!(
            said.iter().any(|l| l == want),
            "no {want:?} in:\n{}",
            said.join("\n")
        );
    }
    guest.stop().unwrap();
    assert_eq!(guest.wait_for(TIMEOUT).unwrap(), Some(RunEnd::Stopped));
}
