//! The WHP backend on arm64 (Windows 11 24H2, build 26100.3915 or later): a Windows Hypervisor
//! Platform partition under the platform's own GICv3 at shards' places (arch/aarch64's
//! layout, QEMU virt's), with the contract every backend presents (hv/mod.rs):
//!
//! - A device access completes inside [`Vcpu::run`]: the backend decodes the syndrome the
//!   memory access exit carries, performs the access through [`Io`], writes Rt and advances
//!   PC, as KVM's `kvm_handle_mmio_return` does.
//! - PSCI runs in the hypervisor: a vCPU held in StartupSuspend starts at a CPU_ON, and
//!   SYSTEM_OFF and SYSTEM_RESET come back as a reset exit, which `run` returns as a
//!   shutdown or a reset (QEMU's target/arm/whpx/whpx-all.c handles no PSCI call itself).
//! - `run` returns only for a kick, a shutdown or a reset.
//! - A snapshot keeps each vCPU's registers, by their encodings as HVF's backend keeps them
//!   (arch/aarch64/state.rs), and the GIC's state in the platform's own layouts, the
//!   partition's global interrupt state and each vCPU's interrupt controller state
//!   (WHvGetVirtualProcessorState); [`restores`] says whether this host restores one.
//!
//! Ground truth: WinHvPlatform.h and WinHvPlatformDefs.h (Windows SDK 10.0.26100), and QEMU's
//! WHPX arm64 accelerator (target/arm/whpx, hw/intc/arm_gicv3_whpx.c), the platform's one
//! other user.

mod ffi;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use self::ffi::*;
use super::{Exit, Io};
use crate::arch::aarch64::state::Power;
use crate::arch::aarch64::{Entry, esr, layout, sysreg};
use crate::devices::get_le;
use crate::snapshot::codec::{DecodeError, Reader, Writer};

pub use crate::arch::aarch64::state::VcpuState;

/// The stage-2 page: WHP maps guest memory 4 KiB at a time.
pub const PAGE: u64 = 4 << 10;

/// A redistributor's frames, RD_base and SGI_base, one a vCPU: GICv3's.
const REDIST_SIZE: u64 = 0x2_0000;
/// The GICv3 distributor's frame.
const DIST_SIZE: u64 = 0x1_0000;
/// The virtual timer's PPI as shards' devicetree gives it (INTID 27, the EL1 virtual timer),
/// and the PMU's (INTID 23), as QEMU's virt gives them the platform's GIC.
const PPI_VTIMER: u32 = 27;
const PPI_PMU: u32 = 23;
/// The first SPI's INTID.
const SPI_BASE: u32 = 32;
/// The platform's GIC: M414's GICD_TYPER.ITLinesNumber 30, INTIDs to 991.
const SPIS: u32 = 960;

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

/// WHvGetCapability's answer to `code`, as the union's first words.
fn capability(code: u32) -> Result<[u64; 8]> {
    let mut buf = [0u64; 8];
    let mut written = 0u32;
    // SAFETY: a buffer of at least the union's size, which the call fills.
    check("WHvGetCapability", unsafe {
        WHvGetCapability(
            code,
            buf.as_mut_ptr().cast(),
            size_of_val(&buf) as u32,
            &raw mut written,
        )
    })?;
    Ok(buf)
}

/// Ok when this host can run VMs: WHP is there, a hypervisor runs, and its arm64 API is the
/// released one.
pub fn check_host() -> std::result::Result<(), String> {
    if !ffi::loaded() {
        return Err(
            "the Windows Hypervisor Platform is not on this host: turn on the optional feature \
             \"Windows Hypervisor Platform\" (HypervisorPlatform) and restart"
                .into(),
        );
    }
    let says = |e: Error| format!("asking the Windows Hypervisor Platform: {e}");
    let present = capability(CAP_HYPERVISOR_PRESENT).map_err(says)?;
    if present.first().is_none_or(|w| w & 1 == 0) {
        return Err(
            "no hypervisor runs on this host: virtualization is off in its firmware, or Windows started \
             without Hyper-V"
                .into(),
        );
    }
    let features = capability(CAP_FEATURES).map_err(says)?;
    if features.first().is_none_or(|w| w & FEATURE_ARM64_SUPPORT == 0) {
        return Err(
            "this Windows' hypervisor platform is the pre-release arm64 one of Windows 11 24H2 builds \
             before 26100.3915: update Windows"
                .into(),
        );
    }
    Ok(())
}

/// M414 set back the global GIC state and each vCPU's interrupt controller state, and
/// reset CNTVCT_EL0 independently on each vCPU: the state a snapshot restores here.
pub fn restores() -> bool {
    check_host().is_ok()
}

/// The most vCPUs one VM can have: as many as shards' GIC layout has redistributor frames
/// for, between GIC_REDIST and GIC_REDIST_MAX_END.
pub fn max_vcpus() -> Result<u32> {
    let frames = (layout::GIC_REDIST_MAX_END - layout::GIC_REDIST) / REDIST_SIZE;
    u32::try_from(frames).map_err(|_| Error::Guest(format!("{frames} redistributor frames")))
}

/// The widest guest-physical address the hypervisor maps (WHvCapabilityCodePhysicalAddressWidth).
pub fn max_ipa_bits() -> Result<u32> {
    let width = capability(CAP_PHYSICAL_ADDRESS_WIDTH)?;
    let bits = width.first().map_or(0, |w| *w as u32);
    if bits == 0 {
        return Err(Error::Guest(
            "the hypervisor gives no physical address width".into(),
        ));
    }
    Ok(bits)
}

/// The GIC's geometry: GICv3's frames, at QEMU virt's places, which the platform's GIC is
/// given ([`Vm::new`]).
#[derive(Debug, Clone, Copy)]
pub struct GicParams {
    pub dist_size: u64,
    pub dist_align: u64,
    /// One redistributor (RD_base + SGI_base frames).
    pub redist_size: u64,
    pub redist_align: u64,
    pub msi_size: u64,
    pub msi_align: u64,
    pub spi_base: u32,
    pub spi_count: u32,
}

