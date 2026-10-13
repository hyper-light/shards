//! The WHP backend (Windows on x86_64): a Windows Hypervisor Platform partition, with the
//! contract every backend presents (hv/mod.rs): device accesses complete inside
//! [`Vcpu::run`] through [`Io`], and `run` returns only a kick or a reset; the guest powers
//! off and resets through the machine's devices, as on KVM. The hypervisor emulates each
//! vCPU's local APIC (XApic) and runs HLT itself; this backend emulates the IOAPIC
//! (arch/x86_64/ioapic.rs), asking WHP for each interrupt and taking each level-triggered
//! EOI back from its exit, and decodes MMIO instructions itself (arch/x86_64/mmio.rs), since
//! WHP hands it an access's bytes and not its effect. Its partitions offer the guest
//! Hyper-V's clocks, which a Linux guest of the `hyperv` kernel keeps time by. Ground truth:
//! WinHvPlatform.h and WinHvPlatformDefs.h (Windows SDK; mingw-w64's copy), and
//! docs/research/platform-measurements.md M197-M198.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

mod ffi;

use std::cell::Cell;
use std::ffi::c_void;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use self::ffi::*;
use super::{Exit, Io};
use crate::arch::x86_64::ioapic::{Ioapic, Message, Sent};
use crate::arch::x86_64::mmio::{self, Access, Extend, Mode};
use crate::arch::x86_64::{Boot, Segment, cpuid, layout};
use crate::snapshot::codec::{self, DecodeError, Reader, Writer};
use crate::sync::lock;

/// A failed WHP call, or guest behavior the VMM does not emulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Call { op: &'static str, hr: i32 },
    Guest(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Call { op, hr } => {
                write!(f, "{op} failed: HRESULT {:#010x}", *hr as u32)?;
                // A Win32 error carried as an HRESULT (FACILITY_WIN32) has its system text.
                if (*hr as u32) & 0xFFFF_0000 == 0x8007_0000 {
                    write!(f, " ({})", std::io::Error::from_raw_os_error(*hr & 0xFFFF))?;
                }
                Ok(())
            }
            Error::Guest(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(op: &'static str, hr: i32) -> Result<()> {
    if hr < 0 {
        Err(Error::Call { op, hr })
    } else {
        Ok(())
    }
}

/// The synthetic processor features a partition offers its guest, of the host's
/// (WHV_SYNTHETIC_PROCESSOR_FEATURES, WinHvPlatformDefs.h): the hypervisor's presence and
/// HV#1; the partition's reference counter and TSC page and the frequency MSRs, which a
/// Linux guest calibrates and keeps time by; the APIC's synthetic MSRs and VP assist page,
/// hypercalls and the VP index, which Linux's Hyper-V setup writes; and the TLB-flush and
/// cluster-IPI hypercalls with extended processor masks, which its hints recommend (PM
/// M198). Not synthetic timers, which the host offers only with the SynIC, nor anything a
/// VMBus guest would use: the guest's APIC timer, emulated by the hypervisor, keeps its
/// time.
const SYNTHETIC: u64 = (1 << 0) // HypervisorPresent
    | (1 << 1) // Hv1
    | (1 << 3) // AccessPartitionReferenceCounter
    | (1 << 6) // AccessIntrCtrlRegs
    | (1 << 7) // AccessHypercallRegs
    | (1 << 8) // AccessVpIndex
    | (1 << 9) // AccessPartitionReferenceTsc
    | (1 << 11) // AccessFrequencyRegs
    | (1 << 24) // ExtendedProcessorMasks
    | (1 << 25) // TbFlushHypercalls
    | (1 << 26); // SyntheticClusterIpi

/// What a guest cannot keep time without, of [`SYNTHETIC`]: a host that lacks any runs no
/// VM. The rest is offered where the host has it.
const NEEDED: u64 = (1 << 0) | (1 << 1) | (1 << 3) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 11);

/// WHV_CAPABILITY_FEATURES' LocalApicEmulation.
const LOCAL_APIC_EMULATION: u64 = 1 << 1;

fn capability(code: WHV_CAPABILITY_CODE) -> Result<WHV_CAPABILITY> {
    let mut cap = WHV_CAPABILITY::default();
    let mut written = 0u32;
    // SAFETY: a buffer of the union's size, which the call fills.
    check("WHvGetCapability", unsafe {
        WHvGetCapability(
            code,
            (&raw mut cap).cast(),
            size_of::<WHV_CAPABILITY>() as u32,
            &raw mut written,
        )
    })?;
    Ok(cap)
}

/// The features this host's partitions offer their guests: [`SYNTHETIC`] of what it has,
/// asked once a process. Only an answer that passed is kept: a host whose platform is
/// missing now is looked at again next time.
fn features() -> std::result::Result<u64, String> {
    static FEATURES: OnceLock<u64> = OnceLock::new();
    if let Some(f) = FEATURES.get() {
        return Ok(*f);
    }
    let f = probe_host()?;
    Ok(*FEATURES.get_or_init(|| f))
}

fn probe_host() -> std::result::Result<u64, String> {
    if !ffi::loaded() {
        return Err(
            "the Windows Hypervisor Platform is not on this host: turn on the optional feature \
                    \"Windows Hypervisor Platform\" (HypervisorPlatform) and restart"
                .into(),
        );
    }
    let says = |e: Error| format!("asking the Windows Hypervisor Platform: {e}");
    // SAFETY: the union member each code fills.
    let present = unsafe {
        capability(WHvCapabilityCodeHypervisorPresent)
            .map_err(says)?
            .HypervisorPresent
    };
    if present == 0 {
        return Err(
            "no hypervisor runs on this host: hardware virtualization is off in its firmware, or Windows \
                    started without Hyper-V"
                .into(),
        );
    }
    // SAFETY: as above.
    let features = unsafe {
        capability(WHvCapabilityCodeFeatures)
            .map_err(says)?
            .Features
            .AsUINT64
    };
    if features & LOCAL_APIC_EMULATION == 0 {
        return Err(
            "this host's Windows Hypervisor Platform cannot emulate a local APIC, which shards' guests need \
                    (Windows 10 1809 or later)"
                .into(),
        );
    }
    // SAFETY: as above.
    let bank = unsafe {
        capability(WHvCapabilityCodeSyntheticProcessorFeaturesBanks)
            .map_err(says)?
            .SyntheticProcessorFeaturesBanks
            .Anonymous
            .AsUINT64[0]
    };
    if bank & NEEDED != NEEDED {
        return Err(format!(
            "this host's hypervisor offers its guests no clock shards' guests can keep time by: synthetic \
             features {bank:#x}, where {NEEDED:#x} are needed"
        ));
    }
    Ok(bank & SYNTHETIC)
}

/// Ok when this host can run VMs: WHP is there, a hypervisor runs, and it offers what shards
/// needs.
pub fn check_host() -> std::result::Result<(), String> {
    features().map(drop)
}

/// The most vCPUs one VM can have: as many as the hypervisor tells a partition's guest it
/// supports (its leaf 0x40000005, Hyper-V TLFS "Implementation Limits"), read for vCPU 0
/// of a partition of one, once a process ([`host`]); and at most 254, since the MADT
/// carries 8-bit APIC ids. WHP sets up a partition of 1,024 (PM M198), but creates no vCPU
/// past what its guest reads there: 240 on GitHub's runners, the 241st refused
/// (HV_STATUS_INVALID_VP_INDEX). The root partition's own leaf says more than a guest's: a
/// VM of as many vCPUs as it said failed at its 241st (CI run 38215340140).
pub fn max_vcpus() -> Result<u32> {
    host().map(|h| h.max_vcpus)
}

/// What this host's partitions give their guests, read once a process from vCPU 0 of a
/// partition of one, set up as every VM's is: a partition offered no synthetic features
/// gives its guest no hypervisor leaves to read (PM M202: 0 there). Only an answer that
/// passed is kept.
#[derive(Debug)]
struct Host {
    max_vcpus: u32,
    /// The CPUID leaves a VM's topology is written into ([`template`]).
    cpuid: Vec<cpuid::Leaf>,
    /// Whether the host's processor has an invariant TSC, as its own CPUID says.
    invariant_tsc: bool,
}

fn host() -> Result<&'static Host> {
    static HOST: OnceLock<Host> = OnceLock::new();
    if let Some(h) = HOST.get() {
        return Ok(h);
    }
    let partition = partition(1)?;
    // SAFETY: the partition just set up.
    check("WHvCreateVirtualProcessor", unsafe {
        WHvCreateVirtualProcessor(partition.0, 0, 0)
    })?;
    let read = |function: u32, index: u32| -> Result<WHV_CPUID_OUTPUT> {
        let mut out = WHV_CPUID_OUTPUT::default();
        // SAFETY: the partition's vCPU 0, its CPUID read into a value of its own size.
        check("WHvGetVirtualProcessorCpuidOutput", unsafe {
            WHvGetVirtualProcessorCpuidOutput(partition.0, 0, function, index, &raw mut out)
        })?;
        Ok(out)
    };
    let limits = read(0x4000_0005, 0);
    let leaves = template(&read);
    // SAFETY: the vCPU made above, which never ran.
    unsafe { WHvDeleteVirtualProcessor(partition.0, 0) };
    let (limits, leaves) = (limits?, leaves?);
    if limits.Eax == 0 {
        return Err(Error::Guest(
            "the hypervisor gives a partition's guest no limit to its vCPUs (CPUID 0x40000005)".into(),
        ));
    }
    let extended = std::arch::x86_64::__cpuid(0x8000_0000).eax;
    let h = Host {
        max_vcpus: limits.Eax.min(254),
        cpuid: leaves,
        invariant_tsc: extended >= 0x8000_0007
            && std::arch::x86_64::__cpuid(0x8000_0007).edx & cpuid::INVARIANT_TSC != 0,
    };
    Ok(HOST.get_or_init(|| h))
}

