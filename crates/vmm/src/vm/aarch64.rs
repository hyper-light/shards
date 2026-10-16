//! The arm64 machine on any backend: memory map, in-kernel GICv3, devicetree, devices.
//! It is built either to boot a kernel or to resume a snapshot.

use std::fs::File;
use std::sync::Arc;

use super::{Config, Console};
use crate::arch::aarch64::state::{MachineState, VcpuState};
use crate::arch::aarch64::{self, Entry, boot, layout};
use crate::devices::control::Control;
use crate::devices::power::Power;
use crate::devices::rtc::Pl031;
use crate::devices::serial::Serial;
use crate::devices::virtio::{mmio as virtio_mmio, pmem};
use crate::devices::vmgenid::VmGenId;
use crate::devices::{Interrupt, MmioBus};
use crate::hv::{self, Gic, GicLayout};
use crate::memory::GuestMemory;
use crate::snapshot::codec::{Reader, Writer};
use crate::snapshot::{MachineConfig, Snapshot};
use crate::{debug, platform, warn};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Smallest guest that can hold a kernel, its early allocations and the DTB window.
const MIN_MEMORY_MIB: u64 = 64;
const SPI_INTID_BASE: u32 = 32;

/// How the vCPUs start.
#[derive(Debug)]
pub enum Start {
    /// The boot vCPU enters the kernel here; the others wait for PSCI CPU_ON.
    Boot(Entry),
    Restore(Restored),
}

#[derive(Debug)]
pub struct Restored {
    pub vcpus: Vec<VcpuState>,
    /// The guest counter when the snapshot was taken. At release every vCPU gets one
    /// offset, so the counter continues from here and agrees across CPUs.
    pub counter: u64,
    /// The CPU the snapshot was taken on; a restore on a different one is refused.
    pub cpu_id: Vec<(u16, u64)>,
    /// The GIC device's state, applied once every vCPU exists (see [`finish`]).
    pub gic: Vec<u8>,
    /// The device bus's saved state, applied by [`finish`].
    pub devices: Vec<u8>,
    /// The snapshot's working set, to prefetch before the guest runs; empty for none.
    pub working_set: Vec<hv::Touch>,
}

/// What vCPU `index` sets itself up from ([`split`]): moved to its thread, its own alone.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "moved once to each vCPU's thread at its setup: boxing the state would add an allocation per vCPU to every restore"
)]
pub enum VcpuStart {
    /// The boot vCPU's entry; `None` for the others, which wait for PSCI CPU_ON.
    Boot(Option<Entry>),
    Restore {
        state: VcpuState,
        /// vCPU 0's part.
        first: Option<First>,
    },
}

/// vCPU 0's part of a restore: the CPU the snapshot was taken on, and the working set it
/// prefetches.
#[derive(Debug)]
pub struct First {
    cpu_id: Vec<(u16, u64)>,
    working_set: Vec<hv::Touch>,
}

/// What a restore's start leaves the machine once the vCPUs have theirs ([`split`]):
/// applied by [`finish`] once every vCPU exists.
#[derive(Debug)]
pub struct Remainder {
    counter: u64,
    gic: Vec<u8>,
    devices: Vec<u8>,
}

/// What vCPU 0 found as it set up, for the machine to keep.
#[derive(Debug, Default)]
pub struct SetUp {
    /// Pages of the working set it prefetched.
    prefetched: usize,
}

/// Divides `start` among `vcpus` vCPUs and the machine: each vCPU takes only its own
/// part, and none is shared.
pub fn split(start: Start, vcpus: usize) -> Result<(Vec<VcpuStart>, Option<Remainder>), String> {
    match start {
        Start::Boot(entry) => Ok((
            (0..vcpus)
                .map(|index| VcpuStart::Boot((index == 0).then_some(entry)))
                .collect(),
            None,
        )),
        Start::Restore(r) => {
            if r.vcpus.len() < vcpus {
                return Err(format!("the snapshot has {} vCPUs, not {vcpus}", r.vcpus.len()));
            }
            let mut first = Some(First {
                cpu_id: r.cpu_id,
                working_set: r.working_set,
            });
            let starts = r
                .vcpus
                .into_iter()
                .take(vcpus)
                .map(|state| VcpuStart::Restore {
                    state,
                    first: first.take(),
                })
                .collect();
            Ok((
                starts,
                Some(Remainder {
                    counter: r.counter,
                    gic: r.gic,
                    devices: r.devices,
                }),
            ))
        }
    }
}

/// The devices' address space: MMIO only on arm64.
pub type Bus = MmioBus;

/// What [`finish`] needs besides the VM and bus.
pub type Finish = VmGenId;

/// A VM ready for its vCPUs. Field order is drop order: the VM goes before the memory
/// it maps.
#[derive(Debug)]
pub struct Machine {
    pub vm: hv::Vm,
    pub memory: Arc<GuestMemory>,
    pub bus: Bus,
    pub serial: Arc<Serial>,
    pub control: Arc<Control>,
    /// Device-raised power events; on arm64 power management is PSCI, so none do yet.
    pub power: Arc<Power>,
    pub finish: Finish,
    pub start: Start,
    pub config: MachineConfig,
}

