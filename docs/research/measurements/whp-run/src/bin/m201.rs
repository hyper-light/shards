//! The Windows VM process as shards would confine it (PM M201), each part of the design
//! measured before it is built (M200 found the AppContainer and the capability):
//!
//! - an AppContainer of a SID derived from a name of its own, with no profile made, so that
//!   nothing is left in the registry or on disk per VM;
//! - holding the capabilities VidExo's DACL grants, read from the device as the child is
//!   made, not written in;
//! - its handles given by DuplicateHandle into it while it is suspended, none made
//!   inheritable but a pipe that tells it their values: so none reaches another child;
//! - in a Job that kills it as the Job's last handle closes, which lets it run no process
//!   of its own; with Windows' mitigations: no Win32k, no extension points, no dynamic
//!   code, no images from remote or low-label files, strict handle checks, no FSCTLs, no
//!   child processes;
//! - reaching an AF_UNIX socket in a directory whose DACL grants its SID, and refused one in
//!   a directory whose DACL does not.
//!
//! The child reads a file and writes a socket through handles so given, dials both
//! sockets, runs a partition, and tries to start a process; each result printed. Twice:
//! without the mitigations, and with them. Then a child left running is ended by closing
//! its Job's handle.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::io::Write as _;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_ALL, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName;
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, FreeSid, GetAce, GetLengthSid, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, ReadFile,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, WaitForSingleObject,
};
use whp_run::{Aligned, bind as bind_api, check, reg64, segment};

/// The mitigations, as UpdateProcThreadAttribute's documentation gives their values
/// (PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY): strict handle checks, no Win32k system
/// calls, no extension points, no dynamic code, no images from remote devices or of a low
/// label; and in the second word, no FSCTL system calls.
/// PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY's PROCESS_CREATION_CHILD_PROCESS_RESTRICTED
/// (UpdateProcThreadAttribute's documentation).
const CHILD_PROCESS_RESTRICTED: u32 = 0x01;