/// The most subleaves of a cache leaf (4, 0x8000001D) read: far past the four levels of
/// cache a processor has.
const CACHE_SUBLEAVES: u32 = 16;

/// The CPUID leaves [`cpuid::for_vcpu`] writes a VM's topology into, as the hypervisor
/// gives `read`'s vCPU (PM M206): the maxima, leaf 1, the caches (4 and 0x8000001D, each to
/// its first subleaf of no type), the extended topologies where the basic leaves reach
/// them (0xB, 0x1F), invariant TSC (0x80000007), the package's threads (0x80000008) and
/// AMD's topology (0x8000001E).
fn template(read: &dyn Fn(u32, u32) -> Result<WHV_CPUID_OUTPUT>) -> Result<Vec<cpuid::Leaf>> {
    let leaf = |function: u32, index: u32, flags: u32| {
        read(function, index).map(|o| cpuid::Leaf {
            function,
            index,
            flags,
            eax: o.Eax,
            ebx: o.Ebx,
            ecx: o.Ecx,
            edx: o.Edx,
        })
    };
    let basic = leaf(0, 0, 0)?;
    let extended = leaf(0x8000_0000, 0, 0)?;
    let mut leaves = vec![basic, extended, leaf(1, 0, 0)?];
    let caches = |function: u32, leaves: &mut Vec<cpuid::Leaf>| -> Result<()> {
        for index in 0..CACHE_SUBLEAVES {
            let l = leaf(function, index, cpuid::SIGNIFICANT_INDEX)?;
            leaves.push(l);
            if l.eax & 0x1f == 0 {
                break;
            }
        }
        Ok(())
    };
    if basic.eax >= 4 {
        caches(4, &mut leaves)?;
    }
    for function in [0xb, 0x1f] {
        if basic.eax >= function {
            for index in 0..3 {
                leaves.push(leaf(function, index, cpuid::SIGNIFICANT_INDEX)?);
            }
        }
    }
    for function in [0x8000_0007, 0x8000_0008] {
        if extended.eax >= function {
            leaves.push(leaf(function, 0, 0)?);
        }
    }
    if extended.eax >= 0x8000_001d {
        caches(0x8000_001d, &mut leaves)?;
    }
    if extended.eax >= 0x8000_001e {
        leaves.push(leaf(0x8000_001e, 0, 0)?);
    }
    Ok(leaves)
}

