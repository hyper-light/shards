//! Whether a process confined as shards would confine its VM process on Windows can run a
//! WHP partition (PM M200): an AppContainer of no capabilities, launched with exactly the
//! handles named for it (PROC_THREAD_ATTRIBUTE_HANDLE_LIST). The child, in it and again
//! outside it as the control: reads a file through a handle it inherited; opens a file by
//! its path, as a confined VM process must not; and runs a partition of one vCPU to a port
//! write, each step's result printed. Then: whether a standard user, not an administrator
//! as the runner's is, runs one (an account the workflow makes, `SHARDS_PROBE_USER`); and
//! which device WinHvPlatform.dll opens, found among its own strings, with that device's
//! security descriptor, which says who may: Authenticated Users, and one capability SID
//! (run 3). So the AppContainer is tried a third time holding that capability alone, and
//! the capability's name is looked for among those the system knows: each derived
//! (DeriveCapabilitySidsFromName) and compared.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, FreeSid, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_CAPABILITIES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, ReadFile,
};
use windows_sys::Win32::System::Threading::{CreateProcessWithLogonW, LOGON_WITH_PROFILE, STARTUPINFOW};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use whp_run::{Aligned, bind, capability, check, reg64, segment};

const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
const DONE: u16 = 0x501;
/// The capability SID VidExo's DACL grants read and write (run 3 of this probe).
const VIDEXO_CAPABILITY: &str =
    "S-1-15-3-1024-1350733694-1533467442-3402045280-1710179238-1008639688-2282869650-3611451526-669908847";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The child's partition: one vCPU in long mode, run to its port write.
fn partition() -> Result<String, String> {
    let api = bind()?;
    // SAFETY: the union member the code fills.
    let present = capability(&api, WHvCapabilityCodeHypervisorPresent).map(|c| unsafe { c.HypervisorPresent });
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes and deletes.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut p))?;
        let one: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(p, WHvPartitionPropertyCodeProcessorCount, (&raw const one).cast(), 4),
        )?;
        let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
        check(
            "LocalApicEmulationMode",
            (api.set_property)(p, WHvPartitionPropertyCodeLocalApicEmulationMode, (&raw const mode).cast(), 4),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(p))?;
        let ram = VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        if ram.is_null() {
            return Err("VirtualAlloc".into());
        }
        let mem = std::slice::from_raw_parts_mut(ram, RAM);
        // mov dx, DONE; out dx, al; hlt
        let code = [0x66, 0xBA, DONE as u8, (DONE >> 8) as u8, 0xEE, 0xF4];
        mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
        mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
        mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
        check(
            "WHvMapGpaRange",
            (api.map)(
                p,
                ram.cast(),
                0,
                RAM as u64,
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
            reg64(PML4),
            reg64(0x20),
            reg64(0x500),
            segment(0x8, 0xA09B),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            reg64(CODE),
            reg64(0x2),
        ]);
        check(
            "registers",
            (api.set_regs)(p, 0, names.as_ptr(), names.len() as u32, values.0.as_ptr()),
        )?;
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        check(
            "WHvRunVirtualProcessor",
            (api.run)(
                p,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            ),
        )?;
        let port = exit.0.Anonymous.IoPortAccess.PortNumber;
        (api.delete_vp)(p, 0);
        (api.delete_partition)(p);
        Ok(format!(
            "hypervisor_present={present:?} exit={:#x} port={port:#x}",
            exit.0.ExitReason
        ))
    }
}

/// A DLL's UTF-16 strings that may name a device: those with "Device", "Vid" or "\\??\\"
/// in them, at least 6 characters long.
fn devices_in(dll: &std::path::Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(dll) else {
        return vec![format!("(no {})", dll.display())];
    };
    let mut found: Vec<String> = Vec::new();
    let mut run: Vec<u16> = Vec::new();
    for c in bytes.chunks_exact(2) {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if (0x20..0x7F).contains(&u) {
            run.push(u);
            continue;
        }
        if run.len() >= 6 {
            let s = String::from_utf16_lossy(&run);
            if (s.contains("Device") || s.contains("Vid") || s.contains("\\??\\")) && !found.contains(&s) {
                found.push(s);
            }
        }
        run.clear();
    }
    found
}

