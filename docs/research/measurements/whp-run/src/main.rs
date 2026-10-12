//! A WHP partition run on a real Windows host (PM M197), before shards' WHP backend is
//! written: what the hypervisor offers (capabilities), what a guest of it sees of its
//! processor and its hypervisor (CPUID's hypervisor leaves, the frequency MSRs a Linux
//! guest reads in their place of kvm-clock), and what an exit costs: an MMIO write and a
//! port write, each emulated by WinHvEmulation as a VMM would, timed from the host, n of
//! each. Twice: a partition as created, and one given every synthetic (Hyper-V) processor
//! feature the host offers. Its functions are bound at run time (LoadLibraryExW,
//! GetProcAddress), as shards binds them, so that the binary starts where they are not.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
use windows_sys::core::HRESULT;

/// The guest's memory: 2 MiB from GPA 0; its code, its results, its page tables in it.
const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
const RESULTS: u64 = 0x2000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// Where its MMIO writes go: a GPA nothing maps.
const MMIO: u32 = 0xD000_0000;
const PORT: u16 = 0x3F8;
/// The writes of each kind timed.
const ROUNDS: u32 = 1000;
/// The CPUID leaves recorded: the hypervisor's (0x40000000-6), leaf 1 (its present bit) and
/// 0x80000007 (invariant TSC).
const LEAVES: [u32; 9] = [
    0x4000_0000,
    0x4000_0001,
    0x4000_0002,
    0x4000_0003,
    0x4000_0004,
    0x4000_0005,
    0x4000_0006,
    0x0000_0001,
    0x8000_0007,
];

type GetCapability = unsafe extern "system" fn(WHV_CAPABILITY_CODE, *mut c_void, u32, *mut u32) -> HRESULT;
type CreatePartition = unsafe extern "system" fn(*mut WHV_PARTITION_HANDLE) -> HRESULT;
type SetPartitionProperty = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE,
    *const c_void,
    u32,
) -> HRESULT;
type SetupPartition = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
type DeletePartition = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
type MapGpaRange = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    *const c_void,
    u64,
    u64,
    WHV_MAP_GPA_RANGE_FLAGS,
) -> HRESULT;
type CreateVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, u32) -> HRESULT;
type DeleteVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32) -> HRESULT;
type SetRegs = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *const WHV_REGISTER_VALUE,
) -> HRESULT;
type GetRegs = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *mut WHV_REGISTER_VALUE,
) -> HRESULT;
type RunVp = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, *mut c_void, u32) -> HRESULT;
type TranslateGva = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    u64,
    WHV_TRANSLATE_GVA_FLAGS,
    *mut WHV_TRANSLATE_GVA_RESULT,
    *mut u64,
) -> HRESULT;
type EmuCreate = unsafe extern "system" fn(*const WHV_EMULATOR_CALLBACKS, *mut *mut c_void) -> HRESULT;
type EmuMmio = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_MEMORY_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;
type EmuIo = unsafe extern "system" fn(
    *const c_void,
    *const c_void,
    *const WHV_VP_EXIT_CONTEXT,
    *const WHV_X64_IO_PORT_ACCESS_CONTEXT,
    *mut WHV_EMULATOR_STATUS,
) -> HRESULT;
type EmuDestroy = unsafe extern "system" fn(*const c_void) -> HRESULT;

/// WinHvPlatform's and WinHvEmulation's functions, bound.
struct Api {
    get_capability: GetCapability,
    create_partition: CreatePartition,
    set_property: SetPartitionProperty,
    setup_partition: SetupPartition,
    delete_partition: DeletePartition,
    map: MapGpaRange,
    create_vp: CreateVp,
    delete_vp: DeleteVp,
    set_regs: SetRegs,
    get_regs: GetRegs,
    run: RunVp,
    translate: TranslateGva,
    emu_create: EmuCreate,
    emu_mmio: EmuMmio,
    emu_io: EmuIo,
    emu_destroy: EmuDestroy,
}

/// A system DLL, loaded from System32 alone.
fn library(name: &str) -> Result<*mut c_void, String> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated wide string.
    let lib = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if lib.is_null() {
        return Err(format!("{name}: {}", std::io::Error::last_os_error()));
    }
    Ok(lib)
}

