//! The Hypervisor.framework backend (macOS on arm64), presenting KVM's semantics
//! (docs/research/hvf-arm64-kvm-ground-truth.md §5):
//!
//! - MMIO completes inside [`Vcpu::run`]. The backend decodes the syndrome, performs the
//!   access through [`Io`], writes Rt and advances PC, as `kvm_handle_mmio_return` does (row 6).
//! - PSCI runs here, CPU_ON included. A powered-off vCPU parks inside `run` (row 4).
//! - Trapped system registers read as zero and ignore writes.
//! - A working set is recorded by taking guest memory away at stage 2 ([`Watch`]), and
//!   prefetched by a vCPU that touches it before the guest runs ([`Vcpu::prefetch`]):
//!   HVF fills stage 2 only as the guest touches it (PM M5).

mod ffi;
mod power;
mod sys;

use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

pub use sys::{Gic, GicLayout, GicParams, MsiFrame};

use super::{Exit, Io, Touch};
use crate::arch::aarch64::{self, Entry, esr, layout, prefetch, psci, sysreg};
use crate::debug;
use crate::devices::get_le;
use crate::sync::lock;
use power::Wake;

/// The stage-2 page: shards sets HVF's 16 KiB IPA granule, the only one before macOS 26
/// (PM M5, D6).
pub const PAGE: u64 = 16 << 10;

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
    /// Whether the vCPUs have EL2, entered there: the guest's kernel may be a hypervisor
    /// (nested virtualization; [`el2_supported`]).
    pub el2: bool,
}

/// Whether this Mac gives a guest EL2: nested virtualization, from the M3 on macOS 15.
pub fn el2_supported() -> bool {
    sys::el2_supported()
}

/// Whether a PCI bus's MSIs go to an ITS: the framework's GIC has none, but a GICv2m frame
/// of its own, which takes the guest's writes to its doorbell
/// (hv_gic_config_set_msi_region_base).
pub const ITS: bool = false;

/// Why a guest is given no EL2 here.
pub const NO_EL2: &str = "this Mac gives a guest no EL2: virtualization of a guest's own needs an M3 or later, on macOS 15 or later";

/// Maps the memory a virtio-mem device plugs into the guest and unmaps it, from whichever
/// thread plugs it, while the VM lives ([`Vm::add_hotplug`]). The pages it maps are the
/// VM's until it unmaps them: replaced in the host while mapped, the guest kept the old
/// ones and its writes no longer reached the host's (PM M193).
#[derive(Debug, Clone, Copy)]
pub struct Stage2(());

impl Stage2 {
    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract; the handle exists only while the VM does.
        Ok(unsafe { sys::map(host, gpa, len, sys::Perms::RWX) }?)
    }

    /// Maps `len` bytes of a file's mapping at `host` at `gpa`, in a share's DAX window
    /// (D189): writable by the guest only if `writable`, as the host maps it, and
    /// executable, as a guest runs programs from its volumes.
    ///
    /// # Safety
    /// As for [`map`](Self::map).
    pub unsafe fn map_file(&self, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<()> {
        let perms = if writable { sys::Perms::RWX } else { sys::Perms::RX };
        // SAFETY: forwarded caller contract; the handle exists only while the VM does.
        Ok(unsafe { sys::map(host, gpa, len, perms) }?)
    }

    /// Unmaps `len` bytes at `gpa`: the guest's accesses there exit as MMIO to nothing.
    pub fn unmap(&self, gpa: u64, len: usize) -> Result<()> {
        Ok(sys::unmap(gpa, len)?)
    }
}

/// Maps the host memory of a GPU's blobs into its device's window and unmaps it, from the
/// device's worker, while the VM lives (D144): data the guest reads and writes, and never
/// executes.
#[derive(Debug, Clone, Copy)]
pub struct Window(());

impl Window {
    /// Maps `len` bytes of host memory at `host` at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract; the handle exists only while the VM does.
        Ok(unsafe { sys::map(host, gpa, len, sys::Perms::RW) }?)
    }

    /// Unmaps `len` bytes at `gpa`: the guest's accesses there exit as MMIO to nothing.
    pub fn unmap(&self, gpa: u64, len: usize) -> Result<()> {
        Ok(sys::unmap(gpa, len)?)
    }
}

/// Device memory a device maps into the guest and unmaps, while the VM lives: a swap
/// window's (D159). Slots are KVM's notion; the framework maps by address, so the index
/// names nothing here.
#[derive(Debug, Clone, Copy)]
pub struct DeviceSlots(());