pub fn gic_params() -> Result<GicParams> {
    Ok(GicParams {
        dist_size: DIST_SIZE,
        dist_align: 64 << 10,
        redist_size: REDIST_SIZE,
        redist_align: 64 << 10,
        msi_size: 4 << 10,
        msi_align: 4 << 10,
        spi_base: SPI_BASE,
        spi_count: SPIS,
    })
}

/// A GICv2m frame's place and the SPIs it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiFrame {
    pub base: u64,
    pub first_intid: u32,
    pub count: u32,
}

/// Where the GIC's parts are: its distributor and redistributors, which the platform's GIC
/// takes at shards' places alone; the GICv2m frame the VMM keeps for a PCI bus's MSIs
/// ([`Gic::send_msi`]); and an ITS, which it is never given.
#[derive(Debug, Clone, Copy)]
pub struct GicLayout {
    pub dist_base: u64,
    pub redist_base: u64,
    pub msi: Option<MsiFrame>,
    pub its: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Guest physical address width.
    pub ipa_bits: u32,
    /// MPIDR of each vCPU, by index.
    pub mpidrs: Vec<u64>,
    /// Whether the vCPUs have EL2: never here ([`el2_supported`]).
    pub el2: bool,
}

/// Whether a guest is given EL2: not by Windows' hypervisor platform on arm64 (QEMU's
/// arm_gicv3_whpx.c: "Nested virtualisation not currently supported by WHPX").
pub fn el2_supported() -> bool {
    false
}

/// Whether a PCI bus's MSIs go to an ITS: no, the partition is given none.
pub const ITS: bool = false;

/// Why a guest is given no EL2 here.
pub const NO_EL2: &str = "Windows' hypervisor platform gives a guest no EL2 on arm64";

/// Whether this process forbids itself dynamic code once a partition is set up
/// ([`forbid_dynamic_code_once_set_up`]).
static FORBID_DYNAMIC_CODE: AtomicBool = AtomicBool::new(false);

/// Has this process forbid itself dynamic code (ACG) as soon as its next partition is set up,
/// for good: a VM process's choice, made before it reads anything of its guest's (D141), as
/// on x86_64 (PM M201, M202: no partition is set up in a process that has forbidden it).
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

/// A partition: deleted, its vCPUs with it, once nothing holds it.
#[derive(Debug)]
struct Partition(ffi::Partition);

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: the partition this value made; nothing runs in it once nothing holds it.
        unsafe { WHvDeletePartition(self.0) };
    }
}

impl Partition {
    fn set<T>(&self, code: u32, value: &T, op: &'static str) -> Result<()> {
        // SAFETY: a property of its own size.
        check(op, unsafe {
            WHvSetPartitionProperty(self.0, code, (value as *const T).cast(), size_of::<T>() as u32)
        })
    }