const MITIGATIONS: [u64; 2] = [
    (1 << 24) | (1 << 28) | (1 << 32) | (1 << 36) | (1 << 52) | (1 << 56),
    1 << 56,
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn last() -> String {
    std::io::Error::last_os_error().to_string()
}

fn startup() {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    unsafe { WSAStartup(0x0202, &raw mut data) };
}

fn unix_addr(path: &str) -> SOCKADDR_UN {
    let mut a = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    for (d, b) in a.sun_path.iter_mut().zip(path.bytes()) {
        *d = b as i8;
    }
    a
}

fn socket() -> Result<SOCKET, String> {
    // SAFETY: plain arguments; no child inherits it.
    let s = unsafe { WSASocketW(i32::from(AF_UNIX), SOCK_STREAM, 0, std::ptr::null(), 0, WSA_FLAG_NO_HANDLE_INHERIT) };
    if s == INVALID_SOCKET {
        // SAFETY: no arguments.
        return Err(format!("WSASocketW: {}", unsafe { WSAGetLastError() }));
    }
    Ok(s)
}

fn listen_at(path: &str) -> Result<SOCKET, String> {
    let s = socket()?;
    let a = unix_addr(path);
    // SAFETY: an address of its own length; a bound socket.
    unsafe {
        if bind(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) != 0 {
            return Err(format!("bind {path}: {}", WSAGetLastError()));
        }
        if listen(s, 4) != 0 {
            return Err(format!("listen: {}", WSAGetLastError()));
        }
        let mut on = 1u32;
        ioctlsocket(s, FIONBIO, &raw mut on);
    }
    Ok(s)
}

fn dial(path: &str) -> Result<SOCKET, String> {
    let s = socket()?;
    let a = unix_addr(path);
    // SAFETY: an address of its own length.
    if unsafe { connect(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) } != 0 {
        // SAFETY: no arguments.
        let e = unsafe { WSAGetLastError() };
        // SAFETY: our own socket.
        unsafe { closesocket(s) };
        return Err(format!("WSA error {e} ({})", std::io::Error::from_raw_os_error(e)));
    }
    Ok(s)
}

/// A connected pair, through a listener at a path of its own.
fn pair(dir: &std::path::Path) -> Result<(SOCKET, SOCKET), String> {
    let path = dir.join("m201-pair.sock").display().to_string();
    let l = listen_at(&path)?;
    let c = dial(&path)?;
    let mut a = INVALID_SOCKET;
    for _ in 0..1000 {
        // SAFETY: a listening socket; no address asked.
        a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
        if a != INVALID_SOCKET {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // SAFETY: our own listener.
    unsafe { closesocket(l) };
    let _ = std::fs::remove_file(&path);
    if a == INVALID_SOCKET {
        return Err("pair: no accept".into());
    }
    let mut off = 0u32;
    // SAFETY: back to blocking.
    unsafe { ioctlsocket(a, FIONBIO, &raw mut off) };
    Ok((c, a))
}

fn accept_within(l: SOCKET, within: Duration) -> Option<SOCKET> {
    let until = Instant::now() + within;
    while Instant::now() < until {
        // SAFETY: a listening socket; no address asked.
        let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
        if a != INVALID_SOCKET {
            return Some(a);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

fn read_some(s: SOCKET) -> String {
    let mut buf = [0u8; 64];
    let mut off = 0u32;
    // SAFETY: back to blocking; a buffer of its length.
    let n = unsafe {
        ioctlsocket(s, FIONBIO, &raw mut off);
        recv(s, buf.as_mut_ptr(), buf.len() as i32, 0)
    };
    if n <= 0 {
        return format!("recv {n}");
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

fn send_text(s: SOCKET, text: &str) -> i32 {
    // SAFETY: the text's bytes.
    unsafe { send(s, text.as_ptr(), text.len() as i32, 0) }
}

/// The capability SIDs VidExo's DACL allows read and write, as owned bytes.
fn videxo_capabilities() -> Result<Vec<Vec<u8>>, String> {
    let name = wide("\\\\?\\VidExo");
    // SAFETY: a NUL-terminated name; READ_CONTROL alone.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(format!("VidExo: {}", last()));
    }
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: our handle; the descriptor freed below, the DACL within it.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    // SAFETY: our handle.
    unsafe { CloseHandle(h) };
    if r != 0 || dacl.is_null() {
        return Err(format!("VidExo's DACL: {r}"));
    }
    const READ_WRITE: u32 = 0x0012_0089 | 0x0012_0116;
    let mut found = Vec::new();
    // SAFETY: the DACL GetSecurityInfo returned, its ACEs read in place.
    unsafe {
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &raw mut ace) == 0 {
                continue;
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            if (*allowed).Header.AceType != 0 || (*allowed).Mask & READ_WRITE != READ_WRITE {
                continue;
            }
            let sid: PSID = (&raw mut (*allowed).SidStart).cast();
            // A capability SID: S-1-15-3-..., its authority 15 and first sub-authority 3.
            let bytes = std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize);
            if bytes.len() > 12 && bytes[7] == 15 && bytes[8..12] == 3u32.to_le_bytes() {
                found.push(bytes.to_vec());
            }
        }
        LocalFree(sd.cast());
    }
    Ok(found)
}

/// Grants `sid` all access to `dir` and what is made in it.
fn grant_dir(dir: &std::path::Path, sid: PSID) -> Result<(), String> {
    let name = wide(&dir.display().to_string());
    let mut old: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated name; the descriptor freed below, the DACL within it.
    let r = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut old,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if r != 0 {
        return Err(format!("GetNamedSecurityInfoW: {r}"));
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    };
    let mut new: *mut ACL = std::ptr::null_mut();
    // SAFETY: one entry and the old DACL; the new one freed below.
    let r = unsafe { SetEntriesInAclW(1, &raw const entry, old, &raw mut new) };
    if r != 0 {
        // SAFETY: the descriptor read above.
        unsafe { LocalFree(sd.cast()) };
        return Err(format!("SetEntriesInAclW: {r}"));
    }
    // SAFETY: the new DACL, set on the directory.
    let r = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new,
            std::ptr::null(),
        )
    };
    // SAFETY: what the calls allocated.
    unsafe {
        LocalFree(new.cast());
        LocalFree(sd.cast());
    }
    if r != 0 {
        return Err(format!("SetNamedSecurityInfoW: {r}"));
    }
    Ok(())
}

/// The child's partition, as M200's: one vCPU in long mode, run to a port write.
fn partition() -> Result<String, String> {
    let api = bind_api()?;
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes and deletes.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut p))?;
        let one: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(p, WHvPartitionPropertyCodeProcessorCount, (&raw const one).cast(), 4),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(p))?;
        let ram = windows_sys::Win32::System::Memory::VirtualAlloc(
            std::ptr::null(),
            2 << 20,
            windows_sys::Win32::System::Memory::MEM_COMMIT | windows_sys::Win32::System::Memory::MEM_RESERVE,
            windows_sys::Win32::System::Memory::PAGE_READWRITE,
        )
        .cast::<u8>();
        if ram.is_null() {
            return Err("VirtualAlloc".into());
        }
        let mem = std::slice::from_raw_parts_mut(ram, 2 << 20);
        mem[0x1000..0x1006].copy_from_slice(&[0x66, 0xBA, 0x01, 0x05, 0xEE, 0xF4]);
        mem[0x10000..0x10008].copy_from_slice(&(0x11000u64 | 0x3).to_le_bytes());
        mem[0x11000..0x11008].copy_from_slice(&0x83u64.to_le_bytes());
        check(
            "WHvMapGpaRange",
            (api.map)(
                p,
                ram.cast(),
                0,
                2 << 20,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        check("WHvCreateVirtualProcessor", (api.create_vp)(p, 0, 0))?;
        let names = [
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterSs,
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
        ];
        let values = Aligned([
            reg64(0x8000_0031),
            reg64(0x10000),
            reg64(0x20),
            reg64(0x500),
            segment(0x8, 0xA09B),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            reg64(0x1000),
            reg64(0x2),
        ]);
        check(
            "registers",
            (api.set_regs)(p, 0, names.as_ptr(), names.len() as u32, values.0.as_ptr()),
        )?;
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        check(
            "WHvRunVirtualProcessor",
            (api.run)(p, 0, (&raw mut exit).cast(), std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32),
        )?;
        let port = exit.0.Anonymous.IoPortAccess.PortNumber;
        (api.delete_vp)(p, 0);
        (api.delete_partition)(p);
        Ok(format!("exit={:#x} port={port:#x}", exit.0.ExitReason))
    }
}

use windows_sys::Win32::System::Hypervisor::*;

/// The confined child: what its grants pipe says, then each check.
fn confined_child(pipe: usize) {
    startup();
    let mut buf = vec![0u8; 4096];
    let mut got = 0usize;
    loop {
        let mut n = 0u32;
        // SAFETY: the pipe's read end, given us; the rest of our buffer.
        let ok = unsafe {
            ReadFile(
                pipe as HANDLE,
                buf[got..].as_mut_ptr(),
                (buf.len() - got) as u32,
                &raw mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            break;
        }
        got += n as usize;
    }
    let text = String::from_utf8_lossy(&buf[..got]).into_owned();
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or("")
            .to_string()
    };
    let say = |line: String| {
        let _ = writeln!(std::io::stdout(), "child: {line}");
    };
    say(format!("grants: {:?}", text.trim()));
    let file = field("file").parse::<usize>().unwrap_or(0);
    let mut fbuf = [0u8; 32];
    let mut n = 0u32;
    // SAFETY: a handle duplicated into us; a buffer of its length.
    let read = unsafe {
        ReadFile(
            file as HANDLE,
            fbuf.as_mut_ptr(),
            fbuf.len() as u32,
            &raw mut n,
            std::ptr::null_mut(),
        )
    };
    say(format!(
        "duplicated file read={read} {:?}",
        String::from_utf8_lossy(&fbuf[..n as usize])
    ));
    let sock = field("socket").parse::<usize>().unwrap_or(0) as SOCKET;
    say(format!("duplicated socket sent {}", send_text(sock, "from the child")));
    match dial(&field("granted")) {
        Ok(s) => say(format!("dial granted dir: ok, sent {}", send_text(s, "dialed"))),
        Err(e) => say(format!("dial granted dir: {e}")),
    }
    match dial(&field("refused")) {
        Ok(_) => say("dial refused dir: CONNECTED".into()),
        Err(e) => say(format!("dial refused dir: {e}")),
    }
    let t = Instant::now();
    say(format!("partition: {:?} in {} us", partition(), t.elapsed().as_micros()));
    say(format!(
        "a process of its own: {:?}",
        std::process::Command::new("cmd.exe")
            .args(["/c", "exit"])
            .status()
            .map_err(|e| e.to_string())
    ));
}

/// A suspended child in an AppContainer of `sid` holding `caps`, in `job`, with `mitigate`
/// or not; its grants pipe's write end, its process and its thread.
fn spawn(
    args: &str,
    sid: Option<PSID>,
    caps: &mut [SID_AND_ATTRIBUTES],
    job: HANDLE,
    mitigate: bool,
    pipe_read: HANDLE,
) -> Result<PROCESS_INFORMATION, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = wide(&format!("\"{}\" {args}", exe.display()));
    let out = std::io::stdout().as_raw_handle() as HANDLE;
    let mut handles = vec![out];
    if !pipe_read.is_null() {
        handles.push(pipe_read);
    }
    // SAFETY: our own handles, made inheritable for the list below.
    for &h in &handles {
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    let mut size = 0usize;
    let count = 5;
    // SAFETY: the size query.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
    let mut list = vec![0u8; size];
    let attrs = list.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    let jobs = [job];
    let child_policy: u32 = CHILD_PROCESS_RESTRICTED;
    let mut capabilities = SECURITY_CAPABILITIES::default();
    let update = |attr: u32, value: *const c_void, size: usize| -> Result<(), String> {
        // SAFETY: a value that outlives the list.
        if unsafe { UpdateProcThreadAttribute(attrs, 0, attr as usize, value, size, std::ptr::null_mut(), std::ptr::null()) }
            == 0
        {
            return Err(format!("attribute {attr:#x}: {}", last()));
        }
        Ok(())
    };
    // SAFETY: a buffer of the size asked.
    if unsafe { InitializeProcThreadAttributeList(attrs, count, 0, &raw mut size) } == 0 {
        return Err(format!("InitializeProcThreadAttributeList: {}", last()));
    }
    update(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_ptr().cast(),
        handles.len() * std::mem::size_of::<HANDLE>(),
    )?;
    update(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs.as_ptr().cast(), std::mem::size_of::<HANDLE>())?;
    if let Some(sid) = sid {
        capabilities.AppContainerSid = sid;
        capabilities.Capabilities = caps.as_mut_ptr();
        capabilities.CapabilityCount = caps.len() as u32;
        update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            (&raw const capabilities).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )?;
        update(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
            (&raw const child_policy).cast(),
            std::mem::size_of::<u32>(),
        )?;
    }
    if mitigate {
        update(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
            MITIGATIONS.as_ptr().cast(),
            std::mem::size_of_val(&MITIGATIONS),
        )?;
    }
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdOutput = out;
    si.StartupInfo.hStdError = out;
    si.lpAttributeList = attrs;
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: a command line we own; startup info and attributes alive.
    let made = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            (&raw const si).cast(),
            &raw mut pi,
        )
    };
    // SAFETY: the list initialized above.
    unsafe { DeleteProcThreadAttributeList(attrs) };
    if made == 0 {
        return Err(format!("CreateProcessW: {}", last()));
    }
    Ok(pi)
}

