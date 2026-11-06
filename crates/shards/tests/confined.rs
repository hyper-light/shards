//! The VM process on Windows, as D141 confines it (PM M200-M203), read from the running
//! process as Windows reports it: in an AppContainer of its own, in a Job that ends it with
//! its spawner and lets it start nothing, with Windows' mitigations, and forbidden dynamic
//! code once its partition is set up; its profile deleted by the next spawner where its
//! own was killed. And a VM's network process, as D142 confines it (PM M204): in an
//! AppContainer of its own that holds the network's capabilities alone, in a Job, with the
//! same mitigations.

#![cfg(windows)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use common::{TempDir, Vm, cannot_run_vms, guest_init, kernel, run_shards_env, served, test_guest, vm_run};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile};
use windows_sys::Win32::Security::{
    FreeSid, GetTokenInformation, PSID, SID_AND_ATTRIBUTES, TOKEN_GROUPS, TOKEN_QUERY, TokenCapabilities,
    TokenIsAppContainer,
};
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

/// Process `pid`, opened to be asked about and waited for.
fn open(pid: u32) -> Handle {
    // SAFETY: plain arguments; the handle closes as it drops.
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) });
    assert!(!process.0.is_null(), "{}", std::io::Error::last_os_error());
    process
}

/// Whether `process` runs in a Job.
fn in_job(process: &Handle) -> bool {
    let mut in_job = 0;
    // SAFETY: the process, and a BOOL for the answer.
    let asked = unsafe { IsProcessInJob(process.0, std::ptr::null_mut(), &raw mut in_job) };
    assert_ne!(asked, 0, "{}", std::io::Error::last_os_error());
    in_job == 1
}

/// `process`'s token, to be asked about.
fn token(process: &Handle) -> Handle {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the process's token, for querying, closed as it drops.
    let opened = unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &raw mut token) };
    assert_ne!(opened, 0, "{}", std::io::Error::last_os_error());
    Handle(token)
}

/// Whether `token` is an AppContainer's.
fn is_app_container(token: &Handle) -> bool {
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
    is == 1
}

