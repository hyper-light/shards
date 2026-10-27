//! What a WHP partition's snapshot rests on (PM M345, D196), before shards' WHP backend saves
//! and restores one:
//! - the vCPU state WHvGetVirtualProcessorState gives (the local APIC's, as
//!   InterruptControllerState2, and the XSAVE area's): their sizes, and whether each, set back,
//!   reads the same;
//! - which registers the host reads beyond the architectural ones a snapshot keeps anyway;
//! - partition time: whether a new partition's TSC and reference time run before its vCPU
//!   does, whether WHvSuspendPartitionTime holds them and the APIC timer's count, whether a
//!   run resumes them, and whether the reference time can be set while they are held;
//! - a guest saved from one partition and restored into a new one, its memory a
//!   copy-on-write view (PAGE_WRITECOPY) of a sparse file of its saved RAM, placed into a
//!   placeholder and mapped at stage 2: whether it reads its saved RAM and zeros where the
//!   file has holes, whether its stores land in the view and never in the file, and whether
//!   its TSC and its reference TSC page's time go on from where they were saved;
//! - what each step of a save and a restore costs, and what a restored guest's first touch
//!   and first store of each page cost.
//!
//! The guest is one vCPU in long mode running an interpreter, as M341's: at each port exit
//! the host names an operation in its registers, and the next port exit carries the result
//! in RAX.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle as _;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use whp_run::{Aligned, Api, bind, check, platform_function, reg64, segment, summary};
use windows_sys::Win32::Foundation::{CloseHandle, WHV_E_INSUFFICIENT_BUFFER};
use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
use windows_sys::Win32::System::Hypervisor::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES, FSCTL_SET_SPARSE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE,
    PAGE_WRITECOPY, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::HRESULT;

/// The guest's memory: 2 MiB from GPA 0, its code and page tables in it.
const RAM: usize = 2 << 20;
const PAGE: usize = 4096;
const CODE: u64 = 0x1000;
const STACK: u64 = 0x8000;
const PML4: u64 = 0x10000;
const PDPT: u64 = 0x11000;
/// The overlay pages the guest's Hyper-V MSRs name, as a Linux guest's would.
const VP_ASSIST: u64 = 0x1C000;
const HYPERCALL_PAGE: u64 = 0x1D000;
const TSC_PAGE: u64 = 0x1E000;
/// A page the saved guest stored into, and one it never touched (a hole in the file).
const DATA: u64 = 0x40000;
const HOLE: u64 = 0x100000;
/// The port the guest's interpreter waits on, and its operations (RBX).
const PORT: u16 = 0xE0;
const LOAD: u64 = 0;
const STORE: u64 = 1;
const TSC: u64 = 2;
const TOUCH: u64 = 3;
const DIRTY: u64 = 4;
/// Restores timed, after the first, which checks.
const ROUNDS: usize = 200;
/// How long the probe may run before the current vCPU's run is canceled.
const WATCHDOG: Duration = Duration::from_secs(900);
/// How long the host waits between two readings of a clock.
const APART: Duration = Duration::from_millis(50);

/// The synthetic features shards offers its guests (hv/whp/mod.rs `SYNTHETIC`), of the host's.
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

type GetVpState = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
    *mut c_void,
    u32,
    *mut u32,
) -> HRESULT;
type SetVpState = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
    *const c_void,
    u32,
) -> HRESULT;
type PartitionCall = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;

/// What this probe binds beyond `Api`.
struct State {
    get: GetVpState,
    set: SetVpState,
    suspend: PartitionCall,
    resume: PartitionCall,
}

fn bind_state() -> Result<State, String> {
    let need = |name: &str| format!("{name}: not on this host");
    // SAFETY: each name with its own signature, from WinHvPlatform.h.
    unsafe {
        Ok(State {
            get: platform_function("WHvGetVirtualProcessorState")?
                .ok_or_else(|| need("WHvGetVirtualProcessorState"))?,
            set: platform_function("WHvSetVirtualProcessorState")?
                .ok_or_else(|| need("WHvSetVirtualProcessorState"))?,
            suspend: platform_function("WHvSuspendPartitionTime")?
                .ok_or_else(|| need("WHvSuspendPartitionTime"))?,
            resume: platform_function("WHvResumePartitionTime")?
                .ok_or_else(|| need("WHvResumePartitionTime"))?,
        })
    }
}

/// A register value at WHV_REGISTER_VALUE's 16-byte alignment.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct Value(WHV_REGISTER_VALUE);

impl Value {
    fn u64(&self) -> u64 {
        // SAFETY: every value's first 8 bytes are a u64's.
        unsafe { self.0.Reg64 }
    }

}

/// The registers a snapshot keeps whatever the host: the architectural ones, the synthetic
/// MSRs a Linux guest of shards' writes, and what is pending of the vCPU's events.
const ARCHITECTURAL: &[(WHV_REGISTER_NAME, &str)] = &[
    (WHvX64RegisterRax, "Rax"),
    (WHvX64RegisterRcx, "Rcx"),
    (WHvX64RegisterRdx, "Rdx"),
    (WHvX64RegisterRbx, "Rbx"),
    (WHvX64RegisterRsp, "Rsp"),
    (WHvX64RegisterRbp, "Rbp"),
    (WHvX64RegisterRsi, "Rsi"),
    (WHvX64RegisterRdi, "Rdi"),
    (WHvX64RegisterR8, "R8"),
    (WHvX64RegisterR9, "R9"),
    (WHvX64RegisterR10, "R10"),
    (WHvX64RegisterR11, "R11"),
    (WHvX64RegisterR12, "R12"),
    (WHvX64RegisterR13, "R13"),
    (WHvX64RegisterR14, "R14"),
    (WHvX64RegisterR15, "R15"),
    (WHvX64RegisterRip, "Rip"),
    (WHvX64RegisterRflags, "Rflags"),
    (WHvX64RegisterEs, "Es"),
    (WHvX64RegisterCs, "Cs"),
    (WHvX64RegisterSs, "Ss"),
    (WHvX64RegisterDs, "Ds"),
    (WHvX64RegisterFs, "Fs"),
    (WHvX64RegisterGs, "Gs"),
    (WHvX64RegisterLdtr, "Ldtr"),
    (WHvX64RegisterTr, "Tr"),
    (WHvX64RegisterIdtr, "Idtr"),
    (WHvX64RegisterGdtr, "Gdtr"),
    (WHvX64RegisterCr0, "Cr0"),
    (WHvX64RegisterCr2, "Cr2"),
    (WHvX64RegisterCr3, "Cr3"),
    (WHvX64RegisterCr4, "Cr4"),
    (WHvX64RegisterCr8, "Cr8"),
    (WHvX64RegisterDr0, "Dr0"),
    (WHvX64RegisterDr1, "Dr1"),
    (WHvX64RegisterDr2, "Dr2"),
    (WHvX64RegisterDr3, "Dr3"),
    (WHvX64RegisterDr6, "Dr6"),
    (WHvX64RegisterDr7, "Dr7"),
    (WHvX64RegisterXCr0, "XCr0"),
    (WHvX64RegisterEfer, "Efer"),
    (WHvX64RegisterKernelGsBase, "KernelGsBase"),
    (WHvX64RegisterApicBase, "ApicBase"),
    (WHvX64RegisterPat, "Pat"),
    (WHvX64RegisterSysenterCs, "SysenterCs"),
    (WHvX64RegisterSysenterEip, "SysenterEip"),
    (WHvX64RegisterSysenterEsp, "SysenterEsp"),
    (WHvX64RegisterStar, "Star"),
    (WHvX64RegisterLstar, "Lstar"),
    (WHvX64RegisterCstar, "Cstar"),
    (WHvX64RegisterSfmask, "Sfmask"),
    (WHvX64RegisterTscAux, "TscAux"),
    (WHvX64RegisterMsrMtrrDefType, "MtrrDefType"),
    (WHvX64RegisterMsrMtrrFix64k00000, "MtrrFix64k00000"),
    (WHvX64RegisterMsrMtrrFix16k80000, "MtrrFix16k80000"),
    (WHvX64RegisterMsrMtrrFix16kA0000, "MtrrFix16kA0000"),
    (WHvX64RegisterMsrMtrrFix4kC0000, "MtrrFix4kC0000"),
    (WHvX64RegisterMsrMtrrFix4kC8000, "MtrrFix4kC8000"),
    (WHvX64RegisterMsrMtrrFix4kD0000, "MtrrFix4kD0000"),
    (WHvX64RegisterMsrMtrrFix4kD8000, "MtrrFix4kD8000"),
    (WHvX64RegisterMsrMtrrFix4kE0000, "MtrrFix4kE0000"),
    (WHvX64RegisterMsrMtrrFix4kE8000, "MtrrFix4kE8000"),
    (WHvX64RegisterMsrMtrrFix4kF0000, "MtrrFix4kF0000"),
    (WHvX64RegisterMsrMtrrFix4kF8000, "MtrrFix4kF8000"),
    (WHvRegisterGuestOsId, "GuestOsId"),
    (WHvX64RegisterHypercall, "Hypercall"),
    (WHvRegisterVpAssistPage, "VpAssistPage"),
    (WHvRegisterReferenceTsc, "ReferenceTsc"),
    (WHvRegisterPendingInterruption, "PendingInterruption"),
    (WHvRegisterInterruptState, "InterruptState"),
    (WHvRegisterPendingEvent, "PendingEvent"),
    (WHvX64RegisterDeliverabilityNotifications, "DeliverabilityNotifications"),
    (WHvRegisterInternalActivityState, "InternalActivityState"),
    (WHvX64RegisterTsc, "Tsc"),
];