#[derive(Debug)]
struct GicLine {
    gic: Gic,
    intid: u32,
}

impl Interrupt for GicLine {
    fn set_level(&self, level: bool) {
        if let Err(e) = self.gic.set_spi(self.intid, level) {
            warn!("interrupt {}: {e}", self.intid);
        }
    }
}

/// The SPI of PCI device `device`'s INTx (layout::SPI_PCI).
fn pci_intx_spi(device: u8) -> u32 {
    layout::SPI_PCI + u32::from(device).saturating_sub(1)
}

/// A PCI bus's interrupts on this machine (D154): each device's INTx on an SPI of its own,
/// level-triggered; and its MSIs to the GIC's ITS (KVM's), by each function's requester
/// ID, or to its GICv2m frame `frame`, whose SPIs they name (Hypervisor.framework's).
struct PciLines {
    gic: Gic,
    #[cfg(hv = "hvf")]
    frame: boot::MsiFrame,
}

impl super::PciInterrupts for PciLines {
    fn intx(&self, device: u8) -> Arc<dyn Interrupt> {
        Arc::new(GicLine {
            gic: self.gic.clone(),
            intid: SPI_INTID_BASE + pci_intx_spi(device),
        })
    }

    #[cfg(hv = "hvf")]
    fn msi(&self, _requester: u32) -> Arc<dyn crate::devices::pci::Msi> {
        Arc::new(V2mFrame {
            gic: self.gic.clone(),
            frame: self.frame,
        })
    }

    #[cfg(not(hv = "hvf"))]
    fn msi(&self, requester: u32) -> Arc<dyn crate::devices::pci::Msi> {
        Arc::new(ItsMsi {
            gic: self.gic.clone(),
            requester,
        })
    }

    /// KVM's irqfds: INTx on the device's SPI, resampled at its EOI; each MSI routed to the
    /// ITS with the function's requester ID.
    #[cfg(target_os = "linux")]
    fn route(&self, requester: u32) -> Option<Arc<dyn crate::devices::pci::vfio::Route>> {
        let device = u8::try_from(requester >> 3).ok()?;
        Some(Arc::new(IrqfdRoute {
            gic: self.gic.clone(),
            intx: SPI_INTID_BASE + pci_intx_spi(device),
            requester,
            msis: std::sync::Mutex::new(Vec::new()),
        }))
    }
}

/// The MSIs of the function of requester ID `requester`, which the VMM emulates, to the
/// GIC's ITS: translated by that ID as its device ID (KVM_SIGNAL_MSI with
/// KVM_MSI_VALID_DEVID; the devicetree's msi-map makes the two one).
#[cfg(not(hv = "hvf"))]
struct ItsMsi {
    gic: Gic,
    requester: u32,
}

#[cfg(not(hv = "hvf"))]
impl crate::devices::pci::Msi for ItsMsi {
    fn send(&self, address: u64, data: u32) {
        if let Err(e) = self.gic.its_msi(address, data, self.requester) {
            warn!("an MSI to {address:#x} with {data:#x}: {e}");
        }
    }
}

/// A passed-through function's interrupts by KVM's irqfds (D154): its INTx on SPI `intx`,
/// its MSIs on GSIs routed to the ITS with its requester ID, each with the eventfd that
/// raises it.
#[cfg(target_os = "linux")]
struct IrqfdRoute {
    gic: Gic,
    intx: u32,
    requester: u32,
    msis: std::sync::Mutex<Vec<(std::os::fd::RawFd, u32)>>,
}

#[cfg(target_os = "linux")]
impl crate::devices::pci::vfio::Route for IrqfdRoute {
    fn intx(&self, trigger: std::os::fd::RawFd, resample: std::os::fd::RawFd) -> Result<(), String> {
        self.gic
            .irqfd(trigger, self.intx, Some(resample))
            .map_err(|e| e.to_string())
    }

    fn intx_off(&self, trigger: std::os::fd::RawFd) -> Result<(), String> {
        self.gic
            .irqfd_remove(trigger, self.intx)
            .map_err(|e| e.to_string())
    }

    fn msi(&self, event: std::os::fd::RawFd, address: u64, data: u32) -> Result<(), String> {
        let gsi = self
            .gic
            .route_msi(event, address, data, self.requester)
            .map_err(|e| e.to_string())?;
        crate::sync::lock(&self.msis).push((event, gsi));
        Ok(())
    }

    fn msi_off(&self, event: std::os::fd::RawFd, _address: u64, _data: u32) -> Result<(), String> {
        let mut msis = crate::sync::lock(&self.msis);
        let Some(at) = msis.iter().position(|&(e, _)| e == event) else {
            return Ok(());
        };
        let (_, gsi) = msis.swap_remove(at);
        self.gic.unroute_msi(event, gsi).map_err(|e| e.to_string())
    }
}

