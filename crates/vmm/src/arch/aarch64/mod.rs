//! AArch64 guest platform: physical memory map, CPU identity, and boot protocol.

pub mod boot;
pub mod esr;
pub mod psci;

/// Guest-physical memory map. Everything below `DRAM_BASE` is MMIO.
pub mod layout {
    /// GICv3 distributor (64 KiB).
    pub const GIC_DIST: u64 = 0x0800_0000;
    /// GICv2m-style MSI frame (64 KiB), reserved for PCI MSI.
    pub const GIC_MSI: u64 = 0x0802_0000;
    /// Redistributors: one 128 KiB frame per vCPU, contiguous.
    pub const GIC_REDIST: u64 = 0x080a_0000;
    pub const GIC_REDIST_MAX_END: u64 = 0x0900_0000;
    /// 16550 UART (4 KiB).
    pub const UART: u64 = 0x0900_0000;
    /// PL031 RTC (4 KiB).
    pub const RTC: u64 = 0x0901_0000;
    /// shards control page: boot-time markers written by the guest.
    pub const CONTROL: u64 = shards_abi::CONTROL_PAGE_AARCH64;
    /// virtio-mmio transports, `VIRTIO_MMIO_STRIDE` apart.
    pub const VIRTIO_MMIO: u64 = 0x0a00_0000;
    pub const VIRTIO_MMIO_STRIDE: u64 = 0x200;
    pub const VIRTIO_MMIO_MAX: u64 = 32;
    pub const DRAM_BASE: u64 = 0x8000_0000;

    /// SPI numbers (INTID = 32 + SPI), as written in devicetree interrupt specifiers.
    pub const SPI_UART: u32 = 1;
    pub const SPI_RTC: u32 = 2;
    pub const SPI_VIRTIO_MMIO: u32 = 16;
}

/// Where a vCPU enters the guest: the kernel entry with X0 = DTB address for the boot
/// vCPU (Linux booting.rst), or PSCI CPU_ON's entry point with X0 = its context ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub pc: u64,
    pub x0: u64,
}

/// The MPIDR affinity of vCPU `index`: 16 CPUs per Aff0 cluster (as KVM does), so SGI
/// target lists never need the GICv3.1 range selector.
pub const fn mpidr(index: u32) -> u64 {
    ((index as u64 / 16) << 8) | (index as u64 % 16)
}

/// System register encodings (op0:op1:CRn:CRm:op2 packed as HVF's `hv_sys_reg_t` and
/// the low 16 bits of KVM's `ARM64_SYS_REG`).
pub mod sysreg {
    pub const fn enc(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
        (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
    }
    pub const MPIDR_EL1: u16 = enc(3, 0, 0, 0, 5);
    pub const ID_AA64PFR1_EL1: u16 = enc(3, 0, 0, 4, 1);
    pub const ID_AA64MMFR0_EL1: u16 = enc(3, 0, 0, 7, 0);
    pub const SCTLR_EL1: u16 = enc(3, 0, 1, 0, 0);
    pub const CNTV_CTL_EL0: u16 = enc(3, 3, 14, 3, 1);
    pub const CNTV_CVAL_EL0: u16 = enc(3, 3, 14, 3, 2);

    /// SCTLR_EL1 with MMU and caches off: only the RES1 bits (Arm ARM D19.2.118).
    pub const SCTLR_EL1_RESET: u64 = 0x30d0_0800;
    /// PSTATE for entry at EL1h with D, A, I, F masked (Linux booting.rst).
    pub const PSTATE_EL1H_DAIF: u64 = 0x3c5;
}

/// ID_AA64MMFR0_EL1.PARange encoding for a physical address width, if architected.
pub fn parange_for_bits(bits: u32) -> Option<u64> {
    Some(match bits {
        32 => 0,
        36 => 1,
        40 => 2,
        42 => 3,
        44 => 4,
        48 => 5,
        52 => 6,
        56 => 7,
        _ => return None,
    })
}
