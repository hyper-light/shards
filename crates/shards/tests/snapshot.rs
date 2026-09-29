//! Snapshots end to end: a real guest builds state, asks for a snapshot, and every
//! restore of it, each in its own process, must continue with that state intact.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Run, cannot_run_vms, cannot_snapshot, kernel, run_shards, test_guest, vm_run};
use shards_testguest::fill;

const TIMEOUT: Duration = Duration::from_secs(120);
const RO_SALT: u64 = 1;
const RO_BYTES: usize = 4 << 20;
const RESUMED: u32 = 2;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = common::workspace().join(format!("target/e2e/{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut data = vec![0u8; RO_BYTES];
        fill(RO_SALT, 0, &mut data);
        std::fs::write(dir.join("ro.img"), &data).unwrap();
        Scratch(dir)
    }

    fn disk(&self) -> String {
        format!("{}:ro", self.0.join("ro.img").display())
    }

    fn snapshot(&self) -> PathBuf {
        self.0.join("snapshot")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn boot_and_snapshot(s: &Scratch, then: &str) -> Run {
    let cmdline = format!("console=ttyS0 quiet panic=-1 shards_test=snapshot shards_vda_bytes={RO_BYTES}");
    let snapshot = s.snapshot().display().to_string();
    vm_run(
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
            &s.disk(),
            "--cmdline",
            &cmdline,
            "--snapshot-dir",
            &snapshot,
            "--snapshot-then",
            then,
        ],
        TIMEOUT,
    )
}

fn restore(dir: &Path) -> Run {
    run_shards(&["vm", "restore"], &[dir.to_str().unwrap()], TIMEOUT)
}

#[test]
fn every_restore_continues_the_guest_where_it_asked_for_the_snapshot() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot");
    let original = boot_and_snapshot(&s, "stop");
    assert_eq!(original.status, Some(0), "{original}");
    assert!(!original.stdout.contains("SHARDS-TEST"), "{original}");
    assert!(s.snapshot().join("state").is_file(), "{original}");
    assert!(s.snapshot().join("memory").is_file(), "{original}");

    let mut randoms = Vec::new();
    for i in 0..3 {
        let r = restore(&s.snapshot());
        assert_eq!(r.status, Some(0), "restore {i}: {r}");
        assert!(r.stdout.contains("generation=1"), "restore {i}: {r}");
        assert!(r.stdout.contains("SHARDS-TEST PASS"), "restore {i}: {r}");
        assert!(r.marker_us(RESUMED).is_some(), "restore {i}: {r}");
        let random = r
            .stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("SHARDS-TEST INFO random="))
            .map(str::to_string);
        randoms.push(random.unwrap_or_else(|| panic!("restore {i} printed no random bytes: {r}")));
    }
    // Clones of one snapshot must not share their kernel RNG state (VMGenID reseeds it).
    randoms.sort();
    randoms.dedup();
    assert_eq!(
        randoms.len(),
        3,
        "clones produced identical random bytes: {randoms:?}"
    );
}

#[test]
fn the_original_guest_can_continue_after_its_snapshot() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-resume");
    let r = boot_and_snapshot(&s, "resume");
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("generation=0"), "{r}");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    let clone = restore(&s.snapshot());
    assert!(clone.stdout.contains("generation=1"), "{clone}");
    assert!(clone.stdout.contains("SHARDS-TEST PASS"), "{clone}");
}

#[test]
fn damaged_or_missing_snapshots_are_refused() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-damaged");
    let missing = restore(&s.snapshot());
    assert_eq!(missing.status, Some(1), "{missing}");
    assert!(missing.stderr.contains("state"), "{missing}");

    assert_eq!(boot_and_snapshot(&s, "stop").status, Some(0));
    let state = s.snapshot().join("state");
    let mut bytes = std::fs::read(&state).unwrap();
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&state, &bytes).unwrap();
    let truncated = restore(&s.snapshot());
    assert_eq!(truncated.status, Some(1), "{truncated}");
    assert!(truncated.stderr.contains("malformed snapshot"), "{truncated}");
}
