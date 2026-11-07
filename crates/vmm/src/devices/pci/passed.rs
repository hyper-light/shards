//! What the VMM keeps of a host's PCI function passed through to a guest, whoever passes it
//! (D154): Linux's VFIO (`vfio.rs`) or Windows' Discrete Device Assignment (`assigned.rs`).
//! The registers it keeps, its capabilities as the device lists them, its BARs as the
//! device sizes them, and the parts of each BAR the guest reaches without exits.

use super::{Bar, BarKind, MsiCap, MsixCap, reg};

/// Power Management's capability ID (PCI Bus Power Management Interface 1.2 §3.2.1), and
/// its control/status register's place from the capability's start, whose bits 1:0 are
/// the function's power state, D0 to D3hot (§3.2.4).
pub const CAP_PM: u8 = 0x01;
pub const PM_CONTROL: u16 = 4;
pub const PM_STATE: u8 = 0b11;

/// Whether `offset` is a register the VMM keeps of a function whose MSI and MSI-X
/// capabilities are at `caps`' places, of their lengths: the BARs, the expansion ROM, the
/// interrupt line and pin (with PCIe's hardwired-zero Min_Gnt and Max_Lat beside them),
/// and the capabilities.
pub fn kept(offset: u16, caps: &[(u16, u16)]) -> bool {
    matches!(offset, 0x10..=0x27 | 0x30..=0x33 | 0x3c..=0x3f)
        || caps.iter().any(|&(at, len)| (at..at + len).contains(&offset))
}

/// Whether `len` bytes at `place`, a BAR's index and an offset in it, lie in a memory BAR
/// of `bars`.
pub fn in_bars(bars: &[Bar], (index, offset): (u8, u32), len: u64) -> bool {
    bars.iter().any(|bar| {
        bar.index == usize::from(index) && bar.kind != BarKind::Io && u64::from(offset) + len <= bar.size
    })
}

/// The parts of BAR `index`, `len` bytes of the device's region, that the guest can reach
/// without exits, each an offset and a length: all of its pages but those of the MSI-X
/// table and pending bits, which the VMM keeps; none where it is less than a page, as the
/// page it would take in the guest holds other BARs.
pub fn windows_of(index: usize, len: u64, msix: Option<MsixCap>, page: u64) -> Vec<(u64, u64)> {
    if len < page || !len.is_multiple_of(page) {
        return Vec::new();
    }
    let mut kept: Vec<(u64, u64)> = Vec::new();
    if let Some(cap) = msix {
        let size = u64::from(cap.size);
        for ((bar, offset), bytes) in [
            (cap.table, size * reg::MSIX_ENTRY),
            (cap.pba, size.div_ceil(64) * 8),
        ] {
            if usize::from(bar) == index {
                let start = u64::from(offset) / page * page;
                let end = (u64::from(offset) + bytes).div_ceil(page) * page;
                kept.push((start, end.min(len)));
            }
        }
    }
    kept.sort_unstable();
    let mut out = Vec::new();
    let mut at = 0;
    for (start, end) in kept {
        if start > at {
            out.push((at, start - at));
        }
        at = at.max(end);
    }
    if at < len {
        out.push((at, len - at));
    }
    out
}

/// Where capability `id` is in the list (PCI Local Bus 3.0 §6.7) of a function whose
/// configuration register at an offset, of a length (at most 4 bytes), `config` reads.
pub fn cap_of(config: &dyn Fn(u16, usize) -> u32, id: u8) -> Option<u16> {
    if config(reg::STATUS, 2) as u16 & reg::STATUS_CAPABILITIES == 0 {
        return None;
    }
    let mut at = config(reg::CAPABILITIES, 1) as u16 & 0xfc;
    // The list lives in the header's 192 bytes past its first 64: 48 entries at most.
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        if config(at, 1) as u8 == id {
            return Some(at);
        }
        at = config(at + 1, 1) as u16 & 0xfc;
    }
    None
}

/// Its MSI capability (§6.8.1): where it is, the next, its addresses' width, and its
/// length, with per-vector masking's registers where it has them.
pub fn msi_cap_of(config: &dyn Fn(u16, usize) -> u32) -> Option<(MsiCap, u16)> {
    let at = cap_of(config, reg::CAP_MSI)?;
    let next = config(at + 1, 1) as u8 & 0xfc;
    let control = config(at + reg::MSI_CONTROL, 2) as u16;
    let is64 = control & reg::MSI_64BIT != 0;
    let masking = control & reg::MSI_MASKING != 0;
    let len = 10 + if is64 { 4 } else { 0 } + if masking { 10 } else { 0 };
    Some((MsiCap { at, next, is64 }, len))
}

