//! The KVM backend (Linux on x86_64), with the contract every backend presents
//! (hv/mod.rs): device accesses complete inside [`Vcpu::run`] through [`Io`], and `run`
//! returns only a kick, power-off or reset. On x86 the guest powers off and resets
//! through devices (ACPI sleep control, i8042), so those come from the machine, and a
//! triple fault is a reset. Ground truth: docs/research/kvm-x86_64-ground-truth.md.

mod state;
mod sys;

pub use state::{VcpuState, VmState};
pub use sys::IOCTLS;

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Exit, Io};
use crate::arch::x86_64::{Boot, Segment, cpuid, layout};
use crate::sync::lock;

/// A failed KVM call, or guest behavior the VMM does not emulate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Call { op: &'static str, error: String },
    Guest(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Call { op, error } => write!(f, "{op} failed: {error}"),
            Error::Guest(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn call(op: &'static str) -> impl Fn(io::Error) -> Error {
    move |e| Error::Call {
        op,
        error: e.to_string(),
    }
}

/// What KVM must offer (research doc §9 step 1). IMMEDIATE_EXIT backs the kick.
const REQUIRED: [(u64, &str); 8] = [
    (sys::CAP_IRQCHIP, "KVM_CAP_IRQCHIP"),
    (sys::CAP_READONLY_MEM, "KVM_CAP_READONLY_MEM"),
    (sys::CAP_USER_MEMORY, "KVM_CAP_USER_MEMORY"),
    (sys::CAP_SET_TSS_ADDR, "KVM_CAP_SET_TSS_ADDR"),
    (sys::CAP_EXT_CPUID, "KVM_CAP_EXT_CPUID"),
    (sys::CAP_MP_STATE, "KVM_CAP_MP_STATE"),
    (sys::CAP_SET_IDENTITY_MAP_ADDR, "KVM_CAP_SET_IDENTITY_MAP_ADDR"),
    (sys::CAP_IMMEDIATE_EXIT, "KVM_CAP_IMMEDIATE_EXIT"),
];

fn open() -> std::result::Result<sys::Kvm, String> {
    let kvm = sys::Kvm::open().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            "KVM is unavailable on this host: /dev/kvm does not exist (no hardware virtualization, or the kvm module is not loaded)".to_string()
        }
        io::ErrorKind::PermissionDenied => {
            "cannot open /dev/kvm: permission denied (add this user to the `kvm` group)".to_string()
        }
        _ => format!("cannot open /dev/kvm: {e}"),
    })?;
    let version = kvm
        .api_version()
        .map_err(|e| format!("KVM_GET_API_VERSION: {e}"))?;
    if version != sys::API_VERSION {
        return Err(format!(
            "KVM API version {version}; shards needs {}",
            sys::API_VERSION
        ));
    }
    for (cap, name) in REQUIRED {
        if kvm
            .check_extension(cap)
            .map_err(|e| format!("KVM_CHECK_EXTENSION: {e}"))?
            <= 0
        {
            return Err(format!("this host's KVM lacks {name}"));
        }
    }
    Ok(kvm)
}

/// /dev/kvm, opened and checked once a process (review 1.8): a start asked whether the
/// host runs VMs, how many vCPUs one may have, and made its VM, each opening it and
/// asking its API version and capabilities again. Only a handle that passed is kept: a
/// host whose KVM is missing now is looked at again next time.
fn kvm() -> std::result::Result<&'static sys::Kvm, String> {
    static KVM: OnceLock<sys::Kvm> = OnceLock::new();
    if let Some(kvm) = KVM.get() {
        return Ok(kvm);
    }
    let opened = open()?;
    Ok(KVM.get_or_init(|| opened))
}

/// Ok when this host can run VMs: /dev/kvm opens and offers what shards needs.
pub fn check_host() -> std::result::Result<(), String> {
    kvm().map(drop)
}

/// The most vCPUs one VM can have: KVM's limit, and 254, since the MADT carries 8-bit
/// APIC ids (research doc §3.5).
pub fn max_vcpus() -> Result<u32> {
    let kvm = kvm().map_err(Error::Guest)?;
    let max = kvm
        .check_extension(sys::CAP_MAX_VCPUS)
        .map_err(call("KVM_CHECK_EXTENSION"))?;
    let max = if max > 0 {
        max
    } else {
        kvm.check_extension(sys::CAP_NR_VCPUS)
            .map_err(call("KVM_CHECK_EXTENSION"))?
    };
    Ok(max.unsigned_abs().min(254))
}