    /// Maps `len` bytes at `host` at `gpa`, as `flags` say.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until unmapped or the partition is deleted.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, flags: u32) -> Result<()> {
        // SAFETY: forwarded caller contract.
        check("WHvMapGpaRange", unsafe {
            WHvMapGpaRange(self.0, host.cast_const().cast(), gpa, len as u64, flags)
        })
    }

    fn unmap(&self, gpa: u64, len: u64) -> Result<()> {
        // SAFETY: the partition's own mapping, which nothing else uses.
        check("WHvUnmapGpaRange", unsafe { WHvUnmapGpaRange(self.0, gpa, len) })
    }

    /// Registers `names` of vCPU `vp`, which is out of the guest, whole: a Q register's 128
    /// bits.
    fn values(&self, vp: u32, names: &[u32]) -> Result<Vec<Value>> {
        let mut values = vec![Value::default(); names.len()];
        self.get_values_into(vp, names, &mut values)?;
        Ok(values)
    }

    fn get_values_into(&self, vp: u32, names: &[u32], values: &mut [Value]) -> Result<()> {
        if names.len() != values.len() {
            return Err(Error::Guest(format!(
                "{} register names, {} values",
                names.len(),
                values.len()
            )));
        }
        let get = |names: &[u32], values: &mut [Value]| {
            // SAFETY: `names.len()` names and as many values, 16-aligned (`Value`).
            check("WHvGetVirtualProcessorRegisters", unsafe {
                WHvGetVirtualProcessorRegisters(
                    self.0,
                    vp,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_mut_ptr(),
                )
            })
        };
        if let Err(batch) = get(names, values) {
            for name in names {
                let mut value = [Value::default()];
                if let Err(one) = get(std::slice::from_ref(name), &mut value) {
                    return Err(Error::Guest(format!("vCPU {vp}'s register {name:#x}: {one}")));
                }
            }
            return Err(batch);
        }
        Ok(())
    }

    /// Registers `names` of vCPU `vp`, which is out of the guest.
    fn registers(&self, vp: u32, names: &[u32]) -> Result<Vec<u64>> {
        Ok(self.values(vp, names)?.iter().map(|v| v.0[0]).collect())
    }

    fn register(&self, vp: u32, name: u32) -> Result<u64> {
        let mut value = [Value::default()];
        self.get_values_into(vp, &[name], &mut value)?;
        Ok(value[0].0[0])
    }

    /// Sets registers `names` of vCPU `vp` to `values`, in their order.
    fn set_registers(&self, vp: u32, names: &[u32], values: &[u64]) -> Result<()> {
        match values {
            [one] => return self.set_values(vp, names, &[Value([*one, 0])]),
            [one, two] => {
                return self.set_values(vp, names, &[Value([*one, 0]), Value([*two, 0])]);
            }
            _ => {}
        }
        let values: Vec<Value> = values.iter().map(|&v| Value([v, 0])).collect();
        self.set_values(vp, names, &values)
    }

    /// Sets registers `names` of vCPU `vp` to `values`, whole, in their order. Where the
    /// hypervisor refuses them, each is set alone, to name the one it refuses.
    fn set_values(&self, vp: u32, names: &[u32], values: &[Value]) -> Result<()> {
        if names.len() != values.len() {
            return Err(Error::Guest(format!(
                "{} register names, {} values",
                names.len(),
                values.len()
            )));
        }
        let set = |names: &[u32], values: &[Value]| {
            // SAFETY: `names.len()` names and as many values, 16-aligned (`Value`).
            check("WHvSetVirtualProcessorRegisters", unsafe {
                WHvSetVirtualProcessorRegisters(
                    self.0,
                    vp,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_ptr(),
                )
            })
        };
        let Err(e) = set(names, values) else {
            return Ok(());
        };
        for (name, value) in names.iter().zip(values) {
            if let Err(one) = set(std::slice::from_ref(name), std::slice::from_ref(value)) {
                return Err(Error::Guest(format!("vCPU {vp}'s register {name:#x}: {one}")));
            }
        }
        Err(e)
    }

    /// Drives SPI or PPI `intid` to `level` (WHvRequestInterrupt), as QEMU's GIC does.
    fn interrupt(&self, intid: u32, level: bool) -> Result<()> {
        let control = InterruptControl {
            control: INTERRUPT_TYPE_FIXED | if level { INTERRUPT_ASSERTED } else { 0 },
            requested_vector: intid,
            ..InterruptControl::default()
        };
        // SAFETY: a control of its own size.
        check("WHvRequestInterrupt", unsafe {
            WHvRequestInterrupt(self.0, &raw const control, size_of::<InterruptControl>() as u32)
        })
    }

    /// State `kind` of vCPU `vp`, or the partition's own for [`ANY_VP`]
    /// (WHvGetVirtualProcessorState), at most `most` bytes, read into pages of its own.
    fn state(&self, vp: u32, kind: u32, most: usize) -> Result<Vec<u8>> {
        let mut pages = vec![Page([0; 4096]); most.div_ceil(4096)];
        let size = u32::try_from(size_of_val(pages.as_slice()))
            .map_err(|_| Error::Guest(format!("a state of {most} bytes")))?;
        let mut written = 0u32;
        // SAFETY: page-aligned memory of `size` bytes, which the call fills.
        check("WHvGetVirtualProcessorState", unsafe {
            WHvGetVirtualProcessorState(
                self.0,
                vp,
                kind,
                pages.as_mut_ptr().cast(),
                size,
                &raw mut written,
            )
        })?;
        let bytes: Vec<u8> = pages.iter().flat_map(|p| p.0).take(written as usize).collect();
        if bytes.len() != written as usize {
            return Err(Error::Guest(format!(
                "state {kind:#x} of vCPU {vp}: {written} bytes written into {size}"
            )));
        }
        Ok(bytes)
    }

    /// Sets state `kind` of vCPU `vp`, or the partition's own for [`ANY_VP`], to `bytes`
    /// (WHvSetVirtualProcessorState), handed over in pages of its own.
    fn set_state(&self, vp: u32, kind: u32, bytes: &[u8]) -> Result<()> {
        let mut pages = vec![Page([0; 4096]); bytes.len().div_ceil(4096).max(1)];
        for (page, chunk) in pages.iter_mut().zip(bytes.chunks(4096)) {
            if let Some(to) = page.0.get_mut(..chunk.len()) {
                to.copy_from_slice(chunk);
            }
        }
        let len = u32::try_from(bytes.len())
            .map_err(|_| Error::Guest(format!("a state of {} bytes", bytes.len())))?;
        // SAFETY: page-aligned memory holding the state's `len` bytes.
        check("WHvSetVirtualProcessorState", unsafe {
            WHvSetVirtualProcessorState(self.0, vp, kind, pages.as_ptr().cast(), len)
        })
    }
}

const RWX: u32 = MAP_READ | MAP_WRITE | MAP_EXECUTE;

/// Maps the memory a virtio-mem device plugs into the guest and unmaps it, block by block,
/// so the guest reaches no memory the host has withheld.
#[derive(Debug, Clone)]
pub struct Stage2(Arc<Partition>);

impl Stage2 {
    /// Maps `len` bytes at `host` at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map(host, gpa, len, RWX) }
    }

    /// Maps `len` bytes at `host` at `gpa` for a share's DAX window (D189): readable and
    /// executable, as a guest runs programs from its volumes, and writable only if
    /// `writable`.
    ///
    /// # Safety
    /// As [`Stage2::map`]'s.
    pub unsafe fn map_file(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<()> {
        let flags = if writable { RWX } else { MAP_READ | MAP_EXECUTE };
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map(host, gpa, len, flags) }
    }

    pub fn unmap(&self, gpa: u64, len: usize) -> Result<()> {
        self.0.unmap(gpa, len as u64)
    }
}

/// Maps the host memory of a device's window into the guest while the VM lives: data the
/// guest reads and writes, and never executes.
#[derive(Debug, Clone)]
pub struct Window(Arc<Partition>);

impl Window {
    /// Maps `len` bytes of host memory at `host` at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map(host, gpa, len, MAP_READ | MAP_WRITE) }
    }

    pub fn unmap(&self, gpa: u64, len: usize) -> Result<()> {
        self.0.unmap(gpa, len as u64)
    }
}

/// Device memory a device maps into the guest and unmaps, while the VM lives: a swap
/// window's (D159). Slots are KVM's notion; the partition maps by address, so the index
/// names nothing here.
#[derive(Debug, Clone)]
pub struct DeviceSlots(Arc<Partition>);

impl DeviceSlots {
    /// Maps `len` bytes of host memory at `host` at `gpa`, read-only to the guest unless
    /// `writable`, never executable.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, _i: u32, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<()> {
        let flags = if writable { MAP_READ | MAP_WRITE } else { MAP_READ };
        // SAFETY: forwarded caller contract.
        unsafe { self.0.map(host, gpa, len, flags) }
    }

    /// Unmaps `len` bytes at `gpa`: the guest's accesses there exit as MMIO to nothing.
    pub fn unmap(&self, _i: u32, gpa: u64, len: usize) -> Result<()> {
        self.0.unmap(gpa, len as u64)
    }
}