/// Function `name` of `lib`, as type `T`, a function pointer of its signature.
unsafe fn sym<T: Copy>(lib: *mut c_void, name: &str) -> Result<T, String> {
    let c: Vec<u8> = name.bytes().chain([0]).collect();
    // SAFETY: a module handle LoadLibraryExW gave, and a NUL-terminated name.
    match unsafe { GetProcAddress(lib, c.as_ptr()) } {
        // SAFETY: the caller names `T` as the function's own signature, a pointer's size.
        Some(f) => Ok(unsafe { std::mem::transmute_copy(&f) }),
        None => Err(format!("{name}: {}", std::io::Error::last_os_error())),
    }
}

fn bind() -> Result<Api, String> {
    let p = library("WinHvPlatform.dll")?;
    let e = library("WinHvEmulation.dll")?;
    // SAFETY: each name with its own signature, from WinHvPlatform.h and WinHvEmulation.h.
    unsafe {
        Ok(Api {
            get_capability: sym(p, "WHvGetCapability")?,
            create_partition: sym(p, "WHvCreatePartition")?,
            set_property: sym(p, "WHvSetPartitionProperty")?,
            setup_partition: sym(p, "WHvSetupPartition")?,
            delete_partition: sym(p, "WHvDeletePartition")?,
            map: sym(p, "WHvMapGpaRange")?,
            create_vp: sym(p, "WHvCreateVirtualProcessor")?,
            delete_vp: sym(p, "WHvDeleteVirtualProcessor")?,
            set_regs: sym(p, "WHvSetVirtualProcessorRegisters")?,
            get_regs: sym(p, "WHvGetVirtualProcessorRegisters")?,
            run: sym(p, "WHvRunVirtualProcessor")?,
            translate: sym(p, "WHvTranslateGva")?,
            emu_create: sym(e, "WHvEmulatorCreateEmulator")?,
            emu_mmio: sym(e, "WHvEmulatorTryMmioEmulation")?,
            emu_io: sym(e, "WHvEmulatorTryIoEmulation")?,
            emu_destroy: sym(e, "WHvEmulatorDestroyEmulator")?,
        })
    }
}

fn check(what: &str, hr: HRESULT) -> Result<(), String> {
    if hr < 0 {
        return Err(format!("{what}: HRESULT {:#010x}", hr as u32));
    }
    Ok(())
}

/// Capability `code` as its union, and whether the host answered it.
fn capability(api: &Api, code: WHV_CAPABILITY_CODE) -> Option<WHV_CAPABILITY> {
    let mut cap = WHV_CAPABILITY::default();
    let mut written = 0u32;
    // SAFETY: a buffer of the union's size, which the call fills.
    let hr = unsafe {
        (api.get_capability)(
            code,
            (&raw mut cap).cast(),
            std::mem::size_of::<WHV_CAPABILITY>() as u32,
            &raw mut written,
        )
    };
    (hr >= 0).then_some(cap)
}

/// The guest's code, hand-assembled (x86-64): each leaf of `LEAVES` asked of CPUID and its
/// four registers stored from `RESULTS` on; then, where leaf 0x40000003's EAX says the
/// frequency MSRs may be read (bit 11, AccessFrequencyRegs), HV_X64_MSR_TSC_FREQUENCY and
/// HV_X64_MSR_APIC_FREQUENCY stored after them; an MMIO write and a port write, each
/// once and then `ROUNDS` times; and HLT.
fn guest_code() -> Vec<u8> {
    let mut c = Vec::new();
    // mov edi, RESULTS
    c.push(0xBF);
    c.extend((RESULTS as u32).to_le_bytes());
    for leaf in LEAVES {
        c.push(0xB8); // mov eax, leaf
        c.extend(leaf.to_le_bytes());
        c.extend([0x31, 0xC9]); // xor ecx, ecx
        c.extend([0x0F, 0xA2]); // cpuid
        // mov [rdi], eax; mov [rdi+4], ebx; mov [rdi+8], ecx; mov [rdi+12], edx
        c.extend([0x89, 0x07, 0x89, 0x5F, 0x04, 0x89, 0x4F, 0x08, 0x89, 0x57, 0x0C]);
        c.extend([0x48, 0x83, 0xC7, 0x10]); // add rdi, 16
    }
    // mov eax, [RESULTS + 3*16]: leaf 0x40000003's EAX
    c.extend([0x8B, 0x04, 0x25]);
    c.extend(((RESULTS + 3 * 16) as u32).to_le_bytes());
    c.extend([0xA9, 0x00, 0x08, 0x00, 0x00]); // test eax, 0x800
    c.extend([0x74, 25]); // jz past the two reads
    c.push(0xB9); // mov ecx, HV_X64_MSR_TSC_FREQUENCY
    c.extend(0x4000_0022u32.to_le_bytes());
    c.extend([0x0F, 0x32]); // rdmsr
    c.extend([0x89, 0x07, 0x89, 0x57, 0x04]); // mov [rdi], eax; mov [rdi+4], edx
    c.push(0xB9); // mov ecx, HV_X64_MSR_APIC_FREQUENCY
    c.extend(0x4000_0023u32.to_le_bytes());
    c.extend([0x0F, 0x32]); // rdmsr
    c.extend([0x89, 0x47, 0x08, 0x89, 0x57, 0x0C]); // mov [rdi+8], eax; mov [rdi+12], edx
    c.push(0xBE); // mov esi, MMIO
    c.extend(MMIO.to_le_bytes());
    c.push(0xB8); // mov eax, 0x12345678
    c.extend(0x1234_5678u32.to_le_bytes());
    c.extend([0x89, 0x06]); // mov [rsi], eax
    c.extend([0x66, 0xBA]); // mov dx, PORT
    c.extend(PORT.to_le_bytes());
    c.extend([0xB0, b'A']); // mov al, 'A'
    c.push(0xEE); // out dx, al
    c.push(0xB9); // mov ecx, ROUNDS
    c.extend(ROUNDS.to_le_bytes());
    c.extend([0x89, 0x06, 0xFF, 0xC9, 0x75, 0xFA]); // 1: mov [rsi], eax; dec ecx; jnz 1b
    c.push(0xB9); // mov ecx, ROUNDS
    c.extend(ROUNDS.to_le_bytes());
    c.extend([0xEE, 0xFF, 0xC9, 0x75, 0xFB]); // 1: out dx, al; dec ecx; jnz 1b
    c.push(0xF4); // hlt
    c
}

