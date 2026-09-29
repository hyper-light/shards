//! The Linux x86_64 64-bit boot protocol on an uncompressed `vmlinux` (boot.rst "64-bit
//! BOOT PROTOCOL"; docs/research/kvm-x86_64-ground-truth.md §2): PT_LOADs at their
//! physical addresses, entry at `e_entry` (the physical address of `startup_64`), with
//! identity page tables, a boot GDT and a zero page carrying the e820 map.

use std::fmt;
use std::fs::File;

use super::{Boot, Segment, layout};
use crate::memory::{GuestMemory, OutOfBounds, Reentered};
use crate::platform;

#[derive(Debug)]
pub enum BootError {
    Io(std::io::Error),
    NotAnElf(&'static str),
    Memory(OutOfBounds),
    Access(Reentered),
    DoesNotFit { what: &'static str, at: u64, len: u64 },
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::Io(e) => write!(f, "reading the kernel: {e}"),
            BootError::NotAnElf(why) => write!(f, "kernel is not an x86_64 vmlinux: {why}"),
            BootError::Memory(e) => write!(f, "{e}"),
            BootError::Access(e) => write!(f, "{e}"),
            BootError::DoesNotFit { what, at, len } => {
                write!(f, "{what} at {at:#x}+{len:#x} does not fit guest RAM")
            }
        }
    }
}

impl std::error::Error for BootError {}

impl From<std::io::Error> for BootError {
    fn from(e: std::io::Error) -> Self {
        BootError::Io(e)
    }
}

impl From<OutOfBounds> for BootError {
    fn from(e: OutOfBounds) -> Self {
        BootError::Memory(e)
    }
}

impl From<Reentered> for BootError {
    fn from(e: Reentered) -> Self {
        BootError::Access(e)
    }
}

/// Guest RAM regions for `ram` bytes: below the 32-bit MMIO gap, then above 4 GiB.
pub fn ram_ranges(ram: u64) -> Vec<(u64, u64)> {
    let low = ram.min(layout::MMIO_GAP);
    let mut ranges = vec![(0, low)];
    if ram > low {
        ranges.push((layout::MMIO_GAP_END, ram - low));
    }
    ranges
}

const PT_LOAD: u32 = 1;
const EM_X86_64: u16 = 62;
const ET_EXEC: u16 = 2;
const MAX_PHDRS: u16 = 64;

