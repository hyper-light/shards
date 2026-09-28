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

// ---- snapshots ---------------------------------------------------------------------

use crate::arch::aarch64::state::{Power, VcpuState};

/// EL1/EL0 system registers a snapshot carries: all the guest-changeable state HVF
/// exposes (hv_vcpu_types.h:257-420). SP_EL0/SP_EL1/ELR_EL1/SPSR_EL1 are system
/// registers here. The guest sees no SVE/SME, TCR2, PIE, POE, GCS or MTE, whose state HVF
/// could not save.
const SYSREGS: &[u16] = &[
    0xc080, 0xc082, 0xc100, 0xc101, 0xc102, // SCTLR, CPACR, TTBR0, TTBR1, TCR
    0xc108, 0xc109, 0xc10a, 0xc10b, 0xc110, 0xc111, 0xc112, 0xc113, 0xc118, 0xc119, // PAC keys
    0xc200, 0xc201, 0xc208, 0xe208, // SPSR_EL1, ELR_EL1, SP_EL0, SP_EL1
    0xc288, 0xc289, 0xc290, 0xc300, 0xc3a0, // AFSR0, AFSR1, ESR, FAR, PAR
    0xc510, 0xc518, 0xc600, 0xc681, 0xc684, // MAIR, AMAIR, VBAR, CONTEXTIDR, TPIDR_EL1
    0xc708, 0xd000, 0xde82, 0xde83, // CNTKCTL, CSSELR, TPIDR_EL0, TPIDRRO_EL0
    0xdf19, 0xdf1a, 0xdf11, 0xdf12, // CNTV_CTL, CNTV_CVAL, CNTP_CTL, CNTP_CVAL
    0x8012, 0x8010, 0xc081, // MDSCR, MDCCINT, ACTLR
];

/// macOS 15.2 additions (SCXTNUM_EL1, SCXTNUM_EL0): saved where the host has them.
const SYSREGS_15_2: &[u16] = &[0xc687, 0xde87];

/// ID registers that define the guest's CPU. A restore on a CPU that reports different
/// values is refused.
const ID_REGS: &[u16] = &[
    0xc000, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038, 0xc039, 0xc03a,
];
const ID_AA64DFR0_EL1: u16 = 0xc028;

/// Redistributor SGI/PPI state by GICR offset. The set-views carry the state; the
/// clear-views (ICENABLER, ICPENDR, ICACTIVER) are the same bits.
const REDIST_REGS: &[u32] = &[
    0x1_0080, 0x1_0100, 0x1_0200, 0x1_0300, // IGROUPR0, ISENABLER0, ISPENDR0, ISACTIVER0
    0x1_0400, 0x1_0404, 0x1_0408, 0x1_040c, 0x1_0410, 0x1_0414, 0x1_0418, 0x1_041c, // IPRIORITYR0-7
    0x1_0c00, 0x1_0c04, // ICFGR0-1
];

/// GIC CPU interface registers, in restore order: SRE and CTLR before the priority
/// state, group enables last. RPR is read-only.
const ICC_REGS: &[u16] = &[
    0xc665, 0xc664, 0xc230, 0xc643, 0xc663, 0xc644, 0xc648, 0xc666, 0xc667,
];

const HV_BAD_ARGUMENT: i32 = 0xfae9_4003_u32 as i32;
const GICD_CTLR: u16 = 0x0000;
/// GICD_CTLR.RWP is read-only; ARE_NS (bit 4) must be set before routing registers mean
/// anything.
const GICD_CTLR_RWP: u64 = 1 << 31;
const GICD_CTLR_ARE: u64 = 1 << 4;

/// The breakpoint and watchpoint registers ID_AA64DFR0_EL1 says exist.
fn debug_regs(dfr0: u64) -> Vec<u16> {
    let brps = ((dfr0 >> 12) & 0xf) + 1;
    let wrps = ((dfr0 >> 20) & 0xf) + 1;
    let mut regs = Vec::new();
    for n in 0..16u16 {
        let base = 0x8004 + (n << 3); // DBGBVRn_EL1; BCR +1, WVR +2, WCR +3
        if u64::from(n) < brps {
            regs.extend([base, base + 1]);
        }
        if u64::from(n) < wrps {
            regs.extend([base + 2, base + 3]);
        }
    }
    regs
}

