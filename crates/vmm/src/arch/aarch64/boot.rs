//! Direct kernel boot: arm64 `Image` loading and devicetree generation.
//!
//! Rules follow Linux Documentation/arch/arm64/booting.rst as summarized, with
//! sources, in docs/research/hvf-arm64-kvm-ground-truth.md §3.

use std::fmt;
use std::fs::File;

use super::layout;
use crate::fdt::{Fdt, FdtError};
use crate::memory::{GuestMemory, OutOfBounds};
use crate::platform;

const IMAGE_MAGIC: u32 = 0x644d_5241; // "ARM\x64"
const IMAGE_HEADER_LEN: usize = 64;
const SZ_2M: u64 = 2 << 20;
/// booting.rst: the DTB may not exceed 2 MiB and the 2 MiB after it must be RAM.
pub const FDT_MAX: u64 = SZ_2M;

#[derive(Debug)]
pub enum BootError {
    Io(std::io::Error),
    NotAnImage(&'static str),
    DoesNotFit {
        what: &'static str,
        need: u64,
        have: u64,
    },
    Memory(OutOfBounds),
    Fdt(FdtError),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::Io(e) => write!(f, "reading boot file: {e}"),
            BootError::NotAnImage(why) => write!(f, "kernel is not an arm64 Image: {why}"),
            BootError::DoesNotFit { what, need, have } => {
                write!(
                    f,
                    "{what} needs {need:#x} bytes of guest RAM but only {have:#x} are available"
                )
            }
            BootError::Memory(e) => write!(f, "{e}"),
            BootError::Fdt(e) => write!(f, "{e}"),
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
impl From<FdtError> for BootError {
    fn from(e: FdtError) -> Self {
        BootError::Fdt(e)
    }
}

/// Where the kernel landed in guest RAM.
#[derive(Debug, Clone, Copy)]
pub struct LoadedKernel {
    pub entry: u64,
    /// End of the kernel's footprint: load address + header `image_size` (which covers
    /// BSS and early page tables, not just the file).
    pub end: u64,
}

/// Parsed fields of the 64-byte arm64 Image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    pub text_offset: u64,
    pub image_size: u64,
    pub flags: u64,
}

fn le_bytes<const N: usize>(h: &[u8], at: usize) -> Result<[u8; N], BootError> {
    h.get(at..at + N)
        .and_then(|b| b.try_into().ok())
        .ok_or(BootError::NotAnImage("shorter than the 64-byte header"))
}

pub fn parse_image_header(h: &[u8]) -> Result<ImageHeader, BootError> {
    let field = |at| le_bytes::<8>(h, at).map(u64::from_le_bytes);
    if u32::from_le_bytes(le_bytes::<4>(h, 0x38)?) != IMAGE_MAGIC {
        return Err(BootError::NotAnImage(
            "bad magic (compressed Image.gz and vmlinux ELF are not accepted)",
        ));
    }
    let (mut text_offset, image_size, flags) = (field(0x08)?, field(0x10)?, field(0x18)?);
    if flags & 1 != 0 {
        return Err(BootError::NotAnImage("big-endian kernels are not supported"));
    }
    if image_size == 0 {
        // Pre-3.17 kernels: text_offset is 0x80000 (booting.rst).
        text_offset = 0x80000;
    }
    Ok(ImageHeader {
        text_offset,
        image_size,
        flags,
    })
}

/// Loads `kernel` at the 2 MiB-aligned base of RAM (+ text_offset).
pub fn load_kernel(mem: &GuestMemory, kernel: &File, ram_size: u64) -> Result<LoadedKernel, BootError> {
    let file_len = kernel.metadata()?.len();
    let mut header = [0u8; IMAGE_HEADER_LEN];
    platform::read_exact_at(kernel, &mut header, 0)?;
    let h = parse_image_header(&header)?;
    // Header fields are untrusted: every sum is checked.
    let have = ram_size.saturating_sub(FDT_MAX);
    let too_big = |need| BootError::DoesNotFit {
        what: "kernel",
        need,
        have,
    };
    // image_size == 0 (old kernels): leave generous room after the file.
    let footprint = if h.image_size == 0 {
        file_len.saturating_add(SZ_2M)
    } else {
        h.image_size.max(file_len)
    };
    let load = layout::DRAM_BASE
        .checked_add(h.text_offset)
        .ok_or(too_big(footprint))?;
    let end = load.checked_add(footprint).ok_or(too_big(footprint))?;
    if end > layout::DRAM_BASE.saturating_add(have) {
        return Err(too_big(footprint));
    }
    read_into_guest(mem, kernel, load, file_len)?;
    Ok(LoadedKernel { entry: load, end })
}

/// Places an initrd directly after the kernel footprint, which keeps it inside the
/// 1 GiB-aligned, ≤32 GiB window that must also cover the Image (booting.rst).
pub fn load_initrd(
    mem: &GuestMemory,
    initrd: &[u8],
    after: u64,
    limit: u64,
) -> Result<(u64, u64), BootError> {
    let len = initrd.len() as u64;
    let start = after.checked_next_multiple_of(SZ_2M).unwrap_or(u64::MAX);
    if start.checked_add(len).is_none_or(|end| end > limit) {
        return Err(BootError::DoesNotFit {
            what: "initrd",
            need: len,
            have: limit.saturating_sub(start),
        });
    }
    mem.write(start, initrd)?;
    Ok((start, len))
}

fn read_into_guest(mem: &GuestMemory, file: &File, gpa: u64, len: u64) -> Result<(), BootError> {
    let dst = mem.host_ptr(gpa, len as usize)?;
    // SAFETY: `dst` is valid for `len` bytes of guest RAM, and no vCPU runs yet.
    let buf = unsafe { std::slice::from_raw_parts_mut(dst, len as usize) };
    platform::read_exact_at(file, buf, 0)?;
    Ok(())
}

/// A virtio-mmio transport to describe in the devicetree.
#[derive(Debug, Clone, Copy)]
pub struct MmioDevice {
    pub base: u64,
    pub size: u64,
    pub spi: u32,
}

/// Everything the devicetree describes.
#[derive(Debug)]
pub struct Machine<'a> {
    pub mpidrs: &'a [u64],
    pub ram_size: u64,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    pub gic_dist: (u64, u64),
    pub gic_redist: (u64, u64),
    pub virtio: &'a [MmioDevice],
    /// Seeds the guest CRNG before any driver runs (`/chosen/rng-seed`).
    pub rng_seed: [u8; 64],
}

