//! What a WHP partition does that shards' WHP backend leans on (PM M198), measured on
//! GitHub's Windows runners before the backend is written, each as a guest of two vCPUs
//! with its local APIC emulated by the hypervisor and the synthetic features shards offers:
//!
//! - an application processor: what it does as created, and whether INIT and SIPI from
//!   the boot processor's emulated APIC start it, held in wait-for-SIPI;
//! - interrupts asked for by the VMM (WHvRequestInterrupt), edge- and level-triggered:
//!   what each call costs, how long until the guest's handler has run, and which EOIs
//!   come back to the VMM as exits;
//! - a register read and write between exits, as a VMM's own MMIO decoder would make;
//! - a guest's writes to a doorbell (a notification port, WHvCreateNotificationPort), and
//!   a round trip through one to a host thread, against a round trip through an MMIO exit
//!   whose VMM wakes the same thread;
//! - a cancel before a run, and one while the vCPU is halted in the hypervisor.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use whp_run::{Aligned, Api, bind, capability, check, reg64, segment, summary};

const RAM: usize = 2 << 20;
const CODE: u64 = 0x1000;
/// The guest's results and the host's answers, from 0x2000.
const COUNT: u32 = 0x2000;
const ACK2: u32 = 0x2008;
const ACK3: u32 = 0x200C;
const T_E1: u32 = 0x2010;
const T_E2: u32 = 0x2018;
const T_E3: u32 = 0x2020;
const TSC_HZ: u32 = 0x2028;
const AP_MARK: u32 = 0x2100;
const LEAVES_AT: u32 = 0x2200;
const IDT: u64 = 0x3000;
const GDT: u64 = 0x4000;
const HANDLER: u64 = 0x5000;
const SPURIOUS: u64 = 0x5100;
const STACK: u64 = 0x7000;
/// Where SIPI starts the application processor: vector 8.
const AP_CODE: u64 = 0x8000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;

const VECTOR: u32 = 0x40;
const DONE: u16 = 0x501;
const READY: u16 = 0x502;
const AP_DONE: u16 = 0x503;
const DONE2: u16 = 0x504;
/// Doorbells, and an MMIO address that is none: no RAM is mapped at any.
const DB1: u32 = 0xD000_1000;
const DB2: u32 = 0xD000_2000;
const MMIO3: u32 = 0xD000_3000;
const N_EDGE: u32 = 200;
const N_LEVEL: u32 = 200;
const N_DB1: u32 = 1000;
const N_RT: u32 = 1000;
const ROUNDS: usize = 1000;
const WATCHDOG: Duration = Duration::from_secs(60);
/// CPUID leaves and subleaves the guest records.
const LEAVES: [(u32, u32); 5] = [(0x4000_0003, 0), (0x4000_0004, 0), (1, 0), (0xB, 0), (0xB, 1)];

/// The synthetic processor features offered (WinHvPlatformDefs.h's
/// WHV_SYNTHETIC_PROCESSOR_FEATURES): the hypervisor's presence and HV#1, the partition's
/// reference counter and TSC page and the frequency MSRs (time), synthetic timers in
/// direct mode, the APIC's synthetic MSRs and VP assist page, hypercalls, the VP index, and
/// the TLB-flush and IPI hypercalls with extended processor masks.
const SYNTHETIC: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 3)
    | (1 << 5)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 11)
    | (1 << 22)
    | (1 << 24)
    | (1 << 25)
    | (1 << 26);

/// Hand assembly, x86-64.
#[derive(Default)]
struct Asm {
    c: Vec<u8>,
}

impl Asm {
    fn b(&mut self, bytes: &[u8]) -> &mut Self {
        self.c.extend_from_slice(bytes);
        self
    }
    fn d(&mut self, v: u32) -> &mut Self {
        self.c.extend(v.to_le_bytes());
        self
    }
    fn mov_eax(&mut self, v: u32) {
        self.b(&[0xB8]).d(v);
    }
    fn mov_ecx(&mut self, v: u32) {
        self.b(&[0xB9]).d(v);
    }
    fn mov_esi(&mut self, v: u32) {
        self.b(&[0xBE]).d(v);
    }
    /// mov [rsi], eax
    fn store_eax_rsi(&mut self) {
        self.b(&[0x89, 0x06]);
    }
    /// mov [rsi], ecx
    fn store_ecx_rsi(&mut self) {
        self.b(&[0x89, 0x0E]);
    }
    /// mov eax, [abs32]
    fn load_eax(&mut self, at: u32) {
        self.b(&[0x8B, 0x04, 0x25]).d(at);
    }
    /// mov [abs32], rax
    fn store_rax(&mut self, at: u32) {
        self.b(&[0x48, 0x89, 0x04, 0x25]).d(at);
    }
    /// rdtsc; shl rdx, 32; or rax, rdx
    fn tsc_rax(&mut self) {
        self.b(&[0x0F, 0x31, 0x48, 0xC1, 0xE2, 0x20, 0x48, 0x09, 0xD0]);
    }
    /// mov dx, port; out dx, al
    fn out(&mut self, port: u16) {
        self.b(&[0x66, 0xBA]);
        self.c.extend(port.to_le_bytes());
        self.b(&[0xEE]);
    }
    fn here(&self) -> usize {
        self.c.len()
    }
    /// A short jump `op` (jb, jnz, jmp) back to `target`.
    fn jump(&mut self, op: u8, target: usize) {
        let rel = target as isize - (self.c.len() as isize + 2);
        self.b(&[op, rel as i8 as u8]);
    }
}