/// WinHvPlatformDefs.h's names for two MSRs windows-sys 0.61.2 lacks (mingw-w64's copy).
const MSR_IA32_MISC_ENABLE: WHV_REGISTER_NAME = 0x0000_20A0;
const IA32_FEATURE_CONTROL: WHV_REGISTER_NAME = 0x0000_20A1;

/// Registers a host may or may not read, each asked alone.
const OPTIONAL: &[(WHV_REGISTER_NAME, &str)] = &[
    (WHvX64RegisterMsrMtrrCap, "MtrrCap"),
    (WHvX64RegisterMsrMtrrPhysBase0, "MtrrPhysBase0"),
    (WHvX64RegisterMsrMtrrPhysMask0, "MtrrPhysMask0"),
    (WHvX64RegisterMsrMtrrPhysBase7, "MtrrPhysBase7"),
    (WHvX64RegisterMsrMtrrPhysBaseF, "MtrrPhysBaseF"),
    (WHvX64RegisterXss, "Xss"),
    (WHvX64RegisterSpecCtrl, "SpecCtrl"),
    (WHvX64RegisterPredCmd, "PredCmd"),
    (WHvX64RegisterTsxCtrl, "TsxCtrl"),
    (WHvX64RegisterTscVirtualOffset, "TscVirtualOffset"),
    (WHvX64RegisterTscDeadline, "TscDeadline"),
    (WHvX64RegisterTscAdjust, "TscAdjust"),
    (WHvX64RegisterUmwaitControl, "UmwaitControl"),
    (WHvX64RegisterXfd, "Xfd"),
    (WHvX64RegisterXfdErr, "XfdErr"),
    (MSR_IA32_MISC_ENABLE, "MiscEnable"),
    (IA32_FEATURE_CONTROL, "FeatureControl"),
    (WHvX64RegisterBndcfgs, "Bndcfgs"),
    (WHvX64RegisterUCet, "UCet"),
    (WHvX64RegisterSCet, "SCet"),
    (WHvX64RegisterSsp, "Ssp"),
    (WHvX64RegisterPl0Ssp, "Pl0Ssp"),
    (WHvX64RegisterInterruptSspTableAddr, "InterruptSspTableAddr"),
    (WHvX64RegisterPendingDebugException, "PendingDebugException"),
    (WHvX64RegisterInitialApicId, "InitialApicId"),
    (WHvRegisterVpRuntime, "VpRuntime"),
    (WHvRegisterReferenceTscSequence, "ReferenceTscSequence"),
    (WHvX64RegisterApicCurrentCount, "ApicCurrentCount"),
    (WHvX64RegisterVirtualCr0, "VirtualCr0"),
];

/// The guest's code, hand-assembled (x86-64): `out dx, al`, and wait for the host's word;
/// then RBX names what to do: LOAD `mov rax, [rcx]`; STORE `mov [rcx], rsi`; TSC `rdtsc`
/// into RAX whole; TOUCH a load of each page of RSI bytes from RCX, summed into RAX; DIRTY a
/// store into each page of RSI bytes from RCX; then back to the `out`.
fn guest_code() -> Vec<u8> {
    let mut c = vec![0xEE]; // out dx, al
    let mut jumps: Vec<(usize, &str)> = Vec::new();
    let mut targets: Vec<(&str, usize)> = Vec::new();
    for (op, label) in [(STORE, "store"), (TSC, "tsc"), (TOUCH, "touch"), (DIRTY, "dirty")] {
        c.extend([0x83, 0xFB, op as u8]); // cmp ebx, op
        c.extend([0x74, 0]); // je label
        jumps.push((c.len() - 1, label));
    }
    c.extend([0x48, 0x8B, 0x01]); // mov rax, [rcx]
    c.extend([0xEB, 0]); // jmp 0
    jumps.push((c.len() - 1, "top"));
    targets.push(("store", c.len()));
    c.extend([0x48, 0x89, 0x31]); // mov [rcx], rsi
    c.extend([0xEB, 0]);
    jumps.push((c.len() - 1, "top"));
    targets.push(("tsc", c.len()));
    c.extend([0x0F, 0x31]); // rdtsc
    c.extend([0x48, 0xC1, 0xE2, 0x20]); // shl rdx, 32
    c.extend([0x48, 0x09, 0xD0]); // or rax, rdx
    c.push(0xBA); // mov edx, PORT: the `out` port, which rdtsc took
    c.extend(u32::from(PORT).to_le_bytes());
    c.extend([0xEB, 0]);
    jumps.push((c.len() - 1, "top"));
    targets.push(("touch", c.len()));
    c.extend([0x31, 0xC0]); // xor eax, eax
    c.extend([0x48, 0x89, 0xCF]); // mov rdi, rcx
    c.extend([0x4C, 0x8D, 0x04, 0x31]); // lea r8, [rcx + rsi]
    targets.push(("touch_each", c.len()));
    c.extend([0x48, 0x03, 0x07]); // add rax, [rdi]
    c.extend([0x48, 0x81, 0xC7]); // add rdi, PAGE
    c.extend((PAGE as u32).to_le_bytes());
    c.extend([0x4C, 0x39, 0xC7]); // cmp rdi, r8
    c.extend([0x72, 0]); // jb touch_each
    jumps.push((c.len() - 1, "touch_each"));
    c.extend([0xEB, 0]);
    jumps.push((c.len() - 1, "top"));
    targets.push(("dirty", c.len()));
    c.extend([0x48, 0x89, 0xCF]); // mov rdi, rcx
    c.extend([0x4C, 0x8D, 0x04, 0x31]); // lea r8, [rcx + rsi]
    targets.push(("dirty_each", c.len()));
    c.extend([0x48, 0x89, 0x3F]); // mov [rdi], rdi
    c.extend([0x48, 0x81, 0xC7]); // add rdi, PAGE
    c.extend((PAGE as u32).to_le_bytes());
    c.extend([0x4C, 0x39, 0xC7]); // cmp rdi, r8
    c.extend([0x72, 0]); // jb dirty_each
    jumps.push((c.len() - 1, "dirty_each"));
    c.extend([0xEB, 0]);
    jumps.push((c.len() - 1, "top"));
    targets.push(("top", 0));
    for (at, label) in jumps {
        let target = targets
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, t)| *t)
            .unwrap_or(0);
        c[at] = (target as isize - (at as isize + 1)) as i8 as u8;
    }
    c
}

