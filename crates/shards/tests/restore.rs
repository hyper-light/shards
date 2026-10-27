//! Restores end to end, cold and warm, on every host that takes snapshots (macOS, x86_64
//! Linux, Windows: D25, D196): a cold restore is `shards restore DIR`, a process of its own
//! from start to end, and several of one snapshot side by side map its memory file
//! copy-on-write; a warm one is `shards restore DIR --hold`, prepared and held until a line
//! comes on its stdin, which is all its start then costs. Each restored guest checks itself
//! as the snapshot tests' do (testguest `snapshot`).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Run, cannot_run_vms, cannot_snapshot, kernel, run_shards, test_guest};
use shards_testguest::fill;

const TIMEOUT: Duration = Duration::from_secs(120);
const RO_SALT: u64 = 1;
const RO_BYTES: usize = 4 << 20;
const RESUMED: u32 = 2;
/// How long a held restore is left before its release: long past anything of a guest's
/// that would run unreleased.
const HELD: Duration = Duration::from_millis(500);

/// A test's directory, its read-only disk written, and its snapshot's directory.
struct Scratch(shards_testdir::TempDir);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = shards_testdir::TempDir::new(tag).unwrap();
        let mut data = vec![0u8; RO_BYTES];
        fill(RO_SALT, 0, &mut data);
        std::fs::write(dir.join("ro.img"), &data).unwrap();
        Scratch(dir)
    }

    fn snapshot(&self) -> PathBuf {
        self.0.join("snapshot")
    }

    /// Boots the snapshot guest on two vCPUs, which asks for its snapshot and stops.
    fn boot_and_snapshot(&self) -> Run {
        let disk = format!("{}:ro", self.0.join("ro.img").display());
        let cmdline =
            format!("console=ttyS0 quiet panic=-1 shards_test=snapshot shards_vda_bytes={RO_BYTES}");
        run_shards(
            &["run"],
            &[
                "--kernel",
                kernel().to_str().unwrap(),
                "--init",
                test_guest().to_str().unwrap(),
                "--cpus",
                "2",
                "--memory",
                "256",
                "--disk",
                &disk,
                "--cmdline",
                &cmdline,
                "--snapshot-dir",
                self.snapshot().to_str().unwrap(),
                "--snapshot-then",
                "stop",
            ],
            TIMEOUT,
        )
    }
}

/// What a restored guest printed of its reseeded RNG.
fn random(r: &Run) -> String {
    r.stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("SHARDS-TEST INFO random="))
        .unwrap_or_else(|| panic!("no random bytes: {r}"))
        .to_string()
}

/// The snapshot's memory file's bytes.
fn memory(dir: &Path) -> Vec<u8> {
    std::fs::read(common::snapshot_file(dir, "memory")).unwrap()
}

/// Cold restores of one snapshot side by side, each a process of its own, its guest's
/// memory the snapshot's file mapped copy-on-write: every guest goes on as it asked for
/// its snapshot, none sees what another wrote, their RNGs are reseeded apart (VMGenID), and
/// the file is as the snapshot wrote it.
#[test]
fn cold_restores_side_by_side_go_on_apart() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("cold");
    let original = s.boot_and_snapshot();
    assert_eq!(original.status, Some(0), "{original}");
    let before = memory(&s.snapshot());
    let snapshot = s.snapshot();
    let runs: Vec<Run> = std::thread::scope(|scope| {
        let restores: Vec<_> = (0..3)
            .map(|_| scope.spawn(|| run_shards(&["restore"], &[snapshot.to_str().unwrap()], TIMEOUT)))
            .collect();
        restores.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let mut randoms = Vec::new();
    for (i, r) in runs.iter().enumerate() {
        assert_eq!(r.status, Some(0), "restore {i}: {r}");
        assert!(r.stdout.contains("SHARDS-TEST PASS"), "restore {i}: {r}");
        assert!(r.stdout.contains("generation=1"), "restore {i}: {r}");
        randoms.push(random(r));
    }
    randoms.sort();
    randoms.dedup();
    assert_eq!(randoms.len(), 3, "clones shared RNG state: {randoms:?}");
    assert!(
        memory(&s.snapshot()) == before,
        "a restore wrote the snapshot's memory file"
    );
}

/// A warm restore, held (`--hold`), prepares its VM and says `shards-ready`, and runs
/// nothing of its guest until a line comes on its stdin; released, its guest goes on as it
/// ran, its first act after the snapshot (the RESUMED marker) after the release.
#[test]
fn a_held_restore_runs_its_guest_only_once_released() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("warm");
    let original = s.boot_and_snapshot();
    assert_eq!(original.status, Some(0), "{original}");
    let started = Instant::now();
    let mut child = common::command()
        .args(["restore", s.snapshot().to_str().unwrap(), "--hold"])
        .env("SHARDS_TIMING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let mut line = String::new();
        while stderr.read_line(&mut line).is_ok_and(|n| n > 0) {
            if line.contains("shards-ready") {
                let _ = ready_tx.send(());
            }
            text.push_str(&line);
            line.clear();
        }
        text
    });
    if ready.recv_timeout(TIMEOUT).is_err() {
        let _ = child.kill();
        panic!("never ready: {}", err.join().unwrap());
    }
    std::thread::sleep(HELD);
    assert!(
        child.try_wait().unwrap().is_none(),
        "a held restore ended unreleased"
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let r = Run {
        status: status.code(),
        signal: None,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
        elapsed: started.elapsed(),
    };
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert!(r.stdout.contains("generation=1"), "{r}");
    let released = r.released_us().unwrap_or_else(|| panic!("no release: {r}"));
    let resumed = r
        .marker_us(RESUMED)
        .unwrap_or_else(|| panic!("no RESUMED marker: {r}"));
    assert!(
        resumed >= released,
        "its guest ran before its release: resumed at {resumed} us, released at {released} us"
    );
    assert!(
        released >= HELD.as_micros(),
        "released at {released} us, before the {HELD:?} it was held: {r}"
    );
}