/// The interrupt controller: the platform's GICv3, which the partition was given as it was
/// made. Interrupts are asked for from any thread.
#[derive(Debug, Clone)]
pub struct Gic(Arc<Partition>);

impl Gic {
    /// Drives an SPI line. For edge-configured INTIDs `true` produces one edge.
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<()> {
        self.0.interrupt(intid, level)
    }

    /// Delivers an MSI written to the doorbell of the GICv2m frame the VMM keeps (the
    /// platform's GIC has none): an edge on SPI `intid`, its line raised and dropped, as a
    /// frame raises the SPIs its writes name.
    pub fn send_msi(&self, _doorbell: u64, intid: u32) -> Result<()> {
        self.0.interrupt(intid, true)?;
        self.0.interrupt(intid, false)
    }
}

/// The VM. Guest memory must outlive it, and it every [`Vcpu`].
#[derive(Debug)]
pub struct Vm {
    partition: Arc<Partition>,
    mpidrs: Vec<u64>,
    restore_counter: Option<u64>,
}

impl Vm {
    /// A partition of `config.mpidrs.len()` vCPUs under the platform's GICv3, its distributor
    /// and ITS frame at shards' places, given before the partition is set up as the platform
    /// requires (QEMU's whpx_accel_init); its redistributors are each vCPU's, given as it is
    /// made ([`Vm::create_vcpu`]). No LPIs: the ITS frame is reserved, never used.
    pub fn new(config: VmConfig) -> Result<Vm> {
        check_host().map_err(Error::Guest)?;
        if config.el2 {
            return Err(Error::Guest(NO_EL2.into()));
        }
        let vcpus = u32::try_from(config.mpidrs.len())
            .map_err(|_| Error::Guest(format!("{} vCPUs", config.mpidrs.len())))?;
        let mut handle: ffi::Partition = 0;
        // SAFETY: a handle for the call to fill.
        check("WHvCreatePartition", unsafe {
            WHvCreatePartition(&raw mut handle)
        })?;
        let partition = Partition(handle);
        partition.set(
            PROP_PROCESSOR_COUNT,
            &vcpus,
            "WHvSetPartitionProperty(ProcessorCount)",
        )?;
        let ic = IcParameters {
            emulation_mode: IC_EMULATION_GICV3,
            gic_v3: GicV3Parameters {
                gicd_base: layout::GIC_DIST,
                gits_translater_base: layout::GIC_ITS,
                gic_lpi_int_id_bits: 0,
                ppi_cntv_overflow: PPI_VTIMER,
                ppi_performance_monitors: PPI_PMU,
                ..GicV3Parameters::default()
            },
            ..IcParameters::default()
        };
        partition.set(
            PROP_ARM64_IC_PARAMETERS,
            &ic,
            "WHvSetPartitionProperty(Arm64IcParameters)",
        )?;
        // SAFETY: the partition just configured.
        check("WHvSetupPartition", unsafe { WHvSetupPartition(partition.0) })?;
        if FORBID_DYNAMIC_CODE.load(Ordering::Relaxed) {
            forbid_dynamic_code()?;
        }
        Ok(Vm {
            partition: Arc::new(partition),
            mpidrs: config.mpidrs,
            restore_counter: None,
        })
    }

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until the VM is destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.partition.map(host, gpa, len, RWX) }
    }

    /// What takes guest RAM from the guest and gives it back (D151).
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

    /// What maps a device's window into the guest.
    pub fn window(&self) -> Window {
        Window(self.partition.clone())
    }

    /// What maps a device's memory into the guest while the VM lives, `count` slots of it
    /// on KVM; here the partition's mappings, by address.
    pub fn device_slots(&self, _count: u32) -> DeviceSlots {
        DeviceSlots(self.partition.clone())
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
        let flags = if writable { RWX } else { MAP_READ | MAP_EXECUTE };
        // SAFETY: forwarded caller contract.
        unsafe { self.partition.map(host, gpa, len, flags) }
    }

    /// Nothing to complete once every vCPU exists: the GIC was whole as the partition was set
    /// up, and each redistributor as its vCPU was made.
    pub fn vcpus_created(&self) -> Result<()> {
        Ok(())
    }

    /// Prepares a fresh VM to restore, before any vCPU is made: partition time stopped,
    /// and every vCPU's counter set as it is made, before its timer state is restored.
    /// M414 measured that every counter then holds the written value until time resumes.
    /// WHvRunVirtualProcessor resumes it as the first vCPU enters, after all are set up.
    /// Sampling the host's counter independently at each vCPU's release introduced skew
    /// and let CLOCK_MONOTONIC go backward between CPUs (the snapshot storm E2E).
    pub fn for_restore(mut self, counter: u64) -> Result<Self> {
        // SAFETY: the fresh partition has no running vCPU.
        check("WHvSuspendPartitionTime", unsafe {
            WHvSuspendPartitionTime(self.partition.0)
        })?;
        self.restore_counter = Some(counter);
        Ok(self)
    }

    /// The partition's GIC, at the places it was given ([`Vm::new`]): a layout elsewhere, or
    /// one with an ITS, is refused.
    pub fn create_gic(&self, layout: &GicLayout) -> Result<Gic> {
        if layout.dist_base != layout::GIC_DIST || layout.redist_base != layout::GIC_REDIST {
            return Err(Error::Guest(format!(
                "the platform's GIC is at {:#x} and {:#x}, not {:#x} and {:#x}",
                layout::GIC_DIST,
                layout::GIC_REDIST,
                layout.dist_base,
                layout.redist_base
            )));
        }
        if layout.its.is_some() {
            return Err(Error::Guest(
                "the platform's GIC is given no ITS: a PCI bus's MSIs go to the GICv2m frame the VMM keeps"
                    .into(),
            ));
        }
        Ok(Gic(self.partition.clone()))
    }

    /// Creates vCPU `index` on the calling thread, which runs it: its MPIDR, its
    /// redistributor the index-th frame from GIC_REDIST, as the devicetree says, and, past
    /// the first, held until PSCI's CPU_ON starts it.
    pub fn create_vcpu(&self, index: usize) -> Result<Vcpu> {
        let &mpidr = self
            .mpidrs
            .get(index)
            .ok_or_else(|| Error::Guest(format!("no vCPU {index} in this VM")))?;
        let vp = u32::try_from(index).map_err(|_| Error::Guest(format!("vCPU {index}")))?;
        let redist = (index as u64)
            .checked_mul(REDIST_SIZE)
            .and_then(|off| off.checked_add(layout::GIC_REDIST))
            .ok_or_else(|| Error::Guest("redistributor offset overflows".into()))?;
        // SAFETY: the partition, set up.
        check("WHvCreateVirtualProcessor", unsafe {
            WHvCreateVirtualProcessor(self.partition.0, vp, 0)
        })?;
        // MPIDR's bit 31 is RES1 (Arm ARM D19.2.86), as QEMU sets it.
        self.partition
            .set_registers(vp, &[MPIDR_EL1, GICR_BASE_GPA], &[mpidr | (1 << 31), redist])?;
        if index > 0 {
            self.partition
                .set_registers(vp, &[INTERNAL_ACTIVITY_STATE], &[STARTUP_SUSPEND])?;
        }
        if let Some(counter) = self.restore_counter {
            self.partition.set_registers(vp, &[CNTVCT_EL0], &[counter])?;
        }
        Ok(Vcpu {
            partition: self.partition.clone(),
            index: vp,
        })
    }

    /// The GIC's state, for a snapshot, in the platform's own layouts (WinHvPlatformDefs.h):
    /// the distributor's and each SPI's (the partition's global interrupt state), then each
    /// vCPU's redistributor and CPU interface (its interrupt controller state), in vCPU
    /// order. Every vCPU must be out of the guest.
    pub fn save_gic(&self) -> Result<Vec<u8>> {
        let mut w = Writer::default();
        w.bytes(
            &self
                .partition
                .state(ANY_VP, STATE_GLOBAL_INTERRUPT, GLOBAL_STATE_MOST)?,
        );
        let locals = (0u32..)
            .take(self.mpidrs.len())
            .map(|vp| self.partition.state(vp, STATE_INTERRUPT_CONTROLLER, LOCAL_STATE))
            .collect::<Result<Vec<_>>>()?;
        w.seq(&locals, |w, local| w.bytes(local));
        Ok(w.into_bytes())
    }

    /// Restores [`save_gic`](Self::save_gic)'s state into this VM's GIC once every vCPU
    /// exists and before any runs: the distributor's first, then each vCPU's, as QEMU orders
    /// a GICv3's (distributor control before the redistributors and CPU interfaces). Every
    /// part is read before any is set.
    pub fn restore_gic(&self, state: &[u8]) -> Result<()> {
        let said = |e: DecodeError| Error::Guest(format!("the GIC's state: {e}"));
        let mut r = Reader::new(state);
        let global = r.bytes(GLOBAL_STATE_MOST).map_err(said)?;
        let locals = r
            .seq(self.mpidrs.len(), 4, |r| r.bytes(LOCAL_STATE))
            .map_err(said)?;
        r.finish().map_err(said)?;
        if locals.len() != self.mpidrs.len() {
            return Err(Error::Guest(format!(
                "the GIC's state holds {} vCPUs' and this VM has {}",
                locals.len(),
                self.mpidrs.len()
            )));
        }
        self.partition.set_state(ANY_VP, STATE_GLOBAL_INTERRUPT, global)?;
        for (vp, local) in (0u32..).zip(&locals) {
            self.partition.set_state(vp, STATE_INTERRUPT_CONTROLLER, local)?;
        }
        Ok(())
    }
}

