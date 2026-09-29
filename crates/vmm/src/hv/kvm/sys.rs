//! Thin wrappers over the KVM ioctls shards uses on x86_64. Numbers and layouts are the
//! uapi's (include/uapi/linux/kvm.h, arch/x86/include/uapi/asm/kvm.h), as probed in
//! docs/research/kvm-x86_64-ground-truth.md §1; sizes are checked at compile time.
#![allow(non_camel_case_types)]

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
pub const KVM_GET_SUPPORTED_CPUID: u64 = 0xC008_AE05;
pub const KVM_CREATE_VCPU: u64 = 0xAE41;
pub const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_AE46;
pub const KVM_SET_TSS_ADDR: u64 = 0xAE47;
pub const KVM_SET_IDENTITY_MAP_ADDR: u64 = 0x4008_AE48;
pub const KVM_CREATE_IRQCHIP: u64 = 0xAE60;
pub const KVM_IRQ_LINE: u64 = 0x4008_AE61;
pub const KVM_RUN: u64 = 0xAE80;
pub const KVM_SET_REGS: u64 = 0x4090_AE82;
pub const KVM_GET_SREGS: u64 = 0x8138_AE83;
pub const KVM_SET_SREGS: u64 = 0x4138_AE84;
pub const KVM_SET_CPUID2: u64 = 0x4008_AE90;

// Snapshot state (research doc §1.9; each number is _IOC(dir, 0xAE, nr, size), as several
// share a number and differ only there).
pub const KVM_GET_MSR_INDEX_LIST: u64 = 0xC004_AE02;
pub const KVM_GET_IRQCHIP: u64 = 0xC208_AE62;
/// Encoded `_IOR`, as KVM's own header has it.
pub const KVM_SET_IRQCHIP: u64 = 0x8208_AE63;
pub const KVM_SET_CLOCK: u64 = 0x4030_AE7B;
pub const KVM_GET_CLOCK: u64 = 0x8030_AE7C;
pub const KVM_GET_REGS: u64 = 0x8090_AE81;
pub const KVM_GET_MSRS: u64 = 0xC008_AE88;
pub const KVM_SET_MSRS: u64 = 0x4008_AE89;
pub const KVM_GET_LAPIC: u64 = 0x8400_AE8E;
pub const KVM_SET_LAPIC: u64 = 0x4400_AE8F;
pub const KVM_GET_MP_STATE: u64 = 0x8004_AE98;
pub const KVM_SET_MP_STATE: u64 = 0x4004_AE99;
pub const KVM_GET_VCPU_EVENTS: u64 = 0x8040_AE9F;
pub const KVM_SET_VCPU_EVENTS: u64 = 0x4040_AEA0;
pub const KVM_GET_DEBUGREGS: u64 = 0x8080_AEA1;
pub const KVM_SET_DEBUGREGS: u64 = 0x4080_AEA2;
pub const KVM_SET_TSC_KHZ: u64 = 0xAEA2;
pub const KVM_GET_TSC_KHZ: u64 = 0xAEA3;
pub const KVM_GET_XSAVE: u64 = 0x9000_AEA4;
pub const KVM_SET_XSAVE: u64 = 0x5000_AEA5;
pub const KVM_GET_XCRS: u64 = 0x8188_AEA6;
pub const KVM_SET_XCRS: u64 = 0x4188_AEA7;
pub const KVM_KVMCLOCK_CTRL: u64 = 0xAEAD;
pub const KVM_GET_XSAVE2: u64 = 0x9000_AECF;