/// Distributor registers covering INTIDs 32..`nint`: groups, enables, pending, active,
/// priorities, configuration and routing.
fn dist_regs(nint: u32) -> Vec<u16> {
    let mut regs = vec![GICD_CTLR];
    for base in [0x0080u32, 0x0100, 0x0200, 0x0300] {
        regs.extend((1..nint.div_ceil(32)).map(|i| (base + 4 * i) as u16));
    }
    regs.extend((8..nint.div_ceil(4)).map(|i| (0x0400 + 4 * i) as u16));
    regs.extend((2..nint.div_ceil(16)).map(|i| (0x0c00 + 4 * i) as u16));
    regs.extend((32..nint).map(|id| (0x6000 + 8 * id) as u16));
    regs
}

/// The host counter guest counters are offset from, now.
pub fn host_counter() -> u64 {
    sys::host_counter()
}

impl Vm {
    /// Distributor state. Every vCPU must be stopped.
    pub fn save_gic(&self) -> Result<Vec<(u32, u64)>> {
        let p = sys::gic_params()?;
        dist_regs(p.spi_base.saturating_add(p.spi_count).min(1020))
            .into_iter()
            .map(|r| Ok((u32::from(r), sys::dist_reg(r)?)))
            .collect()
    }

    /// Restores distributor state into a fresh GIC: routing needs ARE first; the group
    /// enables come last, so nothing is delivered from a half-restored distributor.
    pub fn restore_gic(&self, regs: &[(u32, u64)]) -> Result<()> {
        let reg = |r: u32| u16::try_from(r).map_err(|_| Error::Guest(format!("GICD offset {r:#x}")));
        let ctlr = regs
            .iter()
            .find(|&&(r, _)| r == u32::from(GICD_CTLR))
            .map_or(0, |&(_, v)| v & !GICD_CTLR_RWP);
        sys::set_dist_reg(GICD_CTLR, ctlr & GICD_CTLR_ARE)?;
        for &(r, v) in regs.iter().filter(|&&(r, _)| r != u32::from(GICD_CTLR)) {
            sys::set_dist_reg(reg(r)?, v)?;
        }
        Ok(sys::set_dist_reg(GICD_CTLR, ctlr)?)
    }
}

impl Vcpu {
    /// The CPU identity the guest sees: ID registers by encoding.
    pub fn cpu_id(&self) -> Result<Vec<(u16, u64)>> {
        ID_REGS.iter().map(|&r| Ok((r, self.sys.sys_reg(r)?))).collect()
    }

    /// Sets `CNTVCT_EL0 = host_counter() - offset`. Every vCPU of a VM gets the same
    /// offset, so their counters agree.
    pub fn set_counter_offset(&mut self, offset: u64) -> Result<()> {
        Ok(self.sys.set_vtimer_offset(offset)?)
    }

    /// The guest's virtual counter now.
    pub fn guest_counter(&self) -> Result<u64> {
        Ok(sys::host_counter().wrapping_sub(self.sys.vtimer_offset()?))
    }

    /// Captures the architectural state. Call on the owning thread, with the vCPU out of
    /// the guest.
    pub fn save_state(&self) -> Result<VcpuState> {
        let s = &self.sys;
        let mut x = [0u64; 31];
        for (n, v) in (0u8..).zip(x.iter_mut()) {
            *v = s.x(n)?;
        }
        let mut v = [0u128; 32];
        for (n, q) in (0u32..).zip(v.iter_mut()) {
            *q = s.simd(n)?;
        }
        let mut sys = Vec::with_capacity(SYSREGS.len() + 64);
        for &r in SYSREGS.iter().chain(&debug_regs(s.sys_reg(ID_AA64DFR0_EL1)?)) {
            sys.push((r, s.sys_reg(r)?));
        }
        for &r in SYSREGS_15_2 {
            match s.sys_reg(r) {
                Ok(val) => sys.push((r, val)),
                Err(e) if e.code == HV_BAD_ARGUMENT => {} // before macOS 15.2
                Err(e) => return Err(e.into()),
            }
        }
        Ok(VcpuState {
            x,
            pc: s.reg(sys::Reg::PC)?,
            pstate: s.reg(sys::Reg::CPSR)?,
            v,
            fpcr: s.reg(sys::Reg::FPCR)?,
            fpsr: s.reg(sys::Reg::FPSR)?,
            sys,
            redist: REDIST_REGS
                .iter()
                .map(|&r| Ok((r, s.redist_reg(r)?)))
                .collect::<Result<_>>()?,
            icc: ICC_REGS
                .iter()
                .map(|&r| Ok((r, s.icc_reg(r)?)))
                .collect::<Result<_>>()?,
            power: self
                .power
                .state(self.index)
                .ok_or_else(|| Error::Guest(format!("no power state for vCPU {}", self.index)))?,
        })
    }

