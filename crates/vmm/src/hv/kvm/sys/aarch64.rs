//! The KVM ioctls and structures of arm64 alone (arch/arm64/include/uapi/asm/kvm.h;
//! Documentation/virt/kvm/api.rst and devices/arm-vgic-v3.rst).

use std::io;
use std::os::fd::AsRawFd;

use super::{
    DeviceFd, KVM_CHECK_EXTENSION, KVM_CREATE_DEVICE, KVM_CREATE_VCPU, KVM_CREATE_VM, KVM_GET_API_VERSION,
    KVM_GET_DEVICE_ATTR, KVM_GET_MP_STATE, KVM_GET_VCPU_MMAP_SIZE, KVM_IRQ_LINE, KVM_IRQFD, KVM_RUN,
    KVM_SET_DEVICE_ATTR, KVM_SET_GSI_ROUTING, KVM_SET_MP_STATE, KVM_SET_USER_MEMORY_REGION, KVM_SIGNAL_MSI,
    VcpuFd, VmFd, ioctl,
};

/// Every ioctl request shards makes of KVM: what its seccomp filter allows
/// (docs/research/rootless-security.md R3).
pub const IOCTLS: &[u64] = &[
    KVM_GET_API_VERSION,
    KVM_CREATE_VM,
    KVM_CHECK_EXTENSION,
    KVM_GET_VCPU_MMAP_SIZE,
    KVM_CREATE_VCPU,
    KVM_SET_USER_MEMORY_REGION,
    KVM_IRQ_LINE,
    KVM_RUN,
    KVM_GET_MP_STATE,
    KVM_SET_MP_STATE,
    KVM_SET_DEVICE_ATTR,
    KVM_GET_DEVICE_ATTR,
    KVM_GET_ONE_REG,
    KVM_SET_ONE_REG,
    KVM_ARM_VCPU_INIT,
    KVM_ARM_PREFERRED_TARGET,
    KVM_CREATE_DEVICE,
    KVM_IRQFD,
    KVM_ENABLE_CAP,
    KVM_SET_VCPU_EVENTS,
    KVM_GET_VCPU_EVENTS,
    KVM_GET_REG_LIST,
    KVM_SIGNAL_MSI,
    KVM_SET_GSI_ROUTING,
];

pub const KVM_GET_ONE_REG: u64 = 0x4010_AEAB;
/// _IOWR(KVMIO, 0xb0, struct kvm_reg_list): its count, then that many IDs.
pub const KVM_GET_REG_LIST: u64 = 0xC008_AEB0;
/// _IOR(KVMIO, 0x9f, struct kvm_vcpu_events), arm64's.
pub const KVM_GET_VCPU_EVENTS: u64 = 0x8040_AE9F;
/// _IOW(KVMIO, 0xa3, struct kvm_enable_cap).
pub const KVM_ENABLE_CAP: u64 = 0x4068_AEA3;
/// _IOW(KVMIO, 0xa0, struct kvm_vcpu_events), arm64's.
pub const KVM_SET_VCPU_EVENTS: u64 = 0x4040_AEA0;
/// Returns a data abort KVM cannot emulate to the VMM, in place of KVM_RUN's ENOSYS
/// (api.rst 7.20).
pub const CAP_ARM_NISV_TO_USER: u32 = 177;

/// struct kvm_enable_cap, its 64 bytes of padding as eight words.
#[repr(C)]
#[derive(Debug, Default)]
struct kvm_enable_cap {
    cap: u32,
    flags: u32,
    args: [u64; 4],
    pad: [u64; 8],
}

/// arm64's struct kvm_vcpu_events (api.rst 4.31).
#[repr(C)]
#[derive(Debug, Default)]
struct kvm_vcpu_events {
    serror_pending: u8,
    serror_has_esr: u8,
    ext_dabt_pending: u8,
    pad: [u8; 5],
    serror_esr: u64,
    reserved: [u32; 12],
}

/// A virtual SError pending in a vCPU, which KVM_GET_VCPU_EVENTS shows (api.rst 4.31): its
/// syndrome, where it has one. KVM injects one for the host's RAS errors, and none is
/// otherwise in any register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SError {
    pub esr: Option<u64>,
}

