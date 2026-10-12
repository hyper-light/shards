//! What CPUID a WHP partition's vCPUs see, and what a VMM may change of it (PM M206). A
//! Linux guest's AP skips calibrating its delay loop where the TSC is constant and a CPU of
//! its package is up already (arch/x86/kernel/tsc.c, `calibrate_delay_is_known`); without
//! either, each AP measures the boot CPU's ticks for itself. So: the host's processor
//! features (TscInvariantSupport) and its own CPUID; the CPUID each of four vCPUs of a
//! partition set up as shards sets one up is given (WHvGetVirtualProcessorCpuidOutput), and
//! what a guest reads on vCPU 0; the same given the host's processor features, with
//! invariant TSC asked of them, and with per-vCPU results
//! (WHvPartitionPropertyCodeCpuidResultList2) set before setup, after it, and with masks;
//! and what a list for 64 vCPUs costs to set, and to set up with (n = 20, interleaved with
//! partitions set up without).

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::time::{Duration, Instant};

use whp_run::{Aligned, Api, bind, capability, check, reg64, segment, summary};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};

/// shards' synthetic features (crates/vmm/src/hv/whp/mod.rs, SYNTHETIC; PM M198).
const SYNTHETIC: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 3)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 11)
    | (1 << 24)
    | (1 << 25)
    | (1 << 26);
const VCPUS: u32 = 4;
/// WHV_X64_PROCESSOR_FEATURES1's TscInvariantSupport, in the second bank.
const TSC_INVARIANT: u64 = 1 << 1;
const VP: WHV_X64_CPUID_RESULT2_FLAGS = WHvX64CpuidResult2FlagVpSpecific;
const SUB: WHV_X64_CPUID_RESULT2_FLAGS = WHvX64CpuidResult2FlagSubleafSpecific;
const ALL: WHV_CPUID_OUTPUT = WHV_CPUID_OUTPUT {
    Eax: !0,
    Ebx: !0,
    Ecx: !0,
    Edx: !0,
};
const NONE: WHV_CPUID_OUTPUT = WHV_CPUID_OUTPUT {
    Eax: 0,
    Ebx: 0,
    Ecx: 0,
    Edx: 0,
};

/// The leaves read, (function, subleaf): the maxima, leaf 1, the caches (4, 0x8000001D),
/// the topologies (0xB, 0x1F, 0x80000008, 0x8000001E), the extended features (TOPOEXT),
/// invariant TSC (0x80000007) and the hypervisor's features.
const LEAVES: &[(u32, u32)] = &[
    (0, 0),
    (1, 0),
    (4, 0),
    (4, 1),
    (4, 2),
    (4, 3),
    (4, 4),
    (0xb, 0),
    (0xb, 1),
    (0xb, 2),
    (0x1f, 0),
    (0x1f, 1),
    (0x1f, 2),
    (0x8000_0000, 0),
    (0x8000_0001, 0),
    (0x8000_0007, 0),
    (0x8000_0008, 0),
    (0x8000_001d, 0),
    (0x8000_001d, 1),
    (0x8000_001d, 2),
    (0x8000_001d, 3),
    (0x8000_001d, 4),
    (0x8000_001e, 0),
    (0x4000_0003, 0),
];

/// The guest's memory: 2 MiB from GPA 0; its code, its results, its page tables in it.
const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const RESULTS: u64 = 0x4000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// Where the guest says it is done: a port write, which exits.
const DONE: u16 = 0x501;
const WATCHDOG: Duration = Duration::from_secs(30);
const N: usize = 20;

#[derive(Clone, Copy, PartialEq)]
enum Features {
    /// None given: the partition's own.
    Default,
    /// The host's, as WHvGetCapability gives them.
    Host,
    /// The host's, and invariant TSC.
    Invariant,
}

fn out(eax: u32, ebx: u32, ecx: u32, edx: u32) -> WHV_CPUID_OUTPUT {
    WHV_CPUID_OUTPUT {
        Eax: eax,
        Ebx: ebx,
        Ecx: ecx,
        Edx: edx,
    }
}

fn result(
    function: u32,
    index: u32,
    vp: u32,
    flags: WHV_X64_CPUID_RESULT2_FLAGS,
    output: WHV_CPUID_OUTPUT,
    mask: WHV_CPUID_OUTPUT,
) -> WHV_X64_CPUID_RESULT2 {
    WHV_X64_CPUID_RESULT2 {
        Function: function,
        Index: index,
        VpIndex: vp,
        Flags: flags,
        Output: output,
        Mask: mask,
    }
}