/// The capabilities `token` holds, each SID as its string says it (`S-1-15-3-…`).
fn capabilities(token: &Handle) -> Vec<String> {
    let mut len = 0u32;
    // SAFETY: the size query, which fails and fills `len`.
    unsafe { GetTokenInformation(token.0, TokenCapabilities, std::ptr::null_mut(), 0, &raw mut len) };
    // Aligned for TOKEN_GROUPS and the SIDs it points into.
    let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: a buffer of at least `len` bytes for the call to fill.
    let read = unsafe {
        GetTokenInformation(
            token.0,
            TokenCapabilities,
            buffer.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
    let groups = buffer.as_ptr().cast::<TOKEN_GROUPS>();
    // SAFETY: the TOKEN_GROUPS the call wrote at the buffer's start, `GroupCount` entries of
    // it within the buffer.
    let held = unsafe {
        std::slice::from_raw_parts(
            (&raw const (*groups).Groups).cast::<SID_AND_ATTRIBUTES>(),
            (*groups).GroupCount as usize,
        )
    };
    held.iter().map(|g| sid_text(g.Sid)).collect()
}

/// `sid` as its string says it.
fn sid_text(sid: PSID) -> String {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a SID the caller holds; a pointer for the string the call allocates.
    let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut text) };
    assert_ne!(converted, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the NUL-terminated string the call wrote.
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    // SAFETY: its `len` characters.
    let said = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: what the call allocated, freed once.
    unsafe { LocalFree(text.cast()) };
    said
}

/// The first word of `process`'s `policy`'s flags.
fn mitigation(process: &Handle, policy: PROCESS_MITIGATION_POLICY) -> u32 {
    let mut word = 0u32;
    // SAFETY: a DWORD of its own size for the policy's flags.
    let ok = unsafe { GetProcessMitigationPolicy(process.0, policy, (&raw mut word).cast(), 4) };
    assert_ne!(ok, 0, "policy {policy}: {}", std::io::Error::last_os_error());
    word
}

/// The mitigations each confined process of shards' is made with (shards_ipc's
/// `MITIGATIONS` and its child process policy): each policy, the bits it has set, and what
/// they are.
const MADE_WITH: [(PROCESS_MITIGATION_POLICY, u32, &str); 5] = [
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
];

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
    let process = open(vm_process(cli));
    assert!(in_job(&process), "in a Job");
    assert!(is_app_container(&token(&process)), "in an AppContainer");
    for (policy, bits, what) in [(
        ProcessDynamicCodePolicy,
        0b1,
        "no dynamic code, once its partition is set up",
    )]
    .into_iter()
    .chain(MADE_WITH)
    {
        assert_eq!(mitigation(&process, policy) & bits, bits, "{what}");
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

/// The network process beside VM process `vm`, and their spawner, the daemon, which starts
/// a run's two: shards-net, the spawner's child too.
fn net_process(vm: u32) -> (u32, u32) {
    let all = common::processes();
    let spawner = all
        .iter()
        .find(|(pid, _, _)| *pid == vm)
        .map(|(_, parent, _)| *parent)
        .unwrap_or_else(|| panic!("no VM process {vm}"));
    let found: Vec<u32> = all
        .iter()
        .filter(|(_, parent, name)| *parent == spawner && name.eq_ignore_ascii_case("shards-net.exe"))
        .map(|(pid, _, _)| *pid)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "the network processes beside VM process {vm}: {found:?}"
    );
    (found[0], spawner)
}

/// A VM's network process runs in an AppContainer of its own, holding the network's
/// capabilities and no other (internetClient, internetClientServer and
/// privateNetworkClientServer: S-1-15-3-1 to 3, PM M204), in a Job, under every mitigation
/// a VM process is made with; it ends as its VM does, and its profile's record goes once it
/// has.
#[test]
fn the_network_process_is_confined() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("net-confined");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let ran = shards(&["run", "-d", "--pull", "never", "--name", "net", &image, "sleep"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let vm = shards(&["inspect", "-f", "{{.State.Pid}}", "net"]);
    let vm: u32 = vm.stdout.trim().parse().unwrap_or_else(|_| panic!("{vm}"));
    let (net, spawner) = net_process(vm);
    let process = open(net);
    assert!(in_job(&process), "in a Job");
    let token = token(&process);
    assert!(is_app_container(&token), "in an AppContainer");
    let mut held = capabilities(&token);
    held.sort();
    assert_eq!(
        held,
        ["S-1-15-3-1", "S-1-15-3-2", "S-1-15-3-3"],
        "the network's capabilities alone"
    );
    for (policy, bits, what) in MADE_WITH {
        assert_eq!(mitigation(&process, policy) & bits, bits, "{what}");
    }

    // Its VM ended, it ends, and its profile's record goes with it.
    let removed = shards(&["rm", "-f", "net"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    // SAFETY: the process's handle.
    let waited = unsafe { WaitForSingleObject(process.0, 20_000) };
    assert_eq!(waited, WAIT_OBJECT_0, "the network process outlived its VM");
    let records = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
        .join("shards")
        .join("containers");
    let prefix = format!("shards.net.{spawner}.");
    let left = || -> Vec<String> {
        std::fs::read_dir(&records)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&prefix))
            .collect()
    };
    let deadline = Instant::now() + TIMEOUT;
    while !left().is_empty() {
        assert!(
            Instant::now() < deadline,
            "its profile's record left: {:?}",
            left()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let stopped = shards(&["stop", "daemon"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
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
    match common::machine_ended(&handle, &rx, TIMEOUT) {
        Some(reason) => assert_eq!(reason, ExitReason::Reset),
        None => panic!(
            "two vCPUs with their console did not reach their end in {TIMEOUT:?} in this process, nor said anything new for {:?} after",
            common::DAEMON_QUIET
        ),
    }
}