impl VmFd {
    /// Enables the VM's capability `cap`, without arguments (KVM_ENABLE_CAP, api.rst 4.37).
    pub fn enable_cap(&self, cap: u32) -> io::Result<()> {
        let c = kvm_enable_cap {
            cap,
            ..Default::default()
        };
        // SAFETY: KVM_ENABLE_CAP reads the kvm_enable_cap it is given.
        unsafe { ioctl(self.fd(), KVM_ENABLE_CAP, (&raw const c) as libc::c_ulong) }.map(drop)
    }
}
pub const KVM_SET_ONE_REG: u64 = 0x4010_AEAC;
pub const KVM_ARM_VCPU_INIT: u64 = 0x4020_AEAE;
pub const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_AEAF;

const _: () = {
    const fn size(request: u64) -> usize {
        ((request >> 16) & 0x3fff) as usize
    }
    assert!(size(KVM_GET_ONE_REG) == size_of::<kvm_one_reg>());
    assert!(size(KVM_ENABLE_CAP) == size_of::<kvm_enable_cap>());
    assert!(size(KVM_SET_VCPU_EVENTS) == size_of::<kvm_vcpu_events>());
    assert!(size(KVM_GET_VCPU_EVENTS) == size_of::<kvm_vcpu_events>());
    // struct kvm_reg_list's own size: its count, the IDs following it.
    assert!(size(KVM_GET_REG_LIST) == size_of::<u64>());
    assert!(size(KVM_SET_ONE_REG) == size_of::<kvm_one_reg>());
    assert!(size(KVM_ARM_VCPU_INIT) == size_of::<kvm_vcpu_init>());
    assert!(size(KVM_ARM_PREFERRED_TARGET) == size_of::<kvm_vcpu_init>());
};

/// The physical address width KVM can give a VM, or 0 where every VM has 40 bits.
pub const CAP_ARM_VM_IPA_SIZE: u64 = 165;
pub const CAP_ARM_PSCI_0_2: u64 = 102;
pub const CAP_ONE_REG: u64 = 70;
pub const CAP_DEVICE_CTRL: u64 = 89;
pub const CAP_ARM_PTRAUTH_ADDRESS: u64 = 171;
pub const CAP_ARM_PTRAUTH_GENERIC: u64 = 172;
/// vCPUs at EL2, a guest that is itself a hypervisor (Linux 6.16).
pub const CAP_ARM_EL2: u64 = 240;

/// KVM_ARM_VCPU_INIT's features, by bit.
pub const VCPU_POWER_OFF: u32 = 0;
pub const VCPU_PSCI_0_2: u32 = 2;
pub const VCPU_PTRAUTH_ADDRESS: u32 = 5;
pub const VCPU_PTRAUTH_GENERIC: u32 = 6;
pub const VCPU_HAS_EL2: u32 = 7;

/// KVM_CREATE_DEVICE's type of an in-kernel GICv3.
pub const DEV_TYPE_ARM_VGIC_V3: u32 = 7;
/// The GICv3's ITS (devices/arm-vgic-its.rst): KVM_DEV_TYPE_ARM_VGIC_ITS.
pub const DEV_TYPE_ARM_VGIC_ITS: u32 = 8;
/// The GICv3's attribute groups, and in them its addresses and its control.
pub const VGIC_GRP_ADDR: u32 = 0;
pub const VGIC_GRP_NR_IRQS: u32 = 3;
pub const VGIC_GRP_CTRL: u32 = 4;
pub const VGIC_GRP_MAINT_IRQ: u32 = 9;
/// Its registers (arm-vgic-v3.rst): the distributor's and each redistributor's by offset,
/// 32 bits each; each CPU interface's by encoding, 64 bits; and the input level of 32
/// interrupts' lines.
pub const VGIC_GRP_DIST_REGS: u32 = 1;
pub const VGIC_GRP_REDIST_REGS: u32 = 5;
pub const VGIC_GRP_CPU_SYSREGS: u32 = 6;
pub const VGIC_GRP_LEVEL_INFO: u32 = 7;
/// KVM_GET_MP_STATE's power state of a vCPU PSCI has on (api.rst 4.38).
pub const MP_STATE_RUNNABLE: u32 = 0;
pub const VGIC_V3_ADDR_TYPE_DIST: u64 = 2;
pub const VGIC_V3_ADDR_TYPE_REDIST: u64 = 3;
/// The ITS's register frame, 128 KiB on a 64 KiB boundary (KVM_VGIC_ITS_ADDR_TYPE).
pub const VGIC_ITS_ADDR_TYPE: u64 = 4;
pub const VGIC_CTRL_INIT: u64 = 0;
/// Its distributor's frame, and each vCPU's redistributor's two frames.
pub const VGIC_V3_DIST_SIZE: u64 = 64 << 10;
pub const VGIC_V3_REDIST_SIZE: u64 = 2 * (64 << 10);

