//! The KVM backend (Linux on x86_64 and arm64), with the contract every backend presents
//! (hv/mod.rs): device accesses complete inside `Vcpu::run` through [`Io`], and `run`
//! returns only a kick, power-off or reset. What is KVM's on every architecture is here:
//! /dev/kvm, guest memory's slots, the run loop and the kick. Each architecture's VM and
//! vCPUs are its own module's: x86_64's (ground truth:
//! docs/research/kvm-x86_64-ground-truth.md) and arm64's.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod state;
mod sys;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
#[cfg(target_arch = "x86_64")]
pub use state::{VcpuState, VmState};
pub use sys::IOCTLS;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Exit, Io};
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

/// What KVM must offer on every architecture: IMMEDIATE_EXIT backs the kick (research doc
/// §9 step 1), and irqfds, with resampling, carry a passed-through device's interrupts
/// (D154); each architecture's module adds its own ([`REQUIRED`]).
const COMMON: [(u64, &str); 7] = [
    (sys::CAP_IRQCHIP, "KVM_CAP_IRQCHIP"),
    (sys::CAP_READONLY_MEM, "KVM_CAP_READONLY_MEM"),
    (sys::CAP_USER_MEMORY, "KVM_CAP_USER_MEMORY"),
    (sys::CAP_MP_STATE, "KVM_CAP_MP_STATE"),
    (sys::CAP_IMMEDIATE_EXIT, "KVM_CAP_IMMEDIATE_EXIT"),
    (sys::CAP_IRQFD, "KVM_CAP_IRQFD"),
    (sys::CAP_IRQFD_RESAMPLE, "KVM_CAP_IRQFD_RESAMPLE"),
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
    for &(cap, name) in COMMON.iter().chain(REQUIRED) {
        if kvm
            .check_extension(cap)
            .map_err(|e| format!("KVM_CHECK_EXTENSION: {e}"))?
            <= 0
        {
            return Err(format!("this host's KVM lacks {name}"));
        }
    }
    prepare();
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

/// Whether this host sets a vCPU's state back as a restore does: KVM sets every vCPU's.
pub fn restores() -> bool {
    true
}

/// The most vCPUs one VM can have: KVM's limit, and the architecture's machine's
/// ([`VCPU_LIMIT`]).
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
    Ok(max.unsigned_abs().min(VCPU_LIMIT))
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

/// What maps the memory a virtio-mem device plugs into the guest ([`Vm::add_hotplug`]):
/// nothing, on KVM, whose stage 2 follows the host's mapping of it. The host withholds a
/// block's pages and leaves it inaccessible (`platform::withhold`), and KVM's MMU notifiers
/// unmap it from the guest; a guest touching it then fails its vCPU's run (EFAULT), as it
/// promised not to (VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE).
#[derive(Debug, Clone, Copy)]
pub struct Stage2(());

impl Stage2 {
    /// Maps nothing: the memslot covers the memory, and the host's mapping is the guest's.
    ///
    /// # Safety
    /// As every backend's: `host..host+len` must stay mapped in this process until
    /// unmapped here or the VM is destroyed.
    pub unsafe fn map(&self, _host: *mut u8, _gpa: u64, _len: usize) -> Result<()> {
        Ok(())
    }

    /// Maps nothing, as [`map`](Self::map): a file's mapping in a share's DAX window (D189)
    /// is the guest's as the host maps it, writable where that is.
    ///
    /// # Safety
    /// As for [`map`](Self::map).
    pub unsafe fn map_file(&self, _host: *mut u8, _gpa: u64, _len: usize, _writable: bool) -> Result<()> {
        Ok(())
    }

    /// Unmaps nothing: the host withholding the memory takes it from the guest.
    pub fn unmap(&self, _gpa: u64, _len: usize) -> Result<()> {
        Ok(())
    }
}

/// A VM's memory slots, numbered as they are made.
#[derive(Debug)]
struct Slots {
    fd: Arc<sys::VmFd>,
    next: AtomicU32,
}

impl Slots {
    fn new(fd: Arc<sys::VmFd>) -> Slots {
        Slots {
            fd,
            next: AtomicU32::new(0),
        }
    }

    /// Maps `len` bytes of host memory at `host` at `gpa`, in a new slot, read-only to the
    /// guest where `readonly`: its writes there exit as MMIO writes to nothing.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until the VM is destroyed.
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize, readonly: bool) -> Result<()> {
        let region = sys::kvm_userspace_memory_region {
            slot: self.next.fetch_add(1, Ordering::Relaxed),
            flags: if readonly { sys::MEM_READONLY } else { 0 },
            guest_phys_addr: gpa,
            memory_size: len as u64,
            userspace_addr: host as u64,
        };
        // SAFETY: forwarded caller contract.
        unsafe { self.fd.set_user_memory_region(&region) }.map_err(call("KVM_SET_USER_MEMORY_REGION"))
    }
}