/// One package of `vcpus` single-thread cores, as shards gives KVM's guests
/// (crates/vmm/src/arch/x86_64/cpuid.rs). Per vCPU: leaf 1's logical processors and HTT
/// alone, masked so (its APIC id left to the hypervisor); leaf 0xB's levels, SMT then
/// core; 0x8000001E's APIC and core ids. For all: 0x80000008's threads and APIC id bits,
/// masked so; and invariant TSC asked of 0x80000007.
fn topology(vcpus: u32) -> Vec<WHV_X64_CPUID_RESULT2> {
    let bits = 32 - vcpus.saturating_sub(1).leading_zeros();
    let mut v = Vec::new();
    for vp in 0..vcpus {
        v.push(result(
            1,
            0,
            vp,
            VP,
            out(0, vcpus << 16, 0, 1 << 28),
            out(0, 0xff << 16, 0, 1 << 28),
        ));
        v.push(result(0xb, 0, vp, VP | SUB, out(0, 1, 1 << 8, vp), ALL));
        v.push(result(
            0xb,
            1,
            vp,
            VP | SUB,
            out(bits, vcpus, (2 << 8) | 1, vp),
            ALL,
        ));
        v.push(result(0xb, 2, vp, VP | SUB, out(0, 0, 2, vp), ALL));
        v.push(result(0x8000_001e, 0, vp, VP, out(vp, vp, 0, 0), ALL));
    }
    v.push(result(
        0x8000_0008,
        0,
        0,
        0,
        out(0, 0, (bits << 12) | (vcpus - 1), 0),
        out(0, 0, 0xf0ff, 0),
    ));
    v.push(result(
        0x8000_0007,
        0,
        0,
        0,
        out(0, 0, 0, 1 << 8),
        out(0, 0, 0, 1 << 8),
    ));
    v
}

fn set<T>(
    api: &Api,
    p: WHV_PARTITION_HANDLE,
    code: WHV_PARTITION_PROPERTY_CODE,
    value: &T,
    what: &str,
) -> Result<(), String> {
    // SAFETY: a property of its own size.
    check(what, unsafe {
        (api.set_property)(
            p,
            code,
            (value as *const T).cast(),
            std::mem::size_of::<T>() as u32,
        )
    })
}

fn set_results(api: &Api, p: WHV_PARTITION_HANDLE, list: &[WHV_X64_CPUID_RESULT2]) -> Result<(), String> {
    // SAFETY: the list, of its own size.
    check("CpuidResultList2", unsafe {
        (api.set_property)(
            p,
            WHvPartitionPropertyCodeCpuidResultList2,
            list.as_ptr().cast(),
            std::mem::size_of_val(list) as u32,
        )
    })
}

fn host_synthetic(api: &Api) -> u64 {
    // SAFETY: the union member the code asked fills.
    capability(api, WHvCapabilityCodeSyntheticProcessorFeaturesBanks).map_or(0, |c| unsafe {
        c.SyntheticProcessorFeaturesBanks.Anonymous.AsUINT64[0]
    })
}

/// A partition of `vcpus` as shards sets one up (XApic, its synthetic features), given
/// `features` and `results`, before setup or after it; its vCPUs made.
fn make(
    api: &Api,
    label: &str,
    vcpus: u32,
    features: Features,
    results: &[WHV_X64_CPUID_RESULT2],
    after: bool,
) -> Result<WHV_PARTITION_HANDLE, String> {
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: a handle for the call to fill.
    check("WHvCreatePartition", unsafe {
        (api.create_partition)(&raw mut p)
    })?;
    let made = (|| {
        set(
            api,
            p,
            WHvPartitionPropertyCodeProcessorCount,
            &vcpus,
            "ProcessorCount",
        )?;
        let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
        set(
            api,
            p,
            WHvPartitionPropertyCodeLocalApicEmulationMode,
            &mode,
            "LocalApicEmulationMode",
        )?;
        let synthetic = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
            BanksCount: 1,
            Reserved0: 0,
            Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 {
                AsUINT64: [host_synthetic(api) & SYNTHETIC],
            },
        };
        set(
            api,
            p,
            WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
            &synthetic,
            "SyntheticProcessorFeaturesBanks",
        )?;
        if features != Features::Default {
            let cap = capability(api, WHvCapabilityCodeProcessorFeaturesBanks)
                .ok_or("no ProcessorFeaturesBanks capability")?;
            // SAFETY: the union member the code asked fills.
            let mut banks = unsafe { cap.ProcessorFeaturesBanks };
            if features == Features::Invariant {
                // SAFETY: the banks as words, of the union's own size.
                unsafe { banks.Anonymous.AsUINT64[1] |= TSC_INVARIANT };
            }
            set(
                api,
                p,
                WHvPartitionPropertyCodeProcessorFeaturesBanks,
                &banks,
                "ProcessorFeaturesBanks",
            )?;
        }
        if !results.is_empty() && !after {
            set_results(api, p, results)?;
        }
        // SAFETY: the partition just configured.
        check("WHvSetupPartition", unsafe { (api.setup_partition)(p) })?;
        for vp in 0..vcpus {
            // SAFETY: the partition just set up.
            check("WHvCreateVirtualProcessor", unsafe { (api.create_vp)(p, vp, 0) })?;
        }
        if !results.is_empty() && after {
            println!(
                "{label} results set after setup: {:?}",
                set_results(api, p, results)
            );
        }
        Ok::<(), String>(())
    })();
    if let Err(e) = made {
        // SAFETY: the partition this function made, which nothing else holds.
        unsafe { (api.delete_partition)(p) };
        return Err(e);
    }
    Ok(p)
}