/// WHV_ARM64_GLOBAL_INTERRUPT_CONTROLLER_STATE's size at most, as WinHvPlatformDefs.h lays it
/// out: 16 bytes, then 16 for each interrupt, of GICv3's 1,020 INTIDs at most. WHP returns
/// whole pages for the global state: M414's Linux snapshot is 16,384 bytes, not 16,336.
const GLOBAL_STATE_MOST: usize = (16_usize + 1020 * 16).div_ceil(4096) * 4096;

/// WHV_ARM64_LOCAL_INTERRUPT_CONTROLLER_STATE's size: 8 bytes, six registers, ICH_AP1Rn_EL2's
/// four words and the 32 private interrupts' states of 4 bytes each.
const LOCAL_STATE: usize = 8 + 6 * 8 + 4 * 4 + 32 * 4;

/// A vCPU, owned by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    partition: Arc<Partition>,
    index: u32,
}

impl Vcpu {
    /// Populates each `(gpa, len)` ahead of the guest, for reading or writing (making the
    /// private copies of snapshot pages). M414 exercised both kinds on arm64; the advice
    /// is the same WHvAdviseGpaRange operation as on x86_64.
    pub fn pre_fault_runs(&self, runs: &[(u64, u64)], write: bool) -> Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        let ranges: Vec<MemoryRange> = runs
            .iter()
            .map(|&(gpa, size)| MemoryRange { gpa, size })
            .collect();
        let populate = Populate {
            flags: 0,
            access_type: u32::from(write),
        };
        for chunk in ranges.chunks(u32::MAX as usize) {
            // SAFETY: the chunk's mapped ranges and populate advice of its own layout.
            check("WHvAdviseGpaRange(Populate)", unsafe {
                WHvAdviseGpaRange(
                    self.partition.0,
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    0,
                    (&raw const populate).cast(),
                    size_of::<Populate>() as u32,
                )
            })?;
        }
        Ok(())
    }

    pub fn kicker(&self) -> Kicker {
        Kicker {
            partition: self.partition.clone(),
            index: self.index,
        }
    }

    /// Makes this the boot vCPU: it enters at `entry` at EL1h with DAIF masked, its MMU and
    /// caches off, X0 as given and the other registers zero (Linux's booting.rst), running
    /// at once.
    pub fn boot(&mut self, entry: Entry) -> Result<()> {
        let mut names: Vec<u32> = (0..31).map(|n| X0 + n).collect();
        let mut values = vec![0u64; 31];
        if let Some(x0) = values.first_mut() {
            *x0 = entry.x0;
        }
        names.extend([PC, PSTATE, SCTLR_EL1, INTERNAL_ACTIVITY_STATE]);
        values.extend([entry.pc, sysreg::PSTATE_EL1H_DAIF, sysreg::SCTLR_EL1_RESET, 0]);
        self.partition.set_registers(self.index, &names, &values)
    }

    /// Runs the guest until it powers off, resets, or a kick arrives. Returns an error for
    /// guest behavior the VMM cannot emulate.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        loop {
            let mut exit = ExitContext::default();
            // SAFETY: an exit context of its own size, 16-aligned.
            check("WHvRunVirtualProcessor", unsafe {
                WHvRunVirtualProcessor(
                    self.partition.0,
                    self.index,
                    (&raw mut exit).cast(),
                    size_of::<ExitContext>() as u32,
                )
            })?;
            match exit.exit_reason {
                EXIT_UNMAPPED_GPA | EXIT_GPA_INTERCEPT => {
                    let access = exit.memory_access();
                    let syndrome = access.syndrome;
                    match esr::ec(syndrome) {
                        // A cache maintenance instruction on no guest memory: nothing to keep
                        // coherent, as KVM has it (esr::cache_maintenance).
                        esr::EC_DABT_LOW if esr::cache_maintenance(syndrome) => {
                            self.advance(access.header.pc, syndrome)?;
                        }
                        // Memory the host could not give: a share's DAX window's range past
                        // its file's end (D189). Given now, the access runs again.
                        esr::EC_DABT_LOW | esr::EC_IABT_LOW if io.memory_fault(Some(access.gpa)) => {}
                        esr::EC_DABT_LOW => self.mmio(io, &access)?,
                        ec => return Err(Error::Guest(describe(ec, &access))),
                    }
                }
                EXIT_CANCELED => return Ok(Exit::Canceled),
                EXIT_ARM64_RESET => {
                    return match exit.reset().reset_type {
                        RESET_POWER_OFF => Ok(Exit::Shutdown),
                        RESET_REBOOT => Ok(Exit::Reset),
                        t => Err(Error::Guest(format!("a reset exit of type {t}"))),
                    };
                }
                reason @ (EXIT_HYPERCALL
                | EXIT_UNRECOVERABLE_EXCEPTION
                | EXIT_INVALID_VP_REGISTER_VALUE
                | EXIT_UNSUPPORTED_FEATURE) => {
                    return Err(Error::Guest(format!(
                        "WHP exit {reason:#x} at PC {:#x}",
                        exit.header().pc
                    )));
                }
                other => {
                    return Err(Error::Guest(format!(
                        "unexpected WHP exit {other:#x} at PC {:#x}",
                        exit.header().pc
                    )));
                }
            }
        }
    }

    /// PC past the instruction at `pc` the exit stopped at.
    fn advance(&self, pc: u64, syndrome: u64) -> Result<()> {
        self.partition
            .set_registers(self.index, &[PC], &[pc.wrapping_add(esr::instr_len(syndrome))])
    }

    /// Completes a trapped device access as KVM's `kvm_handle_mmio_return` does
    /// (arch/arm64/kvm/mmio.c): Rt = 31 is XZR, loads extend per SSE and SF.
    fn mmio(&self, io: &dyn Io, access: &MemoryAccess) -> Result<()> {
        let Some(da) = esr::data_abort(access.syndrome) else {
            return Err(Error::Guest(describe(esr::EC_DABT_LOW, access)));
        };
        let bad_size = || Error::Guest(format!("MMIO access of {} bytes at {:#x}", da.size, access.gpa));
        let rt = X0 + u32::from(da.reg);
        if da.write {
            let value = if da.reg == 31 {
                0
            } else {
                self.partition.register(self.index, rt)?
            };
            let bytes = value.to_le_bytes();
            io.mmio_write(access.gpa, bytes.get(..da.size).ok_or_else(bad_size)?);
        } else {
            let mut raw = [0u8; 8];
            let data = raw.get_mut(..da.size).ok_or_else(bad_size)?;
            io.mmio_read(access.gpa, data);
            if da.reg != 31 {
                return self.partition.set_registers(
                    self.index,
                    &[rt, PC],
                    &[
                        da.load_value(get_le(data)),
                        access.header.pc.wrapping_add(esr::instr_len(access.syndrome)),
                    ],
                );
            }
        }
        self.advance(access.header.pc, access.syndrome)
    }

    /// The CPU identity the guest sees: MIDR_EL1 from this vCPU, then the partition's
    /// feature ID registers ([`ID_REGS`]), read with [`ANY_VP`]. MIDR is not a global ID
    /// register (M414's snapshot E2E, as QEMU's whpx_arm_get_host_cpu_features notes).
    pub fn cpu_id(&self) -> Result<Vec<(u16, u64)>> {
        let names: Vec<u32> = ID_REGS.iter().map(|&e| id_register(e)).collect();
        let values = self.partition.registers(ANY_VP, &names)?;
        let mut id = vec![(0xc000, self.partition.register(self.index, MIDR_EL1)?)];
        id.extend(ID_REGS.iter().copied().zip(values));
        Ok(id)
    }

    /// Captures the architectural state, with the vCPU out of the guest: its general, SIMD
    /// and floating-point registers, the system registers its guest has
    /// ([`Partition::saved_sysregs`]), its OS lock, and its power, held in StartupSuspend
    /// (powered off, or never started) or not. A vCPU stopped with an event pending, which
    /// none of these holds, takes no snapshot.
    pub fn save_state(&self) -> Result<VcpuState> {
        let (p, vp) = (&self.partition, self.index);
        let sys = p.saved_sysregs()?;
        // EventPending, WHV_ARM64_PENDING_EVENT's first bit.
        if p.values(vp, &[PENDING_EVENT0])?
            .first()
            .is_some_and(|v| v.0[0] & 1 != 0)
        {
            return Err(Error::Guest(format!(
                "vCPU {vp} stopped with an event pending, which a snapshot does not keep"
            )));
        }
        let mut names: Vec<u32> = (0..31).map(|n| X0 + n).collect();
        names.extend([PC, PSTATE, FPCR, FPSR, INTERNAL_ACTIVITY_STATE, OSLSR_EL1]);
        names.extend(sys.iter().map(|&(_, name)| name));
        let values = p.registers(vp, &names)?;
        let simd = p.values(vp, &(0..32).map(|n| Q0 + n).collect::<Vec<_>>())?;
        let short = || {
            Error::Guest(format!(
                "vCPU {vp}: {} register values for {} names",
                values.len(),
                names.len()
            ))
        };
        let x: [u64; 31] = values
            .get(..31)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(short)?;
        let fixed: [u64; 6] = values
            .get(31..37)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(short)?;
        let [pc, pstate, fpcr, fpsr, activity, oslsr] = fixed;
        let rest = values
            .get(37..)
            .filter(|r| r.len() == sys.len())
            .ok_or_else(short)?;
        let mut v = [0u128; 32];
        if simd.len() != v.len() {
            return Err(short());
        }
        for (q, value) in v.iter_mut().zip(&simd) {
            *q = u128::from(value.0[0]) | u128::from(value.0[1]) << 64;
        }
        let mut saved: Vec<(u16, u64)> = sys.iter().map(|&(e, _)| e).zip(rest.iter().copied()).collect();
        // OSLSR_EL1's OSLK (bit 1), kept under OSLAR_EL1, whose write sets it back.
        saved.push((OSLAR, (oslsr >> 1) & 1));
        Ok(VcpuState {
            x,
            pc,
            pstate,
            v,
            fpcr,
            fpsr,
            sys: saved,
            // The CPU interface is the GIC's state here ([`Vm::save_gic`]).
            icc: Vec::new(),
            power: if activity & STARTUP_SUSPEND != 0 {
                Power::Off
            } else {
                Power::On
            },
        })
    }

    /// Loads captured state into this freshly created vCPU, its power as it was: a vCPU
    /// that ran resumes where it stopped, one powered off waits for its CPU_ON. The guest's
    /// counter was set as the vCPU was made, with partition time stopped ([`Vm::for_restore`]).
    pub fn restore_state(&mut self, st: &VcpuState) -> Result<()> {
        let (p, vp) = (&self.partition, self.index);
        let activity = match st.power {
            Power::Off => STARTUP_SUSPEND,
            Power::On => 0,
            Power::Pending(_) => {
                return Err(Error::Guest(
                    "a CPU_ON pending in the VMM, which arm64's WHP never has: PSCI is its hypervisor's"
                        .into(),
                ));
            }
        };
        let mut names: Vec<u32> = (0..31).map(|n| X0 + n).collect();
        let mut values = st.x.to_vec();
        names.extend([PC, PSTATE, FPCR, FPSR]);
        values.extend([st.pc, st.pstate, st.fpcr, st.fpsr]);
        for &(encoding, value) in &st.sys {
            let name = name_of(encoding)
                .ok_or_else(|| Error::Guest(format!("a system register of encoding {encoding:#06x}")))?;
            names.push(name);
            values.push(value);
        }
        p.set_registers(vp, &names, &values)?;
        let simd: Vec<Value> =
            st.v.iter()
                .map(|&q| Value([q as u64, (q >> 64) as u64]))
                .collect();
        p.set_values(vp, &(0..32).map(|n| Q0 + n).collect::<Vec<_>>(), &simd)?;
        p.set_registers(vp, &[INTERNAL_ACTIVITY_STATE], &[activity])
    }

    /// The guest's virtual counter now (CNTVCT_EL0).
    pub fn guest_counter(&self) -> Result<u64> {
        self.partition.register(self.index, CNTVCT_EL0)
    }
}

