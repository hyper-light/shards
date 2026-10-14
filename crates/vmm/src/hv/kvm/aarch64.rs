//! The KVM backend's arm64 VM and vCPUs, with the interface the arm64 machine
//! (vm/aarch64.rs) has of Hypervisor.framework's: guest memory at stage 2, an in-kernel
//! GICv3 (KVM_DEV_TYPE_ARM_VGIC_V3, Documentation/virt/kvm/devices/arm-vgic-v3.rst), and
//! PSCI, which here is KVM's own (KVM_ARM_VCPU_PSCI_0_2, api.rst 4.82): a secondary vCPU
//! starts powered off, CPU_ON and CPU_OFF are the kernel's, and SYSTEM_OFF and
//! SYSTEM_RESET come back as system events. A trapped device access completes in the
//! kernel, which takes a read's data from the run's exit as the vCPU enters again.

use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, OnceLock};

use super::{Error, Kicker, Result, Slots, Thread, call, install_kick_handler, kvm, sys};
use crate::arch::aarch64::state::VcpuState;
use crate::arch::aarch64::{Entry, layout, sysreg};
use crate::hv::{Exit, Io, Touch};

/// Working sets' pages: the host's, as on x86_64.
pub const PAGE: u64 = 4 << 10;

/// What KVM must offer on arm64 besides what every architecture needs: its registers one
/// by one, in-kernel devices (the GICv3), and PSCI 0.2 or later.
pub(super) const REQUIRED: &[(u64, &str)] = &[
    (sys::CAP_ONE_REG, "KVM_CAP_ONE_REG"),
    (sys::CAP_DEVICE_CTRL, "KVM_CAP_DEVICE_CTRL"),
    (sys::CAP_ARM_PSCI_0_2, "KVM_CAP_ARM_PSCI_0_2"),
];

/// The most vCPUs a machine has: as many redistributors as the guest's memory map has room
/// for, contiguous from `GIC_REDIST`.
pub(super) const VCPU_LIMIT: u32 =
    ((layout::GIC_REDIST_MAX_END - layout::GIC_REDIST) / sys::VGIC_V3_REDIST_SIZE) as u32;

/// Nothing for the process to ask of Linux first.
pub(super) fn prepare() {}

/// The GIC's interrupts: 32 private to each vCPU, then the SPIs the machine uses
/// (arch::aarch64::layout), in KVM's steps of 32 (arm-vgic-v3.rst,
/// KVM_DEV_ARM_VGIC_GRP_NR_IRQS).
const NR_IRQS: u32 = (32 + layout::SPIS).next_multiple_of(32);

/// Whether a PCI bus's MSIs go to an ITS (D154): KVM's GICv3 has one (KVM_DEV_TYPE_ARM_VGIC_ITS),
/// which translates each MSI by the ID of the device that wrote it, so that its MSIs are
/// isolated from each other's, as Linux takes an ITS's (irq-gic-v3-its.c,
/// IRQ_DOMAIN_FLAG_ISOLATED_MSI) and a guest's VFIO needs (vfio_iommu_type1.c).
pub const ITS: bool = true;

/// The maintenance interrupt of a guest with EL2: PPI 9, INTID 25, as its devicetree says
/// (arch::aarch64::boot) and as Hypervisor.framework's.
const MAINTENANCE_INTID: u32 = 25;

/// MPIDR_EL1's affinity fields: Aff3 (39:32), Aff2, Aff1 and Aff0 (23:0).
const MPIDR_AFFINITY: u64 = 0xff_00ff_ffff;

/// ID registers that define the guest's CPU, as Hypervisor.framework's backend keeps them.
const ID_REGS: &[u16] = &[
    0xc000, 0xc020, 0xc021, 0xc028, 0xc029, 0xc030, 0xc031, 0xc038, 0xc039, 0xc03a,
];

