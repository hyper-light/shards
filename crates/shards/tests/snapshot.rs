//! Snapshots end to end: a real guest builds state, asks for a snapshot, and every
//! restore of it, each in its own process, must continue with that state intact.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Run, cannot_run_vms, cannot_snapshot, kernel, run_shards, run_shards_in, test_guest};
use shards_testguest::fill;

const RO_SALT: u64 = 1;
const RO_BYTES: usize = 4 << 20;
const RESUMED: u32 = 2;

/// A test's directory, its read-only disk written: a test's own (`shards_testdir`), short
/// enough for the sockets made in it on any checkout (sockaddr_un: 104 bytes on macOS, 108
/// on Linux), where one under the checkout's `target/` was not.
struct Scratch(shards_testdir::TempDir);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = shards_testdir::TempDir::new(tag).unwrap();
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

fn boot_and_snapshot(s: &Scratch, then: &str) -> Run {
    boot_and_snapshot_in(&common::workspace(), &s.disk(), s, then)
}

/// Boots the snapshot guest in the working directory `dir`, with `disk` as its disk.
#[track_caller]
fn boot_and_snapshot_in(dir: &Path, disk: &str, s: &Scratch, then: &str) -> Run {
    let cmdline = format!("console=hvc0 quiet panic=-1 shards_test=snapshot shards_vda_bytes={RO_BYTES}");
    let snapshot = s.snapshot().display().to_string();
    run_shards_in(
        dir,
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
            disk,
            "--cmdline",
            &cmdline,
            "--snapshot-dir",
            &snapshot,
            "--snapshot-then",
            then,
        ],
    )
}

#[track_caller]
fn restore(dir: &Path) -> Run {
    run_shards(&["restore"], &[dir.to_str().unwrap()])
}

/// [`restore`], logging at `info`, which says what the restore dropped once set up.
#[track_caller]
fn restore_logged(dir: &Path) -> Run {
    let info = std::ffi::OsStr::new("info");
    common::run_shards_env(&["restore"], &[dir.to_str().unwrap()], &[("SHARDS_LOG", info)])
}