const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_PCLK: u32 = 2;
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_EDGE_RISING: u32 = 1;
const IRQ_LEVEL_HIGH: u32 = 4;

pub fn build_fdt(m: &Machine<'_>) -> Result<Vec<u8>, FdtError> {
    let mut f = Fdt::new();
    f.begin_node("");
    f.prop_strs("compatible", &["linux,dummy-virt"]);
    // Linux defaults to 1/1 when absent, DTSpec to 2/1: always state both.
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("interrupt-parent", PHANDLE_GIC);

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    for &mpidr in m.mpidrs {
        f.begin_node(&format!("cpu@{mpidr:x}"));
        f.prop_str("device_type", "cpu");
        f.prop_str("compatible", "arm,armv8");
        f.prop_str("enable-method", "psci");
        f.prop_u32("reg", mpidr as u32);
        f.end_node();
    }
    f.end_node();

    f.begin_node(&format!("memory@{:x}", layout::DRAM_BASE));
    f.prop_str("device_type", "memory");
    f.prop_u64s("reg", &[layout::DRAM_BASE, m.ram_size]);
    f.end_node();

    f.begin_node("chosen");
    f.prop_str("bootargs", m.cmdline);
    f.prop_str("stdout-path", &format!("/uart@{:x}", layout::UART));
    f.prop("rng-seed", &m.rng_seed);
    if let Some((start, len)) = m.initrd {
        f.prop_u64s("linux,initrd-start", &[start]);
        f.prop_u64s("linux,initrd-end", &[start + len]);
    }
    f.end_node();

    f.begin_node(&format!("intc@{:x}", m.gic_dist.0));
    f.prop_str("compatible", "arm,gic-v3");
    f.prop_null("interrupt-controller");
    f.prop_u32("#interrupt-cells", 3);
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_null("ranges");
    f.prop_u64s(
        "reg",
        &[m.gic_dist.0, m.gic_dist.1, m.gic_redist.0, m.gic_redist.1],
    );
    f.prop_u32("phandle", PHANDLE_GIC);
    f.end_node();

    f.begin_node("timer");
    f.prop_str("compatible", "arm,armv8-timer");
    f.prop_null("always-on");
    // Positional: secure phys, non-secure phys, virtual (the one an EL1 guest uses), hyp.
    f.prop_cells(
        "interrupts",
        &[
            GIC_PPI,
            13,
            IRQ_LEVEL_HIGH,
            GIC_PPI,
            14,
            IRQ_LEVEL_HIGH,
            GIC_PPI,
            11,
            IRQ_LEVEL_HIGH,
            GIC_PPI,
            10,
            IRQ_LEVEL_HIGH,
        ],
    );
    f.end_node();

    // AMBA PrimeCell devices (the PL031) never probe without an "apb_pclk" clock.
    f.begin_node("apb-pclk");
    f.prop_str("compatible", "fixed-clock");
    f.prop_u32("#clock-cells", 0);
    f.prop_u32("clock-frequency", 24_000_000);
    f.prop_str("clock-output-names", "clk24mhz");
    f.prop_u32("phandle", PHANDLE_APB_PCLK);
    f.end_node();

    f.begin_node("psci");
    f.prop_strs("compatible", &["arm,psci-1.0", "arm,psci-0.2", "arm,psci"]);
    f.prop_str("method", "hvc");
    f.end_node();

    f.begin_node(&format!("uart@{:x}", layout::UART));
    f.prop_str("compatible", "ns16550a");
    f.prop_u64s("reg", &[layout::UART, 0x1000]);
    f.prop_u32("clock-frequency", 1_843_200);
    f.prop_cells("interrupts", &[GIC_SPI, layout::SPI_UART, IRQ_LEVEL_HIGH]);
    f.end_node();

    f.begin_node(&format!("rtc@{:x}", layout::RTC));
    f.prop_strs("compatible", &["arm,pl031", "arm,primecell"]);
    f.prop_u64s("reg", &[layout::RTC, 0x1000]);
    f.prop_u32("clocks", PHANDLE_APB_PCLK);
    f.prop_str("clock-names", "apb_pclk");
    f.end_node();

    for d in m.virtio {
        f.begin_node(&format!("virtio_mmio@{:x}", d.base));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_u64s("reg", &[d.base, d.size]);
        f.prop_cells("interrupts", &[GIC_SPI, d.spi, IRQ_EDGE_RISING]);
        f.prop_null("dma-coherent");
        f.end_node();
    }

    f.end_node();
    f.finish(m.mpidrs.first().copied().unwrap_or(0) as u32)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fdt::decode;

    #[test]
    fn parses_the_bootstrap_kernel_header() {
        // First 64 bytes of Firecracker CI's aarch64 vmlinux-6.18.48 (EFI-stub Image).
        let mut h = [0u8; 64];
        h[0..4].copy_from_slice(&0xfa40_5a4du32.to_le_bytes());
        h[0x10..0x18].copy_from_slice(&0x13a_0000u64.to_le_bytes());
        h[0x18..0x20].copy_from_slice(&0xau64.to_le_bytes());
        h[0x38..0x3c].copy_from_slice(&IMAGE_MAGIC.to_le_bytes());
        let p = parse_image_header(&h).unwrap();
        assert_eq!(
            p,
            ImageHeader {
                text_offset: 0,
                image_size: 0x13a_0000,
                flags: 0xa
            }
        );

        h[0x18] |= 1;
        assert!(matches!(parse_image_header(&h), Err(BootError::NotAnImage(_))));
        h[0x38] = 0;
        assert!(matches!(parse_image_header(&h), Err(BootError::NotAnImage(_))));
        assert!(parse_image_header(&h[..10]).is_err());
    }

    #[test]
    fn devicetree_matches_the_boot_contract() {
        let mpidrs = [
            super::super::mpidr(0),
            super::super::mpidr(1),
            super::super::mpidr(17),
        ];
        let virtio = [MmioDevice {
            base: layout::VIRTIO_MMIO,
            size: 0x200,
            spi: layout::SPI_VIRTIO_MMIO,
        }];
        let m = Machine {
            mpidrs: &mpidrs,
            ram_size: 512 << 20,
            cmdline: "console=ttyS0 panic=-1",
            initrd: Some((0x8200_0000, 0x1000)),
            gic_dist: (layout::GIC_DIST, 0x1_0000),
            gic_redist: (layout::GIC_REDIST, 3 * 0x2_0000),
            virtio: &virtio,
            rng_seed: [7; 64],
        };
        let root = decode::parse(&build_fdt(&m).unwrap());
        assert_eq!((root.u32("#address-cells"), root.u32("#size-cells")), (2, 2));
        let cpus = root.path("cpus");
        assert_eq!(cpus.children.len(), 3);
        assert_eq!(root.path("cpus/cpu@101").u32("reg"), 0x101); // vCPU 17 -> Aff1=1, Aff0=1
        assert_eq!(root.path("cpus/cpu@0").str("enable-method"), "psci");
        let chosen = root.path("chosen");
        assert_eq!(chosen.str("bootargs"), "console=ttyS0 panic=-1");
        assert_eq!(chosen.cells("linux,initrd-end"), vec![0, 0x8200_1000]);
        assert_eq!(chosen.props["rng-seed"].len(), 64);
        let timer = root.path("timer").cells("interrupts");
        assert_eq!(&timer[6..9], &[1, 11, 4]); // [2] = EL1 virtual timer, PPI 11 = INTID 27
        let gic = root.path("intc@8000000");
        assert_eq!(
            gic.cells("reg"),
            vec![0, 0x0800_0000, 0, 0x1_0000, 0, 0x080a_0000, 0, 0x6_0000]
        );
        assert_eq!(root.path("rtc@9010000").str("clock-names"), "apb_pclk");
        assert_eq!(
            root.path("virtio_mmio@a000000").cells("interrupts"),
            vec![0, 16, 1]
        );
    }
}