/// The GIC's GICv2m frame, Hypervisor.framework's (Arm's GICv2m specification, as Linux's
/// irq-gic-v2m.c drives it): a write to its doorbell, MSI_SETSPI_NS, raises the SPI it
/// names, edge-triggered. The MSIs of the functions the VMM emulates.
#[cfg(hv = "hvf")]
#[derive(Debug)]
struct V2mFrame {
    gic: Gic,
    frame: boot::MsiFrame,
}

#[cfg(hv = "hvf")]
const V2M_MSI_SETSPI_NS: u64 = 0x040;

#[cfg(hv = "hvf")]
impl V2mFrame {
    /// Raises SPI `intid`, where it is one of the frame's.
    fn raise(&self, intid: u32) {
        let f = &self.frame;
        if intid < f.first_intid || intid - f.first_intid >= f.count {
            warn!("an MSI for INTID {intid}, none of the GICv2m frame's");
            return;
        }
        if let Err(e) = self.gic.send_msi(f.base + V2M_MSI_SETSPI_NS, intid) {
            warn!("MSI {intid}: {e}");
        }
    }
}

#[cfg(hv = "hvf")]
impl crate::devices::pci::Msi for V2mFrame {
    /// An MSI the guest aimed at the frame's doorbell; any other address is none of its.
    fn send(&self, address: u64, data: u32) {
        if address != self.frame.base + V2M_MSI_SETSPI_NS {
            warn!("an MSI to {address:#x}, not the GICv2m frame's doorbell");
            return;
        }
        self.raise(data);
    }
}

/// The smallest architected IPA width that covers guest RAM, within the host's limit.
/// The IPA sizes a VM may be given, of which [`ipa_bits`] takes the least that holds its
/// memory and devices.
const IPA_SIZES: [u32; 5] = [36, 40, 42, 44, 48];

/// The first guest address past those a VM may be given on this host: of the largest IPA
/// size it allows.
fn ipa_reach() -> Result<u64, String> {
    let max = hv::max_ipa_bits().map_err(|e| e.to_string())?;
    let bits = IPA_SIZES
        .into_iter()
        .filter(|&b| b <= max)
        .max()
        .ok_or_else(|| format!("the host allows {max} address bits, fewer than a guest needs"))?;
    Ok(1 << bits)
}

fn ipa_bits(end: u64) -> Result<u32, String> {
    let needed = 64 - end.saturating_sub(1).leading_zeros();
    let max = hv::max_ipa_bits().map_err(|e| e.to_string())?;
    IPA_SIZES
        .into_iter()
        .find(|&b| b >= needed && b <= max)
        .ok_or_else(|| {
            format!("guest memory ending at {end:#x} needs {needed} address bits; host allows {max}")
        })
}

fn ram_bytes(memory_mib: u64) -> Result<u64, String> {
    if memory_mib < MIN_MEMORY_MIB || !memory_mib.is_multiple_of(2) {
        return Err(format!(
            "guest memory must be an even number of MiB, at least {MIN_MEMORY_MIB}"
        ));
    }
    memory_mib
        .checked_mul(MIB)
        .ok_or_else(|| format!("{memory_mib} MiB of guest memory"))
}

/// Where the memory to plug starts (D133): the first GiB boundary after RAM.
fn plug_at(ram: u64) -> u64 {
    (layout::DRAM_BASE + ram).next_multiple_of(GIB)
}

/// Guest RAM: one region at DRAM_BASE.
fn ram_ranges(ram: u64) -> Result<[(u64, usize); 1], String> {
    let len = usize::try_from(ram).map_err(|_| format!("{ram} bytes of guest memory"))?;
    Ok([(layout::DRAM_BASE, len)])
}

/// What boot and restore share: the VM with its RAM and GIC, and the devices.
struct Assembled {
    vm: hv::Vm,
    bus: MmioBus,
    serial: Arc<Serial>,
    control: Arc<Control>,
    vmgenid: VmGenId,
    gic_dist_size: u64,
    redist_total: u64,
    virtio: Vec<boot::MmioDevice>,
    mpidrs: Vec<u64>,
    /// Each pmem region's guest address and length.
    pmem: Vec<(u64, u64)>,
    /// The GPU's window's length, where it has a GPU.
    gpu_window: Option<u64>,
    /// Its PCI host bridge, where it has a PCI bus (D153).
    pci: Option<boot::PciBridge>,
}