/// The boot processor's code: CPUID and the TSC's frequency recorded; its APIC enabled; the
/// AP started (INIT, SIPI); then halts with interrupts on until the host's interrupts have
/// all been handled; then the doorbells (E1: writes alone, E2: round trips) and the MMIO
/// round trips (E3), each timed with the TSC; then DONE, DONE2, and halts for good.
fn bsp_code() -> Vec<u8> {
    let mut a = Asm::default();
    a.b(&[0xBF]).d(LEAVES_AT); // mov edi, LEAVES_AT
    for (leaf, sub) in LEAVES {
        a.mov_eax(leaf);
        a.mov_ecx(sub);
        a.b(&[0x0F, 0xA2]); // cpuid
        // mov [rdi], eax; mov [rdi+4], ebx; mov [rdi+8], ecx; mov [rdi+12], edx
        a.b(&[0x89, 0x07, 0x89, 0x5F, 0x04, 0x89, 0x4F, 0x08, 0x89, 0x57, 0x0C]);
        a.b(&[0x48, 0x83, 0xC7, 0x10]); // add rdi, 16
    }
    a.mov_ecx(0x4000_0022); // HV_X64_MSR_TSC_FREQUENCY
    a.b(&[0x0F, 0x32]); // rdmsr
    a.b(&[0x89, 0x04, 0x25]).d(TSC_HZ); // mov [TSC_HZ], eax
    a.b(&[0x89, 0x14, 0x25]).d(TSC_HZ + 4); // mov [TSC_HZ+4], edx
    a.mov_esi(0xFEE0_00F0); // the spurious-interrupt vector register: enabled, 0xFF
    a.mov_eax(0x1FF);
    a.store_eax_rsi();
    a.mov_esi(0xFEE0_0310); // ICR high: APIC ID 1
    a.mov_eax(1 << 24);
    a.store_eax_rsi();
    a.mov_esi(0xFEE0_0300); // ICR low: INIT, then SIPI to AP_CODE
    a.mov_eax(0x4500);
    a.store_eax_rsi();
    a.mov_eax(0x4600 | (AP_CODE >> 12) as u32);
    a.store_eax_rsi();
    a.out(READY);
    a.b(&[0xFB]); // sti
    let wait = a.here();
    a.b(&[0xF4]); // hlt
    a.load_eax(COUNT);
    a.b(&[0x3D]).d(N_EDGE + N_LEVEL); // cmp eax, total
    a.jump(0x72, wait); // jb
    a.b(&[0xFA]); // cli
    // E1: N_DB1 doorbell writes.
    a.tsc_rax();
    a.b(&[0x49, 0x89, 0xC0]); // mov r8, rax
    a.mov_esi(DB1);
    a.mov_ecx(N_DB1);
    let e1 = a.here();
    a.store_ecx_rsi();
    a.b(&[0xFF, 0xC9]); // dec ecx
    a.jump(0x75, e1); // jnz
    a.tsc_rax();
    a.b(&[0x4C, 0x29, 0xC0]); // sub rax, r8
    a.store_rax(T_E1);
    // E2, E3: N_RT round trips each, a write then a wait for the host's answer.
    for (target, ack, time) in [(DB2, ACK2, T_E2), (MMIO3, ACK3, T_E3)] {
        a.tsc_rax();
        a.b(&[0x49, 0x89, 0xC0]);
        a.mov_esi(target);
        a.mov_ecx(1);
        let send = a.here();
        a.store_ecx_rsi();
        let spin = a.here();
        a.b(&[0xF3, 0x90]); // pause
        a.load_eax(ack);
        a.b(&[0x39, 0xC8]); // cmp eax, ecx
        a.jump(0x72, spin); // jb
        a.b(&[0xFF, 0xC1]); // inc ecx
        a.b(&[0x81, 0xF9]).d(N_RT + 1); // cmp ecx, N_RT + 1
        a.jump(0x72, send);
        a.tsc_rax();
        a.b(&[0x4C, 0x29, 0xC0]);
        a.store_rax(time);
    }
    a.out(DONE);
    a.out(DONE2);
    let halt = a.here();
    a.b(&[0xF4]);
    a.jump(0xEB, halt);
    a.c
}

