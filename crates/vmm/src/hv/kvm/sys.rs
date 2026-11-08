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
pub const KVM_SIGNAL_MSI: u64 = 0x4020_AEA5;
pub const KVM_SET_GSI_ROUTING: u64 = 0x4008_AE6A;
/// _IO(KVMIO, 0xc7): harvested dirty-ring entries given back to the rings (api.rst 4.137).
pub const KVM_RESET_DIRTY_RINGS: u64 = 0xAEC7;
/// _IOW(KVMIO, 0x42, struct kvm_dirty_log): a slot's pages written since it was last asked,
/// its bits cleared and their pages taken away for writing again as it answers (api.rst
/// 4.8), where a VM's vCPUs keep no dirty rings.
pub const KVM_GET_DIRTY_LOG: u64 = 0x4010_AE42;

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
    assert!(size(KVM_SIGNAL_MSI) == size_of::<kvm_msi>());
    assert!(size(KVM_SET_GSI_ROUTING) == 8);
    assert!(size(KVM_GET_DIRTY_LOG) == size_of::<kvm_dirty_log>());
};

/// What KVM_GET_DIRTY_LOG asks: a slot, and where its bitmap goes, a bit a host page of it.
#[repr(C)]
#[derive(Debug, Default)]
struct kvm_dirty_log {
    slot: u32,
    padding: u32,
    dirty_bitmap: u64,
}

/// An MSI for the VMM to deliver (api.rst 4.70, KVM_SIGNAL_MSI).
#[repr(C)]
#[derive(Debug, Default)]
pub struct kvm_msi {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
    pub flags: u32,
    pub devid: u32,
    pub pad: [u8; 12],
}

/// An MSI's `devid` holds the ID of the device that wrote it (api.rst 4.70, 4.52): what
/// an ITS translates by (arm64), which KVM_CAP_MSI_DEVID says a VM needs.
pub const MSI_VALID_DEVID: u32 = 1;

/// An entry of the GSI routing table (api.rst 4.52, KVM_SET_GSI_ROUTING): a GSI's pin of
/// an in-kernel irqchip, or the MSI it sends; its union padded to 32 bytes.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_irq_routing_entry {
    pub gsi: u32,
    pub kind: u32,
    pub flags: u32,
    pub pad: u32,
    pub u: [u32; 8],
}

/// kvm_irq_routing_entry types.
pub const IRQ_ROUTING_IRQCHIP: u32 = 1;
pub const IRQ_ROUTING_MSI: u32 = 2;

pub const CAP_IRQCHIP: u64 = 0;
pub const CAP_USER_MEMORY: u64 = 3;
pub const CAP_NR_VCPUS: u64 = 9;
pub const CAP_MP_STATE: u64 = 14;
pub const CAP_MAX_VCPUS: u64 = 66;
pub const CAP_IMMEDIATE_EXIT: u64 = 136;
pub const CAP_READONLY_MEM: u64 = 81;
/// Per-vCPU rings of the pages its guest dirtied (api.rst 8.29), and their ordered variant,
/// arm64's only one (8.35). Each reports the most bytes a ring may take.
#[cfg(target_arch = "x86_64")]
pub const CAP_DIRTY_LOG_RING: u64 = 192;
#[cfg(target_arch = "x86_64")]
pub const CAP_DIRTY_LOG_RING_ACQ_REL: u64 = 223;
/// A memory slot whose guest writes KVM logs (api.rst 4.35, KVM_MEM_LOG_DIRTY_PAGES).
pub const MEM_LOG_DIRTY_PAGES: u32 = 1 << 0;
/// A vCPU whose dirty ring is full exits so, until its entries are harvested and reset.
pub const EXIT_DIRTY_RING_FULL: u32 = 31;
/// Where a vCPU's dirty ring is in its descriptor's mapping, in pages (KVM_DIRTY_LOG_PAGE_
/// OFFSET, x86's and arm64's asm/kvm.h).
const DIRTY_LOG_PAGE_OFFSET: libc::off_t = 64;
/// struct kvm_dirty_gfn's flags: dirtied by the vCPU, and harvested by its VMM.
const DIRTY_GFN_DIRTY: u32 = 1 << 0;
const DIRTY_GFN_RESET: u32 = 1 << 1;

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
pub const DEV_VFIO_FILE_DEL: u64 = 2;

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
    assert!(size_of::<kvm_msi>() == 32);
    assert!(size_of::<kvm_irq_routing_entry>() == 48);
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