/// The physical address width a VM may have here: KVM_CAP_ARM_VM_IPA_SIZE's, or 40 bits
/// where KVM sizes no VM's, as every VM then has (api.rst 4.2, KVM_CREATE_VM).
pub fn max_ipa_bits() -> Result<u32> {
    let bits = kvm()
        .map_err(Error::Guest)?
        .check_extension(sys::CAP_ARM_VM_IPA_SIZE)
        .map_err(call("KVM_CHECK_EXTENSION"))?;
    Ok(if bits > 0 { bits.unsigned_abs() } else { 40 })
}

/// Whether this host's KVM gives a guest EL2: a CPU with FEAT_NV2, its kernel booted with
/// `kvm-arm.mode=nested` (arch/arm64/kernel/cpufeature.c, has_nested_virt_support).
pub fn el2_supported() -> bool {
    kvm().is_ok_and(|k| k.check_extension(sys::CAP_ARM_EL2).is_ok_and(|n| n > 0))
}

/// Why a guest is given no EL2 here.
pub const NO_EL2: &str = "this host's KVM gives a guest no EL2: virtualization of a guest's own needs a CPU with FEAT_NV2, its kernel booted with kvm-arm.mode=nested";

/// KVM's GICv3 geometry: a distributor of one 64 KiB frame, and two for each vCPU's
/// redistributor, each placed on a 64 KiB boundary (arm-vgic-v3.rst); its SPIs, INTID 32
/// on, as many as the machine asks ([`NR_IRQS`]); and a 64 KiB place for a GICv2m frame,
/// which its machines leave empty, their MSIs going to the ITS ([`ITS`]).
#[derive(Debug, Clone, Copy)]
pub struct GicParams {
    pub dist_size: u64,
    pub dist_align: u64,
    pub redist_size: u64,
    pub redist_align: u64,
    pub msi_size: u64,
    pub msi_align: u64,
    pub spi_base: u32,
    pub spi_count: u32,
}

pub fn gic_params() -> Result<GicParams> {
    Ok(GicParams {
        dist_size: sys::VGIC_V3_DIST_SIZE,
        dist_align: 64 << 10,
        redist_size: sys::VGIC_V3_REDIST_SIZE,
        redist_align: 64 << 10,
        msi_size: 64 << 10,
        msi_align: 64 << 10,
        spi_base: 32,
        spi_count: NR_IRQS - 32,
    })
}

/// A GICv2m frame: the VMM's, at `base`, its SPIs `count` from INTID `first_intid`.
#[derive(Debug, Clone, Copy)]
pub struct MsiFrame {
    pub base: u64,
    pub first_intid: u32,
    pub count: u32,
}

/// Where the GIC's parts are: its distributor and redistributors, and its ITS (KVM's
/// machines have no GICv2m frame: `msi` is refused).
#[derive(Debug, Clone, Copy)]
pub struct GicLayout {
    pub dist_base: u64,
    pub redist_base: u64,
    pub msi: Option<MsiFrame>,
    pub its: Option<u64>,
}

/// Drives the GIC's SPIs, and the MSIs its ITS translates, from any thread.
#[derive(Debug, Clone)]
pub struct Gic(Arc<GicState>);

#[derive(Debug)]
struct GicState {
    fd: Arc<sys::VmFd>,
    /// The MSIs routed to GSIs of their own for irqfds to raise ([`Gic::route_msi`]).
    routes: Mutex<Vec<MsiRoute>>,
}

/// An MSI on a GSI of its own: its address and data, and the ID of the device that writes
/// it, which the ITS translates by.
#[derive(Debug, Clone, Copy)]
struct MsiRoute {
    gsi: u32,
    address: u64,
    data: u32,
    devid: u32,
}

impl Gic {
    /// Drives SPI `intid`'s line. For an edge-configured INTID `true` makes it pending
    /// once and `false` does nothing, as KVM injects (vgic.c, kvm_vgic_inject_irq).
    pub fn set_spi(&self, intid: u32, level: bool) -> Result<()> {
        let irq = (sys::IRQ_TYPE_SPI << sys::IRQ_TYPE_SHIFT) | (intid & sys::IRQ_NUM_MASK);
        self.0.fd.irq_line(irq, level).map_err(call("KVM_IRQ_LINE"))
    }

