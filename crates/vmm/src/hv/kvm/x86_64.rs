//! The KVM backend's x86_64 VM and vCPUs. On x86 the guest powers off and resets through
//! devices (ACPI sleep control, i8042), so those come from the machine, and a triple fault
//! is a reset. Ground truth: docs/research/kvm-x86_64-ground-truth.md.

use std::io;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, OnceLock};

use super::state::{VcpuState, VmState};
use super::{Error, Kicker, Result, Slots, Thread, call, install_kick_handler, kvm, sys};
use crate::arch::x86_64::{Boot, Segment, cpuid, layout};
use crate::hv::{Exit, Io};
use crate::sync::lock;

/// What KVM must offer on x86_64 besides what every architecture needs (research doc §9
/// step 1).
pub(super) const REQUIRED: &[(u64, &str)] = &[
    (sys::CAP_SET_TSS_ADDR, "KVM_CAP_SET_TSS_ADDR"),
    (sys::CAP_EXT_CPUID, "KVM_CAP_EXT_CPUID"),
    (sys::CAP_SET_IDENTITY_MAP_ADDR, "KVM_CAP_SET_IDENTITY_MAP_ADDR"),
];

/// The most vCPUs a machine has: 254, since the MADT carries 8-bit APIC ids (research
/// doc §3.5).
pub(super) const VCPU_LIMIT: u32 = 254;

/// What the process asks of Linux once, before KVM is asked what guests may have.
pub(super) fn prepare() {
    permit_amx();
}

/// arch/x86/include/uapi/asm/prctl.h.
const ARCH_GET_XCOMP_SUPP: libc::c_int = 0x1021;
const ARCH_REQ_XCOMP_GUEST_PERM: libc::c_int = 0x1025;
const ARCH_XCOMP_TILEDATA: u64 = 18;

/// What `arch_prctl(code, &features)` says, or `None` where Linux knows no such request:
/// before 5.16 for what the host supports, before 5.17 for what guests are permitted.
fn xcomp(code: libc::c_int) -> Option<u64> {
    let mut features: u64 = 0;
    // SAFETY: arch_prctl(2) writing a feature mask into a u64 of ours.
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, code, &raw mut features) };
    (r == 0).then_some(features)
}

/// Asks that this process's guests may have AMX's tile data, where the host has it, before
/// KVM is asked what guests may have: KVM_GET_SUPPORTED_CPUID offers a dynamic XSAVE
/// feature only to a process permitted it (research doc §1.8 row 3), as Firecracker asks
/// (src/vmm/src/arch/x86_64/xstate.rs, request_dynamic_xstate_features; review 1.10). A
/// refusal costs guests AMX alone: where Linux before 6.4 then offers the tile
/// configuration without its data, the CPUID template goes without it too
/// ([`cpuid::without_lone_tile_config`]), where Firecracker fails the VM.
fn permit_amx() {
    let tiles = 1 << ARCH_XCOMP_TILEDATA;
    if xcomp(ARCH_GET_XCOMP_SUPP).is_none_or(|supported| supported & tiles == 0) {
        return;
    }
    // SAFETY: arch_prctl(2) with integer arguments.
    let r = unsafe {
        libc::syscall(
            libc::SYS_arch_prctl,
            ARCH_REQ_XCOMP_GUEST_PERM,
            ARCH_XCOMP_TILEDATA,
        )
    };
    if r != 0 {
        crate::warn!(
            "guests go without AMX: arch_prctl(ARCH_REQ_XCOMP_GUEST_PERM): {}",
            io::Error::last_os_error()
        );
    }
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub vcpus: u32,
}