/// The partition whose vCPU the watchdog cancels.
static CURRENT: AtomicIsize = AtomicIsize::new(0);

struct Vm<'a> {
    api: &'a Api,
    st: &'a State,
    partition: WHV_PARTITION_HANDLE,
    /// Where the vCPU resumes: past its `out`.
    rip: u64,
}

struct Answer {
    rax: u64,
    took: Duration,
}

impl Vm<'_> {
    fn run(&self) -> Result<WHV_RUN_VP_EXIT_CONTEXT, String> {
        let mut exit = Aligned(WHV_RUN_VP_EXIT_CONTEXT::default());
        // SAFETY: an exit context of its own size, on the partition's one vCPU.
        let hr = unsafe {
            (self.api.run)(
                self.partition,
                0,
                (&raw mut exit).cast(),
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        check("WHvRunVirtualProcessor", hr)?;
        Ok(exit.0)
    }

    fn set(&self, names: &[WHV_REGISTER_NAME], values: &[Value]) -> Result<(), String> {
        if names.len() != values.len() {
            return Err("names and values differ".into());
        }
        // SAFETY: as many 16-aligned values as names.
        check("WHvSetVirtualProcessorRegisters", unsafe {
            (self.api.set_regs)(
                self.partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr().cast(),
            )
        })
    }

    fn get(&self, names: &[WHV_REGISTER_NAME]) -> Result<Vec<Value>, String> {
        let mut values = vec![Value::default(); names.len()];
        // SAFETY: as many 16-aligned values as names.
        check("WHvGetVirtualProcessorRegisters", unsafe {
            (self.api.get_regs)(
                self.partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.as_mut_ptr().cast(),
            )
        })?;
        Ok(values)
    }

    fn get1(&self, name: WHV_REGISTER_NAME) -> Result<u64, String> {
        Ok(self.get(&[name])?.first().map(Value::u64).unwrap_or(0))
    }

    fn reference_time(&self) -> Result<u64, String> {
        let mut value = 0u64;
        let mut written = 0u32;
        // SAFETY: a u64, the property's type.
        check("WHvGetPartitionProperty(ReferenceTime)", unsafe {
            (self.api.get_property)(
                self.partition,
                WHvPartitionPropertyCodeReferenceTime,
                (&raw mut value).cast(),
                8,
                &raw mut written,
            )
        })?;
        Ok(value)
    }

    fn set_reference_time(&self, value: u64) -> Result<(), String> {
        // SAFETY: a u64, the property's type.
        check("WHvSetPartitionProperty(ReferenceTime)", unsafe {
            (self.api.set_property)(
                self.partition,
                WHvPartitionPropertyCodeReferenceTime,
                (&raw const value).cast(),
                8,
            )
        })
    }

    fn suspend(&self) -> Result<(), String> {
        // SAFETY: the partition, whose vCPU is not running.
        check("WHvSuspendPartitionTime", unsafe { (self.st.suspend)(self.partition) })
    }

    fn resume(&self) -> Result<(), String> {
        // SAFETY: the partition.
        check("WHvResumePartitionTime", unsafe { (self.st.resume)(self.partition) })
    }

    /// State `kind` of vCPU 0, the size asked first.
    fn get_state(&self, kind: WHV_VIRTUAL_PROCESSOR_STATE_TYPE) -> Result<Vec<u8>, String> {
        let mut needed = 0u32;
        // SAFETY: an empty buffer, which the call answers with the size it needs.
        let hr = unsafe { (self.st.get)(self.partition, 0, kind, std::ptr::null_mut(), 0, &raw mut needed) };
        if hr != WHV_E_INSUFFICIENT_BUFFER && hr < 0 {
            return Err(format!(
                "WHvGetVirtualProcessorState({kind:#x}) sizing: HRESULT {:#010x}",
                hr as u32
            ));
        }
        let mut buf = vec![0u8; needed as usize];
        let mut written = 0u32;
        // SAFETY: a buffer of the size it asked for.
        check("WHvGetVirtualProcessorState", unsafe {
            (self.st.get)(
                self.partition,
                0,
                kind,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &raw mut written,
            )
        })?;
        buf.truncate(written as usize);
        Ok(buf)
    }

    fn set_state(&self, kind: WHV_VIRTUAL_PROCESSOR_STATE_TYPE, bytes: &[u8]) -> Result<(), String> {
        // SAFETY: a buffer of its own length.
        check("WHvSetVirtualProcessorState", unsafe {
            (self.st.set)(self.partition, 0, kind, bytes.as_ptr().cast(), bytes.len() as u32)
        })
    }

    /// Runs the vCPU from where it is to its next `out`.
    fn start(&mut self) -> Result<(), String> {
        let exit = self.run()?;
        if exit.ExitReason != WHvRunVpExitReasonX64IoPortAccess {
            return Err(format!(
                "first exit {:#x} at rip {:#x}",
                exit.ExitReason, exit.VpContext.Rip
            ));
        }
        self.rip = exit.VpContext.Rip + 1;
        Ok(())
    }

    fn ask(&mut self, op: u64, gpa: u64, value: u64) -> Result<Answer, String> {
        let started = Instant::now();
        let values = [
            Value(reg64(self.rip)),
            Value(reg64(op)),
            Value(reg64(gpa)),
            Value(reg64(value)),
        ];
        self.set(
            &[
                WHvX64RegisterRip,
                WHvX64RegisterRbx,
                WHvX64RegisterRcx,
                WHvX64RegisterRsi,
            ],
            &values,
        )?;
        let exit = self.run()?;
        // SAFETY (each union read): the member the exit reason names.
        if exit.ExitReason == WHvRunVpExitReasonX64IoPortAccess
            && unsafe { exit.Anonymous.IoPortAccess.PortNumber } == PORT
        {
            self.rip = exit.VpContext.Rip + 1;
            return Ok(Answer {
                rax: unsafe { exit.Anonymous.IoPortAccess.Rax },
                took: started.elapsed(),
            });
        }
        if exit.ExitReason == WHvRunVpExitReasonMemoryAccess {
            return Err(format!(
                "a memory exit at GPA {:#x}, rip {:#x}",
                unsafe { exit.Anonymous.MemoryAccess.Gpa },
                exit.VpContext.Rip
            ));
        }
        Err(format!(
            "exit reason {:#x} at rip {:#x}",
            exit.ExitReason, exit.VpContext.Rip
        ))
    }

    /// The reference time the guest reads from its reference TSC page at its TSC now, in
    /// 100 ns units (TLFS "Partition Reference Time Enlightenment").
    fn guest_reference_time(&mut self) -> Result<(u64, u64), String> {
        let sequence = self.ask(LOAD, TSC_PAGE, 0)?.rax & 0xFFFF_FFFF;
        let scale = self.ask(LOAD, TSC_PAGE + 8, 0)?.rax;
        let offset = self.ask(LOAD, TSC_PAGE + 16, 0)?.rax;
        let tsc = self.ask(TSC, 0, 0)?.rax;
        let time = ((u128::from(tsc) * u128::from(scale)) >> 64) as u64;
        Ok((time.wrapping_add(offset), sequence))
    }
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// A partition of one vCPU as shards makes one (D140): its APIC emulated (XApic), offered
/// the synthetic features shards offers of the host's; then its time suspended right
/// after setup, where `suspended`. How long each took.
fn partition(api: &Api, st: &State, bank: u64, suspended: bool) -> Result<(WHV_PARTITION_HANDLE, [Duration; 2]), String> {
    let mut p: WHV_PARTITION_HANDLE = 0;
    let t = Instant::now();
    // SAFETY: each call on the partition this function makes.
    unsafe {
        check("WHvCreatePartition", (api.create_partition)(&raw mut p))?;
        let count: u32 = 1;
        check(
            "ProcessorCount",
            (api.set_property)(
                p,
                WHvPartitionPropertyCodeProcessorCount,
                (&raw const count).cast(),
                4,
            ),
        )?;
        let mode = WHvX64LocalApicEmulationModeXApic;
        check(
            "LocalApicEmulationMode",
            (api.set_property)(
                p,
                WHvPartitionPropertyCodeLocalApicEmulationMode,
                (&raw const mode).cast(),
                std::mem::size_of_val(&mode) as u32,
            ),
        )?;
        let banks = WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
            BanksCount: 1,
            Reserved0: 0,
            Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 { AsUINT64: [bank] },
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
    let made = t.elapsed();
    let t = Instant::now();
    if suspended {
        // SAFETY: the partition just set up, no vCPU running.
        check("WHvSuspendPartitionTime", unsafe { (st.suspend)(p) })?;
    }
    Ok((p, [made, t.elapsed()]))
}

/// The first partition's guest: RAM written with its code and tables, mapped, and its vCPU
/// in long mode at its first instruction.
fn first_guest(api: &Api, p: WHV_PARTITION_HANDLE) -> Result<*mut u8, String> {
    // SAFETY: memory this function allocates and maps, kept for the probe's life.
    unsafe {
        let ram = VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE).cast::<u8>();
        if ram.is_null() {
            return Err(last_error("VirtualAlloc"));
        }
        let mem = std::slice::from_raw_parts_mut(ram, RAM);
        let code = guest_code();
        mem[CODE as usize..CODE as usize + code.len()].copy_from_slice(&code);
        mem[PML4 as usize..PML4 as usize + 8].copy_from_slice(&(PDPT | 0x3).to_le_bytes());
        mem[PDPT as usize..PDPT as usize + 8].copy_from_slice(&0x83u64.to_le_bytes());
        check(
            "WHvMapGpaRange RAM",
            (api.map)(
                p,
                ram.cast(),
                0,
                RAM as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            ),
        )?;
        check("WHvCreateVirtualProcessor", (api.create_vp)(p, 0, 0))?;
        Ok(ram)
    }
}

fn long_mode(vm: &Vm<'_>) -> Result<(), String> {
    vm.set(
        &[
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
            WHvX64RegisterRdx,
        ],
        &[
            Value(reg64(0x8000_0031)),
            Value(reg64(PML4)),
            Value(reg64(0x20)),
            Value(reg64(0x500)),
            Value(segment(0x8, 0xA09B)),
            Value(segment(0x10, 0xC093)),
            Value(segment(0x10, 0xC093)),
            Value(segment(0x10, 0xC093)),
            Value(reg64(CODE)),
            Value(reg64(0x2)),
            Value(reg64(STACK)),
            Value(reg64(u64::from(PORT))),
        ],
    )
}

/// The APIC timer's current count, from the APIC state page (its register at 0x390).
fn apic_count(vm: &Vm<'_>) -> Result<u32, String> {
    let page = vm.get_state(WHvVirtualProcessorStateTypeInterruptControllerState2)?;
    page.get(0x390..0x394)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| format!("an APIC state of {} bytes", page.len()))
}

/// Sets the APIC timer counting from the APIC state page: the APIC enabled (its spurious
/// vector register's bit 8), the timer masked, periodic, its divider 1 and its count the
/// most.
fn apic_timer(vm: &Vm<'_>) -> Result<(), String> {
    let mut page = vm.get_state(WHvVirtualProcessorStateTypeInterruptControllerState2)?;
    for (at, value) in [
        (0xF0usize, 0x1FFu32),
        (0x3E0, 0xB),
        (0x320, (1 << 17) | (1 << 16) | 0x30),
        (0x380, u32::MAX),
    ] {
        page.get_mut(at..at + 4)
            .ok_or("a short APIC state")?
            .copy_from_slice(&value.to_le_bytes());
    }
    vm.set_state(WHvVirtualProcessorStateTypeInterruptControllerState2, &page)
}

/// Two readings of the vCPU's TSC, the partition's reference time and the APIC timer's
/// current count, APART apart: whether each moved. Each reading says its own error.
fn clocks(vm: &Vm<'_>, label: &str) {
    let read = || {
        (
            vm.get1(WHvX64RegisterTsc),
            vm.reference_time(),
            apic_count(vm).map(u64::from),
        )
    };
    let (t0, r0, c0) = read();
    std::thread::sleep(APART);
    let (t1, r1, c1) = read();
    let moved = |a: &Result<u64, String>, b: &Result<u64, String>| match (a, b) {
        (Ok(a), Ok(b)) => format!("{} ({a:#x} -> {b:#x})", b.wrapping_sub(*a) as i64),
        (Err(e), _) | (_, Err(e)) => format!("unread: {e}"),
    };
    println!(
        "{label}: over {} ms, TSC moved {}, reference time moved {} x100ns, APIC timer count fell {}",
        APART.as_millis(),
        moved(&t0, &t1),
        moved(&r0, &r1),
        moved(&c1, &c0),
    );
}

/// The first partition's questions on time; leaves its time running.
fn time_questions(vm: &mut Vm<'_>) -> Result<(), String> {
    clocks(vm, "a new partition, its vCPU never run");
    vm.start()?;
    let timer = apic_timer(vm);
    println!("APIC timer set counting through the APIC state page: {timer:?}");
    clocks(vm, "after a run, the vCPU out of it");
    vm.suspend()?;
    clocks(vm, "suspended");
    let before = vm.reference_time()?;
    let tsc_before = vm.get1(WHvX64RegisterTsc)?;
    let ahead = before + 10_000_000;
    let set = vm.set_reference_time(ahead);
    let after = vm.reference_time()?;
    let tsc_after = vm.get1(WHvX64RegisterTsc)?;
    println!(
        "reference time set to +10 s while suspended: {set:?}; read back {after} (asked {ahead}, was {before}); TSC {tsc_before:#x} -> {tsc_after:#x}"
    );
    let set_tsc = vm.set(&[WHvX64RegisterTsc], &[Value(reg64(tsc_before + 1_000_000))]);
    let tsc_set = vm.get1(WHvX64RegisterTsc)?;
    println!(
        "TSC set to +1,000,000 while suspended: {set_tsc:?}; read back {tsc_set:#x} (asked {:#x}); reference time now {}",
        tsc_before + 1_000_000,
        vm.reference_time()?
    );
    vm.resume()?;
    clocks(vm, "resumed by WHvResumePartitionTime");
    vm.suspend()?;
    let asked = vm.ask(LOAD, CODE, 0)?;
    println!("a run while suspended answered in {:.1} us", us(asked.took));
    clocks(vm, "after a run made while suspended");
    let running = vm.set_reference_time(vm.reference_time()?);
    println!("reference time set while running: {running:?}");
    Ok(())
}

/// The synthetic MSRs a Linux guest of shards' writes, written by the host as a restore
/// would; then what the guest reads of its reference TSC page.
fn hyperv_msrs(vm: &mut Vm<'_>) -> Result<(), String> {
    let set = vm.set(
        &[
            WHvRegisterGuestOsId,
            WHvX64RegisterHypercall,
            WHvRegisterVpAssistPage,
            WHvRegisterReferenceTsc,
        ],
        &[
            Value(reg64(0x8100_0006_1200_0000)),
            Value(reg64(HYPERCALL_PAGE | 1)),
            Value(reg64(VP_ASSIST | 1)),
            Value(reg64(TSC_PAGE | 1)),
        ],
    );
    println!("Hyper-V MSRs written by the host: {set:?}");
    set?;
    let (guest, sequence) = vm.guest_reference_time()?;
    let host = vm.reference_time()?;
    println!(
        "the guest's reference TSC page (sequence {sequence}) reads {guest} x100ns; the partition's reference time {host}: the guest {} x100ns behind",
        host as i64 - guest as i64
    );
    Ok(())
}

/// What a snapshot keeps of the vCPU and the partition.
struct Saved {
    names: Vec<WHV_REGISTER_NAME>,
    values: Vec<Value>,
    apic: Vec<u8>,
    xsave: Vec<u8>,
    reference_time: u64,
    tsc: u64,
}

/// Of `names`, those this host reads, each asked alone; and those it refuses, with why.
fn readable(
    vm: &Vm<'_>,
    names: &[(WHV_REGISTER_NAME, &'static str)],
    show: bool,
) -> (Vec<(WHV_REGISTER_NAME, &'static str)>, Vec<String>) {
    let mut found = Vec::new();
    let mut refused = Vec::new();
    for &(name, label) in names {
        match vm.get(&[name]) {
            Ok(v) => {
                found.push((name, label));
                if show {
                    print!("{label}={:#x} ", v.first().map(Value::u64).unwrap_or(0));
                }
            }
            Err(e) => refused.push(format!("{label} ({e})")),
        }
    }
    if show {
        println!();
    }
    (found, refused)
}

/// Saves the vCPU with the partition's time suspended, each step timed.
fn save(
    vm: &Vm<'_>,
    architectural: &[(WHV_REGISTER_NAME, &'static str)],
    optional: &[(WHV_REGISTER_NAME, &'static str)],
) -> Result<(Saved, [Duration; 5]), String> {
    let t = Instant::now();
    vm.suspend()?;
    let suspend = t.elapsed();
    let names: Vec<WHV_REGISTER_NAME> = architectural
        .iter()
        .chain(optional.iter().filter(|(n, _)| {
            // Read-only or the hypervisor's own: not set back.
            ![
                WHvX64RegisterMsrMtrrCap,
                WHvX64RegisterInitialApicId,
                WHvRegisterVpRuntime,
                WHvRegisterReferenceTscSequence,
                WHvX64RegisterApicCurrentCount,
                WHvX64RegisterPredCmd,
                WHvX64RegisterTscVirtualOffset,
            ]
            .contains(n)
        }))
        .map(|(n, _)| *n)
        .collect();
    let t = Instant::now();
    let values = vm.get(&names)?;
    let registers = t.elapsed();
    let t = Instant::now();
    let apic = vm.get_state(WHvVirtualProcessorStateTypeInterruptControllerState2)?;
    let apic_took = t.elapsed();
    let t = Instant::now();
    let xsave = vm.get_state(WHvVirtualProcessorStateTypeXsaveState)?;
    let xsave_took = t.elapsed();
    let t = Instant::now();
    let reference_time = vm.reference_time()?;
    let time_took = t.elapsed();
    let tsc = names
        .iter()
        .position(|n| *n == WHvX64RegisterTsc)
        .and_then(|i| values.get(i))
        .map(Value::u64)
        .unwrap_or(0);
    Ok((
        Saved {
            names,
            values,
            apic,
            xsave,
            reference_time,
            tsc,
        },
        [suspend, registers, apic_took, xsave_took, time_took],
    ))
}

/// The guest's RAM written to `path` as a sparse file, its all-zero pages left holes.
fn save_ram(ram: *const u8, path: &std::path::Path) -> Result<(Duration, usize), String> {
    let t = Instant::now();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut returned = 0u32;
    // SAFETY: an FSCTL with no input buffer on a handle of ours.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("FSCTL_SET_SPARSE"));
    }
    let mut written = 0;
    use std::os::windows::fs::FileExt as _;
    for page in 0..RAM / PAGE {
        // SAFETY: a page of the guest's RAM, which no vCPU runs on meanwhile.
        let bytes = unsafe { std::slice::from_raw_parts(ram.add(page * PAGE), PAGE) };
        if bytes.iter().any(|b| *b != 0) {
            file.seek_write(bytes, (page * PAGE) as u64)
                .map_err(|e| format!("writing page {page}: {e}"))?;
            written += 1;
        }
    }
    file.set_len(RAM as u64).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok((t.elapsed(), written))
}

/// The file's allocated ranges (FSCTL_QUERY_ALLOCATED_RANGES) and its size on disk.
fn allocated(path: &std::path::Path) -> Result<(Vec<(i64, i64)>, u64), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let query = FILE_ALLOCATED_RANGE_BUFFER {
        FileOffset: 0,
        Length: RAM as i64,
    };
    let mut out = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 64];
    let mut returned = 0u32;
    // SAFETY: an input range and an output buffer of their own sizes.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_QUERY_ALLOCATED_RANGES,
            (&raw const query).cast(),
            std::mem::size_of_val(&query) as u32,
            out.as_mut_ptr().cast(),
            std::mem::size_of_val(out.as_slice()) as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("FSCTL_QUERY_ALLOCATED_RANGES"));
    }
    let n = returned as usize / std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
    let ranges = out.iter().take(n).map(|r| (r.FileOffset, r.Length)).collect();
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let mut high = 0u32;
    // SAFETY: a NUL-terminated wide path.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    Ok((ranges, (u64::from(high) << 32) | u64::from(low)))
}

/// Guest RAM for a restore: a placeholder of RAM bytes that a private allocation replaces,
/// as shards reserves guest RAM; that allocation freed back to its placeholder, and a
/// copy-on-write view of `file` placed there. How long each step took.
fn restored_ram(file: &std::fs::File) -> Result<(*mut u8, [Duration; 3]), String> {
    // SAFETY: a placeholder of this process's, replaced by memory of its own and then by a
    // view, which only this probe references.
    unsafe {
        let process = GetCurrentProcess();
        let t = Instant::now();
        let base = VirtualAlloc2(
            process,
            std::ptr::null(),
            RAM,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        );
        if base.is_null() {
            return Err(last_error("VirtualAlloc2 placeholder"));
        }
        let private = VirtualAlloc2(
            process,
            base,
            RAM,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        );
        if private != base {
            return Err(last_error("VirtualAlloc2 replacing the placeholder"));
        }
        let reserved = t.elapsed();
        let t = Instant::now();
        if VirtualFree(base, RAM, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
            return Err(last_error("VirtualFree back to a placeholder"));
        }
        let freed = t.elapsed();
        let t = Instant::now();
        let section = CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_WRITECOPY,
            0,
            0,
            std::ptr::null(),
        );
        if section.is_null() {
            return Err(last_error("CreateFileMappingW(PAGE_WRITECOPY)"));
        }
        let view = MapViewOfFile3(
            section,
            process,
            base,
            0,
            RAM,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_WRITECOPY,
            std::ptr::null_mut(),
            0,
        );
        CloseHandle(section);
        if view.Value != base {
            return Err(last_error("MapViewOfFile3(PAGE_WRITECOPY)"));
        }
        Ok((base.cast(), [reserved, freed, t.elapsed()]))
    }
}

/// How guest RAM is made for a restore: a copy-on-write view of the memory file placed
/// into the placeholder shards reserves, or private memory the file is read into.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ram {
    Cow,
    Read,
}

/// A view of `section`, `RAM` bytes at its start, with `protection`, wherever the system
/// places it.
fn view_anywhere(section: windows_sys::Win32::Foundation::HANDLE, protection: u32) -> Result<*mut u8, String> {
    // SAFETY: a fresh view of the section, which only this probe references.
    let view = unsafe {
        MapViewOfFile3(
            section,
            GetCurrentProcess(),
            std::ptr::null(),
            0,
            RAM,
            0,
            protection,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value.is_null() {
        return Err(last_error("MapViewOfFile3"));
    }
    Ok(view.Value.cast())
}

fn section_of(file: windows_sys::Win32::Foundation::HANDLE, protection: u32) -> Result<windows_sys::Win32::Foundation::HANDLE, String> {
    // SAFETY: a section of the file (or of the pagefile, for an invalid handle) of RAM bytes.
    let section = unsafe {
        CreateFileMappingW(file, std::ptr::null(), protection, 0, RAM as u32, std::ptr::null())
    };
    if section.is_null() {
        return Err(last_error("CreateFileMappingW"));
    }
    Ok(section)
}

/// Which views WHvMapGpaRange takes as guest RAM, and with which flags, each on a partition
/// of its own: the copy-on-write view a restore would place, and others beside it. Whether
/// the copy-on-write view in a placeholder is taken read, write and execute.
fn views(api: &Api, st: &State, bank: u64, path: &std::path::Path) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::PAGE_READONLY;
    let ro = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let rw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let rwx = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
    let rx = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagExecute;
    let mut cow_placed = false;
    // Each: a label; the section's file and protection (None: private memory); the view's
    // protection; whether it is placed into a placeholder.
    type Case<'a> = (&'a str, Option<(windows_sys::Win32::Foundation::HANDLE, u32)>, u32, bool);
    let cases: [Case<'_>; 8] = [
        ("read-only file, WRITECOPY section, WRITECOPY view in a placeholder", Some((ro.as_raw_handle(), PAGE_WRITECOPY)), PAGE_WRITECOPY, true),
        ("read-only file, WRITECOPY section, WRITECOPY view anywhere", Some((ro.as_raw_handle(), PAGE_WRITECOPY)), PAGE_WRITECOPY, false),
        ("read-only file, READONLY section, WRITECOPY view anywhere", Some((ro.as_raw_handle(), PAGE_READONLY)), PAGE_WRITECOPY, false),
        ("read-only file, READONLY section, READONLY view anywhere", Some((ro.as_raw_handle(), PAGE_READONLY)), PAGE_READONLY, false),
        ("pagefile, READWRITE section, WRITECOPY view anywhere", Some((INVALID_HANDLE_VALUE, PAGE_READWRITE)), PAGE_WRITECOPY, false),
        ("read-write file, READWRITE section, WRITECOPY view anywhere", Some((rw.as_raw_handle(), PAGE_READWRITE)), PAGE_WRITECOPY, false),
        ("read-write file, READWRITE section, READWRITE view anywhere", Some((rw.as_raw_handle(), PAGE_READWRITE)), PAGE_READWRITE, false),
        ("private memory", None, PAGE_READWRITE, false),
    ];
    for (label, section, protection, placed) in cases {
        let (p, _) = partition(api, st, bank, false)?;
        let made: Result<(*mut u8, bool), String> = match section {
            None => {
                // SAFETY: fresh private memory, kept until this case ends.
                let m = unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
                if m.is_null() { Err(last_error("VirtualAlloc")) } else { Ok((m.cast(), false)) }
            }
            Some((handle, section_protection)) if placed => {
                let _ = (handle, section_protection);
                restored_ram(&ro).map(|(base, _)| (base, true))
            }
            Some((handle, section_protection)) => section_of(handle, section_protection).and_then(|section| {
                let view = view_anywhere(section, protection);
                // SAFETY: the section, which its view (if any) holds open by itself.
                unsafe { CloseHandle(section) };
                view.map(|v| (v, true))
            }),
        };
        let (base, is_view) = match made {
            Ok(m) => m,
            Err(e) => {
                println!("view: {label}: not made: {e}");
                // SAFETY: the partition made above, nothing in it.
                unsafe { (api.delete_partition)(p) };
                continue;
            }
        };
        let mut said = Vec::new();
        for (flags, name) in [(rwx, "RWX"), (rx, "RX"), (WHvMapGpaRangeFlagRead, "R")] {
            // SAFETY: memory kept until this case ends.
            let hr = unsafe { (api.map)(p, base.cast(), 0, RAM as u64, flags) };
            if hr >= 0 {
                said.push(format!("{name} taken"));
                if placed && flags == rwx {
                    cow_placed = true;
                }
                // SAFETY: the range just mapped.
                unsafe { (api.unmap)(p, 0, RAM as u64) };
            } else {
                said.push(format!("{name} refused {:#010x}", hr as u32));
            }
        }
        println!("view: {label}: {}", said.join(", "));
        if section.is_none() {
            // The same memory on a partition whose time is suspended, before and after a map.
            let (q, _) = partition(api, st, bank, true)?;
            // SAFETY: memory kept until this case ends.
            let hr = unsafe { (api.map)(q, base.cast(), 0, RAM as u64, rwx) };
            println!("view: private memory, time suspended before the map: {:#010x}", hr as u32);
            // SAFETY: the partition, nothing mapped in it.
            unsafe { (api.delete_partition)(q) };
            let (q, _) = partition(api, st, bank, false)?;
            // SAFETY: as above; half the memory, then the other half once time is suspended.
            unsafe {
                let first = (api.map)(q, base.cast(), 0, (RAM / 2) as u64, rwx);
                let suspended = (st.suspend)(q);
                let second = (api.map)(q, base.add(RAM / 2).cast(), (RAM / 2) as u64, (RAM / 2) as u64, rwx);
                let resumed = (st.resume)(q);
                let third = (api.map)(q, base.add(RAM / 2).cast(), (RAM / 2) as u64, (RAM / 2) as u64, rwx);
                println!(
                    "view: private memory, half mapped {:#010x}, time suspended {:#010x}, the other half mapped {:#010x}; time resumed {:#010x}, mapped then {:#010x}",
                    first as u32, suspended as u32, second as u32, resumed as u32, third as u32
                );
                (api.delete_partition)(q);
            }
        }
        // SAFETY: the partition, then the memory it mapped, which nothing maps any more.
        unsafe {
            (api.delete_partition)(p);
            if is_view {
                unmap_view(base);
            } else {
                VirtualFree(base.cast(), 0, MEM_RELEASE);
            }
        }
    }
    Ok(cow_placed)
}

fn unmap_view(base: *mut u8) {
    // SAFETY: a view this probe placed, which no partition maps any more.
    unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS { Value: base.cast() },
            0,
        );
    }
}