fn read(
    api: &Api,
    p: WHV_PARTITION_HANDLE,
    vp: u32,
    (function, index): (u32, u32),
) -> Result<[u32; 4], String> {
    let mut o = WHV_CPUID_OUTPUT::default();
    // SAFETY: an output of its own size.
    check("WHvGetVirtualProcessorCpuidOutput", unsafe {
        (api.cpuid_output)(p, vp, function, index, &raw mut o)
    })?;
    Ok([o.Eax, o.Ebx, o.Ecx, o.Edx])
}

fn show(r: &Result<[u32; 4], String>) -> String {
    match r {
        Ok([a, b, c, d]) => format!("eax={a:#010x} ebx={b:#010x} ecx={c:#010x} edx={d:#010x}"),
        Err(e) => e.clone(),
    }
}

fn name((function, index): (u32, u32)) -> String {
    format!("{function:#x}.{index}")
}

/// Every leaf of vCPU 0, and of the others where they differ from vCPU 0's.
fn dump(api: &Api, p: WHV_PARTITION_HANDLE, label: &str, vcpus: u32) -> Vec<Result<[u32; 4], String>> {
    let first: Vec<_> = LEAVES.iter().map(|&l| read(api, p, 0, l)).collect();
    for (&l, r) in LEAVES.iter().zip(&first) {
        println!("{label} vp0 {}: {}", name(l), show(r));
    }
    for vp in 1..vcpus {
        for (&l, r0) in LEAVES.iter().zip(&first) {
            let r = read(api, p, vp, l);
            if r != *r0 {
                println!("{label} vp{vp} {}: {}", name(l), show(&r));
            }
        }
    }
    first
}

/// The guest's code, hand-assembled (x86-64): each leaf of `LEAVES` asked of CPUID, its
/// four registers stored from `RESULTS` on, then a write to `DONE`.
fn guest_code() -> Vec<u8> {
    let mut c = vec![0xBF]; // mov edi, RESULTS
    c.extend((RESULTS as u32).to_le_bytes());
    for &(function, index) in LEAVES {
        c.push(0xB8); // mov eax, function
        c.extend(function.to_le_bytes());
        c.push(0xB9); // mov ecx, index
        c.extend(index.to_le_bytes());
        c.extend([0x0F, 0xA2]); // cpuid
        // mov [rdi], eax; mov [rdi+4], ebx; mov [rdi+8], ecx; mov [rdi+12], edx
        c.extend([0x89, 0x07, 0x89, 0x5F, 0x04, 0x89, 0x4F, 0x08, 0x89, 0x57, 0x0C]);
        c.extend([0x48, 0x83, 0xC7, 0x10]); // add rdi, 16
    }
    c.extend([0x66, 0xBA]); // mov dx, DONE
    c.extend(DONE.to_le_bytes());
    c.extend([0xEE, 0xF4]); // out dx, al; hlt
    c
}

