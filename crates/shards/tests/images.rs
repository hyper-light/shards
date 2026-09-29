//! Images from a registry, end to end: `shards run IMAGE` pulls the image from a registry
//! (a loopback one here, which containerd's rules reach over plain HTTP), builds its root
//! filesystem, boots a real VM into it, and runs the image's command as `docker run`
//! would. The image's program is the test guest (crates/testguest/src/workload.rs).
//! Runs need vsock, which shards has on Unix hosts.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The report's lines as a map from each line's first word.
fn report(stdout: &str) -> std::collections::BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn images_run_from_a_registry_as_docker_run_runs_them() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, served) = served();
    let home = TempDir::new("images");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];

    // Not here yet: pulled, then run with the image's settings.
    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", first.stdout, first.stderr);
    assert_eq!(first.status, Some(0), "{shown}");
    assert!(
        first
            .stderr
            .contains(&format!("Unable to find image '{image}' locally")),
        "{shown}"
    );
    for line in ["uid 1000", "cwd /work", "env FROM_IMAGE=yes", "env PATH=/bin"] {
        assert!(first.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }

    // Now stored: nothing is fetched, and the command line wins over the image.
    let asked = served.load(Ordering::SeqCst);
    let args = [
        "--pull",
        "never",
        "-u",
        "root",
        "-e",
        "FROM_IMAGE=cli",
        "-w",
        "/",
        image.as_str(),
        "report",
    ];
    let second = run_shards_env(&["run"], &args, &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
    assert_eq!(second.status, Some(0), "{shown}");
    for line in ["uid 0", "cwd /", "env FROM_IMAGE=cli"] {
        assert!(second.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
    assert_eq!(
        served.load(Ordering::SeqCst),
        asked,
        "the stored image served the second run"
    );
}

/// With the recorded guest, the first run of an image boots and saves a template, and the
/// next restores it: no `INIT_STARTED` marker, since init started in the template. The
/// restored run has the image's settings and the host's clock.
#[test]
fn repeat_runs_restore_a_template_of_the_image() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("templates");
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
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    let templates = || -> Vec<String> {
        std::fs::read_dir(home.join("templates"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    let first = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert!(booted(&first), "{}", first.stderr);
    if !shards_vmm::vm::SNAPSHOTS {
        assert!(templates().is_empty(), "no snapshots here: {:?}", templates());
        return;
    }
    let saved = templates();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(!saved[0].contains(".new-"), "{saved:?}");

    let before = now_ns();
    let second = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    let after = now_ns();
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
    assert_eq!(second.status, Some(0), "{shown}");
    assert!(!booted(&second), "restored, not booted: {shown}");
    assert!(second.marker_us(shards_abi::marker::RESUMED).is_some(), "{shown}");
    for line in ["uid 1000", "cwd /work", "env FROM_IMAGE=yes"] {
        assert!(second.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
    let realtime: u128 = report(&second.stdout)["realtime"].parse().unwrap();
    assert!(
        (before..=after).contains(&realtime),
        "{before} <= {realtime} <= {after}"
    );
    assert_eq!(templates(), saved, "the template was reused");

    // A template that no longer restores is removed, and the run boots instead; the next
    // run saves a new one.
    std::fs::write(
        home.join("templates").join(&saved[0]).join("state"),
        b"not a snapshot",
    )
    .unwrap();
    let third = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(third.status, Some(0), "{}", third.stderr);
    assert!(booted(&third), "{}", third.stderr);
    assert!(third.stderr.contains("booting instead"), "{}", third.stderr);
    assert!(templates().is_empty(), "{:?}", templates());
    let fourth = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(fourth.status, Some(0), "{}", fourth.stderr);
    assert_eq!(templates(), saved, "saved again under the same key");
}
