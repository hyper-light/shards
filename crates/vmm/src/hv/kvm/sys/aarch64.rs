//! The KVM ioctls and structures of arm64 alone (arch/arm64/include/uapi/asm/kvm.h;
//! Documentation/virt/kvm/api.rst and devices/arm-vgic-v3.rst).

use std::io;
use std::os::fd::AsRawFd;

use super::{
    DeviceFd, KVM_CHECK_EXTENSION, KVM_CREATE_DEVICE, KVM_CREATE_VCPU, KVM_CREATE_VM, KVM_GET_API_VERSION,
    KVM_GET_DEVICE_ATTR, KVM_GET_MP_STATE, KVM_GET_VCPU_MMAP_SIZE, KVM_IRQ_LINE, KVM_IRQFD, KVM_RUN,
    KVM_SET_DEVICE_ATTR, KVM_SET_MP_STATE, KVM_SET_USER_MEMORY_REGION, VcpuFd, VmFd, ioctl,
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
];

pub const KVM_GET_ONE_REG: u64 = 0x4010_AEAB;
pub const KVM_SET_ONE_REG: u64 = 0x4010_AEAC;
pub const KVM_ARM_VCPU_INIT: u64 = 0x4020_AEAE;
pub const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_AEAF;

const _: () = {
    const fn size(request: u64) -> usize {
        ((request >> 16) & 0x3fff) as usize
    }
    assert!(size(KVM_GET_ONE_REG) == size_of::<kvm_one_reg>());
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
/// The GICv3's attribute groups, and in them its addresses and its control.
pub const VGIC_GRP_ADDR: u32 = 0;
pub const VGIC_GRP_NR_IRQS: u32 = 3;
pub const VGIC_GRP_CTRL: u32 = 4;
pub const VGIC_GRP_MAINT_IRQ: u32 = 9;
pub const VGIC_V3_ADDR_TYPE_DIST: u64 = 2;
pub const VGIC_V3_ADDR_TYPE_REDIST: u64 = 3;
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

/// The system register of encoding `enc` (op0:op1:CRn:CRm:op2, as arch::aarch64::sysreg).
pub const fn sysreg(enc: u16) -> u64 {
    REG_ARM64 | REG_SIZE_U64 | REG_ARM64_SYSREG | enc as u64
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