/// `vsock` is the host side of the vsock device `config` asks for; `gpu`, whether it has a
/// GPU, and its window's length: as its renderer offers, where `Some(0)`, a boot's; that
/// long, a restore's, where its renderer offers at least as much (D150).
fn assemble(
    memory: &Arc<GuestMemory>,
    config: &MachineConfig,
    console: Console,
    hosts: super::Hosts<'_>,
    gpu: Option<u64>,
    pci: super::Pci<'_>,
) -> Result<Assembled, String> {
    let ram = ram_bytes(config.memory_mib)?;
    // Memory to plug, where the guest has any, starts at the first GiB boundary after RAM
    // (`plug_at`), device memory (pmem regions) at the first after that, and the address
    // space must reach the last's end.
    let ram_end = layout::DRAM_BASE + ram;
    let plug_end = memory
        .hotplug()
        .map_or(Some(ram_end), |(gpa, _, len)| gpa.checked_add(len as u64))
        .ok_or("memory to plug overflows the address space")?;
    let pmem_at = plug_end
        .checked_next_multiple_of(GIB)
        .ok_or("pmem regions overflow the address space")?;
    let (regions, end) = super::pmem_regions(config, pmem_at)?;
    let top = if regions.is_empty() { plug_end } else { end };
    // A GPU's window, at the first GiB boundary past all of that, as long as the renderer
    // says (D144): on macOS alone so far.
    #[cfg(not(hv = "hvf"))]
    if gpu.is_some() {
        return Err("a GPU is given on macOS hosts alone so far (D144)".into());
    }
    #[cfg(not(hv = "hvf"))]
    let gpu: Option<((), crate::devices::virtio::ShmRegion)> = None;
    #[cfg(hv = "hvf")]
    let gpu = match gpu {
        None => None,
        Some(saw) => {
            let (renderer, offers) = super::gpu_renderer()?;
            let offers = offers
                .checked_next_multiple_of(PAGE)
                .ok_or("the GPU's window overflows the address space")?;
            if offers < saw {
                return Err(format!(
                    "this host's GPU offers {offers} bytes of memory the host can map, where the snapshot's guest saw {saw}"
                ));
            }
            let len = if saw == 0 { offers } else { saw };
            let at = top
                .checked_next_multiple_of(GIB)
                .ok_or("the GPU's window overflows the address space")?;
            let region = crate::devices::virtio::ShmRegion {
                id: crate::devices::virtio::gpu::HOST_VISIBLE,
                base: at,
                len,
            };
            Some((renderer, region))
        }
    };
    let top = match &gpu {
        Some((_, r)) => r
            .base
            .checked_add(r.len)
            .ok_or("the GPU's window overflows the address space")?,
        None => top,
    };
    // The PCI bus's 64-bit window past all of that, above 4 GiB, as long as its 64-bit
    // BARs need (D154).
    let mem64 = match pci.mem64()? {
        Some((len, align)) => {
            let at = top
                .max(1 << 32)
                .checked_next_multiple_of(align.max(GIB))
                .ok_or("the PCI 64-bit window overflows the address space")?;
            Some((at, len))
        }
        None => None,
    };
    let top = match mem64 {
        Some((at, len)) => at
            .checked_add(len)
            .ok_or("the PCI 64-bit window overflows the address space")?,
        None => top,
    };
    // The swap window past all of that (D159), within the addresses the host can give.
    let (swap_at, top) = super::swap_layout(config, top, ipa_reach()?)?;
    let pmem: Vec<(u64, u64)> = regions
        .iter()
        .map(|(region, gpa)| (*gpa, region.len() as u64))
        .collect();
    let ipa_bits = ipa_bits(top)?;
    let mpidrs: Vec<u64> = (0..config.vcpus).map(aarch64::mpidr).collect();
    if config.nested && !hv::el2_supported() {
        return Err(hv::NO_EL2.into());
    }
    let vm = hv::Vm::new(hv::VmConfig {
        ipa_bits,
        mpidrs: mpidrs.clone(),
        el2: config.nested,
    })
    .map_err(|e| e.to_string())?;
    for (gpa, host, len) in memory.regions() {
        // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
        unsafe { vm.map_ram(host, gpa, len) }.map_err(|e| e.to_string())?;
    }
    for (region, gpa) in &regions {
        // SAFETY: the region outlives the VM: its device sits on the bus, which
        // `Machine` and `Running` drop after the VM.
        unsafe { vm.map_device_memory(region.host(), *gpa, region.len(), false) }
            .map_err(|e| e.to_string())?;
    }
    let (hotplug, sizes) = super::hotplug_device(&vm, memory, config)?.unzip();
    let reporting = super::reporting_device(&vm, config);
    let swap = super::swap_window(&vm, swap_at)?;
    debug!("VM created and RAM mapped");

    let gp = hv::gic_params().map_err(|e| e.to_string())?;
    let redist_total = gp.redist_size * u64::from(config.vcpus);
    // A PCI bus's MSIs (D154): to the GIC's ITS where the backend has one (KVM's), which
    // translates each by the ID of the function that wrote it, so that they are isolated
    // as a guest's VFIO needs; else to the backend's own GICv2m frame.
    let msi = (!pci.test.is_empty() || !pci.vfio.is_empty()).then_some(if hv::ITS {
        boot::Msi::Its {
            base: layout::GIC_ITS,
            size: layout::GIC_ITS_SIZE,
        }
    } else {
        boot::Msi::V2m(boot::MsiFrame {
            base: layout::GIC_MSI,
            size: gp.msi_size,
            first_intid: SPI_INTID_BASE + layout::SPI_MSI,
            count: layout::MSI_SPIS,
        })
    });
    if !layout::GIC_DIST.is_multiple_of(gp.dist_align)
        || !layout::GIC_REDIST.is_multiple_of(gp.redist_align)
        || !layout::GIC_MSI.is_multiple_of(gp.msi_align)
        || !layout::GIC_ITS.is_multiple_of(64 << 10)
        || layout::GIC_DIST + gp.dist_size > layout::GIC_MSI
        || layout::GIC_MSI + gp.msi_size > layout::GIC_ITS
        || layout::GIC_ITS + layout::GIC_ITS_SIZE > layout::GIC_REDIST
        || layout::GIC_REDIST + redist_total > layout::GIC_REDIST_MAX_END
        || gp.spi_base > SPI_INTID_BASE
        || SPI_INTID_BASE + layout::SPIS > gp.spi_base + gp.spi_count
    {
        return Err(format!(
            "host GIC geometry {gp:?} does not fit the guest memory map"
        ));
    }
    let gic = vm
        .create_gic(&GicLayout {
            dist_base: layout::GIC_DIST,
            redist_base: layout::GIC_REDIST,
            msi: match msi {
                Some(boot::Msi::V2m(f)) => Some(hv::MsiFrame {
                    base: f.base,
                    first_intid: f.first_intid,
                    count: f.count,
                }),
                _ => None,
            },
            its: match msi {
                Some(boot::Msi::Its { base, .. }) => Some(base),
                _ => None,
            },
        })
        .map_err(|e| e.to_string())?;
    debug!("GIC created");

    let gpu_window = gpu.as_ref().map(|(_, region)| region.len);
    #[cfg(not(hv = "hvf"))]
    let gpu: Option<Box<dyn crate::devices::virtio::VirtioDevice>> = None;
    #[cfg(hv = "hvf")]
    let gpu = match gpu {
        None => None,
        Some((renderer, region)) => {
            // The largest request read: no legitimate command outsizes the memory it comes
            // from.
            let max = usize::try_from(ram).map_err(|_| format!("{ram} bytes of guest memory"))?;
            let served =
                crate::devices::virtio::gpu::Served::new(renderer, Box::new(vm.window()), region, PAGE, max);
            Some(Box::new(crate::devices::virtio::gpu::Gpu::new(Box::new(served)))
                as Box<dyn crate::devices::virtio::VirtioDevice>)
        }
    };
    // The IOMMU of the PCI functions behind it, last of the virtio devices (D154): no MSI
    // window bypassed, as Linux has the IOMMU map the GICv2m frame's doorbell.
    let (iommu, iommu_device, behind) = match msi.and_then(|_| super::iommu_of(pci, None)) {
        Some((translation, device, endpoints)) => (Some(translation), Some(device), endpoints),
        None => (None, None, Vec::new()),
    };
    let swap_device = swap
        .clone()
        .map(|w| Box::new(pmem::Pmem::swap(w)) as Box<dyn crate::devices::virtio::VirtioDevice>);
    let last = gpu
        .into_iter()
        .chain(hotplug)
        .chain(reporting)
        .chain(swap_device)
        .chain(iommu_device)
        .collect();
    let devices = super::virtio_devices(config, &regions, hosts, last, layout::VIRTIO_MMIO_MAX)?;
    let iommu_at = iommu.is_some().then(|| devices.len() - 1);
    let mut bus = MmioBus::default();
    let mut virtio = Vec::with_capacity(devices.len());
    // Each virtio device takes the next MMIO window and SPI, in the guest's probe order.
    for (i, device) in devices.into_iter().enumerate() {
        let spi = layout::SPI_VIRTIO_MMIO + i as u32;
        let base = layout::VIRTIO_MMIO + i as u64 * layout::VIRTIO_MMIO_STRIDE;
        let line = Arc::new(GicLine {
            gic: gic.clone(),
            intid: SPI_INTID_BASE + spi,
        });
        let transport = virtio_mmio::MmioTransport::new(device, memory.clone(), line);
        bus.insert(base, virtio_mmio::WINDOW, Arc::new(transport))?;
        virtio.push(boot::MmioDevice {
            base,
            size: virtio_mmio::WINDOW,
            spi,
            iommu: iommu_at == Some(i),
        });
    }
    let serial = Arc::new(Serial::new(
        super::console_out(console)?,
        Arc::new(GicLine {
            gic: gic.clone(),
            intid: SPI_INTID_BASE + layout::SPI_UART,
        }),
    ));
    bus.insert(layout::UART, 0x1000, serial.clone())?;
    bus.insert(layout::RTC, 0x1000, Arc::new(Pl031::default()))?;
    let control = Arc::new(Control::default());
    if let Some(window) = swap {
        control.on_swap(window);
    }
    if let Some(sizes) = sizes {
        control.on_hotplug(sizes);
    }
    // The MAC the VMM was given, before a snapshot's took its device's place: a restored
    // VM's own, which its guest takes as it resumes (D31, E1).
    if let Some(net) = hosts.net {
        control.on_mac(net.mac);
    }
    bus.insert(layout::CONTROL, 0x1000, control.clone())?;
    // A PCI bus (D153, D154): its functions from 00:01.0, their BARs in its window, each
    // device's INTx on an SPI of its own, its MSIs to the ITS or the GICv2m frame.
    let mem32 = (layout::PCI_MEM32, layout::PCI_MEM32_SIZE);
    #[cfg(hv = "hvf")]
    let lines = match msi {
        Some(boot::Msi::V2m(frame)) => Some(PciLines {
            gic: gic.clone(),
            frame,
        }),
        _ => None,
    };
    #[cfg(not(hv = "hvf"))]
    let lines = msi.map(|_| PciLines { gic: gic.clone() });
    let irqs = lines.as_ref().map(|l| l as &dyn super::PciInterrupts);
    let machine = super::PciMachine {
        vm: &vm,
        memory,
        irqs,
        iommu: iommu.clone(),
    };
    let pci = super::pci_bus(&mut bus, layout::PCI_ECAM, mem32, mem64, pci, config, machine)?.map(|routed| {
        boot::PciBridge {
            ecam: (layout::PCI_ECAM, crate::devices::pci::ECAM_SIZE),
            mem32,
            mem64,
            iommu: behind,
            intx: routed
                .intx
                .into_iter()
                .map(|(device, pin)| boot::Intx {
                    device,
                    pin,
                    spi: pci_intx_spi(device),
                })
                .collect(),
            msi,
        }
    });
    let vmgenid = VmGenId::new(
        memory.clone(),
        layout::VMGENID,
        Arc::new(GicLine {
            gic,
            intid: SPI_INTID_BASE + layout::SPI_VMGENID,
        }),
    );
    Ok(Assembled {
        vm,
        bus,
        serial,
        control,
        vmgenid,
        gic_dist_size: gp.dist_size,
        redist_total,
        virtio,
        mpidrs,
        pmem,
        gpu_window,
        pci,
    })
}