/// A device's owner, DACL and integrity label, as SDDL, through a handle opened for
/// READ_CONTROL alone.
fn sddl(device: &str) -> String {
    // An NT device path through GLOBALROOT; a DOS one (\\?\NAME) as it is.
    let name = if device.starts_with("\\\\?\\") {
        wide(device)
    } else {
        wide(&format!("\\\\?\\GLOBALROOT{device}"))
    };
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
        return format!("open: {}", std::io::Error::last_os_error());
    }
    let what = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: our handle; the descriptor is freed below.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            what,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    // SAFETY: our handle.
    unsafe { CloseHandle(h) };
    if r != 0 {
        return format!("GetSecurityInfo: {}", std::io::Error::from_raw_os_error(r as i32));
    }
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: the descriptor just read; the string is freed below.
    let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, what, &raw mut text, &raw mut len) };
    let out = if ok == 0 || text.is_null() {
        format!("convert: {}", std::io::Error::last_os_error())
    } else {
        // SAFETY: `len` UTF-16 units the call wrote.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) })
            .trim_end_matches('\0')
            .to_string()
    };
    // SAFETY: what the calls allocated.
    unsafe {
        LocalFree(text.cast());
        LocalFree(sd.cast());
    }
    out
}

/// Runs `exe` as `child` under the standard user `user`; its exit code: 0 where the
/// partition ran.
fn as_user(exe: &std::path::Path, user: &str, password: &str) -> Result<u32, String> {
    let mut cmd = wide(&format!("\"{}\" child 0 \"\"", exe.display()));
    let (user_w, domain, password_w) = (wide(user), wide("."), wide(password));
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: names and a command line we own; startup info alive.
    let made = unsafe {
        CreateProcessWithLogonW(
            user_w.as_ptr(),
            domain.as_ptr(),
            password_w.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            &raw const si,
            &raw mut pi,
        )
    };
    if made == 0 {
        return Err(format!("CreateProcessWithLogonW: {}", std::io::Error::last_os_error()));
    }
    let mut code = 0u32;
    // SAFETY: the child's handles, closed once it has ended.
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(code)
}

fn child(handle: usize, path: &str) -> i32 {
    let mut buf = [0u8; 64];
    let mut n = 0u32;
    // SAFETY: an inherited file handle; a buffer of its length.
    let read = unsafe {
        ReadFile(
            handle as HANDLE,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &raw mut n,
            std::ptr::null_mut(),
        )
    };
    println!(
        "child: inherited handle read={read} {:?}",
        String::from_utf8_lossy(&buf[..n as usize])
    );
    println!(
        "child: by path {path}: {:?}",
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    );
    let ran = partition();
    println!("child: partition: {ran:?}");
    if ran.is_ok() { 0 } else { 10 }
}

/// A SID from its string form; freed with LocalFree.
fn sid_of(text: &str) -> Option<PSID> {
    let w = wide(text);
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string; the SID the call allocates.
    let ok = unsafe { windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(w.as_ptr(), &raw mut sid) };
    (ok != 0).then_some(sid)
}

