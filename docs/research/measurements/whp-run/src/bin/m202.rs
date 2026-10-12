//! Whether a VM process may forbid itself dynamic code (ACG) once its partition is set up
//! (PM M202). On windows-2022 a process made with ACG cannot set a partition up (M201,
//! ERROR_DYNAMIC_CODE_BLOCKED); a process may turn ACG on for itself later, for good
//! (SetProcessMitigationPolicy, ProcessDynamicCodePolicy). So: a partition of two vCPUs set
//! up, then ACG, then every call the VMM makes after setup, each result printed: mapping and
//! unmapping memory, creating vCPUs (one on another thread), setting registers, running to
//! an exit, asking an interrupt, canceling a run, reading a vCPU's CPUID; and last, a second
//! partition's setup, which is expected to fail where the first did.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
use windows_sys::Win32::System::SystemServices::{
    PROCESS_MITIGATION_DYNAMIC_CODE_POLICY, PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0,
};
use windows_sys::Win32::System::Threading::{ProcessDynamicCodePolicy, SetProcessMitigationPolicy};
use whp_run::{Aligned, Api, bind, check, reg64, segment};

const RAM: usize = 2 << 20;

fn setup(api: &Api, vcpus: u32) -> Result<WHV_PARTITION_HANDLE, String> {
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut p))?;
        check(
            "ProcessorCount",
            (api.set_property)(p, WHvPartitionPropertyCodeProcessorCount, (&raw const vcpus).cast(), 4),
        )?;
        let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
        check(
            "LocalApicEmulationMode",
            (api.set_property)(p, WHvPartitionPropertyCodeLocalApicEmulationMode, (&raw const mode).cast(), 4),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(p))?;
    }
    Ok(p)
}

fn step(label: &str, r: Result<String, String>) {
    println!("{label}: {r:?}");
}

fn main() {
    let api = match bind() {
        Ok(api) => api,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    let p = match setup(&api, 2) {
        Ok(p) => p,
        Err(e) => {
            println!("setup before ACG: {e}");
            return;
        }
    };
    println!("setup before ACG: ok");
    let policy = PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
        Anonymous: PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0 { Flags: 1 },
    };
    // SAFETY: a policy of its own size, for this process.
    let set = unsafe {
        SetProcessMitigationPolicy(
            ProcessDynamicCodePolicy,
            (&raw const policy).cast(),
            std::mem::size_of_val(&policy),
        )
    };
    println!(
        "ACG on: {set} {}",
        if set == 0 { std::io::Error::last_os_error().to_string() } else { String::new() }
    );
    // SAFETY: a fresh allocation kept for the probe's life.
    let ram = unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
    if ram.is_null() {
        println!("VirtualAlloc failed");
        return;
    }
    // SAFETY: the allocation above.
    let mem = unsafe { std::slice::from_raw_parts_mut(ram, RAM) };
    mem[0x1000..0x1006].copy_from_slice(&[0x66, 0xBA, 0x01, 0x05, 0xEE, 0xF4]);
    mem[0x10000..0x10008].copy_from_slice(&(0x11000u64 | 0x3).to_le_bytes());
    mem[0x11000..0x11008].copy_from_slice(&0x83u64.to_le_bytes());
    let rwx = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
    // SAFETY: each call on the partition above, with memory that outlives it.
    unsafe {
        step(
            "map",
            check("WHvMapGpaRange", (api.map)(p, ram.cast(), 0, RAM as u64, rwx)).map(|()| "ok".into()),
        );
        step(
            "create vCPU 0",
            check("WHvCreateVirtualProcessor", (api.create_vp)(p, 0, 0)).map(|()| "ok".into()),
        );
    }
    let create = api.create_vp;
    let on_thread = std::thread::scope(|s| {
        s.spawn(|| {
            // SAFETY: the partition, alive in this scope.
            check("WHvCreateVirtualProcessor", unsafe { create(p, 1, 0) })
        })
        .join()
        .unwrap_or(Err("panicked".into()))
    });
    step("create vCPU 1 on another thread", on_thread.map(|()| "ok".into()));
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
    // SAFETY: as above.
    unsafe {
        step(
            "set registers",
            check(
                "registers",
                (api.set_regs)(p, 0, names.as_ptr(), names.len() as u32, values.0.as_ptr()),
            )
            .map(|()| "ok".into()),
        );
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        step(
            "run",
            check(
                "WHvRunVirtualProcessor",
                (api.run)(p, 0, (&raw mut exit).cast(), std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32),
            )
            .map(|()| format!("exit {:#x} port {:#x}", exit.0.ExitReason, exit.0.Anonymous.IoPortAccess.PortNumber)),
        );
        let control = WHV_INTERRUPT_CONTROL {
            _bitfield: 0,
            Destination: 0,
            Vector: 0x40,
        };
        step(
            "request an interrupt",
            check(
                "WHvRequestInterrupt",
                (api.request_interrupt)(p, &raw const control, std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32),
            )
            .map(|()| "ok".into()),
        );
        step(
            "cancel a run",
            check("WHvCancelRunVirtualProcessor", (api.cancel)(p, 1, 0)).map(|()| "ok".into()),
        );
        let unmap: Result<(), String> = {
            let f: unsafe extern "system" fn(WHV_PARTITION_HANDLE, u64, u64) -> windows_sys::core::HRESULT =
                WHvUnmapGpaRange;
            check("WHvUnmapGpaRange", f(p, 0x100000, 0x100000))
        };
        step("unmap a part", unmap.map(|()| "ok".into()));
        step(
            "map it again",
            check("WHvMapGpaRange", (api.map)(p, ram.add(0x100000).cast(), 0x100000, 0x100000, rwx))
                .map(|()| "ok".into()),
        );
        let mut out = WHV_CPUID_OUTPUT::default();
        step(
            "a vCPU's CPUID",
            check(
                "WHvGetVirtualProcessorCpuidOutput",
                WHvGetVirtualProcessorCpuidOutput(p, 0, 0x4000_0005, 0, &raw mut out),
            )
            .map(|()| format!("max vCPUs {}", out.Eax)),
        );
        step(
            "delete vCPU 1",
            check("WHvDeleteVirtualProcessor", (api.delete_vp)(p, 1)).map(|()| "ok".into()),
        );
    }
    step("another partition's setup, after ACG", setup(&api, 1).map(|_| "ok".into()));
}