/// KVM's counters of a restored VM, as the Firecracker comparison reads them from debugfs
/// (`common::kvm_counters`): each counter summed over the directories listed, a file of
/// several values (a histogram) left out, and a counter not listed absent, not zero.
#[test]
fn kvm_counters_are_summed_per_name_and_absent_where_unlisted() {
    let counters = common::kvm_counters_in(
        "pf_taken 10 \nexits 3 \nhalt_poll_success_hist 0 4 9 \npf_taken 5 \nnot_a_number x \n",
    );
    assert_eq!(counters.get("pf_taken"), Some(&15));
    assert_eq!(counters.get("exits"), Some(&3));
    assert_eq!(counters.get("halt_poll_success_hist"), None);
    assert_eq!(counters.get("not_a_number"), None);
    assert_eq!(counters.get("mmio_exits"), None);
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

/// A restore records the working set of a generation that has none, and its spawner,
/// `shards restore` itself, keeps it beside the generation (no VM writes its snapshot,
/// D30); the next restore of that generation maps it ahead before its guest runs, each page
/// a stage-2 fault its guest does not take (PM M310), and its guest goes on as before.
#[test]
fn a_restore_maps_ahead_the_working_set_the_restore_before_it_recorded() {
    if cannot_run_vms() || cannot_snapshot() || !shards_vmm::vm::RESTORES_RECORD {
        return;
    }
    let s = Scratch::new("wset");
    let original = boot_and_snapshot(&s, "stop");
    assert_eq!(original.status, Some(0), "{original}");
    assert!(!shards_vmm::snapshot::has_working_set(&s.snapshot()));
    let mapped = |r: &Run| -> Option<u64> {
        r.stderr
            .lines()
            .find_map(|l| l.split_once("mapped ")?.1.split_once(" of ")?.0.parse().ok())
    };
    let first = restore_logged(&s.snapshot());
    assert_eq!(first.status, Some(0), "{first}");
    assert!(first.stdout.contains("SHARDS-TEST PASS"), "{first}");
    assert_eq!(mapped(&first), None, "nothing to map ahead yet: {first}");
    // Its writer keeps it once the VM has handed it over, as the VM ends.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !shards_vmm::snapshot::has_working_set(&s.snapshot()) {
        assert!(
            std::time::Instant::now() < deadline,
            "no working set kept after the first restore: {first}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = restore_logged(&s.snapshot());
    assert_eq!(second.status, Some(0), "{second}");
    assert!(second.stdout.contains("SHARDS-TEST PASS"), "{second}");
    let pages =
        mapped(&second).unwrap_or_else(|| panic!("the second restore mapped nothing ahead: {second}"));
    assert!(pages > 0, "{second}");
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
    let given = run_shards(&["restore"], &[dir.to_str().unwrap(), "--backing", &ours]);
    assert!(given.stdout.contains("SHARDS-TEST PASS"), "{given}");
    // Given through a link, it is the file the snapshot names, which it named resolved:
    // macOS's temporary directory, under /var, a link to /private/var, is one such path.
    #[cfg(unix)]
    {
        let link = s.0.join("link.img");
        std::os::unix::fs::symlink(s.0.join("ro.img"), &link).unwrap();
        let through = run_shards(
            &["restore"],
            &[
                dir.to_str().unwrap(),
                "--backing",
                &format!("{}:ro", link.display()),
            ],
        );
        assert!(through.stdout.contains("SHARDS-TEST PASS"), "{through}");
    }
    // Another file there is: the restore is confined to it before it reads the state,
    // which then names one it was not given.
    std::fs::copy(s.0.join("ro.img"), s.0.join("other.img")).unwrap();
    let other = format!("{}:ro", s.0.join("other.img").display());
    let refused = run_shards(&["restore"], &[dir.to_str().unwrap(), "--backing", &other]);
    assert_ne!(refused.status, Some(0), "{refused}");
    let named = format!(
        "{}:ro",
        std::fs::canonicalize(s.0.join("ro.img")).unwrap().display()
    );
    assert!(
        refused
            .stderr
            .contains(&format!("the template names files it was not given: {named}")),
        "{refused}"
    );
}

/// A directory nothing may write in, as it was again once this goes: read-only on Unix
/// (mode 0555); on Windows, writing in it denied to everyone (`icacls /deny`). Root writes
/// past a mode (CAP_DAC_OVERRIDE, capabilities(7)), as the L1 guest runs these tests
/// (boot.rs' `snapshot_tests_pass_on_a_kvm_host_of_this_architecture`, aca922771: a
/// snapshot written into a 0555 directory): on Linux, root's is a read-only bind mount of
/// it over itself as well, which refuses root too (EROFS, mount(2) `MS_RDONLY`). Either
/// way it is the directory the run was given, there and granted as it starts, which only
/// a write refuses.
struct Unwritable {
    dir: PathBuf,
    /// Whether it is mounted read-only over itself (root on Linux).
    #[cfg(target_os = "linux")]
    mounted: bool,
}

impl Unwritable {
    fn new(dir: &Path) -> Unwritable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        #[cfg(windows)]
        icacls(dir, &["/deny", "*S-1-1-0:(OI)(CI)(W)"]);
        let unwritable = Unwritable {
            dir: dir.to_path_buf(),
            #[cfg(target_os = "linux")]
            // SAFETY: geteuid(2) takes nothing and cannot fail.
            mounted: unsafe { libc::geteuid() } == 0 && read_only_over_itself(dir),
        };
        // What it is for, checked: a test given a directory it could write in proves nothing.
        let probe = dir.join("probe");
        assert!(
            std::fs::create_dir(&probe).is_err(),
            "{} is writable, by this test itself",
            dir.display()
        );
        unwritable
    }
}

impl Drop for Unwritable {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if self.mounted {
            let path = std::ffi::CString::new(self.dir.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: umount2(2) of a path, its string NUL-terminated.
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(windows)]
        icacls(&self.dir, &["/remove:d", "*S-1-1-0"]);
    }
}

/// Mounts `dir` over itself, read-only: a bind mount, then its remount read-only, as a bind
/// mount takes `MS_RDONLY` only on a remount (mount(2), "Creating a bind mount"). True once
/// done.
#[cfg(target_os = "linux")]
fn read_only_over_itself(dir: &Path) -> bool {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: mount(2) of a path over itself, its strings NUL-terminated, no data.
    let bound = unsafe {
        libc::mount(
            path.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    assert_eq!(
        bound,
        0,
        "binding {}: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    // SAFETY: as above, a remount of the bind just made.
    let read_only = unsafe {
        libc::mount(
            std::ptr::null(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            std::ptr::null(),
        )
    };
    let e = std::io::Error::last_os_error();
    if read_only != 0 {
        // SAFETY: umount2(2) of the bind just made.
        unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
    }
    assert_eq!(read_only, 0, "remounting {} read-only: {e}", dir.display());
    true
}

/// Windows' own tool for a directory's access list.
#[cfg(windows)]
fn icacls(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("icacls")
        .arg(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "icacls {} {args:?}: {status}", dir.display());
}

/// A VM that resumes after its snapshot serves its run whatever becomes of the snapshot:
/// one that cannot be written loses the template, not the run.
#[test]
fn a_snapshot_that_cannot_be_written_stops_no_resumed_run() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let s = Scratch::new("snapshot-unwritable");
    std::fs::create_dir_all(s.snapshot()).unwrap();
    let unwritable = Unwritable::new(&s.snapshot());
    let r = boot_and_snapshot(&s, "resume");
    drop(unwritable);
    assert_eq!(r.status, Some(0), "{r}");
    assert!(r.stdout.contains("generation=0"), "{r}");
    assert!(r.stdout.contains("SHARDS-TEST PASS"), "{r}");
    assert!(!shards_vmm::snapshot::exists(&s.snapshot()), "{r}");
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
    let moved = run_shards_in(&elsewhere, &["restore"], &[snapshot.as_os_str()]);
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
/// stream through vsock, which the host reaches through Unix sockets (AF_UNIX on Windows).
mod storm {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::common::{self, Vm, echo_of};
    use super::{RO_BYTES, Scratch, cannot_run_vms, cannot_snapshot, kernel, test_guest};

    /// The writable disk of a storm: 256 records of 4 KiB.
    const RW_BYTES: usize = 1 << 20;
    const ECHO_PORT: u32 = 1234;
    /// Where the host tells a storm it has stopped streaming.
    const DONE_PORT: u32 = 1235;

    /// The memory of a storm of `cpus` vCPUs, in MiB: 256 for 64 vCPUs or fewer, and as much
    /// a vCPU past that. What binds is the kernel's thread limit, an eighth of the memory free
    /// at boot in 16 KiB stacks (kernel/fork.c `set_max_threads`), which must hold its own
    /// threads, six a vCPU, and the storm's, two a vCPU (its pairs' turns and its clock's
    /// readers). Need and limit both grow linearly (PM M532: the limit by 7.9 a MiB, less 1.7 a
    /// vCPU), so the 4 MiB a vCPU that gives a 64-vCPU storm 2.9 times its threads to spare
    /// keeps at least that past it: 3.5 times at 240. 240 vCPUs in 256 MiB could not start the
    /// kernel's own threads (`-EAGAIN`), and the guest panicked at boot.
    fn storm_mib(cpus: u32) -> u32 {
        /// A storm of this many vCPUs ran whole in this many MiB, 5 times of 5 (PM M532).
        const CPUS: u32 = 64;
        const MIB: u32 = 256;
        cpus.saturating_mul(MIB).div_ceil(CPUS).max(MIB)
    }

    /// A storm (the test guest's `storm` mode) on `cpus` vCPUs, with both disks and a vsock
    /// device at `sock`, whose snapshot goes to `s.snapshot()`, then `then`.
    fn storm_args(s: &Scratch, cpus: u32, sock: &Path, then: &str) -> Vec<std::ffi::OsString> {
        let rw = s.0.join("rw.img");
        [
            "run".into(),
            "--kernel".into(),
            kernel().as_os_str().to_owned(),
            "--init".into(),
            test_guest().as_os_str().to_owned(),
            "--cpus".into(),
            cpus.to_string().into(),
            "--memory".into(),
            storm_mib(cpus).to_string().into(),
            "--disk".into(),
            s.disk().into(),
            "--disk".into(),
            rw.into_os_string(),
            "--vsock".into(),
            sock.as_os_str().to_owned(),
            "--cmdline".into(),
            format!(
                "console=hvc0 quiet panic=-1 shards_test=storm shards_vda_bytes={RO_BYTES} shards_vdb_bytes={RW_BYTES} shards_storm_ms={}",
                std::env::var("SHARDS_STORM_MS").unwrap_or_else(|_| "0".into()),
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

    /// A storm's size, which keys its waits' kinds beside their call sites (`Vm::sized`,
    /// `common::echo_of`): a storm of 64 vCPUs runs slower than one of 2 on the same host,
    /// and is judged by what storms of its own size taught (HVF at ddb88cd47: a 64-vCPU
    /// storm's stream refused past the W* 2-vCPU storms taught).
    fn size(cpus: u32) -> String {
        format!("{cpus} vCPUs")
    }

    /// Streams rounds of 4 MiB through the echo of the VM at `sock`, a guest of `size`
    /// ([`size`]), until `stop`, from when it answers, counting what comes back in
    /// `carried`; returns each round's result, a failed one saying when it began and ended
    /// from `start` (its VM's).
    ///
    /// No wall clock bounds a storm: each round's reads are a wait judged by its kind's
    /// slowest round seen ([`common::Following`]), and each wait on its guest by its next new
    /// line or byte of the stream ([`Vm::wait_exit_streamed`]), so a stream that stops fails
    /// a whole window past the slowest of its kind after its last byte, and one that goes on
    /// is waited for (D209). At the
    /// slowest rate measured, 196,608 bytes in 120 s (1,638.4 B/s: 64 vCPUs on an arm64
    /// Windows host nested under QEMU's TCG on 4 CPUs, the parity plan's step 10), a busy
    /// machine's restores carry 13 MiB, 1 MiB before its snapshot (the guest's own wait) and
    /// a whole round in each of three restores (`all_whole`), in 8,320 s (2 h 19 min); the
    /// one that goes on 12 MiB, 1 MiB before its snapshot, 8 after (the guest's) and the
    /// rest of the round they end in, in 7,680 s (2 h 8 min).
    fn stream(
        sock: PathBuf,
        size: String,
        stop: Arc<AtomicBool>,
        carried: Arc<AtomicU64>,
        start: Instant,
    ) -> std::thread::JoinHandle<Rounds> {
        std::thread::spawn(move || {
            let mut rounds = Vec::new();
            let mut salt = 0;
            while !stop.load(Ordering::Relaxed) {
                if !sock.exists() {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                salt += 1;
                let began = start.elapsed();
                let round = echo_of(&sock, ECHO_PORT, salt, 4 << 20, Some(&carried), &size)
                    .map_err(|e| format!("{e} (its round {began:?} to {:?} in)", start.elapsed()));
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
    /// the rounds, and when it said `storm checked` from `start` (its VM's).
    #[track_caller]
    fn storm_done(
        vm: &mut Vm,
        sock: &Path,
        stop: &AtomicBool,
        streamer: std::thread::JoinHandle<Rounds>,
        carried: &AtomicU64,
        start: Instant,
    ) -> (Rounds, Duration) {
        // As long as its guest gets on with its storm, saying something new or carrying the
        // stream: 64 vCPUs on a host of 4 TCG CPUs ran past two minutes (the parity plan's
        // step 10).
        let line = vm.wait_for_any_streamed(&["storm checked", "SHARDS-TEST FAIL"], Some(carried));
        let checked = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        let rounds = streamer.join().unwrap();
        assert!(line.contains("storm checked"), "{}", vm.seen.join("\n"));
        // Connected, then a byte: the guest waits for it, so it lives until the host has
        // read its OK. Past it the storm's workers have stopped and its vCPUs wait, whatever
        // their count: its end is waited on with its VM process's CPU its progress
        // (`Vm::wait_exit_following`), as its guest's power-off and its process's own end
        // are work, not a spin (HVF at e6fb55548: a restored 2-vCPU storm's end refused
        // 93 to 131 ms in, past a W* of 16.5 ms, its CPU read as its liveness alone).
        let told = common::vsock_connect(sock, DONE_PORT)
            .and_then(|mut s| s.write_all(b"\n").map_err(|e| e.to_string()));
        if let Err(e) = told {
            vm.wait_exit_streamed(None);
            panic!("telling the guest it may finish: {e}\n{}", vm.seen.join("\n"));
        }
        (rounds, checked)
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
    /// and at least `least` did. A failure says what its guest `said`.
    fn all_whole(rounds: &[Result<(), String>], least: usize, what: &str, said: &str) {
        for e in rounds.iter().filter_map(|r| r.as_ref().err()) {
            assert!(e.contains("refused"), "{what}: {e}\n--- its guest said\n{said}");
        }
        let whole = rounds.iter().filter(|r| r.is_ok()).count();
        assert!(
            whole >= least,
            "{what}: {whole} whole rounds\n--- its guest said\n{said}"
        );
    }

    /// vCPU counts from two to as many as this host and the kernel take, or
    /// `SHARDS_STORM_CPUS` alone. `SHARDS_STORM_MS` runs the storm that long before its
    /// snapshot, for its report of the workers' longest gaps (platform-measurements M45).
    /// The most a host takes is what it is held to, however slowly it runs them: 64 vCPUs
    /// on the nested arm64 Windows host's 4 TCG CPUs left woken threads unrun for 15 s
    /// before any snapshot, and lost no interrupt (M427); the guest's watchdog gives the
    /// kernel's account of a stall only once its main thread and the stream through its
    /// echo have both stood still past their own pace (the test guest's `stall::Watch`).
    fn storm_cpus() -> Vec<u32> {
        let max = shards_vmm::vm::max_vcpus().unwrap().min(common::kernel_nr_cpus());
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
            let s = Scratch::new(&format!("storm{cpus}"));
            let rw = s.0.join("rw.img");
            std::fs::write(&rw, vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let carried = Arc::new(AtomicU64::new(0));
            let start = Instant::now();
            let mut original = Vm::spawn(&storm_args(&s, cpus, &sock, "stop")).sized(size(cpus));
            let streamer = stream(sock.clone(), size(cpus), stop.clone(), carried.clone(), start);
            let code = original.wait_exit_streamed(Some(&carried));
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
                let carried = Arc::new(AtomicU64::new(0));
                let start = Instant::now();
                let mut copy = Vm::spawn(&[
                    "restore".as_ref(),
                    s.snapshot().as_os_str(),
                    "--vsock".as_ref(),
                    sock.as_os_str(),
                ])
                .sized(size(cpus));
                let streamer = stream(sock.clone(), size(cpus), stop.clone(), carried.clone(), start);
                let (rounds, checked) = storm_done(&mut copy, &sock, &stop, streamer, &carried, start);
                let code = copy.wait_exit_following();
                let what = format!("{cpus} vCPUs, restore {i} (storm checked {checked:?} in)");
                let said = copy.seen.join("\n");
                assert_eq!(code, Some(0), "{what}: {said}");
                assert!(said.contains("SHARDS-TEST PASS"), "{what}: {said}");
                assert!(said.contains("generation=1"), "{what}: {said}");
                clock_checked(&said, "after the snapshot", &what);
                all_whole(&rounds, 1, &what, &said);
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
            let s = Scratch::new(&format!("resume{cpus}"));
            std::fs::write(s.0.join("rw.img"), vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let carried = Arc::new(AtomicU64::new(0));
            let start = Instant::now();
            let mut vm = Vm::spawn(&storm_args(&s, cpus, &sock, "resume")).sized(size(cpus));
            let streamer = stream(sock.clone(), size(cpus), stop.clone(), carried.clone(), start);
            let (rounds, checked) = storm_done(&mut vm, &sock, &stop, streamer, &carried, start);
            let code = vm.wait_exit_following();
            let said = vm.seen.join("\n");
            assert_eq!(code, Some(0), "{cpus} vCPUs: {said}");
            assert!(said.contains("SHARDS-TEST PASS"), "{cpus} vCPUs: {said}");
            assert!(said.contains("generation=0"), "{cpus} vCPUs: {said}");
            // What the guest wrote as it asked for the snapshot reaches the console only
            // from a machine that goes on, as this one does.
            clock_checked(&said, "before the snapshot", &format!("{cpus} vCPUs"));
            clock_checked(&said, "after the snapshot", &format!("{cpus} vCPUs"));
            all_whole(
                &rounds,
                2,
                &format!("{cpus} vCPUs (storm checked {checked:?} in)"),
                &said,
            );
        }
    }
}