// ---- snapshots ---------------------------------------------------------------------

/// The EL1 and EL0 system registers a snapshot carries, by encoding (op0:op1:CRn:CRm:op2, as
/// arch/aarch64/state.rs keys them) and WHP's name: those QEMU's WHPX accelerator syncs
/// (target/arm/whpx/whpx-all.c), SP_EL0, SP_EL1, ELR_EL1 and SPSR_EL1 among them, as HVF's
/// backend carries them. Not the IMPLEMENTATION DEFINED ones: AFSR0, AFSR1 and AMAIR, which
/// WinHvPlatformDefs.h does not name, and ACTLR_EL1, which Linux never writes; nor the EL1
/// physical timer, which a guest at EL1 does not use, taking the virtual one
/// (docs/research/hvf-arm64-kvm-ground-truth.md, the timer's PPI); nor a PMU's, as no PMU
/// is in the guest's devicetree.
const SYSREGS: &[(u16, u32)] = &[
    (0xc080, SCTLR_EL1),
    (0xc082, CPACR_EL1),
    (0xc100, TTBR0_EL1),
    (0xc101, TTBR1_EL1),
    (0xc102, TCR_EL1),
    (0xc200, SPSR_EL1),
    (0xc201, ELR_EL1),
    (0xc208, SP_EL0),
    (0xe208, SP_EL1),
    (0xc290, ESR_EL1),
    (0xc300, FAR_EL1),
    (0xc3a0, PAR_EL1),
    (0xc510, MAIR_EL1),
    (0xc600, VBAR_EL1),
    (0xc681, CONTEXTIDR_EL1),
    (0xc684, TPIDR_EL1),
    (0xc708, CNTKCTL_EL1),
    (0xd000, CSSELR_EL1),
    (0xde82, TPIDR_EL0),
    (0xde83, TPIDRRO_EL0),
    (0xdf19, CNTV_CTL_EL0),
    (0xdf1a, CNTV_CVAL_EL0),
    (0x8012, MDSCR_EL1),
];