/// The interrupt handler: counts, then EOI.
fn handler_code() -> Vec<u8> {
    let mut a = Asm::default();
    a.b(&[0xF0, 0xFF, 0x04, 0x25]).d(COUNT); // lock inc dword [COUNT]
    a.b(&[0x50, 0x56]); // push rax; push rsi
    a.mov_esi(0xFEE0_00B0); // EOI
    a.b(&[0x31, 0xC0]); // xor eax, eax
    a.store_eax_rsi();
    a.b(&[0x5E, 0x58, 0x48, 0xCF]); // pop rsi; pop rax; iretq
    a.c
}

/// The AP's code, in real mode at AP_CODE: marks, says so, halts.
fn ap_code() -> Vec<u8> {
    let mut c = vec![0xB8, 0x34, 0x12]; // mov ax, 0x1234
    c.push(0xA3); // mov [AP_MARK], ax
    c.extend((AP_MARK as u16).to_le_bytes());
    c.push(0xBA); // mov dx, AP_DONE
    c.extend(AP_DONE.to_le_bytes());
    c.extend([0xEE, 0xFA, 0xF4, 0xEB, 0xFD]); // out dx, al; cli; hlt; jmp hlt
    c
}

/// A 64-bit interrupt gate to `handler` in the code segment.
fn gate(handler: u64) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[0..2].copy_from_slice(&(handler as u16).to_le_bytes());
    g[2..4].copy_from_slice(&0x08u16.to_le_bytes());
    g[5] = 0x8E;
    g[6..8].copy_from_slice(&((handler >> 16) as u16).to_le_bytes());
    g[8..12].copy_from_slice(&((handler >> 32) as u32).to_le_bytes());
    g
}

/// The guest's memory, from any host thread.
#[derive(Clone, Copy)]
struct Mem(*mut u8);
// SAFETY: the guest's RAM, mapped for the probe's whole life; its words are read and
// written volatile, as the guest's own are.
unsafe impl Send for Mem {}
// SAFETY: as above.
unsafe impl Sync for Mem {}

impl Mem {
    fn put(&self, at: u64, bytes: &[u8]) {
        // SAFETY: within RAM, which no vCPU runs yet.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.0.add(at as usize), bytes.len()) };
    }
    fn u32(&self, at: u32) -> u32 {
        // SAFETY: an aligned word of RAM.
        unsafe { std::ptr::read_volatile(self.0.add(at as usize).cast::<u32>()) }
    }
    fn u64(&self, at: u32) -> u64 {
        // SAFETY: as above.
        unsafe { std::ptr::read_volatile(self.0.add(at as usize).cast::<u64>()) }
    }
    fn set_u32(&self, at: u32, v: u32) {
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(self.0.add(at as usize).cast::<u32>(), v) };
    }
}

/// An event a thread waits on: auto-reset, unsignaled.
fn event() -> Result<HANDLE, String> {
    // SAFETY: no attributes, no name.
    let e = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if e.is_null() {
        return Err(format!("CreateEventW: {}", std::io::Error::last_os_error()));
    }
    Ok(e)
}

/// An event handle, shared by threads.
#[derive(Clone, Copy)]
struct Event(HANDLE);
// SAFETY: a kernel handle, usable from any thread.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl Event {
    /// Whether it was signaled within `ms` milliseconds.
    fn wait(&self, ms: u32) -> bool {
        // SAFETY: an event handle alive until the probe ends.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }

    fn signal(&self) {
        // SAFETY: as above.
        unsafe { SetEvent(self.0) };
    }
}

/// One run of vCPU `vp` to its next exit.
fn run_vp(api: &Api, p: WHV_PARTITION_HANDLE, vp: u32) -> Result<WHV_RUN_VP_EXIT_CONTEXT, String> {
    let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
    // SAFETY: an exit context of the size given, 16-aligned.
    check("WHvRunVirtualProcessor", unsafe {
        (api.run)(
            p,
            vp,
            (&raw mut exit).cast(),
            std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
        )
    })?;
    Ok(exit.0)
}

fn set(api: &Api, p: WHV_PARTITION_HANDLE, vp: u32, names: &[WHV_REGISTER_NAME], values: &[WHV_REGISTER_VALUE]) -> Result<(), String> {
    let mut aligned: Vec<Aligned<WHV_REGISTER_VALUE>> = values.iter().map(|v| Aligned(*v)).collect();
    // Aligned<WHV_REGISTER_VALUE> is 16 bytes and 16-aligned: the array WinHvPlatform reads.
    let values = aligned.as_mut_ptr().cast::<WHV_REGISTER_VALUE>();
    // SAFETY: `names.len()` names and as many values.
    check("WHvSetVirtualProcessorRegisters", unsafe {
        (api.set_regs)(p, vp, names.as_ptr(), names.len() as u32, values)
    })
}