/// A machine that boots `cfg.kernel`.
pub fn build(cfg: &Config) -> Result<Machine, String> {
    let ram = ram_bytes(cfg.memory_mib)?;
    let mut config = super::machine_config(cfg)?;
    let memory = Arc::new(super::guest_memory(&ram_ranges(ram)?, &config, plug_at(ram))?);
    // The image is copied, not mapped: a mapped image stalled boots for up to 1 s
    // (platform-measurements M16). Where it and the initrd go, from its header and the
    // initrd's length: the initrd between the kernel, at the next 2 MiB, and the device
    // tree (boot.rs, place_initrd). Their bytes are copied in as the machine is made.
    let kernel_file = crate::platform::open_input(&cfg.kernel, false)
        .map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let fdt_addr = layout::DRAM_BASE + ram - boot::FDT_MAX;
    let (kernel, span) = boot::plan_kernel(&kernel_file, ram).map_err(|e| e.to_string())?;
    let mut pieces = vec![super::Piece {
        span,
        file: &kernel_file,
        path: &cfg.kernel,
    }];
    let room = fdt_addr.saturating_sub(kernel.end.next_multiple_of(2 << 20));
    let initrd_file = super::initrd(cfg, room)?;
    let initrd = match &initrd_file {
        None => None,
        Some(i) => {
            let start = boot::place_initrd(i.len(), kernel.end, fdt_addr).map_err(|e| e.to_string())?;
            pieces.push(i.place(&memory, start)?);
            Some((start, i.len()))
        }
    };
    let a = super::beside_copy(&memory, &pieces, || {
        let hosts = super::Hosts {
            vsock: cfg.vsock.as_ref(),
            #[cfg(unix)]
            net: cfg.net.as_ref(),
            #[cfg(unix)]
            shares: &cfg.shares,
            join: cfg.join,
            #[cfg(unix)]
            join_share: cfg.join_share,
            io: cfg.io_limit,
        };
        let a = assemble(
            &memory,
            &config,
            cfg.console,
            hosts,
            cfg.gpu.then_some(0),
            super::Pci {
                test: &cfg.pci_test,
                vfio: &cfg.vfio,
            },
        )?;
        a.vmgenid.write_new_id()?;
        Ok(a)
    })?;
    debug!("kernel and initrd copied");
    // Its GPU's window as the guest sees it, which a snapshot keeps for a restore (D150).
    config.gpu = a.gpu_window;
    let mut rng_seed = [0u8; 64];
    platform::fill_random(&mut rng_seed).map_err(|e| format!("host entropy: {e}"))?;
    let fdt = boot::build_fdt(&boot::Machine {
        mpidrs: &a.mpidrs,
        ram_size: ram,
        cmdline: &cfg.cmdline,
        initrd,
        gic_dist: (layout::GIC_DIST, a.gic_dist_size),
        gic_redist: (layout::GIC_REDIST, a.redist_total),
        virtio: &a.virtio,
        rng_seed,
        el2: cfg.nested,
        pci: a.pci,
    })
    .map_err(|e| e.to_string())?;
    memory
        .access()
        .map_err(|e| e.to_string())?
        .write(fdt_addr, &fdt)
        .map_err(|e| e.to_string())?;
    debug!(
        "kernel entry {:#x} end {:#x}; dtb at {fdt_addr:#x} ({} bytes)",
        kernel.entry,
        kernel.end,
        fdt.len()
    );
    Ok(Machine {
        vm: a.vm,
        memory,
        bus: a.bus,
        serial: a.serial,
        control: a.control,
        power: Arc::new(Power::default()),
        finish: a.vmgenid,
        start: Start::Boot(Entry {
            pc: kernel.entry,
            x0: fdt_addr,
        }),
        config,
    })
}