impl DeviceSlots {
    /// Maps `len` bytes of host memory at `host` at `gpa`, read-only to the guest unless
    /// `writable`, never executable.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until unmapped here or the VM is
    /// destroyed.
    pub unsafe fn map(&self, _i: u32, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<()> {
        let perms = if writable { sys::Perms::RW } else { sys::Perms::R };
        // SAFETY: forwarded caller contract; the handle exists only while the VM does.
        Ok(unsafe { sys::map(host, gpa, len, perms) }?)
    }

    /// Unmaps `len` bytes at `gpa`: the guest's accesses there exit as MMIO to nothing.
    pub fn unmap(&self, _i: u32, gpa: u64, len: usize) -> Result<()> {
        Ok(sys::unmap(gpa, len)?)
    }
}

/// The process's VM. It must outlive every [`Vcpu`], and guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    sys: sys::Vm,
    power: Arc<power::Table>,
    mpidrs: Vec<u64>,
    ipa_bits: u32,
    el2: bool,
    /// Base and per-vCPU size of the redistributor frames, once the GIC exists.
    redist: OnceLock<(u64, u64)>,
    watch: Arc<Watch>,
}

impl Vm {
    pub fn new(config: VmConfig) -> Result<Vm> {
        let sys = sys::Vm::new(sys::VmConfig {
            ipa_bits: (config.ipa_bits > 36).then_some(config.ipa_bits),
            el2: config.el2,
        })?;
        Ok(Vm {
            sys,
            power: Arc::new(power::Table::new(&config.mpidrs)),
            mpidrs: config.mpidrs,
            ipa_bits: config.ipa_bits,
            el2: config.el2,
            redist: OnceLock::new(),
            watch: Arc::new(Watch::default()),
        })
    }