/// KVM_IRQ_LINE's `irq` on arm64: its type (bits 27:24) and number (15:0); an SPI's
/// number is its INTID.
pub const IRQ_TYPE_SHIFT: u32 = 24;
pub const IRQ_TYPE_SPI: u32 = 1;
pub const IRQ_NUM_MASK: u32 = 0xffff;

/// A register's ID (api.rst 4.68, KVM_SET_ONE_REG): arm64's, its size, and where it is.
const REG_ARM64: u64 = 0x6000_0000_0000_0000;
const REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
const REG_ARM_CORE: u64 = 0x0010 << 16;
const REG_ARM64_SYSREG: u64 = 0x0013 << 16;

/// A core register (`struct kvm_regs`) by its offset in 32-bit words.
const fn core(offset_words: u64) -> u64 {
    REG_ARM64 | REG_SIZE_U64 | REG_ARM_CORE | offset_words
}

/// `regs.regs[n]`, X0 to X30.
pub const fn reg_x(n: u8) -> u64 {
    core(2 * n as u64)
}
/// `regs.pc`, after the 31 general registers and SP.
pub const REG_PC: u64 = core(2 * 32);
/// PSTATE, after PC in struct user_pt_regs.
pub const REG_PSTATE: u64 = core(2 * 33);

/// The system register of encoding `enc` (op0:op1:CRn:CRm:op2, as arch::aarch64::sysreg).
pub const fn sysreg(enc: u16) -> u64 {
    REG_ARM64 | REG_SIZE_U64 | REG_ARM64_SYSREG | enc as u64
}

/// The guest's virtual and physical counters (KVM_REG_ARM_TIMER_CNT, whose encoding is
/// CNTV_CVAL_EL0's, the uapi's swap; KVM_REG_ARM_PTIMER_CNT, CNTPCT_EL0's): a write sets
/// the VM's one offset of that counter from the host's (sys_regs.c, arch_timer_set_user).
pub const REG_TIMER_CNT: u64 = sysreg(0xdf1a);
pub const REG_PTIMER_CNT: u64 = sysreg(0xdf01);