/// Stage-2 pages, as working sets record them.
pub const PAGE: u64 = hv::PAGE;

/// What records a working set.
#[cfg(hv = "hvf")]
pub type Recorder = Arc<hv::Watch>;

/// Nothing records one: a VM on arm64 KVM is not snapshotted yet (vm::SNAPSHOTS).
#[cfg(hv = "kvm")]
#[derive(Debug)]
pub enum Recorder {}

/// Starts recording `vm`'s working set: its stage 2 watches RAM and pmem alike. Every
/// vCPU must be out of the guest.
#[cfg(hv = "hvf")]
pub fn record(vm: &hv::Vm) -> Result<Option<Recorder>, String> {
    let watch = vm.watch();
    watch.start().map_err(|e| e.to_string())?;
    Ok(Some(watch))
}

#[cfg(hv = "kvm")]
pub fn record(_vm: &hv::Vm) -> Result<Option<Recorder>, String> {
    Ok(None)
}

/// Stops recording; the pages touched since [`record`], in order.
#[cfg(hv = "hvf")]
pub fn recorded(recorder: &Recorder) -> Result<Vec<hv::Touch>, String> {
    Ok(recorder.stop())
}

#[cfg(hv = "kvm")]
pub fn recorded(recorder: &Recorder) -> Result<Vec<hv::Touch>, String> {
    match *recorder {}
}

