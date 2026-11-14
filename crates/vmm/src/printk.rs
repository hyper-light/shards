//! The guest kernel's log, read from its memory once its vCPUs have stopped (D12): what it
//! said before its console could carry it. The console is virtio-console (hvc0), which
//! registers only as its driver probes (device_initcall); a kernel that panics before then
//! leaves its words in its printk ring alone.
//!
//! The kernel publishes where the ring is in VMCOREINFO (kernel/vmcore_info.c), a page it
//! fills at subsys_initcall, before any virtio driver probes: the ring's symbol, the offsets
//! of every field read here (kernel/printk/printk.c `log_buf_vmcoreinfo_setup`), and what
//! turns its addresses into physical ones (arch/x86/kernel/vmcore_info_64.c,
//! arch/arm64/kernel/vmcore_info.c). No channel hands that page to a host here (QEMU's is
//! fw_cfg, which shards has none of), so it is found by how it begins: the page from
//! `__get_free_pages` whose first line is `OSRELEASE=`. A guest can plant a page so, but it
//! could as well write anything to its console: what is read is quoted, never acted on,
//! and every length is bounded by the guest's own RAM.

use crate::devices::said::{Kept, Said};
use crate::memory::{Access, GuestMemory};

/// The guest's page: 4 KiB in shards' guest kernels (x86-64's one size; arm64's
/// `CONFIG_ARM64_4K_PAGES=y`, resources/kernel), which VMCOREINFO's page is one of
/// (`VMCOREINFO_BYTES` = PAGE_SIZE, include/linux/vmcore_info.h).
const PAGE: u64 = 4 << 10;
/// How VMCOREINFO begins (`VMCOREINFO_OSRELEASE`, the first entry
/// `crash_save_vmcoreinfo_init` appends).
const START: &[u8] = b"OSRELEASE=";
/// `__START_KERNEL_map` (arch/x86/include/asm/page_64_types.h): where x86-64's kernel image
/// is mapped, `phys_base` below its physical place.
const X86_START_KERNEL_MAP: u64 = 0xffff_ffff_8000_0000;
/// `__PAGE_OFFSET_BASE_L4` and `_L5` (page_64_types.h): the direct map's base where memory
/// is not randomized, as no shards boot randomizes it: its zero page's loadflags carry no
/// KASLR_FLAG (arch/x86_64/boot.rs), which `kaslr_enabled()` reads
/// (arch/x86/include/asm/setup.h) and only the bzImage decompressor sets.
const X86_PAGE_OFFSET_L4: u64 = 0xffff_8880_0000_0000;
const X86_PAGE_OFFSET_L5: u64 = 0xff11_0000_0000_0000;
/// A descriptor's state, in its state variable's top two bits, and its ID below them
/// (kernel/printk/printk_ringbuffer.h `DESC_FLAGS_SHIFT`, `DESC_STATE`, `DESC_ID_MASK`):
/// committed and finalized records are whole.
const STATE_SHIFT: u32 = 62;
const ID_MASK: u64 = !(3 << STATE_SHIFT);
const COMMITTED: u64 = 1;
const FINALIZED: u64 = 2;
/// A data block's ID before its text (`struct prb_data_block`, an unsigned long).
const BLOCK_ID: u64 = 8;
/// An empty line's block positions (`EMPTY_LINE_LPOS`, printk_ringbuffer.h).
const EMPTY_LINE: u64 = 0x3;

/// Why a guest's log could not be read (R04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unread {
    /// No page of its RAM begins as VMCOREINFO does: it stopped before subsys_initcall
    /// made one, or its kernel has no CONFIG_VMCORE_INFO.
    NoInfo,
    /// Its memory is held by another access on this thread.
    Busy,
    /// Its VMCOREINFO says nothing, or no number, of this.
    Missing(&'static str),
    /// An address its kernel names is in none of its RAM.
    Unmapped(u64),
    /// Its ring is larger than its RAM.
    PastRam,
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unread::NoInfo => f.write_str("its kernel published no VMCOREINFO"),
            Unread::Busy => f.write_str("its memory is busy"),
            Unread::Missing(key) => write!(f, "its VMCOREINFO has no {key}"),
            Unread::Unmapped(va) => write!(f, "its kernel's address {va:#x} is in none of its RAM"),
            Unread::PastRam => f.write_str("its log's ring is larger than its RAM"),
        }
    }
}