/// Memory slots of a device's own (D154): a passed-through function's BARs, mapped into the
/// guest while its memory decoding is on, which the guest then reaches without an exit.
#[derive(Debug)]
pub struct DeviceSlots {
    fd: Arc<sys::VmFd>,
    first: u32,
    count: u32,
}

impl DeviceSlots {
    fn slot(&self, i: u32) -> Result<u32> {
        (i < self.count)
            .then_some(self.first + i)
            .ok_or_else(|| Error::Guest(format!("a device has {} memory slots, not {}", self.count, i + 1)))
    }

    /// Maps `len` bytes of host memory at `host` at `gpa`, as slot `i` of these, read-only
    /// to the guest unless `writable`: its writes there exit as MMIO writes to nothing.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until slot `i` is unmapped or the VM is destroyed.
    pub unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize, writable: bool) -> Result<()> {
        let region = sys::kvm_userspace_memory_region {
            slot: self.slot(i)?,
            flags: if writable { 0 } else { sys::MEM_READONLY },
            guest_phys_addr: gpa,
            memory_size: len as u64,
            userspace_addr: host as u64,
        };
        // SAFETY: forwarded caller contract.
        unsafe { self.fd.set_user_memory_region(&region) }.map_err(call("KVM_SET_USER_MEMORY_REGION"))
    }

    /// Unmaps slot `i`, at `gpa` for `len` bytes as it was mapped: the guest reaches its
    /// addresses as MMIO again.
    pub fn unmap(&self, i: u32, _gpa: u64, _len: usize) -> Result<()> {
        self.fd
            .delete_user_memory_region(self.slot(i)?)
            .map_err(call("KVM_SET_USER_MEMORY_REGION"))
    }
}

impl Vm {
    /// Tells KVM of VFIO's file `file`, a group's or a device's opened by its node, through
    /// KVM's VFIO device (Documentation/virt/kvm/devices/vfio.rst, KVM_DEV_VFIO_FILE_ADD),
    /// before the device is opened or bound, as drivers that need the VM to open a device
    /// ask. The device is made with the first file and kept as long as the VM: KVM forgets
    /// the files as its descriptor closes (virt/kvm/vfio.c, kvm_vfio_release).
    pub fn add_vfio_file(&self, file: std::os::fd::RawFd) -> Result<()> {
        let device = match self.vfio.get() {
            Some(device) => device,
            None => {
                let device = self
                    .slots
                    .fd
                    .create_device(sys::DEV_TYPE_VFIO)
                    .map_err(call("KVM_CREATE_DEVICE"))?;
                self.vfio.get_or_init(|| device)
            }
        };
        device
            .set_attr(sys::DEV_VFIO_FILE, sys::DEV_VFIO_FILE_ADD, &file)
            .map_err(call("KVM_SET_DEVICE_ATTR"))
    }