/// Reads attribute `attr` of `group` of the device or vCPU `fd`, of type `T`.
fn get_attr<T: Default>(fd: RawFd, group: u32, attr: u64) -> io::Result<T> {
    let mut value = T::default();
    let a = kvm_device_attr {
        group,
        attr,
        addr: (&raw mut value) as u64,
        ..Default::default()
    };
    // SAFETY: a kvm_device_attr whose `addr` is a live `T`, which KVM writes.
    unsafe { ioctl(fd, KVM_GET_DEVICE_ATTR, (&raw const a) as libc::c_ulong) }?;
    Ok(value)
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

    /// The dirty log of slot `slot`, of `pages` host pages, logged (KVM_MEM_LOG_DIRTY_PAGES):
    /// a bit a page written since it was last asked, each cleared and its page taken away
    /// for writing again before KVM answers, so that a write after it is in the next
    /// (api.rst 4.8; virt/kvm/kvm_main.c, kvm_get_dirty_log_protect). Bit `i` is word
    /// `i / 64`'s bit `i % 64`, KVM's little-endian `unsigned long`s read as words.
    pub fn dirty_log(&self, slot: u32, pages: u64) -> io::Result<Vec<u64>> {
        let words =
            usize::try_from(pages.div_ceil(64)).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut bitmap = vec![0u64; words];
        let log = kvm_dirty_log {
            slot,
            padding: 0,
            dirty_bitmap: bitmap.as_mut_ptr() as u64,
        };
        // SAFETY: KVM writes the slot's bitmap, ALIGN(pages, 64) / 8 bytes, which `bitmap`
        // holds, and reads the struct it is given.
        unsafe { ioctl(self.fd(), KVM_GET_DIRTY_LOG, (&raw const log) as libc::c_ulong) }?;
        Ok(bitmap)
    }

    /// Delivers the MSI of `address` and `data`, from the device of ID `devid` where the
    /// VM's interrupt controller translates by one (api.rst 4.70).
    pub fn signal_msi(&self, address: u64, data: u32, devid: Option<u32>) -> io::Result<()> {
        let msi = kvm_msi {
            address_lo: address as u32,
            address_hi: (address >> 32) as u32,
            data,
            flags: if devid.is_some() { MSI_VALID_DEVID } else { 0 },
            devid: devid.unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: KVM reads one kvm_msi.
        unsafe { ioctl(self.fd(), KVM_SIGNAL_MSI, (&raw const msi) as libc::c_ulong) }.map(drop)
    }

    /// Replaces the GSI routing table with `entries` (api.rst 4.52): `struct
    /// kvm_irq_routing`, its count and flags, then the entries.
    pub fn set_gsi_routing(&self, entries: &[kvm_irq_routing_entry]) -> io::Result<()> {
        let mut table: Vec<u32> = Vec::with_capacity(2 + 12 * entries.len());
        table.extend([entries.len() as u32, 0]);
        for e in entries {
            table.extend([e.gsi, e.kind, e.flags, e.pad]);
            table.extend(e.u);
        }
        // SAFETY: a kvm_irq_routing holding `nr` entries, which KVM reads.
        unsafe { ioctl(self.fd(), KVM_SET_GSI_ROUTING, table.as_ptr() as libc::c_ulong) }.map(drop)
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

    /// Gives each vCPU made after this a dirty ring of `bytes` (api.rst 4.37 and 8.29):
    /// `cap`, [`CAP_DIRTY_LOG_RING_ACQ_REL`] where KVM has it, else [`CAP_DIRTY_LOG_RING`].
    #[cfg(target_arch = "x86_64")]
    pub fn enable_dirty_ring(&self, cap: u64, bytes: u32) -> io::Result<()> {
        #[repr(C)]
        struct EnableCap {
            cap: u32,
            flags: u32,
            args: [u64; 4],
            pad: [u64; 8],
        }
        let c = EnableCap {
            cap: u32::try_from(cap).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
            flags: 0,
            args: [u64::from(bytes), 0, 0, 0],
            pad: [0; 8],
        };
        // SAFETY: KVM_ENABLE_CAP reads the struct it is given.
        unsafe { ioctl(self.fd(), KVM_ENABLE_CAP, (&raw const c) as libc::c_ulong) }.map(drop)
    }

    /// Gives every vCPU's harvested ring entries back to it (KVM_RESET_DIRTY_RINGS): how
    /// many it reset.
    pub fn reset_dirty_rings(&self) -> io::Result<u32> {
        // SAFETY: no argument.
        unsafe { ioctl(self.fd(), KVM_RESET_DIRTY_RINGS, 0) }.map(|n| n.unsigned_abs())
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

/// A vCPU's dirty ring (api.rst 8.29): `struct kvm_dirty_gfn`s, 16 bytes each, which KVM
/// fills as the vCPU's guest writes pages of a slot whose writes it logs, and which its VMM
/// harvests in order, each marked harvested, then resets through the VM. Harvested from
/// any thread, one at a time ([`DirtyRing::harvest`]).
#[derive(Debug)]
pub struct DirtyRing {
    ptr: NonNull<u8>,
    entries: u32,
    /// The next entry to harvest.
    next: std::sync::Mutex<u32>,
}

// SAFETY: the mapping is shared memory KVM and its harvester exchange through each entry's
// flags, read with acquire and written with release; the cursor is under its lock.
unsafe impl Send for DirtyRing {}
// SAFETY: as above.
unsafe impl Sync for DirtyRing {}

impl DirtyRing {
    /// Maps `vcpu`'s ring of `bytes` (a power of two, as enabled).
    pub fn map(vcpu: &VcpuFd, bytes: u32) -> io::Result<DirtyRing> {
        let page = page_size()?;
        // SAFETY: maps the vCPU's ring where KVM documents it, shared and writable, as the
        // harvest marks entries.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                vcpu.fd(),
                DIRTY_LOG_PAGE_OFFSET * page,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast()).ok_or_else(|| io::Error::other("mmap returned NULL"))?;
        Ok(DirtyRing {
            ptr,
            entries: bytes / 16,
            next: std::sync::Mutex::new(0),
        })
    }

    /// Takes every entry KVM has filled since the last harvest, each given to `take` as its
    /// slot and its page's index in it, and marks it harvested; how many it took. The VM's
    /// [`VmFd::reset_dirty_rings`] gives them back to KVM.
    pub fn harvest(&self, take: &mut dyn FnMut(u32, u64)) -> u32 {
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut taken = 0;
        while taken < self.entries {
            let at = (*next % self.entries) as usize * 16;
            // SAFETY: an entry of the mapping, 16 bytes at a 16-byte boundary; its flags
            // word is 4-aligned, and KVM and this process exchange it atomically.
            let flags = unsafe { std::sync::atomic::AtomicU32::from_ptr(self.ptr.as_ptr().add(at).cast()) };
            if flags.load(Ordering::Acquire) & DIRTY_GFN_DIRTY == 0 {
                break;
            }
            // SAFETY: the rest of the entry, KVM's until its flags said it was dirty.
            let (slot, offset) = unsafe {
                (
                    self.ptr.as_ptr().add(at + 4).cast::<u32>().read(),
                    self.ptr.as_ptr().add(at + 8).cast::<u64>().read(),
                )
            };
            take(slot, offset);
            flags.store(DIRTY_GFN_RESET, Ordering::Release);
            *next = next.wrapping_add(1);
            taken += 1;
        }
        taken
    }
}

impl Drop for DirtyRing {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, used by nothing else now.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.entries as usize * 16) };
    }
}

/// The host's page size.
fn page_size() -> io::Result<libc::off_t> {
    // SAFETY: sysconf(3) has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as libc::off_t)
}

/// Why KVM_RUN returned, decoded from `kvm_run`.
#[derive(Debug)]
pub enum RunExit<'a> {
    /// A signal or `immediate_exit`: EINTR.
    Interrupted,
    /// EAGAIN: an AP just received INIT/SIPI; run it again.
    Again,
    /// EFAULT: the guest touched memory of a slot whose host mapping gave no page (a file
    /// past its end, memory withheld); KVM says not where.
    Fault,
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
    /// The vCPU's dirty ring is full: it may run again once its entries are reset.
    DirtyRingFull,
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
            Err(e) if e.raw_os_error() == Some(libc::EFAULT) => return Ok(RunExit::Fault),
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
            EXIT_DIRTY_RING_FULL => RunExit::DirtyRingFull,
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