/// A restore's steps, each timed.
#[derive(Default)]
struct Steps {
    partition: Vec<Duration>,
    suspend: Vec<Duration>,
    reserve: Vec<Duration>,
    free: Vec<Duration>,
    view: Vec<Duration>,
    map: Vec<Duration>,
    vcpu: Vec<Duration>,
    apic_base: Vec<Duration>,
    apic: Vec<Duration>,
    xsave: Vec<Duration>,
    registers: Vec<Duration>,
    reference_time: Vec<Duration>,
    whole: Vec<Duration>,
    first_answer: Vec<Duration>,
    touch_first: Vec<Duration>,
    touch_next: Vec<Duration>,
    dirty_first: Vec<Duration>,
    dirty_next: Vec<Duration>,
}

/// What every partition of the probe is made with.
struct Host<'a> {
    api: &'a Api,
    st: &'a State,
    bank: u64,
}

/// Restores `saved` into a new partition with RAM a copy-on-write view of `file`; on the
/// first round, checks what the guest finds. Leaves nothing behind.
fn restore(
    host: &Host<'_>,
    saved: &Saved,
    file: &std::fs::File,
    first: bool,
    kind: Ram,
    steps: &mut Steps,
) -> Result<(), String> {
    let whole = Instant::now();
    let Host { api, st, bank } = *host;
    let (p, [made, _]) = partition(api, st, bank, false)?;
    CURRENT.store(p, Ordering::Relaxed);
    steps.partition.push(made);
    let mut vm = Vm {
        api,
        st,
        partition: p,
        rip: 0,
    };
    let result = (|| -> Result<(), String> {
        let ram = match kind {
            Ram::Cow => {
                let (ram, [reserve, free, view]) = restored_ram(file)?;
                steps.reserve.push(reserve);
                steps.free.push(free);
                steps.view.push(view);
                ram
            }
            Ram::Read => {
                let t = Instant::now();
                // SAFETY: fresh private memory, released as this restore ends.
                let ram = unsafe { VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
                    .cast::<u8>();
                if ram.is_null() {
                    return Err(last_error("VirtualAlloc"));
                }
                use std::os::windows::fs::FileExt as _;
                // SAFETY: RAM bytes of memory this restore owns.
                let buf = unsafe { std::slice::from_raw_parts_mut(ram, RAM) };
                file.seek_read(buf, 0).map_err(|e| e.to_string())?;
                steps.view.push(t.elapsed());
                ram
            }
        };
        let done = (|| -> Result<(), String> {
            let t = Instant::now();
            // SAFETY: a view kept until the partition is deleted.
            check("WHvMapGpaRange view", unsafe {
                (api.map)(
                    p,
                    ram.cast(),
                    0,
                    RAM as u64,
                    WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
                )
            })?;
            steps.map.push(t.elapsed());
            let t = Instant::now();
            // SAFETY: the partition, set up.
            check("WHvCreateVirtualProcessor", unsafe { (api.create_vp)(p, 0, 0) })?;
            steps.vcpu.push(t.elapsed());
            if first {
                // What a partition never run keeps of its time, unsuspended.
                let (t0, r0) = (vm.get1(WHvX64RegisterTsc)?, vm.reference_time()?);
                std::thread::sleep(APART);
                let (t1, r1) = (vm.get1(WHvX64RegisterTsc)?, vm.reference_time()?);
                println!(
                    "a restore's partition, never run nor suspended, over {} ms: TSC {t0:#x} -> {t1:#x}, reference time {r0} -> {r1}",
                    APART.as_millis()
                );
            }
            let t = Instant::now();
            vm.suspend()?;
            steps.suspend.push(t.elapsed());
            let base = saved
                .names
                .iter()
                .position(|n| *n == WHvX64RegisterApicBase)
                .and_then(|i| saved.values.get(i))
                .copied()
                .ok_or("no APIC base saved")?;
            let t = Instant::now();
            vm.set(&[WHvX64RegisterApicBase], &[base])?;
            steps.apic_base.push(t.elapsed());
            let t = Instant::now();
            vm.set_state(WHvVirtualProcessorStateTypeInterruptControllerState2, &saved.apic)?;
            steps.apic.push(t.elapsed());
            let t = Instant::now();
            vm.set_state(WHvVirtualProcessorStateTypeXsaveState, &saved.xsave)?;
            steps.xsave.push(t.elapsed());
            let t = Instant::now();
            vm.set(&saved.names, &saved.values)?;
            steps.registers.push(t.elapsed());
            let t = Instant::now();
            vm.set_reference_time(saved.reference_time)?;
            steps.reference_time.push(t.elapsed());
            if first {
                let tsc = vm.get1(WHvX64RegisterTsc)?;
                let time = vm.reference_time()?;
                let apic = vm.get_state(WHvVirtualProcessorStateTypeInterruptControllerState2)?;
                let xsave = vm.get_state(WHvVirtualProcessorStateTypeXsaveState)?;
                let differ = |a: &[u8], b: &[u8]| a.iter().zip(b).filter(|(x, y)| x != y).count();
                println!(
                    "restored, still suspended: TSC {tsc:#x} (saved {:#x}), reference time {time} (saved {}), APIC state {} bytes differ of {}, XSAVE {} bytes differ of {}",
                    saved.tsc,
                    saved.reference_time,
                    differ(&apic, &saved.apic),
                    apic.len(),
                    differ(&xsave, &saved.xsave),
                    xsave.len()
                );
                std::thread::sleep(APART);
                println!(
                    "{} ms later, still suspended: TSC {:#x}, reference time {}",
                    APART.as_millis(),
                    vm.get1(WHvX64RegisterTsc)?,
                    vm.reference_time()?
                );
            }
            // The original was saved stopped at its `out`, which the host completes by
            // stepping past it, as it does after every exit.
            vm.rip = saved
                .names
                .iter()
                .position(|n| *n == WHvX64RegisterRip)
                .and_then(|i| saved.values.get(i))
                .map(|v| v.u64() + 1)
                .ok_or("no RIP saved")?;
            steps.whole.push(whole.elapsed());
            let answer = vm.ask(LOAD, DATA, 0)?;
            steps.first_answer.push(answer.took);
            if first {
                println!(
                    "the restored guest's load of the page it stored into: {:#x} (stored 0x5a5a_0001_0002_0003)",
                    answer.rax
                );
                let tsc = vm.ask(TSC, 0, 0)?.rax;
                println!(
                    "its TSC now {tsc:#x}: {} ticks past the saved one",
                    tsc.wrapping_sub(saved.tsc) as i64
                );
                let (guest, sequence) = vm.guest_reference_time()?;
                println!(
                    "its reference TSC page (sequence {sequence}) reads {guest}: {} x100ns past the saved reference time; the partition's reads {}",
                    guest as i64 - saved.reference_time as i64,
                    vm.reference_time()?
                );
                let hole = vm.ask(LOAD, HOLE, 0)?.rax;
                println!("a page the file holds as a hole reads {hole:#x}");
                let value = 0x7777_6666_5555_4444u64;
                vm.ask(STORE, DATA + 8, value)?;
                vm.ask(STORE, HOLE + 8, value)?;
                // SAFETY: the view's pages, read on this thread while the vCPU is out.
                let (in_view, in_hole) = unsafe {
                    (
                        std::ptr::read_volatile(ram.add(DATA as usize + 8).cast::<u64>()),
                        std::ptr::read_volatile(ram.add(HOLE as usize + 8).cast::<u64>()),
                    )
                };
                let mut on_disk = [0u8; 8];
                use std::os::windows::fs::FileExt as _;
                file.seek_read(&mut on_disk, DATA + 8).map_err(|e| e.to_string())?;
                let mut hole_disk = [0u8; 8];
                file.seek_read(&mut hole_disk, HOLE + 8).map_err(|e| e.to_string())?;
                println!(
                    "the guest's stores: the view reads {in_view:#x} and {in_hole:#x}; the file {:#x} and {:#x}",
                    u64::from_le_bytes(on_disk),
                    u64::from_le_bytes(hole_disk)
                );
            }
            let t = vm.ask(TOUCH, 0, RAM as u64)?;
            steps.touch_first.push(t.took);
            steps.touch_next.push(vm.ask(TOUCH, 0, RAM as u64)?.took);
            // Past the code, tables and overlay pages: the guest's own pages from 256 KiB.
            let from = 0x40000u64;
            steps.dirty_first.push(vm.ask(DIRTY, from, RAM as u64 - from)?.took);
            steps.dirty_next.push(vm.ask(DIRTY, from, RAM as u64 - from)?.took);
            Ok(())
        })();
        // SAFETY: the partition's vCPU and the partition, which nothing runs.
        unsafe {
            (api.delete_vp)(p, 0);
        }
        CURRENT.store(0, Ordering::Relaxed);
        unsafe {
            (api.delete_partition)(p);
        }
        match kind {
            Ram::Cow => unmap_view(ram),
            // SAFETY: the memory this restore allocated, which nothing maps any more.
            Ram::Read => unsafe {
                VirtualFree(ram.cast(), 0, MEM_RELEASE);
            },
        }
        done
    })();
    if result.is_err() {
        CURRENT.store(0, Ordering::Relaxed);
    }
    result
}

/// Two partitions alive in one process, each mapping private memory at GPA 0: whether the
/// second's map is taken while the first lives, and once the first is deleted.
fn two_partitions(api: &Api, st: &State, bank: u64) -> Result<(), String> {
    let rwx = WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;
    // SAFETY: private memory of this probe's, two RAM-sized regions, released below.
    let (m1, m2) = unsafe {
        (
            VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE),
            VirtualAlloc(std::ptr::null(), RAM, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE),
        )
    };
    if m1.is_null() || m2.is_null() {
        return Err(last_error("VirtualAlloc"));
    }
    let (p1, _) = partition(api, st, bank, false)?;
    let (p2, _) = partition(api, st, bank, false)?;
    // SAFETY: each partition maps memory kept until it is deleted.
    unsafe {
        let first = (api.map)(p1, m1, 0, RAM as u64, rwx);
        let second = (api.map)(p2, m2, 0, RAM as u64, rwx);
        let elsewhere = (api.map)(p2, m2, 1 << 30, RAM as u64, rwx);
        (api.delete_partition)(p1);
        let after = (api.map)(p2, m2, 0, RAM as u64, rwx);
        (api.delete_partition)(p2);
        println!(
            "two partitions in one process: the first's map {:#010x}; the second's at GPA 0 {:#010x}, at 1 GiB {:#010x}; once the first is deleted {:#010x}",
            first as u32, second as u32, elsewhere as u32, after as u32
        );
        VirtualFree(m1, 0, MEM_RELEASE);
        VirtualFree(m2, 0, MEM_RELEASE);
    }
    Ok(())
}