    /// Delivers the MSI of `address` and `data` that device `devid` wrote to the ITS,
    /// which translates it by that ID (api.rst 4.70, KVM_SIGNAL_MSI with
    /// KVM_MSI_VALID_DEVID): as a function the VMM emulates sends one.
    pub fn its_msi(&self, address: u64, data: u32, devid: u32) -> Result<()> {
        self.0
            .fd
            .signal_msi(address, data, Some(devid))
            .map_err(call("KVM_SIGNAL_MSI"))
    }

    /// Raises SPI `intid` each time `event` is written, in the kernel (KVM_IRQFD): its GSI
    /// is the SPI's number, as KVM routes every SPI by default (vgic-irqfd.c,
    /// kvm_vgic_setup_default_irq_routing). With `resample`, a level-triggered line, which
    /// KVM lowers as the guest ends the interrupt, writing `resample`.
    pub fn irqfd(&self, event: RawFd, intid: u32, resample: Option<RawFd>) -> Result<()> {
        let gsi = intid
            .checked_sub(32)
            .ok_or_else(|| Error::Guest(format!("INTID {intid}: not an SPI")))?;
        self.0.fd.irqfd(event, gsi, resample).map_err(call("KVM_IRQFD"))
    }

    /// Takes `event` from SPI `intid` again.
    pub fn irqfd_remove(&self, event: RawFd, intid: u32) -> Result<()> {
        let gsi = intid
            .checked_sub(32)
            .ok_or_else(|| Error::Guest(format!("INTID {intid}: not an SPI")))?;
        self.0.fd.irqfd_remove(event, gsi).map_err(call("KVM_IRQFD"))
    }

    /// A GSI past the SPIs' that sends the MSI of `address` and `data` from device `devid`
    /// to the ITS, which `event` then raises (KVM_IRQFD); the routing table set whole, as
    /// KVM_SET_GSI_ROUTING replaces it (api.rst 4.52): KVM's defaults, each SPI on the GSI
    /// of its number (vgic-irqfd.c, kvm_vgic_setup_default_irq_routing), and every MSI
    /// routed, each with its device's ID (KVM_MSI_VALID_DEVID).
    pub fn route_msi(&self, event: RawFd, address: u64, data: u32, devid: u32) -> Result<u32> {
        let mut routes = crate::sync::lock(&self.0.routes);
        let gsi = (NR_IRQS - 32..)
            .find(|gsi| routes.iter().all(|r| r.gsi != *gsi))
            .ok_or_else(|| Error::Guest("no GSI left for an MSI".into()))?;
        routes.push(MsiRoute {
            gsi,
            address,
            data,
            devid,
        });
        if let Err(e) = self.commit(&routes) {
            routes.pop();
            return Err(e);
        }
        if let Err(e) = self.0.fd.irqfd(event, gsi, None).map_err(call("KVM_IRQFD")) {
            routes.pop();
            let _ = self.commit(&routes);
            return Err(e);
        }
        Ok(gsi)
    }

    /// Takes `event` from the MSI route of `gsi`, and the route from the table.
    pub fn unroute_msi(&self, event: RawFd, gsi: u32) -> Result<()> {
        let mut routes = crate::sync::lock(&self.0.routes);
        self.0.fd.irqfd_remove(event, gsi).map_err(call("KVM_IRQFD"))?;
        routes.retain(|r| r.gsi != gsi);
        self.commit(&routes)
    }