/// A SID's string form.
fn text_of(sid: PSID) -> String {
    let mut out: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: a valid SID; the string the call allocates, freed below.
    if unsafe { windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(sid, &raw mut out) } == 0 {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: a NUL-terminated string the call wrote.
    while unsafe { *out.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` units.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, n) });
    // SAFETY: the call's allocation.
    unsafe { LocalFree(out.cast()) };
    s
}

/// The capability SIDs `name` derives to, as strings.
fn derived(name: &str) -> Vec<String> {
    let w = wide(name);
    let (mut groups, mut ngroups, mut caps, mut ncaps): (*mut PSID, u32, *mut PSID, u32) =
        (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
    // SAFETY: a NUL-terminated name; arrays the call allocates, freed below.
    let ok = unsafe {
        windows_sys::Win32::Security::DeriveCapabilitySidsFromName(
            w.as_ptr(),
            &raw mut groups,
            &raw mut ngroups,
            &raw mut caps,
            &raw mut ncaps,
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    // SAFETY: the arrays and SIDs the call allocated, read and freed once each.
    unsafe {
        for i in 0..ncaps as usize {
            let sid = *caps.add(i);
            out.push(text_of(sid));
            LocalFree(sid.cast());
        }
        for i in 0..ngroups as usize {
            LocalFree((*groups.add(i)).cast());
        }
        LocalFree(caps.cast());
        LocalFree(groups.cast());
    }
    out
}

/// Capability names the system knows: the registry's cached list
/// (HKLM\SOFTWARE\Microsoft\SecurityManager\CapabilityClasses, AllCachedCapabilities), and
/// those the system's own app manifests declare.
fn capability_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let key = wide(r"SOFTWARE\Microsoft\SecurityManager\CapabilityClasses");
    let value = wide("AllCachedCapabilities");
    let mut size = 0u32;
    // SAFETY: the size query.
    unsafe {
        windows_sys::Win32::System::Registry::RegGetValueW(
            windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            windows_sys::Win32::System::Registry::RRF_RT_REG_MULTI_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    let mut buf = vec![0u16; size as usize / 2 + 1];
    // SAFETY: a buffer of the size asked.
    let r = unsafe {
        windows_sys::Win32::System::Registry::RegGetValueW(
            windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            windows_sys::Win32::System::Registry::RRF_RT_REG_MULTI_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &raw mut size,
        )
    };
    if r == 0 {
        names.extend(
            String::from_utf16_lossy(&buf)
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    println!("registry: {} cached capability names (RegGetValueW {r})", names.len());
    for root in [r"C:\Windows\SystemApps", r"C:\Program Files\WindowsApps"] {
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path().join("AppxManifest.xml")) else {
                continue;
            };
            for part in text.split("Capability Name=\"").skip(1) {
                if let Some(name) = part.split('"').next()
                    && !names.iter().any(|n| n == name)
                {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Runs this binary as `child`, in AppContainer `sid` if given, holding `capabilities`, with
/// exactly `file`, and the parent's stdout and stderr, inherited.
fn launch(sid: Option<PSID>, capabilities: &[PSID], file: HANDLE, path: &str) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = wide(&format!("\"{}\" child {} \"{path}\"", exe.display(), file as usize));
    let out = std::io::stdout().as_raw_handle() as HANDLE;
    let err = std::io::stderr().as_raw_handle() as HANDLE;
    let mut handles = vec![file];
    for h in [out, err] {
        // SAFETY: our own standard handles, made inheritable for the list below.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        if !handles.contains(&h) {
            handles.push(h);
        }
    }
    let count = if sid.is_some() { 2 } else { 1 };
    let mut size = 0usize;
    // SAFETY: the size query, which fails and fills `size`.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
    let mut list = vec![0u8; size];
    let attrs = list.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    let mut caps = SECURITY_CAPABILITIES::default();
    let mut held: Vec<windows_sys::Win32::Security::SID_AND_ATTRIBUTES> = capabilities
        .iter()
        .map(|&sid| windows_sys::Win32::Security::SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED as u32,
        })
        .collect();
    // SAFETY: a buffer of the size asked; attributes whose values outlive the list.
    unsafe {
        if InitializeProcThreadAttributeList(attrs, count, 0, &raw mut size) == 0 {
            return Err(format!("InitializeProcThreadAttributeList: {}", std::io::Error::last_os_error()));
        }
        if UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            handles.len() * std::mem::size_of::<HANDLE>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            return Err(format!("handle list: {}", std::io::Error::last_os_error()));
        }
        if let Some(sid) = sid {
            caps.AppContainerSid = sid;
            caps.Capabilities = if held.is_empty() { std::ptr::null_mut() } else { held.as_mut_ptr() };
            caps.CapabilityCount = held.len() as u32;
            if UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (&raw const caps).cast::<c_void>(),
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(format!("security capabilities: {}", std::io::Error::last_os_error()));
            }
        }
    }
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdOutput = out;
    si.StartupInfo.hStdError = err;
    si.lpAttributeList = attrs;
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: a command line we own, writable; startup info and attributes alive.
    let made = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            (&raw const si).cast(),
            &raw mut pi,
        )
    };
    // SAFETY: the list initialized above.
    unsafe { DeleteProcThreadAttributeList(attrs) };
    if made == 0 {
        return Err(format!("CreateProcessW: {}", std::io::Error::last_os_error()));
    }
    let mut code = 0u32;
    // SAFETY: the child's handles, closed once it has ended.
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(code)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        let handle = args.get(2).and_then(|h| h.parse::<usize>().ok()).unwrap_or(0);
        std::process::exit(child(handle, args.get(3).map_or("", String::as_str)));
    }
    let dir = std::env::current_dir().unwrap_or_default();
    let path = dir.join("m200-input.txt");
    if let Err(e) = std::fs::write(&path, "granted") {
        println!("writing the input: {e}");
        return;
    }
    let path_s = path.display().to_string();
    let open = || std::fs::File::open(&path);
    let name = wide(&format!("shards.m200.{}", std::process::id()));
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: names we own; the SID is freed below.
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            std::ptr::null(),
            0,
            &raw mut sid,
        )
    };
    if hr < 0 {
        // SAFETY: as above.
        let hr2 = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &raw mut sid) };
        println!("CreateAppContainerProfile: {:#010x}; derived: {:#010x}", hr as u32, hr2 as u32);
    }
    let vid = sid_of(VIDEXO_CAPABILITY);
    let with_vid: Vec<PSID> = vid.into_iter().collect();
    for (label, container, capabilities) in [
        ("control (no AppContainer)", None, &[][..]),
        ("AppContainer", Some(sid), &[][..]),
        ("AppContainer with VidExo's capability", Some(sid), &with_vid[..]),
    ] {
        let file = match open() {
            Ok(f) => f,
            Err(e) => {
                println!("{label}: opening the input: {e}");
                continue;
            }
        };
        let h = file.as_raw_handle() as HANDLE;
        // SAFETY: our own file handle, made inheritable for the child's list.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        println!("== {label}");
        match launch(container, capabilities, h, &path_s) {
            Ok(code) => println!("{label}: child exited {code}"),
            Err(e) => println!("{label}: {e}"),
        }
    }
    // SAFETY: the profile and SID this run made.
    unsafe {
        DeleteAppContainerProfile(name.as_ptr());
        if !sid.is_null() {
            FreeSid(sid);
        }
    }
    let _ = std::fs::remove_file(&path);
    // A standard user, where the workflow made one: the probe copied where it may run it.
    if let (Ok(user), Ok(password)) = (std::env::var("SHARDS_PROBE_USER"), std::env::var("SHARDS_PROBE_PASSWORD")) {
        let public = std::path::PathBuf::from(r"C:\Users\Public\shards-m200");
        let exe = public.join("m200.exe");
        let copied = std::fs::create_dir_all(&public)
            .and_then(|()| std::fs::copy(std::env::current_exe()?, &exe).map(drop));
        match copied.map_err(|e| e.to_string()).and_then(|()| as_user(&exe, &user, &password)) {
            Ok(code) => println!("standard user {user}: child exited {code} (0: the partition ran)"),
            Err(e) => println!("standard user {user}: {e}"),
        }
        let _ = std::fs::remove_dir_all(&public);
    }
    // The capability's name, among those the system knows.
    let names = capability_names();
    let mut matched = Vec::new();
    for name in &names {
        if derived(name).iter().any(|s| s == VIDEXO_CAPABILITY) {
            matched.push(name.clone());
        }
    }
    println!("capability names tried: {}; VidExo's: {matched:?}", names.len());
    if let Some(v) = vid {
        // SAFETY: the SID ConvertStringSidToSidW allocated.
        unsafe { LocalFree(v.cast()) };
    }
    // The device WHP opens, and who may.
    let system32 = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32");
    for dll in ["WinHvPlatform.dll", "vid.dll", "VidCore.dll", "WinHvEmulation.dll"] {
        for text in devices_in(&system32.join(dll)) {
            if text.starts_with("\\Device\\") || text.starts_with("\\\\?\\") {
                println!("{dll}: {text}: {}", sddl(&text));
            } else {
                println!("{dll}: string {text:?}");
            }
        }
    }
}