fn le<const N: usize>(b: &[u8], at: usize) -> Result<[u8; N], BootError> {
    b.get(at..at + N)
        .and_then(|s| s.try_into().ok())
        .ok_or(BootError::NotAnElf("truncated header"))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, BootError> {
    Ok(u16::from_le_bytes(le(b, at)?))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, BootError> {
    Ok(u32::from_le_bytes(le(b, at)?))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, BootError> {
    Ok(u64::from_le_bytes(le(b, at)?))
}

/// A PT_LOAD segment: file bytes to copy to a physical address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment64 {
    pub offset: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elf {
    pub entry: u64,
    pub loads: Vec<Segment64>,
}

/// Parses an ELF64 x86-64 executable's entry and loadable segments. Every field is
/// untrusted.
pub fn parse_elf(
    header: &[u8],
    phdrs: impl Fn(u64, usize) -> Result<Vec<u8>, BootError>,
) -> Result<Elf, BootError> {
    if header.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err(BootError::NotAnElf("bad magic"));
    }
    if header.get(4) != Some(&2) || header.get(5) != Some(&1) {
        return Err(BootError::NotAnElf("not 64-bit little-endian"));
    }
    if u16_at(header, 16)? != ET_EXEC || u16_at(header, 18)? != EM_X86_64 {
        return Err(BootError::NotAnElf("not an x86-64 executable"));
    }
    let entry = u64_at(header, 24)?;
    let phoff = u64_at(header, 32)?;
    let phentsize = u16_at(header, 54)?;
    let phnum = u16_at(header, 56)?;
    if phentsize != 56 || phnum == 0 || phnum > MAX_PHDRS {
        return Err(BootError::NotAnElf("unexpected program headers"));
    }
    let table = phdrs(phoff, usize::from(phnum) * 56)?;
    let mut loads = Vec::new();
    for i in 0..usize::from(phnum) {
        let at = i * 56;
        if u32_at(&table, at)? != PT_LOAD {
            continue;
        }
        let seg = Segment64 {
            offset: u64_at(&table, at + 8)?,
            paddr: u64_at(&table, at + 24)?,
            filesz: u64_at(&table, at + 32)?,
            memsz: u64_at(&table, at + 40)?,
        };
        if seg.filesz > seg.memsz {
            return Err(BootError::NotAnElf("segment larger in the file than in memory"));
        }
        loads.push(seg);
    }
    if loads.is_empty() {
        return Err(BootError::NotAnElf("no loadable segments"));
    }
    Ok(Elf { entry, loads })
}

#[derive(Debug, Clone, Copy)]
pub struct LoadedKernel {
    pub entry: u64,
    /// End of the highest segment in memory (bss included).
    pub end: u64,
}

/// Copies each PT_LOAD to its physical address. Segments must lie in low RAM, above
/// 1 MiB and inside the identity map, which is all `startup_64` can reach.
pub fn load_kernel(mem: &GuestMemory, kernel: &File, low_ram_end: u64) -> Result<LoadedKernel, BootError> {
    let mut header = [0u8; 64];
    platform::read_exact_at(kernel, &mut header, 0)?;
    let elf = parse_elf(&header, |offset, len| {
        let mut table = vec![0u8; len];
        platform::read_exact_at(kernel, &mut table, offset)?;
        Ok(table)
    })?;
    let limit = low_ram_end.min(layout::IDENTITY_MAPPED);
    let mut end = 0u64;
    for seg in &elf.loads {
        let seg_end = seg.paddr.checked_add(seg.memsz);
        if seg.paddr < layout::HIMEM || seg_end.is_none_or(|e| e > limit) {
            return Err(BootError::DoesNotFit {
                what: "kernel segment",
                at: seg.paddr,
                len: seg.memsz,
            });
        }
        let len = usize::try_from(seg.filesz).map_err(|_| BootError::NotAnElf("segment size"))?;
        mem.read_file(seg.paddr, len, kernel, seg.offset)?;
        // Fresh guest RAM is zero, so bss (memsz beyond filesz) needs no clearing.
        end = end.max(seg_end.unwrap_or(0));
    }
    if elf.entry < layout::HIMEM || elf.entry >= end {
        return Err(BootError::NotAnElf("entry point outside the loaded image"));
    }
    Ok(LoadedKernel {
        entry: elf.entry,
        end,
    })
}

/// Places the initrd at the first 2 MiB boundary after the kernel, below `limit`.
pub fn load_initrd(
    mem: &GuestMemory,
    initrd: &[u8],
    after: u64,
    limit: u64,
) -> Result<(u64, u64), BootError> {
    let len = initrd.len() as u64;
    let start = after.checked_next_multiple_of(2 << 20).unwrap_or(u64::MAX);
    if start.checked_add(len).is_none_or(|end| end > limit) {
        return Err(BootError::DoesNotFit {
            what: "initrd",
            at: start,
            len,
        });
    }
    mem.access()?.write(start, initrd)?;
    Ok((start, len))
}

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

/// The e820 map (research doc §6.2): conventional RAM below the firmware area, the
/// firmware area reserved, then RAM from 1 MiB to the MMIO gap and above 4 GiB.
pub fn e820(ram: u64) -> Vec<(u64, u64, u32)> {
    let mut map = vec![
        (0, layout::SYSTEM, E820_RAM),
        (layout::SYSTEM, layout::HIMEM - layout::SYSTEM, E820_RESERVED),
    ];
    for (start, len) in ram_ranges(ram) {
        let from = start.max(layout::HIMEM);
        let end = start + len;
        if end > from {
            map.push((from, end - from, E820_RAM));
        }
    }
    map
}

/// Boot-protocol inputs the zero page carries.
#[derive(Debug, Clone, Copy)]
pub struct ZeroPage {
    pub ram: u64,
    pub initrd: Option<(u64, u64)>,
    pub rsdp: u64,
}

/// `struct boot_params` for a raw vmlinux, built from zero (arch/x86/include/uapi/asm/
/// bootparam.h; research doc §2.2). Zeroing matters: a non-zero sentinel makes the
/// kernel wipe `acpi_rsdp_addr`.
pub fn zero_page(z: &ZeroPage) -> Result<[u8; 4096], BootError> {
    let mut bp = [0u8; 4096];
    let mut put = |at: usize, bytes: &[u8]| -> Result<(), BootError> {
        bp.get_mut(at..at + bytes.len())
            .ok_or(BootError::NotAnElf("zero page field"))?
            .copy_from_slice(bytes);
        Ok(())
    };
    put(0x070, &z.rsdp.to_le_bytes())?; // acpi_rsdp_addr
    put(0x1fe, &0xaa55u16.to_le_bytes())?; // hdr.boot_flag
    put(0x202, b"HdrS")?; // hdr.header
    put(0x206, &0x020cu16.to_le_bytes())?; // hdr.version: 2.12
    put(0x210, &[0xff])?; // hdr.type_of_loader: undefined; 0 would ignore the initrd
    put(0x211, &[0x01])?; // hdr.loadflags: LOADED_HIGH
    if let Some((start, len)) = z.initrd {
        let start = u32::try_from(start).map_err(|_| BootError::NotAnElf("initrd above 4 GiB"))?;
        let len = u32::try_from(len).map_err(|_| BootError::NotAnElf("initrd over 4 GiB"))?;
        put(0x218, &start.to_le_bytes())?; // hdr.ramdisk_image
        put(0x21c, &len.to_le_bytes())?; // hdr.ramdisk_size
    }
    put(0x228, &(layout::CMDLINE as u32).to_le_bytes())?; // hdr.cmd_line_ptr
    let map = e820(z.ram);
    put(0x1e8, &[map.len() as u8])?; // e820_entries
    for (i, (addr, size, kind)) in map.iter().enumerate() {
        let at = 0x2d0 + 20 * i;
        put(at, &addr.to_le_bytes())?;
        put(at + 8, &size.to_le_bytes())?;
        put(at + 16, &kind.to_le_bytes())?;
    }
    Ok(bp)
}

/// 64-bit code, execute/read, accessed (`__BOOT_CS`) and data, read/write, accessed
/// (`__BOOT_DS`), both flat 4 GiB (arch/x86/include/asm/segment.h).
const GDT_CODE64: u64 = 0x00af_9b00_0000_ffff;
const GDT_DATA: u64 = 0x00cf_9300_0000_ffff;
const BOOT_CS: u16 = 0x10;
const BOOT_DS: u16 = 0x18;
/// Where TR points; no GDT slot is needed because only VM entry reads it.
const BOOT_TR: u16 = 0x20;

/// The segment register a GDT descriptor loads.
fn segment(selector: u16, descriptor: u64) -> Segment {
    let granular = descriptor >> 55 & 1 == 1;
    let raw_limit = ((descriptor & 0xffff) | (descriptor >> 32 & 0xf_0000)) as u32;
    Segment {
        base: (descriptor >> 16 & 0xff_ffff) | (descriptor >> 32 & 0xff00_0000),
        limit: if granular {
            raw_limit << 12 | 0xfff
        } else {
            raw_limit
        },
        selector,
        kind: (descriptor >> 40 & 0xf) as u8,
        code_or_data: descriptor >> 44 & 1 == 1,
        present: descriptor >> 47 & 1 == 1,
        db: descriptor >> 54 & 1 == 1,
        long: descriptor >> 53 & 1 == 1,
        granular,
    }
}

const CR0_PE: u64 = 1;
const CR0_ET: u64 = 1 << 4;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const PTE_PRESENT_RW: u64 = 0x3;
const PDE_2M: u64 = 0x83;

/// Writes the command line, the zero page, the boot GDT and IDT and the identity page
/// tables; returns the boot vCPU's state. CR0 is set outright (PE|ET|PG): OR-ing into
/// KVM's reset value would keep CD|NW and boot with caches disabled (research §2.4).
pub fn write_boot_state(
    mem: &GuestMemory,
    entry: u64,
    cmdline: &str,
    z: &ZeroPage,
) -> Result<Boot, BootError> {
    let bytes = cmdline.as_bytes();
    if bytes.len() >= layout::CMDLINE_MAX || bytes.contains(&0) {
        return Err(BootError::DoesNotFit {
            what: "command line",
            at: layout::CMDLINE,
            len: bytes.len() as u64,
        });
    }
    let a = mem.access()?;
    a.write(layout::CMDLINE, bytes)?;
    a.write(layout::ZERO_PAGE, &zero_page(z)?)?;

    for (i, d) in [0u64, 0, GDT_CODE64, GDT_DATA].iter().enumerate() {
        a.write_obj(layout::GDT + 8 * i as u64, *d)?;
    }
    a.write_obj(layout::IDT, 0u64)?;

    a.write_obj(layout::PML4, layout::PDPT | PTE_PRESENT_RW)?;
    a.write_obj(layout::PDPT, layout::PD | PTE_PRESENT_RW)?;
    for i in 0..512u64 {
        a.write_obj(layout::PD + 8 * i, (i << 21) | PDE_2M)?;
    }

    Ok(Boot {
        rip: entry,
        rsi: layout::ZERO_PAGE,
        cr0: CR0_PE | CR0_ET | CR0_PG,
        cr3: layout::PML4,
        cr4: CR4_PAE,
        efer: EFER_LME | EFER_LMA,
        cs: segment(BOOT_CS, GDT_CODE64),
        data: segment(BOOT_DS, GDT_DATA),
        tr: Segment {
            base: 0,
            limit: 0x67,
            selector: BOOT_TR,
            kind: 11, // busy 64-bit TSS
            code_or_data: false,
            present: true,
            db: false,
            long: false,
            granular: false,
        },
        gdt: (layout::GDT, 31),
        idt: (layout::IDT, 7),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn gdt_descriptors_decode_to_the_boot_segments() {
        let cs = segment(BOOT_CS, GDT_CODE64);
        assert_eq!((cs.base, cs.limit, cs.kind), (0, 0xffff_ffff, 0xb));
        assert!(cs.long && !cs.db && cs.present && cs.code_or_data && cs.granular);
        let ds = segment(BOOT_DS, GDT_DATA);
        assert_eq!((ds.base, ds.limit, ds.kind), (0, 0xffff_ffff, 0x3));
        assert!(!ds.long && ds.db && ds.present);
    }

    #[test]
    fn e820_splits_around_firmware_and_the_mmio_gap() {
        let small = e820(512 << 20);
        assert_eq!(small[0], (0, 0x9_fc00, E820_RAM));
        assert_eq!(small[1], (0x9_fc00, 0x6_0400, E820_RESERVED));
        assert_eq!(small[2], (0x10_0000, (512 << 20) - 0x10_0000, E820_RAM));
        assert_eq!(small.len(), 3);
        let big = e820(4 << 30);
        assert_eq!(big[2], (0x10_0000, 0xc000_0000 - 0x10_0000, E820_RAM));
        assert_eq!(big[3], (1 << 32, 1 << 30, E820_RAM));
    }

    #[test]
    fn zero_page_carries_the_boot_protocol_fields() {
        let bp = zero_page(&ZeroPage {
            ram: 256 << 20,
            initrd: Some((0x400_0000, 0x1234)),
            rsdp: layout::RSDP,
        })
        .unwrap();
        assert_eq!(
            u64::from_le_bytes(bp[0x70..0x78].try_into().unwrap()),
            layout::RSDP
        );
        assert_eq!((bp[0x210], bp[0x211]), (0xff, 0x01));
        assert_eq!(
            u32::from_le_bytes(bp[0x218..0x21c].try_into().unwrap()),
            0x400_0000
        );
        assert_eq!(u32::from_le_bytes(bp[0x228..0x22c].try_into().unwrap()), 0x2_0000);
        assert_eq!(bp[0x1e8], 3);
        assert_eq!(bp[0x1ef], 0, "sentinel must stay zero");
    }

    #[test]
    fn rejects_malformed_elves() {
        let phdrs = |_, _| Ok(vec![0u8; 56]);
        assert!(parse_elf(b"not an elf at all", phdrs).is_err());
        let mut h = [0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = 2;
        h[5] = 1;
        h[16] = 2;
        h[18] = 62;
        h[54] = 56;
        h[56] = 1;
        // One header, but not PT_LOAD.
        assert!(parse_elf(&h, phdrs).is_err());
        let mut load = vec![0u8; 56];
        load[0] = 1;
        load[32] = 2; // filesz 2 > memsz 0
        assert!(parse_elf(&h, |_, _| Ok(load.clone())).is_err());
        load[40] = 4;
        assert_eq!(parse_elf(&h, |_, _| Ok(load.clone())).unwrap().loads.len(), 1);
        h[56] = 0;
        assert!(parse_elf(&h, phdrs).is_err());
    }
}