/// The pointer authentication keys, which a guest has with FEAT_PAuth alone.
const PAUTH_KEYS: &[(u16, u32)] = &[
    (0xc108, APIA_KEY_LO),
    (0xc109, APIA_KEY_HI),
    (0xc10a, APIB_KEY_LO),
    (0xc10b, APIB_KEY_HI),
    (0xc110, APDA_KEY_LO),
    (0xc111, APDA_KEY_HI),
    (0xc112, APDB_KEY_LO),
    (0xc113, APDB_KEY_HI),
    (0xc118, APGA_KEY_LO),
    (0xc119, APGA_KEY_HI),
];

/// OSLAR_EL1's encoding, under which a snapshot keeps OSLSR_EL1's OSLK: the OS lock, which
/// a write of OSLAR_EL1 sets (Arm ARM D19.3.29).
const OSLAR: u16 = 0x8084;

/// The ID registers that define the guest's CPU, by encoding: a restore on a CPU that
/// reports any of them different is refused. The global AArch64 feature registers it
/// patches its own code by as it boots (processor, debug, instruction set and memory
/// model). MIDR is read from the vCPU separately ([`Vcpu::cpu_id`]).
const ID_REGS: &[u16] = &[
    0xc020, 0xc021, 0xc022, 0xc024, 0xc025, 0xc028, 0xc029, 0xc030, 0xc031, 0xc032, 0xc038, 0xc039, 0xc03a,
    0xc03b, 0xc03c,
];
const ID_AA64PFR0_EL1: u16 = 0xc020;
const ID_AA64PFR1_EL1: u16 = 0xc021;
const ID_AA64DFR0_EL1: u16 = 0xc028;
const ID_AA64ISAR1_EL1: u16 = 0xc031;
const ID_AA64ISAR2_EL1: u16 = 0xc032;