/// The CPUID results a VM of `vcpus` vCPUs is given (PM M206): one package of
/// single-thread cores, as KVM's guests get ([`cpuid::for_vcpu`]), each result in only
/// the bits it changes of the hypervisor's ([`cpuid::masked`]); and invariant TSC where the
/// host's processor has it, which Windows Server 2025 leaves out of its partitions'
/// CPUID though their TSC runs at the host's fixed rate, a partition never moving to
/// another host (Server 2022 gives it, on the same processor). With both, a Linux guest's
/// APs take the boot CPU's delay-loop calibration rather than each timing the boot CPU's
/// ticks for itself, which took 63 APs past a minute on a 4-CPU host (CI run 38240024887).
fn cpuid_results(host: &Host, vcpus: u32) -> Vec<cpuid::Masked> {
    cpuid::masked(&host.cpuid, vcpus, host.invariant_tsc)
}

/// `results` as WHP takes them (WHvPartitionPropertyCodeCpuidResultList2).
fn cpuid_list(results: &[cpuid::Masked]) -> Vec<WHV_X64_CPUID_RESULT2> {
    results
        .iter()
        .map(|m| {
            let output = |[eax, ebx, ecx, edx]: [u32; 4]| WHV_CPUID_OUTPUT {
                Eax: eax,
                Ebx: ebx,
                Ecx: ecx,
                Edx: edx,
            };
            let subleaf = if m.subleaf {
                WHvX64CpuidResult2FlagSubleafSpecific
            } else {
                0
            };
            let (vp, one) = match m.vcpu {
                Some(vp) => (vp, WHvX64CpuidResult2FlagVpSpecific),
                None => (0, 0),
            };
            WHV_X64_CPUID_RESULT2 {
                Function: m.function,
                Index: m.index,
                VpIndex: vp,
                Flags: subleaf | one,
                Output: output(m.value),
                Mask: output(m.mask),
            }
        })
        .collect()
}

/// A partition: deleted, its vCPUs with it, once nothing holds it. With what it maps of this
/// process's memory, where the instruction emulator's accesses to guest RAM go
/// ([`Vcpu::emulate`]).
#[derive(Debug)]
struct Partition(WHV_PARTITION_HANDLE, Mutex<Vec<Mapped>>);

/// `len` bytes of guest-physical memory from `gpa`, at `host` in this process.
#[derive(Debug, Clone, Copy)]
struct Mapped {
    gpa: u64,
    host: usize,
    len: u64,
}

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: the partition this value made; nothing runs in it once nothing holds it.
        unsafe { WHvDeletePartition(self.0) };
    }
}

impl Partition {
    fn set<T>(&self, code: WHV_PARTITION_PROPERTY_CODE, value: &T, op: &'static str) -> Result<()> {
        // SAFETY: a property of its own size.
        check(op, unsafe {
            WHvSetPartitionProperty(self.0, code, (value as *const T).cast(), size_of::<T>() as u32)
        })
    }

    /// Maps `len` bytes at `host` at `gpa`, as `flags` say.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until unmapped or the partition is deleted.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, flags: WHV_MAP_GPA_RANGE_FLAGS) -> Result<()> {
        // SAFETY: forwarded caller contract.
        check("WHvMapGpaRange", unsafe {
            WHvMapGpaRange(self.0, host.cast_const().cast::<c_void>(), gpa, len as u64, flags)
        })?;
        lock(&self.1).push(Mapped {
            gpa,
            host: host as usize,
            len: len as u64,
        });
        Ok(())
    }

    fn unmap(&self, gpa: u64, len: u64) -> Result<()> {
        // SAFETY: the partition's own mapping, which nothing else uses.
        check("WHvUnmapGpaRange", unsafe { WHvUnmapGpaRange(self.0, gpa, len) })?;
        lock(&self.1).retain(|m| m.gpa != gpa || m.len != len);
        Ok(())
    }

    /// Where `len` bytes of guest RAM at `gpa` lie in this process, if one mapping holds
    /// them all.
    fn host(&self, gpa: u64, len: u64) -> Option<*mut u8> {
        lock(&self.1).iter().find_map(|m| {
            let offset = gpa.checked_sub(m.gpa)?;
            (offset.checked_add(len)? <= m.len).then(|| (m.host + offset as usize) as *mut u8)
        })
    }

    /// Sends each of `sent` to its local APICs (WHvRequestInterrupt).
    fn deliver(&self, sent: &Sent) -> Result<()> {
        for m in sent.iter() {
            let kind = match m.delivery {
                0 => WHvX64InterruptTypeFixed,
                1 => WHvX64InterruptTypeLowestPriority,
                4 => WHvX64InterruptTypeNmi,
                5 => WHvX64InterruptTypeInit,
                d => {
                    return Err(Error::Guest(format!(
                        "an IOAPIC entry of delivery mode {d} (vector {:#x}), which shards does not deliver",
                        m.vector
                    )));
                }
            };
            let control = WHV_INTERRUPT_CONTROL {
                // Type (7:0), DestinationMode (11:8), TriggerMode (15:12): WinHvPlatformDefs.h.
                _bitfield: kind as u64
                    | (u64::from(m.logical) * WHvX64InterruptDestinationModeLogical as u64) << 8
                    | (u64::from(m.level) * WHvX64InterruptTriggerModeLevel as u64) << 12,
                Destination: u32::from(m.destination),
                Vector: u32::from(m.vector),
            };
            // SAFETY: a control of its own size.
            check("WHvRequestInterrupt", unsafe {
                WHvRequestInterrupt(
                    self.0,
                    &raw const control,
                    size_of::<WHV_INTERRUPT_CONTROL>() as u32,
                )
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub vcpus: u32,
}

/// What maps the memory a virtio-mem device plugs into the guest ([`Vm::add_hotplug`]):
/// each block mapped into the partition as it is plugged and unmapped as it is unplugged,
/// so the guest reaches no memory the host has withheld.
#[derive(Debug, Clone)]
pub struct Stage2(Arc<Partition>);

impl Stage2 {
    /// Maps `len` bytes at `host` at `gpa`.
    ///
    /// # Safety
    /// As every backend's: `host..host+len` must stay mapped in this process until
    /// unmapped here or the VM is destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map(host, gpa, len, RWX) }
    }

    pub fn unmap(&self, gpa: u64, len: usize) -> Result<()> {
        self.0.unmap(gpa, len as u64)
    }
}

const RWX: WHV_MAP_GPA_RANGE_FLAGS =
    WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute;

/// The VM. Guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    partition: Arc<Partition>,
    ioapic: Arc<Ioapic>,
    vcpus: u32,
    /// Its vCPUs' CPUID, set once the last of them exists ([`Vm::create_vcpu`]).
    cpuid: Vec<cpuid::Masked>,
}

/// A partition of `vcpus` vCPUs, set up as every one of shards' is: its local APICs
/// emulated by the hypervisor (XApic), its guest offered [`SYNTHETIC`] of the host's
/// synthetic features, which the hypervisor sets up only with the APICs it emulates
/// (HV_STATUS_INVALID_PARAMETER, 0xc0350005, without; CI run 38222358454).
fn partition(vcpus: u32) -> Result<Partition> {
    let features = features().map_err(Error::Guest)?;
    let mut handle: WHV_PARTITION_HANDLE = 0;
    // SAFETY: a handle for the call to fill.
    check("WHvCreatePartition", unsafe {
        WHvCreatePartition(&raw mut handle)
    })?;
    let partition = Partition(handle, Mutex::new(Vec::new()));
    partition.set(
        WHvPartitionPropertyCodeProcessorCount,
        &vcpus,
        "WHvSetPartitionProperty(ProcessorCount)",
    )?;
    partition.set(
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        &WHvX64LocalApicEmulationModeXApic,
        "WHvSetPartitionProperty(LocalApicEmulationMode)",
    )?;
    partition.set(
        WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks,
        &WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
            BanksCount: 1,
            Reserved0: 0,
            Anonymous: WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS_0 { AsUINT64: [features] },
        },
        "WHvSetPartitionProperty(SyntheticProcessorFeaturesBanks)",
    )?;
    // SAFETY: the partition just configured.
    check("WHvSetupPartition", unsafe { WHvSetupPartition(partition.0) })?;
    Ok(partition)
}

