//! Safe wrappers over Hypervisor.framework (arm64).
//!
//! HVF allows one VM per process and binds each vCPU to the thread that created it
//! (docs/research/hvf-arm64-kvm-ground-truth.md §1.1). [`Vcpu`] is therefore `!Send`;
//! the only cross-thread vCPU operation, forcing an exit, goes through [`VcpuKicker`].

mod ffi;

use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

pub use ffi::sys_reg;

/// A failed Hypervisor.framework call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub op: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.code as u32 {
            0xfae9_4001 => "HV_ERROR",
            0xfae9_4002 => "HV_BUSY",
            0xfae9_4003 => "HV_BAD_ARGUMENT",
            0xfae9_4004 => "HV_ILLEGAL_GUEST_STATE",
            0xfae9_4005 => "HV_NO_RESOURCES",
            0xfae9_4006 => "HV_NO_DEVICE",
            0xfae9_4007 => "HV_DENIED (is the binary signed with com.apple.security.hypervisor?)",
            0xfae9_4008 => "HV_EXISTS",
            0xfae9_400f => "HV_UNSUPPORTED",
            _ => "unknown",
        };
        write!(f, "{} failed: {name} ({:#x})", self.op, self.code as u32)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(op: &'static str, code: ffi::hv_return_t) -> Result<()> {
    if code == ffi::HV_SUCCESS { Ok(()) } else { Err(Error { op, code }) }
}

/// Stage-2 permissions for a guest-physical mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perms(u64);

impl Perms {
    pub const R: Perms = Perms(ffi::HV_MEMORY_READ);
    pub const RX: Perms = Perms(ffi::HV_MEMORY_READ | ffi::HV_MEMORY_EXEC);
    pub const RWX: Perms = Perms(ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granule {
    K4,
    K16,
}

#[derive(Debug, Clone, Copy)]
pub struct VmConfig {
    /// Guest physical address width; `None` keeps the framework default (36 bits).
    pub ipa_bits: Option<u32>,
    pub granule: Granule,
}

static VM_EXISTS: AtomicBool = AtomicBool::new(false);

/// The process's virtual machine. Destroying it requires every [`Vcpu`] to be gone.
#[derive(Debug)]
pub struct Vm(());

impl Vm {
    pub fn new(config: VmConfig) -> Result<Vm> {
        if VM_EXISTS.swap(true, Ordering::AcqRel) {
            return Err(Error { op: "hv_vm_create (second VM in process)", code: 0xfae9_4008u32 as i32 });
        }
        // SAFETY: config objects are created, configured and released on this thread.
        let res = unsafe {
            let cfg = ffi::hv_vm_config_create();
            let granule = match config.granule {
                Granule::K4 => ffi::HV_IPA_GRANULE_4KB,
                Granule::K16 => ffi::HV_IPA_GRANULE_16KB,
            };
            let mut res = check("hv_vm_config_set_ipa_granule", ffi::hv_vm_config_set_ipa_granule(cfg, granule));
            if let (Ok(()), Some(bits)) = (res, config.ipa_bits) {
                res = check("hv_vm_config_set_ipa_size", ffi::hv_vm_config_set_ipa_size(cfg, bits));
            }
            if res.is_ok() {
                res = check("hv_vm_create", ffi::hv_vm_create(cfg));
            }
            ffi::os_release(cfg);
            res
        };
        match res {
            Ok(()) => Ok(Vm(())),
            Err(e) => {
                VM_EXISTS.store(false, Ordering::Release);
                Err(e)
            }
        }
    }

    /// Maps `size` bytes of host memory at `host` to guest-physical `gpa`.
    ///
    /// # Safety
    /// `host..host+size` must be a single host VM region (e.g. from `mmap`) that stays
    /// mapped until it is unmapped here or the VM is destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, size: usize, perms: Perms) -> Result<()> {
        // SAFETY: forwarded caller contract.
        check("hv_vm_map", unsafe { ffi::hv_vm_map(host as *mut c_void, gpa, size, perms.0) })
    }

    pub fn unmap(&self, gpa: u64, size: usize) -> Result<()> {
        // SAFETY: unmapping only affects the guest's view of memory.
        check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(gpa, size) })
    }

    pub fn protect(&self, gpa: u64, size: usize, perms: Perms) -> Result<()> {
        // SAFETY: changes stage-2 permissions of an existing mapping only.
        check("hv_vm_protect", unsafe { ffi::hv_vm_protect(gpa, size, perms.0) })
    }

    /// Creates the in-kernel GICv3. Must happen before any vCPU is created.
    pub fn create_gic(&self, layout: &GicLayout) -> Result<Gic> {
        // SAFETY: the config object is created, used and released on this thread.
        unsafe {
            let cfg = ffi::hv_gic_config_create();
            let mut res = check(
                "hv_gic_config_set_distributor_base",
                ffi::hv_gic_config_set_distributor_base(cfg, layout.dist_base),
            );
            if res.is_ok() {
                res = check(
                    "hv_gic_config_set_redistributor_base",
                    ffi::hv_gic_config_set_redistributor_base(cfg, layout.redist_base),
                );
            }
            if let (Ok(()), Some(msi)) = (res, layout.msi) {
                res = check("hv_gic_config_set_msi_region_base", ffi::hv_gic_config_set_msi_region_base(cfg, msi.base));
                if res.is_ok() {
                    res = check(
                        "hv_gic_config_set_msi_interrupt_range",
                        ffi::hv_gic_config_set_msi_interrupt_range(cfg, msi.first_intid, msi.count),
                    );
                }
            }
            if res.is_ok() {
                res = check("hv_gic_create", ffi::hv_gic_create(cfg));
            }
            ffi::os_release(cfg);
            res.map(|()| Gic(()))
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: all vCPUs borrow nothing from `Vm`, but HVF requires they be destroyed
        // first; the VMM joins every vCPU thread before dropping the VM.
        let code = unsafe { ffi::hv_vm_destroy() };
        debug_assert_eq!(code, ffi::HV_SUCCESS, "hv_vm_destroy");
        VM_EXISTS.store(false, Ordering::Release);
    }
}

