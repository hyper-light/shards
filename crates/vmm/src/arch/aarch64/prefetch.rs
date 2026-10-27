//! The program a restored machine's first vCPU runs before its state is restored, touching a
//! working set's pages so that its hypervisor maps them at stage 2 ahead of the guest
//! (REAP; PM M30, M310): each backend maps it at [`layout::PREFETCH`] with its list and its
//! page table, sets the vCPU's registers as [`registers`] says, runs it, and then restores
//! the vCPU's own state over all of it. How it ends is the backend's (`hvc #0` under
//! Hypervisor.framework, which returns it; a write KVM gives back as MMIO under KVM, whose
//! HVCs are its own), the words after [`LOOP`].

use super::{layout, sysreg};
use crate::hv::Touch;

/// The first 512 GiB of guest memory, which [`table`]'s one level of 1 GiB blocks can map.
pub const REACH: u64 = 512 << 30;

/// The page table's place in the program's memory, after its code: 4 KiB aligned, as a
/// 4 KiB granule's table must be.
pub const TABLE: u64 = 4 << 10;

/// Touches the list at X0, X1 entries long, then goes on to the backend's end, the words
/// that follow it. The MMU is on, over an identity map of 1 GiB Normal write-back blocks,
/// so the writes are coherent with the host's view of the memory. An entry is a page's
/// address, with bit 0 set for a write: an atomic add of zero, a write for permission
/// purposes that cannot change the data (Arm ARM, LDADD). `tlbi vmalle1is` last, so that
/// no translation of the map outlives it on any core.
///
/// ```text
///     cbz  x1, 3f            2:  and  x2, x2, #~1
/// 1:  ldr  x2, [x0], #8          staddb wzr, [x2]
///     tbnz x2, #0, 2f            subs x1, x1, #1
///     ldrb w3, [x2]              b.ne 1b
///     subs x1, x1, #1        3:  tlbi vmalle1is
///     b.ne 1b                    dsb  ish
///     b    3f                    isb
/// ```
pub const LOOP: [u32; 14] = [
    0xb400_0161,
    0xf840_8402,
    0x3700_00a2,
    0x3940_0043,
    0xf100_0421,
    0x54ff_ff81,
    0x1400_0005,
    0x927f_f842,
    0x383f_005f,
    0xf100_0421,
    0x54ff_fee1,
    0xd508_831f,
    0xd503_3b9f,
    0xd503_3fdf,
];

/// The list's entries for `pages`, of `page` bytes each, in order: each page once, within
/// [`REACH`], where `memory` says it is guest memory (`Some`), and marked for a write where
/// the guest wrote it, the memory is writable (`Some(true)`), and the CPU has atomic
/// instructions (`lse`, FEAT_LSE). So a damaged list costs no more than touching guest
/// memory once.
pub fn entries(pages: &[Touch], page: u64, lse: bool, memory: impl Fn(u64) -> Option<bool>) -> Vec<u64> {
    let mut listed = std::collections::HashSet::new();
    pages
        .iter()
        .filter_map(|t| {
            let at = t.gpa & !(page - 1);
            let writable = memory(at)?;
            let write = t.written && lse && writable;
            (at < REACH && listed.insert(at)).then_some(at | u64::from(write))
        })
        .collect()
}

/// The program's one table: a 1 GiB block for each GiB `entries` reach, as data, and the
/// block of [`layout::PREFETCH`], which holds the program, executable.
pub fn table(entries: &[u64]) -> [u64; 512] {
    let mut table = [0u64; 512];
    let gib = |gpa: u64| (gpa >> 30) as usize;
    let block = |gpa: u64, code: bool| {
        // Normal memory (MAIR attribute 0), inner shareable, accessed, EL1 read-write;
        // never executable at EL0, nor at EL1 unless it holds the code.
        let never = if code { 1 << 54 } else { 3 << 53 };
        (gpa & !((1 << 30) - 1)) | never | (1 << 10) | (3 << 8) | 0b01
    };
    for &entry in entries {
        if let Some(slot) = table.get_mut(gib(entry)) {
            *slot = block(entry, false);
        }
    }
    if let Some(slot) = table.get_mut(gib(layout::PREFETCH)) {
        *slot = block(layout::PREFETCH, true);
    }
    table
}

/// The system registers the program runs under, by encoding, given ID_AA64MMFR0_EL1's
/// PARange: MAIR_EL1's attribute 0 Normal write-back; TCR_EL1 a 39-bit space from TTBR0
/// at 4 KiB granules, whose first level maps 1 GiB blocks, walks write-back cacheable and
/// inner shareable, no TTBR1 walks, IPS = PARange; TTBR0_EL1 the table at `table`; and
/// SCTLR_EL1 with the MMU and caches on.
pub fn registers(parange: u64, table: u64) -> [(u16, u64); 4] {
    let tcr = 25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 23) | ((parange & 0xf) << 32);
    [
        (sysreg::MAIR_EL1, 0xff),
        (sysreg::TCR_EL1, tcr),
        (sysreg::TTBR0_EL1, table),
        (
            sysreg::SCTLR_EL1,
            sysreg::SCTLR_EL1_RESET | sysreg::SCTLR_EL1_MMU_CACHES,
        ),
    ]
}

/// Whether ID_AA64ISAR0_EL1 `isar0` says the CPU has atomic instructions (FEAT_LSE): its
/// Atomic field, bits 23:20, 2 or more.
pub fn lse(isar0: u64) -> bool {
    (isar0 >> 20) & 0xf >= 2
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Each page once, in the list's order; writes only where the guest wrote, the memory
    /// is writable and the CPU has atomics; pages outside guest memory or past the reach
    /// left out.
    #[test]
    fn entries_list_each_page_once_and_mark_writes_only_where_they_may_be() {
        let page = 4096;
        let t = |gpa: u64, written: bool| Touch { gpa, written };
        let pages = [
            t(0x8000_1234, true),
            t(0x8000_1000, false),
            t(0x8000_2000, false),
            t(0x9000_0000, true),
            t(0x1000, false),
            t(REACH, false),
        ];
        // RAM from 2 GiB, and past the reach, writable; 0x9000_0000 read-only device memory.
        let memory = |at: u64| {
            if (0x9000_0000..0x9000_1000).contains(&at) {
                Some(false)
            } else if (0x8000_0000..0x9000_0000).contains(&at) || at >= REACH {
                Some(true)
            } else {
                None
            }
        };
        assert_eq!(
            entries(&pages, page, true, memory),
            [0x8000_1001, 0x8000_2000, 0x9000_0000]
        );
        assert_eq!(
            entries(&pages, page, false, memory),
            [0x8000_1000, 0x8000_2000, 0x9000_0000]
        );
    }

    /// The table maps the entries' GiBs as data and the program's as code, each once.
    #[test]
    fn the_table_maps_each_gib_the_list_reaches_and_the_programs() {
        let table = table(&[0x8000_1001, 0x8000_2000, 0x1_4000_0000]);
        let data = |gpa: u64| gpa | (3 << 53) | (1 << 10) | (3 << 8) | 1;
        assert_eq!(table[2], data(0x8000_0000));
        assert_eq!(table[5], data(0x1_4000_0000));
        assert_eq!(table[1], layout::PREFETCH | (1 << 54) | (1 << 10) | (3 << 8) | 1);
        assert_eq!(table.iter().filter(|&&e| e != 0).count(), 3);
    }
}
