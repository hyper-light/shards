//! Thin wrappers over the KVM ioctls shards uses. Numbers and layouts are the uapi's
//! (include/uapi/linux/kvm.h and each architecture's asm/kvm.h), as probed for x86_64 in
//! docs/research/kvm-x86_64-ground-truth.md §1; sizes are checked at compile time. What
//! one architecture alone uses is in its own module.
#![allow(non_camel_case_types)]

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};

pub const API_VERSION: i32 = 12;

pub const KVM_GET_API_VERSION: u64 = 0xAE00;
pub const KVM_CREATE_VM: u64 = 0xAE01;
pub const KVM_CHECK_EXTENSION: u64 = 0xAE03;
pub const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xAE04;
pub const KVM_CREATE_VCPU: u64 = 0xAE41;
pub const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_AE46;
pub const KVM_IRQ_LINE: u64 = 0x4008_AE61;
pub const KVM_RUN: u64 = 0xAE80;
pub const KVM_GET_MP_STATE: u64 = 0x8004_AE98;
pub const KVM_SET_MP_STATE: u64 = 0x4004_AE99;
pub const KVM_SET_DEVICE_ATTR: u64 = 0x4018_AEE1;
pub const KVM_GET_DEVICE_ATTR: u64 = 0x4018_AEE2;
pub const KVM_IRQFD: u64 = 0x4020_AE76;
pub const KVM_CREATE_DEVICE: u64 = 0xC00C_AEE0;

/// Each structure is the size its ioctl numbers encode (bits 16-29).
const _: () = {
    const fn size(request: u64) -> usize {
        ((request >> 16) & 0x3fff) as usize
    }
    assert!(size(KVM_SET_USER_MEMORY_REGION) == size_of::<kvm_userspace_memory_region>());
    assert!(size(KVM_IRQ_LINE) == size_of::<kvm_irq_level>());
    assert!(size(KVM_GET_DEVICE_ATTR) == size_of::<kvm_device_attr>());
    assert!(size(KVM_SET_DEVICE_ATTR) == size_of::<kvm_device_attr>());
    assert!(size(KVM_IRQFD) == size_of::<kvm_irqfd>());
    assert!(size(KVM_CREATE_DEVICE) == size_of::<kvm_create_device>());
};

pub const CAP_IRQCHIP: u64 = 0;
pub const CAP_USER_MEMORY: u64 = 3;
pub const CAP_NR_VCPUS: u64 = 9;
pub const CAP_MP_STATE: u64 = 14;
pub const CAP_MAX_VCPUS: u64 = 66;
pub const CAP_IMMEDIATE_EXIT: u64 = 136;
pub const CAP_READONLY_MEM: u64 = 81;

/// A device attribute: `addr` is where its value is read from or written to.
#[repr(C)]
#[derive(Debug, Default)]
pub struct kvm_device_attr {
    pub flags: u32,
    pub group: u32,
    pub attr: u64,
    pub addr: u64,
}