    /// What records this VM's working set.
    pub fn watch(&self) -> Arc<Watch> {
        self.watch.clone()
    }

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`.
    ///
    /// # Safety
    /// `host..host+len` must be one host allocation that stays mapped until the VM is
    /// destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.sys.map(host, gpa, len, sys::Perms::RWX) }?;
        self.watch.add(Region {
            gpa,
            len: len as u64,
            perms: sys::Perms::RWX,
        });
        self.power.add_ram(gpa, gpa.saturating_add(len as u64));
        Ok(())
    }

    /// What takes guest RAM from the guest and gives it back, from whichever thread gives
    /// its pages back to the host as the guest reports them free (D151).
    pub fn stage2(&self) -> Stage2 {
        Stage2(())
    }

    /// Gives the guest the memory a virtio-mem device plugs, `len` bytes at `host` seen at
    /// `gpa` (D133): none of it yet. Hypervisor.framework's stage 2 holds the pages it maps
    /// (PM M193), so each range is mapped as it is plugged, and unmapped before its pages
    /// are given back, through the [`Stage2`] this returns.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until the VM is destroyed.
    pub unsafe fn add_hotplug(&self, _host: *mut u8, _gpa: u64, _len: usize) -> Result<Stage2> {
        Ok(Stage2(()))
    }

    /// What maps a GPU's blobs into its window (D144).
    pub fn window(&self) -> Window {
        Window(())
    }

    /// What maps a device's memory into the guest while the VM lives, `count` slots of it
    /// on KVM; here the framework's mappings, by address.
    pub fn device_slots(&self, _count: u32) -> DeviceSlots {
        DeviceSlots(())
    }

    /// Maps device memory (a virtio-pmem region) at `gpa`, writable by the guest only if
    /// `writable`. It is not RAM: CPU_ON never enters it. A guest write to a read-only
    /// mapping traps like an MMIO write to nothing, and is dropped.
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
        let perms = if writable { sys::Perms::RWX } else { sys::Perms::RX };
        // SAFETY: forwarded caller contract.
        unsafe { self.sys.map(host, gpa, len, perms) }?;
        self.watch.add(Region {
            gpa,
            len: len as u64,
            perms,
        });
        Ok(())
    }

    /// Nothing to complete once every vCPU exists: HVF's GIC was whole as it was made.
    pub fn vcpus_created(&self) -> Result<()> {
        Ok(())
    }

    /// Creates the in-kernel GICv3. HVF requires it before any vCPU exists.
    pub fn create_gic(&self, layout: &GicLayout) -> Result<Gic> {
        if layout.its.is_some() {
            return Err(Error::Guest(
                "the framework's GIC has no ITS: a PCI bus's MSIs go to its GICv2m frame".into(),
            ));
        }
        let redist_size = sys::gic_params()?.redist_size;
        let gic = self.sys.create_gic(layout)?;
        let _ = self.redist.set((layout.redist_base, redist_size));
        Ok(gic)
    }

    /// Creates vCPU `index` on the calling thread, which owns it from then on. vCPUs must
    /// be created in index order: redistributor frames follow creation order (PM M13),
    /// and after guest memory is mapped.
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
        // Where it has EL2, its SCTLR_EL2 as the framework resets it, MMU and caches off,
        // which each entry puts back; and no SME: the framework leaves SMCR_EL2 undefined
        // at the guest's EL2, where Linux's `__finalise_el2` sets its vector length, and
        // faults there for ever (M5 Max, macOS 26.4.1).
        let el2 = if self.el2 {
            let pfr1 = vcpu.sys_reg(sysreg::ID_AA64PFR1_EL1)?;
            if pfr1 & sysreg::ID_AA64PFR1_SME != 0 {
                vcpu.set_sys_reg(sysreg::ID_AA64PFR1_EL1, pfr1 & !sysreg::ID_AA64PFR1_SME)?;
                if vcpu.sys_reg(sysreg::ID_AA64PFR1_EL1)? & sysreg::ID_AA64PFR1_SME != 0 {
                    return Err(guest("a vCPU at EL2 keeps SME, which it cannot use there".into()));
                }
            }
            Some(vcpu.sys_reg(sysreg::SCTLR_EL2)?)
        } else {
            None
        };
        Ok(Vcpu {
            kicker: Kicker {
                sys: vcpu.kicker(),
                power: self.power.clone(),
                index,
            },
            el2,
            sys: vcpu,
            power: self.power.clone(),
            index,
            on: false,
            memory: lock(&self.watch.regions).clone(),
            watch: self.watch.clone(),
        })
    }
}

/// Guest memory the VM maps, and the guest's access to it.
#[derive(Debug, Clone, Copy)]
struct Region {
    gpa: u64,
    len: u64,
    perms: sys::Perms,
}

impl Region {
    fn holds(&self, gpa: u64) -> bool {
        gpa.checked_sub(self.gpa).is_some_and(|off| off < self.len)
    }
}

/// Records a working set: guest memory is taken away from the guest at stage 2, and each
/// page goes back as the guest faults on it, noted in the order of first touch. A page
/// read goes back read-only while recording, so that a later write is noted too.
#[derive(Debug, Default)]
pub struct Watch {
    regions: Mutex<Vec<Region>>,
    recording: Mutex<Option<Recording>>,
}

#[derive(Debug, Default)]
struct Recording {
    touches: Vec<Touch>,
    /// Each page's place in `touches`.
    seen: HashMap<u64, usize>,
}

impl Watch {
    fn add(&self, region: Region) {
        lock(&self.regions).push(region);
    }

    /// Takes all guest memory away and starts recording. Call with every vCPU out of the
    /// guest.
    pub fn start(&self) -> Result<()> {
        *lock(&self.recording) = Some(Recording::default());
        let regions = lock(&self.regions).clone();
        for r in &regions {
            if let Err(e) = sys::protect(r.gpa, r.len, sys::Perms::NONE) {
                self.stop();
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Stops recording and gives the guest all its memory back. Returns the pages touched
    /// since [`Watch::start`], in the order of their first touch; none if it never started.
    /// Callable from any thread, while vCPUs run: a vCPU that faulted before the memory
    /// came back gets its page back in [`Vcpu::run`]. Under the recording's lock, as each
    /// page a vCPU gets back ([`Watch::give`]).
    pub fn stop(&self) -> Vec<Touch> {
        let mut recording = lock(&self.recording);
        let recorded = recording.take();
        if recorded.is_some() {
            for r in lock(&self.regions).iter() {
                // It fails only once the VM is gone, with nothing left to give back.
                let _ = sys::protect(r.gpa, r.len, r.perms);
            }
        }
        drop(recording);
        recorded.map(|r| r.touches).unwrap_or_default()
    }

    /// Gives `page` back after a fault on it, `write` if a write, in memory the guest may
    /// access with `full`: what it gets back ([`Recording::touched`]), given under the
    /// recording's lock, so that the access each vCPU is given lands in the order it was
    /// decided. A page given back read-only for one vCPU's read, after another's write had
    /// it back writable, took the write away again, and the writer faulted on a page it had
    /// just had back whole (PM M183). `again`: the faulting vCPU had this page back whole
    /// at its last exit, so that a fault on it now, still whole, is no taking away of the
    /// watch's: none is given then.
    fn give(&self, page: u64, write: bool, full: sys::Perms, again: bool) -> Result<Option<sys::Perms>> {
        let mut recording = lock(&self.recording);
        let perms = match recording.as_mut() {
            Some(r) => r.touched(page, write, full),
            None => full,
        };
        if perms == full && again {
            return Ok(None);
        }
        sys::protect(page, PAGE, perms)?;
        Ok(Some(perms))
    }
}

impl Recording {
    /// Notes a fault on `page`, `write` if a write: the access the page gets back, the most
    /// it may have (`full`) once any vCPU has written it, and read-only before, so that a
    /// later write is noted.
    fn touched(&mut self, page: u64, write: bool, full: sys::Perms) -> sys::Perms {
        let written = match self.seen.entry(page) {
            Slot::Occupied(at) => match self.touches.get_mut(*at.get()) {
                Some(t) => {
                    t.written |= write;
                    t.written
                }
                None => write,
            },
            Slot::Vacant(at) => {
                at.insert(self.touches.len());
                self.touches.push(Touch {
                    gpa: page,
                    written: write,
                });
                write
            }
        };
        if written { full } else { sys::Perms::RX }
    }
}

/// A vCPU, owned by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    sys: sys::Vcpu,
    power: Arc<power::Table>,
    kicker: Kicker,
    index: usize,
    /// Where the VM has EL2: the SCTLR_EL2 an entry puts back.
    el2: Option<u64>,
    /// Powered on. Only a powered-off vCPU consults the shared power table, so exits
    /// in the running guest take no lock.
    on: bool,
    /// The VM's guest memory, fixed before any vCPU exists: a fault there is a page taken
    /// away by `watch`, never a device access.
    memory: Vec<Region>,
    watch: Arc<Watch>,
}

impl Vcpu {
    pub fn kicker(&self) -> Kicker {
        self.kicker.clone()
    }

    /// Makes this the boot vCPU: it enters at `entry` on its first `run`. Others stay
    /// off until PSCI CPU_ON.
    pub fn boot(&mut self, entry: Entry) -> Result<()> {
        self.power.boot(self.index, entry);
        Ok(())
    }

    /// Runs the guest until it powers off, resets, or a kick arrives. Returns an error for
    /// guest behavior the VMM cannot emulate.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        // The page the last exit gave back whole: faulting again at once, it never will stop.
        let mut given_back = None;
        loop {
            if !self.on {
                match self.power.park(self.index) {
                    Wake::Kicked => return Ok(Exit::Canceled),
                    Wake::Start(entry) => self.enter(entry)?,
                    Wake::Running => {}
                }
                self.on = true;
            }
            // A kick may arrive as the vCPU leaves the guest for another exit, which HVF
            // then reports in its place, dropping the cancel: a snapshot's barrier waited
            // on a vCPU idle in the guest forever (the storm E2E test). Every entry
            // checks for one first, as KVM checks a vCPU's requests before entering.
            if self.power.take_kick(self.index) {
                return Ok(Exit::Canceled);
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
            let returned = given_back.take();
            match esr::ec(syndrome) {
                ec @ (esr::EC_DABT_LOW | esr::EC_IABT_LOW) if self.memory.iter().any(|r| r.holds(ipa)) => {
                    given_back = self.give_back(io, ec, syndrome, ipa, returned)?;
                }
                // A cache maintenance instruction on no guest memory: nothing to keep
                // coherent, as KVM has it (esr::cache_maintenance).
                esr::EC_DABT_LOW if esr::cache_maintenance(syndrome) => self.advance_pc(syndrome)?,
                // Memory the host could not give: a share's DAX window's range past its
                // file's end, the file cut short on the host under the guest (D189, PM
                // M271). Given now, the access runs again.
                esr::EC_DABT_LOW | esr::EC_IABT_LOW if io.memory_fault(Some(ipa)) => {}
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

    /// A fault on guest memory, which faults only while [`Watch`] has taken it away, or
    /// for a write to read-only device memory. The page goes back, and the access runs
    /// again. Returns the page if it went back whole; `last` is the page the previous exit
    /// gave back whole.
    fn give_back(
        &mut self,
        io: &dyn Io,
        ec: u32,
        syndrome: u64,
        ipa: u64,
        last: Option<u64>,
    ) -> Result<Option<u64>> {
        let Some(region) = self.memory.iter().find(|r| r.holds(ipa)).copied() else {
            return Err(Error::Guest(self.describe_fault(ec, syndrome, ipa)));
        };
        // Cache maintenance reports a write, and needs only to read: it goes on once the
        // page is back, read-only or not (esr::cache_maintenance).
        let write = ec == esr::EC_DABT_LOW && esr::writes(syndrome) && !esr::cache_maintenance(syndrome);
        if write && region.perms != sys::Perms::RWX {
            // Dropped, like a write to no device (map_device_memory).
            self.mmio(io, syndrome, ipa)?;
            return Ok(None);
        }
        let page = ipa & !(PAGE - 1);
        let Some(perms) = self.watch.give(page, write, region.perms, last == Some(page))? else {
            return Err(Error::Guest(format!(
                "guest memory at {ipa:#x} faults with its access given back"
            )));
        };
        Ok((perms == region.perms).then_some(page))
    }

    /// Architectural entry state for boot and CPU_ON: EL1h with DAIF masked, MMU and
    /// caches off, X0 as given, other GPRs zero (Linux booting.rst; PSCI 1.1 §6.4). Where
    /// the VM has EL2, EL2h instead, its MMU and caches off as the framework reset them.
    fn enter(&mut self, entry: Entry) -> Result<()> {
        match self.el2 {
            Some(sctlr) => {
                self.sys.set_reg(sys::Reg::CPSR, sysreg::PSTATE_EL2H_DAIF)?;
                self.sys.set_sys_reg(sysreg::SCTLR_EL2, sctlr)?;
            }
            None => {
                self.sys.set_reg(sys::Reg::CPSR, sysreg::PSTATE_EL1H_DAIF)?;
            }
        }
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
        // At EL2, an EL2 register the framework holds traps to be read or written there:
        // the guest's own hypervisor programs it (its timers, CNTHCTL_EL2 first).
        if self.el2.is_some() && sysreg::EL2_HELD.contains(&a.encoding) {
            if a.read {
                let value = self.sys.sys_reg(a.encoding)?;
                self.sys.set_x(a.reg, value)?;
            } else {
                let value = self.sys.x(a.reg)?;
                self.sys.set_sys_reg(a.encoding, value)?;
            }
            return self.advance_pc(syndrome);
        }
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

use crate::arch::aarch64::state::Power;
pub use crate::arch::aarch64::state::VcpuState;

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

/// GIC CPU interface registers, in restore order: SRE and CTLR before the priority
/// state, group enables last. RPR is read-only.
const ICC_REGS: &[u16] = &[
    0xc665, 0xc664, 0xc230, 0xc643, 0xc663, 0xc644, 0xc648, 0xc666, 0xc667,
];

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

/// The host counter guest counters are offset from, now.
pub fn host_counter() -> u64 {
    sys::host_counter()
}

impl Vm {
    /// The GIC device's state, distributor, redistributors and the interrupts HVF holds
    /// on their way to a vCPU, which no register shows: Hypervisor.framework's own
    /// serialization (hv_gic_state.h). Every vCPU must be stopped.
    pub fn save_gic(&self) -> Result<Vec<u8>> {
        Ok(sys::gic_state()?)
    }

    /// Restores [`save_gic`](Self::save_gic)'s state into this VM's GIC, once every vCPU
    /// exists and before any runs (hv_gic.h).
    pub fn restore_gic(&self, state: &[u8]) -> Result<()> {
        Ok(sys::set_gic_state(state)?)
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
                Err(e) if e.code == ffi::HV_BAD_ARGUMENT => {} // before macOS 15.2
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
        for &(r, v) in &st.icc {
            s.set_icc_reg(r, v)?;
        }
        self.power.set_state(self.index, st.power);
        // A running vCPU resumes where it stopped; others park until their CPU_ON.
        self.on = st.power == Power::On;
        Ok(())
    }

    /// Touches a working set's pages in guest memory from this vCPU, so that HVF maps them
    /// at stage 2 before the guest runs: a read for each page, and for each page the guest
    /// wrote, a write that copies it now. Pages outside guest memory are left out, as are
    /// writes to read-only memory and pages listed twice, so a damaged list costs no more
    /// than touching guest memory once. Returns how many pages it touched.
    ///
    /// For a fresh vCPU, before its state is restored: it runs the prefetch program
    /// ([`prefetch`](crate::arch::aarch64::prefetch)) at EL1 with its own MMU state, all of
    /// which the restore sets again; HVF returns its `hvc #0` as the loop's end.
    pub fn prefetch(&mut self, vm: &Vm, pages: &[Touch]) -> Result<usize> {
        let lse = prefetch::lse(self.sys.sys_reg(sysreg::ID_AA64ISAR0_EL1)?);
        let list = prefetch::entries(pages, PAGE, lse, |page| {
            let r = self.memory.iter().find(|r| r.holds(page))?;
            Some(r.perms == sys::Perms::RWX)
        });
        if list.is_empty() {
            return Ok(0);
        }
        let size = (PAGE as usize).saturating_add((list.len() * 8).next_multiple_of(PAGE as usize));
        let table = prefetch::table(&list);
        let scratch = Scratch::new(size)?;
        scratch.put(
            0,
            &prefetch::LOOP
                .iter()
                .chain([&HVC])
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        scratch.put(
            prefetch::TABLE as usize,
            &table.iter().flat_map(|e| e.to_le_bytes()).collect::<Vec<u8>>(),
        );
        scratch.put(
            PAGE as usize,
            &list.iter().flat_map(|e| e.to_le_bytes()).collect::<Vec<u8>>(),
        );
        let began = crate::log::uptime_us();
        // SAFETY: `scratch` stays mapped until after the unmap below.
        unsafe { vm.sys.map(scratch.host, layout::PREFETCH, size, sys::Perms::RX) }?;
        let mapped = crate::log::uptime_us();
        // What the loop takes from its host: its thread's CPU time, the guest's included
        // (PM M288), and the process's faults, page-ins and context switches.
        let clock = crate::platform::ThreadClock::current().ok();
        let (cpu, events) = (clock.and_then(|c| c.read().ok()), crate::platform::task_events());
        let ran = self.run_prefetch(
            layout::PREFETCH + prefetch::TABLE,
            layout::PREFETCH + PAGE,
            list.len(),
        );
        let looped = crate::log::uptime_us();
        let cpu = cpu
            .zip(clock.and_then(|c| c.read().ok()))
            .map(|(a, b)| b.saturating_sub(a) / 1000);
        // Each a 32-bit counter, which may have wrapped between the two.
        let events = events.zip(crate::platform::task_events()).map(|(a, b)| {
            a.iter()
                .zip(b)
                .map(|(a, b)| b.wrapping_sub(*a) & u64::from(u32::MAX))
                .collect::<Vec<_>>()
        });
        if let Err(e) = vm.sys.unmap(layout::PREFETCH, size) {
            // The guest still maps it: it must outlive the VM.
            std::mem::forget(scratch);
            return Err(e.into());
        }
        debug!(
            "the prefetch's loop mapped in {} us, ran over {} pages in {} us ({cpu:?} us of it \
             on its CPU; the process's faults, page-ins, copy-on-write faults and context \
             switches meanwhile {events:?}), unmapped in {} us",
            mapped.saturating_sub(began),
            list.len(),
            looped.saturating_sub(mapped),
            crate::log::uptime_us().saturating_sub(looped)
        );
        ran.map(|()| list.len())
    }

    fn run_prefetch(&mut self, table: u64, list: u64, n: usize) -> Result<()> {
        let s = &mut self.sys;
        let parange = s.sys_reg(sysreg::ID_AA64MMFR0_EL1)?;
        for (r, value) in prefetch::registers(parange, table) {
            s.set_sys_reg(r, value)?;
        }
        s.set_reg(sys::Reg::CPSR, sysreg::PSTATE_EL1H_DAIF)?;
        s.set_x(0, list)?;
        s.set_x(1, n as u64)?;
        s.set_reg(sys::Reg::PC, layout::PREFETCH)?;
        // Nothing kicks a vCPU before its setup is done, so the loop ends only at its HVC.
        match s.run()? {
            sys::Exit::Exception { syndrome, .. } if esr::ec(syndrome) == esr::EC_HVC64 => Ok(()),
            sys::Exit::Exception { syndrome, ipa, .. } => Err(Error::Guest(format!(
                "prefetching the working set: ESR {syndrome:#x} at IPA {ipa:#x}"
            ))),
            other => Err(Error::Guest(format!("prefetching the working set: {other:?}"))),
        }
    }
}

/// The prefetch loop's end here: `hvc #0`, which HVF returns as an exception.
const HVC: u32 = 0xd400_0002;

/// Anonymous host memory for the prefetch loop, unmapped on drop.
struct Scratch {
    host: *mut u8,
    len: usize,
}

impl Scratch {
    fn new(len: usize) -> Result<Scratch> {
        // SAFETY: a fresh private anonymous mapping, owned by the Scratch.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if host == libc::MAP_FAILED {
            return Err(Error::Guest(format!(
                "memory for the prefetch loop: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Scratch {
            host: host.cast(),
            len,
        })
    }

    /// Copies `bytes` in at `offset`, as far as they fit.
    fn put(&self, offset: usize, bytes: &[u8]) {
        let n = bytes.len().min(self.len.saturating_sub(offset));
        if n == 0 {
            return;
        }
        // SAFETY: `offset + n` is within the mapping, which nothing else references.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.host.add(offset), n) };
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: our own mapping, which the guest no longer maps.
        unsafe { libc::munmap(self.host.cast(), self.len) };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A page any vCPU has written is given back writable to every vCPU that faults on it
    /// after, a reader's too: one given back read-only after a write took the write away
    /// from the writer (PM M183).
    #[test]
    fn a_page_written_stays_writable_for_every_vcpu() {
        let full = sys::Perms::RWX;
        let mut r = Recording::default();
        assert_eq!(r.touched(0x1000, false, full), sys::Perms::RX);
        assert_eq!(r.touched(0x1000, true, full), full);
        assert_eq!(r.touched(0x1000, false, full), full, "a read after the write");
        assert_eq!(r.touched(0x2000, true, full), full);
        assert_eq!(
            r.touched(0x2000, false, full),
            full,
            "a read after the first write"
        );
        assert_eq!(r.touches.len(), 2);
        assert!(r.touches.iter().all(|t| t.written));
    }

    /// Real HVF, one VM (the per-process limit): this is the crate's only test that
    /// creates one.
    #[test]
    fn vcpu_and_gic_state_round_trip_through_a_fresh_vcpu() {
        if check_host().is_err() {
            return;
        }
        // Guest RAM with a pattern, mapped before any vCPU exists. It outlives the VM.
        const RAM: u64 = 0x8000_0000;
        let ram_len = 1usize << 20;
        let ram = crate::platform::reserve(ram_len).unwrap().as_ptr();
        let pattern = |i: usize| (i % 251) as u8;
        for i in 0..ram_len {
            // SAFETY: within the fresh reservation.
            unsafe { ram.add(i).write(pattern(i)) };
        }
        let vm = Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0, 1],
            el2: false,
        })
        .unwrap();
        // SAFETY: the reservation is never unmapped.
        unsafe { vm.map_ram(ram, RAM, ram_len) }.unwrap();
        let gic = vm
            .create_gic(&GicLayout {
                dist_base: 0x0800_0000,
                redist_base: 0x080a_0000,
                msi: None,
                its: None,
            })
            .unwrap();