/// The last `keep` bytes of what the guest's kernel logged, in whole lines, each as its
/// console would have shown it (`[seconds.micros] text`).
pub fn read(memory: &GuestMemory, keep: usize) -> Result<Said, Unread> {
    let access = memory.access().map_err(|_| Unread::Busy)?;
    let ram: u64 = memory.regions().map(|(_, _, len)| len as u64).sum();
    let mut why = Unread::NoInfo;
    for info in infos(&access, memory) {
        match walk(&access, &info, ram, keep) {
            Ok(said) => return Ok(said),
            Err(e) => why = e,
        }
    }
    Err(why)
}

/// Each page of RAM that begins as VMCOREINFO does, its text to its first NUL.
fn infos(access: &Access<'_>, memory: &GuestMemory) -> Vec<String> {
    let mut found = Vec::new();
    for (gpa, _, len) in memory.regions() {
        let end = gpa.saturating_add(len as u64);
        let mut at = gpa.next_multiple_of(PAGE);
        while at.saturating_add(PAGE) <= end {
            let mut head = [0u8; START.len()];
            if access.read(at, &mut head).is_ok() && head == START {
                let mut page = vec![0u8; PAGE as usize];
                if access.read(at, &mut page).is_ok() {
                    let text = page.split(|&b| b == 0).next().unwrap_or_default();
                    found.push(String::from_utf8_lossy(text).into_owned());
                }
            }
            at = at.saturating_add(PAGE);
        }
    }
    found
}

/// VMCOREINFO's text: `KEY=VALUE` lines.
struct Info<'a>(&'a str);

impl Info<'_> {
    fn get(&self, key: &'static str) -> Result<&str, Unread> {
        self.0
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .ok_or(Unread::Missing(key))
    }

    /// A `SYMBOL(..)`, in hex without a prefix (`VMCOREINFO_SYMBOL`'s `%lx`).
    fn symbol(&self, key: &'static str) -> Result<u64, Unread> {
        u64::from_str_radix(self.get(key)?, 16).map_err(|_| Unread::Missing(key))
    }

    /// An `OFFSET(..)` or `SIZE(..)`, in decimal (`%lu`).
    fn offset(&self, key: &'static str) -> Result<u64, Unread> {
        self.get(key)?.parse().map_err(|_| Unread::Missing(key))
    }

    /// A `NUMBER(..)`: decimal (`VMCOREINFO_NUMBER`'s `%ld`), or hex with `0x`, as arm64
    /// appends its own.
    fn number(&self, key: &'static str) -> Result<u64, Unread> {
        let value = self.get(key)?;
        match value.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => value.parse::<i64>().ok().map(|v| v as u64),
        }
        .ok_or(Unread::Missing(key))
    }
}

/// How a kernel's addresses become physical ones, as its VMCOREINFO says: x86-64's or
/// arm64's, by which numbers it publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Map {
    X86 {
        phys_base: u64,
        page_offset: u64,
    },
    Arm64 {
        kimage_voffset: u64,
        phys_offset: u64,
        page_offset: u64,
        page_end: u64,
    },
}