    fn commit(&self, routes: &[MsiRoute]) -> Result<()> {
        let mut entries: Vec<sys::kvm_irq_routing_entry> = (0..NR_IRQS - 32)
            .map(|gsi| {
                let mut u = [0u32; 8];
                // struct kvm_irq_routing_irqchip: the irqchip, 0, and the pin, the SPI.
                u[1] = gsi;
                sys::kvm_irq_routing_entry {
                    gsi,
                    kind: sys::IRQ_ROUTING_IRQCHIP,
                    u,
                    ..Default::default()
                }
            })
            .collect();
        for r in routes {
            let mut u = [0u32; 8];
            // struct kvm_irq_routing_msi: address_lo, address_hi, data, devid.
            (u[0], u[1], u[2], u[3]) = (r.address as u32, (r.address >> 32) as u32, r.data, r.devid);
            entries.push(sys::kvm_irq_routing_entry {
                gsi: r.gsi,
                kind: sys::IRQ_ROUTING_MSI,
                flags: sys::MSI_VALID_DEVID,
                u,
                ..Default::default()
            });
        }
        self.0
            .fd
            .set_gsi_routing(&entries)
            .map_err(call("KVM_SET_GSI_ROUTING"))
    }
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

/// The VM. It must outlive every [`Vcpu`], and guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    fd: Arc<sys::VmFd>,
    pub(super) slots: Slots,
    mmap_size: usize,
    mpidrs: Vec<u64>,
    el2: bool,
    /// Each vCPU's target and features; secondaries add POWER_OFF.
    init: sys::kvm_vcpu_init,
    /// The GICv3, once made, initialized once every vCPU exists ([`Vm::vcpus_created`]).
    gic: OnceLock<sys::DeviceFd>,
    /// Its ITS, where the machine has one.
    its: OnceLock<sys::DeviceFd>,
    /// KVM's VFIO device, made as the first VFIO group is added ([`Vm::add_vfio_group`]).
    pub(super) vfio: OnceLock<sys::DeviceFd>,
}

impl Vm {
    pub fn new(config: VmConfig) -> Result<Vm> {
        install_kick_handler()?;
        let kvm = kvm().map_err(Error::Guest)?;
        let has = |cap| {
            kvm.check_extension(cap)
                .map(|n| n > 0)
                .map_err(call("KVM_CHECK_EXTENSION"))
        };
        // A width asked only of a KVM that sizes VMs: elsewhere every VM has 40 bits, and
        // any other type fails.
        let kind = if has(sys::CAP_ARM_VM_IPA_SIZE)? {
            u64::from(config.ipa_bits)
        } else {
            0
        };
        let fd = Arc::new(kvm.create_vm(kind).map_err(call("KVM_CREATE_VM"))?);
        // A data abort KVM cannot emulate, outside every memory slot with no access in its
        // syndrome, returned to the VMM, which says where it was: else KVM_RUN fails with
        // ENOSYS alone (api.rst 4.10, 7.20).
        if has(u64::from(sys::CAP_ARM_NISV_TO_USER))? {
            fd.enable_cap(sys::CAP_ARM_NISV_TO_USER)
                .map_err(call("KVM_ENABLE_CAP(KVM_CAP_ARM_NISV_TO_USER)"))?;
        }
        let mut init = fd.preferred_target().map_err(call("KVM_ARM_PREFERRED_TARGET"))?;
        let mut features = 1 << sys::VCPU_PSCI_0_2;
        // Pointer authentication where KVM gives both kinds, as the guest has it under
        // Hypervisor.framework: KVM takes both or neither (api.rst 4.82).
        if has(sys::CAP_ARM_PTRAUTH_ADDRESS)? && has(sys::CAP_ARM_PTRAUTH_GENERIC)? {
            features |= (1 << sys::VCPU_PTRAUTH_ADDRESS) | (1 << sys::VCPU_PTRAUTH_GENERIC);
        }
        if config.el2 {
            if !has(sys::CAP_ARM_EL2)? {
                return Err(Error::Guest(NO_EL2.into()));
            }
            features |= 1 << sys::VCPU_HAS_EL2;
        }
        if let Some(first) = init.features.first_mut() {
            *first |= features;
        }
        Ok(Vm {
            slots: Slots::new(fd.clone()),
            fd,
            mmap_size: kvm.vcpu_mmap_size().map_err(call("KVM_GET_VCPU_MMAP_SIZE"))?,
            mpidrs: config.mpidrs,
            el2: config.el2,
            init,
            gic: OnceLock::new(),
            its: OnceLock::new(),
            vfio: OnceLock::new(),
        })
    }