    /// `count` memory slots of a device's own.
    pub fn device_slots(&self, count: u32) -> DeviceSlots {
        DeviceSlots {
            fd: self.slots.fd.clone(),
            first: self.slots.next.fetch_add(count, Ordering::Relaxed),
            count,
        }
    }

    /// Maps `len` bytes of host memory at `host` as guest RAM at `gpa`, in a new memslot.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until the VM is destroyed.
    pub unsafe fn map_ram(&self, host: *mut u8, gpa: u64, len: usize) -> Result<()> {
        // SAFETY: forwarded caller contract.
        unsafe { self.slots.map(host, gpa, len, false) }
    }

    /// What takes guest RAM from the guest and gives it back, from whichever thread gives
    /// its pages back to the host as the guest reports them free (D151).
    pub fn stage2(&self) -> Stage2 {
        Stage2(())
    }

    /// Gives the guest the memory a virtio-mem device plugs, `len` bytes at `host` seen at
    /// `gpa` (D133), as one memslot: KVM's stage 2 follows the host's mapping of it, through
    /// its MMU notifiers (Documentation/virt/kvm/mmu.rst), so what the host withholds the
    /// guest cannot reach, and the [`Stage2`] this returns has nothing to map.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped in this process until the VM is destroyed.
    pub unsafe fn add_hotplug(&self, host: *mut u8, gpa: u64, len: usize) -> Result<Stage2> {
        // SAFETY: forwarded caller contract.
        unsafe { self.map_ram(host, gpa, len) }?;
        Ok(Stage2(()))
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
        // SAFETY: forwarded caller contract.
        unsafe { self.slots.map(host, gpa, len, !writable) }
    }
}

/// Runs the vCPU of `fd` until a kick, power-off or reset; device accesses complete
/// through `io` on the way.
fn run(fd: &mut sys::VcpuFd, io: &dyn Io) -> Result<Exit> {
    loop {
        match fd.run().map_err(call("KVM_RUN"))? {
            sys::RunExit::Interrupted => {
                fd.clear_immediate_exit();
                return Ok(Exit::Canceled);
            }
            sys::RunExit::Again => {}
            // Memory the host could not give: given now where it is the VMM's to give (a
            // share's DAX window's range whose file another process cut short, D189), and
            // the access runs again; else the guest touched what it promised not to (memory
            // withheld). x86's KVM says so only with the guest's interrupts off, and
            // otherwise retries the access itself until the window gives it (PM M273).
            sys::RunExit::Fault => {
                let given = io.memory_fault(None);
                crate::debug!("KVM_RUN: EFAULT; memory given: {given}");
                if !given {
                    return Err(call("KVM_RUN")(io::Error::from_raw_os_error(libc::EFAULT)));
                }
            }
            // String I/O arrives as `count` elements of `size` bytes.
            sys::RunExit::IoIn { port, size, data } => {
                data.chunks_mut(size.max(1)).for_each(|c| io.pio_read(port, c));
            }
            sys::RunExit::IoOut { port, size, data } => {
                data.chunks(size.max(1)).for_each(|c| io.pio_write(port, c));
            }
            sys::RunExit::MmioRead { addr, data } => io.mmio_read(addr, data),
            sys::RunExit::MmioWrite { addr, data } => io.mmio_write(addr, data),
            // An x86 triple fault: the guest crashed.
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
            // A data abort KVM cannot emulate, its syndrome naming no access (api.rst
            // 7.20): a cache maintenance instruction on read-only memory not yet mapped at
            // stage 2, as a guest's I/D cache sync of a page of its DAX root, mapped for
            // execution (arm64 mm/flush.c, __sync_icache_dcache), makes where its CPU
            // has neither CTR_EL0.IDC nor DIC; or an access to no memory by an
            // instruction whose syndrome names no register (a pair, a writeback, SIMD).
            #[cfg(target_arch = "aarch64")]
            sys::RunExit::ArmNisv { esr_iss, fault_ipa } => {
                if esr_iss & ESR_ISS_CM != 0 {
                    skip_cmo(fd, fault_ipa)?;
                    continue;
                }
                // What an access to nothing does on hardware, an external abort, as
                // QEMU gives one (target/arm/kvm.c, kvm_arm_handle_dabt_nisv).
                if fd.inject_ext_dabt().is_ok() {
                    continue;
                }
                let pc = fd
                    .get_reg(sys::REG_PC)
                    .map_or_else(|e| format!("its PC unread: {e}"), |pc| format!("at PC {pc:#x}"));
                return Err(Error::Guest(format!(
                    "the guest reached {fault_ipa:#x}, outside its memory, by an access KVM cannot emulate, {pc} (ESR_EL2 ISS {esr_iss:#x}, no syndrome: KVM_EXIT_ARM_NISV)"
                )));
            }

            sys::RunExit::Other(reason) => {
                return Err(Error::Guest(format!("unexpected KVM exit {reason}")));
            }
        }
    }
}