impl Map {
    fn of(info: &Info<'_>) -> Result<Map, Unread> {
        if let Ok(kimage_voffset) = info.number("NUMBER(kimage_voffset)") {
            let va_bits = info.number("NUMBER(VA_BITS)")?;
            let bits = u32::try_from(va_bits)
                .ok()
                .filter(|b| (2..64).contains(b))
                .ok_or(Unread::Missing("NUMBER(VA_BITS)"))?;
            // arch/arm64/include/asm/memory.h: PAGE_OFFSET = -(1 << VA_BITS), the linear
            // map below PAGE_END = -(1 << (VA_BITS - 1)), `__lm_to_phys`.
            return Ok(Map::Arm64 {
                kimage_voffset,
                phys_offset: info.number("NUMBER(PHYS_OFFSET)")?,
                page_offset: u64::MAX << bits,
                page_end: u64::MAX << (bits - 1),
            });
        }
        let l5 = info.number("NUMBER(pgtable_l5_enabled)")? != 0;
        Ok(Map::X86 {
            phys_base: info.number("NUMBER(phys_base)")?,
            page_offset: if l5 {
                X86_PAGE_OFFSET_L5
            } else {
                X86_PAGE_OFFSET_L4
            },
        })
    }

    fn physical(self, va: u64) -> Option<u64> {
        match self {
            Map::X86 {
                phys_base,
                page_offset,
            } => {
                if va >= X86_START_KERNEL_MAP {
                    (va - X86_START_KERNEL_MAP).checked_add(phys_base)
                } else {
                    va.checked_sub(page_offset)
                }
            }
            Map::Arm64 {
                kimage_voffset,
                phys_offset,
                page_offset,
                page_end,
            } => {
                if (page_offset..page_end).contains(&va) {
                    (va & !page_offset).checked_add(phys_offset)
                } else {
                    va.checked_sub(kimage_voffset)
                }
            }
        }
    }
}

/// The guest's memory, read at its kernel's addresses.
struct Guest<'a> {
    access: &'a Access<'a>,
    map: Map,
}