fn probe(api: &Api, st: &State) -> Result<(), String> {
    let host = whp_run::capability(api, WHvCapabilityCodeSyntheticProcessorFeaturesBanks)
        // SAFETY: the union member that code fills.
        .map(|c| unsafe { c.SyntheticProcessorFeaturesBanks.Anonymous.AsUINT64[0] })
        .ok_or("no synthetic features")?;
    let bank = host & SYNTHETIC;
    println!("host synthetic features {host:#x}; offered {bank:#x}");
    let (p, [made, _]) = partition(api, st, bank, false)?;
    println!("partition set up in {:.1} us", us(made));
    CURRENT.store(p, Ordering::Relaxed);
    let ram = first_guest(api, p)?;
    let mut vm = Vm {
        api,
        st,
        partition: p,
        rip: CODE,
    };
    long_mode(&vm)?;
    time_questions(&mut vm)?;
    hyperv_msrs(&mut vm)?;
    vm.ask(STORE, DATA, 0x5a5a_0001_0002_0003)?;
    let (architectural, refused) = readable(&vm, ARCHITECTURAL, false);
    println!(
        "architectural registers: {} read, refused: [{}]",
        architectural.len(),
        refused.join(", ")
    );
    let (optional, refused) = readable(&vm, OPTIONAL, true);
    println!("optional registers refused: [{}]", refused.join(", "));
    let (saved, [suspend, registers, apic, xsave, time]) = save(&vm, &architectural, &optional)?;
    println!(
        "saved: {} registers in {:.1} us, APIC state {} bytes in {:.1} us, XSAVE {} bytes in {:.1} us, reference time {} in {:.1} us; suspend {:.1} us",
        saved.names.len(),
        us(registers),
        saved.apic.len(),
        us(apic),
        saved.xsave.len(),
        us(xsave),
        saved.reference_time,
        us(time),
        us(suspend)
    );
    // The directory named on the command line, else one beside the probe's working directory.
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(format!("m345-{}", std::process::id())));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("memory");
    let (took, pages) = save_ram(ram, &path)?;
    let (ranges, on_disk) = allocated(&path)?;
    println!(
        "RAM saved sparse in {:.1} us: {pages} of {} pages written; {} allocated ranges {ranges:x?}; {on_disk} bytes on disk of {RAM}",
        us(took),
        RAM / PAGE,
        ranges.len()
    );
    // The original goes on: its time resumed, as a VM that resumes after its snapshot would.
    vm.resume()?;
    let answer = vm.ask(LOAD, DATA, 0)?;
    println!("the original goes on: it loads {:#x}", answer.rax);
    // A VM process holds one partition: the original's goes before any restore's is made.
    // SAFETY: the first partition's vCPU and the partition, which nothing runs.
    unsafe {
        (api.delete_vp)(p, 0);
    }
    CURRENT.store(0, Ordering::Relaxed);
    unsafe {
        (api.delete_partition)(p);
    }
    two_partitions(api, st, bank)?;
    let cow = views(api, st, bank, &path)?;
    let kind = if cow { Ram::Cow } else { Ram::Read };
    println!("restores make their RAM as {kind:?}");
    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let host = Host { api, st, bank };
    // The first restore checks what its guest finds, waiting between readings: the rounds
    // after it are the ones timed.
    restore(&host, &saved, &file, true, kind, &mut Steps::default())?;
    let mut steps = Steps::default();
    for _ in 0..ROUNDS {
        restore(&host, &saved, &file, false, kind, &mut steps)?;
    }
    let (ranges, on_disk) = allocated(&path)?;
    println!(
        "after {} restores the file has {} allocated ranges, {on_disk} bytes on disk",
        ROUNDS + 1,
        ranges.len()
    );
    for (what, v) in [
        ("partition made and set up", &mut steps.partition),
        ("time suspended", &mut steps.suspend),
        ("RAM reserved and committed", &mut steps.reserve),
        ("RAM freed back to its placeholder", &mut steps.free),
        ("copy-on-write view placed, or the file read in", &mut steps.view),
        ("view mapped at stage 2", &mut steps.map),
        ("vCPU made", &mut steps.vcpu),
        ("APIC base set", &mut steps.apic_base),
        ("APIC state set", &mut steps.apic),
        ("XSAVE set", &mut steps.xsave),
        ("registers set", &mut steps.registers),
        ("reference time set", &mut steps.reference_time),
        ("whole restore, to the run", &mut steps.whole),
        ("first answer (run, time resumed)", &mut steps.first_answer),
        ("first touch of 512 pages", &mut steps.touch_first),
        ("next touch", &mut steps.touch_next),
        ("first store into 448 pages", &mut steps.dirty_first),
        ("next store into them", &mut steps.dirty_next),
    ] {
        println!("restore {what} (us): {}", summary(v));
    }
    drop(file);
    let _ = std::fs::remove_dir_all(&dir);
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
    let st = match bind_state() {
        Ok(st) => st,
        Err(e) => {
            println!("unbound: {e}");
            return;
        }
    };
    let done = AtomicBool::new(false);
    let cancel = api.cancel;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let until = Instant::now() + WATCHDOG;
            while !done.load(Ordering::Relaxed) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            let p = CURRENT.load(Ordering::Relaxed);
            if !done.load(Ordering::Relaxed) && p != 0 {
                // SAFETY: a partition this probe made; a call on one deleted meanwhile fails.
                unsafe { cancel(p, 0, 0) };
            }
        });
        if let Err(e) = probe(&api, &st) {
            println!("stopped: {e}");
        }
        done.store(true, Ordering::Relaxed);
    });
}