/// What the emulator's callbacks reach: the partition, and what the guest wrote.
struct Emulated<'a> {
    api: &'a Api,
    partition: WHV_PARTITION_HANDLE,
    mmio: u32,
    pio: u32,
}

unsafe extern "system" fn io_port(ctx: *const c_void, io: *mut WHV_EMULATOR_IO_ACCESS_INFO) -> HRESULT {
    // SAFETY: the context is this probe's `Emulated`, and the access the emulator's own.
    unsafe {
        let e = &mut *(ctx as *mut Emulated<'_>);
        let io = &mut *io;
        if io.Direction == 0 {
            io.Data = 0xFF;
        } else {
            e.pio += 1;
        }
    }
    0
}

unsafe extern "system" fn memory(
    ctx: *const c_void,
    access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    // SAFETY: as `io_port`.
    unsafe {
        let e = &mut *(ctx as *mut Emulated<'_>);
        let access = &mut *access;
        if access.Direction == 0 {
            access.Data = [0; 8];
        } else {
            e.mmio += 1;
        }
    }
    0
}

unsafe extern "system" fn get_registers(
    ctx: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: as `io_port`; the registers asked, of the one vCPU.
    unsafe {
        let e = &*(ctx as *const Emulated<'_>);
        (e.api.get_regs)(e.partition, 0, names, count, values)
    }
}

unsafe extern "system" fn set_registers(
    ctx: *const c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    // SAFETY: as `get_registers`.
    unsafe {
        let e = &*(ctx as *const Emulated<'_>);
        (e.api.set_regs)(e.partition, 0, names, count, values)
    }
}

unsafe extern "system" fn translate_gva(
    ctx: *const c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    // SAFETY: as `get_registers`.
    unsafe {
        let e = &*(ctx as *const Emulated<'_>);
        let mut r = WHV_TRANSLATE_GVA_RESULT::default();
        let hr = (e.api.translate)(e.partition, 0, gva, flags, &raw mut r, gpa);
        *result = r.ResultCode;
        hr
    }
}

/// `values`, sorted: n, and the nearest-rank p50, p90, p99 and max, in µs.
fn summary(values: &mut [Duration]) -> String {
    values.sort();
    let n = values.len();
    if n == 0 {
        return "n=0".into();
    }
    let rank = |p: usize| values[(n * p).div_ceil(100).clamp(1, n) - 1].as_secs_f64() * 1e6;
    format!(
        "n={n} p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        rank(50),
        rank(90),
        rank(99),
        rank(100)
    )
}

fn segment(selector: u16, attributes: u16) -> WHV_REGISTER_VALUE {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF_FFFF,
        Selector: selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    };
    v
}

fn reg64(x: u64) -> WHV_REGISTER_VALUE {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Reg64 = x;
    v
}

/// A partition of one vCPU in long mode, run to its HLT: `synthetic`, the synthetic
/// processor features it is given, if any.
fn run(api: &Api, apic: bool, synthetic: Option<u64>) -> Result<(), String> {
    let label = match synthetic {
        Some(bank) => format!("synthetic={bank:#x}"),
        None => "as-created".into(),
    };
    let started = Instant::now();
    let mut partition: WHV_PARTITION_HANDLE = 0;
    // SAFETY: each call on the partition this function makes and deletes.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut partition))?;
        let count: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(
                partition,
                WHvPartitionPropertyCodeProcessorCount,
                (&raw const count).cast(),
                4,
            ),
        )?;
        if apic {
            let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
            check(
                "LocalApicEmulationMode",
                (api.set_property)(
                    partition,
                    WHvPartitionPropertyCodeLocalApicEmulationMode,
                    (&raw const mode).cast(),
                    4,
                ),
            )?;
        }
        if let Some(bank) = synthetic {
            let banks = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
                BanksCount: 1,
                Reserved0: 0,
                Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 { AsUINT64: [bank] },
            };
            check(
                "SyntheticProcessorFeaturesBanks",
                (api.set_property)(
                    partition,
                    WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
                    (&raw const banks).cast(),
                    std::mem::size_of_val(&banks) as u32,
                ),
            )?;
        }
        check("WHvSetupPartition", (api.setup_partition)(partition))?;
        let made = started.elapsed();
        let ram = VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        if ram.is_null() {
            return Err("VirtualAlloc".into());
        }
        let mem = std::slice::from_raw_parts_mut(ram, RAM);
        let code = guest_code();
        mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
        // PML4[0] -> PDPT; PDPT[0] and [3]: 1 GiB pages at 0 and 3 GiB (where MMIO lies).
        mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
        mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
        mem[PDPT as usize + 24..PDPT as usize + 32].copy_from_slice(&(0xC000_0000u64 | 0x83).to_le_bytes());
        let t = Instant::now();
        check(
            "WHvMapGpaRange",
            (api.map)(
                partition,
                ram.cast(),
                0,
                RAM as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        let mapped = t.elapsed();
        let t = Instant::now();
        check("WHvCreateVirtualProcessor", (api.create_vp)(partition, 0, 0))?;
        let vp = t.elapsed();
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
        let values = [
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
        ];
        check(
            "registers",
            (api.set_regs)(partition, 0, names.as_ptr(), names.len() as u32, values.as_ptr()),
        )?;
        let mut emulated = Emulated {
            api,
            partition,
            mmio: 0,
            pio: 0,
        };
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: std::mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(io_port),
            WHvEmulatorMemoryCallback: Some(memory),
            WHvEmulatorGetVirtualProcessorRegisters: Some(get_registers),
            WHvEmulatorSetVirtualProcessorRegisters: Some(set_registers),
            WHvEmulatorTranslateGvaPage: Some(translate_gva),
        };
        let mut emulator: *mut c_void = std::ptr::null_mut();
        check(
            "WHvEmulatorCreateEmulator",
            (api.emu_create)(&raw const callbacks, &raw mut emulator),
        )?;
        let (mut mmio_run, mut mmio_handle, mut pio_run, mut pio_handle) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut first: Option<Duration> = None;
        let ended = loop {
            let mut exit = WHV_RUN_VP_EXIT_CONTEXT::default();
            let t = Instant::now();
            let hr = (api.run)(
                partition,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            );
            let ran = t.elapsed();
            first.get_or_insert(ran);
            if hr < 0 {
                break format!("WHvRunVirtualProcessor HRESULT {:#010x}", hr as u32);
            }
            let t = Instant::now();
            let mut status = WHV_EMULATOR_STATUS::default();
            let ctx = (&raw mut emulated).cast::<c_void>();
            match exit.ExitReason {
                r if r == WHvRunVpExitReasonMemoryAccess => {
                    check(
                        "mmio",
                        (api.emu_mmio)(
                            emulator,
                            ctx,
                            &raw const exit.VpContext,
                            &raw const exit.Anonymous.MemoryAccess,
                            &raw mut status,
                        ),
                    )?;
                    mmio_run.push(ran);
                    mmio_handle.push(t.elapsed());
                }
                r if r == WHvRunVpExitReasonX64IoPortAccess => {
                    check(
                        "io",
                        (api.emu_io)(
                            emulator,
                            ctx,
                            &raw const exit.VpContext,
                            &raw const exit.Anonymous.IoPortAccess,
                            &raw mut status,
                        ),
                    )?;
                    pio_run.push(ran);
                    pio_handle.push(t.elapsed());
                }
                r if r == WHvRunVpExitReasonX64Halt => break "halt".to_string(),
                other => {
                    break format!("exit reason {other:#x} at rip {:#x}", exit.VpContext.Rip);
                }
            }
            if status.AsUINT32 & 1 == 0 {
                break format!("emulation failed, status {:#x}", status.AsUINT32);
            }
        };
        println!(
            "{label} ended={ended} mmio_writes={} pio_writes={}",
            emulated.mmio, emulated.pio
        );
        println!(
            "{label} partition_us={:.0} map_us={:.0} vp_us={:.0} first_run_us={:.0}",
            made.as_secs_f64() * 1e6,
            mapped.as_secs_f64() * 1e6,
            vp.as_secs_f64() * 1e6,
            first.unwrap_or_default().as_secs_f64() * 1e6
        );
        // The first of each kind ran the code before it: timed from the second on.
        for (what, v) in [
            ("mmio run", &mut mmio_run),
            ("mmio emulate", &mut mmio_handle),
            ("pio run", &mut pio_run),
            ("pio emulate", &mut pio_handle),
        ] {
            let rest = v.get(1..).map(<[_]>::to_vec).unwrap_or_default();
            let mut rest = rest;
            println!("{label} {what} {}", summary(&mut rest));
        }
        let word =
            |at: u64| u32::from_le_bytes(mem[at as usize..at as usize + 4].try_into().unwrap_or([0; 4]));
        for (i, leaf) in LEAVES.iter().enumerate() {
            let at = RESULTS + 16 * i as u64;
            let (a, b, c, d) = (word(at), word(at + 4), word(at + 8), word(at + 12));
            let text: String = [b, c, d]
                .iter()
                .flat_map(|r| r.to_le_bytes())
                .map(|x| {
                    if x.is_ascii_graphic() || x == b' ' {
                        x as char
                    } else {
                        '.'
                    }
                })
                .collect();
            println!(
                "{label} cpuid {leaf:#010x}: eax={a:#010x} ebx={b:#010x} ecx={c:#010x} edx={d:#010x} {text:?}"
            );
        }
        let msrs = RESULTS + 16 * LEAVES.len() as u64;
        let tsc = u64::from(word(msrs)) | u64::from(word(msrs + 4)) << 32;
        let apic_hz = u64::from(word(msrs + 8)) | u64::from(word(msrs + 12)) << 32;
        println!("{label} msr tsc_frequency={tsc} apic_frequency={apic_hz}");
        (api.emu_destroy)(emulator);
        (api.delete_vp)(partition, 0);
        (api.delete_partition)(partition);
    }
    Ok(())
}

fn main() {
    let api = match bind() {
        Ok(api) => api,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    let present = capability(&api, WHvCapabilityCodeHypervisorPresent);
    // SAFETY: each union read as the code asked of it fills it.
    unsafe {
        println!(
            "hypervisor_present={}",
            present.map_or(-1, |c| c.HypervisorPresent)
        );
        let features = capability(&api, WHvCapabilityCodeFeatures).map_or(0, |c| c.Features.AsUINT64);
        println!("features={features:#x}");
        println!(
            "extended_vm_exits={:#x}",
            capability(&api, WHvCapabilityCodeExtendedVmExits).map_or(0, |c| c.ExtendedVmExits.AsUINT64)
        );
        println!(
            "processor_vendor={}",
            capability(&api, WHvCapabilityCodeProcessorVendor).map_or(-1, |c| c.ProcessorVendor)
        );
        println!(
            "processor_clock_hz={} interrupt_clock_hz={}",
            capability(&api, WHvCapabilityCodeProcessorClockFrequency)
                .map_or(0, |c| c.ProcessorClockFrequency),
            capability(&api, WHvCapabilityCodeInterruptClockFrequency)
                .map_or(0, |c| c.InterruptClockFrequency)
        );
        let synthetic = capability(&api, WHvCapabilityCodeSyntheticProcessorFeaturesBanks).map(|c| {
            (
                c.SyntheticProcessorFeaturesBanks.BanksCount,
                c.SyntheticProcessorFeaturesBanks.Anonymous.AsUINT64[0],
            )
        });
        println!("synthetic_features={synthetic:x?}");
        // LocalApicEmulation is WHV_CAPABILITY_FEATURES' second bit.
        let apic = features & 0b10 != 0;
        if let Err(e) = run(&api, apic, None) {
            println!("as-created failed: {e}");
        }
        if let Some((_, bank)) = synthetic
            && let Err(e) = run(&api, apic, Some(bank))
        {
            println!("synthetic failed: {e}");
        }
    }
}