fn get(api: &Api, p: WHV_PARTITION_HANDLE, vp: u32, name: WHV_REGISTER_NAME) -> Result<u64, String> {
    let mut v = Aligned(WHV_REGISTER_VALUE::default());
    // SAFETY: one name and one value.
    check("WHvGetVirtualProcessorRegisters", unsafe {
        (api.get_regs)(p, vp, &raw const name, 1, &raw mut v.0)
    })?;
    // SAFETY: each register asked here is a 64-bit value.
    Ok(unsafe { v.0.Reg64 })
}

/// Moves vCPU `vp` past the instruction it exited on: `len` bytes from `rip`.
fn skip(api: &Api, p: WHV_PARTITION_HANDLE, vp: u32, rip: u64, len: u64) -> Result<(), String> {
    set(api, p, vp, &[WHvX64RegisterRip], &[reg64(rip + len)])
}

fn reason(r: WHV_RUN_VP_EXIT_REASON) -> String {
    match r {
        WHvRunVpExitReasonMemoryAccess => "MemoryAccess".into(),
        WHvRunVpExitReasonX64IoPortAccess => "IoPortAccess".into(),
        WHvRunVpExitReasonUnrecoverableException => "UnrecoverableException".into(),
        WHvRunVpExitReasonInvalidVpRegisterValue => "InvalidVpRegisterValue".into(),
        WHvRunVpExitReasonUnsupportedFeature => "UnsupportedFeature".into(),
        WHvRunVpExitReasonX64InterruptWindow => "InterruptWindow".into(),
        WHvRunVpExitReasonX64Halt => "Halt".into(),
        WHvRunVpExitReasonX64ApicEoi => "ApicEoi".into(),
        WHvRunVpExitReasonCanceled => "Canceled".into(),
        other => format!("{other:#x}"),
    }
}

/// A partition of `vcpus` vCPUs, its APIC emulated, offered the synthetic `features`.
fn partition(api: &Api, vcpus: u32, features: u64) -> Result<WHV_PARTITION_HANDLE, String> {
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
        let banks = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
            BanksCount: 1,
            Reserved0: 0,
            Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 {
                AsUINT64: [features],
            },
        };
        check(
            "SyntheticProcessorFeaturesBanks",
            (api.set_property)(
                p,
                WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
                (&raw const banks).cast(),
                std::mem::size_of_val(&banks) as u32,
            ),
        )?;
        check("WHvSetupPartition", (api.setup_partition)(p))?;
    }
    Ok(p)
}

/// Guest RAM, zeroed, mapped at GPA 0.
fn ram(api: &Api, p: WHV_PARTITION_HANDLE) -> Result<Mem, String> {
    // SAFETY: a fresh allocation the probe keeps for its life.
    let ram = unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }.cast::<u8>();
    if ram.is_null() {
        return Err("VirtualAlloc".into());
    }
    // SAFETY: RAM of this length, alive for the partition's life.
    check("WHvMapGpaRange", unsafe {
        (api.map)(
            p,
            ram.cast(),
            0,
            RAM as u64,
            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
        )
    })?;
    Ok(Mem(ram))
}

/// Whether a partition offered `features` (with its APIC emulated) sets up: the host
/// refuses some sets of synthetic features (HV_STATUS_INVALID_PARAMETER), so which bits
/// need which is measured, not assumed: the host's whole bank less each bit, and
/// `SYNTHETIC` with each other bit of the bank.
fn combinations(api: &Api, bank: u64) {
    let sets_up = |features: u64| -> String {
        let mut p: WHV_PARTITION_HANDLE = 0;
        // SAFETY: each call on the partition made here, deleted at the end.
        unsafe {
            if (api.create_partition)(&raw mut p) < 0 {
                return "WHvCreatePartition failed".into();
            }
            let vcpus: u32 = 1;
            let mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
            let banks = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
                BanksCount: 1,
                Reserved0: 0,
                Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 { AsUINT64: [features] },
            };
            let mut hr = (api.set_property)(p, WHvPartitionPropertyCodeProcessorCount, (&raw const vcpus).cast(), 4);
            if hr >= 0 {
                hr = (api.set_property)(p, WHvPartitionPropertyCodeLocalApicEmulationMode, (&raw const mode).cast(), 4);
            }
            if hr >= 0 {
                hr = (api.set_property)(
                    p,
                    WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
                    (&raw const banks).cast(),
                    std::mem::size_of_val(&banks) as u32,
                );
            }
            if hr >= 0 {
                hr = (api.setup_partition)(p);
            }
            (api.delete_partition)(p);
            if hr >= 0 { "ok".into() } else { format!("{:#010x}", hr as u32) }
        }
    };
    println!("features full bank {bank:#x}: {}", sets_up(bank));
    for bit in (0..64).filter(|b| bank & (1u64 << b) != 0) {
        println!("features bank less bit {bit}: {}", sets_up(bank & !(1u64 << bit)));
    }
    let ours = SYNTHETIC & bank;
    println!("features ours {ours:#x}: {}", sets_up(ours));
    for bit in (0..64).filter(|b| bank & !ours & (1u64 << b) != 0) {
        println!("features ours with bit {bit}: {}", sets_up(ours | 1u64 << bit));
    }
    for (label, set) in [
        ("presence", 0b11),
        ("time", 0b11 | 1 << 3 | 1 << 9 | 1 << 11),
        ("time+hypercalls+vp_index", 0b11 | 1 << 3 | 1 << 7 | 1 << 8 | 1 << 9 | 1 << 11),
        ("ours less ipi/flush", ours & !(7 << 24)),
        ("ours less timers", ours & !(1 << 5 | 1 << 22)),
        ("ours less apic msrs", ours & !(1 << 6)),
    ] {
        println!("features {label} {:#x}: {}", set & bank, sets_up(set & bank));
    }
}