/// A register's size in bytes, which its ID holds as a power of two (KVM_REG_SIZE).
pub const fn reg_size(id: u64) -> usize {
    1 << ((id >> 52) & 0xf)
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_vcpu_init {
    pub target: u32,
    pub features: [u32; 7],
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct kvm_one_reg {
    pub id: u64,
    pub addr: u64,
}

const _: () = {
    assert!(size_of::<kvm_vcpu_init>() == 32);
    assert!(size_of::<kvm_one_reg>() == 16);
};

impl VmFd {
    /// The vCPU target and features KVM prefers for this host's CPUs.
    pub fn preferred_target(&self) -> io::Result<kvm_vcpu_init> {
        let mut init = kvm_vcpu_init::default();
        // SAFETY: KVM fills one kvm_vcpu_init.
        unsafe {
            ioctl(
                self.fd(),
                KVM_ARM_PREFERRED_TARGET,
                (&raw mut init) as libc::c_ulong,
            )
        }?;
        Ok(init)
    }
}

impl DeviceFd {
    /// Reads attribute `attr` of `group`, of the attribute's own type: a GICv3 register.
    pub fn get_attr<T: Default>(&self, group: u32, attr: u64) -> io::Result<T> {
        super::get_attr(self.0.as_raw_fd(), group, attr)
    }

    /// Sets an attribute that takes no value: a control (the GICv3's initialization).
    pub fn control(&self, group: u32, attr: u64) -> io::Result<()> {
        let a = super::kvm_device_attr {
            group,
            attr,
            ..Default::default()
        };
        // SAFETY: a kvm_device_attr with no value for KVM to read.
        unsafe {
            ioctl(
                self.0.as_raw_fd(),
                KVM_SET_DEVICE_ATTR,
                (&raw const a) as libc::c_ulong,
            )
        }
        .map(drop)
    }
}

impl VcpuFd {
    /// Initializes the vCPU as `init` says: its target and features, before it runs or
    /// any of its registers are reached (api.rst 4.82).
    pub fn vcpu_init(&self, init: &kvm_vcpu_init) -> io::Result<()> {
        // SAFETY: KVM reads one kvm_vcpu_init.
        unsafe { ioctl(self.fd(), KVM_ARM_VCPU_INIT, init as *const _ as libc::c_ulong) }.map(drop)
    }

    /// Reads the 64-bit register `id`.
    /// An external data abort pending in the vCPU, at the address of the access its last
    /// exit stopped at (api.rst 4.32: after KVM_EXIT_MMIO or KVM_EXIT_ARM_NISV).
    pub fn inject_ext_dabt(&self) -> io::Result<()> {
        let events = kvm_vcpu_events {
            ext_dabt_pending: 1,
            ..Default::default()
        };
        // SAFETY: KVM_SET_VCPU_EVENTS reads the kvm_vcpu_events it is given.
        unsafe {
            ioctl(
                self.fd(),
                KVM_SET_VCPU_EVENTS,
                (&raw const events) as libc::c_ulong,
            )
        }
        .map(drop)
    }

    pub fn get_reg(&self, id: u64) -> io::Result<u64> {
        let mut value = 0u64;
        let reg = kvm_one_reg {
            id,
            addr: (&raw mut value) as u64,
        };
        // SAFETY: a kvm_one_reg whose `addr` is a live u64, the register's size.
        unsafe { ioctl(self.fd(), KVM_GET_ONE_REG, (&raw const reg) as libc::c_ulong) }?;
        Ok(value)
    }

    /// Writes the 64-bit register `id`.
    pub fn set_reg(&self, id: u64, value: u64) -> io::Result<()> {
        let reg = kvm_one_reg {
            id,
            addr: (&raw const value) as u64,
        };
        // SAFETY: a kvm_one_reg whose `addr` is a live u64, the register's size.
        unsafe { ioctl(self.fd(), KVM_SET_ONE_REG, (&raw const reg) as libc::c_ulong) }.map(drop)
    }

    /// Every register of this vCPU KVM gives a user, by ID (KVM_GET_REG_LIST, api.rst
    /// 4.84): asked first for none, which KVM answers with E2BIG and the count.
    pub fn reg_list(&self) -> io::Result<Vec<u64>> {
        let mut list = vec![0u64];
        loop {
            // SAFETY: a kvm_reg_list of `list[0]` IDs after its count, which KVM fills
            // up to that count.
            match unsafe { ioctl(self.fd(), KVM_GET_REG_LIST, list.as_mut_ptr() as libc::c_ulong) } {
                Ok(_) => {
                    let n = usize::try_from(list.first().copied().unwrap_or(0)).map_err(io::Error::other)?;
                    return Ok(list.get(1..=n).map(<[u64]>::to_vec).unwrap_or_default());
                }
                Err(e) if e.raw_os_error() == Some(libc::E2BIG) => {
                    let n = usize::try_from(list.first().copied().unwrap_or(0)).map_err(io::Error::other)?;
                    // Registers come and go only as the vCPU is initialized; a count that
                    // grows past any arm64 vCPU's is not one.
                    if n == 0 || n > MAX_REGS {
                        return Err(io::Error::other(format!("KVM_GET_REG_LIST counts {n} registers")));
                    }
                    list = vec![0u64; n + 1];
                    if let Some(count) = list.first_mut() {
                        *count = n as u64;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Reads register `id`, of 16 bytes at most (a V register's), as a little-endian value.
    pub fn get_reg_value(&self, id: u64) -> io::Result<u128> {
        let size = reg_size(id);
        if size > 16 {
            return Err(io::Error::other(format!("register {id:#x}: {size} bytes")));
        }
        let mut value = 0u128;
        let reg = kvm_one_reg {
            id,
            addr: (&raw mut value) as u64,
        };
        // SAFETY: a kvm_one_reg whose `addr` is a live u128, at least the register's size,
        // of which KVM writes the register's bytes.
        unsafe { ioctl(self.fd(), KVM_GET_ONE_REG, (&raw const reg) as libc::c_ulong) }?;
        Ok(value)
    }

    /// Writes register `id` from a little-endian value of its size ([`get_reg_value`]).
    ///
    /// [`get_reg_value`]: Self::get_reg_value
    pub fn set_reg_value(&self, id: u64, value: u128) -> io::Result<()> {
        let size = reg_size(id);
        if size > 16 {
            return Err(io::Error::other(format!("register {id:#x}: {size} bytes")));
        }
        let reg = kvm_one_reg {
            id,
            addr: (&raw const value) as u64,
        };
        // SAFETY: a kvm_one_reg whose `addr` is a live u128, at least the register's size,
        // of which KVM reads the register's bytes.
        unsafe { ioctl(self.fd(), KVM_SET_ONE_REG, (&raw const reg) as libc::c_ulong) }.map(drop)
    }

    /// Its power state: [`MP_STATE_RUNNABLE`], or KVM_MP_STATE_STOPPED, off (api.rst 4.38).
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

    /// The virtual SError pending in the vCPU, if one is (api.rst 4.31).
    pub fn serror(&self) -> io::Result<Option<SError>> {
        let mut events = kvm_vcpu_events::default();
        // SAFETY: KVM_GET_VCPU_EVENTS fills the kvm_vcpu_events it is given.
        unsafe { ioctl(self.fd(), KVM_GET_VCPU_EVENTS, (&raw mut events) as libc::c_ulong) }?;
        Ok((events.serror_pending != 0).then_some(SError {
            esr: (events.serror_has_esr != 0).then_some(events.serror_esr),
        }))
    }

    /// Makes `serror` pending in the vCPU (api.rst 4.32).
    pub fn set_serror(&self, serror: SError) -> io::Result<()> {
        let events = kvm_vcpu_events {
            serror_pending: 1,
            serror_has_esr: u8::from(serror.esr.is_some()),
            serror_esr: serror.esr.unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: KVM_SET_VCPU_EVENTS reads the kvm_vcpu_events it is given.
        unsafe {
            ioctl(
                self.fd(),
                KVM_SET_VCPU_EVENTS,
                (&raw const events) as libc::c_ulong,
            )
        }
        .map(drop)
    }

    /// A handle that does not keep the vCPU: for a thread that reaches it while its own
    /// thread waits (the machine's release, its prefetch).
    pub fn weak(&self) -> WeakVcpu {
        WeakVcpu {
            fd: std::sync::Arc::downgrade(&self.fd),
            run: std::sync::Arc::downgrade(&self.run),
        }
    }
}

/// More registers than any arm64 vCPU has: a reg list's count past it is no vCPU's.
const MAX_REGS: usize = 4096;

/// A vCPU's descriptor and its `kvm_run`, not kept alive by this handle ([`VcpuFd::weak`]).
#[derive(Debug, Clone)]
pub struct WeakVcpu {
    fd: std::sync::Weak<std::os::fd::OwnedFd>,
    run: std::sync::Weak<super::RunMap>,
}

impl WeakVcpu {
    /// The vCPU, while it lives: its ioctls, KVM_RUN among them, from this thread.
    pub fn upgrade(&self) -> Option<VcpuFd> {
        Some(VcpuFd {
            fd: self.fd.upgrade()?,
            run: self.run.upgrade()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register IDs as the uapi's macros make them: X0, X30 and PC by their offsets in
    /// `struct kvm_regs`, and MPIDR_EL1 by its encoding (ARM64_SYS_REG(3, 0, 0, 0, 5)).
    #[test]
    fn register_ids_are_the_uapis() {
        assert_eq!(reg_x(0), 0x6030_0000_0010_0000);
        assert_eq!(reg_x(30), 0x6030_0000_0010_003c);
        assert_eq!(REG_PC, 0x6030_0000_0010_0040);
        assert_eq!(sysreg(0xc005), 0x6030_0000_0013_c005);
    }
}