/// Whether this process forbids itself dynamic code once a partition is set up
/// ([`forbid_dynamic_code_once_set_up`]).
static FORBID_DYNAMIC_CODE: AtomicBool = AtomicBool::new(false);

/// Has this process forbid itself dynamic code (ACG) as soon as its next partition is set
/// up, for good: a VM process's choice, made before it reads anything of its guest's (D141).
/// Not a partition's own: windows-2022 sets up no partition in a process that has forbidden
/// it, a VM process's own included, while every call a VM makes after its setup works
/// (PM M201, M202); and a process that runs many VMs, a test's, sets many up.
pub fn forbid_dynamic_code_once_set_up() {
    FORBID_DYNAMIC_CODE.store(true, Ordering::Relaxed);
}

/// Forbids this process dynamic code from here on (SetProcessMitigationPolicy,
/// ProcessDynamicCodePolicy).
fn forbid_dynamic_code() -> Result<()> {
    use windows_sys::Win32::System::SystemServices::{
        PROCESS_MITIGATION_DYNAMIC_CODE_POLICY, PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0,
    };
    use windows_sys::Win32::System::Threading::{ProcessDynamicCodePolicy, SetProcessMitigationPolicy};
    // ProhibitDynamicCode, the policy's first flag (winnt.h).
    let policy = PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
        Anonymous: PROCESS_MITIGATION_DYNAMIC_CODE_POLICY_0 { Flags: 1 },
    };
    // SAFETY: a policy of its own size, for this process.
    let set = unsafe {
        SetProcessMitigationPolicy(
            ProcessDynamicCodePolicy,
            (&raw const policy).cast(),
            size_of::<PROCESS_MITIGATION_DYNAMIC_CODE_POLICY>(),
        )
    };
    if set == 0 {
        // HRESULT_FROM_WIN32 of the call's error, as WHP's own failures are reported.
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        return Err(Error::Call {
            op: "SetProcessMitigationPolicy(ProcessDynamicCodePolicy)",
            hr: ((code & 0xFFFF) | 0x8007_0000) as i32,
        });
    }
    Ok(())
}

impl Vm {
    /// A partition of `config.vcpus` vCPUs, their local APICs emulated by the hypervisor, its
    /// guest offered [`SYNTHETIC`] of the host's synthetic features.
    pub fn new(config: VmConfig) -> Result<Vm> {
        let cpuid = cpuid_results(host()?, config.vcpus);
        let partition = partition(config.vcpus)?;
        if FORBID_DYNAMIC_CODE.load(Ordering::Relaxed) {
            forbid_dynamic_code()?;
        }
        Ok(Vm {
            partition: Arc::new(partition),
            ioapic: Arc::new(Ioapic::default()),
            vcpus: config.vcpus,
            cpuid,
        })
    }

    /// Snapshots of WHP VMs are not taken yet.
    pub fn save_state(&self) -> Result<VmState> {
        Err(no_snapshots())
    }