/// Installs the process's kick signal handler: a no-op, and no SA_RESTART, so a kick
/// makes KVM_RUN return EINTR. The kicker sets `immediate_exit` itself (§1.6).
fn install_kick_handler() -> Result<()> {
    static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    extern "C" fn on_kick(_: libc::c_int) {}
    INSTALLED
        .get_or_init(|| {
            // SAFETY: a zeroed sigaction with a valid handler and an empty mask.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_kick as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut sa.sa_mask);
                if libc::sigaction(libc::SIGRTMIN(), &sa, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
            }
            Ok(())
        })
        .clone()
        .map_err(|error| Error::Call {
            op: "sigaction(SIGRTMIN)",
            error,
        })
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub vcpus: u32,
}

/// The VM. It must outlive every [`Vcpu`], and guest memory must outlive it.
#[derive(Debug)]
pub struct Vm {
    fd: Arc<sys::VmFd>,
    cpuid: Vec<cpuid::Leaf>,
    mmap_size: usize,
    tsc_deadline: bool,
    vcpus: u32,
    next_slot: AtomicU32,
    /// The MSRs a snapshot keeps: KVM's list (api.rst, KVM_GET_MSR_INDEX_LIST).
    msrs: Arc<Vec<u32>>,
    /// The guest's XSAVE area: KVM_CAP_XSAVE2's size, or 4096 before it.
    xsave_size: usize,
    /// Whether vCPUs can map memory ahead of the guest ([`Vcpu::pre_fault`]).
    pre_fault: bool,
    /// Whether each vCPU's TSC offset can be read and set (Linux 5.16), which snapshots
    /// then keep, so that every vCPU's TSC comes back in step with the others'.
    tsc_offsets: bool,
}