/// An irqfd (api.rst 4.75, KVM_IRQFD): an eventfd whose writes raise a GSI in the kernel;
/// with a resample eventfd, a level-triggered one, lowered at the guest's EOI.
#[repr(C)]
#[derive(Debug, Default)]
pub struct kvm_irqfd {
    pub fd: u32,
    pub gsi: u32,
    pub flags: u32,
    pub resamplefd: u32,
    pub pad: [u8; 16],
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct kvm_create_device {
    pub kind: u32,
    pub fd: u32,
    pub flags: u32,
}

/// KVM's VFIO device (Documentation/virt/kvm/devices/vfio.rst): its type, and its group
/// of attributes that add a VFIO file (a group), whose value is the file's descriptor.
pub const DEV_TYPE_VFIO: u32 = 4;
pub const DEV_VFIO_FILE: u32 = 1;
pub const DEV_VFIO_FILE_ADD: u64 = 1;

pub const IRQFD_FLAG_DEASSIGN: u32 = 1 << 0;
pub const IRQFD_FLAG_RESAMPLE: u32 = 1 << 1;
pub const CAP_IRQFD: u64 = 32;
pub const CAP_IRQFD_RESAMPLE: u64 = 82;

/// kvm_userspace_memory_region flags.
pub const MEM_READONLY: u32 = 1 << 1;

pub const EXIT_IO: u32 = 2;
pub const EXIT_HLT: u32 = 5;
pub const EXIT_MMIO: u32 = 6;
pub const EXIT_SHUTDOWN: u32 = 8;
pub const EXIT_FAIL_ENTRY: u32 = 9;
pub const EXIT_INTR: u32 = 10;
pub const EXIT_INTERNAL_ERROR: u32 = 17;
pub const EXIT_SYSTEM_EVENT: u32 = 24;
/// A data abort KVM could not emulate (api.rst, KVM_EXIT_ARM_NISV): its syndrome's valid
/// bits and its IPA.
#[cfg(target_arch = "aarch64")]
pub const EXIT_ARM_NISV: u32 = 28;

pub const SYSTEM_EVENT_SHUTDOWN: u32 = 1;
pub const SYSTEM_EVENT_RESET: u32 = 2;
pub const SYSTEM_EVENT_CRASH: u32 = 3;

pub const IO_IN: u8 = 0;

/// `sizeof(struct kvm_run)`, the same on x86_64 and arm64: its exit union and synced
/// registers are padded to 256 and 2048 bytes (research doc §1.5).
const KVM_RUN_SIZE: usize = 2352;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_userspace_memory_region {
    pub slot: u32,
    pub flags: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_irq_level {
    pub irq: u32,
    pub level: u32,
}

const _: () = {
    assert!(size_of::<kvm_userspace_memory_region>() == 32);
    assert!(size_of::<kvm_irq_level>() == 8);
    assert!(size_of::<kvm_device_attr>() == 24);
    assert!(size_of::<kvm_irqfd>() == 32);
    assert!(size_of::<kvm_create_device>() == 12);
};

fn ret(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// # Safety
/// `arg` must be what `request` expects: a value, or a pointer to a live struct of the
/// request's type.
unsafe fn ioctl(fd: RawFd, request: u64, arg: libc::c_ulong) -> io::Result<libc::c_int> {
    // SAFETY: forwarded caller contract.
    ret(unsafe { libc::ioctl(fd, request as libc::Ioctl, arg) })
}

/// Sets attribute `attr` of `group` of the device or vCPU `fd` to the value at `value`, of
/// the attribute's type `T`.
fn set_attr<T>(fd: RawFd, group: u32, attr: u64, value: &T) -> io::Result<()> {
    let a = kvm_device_attr {
        group,
        attr,
        addr: (value as *const T) as u64,
        ..Default::default()
    };
    // SAFETY: a kvm_device_attr whose `addr` is a live `T`, which KVM reads.
    unsafe { ioctl(fd, KVM_SET_DEVICE_ATTR, (&raw const a) as libc::c_ulong) }.map(drop)
}

/// The system handle, `/dev/kvm`.
#[derive(Debug)]
pub struct Kvm(File);

impl Kvm {
    pub fn open() -> io::Result<Kvm> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map(Kvm)
    }

    pub fn api_version(&self) -> io::Result<i32> {
        // SAFETY: no argument.
        unsafe { ioctl(self.0.as_raw_fd(), KVM_GET_API_VERSION, 0) }
    }

    /// 0 when absent; some capabilities report a count.
    pub fn check_extension(&self, cap: u64) -> io::Result<i32> {
        // SAFETY: the capability number is passed by value.
        unsafe { ioctl(self.0.as_raw_fd(), KVM_CHECK_EXTENSION, cap as libc::c_ulong) }
    }

    pub fn vcpu_mmap_size(&self) -> io::Result<usize> {
        // SAFETY: no argument.
        let n = unsafe { ioctl(self.0.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0) }?;
        Ok(n.unsigned_abs() as usize)
    }

    /// A VM of machine type `kind`: 0 on x86_64 (KVM_X86_DEFAULT_VM); on arm64 its
    /// physical address width (KVM_VM_TYPE_ARM_IPA_SIZE).
    pub fn create_vm(&self, kind: u64) -> io::Result<VmFd> {
        // SAFETY: the type by value; the result is a new fd we own.
        let fd = unsafe { ioctl(self.0.as_raw_fd(), KVM_CREATE_VM, kind as libc::c_ulong) }?;
        // SAFETY: a fresh descriptor, owned from here on.
        Ok(VmFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

#[derive(Debug)]
pub struct VmFd(OwnedFd);

/// An in-kernel device (`KVM_CREATE_DEVICE`): arm64's GICv3, KVM's VFIO device.
#[derive(Debug)]
pub struct DeviceFd(OwnedFd);

impl DeviceFd {
    /// Sets attribute `attr` of `group` to `value`, of the attribute's own type.
    pub fn set_attr<T>(&self, group: u32, attr: u64, value: &T) -> io::Result<()> {
        set_attr(self.0.as_raw_fd(), group, attr, value)
    }
}

impl VmFd {
    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// # Safety
    /// The host range must stay mapped while the slot exists.
    pub unsafe fn set_user_memory_region(&self, r: &kvm_userspace_memory_region) -> io::Result<()> {
        // SAFETY: a pointer to a live struct; the mapping contract is the caller's.
        unsafe {
            ioctl(
                self.fd(),
                KVM_SET_USER_MEMORY_REGION,
                r as *const _ as libc::c_ulong,
            )
        }
        .map(drop)
    }

    /// Deletes memory slot `slot`, which the guest then reaches as MMIO (api.rst 4.35: a
    /// slot of size 0).
    pub fn delete_user_memory_region(&self, slot: u32) -> io::Result<()> {
        let r = kvm_userspace_memory_region {
            slot,
            ..Default::default()
        };
        // SAFETY: a slot of no memory maps none.
        unsafe { self.set_user_memory_region(&r) }
    }

    /// An in-kernel device of type `kind`, configured through its attributes.
    pub fn create_device(&self, kind: u32) -> io::Result<DeviceFd> {
        let mut d = kvm_create_device {
            kind,
            ..Default::default()
        };
        // SAFETY: KVM reads the type and writes the device's descriptor into `d`.
        unsafe { ioctl(self.fd(), KVM_CREATE_DEVICE, (&raw mut d) as libc::c_ulong) }?;
        let fd = RawFd::try_from(d.fd).map_err(io::Error::other)?;
        // SAFETY: a fresh descriptor, owned from here on.
        Ok(DeviceFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Drives an interrupt line: a GSI on x86_64, an SPI or a PPI on arm64 (api.rst,
    /// KVM_IRQ_LINE); any thread may call it.
    pub fn irq_line(&self, irq: u32, level: bool) -> io::Result<()> {
        let l = kvm_irq_level {
            irq,
            level: u32::from(level),
        };
        // SAFETY: a pointer to a live struct.
        unsafe { ioctl(self.fd(), KVM_IRQ_LINE, (&raw const l) as libc::c_ulong) }.map(drop)
    }

    /// Raises `gsi` each time `event` is written, in the kernel; with `resample`, a
    /// level-triggered line, lowered as the guest ends the interrupt and `resample` written.
    pub fn irqfd(&self, event: RawFd, gsi: u32, resample: Option<RawFd>) -> io::Result<()> {
        let irqfd = kvm_irqfd {
            fd: event as u32,
            gsi,
            flags: if resample.is_some() {
                IRQFD_FLAG_RESAMPLE
            } else {
                0
            },
            resamplefd: resample.unwrap_or(0) as u32,
            ..Default::default()
        };
        // SAFETY: KVM reads one kvm_irqfd.
        unsafe { ioctl(self.fd(), KVM_IRQFD, (&raw const irqfd) as libc::c_ulong) }.map(drop)
    }

    /// Removes the irqfd of `event` on `gsi`.
    pub fn irqfd_remove(&self, event: RawFd, gsi: u32) -> io::Result<()> {
        let irqfd = kvm_irqfd {
            fd: event as u32,
            gsi,
            flags: IRQFD_FLAG_DEASSIGN,
            ..Default::default()
        };
        // SAFETY: KVM reads one kvm_irqfd.
        unsafe { ioctl(self.fd(), KVM_IRQFD, (&raw const irqfd) as libc::c_ulong) }.map(drop)
    }

    pub fn create_vcpu(&self, id: u64, mmap_size: usize) -> io::Result<VcpuFd> {
        // SAFETY: the vCPU id by value; the result is a new fd we own.
        let fd = unsafe { ioctl(self.fd(), KVM_CREATE_VCPU, id as libc::c_ulong) }?;
        // SAFETY: a fresh descriptor, owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let run = RunMap::new(&fd, mmap_size)?;
        Ok(VcpuFd {
            fd: std::sync::Arc::new(fd),
            run: std::sync::Arc::new(run),
        })
    }
}

/// The vCPU's `struct kvm_run` mapping. Shared with the kicker, which writes
/// `immediate_exit` from another thread; unmapped when both are gone.
#[derive(Debug)]
pub struct RunMap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain shared memory; cross-thread access is the atomic
// `immediate_exit` byte, everything else is touched only by the owning vCPU thread.
unsafe impl Send for RunMap {}
// SAFETY: as above.
unsafe impl Sync for RunMap {}

impl RunMap {
    fn new(fd: &OwnedFd, len: usize) -> io::Result<RunMap> {
        if len < KVM_RUN_SIZE {
            return Err(io::Error::other(format!(
                "KVM_GET_VCPU_MMAP_SIZE {len} is smaller than struct kvm_run"
            )));
        }
        // SAFETY: maps the vCPU fd as KVM documents (api: KVM_GET_VCPU_MMAP_SIZE).
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap returned NULL"))?;
        Ok(RunMap { ptr, len })
    }

    /// `kvm_run.immediate_exit`, offset 1.
    pub fn immediate_exit(&self) -> &AtomicU8 {
        // SAFETY: offset 1 is inside the mapping and u8-aligned; it lives as long as self.
        unsafe { AtomicU8::from_ptr(self.ptr.as_ptr().add(1)) }
    }

    /// Reads a field of `struct kvm_run`. Callers pass constant offsets inside it, and
    /// `new` checked that the mapping covers all of it.
    fn read<T: Copy>(&self, offset: usize) -> T {
        // SAFETY: offset + size ≤ KVM_RUN_SIZE ≤ len (checked in `new`); unaligned reads
        // are tolerated.
        unsafe { self.ptr.as_ptr().add(offset).cast::<T>().read_unaligned() }
    }

    /// Where the exit data at `offset..offset+len` is, if inside the mapping and clear of
    /// `immediate_exit` (the one byte another thread writes).
    fn data(&self, offset: usize, len: usize) -> Option<*mut u8> {
        let end = offset.checked_add(len)?;
        if end > self.len || offset < 2 {
            return None;
        }
        // SAFETY: in bounds (checked above).
        Some(unsafe { self.ptr.as_ptr().add(offset) })
    }
}

impl Drop for RunMap {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, used by nothing else now.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// Why KVM_RUN returned, decoded from `kvm_run`.
#[derive(Debug)]
pub enum RunExit<'a> {
    /// A signal or `immediate_exit`: EINTR.
    Interrupted,
    /// EAGAIN: an AP just received INIT/SIPI; run it again.
    Again,
    IoIn {
        port: u16,
        size: usize,
        data: &'a mut [u8],
    },
    IoOut {
        port: u16,
        size: usize,
        data: &'a [u8],
    },
    MmioRead {
        addr: u64,
        data: &'a mut [u8],
    },
    MmioWrite {
        addr: u64,
        data: &'a [u8],
    },
    Shutdown,
    SystemEvent(u32),
    Hlt,
    FailEntry(u64),
    InternalError(u32),
    /// A data abort outside every memory slot whose syndrome holds no access (arm64):
    /// ESR_EL2's valid bits, and the IPA.
    #[cfg(target_arch = "aarch64")]
    ArmNisv {
        esr_iss: u64,
        fault_ipa: u64,
    },
    Other(u32),
}

#[derive(Debug)]
pub struct VcpuFd {
    fd: std::sync::Arc<OwnedFd>,
    pub run: std::sync::Arc<RunMap>,
}

impl VcpuFd {
    fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Enters the guest and decodes the exit. The returned data borrows the
    /// kvm_run page; KVM completes a read on the next run.
    pub fn run(&mut self) -> io::Result<RunExit<'_>> {
        // SAFETY: KVM_RUN takes no argument; it writes only the mapped kvm_run.
        match unsafe { ioctl(self.fd(), KVM_RUN, 0) } {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => return Ok(RunExit::Interrupted),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(RunExit::Again),
            Err(e) => return Err(e),
        }
        let run = &*self.run;
        let reason: u32 = run.read(8);
        Ok(match reason {
            EXIT_IO => {
                let direction: u8 = run.read(32);
                let size: u8 = run.read(33);
                let port: u16 = run.read(34);
                let count: u32 = run.read(36);
                let offset: u64 = run.read(40);
                // String I/O (count > 1) is handled one element at a time by the caller.
                let len = usize::from(size).saturating_mul(count as usize);
                let p = run
                    .data(offset as usize, len)
                    .ok_or_else(|| io::Error::other(format!("port I/O data at {offset:#x}+{len}")))?;
                // SAFETY: in bounds; `&mut self` makes this the only reference to the exit
                // data until the next run, and the kicker never touches it.
                let data = unsafe { std::slice::from_raw_parts_mut(p, len) };
                let size = usize::from(size);
                if direction == IO_IN {
                    RunExit::IoIn { port, size, data }
                } else {
                    RunExit::IoOut { port, size, data }
                }
            }
            EXIT_MMIO => {
                let addr: u64 = run.read(32);
                let len: u32 = run.read(48);
                let is_write: u8 = run.read(52);
                let len = (len as usize).min(8);
                let p = run.data(40, len).ok_or_else(|| io::Error::other("MMIO data"))?;
                // SAFETY: as for port I/O above.
                let data = unsafe { std::slice::from_raw_parts_mut(p, len) };
                if is_write != 0 {
                    RunExit::MmioWrite { addr, data }
                } else {
                    RunExit::MmioRead { addr, data }
                }
            }
            EXIT_SHUTDOWN => RunExit::Shutdown,
            EXIT_SYSTEM_EVENT => RunExit::SystemEvent(run.read(32)),
            EXIT_HLT => RunExit::Hlt,
            EXIT_FAIL_ENTRY => RunExit::FailEntry(run.read(32)),
            EXIT_INTR => RunExit::Interrupted,
            EXIT_INTERNAL_ERROR => RunExit::InternalError(run.read(32)),
            #[cfg(target_arch = "aarch64")]
            EXIT_ARM_NISV => RunExit::ArmNisv {
                esr_iss: run.read(32),
                fault_ipa: run.read(40),
            },
            other => RunExit::Other(other),
        })
    }

    /// Clears `immediate_exit` after a kick was delivered.
    pub fn clear_immediate_exit(&self) {
        self.run.immediate_exit().store(0, Ordering::SeqCst);
    }
}