    pub fn restore_state(&self, _: &VmState) -> Result<()> {
        Err(no_snapshots())
    }

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until the VM is destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.partition.map(host, gpa, len, RWX) }
    }

    /// What takes guest RAM from the guest and gives it back, from whichever thread gives
    /// its pages back to the host as the guest reports them free (D151).
    pub fn stage2(&self) -> Stage2 {
        Stage2(self.partition.clone())
    }

    /// The memory a virtio-mem device plugs, `len` bytes at `host` seen at `gpa` (D133):
    /// none of it mapped until plugged, block by block, through the [`Stage2`] this returns.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until the VM is destroyed.
    pub unsafe fn add_hotplug(&self, _host: *mut u8, _gpa: u64, _len: usize) -> Result<Stage2> {
        Ok(Stage2(self.partition.clone()))
    }

    /// Maps device memory (a virtio-pmem region) at `gpa`, writable by the guest only if
    /// `writable`.
    ///
    /// # Safety
    /// As for `map_ram`.
    pub unsafe fn map_device_memory(
        &self,
        host: *mut u8,
        gpa: u64,
        len: usize,
        writable: bool,
    ) -> Result<()> {
        let flags = if writable {
            RWX
        } else {
            WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagExecute
        };
        // SAFETY: forwarded caller contract.
        unsafe { self.partition.map(host, gpa, len, flags) }
    }

    /// A handle for driving interrupt lines from any thread.
    pub fn irqs(&self) -> Irqs {
        Irqs {
            partition: self.partition.clone(),
            ioapic: self.ioapic.clone(),
        }
    }

    /// Creates vCPU `index` on the calling thread, which runs it. Application processors
    /// are created held in wait-for-SIPI, which the boot processor's INIT and SIPI end
    /// (PM M198). The last one sets every vCPU's CPUID: WHP takes a result for one vCPU
    /// only once that vCPU exists (HV_STATUS_INVALID_VP_INDEX at setup, PM M206), and no
    /// vCPU runs before all of them exist, made in index order (D5).
    pub fn create_vcpu(&self, index: usize) -> Result<Vcpu> {
        let index = u32::try_from(index).map_err(|_| Error::Guest(format!("vCPU {index}")))?;
        // SAFETY: the partition, set up.
        check("WHvCreateVirtualProcessor", unsafe {
            WHvCreateVirtualProcessor(self.partition.0, index, 0)
        })?;
        if index + 1 == self.vcpus && !self.cpuid.is_empty() {
            let list = cpuid_list(&self.cpuid);
            // SAFETY: the list, of its own size.
            check("WHvSetPartitionProperty(CpuidResultList2)", unsafe {
                WHvSetPartitionProperty(
                    self.partition.0,
                    WHvPartitionPropertyCodeCpuidResultList2,
                    list.as_ptr().cast(),
                    size_of_val(list.as_slice()) as u32,
                )
            })?;
        }
        Ok(Vcpu {
            partition: self.partition.clone(),
            ioapic: self.ioapic.clone(),
            index,
            emulator: None,
        })
    }
}

fn no_snapshots() -> Error {
    Error::Guest("snapshots of microVMs are not taken on the Windows Hypervisor Platform yet".into())
}

/// A snapshot's state of a WHP VM: there is none yet ([`Vm::save_state`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState(());

/// A snapshot's state of a WHP vCPU: there is none yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuState(());

impl VmState {
    pub fn encode(&self, _: &mut Writer) {}

    pub fn decode(_: &mut Reader<'_>) -> codec::Result<VmState> {
        Err(DecodeError(no_snapshots().to_string()))
    }
}

impl VcpuState {
    pub fn encode(&self, _: &mut Writer) {}

    pub fn decode(_: &mut Reader<'_>) -> codec::Result<VcpuState> {
        Err(DecodeError(no_snapshots().to_string()))
    }

    /// Where this vCPU's TSC starts: nowhere, as no snapshot is taken.
    pub fn tsc_start(&self) -> Option<TscStart> {
        None
    }
}

/// A restored vCPU's TSC: none, as no snapshot is taken.
#[derive(Debug, Clone)]
pub struct Tsc(());

/// Where a restored TSC starts.
#[derive(Debug, Clone, Copy)]
pub struct TscStart(());

impl Tsc {
    pub fn restart(&self, _: TscStart) -> Result<Option<u64>> {
        Ok(None)
    }
}

/// Drives the IOAPIC's pins (GSIs) from any thread.
#[derive(Debug, Clone)]
pub struct Irqs {
    partition: Arc<Partition>,
    ioapic: Arc<Ioapic>,
}

impl Irqs {
    pub fn set(&self, gsi: u32, level: bool) -> Result<()> {
        self.partition.deliver(&self.ioapic.set(gsi, level))
    }

    /// One edge on an edge-triggered pin: raised and lowered, as KVM's backend pulses one.
    pub fn pulse(&self, gsi: u32) -> Result<()> {
        self.set(gsi, true)?;
        self.set(gsi, false)
    }

    /// Delivers the MSI of `address` and `data`, as the IOAPIC delivers its messages.
    pub fn msi(&self, address: u64, data: u32) -> Result<()> {
        self.partition.deliver(&Sent::one(Message::of_msi(address, data)))
    }
}

/// x86's segment as WHP takes it: the descriptor's access rights in VMX's layout.
fn segment(s: &Segment) -> Value {
    let mut v = WHV_REGISTER_VALUE::default();
    let attributes = u16::from(s.kind & 0xF)
        | u16::from(s.code_or_data) << 4
        | u16::from(s.present) << 7
        | u16::from(s.long) << 13
        | u16::from(s.db) << 14
        | u16::from(s.granular) << 15;
    v.Segment = WHV_X64_SEGMENT_REGISTER {
        Base: s.base,
        Limit: s.limit,
        Selector: s.selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    };
    Value(v)
}

fn table(base: u64, limit: u16) -> Value {
    let mut v = WHV_REGISTER_VALUE::default();
    v.Table = WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: limit,
        Base: base,
    };
    Value(v)
}

/// A vCPU, run by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    partition: Arc<Partition>,
    ioapic: Arc<Ioapic>,
    index: u32,
    /// Microsoft's instruction emulator, made the first time an MMIO access is by an
    /// instruction shards does not decode itself ([`Vcpu::emulate`]).
    emulator: Option<Emulator>,
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: this vCPU, which no thread runs once it drops.
        unsafe { WHvDeleteVirtualProcessor(self.partition.0, self.index) };
    }
}

/// An instruction emulator (WinHvEmulation.h), destroyed as it drops; used by the thread
/// that runs its vCPU alone.
#[derive(Debug)]
struct Emulator(*mut c_void);

// SAFETY: a handle the emulator's library made, used by one thread at a time: its vCPU's,
// which takes it along.
unsafe impl Send for Emulator {}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: the emulator this value made.
        unsafe { WHvEmulatorDestroyEmulator(self.0) };
    }
}

