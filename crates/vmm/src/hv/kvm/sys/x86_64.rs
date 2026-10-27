//! The KVM ioctls and structures of x86_64 alone (arch/x86/include/uapi/asm/kvm.h), as
//! probed in docs/research/kvm-x86_64-ground-truth.md §1.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::{
    KVM_CHECK_EXTENSION, KVM_CREATE_DEVICE, KVM_CREATE_VCPU, KVM_CREATE_VM, KVM_GET_API_VERSION,
    KVM_GET_DEVICE_ATTR, KVM_GET_MP_STATE, KVM_GET_VCPU_MMAP_SIZE, KVM_IRQ_LINE, KVM_IRQFD, KVM_RUN,
    KVM_SET_DEVICE_ATTR, KVM_SET_GSI_ROUTING, KVM_SET_MP_STATE, KVM_SET_USER_MEMORY_REGION, KVM_SIGNAL_MSI,
    Kvm, VcpuFd, VmFd, get_attr, ioctl, set_attr,
};

/// Every ioctl request shards makes of KVM: what its seccomp filter allows
/// (docs/research/rootless-security.md R3).
pub const IOCTLS: &[u64] = &[
    KVM_GET_API_VERSION,
    KVM_CREATE_VM,
    KVM_CHECK_EXTENSION,
    KVM_GET_VCPU_MMAP_SIZE,
    KVM_GET_SUPPORTED_CPUID,
    KVM_CREATE_VCPU,
    KVM_SET_USER_MEMORY_REGION,
    KVM_SET_TSS_ADDR,
    KVM_SET_IDENTITY_MAP_ADDR,
    KVM_CREATE_IRQCHIP,
    KVM_IRQ_LINE,
    KVM_IRQ_LINE_STATUS,
    KVM_RUN,
    KVM_SET_REGS,
    KVM_GET_SREGS,
    KVM_SET_SREGS,
    KVM_SET_CPUID2,
    KVM_GET_MSR_INDEX_LIST,
    KVM_GET_IRQCHIP,
    KVM_SET_IRQCHIP,
    KVM_SET_CLOCK,
    KVM_GET_CLOCK,
    KVM_GET_REGS,
    KVM_GET_MSRS,
    KVM_SET_MSRS,
    KVM_GET_LAPIC,
    KVM_SET_LAPIC,
    KVM_GET_MP_STATE,
    KVM_SET_MP_STATE,
    KVM_GET_VCPU_EVENTS,
    KVM_SET_VCPU_EVENTS,
    KVM_GET_DEBUGREGS,
    KVM_SET_DEBUGREGS,
    KVM_SET_TSC_KHZ,
    KVM_GET_TSC_KHZ,
    KVM_GET_XSAVE,
    KVM_SET_XSAVE,
    KVM_GET_XCRS,
    KVM_SET_XCRS,
    KVM_KVMCLOCK_CTRL,
    KVM_GET_XSAVE2,
    KVM_PRE_FAULT_MEMORY,
    KVM_SET_DEVICE_ATTR,
    KVM_GET_DEVICE_ATTR,
    KVM_IRQFD,
    KVM_SIGNAL_MSI,
    KVM_SET_GSI_ROUTING,
    KVM_CREATE_DEVICE,
];

pub const KVM_GET_SUPPORTED_CPUID: u64 = 0xC008_AE05;
pub const KVM_SET_TSS_ADDR: u64 = 0xAE47;
pub const KVM_SET_IDENTITY_MAP_ADDR: u64 = 0x4008_AE48;
pub const KVM_CREATE_IRQCHIP: u64 = 0xAE60;
/// KVM_IRQ_LINE, saying what became of the change: an edge's pulse's (`Irqs::pulse`).
pub const KVM_IRQ_LINE_STATUS: u64 = 0xC008_AE67;
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
pub const KVM_PRE_FAULT_MEMORY: u64 = 0xC040_AED5;

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
    assert!(size(KVM_PRE_FAULT_MEMORY) == size_of::<kvm_pre_fault_memory>());
    assert!(size(KVM_IRQ_LINE_STATUS) == size_of::<super::kvm_irq_level>());
};
/// `kvm_irqchip.chip_id`s.
pub const IRQCHIP_PIC_MASTER: u32 = 0;
pub const IRQCHIP_PIC_SLAVE: u32 = 1;
pub const IRQCHIP_IOAPIC: u32 = 2;

pub const CAP_SET_TSS_ADDR: u64 = 4;
pub const CAP_EXT_CPUID: u64 = 7;
pub const CAP_SET_IDENTITY_MAP_ADDR: u64 = 37;
pub const CAP_TSC_DEADLINE_TIMER: u64 = 72;
/// On the VM fd: the size of the guest's XSAVE area, 4096 or more, or 0 without XSAVE2.
pub const CAP_XSAVE2: u64 = 208;
/// KVM_PRE_FAULT_MEMORY (Linux 6.10); x86 reports it only with two-dimensional paging.
pub const CAP_PRE_FAULT_MEMORY: u64 = 236;
/// vCPUs take KVM_{GET,SET,HAS}_DEVICE_ATTR (Linux 5.16), on x86 for their TSC's offset.
pub const CAP_VCPU_ATTRIBUTES: u64 = 127;

/// A vCPU's TSC controls, and in them its offset from the host's TSC
/// (arch/x86/include/uapi/asm/kvm.h; Documentation/virt/kvm/devices/vcpu.rst §4).
const VCPU_TSC_CTRL: u32 = 0;
const VCPU_TSC_OFFSET: u64 = 0;

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

