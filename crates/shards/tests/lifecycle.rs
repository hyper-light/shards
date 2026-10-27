//! A machine's lifetime through the library: one dropped without being waited on stops,
//! joins its threads and destroys its VM before the memory it mapped goes (audit A04).
//! Hypervisor.framework allows one VM per process, so a VM left alive fails the next
//! start; the process's threads are counted too, on every host. The cases run in one test,
//! each machine after the last.
// Where no backend runs VMs, `Running` is uninhabited, and the test returns before it
// drops one.
#![allow(clippy::unwrap_used, clippy::panic, clippy::drop_non_drop)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TempDir, cannot_run_vms, kernel, test_guest};
use shards_vmm::vm::{self, AfterSnapshot, Config, Console, Disk, ExitReason, SnapshotPolicy};

/// The test guest in `mode`, its console discarded.
fn config(mode: &str) -> Config {
    let mut cfg = Config::new(kernel().to_path_buf(), Some(test_guest().to_path_buf()));
    cfg.cmdline = format!("console=ttyS0 quiet panic=-1 shards_test={mode}");
    cfg.console = Console::Discard;
    cfg
}

/// The process's threads: its tasks on Linux (/proc/self/task), its task's threads on macOS
/// (proc_pidinfo, PROC_PIDTASKINFO), and on Windows those a snapshot of the system's threads
/// says are its own (CreateToolhelp32Snapshot).
fn threads() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task").ok().map(Iterator::count)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_taskinfo is plain data, zeroes a valid value of it.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).ok()?;
        // SAFETY: this process's task info, written into `info`, `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        (got == size)
            .then(|| usize::try_from(info.pti_threadnum).ok())
            .flatten()
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        // SAFETY: a snapshot of the system's threads, closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: THREADENTRY32 is plain data, zeroes a valid value of it.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(size_of::<THREADENTRY32>()).ok()?;
        // SAFETY: takes no arguments.
        let me = unsafe { GetCurrentProcessId() };
        let mut count = 0;
        // SAFETY: the snapshot just taken, and an entry its size says it is.
        let mut more = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == me {
                count += 1;
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
        }
        // SAFETY: the snapshot taken above, closed once.
        unsafe { CloseHandle(snapshot) };
        Some(count)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// Starts the threads a process keeps for its life once a machine has run: on macOS the
/// real-time governor (platform `prioritize_vcpu_thread`), which the first machine's vCPUs
/// would start, past a count taken before them; elsewhere none. Asked for by a thread of its
/// own, which lets go of its watch as it ends.
fn start_process_threads() {
    std::thread::spawn(|| drop(shards_vmm::platform::prioritize_vcpu_thread()))
        .join()
        .unwrap();
}

/// Starts `cfg`, lets it run for `run`, and drops it without waiting for it. It must be
/// gone at once: no thread of it left, and room for the next VM.
fn start_and_drop(what: &str, cfg: &Config, run: Duration) {
    let before = threads();
    let (handle, running) = vm::start(cfg).unwrap_or_else(|e| panic!("{what}: {e}"));
    std::thread::sleep(run);
    let t0 = Instant::now();
    drop(running);
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "{what}: teardown took {:?}",
        t0.elapsed()
    );
    drop(handle);
    assert_eq!(threads(), before, "{what}: threads left behind");
}

#[test]
fn a_machine_dropped_unwaited_is_stopped_and_torn_down() {
    if cannot_run_vms() {
        return;
    }
    start_process_threads();
    // Plain machines: dropped while booting, and once idle.
    start_and_drop("dropped while booting", &config("idle"), Duration::ZERO);
    start_and_drop("dropped idle", &config("idle"), Duration::from_millis(300));

    // With devices, whose workers use guest memory, and a snapshot coordinator.
    let dir = TempDir::new("lifecycle");
    let disk = dir.join("disk.img");
    std::fs::write(&disk, vec![0u8; 1 << 20]).unwrap();
    let mut devices = config("idle");
    devices.disks = vec![Disk {
        path: disk,
        read_only: false,
    }];
    devices.vsock = Some(shards_vmm::vm::VsockHost::at(dir.join("v.sock")));
    devices.snapshot = Some(SnapshotPolicy {
        dir: dir.join("snapshot"),
        then: AfterSnapshot::Resume,
        working_set: false,
    });
    start_and_drop("with devices", &devices, Duration::from_millis(300));

    // A machine that failed after starting: its snapshot cannot be written, since a file
    // is where the snapshot's directory would go.
    let blocked = dir.join("blocked");
    std::fs::write(&blocked, b"").unwrap();
    let mut failing = config("resume");
    failing.snapshot = Some(SnapshotPolicy {
        dir: blocked.join("snapshot"),
        then: AfterSnapshot::Stop,
        working_set: false,
    });
    start_and_drop("failed", &failing, Duration::from_millis(300));

    // And a machine waited on after all that still runs, and ends as it should.
    let reason = vm::run(&config("resume")).unwrap();
    assert!(matches!(reason, ExitReason::PowerOff), "{reason:?}");
    assert!(!Path::new(&blocked).is_dir());
}