use windows_sys::Win32::System::Threading::UpdateProcThreadAttribute;

fn job(one_process: bool) -> Result<HANDLE, String> {
    // SAFETY: an anonymous job.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(format!("CreateJobObjectW: {}", last()));
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if one_process {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
    }
    // SAFETY: limits of their own size.
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(format!("SetInformationJobObject: {}", last()));
    }
    Ok(job)
}

fn confined_run(mitigate: bool, caps_bytes: &[Vec<u8>]) -> Result<(), String> {
    let base = std::env::current_dir().map_err(|e| e.to_string())?.join(format!("m201-{}", mitigate as u8));
    let (granted, refused) = (base.join("granted"), base.join("refused"));
    for d in [&granted, &refused] {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let name = wide(&format!("shards.m201.{}.{}", std::process::id(), mitigate as u8));
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: a name we own; the SID freed below. No profile is made.
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &raw mut sid) };
    if hr < 0 {
        return Err(format!("DeriveAppContainerSidFromAppContainerName: {:#010x}", hr as u32));
    }
    grant_dir(&granted, sid)?;
    let granted_sock = granted.join("g.sock").display().to_string();
    let refused_sock = refused.join("r.sock").display().to_string();
    let lg = listen_at(&granted_sock)?;
    let lr = listen_at(&refused_sock)?;
    let input = base.join("input.txt");
    std::fs::write(&input, "granted by handle").map_err(|e| e.to_string())?;
    let file = std::fs::File::open(&input).map_err(|e| e.to_string())?;
    let (ours, theirs) = pair(&base)?;
    let mut caps: Vec<SID_AND_ATTRIBUTES> = caps_bytes
        .iter()
        .map(|b| SID_AND_ATTRIBUTES {
            Sid: b.as_ptr().cast_mut().cast(),
            Attributes: SE_GROUP_ENABLED as u32,
        })
        .collect();
    let (mut pr, mut pw): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill, not inheritable.
    if unsafe { CreatePipe(&raw mut pr, &raw mut pw, std::ptr::null(), 0) } == 0 {
        return Err(format!("CreatePipe: {}", last()));
    }
    let job = job(true)?;
    let t = Instant::now();
    let pi = spawn(&format!("child-confined {}", pr as usize), Some(sid), &mut caps, job, mitigate, pr)?;
    let dup = |h: HANDLE| -> usize {
        let mut out: HANDLE = std::ptr::null_mut();
        // SAFETY: our handle, duplicated into the child we made, of the same access.
        unsafe {
            DuplicateHandle(GetCurrentProcess(), h, pi.hProcess, &raw mut out, 0, 0, DUPLICATE_SAME_ACCESS);
        }
        out as usize
    };
    let child_file = dup(file.as_raw_handle() as HANDLE);
    let child_sock = dup(theirs as HANDLE);
    let grants = format!(
        "file={child_file}\nsocket={child_sock}\ngranted={granted_sock}\nrefused={refused_sock}\n"
    );
    let mut written = 0u32;
    // SAFETY: our pipe's write end; the grants' bytes. Then the ends we no longer need.
    unsafe {
        windows_sys::Win32::Storage::FileSystem::WriteFile(
            pw,
            grants.as_ptr(),
            grants.len() as u32,
            &raw mut written,
            std::ptr::null_mut(),
        );
        CloseHandle(pw);
        CloseHandle(pr);
        closesocket(theirs);
        ResumeThread(pi.hThread);
    }
    let spawned = t.elapsed();
    let dialed = accept_within(lg, Duration::from_secs(20)).map(read_some);
    let mut code = 0u32;
    // SAFETY: the child's handles, closed once it has ended.
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    let ran = t.elapsed();
    let refused_conn = accept_within(lr, Duration::from_millis(200)).is_some();
    println!(
        "parent: mitigations={mitigate} spawned_us={} ran_us={} exit={code} granted dir dialed: {dialed:?}; refused dir reached: {refused_conn}; pair heard: {:?}",
        spawned.as_micros(),
        ran.as_micros(),
        read_some(ours)
    );
    // SAFETY: what this run made.
    unsafe {
        closesocket(lg);
        closesocket(lr);
        closesocket(ours);
        CloseHandle(job);
        FreeSid(sid);
    }
    drop(file);
    let _ = std::fs::remove_dir_all(&base);
    Ok(())
}