/// What the emulator's callbacks reach while it emulates one instruction: the vCPU, the
/// VMM's devices, and the first failure of a device's, which no HRESULT carries.
struct Emulating<'a> {
    vcpu: &'a Vcpu,
    io: &'a dyn Io,
    failed: Cell<Option<Error>>,
}

/// The emulator's callbacks (WinHvEmulation.h, WHV_EMULATOR_CALLBACKS): each given the
/// [`Emulating`] its call was.
mod emulate {
    use super::*;

    /// E_FAIL.
    const FAILED: windows_sys::core::HRESULT = 0x8000_4005_u32 as i32;

    /// # Safety
    /// `context` is the [`Emulating`] of the call that is emulating.
    unsafe fn emulating<'a>(context: *const c_void) -> &'a Emulating<'a> {
        // SAFETY: as the caller says.
        unsafe { &*context.cast::<Emulating<'a>>() }
    }

    /// Port I/O, which shards emulates itself before it asks the emulator anything.
    pub unsafe extern "system" fn io_port(
        _: *const c_void,
        _: *mut WHV_EMULATOR_IO_ACCESS_INFO,
    ) -> windows_sys::core::HRESULT {
        FAILED
    }

    /// An access of 1 to 8 bytes at a GPA: guest RAM where the partition maps it, else the
    /// VMM's devices.
    pub unsafe extern "system" fn memory(
        context: *const c_void,
        access: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
    ) -> windows_sys::core::HRESULT {
        // SAFETY: the emulator's context and access, for this call.
        let (cx, access) = unsafe { (emulating(context), &mut *access) };
        let size = usize::from(access.AccessSize).min(access.Data.len());
        let Some(data) = access.Data.get_mut(..size) else {
            return FAILED;
        };
        let gpa = access.GpaAddress;
        let write = access.Direction != 0;
        if let Some(at) = cx.vcpu.partition.host(gpa, size as u64) {
            for (i, b) in data.iter_mut().enumerate() {
                // SAFETY: inside a mapping of guest RAM, which stays mapped while the VM
                // lives; byte by byte, as the guest's other vCPUs may touch it meanwhile.
                unsafe {
                    if write {
                        at.add(i).write_volatile(*b);
                    } else {
                        *b = at.add(i).read_volatile();
                    }
                }
            }
            return 0;
        }
        if !write {
            cx.vcpu.mmio_read(cx.io, gpa, data);
            return 0;
        }
        match cx.vcpu.mmio_write(cx.io, gpa, data) {
            Ok(()) => 0,
            Err(e) => {
                cx.failed.set(Some(e));
                FAILED
            }
        }
    }

    pub unsafe extern "system" fn get_registers(
        context: *const c_void,
        names: *const WHV_REGISTER_NAME,
        count: u32,
        values: *mut WHV_REGISTER_VALUE,
    ) -> windows_sys::core::HRESULT {
        // SAFETY: the emulator's context, names and values, `count` of each.
        unsafe {
            let cx = emulating(context);
            WHvGetVirtualProcessorRegisters(cx.vcpu.partition.0, cx.vcpu.index, names, count, values)
        }
    }

    pub unsafe extern "system" fn set_registers(
        context: *const c_void,
        names: *const WHV_REGISTER_NAME,
        count: u32,
        values: *const WHV_REGISTER_VALUE,
    ) -> windows_sys::core::HRESULT {
        // SAFETY: the emulator's context, names and values, `count` of each.
        unsafe {
            let cx = emulating(context);
            WHvSetVirtualProcessorRegisters(cx.vcpu.partition.0, cx.vcpu.index, names, count, values)
        }
    }

    /// The GPA of a guest-virtual page, as the vCPU's own page tables have it.
    pub unsafe extern "system" fn translate(
        context: *const c_void,
        gva: u64,
        flags: WHV_TRANSLATE_GVA_FLAGS,
        result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
        gpa: *mut u64,
    ) -> windows_sys::core::HRESULT {
        let mut translated = WHV_TRANSLATE_GVA_RESULT::default();
        // SAFETY: the emulator's context, result and GPA, for this call.
        unsafe {
            let cx = emulating(context);
            let hr = WHvTranslateGva(
                cx.vcpu.partition.0,
                cx.vcpu.index,
                gva,
                flags,
                &raw mut translated,
                gpa,
            );
            *result = translated.ResultCode;
            hr
        }
    }
}

impl Vcpu {
    pub fn save_state(&self) -> Result<VcpuState> {
        Err(no_snapshots())
    }

    pub fn restore_state(&mut self, _: &VcpuState) -> Result<()> {
        Err(no_snapshots())
    }

    /// No restore holds a TSC back.
    pub fn tsc(&self) -> Option<Tsc> {
        None
    }

    pub fn resume_tsc(&mut self, _: u64) -> Result<()> {
        Ok(())
    }

    /// Memory is not mapped ahead on WHP.
    pub fn pre_fault(&self, _: u64, _: u64) -> Result<bool> {
        Ok(false)
    }

    pub fn kicker(&self) -> Kicker {
        Kicker {
            partition: self.partition.clone(),
            index: self.index,
        }
    }

    fn set(&self, names: &[WHV_REGISTER_NAME], values: &[Value], op: &'static str) -> Result<()> {
        if names.len() != values.len() {
            return Err(Error::Guest(format!(
                "{op}: {} names, {} values",
                names.len(),
                values.len()
            )));
        }
        // SAFETY: `names.len()` names and as many values, 16-aligned (`Value`), whose
        // layout is WHV_REGISTER_VALUE's.
        check(op, unsafe {
            WHvSetVirtualProcessorRegisters(
                self.partition.0,
                self.index,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr().cast(),
            )
        })
    }

    fn get(&self, name: WHV_REGISTER_NAME) -> Result<u64> {
        let mut v = Value::default();
        // SAFETY: one name and one 16-aligned value.
        check("WHvGetVirtualProcessorRegisters", unsafe {
            WHvGetVirtualProcessorRegisters(self.partition.0, self.index, &raw const name, 1, &raw mut v.0)
        })?;
        Ok(v.get())
    }