/// What an application processor does as created: its activity state and APIC base, and
/// its first exit when run alone, the boot processor never run.
fn ap_as_created(api: &Api, features: u64) -> Result<(), String> {
    let p = partition(api, 2, features)?;
    let _ram = ram(api, p)?;
    // SAFETY: vCPUs of this partition.
    unsafe {
        check("WHvCreateVirtualProcessor 0", (api.create_vp)(p, 0, 0))?;
        check("WHvCreateVirtualProcessor 1", (api.create_vp)(p, 1, 0))?;
    }
    for vp in [0, 1] {
        println!(
            "as-created vp{vp} activity={:#x} apic_base={:#x} rip={:#x}",
            get(api, p, vp, WHvRegisterInternalActivityState)?,
            get(api, p, vp, WHvX64RegisterApicBase)?,
            get(api, p, vp, WHvX64RegisterRip)?
        );
    }
    let cancel = api.cancel;
    let started = Instant::now();
    let exit = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            // SAFETY: the partition, alive in this scope.
            unsafe { cancel(p, 1, 0) };
        });
        run_vp(api, p, 1)
    })?;
    // SAFETY: the union member each reason fills.
    let gpa = unsafe { exit.Anonymous.MemoryAccess.Gpa };
    println!(
        "as-created vp1 run: exit={} after_us={} rip={:#x} cs={:#x} gpa_if_memory={gpa:#x}",
        reason(exit.ExitReason),
        started.elapsed().as_micros(),
        exit.VpContext.Rip,
        exit.VpContext.Cs.Selector,
    );
    // SAFETY: the partition this function made.
    unsafe {
        (api.delete_vp)(p, 1);
        (api.delete_vp)(p, 0);
        (api.delete_partition)(p);
    }
    Ok(())
}

/// What the AP did: its first exit, and when.
struct ApRun {
    first: String,
    after: Duration,
}