/// None: on HVF, the run that saves a template records its working set ([`record`]).
pub fn recorder(_m: &Machine) -> Option<Recorder> {
    None
}

/// What a machine keeps of its [`Start`] once its vCPUs are set up, for its release and
/// its diagnostics; the rest is dropped (audit D03).
#[derive(Debug)]
pub struct Kept {
    /// A restore's guest counter at its snapshot.
    counter: Option<u64>,
    /// How many pages of its working set a restored machine prefetched.
    prefetched: usize,
}

/// What the start leaves the running machine, once every vCPU is set up.
pub fn keep(rest: Option<&Remainder>, first: &SetUp) -> Kept {
    Kept {
        counter: rest.map(|r| r.counter),
        prefetched: first.prefetched,
    }
}

/// The bytes a restore's start holds, by part: its vCPU states (their inline size),
/// its interrupt controller and device state, and its working set.
pub fn start_bytes(start: &Start) -> Option<[usize; 3]> {
    let Start::Restore(r) = start else {
        return None;
    };
    Some([
        r.vcpus.capacity() * std::mem::size_of::<VcpuState>(),
        r.gic.capacity() + r.devices.capacity(),
        r.working_set.capacity() * std::mem::size_of::<hv::Touch>(),
    ])
}

pub fn prefetched(kept: &Kept) -> usize {
    kept.prefetched
}

/// A machine that resumes `snap`, with guest RAM mapped copy-on-write from `memory_file`,
/// that prefetches `working_set` before the guest runs.
pub fn restore(
    snap: &Snapshot,
    memory_file: &File,
    console: Console,
    hosts: super::Hosts<'_>,
    working_set: Vec<hv::Touch>,
) -> Result<Machine, String> {
    super::check_hosts(snap, hosts)?;
    let state = decode_state(&snap.arch)?;
    if state.vcpus.len() != snap.config.vcpus as usize {
        return Err(format!(
            "snapshot has state for {} vCPUs but configures {}",
            state.vcpus.len(),
            snap.config.vcpus
        ));
    }
    let ram = ram_bytes(snap.config.memory_mib)?;
    let memory = Arc::new(super::restored_memory(
        &ram_ranges(ram)?,
        memory_file,
        &snap.config,
        plug_at(ram),
    )?);
    // A machine with a PCI bus saves no snapshot, so a restored one has none.
    let a = assemble(
        &memory,
        &snap.config,
        console,
        hosts,
        snap.config.gpu,
        super::Pci::default(),
    )?;
    super::replug(&memory, snap, memory_file, &a.vm)?;
    let guest: Vec<(u64, u64)> = memory
        .regions()
        .map(|(gpa, _, len)| (gpa, len as u64))
        .chain(a.pmem.iter().copied())
        .collect();
    let working_set = super::usable_working_set(working_set, PAGE, &guest);
    Ok(Machine {
        vm: a.vm,
        memory,
        bus: a.bus,
        serial: a.serial,
        control: a.control,
        power: Arc::new(Power::default()),
        finish: a.vmgenid,
        start: Start::Restore(Restored {
            vcpus: state.vcpus,
            counter: state.counter,
            cpu_id: state.cpu_id,
            gic: state.gic,
            devices: snap.devices.clone(),
            working_set,
        }),
        config: snap.config.clone(),
    })
}

