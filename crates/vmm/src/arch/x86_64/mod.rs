//! x86_64 guest platform: physical memory map, boot protocol, firmware tables and CPU
//! identity, all described architecturally so every x86 backend (KVM, WHP, HVF) can
//! apply them. Ground truth: docs/research/kvm-x86_64-ground-truth.md.

pub mod acpi;
pub mod boot;
pub mod cpuid;

/// Guest-physical memory map (research doc §6.4, after Firecracker's layout).
pub mod layout {
    /// Boot GDT (4 entries) and an empty IDT.
    pub const GDT: u64 = 0x500;
    pub const IDT: u64 = 0x520;
    /// `struct boot_params` (the zero page).
    pub const ZERO_PAGE: u64 = 0x7000;
    /// Identity map of [0, 1 GiB) with 2 MiB pages.
    pub const PML4: u64 = 0x9000;
    pub const PDPT: u64 = 0xa000;
    pub const PD: u64 = 0xb000;
    /// Kernel command line; the kernel copies COMMAND_LINE_SIZE = 2048 bytes from here.
    pub const CMDLINE: u64 = 0x2_0000;
    pub const CMDLINE_MAX: usize = 2048;
    /// Firmware tables (reserved in e820): ACPI tables from here up to the RSDP.
    pub const SYSTEM: u64 = 0x9_fc00;
    /// The VM generation ID's 16 bytes, first in the firmware area (devices/vmgenid.rs).
    pub const VMGENID: u64 = SYSTEM;
    pub const RSDP: u64 = 0xe_0000;
    /// Where e820 RAM resumes, and the lowest a kernel segment may load.
    pub const HIMEM: u64 = 0x10_0000;
    /// The boot page tables map this much; kernel, zero page and command line live below.
    pub const IDENTITY_MAPPED: u64 = 1 << 30;
    /// The 32-bit MMIO gap: RAM stops here and resumes at 4 GiB.
    pub const MMIO_GAP: u64 = 0xc000_0000;
    pub const MMIO_GAP_END: u64 = 1 << 32;
    /// The shards control page, first in the gap (crates/abi).
    pub const CONTROL: u64 = shards_abi::CONTROL_PAGE;
    /// virtio-mmio transports, `VIRTIO_MMIO_STRIDE` apart, on GSIs from `GSI_VIRTIO`.
    pub const VIRTIO_MMIO: u64 = 0xc000_1000;
    pub const VIRTIO_MMIO_STRIDE: u64 = 0x200;
    /// GSIs 5-22 are free IOAPIC pins: at most 18 virtio devices.
    pub const VIRTIO_MMIO_MAX: u64 = 18;
    /// KVM's in-kernel IOAPIC and LAPIC.
    pub const IOAPIC: u64 = 0xfec0_0000;
    pub const LAPIC: u64 = 0xfee0_0000;
    /// Pages KVM reserves on Intel for the real-mode identity map and TSS.
    pub const IDENTITY_MAP: u64 = 0xfffb_c000;
    pub const TSS: u64 = 0xfffb_d000;

    /// COM1: ports 0x3F8-0x3FF on GSI 4.
    pub const COM1: u16 = 0x3f8;
    pub const GSI_COM1: u32 = 4;
    pub const GSI_VIRTIO: u32 = 5;
    /// The Generic Event Device's, which tells the guest of a new VM generation ID: the
    /// IOAPIC's last pin.
    pub const GSI_GED: u32 = 23;
    /// The i8042 data and command ports (only the reset command is emulated).
    pub const I8042: u16 = 0x60;
    /// ACPI SLEEP_CONTROL_REG and SLEEP_STATUS_REG (HW-reduced ACPI), a free port pair.
    pub const ACPI_SLEEP: u16 = 0x600;
}

/// A segment register as VM entry wants it (Intel SDM Vol. 3A §3.4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub base: u64,
    /// Byte-granular limit.
    pub limit: u32,
    pub selector: u16,
    /// Descriptor type (bits 3:0 of the access byte).
    pub kind: u8,
    /// Code or data (true), or system (false).
    pub code_or_data: bool,
    pub present: bool,
    /// Default operand size 32-bit.
    pub db: bool,
    /// 64-bit code.
    pub long: bool,
    /// Limit in 4 KiB units.
    pub granular: bool,
}

/// The boot vCPU's state for the Linux 64-bit boot protocol (boot.rst "64-bit BOOT
/// PROTOCOL"). Application processors start in wait-for-SIPI and need none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub rip: u64,
    /// `struct boot_params`, in %rsi.
    pub rsi: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub cs: Segment,
    /// DS, ES, FS, GS and SS.
    pub data: Segment,
    pub tr: Segment,
    pub gdt: (u64, u16),
    pub idt: (u64, u16),
}