/// ESR_ELx's CM bit: a fault by a cache maintenance instruction (Arm ARM D24.2.40, ISS
/// encoding for an exception from a Data Abort).
#[cfg(target_arch = "aarch64")]
const ESR_ISS_CM: u64 = 1 << 8;

/// The cache maintenance instructions skipped so far, for the log.
#[cfg(target_arch = "aarch64")]
static CMOS_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Skips the cache maintenance instruction the vCPU stopped at, as KVM skips one outside
/// guest memory (arch/arm64/kvm/mmu.c, kvm_handle_guest_abort; "Handle out-of-RAM cache
/// maintenance as a NOP"): KVM takes one on read-only memory not yet mapped at stage 2
/// for a write (its WnR is 1), which the memory refuses, and cannot emulate it. Skipped,
/// it changes nothing the guest could see: the guest cannot have written memory it may
/// not write, and KVM cleans and invalidates what the host wrote there as it maps a page
/// for the guest. As kvm_skip_instr: past the instruction, PSTATE's BTYPE and SS cleared.
#[cfg(target_arch = "aarch64")]
fn skip_cmo(fd: &mut sys::VcpuFd, fault_ipa: u64) -> Result<()> {
    const BTYPE: u64 = 0b11 << 10;
    const SS: u64 = 1 << 21;
    let pc = fd.get_reg(sys::REG_PC).map_err(call("KVM_GET_ONE_REG"))?;
    let pstate = fd.get_reg(sys::REG_PSTATE).map_err(call("KVM_GET_ONE_REG"))?;
    fd.set_reg(sys::REG_PC, pc.wrapping_add(4))
        .map_err(call("KVM_SET_ONE_REG"))?;
    fd.set_reg(sys::REG_PSTATE, pstate & !(BTYPE | SS))
        .map_err(call("KVM_SET_ONE_REG"))?;
    let n = CMOS_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        crate::debug!("cache maintenance skipped: {n} so far, the last at {fault_ipa:#x}, PC {pc:#x}");
    }
    Ok(())
}

/// A thread, as tgkill(2) names it: its process and its own ID.
#[derive(Debug, Clone, Copy)]
struct Thread {
    tgid: libc::pid_t,
    tid: libc::pid_t,
}

/// The calling thread, which creates, runs and drops a vCPU: cleared as the vCPU drops,
/// so that no kick signals a thread that no longer runs it.
fn this_thread() -> Arc<Mutex<Option<Thread>>> {
    Arc::new(Mutex::new(Some(Thread {
        // SAFETY: getpid(2) and gettid(2) have no preconditions.
        tgid: unsafe { libc::getpid() },
        // SAFETY: as above.
        tid: unsafe { libc::gettid() },
    })))
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

impl Drop for Vcpu {
    fn drop(&mut self) {
        *lock(&self.thread) = None;
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