/// What the guest reads of `LEAVES` on vCPU 0, in long mode.
fn guest(api: &Api, p: WHV_PARTITION_HANDLE) -> Result<Vec<[u32; 4]>, String> {
    // SAFETY: a fresh allocation, kept for the probe's life (the partition maps it).
    let ram =
        unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
    if ram.is_null() {
        return Err("VirtualAlloc".into());
    }
    // SAFETY: the allocation above, RAM bytes.
    let mem = unsafe { std::slice::from_raw_parts_mut(ram, RAM) };
    let code = guest_code();
    mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
    // PML4[0] -> PDPT; PDPT[0]: a 1 GiB page at 0.
    mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
    mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
    let names = [
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterSs,
        WHvX64RegisterRip,
        WHvX64RegisterRflags,
        WHvX64RegisterRsp,
    ];
    let values = Aligned([
        reg64(0x8000_0031),
        reg64(PML4),
        reg64(0x20),
        reg64(0x500),
        segment(0x8, 0xA09B),
        segment(0x10, 0xC093),
        segment(0x10, 0xC093),
        segment(0x10, 0xC093),
        reg64(CODE),
        reg64(0x2),
        reg64(0x8000),
    ]);
    // SAFETY: memory that outlives the partition; registers of vCPU 0, 16-aligned.
    unsafe {
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
        check(
            "registers",
            (api.set_regs)(p, 0, names.as_ptr(), names.len() as u32, values.0.as_ptr()),
        )?;
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    let cancel = api.cancel;
    let ended = std::thread::scope(|scope| {
        scope.spawn(|| {
            let until = Instant::now() + WATCHDOG;
            while !done.load(std::sync::atomic::Ordering::Relaxed) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !done.load(std::sync::atomic::Ordering::Relaxed) {
                // SAFETY: the partition, alive in this scope.
                unsafe { cancel(p, 0, 0) };
            }
        });
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        // SAFETY: an exit context of its own size, 16-aligned.
        let hr = unsafe {
            (api.run)(
                p,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        check("WHvRunVirtualProcessor", hr)?;
        let reason = exit.0.ExitReason;
        // SAFETY: the union member a port exit fills, read only for one.
        if reason == WHvRunVpExitReasonX64IoPortAccess
            && unsafe { exit.0.Anonymous.IoPortAccess.PortNumber } == DONE
        {
            Ok(())
        } else {
            Err(format!("exit {reason:#x} at rip {:#x}", exit.0.VpContext.Rip))
        }
    });
    ended?;
    let word = |at: usize| u32::from_le_bytes(mem[at..at + 4].try_into().unwrap_or([0; 4]));
    Ok((0..LEAVES.len())
        .map(|i| {
            let at = RESULTS as usize + 16 * i;
            [word(at), word(at + 4), word(at + 8), word(at + 12)]
        })
        .collect())
}

/// The host's own CPUID, as this process reads it.
#[allow(unused_unsafe)]
fn host_cpuid((function, index): (u32, u32)) -> [u32; 4] {
    // SAFETY: CPUID, which every x86-64 processor has.
    let r = unsafe { std::arch::x86_64::__cpuid_count(function, index) };
    [r.eax, r.ebx, r.ecx, r.edx]
}

fn host(api: &Api) {
    // SAFETY: each union member as its code asked fills it.
    unsafe {
        println!(
            "processor_vendor={} processor_clock_hz={}",
            capability(api, WHvCapabilityCodeProcessorVendor).map_or(-1, |c| c.ProcessorVendor),
            capability(api, WHvCapabilityCodeProcessorClockFrequency)
                .map_or(0, |c| c.ProcessorClockFrequency)
        );
        match capability(api, WHvCapabilityCodeProcessorFeaturesBanks) {
            Some(c) => {
                let b = c.ProcessorFeaturesBanks;
                println!(
                    "processor_features banks={} bank0={:#018x} bank1={:#018x} tsc_invariant={}",
                    b.BanksCount,
                    b.Anonymous.AsUINT64[0],
                    b.Anonymous.AsUINT64[1],
                    b.Anonymous.AsUINT64[1] & TSC_INVARIANT != 0
                );
            }
            None => println!("processor_features: not answered"),
        }
        println!("synthetic_features={:#x}", host_synthetic(api));
    }
    for l in [
        (1, 0),
        (0xb, 0),
        (0xb, 1),
        (0x8000_0001, 0),
        (0x8000_0007, 0),
        (0x8000_0008, 0),
        (0x8000_001e, 0),
    ] {
        let [a, b, c, d] = host_cpuid(l);
        println!(
            "host cpuid {}: eax={a:#010x} ebx={b:#010x} ecx={c:#010x} edx={d:#010x}",
            name(l)
        );
    }
    // A new partition's own processor features, before anything is set.
    let mut p: WHV_PARTITION_HANDLE = 0;
    // SAFETY: a handle for the call to fill; the property of its own size; the partition
    // deleted after.
    unsafe {
        if check("WHvCreatePartition", (api.create_partition)(&raw mut p)).is_ok() {
            let mut banks = WHV_PROCESSOR_FEATURES_BANKS {
                BanksCount: 2,
                ..Default::default()
            };
            let mut written = 0u32;
            let hr = (api.get_property)(
                p,
                WHvPartitionPropertyCodeProcessorFeaturesBanks,
                (&raw mut banks).cast(),
                std::mem::size_of_val(&banks) as u32,
                &raw mut written,
            );
            match check("WHvGetPartitionProperty", hr) {
                Ok(()) => println!(
                    "partition processor_features banks={} bank0={:#018x} bank1={:#018x}",
                    banks.BanksCount, banks.Anonymous.AsUINT64[0], banks.Anonymous.AsUINT64[1]
                ),
                Err(e) => println!("partition processor_features: {e}"),
            }
            (api.delete_partition)(p);
        }
    }
}

/// What a list for 64 vCPUs costs to set, and setup with it and without, interleaved.
fn cost(api: &Api) {
    let vcpus = 64;
    let list = topology(vcpus);
    let (mut setting, mut with, mut without) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..N {
        for listed in [true, false] {
            let mut p: WHV_PARTITION_HANDLE = 0;
            // SAFETY: a handle for the call to fill.
            if let Err(e) = check("WHvCreatePartition", unsafe {
                (api.create_partition)(&raw mut p)
            }) {
                println!("cost: {e}");
                return;
            }
            let r = (|| {
                set(
                    api,
                    p,
                    WHvPartitionPropertyCodeProcessorCount,
                    &vcpus,
                    "ProcessorCount",
                )?;
                let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
                set(
                    api,
                    p,
                    WHvPartitionPropertyCodeLocalApicEmulationMode,
                    &mode,
                    "LocalApicEmulationMode",
                )?;
                let synthetic = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
                    BanksCount: 1,
                    Reserved0: 0,
                    Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 {
                        AsUINT64: [host_synthetic(api) & SYNTHETIC],
                    },
                };
                set(
                    api,
                    p,
                    WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
                    &synthetic,
                    "SyntheticProcessorFeaturesBanks",
                )?;
                if listed {
                    let t = Instant::now();
                    set_results(api, p, &list)?;
                    setting.push(t.elapsed());
                }
                let t = Instant::now();
                // SAFETY: the partition just configured.
                check("WHvSetupPartition", unsafe { (api.setup_partition)(p) })?;
                if listed { &mut with } else { &mut without }.push(t.elapsed());
                Ok::<(), String>(())
            })();
            // SAFETY: the partition made above, which nothing else holds.
            unsafe { (api.delete_partition)(p) };
            if let Err(e) = r {
                println!("cost: {e}");
                return;
            }
        }
    }
    println!("cost entries={} set_us {}", list.len(), summary(&mut setting));
    println!("cost setup_with_us {}", summary(&mut with));
    println!("cost setup_without_us {}", summary(&mut without));
}

fn main() {
    let api = match bind() {
        Ok(api) => api,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    host(&api);
    let topo = topology(VCPUS);
    // Mask semantics: leaf 1's processor count with no bit masked; and leaf 1 whole, for
    // every vCPU, its APIC id 0.
    let unmasked = [result(1, 0, 0, 0, out(0, 0x12 << 16, 0, 0), NONE)];
    let flat = [result(1, 0, 0, 0, out(0, VCPUS << 16, 0, 1 << 28), ALL)];
    let variants: [(&str, Features, &[WHV_X64_CPUID_RESULT2], bool); 7] = [
        ("shards", Features::Default, &[], false),
        ("host-features", Features::Host, &[], false),
        ("invariant", Features::Invariant, &[], false),
        ("topology", Features::Default, &topo, false),
        ("topology-after", Features::Default, &topo, true),
        ("unmasked", Features::Default, &unmasked, false),
        ("flat", Features::Default, &flat, false),
    ];
    for (label, features, results, after) in variants {
        let p = match make(&api, label, VCPUS, features, results, after) {
            Ok(p) => p,
            Err(e) => {
                println!("{label}: {e}");
                continue;
            }
        };
        let api_view = dump(&api, p, label, VCPUS);
        match guest(&api, p) {
            Ok(seen) => {
                let mut agrees = true;
                for ((&l, r), g) in LEAVES.iter().zip(&api_view).zip(&seen) {
                    if r.as_ref().ok() != Some(g) {
                        agrees = false;
                        println!("{label} guest vp0 {}: {}", name(l), show(&Ok(*g)));
                    }
                }
                if agrees {
                    println!("{label} guest vp0 agrees with WHvGetVirtualProcessorCpuidOutput");
                }
            }
            Err(e) => println!("{label} guest: {e}"),
        }
        // SAFETY: the partition made above, which nothing else holds.
        unsafe { (api.delete_partition)(p) };
    }
    cost(&api);
}