        // The GIC's state, a pending edge on an SPI among it: perturb it all through the
        // clear-views, restore the state, and read it all back.
        sys::set_dist_reg(0x0104, 0b1010).unwrap(); // ISENABLER1: SPIs 33, 35
        sys::set_dist_reg(0x0420, 0xa0b0_c0d0).unwrap(); // IPRIORITYR8
        sys::set_dist_reg(0x0c08, 0x8).unwrap(); // ICFGR2: SPI 33 edge
        gic.set_spi(33, true).unwrap();
        // ISENABLER1, IPRIORITYR8, ICFGR2, ISPENDR1.
        let regs = || [0x0104, 0x0420, 0x0c08, 0x0204].map(|r| sys::dist_reg(r).unwrap());
        let before = regs();
        assert_eq!(before, [0b1010, 0xa0b0_c0d0, 0x8, 0b10]);
        let saved = vm.save_gic().unwrap();
        sys::set_dist_reg(0x0184, u64::from(u32::MAX)).unwrap(); // ICENABLER1
        sys::set_dist_reg(0x0420, 0).unwrap();
        sys::set_dist_reg(0x0c08, 0).unwrap();
        sys::set_dist_reg(0x0284, u64::from(u32::MAX)).unwrap(); // ICPENDR1
        assert_eq!(regs(), [0; 4]);
        vm.restore_gic(&saved).unwrap();
        assert_eq!(regs(), before);