/// The VM. It must outlive every [`Vcpu`], and guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    fd: Arc<sys::VmFd>,
    pub(super) slots: Slots,
    cpuid: Vec<cpuid::Leaf>,
    mmap_size: usize,
    tsc_deadline: bool,
    vcpus: u32,
    /// The MSRs a snapshot keeps: KVM's list (api.rst, KVM_GET_MSR_INDEX_LIST).
    msrs: Arc<Vec<u32>>,
    /// The guest's XSAVE area: KVM_CAP_XSAVE2's size, or 4096 before it.
    xsave_size: usize,
    /// Whether vCPUs can map memory ahead of the guest ([`Vcpu::pre_fault`]).
    pre_fault: bool,
    /// Whether each vCPU's TSC offset can be read and set (Linux 5.16), which snapshots
    /// then keep, so that every vCPU's TSC comes back in step with the others'.
    tsc_offsets: bool,
    /// The MSIs routed to GSIs past the IOAPIC's, as the guest programmed its
    /// passed-through functions' (D154).
    routes: Arc<Mutex<Vec<MsiRoute>>>,
    /// KVM's VFIO device, made as the first VFIO group is added ([`Vm::add_vfio_group`]).
    pub(super) vfio: OnceLock<sys::DeviceFd>,
}

/// An MSI's route: the GSI an irqfd raises it by, and its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MsiRoute {
    gsi: u32,
    address: u64,
    data: u32,
}

/// The GSIs of the in-kernel irqchip's pins: the IOAPIC's 24 (with the PICs' beside its
/// first 16), whose routes KVM makes by default; MSI routes take GSIs past them.
const IOAPIC_PINS: u32 = 24;

impl Vm {
    /// Creates the VM with the in-kernel irqchip (PIC, IOAPIC, LAPICs), before any vCPU
    /// (research doc §9 step 2). No PIT: with a HW-reduced FADT the guest never uses one.
    pub fn new(config: VmConfig) -> Result<Vm> {
        install_kick_handler()?;
        let kvm = kvm().map_err(Error::Guest)?;
        // Type 0, KVM_X86_DEFAULT_VM.
        let fd = Arc::new(kvm.create_vm(0).map_err(call("KVM_CREATE_VM"))?);
        fd.set_tss_addr(layout::TSS).map_err(call("KVM_SET_TSS_ADDR"))?;
        fd.set_identity_map_addr(layout::IDENTITY_MAP)
            .map_err(call("KVM_SET_IDENTITY_MAP_ADDR"))?;
        fd.create_irqchip().map_err(call("KVM_CREATE_IRQCHIP"))?;
        let mut cpuid: Vec<cpuid::Leaf> = kvm
            .supported_cpuid()
            .map_err(call("KVM_GET_SUPPORTED_CPUID"))?
            .into_iter()
            .map(|e| cpuid::Leaf {
                function: e.function,
                index: e.index,
                flags: e.flags,
                eax: e.eax,
                ebx: e.ebx,
                ecx: e.ecx,
                edx: e.edx,
            })
            .collect();
        cpuid::without_lone_tile_config(&mut cpuid);
        let tsc_deadline = kvm
            .check_extension(sys::CAP_TSC_DEADLINE_TIMER)
            .map_err(call("KVM_CHECK_EXTENSION"))?
            > 0;
        let msrs = kvm.msr_index_list().map_err(call("KVM_GET_MSR_INDEX_LIST"))?;
        let xsave2 = fd
            .check_extension(sys::CAP_XSAVE2)
            .map_err(call("KVM_CHECK_EXTENSION"))?;
        let pre_fault = fd
            .check_extension(sys::CAP_PRE_FAULT_MEMORY)
            .map_err(call("KVM_CHECK_EXTENSION"))?
            > 0;
        let tsc_offsets = fd
            .check_extension(sys::CAP_VCPU_ATTRIBUTES)
            .map_err(call("KVM_CHECK_EXTENSION"))?
            > 0;
        Ok(Vm {
            slots: Slots::new(fd.clone()),
            fd,
            cpuid,
            mmap_size: kvm.vcpu_mmap_size().map_err(call("KVM_GET_VCPU_MMAP_SIZE"))?,
            tsc_deadline,
            vcpus: config.vcpus,
            msrs: Arc::new(msrs),
            xsave_size: (xsave2.unsigned_abs() as usize).max(sys::XSAVE_SIZE),
            pre_fault,
            tsc_offsets,
            routes: Arc::new(Mutex::new(Vec::new())),
            vfio: OnceLock::new(),
        })
    }