    /// The boot vCPU's registers for the 64-bit boot protocol: every one the protocol
    /// names, over the reset state.
    pub fn boot(&mut self, b: &Boot) -> Result<()> {
        let data = segment(&b.data);
        self.set(
            &[
                WHvX64RegisterCr0,
                WHvX64RegisterCr3,
                WHvX64RegisterCr4,
                WHvX64RegisterEfer,
                WHvX64RegisterCs,
                WHvX64RegisterDs,
                WHvX64RegisterEs,
                WHvX64RegisterFs,
                WHvX64RegisterGs,
                WHvX64RegisterSs,
                WHvX64RegisterTr,
                WHvX64RegisterGdtr,
                WHvX64RegisterIdtr,
                WHvX64RegisterRip,
                WHvX64RegisterRsi,
                WHvX64RegisterRflags,
            ],
            &[
                Value::u64(b.cr0),
                Value::u64(b.cr3),
                Value::u64(b.cr4),
                Value::u64(b.efer),
                segment(&b.cs),
                data,
                data,
                data,
                data,
                data,
                segment(&b.tr),
                table(b.gdt.0, b.gdt.1),
                table(b.idt.0, b.idt.1),
                Value::u64(b.rip),
                Value::u64(b.rsi),
                Value::u64(0x2),
            ],
            "WHvSetVirtualProcessorRegisters(boot)",
        )
    }