    /// Creates the in-kernel GICv3 at `layout`'s places, its interrupts the machine's
    /// ([`NR_IRQS`]), and where the guest has EL2 its maintenance interrupt; it is
    /// initialized once every vCPU exists ([`Vm::vcpus_created`]). `layout`'s MSI frame is
    /// the machine's to emulate ([`MSI_FRAME_IN_KERNEL`]).
    pub fn create_gic(&self, layout: &GicLayout) -> Result<Gic> {
        if layout.msi.is_some() {
            return Err(Error::Guest(
                "KVM's GICv3 takes a PCI bus's MSIs at an ITS, not a GICv2m frame".into(),
            ));
        }
        let gic = self
            .fd
            .create_device(sys::DEV_TYPE_ARM_VGIC_V3)
            .map_err(call("KVM_CREATE_DEVICE"))?;
        let attr = call("KVM_SET_DEVICE_ATTR");
        gic.set_attr(sys::VGIC_GRP_ADDR, sys::VGIC_V3_ADDR_TYPE_DIST, &layout.dist_base)
            .map_err(&attr)?;
        gic.set_attr(
            sys::VGIC_GRP_ADDR,
            sys::VGIC_V3_ADDR_TYPE_REDIST,
            &layout.redist_base,
        )
        .map_err(&attr)?;
        gic.set_attr(sys::VGIC_GRP_NR_IRQS, 0, &NR_IRQS).map_err(&attr)?;
        if self.el2 {
            gic.set_attr(sys::VGIC_GRP_MAINT_IRQ, 0, &MAINTENANCE_INTID)
                .map_err(&attr)?;
        }
        self.gic
            .set(gic)
            .map_err(|_| Error::Guest("the VM has a GIC already".into()))?;
        // Its ITS (devices/arm-vgic-its.rst): its frame placed, then initialized, before
        // the GIC is, as every vCPU exists (KVM_DEV_ARM_VGIC_CTRL_INIT, which KVM's ITS
        // takes as done).
        if let Some(base) = layout.its {
            let its = self
                .fd
                .create_device(sys::DEV_TYPE_ARM_VGIC_ITS)
                .map_err(call("KVM_CREATE_DEVICE"))?;
            its.set_attr(sys::VGIC_GRP_ADDR, sys::VGIC_ITS_ADDR_TYPE, &base)
                .map_err(&attr)?;
            its.control(sys::VGIC_GRP_CTRL, sys::VGIC_CTRL_INIT)
                .map_err(&attr)?;
            self.its
                .set(its)
                .map_err(|_| Error::Guest("the VM has an ITS already".into()))?;
        }
        Ok(Gic(Arc::new(GicState {
            fd: self.fd.clone(),
            routes: Mutex::new(Vec::new()),
        })))
    }

    /// Initializes the GIC, once every vCPU exists and before any runs: its redistributors
    /// are the vCPUs' (arm-vgic-v3.rst, KVM_DEV_ARM_VGIC_CTRL_INIT).
    pub fn vcpus_created(&self) -> Result<()> {
        match self.gic.get() {
            Some(gic) => gic
                .control(sys::VGIC_GRP_CTRL, sys::VGIC_CTRL_INIT)
                .map_err(call("KVM_SET_DEVICE_ATTR")),
            None => Ok(()),
        }
    }