    /// The in-kernel interrupt controllers and kvmclock. Every vCPU must be stopped.
    pub fn save_state(&self) -> Result<VmState> {
        let chip = |id| self.fd.get_irqchip(id).map_err(call("KVM_GET_IRQCHIP"));
        Ok(VmState {
            pic_master: chip(sys::IRQCHIP_PIC_MASTER)?,
            pic_slave: chip(sys::IRQCHIP_PIC_SLAVE)?,
            ioapic: chip(sys::IRQCHIP_IOAPIC)?,
            clock: self.fd.get_clock().map_err(call("KVM_GET_CLOCK"))?,
        })
    }

    /// Restores `st` once every vCPU exists and holds its own state: an IOAPIC restored
    /// earlier could deliver to LAPICs that are not there yet. kvmclock goes on from the
    /// value saved, so the guest's clocks do not jump by the time between.
    pub fn restore_state(&self, st: &VmState) -> Result<()> {
        self.fd.set_clock(&st.clock).map_err(call("KVM_SET_CLOCK"))?;
        for chip in [&st.pic_master, &st.pic_slave, &st.ioapic] {
            self.fd.set_irqchip(chip).map_err(call("KVM_SET_IRQCHIP"))?;
        }
        Ok(())
    }

    /// The guest's physical address width, as its CPUID says: no address it is given may
    /// be past it.
    pub fn phys_bits(&self) -> Result<u32> {
        Ok(cpuid::phys_bits(&self.cpuid))
    }

    /// A handle for driving interrupt lines from any thread.
    pub fn irqs(&self) -> Irqs {
        Irqs {
            fd: self.fd.clone(),
            routes: self.routes.clone(),
        }
    }

    /// Creates vCPU `index` on the calling thread, which runs it. Its CPUID has its own
    /// APIC id and a flat topology. Application processors start in wait-for-SIPI.
    pub fn create_vcpu(&self, index: usize) -> Result<Vcpu> {
        let fd = self
            .fd
            .create_vcpu(index as u64, self.mmap_size)
            .map_err(call("KVM_CREATE_VCPU"))?;
        let leaves: Vec<sys::kvm_cpuid_entry2> =
            cpuid::for_vcpu(&self.cpuid, index as u32, self.vcpus, self.tsc_deadline)
                .into_iter()
                .map(|l| sys::kvm_cpuid_entry2 {
                    function: l.function,
                    index: l.index,
                    flags: l.flags,
                    eax: l.eax,
                    ebx: l.ebx,
                    ecx: l.ecx,
                    edx: l.edx,
                    padding: [0; 3],
                })
                .collect();
        fd.set_cpuid(&leaves).map_err(call("KVM_SET_CPUID2"))?;
        let thread = super::this_thread();
        Ok(Vcpu {
            kicker: Kicker {
                run: fd.run.clone(),
                thread: thread.clone(),
            },
            fd,
            thread,
            cpuid: leaves
                .iter()
                .map(|e| [e.function, e.index, e.flags, e.eax, e.ebx, e.ecx, e.edx])
                .collect(),
            msrs: self.msrs.clone(),
            xsave_size: self.xsave_size,
            pre_fault: self.pre_fault,
            tsc_offsets: self.tsc_offsets,
            held_tsc: None,
        })
    }
}

/// A restored vCPU's TSC, for the thread that releases the VM: KVM's documented way to
/// bring a VM's TSCs back is each vCPU's offset (Documentation/virt/kvm/devices/vcpu.rst
/// §4, KVM_VCPU_TSC_OFFSET). It holds the vCPU weakly: once the vCPU is gone, there is
/// nothing to start.
#[derive(Debug, Clone)]
pub struct Tsc(sys::WeakVcpu);

/// Where a restored TSC starts: its offset and value in a vCPU's saved state, all a
/// release needs of it once the state is restored (audit D03).
#[derive(Debug, Clone, Copy)]
pub struct TscStart {
    offset: u64,
    value: u64,
}

