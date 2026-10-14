//! Direct kernel boot: arm64 `Image` loading and devicetree generation.
//!
//! Rules follow Linux Documentation/arch/arm64/booting.rst as summarized, with
//! sources, in docs/research/hvf-arm64-kvm-ground-truth.md §3.

use std::fmt;
use std::fs::File;

use super::layout;
use crate::fdt::{Fdt, FdtError};
use crate::memory::{FileSpan, OutOfBounds, Reentered};
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
    Access(Reentered),
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
            BootError::Access(e) => write!(f, "{e}"),
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
impl From<Reentered> for BootError {
    fn from(e: Reentered) -> Self {
        BootError::Access(e)
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

/// Where `kernel` goes, from its header alone: at [`layout::KERNEL_BASE`] (+ text_offset),
/// 2 MiB aligned. The Image is copied in later, whole, as the span of the file this gives.
pub fn plan_kernel(kernel: &File, ram_size: u64) -> Result<(LoadedKernel, FileSpan), BootError> {
    let file_len = kernel.metadata()?.len();
    let mut header = [0u8; IMAGE_HEADER_LEN];
    platform::read_exact_at(kernel, &mut header, 0)?;
    let h = parse_image_header(&header)?;
    // Header fields are untrusted: every sum is checked.
    let have = ram_size.saturating_sub(FDT_MAX + layout::SYSTEM_MEM_SIZE);
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
    let load = layout::KERNEL_BASE
        .checked_add(h.text_offset)
        .ok_or(too_big(footprint))?;
    let end = load.checked_add(footprint).ok_or(too_big(footprint))?;
    if end > layout::KERNEL_BASE.saturating_add(have) {
        return Err(too_big(footprint));
    }
    let len = usize::try_from(file_len).map_err(|_| too_big(footprint))?;
    Ok((
        LoadedKernel { entry: load, end },
        FileSpan {
            gpa: load,
            len,
            offset: 0,
        },
    ))
}

/// Where an initrd of `len` bytes starts: directly after the kernel footprint, which keeps
/// it inside the 1 GiB-aligned, ≤32 GiB window that must also cover the Image
/// (booting.rst).
pub fn place_initrd(len: u64, after: u64, limit: u64) -> Result<u64, BootError> {
    let start = after.checked_next_multiple_of(SZ_2M).unwrap_or(u64::MAX);
    if start.checked_add(len).is_none_or(|end| end > limit) {
        return Err(BootError::DoesNotFit {
            what: "initrd",
            need: len,
            have: limit.saturating_sub(start),
        });
    }
    Ok(start)
}

/// A virtio-mmio transport to describe in the devicetree.
#[derive(Debug, Clone, Copy)]
pub struct MmioDevice {
    pub base: u64,
    pub size: u64,
    pub spi: u32,
    /// Whether it is the machine's IOMMU (virtio-iommu, D154), which `PciBridge::iommu`'s
    /// functions' DMA goes through.
    pub iommu: bool,
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
    /// Whether the vCPUs have EL2: the kernel, entered there, is a hypervisor, and its
    /// own calls to EL2 are its own, so PSCI is reached by SMC; the GIC's virtualization
    /// interface raises its maintenance interrupt.
    pub el2: bool,
    /// A PCI host bridge, where the machine has a PCI bus (D153).
    pub pci: Option<PciBridge>,
}

/// A PCI host bridge (D153): its ECAM window and the 32-bit memory window its functions'
/// BARs are placed in, each an address and a length; each device that raises INTx, its
/// pin and the SPI its line is; and where its functions' MSIs go (D154).
#[derive(Debug, Clone)]
pub struct PciBridge {
    pub ecam: (u64, u64),
    pub mem32: (u64, u64),
    /// Its 64-bit window, above 4 GiB, where its functions have 64-bit BARs (D154).
    pub mem64: Option<(u64, u64)>,
    /// The requester IDs of its functions the machine's IOMMU translates (D154).
    pub iommu: Vec<u32>,
    pub intx: Vec<Intx>,
    pub msi: Option<Msi>,
}

/// Where a PCI bus's MSIs go (D154): a GICv2m frame, whose doorbell's writes name SPIs; or
/// the GIC's ITS, its register frame's address and length, which translates each MSI by
/// the ID of the device that wrote it, its requester ID (Linux Documentation/devicetree/
/// bindings/pci/pci-msi.txt, msi-map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msi {
    V2m(MsiFrame),
    Its { base: u64, size: u64 },
}

/// A PCI device's INTx: its device number on bus 0, its pin (1 for INTA#) and its SPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intx {
    pub device: u8,
    pub pin: u8,
    pub spi: u32,
}