/// The sizes of the state structures above, which a snapshot keeps as their bytes.
pub const LAPIC_SIZE: usize = 1024;
pub const VCPU_EVENTS_SIZE: usize = 64;
pub const DEBUGREGS_SIZE: usize = 128;
pub const XSAVE_SIZE: usize = 4096;
pub const XCRS_SIZE: usize = 392;
pub const IRQCHIP_SIZE: usize = 520;
pub const CLOCK_SIZE: usize = 48;
/// Each size is the one its ioctl numbers encode (bits 16-29).
const _: () = {
    const fn size(request: u64) -> usize {
        ((request >> 16) & 0x3fff) as usize
    }
    assert!(size(KVM_GET_LAPIC) == LAPIC_SIZE && size(KVM_SET_LAPIC) == LAPIC_SIZE);
    assert!(size(KVM_GET_VCPU_EVENTS) == VCPU_EVENTS_SIZE);
    assert!(size(KVM_SET_VCPU_EVENTS) == VCPU_EVENTS_SIZE);
    assert!(size(KVM_GET_DEBUGREGS) == DEBUGREGS_SIZE && size(KVM_SET_DEBUGREGS) == DEBUGREGS_SIZE);
    assert!(size(KVM_GET_XSAVE) == XSAVE_SIZE && size(KVM_SET_XSAVE) == XSAVE_SIZE);
    assert!(size(KVM_GET_XCRS) == XCRS_SIZE && size(KVM_SET_XCRS) == XCRS_SIZE);
    assert!(size(KVM_GET_IRQCHIP) == IRQCHIP_SIZE && size(KVM_SET_IRQCHIP) == IRQCHIP_SIZE);
    assert!(size(KVM_GET_CLOCK) == CLOCK_SIZE && size(KVM_SET_CLOCK) == CLOCK_SIZE);
    assert!(size(KVM_GET_REGS) == size_of::<kvm_regs>());
};
/// `kvm_irqchip.chip_id`s.
pub const IRQCHIP_PIC_MASTER: u32 = 0;
pub const IRQCHIP_PIC_SLAVE: u32 = 1;
pub const IRQCHIP_IOAPIC: u32 = 2;

pub const CAP_IRQCHIP: u64 = 0;
pub const CAP_USER_MEMORY: u64 = 3;
pub const CAP_SET_TSS_ADDR: u64 = 4;
pub const CAP_EXT_CPUID: u64 = 7;
pub const CAP_NR_VCPUS: u64 = 9;
pub const CAP_MP_STATE: u64 = 14;
pub const CAP_SET_IDENTITY_MAP_ADDR: u64 = 37;
pub const CAP_MAX_VCPUS: u64 = 66;
pub const CAP_TSC_DEADLINE_TIMER: u64 = 72;
pub const CAP_IMMEDIATE_EXIT: u64 = 136;
pub const CAP_READONLY_MEM: u64 = 81;
/// On the VM fd: the size of the guest's XSAVE area, 4096 or more, or 0 without XSAVE2.
pub const CAP_XSAVE2: u64 = 208;

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

pub const SYSTEM_EVENT_SHUTDOWN: u32 = 1;
pub const SYSTEM_EVENT_RESET: u32 = 2;
pub const SYSTEM_EVENT_CRASH: u32 = 3;

pub const IO_IN: u8 = 0;

/// `sizeof(struct kvm_run)` on x86_64 (research doc §1.5).
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
pub struct kvm_regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
    pub padding: u8,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_dtable {
    pub base: u64,
    pub limit: u16,
    pub padding: [u16; 3],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_sregs {
    pub cs: kvm_segment,
    pub ds: kvm_segment,
    pub es: kvm_segment,
    pub fs: kvm_segment,
    pub gs: kvm_segment,
    pub ss: kvm_segment,
    pub tr: kvm_segment,
    pub ldt: kvm_segment,
    pub gdt: kvm_dtable,
    pub idt: kvm_dtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    pub interrupt_bitmap: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct kvm_cpuid_entry2 {
    pub function: u32,
    pub index: u32,
    pub flags: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub padding: [u32; 3],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_irq_level {
    pub irq: u32,
    pub level: u32,
}

const _: () = {
    assert!(size_of::<kvm_userspace_memory_region>() == 32);
    assert!(size_of::<kvm_regs>() == 144);
    assert!(size_of::<kvm_segment>() == 24);
    assert!(size_of::<kvm_dtable>() == 16);
    assert!(size_of::<kvm_sregs>() == 312);
    assert!(size_of::<kvm_cpuid_entry2>() == 40);
    assert!(size_of::<kvm_irq_level>() == 8);
};

/// The most CPUID entries KVM returns (it clamps `nent` to 256).
pub const MAX_CPUID_ENTRIES: usize = 256;

/// `struct kvm_cpuid2` with its entries inline.
#[repr(C)]
struct Cpuid2 {
    nent: u32,
    padding: u32,
    entries: [kvm_cpuid_entry2; MAX_CPUID_ENTRIES],
}

/// Calls `request`, which fills a structure of `len` bytes, and returns the bytes: state a
/// snapshot keeps whole, to give back on the host that saved it. `init` goes in first, for
/// requests that read a field before they write (`kvm_irqchip.chip_id`).
fn get_bytes(fd: RawFd, request: u64, len: usize, init: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    for (b, &i) in buf.iter_mut().zip(init) {
        *b = i;
    }
    // SAFETY: a buffer of the structure's size, which the kernel copies whole.
    unsafe { ioctl(fd, request, buf.as_mut_ptr() as libc::c_ulong) }?;
    Ok(buf)
}

/// Calls `request` with `bytes`, a structure of its size, which the kernel copies in.
fn set_bytes(fd: RawFd, request: u64, bytes: &[u8], len: usize) -> io::Result<()> {
    if bytes.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} bytes where the kernel reads {len}", bytes.len()),
        ));
    }
    // SAFETY: at least `len` bytes, which the kernel copies in and does not keep.
    unsafe { ioctl(fd, request, bytes.as_ptr() as libc::c_ulong) }.map(drop)
}