/// WHP's name for ID register `encoding`: its CRm:op2 from [`ID_REGISTERS`].
fn id_register(encoding: u16) -> u32 {
    ID_REGISTERS + u32::from(encoding & 0x7f)
}

/// The breakpoint and watchpoint registers ID_AA64DFR0_EL1 says exist, by encoding and
/// WHP's name.
fn debug_regs(dfr0: u64) -> Vec<(u16, u32)> {
    let brps = ((dfr0 >> 12) & 0xf) + 1;
    let wrps = ((dfr0 >> 20) & 0xf) + 1;
    let mut regs = Vec::new();
    for n in 0..16u16 {
        let base = 0x8004 + (n << 3); // DBGBVRn_EL1; BCR +1, WVR +2, WCR +3
        let i = u32::from(n);
        if u64::from(n) < brps {
            regs.extend([(base, DBGBVR0_EL1 + i), (base + 1, DBGBCR0_EL1 + i)]);
        }
        if u64::from(n) < wrps {
            regs.extend([(base + 2, DBGWVR0_EL1 + i), (base + 3, DBGWCR0_EL1 + i)]);
        }
    }
    regs
}

/// Whether ID_AA64ISAR1_EL1 and ID_AA64ISAR2_EL1 give the guest pointer authentication: any
/// of APA, API, GPA, GPI, APA3 or GPA3.
fn pauth(isar1: u64, isar2: u64) -> bool {
    (isar1 >> 4) & 0xff != 0 || (isar1 >> 24) & 0xff != 0 || (isar2 >> 8) & 0xff != 0
}

/// WHP's name for a system register a snapshot may carry, by its encoding.
fn name_of(encoding: u16) -> Option<u32> {
    if encoding == OSLAR {
        return Some(OSLAR_EL1);
    }
    // Every breakpoint and watchpoint register, 16 of each.
    let every_debug = (0xf << 12) | (0xf << 20);
    SYSREGS
        .iter()
        .chain(PAUTH_KEYS)
        .copied()
        .chain(debug_regs(every_debug))
        .find_map(|(e, name)| (e == encoding).then_some(name))
}

impl Partition {
    /// The system registers a snapshot of this partition's vCPUs carries: [`SYSREGS`], and
    /// those its guest's ID registers say it has (the pointer authentication keys, the
    /// breakpoint and watchpoint registers). A guest given SVE or SME, whose vector state no
    /// register here holds, is refused.
    fn saved_sysregs(&self) -> Result<Vec<(u16, u32)>> {
        let ids = [
            ID_AA64PFR0_EL1,
            ID_AA64PFR1_EL1,
            ID_AA64DFR0_EL1,
            ID_AA64ISAR1_EL1,
            ID_AA64ISAR2_EL1,
        ];
        let names: Vec<u32> = ids.iter().map(|&e| id_register(e)).collect();
        let values = self.registers(ANY_VP, &names)?;
        let [pfr0, pfr1, dfr0, isar1, isar2]: [u64; 5] = values
            .as_slice()
            .try_into()
            .map_err(|_| Error::Guest(format!("{} ID register values for {}", values.len(), ids.len())))?;
        // ID_AA64PFR0_EL1.SVE (35:32), ID_AA64PFR1_EL1.SME (27:24).
        if (pfr0 >> 32) & 0xf != 0 || (pfr1 >> 24) & 0xf != 0 {
            return Err(Error::Guest(
                "a guest given SVE or SME takes no snapshot on arm64's WHP: no register of its vector state is kept"
                    .into(),
            ));
        }
        let mut sys = SYSREGS.to_vec();
        if pauth(isar1, isar2) {
            sys.extend_from_slice(PAUTH_KEYS);
        }
        sys.extend(debug_regs(dfr0));
        Ok(sys)
    }
}

/// An access the VMM does not answer, as a log says it.
fn describe(ec: u32, access: &MemoryAccess) -> String {
    format!(
        "unhandled guest exception: EC {ec:#04x}, ESR {:#x}, GPA {:#x}, PC {:#x}",
        access.syndrome, access.gpa, access.header.pc
    )
}

/// Interrupts a vCPU's `run` from any thread: one out of the guest returns at its next
/// entry (WHvCancelRunVirtualProcessor).
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

/// The host's virtual counter (CNTVCT_EL0), which Windows lets user mode read.
pub fn host_counter() -> u64 {
    let counter: u64;
    // SAFETY: a read of the counter register, which has no other effect.
    unsafe {
        std::arch::asm!("mrs {}, cntvct_el0", out(reg) counter, options(nomem, nostack, preserves_flags))
    };
    counter
}