/// Maximum number of vCPUs a VM can have on this host.
pub fn max_vcpus() -> Result<u32> {
    let mut n = 0;
    // SAFETY: writes one u32.
    check("hv_vm_get_max_vcpu_count", unsafe { ffi::hv_vm_get_max_vcpu_count(&mut n) })?;
    Ok(n)
}

/// Largest supported guest physical address width.
pub fn max_ipa_bits() -> Result<u32> {
    let mut n = 0;
    // SAFETY: writes one u32.
    check("hv_vm_config_get_max_ipa_size", unsafe { ffi::hv_vm_config_get_max_ipa_size(&mut n) })?;
    Ok(n)
}

/// Host-imposed GIC geometry (runtime queries; they are not published constants).
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
    let (mut ds, mut da, mut rs, mut ra, mut ms, mut ma) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut sb, mut sc) = (0u32, 0u32);
    // SAFETY: each call writes one scalar.
    unsafe {
        check("hv_gic_get_distributor_size", ffi::hv_gic_get_distributor_size(&mut ds))?;
        check("hv_gic_get_distributor_base_alignment", ffi::hv_gic_get_distributor_base_alignment(&mut da))?;
        check("hv_gic_get_redistributor_size", ffi::hv_gic_get_redistributor_size(&mut rs))?;
        check("hv_gic_get_redistributor_base_alignment", ffi::hv_gic_get_redistributor_base_alignment(&mut ra))?;
        check("hv_gic_get_msi_region_size", ffi::hv_gic_get_msi_region_size(&mut ms))?;
        check("hv_gic_get_msi_region_base_alignment", ffi::hv_gic_get_msi_region_base_alignment(&mut ma))?;
        check("hv_gic_get_spi_interrupt_range", ffi::hv_gic_get_spi_interrupt_range(&mut sb, &mut sc))?;
    }
    Ok(GicParams {
        dist_size: ds as u64,
        dist_align: da as u64,
        redist_size: rs as u64,
        redist_align: ra as u64,
        msi_size: ms as u64,
        msi_align: ma as u64,
        spi_base: sb,
        spi_count: sc,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct MsiFrame {
    pub base: u64,
    pub first_intid: u32,
    pub count: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct GicLayout {
    pub dist_base: u64,
    pub redist_base: u64,
    pub msi: Option<MsiFrame>,
}

/// Handle to the in-kernel GIC. Interrupt injection is legal from any thread
/// (no owning-thread rule on `hv_gic_set_spi`/`hv_gic_send_msi`).
#[derive(Debug, Clone, Copy)]
pub struct Gic(());

impl Gic {
    /// Drives an SPI line. For edge-configured INTIDs `true` produces one edge.
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<()> {
        // SAFETY: no memory is passed; HVF validates the INTID.
        check("hv_gic_set_spi", unsafe { ffi::hv_gic_set_spi(intid, level) })
    }

    /// Delivers an MSI written to `doorbell` (the frame's GICM_SET_SPI_NSR address).
    pub fn send_msi(&self, doorbell: u64, intid: u32) -> Result<()> {
        // SAFETY: no memory is passed; HVF validates address and INTID.
        check("hv_gic_send_msi", unsafe { ffi::hv_gic_send_msi(doorbell, intid) })
    }
}

/// General-purpose and special registers (`hv_reg_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(u32);

impl Reg {
    pub const PC: Reg = Reg(ffi::HV_REG_PC);
    pub const FPCR: Reg = Reg(ffi::HV_REG_FPCR);
    pub const FPSR: Reg = Reg(ffi::HV_REG_FPSR);
    pub const CPSR: Reg = Reg(ffi::HV_REG_CPSR);

    /// `Xn` for n in 0..=30. (Register number 31 in an instruction encoding is XZR/SP,
    /// never PC; callers must handle it before asking for a register.)
    pub const fn x(n: u8) -> Reg {
        assert!(n <= 30);
        Reg(ffi::HV_REG_X0 + n as u32)
    }
}

/// Why `hv_vcpu_run` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Forced by [`VcpuKicker::kick`].
    Canceled,
    /// Synchronous exception to EL2; `syndrome` is ESR_EL2, `ipa` the faulting IPA.
    Exception { syndrome: u64, va: u64, ipa: u64 },
    /// Only without an in-kernel GIC.
    VtimerActivated,
    Unknown(u32),
}

/// A vCPU bound to the current thread.
#[derive(Debug)]
pub struct Vcpu {
    id: ffi::hv_vcpu_t,
    exit: *const ffi::hv_vcpu_exit_t,
    _thread_bound: PhantomData<*const ()>,
}

impl Vcpu {
    /// Creates a vCPU owned by the calling thread. Redistributor frames are assigned
    /// in creation order, so callers create vCPUs strictly by index.
    pub fn new() -> Result<Vcpu> {
        let mut id = 0;
        let mut exit = std::ptr::null();
        // SAFETY: out-parameters are valid; NULL config selects defaults.
        check("hv_vcpu_create", unsafe { ffi::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) })?;
        Ok(Vcpu { id, exit, _thread_bound: PhantomData })
    }

    pub fn kicker(&self) -> VcpuKicker {
        VcpuKicker(self.id)
    }

    pub fn run(&mut self) -> Result<Exit> {
        // SAFETY: owning thread (enforced by !Send); `exit` is valid for the vCPU's life
        // and only written by hv_vcpu_run on this thread.
        unsafe {
            check("hv_vcpu_run", ffi::hv_vcpu_run(self.id))?;
            let e = &*self.exit;
            Ok(match e.reason {
                ffi::HV_EXIT_REASON_CANCELED => Exit::Canceled,
                ffi::HV_EXIT_REASON_EXCEPTION => Exit::Exception {
                    syndrome: e.exception.syndrome,
                    va: e.exception.virtual_address,
                    ipa: e.exception.physical_address,
                },
                ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => Exit::VtimerActivated,
                other => Exit::Unknown(other),
            })
        }
    }

    pub fn reg(&self, reg: Reg) -> Result<u64> {
        let mut v = 0;
        // SAFETY: owning thread; writes one u64.
        check("hv_vcpu_get_reg", unsafe { ffi::hv_vcpu_get_reg(self.id, reg.0, &mut v) })?;
        Ok(v)
    }

    pub fn set_reg(&mut self, reg: Reg, value: u64) -> Result<()> {
        // SAFETY: owning thread.
        check("hv_vcpu_set_reg", unsafe { ffi::hv_vcpu_set_reg(self.id, reg.0, value) })
    }

    pub fn sys_reg(&self, reg: u16) -> Result<u64> {
        let mut v = 0;
        // SAFETY: owning thread; writes one u64.
        check("hv_vcpu_get_sys_reg", unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    pub fn set_sys_reg(&mut self, reg: u16, value: u64) -> Result<()> {
        // SAFETY: owning thread.
        check("hv_vcpu_set_sys_reg", unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg, value) })
    }

    /// `CNTVCT_EL0 = mach_absolute_time() - offset` for this vCPU.
    pub fn vtimer_offset(&self) -> Result<u64> {
        let mut v = 0;
        // SAFETY: owning thread; writes one u64.
        check("hv_vcpu_get_vtimer_offset", unsafe { ffi::hv_vcpu_get_vtimer_offset(self.id, &mut v) })?;
        Ok(v)
    }

    pub fn set_vtimer_offset(&mut self, offset: u64) -> Result<()> {
        // SAFETY: owning thread.
        check("hv_vcpu_set_vtimer_offset", unsafe { ffi::hv_vcpu_set_vtimer_offset(self.id, offset) })
    }

    /// Guest-physical base of this vCPU's GIC redistributor (MPIDR must be set first).
    pub fn redistributor_base(&self) -> Result<u64> {
        let mut v = 0;
        // SAFETY: writes one u64.
        check("hv_gic_get_redistributor_base", unsafe { ffi::hv_gic_get_redistributor_base(self.id, &mut v) })?;
        Ok(v)
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: owning thread (the value never left it).
        let code = unsafe { ffi::hv_vcpu_destroy(self.id) };
        debug_assert_eq!(code, ffi::HV_SUCCESS, "hv_vcpu_destroy");
    }
}

/// Forces a vCPU out of `hv_vcpu_run` from any thread. If the vCPU is not running, its
/// next `run` returns [`Exit::Canceled`] immediately.
#[derive(Debug, Clone, Copy)]
pub struct VcpuKicker(ffi::hv_vcpu_t);

impl VcpuKicker {
    pub fn kick(&self) -> Result<()> {
        // SAFETY: hv_vcpus_exit is the documented cross-thread call; reads one id.
        check("hv_vcpus_exit", unsafe { ffi::hv_vcpus_exit(&self.0, 1) })
    }
}