/// `struct kvm_msrs` holding `entries`: nmsrs, padding, then each index, a reserved word
/// and the value.
fn msrs_buffer(entries: &[(u32, u64)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + 16 * entries.len());
    b.extend_from_slice(&u32::try_from(entries.len()).unwrap_or(u32::MAX).to_ne_bytes());
    b.extend_from_slice(&0u32.to_ne_bytes());
    for &(index, data) in entries {
        b.extend_from_slice(&index.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        b.extend_from_slice(&data.to_ne_bytes());
    }
    b
}

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

    /// The CPUID KVM can offer: the template every vCPU's CPUID is built from.
    pub fn supported_cpuid(&self) -> io::Result<Vec<kvm_cpuid_entry2>> {
        let mut c = Box::new(Cpuid2 {
            nent: MAX_CPUID_ENTRIES as u32,
            padding: 0,
            entries: [kvm_cpuid_entry2::default(); MAX_CPUID_ENTRIES],
        });
        // SAFETY: a kvm_cpuid2 with room for `nent` entries; KVM writes at most that many.
        unsafe {
            ioctl(
                self.0.as_raw_fd(),
                KVM_GET_SUPPORTED_CPUID,
                (&raw mut *c) as libc::c_ulong,
            )
        }?;
        let n = (c.nent as usize).min(MAX_CPUID_ENTRIES);
        Ok(c.entries.iter().take(n).copied().collect())
    }

    /// The MSRs KVM saves and restores for a VMM: those it passes through and those it
    /// emulates (api.rst, KVM_GET_MSR_INDEX_LIST).
    pub fn msr_index_list(&self) -> io::Result<Vec<u32>> {
        const MAX: usize = 1024;
        let mut buf = vec![0u8; 4 + 4 * MAX];
        if let Some(n) = buf.get_mut(..4) {
            n.copy_from_slice(&(MAX as u32).to_ne_bytes());
        }
        // SAFETY: a kvm_msr_list with room for MAX indices.
        unsafe {
            ioctl(
                self.0.as_raw_fd(),
                KVM_GET_MSR_INDEX_LIST,
                buf.as_mut_ptr() as libc::c_ulong,
            )
        }?;
        let n = buf
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map_or(0, |b: [u8; 4]| u32::from_ne_bytes(b) as usize)
            .min(MAX);
        Ok(buf
            .get(4..4 + 4 * n)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| u32::from_ne_bytes(c))
            .collect())
    }

    pub fn create_vm(&self) -> io::Result<VmFd> {
        // SAFETY: type 0 (KVM_X86_DEFAULT_VM) by value; the result is a new fd we own.
        let fd = unsafe { ioctl(self.0.as_raw_fd(), KVM_CREATE_VM, 0) }?;
        // SAFETY: a fresh descriptor, owned from here on.
        Ok(VmFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

#[derive(Debug)]
pub struct VmFd(OwnedFd);

impl VmFd {
    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Three pages below 4 GiB for Intel's real-mode TSS; passed by value.
    pub fn set_tss_addr(&self, addr: u64) -> io::Result<()> {
        // SAFETY: address by value.
        unsafe { ioctl(self.fd(), KVM_SET_TSS_ADDR, addr as libc::c_ulong) }.map(drop)
    }

    /// One page below 4 GiB for EPT's real-mode identity map; passed by pointer.
    pub fn set_identity_map_addr(&self, addr: u64) -> io::Result<()> {
        // SAFETY: a pointer to a live u64.
        unsafe {
            ioctl(
                self.fd(),
                KVM_SET_IDENTITY_MAP_ADDR,
                (&raw const addr) as libc::c_ulong,
            )
        }
        .map(drop)
    }

    /// PIC, IOAPIC (0xFEC0_0000) and a LAPIC per future vCPU (0xFEE0_0000), in the kernel.
    pub fn create_irqchip(&self) -> io::Result<()> {
        // SAFETY: no argument.
        unsafe { ioctl(self.fd(), KVM_CREATE_IRQCHIP, 0) }.map(drop)
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

    /// As `Kvm::check_extension`, for capabilities that depend on the VM.
    pub fn check_extension(&self, cap: u64) -> io::Result<i32> {
        // SAFETY: the capability number is passed by value.
        unsafe { ioctl(self.fd(), KVM_CHECK_EXTENSION, cap as libc::c_ulong) }
    }

    /// One of the in-kernel interrupt controllers' state (`IRQCHIP_*`), as its bytes.
    pub fn get_irqchip(&self, chip: u32) -> io::Result<Vec<u8>> {
        get_bytes(self.fd(), KVM_GET_IRQCHIP, IRQCHIP_SIZE, &chip.to_ne_bytes())
    }

    pub fn set_irqchip(&self, state: &[u8]) -> io::Result<()> {
        set_bytes(self.fd(), KVM_SET_IRQCHIP, state, IRQCHIP_SIZE)
    }

    /// kvmclock (`struct kvm_clock_data`), as its bytes.
    pub fn get_clock(&self) -> io::Result<Vec<u8>> {
        get_bytes(self.fd(), KVM_GET_CLOCK, CLOCK_SIZE, &[])
    }

    /// Sets kvmclock to the value saved in `state`, from now: with its flags cleared, as
    /// Firecracker restores it, so that the guest's clock goes on from the snapshot.
    pub fn set_clock(&self, state: &[u8]) -> io::Result<()> {
        let mut clock = state.to_vec();
        if let Some(flags) = clock.get_mut(8..12) {
            flags.fill(0);
        }
        set_bytes(self.fd(), KVM_SET_CLOCK, &clock, CLOCK_SIZE)
    }

    /// Drives a GSI; any thread may call it.
    pub fn irq_line(&self, gsi: u32, level: bool) -> io::Result<()> {
        let l = kvm_irq_level {
            irq: gsi,
            level: u32::from(level),
        };
        // SAFETY: a pointer to a live struct.
        unsafe { ioctl(self.fd(), KVM_IRQ_LINE, (&raw const l) as libc::c_ulong) }.map(drop)
    }

    pub fn create_vcpu(&self, id: u64, mmap_size: usize) -> io::Result<VcpuFd> {
        // SAFETY: the vCPU id by value; the result is a new fd we own.
        let fd = unsafe { ioctl(self.fd(), KVM_CREATE_VCPU, id as libc::c_ulong) }?;
        // SAFETY: a fresh descriptor, owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let run = RunMap::new(&fd, mmap_size)?;
        Ok(VcpuFd {
            fd,
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
    Other(u32),
}

#[derive(Debug)]
pub struct VcpuFd {
    fd: OwnedFd,
    pub run: std::sync::Arc<RunMap>,
}

impl VcpuFd {
    fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn set_cpuid(&self, entries: &[kvm_cpuid_entry2]) -> io::Result<()> {
        let n = entries.len().min(MAX_CPUID_ENTRIES);
        let mut c = Box::new(Cpuid2 {
            nent: n as u32,
            padding: 0,
            entries: [kvm_cpuid_entry2::default(); MAX_CPUID_ENTRIES],
        });
        for (dst, src) in c.entries.iter_mut().zip(entries) {
            *dst = *src;
        }
        // SAFETY: a kvm_cpuid2 holding `nent` entries.
        unsafe { ioctl(self.fd(), KVM_SET_CPUID2, (&raw const *c) as libc::c_ulong) }.map(drop)
    }

    pub fn get_sregs(&self) -> io::Result<kvm_sregs> {
        let mut s = kvm_sregs::default();
        // SAFETY: KVM fills the struct.
        unsafe { ioctl(self.fd(), KVM_GET_SREGS, (&raw mut s) as libc::c_ulong) }?;
        Ok(s)
    }

    pub fn set_sregs(&self, s: &kvm_sregs) -> io::Result<()> {
        // SAFETY: a pointer to a live struct.
        unsafe { ioctl(self.fd(), KVM_SET_SREGS, s as *const _ as libc::c_ulong) }.map(drop)
    }

    pub fn set_regs(&self, r: &kvm_regs) -> io::Result<()> {
        // SAFETY: a pointer to a live struct.
        unsafe { ioctl(self.fd(), KVM_SET_REGS, r as *const _ as libc::c_ulong) }.map(drop)
    }

    /// A state structure `request` fills, of `len` bytes.
    pub fn get_state(&self, request: u64, len: usize) -> io::Result<Vec<u8>> {
        get_bytes(self.fd(), request, len, &[])
    }

    /// A state structure `request` reads, of `len` bytes.
    pub fn set_state(&self, request: u64, state: &[u8], len: usize) -> io::Result<()> {
        set_bytes(self.fd(), request, state, len)
    }

    /// The values of the MSRs `indices` names, as far as KVM reads them: it stops at the
    /// first it cannot, and says how many it read.
    pub fn get_msrs(&self, indices: &[u32]) -> io::Result<Vec<(u32, u64)>> {
        let entries: Vec<(u32, u64)> = indices.iter().map(|&i| (i, 0)).collect();
        let mut buf = msrs_buffer(&entries);
        // SAFETY: a kvm_msrs with `nmsrs` entries, which KVM fills.
        let read = unsafe { ioctl(self.fd(), KVM_GET_MSRS, buf.as_mut_ptr() as libc::c_ulong) }?;
        Ok(buf
            .get(8..)
            .unwrap_or_default()
            .as_chunks::<16>()
            .0
            .iter()
            .take(read.unsigned_abs() as usize)
            .map(|e| {
                let (index, rest) = e.split_first_chunk::<4>().unwrap_or((&[0; 4], &[]));
                let data = rest.get(4..12).and_then(|d| d.try_into().ok()).unwrap_or([0; 8]);
                (u32::from_ne_bytes(*index), u64::from_ne_bytes(data))
            })
            .collect())
    }

    /// Writes `entries`; returns how many KVM took, stopping at the first it refused.
    pub fn set_msrs(&self, entries: &[(u32, u64)]) -> io::Result<usize> {
        let buf = msrs_buffer(entries);
        // SAFETY: a kvm_msrs with `nmsrs` entries, which KVM reads.
        let set = unsafe { ioctl(self.fd(), KVM_SET_MSRS, buf.as_ptr() as libc::c_ulong) }?;
        Ok(set.unsigned_abs() as usize)
    }

    pub fn get_mp_state(&self) -> io::Result<u32> {
        let mut state = 0u32;
        // SAFETY: KVM writes one u32.
        unsafe { ioctl(self.fd(), KVM_GET_MP_STATE, (&raw mut state) as libc::c_ulong) }?;
        Ok(state)
    }

    pub fn set_mp_state(&self, state: u32) -> io::Result<()> {
        // SAFETY: KVM reads one u32.
        unsafe { ioctl(self.fd(), KVM_SET_MP_STATE, (&raw const state) as libc::c_ulong) }.map(drop)
    }

    /// The guest TSC's frequency in kHz.
    pub fn tsc_khz(&self) -> io::Result<u32> {
        // SAFETY: no argument; the result is the frequency.
        let khz = unsafe { ioctl(self.fd(), KVM_GET_TSC_KHZ, 0) }?;
        Ok(khz.unsigned_abs())
    }

    pub fn set_tsc_khz(&self, khz: u32) -> io::Result<()> {
        // SAFETY: the frequency by value.
        unsafe { ioctl(self.fd(), KVM_SET_TSC_KHZ, libc::c_ulong::from(khz)) }.map(drop)
    }

    /// Tells a guest with kvmclock that its vCPU was stopped, so its soft-lockup watchdog
    /// does not take the pause for a hang (api.rst, KVM_KVMCLOCK_CTRL).
    pub fn kvmclock_ctrl(&self) -> io::Result<()> {
        // SAFETY: no argument.
        unsafe { ioctl(self.fd(), KVM_KVMCLOCK_CTRL, 0) }.map(drop)
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
            other => RunExit::Other(other),
        })
    }

    /// Clears `immediate_exit` after a kick was delivered.
    pub fn clear_immediate_exit(&self) {
        self.run.immediate_exit().store(0, Ordering::SeqCst);
    }
}