impl VcpuState {
    /// Where this vCPU's TSC starts, if the snapshot kept it.
    pub fn tsc_start(&self) -> Option<TscStart> {
        let &(_, value) = self.msrs.iter().find(|&&(index, _)| index == MSR_IA32_TSC)?;
        Some(TscStart {
            offset: self.tsc_offset?,
            value,
        })
    }
}

impl Tsc {
    /// Starts this vCPU's TSC now at the value `start` saved, and returns what every vCPU
    /// adds to the offset it saved for it to stay as far from this one as it was
    /// ([`Vcpu::resume_tsc`]). The first TSC written in a VM is taken as given, since
    /// KVM matches a write to others only once one has been made. `None` once the vCPU
    /// has gone.
    pub fn restart(&self, start: TscStart) -> Result<Option<u64>> {
        let Some(fd) = self.0.upgrade() else {
            return Ok(None);
        };
        let TscStart { offset, value } = start;
        if fd
            .set_msrs(&[(MSR_IA32_TSC, value)])
            .map_err(call("KVM_SET_MSRS"))?
            != 1
        {
            return Err(Error::Guest(format!("KVM refused MSR {MSR_IA32_TSC:#x}")));
        }
        let now = fd.tsc_offset().map_err(call("KVM_GET_DEVICE_ATTR"))?;
        Ok(Some(now.wrapping_sub(offset)))
    }
}

/// Drives the in-kernel IOAPIC's pins (GSIs) from any thread, and the MSIs of the VM's
/// PCI functions.
#[derive(Debug, Clone)]
pub struct Irqs {
    fd: Arc<sys::VmFd>,
    routes: Arc<Mutex<Vec<MsiRoute>>>,
}

impl Irqs {
    pub fn set(&self, gsi: u32, level: bool) -> Result<()> {
        self.fd.irq_line(gsi, level).map_err(call("KVM_IRQ_LINE"))
    }

    /// One edge on an edge-triggered pin: KVM coalesces a re-assertion that finds the
    /// line still high, so it is raised and lowered (research doc §1.7).
    pub fn pulse(&self, gsi: u32) -> Result<()> {
        self.set(gsi, true)?;
        self.set(gsi, false)
    }

    /// Delivers the MSI of `address` and `data`, as a function the VMM emulates sends it.
    pub fn msi(&self, address: u64, data: u32) -> Result<()> {
        self.fd.signal_msi(address, data).map_err(call("KVM_SIGNAL_MSI"))
    }

    /// Raises `gsi` each time `event` is written, in the kernel (KVM_IRQFD); with
    /// `resample`, a level-triggered line, lowered as the guest ends it, `resample` written.
    pub fn irqfd(&self, event: RawFd, gsi: u32, resample: Option<RawFd>) -> Result<()> {
        self.fd.irqfd(event, gsi, resample).map_err(call("KVM_IRQFD"))
    }

    pub fn irqfd_remove(&self, event: RawFd, gsi: u32) -> Result<()> {
        self.fd.irqfd_remove(event, gsi).map_err(call("KVM_IRQFD"))
    }

    /// A GSI past the IOAPIC's that sends the MSI of `address` and `data`, for an irqfd to
    /// raise: the routing table is KVM's defaults and every MSI routed, set whole
    /// (api.rst 4.52, which replaces the table each time).
    pub fn route_msi(&self, address: u64, data: u32) -> Result<u32> {
        let mut routes = lock(&self.routes);
        let gsi = (IOAPIC_PINS..)
            .find(|gsi| routes.iter().all(|r| r.gsi != *gsi))
            .ok_or_else(|| Error::Guest("no GSI left for an MSI".into()))?;
        routes.push(MsiRoute { gsi, address, data });
        if let Err(e) = self.commit(&routes) {
            routes.pop();
            return Err(e);
        }
        Ok(gsi)
    }

    /// Takes the MSI route of `gsi` from the table.
    pub fn unroute_msi(&self, gsi: u32) -> Result<()> {
        let mut routes = lock(&self.routes);
        routes.retain(|r| r.gsi != gsi);
        self.commit(&routes)
    }

