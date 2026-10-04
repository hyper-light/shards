//! Snapshots end to end: a real guest builds state, asks for a snapshot, and every
//! restore of it, each in its own process, must continue with that state intact.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Run, cannot_run_vms, cannot_snapshot, kernel, run_shards, run_shards_in, test_guest};
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
    boot_and_snapshot_in(&common::workspace(), &s.disk(), s, then)
}

/// Boots the snapshot guest in the working directory `dir`, with `disk` as its disk.
fn boot_and_snapshot_in(dir: &Path, disk: &str, s: &Scratch, then: &str) -> Run {
    let cmdline = format!("console=ttyS0 quiet panic=-1 shards_test=snapshot shards_vda_bytes={RO_BYTES}");
    let snapshot = s.snapshot().display().to_string();
    run_shards_in(
        dir,
        &["vm", "run"],
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
            disk,
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

/// [`restore`], logging at `info`, which says what the restore dropped once set up.
fn restore_logged(dir: &Path) -> Run {
    let info = std::ffi::OsStr::new("info");
    common::run_shards_env(
        &["vm", "restore"],
        &[dir.to_str().unwrap()],
        &[("SHARDS_LOG", info)],
        TIMEOUT,
    )
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
    assert!(
        common::snapshot_file(&s.snapshot(), "state").is_file(),
        "{original}"
    );
    assert!(
        common::snapshot_file(&s.snapshot(), "memory").is_file(),
        "{original}"
    );

    let mut randoms = Vec::new();
    for i in 0..3 {
        let r = restore_logged(&s.snapshot());
        assert_eq!(r.status, Some(0), "restore {i}: {r}");
        // What only setting the vCPUs up needs goes before the guest runs (audit D03).
        assert!(
            r.stderr.contains("dropped the restore's state"),
            "restore {i}: {r}"
        );
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

/// A restore given the files its snapshot restores against (`--backing`) refuses a
/// snapshot that names another: what a VM process wrote does not choose what the next
/// one is given.
#[test]
fn a_restore_given_its_files_refuses_a_snapshot_naming_others() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-backing");
    let r = boot_and_snapshot(&s, "stop");
    assert_eq!(r.status, Some(0), "{r}");
    let dir = s.snapshot();
    let ours = s.disk();
    let given = run_shards(
        &["vm", "restore"],
        &[dir.to_str().unwrap(), "--backing", &ours],
        TIMEOUT,
    );
    assert!(given.stdout.contains("SHARDS-TEST PASS"), "{given}");
    // Another file there is: the restore is confined to it before it reads the state,
    // which then names one it was not given.
    std::fs::copy(s.0.join("ro.img"), s.0.join("other.img")).unwrap();
    let other = format!("{}:ro", s.0.join("other.img").display());
    let refused = run_shards(
        &["vm", "restore"],
        &[dir.to_str().unwrap(), "--backing", &other],
        TIMEOUT,
    );
    assert_ne!(refused.status, Some(0), "{refused}");
    assert!(
        refused
            .stderr
            .contains(&format!("the template names files it was not given: {ours}")),
        "{refused}"
    );
}

/// A VM that resumes after its snapshot serves its run whatever becomes of the snapshot:
/// one that cannot be written loses the template, not the run.
#[cfg(unix)]
#[test]
fn a_snapshot_that_cannot_be_written_stops_no_resumed_run() {
    use std::os::unix::fs::PermissionsExt as _;
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-unwritable");
    std::fs::create_dir_all(s.snapshot()).unwrap();
    std::fs::set_permissions(s.snapshot(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = boot_and_snapshot(&s, "resume");
    std::fs::set_permissions(s.snapshot(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("generation=0"), "{r}");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
}

#[test]
fn damaged_or_missing_snapshots_are_refused() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-damaged");
    let missing = restore(&s.snapshot());
    assert_eq!(missing.status, Some(1), "{missing}");
    assert!(missing.stderr.contains("no snapshot here"), "{missing}");

    assert_eq!(boot_and_snapshot(&s, "stop").status, Some(0));
    let state = common::snapshot_file(&s.snapshot(), "state");
    let mut bytes = std::fs::read(&state).unwrap();
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&state, &bytes).unwrap();
    let truncated = restore(&s.snapshot());
    assert_eq!(truncated.status, Some(1), "{truncated}");
    assert!(truncated.stderr.contains("malformed snapshot"), "{truncated}");
}

/// A snapshot names its disks by absolute path: a restore from another directory, where a
/// file of the same name holds other bytes, reads the disk the snapshot was taken with,
/// and a restore refuses a disk that is no longer that file (audit A18).
#[test]
fn restores_elsewhere_read_the_disks_the_snapshot_was_taken_with() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-elsewhere");
    let elsewhere = s.0.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("ro.img"), vec![0u8; RO_BYTES]).unwrap();
    let original = boot_and_snapshot_in(&s.0, "ro.img:ro", &s, "stop");
    assert_eq!(original.status, Some(0), "{original}");

    let snapshot = s.snapshot();
    let moved = run_shards_in(&elsewhere, &["vm", "restore"], &[snapshot.as_os_str()], TIMEOUT);
    assert_eq!(moved.status, Some(0), "{moved}");
    assert!(moved.stdout.contains("SHARDS-TEST PASS"), "{moved}");

    // The same bytes, in another file renamed over the disk.
    std::fs::copy(s.0.join("ro.img"), s.0.join("ro.new")).unwrap();
    std::fs::rename(s.0.join("ro.new"), s.0.join("ro.img")).unwrap();
    let replaced = restore(&snapshot);
    assert_eq!(replaced.status, Some(1), "{replaced}");
    assert!(
        replaced
            .stderr
            .contains("ro.img: not the file this snapshot was taken with"),
        "{replaced}"
    );
}

/// Storms: machines busy in every way at once when they are snapshotted (audit A02). They
/// stream through vsock, which the host reaches through Unix sockets.
#[cfg(unix)]
mod storm {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::common::{self, Vm, echo};
    use super::{RO_BYTES, Scratch, TIMEOUT, cannot_run_vms, cannot_snapshot, kernel, test_guest};

    /// The writable disk of a storm: 256 records of 4 KiB.
    const RW_BYTES: usize = 1 << 20;
    const ECHO_PORT: u32 = 1234;
    /// Where the host tells a storm it has stopped streaming.
    const DONE_PORT: u32 = 1235;

    /// A storm (the test guest's `storm` mode) on `cpus` vCPUs, with both disks and a vsock
    /// device at `sock`, whose snapshot goes to `s.snapshot()`, then `then`.
    fn storm_args(s: &Scratch, cpus: u32, sock: &Path, then: &str) -> Vec<std::ffi::OsString> {
        let rw = s.0.join("rw.img");
        [
            "vm".into(),
            "run".into(),
            "--kernel".into(),
            kernel().as_os_str().to_owned(),
            "--init".into(),
            test_guest().as_os_str().to_owned(),
            "--cpus".into(),
            cpus.to_string().into(),
            "--memory".into(),
            "256".into(),
            "--disk".into(),
            s.disk().into(),
            "--disk".into(),
            rw.into_os_string(),
            "--vsock".into(),
            sock.as_os_str().to_owned(),
            "--cmdline".into(),
            format!(
                "console=ttyS0 quiet panic=-1 shards_test=storm shards_vda_bytes={RO_BYTES} shards_vdb_bytes={RW_BYTES} shards_storm_ms={}",
                std::env::var("SHARDS_STORM_MS").unwrap_or_else(|_| "0".into())
            )
            .into(),
            "--snapshot-dir".into(),
            s.snapshot().into_os_string(),
            "--snapshot-then".into(),
            then.into(),
        ]
        .into()
    }

    type Rounds = Vec<Result<(), String>>;

    /// Streams rounds of 4 MiB through the echo of the VM at `sock` until `stop`, from when
    /// it answers; returns each round's result.
    fn stream(sock: PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Rounds> {
        std::thread::spawn(move || {
            let mut rounds = Vec::new();
            let mut salt = 0;
            while !stop.load(Ordering::Relaxed) {
                if !sock.exists() {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                salt += 1;
                let round = echo(&sock, ECHO_PORT, salt, 4 << 20);
                if round.is_err() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                rounds.push(round);
            }
            rounds
        })
    }

    /// Lets a storm that has done its checks (`storm checked`) give its verdict once the host
    /// has stopped streaming through it, so every round ends with the guest alive. Returns
    /// the rounds.
    fn storm_done(
        vm: &mut Vm,
        sock: &Path,
        stop: &AtomicBool,
        streamer: std::thread::JoinHandle<Rounds>,
    ) -> Rounds {
        let line = vm.wait_for_any(&["storm checked", "SHARDS-TEST FAIL"], TIMEOUT);
        stop.store(true, Ordering::Relaxed);
        let rounds = streamer.join().unwrap();
        assert!(line.contains("storm checked"), "{}", vm.seen.join("\n"));
        // Connected, then a byte: the guest waits for it, so it lives until the host has
        // read its OK.
        let told = common::vsock_connect(sock, DONE_PORT, TIMEOUT)
            .and_then(|mut s| s.write_all(b"\n").map_err(|e| e.to_string()));
        if let Err(e) = told {
            vm.wait_exit_within(TIMEOUT);
            panic!("telling the guest it may finish: {e}\n{}", vm.seen.join("\n"));
        }
        rounds
    }

    /// The guest compared its clock across all its CPUs `when`, many times: a check that
    /// never ran, or barely did, proves nothing. Whether it found the clock in step is the
    /// guest's own verdict.
    fn clock_checked(said: &str, when: &str, what: &str) {
        let reads: u64 = said
            .lines()
            .find_map(|l| l.split(&format!("clock {when}: ")).nth(1))
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{what}: no clock check {when}:\n{said}"));
        assert!(reads >= 100, "{what}: {reads} clock reads {when}:\n{said}");
    }

    /// Every round came back whole, but for connections refused before the guest listened,
    /// and at least `least` did.
    fn all_whole(rounds: &[Result<(), String>], least: usize, what: &str) {
        for e in rounds.iter().filter_map(|r| r.as_ref().err()) {
            assert!(e.contains("refused"), "{what}: {e}");
        }
        let whole = rounds.iter().filter(|r| r.is_ok()).count();
        assert!(whole >= least, "{what}: {whole} whole rounds");
    }

    /// vCPU counts from two to as many as this host and the kernel take, or
    /// `SHARDS_STORM_CPUS` alone. `SHARDS_STORM_MS` runs the storm that long before its
    /// snapshot, for its report of the workers' longest gaps (platform-measurements M45).
    fn storm_cpus() -> Vec<u32> {
        let max = shards_vmm::vm::max_vcpus().unwrap().min(common::KERNEL_NR_CPUS);
        if let Ok(only) = std::env::var("SHARDS_STORM_CPUS") {
            return vec![only.parse().unwrap()];
        }
        let mut cpus: Vec<u32> = [2, 8, max].into_iter().filter(|&n| n <= max).collect();
        cpus.dedup();
        cpus
    }

    /// A snapshot of a machine busy in every way at once, its CPUs waking one another, its
    /// timers ticking, both disks mid-request and a vsock stream in both directions, restores
    /// whole (audit A02): each restore finds every activity going on, no interrupt lost, every
    /// record on the writable disk written exactly once, and the host's stream whole.
    #[test]
    fn restores_of_a_busy_machine_lose_no_interrupt_and_repeat_no_write() {
        if cannot_run_vms() || cannot_snapshot() {
            return;
        }
        for cpus in storm_cpus() {
            let s = Scratch::new(&format!("storm-{cpus}"));
            let rw = s.0.join("rw.img");
            std::fs::write(&rw, vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let streamer = stream(sock.clone(), stop.clone());
            let mut original = Vm::spawn(&storm_args(&s, cpus, &sock, "stop"));
            let code = original.wait_exit_within(TIMEOUT);
            // Its stream breaks where it stopped, at the snapshot.
            stop.store(true, Ordering::Relaxed);
            drop(streamer.join().unwrap());
            let said = original.seen.join("\n");
            assert_eq!(code, Some(0), "{cpus} vCPUs: {said}");
            assert!(!said.contains("SHARDS-TEST FAIL"), "{cpus} vCPUs: {said}");

            // The writable disk as the snapshot left it, in the same file, for each restore.
            let taken = std::fs::read(&rw).unwrap();
            for i in 0..3 {
                std::fs::write(&rw, &taken).unwrap();
                let sock = s.0.join(format!("r{i}.sock"));
                let stop = Arc::new(AtomicBool::new(false));
                let streamer = stream(sock.clone(), stop.clone());
                let mut copy = Vm::spawn(&[
                    "vm".as_ref(),
                    "restore".as_ref(),
                    s.snapshot().as_os_str(),
                    "--vsock".as_ref(),
                    sock.as_os_str(),
                ]);
                let rounds = storm_done(&mut copy, &sock, &stop, streamer);
                let code = copy.wait_exit_within(TIMEOUT);
                let what = format!("{cpus} vCPUs, restore {i}");
                let said = copy.seen.join("\n");
                assert_eq!(code, Some(0), "{what}: {said}");
                assert!(said.contains("SHARDS-TEST PASS"), "{what}: {said}");
                assert!(said.contains("generation=1"), "{what}: {said}");
                clock_checked(&said, "after the snapshot", &what);
                all_whole(&rounds, 1, &what);
            }
        }
    }

    /// A machine busy in every way at once that resumes after its snapshot goes on whole
    /// (audit A02): every activity continues, and the host's vsock stream, running through
    /// the snapshot, loses and repeats nothing.
    #[test]
    fn a_busy_machine_goes_on_whole_past_its_snapshot() {
        if cannot_run_vms() || cannot_snapshot() {
            return;
        }
        for cpus in storm_cpus() {
            let s = Scratch::new(&format!("storm-resume-{cpus}"));
            std::fs::write(s.0.join("rw.img"), vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let streamer = stream(sock.clone(), stop.clone());
            let mut vm = Vm::spawn(&storm_args(&s, cpus, &sock, "resume"));
            let rounds = storm_done(&mut vm, &sock, &stop, streamer);
            let code = vm.wait_exit_within(TIMEOUT);
            let said = vm.seen.join("\n");
            assert_eq!(code, Some(0), "{cpus} vCPUs: {said}");
            assert!(said.contains("SHARDS-TEST PASS"), "{cpus} vCPUs: {said}");
            assert!(said.contains("generation=0"), "{cpus} vCPUs: {said}");
            // What the guest wrote as it asked for the snapshot reaches the console only
            // from a machine that goes on, as this one does.
            clock_checked(&said, "before the snapshot", &format!("{cpus} vCPUs"));
            clock_checked(&said, "after the snapshot", &format!("{cpus} vCPUs"));
            all_whole(&rounds, 2, &format!("{cpus} vCPUs"));
        }
    }
}