/// A child left running, ended as its Job's last handle closes.
fn lifeline() -> Result<(), String> {
    let job = job(false)?;
    let pi = spawn("child-sleep", None, &mut [], job, false, std::ptr::null_mut())?;
    // SAFETY: the child we made.
    unsafe { ResumeThread(pi.hThread) };
    std::thread::sleep(Duration::from_millis(500));
    let t = Instant::now();
    // SAFETY: the job's only handle, closed.
    unsafe { CloseHandle(job) };
    // SAFETY: the child's handles.
    let waited = unsafe { WaitForSingleObject(pi.hProcess, 5000) };
    let mut code = 0u32;
    // SAFETY: as above.
    unsafe {
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    println!(
        "lifeline: ended={} in {} us, exit code {code:#x}",
        waited == WAIT_OBJECT_0,
        t.elapsed().as_micros()
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child-confined") => {
            confined_child(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0));
            return;
        }
        Some("child-sleep") => {
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        _ => {}
    }
    startup();
    let caps = match videxo_capabilities() {
        Ok(c) => c,
        Err(e) => {
            println!("VidExo's capabilities: {e}");
            Vec::new()
        }
    };
    println!("VidExo grants {} capability SID(s)", caps.len());
    for mitigate in [false, true] {
        if let Err(e) = confined_run(mitigate, &caps) {
            println!("confined (mitigations={mitigate}): {e}");
        }
    }
    if let Err(e) = lifeline() {
        println!("lifeline: {e}");
    }
}
