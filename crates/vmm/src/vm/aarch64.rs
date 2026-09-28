//! The arm64 machine on any backend: memory map, in-kernel GICv3, devicetree, devices.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use super::{Config, Console};
use crate::arch::aarch64::{self, Entry, boot, layout};
use crate::devices::control::Control;
use crate::devices::rtc::Pl031;
use crate::devices::serial::Serial;
use crate::devices::virtio::{block::Block, mmio as virtio_mmio};
use crate::devices::{Interrupt, MmioBus};
use crate::hv::{self, Gic, GicLayout};
use crate::memory::GuestMemory;
use crate::{debug, initramfs, platform, warn};

const MIB: u64 = 1 << 20;
/// Smallest guest that can hold a kernel, its early allocations and the DTB window.
const MIN_MEMORY_MIB: u64 = 64;
const SPI_INTID_BASE: u32 = 32;

/// How the boot vCPU enters the guest.
pub type Boot = Entry;

/// A VM ready for its vCPUs. Field order is drop order: the VM goes before the memory
/// it maps.
#[derive(Debug)]
pub struct Machine {
    pub vm: hv::Vm,
    pub memory: Arc<GuestMemory>,
    pub bus: MmioBus,
    pub serial: Arc<Serial>,
    pub control: Arc<Control>,
    pub boot: Boot,
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

/// The smallest architected IPA width that covers guest RAM, within the host's limit.
fn ipa_bits(ram_end: u64) -> Result<u32, String> {
    let needed = 64 - ram_end.saturating_sub(1).leading_zeros();
    let max = hv::max_ipa_bits().map_err(|e| e.to_string())?;
    [36, 40, 42, 44, 48]
        .into_iter()
        .find(|&b| b >= needed && b <= max)
        .ok_or_else(|| {
            format!("guest RAM ending at {ram_end:#x} needs {needed} address bits; host allows {max}")
        })
}

pub fn build(cfg: &Config) -> Result<Machine, String> {
    if cfg.memory_mib < MIN_MEMORY_MIB || !cfg.memory_mib.is_multiple_of(2) {
        return Err(format!(
            "guest memory must be an even number of MiB, at least {MIN_MEMORY_MIB}"
        ));
    }
    let ram = cfg.memory_mib * MIB;
    let ipa_bits = ipa_bits(layout::DRAM_BASE + ram)?;
    let mpidrs: Vec<u64> = (0..cfg.vcpus).map(aarch64::mpidr).collect();

    let memory = Arc::new(
        GuestMemory::anonymous(&[(layout::DRAM_BASE, ram as usize)])
            .map_err(|e| format!("guest RAM: {e}"))?,
    );
    let vm = hv::Vm::new(hv::VmConfig {
        ipa_bits,
        mpidrs: mpidrs.clone(),
    })
    .map_err(|e| e.to_string())?;
    for (gpa, host, len) in memory.regions() {
        // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
        unsafe { vm.map_ram(host, gpa, len) }.map_err(|e| e.to_string())?;
    }

    let gp = hv::gic_params().map_err(|e| e.to_string())?;
    let redist_total = gp.redist_size * u64::from(cfg.vcpus);
    if !layout::GIC_DIST.is_multiple_of(gp.dist_align)
        || !layout::GIC_REDIST.is_multiple_of(gp.redist_align)
        || layout::GIC_DIST + gp.dist_size > layout::GIC_MSI
        || layout::GIC_REDIST + redist_total > layout::GIC_REDIST_MAX_END
    {
        return Err(format!(
            "host GIC geometry {gp:?} does not fit the guest memory map"
        ));
    }
    let gic = vm
        .create_gic(&GicLayout {
            dist_base: layout::GIC_DIST,
            redist_base: layout::GIC_REDIST,
            msi: None,
        })
        .map_err(|e| e.to_string())?;

    let kernel_file = File::open(&cfg.kernel).map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, ram).map_err(|e| e.to_string())?;
    let fdt_addr = layout::DRAM_BASE + ram - boot::FDT_MAX;
    let read = |p: &PathBuf| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let initrd_bytes = match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => return Err("--initrd and --init are mutually exclusive".into()),
        (Some(p), None) => Some(read(p)?),
        (None, Some(p)) => Some(initramfs::with_init(&read(p)?)),
        (None, None) => None,
    };
    let initrd = match initrd_bytes {
        None => None,
        Some(bytes) => {
            Some(boot::load_initrd(&memory, &bytes, kernel.end, fdt_addr).map_err(|e| e.to_string())?)
        }
    };
    let mut rng_seed = [0u8; 64];
    platform::fill_random(&mut rng_seed).map_err(|e| format!("host entropy: {e}"))?;

    if cfg.disks.len() as u64 > layout::VIRTIO_MMIO_MAX {
        return Err(format!(
            "at most {} virtio devices are supported",
            layout::VIRTIO_MMIO_MAX
        ));
    }
    let mut bus = MmioBus::default();
    let mut virtio_nodes = Vec::with_capacity(cfg.disks.len());
    for (i, disk) in cfg.disks.iter().enumerate() {
        let block = Block::open(&disk.path, disk.read_only, &format!("shards-disk{i}"))?;
        let spi = layout::SPI_VIRTIO_MMIO + i as u32;
        let base = layout::VIRTIO_MMIO + i as u64 * layout::VIRTIO_MMIO_STRIDE;
        let line = Arc::new(GicLine {
            gic,
            intid: SPI_INTID_BASE + spi,
        });
        let transport = virtio_mmio::MmioTransport::new(Box::new(block), memory.clone(), line);
        bus.insert(base, virtio_mmio::WINDOW, Arc::new(transport))?;
        virtio_nodes.push(boot::MmioDevice {
            base,
            size: virtio_mmio::WINDOW,
            spi,
        });
    }
    let fdt = boot::build_fdt(&boot::Machine {
        mpidrs: &mpidrs,
        ram_size: ram,
        cmdline: &cfg.cmdline,
        initrd,
        gic_dist: (layout::GIC_DIST, gp.dist_size),
        gic_redist: (layout::GIC_REDIST, redist_total),
        virtio: &virtio_nodes,
        rng_seed,
    })
    .map_err(|e| e.to_string())?;
    memory.write(fdt_addr, &fdt).map_err(|e| e.to_string())?;
    debug!(
        "kernel entry {:#x} end {:#x}; dtb at {fdt_addr:#x} ({} bytes)",
        kernel.entry,
        kernel.end,
        fdt.len()
    );

    let out: Box<dyn Write + Send> = match cfg.console {
        Console::Stdout => Box::new(platform::stdout_file().map_err(|e| format!("console: {e}"))?),
        Console::Discard => Box::new(std::io::sink()),
    };
    let serial = Arc::new(Serial::new(
        out,
        Arc::new(GicLine {
            gic,
            intid: SPI_INTID_BASE + layout::SPI_UART,
        }),
    ));
    bus.insert(layout::UART, 0x1000, serial.clone())?;
    bus.insert(layout::RTC, 0x1000, Arc::new(Pl031::default()))?;
    let control = Arc::new(Control::default());
    bus.insert(layout::CONTROL, 0x1000, control.clone())?;

    Ok(Machine {
        vm,
        memory,
        bus,
        serial,
        control,
        boot: Entry {
            pc: kernel.entry,
            x0: fdt_addr,
        },
    })
}