/// Completes the machine once every vCPU exists and before any runs: KVM initializes its
/// GIC now, whose redistributors are the vCPUs' (arm-vgic-v3.rst). A restore applies
/// the GIC's state only now, as Hypervisor.framework requires (hv_gic.h): HVF routes an
/// SPI to its vCPU, and routing to a CPU that does not exist yet loses the SPI for good.
/// Found by the snapshot E2E test; SPIs were pending but never delivered. The state is
/// HVF's own serialization, not the distributor's registers, which lose an interrupt HVF
/// has passed on toward a vCPU that has yet to take it (found by the storm E2E test: a
/// request completed, the guest never told). Devices go after the GIC, because they
/// re-raise their interrupt lines as they restore. Last, the restored guest gets a new
/// generation ID, so it reseeds its RNG before it runs anything that uses it.
pub fn finish(vm: &hv::Vm, bus: &MmioBus, vmgenid: &VmGenId, rest: Option<&Remainder>) -> Result<(), String> {
    vm.vcpus_created().map_err(|e| e.to_string())?;
    let Some(r) = rest else {
        return Ok(());
    };
    vm.restore_gic(&r.gic).map_err(|e| e.to_string())?;
    let mut devices = Reader::new(&r.devices);
    bus.restore(&mut devices)
        .and_then(|()| devices.finish())
        .map_err(|e| e.to_string())?;
    vmgenid.new_generation()
}

/// Creates vCPU `index` and puts it where `start` says: the boot entry, parked for
/// PSCI CPU_ON, or its restored state.
pub fn setup_vcpu(vm: &hv::Vm, index: usize, start: VcpuStart) -> Result<(hv::Vcpu, SetUp), String> {
    let e = |e: hv::Error| e.to_string();
    let mut vcpu = vm.create_vcpu(index).map_err(e)?;
    let mut set_up = SetUp::default();
    match start {
        VcpuStart::Boot(entry) => {
            if let Some(entry) = entry {
                vcpu.boot(entry).map_err(e)?;
            }
        }
        VcpuStart::Restore { state, first } => {
            if let Some(First { cpu_id, working_set }) = first {
                let here = vcpu.cpu_id().map_err(e)?;
                if here != cpu_id {
                    return Err(format!(
                        "this CPU is not the one the snapshot was taken on: {here:x?} vs {cpu_id:x?}"
                    ));
                }
                // Before the state: the prefetch runs on this vCPU. A failure is the VM's,
                // since its translations may outlive a loop that stopped short.
                if !working_set.is_empty() {
                    set_up.prefetched = vcpu
                        .prefetch(vm, &working_set)
                        .map_err(|e| format!("prefetching the working set: {e}"))?;
                }
            }
            vcpu.restore_state(&state).map_err(e)?;
        }
    }
    Ok((vcpu, set_up))
}

/// For a restore, the counter offset every vCPU applies at release, taken now so the
/// guest counter continues from the snapshot.
pub fn release_offset(kept: &Kept) -> Result<Option<u64>, String> {
    Ok(kept
        .counter
        .map(|counter| hv::host_counter().wrapping_sub(counter)))
}

pub fn set_counter_offset(vcpu: &mut hv::Vcpu, offset: u64) -> Result<(), String> {
    vcpu.set_counter_offset(offset).map_err(|e| e.to_string())
}

/// One vCPU's contribution to a snapshot, captured on its own thread.
#[derive(Debug)]
pub struct Captured {
    state: VcpuState,
    /// Its guest counter when it stopped.
    counter: u64,
    /// The CPU identity, from vCPU 0.
    cpu_id: Option<Vec<(u16, u64)>>,
}

pub fn capture(vcpu: &hv::Vcpu, index: usize) -> Result<Captured, String> {
    let e = |e: hv::Error| e.to_string();
    Ok(Captured {
        state: vcpu.save_state().map_err(e)?,
        counter: vcpu.guest_counter().map_err(e)?,
        cpu_id: if index == 0 {
            Some(vcpu.cpu_id().map_err(e)?)
        } else {
            None
        },
    })
}

/// The architecture state a snapshot records: every vCPU's, in index order, and the GIC
/// device's. The VM's counter is the latest any vCPU saw.
pub fn encode_state(vm: &hv::Vm, captured: Vec<Captured>) -> Result<Vec<u8>, String> {
    let counter = captured.iter().map(|c| c.counter).max().unwrap_or(0);
    let cpu_id = captured
        .iter()
        .find_map(|c| c.cpu_id.clone())
        .ok_or("vCPU 0 captured no CPU identity")?;
    let state = MachineState {
        counter,
        cpu_id,
        gic: vm.save_gic().map_err(|e| e.to_string())?,
        vcpus: captured.into_iter().map(|c| c.state).collect(),
    };
    let mut w = Writer::default();
    state.encode(&mut w);
    Ok(w.into_bytes())
}

fn decode_state(bytes: &[u8]) -> Result<MachineState, String> {
    let mut r = Reader::new(bytes);
    let state = MachineState::decode(&mut r).map_err(|e| e.to_string())?;
    r.finish().map_err(|e| e.to_string())?;
    Ok(state)
}