        // vCPU 0 gets distinctive state and is captured; vCPU 1 (the next redistributor
        // frame, so vCPU 0 stays alive meanwhile) restores it and must capture the same.
        let (to_b, from_a) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let vm = &vm;
        std::thread::scope(|s| {
            s.spawn(move || {
                let mut a = vm.create_vcpu(0).unwrap();

                // The prefetch touches a page read and a page written, leaves out a page
                // outside guest memory and a page listed twice, and changes no byte. A
                // recording with nothing running records nothing, and gives the memory back.
                let pages = [
                    Touch {
                        gpa: RAM,
                        written: false,
                    },
                    Touch {
                        gpa: RAM + PAGE,
                        written: true,
                    },
                    Touch {
                        gpa: 0x1_0000_0000,
                        written: true,
                    },
                    Touch {
                        gpa: RAM + PAGE,
                        written: false,
                    },
                ];
                assert_eq!(a.prefetch(vm, &pages).unwrap(), 2);
                let watch = vm.watch();
                watch.start().unwrap();
                assert!(watch.stop().is_empty());
                assert_eq!(a.prefetch(vm, &pages).unwrap(), 2);

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
        for i in 0..ram_len {
            // SAFETY: within the reservation; no vCPU runs any more.
            assert_eq!(unsafe { ram.add(i).read() }, pattern(i), "byte {i}");
        }
    }

    /// Which host mappings `hv_vm_map` accepts for read-only device memory
    /// (platform-measurements M17). Ignored because HVF allows one VM per process: run it
    /// alone with `--ignored --exact`.
    #[test]
    #[ignore]
    fn hvf_maps_private_but_not_shared_read_only_files() {
        use std::os::fd::AsRawFd;
        let vm = Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0],
            el2: false,
        })
        .unwrap();
        let path_dir = shards_testdir::TempDir::new("m17.img").unwrap();
        let path = path_dir.join("m17.img");
        std::fs::write(&path, vec![1u8; 2 << 20]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let len = 2usize << 20;
        let mut gpa = 0x1_0000_0000u64;
        let mut map = |flags: libc::c_int, perms: sys::Perms| {
            // SAFETY: a fresh mapping of our own file; never unmapped while the VM lives.
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    flags,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(p, libc::MAP_FAILED);
            gpa += len as u64;
            // SAFETY: as above.
            unsafe { vm.sys.map(p.cast(), gpa, len, perms) }.is_ok()
        };
        for perms in [sys::Perms::R, sys::Perms::RX, sys::Perms::RWX] {
            assert!(
                !map(libc::MAP_SHARED, perms),
                "HVF now maps shared read-only files"
            );
            assert!(
                map(libc::MAP_PRIVATE, perms),
                "HVF refused a private read-only file"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Which mappings of a sparse file keep APFS from saying where its holes are
    /// (lseek(2) SEEK_DATA and SEEK_HOLE): the host's shared writable one, and the guest's
    /// through hv_vm_map of it. Ignored because HVF allows one VM per process: run it alone
    /// with `--ignored --exact`.
    #[test]
    #[ignore]
    fn which_mappings_hide_a_files_holes() {
        use std::os::fd::AsRawFd;
        let vm = Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0],
            el2: false,
        })
        .unwrap();
        let dir = shards_testdir::TempDir::new("holes").unwrap();
        let len = 64usize << 20;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("sparse"))
            .unwrap();
        file.set_len(len as u64).unwrap();
        crate::platform::write_all_at(&file, &[1; 16 << 10], 32 << 20).unwrap();
        file.sync_all().unwrap();
        let data = || crate::platform::data_ranges(&file, len as u64).unwrap();
        let alone = data();
        assert_eq!(alone, [(32 << 20, 16 << 10)]);
        // SAFETY: a fresh shared mapping of our own file, which only this test uses.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        let hosts = data();
        let gpa = 0x1_0000_0000u64;
        // SAFETY: the mapping stays, or memory of the process's own over it, until the VM goes.
        unsafe { vm.sys.map(host.cast(), gpa, len, sys::Perms::RW) }.unwrap();
        let both = data();
        // The host's mapping replaced by memory of its own, the guest's kept.
        // SAFETY: MAP_FIXED over the test's own mapping, which nothing touches.
        let replaced = unsafe {
            libc::mmap(
                host,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        assert_eq!(replaced, host);
        let guests = data();
        sys::unmap(gpa, len).unwrap();
        let neither = data();
        eprintln!(
            "none {alone:?}; the host's {hosts:?}; both {both:?}; the guest's alone {guests:?}; neither {neither:?}"
        );
        assert_eq!(neither, alone);
    }

    /// What a vCPU thread's CPU time says, read from another thread, while its guest runs
    /// with no exit (D191's hold from the host): a guest spinning on `b .`, its timer never
    /// armed, read each 10 ms for a second by thread_info(THREAD_EXTENDED_INFO); and what a
    /// read costs. Ignored because HVF allows one VM per process: run it alone with
    /// `--ignored --exact`.
    #[test]
    #[ignore]
    fn a_vcpu_threads_cpu_time_counts_its_guest() {
        use std::time::{Duration, Instant};
        const RAM: u64 = 0x8000_0000;
        let ram_len = 1usize << 20;
        let ram = crate::platform::reserve(ram_len).unwrap().as_ptr();
        // SAFETY: within the fresh reservation: `b .`.
        unsafe { ram.cast::<u32>().write(0x1400_0000) };
        let vm = Vm::new(VmConfig {
            ipa_bits: 36,
            mpidrs: vec![0],
            el2: false,
        })
        .unwrap();
        // SAFETY: the reservation is never unmapped.
        unsafe { vm.map_ram(ram, RAM, ram_len) }.unwrap();
        let _gic = vm
            .create_gic(&GicLayout {
                dist_base: 0x0800_0000,
                redist_base: 0x080a_0000,
                msi: None,
                its: None,
            })
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let vm = &vm;
        std::thread::scope(|s| {
            let spinning = s.spawn(move || {
                let mut a = vm.create_vcpu(0).unwrap();
                a.sys.set_reg(sys::Reg::PC, RAM).unwrap();
                a.sys.set_reg(sys::Reg::CPSR, 0x3c5).unwrap();
                // SAFETY: this thread's own port, which lives as long as it.
                let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
                tx.send((port, a.sys.kicker())).unwrap();
                let mut exits = 0u64;
                loop {
                    match a.sys.run().unwrap() {
                        sys::Exit::Canceled => break,
                        _ => exits += 1,
                    }
                }
                exits
            });
            let (port, kicker) = rx.recv().unwrap();
            let cpu = || {
                // SAFETY: zeroed plain data, which thread_info fills.
                let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
                let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
                // SAFETY: a live thread's port, a buffer of `count` words.
                let r = unsafe {
                    libc::thread_info(
                        port,
                        libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
                        (&raw mut info).cast(),
                        &mut count,
                    )
                };
                assert_eq!(r, libc::KERN_SUCCESS);
                info.pth_user_time + info.pth_system_time
            };
            let (t0, c0) = (Instant::now(), cpu());
            let mut samples = Vec::new();
            let mut reads = Vec::new();
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(10));
                let r0 = Instant::now();
                let c = cpu();
                reads.push(r0.elapsed().as_nanos());
                samples.push((t0.elapsed().as_nanos(), u128::from(c - c0)));
            }
            kicker.kick().unwrap();
            let exits = spinning.join().unwrap();
            // How far behind the wall the thread's time ever fell, and its steps.
            let lag = samples.iter().map(|&(w, c)| w.saturating_sub(c)).max().unwrap();
            let steps: Vec<u128> = samples.windows(2).map(|w| w[1].1 - w[0].1).collect();
            reads.sort_unstable();
            let (w, c) = samples[samples.len() - 1];
            eprintln!(
                "wall {w} ns, cpu {c} ns ({:.4}), {exits} exits; most behind {lag} ns; steps min {} \
                 max {} ns; a read p50 {} p99 {} max {} ns",
                c as f64 / w as f64,
                steps.iter().min().unwrap(),
                steps.iter().max().unwrap(),
                reads[reads.len() / 2],
                reads[reads.len() * 99 / 100],
                reads[reads.len() - 1],
            );
        });
    }
}