    /// Loads captured state into this freshly created vCPU. The guest counter is set
    /// separately, when the vCPU is released ([`Vcpu::set_counter_offset`]).
    pub fn restore_state(&mut self, st: &VcpuState) -> Result<()> {
        let s = &mut self.sys;
        for (n, &v) in (0u8..).zip(st.x.iter()) {
            s.set_x(n, v)?;
        }
        s.set_reg(sys::Reg::PC, st.pc)?;
        s.set_reg(sys::Reg::CPSR, st.pstate)?;
        for (n, &q) in (0u32..).zip(st.v.iter()) {
            s.set_simd(n, q)?;
        }
        s.set_reg(sys::Reg::FPCR, st.fpcr)?;
        s.set_reg(sys::Reg::FPSR, st.fpsr)?;
        for &(r, v) in &st.sys {
            s.set_sys_reg(r, v)?;
        }
        for &(r, v) in &st.redist {
            s.set_redist_reg(r, v)?;
        }
        for &(r, v) in &st.icc {
            s.set_icc_reg(r, v)?;
        }
        self.power.set_state(self.index, st.power);
        // A running vCPU resumes where it stopped; others park until their CPU_ON.
        self.on = st.power == Power::On;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Real HVF, one VM (the per-process limit): this is the crate's only test that
    /// creates one.
    #[test]
    fn vcpu_and_gic_state_round_trip_through_a_fresh_vcpu() {
        if check_host().is_err() {
            return;
        }
        let vm = Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0, 1],
        })
        .unwrap();
        vm.create_gic(&GicLayout {
            dist_base: 0x0800_0000,
            redist_base: 0x080a_0000,
            msi: None,
        })
        .unwrap();

        // Distributor: perturb through the clear-views, restore, compare.
        sys::set_dist_reg(0x0104, 0b1010).unwrap(); // ISENABLER1: SPIs 33, 35
        sys::set_dist_reg(0x0420, 0xa0b0_c0d0).unwrap(); // IPRIORITYR8
        sys::set_dist_reg(0x0c08, 0x8).unwrap(); // ICFGR2: SPI 33 edge
        let saved = vm.save_gic().unwrap();
        sys::set_dist_reg(0x0184, u64::from(u32::MAX)).unwrap(); // ICENABLER1
        sys::set_dist_reg(0x0420, 0).unwrap();
        sys::set_dist_reg(0x0c08, 0).unwrap();
        vm.restore_gic(&saved).unwrap();
        assert_eq!(vm.save_gic().unwrap(), saved);

        // vCPU 0 gets distinctive state and is captured; vCPU 1 (the next redistributor
        // frame, so vCPU 0 stays alive meanwhile) restores it and must capture the same.
        let (to_b, from_a) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let vm = &vm;
        std::thread::scope(|s| {
            s.spawn(move || {
                let mut a = vm.create_vcpu(0).unwrap();
                for n in 0..31u8 {
                    a.sys.set_x(n, 0x1111_0000 + u64::from(n)).unwrap();
                }
                a.sys.set_reg(sys::Reg::PC, 0x8000_1234).unwrap();
                a.sys.set_reg(sys::Reg::CPSR, 0x3c5).unwrap();
                for n in 0..32u32 {
                    a.sys.set_simd(n, (u128::from(n) << 64) | 0xdead_beef).unwrap();
                }
                a.sys.set_sys_reg(0xde82, 0x7777).unwrap(); // TPIDR_EL0
                a.sys.set_sys_reg(0xc684, 0x8888).unwrap(); // TPIDR_EL1
                a.sys.set_sys_reg(0xc510, 0x04ff).unwrap(); // MAIR_EL1
                a.sys.set_icc_reg(0xc230, 0xf0).unwrap(); // ICC_PMR_EL1
                a.sys.set_redist_reg(0x1_0400, 0x8080_8080).unwrap(); // GICR_IPRIORITYR0
                let st = a.save_state().unwrap();
                assert_eq!(st.x[5], 0x1111_0005);
                assert_eq!(st.v[31], (31u128 << 64) | 0xdead_beef);
                to_b.send(st).unwrap();
                done_rx.recv().unwrap();
            });
            s.spawn(move || {
                let st = from_a.recv().unwrap();
                let mut b = vm.create_vcpu(1).unwrap();
                b.restore_state(&st).unwrap();
                assert_eq!(b.save_state().unwrap(), st);
                done_tx.send(()).unwrap();
            });
        });
    }
}
