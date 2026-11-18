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

/// A restore whose guest runs no command records its working set until its spawner says so
/// (`--record-until`), as a run's recording ends when its command answers, and hands it then
/// while its guest goes on (D25, amended 2026-11-18): told at once, it hands fewer pages
/// than a restore that records until its VM ends, whose guest wrote more after. Each set
/// goes to the test, which keeps none, so each restore of the generation records anew.
#[cfg(unix)]
#[test]
fn a_bare_restore_told_to_end_its_recording_hands_what_it_had_recorded_by_then() {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    if !shards_vmm::vm::RESTORES_RECORD {
        // Straight to stderr, as common's SKIP lines: a passing test's captured output is
        // never shown.
        let _ = writeln!(
            std::io::stderr(),
            "SKIP: this host's restores record no working set (vm::RESTORES_RECORD: KVM and WHP alone)"
        );
        return;
    }
    let s = Scratch::new("wset-until");
    let original = boot_and_snapshot(&s, "stop");
    assert_eq!(original.status, Some(0), "{original}");
    let dir = s.snapshot();
    // The set a restore hands, and its guest's console; told at once where `told`.
    let recorded = |told: bool| -> (Vec<u8>, String) {
        let (mut from_vm, to_test) = std::io::pipe().unwrap();
        let (until, tell) = std::io::pipe().unwrap();
        let (set_fd, until_fd) = (to_test.as_raw_fd(), until.as_raw_fd());
        let mut command = std::process::Command::new(common::shards_vm());
        command.arg("restore").args(["--working-set", "3"]);
        if told {
            command.args(["--record-until", "4"]);
        }
        command
            .arg(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // SAFETY: runs in the child between fork and exec, calling only fcntl(2) and dup2(2),
        // which are async-signal-safe; each end copied above 4 first, closed on exec.
        unsafe {
            command.pre_exec(move || {
                let high = |fd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5);
                let (set, until) = (high(set_fd), high(until_fd));
                if set < 0 || until < 0 || libc::dup2(set, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The word's pipe given to the restore told alone.
                if told && (libc::dup2(until, 4) < 0 || libc::fcntl(4, libc::F_SETFD, 0) < 0) {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop((to_test, until));
        // Told at once: the end of its pipe is its word.
        drop(tell);
        let mut set = Vec::new();
        from_vm.read_to_end(&mut set).unwrap();
        let mut console = String::new();
        child.stdout.take().unwrap().read_to_string(&mut console).unwrap();
        assert!(child.wait().unwrap().success(), "{console}");
        (set, console)
    };
    let (at_once, said) = recorded(true);
    assert!(said.contains("SHARDS-TEST PASS"), "its guest went on: {said}");
    let (to_its_end, said) = recorded(false);
    assert!(said.contains("SHARDS-TEST PASS"), "{said}");
    assert!(!to_its_end.is_empty(), "nothing recorded until its VM ended");
    assert!(
        at_once.len() < to_its_end.len(),
        "told at once it handed {} bytes, recorded until its VM ended {}",
        at_once.len(),
        to_its_end.len()
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
        // What it is for, checked as the run writes there first: a file, its snapshot's lock
        // (vmm `snapshot::staging`), which a directory's FILE_ADD_FILE grants (icacls' WD; a
        // subdirectory takes FILE_ADD_SUBDIRECTORY, AD: Microsoft's File Access Rights
        // Constants). Not a directory: a host whose token holds SeBackupPrivilege and
        // SeRestorePrivilege enabled makes directories past the deny. On jupiter, under kiln's
        // service token, CreateDirectoryW made a subdirectory past a deny of W, WD, AD and WD,AD
        // alike, where CreateFileW was refused by each naming WD (2026-11-18), and the hosted
        // windows-2025 runner made one past it too (CI 44189693085). SE_RESTORE_NAME grants
        // FILE_ADD_FILE and FILE_ADD_SUBDIRECTORY whatever the ACL (Microsoft's Privilege
        // Constants) to an open made for backup or restore (CreateFileW's
        // FILE_FLAG_BACKUP_SEMANTICS); CreateDirectoryW's page names no such open, so that it
        // makes one is the probe's word alone. A test given a directory it could write in proves
        // nothing.
        let probe = dir.join("probe");
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
                .is_err(),
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
/// Beside them, a machine of the most vCPUs it may have, each read back by a restore.
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
    /// What the host writes to `DONE_PORT` once it has stopped streaming: the guest's verdict
    /// waits for these bytes alone, and any other stream there fails it, naming itself
    /// (testguest linux.rs `DONE_WORD`, `host_word`).
    const DONE_WORD: &[u8] = b"SHARDS-DONE\n";

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
    /// device at `sock`, whose snapshot goes to `s.snapshot()`, then `then`. Its CPUs come up as
    /// the product's guests' do: where every CPU online at boot makes each of the kernel's text
    /// patches wait for vCPUs that have no host CPU (nested Hyper-V's WHP), its kernel brings
    /// up its boot CPU alone and the test guest the rest before its storm, as shards-init does
    /// (`vm::late_cpus_cmdline`, PM M379, D261).
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
                "console=hvc0 quiet panic=-1{} shards_test=storm shards_vda_bytes={RO_BYTES} shards_vdb_bytes={RW_BYTES} shards_storm_ms={}",
                shards_vmm::vm::late_cpus_cmdline(cpus),
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
    /// from `start` (its VM's). Each round it says is said after `who`, its storm and VM:
    /// both tests run storms of one size at once, and a stopping original's last round breaks
    /// as its VM ends, as it should (CI 43968984258: that break, `sending: Broken pipe`, was
    /// read as the resumed storm's).
    ///
    /// No wall clock bounds a storm: each round's reads are a wait judged by its kind's
    /// slowest round seen ([`common::Following`]), and each wait on its guest by its next new
    /// line or byte of the stream ([`Vm::wait_exit_streamed`]), so a stream that stops fails
    /// a whole window past the slowest of its kind after its last byte, and one that goes on
    /// is waited for (D209). At the
    /// slowest rate measured, 196,608 bytes in 120 s (1,638.4 B/s: 64 vCPUs on an arm64
    /// Windows host nested under QEMU's TCG on 4 CPUs, the parity plan's step 10), a busy
    /// machine's restores carry 13 MiB, 1 MiB before its snapshot (the guest's own wait) and
    /// a whole round in each of three restores (`all_whole`), in 8,320 s (2 h 19 min); the one
    /// that goes on 12 MiB, 1 MiB before its snapshot, 8 after (the guest's) and the rest of
    /// the round they end in, in 7,680 s (2 h 8 min).
    fn stream(
        sock: PathBuf,
        size: String,
        who: String,
        stop: Arc<AtomicBool>,
        carried: Arc<AtomicU64>,
        start: Instant,
        vm: u32,
    ) -> std::thread::JoinHandle<Rounds> {
        std::thread::spawn(move || {
            let mut rounds = Vec::new();
            let mut salt = 0;
            let mut slowest = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                if !sock.exists() {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                salt += 1;
                let began = start.elapsed();
                let before = costs::read(vm);
                let round = echo_of(&sock, ECHO_PORT, salt, 4 << 20, Some(&carried), &size)
                    .map_err(|e| format!("{e} (its round {began:?} to {:?} in)", start.elapsed()));
                let took = start.elapsed().saturating_sub(began);
                if round.as_ref().is_err_and(|e| !e.contains("refused")) || took > slowest {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{who}: round {salt}, {began:?} to {:?} in, {}: {}",
                        start.elapsed(),
                        round.as_ref().map_or_else(|e| e.as_str(), |_| "whole"),
                        costs::since(&before, &costs::read(vm))
                    );
                }
                slowest = slowest.max(took);
                if round.is_err() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                rounds.push(round);
            }
            rounds
        })
    }

    /// What a storm's round cost the threads that carry it: this test's (its stream's reader
    /// and writer) and its VM process's and their descendants', each thread's CPU and its wait
    /// on a run queue (`/proc/PID/task/TID/schedstat`, Documentation/scheduler/sched-stats.rst)
    /// and its minor and major faults (`stat` fields 10 and 12, proc(5)), its vCPUs summed: to
    /// tell a stream starved of a CPU from one waiting on its guest's faults (ROOT's batch 26,
    /// L1: a restored storm's stream still across its guest's clock check). Linux alone; empty
    /// elsewhere.
    mod costs {
        /// Each thread's counters, by process and thread: its process's part (`test` or `vm`),
        /// its name, and its CPU ns, run-queue wait ns, minor and major faults.
        pub type Costs = std::collections::BTreeMap<(u32, u32), (&'static str, String, [u64; 4])>;

        #[cfg(target_os = "linux")]
        pub fn read(vm: u32) -> Costs {
            let listed: Vec<(u32, u32)> = std::fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(|pid| {
                    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                    let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
                    Some((pid, ppid))
                })
                .collect();
            let mut tree = vec![vm];
            let mut i = 0;
            while let Some(&parent) = tree.get(i) {
                tree.extend(
                    listed
                        .iter()
                        .filter(|(_, ppid)| *ppid == parent)
                        .map(|(pid, _)| *pid),
                );
                i += 1;
            }
            let mut costs = Costs::new();
            for (part, pid) in
                std::iter::once(("test", std::process::id())).chain(tree.into_iter().map(|pid| ("vm", pid)))
            {
                for task in std::fs::read_dir(format!("/proc/{pid}/task"))
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let Some(tid) = task.file_name().to_str().and_then(|t| t.parse::<u32>().ok()) else {
                        continue;
                    };
                    let (Ok(stat), Ok(sched)) = (
                        std::fs::read_to_string(task.path().join("stat")),
                        std::fs::read_to_string(task.path().join("schedstat")),
                    ) else {
                        continue;
                    };
                    let Some((head, rest)) = stat.rsplit_once(')') else {
                        continue;
                    };
                    let name = head.split_once('(').map_or("", |(_, n)| n).to_string();
                    // Fields from the state on: minflt is field 10, majflt 12.
                    let fields: Vec<u64> = rest.split_whitespace().map(|f| f.parse().unwrap_or(0)).collect();
                    let sched: Vec<u64> = sched.split_whitespace().map(|f| f.parse().unwrap_or(0)).collect();
                    let at = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
                    costs.insert(
                        (pid, tid),
                        (
                            part,
                            name,
                            [at(&sched, 0), at(&sched, 1), at(&fields, 7), at(&fields, 9)],
                        ),
                    );
                }
            }
            costs
        }

        #[cfg(not(target_os = "linux"))]
        pub fn read(_vm: u32) -> Costs {
            Costs::new()
        }

        /// What each thread spent from `before` to `now`, those that spent nothing left out,
        /// a process's vCPUs summed: `part name: cpu C ms, queued Q ms, faults MIN+MAJ`.
        pub fn since(before: &Costs, now: &Costs) -> String {
            let mut by: std::collections::BTreeMap<String, [u64; 4]> = std::collections::BTreeMap::new();
            for (key, (part, name, n)) in now {
                let was = before.get(key).map_or([0; 4], |(_, _, b)| *b);
                let d: Vec<u64> = n.iter().zip(was).map(|(n, b)| n.saturating_sub(b)).collect();
                if d.iter().all(|&x| x == 0) {
                    continue;
                }
                let label = match name.strip_prefix("vcpu") {
                    Some(_) => format!("{part} {} vcpus", key.0),
                    None => format!("{part} {} {name}", key.0),
                };
                let sum = by.entry(label).or_default();
                for (s, d) in sum.iter_mut().zip(d) {
                    *s = s.saturating_add(d);
                }
            }
            by.iter()
                .map(|(label, [cpu, queued, minor, major])| {
                    format!(
                        "{label}: cpu {} ms, queued {} ms, faults {minor}+{major}",
                        cpu / 1_000_000,
                        queued / 1_000_000
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
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
        assert!(line.contains("storm checked"), "{}", said_with_vsock(vm));
        // Connected, then a byte: the guest waits for it, so it lives until the host has
        // read its OK. Past it the storm's workers have stopped and its vCPUs wait, whatever
        // their count: its end is waited on with its VM process's CPU its progress
        // (`Vm::wait_exit_following`), as its guest's power-off and its process's own end
        // are work, not a spin (HVF at e6fb55548: a restored 2-vCPU storm's end refused
        // 93 to 131 ms in, past a W* of 16.5 ms, its CPU read as its liveness alone).
        let told = common::vsock_connect(sock, DONE_PORT)
            .and_then(|mut s| s.write_all(DONE_WORD).map_err(|e| e.to_string()));
        if let Err(e) = told {
            vm.wait_exit_streamed(None);
            panic!("telling the guest it may finish: {e}\n{}", said_with_vsock(vm));
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

    /// A storm's VM process logs at info, so that its vsock muxer says each host connection's
    /// ports as it takes it ([`said_with_vsock`]); its wait counts any new line as progress,
    /// and a connect line comes only as the host begins a round, so none masks a stalled one.
    const VSOCK_LOGGED: &[(&str, &str)] = &[("SHARDS_LOG", "info")];

    /// What `vm`'s guest said, and below it its VM process's own `vsock:` lines, every one it
    /// printed whether read or not: a failure then names the stream its muxer took and the
    /// port it gave it (ROOT, 2026-11-18), which the harness, keeping no failing test's
    /// directory, would otherwise lose.
    fn said_with_vsock(vm: &Vm) -> String {
        let vsock: Vec<String> = vm
            .log()
            .lock()
            .map(|log| {
                log.iter()
                    .filter(|line| line.contains("vsock:"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        format!(
            "{}\n--- its VM process's vsock lines\n{}",
            vm.seen.join("\n"),
            vsock.join("\n")
        )
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

    /// What a storm's largest size shows (D14).
    #[derive(Debug, Clone, Copy)]
    enum Largest {
        /// This host's CPUs: the most it runs at once, every vCPU on a CPU of its own. The
        /// bounds a snapshot and its hypervisor keep of each vCPU, which only a restore reads
        /// back and only the most a machine may have crosses (D13: every 255-vCPU snapshot was
        /// refused as it restored, "sequence of 255 (limit 254)", found by this storm), are
        /// [`a_restore_reads_back_every_vcpu_a_machine_may_have`]'s, with no storm's work on
        /// each vCPU: a restored storm of 255 vCPUs on a host of 4 CPUs took 1,782 s.
        Host,
        /// One vCPU more than this host has CPUs: the least it cannot run at once, so the
        /// barrier kicks a vCPU its host is not running.
        Overcommitted,
    }

    /// A storm's sizes in vCPUs, each a regime its claim needs, none past `most`, the most a
    /// machine may have here: two, the least whose CPUs wake one another (the guest's pairs);
    /// this host's `host` CPUs, or `most` where it has more, the most it runs at once, every
    /// vCPU on a CPU of its own; and `largest`.
    fn sizes(host: u32, most: u32, largest: Largest) -> Vec<u32> {
        let largest = match largest {
            Largest::Host => host,
            Largest::Overcommitted => host.saturating_add(1),
        };
        let mut cpus: Vec<u32> = [2, host.min(most), largest]
            .into_iter()
            .filter(|n| (2..=most).contains(n))
            .collect();
        cpus.sort_unstable();
        cpus.dedup();
        cpus
    }

    /// A storm's sizes on this host ([`sizes`]), or `SHARDS_STORM_CPUS` alone: the host's CPUs
    /// those this test may run on, as its VM processes inherit them (its affinity, and on Linux
    /// its cgroup's quota: `available_parallelism`), and the most vCPUs those its hypervisor
    /// and the pinned kernel both take (D13).
    /// `SHARDS_STORM_MS` runs the storm that long before its snapshot, for its report of the
    /// workers' longest gaps (platform-measurements M45). However slowly a host runs a size,
    /// its waits go on while the guest says something new or carries the stream (D209): 64
    /// vCPUs on the nested arm64 Windows host's 4 TCG CPUs left woken threads unrun for 15 s
    /// before any snapshot, and lost no interrupt (M427); the guest's watchdog gives the
    /// kernel's account of a stall only once its main thread and the stream through its echo
    /// have both stood still past their own pace (the test guest's `stall::Watch`).
    fn storm_cpus(largest: Largest) -> Vec<u32> {
        let most = most();
        if let Ok(only) = std::env::var("SHARDS_STORM_CPUS") {
            return vec![only.parse().unwrap()];
        }
        let host = std::thread::available_parallelism().unwrap().get();
        sizes(u32::try_from(host).unwrap(), most, largest)
    }

    /// The most vCPUs a machine may have here: those its hypervisor and the pinned kernel both
    /// take (D13), and its host enters (D261): where this host model has only refused that
    /// many, its known-good count ([`common::vcpus_admitted`]), as boot.rs's
    /// `boots_the_hosts_maximum_vcpu_count`, run before this binary, keeps it.
    fn most() -> u32 {
        common::vcpus_admitted(shards_vmm::vm::max_vcpus().unwrap().min(common::kernel_nr_cpus()))
    }

    /// How many restores the restores' storm takes of each size (D14): each a trial of a loss
    /// that comes in one restore of several (180 restores in 20 runs, 00519521d1).
    const RESTORES: u32 = 3;

    /// A storm's sizes are the regimes its claim needs, from its host's CPUs and the most vCPUs
    /// a machine may have: CI's x86_64 runners (4 CPUs, 255), a Mac (18, 64), a host of one
    /// CPU, and one of more CPUs than a machine may have vCPUs (an arm64 KVM host: 192, 123).
    /// Past its host's CPUs, the restores' storm takes no size: the most is
    /// [`a_restore_reads_back_every_vcpu_a_machine_may_have`]'s.
    #[test]
    fn a_storms_sizes_are_the_regimes_its_claim_needs() {
        assert_eq!(sizes(4, 255, Largest::Host), [2, 4]);
        assert_eq!(sizes(4, 255, Largest::Overcommitted), [2, 4, 5]);
        assert_eq!(sizes(18, 64, Largest::Host), [2, 18]);
        assert_eq!(sizes(18, 64, Largest::Overcommitted), [2, 18, 19]);
        assert_eq!(sizes(1, 255, Largest::Host), [2]);
        assert_eq!(sizes(1, 255, Largest::Overcommitted), [2]);
        assert_eq!(sizes(2, 255, Largest::Overcommitted), [2, 3]);
        assert_eq!(sizes(192, 123, Largest::Host), [2, 123]);
        assert_eq!(sizes(192, 123, Largest::Overcommitted), [2, 123]);
    }

    /// A snapshot of a machine busy in every way at once, its CPUs waking one another, its
    /// timers ticking, both disks mid-request and a vsock stream in both directions, restores
    /// whole (audit A02): each restore finds every activity going on, no interrupt lost, every
    /// record on the writable disk written exactly once, and the host's stream whole. Up to
    /// its host's CPUs ([`Largest::Host`]), each size restored [`RESTORES`] times.
    #[test]
    fn restores_of_a_busy_machine_lose_no_interrupt_and_repeat_no_write() {
        if cannot_run_vms() || cannot_snapshot() {
            return;
        }
        for cpus in storm_cpus(Largest::Host) {
            let s = Scratch::new(&format!("storm{cpus}"));
            let rw = s.0.join("rw.img");
            std::fs::write(&rw, vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let carried = Arc::new(AtomicU64::new(0));
            let start = Instant::now();
            let mut original =
                Vm::spawn_env(&storm_args(&s, cpus, &sock, "stop"), VSOCK_LOGGED).sized(size(cpus));
            let streamer = stream(
                sock.clone(),
                size(cpus),
                format!("storm {cpus} vCPUs, its original (it stops at its snapshot)"),
                stop.clone(),
                carried.clone(),
                start,
                original.id(),
            );
            let code = original.wait_exit_streamed(Some(&carried));
            // Its stream breaks where it stopped, at the snapshot.
            stop.store(true, Ordering::Relaxed);
            drop(streamer.join().unwrap());
            let said = said_with_vsock(&original);
            assert_eq!(code, Some(0), "{cpus} vCPUs: {said}");
            assert!(!said.contains("SHARDS-TEST FAIL"), "{cpus} vCPUs: {said}");

            // The writable disk as the snapshot left it, in the same file, for each restore.
            let taken = std::fs::read(&rw).unwrap();
            for i in 0..RESTORES {
                std::fs::write(&rw, &taken).unwrap();
                let sock = s.0.join(format!("r{i}.sock"));
                let stop = Arc::new(AtomicBool::new(false));
                let carried = Arc::new(AtomicU64::new(0));
                let start = Instant::now();
                let mut copy = Vm::spawn_env(
                    &[
                        "restore".as_ref(),
                        s.snapshot().as_os_str(),
                        "--vsock".as_ref(),
                        sock.as_os_str(),
                    ],
                    VSOCK_LOGGED,
                )
                .sized(size(cpus));
                let streamer = stream(
                    sock.clone(),
                    size(cpus),
                    format!("storm {cpus} vCPUs, restore {i}"),
                    stop.clone(),
                    carried.clone(),
                    start,
                    copy.id(),
                );
                let (rounds, checked) = storm_done(&mut copy, &sock, &stop, streamer, &carried, start);
                let code = copy.wait_exit_following();
                let what = format!("{cpus} vCPUs, restore {i} (storm checked {checked:?} in)");
                let said = said_with_vsock(&copy);
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
    /// the snapshot, loses and repeats nothing. Up to one vCPU more than its host has CPUs
    /// ([`Largest::Overcommitted`]): what a resume adds to a snapshot, the barrier letting its
    /// vCPUs go on, holds no bound of the machine's, whose largest snapshot
    /// [`a_restore_reads_back_every_vcpu_a_machine_may_have`] takes and reads back.
    #[test]
    fn a_busy_machine_goes_on_whole_past_its_snapshot() {
        if cannot_run_vms() || cannot_snapshot() {
            return;
        }
        for cpus in storm_cpus(Largest::Overcommitted) {
            let s = Scratch::new(&format!("resume{cpus}"));
            std::fs::write(s.0.join("rw.img"), vec![0u8; RW_BYTES]).unwrap();
            let sock = s.0.join("v.sock");
            let stop = Arc::new(AtomicBool::new(false));
            let carried = Arc::new(AtomicU64::new(0));
            let start = Instant::now();
            let mut vm =
                Vm::spawn_env(&storm_args(&s, cpus, &sock, "resume"), VSOCK_LOGGED).sized(size(cpus));
            let streamer = stream(
                sock.clone(),
                size(cpus),
                format!("resume {cpus} vCPUs"),
                stop.clone(),
                carried.clone(),
                start,
                vm.id(),
            );
            let (rounds, checked) = storm_done(&mut vm, &sock, &stop, streamer, &carried, start);
            let code = vm.wait_exit_following();
            let said = said_with_vsock(&vm);
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

    /// A machine of `cpus` vCPUs in the test guest's `vcpus` mode, whose snapshot goes to
    /// `s.snapshot()`, then stops. Its memory is a storm's of its size ([`storm_mib`]): the
    /// kernel's threads and one of the guest's a vCPU, fewer than a storm's two. Its CPUs come
    /// up as a storm's do ([`storm_args`]).
    fn every_vcpu_args(s: &Scratch, cpus: u32) -> Vec<std::ffi::OsString> {
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
            "--cmdline".into(),
            format!(
                "console=hvc0 quiet panic=-1{} shards_test=vcpus",
                shards_vmm::vm::late_cpus_cmdline(cpus)
            )
            .into(),
            "--snapshot-dir".into(),
            s.snapshot().into_os_string(),
            "--snapshot-then".into(),
            "stop".into(),
        ]
        .into()
    }

    /// Every vCPU a machine may have here ([`most`]) read back by a restore (D13, D14): the
    /// bounds a snapshot and its hypervisor keep of each vCPU are crossed only at the most
    /// ("sequence of 255 (limit 254)" refused every 255-vCPU restore), which every restore
    /// shows or none does, so one restore shows it. In the restore each vCPU runs a thread of
    /// its own, its clock read in turn with every other's (the test guest's `vcpus`), and none
    /// carries a storm's work: past its host's CPUs, that work is what a restore of the most
    /// waited on, 1,782 s on a host of 4 CPUs, and the storms keep it to the sizes their
    /// claims need ([`Largest`]).
    #[test]
    fn a_restore_reads_back_every_vcpu_a_machine_may_have() {
        if cannot_run_vms() || cannot_snapshot() {
            return;
        }
        let cpus = most();
        let s = Scratch::new(&format!("vcpus{cpus}"));
        let mut original = Vm::spawn(&every_vcpu_args(&s, cpus)).sized(size(cpus));
        let code = original.wait_exit_streamed(None);
        let said = original.seen.join("\n");
        assert_eq!(code, Some(0), "{cpus} vCPUs: {said}");
        assert!(!said.contains("SHARDS-TEST FAIL"), "{cpus} vCPUs: {said}");

        let mut copy = Vm::spawn(&["restore".as_ref(), s.snapshot().as_os_str()]).sized(size(cpus));
        let code = copy.wait_exit_streamed(None);
        let what = format!("{cpus} vCPUs, restored");
        let said = copy.seen.join("\n");
        assert_eq!(code, Some(0), "{what}: {said}");
        assert!(said.contains("SHARDS-TEST PASS"), "{what}: {said}");
        assert!(said.contains("generation=1"), "{what}: {said}");
        assert!(
            said.contains(&format!("in turn on {cpus} CPUs")),
            "{what}: {said}"
        );
        clock_checked(&said, "after the snapshot", &what);
    }
}