/// A range of guest memory to map ahead (api.rst 4.143); KVM advances it as it goes.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_pre_fault_memory {
    pub gpa: u64,
    pub size: u64,
    pub flags: u64,
    pub padding: [u64; 5],
}

const _: () = {
    assert!(size_of::<kvm_pre_fault_memory>() == 64);
    assert!(size_of::<kvm_regs>() == 144);
    assert!(size_of::<kvm_segment>() == 24);
    assert!(size_of::<kvm_dtable>() == 16);
    assert!(size_of::<kvm_sregs>() == 312);
    assert!(size_of::<kvm_cpuid_entry2>() == 40);
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

impl Kvm {
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
}

impl VmFd {
    /// As `irq_line`, saying what became of it (api.rst, KVM_IRQ_LINE_STATUS): above 0
    /// delivered, 0 coalesced with one still pending, below 0 held by a masked pin or
    /// refused.
    pub fn irq_line_status(&self, irq: u32, level: bool) -> io::Result<i32> {
        let mut l = super::kvm_irq_level {
            irq,
            level: u32::from(level),
        };
        // SAFETY: a pointer to a live struct, whose first word KVM writes the status into.
        unsafe { ioctl(self.fd(), KVM_IRQ_LINE_STATUS, (&raw mut l) as libc::c_ulong) }?;
        Ok(i32::from_ne_bytes(l.irq.to_ne_bytes()))
    }

    /// As `Kvm::check_extension`, for capabilities that depend on the VM.
    pub fn check_extension(&self, cap: u64) -> io::Result<i32> {
        // SAFETY: the capability number is passed by value.
        unsafe { ioctl(self.fd(), KVM_CHECK_EXTENSION, cap as libc::c_ulong) }
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
}

/// A vCPU's descriptor as another thread holds it, for ioctls that need not come from the
/// vCPU's own thread (api.rst 4: they work from any, at some cost); never KVM_RUN. Held
/// weakly, it does not keep the vCPU.
#[derive(Debug, Clone)]
pub struct WeakVcpu(std::sync::Weak<OwnedFd>);

impl WeakVcpu {
    pub fn upgrade(&self) -> Option<VcpuControl> {
        self.0.upgrade().map(VcpuControl)
    }
}

/// A [`WeakVcpu`] in use.
#[derive(Debug)]
pub struct VcpuControl(std::sync::Arc<OwnedFd>);

impl VcpuControl {
    pub fn tsc_offset(&self) -> io::Result<u64> {
        tsc_offset(self.0.as_raw_fd())
    }

    pub fn set_msrs(&self, entries: &[(u32, u64)]) -> io::Result<usize> {
        set_msrs(self.0.as_raw_fd(), entries)
    }
}

/// Writes `entries`; returns how many KVM took, stopping at the first it refused.
fn set_msrs(fd: RawFd, entries: &[(u32, u64)]) -> io::Result<usize> {
    let buf = msrs_buffer(entries);
    // SAFETY: a kvm_msrs with `nmsrs` entries, which KVM reads.
    let set = unsafe { ioctl(fd, KVM_SET_MSRS, buf.as_ptr() as libc::c_ulong) }?;
    Ok(set.unsigned_abs() as usize)
}

/// The guest TSC's offset from the host's: the guest reads the host's TSC, scaled to its
/// own frequency, plus this (`KVM_VCPU_TSC_OFFSET`).
fn tsc_offset(fd: RawFd) -> io::Result<u64> {
    get_attr(fd, VCPU_TSC_CTRL, VCPU_TSC_OFFSET)
}

impl VcpuFd {
    /// This vCPU's descriptor for another thread ([`WeakVcpu`]).
    pub fn weak(&self) -> WeakVcpu {
        WeakVcpu(std::sync::Arc::downgrade(&self.fd))
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
        set_msrs(self.fd(), entries)
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

    /// The guest TSC's offset from the host's ([`VcpuControl::tsc_offset`]).
    pub fn tsc_offset(&self) -> io::Result<u64> {
        tsc_offset(self.fd())
    }

    /// Sets the offset [`tsc_offset`](Self::tsc_offset) reads, exactly.
    pub fn set_tsc_offset(&self, offset: u64) -> io::Result<()> {
        set_attr(self.fd(), VCPU_TSC_CTRL, VCPU_TSC_OFFSET, &offset)
    }

    /// Maps what it can of `range` into the stage-2 tables, and advances it past that.
    pub fn pre_fault_memory(&self, range: &mut kvm_pre_fault_memory) -> io::Result<()> {
        // SAFETY: KVM reads and updates one kvm_pre_fault_memory, which `range` is.
        unsafe {
            ioctl(
                self.fd(),
                KVM_PRE_FAULT_MEMORY,
                (&raw mut *range) as libc::c_ulong,
            )
        }
        .map(drop)
    }

    /// Tells a guest with kvmclock that its vCPU was stopped, so its soft-lockup watchdog
    /// does not take the pause for a hang (api.rst, KVM_KVMCLOCK_CTRL).
    pub fn kvmclock_ctrl(&self) -> io::Result<()> {
        // SAFETY: no argument.
        unsafe { ioctl(self.fd(), KVM_KVMCLOCK_CTRL, 0) }.map(drop)
    }
}
