//! MicroVMs saved whole as they run (D176), end to end: `shards save vm` of real
//! microVMs, from the test image a loopback registry serves (tests/common, `served`),
//! booted from SHARDS_KERNEL and SHARDS_INIT.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Lines};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use common::{Run, TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(60);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// A home with the test image pulled, and the image's name.
fn home(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let pulled = shards_in(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    Some((home, image))
}

/// An attached run of the test image left going, its command's output read line by
/// line, its stderr in the home: the client, the lines, and where it said what it said.
fn start(
    home: &Path,
    image: &str,
    options: &[&str],
    command: &[&str],
) -> (Child, Lines<BufReader<ChildStdout>>, PathBuf) {
    let said = home.join(format!("run-{}.stderr", options.join("-").replace('/', "_")));
    let mut child = common::command()
        .args(["run", "--pull", "never"])
        .args(options)
        .arg(image)
        .args(command)
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&said).unwrap())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().map(Result::unwrap);
    assert_eq!(
        first.as_deref(),
        Some("ready"),
        "{command:?}: {}",
        std::fs::read_to_string(&said).unwrap_or_default()
    );
    (child, lines, said)
}

/// The count the counter says next.
fn next_count(lines: &mut Lines<BufReader<ChildStdout>>) -> u64 {
    lines.next().unwrap().unwrap().parse().unwrap()
}

/// Waits for a child, failing the test rather than hanging it.
fn exit(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("shards run did not exit within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The ID of the container `name`.
fn id_of(home: &Path, name: &str) -> String {
    let inspected = shards_in(home, &["inspect", "-f", "{{.Id}}", name]);
    assert_eq!(inspected.status, Some(0), "{inspected}");
    inspected.stdout.trim().to_string()
}

/// A running microVM is saved whole, as snapshot of its own: left running, it goes on
/// counting; stopped by its save, its client hears so, with the status a shell gives a
/// job it stopped, and `--rm` keeps it, snapshots and all. Names are a checkpoint's, each
/// once; a save without one takes the time's.
#[test]
fn a_running_microvm_is_saved_whole_left_running_or_stopped() {
    let Some((home, image)) = home("saved") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let (mut client, mut counts, said) =
        start(&home, &image, &["--name", "counter", "--rm"], &["counter", "20"]);
    if cannot_snapshot() {
        let refused = shards(&["save", "vm", "counter", "first"]);
        assert_eq!(refused.status, Some(1), "{refused}");
        assert!(refused.stderr.contains("take no snapshots yet"), "{refused}");
        let _ = shards(&["kill", "counter"]);
        let _ = exit(&mut client);
        return;
    }
    let before = next_count(&mut counts);
    let saved = shards(&["save", "vm", "counter", "first", "--leave-running"]);
    assert_eq!(
        (saved.status, saved.stdout.as_str()),
        (Some(0), "first\n"),
        "{saved}"
    );
    let snapshot = home
        .join("containers")
        .join(id_of(&home, "counter"))
        .join("snapshots")
        .join("first");
    for file in ["state", "memory", "swap"] {
        assert!(common::snapshot_file(&snapshot, file).is_file(), "{file}");
    }
    // It goes on counting where it was.
    let after = next_count(&mut counts);
    assert!(after > before, "{before} then {after}");
    assert_eq!(next_count(&mut counts), after + 1);
    // A name once; and a checkpoint's.
    let again = shards(&["save", "vm", "counter", "first", "--leave-running"]);
    assert_eq!(again.status, Some(1), "{again}");
    assert!(
        again.stderr.ends_with("has a snapshot named first already\n"),
        "{again}"
    );
    let bad = shards(&["save", "vm", "counter", "first."]);
    assert_eq!(bad.status, Some(1), "{bad}");
    assert!(bad.stderr.contains("cannot name a snapshot"), "{bad}");
    // Stopped by its save, as the time names it.
    let stopped = shards(&["save", "vm", "counter"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let name = stopped.stdout.trim().to_string();
    assert!(name.ends_with('Z') && !name.contains(':'), "{stopped}");
    assert!(common::snapshot_file(&snapshot.with_file_name(&name), "state").is_file());
    assert_eq!(exit(&mut client), Some(148));
    let told = std::fs::read_to_string(&said).unwrap();
    assert!(
        told.ends_with(&format!("shards: counter was saved as {name}, and stopped\n")),
        "{told}"
    );
    let inspected = shards(&[
        "inspect",
        "-f",
        "{{.State.Status}} {{.State.ExitCode}}",
        "counter",
    ]);
    assert_eq!(inspected.stdout, "exited 148\n", "{inspected}");
    let not = shards(&["save", "vm", "counter", "later"]);
    assert_eq!(not.status, Some(1), "{not}");
    assert!(not.stderr.ends_with("counter is not running\n"), "{not}");
}

/// A save a microVM cannot keep whole is refused, and the microVM goes on as it was, with
/// nothing of the save left: a volume's open files are its server's, which no snapshot
/// holds yet.
#[test]
fn a_save_refused_leaves_the_microvm_running_and_nothing_behind() {
    let Some((home, image)) = home("saved-refused") else {
        return;
    };
    if cannot_snapshot() {
        return;
    }
    let shards = |args: &[&str]| shards_in(&home, args);
    let (mut client, mut counts, _) = start(
        &home,
        &image,
        &["--name", "mounted", "-v", "kept:/kept"],
        &["counter", "20"],
    );
    let refused = shards(&["save", "vm", "mounted", "first"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused.stderr.contains("a volume is mounted in the microVM"),
        "{refused}"
    );
    let before = next_count(&mut counts);
    assert_eq!(next_count(&mut counts), before + 1);
    let snapshots = home
        .join("containers")
        .join(id_of(&home, "mounted"))
        .join("snapshots");
    let left: Vec<_> = std::fs::read_dir(&snapshots).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    let _ = shards(&["kill", "mounted"]);
    let _ = exit(&mut client);
}