impl Vm {
    /// Creates the VM with the in-kernel irqchip (PIC, IOAPIC, LAPICs), before any vCPU
    /// (research doc §9 step 2). No PIT: with a HW-reduced FADT the guest never uses one.
    pub fn new(config: VmConfig) -> Result<Vm> {
        install_kick_handler()?;
        let kvm = kvm().map_err(Error::Guest)?;
        let fd = kvm.create_vm().map_err(call("KVM_CREATE_VM"))?;
        fd.set_tss_addr(layout::TSS).map_err(call("KVM_SET_TSS_ADDR"))?;
        fd.set_identity_map_addr(layout::IDENTITY_MAP)
            .map_err(call("KVM_SET_IDENTITY_MAP_ADDR"))?;
        fd.create_irqchip().map_err(call("KVM_CREATE_IRQCHIP"))?;
        let cpuid = kvm
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
            fd: Arc::new(fd),
            cpuid,
            mmap_size: kvm.vcpu_mmap_size().map_err(call("KVM_GET_VCPU_MMAP_SIZE"))?,
            tsc_deadline,
            vcpus: config.vcpus,
            next_slot: AtomicU32::new(0),
            msrs: Arc::new(msrs),
            xsave_size: (xsave2.unsigned_abs() as usize).max(sys::XSAVE_SIZE),
            pre_fault,
            tsc_offsets,
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

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`, in a new memslot.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until the VM is destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        let region = sys::kvm_userspace_memory_region {
            slot: self.next_slot.fetch_add(1, Ordering::Relaxed),
            flags: 0,
            guest_phys_addr: gpa,
            memory_size: len as u64,
            userspace_addr: host as u64,
        };
        // SAFETY: forwarded caller contract.
        unsafe { self.fd.set_user_memory_region(&region) }.map_err(call("KVM_SET_USER_MEMORY_REGION"))
    }

    /// Maps device memory (a virtio-pmem region) at `gpa`, writable by the guest only if
    /// `writable`. A guest write to a read-only slot exits as an MMIO write to nothing,
    /// and is dropped.
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
        let region = sys::kvm_userspace_memory_region {
            slot: self.next_slot.fetch_add(1, Ordering::Relaxed),
            flags: if writable { 0 } else { sys::MEM_READONLY },
            guest_phys_addr: gpa,
            memory_size: len as u64,
            userspace_addr: host as u64,
        };
        // SAFETY: forwarded caller contract.
        unsafe { self.fd.set_user_memory_region(&region) }.map_err(call("KVM_SET_USER_MEMORY_REGION"))
    }

    /// A handle for driving interrupt lines from any thread.
    pub fn irqs(&self) -> Irqs {
        Irqs(self.fd.clone())
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
        // This vCPU's thread: it creates, runs and drops the vCPU.
        let thread = Arc::new(Mutex::new(Some(Thread {
            // SAFETY: getpid(2) and gettid(2) have no preconditions.
            tgid: unsafe { libc::getpid() },
            // SAFETY: as above.
            tid: unsafe { libc::gettid() },
        })));
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

/// Drives the in-kernel IOAPIC's pins (GSIs) from any thread.
#[derive(Debug, Clone)]
pub struct Irqs(Arc<sys::VmFd>);

impl Irqs {
    pub fn set(&self, gsi: u32, level: bool) -> Result<()> {
        self.0.irq_line(gsi, level).map_err(call("KVM_IRQ_LINE"))
    }

    /// One edge on an edge-triggered pin: KVM coalesces a re-assertion that finds the
    /// line still high, so it is raised and lowered (research doc §1.7).
    pub fn pulse(&self, gsi: u32) -> Result<()> {
        self.set(gsi, true)?;
        self.set(gsi, false)
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
    thread: Arc<Mutex<Option<Thread>>>,
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
        loop {
            match self.fd.run().map_err(call("KVM_RUN"))? {
                sys::RunExit::Interrupted => {
                    self.fd.clear_immediate_exit();
                    return Ok(Exit::Canceled);
                }
                sys::RunExit::Again => {}
                // String I/O arrives as `count` elements of `size` bytes.
                sys::RunExit::IoIn { port, size, data } => {
                    data.chunks_mut(size.max(1)).for_each(|c| io.pio_read(port, c));
                }
                sys::RunExit::IoOut { port, size, data } => {
                    data.chunks(size.max(1)).for_each(|c| io.pio_write(port, c));
                }
                sys::RunExit::MmioRead { addr, data } => io.mmio_read(addr, data),
                sys::RunExit::MmioWrite { addr, data } => io.mmio_write(addr, data),
                // A triple fault: the guest crashed.
                sys::RunExit::Shutdown => return Ok(Exit::Reset),
                sys::RunExit::SystemEvent(sys::SYSTEM_EVENT_SHUTDOWN) => return Ok(Exit::Shutdown),
                sys::RunExit::SystemEvent(sys::SYSTEM_EVENT_RESET | sys::SYSTEM_EVENT_CRASH) => {
                    return Ok(Exit::Reset);
                }
                sys::RunExit::SystemEvent(t) => {
                    return Err(Error::Guest(format!("unexpected KVM system event {t}")));
                }
                sys::RunExit::Hlt => {
                    return Err(Error::Guest("HLT exit despite the in-kernel LAPIC".into()));
                }
                sys::RunExit::FailEntry(reason) => {
                    return Err(Error::Guest(format!(
                        "VM entry failed (hardware reason {reason:#x})"
                    )));
                }
                sys::RunExit::InternalError(suberror) => {
                    return Err(Error::Guest(format!("KVM internal error (suberror {suberror})")));
                }
                sys::RunExit::Other(reason) => {
                    return Err(Error::Guest(format!("unexpected KVM exit {reason}")));
                }
            }
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        *lock(&self.thread) = None;
    }
}

/// A thread, as tgkill(2) names it: its process and its own ID.
#[derive(Debug, Clone, Copy)]
struct Thread {
    tgid: libc::pid_t,
    tid: libc::pid_t,
}

/// Interrupts a vCPU's `run` from any thread (research doc §1.6): `immediate_exit`
/// covers a vCPU about to enter the guest, the signal one inside it.
#[derive(Debug, Clone)]
pub struct Kicker {
    run: Arc<sys::RunMap>,
    thread: Arc<Mutex<Option<Thread>>>,
}

impl Kicker {
    pub fn kick(&self) {
        let thread = lock(&self.thread);
        if let Some(Thread { tgid, tid }) = *thread {
            self.run.immediate_exit().store(1, Ordering::SeqCst);
            // tgkill(2) itself: musl's pthread_kill is tkill(2), which the VM process's
            // seccomp filter refuses, as it refuses a tgkill of any process but its own.
            // SAFETY: `tid` runs this vCPU until the Vcpu drops, which waits for this lock;
            // `tgid` keeps a reused ID in another process from being signalled.
            unsafe { libc::syscall(libc::SYS_tgkill, tgid, tid, libc::SIGRTMIN()) };
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// /dev/kvm is opened and checked once a process: the next start takes the same handle
    /// (review 1.8).
    #[test]
    fn kvm_is_opened_and_checked_once() {
        if let Err(e) = check_host() {
            eprintln!("SKIP: {e}");
            return;
        }
        let first: *const sys::Kvm = kvm().unwrap();
        let again: *const sys::Kvm = kvm().unwrap();
        assert_eq!(first, again);
        assert!(max_vcpus().unwrap() >= 1);
    }
}
