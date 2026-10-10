//! Images from a registry, end to end: `shards run IMAGE` pulls the image from a registry
//! (a loopback one here, which containerd's rules reach over plain HTTP), builds its root
//! filesystem, boots a real VM into it, and runs the image's command as `docker run`
//! would, through the daemon each test's first run starts. The image's program is the
//! test guest (crates/testguest/src/workload.rs). Runs need vsock, which shards has on
//! Unix hosts.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    TempDir, cannot_run_vms, guest_init, kernel, rootfs_dir, run_shards_env, served, sha256_digest,
    test_image,
};

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

    // The stored config changed under its digest, to run `exit 9`: a run that may not
    // pull refuses it rather than run a spec nobody pulled; one that may pulls it again,
    // and runs what was pulled (audit A11).
    let (_, blobs) = test_image();
    let config = &blobs[0];
    let digest = sha256_digest(config);
    let stored = home
        .join("images/blobs/sha256")
        .join(digest.trim_start_matches("sha256:"));
    assert_eq!(&std::fs::read(&stored).unwrap(), config);
    let text = String::from_utf8(config.clone()).unwrap();
    let changed = text.replace(r#""Cmd":["report"]"#, r#""Cmd":["exit","9"]"#);
    assert_ne!(changed, text);
    std::fs::write(&stored, changed).unwrap();
    let refused = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", refused.stdout, refused.stderr);
    assert_eq!(refused.status, Some(125), "{shown}");
    assert!(
        refused.stderr.contains(&format!(
            "the stored copy has changed; pull '{image}' again to mend it"
        )),
        "{shown}"
    );
    let mended = run_shards_env(&["run"], &["--pull", "missing", image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", mended.stdout, mended.stderr);
    assert_eq!(mended.status, Some(0), "{shown}");
    assert!(
        mended.stdout.lines().any(|l| l == "uid 1000"),
        "the pulled command ran\n{shown}"
    );
    assert!(
        mended
            .stderr
            .contains(&format!("the stored copy has changed; pulling '{image}' again")),
        "{shown}"
    );
    assert!(!mended.stderr.contains("Unable to find image"), "{shown}");
    assert_eq!(&std::fs::read(&stored).unwrap(), config, "the stored copy mended");
}

/// An image whose root filesystem would take more than the daemon's limits allow is
/// refused, as `docker run` refuses what its daemon cannot do, and nothing of it is built
/// (audit A10): here it has more entries than `SHARDS_MAX_IMAGE_ENTRIES`.
#[test]
fn an_image_past_its_limits_is_refused() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("images-limits");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
        ("SHARDS_MAX_IMAGE_ENTRIES", "3".as_ref()),
    ];
    let refused = run_shards_env(&["run"], &[image.as_str()], &env, TIMEOUT);
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", refused.stdout, refused.stderr);
    assert_eq!(refused.status, Some(125), "{shown}");
    assert!(
        refused
            .stderr
            .contains("more than 3 entries (SHARDS_MAX_IMAGE_ENTRIES)"),
        "{shown}"
    );
    let built = std::fs::read_dir(home.join(rootfs_dir()))
        .unwrap()
        .filter(|e| !e.as_ref().unwrap().file_name().to_string_lossy().starts_with('.'))
        .count();
    assert_eq!(built, 0, "{shown}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0));
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

    // The working set that restores prefetch, where the backend records one: what the
    // first run touched, once it had answered (HVF), or the first warm restore, the second
    // run's (KVM, `vm::RESTORES_RECORD`). The daemon's pool was restored before it
    // existed, so a new daemon restores the next run.
    if shards_vmm::vm::RESTORES_RECORD {
        let recording = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
        assert_eq!(recording.status, Some(0), "{}", recording.stderr);
        assert!(!booted(&recording), "{}", recording.stderr);
        assert_eq!(recording.prefetched(), Some(0), "{}", recording.stderr);
    }
    // Stopped at once, the daemon writes it before it exits, which `daemon stop` waits
    // for: no restore here records it again where restores record none.
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let working_set = common::snapshot_file(&home.join("templates").join(&saved[0]), "working-set");
    let recorded = std::fs::metadata(&working_set).map(|m| m.len()).unwrap_or(0);
    assert!(
        recorded > 1024 || !shards_vmm::vm::WORKING_SETS,
        "a working set of {recorded} bytes"
    );

    let before = now_ns();
    let second = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    let after = now_ns();
    let shown = format!("--- stdout\n{}\n--- stderr\n{}", second.stdout, second.stderr);
    assert_eq!(second.status, Some(0), "{shown}");
    assert!(!booted(&second), "restored, not booted: {shown}");
    assert!(second.marker_us(shards_abi::marker::RESUMED).is_some(), "{shown}");
    let prefetched = second.prefetched().unwrap_or(0);
    assert!(
        (prefetched > 100 && prefetched * 8 < u128::from(recorded)) || !shards_vmm::vm::WORKING_SETS,
        "{prefetched} pages prefetched of a {recorded}-byte working set: {shown}"
    );
    for line in ["uid 1000", "cwd /work", "env FROM_IMAGE=yes"] {
        assert!(second.stdout.lines().any(|l| l == line), "{line}\n{shown}");
    }
    let realtime: u128 = report(&second.stdout)["realtime"].parse().unwrap();
    assert!(
        (before..=after).contains(&realtime),
        "{before} <= {realtime} <= {after}"
    );
    assert_eq!(templates(), saved, "the template was reused");

    // A damaged working set is only a lost prefetch; without working sets, a file that is
    // none is ignored alike.
    std::fs::write(&working_set, b"not a working set").unwrap();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let unprefetched = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(unprefetched.status, Some(0), "{}", unprefetched.stderr);
    assert!(!booted(&unprefetched), "{}", unprefetched.stderr);
    assert_eq!(unprefetched.prefetched(), Some(0), "{}", unprefetched.stderr);
    assert!(
        unprefetched.stdout.lines().any(|l| l == "uid 1000"),
        "{}",
        unprefetched.stdout
    );

    // A template that no longer restores is removed, and the run boots instead, saving it
    // again. The daemon's pool holds VMs restored before the damage, so a new daemon, with
    // none, has to find out.
    std::fs::write(
        common::snapshot_file(&home.join("templates").join(&saved[0]), "state"),
        b"not a snapshot",
    )
    .unwrap();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let third = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(third.status, Some(0), "{}", third.stderr);
    assert!(booted(&third), "{}", third.stderr);
    assert!(
        third.stderr.contains("does not restore; booting instead"),
        "{}",
        third.stderr
    );
    assert_eq!(templates(), saved, "saved again under the same key");
    let fourth = run_shards_env(&["run"], &["--pull", "never", image.as_str()], &env, TIMEOUT);
    assert_eq!(fourth.status, Some(0), "{}", fourth.stderr);
    assert!(
        !booted(&fourth),
        "restored from the template saved again: {}",
        fourth.stderr
    );
}

/// An Agentfile's image restores its template as any image does. Its template is saved
/// with the in-VM server's device after its root filesystem (D60), and each restore is
/// given that device too: given its root filesystem alone, every restore was refused ("the
/// template names files it was not given") and each run booted. The guest is recorded
/// here, as users run, where `SHARDS_KERNEL` and `SHARDS_INIT` would boot every run.
#[test]
fn an_agentfiles_image_restores_its_template_too() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("agentfile-templates");
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
    let ctx = TempDir::new("agentfile-templates-ctx");
    std::fs::create_dir_all(ctx.join("agent")).unwrap();
    std::fs::write(ctx.join("agent/agent.json"), r#"{"name":"main"}"#).unwrap();
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM ./agent\n"),
    )
    .unwrap();
    let built = run_shards_env(
        &["build"],
        &["-t", "agents:1", ctx.to_str().unwrap()],
        &env,
        TIMEOUT,
    );
    assert_eq!(built.status, Some(0), "{}", built.stderr);
    let booted = |run: &common::Run| run.marker_us(shards_abi::marker::INIT_STARTED).is_some();
    let first = run_shards_env(&["run"], &["agents:1"], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    assert!(booted(&first), "{}", first.stderr);
    if !shards_vmm::vm::SNAPSHOTS {
        return;
    }
    for _ in 0..2 {
        let again = run_shards_env(&["run"], &["agents:1"], &env, TIMEOUT);
        let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
        let shown = format!(
            "--- stdout\n{}\n--- stderr\n{}\n--- daemon.log\n{log}",
            again.stdout, again.stderr
        );
        assert_eq!(again.status, Some(0), "{shown}");
        assert!(!again.stderr.contains("does not restore"), "{shown}");
        assert!(!booted(&again), "restored, not booted: {shown}");
        assert!(again.marker_us(shards_abi::marker::RESUMED).is_some(), "{shown}");
    }
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    assert!(!log.contains("names files it was not given"), "{log}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
}

/// shards' grammar makes an image a microVM before any action but its removal, as `run`
/// does: `inspect vm`, `inspect image`, `history image` and `tag image` of an image not
/// here pull and convert it first. Docker's own order (`image inspect`) answers as
/// Docker's does, and removal fetches nothing.
#[test]
fn actions_on_an_image_not_here_convert_it_first() {
    if cannot_run_vms() {
        eprintln!("SKIP: this host cannot run VMs");
        return;
    }
    let (image, served) = served();
    let home = TempDir::new("convert");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel().as_os_str()),
        ("SHARDS_INIT", guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| {
        let (command, rest) = args.split_first().unwrap();
        run_shards_env(&[command], rest, &env, TIMEOUT)
    };
    let shown = |r: &common::Run| format!("--- stdout\n{}\n--- stderr\n{}", r.stdout, r.stderr);
    let rmi = || {
        let r = shards(&["rmi", image.as_str()]);
        assert_eq!(r.status, Some(0), "{}", shown(&r));
    };

    // Neither Docker's order nor removal fetches anything.
    let docker = shards(&["image", "inspect", image.as_str()]);
    assert_eq!(docker.status, Some(1), "{}", shown(&docker));
    assert!(docker.stderr.contains("No such image"), "{}", shown(&docker));
    let removed = shards(&["remove", "image", image.as_str()]);
    assert_ne!(removed.status, Some(0), "{}", shown(&removed));
    assert_eq!(served.load(Ordering::SeqCst), 0, "nothing was fetched");

    // `inspect vm IMAGE`: converted, and its microVM described.
    let vm = shards(&[
        "inspect",
        "vm",
        "--format",
        "{{.Id}} {{.Config.Cmd}}",
        image.as_str(),
    ]);
    assert_eq!(vm.status, Some(0), "{}", shown(&vm));
    assert!(
        vm.stderr
            .contains(&format!("Unable to find image '{image}' locally")),
        "{}",
        shown(&vm)
    );
    assert!(
        vm.stdout.starts_with("sha256:") && vm.stdout.trim_end().ends_with("[report]"),
        "{}",
        shown(&vm)
    );
    rmi();

    let inspected = shards(&["inspect", "image", "--format", "{{.Id}}", image.as_str()]);
    assert_eq!(inspected.status, Some(0), "{}", shown(&inspected));
    assert_eq!(
        inspected.stdout,
        vm.stdout.split(' ').next().unwrap().to_string() + "\n"
    );
    rmi();

    let history = shards(&["history", "image", "-q", image.as_str()]);
    assert_eq!(history.status, Some(0), "{}", shown(&history));
    // The test image's config records no history, so none is listed; it is stored now.
    let stored = shards(&["images", "-q"]);
    assert!(!stored.stdout.trim().is_empty(), "{}", shown(&stored));
    rmi();

    let tagged = shards(&["tag", "image", image.as_str(), "mine:converted"]);
    assert_eq!(tagged.status, Some(0), "{}", shown(&tagged));
    let listed = shards(&["images", "--format", "{{.Repository}}:{{.Tag}}"]);
    assert!(
        listed.stdout.lines().any(|l| l == "mine:converted"),
        "{}",
        shown(&listed)
    );
    let _ = shards(&["stop", "daemon"]);
}