    /// Runs the guest until a kick or a reset; device accesses complete through `io` on
    /// the way.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        loop {
            let mut exit = ExitContext::default();
            // SAFETY: an exit context of the size given, 16-aligned.
            check("WHvRunVirtualProcessor", unsafe {
                WHvRunVirtualProcessor(
                    self.partition.0,
                    self.index,
                    (&raw mut exit).cast(),
                    size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
                )
            })?;
            let e = &exit.0;
            match e.ExitReason {
                WHvRunVpExitReasonMemoryAccess => self.mmio(io, e)?,
                WHvRunVpExitReasonX64IoPortAccess => self.pio(io, e)?,
                WHvRunVpExitReasonX64ApicEoi => {
                    // SAFETY: the union member an EOI exit fills.
                    let vector = unsafe { e.Anonymous.ApicEoi.InterruptVector };
                    self.partition.deliver(&self.ioapic.eoi(vector as u8))?;
                }
                WHvRunVpExitReasonCanceled => return Ok(Exit::Canceled),
                // A triple fault: the guest crashed.
                WHvRunVpExitReasonUnrecoverableException => return Ok(Exit::Reset),
                WHvRunVpExitReasonX64Halt => {
                    return Err(Error::Guest("HLT exit despite the emulated LAPIC".into()));
                }
                other => {
                    return Err(Error::Guest(format!(
                        "unexpected WHP exit {other:#x} at rip {:#x}",
                        e.VpContext.Rip
                    )));
                }
            }
        }
    }

    /// The instruction's length, which WHP gives for port I/O exits (PM M198).
    fn length(e: &WHV_RUN_VP_EXIT_CONTEXT) -> u64 {
        u64::from(e.VpContext._bitfield & 0xF)
    }

    fn pio(&self, io: &dyn Io, e: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        // SAFETY: the union member a port I/O exit fills.
        let p = unsafe { e.Anonymous.IoPortAccess };
        // SAFETY: the access's bits: IsWrite (0), AccessSize (3:1), StringOp (4), RepPrefix (5).
        let info = unsafe { p.AccessInfo.AsUINT32 };
        let size = ((info >> 1) & 7) as usize;
        let port = p.PortNumber;
        if info & (1 << 4) != 0 || !matches!(size, 1 | 2 | 4) {
            return Err(Error::Guest(format!(
                "a string or {size}-byte port access at {port:#x}, which shards does not emulate"
            )));
        }
        let rip = e.VpContext.Rip.wrapping_add(Self::length(e));
        if info & 1 != 0 {
            io.pio_write(port, p.Rax.to_le_bytes().get(..size).unwrap_or_default());
            return self.set(
                &[WHvX64RegisterRip],
                &[Value::u64(rip)],
                "WHvSetVirtualProcessorRegisters(rip)",
            );
        }
        let mut data = [0u8; 8];
        if let Some(d) = data.get_mut(..size) {
            io.pio_read(port, d);
        }
        let size = size as u8;
        let rax = mmio::loaded(p.Rax, u64::from_le_bytes(data), size, size, false, Extend::None);
        self.set(
            &[WHvX64RegisterRax, WHvX64RegisterRip],
            &[Value::u64(rax), Value::u64(rip)],
            "WHvSetVirtualProcessorRegisters(rax, rip)",
        )
    }

    fn mmio(&mut self, io: &dyn Io, e: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        // SAFETY: the union member a memory exit fills.
        let m = unsafe { e.Anonymous.MemoryAccess };
        let gpa = m.Gpa;
        // SAFETY: the access's bits: AccessType (1:0), 0 read, 1 write, 2 execute.
        let kind = unsafe { m.AccessInfo.AsUINT32 } & 3;
        if kind == 2 {
            return Err(Error::Guest(format!(
                "the guest ran code at {gpa:#x}, where it has no memory (rip {:#x})",
                e.VpContext.Rip
            )));
        }
        // SAFETY: the execution state's bits: Cr0Pe (2), EferLma (4).
        let state = unsafe { e.VpContext.ExecutionState.AsUINT16 };
        // SAFETY: CS's access rights: L (13), D/B (14).
        let cs = unsafe { e.VpContext.Cs.Anonymous.Attributes };
        let mode = if state & (1 << 4) != 0 && cs & (1 << 13) != 0 {
            Some(Mode::Long)
        } else if state & (1 << 2) != 0 && cs & (1 << 14) != 0 {
            Some(Mode::Protected32)
        } else {
            None
        };
        let bytes = m
            .InstructionBytes
            .get(..usize::from(m.InstructionByteCount))
            .unwrap_or_default();
        let Some(decoded) = mode.and_then(|mode| mmio::decode(bytes, mode)) else {
            return self.emulate(io, e).map_err(|why| {
                Error::Guest(format!(
                    "an MMIO {} at {gpa:#x} by an instruction neither shards nor Windows' emulator emulates ({why}): {bytes:02x?} (rip {:#x})",
                    if kind == 1 { "write" } else { "read" },
                    e.VpContext.Rip
                ))
            });
        };
        let rip = e.VpContext.Rip.wrapping_add(u64::from(decoded.len));
        let write = |data: u64, size: u8| {
            let bytes = data.to_le_bytes();
            let bytes = bytes.get(..usize::from(size)).unwrap_or_default();
            self.mmio_write(io, gpa, bytes)
        };
        match decoded.access {
            Access::Store { reg, high, size } if kind == 1 => {
                let value = self.get(WHV_REGISTER_NAME::from(reg))?;
                write(mmio::stored(value, high, size), size)?;
                self.set(
                    &[WHvX64RegisterRip],
                    &[Value::u64(rip)],
                    "WHvSetVirtualProcessorRegisters(rip)",
                )
            }
            Access::StoreImm { value, size } if kind == 1 => {
                write(value, size)?;
                self.set(
                    &[WHvX64RegisterRip],
                    &[Value::u64(rip)],
                    "WHvSetVirtualProcessorRegisters(rip)",
                )
            }
            Access::Load {
                reg,
                high,
                size,
                dest,
                extend,
            } if kind == 0 => {
                let mut data = [0u8; 8];
                if let Some(d) = data.get_mut(..usize::from(size)) {
                    self.mmio_read(io, gpa, d);
                }
                let name = WHV_REGISTER_NAME::from(reg);
                // Only an 8- or 16-bit destination keeps the rest of its register.
                let old = if dest < 4 { self.get(name)? } else { 0 };
                let value = mmio::loaded(old, u64::from_le_bytes(data), size, dest, high, extend);
                self.set(
                    &[name, WHvX64RegisterRip],
                    &[Value::u64(value), Value::u64(rip)],
                    "WHvSetVirtualProcessorRegisters(load)",
                )
            }
            access => Err(Error::Guest(format!(
                "an MMIO access at {gpa:#x} whose instruction ({access:?}) is not of its direction"
            ))),
        }
    }

    /// Emulates the instruction of MMIO exit `e` with Windows' own instruction emulator
    /// (WinHvEmulation.h, WHvEmulatorTryMmioEmulation), for what shards' decoder does not
    /// take (arch/x86_64/mmio.rs): string instructions above all, as KVM's emulator takes
    /// them. Linux copies a page of a mount's options from its caller, however long they
    /// are (fs/namespace.c, copy_mount_options), so a REP MOVSB reads the page that follows
    /// them, which may be the control page a process of the guest maps.
    fn emulate(&mut self, io: &dyn Io, e: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        static CALLBACKS: WHV_EMULATOR_CALLBACKS = WHV_EMULATOR_CALLBACKS {
            Size: size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(emulate::io_port),
            WHvEmulatorMemoryCallback: Some(emulate::memory),
            WHvEmulatorGetVirtualProcessorRegisters: Some(emulate::get_registers),
            WHvEmulatorSetVirtualProcessorRegisters: Some(emulate::set_registers),
            WHvEmulatorTranslateGvaPage: Some(emulate::translate),
        };
        let emulator = match &self.emulator {
            Some(made) => made.0,
            None => {
                let mut made = std::ptr::null_mut();
                // SAFETY: callbacks that live as long as the process, and a handle for the
                // call to fill, destroyed as the Emulator holding it drops.
                check("WHvEmulatorCreateEmulator", unsafe {
                    WHvEmulatorCreateEmulator(&raw const CALLBACKS, &raw mut made)
                })?;
                self.emulator = Some(Emulator(made));
                made
            }
        };
        let cx = Emulating {
            vcpu: self,
            io,
            failed: Cell::new(None),
        };
        let mut status = WHV_EMULATOR_STATUS::default();
        // SAFETY: this vCPU's emulator; the context its callbacks are given, alive for the
        // call; and the exit's own contexts.
        let hr = unsafe {
            WHvEmulatorTryMmioEmulation(
                emulator,
                (&raw const cx).cast(),
                &raw const e.VpContext,
                &raw const e.Anonymous.MemoryAccess,
                &raw mut status,
            )
        };
        if let Some(failed) = cx.failed.take() {
            return Err(failed);
        }
        check("WHvEmulatorTryMmioEmulation", hr)?;
        // EmulationSuccessful, the status's first bit (WinHvEmulation.h).
        // SAFETY: the union's word, which every member shares.
        let bits = unsafe { status.AsUINT32 };
        if bits & 1 == 0 {
            return Err(Error::Guest(format!("its status {bits:#x}")));
        }
        Ok(())
    }

    fn mmio_read(&self, io: &dyn Io, gpa: u64, data: &mut [u8]) {
        match gpa.checked_sub(layout::IOAPIC) {
            Some(offset) if offset < 0x1000 => self.ioapic.read(offset, data),
            _ => io.mmio_read(gpa, data),
        }
    }

    fn mmio_write(&self, io: &dyn Io, gpa: u64, data: &[u8]) -> Result<()> {
        match gpa.checked_sub(layout::IOAPIC) {
            Some(offset) if offset < 0x1000 => self.partition.deliver(&self.ioapic.write(offset, data)),
            _ => {
                io.mmio_write(gpa, data);
                Ok(())
            }
        }
    }
}

/// Interrupts a vCPU's `run` from any thread: a cancel, which returns the run it finds or,
/// made before one, the next (PM M198).
#[derive(Debug, Clone)]
pub struct Kicker {
    partition: Arc<Partition>,
    index: u32,
}

impl Kicker {
    pub fn kick(&self) {
        // SAFETY: the partition, which this kicker holds; a vCPU that no longer exists
        // fails the call, harmlessly.
        unsafe { WHvCancelRunVirtualProcessor(self.partition.0, self.index, 0) };
    }
}
