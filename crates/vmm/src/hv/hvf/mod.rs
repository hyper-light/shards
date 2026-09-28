//! The Hypervisor.framework backend (macOS on arm64), presenting KVM's semantics
//! (docs/research/hvf-arm64-kvm-ground-truth.md §5):
//!
//! - MMIO completes inside [`Vcpu::run`]. The backend decodes the syndrome, performs the
//!   access through [`Io`], writes Rt and advances PC, as `kvm_handle_mmio_return` does (row 6).
//! - PSCI runs here, CPU_ON included. A powered-off vCPU parks inside `run` (row 4).
//! - Trapped system registers read as zero and ignore writes.

mod ffi;
mod power;
mod sys;

use std::fmt;
use std::sync::{Arc, OnceLock};

pub use sys::{Gic, GicLayout, GicParams, MsiFrame};

use super::{Exit, Io};
use crate::arch::aarch64::{self, Entry, esr, psci, sysreg};
use crate::debug;
use crate::devices::get_le;
use power::Wake;

/// A failed Hypervisor.framework call, or guest behavior the VMM does not emulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Call(sys::Error),
    Guest(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Call(e) => e.fmt(f),
            Error::Guest(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<sys::Error> for Error {
    fn from(e: sys::Error) -> Error {
        Error::Call(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Ok when this host can run VMs: `kern.hv_support` is 1, Apple's documented check.
pub fn check_host() -> std::result::Result<(), String> {
    if sys::supported() {
        Ok(())
    } else {
        Err(
            "Hypervisor.framework is unavailable on this host (sysctl kern.hv_support is not 1), \
             as inside a VM without nested virtualization"
                .into(),
        )
    }
}

pub fn max_vcpus() -> Result<u32> {
    Ok(sys::max_vcpus()?)
}

pub fn max_ipa_bits() -> Result<u32> {
    Ok(sys::max_ipa_bits()?)
}

/// Host-imposed GIC geometry.
pub fn gic_params() -> Result<GicParams> {
    Ok(sys::gic_params()?)
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Guest physical address width.
    pub ipa_bits: u32,
    /// MPIDR of each vCPU, by index.
    pub mpidrs: Vec<u64>,
}

/// The process's VM. It must outlive every [`Vcpu`], and guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    sys: sys::Vm,
    power: Arc<power::Table>,
    mpidrs: Vec<u64>,
    ipa_bits: u32,
    /// Base and per-vCPU size of the redistributor frames, once the GIC exists.
    redist: OnceLock<(u64, u64)>,
}

impl Vm {
    pub fn new(config: VmConfig) -> Result<Vm> {
        let sys = sys::Vm::new(sys::VmConfig {
            ipa_bits: (config.ipa_bits > 36).then_some(config.ipa_bits),
        })?;
        Ok(Vm {
            sys,
            power: Arc::new(power::Table::new(&config.mpidrs)),
            mpidrs: config.mpidrs,
            ipa_bits: config.ipa_bits,
            redist: OnceLock::new(),
        })
    }

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must be one host allocation that stays mapped until the VM is
    /// destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.sys.map(host, gpa, len, sys::Perms::RWX) }?;
        self.power.add_ram(gpa, gpa.saturating_add(len as u64));
        Ok(())
    }

    /// Creates the in-kernel GICv3. HVF requires it before any vCPU exists.
    pub fn create_gic(&self, layout: &GicLayout) -> Result<Gic> {
        let redist_size = sys::gic_params()?.redist_size;
        let gic = self.sys.create_gic(layout)?;
        let _ = self.redist.set((layout.redist_base, redist_size));
        Ok(gic)
    }

    /// Creates vCPU `index` on the calling thread, which owns it from then on. vCPUs must
    /// be created in index order: redistributor frames follow creation order (PM M13).
    pub fn create_vcpu(&self, index: usize) -> Result<Vcpu> {
        let guest = |msg: String| Error::Guest(msg);
        let &mpidr = self
            .mpidrs
            .get(index)
            .ok_or_else(|| guest(format!("no vCPU {index} in this VM")))?;
        let &(redist_base, redist_size) = self
            .redist
            .get()
            .ok_or_else(|| guest("vCPUs need the GIC first".into()))?;
        let mut vcpu = sys::Vcpu::new()?;
        vcpu.set_sys_reg(sysreg::MPIDR_EL1, mpidr)?;

        // The redistributor must be the index-th frame, or GIC routing and the DT disagree.
        let expected = (index as u64)
            .checked_mul(redist_size)
            .and_then(|off| off.checked_add(redist_base))
            .ok_or_else(|| guest("redistributor offset overflows".into()))?;
        let actual = vcpu.redistributor_base()?;
        if actual != expected {
            return Err(guest(format!(
                "vCPU {index}: redistributor at {actual:#x}, expected {expected:#x} (creation order)"
            )));
        }

        // Advertise the physical address width stage 2 actually covers (D10, PM M12).
        let parange = aarch64::parange_for_bits(self.ipa_bits)
            .ok_or_else(|| guest(format!("no PARange encoding for {} IPA bits", self.ipa_bits)))?;
        let mmfr0 = vcpu.sys_reg(sysreg::ID_AA64MMFR0_EL1)?;
        let want = (mmfr0 & !0xf) | parange;
        if want != mmfr0 {
            vcpu.set_sys_reg(sysreg::ID_AA64MMFR0_EL1, want)?;
            let got = vcpu.sys_reg(sysreg::ID_AA64MMFR0_EL1)?;
            if got != want && index == 0 {
                crate::warn!(
                    "ID_AA64MMFR0_EL1.PARange stays {:#x}; the guest may probe beyond {} IPA bits",
                    got & 0xf,
                    self.ipa_bits
                );
            }
        }
        Ok(Vcpu {
            kicker: Kicker {
                sys: vcpu.kicker(),
                power: self.power.clone(),
                index,
            },
            sys: vcpu,
            power: self.power.clone(),
            index,
            on: false,
        })
    }
}

/// A vCPU, owned by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    sys: sys::Vcpu,
    power: Arc<power::Table>,
    kicker: Kicker,
    index: usize,
    /// Powered on. Only a powered-off vCPU consults the shared power table, so exits
    /// in the running guest take no lock.
    on: bool,
}

impl Vcpu {
    pub fn kicker(&self) -> Kicker {
        self.kicker.clone()
    }

    /// Makes this the boot vCPU: it enters at `entry` on its first `run`. Others stay
    /// off until PSCI CPU_ON.
    pub fn boot(&mut self, entry: Entry) {
        self.power.boot(self.index, entry);
    }

    /// Runs the guest until it powers off, resets, or a kick arrives. Returns an error for
    /// guest behavior the VMM cannot emulate.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        loop {
            if !self.on {
                match self.power.park(self.index) {
                    Wake::Kicked => return Ok(Exit::Canceled),
                    Wake::Start(entry) => self.enter(entry)?,
                    Wake::Running => {}
                }
                self.on = true;
            }
            let (syndrome, ipa) = match self.sys.run()? {
                sys::Exit::Canceled => {
                    self.power.kick_delivered(self.index);
                    return Ok(Exit::Canceled);
                }
                sys::Exit::Exception { syndrome, ipa, .. } => (syndrome, ipa),
                sys::Exit::VtimerActivated => {
                    debug!("vCPU {}: VTIMER_ACTIVATED despite the in-kernel GIC", self.index);
                    continue;
                }
                sys::Exit::Unknown(reason) => {
                    return Err(Error::Guest(format!("unknown exit reason {reason}")));
                }
            };
            match esr::ec(syndrome) {
                esr::EC_DABT_LOW => self.mmio(io, syndrome, ipa)?,
                esr::EC_HVC64 | esr::EC_SMC64 => {
                    // HVC exits with PC already past the instruction; a trapped SMC does not
                    // (Arm ARM D1.4.1.5).
                    if esr::ec(syndrome) == esr::EC_SMC64 {
                        self.advance_pc(syndrome)?;
                    }
                    if let Some(exit) = self.firmware_call()? {
                        return Ok(exit);
                    }
                }
                esr::EC_SYS64 => self.sysreg_trap(syndrome)?,
                esr::EC_WFX => self.advance_pc(syndrome)?,
                ec => return Err(Error::Guest(self.describe_fault(ec, syndrome, ipa))),
            }
        }
    }

    /// Architectural entry state for boot and CPU_ON: EL1h with DAIF masked, MMU and
    /// caches off, X0 as given, other GPRs zero (Linux booting.rst; PSCI 1.1 §6.4).
    fn enter(&mut self, entry: Entry) -> Result<()> {
        self.sys.set_reg(sys::Reg::CPSR, sysreg::PSTATE_EL1H_DAIF)?;
        self.sys.set_sys_reg(sysreg::SCTLR_EL1, sysreg::SCTLR_EL1_RESET)?;
        self.sys.set_x(0, entry.x0)?;
        for n in 1..=30 {
            self.sys.set_x(n, 0)?;
        }
        Ok(self.sys.set_reg(sys::Reg::PC, entry.pc)?)
    }

    fn advance_pc(&mut self, syndrome: u64) -> Result<()> {
        let pc = self.sys.reg(sys::Reg::PC)?;
        Ok(self
            .sys
            .set_reg(sys::Reg::PC, pc.wrapping_add(esr::instr_len(syndrome)))?)
    }

    /// Completes a trapped device access as KVM's `kvm_handle_mmio_return` does
    /// (arch/arm64/kvm/mmio.c): Rt = 31 is XZR, loads extend per SSE and SF.
    fn mmio(&mut self, io: &dyn Io, syndrome: u64, ipa: u64) -> Result<()> {
        let Some(da) = esr::data_abort(syndrome) else {
            return Err(Error::Guest(self.describe_fault(esr::EC_DABT_LOW, syndrome, ipa)));
        };
        let bad_size = || Error::Guest(format!("MMIO access of {} bytes at {ipa:#x}", da.size));
        if da.write {
            let value = self.sys.x(da.reg)?;
            let bytes = value.to_le_bytes();
            io.mmio_write(ipa, bytes.get(..da.size).ok_or_else(bad_size)?);
        } else {
            let mut raw = [0u8; 8];
            let data = raw.get_mut(..da.size).ok_or_else(bad_size)?;
            io.mmio_read(ipa, data);
            self.sys.set_x(da.reg, da.load_value(get_le(data)))?;
        }
        self.advance_pc(syndrome)
    }

    /// PSCI and SMCCC. Returns the exit when the guest powers off or resets.
    fn firmware_call(&mut self) -> Result<Option<Exit>> {
        let x = [self.sys.x(0)?, self.sys.x(1)?, self.sys.x(2)?, self.sys.x(3)?];
        let ret = match psci::decode(x) {
            psci::Call::Immediate(v) => v,
            psci::Call::CpuSuspend => psci::SUCCESS,
            // CPU_OFF does not return: the vCPU parks until the next CPU_ON.
            psci::Call::CpuOff => {
                self.power.off(self.index);
                self.on = false;
                return Ok(None);
            }
            psci::Call::CpuOn {
                target,
                entry,
                context,
            } => self.power.cpu_on(target, entry, context),
            psci::Call::AffinityInfo { target } => self.power.affinity_info(target),
            psci::Call::SystemOff => return Ok(Some(Exit::Shutdown)),
            psci::Call::SystemReset => return Ok(Some(Exit::Reset)),
        };
        self.sys.set_x(0, ret as u64)?;
        Ok(None)
    }

    /// Trapped system-register accesses read as zero and ignore writes. HVF traps only a
    /// few debug/OS-lock registers here (e.g. OSLAR_EL1, MDCCINT_EL1 on macOS 26).
    fn sysreg_trap(&mut self, syndrome: u64) -> Result<()> {
        let a = esr::sysreg_access(syndrome);
        debug!(
            "trapped {} of sysreg {:#06x}",
            if a.read { "read" } else { "write" },
            a.encoding
        );
        if a.read {
            self.sys.set_x(a.reg, 0)?;
        }
        self.advance_pc(syndrome)
    }

    fn describe_fault(&self, ec: u32, syndrome: u64, ipa: u64) -> String {
        let pc = self.sys.reg(sys::Reg::PC).unwrap_or(0);
        format!("unhandled guest exception: EC {ec:#04x}, ESR {syndrome:#x}, IPA {ipa:#x}, PC {pc:#x}")
    }
}

/// Interrupts a vCPU's `run` from any thread, whether it is in the guest or parked.
#[derive(Debug, Clone)]
pub struct Kicker {
    sys: sys::VcpuKicker,
    power: Arc<power::Table>,
    index: usize,
}

impl Kicker {
    pub fn kick(&self) {
        self.power.kick(self.index);
        // A cancel is sticky for a vCPU outside the guest (hv_vcpu.h:371-381). It fails
        // only once the vCPU is destroyed, when there is nothing left to interrupt.
        let _ = self.sys.kick();
    }
}