    fn commit(&self, routes: &[MsiRoute]) -> Result<()> {
        let irqchip = |gsi: u32, chip: u32, pin: u32| {
            let mut u = [0u32; 8];
            (u[0], u[1]) = (chip, pin);
            sys::kvm_irq_routing_entry {
                gsi,
                kind: sys::IRQ_ROUTING_IRQCHIP,
                u,
                ..Default::default()
            }
        };
        // KVM's defaults (arch/x86/kvm/irq.c, default_routing): each GSI to the IOAPIC's
        // pin, and the first 16 also to a PIC's, the master's below 8.
        let mut entries: Vec<sys::kvm_irq_routing_entry> = Vec::with_capacity(40 + routes.len());
        for gsi in 0..IOAPIC_PINS {
            entries.push(irqchip(gsi, sys::IRQCHIP_IOAPIC, gsi));
            if gsi < 16 {
                let pic = if gsi < 8 {
                    sys::IRQCHIP_PIC_MASTER
                } else {
                    sys::IRQCHIP_PIC_SLAVE
                };
                entries.push(irqchip(gsi, pic, gsi % 8));
            }
        }
        for r in routes {
            let mut u = [0u32; 8];
            (u[0], u[1], u[2]) = (r.address as u32, (r.address >> 32) as u32, r.data);
            entries.push(sys::kvm_irq_routing_entry {
                gsi: r.gsi,
                kind: sys::IRQ_ROUTING_MSI,
                u,
                ..Default::default()
            });
        }
        self.fd
            .set_gsi_routing(&entries)
            .map_err(call("KVM_SET_GSI_ROUTING"))
    }
}

