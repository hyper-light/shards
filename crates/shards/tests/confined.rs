//! The VM process on Windows, as D141 confines it (PM M200-M203), read from the running
//! process as Windows reports it: in an AppContainer of its own, in a Job that ends it with
//! its spawner and lets it start nothing, with Windows' mitigations, and forbidden dynamic
//! code once its partition is set up; its profile deleted by the next spawner where its
//! own was killed.

#![cfg(windows)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{Vm, cannot_run_vms, kernel, test_guest, vm_run};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile};
use windows_sys::Win32::Security::{FreeSid, GetTokenInformation, PSID, TOKEN_QUERY, TokenIsAppContainer};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{
    GetProcessMitigationPolicy, OpenProcess, OpenProcessToken, PROCESS_MITIGATION_POLICY,
    PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, ProcessChildProcessPolicy, ProcessDynamicCodePolicy,
    ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, ProcessStrictHandleCheckPolicy,
    ProcessSystemCallDisablePolicy, WaitForSingleObject,
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A handle, closed as it drops.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this value owns.
        unsafe { CloseHandle(self.0) };
    }
}

/// The ID of the VM process `shards` (`parent`) started: shards-vm, its child.
fn vm_process(parent: u32) -> u32 {
    let found = common::vm_processes(parent);
    assert_eq!(found.len(), 1, "shards {parent}'s VM processes: {found:?}");
    found[0]
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A VM process runs in an AppContainer, in a Job, under every mitigation shards asks of
/// Windows, dynamic code forbidden once its partition is set up; it ends as its spawner is
/// killed; and the profile its killed spawner left is deleted by the next.
#[test]
fn the_vm_process_is_confined() {
    if cannot_run_vms() {
        return;
    }
    let (k, g) = (kernel(), test_guest());
    let mut vm = Vm::spawn(&[
        "run".as_ref(),
        "--kernel".as_ref(),
        k.as_os_str(),
        "--init".as_ref(),
        g.as_os_str(),
        "--memory".as_ref(),
        "256".as_ref(),
        "--cmdline".as_ref(),
        "console=ttyS0 quiet shards_test=idle".as_ref(),
    ]);
    vm.wait_for("SHARDS-TEST READY", TIMEOUT);
    let cli = vm.id();
    let pid = vm_process(cli);
    // SAFETY: plain arguments; the handle closes as it drops.
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) });
    assert!(!process.0.is_null(), "{}", std::io::Error::last_os_error());

    let mut in_job = 0;
    // SAFETY: the process, and a BOOL for the answer.
    let asked = unsafe { IsProcessInJob(process.0, std::ptr::null_mut(), &raw mut in_job) };
    assert_ne!(asked, 0, "{}", std::io::Error::last_os_error());
    assert_eq!(in_job, 1, "in a Job");

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the process's token, for querying, closed as it drops.
    let opened = unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &raw mut token) };
    assert_ne!(opened, 0, "{}", std::io::Error::last_os_error());
    let token = Handle(token);
    let (mut is, mut len) = (0u32, 0u32);
    // SAFETY: a DWORD of its own size for the answer.
    let read = unsafe {
        GetTokenInformation(
            token.0,
            TokenIsAppContainer,
            (&raw mut is).cast(),
            4,
            &raw mut len,
        )
    };
    assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
    assert_eq!(is, 1, "in an AppContainer");

    // Each policy's flags, its first word.
    let flags = |policy: PROCESS_MITIGATION_POLICY| -> u32 {
        let mut word = 0u32;
        // SAFETY: a DWORD of its own size for the policy's flags.
        let ok = unsafe { GetProcessMitigationPolicy(process.0, policy, (&raw mut word).cast(), 4) };
        assert_ne!(ok, 0, "policy {policy}: {}", std::io::Error::last_os_error());
        word
    };
    for (policy, bits, what) in [
        (
            ProcessDynamicCodePolicy,
            0b1,
            "no dynamic code, once its partition is set up",
        ),
        (
            ProcessStrictHandleCheckPolicy,
            0b11,
            "strict handle checks, for good",
        ),
        (ProcessSystemCallDisablePolicy, 0b1, "no Win32k system calls"),
        (ProcessExtensionPointDisablePolicy, 0b1, "no extension points"),
        (
            ProcessImageLoadPolicy,
            0b11,
            "no images from remote devices or of a low label",
        ),
        (ProcessChildProcessPolicy, 0b1, "no child processes"),
    ] {
        assert_eq!(flags(policy) & bits, bits, "{what}");
    }

    // Its spawner killed, it ends: the Job's last handle was the spawner's.
    vm.kill();
    // SAFETY: the process's handle.
    let waited = unsafe { WaitForSingleObject(process.0, 10_000) };
    assert_eq!(waited, WAIT_OBJECT_0, "the VM process outlived its spawner");

    // The killed spawner left its profile and its record; the next spawner sweeps both,
    // whatever its own VM does.
    let name = format!("shards.vm.{cli}.0");
    let record = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join("shards")
        .join("containers")
        .join(&name);
    assert!(record.exists(), "{} left", record.display());
    let next = vm_run(&["--kernel", "nonexistent-kernel"], TIMEOUT);
    assert_ne!(next.status, Some(0), "{next}");
    assert!(!record.exists(), "{} swept", record.display());
    let profile = wide(&name);
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: names we own; the profile made is deleted and its SID freed below.
    let hr = unsafe {
        CreateAppContainerProfile(
            profile.as_ptr(),
            profile.as_ptr(),
            profile.as_ptr(),
            std::ptr::null(),
            0,
            &raw mut sid,
        )
    };
    assert!(hr >= 0, "the profile is gone, and made again: {hr:#010x}");
    // SAFETY: the profile and SID just made.
    unsafe {
        DeleteAppContainerProfile(profile.as_ptr());
        FreeSid(sid);
    }
}

/// Two vCPUs booted in this process to their end, their console written to this process's
/// output as a VM process writes its own: the control that tells the console's path apart
/// from the confinement where the confined SMP boots stall (boot.rs's in-process boot
/// discards its console).
#[test]
fn two_vcpus_boot_to_their_end_with_their_console() {
    use shards_vmm::vm::{self, Config, Console, ExitReason};
    if cannot_run_vms() {
        return;
    }
    let mut cfg = Config::new(kernel().to_path_buf(), None);
    cfg.vcpus = 2;
    cfg.memory_mib = 512;
    cfg.cmdline = "console=ttyS0 panic=-1".into();
    cfg.console = Console::Stdout;
    let (handle, running) = vm::start(&cfg).unwrap_or_else(|e| panic!("{e}"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(reason) => assert_eq!(reason, ExitReason::Reset),
        Err(_) => {
            handle.stop();
            panic!("two vCPUs with their console did not reach their end in {TIMEOUT:?} in this process");
        }
    }
}