impl Guest<'_> {
    fn bytes(&self, va: u64, buf: &mut [u8]) -> Result<(), Unread> {
        let pa = self.map.physical(va).ok_or(Unread::Unmapped(va))?;
        self.access.read(pa, buf).map_err(|_| Unread::Unmapped(va))
    }

    fn u64(&self, va: u64) -> Result<u64, Unread> {
        let mut b = [0u8; 8];
        self.bytes(va, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn u32(&self, va: u64) -> Result<u32, Unread> {
        let mut b = [0u8; 4];
        self.bytes(va, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u16(&self, va: u64) -> Result<u16, Unread> {
        let mut b = [0u8; 2];
        self.bytes(va, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }
}

/// `at + offset`, an address past every one a kernel has refused.
fn plus(at: u64, offset: u64) -> Result<u64, Unread> {
    at.checked_add(offset).ok_or(Unread::Unmapped(at))
}

/// `1 << bits` of `each` bytes, bounded by the guest's RAM.
fn within_ram(bits: u32, each: u64, ram: u64) -> Result<u64, Unread> {
    let count = 1u64.checked_shl(bits).ok_or(Unread::PastRam)?;
    match count.checked_mul(each.max(1)) {
        Some(bytes) if bytes <= ram => Ok(count),
        _ => Err(Unread::PastRam),
    }
}

/// The ring VMCOREINFO's text `info` names, from its oldest record to its newest, as
/// `prb_read_valid` reads each (kernel/printk/printk_ringbuffer.c).
fn walk(access: &Access<'_>, info: &str, ram: u64, keep: usize) -> Result<Said, Unread> {
    let info = Info(info);
    let g = Guest {
        access,
        map: Map::of(&info)?,
    };
    let o = |key| info.offset(key);
    let counter = o("OFFSET(atomic_long_t.counter)")?;
    let prb = g.u64(info.symbol("SYMBOL(prb)")?)?;
    let descs_ring = plus(prb, o("OFFSET(printk_ringbuffer.desc_ring)")?)?;
    let text_ring = plus(prb, o("OFFSET(printk_ringbuffer.text_data_ring)")?)?;
    let desc_size = o("SIZE(prb_desc)")?;
    let info_size = o("SIZE(printk_info)")?;
    let count = within_ram(
        g.u32(plus(descs_ring, o("OFFSET(prb_desc_ring.count_bits)")?)?)?,
        desc_size,
        ram,
    )?;
    let descs = g.u64(plus(descs_ring, o("OFFSET(prb_desc_ring.descs)")?)?)?;
    let infos = g.u64(plus(descs_ring, o("OFFSET(prb_desc_ring.infos)")?)?)?;
    let head = g.u64(plus(
        plus(descs_ring, o("OFFSET(prb_desc_ring.head_id)")?)?,
        counter,
    )?)? & ID_MASK;
    let tail = g.u64(plus(
        plus(descs_ring, o("OFFSET(prb_desc_ring.tail_id)")?)?,
        counter,
    )?)? & ID_MASK;
    let size_bits = g.u32(plus(text_ring, o("OFFSET(prb_data_ring.size_bits)")?)?)?;
    let ring = Ring {
        data: g.u64(plus(text_ring, o("OFFSET(prb_data_ring.data)")?)?)?,
        size: within_ram(size_bits, 1, ram)?,
        bits: size_bits,
    };
    let (state_var, blk) = (
        o("OFFSET(prb_desc.state_var)")?,
        o("OFFSET(prb_desc.text_blk_lpos)")?,
    );
    let (begin_at, next_at) = (
        o("OFFSET(prb_data_blk_lpos.begin)")?,
        o("OFFSET(prb_data_blk_lpos.next)")?,
    );
    let (ts_at, len_at) = (
        o("OFFSET(printk_info.ts_nsec)")?,
        o("OFFSET(printk_info.text_len)")?,
    );
    let mut kept = Kept::new(keep);
    let mut id = tail;
    // At most a ring's worth: the IDs from the oldest to the newest.
    for _ in 0..count {
        let index = id & (count - 1);
        let desc = plus(descs, index.checked_mul(desc_size).ok_or(Unread::PastRam)?)?;
        let sv = g.u64(plus(plus(desc, state_var)?, counter)?)?;
        let state = sv >> STATE_SHIFT;
        if sv & ID_MASK == id && (state == COMMITTED || state == FINALIZED) {
            let begin = g.u64(plus(plus(desc, blk)?, begin_at)?)?;
            let next = g.u64(plus(plus(desc, blk)?, next_at)?)?;
            let record = plus(infos, index.checked_mul(info_size).ok_or(Unread::PastRam)?)?;
            let text_len = g.u16(plus(record, len_at)?)?;
            if let Some(text) = ring.text(&g, begin, next, text_len)? {
                let ns = g.u64(plus(record, ts_at)?)?;
                let line = format!(
                    "[{:5}.{:06}] {}\n",
                    ns / 1_000_000_000,
                    ns % 1_000_000_000 / 1_000,
                    String::from_utf8_lossy(&text)
                );
                kept.push(line.as_bytes());
            }
        }
        if id == head {
            break;
        }
        id = id.wrapping_add(1) & ID_MASK;
    }
    Ok(kept.said())
}

/// The text data ring (`struct prb_data_ring`): `size` bytes at `data`, `1 << bits`.
struct Ring {
    data: u64,
    size: u64,
    bits: u32,
}

impl Ring {
    /// A record's text, as `get_data` finds its block: within one wrap of the ring, or
    /// wrapped to its start; none where its data was lost (a data-less block but an empty
    /// line's) or its positions are of no block.
    fn text(&self, g: &Guest<'_>, begin: u64, next: u64, text_len: u16) -> Result<Option<Vec<u8>>, Unread> {
        if begin & 1 == 1 && next & 1 == 1 {
            return Ok((begin == EMPTY_LINE && next == EMPTY_LINE).then(Vec::new));
        }
        let wraps = |lpos: u64| lpos >> self.bits;
        let last = next.wrapping_sub(1);
        let (at, len) = if wraps(begin) == wraps(last) {
            (begin & (self.size - 1), next.wrapping_sub(begin))
        } else if wraps(begin.wrapping_add(self.size)) == wraps(last) {
            (0, next & (self.size - 1))
        } else {
            return Ok(None);
        };
        let Some(room) = len.checked_sub(BLOCK_ID) else {
            return Ok(None);
        };
        let n = room.min(u64::from(text_len));
        if at.saturating_add(BLOCK_ID).saturating_add(n) > self.size {
            return Ok(None);
        }
        let mut text = vec![0u8; n as usize];
        g.bytes(plus(self.data, at + BLOCK_ID)?, &mut text)?;
        Ok(Some(text))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const RAM: u64 = 0x4000_0000;
    const LEN: usize = 2 << 20;

    /// A guest whose kernel's log is laid out as Linux lays it, with offsets as a kernel's
    /// VMCOREINFO says them, at `va` of each physical place `pa` by `to_va`.
    struct Fake {
        memory: GuestMemory,
    }

    const PRB: u64 = 0x1000; // where `prb`, the pointer, is
    const RB: u64 = 0x2000; // the printk_ringbuffer
    const DESCS: u64 = 0x3000;
    const INFOS: u64 = 0x4000;
    const DATA: u64 = 0x6000;
    const INFO: u64 = 0x10_0000; // the VMCOREINFO page
    const COUNT_BITS: u32 = 3;
    const SIZE_BITS: u32 = 7; // 128 bytes of text
    const DESC: u64 = 24;
    const RECORD: u64 = 88;

    impl Fake {
        fn new(numbers: &str, to_va: impl Fn(u64) -> u64) -> Fake {
            let memory = GuestMemory::anonymous(&[(RAM, LEN)]).unwrap();
            let a = memory.access().unwrap();
            let text = format!(
                "OSRELEASE=6.18.48\nPAGESIZE=4096\nSYMBOL(prb)={:x}\n{numbers}\
                 OFFSET(atomic_long_t.counter)=0\nOFFSET(printk_ringbuffer.desc_ring)=0\n\
                 OFFSET(printk_ringbuffer.text_data_ring)=48\nSIZE(prb_desc)={DESC}\n\
                 SIZE(printk_info)={RECORD}\nOFFSET(prb_desc_ring.count_bits)=0\n\
                 OFFSET(prb_desc_ring.descs)=8\nOFFSET(prb_desc_ring.infos)=16\n\
                 OFFSET(prb_desc_ring.head_id)=24\nOFFSET(prb_desc_ring.tail_id)=32\n\
                 OFFSET(prb_data_ring.size_bits)=0\nOFFSET(prb_data_ring.data)=8\n\
                 OFFSET(prb_desc.state_var)=0\nOFFSET(prb_desc.text_blk_lpos)=8\n\
                 OFFSET(prb_data_blk_lpos.begin)=0\nOFFSET(prb_data_blk_lpos.next)=8\n\
                 OFFSET(printk_info.ts_nsec)=8\nOFFSET(printk_info.text_len)=16\n",
                to_va(RAM + PRB)
            );
            a.write(RAM + INFO, text.as_bytes()).unwrap();
            let w = |pa: u64, v: u64| a.write(RAM + pa, &v.to_le_bytes()).unwrap();
            w(PRB, to_va(RAM + RB));
            a.write(RAM + RB, &COUNT_BITS.to_le_bytes()).unwrap();
            w(RB + 8, to_va(RAM + DESCS));
            w(RB + 16, to_va(RAM + INFOS));
            a.write(RAM + RB + 48, &SIZE_BITS.to_le_bytes()).unwrap();
            w(RB + 48 + 8, to_va(RAM + DATA));
            drop(a);
            Fake { memory }
        }

        /// Record `id` in state `state`, its text at `begin..next` of the data ring
        /// (`text` written at `at` past its block's ID), logged at `ns`.
        fn record(&self, id: u64, state: u64, (begin, next): (u64, u64), at: u64, text: &[u8], ns: u64) {
            let a = self.memory.access().unwrap();
            let index = id & ((1 << COUNT_BITS) - 1);
            let desc = RAM + DESCS + index * DESC;
            a.write(desc, &(state << STATE_SHIFT | id).to_le_bytes()).unwrap();
            a.write(desc + 8, &begin.to_le_bytes()).unwrap();
            a.write(desc + 16, &next.to_le_bytes()).unwrap();
            let record = RAM + INFOS + index * RECORD;
            a.write(record + 8, &ns.to_le_bytes()).unwrap();
            a.write(record + 16, &(text.len() as u16).to_le_bytes()).unwrap();
            a.write(RAM + DATA + at + BLOCK_ID, text).unwrap();
        }

        fn ids(&self, tail: u64, head: u64) {
            let a = self.memory.access().unwrap();
            a.write(RAM + RB + 24, &head.to_le_bytes()).unwrap();
            a.write(RAM + RB + 32, &tail.to_le_bytes()).unwrap();
        }
    }

    /// Records from the oldest to the newest, one wrapped to the ring's start, an empty
    /// line, one being written (reserved) and one whose data was lost left out, each as the
    /// console shows it.
    fn logged(fake: &Fake) {
        // A 128-byte ring: record 5 at 48..80, 6 wrapping from 112 into 0..40 of the next
        // wrap, the one being written at 80..104.
        fake.record(5, FINALIZED, (48, 80), 48, b"Linux version 6.18.48", 1_500_000);
        fake.record(
            6,
            COMMITTED,
            (112, 128 + 40),
            0,
            b"Kernel panic - not syncing",
            2_000_000_000,
        );
        fake.record(7, FINALIZED, (EMPTY_LINE, EMPTY_LINE), 0, b"", 2_000_001_000);
        fake.record(8, 0, (80, 104), 80, b"being written", 3);
        fake.record(9, FINALIZED, (1, 1), 0, b"", 4);
        fake.ids(5, 9);
    }

    const SAID: &[u8] =
        b"[    0.001500] Linux version 6.18.48\n[    2.000000] Kernel panic - not syncing\n[    2.000001] \n";

    #[test]
    fn an_x86_kernels_log_is_read_through_its_image_map() {
        let phys_base = 0;
        let fake = Fake::new("NUMBER(phys_base)=0\nNUMBER(pgtable_l5_enabled)=0\n", |pa| {
            pa - phys_base + X86_START_KERNEL_MAP
        });
        logged(&fake);
        let said = read(&fake.memory, 4096).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&said.lines),
            String::from_utf8_lossy(SAID)
        );
    }

    #[test]
    fn an_arm64_kernels_log_is_read_through_its_linear_map() {
        // VA_BITS 48: PAGE_OFFSET 0xffff_0000_0000_0000; PHYS_OFFSET the RAM's base.
        let fake = Fake::new(
            &format!(
                "NUMBER(VA_BITS)=48\nNUMBER(kimage_voffset)=0xffff800000000000\nNUMBER(PHYS_OFFSET)={RAM:#x}\n"
            ),
            |pa| (pa - RAM) | 0xffff_0000_0000_0000,
        );
        logged(&fake);
        let said = read(&fake.memory, 4096).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&said.lines),
            String::from_utf8_lossy(SAID)
        );
        // Kept to its last whole lines, as the console's report is.
        let cut = read(&fake.memory, 60).unwrap();
        assert_eq!(
            cut.lines,
            b"[    2.000000] Kernel panic - not syncing\n[    2.000001] \n"
        );
    }

    #[test]
    fn a_guest_without_vmcoreinfo_or_with_a_ring_past_its_ram_is_refused() {
        let memory = GuestMemory::anonymous(&[(RAM, LEN)]).unwrap();
        assert_eq!(read(&memory, 4096), Err(Unread::NoInfo));
        let fake = Fake::new("NUMBER(phys_base)=0\nNUMBER(pgtable_l5_enabled)=0\n", |pa| {
            pa + X86_START_KERNEL_MAP
        });
        fake.memory
            .access()
            .unwrap()
            .write(RAM + RB, &40u32.to_le_bytes())
            .unwrap();
        assert_eq!(read(&fake.memory, 4096), Err(Unread::PastRam));
        let missing = Fake::new("NUMBER(pgtable_l5_enabled)=0\n", |pa| pa + X86_START_KERNEL_MAP);
        assert_eq!(
            read(&missing.memory, 4096),
            Err(Unread::Missing("NUMBER(phys_base)"))
        );
    }
}
