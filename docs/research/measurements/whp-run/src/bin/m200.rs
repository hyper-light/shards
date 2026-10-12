//! Whether a process confined as shards would confine its VM process on Windows can run a
//! WHP partition (PM M200): an AppContainer of no capabilities, launched with exactly the
//! handles named for it (PROC_THREAD_ATTRIBUTE_HANDLE_LIST). The child, in it and again
//! outside it as the control: reads a file through a handle it inherited; opens a file by
//! its path, as a confined VM process must not; and runs a partition of one vCPU to a port
//! write, each step's result printed.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{FreeSid, PSID, SECURITY_CAPABILITIES};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
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

fn child(handle: usize, path: &str) {
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
    println!("child: partition: {:?}", partition());
}

/// Runs this binary as `child`, in AppContainer `sid` if given, with exactly `file`, and
/// the parent's stdout and stderr, inherited.
fn launch(sid: Option<PSID>, file: HANDLE, path: &str) -> Result<u32, String> {
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
        child(handle, args.get(3).map_or("", String::as_str));
        return;
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
    for (label, container) in [("control (no AppContainer)", None), ("AppContainer", Some(sid))] {
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
        match launch(container, h, &path_s) {
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
}