fn kvm_segment(s: &Segment) -> sys::kvm_segment {
    sys::kvm_segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.kind,
        present: u8::from(s.present),
        dpl: 0,
        db: u8::from(s.db),
        s: u8::from(s.code_or_data),
        l: u8::from(s.long),
        g: u8::from(s.granular),
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

/// A vCPU, run by the thread that created it.
#[derive(Debug)]
pub struct Vcpu {
    fd: sys::VcpuFd,
    kicker: Kicker,
    /// Cleared on drop, so no kick can signal a thread that no longer runs this vCPU.
    pub(super) thread: Arc<Mutex<Option<Thread>>>,
    /// The CPUID it was given, as a snapshot keeps it.
    cpuid: Vec<[u32; 7]>,
    msrs: Arc<Vec<u32>>,
    xsave_size: usize,
    pre_fault: bool,
    tsc_offsets: bool,
    /// A restored TSC, held back until the vCPUs are released ([`Vcpu::resume_tsc`]).
    held_tsc: Option<HeldTsc>,
}

/// What a restore holds back of a vCPU's TSC: its offset in the snapshot, and the deadline
/// that must be armed against the TSC once it runs.
#[derive(Debug)]
struct HeldTsc {
    offset: u64,
    deadline: Option<u64>,
}

/// IA32_TSC_DEADLINE, restored after IA32_TSC: KVM arms the deadline against the TSC, so
/// in the other order a timer could fire at the wrong time or not at all (as Firecracker's
/// DEFERRED_MSRS notes; arch/x86/kvm/lapic.c).
const MSR_IA32_TSC_DEADLINE: u32 = 0x6e0;
const MSR_IA32_TSC: u32 = 0x10;

/// How far the TSC's frequency may differ from the snapshot's before a restore sets it:
/// 250 parts per million, QEMU's tolerance, which Firecracker keeps.
const TSC_KHZ_TOLERANCE_PPM: u64 = 250;

impl Vcpu {
    /// Captures the vCPU's state, in the order Firecracker's comments give: MP state first,
    /// since reading it may change the LAPIC's, and the events last. Call on the vCPU's
    /// thread, with the vCPU out of the guest and every other stopped.
    pub fn save_state(&self) -> Result<VcpuState> {
        let fd = &self.fd;
        let mp_state = fd.get_mp_state().map_err(call("KVM_GET_MP_STATE"))?;
        let get = |request: u64, len: usize, op| fd.get_state(request, len).map_err(call(op));
        let regs = get(sys::KVM_GET_REGS, size_of::<sys::kvm_regs>(), "KVM_GET_REGS")?;
        let sregs = get(sys::KVM_GET_SREGS, size_of::<sys::kvm_sregs>(), "KVM_GET_SREGS")?;
        let xsave = if self.xsave_size > sys::XSAVE_SIZE {
            get(sys::KVM_GET_XSAVE2, self.xsave_size, "KVM_GET_XSAVE2")?
        } else {
            get(sys::KVM_GET_XSAVE, sys::XSAVE_SIZE, "KVM_GET_XSAVE")?
        };
        let xcrs = get(sys::KVM_GET_XCRS, sys::XCRS_SIZE, "KVM_GET_XCRS")?;
        let debugregs = get(sys::KVM_GET_DEBUGREGS, sys::DEBUGREGS_SIZE, "KVM_GET_DEBUGREGS")?;
        let lapic = get(sys::KVM_GET_LAPIC, sys::LAPIC_SIZE, "KVM_GET_LAPIC")?;
        let tsc_khz = fd.tsc_khz().map_err(call("KVM_GET_TSC_KHZ"))?;
        let tsc_offset = if self.tsc_offsets {
            Some(fd.tsc_offset().map_err(call("KVM_GET_DEVICE_ATTR"))?)
        } else {
            None
        };
        let msrs = self.save_msrs()?;
        let events = get(
            sys::KVM_GET_VCPU_EVENTS,
            sys::VCPU_EVENTS_SIZE,
            "KVM_GET_VCPU_EVENTS",
        )?;
        Ok(VcpuState {
            cpuid: self.cpuid.clone(),
            mp_state,
            regs,
            sregs,
            xsave,
            xcrs,
            debugregs,
            lapic,
            msrs,
            events,
            tsc_khz,
            tsc_offset,
        })
    }

    fn set_msrs(&self, msrs: &[(u32, u64)]) -> Result<()> {
        let set = self.fd.set_msrs(msrs).map_err(call("KVM_SET_MSRS"))?;
        match msrs.get(set) {
            Some(&(index, _)) => Err(Error::Guest(format!("KVM refused MSR {index:#x}"))),
            None => Ok(()),
        }
    }

    /// The handle through which a restore starts this vCPU's TSC ([`Tsc::restart`]):
    /// `Some` if [`restore_state`](Self::restore_state) held the TSC back. It does not
    /// keep the vCPU.
    pub fn tsc(&self) -> Option<Tsc> {
        self.held_tsc.as_ref().map(|_| Tsc(self.fd.weak()))
    }

    /// Brings the TSC [`restore_state`](Self::restore_state) held back in step with the
    /// vCPU [`Tsc::restart`] started: its offset in the snapshot plus `delta`, which
    /// keeps every vCPU as far from the others as it was, then arms its deadline. On the
    /// vCPU's thread, before it runs.
    pub fn resume_tsc(&mut self, delta: u64) -> Result<()> {
        let Some(held) = self.held_tsc.take() else {
            return Ok(());
        };
        self.fd
            .set_tsc_offset(held.offset.wrapping_add(delta))
            .map_err(call("KVM_SET_DEVICE_ATTR"))?;
        match held.deadline {
            Some(deadline) => self.set_msrs(&[(MSR_IA32_TSC_DEADLINE, deadline)]),
            None => Ok(()),
        }
    }

    /// Every MSR in KVM's list that it reads for this vCPU. KVM stops at the first it
    /// cannot read, such as a PMU MSR of a guest without a PMU; that one is left out.
    fn save_msrs(&self) -> Result<Vec<(u32, u64)>> {
        let mut saved = Vec::with_capacity(self.msrs.len());
        let mut rest: &[u32] = &self.msrs;
        while !rest.is_empty() {
            let read = self.fd.get_msrs(rest).map_err(call("KVM_GET_MSRS"))?;
            let skip = read.len() + 1;
            saved.extend(read);
            rest = rest.get(skip..).unwrap_or_default();
        }
        Ok(saved)
    }

    /// Loads `st` into this new vCPU, in the order Firecracker's comments give: CPUID
    /// first, the registers before the events (SET_REGS drops a pending exception, the
    /// events bring it back), the LAPIC after the SREGS (which hold its base) and before
    /// the MSRs (the TSC deadline needs it). A vCPU given another CPUID refuses: the
    /// snapshot was taken on another CPU.
    ///
    /// Where the snapshot kept the TSC's offset and KVM can set it, the TSC and its
    /// deadline wait for the release ([`resume_tsc`](Self::resume_tsc)): each vCPU's TSC
    /// written on its own would pass through KVM's legacy synchronization, which matches
    /// the writes to one another only on a host whose TSC it trusts, and otherwise leaves
    /// each vCPU's TSC where its own capture found it, apart from the others' by however
    /// far apart the captures were (arch/x86/kvm/x86.c, kvm_synchronize_tsc).
    pub fn restore_state(&mut self, st: &VcpuState) -> Result<()> {
        if st.cpuid != self.cpuid {
            return Err(Error::Guest(
                "this CPU is not the one the snapshot was taken on (its CPUID differs)".into(),
            ));
        }
        if st.xsave.len() < self.xsave_size {
            return Err(Error::Guest(format!(
                "the snapshot's XSAVE area is {} bytes; this CPU's is {}",
                st.xsave.len(),
                self.xsave_size
            )));
        }
        let fd = &self.fd;
        let set =
            |request: u64, state: &[u8], len: usize, op| fd.set_state(request, state, len).map_err(call(op));
        fd.set_mp_state(st.mp_state).map_err(call("KVM_SET_MP_STATE"))?;
        set(
            sys::KVM_SET_REGS,
            &st.regs,
            size_of::<sys::kvm_regs>(),
            "KVM_SET_REGS",
        )?;
        set(
            sys::KVM_SET_SREGS,
            &st.sregs,
            size_of::<sys::kvm_sregs>(),
            "KVM_SET_SREGS",
        )?;
        set(sys::KVM_SET_XSAVE, &st.xsave, sys::XSAVE_SIZE, "KVM_SET_XSAVE")?;
        set(sys::KVM_SET_XCRS, &st.xcrs, sys::XCRS_SIZE, "KVM_SET_XCRS")?;
        set(
            sys::KVM_SET_DEBUGREGS,
            &st.debugregs,
            sys::DEBUGREGS_SIZE,
            "KVM_SET_DEBUGREGS",
        )?;
        set(sys::KVM_SET_LAPIC, &st.lapic, sys::LAPIC_SIZE, "KVM_SET_LAPIC")?;
        let khz = fd.tsc_khz().map_err(call("KVM_GET_TSC_KHZ"))?;
        if u64::from(khz.abs_diff(st.tsc_khz)) * 1_000_000 > u64::from(st.tsc_khz) * TSC_KHZ_TOLERANCE_PPM {
            fd.set_tsc_khz(st.tsc_khz).map_err(call("KVM_SET_TSC_KHZ"))?;
        }
        type Msrs = Vec<(u32, u64)>;
        let (deadline, rest): (Msrs, Msrs) = st
            .msrs
            .iter()
            .partition(|&&(index, _)| index == MSR_IA32_TSC_DEADLINE);
        let held = st.tsc_offset.filter(|_| self.tsc_offsets);
        let rest: Msrs = match held {
            Some(_) => rest
                .into_iter()
                .filter(|&(index, _)| index != MSR_IA32_TSC)
                .collect(),
            None => rest,
        };
        self.set_msrs(&rest)?;
        match held {
            Some(offset) => {
                self.held_tsc = Some(HeldTsc {
                    offset,
                    deadline: deadline.first().map(|&(_, value)| value),
                });
            }
            None => self.set_msrs(&deadline)?,
        }
        set(
            sys::KVM_SET_VCPU_EVENTS,
            &st.events,
            sys::VCPU_EVENTS_SIZE,
            "KVM_SET_VCPU_EVENTS",
        )?;
        // Fails, harmlessly, for a guest that never enabled kvmclock.
        let _ = fd.kvmclock_ctrl();
        Ok(())
    }

    /// Maps `len` bytes of guest memory at `gpa` into the stage-2 tables now, so that the
    /// guest finds them there instead of faulting on each page. KVM maps them as a read
    /// fault would: writable where the host page is writable already (a copy made ahead,
    /// kvm_main.c hva_to_pfn_fast), read-only where a write would still copy it (api.rst
    /// 4.143). `Ok(false)` where KVM cannot: before Linux 6.10, or without two-dimensional
    /// paging. Called after [`restore_state`](Self::restore_state), so that it maps for
    /// the state the guest runs in.
    pub fn pre_fault(&self, gpa: u64, len: u64) -> Result<bool> {
        if !self.pre_fault {
            return Ok(false);
        }
        let mut range = sys::kvm_pre_fault_memory {
            gpa,
            size: len,
            ..Default::default()
        };
        while range.size > 0 {
            match self.fd.pre_fault_memory(&mut range) {
                // Each success maps at least a page, and advances `range` past it.
                Ok(()) => {}
                // A signal came first, and was delivered on the way out.
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => return Ok(false),
                Err(e) => return Err(call("KVM_PRE_FAULT_MEMORY")(e)),
            }
        }
        Ok(true)
    }

    pub fn kicker(&self) -> Kicker {
        self.kicker.clone()
    }

    /// The boot vCPU's registers for the 64-bit boot protocol. Every field is written, so
    /// nothing of KVM's reset state (CR0.CD|NW among it) is inherited (research §2.5).
    pub fn boot(&mut self, b: &Boot) -> Result<()> {
        let mut s = self.fd.get_sregs().map_err(call("KVM_GET_SREGS"))?;
        s.cs = kvm_segment(&b.cs);
        let data = kvm_segment(&b.data);
        (s.ds, s.es, s.fs, s.gs, s.ss) = (data, data, data, data, data);
        s.tr = kvm_segment(&b.tr);
        s.gdt = sys::kvm_dtable {
            base: b.gdt.0,
            limit: b.gdt.1,
            padding: [0; 3],
        };
        s.idt = sys::kvm_dtable {
            base: b.idt.0,
            limit: b.idt.1,
            padding: [0; 3],
        };
        (s.cr0, s.cr3, s.cr4, s.efer) = (b.cr0, b.cr3, b.cr4, b.efer);
        self.fd.set_sregs(&s).map_err(call("KVM_SET_SREGS"))?;
        self.fd
            .set_regs(&sys::kvm_regs {
                rip: b.rip,
                rsi: b.rsi,
                rflags: 0x2,
                ..sys::kvm_regs::default()
            })
            .map_err(call("KVM_SET_REGS"))
    }

    /// Runs the guest until a kick, power-off or reset; device accesses complete through
    /// `io` on the way.
    pub fn run(&mut self, io: &dyn Io) -> Result<Exit> {
        super::run(&mut self.fd, io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guests are permitted AMX's tile data exactly where the host has it, once the
    /// process asks (review 1.10); where Linux knows no such request, nothing is asked.
    #[test]
    fn guests_are_permitted_amx_where_the_host_has_it() {
        const ARCH_GET_XCOMP_GUEST_PERM: libc::c_int = 0x1024;
        permit_amx();
        let tiles = 1 << ARCH_XCOMP_TILEDATA;
        match (xcomp(ARCH_GET_XCOMP_SUPP), xcomp(ARCH_GET_XCOMP_GUEST_PERM)) {
            (Some(supported), Some(permitted)) => assert_eq!(supported & tiles, permitted & tiles),
            (supported, permitted) => assert!(
                supported.is_none_or(|s| s & tiles == 0),
                "AMX supported, guest permission unknown: {supported:?} {permitted:?}"
            ),
        }
    }
}
