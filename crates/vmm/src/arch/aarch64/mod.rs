//! AArch64 guest platform: physical memory map, CPU identity, and boot protocol.

pub mod boot;
pub mod esr;
pub mod prefetch;
pub mod psci;
pub mod state;

/// Guest-physical memory map. Everything below `DRAM_BASE` is MMIO.
pub mod layout {
    /// GICv3 distributor (64 KiB).
    pub const GIC_DIST: u64 = 0x0800_0000;
    /// A GICv2m frame (64 KiB), its doorbell taking the MSIs of a PCI bus's functions
    /// (D154), where the GIC has no ITS.
    pub const GIC_MSI: u64 = 0x0802_0000;
    /// An ITS's register frame (128 KiB, on a 64 KiB boundary), taking the MSIs of a PCI
    /// bus's functions where the GIC has one (D154): QEMU virt's place.
    pub const GIC_ITS: u64 = 0x0808_0000;
    pub const GIC_ITS_SIZE: u64 = 0x2_0000;
    /// Redistributors: one 128 KiB frame per vCPU, contiguous.
    pub const GIC_REDIST: u64 = 0x080a_0000;
    pub const GIC_REDIST_MAX_END: u64 = 0x0900_0000;
    /// 16550 UART (4 KiB).
    pub const UART: u64 = 0x0900_0000;
    /// PL031 RTC (4 KiB).
    pub const RTC: u64 = 0x0901_0000;
    /// The shards control page ([`shards_abi::control`]).
    pub const CONTROL: u64 = shards_abi::CONTROL_PAGE;
    /// virtio-mmio transports, `VIRTIO_MMIO_STRIDE` apart.
    pub const VIRTIO_MMIO: u64 = 0x0a00_0000;
    pub const VIRTIO_MMIO_STRIDE: u64 = 0x200;
    pub const VIRTIO_MMIO_MAX: u64 = 32;
    /// Where the VMM maps a working set's prefetch loop while it runs, before the guest
    /// does (hv::Vcpu::prefetch): 1 GiB between the devices and RAM that nothing else uses.
    pub const PREFETCH: u64 = 0x4000_0000;
    pub const DRAM_BASE: u64 = 0x8000_0000;
    /// A PCI bus's 32-bit memory window, where its functions' BARs are placed, and its
    /// ECAM window above it, bus 0 alone (D153): QEMU virt's places.
    pub const PCI_MEM32: u64 = 0x1000_0000;
    pub const PCI_MEM32_SIZE: u64 = 0x2f00_0000;
    pub const PCI_ECAM: u64 = PCI_MEM32 + PCI_MEM32_SIZE;
    /// The first 2 MiB of DRAM hold data the VMM writes for guest drivers that map it as
    /// a device (VMGenID). Left out of the devicetree's memory node, so Linux can
    /// ioremap it; the kernel loads above it, 2 MiB aligned (Firecracker's layout:
    /// arch/aarch64/layout.rs SYSTEM_MEM_*, fdt.rs create_memory_node).
    pub const SYSTEM_MEM_SIZE: u64 = 0x20_0000;
    /// The 16-byte VM generation ID (drivers/virt/vmgenid.c).
    pub const VMGENID: u64 = DRAM_BASE;
    pub const VMGENID_SIZE: u64 = 16;
    /// Where the kernel's 2 MiB-aligned base goes.
    pub const KERNEL_BASE: u64 = DRAM_BASE + SYSTEM_MEM_SIZE;

    /// SPI numbers (INTID = 32 + SPI), as written in devicetree interrupt specifiers.
    pub const SPI_UART: u32 = 1;
    pub const SPI_RTC: u32 = 2;
    pub const SPI_VMGENID: u32 = 3;
    pub const SPI_VIRTIO_MMIO: u32 = 16;
    /// The INTx line of PCI device `n` (1 to 31) on bus 0 is SPI `SPI_PCI + n - 1`: one
    /// line for each device, none shared (D154).
    pub const SPI_PCI: u32 = SPI_VIRTIO_MMIO + VIRTIO_MMIO_MAX as u32;
    /// The SPIs the GICv2m frame raises for MSIs, `MSI_SPIS` of them from `SPI_MSI`.
    pub const SPI_MSI: u32 = SPI_PCI + 32;
    pub const MSI_SPIS: u32 = 64;
    /// One more than the highest SPI the machine uses.
    pub const SPIS: u32 = SPI_MSI + MSI_SPIS;
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
    pub const ID_AA64ISAR0_EL1: u16 = enc(3, 0, 0, 6, 0);
    pub const SCTLR_EL1: u16 = enc(3, 0, 1, 0, 0);
    pub const TTBR0_EL1: u16 = enc(3, 0, 2, 0, 0);
    pub const TCR_EL1: u16 = enc(3, 0, 2, 0, 2);
    pub const MAIR_EL1: u16 = enc(3, 0, 10, 2, 0);
    pub const CNTV_CTL_EL0: u16 = enc(3, 3, 14, 3, 1);
    pub const CNTV_CVAL_EL0: u16 = enc(3, 3, 14, 3, 2);

    /// SCTLR_EL1 with MMU and caches off: only the RES1 bits (Arm ARM D19.2.118).
    pub const SCTLR_EL1_RESET: u64 = 0x30d0_0800;
    /// SCTLR_EL1.M, .C and .I: the MMU on, data and instruction caches on.
    pub const SCTLR_EL1_MMU_CACHES: u64 = 1 | (1 << 2) | (1 << 12);
    /// PSTATE for entry at EL1h with D, A, I, F masked (Linux booting.rst).
    pub const PSTATE_EL1H_DAIF: u64 = 0x3c5;
    /// PSTATE for entry at EL2h with D, A, I, F masked, where the guest has EL2: the
    /// level booting.rst recommends, the kernel then a hypervisor.
    pub const PSTATE_EL2H_DAIF: u64 = 0x3c9;
    pub const SCTLR_EL2: u16 = enc(3, 4, 1, 0, 0);
    /// ID_AA64PFR1_EL1.SME, bits 27:24: whether the CPU has the Scalable Matrix Extension.
    pub const ID_AA64PFR1_SME: u64 = 0xf << 24;
    /// The EL2 registers Hypervisor.framework holds for a guest that has EL2
    /// (hv_vcpu_types.h, macOS 15): CNTHCTL, CNTHP_CTL, CNTHP_CVAL, CNTHP_TVAL, CNTVOFF,
    /// CPTR, ELR, ESR, FAR, HCR, HPFAR, MAIR, MDCR, SCTLR, SPSR, SP, TCR, TPIDR, TTBR0,
    /// TTBR1, VBAR, VMPIDR, VPIDR, VTCR and VTTBR, each `_EL2`.
    pub const EL2_HELD: [u16; 25] = [
        0xe708, 0xe711, 0xe712, 0xe710, 0xe703, 0xe08a, 0xe201, 0xe290, 0xe300, 0xe088, 0xe304, 0xe510,
        0xe089, 0xe080, 0xe200, 0xf208, 0xe102, 0xe682, 0xe100, 0xe101, 0xe600, 0xe005, 0xe000, 0xe10a,
        0xe108,
    ];
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