    /// Creates vCPU `index` on the calling thread, which owns it from then on: powered on
    /// for the boot vCPU, off for the others until their PSCI CPU_ON. vCPUs must be created
    /// in index order, as each takes the next redistributor (api.rst 4.7, `vcpu_idx`).
    pub fn create_vcpu(&self, index: usize) -> Result<Vcpu> {
        let &mpidr = self
            .mpidrs
            .get(index)
            .ok_or_else(|| Error::Guest(format!("no vCPU {index} in this VM")))?;
        let fd = self
            .fd
            .create_vcpu(index as u64, self.mmap_size)
            .map_err(call("KVM_CREATE_VCPU"))?;
        let mut init = self.init;
        if index > 0
            && let Some(first) = init.features.first_mut()
        {
            *first |= 1 << sys::VCPU_POWER_OFF;
        }
        fd.vcpu_init(&init).map_err(call("KVM_ARM_VCPU_INIT"))?;
        // KVM gives vCPU `index` the affinity the devicetree names (arch::aarch64::mpidr);
        // where they differ, PSCI and the GIC would not reach the CPU the guest means.
        let given = fd
            .get_reg(sys::sysreg(sysreg::MPIDR_EL1))
            .map_err(call("KVM_GET_ONE_REG"))?;
        if given & MPIDR_AFFINITY != mpidr {
            return Err(Error::Guest(format!(
                "vCPU {index}: KVM's MPIDR {given:#x}, where the devicetree says {mpidr:#x}"
            )));
        }
        let thread = super::this_thread();
        Ok(Vcpu {
            kicker: Kicker {
                run: fd.run.clone(),
                thread: thread.clone(),
            },
            fd,
            thread,
        })
    }

    /// The GIC's state, which no snapshot keeps yet: arm64 KVM VMs are not snapshotted
    /// (vm::SNAPSHOTS).
    pub fn save_gic(&self) -> Result<Vec<u8>> {
        Err(not_snapshotted())
    }

    pub fn restore_gic(&self, _state: &[u8]) -> Result<()> {
        Err(not_snapshotted())
    }
}

fn not_snapshotted() -> Error {
    Error::Guest("a VM on arm64 KVM is not snapshotted yet".into())
}

/// The host counter guest counters are offset from, now: CNTVCT_EL0, which Linux gives
/// its processes (arch_timer: CNTKCTL_EL1.EL0VCTEN).
pub fn host_counter() -> u64 {
    let value: u64;
    // SAFETY: reads a counter register EL0 may read; no memory is touched.
    unsafe { std::arch::asm!("mrs {0}, cntvct_el0", out(reg) value, options(nomem, nostack)) };
    value
}

/// A vCPU, run by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    fd: sys::VcpuFd,
    kicker: Kicker,
    /// Cleared on drop, so no kick can signal a thread that no longer runs this vCPU.
    pub(super) thread: Arc<Mutex<Option<Thread>>>,
}

impl Vcpu {
    pub fn kicker(&self) -> Kicker {
        self.kicker.clone()
    }

    /// Makes this the boot vCPU: it enters at `entry` on its first `run`, at EL1h (or
    /// EL2h where the VM has EL2) with DAIF masked and its MMU off, as KVM resets it
    /// (api.rst 4.82) and Linux boots (booting.rst).
    pub fn boot(&mut self, entry: Entry) -> Result<()> {
        self.fd
            .set_reg(sys::REG_PC, entry.pc)
            .map_err(call("KVM_SET_ONE_REG"))?;
        self.fd
            .set_reg(sys::reg_x(0), entry.x0)
            .map_err(call("KVM_SET_ONE_REG"))
    }

    /// Runs the guest until it powers off, resets, or a kick arrives.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        super::run(&mut self.fd, io)
    }

    /// The CPU identity the guest sees: ID registers by encoding.
    pub fn cpu_id(&self) -> Result<Vec<(u16, u64)>> {
        ID_REGS
            .iter()
            .map(|&r| {
                let value = self.fd.get_reg(sys::sysreg(r)).map_err(call("KVM_GET_ONE_REG"))?;
                Ok((r, value))
            })
            .collect()
    }

    pub fn save_state(&self) -> Result<VcpuState> {
        Err(not_snapshotted())
    }

    pub fn restore_state(&mut self, _state: &VcpuState) -> Result<()> {
        Err(not_snapshotted())
    }

    pub fn guest_counter(&self) -> Result<u64> {
        Err(not_snapshotted())
    }

    pub fn set_counter_offset(&mut self, _offset: u64) -> Result<()> {
        Err(not_snapshotted())
    }

    pub fn prefetch(&mut self, _vm: &Vm, _pages: &[Touch]) -> Result<usize> {
        Err(not_snapshotted())
    }
}