fn probe(api: &Api, features: u64) -> Result<(), String> {
    let p = partition(api, 2, features)?;
    let mem = ram(api, p)?;
    mem.put(CODE, &bsp_code());
    mem.put(HANDLER, &handler_code());
    mem.put(SPURIOUS, &[0x48, 0xCF]);
    mem.put(AP_CODE, &ap_code());
    mem.put(IDT + u64::from(VECTOR) * 16, &gate(HANDLER));
    mem.put(IDT + 0xFF * 16, &gate(SPURIOUS));
    mem.put(GDT + 8, &0x00AF_9B00_0000_FFFFu64.to_le_bytes());
    mem.put(GDT + 16, &0x00CF_9300_0000_FFFFu64.to_le_bytes());
    // PML4[0] -> PDPT; PDPT[0] and [3]: 1 GiB pages at 0 and 3 GiB (the APIC, the doorbells).
    mem.put(PML4, &(PDPT | 0x3).to_le_bytes());
    mem.put(PDPT, &0x83u64.to_le_bytes());
    mem.put(PDPT + 24, &(0xC000_0000u64 | 0x83).to_le_bytes());
    // SAFETY: vCPUs of this partition.
    unsafe {
        check("WHvCreateVirtualProcessor 0", (api.create_vp)(p, 0, 0))?;
        check("WHvCreateVirtualProcessor 1", (api.create_vp)(p, 1, 0))?;
    }
    let mut gdtr = WHV_REGISTER_VALUE::default();
    gdtr.Table = WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: 23,
        Base: GDT,
    };
    let mut idtr = WHV_REGISTER_VALUE::default();
    idtr.Table = WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: 0xFFF,
        Base: IDT,
    };
    set(
        api,
        p,
        0,
        &[
            WHvX64RegisterCr0,
            WHvX64RegisterCr3,
            WHvX64RegisterCr4,
            WHvX64RegisterEfer,
            WHvX64RegisterCs,
            WHvX64RegisterDs,
            WHvX64RegisterEs,
            WHvX64RegisterSs,
            WHvX64RegisterGdtr,
            WHvX64RegisterIdtr,
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
            WHvX64RegisterRsp,
        ],
        &[
            reg64(0x8000_0031),
            reg64(PML4),
            reg64(0x20),
            reg64(0x500),
            segment(0x8, 0xA09B),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            segment(0x10, 0xC093),
            gdtr,
            idtr,
            reg64(CODE),
            reg64(0x2),
            reg64(STACK),
        ],
    )?;
    let activity = get(api, p, 1, WHvRegisterInternalActivityState)?;
    let mut suspend = WHV_REGISTER_VALUE::default();
    suspend.InternalActivity.AsUINT64 = activity | 1;
    let held = set(api, p, 1, &[WHvRegisterInternalActivityState], &[suspend]);
    println!("vp1 activity={activity:#x}, held in wait-for-SIPI: {held:?}");

    let (ev1, ev2, ev3) = (Event(event()?), Event(event()?), Event(event()?));
    let mut ports: Vec<*mut c_void> = Vec::new();
    for (at, ev) in [(DB1, ev1), (DB2, ev2)] {
        let params = WHV_NOTIFICATION_PORT_PARAMETERS {
            NotificationPortType: WHvNotificationPortTypeDoorbell,
            Reserved: 0,
            Anonymous: WHV_NOTIFICATION_PORT_PARAMETERS_0 {
                Doorbell: WHV_DOORBELL_MATCH_DATA {
                    GuestAddress: u64::from(at),
                    Value: 0,
                    Length: 4,
                    // MatchOnLength, not MatchOnValue: any 4-byte write.
                    _bitfield: 0b10,
                },
            },
        };
        let mut port: *mut c_void = std::ptr::null_mut();
        // SAFETY: parameters and an event that outlive the port.
        check("WHvCreateNotificationPort", unsafe {
            (api.create_port)(p, &raw const params, ev.0, &raw mut port)
        })?;
        ports.push(port);
    }

    let ready = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let cancel = api.cancel;
    let mut report: Vec<String> = Vec::new();
    std::thread::scope(|s| -> Result<(), String> {
        // The watchdog: cancels both vCPUs if the probe hangs.
        s.spawn(|| {
            let until = Instant::now() + WATCHDOG;
            while !done.load(Ordering::Relaxed) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !done.load(Ordering::Relaxed) {
                println!("watchdog: canceling both vCPUs");
                // SAFETY: the partition, alive in this scope.
                unsafe {
                    cancel(p, 0, 0);
                    cancel(p, 1, 0);
                }
            }
        });
        // The AP: run until its first exit, then halted until canceled at the end.
        let ap = s.spawn(|| -> Result<ApRun, String> {
            let started = Instant::now();
            let exit = run_vp(api, p, 1)?;
            let after = started.elapsed();
            // SAFETY: the union member an I/O exit fills.
            let port = unsafe { exit.Anonymous.IoPortAccess.PortNumber };
            let first = format!(
                "{} port={port:#x} rip={:#x} cs={:#x} len={}",
                reason(exit.ExitReason),
                exit.VpContext.Rip,
                exit.VpContext.Cs.Selector,
                exit.VpContext._bitfield & 0xF
            );
            if exit.ExitReason == WHvRunVpExitReasonX64IoPortAccess {
                skip(api, p, 1, exit.VpContext.Rip, u64::from(exit.VpContext._bitfield & 0xF).max(1))?;
                let again = run_vp(api, p, 1)?;
                if again.ExitReason != WHvRunVpExitReasonCanceled {
                    return Err(format!("the halted AP exited {}", reason(again.ExitReason)));
                }
            }
            Ok(ApRun { first, after })
        });
        // The VMM's interrupts.
        let sender = s.spawn(|| -> Result<Vec<String>, String> {
            while !ready.load(Ordering::Acquire) {
                if done.load(Ordering::Relaxed) {
                    return Err("never ready".into());
                }
                std::hint::spin_loop();
            }
            let mut lines = Vec::new();
            for (label, trigger, first, n) in [
                ("edge", WHvX64InterruptTriggerModeEdge, 0, N_EDGE),
                ("level", WHvX64InterruptTriggerModeLevel, N_EDGE, N_LEVEL),
            ] {
                let (mut calls, mut handled) = (Vec::new(), Vec::new());
                for i in first..first + n {
                    let control = WHV_INTERRUPT_CONTROL {
                        _bitfield: WHvX64InterruptTypeFixed as u64
                            | (WHvX64InterruptDestinationModePhysical as u64) << 8
                            | (trigger as u64) << 12,
                        Destination: 0,
                        Vector: VECTOR,
                    };
                    let t = Instant::now();
                    // SAFETY: a control of its own size.
                    check("WHvRequestInterrupt", unsafe {
                        (api.request_interrupt)(
                            p,
                            &raw const control,
                            std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
                        )
                    })?;
                    calls.push(t.elapsed());
                    while mem.u32(COUNT) <= i {
                        if t.elapsed() > Duration::from_secs(2) {
                            return Err(format!("{label} interrupt {i} never handled"));
                        }
                        std::hint::spin_loop();
                    }
                    handled.push(t.elapsed());
                }
                lines.push(format!("interrupt {label} call {}", summary(&mut calls)));
                lines.push(format!("interrupt {label} handled {}", summary(&mut handled)));
            }
            Ok(lines)
        });
        // The doorbells' and the MMIO round trips' host threads.
        let counter = s.spawn(|| {
            let mut wakes = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if ev1.wait(100) {
                    wakes += 1;
                }
            }
            wakes
        });
        let acker = |ev: Event, at: u32| {
            let mem = mem;
            move || -> Result<(), String> {
                for ack in 1..=N_RT {
                    if !ev.wait(10_000) {
                        return Err(format!("no signal for round trip {ack} at {at:#x}"));
                    }
                    mem.set_u32(at, ack);
                }
                Ok(())
            }
        };
        let e2 = s.spawn(acker(ev2, ACK2));
        let e3 = s.spawn(acker(ev3, ACK3));

        // The BSP, on this thread.
        let mut eois: u32 = 0;
        let mut eoi_vectors: Vec<u32> = Vec::new();
        let mut io_len: Option<u8> = None;
        let mut mmio: Option<String> = None;
        let ended: Result<(), String> = loop {
            let exit = match run_vp(api, p, 0) {
                Ok(exit) => exit,
                Err(e) => break Err(e),
            };
            let len = exit.VpContext._bitfield & 0xF;
            match exit.ExitReason {
                WHvRunVpExitReasonX64IoPortAccess => {
                    // SAFETY: the union member an I/O exit fills.
                    let port = unsafe { exit.Anonymous.IoPortAccess.PortNumber };
                    io_len.get_or_insert(len);
                    if let Err(e) = skip(api, p, 0, exit.VpContext.Rip, u64::from(len).max(1)) {
                        break Err(e);
                    }
                    match port {
                        READY => ready.store(true, Ordering::Release),
                        DONE => break Ok(()),
                        other => break Err(format!("port {other:#x}")),
                    }
                }
                WHvRunVpExitReasonX64ApicEoi => {
                    eois += 1;
                    // SAFETY: the union member an EOI exit fills.
                    let v = unsafe { exit.Anonymous.ApicEoi.InterruptVector };
                    if !eoi_vectors.contains(&v) {
                        eoi_vectors.push(v);
                    }
                }
                WHvRunVpExitReasonMemoryAccess => {
                    // SAFETY: the union member a memory exit fills.
                    let m = unsafe { exit.Anonymous.MemoryAccess };
                    if m.Gpa != u64::from(MMIO3) {
                        break Err(format!("memory access at {:#x}, rip {:#x}", m.Gpa, exit.VpContext.Rip));
                    }
                    if mmio.is_none() {
                        let n = usize::from(m.InstructionByteCount).min(16);
                        // SAFETY: the access info's bits.
                        let info = unsafe { m.AccessInfo.AsUINT32 };
                        mmio = Some(format!(
                            "mmio exit: instruction_bytes={:02x?} instruction_length={len} access_info={info:#x} gva={:#x}",
                            &m.InstructionBytes[..n],
                            m.Gva
                        ));
                    }
                    ev3.signal();
                    // `mov [rsi], ecx`: two bytes.
                    if let Err(e) = skip(api, p, 0, exit.VpContext.Rip, 2) {
                        break Err(e);
                    }
                }
                other => {
                    break Err(format!("exit {} at rip {:#x}", reason(other), exit.VpContext.Rip));
                }
            }
        };
        stop.store(true, Ordering::Relaxed);
        report.push(format!("bsp: {ended:?}"));
        report.push(format!(
            "eoi exits={eois} vectors={eoi_vectors:x?} io_instruction_length={io_len:?}"
        ));
        report.extend(mmio);
        if ended.is_ok() {
            // Register reads and writes, as an own MMIO decoder makes them.
            let (mut reads, mut writes) = (Vec::new(), Vec::new());
            let rip = get(api, p, 0, WHvX64RegisterRip)?;
            for _ in 0..ROUNDS {
                let t = Instant::now();
                let rax = get(api, p, 0, WHvX64RegisterRax)?;
                reads.push(t.elapsed());
                let t = Instant::now();
                set(api, p, 0, &[WHvX64RegisterRax, WHvX64RegisterRip], &[reg64(rax), reg64(rip)])?;
                writes.push(t.elapsed());
            }
            report.push(format!("register read (1) {}", summary(&mut reads)));
            report.push(format!("register write (2) {}", summary(&mut writes)));
            // A cancel before the run.
            // SAFETY: the partition, alive in this scope.
            unsafe { cancel(p, 0, 0) };
            let t = Instant::now();
            let exit = run_vp(api, p, 0)?;
            report.push(format!(
                "cancel before run: exit={} after_us={}",
                reason(exit.ExitReason),
                t.elapsed().as_micros()
            ));
            let next = if exit.ExitReason == WHvRunVpExitReasonCanceled {
                run_vp(api, p, 0)?
            } else {
                exit
            };
            // SAFETY: the union member an I/O exit fills.
            let port = unsafe { next.Anonymous.IoPortAccess.PortNumber };
            report.push(format!("then: exit={} port={port:#x}", reason(next.ExitReason)));
            skip(api, p, 0, next.VpContext.Rip, u64::from(next.VpContext._bitfield & 0xF).max(1))?;
            // A cancel while halted in the hypervisor, interrupts off.
            let canceled = std::sync::OnceLock::new();
            let exit = std::thread::scope(|s2| {
                s2.spawn(|| {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = canceled.set(Instant::now());
                    // SAFETY: the partition, alive in this scope.
                    unsafe { cancel(p, 0, 0) };
                });
                run_vp(api, p, 0)
            })?;
            let back = canceled.get().map(|t| t.elapsed().as_micros());
            report.push(format!(
                "cancel while halted: exit={} returned_us_after_cancel={back:?}",
                reason(exit.ExitReason)
            ));
        }
        // The AP, halted for good: canceled.
        // SAFETY: the partition, alive in this scope.
        unsafe { cancel(p, 1, 0) };
        done.store(true, Ordering::Relaxed);
        match ap.join() {
            Ok(Ok(run)) => report.push(format!(
                "ap first exit: {} after_us={} mark={:#x}",
                run.first,
                run.after.as_micros(),
                mem.u32(AP_MARK) & 0xFFFF
            )),
            Ok(Err(e)) => report.push(format!("ap: {e}")),
            Err(_) => report.push("ap: panicked".into()),
        }
        match sender.join() {
            Ok(Ok(lines)) => report.extend(lines),
            Ok(Err(e)) => report.push(format!("interrupts: {e}")),
            Err(_) => report.push("interrupts: panicked".into()),
        }
        report.push(format!("doorbell 1 wakes={:?} of {N_DB1} writes", counter.join().ok()));
        for (label, h) in [("doorbell round trips", e2), ("mmio round trips", e3)] {
            report.push(format!("{label}: {:?}", h.join().map_err(|_| "panicked")));
        }
        Ok(())
    })?;
    let hz = mem.u64(TSC_HZ);
    let us = |ticks: u64, n: u32| {
        if hz == 0 {
            0.0
        } else {
            ticks as f64 * 1e6 / hz as f64 / f64::from(n)
        }
    };
    report.push(format!(
        "guest tsc_hz={hz} doorbell write us={:.3} doorbell round trip us={:.3} mmio round trip us={:.3}",
        us(mem.u64(T_E1), N_DB1),
        us(mem.u64(T_E2), N_RT),
        us(mem.u64(T_E3), N_RT)
    ));
    for (i, (leaf, sub)) in LEAVES.iter().enumerate() {
        let at = LEAVES_AT + 16 * i as u32;
        report.push(format!(
            "cpuid {leaf:#010x}.{sub}: eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x}",
            mem.u32(at),
            mem.u32(at + 4),
            mem.u32(at + 8),
            mem.u32(at + 12)
        ));
    }
    for line in report {
        println!("{line}");
    }
    // SAFETY: the partition and handles this function made.
    unsafe {
        for port in ports {
            (api.delete_port)(p, port);
        }
        for ev in [ev1, ev2, ev3] {
            CloseHandle(ev.0);
        }
        (api.delete_vp)(p, 1);
        (api.delete_vp)(p, 0);
        (api.delete_partition)(p);
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
    // SAFETY: the union member the code asked fills it.
    let bank = capability(&api, WHvCapabilityCodeSyntheticProcessorFeaturesBanks)
        .map_or(0, |c| unsafe { c.SyntheticProcessorFeaturesBanks.Anonymous.AsUINT64[0] });
    println!(
        "host bank={bank:#x} offered={:#x} missing={:#x}",
        SYNTHETIC & bank,
        SYNTHETIC & !bank
    );
    combinations(&api, bank);
    // Ours where the host sets a partition up with them, else the whole bank, so that the
    // rest is measured either way.
    let features = match partition(&api, 1, SYNTHETIC & bank) {
        Ok(p) => {
            // SAFETY: the partition just made.
            unsafe { (api.delete_partition)(p) };
            SYNTHETIC & bank
        }
        Err(e) => {
            println!("ours refused ({e}); measuring with the whole bank");
            bank
        }
    };
    println!("features={features:#x}");
    if let Err(e) = ap_as_created(&api, features) {
        println!("as-created failed: {e}");
    }
    if let Err(e) = probe(&api, features) {
        println!("probe failed: {e}");
    }
}