/// Its MSI-X capability (§6.8.2): where it is, the next, its table's size, and its
/// table's and pending bits' BARs and offsets.
pub fn msix_cap_of(config: &dyn Fn(u16, usize) -> u32) -> Option<MsixCap> {
    let at = cap_of(config, reg::CAP_MSIX)?;
    let next = config(at + 1, 1) as u8 & 0xfc;
    let control = config(at + reg::MSIX_CONTROL, 2) as u16;
    let place = |register: u16| {
        let value = config(at + register, 4);
        ((value & 7) as u8, value & !7)
    };
    Some(MsixCap {
        at,
        next,
        size: (control & 0x7ff) + 1,
        table: place(reg::MSIX_TABLE),
        pba: place(reg::MSIX_PBA),
    })
}

/// The memory BARs of a type 0 function whose BAR registers read `probed` once all ones
/// were written to each (PCI Local Bus 3.0 §6.2.5.1): each BAR's index, size and kind. A
/// 64-bit BAR takes two registers, its size's mask in both; a register that reads 0 holds
/// no BAR; an I/O BAR is left out, as the machine has no I/O window.
pub fn sized_bars(probed: [u32; 6]) -> Vec<(usize, u64, BarKind)> {
    let mut out = Vec::new();
    let mut index = 0;
    while let Some(&low) = probed.get(index) {
        let wide = low & 1 == 0 && (low >> 1) & 3 == 2;
        let mask = if low & 1 == 1 {
            0
        } else if wide {
            let high = probed.get(index + 1).copied().unwrap_or(0);
            u64::from(high) << 32 | u64::from(low & !0xf)
        } else if low & !0xf == 0 {
            0
        } else {
            0xffff_ffff_0000_0000 | u64::from(low & !0xf)
        };
        // The size is the mask's lowest bit set (§6.2.5.1: the bits a BAR's size keeps
        // zero read back as zero).
        let size = mask.isolate_lowest_one();
        if size >= 16 {
            let kind = if wide {
                BarKind::Memory64
            } else {
                BarKind::Memory32
            };
            out.push((index, size, kind));
        }
        index += if wide { 2 } else { 1 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VMM keeps the BARs, the expansion ROM, the interrupt line and pin (with PCIe's
    /// hardwired-zero Min_Gnt and Max_Lat beside them) and the MSI and MSI-X
    /// capabilities, all of them; every other register is the device's.
    #[test]
    fn the_vmm_keeps_the_bars_rom_interrupt_registers_msi_and_msix() {
        let caps = [(0x50, 14), (0x70, reg::MSIX_LEN)];
        for offset in [
            0x10, 0x14, 0x24, 0x27, 0x30, 0x33, 0x3c, 0x3d, 0x3e, 0x3f, 0x50, 0x5d, 0x70, 0x7b,
        ] {
            assert!(kept(offset, &caps), "{offset:#x}");
        }
        for offset in [
            0x00, 0x04, 0x06, 0x0f, 0x28, 0x2c, 0x34, 0x38, 0x40, 0x4f, 0x5e, 0x6f, 0x7c, 0x100,
        ] {
            assert!(!kept(offset, &caps), "{offset:#x}");
        }
        assert!(!kept(0x50, &[]));
    }

    /// A BAR's pages reach the guest without exits but for those of the MSI-X table and
    /// pending bits, which the VMM keeps; a BAR of less than a page stays trapped whole.
    #[test]
    fn a_bar_is_mapped_but_for_its_msix_pages() {
        let page = 0x1000;
        let msix = |table: (u8, u32), pba: (u8, u32), size: u16| {
            Some(MsixCap {
                at: 0x70,
                next: 0,
                size,
                table,
                pba,
            })
        };
        assert_eq!(windows_of(0, 0x10_0000, None, page), vec![(0, 0x10_0000)]);
        assert_eq!(windows_of(0, 0x800, None, page), vec![], "under a page");
        // Two vectors and their pending bits in one page: none of it mapped.
        assert_eq!(windows_of(2, 0x1000, msix((2, 0), (2, 0x800), 2), page), vec![]);
        // In another BAR: all of this one.
        assert_eq!(
            windows_of(0, 0x4000, msix((2, 0), (2, 0x800), 2), page),
            vec![(0, 0x4000)]
        );
        // A table of 512 vectors (8 KiB) at 0x2000, its pending bits at 0x6008: the
        // pages around each mapped.
        assert_eq!(
            windows_of(4, 0x10000, msix((4, 0x2000), (4, 0x6008), 512), page),
            vec![(0, 0x2000), (0x4000, 0x2000), (0x7000, 0x9000)]
        );
    }

    /// MSI-X's table and pending bits are the VMM's only where they lie in a memory BAR
    /// the function has.
    #[test]
    fn an_msix_table_is_kept_only_within_a_bar() {
        let bars = [
            Bar {
                index: 0,
                addr: 0x1000_0000,
                size: 0x4000,
                kind: BarKind::Memory32,
            },
            Bar {
                index: 2,
                addr: 0x1000,
                size: 0x100,
                kind: BarKind::Io,
            },
        ];
        assert!(in_bars(&bars, (0, 0x2000), 0x2000));
        assert!(!in_bars(&bars, (0, 0x2000), 0x2001));
        assert!(!in_bars(&bars, (1, 0), 16));
        assert!(!in_bars(&bars, (2, 0), 16), "an I/O BAR");
    }

    /// BARs sized from what their registers read back with all ones written: a 32-bit BAR
    /// of 16 KiB, a 64-bit prefetchable one of 16 GiB in registers 2 and 3, an I/O BAR left
    /// out, and registers that hold no BAR; a register of a mask too narrow for memory, none.
    #[test]
    fn bars_are_sized_as_their_registers_read_back() {
        let probed = [
            0xffff_c000, // 32-bit memory, 16 KiB
            0xffff_ff01, // I/O, 256 bytes
            0x0000_000c, // 64-bit prefetchable memory, its low address bits all zero
            0xffff_fffc, // its upper half: 16 GiB
            0,           // none
            0xffff_fff0, // 32-bit memory, 16 bytes
        ];
        assert_eq!(
            sized_bars(probed),
            vec![
                (0, 0x4000, BarKind::Memory32),
                (2, 0x4_0000_0000, BarKind::Memory64),
                (5, 0x10, BarKind::Memory32),
            ]
        );
        assert_eq!(sized_bars([0; 6]), vec![]);
        // A 64-bit BAR in the last register has no upper half: its size is past the mask.
        assert_eq!(sized_bars([0, 0, 0, 0, 0, 0x0000_0004]), vec![]);
    }

    /// Capabilities found as a function's list holds them: MSI with 64-bit addresses and
    /// per-vector masking at 0x50, then MSI-X at 0x70, its table in BAR 4 and its pending
    /// bits after it, then Power Management at 0x80, the last.
    #[test]
    fn capabilities_are_found_in_the_list() {
        let mut space = [0u8; 256];
        space[usize::from(reg::STATUS)] = reg::STATUS_CAPABILITIES as u8;
        space[usize::from(reg::CAPABILITIES)] = 0x50;
        space[0x50..0x54].copy_from_slice(&[reg::CAP_MSI, 0x70, 0x80, 0x01]);
        space[0x70..0x74].copy_from_slice(&[reg::CAP_MSIX, 0x80, 0x1f, 0x00]);
        space[0x74..0x78].copy_from_slice(&0x2004u32.to_le_bytes());
        space[0x78..0x7c].copy_from_slice(&0x3004u32.to_le_bytes());
        space[0x80..0x82].copy_from_slice(&[CAP_PM, 0x00]);
        let config = |at: u16, len: usize| {
            let at = usize::from(at);
            space
                .get(at..at + len)
                .map_or(0, |b| crate::devices::get_le(b) as u32)
        };
        assert_eq!(
            msi_cap_of(&config),
            Some((
                MsiCap {
                    at: 0x50,
                    next: 0x70,
                    is64: true
                },
                24
            ))
        );
        assert_eq!(
            msix_cap_of(&config),
            Some(MsixCap {
                at: 0x70,
                next: 0x80,
                size: 32,
                table: (4, 0x2000),
                pba: (4, 0x3000),
            })
        );
        assert_eq!(cap_of(&config, CAP_PM), Some(0x80));
        assert_eq!(cap_of(&config, 0x10), None, "PCI Express's: not in the list");
        space[usize::from(reg::STATUS)] = 0;
        let config = |at: u16, len: usize| {
            let at = usize::from(at);
            space
                .get(at..at + len)
                .map_or(0, |b| crate::devices::get_le(b) as u32)
        };
        assert_eq!(cap_of(&config, reg::CAP_MSI), None, "no list");
    }
}