/// A GICv2m frame (Arm GIC architecture, GICv2m; Linux Documentation/devicetree/bindings/
/// interrupt-controller/arm,gic.yaml): its registers' window, and the SPIs its doorbell
/// raises, by INTID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiFrame {
    pub base: u64,
    pub size: u64,
    pub first_intid: u32,
    pub count: u32,
}

const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_PCLK: u32 = 2;
const PHANDLE_V2M: u32 = 3;
const PHANDLE_IOMMU: u32 = 4;
const PHANDLE_ITS: u32 = 5;
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

    // Guest RAM minus the system region the VMM writes for devices (layout::SYSTEM_MEM_SIZE).
    f.begin_node(&format!("memory@{:x}", layout::KERNEL_BASE));
    f.prop_str("device_type", "memory");
    f.prop_u64s(
        "reg",
        &[
            layout::KERNEL_BASE,
            m.ram_size.saturating_sub(layout::SYSTEM_MEM_SIZE),
        ],
    );
    f.end_node();

    f.begin_node(&format!("vmgenid@{:x}", layout::VMGENID));
    f.prop_str("compatible", "microsoft,vmgenid");
    f.prop_u64s("reg", &[layout::VMGENID, layout::VMGENID_SIZE]);
    f.prop_cells("interrupts", &[GIC_SPI, layout::SPI_VMGENID, IRQ_EDGE_RISING]);
    f.end_node();

    f.begin_node("chosen");
    f.prop_str("bootargs", m.cmdline);
    f.prop_str("stdout-path", &format!("/uart@{:x}", layout::UART));
    f.prop("rng-seed", &m.rng_seed);
    if let Some((start, len)) = m.initrd {
        f.prop_u64s("linux,initrd-start", &[start]);
        f.prop_u64s("linux,initrd-end", &[start + len]);
    }
    // The BARs where the VMM placed them, kept (drivers/pci/of.c,
    // of_pci_check_probe_only).
    if m.pci.is_some() {
        f.prop_u32("linux,pci-probe-only", 1);
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
    if m.el2 {
        // The maintenance interrupt, PPI 9 (INTID 25), as the GICv3 binding and the
        // framework have it (hv_gic_types.h, HV_GIC_INT_MAINTENANCE).
        f.prop_cells("interrupts", &[GIC_PPI, 9, IRQ_LEVEL_HIGH]);
    }
    f.prop_u32("phandle", PHANDLE_GIC);
    // The GICv2m frame of a PCI bus's MSIs (D154): Linux's GICv3 driver takes one where
    // the distributor has no LPIs (irq-gic-v3.c, gicv2m_init), and the properties say its
    // SPIs (irq-gic-v2m.c, "arm,msi-base-spi" by INTID). Or its ITS (Documentation/
    // devicetree/bindings/interrupt-controller/arm,gic-v3.yaml): one cell, the device ID.
    match m.pci.as_ref().and_then(|p| p.msi) {
        Some(Msi::V2m(v2m)) => {
            f.begin_node(&format!("v2m@{:x}", v2m.base));
            f.prop_str("compatible", "arm,gic-v2m-frame");
            f.prop_null("msi-controller");
            f.prop_u64s("reg", &[v2m.base, v2m.size]);
            f.prop_u32("arm,msi-base-spi", v2m.first_intid);
            f.prop_u32("arm,msi-num-spis", v2m.count);
            f.prop_u32("phandle", PHANDLE_V2M);
            f.end_node();
        }
        Some(Msi::Its { base, size }) => {
            f.begin_node(&format!("its@{base:x}"));
            f.prop_str("compatible", "arm,gic-v3-its");
            f.prop_null("msi-controller");
            f.prop_u32("#msi-cells", 1);
            f.prop_u64s("reg", &[base, size]);
            f.prop_u32("phandle", PHANDLE_ITS);
            f.end_node();
        }
        None => {}
    }
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
    f.prop_str("method", if m.el2 { "smc" } else { "hvc" });
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

    // Linux Documentation/devicetree/bindings/pci/host-generic-pci.yaml: ECAM's window,
    // bus 0, and its ranges at the same addresses in both spaces: 32-bit memory (phys.hi
    // 0x02000000), and where its functions have 64-bit BARs, 64-bit memory (0x03000000),
    // not prefetchable, so that BARs of either kind fit it (Documentation/devicetree/
    // bindings/pci/pci.txt, IEEE 1275's PCI bus binding).
    if let Some(p) = &m.pci {
        f.begin_node(&format!("pcie@{:x}", p.ecam.0));
        f.prop_str("compatible", "pci-host-ecam-generic");
        f.prop_str("device_type", "pci");
        f.prop_u64s("reg", &[p.ecam.0, p.ecam.1]);
        f.prop_cells("bus-range", &[0, 0]);
        f.prop_u32("#address-cells", 3);
        f.prop_u32("#size-cells", 2);
        let range = |space: u32, (base, len): (u64, u64)| {
            [
                space,
                (base >> 32) as u32,
                base as u32,
                (base >> 32) as u32,
                base as u32,
                (len >> 32) as u32,
                len as u32,
            ]
        };
        let mut ranges = range(0x0200_0000, p.mem32).to_vec();
        if let Some(mem64) = p.mem64 {
            ranges.extend(range(0x0300_0000, mem64));
        }
        f.prop_cells("ranges", &ranges);
        f.prop_u32("linux,pci-domain", 0);
        f.prop_null("dma-coherent");
        // Each device's INTx by its device number and pin (the binding's interrupt-map,
        // Documentation/devicetree/bindings/pci/pci.txt): its own SPI, level-triggered.
        if !p.intx.is_empty() {
            f.prop_u32("#interrupt-cells", 1);
            f.prop_cells("interrupt-map-mask", &[0xf800, 0, 0, 7]);
            let mut map = Vec::with_capacity(p.intx.len() * 10);
            for i in &p.intx {
                // Child: phys.hi's device (bits 15:11), phys.mid, phys.lo, the pin; the
                // GIC, its two address cells, and its SPI specifier.
                map.extend([
                    u32::from(i.device) << 11,
                    0,
                    0,
                    u32::from(i.pin),
                    PHANDLE_GIC,
                    0,
                    0,
                    GIC_SPI,
                    i.spi,
                    IRQ_LEVEL_HIGH,
                ]);
            }
            f.prop_cells("interrupt-map", &map);
        }
        match p.msi {
            Some(Msi::V2m(_)) => f.prop_u32("msi-parent", PHANDLE_V2M),
            // Each function's requester ID its device ID at the ITS (pci-msi.txt).
            Some(Msi::Its { .. }) => f.prop_cells("msi-map", &[0, PHANDLE_ITS, 0, 0x1_0000]),
            None => {}
        }
        // Each function the IOMMU translates, by its requester ID, to the same ID of the
        // IOMMU's (Documentation/devicetree/bindings/pci/pci-iommu.txt); the rest, passed
        // through, no IOMMU's.
        if m.virtio.iter().any(|d| d.iommu) && !p.iommu.is_empty() {
            let map: Vec<u32> = p
                .iommu
                .iter()
                .flat_map(|&rid| [rid, PHANDLE_IOMMU, rid, 1])
                .collect();
            f.prop_cells("iommu-map", &map);
        }
        f.end_node();
    }

    for d in m.virtio {
        f.begin_node(&format!("virtio_mmio@{:x}", d.base));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_u64s("reg", &[d.base, d.size]);
        f.prop_cells("interrupts", &[GIC_SPI, d.spi, IRQ_EDGE_RISING]);
        f.prop_null("dma-coherent");
        // virtio-iommu's binding (Documentation/devicetree/bindings/virtio/iommu.yaml): one
        // cell, the endpoint's ID.
        if d.iommu {
            f.prop_u32("#iommu-cells", 1);
            f.prop_u32("phandle", PHANDLE_IOMMU);
        }
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
            iommu: false,
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
            el2: false,
            pci: None,
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
        // RAM starts above the system region that holds the VM generation ID.
        assert_eq!(
            root.path("memory@80200000").cells("reg"),
            vec![0, 0x8020_0000, 0, (512 << 20) - 0x20_0000]
        );
        let genid = root.path("vmgenid@80000000");
        assert_eq!(genid.str("compatible"), "microsoft,vmgenid");
        assert_eq!(genid.cells("reg"), vec![0, 0x8000_0000, 0, 16]);
        assert_eq!(genid.cells("interrupts"), vec![0, 3, 1]);
        assert_eq!(
            root.path("virtio_mmio@a000000").cells("interrupts"),
            vec![0, 16, 1]
        );
        // At EL1 the kernel calls PSCI by HVC, and the GIC raises no maintenance interrupt.
        assert_eq!(root.path("psci").str("method"), "hvc");
        assert!(!gic.props.contains_key("interrupts"));
        // At EL2 the kernel is the hypervisor: PSCI by SMC, the GIC's maintenance interrupt
        // PPI 9 (INTID 25), and the EL2 physical timer still the timer's fourth (PPI 10).
        let at_el2 = decode::parse(
            &build_fdt(&Machine {
                el2: true,
                pci: None,
                ..m
            })
            .unwrap(),
        );
        assert_eq!(at_el2.path("psci").str("method"), "smc");
        assert_eq!(at_el2.path("intc@8000000").cells("interrupts"), vec![1, 9, 4]);
        assert_eq!(&at_el2.path("timer").cells("interrupts")[9..12], &[1, 10, 4]);
        // No PCI bus but where the machine has one (D153): pci-host-generic's node, its
        // ECAM window and 32-bit memory range, and the BARs kept where they were placed.
        assert!(!root.children.keys().any(|name| name.starts_with("pcie@")));
        assert!(!root.path("chosen").props.contains_key("linux,pci-probe-only"));
        assert!(
            !root
                .path("intc@8000000")
                .children
                .keys()
                .any(|name| name.starts_with("v2m@"))
        );
        let pci = PciBridge {
            ecam: (layout::PCI_ECAM, 1 << 20),
            mem32: (layout::PCI_MEM32, layout::PCI_MEM32_SIZE),
            mem64: Some((0x1_0000_0000, 0x10_0000)),
            iommu: vec![0x8],
            intx: vec![Intx {
                device: 1,
                pin: 1,
                spi: layout::SPI_PCI,
            }],
            msi: Some(Msi::V2m(MsiFrame {
                base: layout::GIC_MSI,
                size: 0x1_0000,
                first_intid: 32 + layout::SPI_MSI,
                count: layout::MSI_SPIS,
            })),
        };
        let with_pci = decode::parse(
            &build_fdt(&Machine {
                pci: Some(pci.clone()),
                ..m
            })
            .unwrap(),
        );
        let bridge = with_pci.path("pcie@3f000000");
        assert_eq!(bridge.str("compatible"), "pci-host-ecam-generic");
        assert_eq!(bridge.str("device_type"), "pci");
        assert_eq!(bridge.cells("reg"), vec![0, 0x3f00_0000, 0, 0x10_0000]);
        assert_eq!(bridge.cells("bus-range"), vec![0, 0]);
        assert_eq!(
            bridge.cells("ranges"),
            vec![
                0x0200_0000,
                0,
                0x1000_0000,
                0,
                0x1000_0000,
                0,
                0x2f00_0000,
                0x0300_0000,
                1,
                0,
                1,
                0,
                0,
                0x10_0000
            ]
        );
        assert_eq!(with_pci.path("chosen").u32("linux,pci-probe-only"), 1);
        // Its devices' INTx (D154): device 1's INTA# on its own SPI, level-triggered, and
        // its MSIs to the GIC's v2m frame, its SPIs by INTID.
        assert_eq!(bridge.u32("#interrupt-cells"), 1);
        assert_eq!(bridge.cells("interrupt-map-mask"), vec![0xf800, 0, 0, 7]);
        assert_eq!(
            bridge.cells("interrupt-map"),
            vec![1 << 11, 0, 0, 1, PHANDLE_GIC, 0, 0, 0, layout::SPI_PCI, 4]
        );
        assert_eq!(bridge.u32("msi-parent"), PHANDLE_V2M);
        let v2m = with_pci.path("intc@8000000/v2m@8020000");
        assert_eq!(v2m.str("compatible"), "arm,gic-v2m-frame");
        assert!(v2m.props.contains_key("msi-controller"));
        assert_eq!(v2m.cells("reg"), vec![0, 0x0802_0000, 0, 0x1_0000]);
        assert_eq!(v2m.u32("arm,msi-base-spi"), 32 + layout::SPI_MSI);
        assert_eq!(v2m.u32("arm,msi-num-spis"), layout::MSI_SPIS);
        assert_eq!(v2m.u32("phandle"), PHANDLE_V2M);
        // Or to the GIC's ITS: each function's requester ID its device ID there.
        let with_its = decode::parse(
            &build_fdt(&Machine {
                pci: Some(PciBridge {
                    msi: Some(Msi::Its {
                        base: layout::GIC_ITS,
                        size: layout::GIC_ITS_SIZE,
                    }),
                    ..pci.clone()
                }),
                ..m
            })
            .unwrap(),
        );
        let bridge_its = with_its.path("pcie@3f000000");
        assert!(!bridge_its.props.contains_key("msi-parent"));
        assert_eq!(bridge_its.cells("msi-map"), vec![0, PHANDLE_ITS, 0, 0x1_0000]);
        let its = with_its.path("intc@8000000/its@8080000");
        assert_eq!(its.str("compatible"), "arm,gic-v3-its");
        assert!(its.props.contains_key("msi-controller"));
        assert_eq!(its.u32("#msi-cells"), 1);
        assert_eq!(its.cells("reg"), vec![0, 0x0808_0000, 0, 0x2_0000]);
        assert_eq!(its.u32("phandle"), PHANDLE_ITS);
        assert!(
            !with_its
                .path("intc@8000000")
                .children
                .keys()
                .any(|name| name.starts_with("v2m@"))
        );
        // No IOMMU but where the machine has one (D154): its virtio node's one cell, the
        // endpoint's ID, and the bridge's map of each function it translates to it.
        assert!(!bridge.props.contains_key("iommu-map"));
        let iommu = [MmioDevice {
            base: layout::VIRTIO_MMIO,
            size: 0x200,
            spi: layout::SPI_VIRTIO_MMIO,
            iommu: true,
        }];
        let with_iommu = decode::parse(
            &build_fdt(&Machine {
                virtio: &iommu,
                pci: Some(pci),
                ..m
            })
            .unwrap(),
        );
        let node = with_iommu.path(&format!("virtio_mmio@{:x}", layout::VIRTIO_MMIO));
        assert_eq!(node.u32("#iommu-cells"), 1);
        assert_eq!(node.u32("phandle"), PHANDLE_IOMMU);
        assert_eq!(
            with_iommu.path("pcie@3f000000").cells("iommu-map"),
            vec![0x8, PHANDLE_IOMMU, 0x8, 1]
        );
    }
}
