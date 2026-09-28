//! The x86_64 machine on any backend: RAM around the 32-bit MMIO gap, the in-kernel
//! IOAPIC, ACPI tables, COM1, and the i8042 and ACPI sleep ports the guest resets and
//! powers off through (docs/research/kvm-x86_64-ground-truth.md §3-§6).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Config, Console};
use crate::arch::x86_64::{self, acpi, boot, layout};
use crate::devices::acpi_sleep::AcpiSleep;
use crate::devices::control::Control;
use crate::devices::i8042::I8042;
use crate::devices::power::Power;
use crate::devices::serial::Serial;
use crate::devices::virtio::{VirtioDevice, block::Block, mmio as virtio_mmio, pmem, vsock};
use crate::devices::{Interrupt, MmioBus};
use crate::hv::{self, Io};
use crate::memory::GuestMemory;
use crate::snapshot::codec::Writer;
use crate::snapshot::{MachineConfig, Snapshot};
use crate::{debug, initramfs, platform, warn};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Smallest guest that holds the kernel (loaded at 16 MiB), its bss and early allocations.
const MIN_MEMORY_MIB: u64 = 128;
/// Kernel parameters the machine depends on: restart through the i8042 (the device the
/// VMM emulates), and no PCI bus (there is none yet).
const MACHINE_CMDLINE: &str = "reboot=k pci=off";

/// How the vCPUs start. Snapshots are not supported on x86_64 yet.
#[derive(Debug)]
pub enum Start {
    /// The boot vCPU's registers; application processors wait for INIT/SIPI in the kernel.
    Boot(x86_64::Boot),
}

/// MMIO devices and port I/O devices.
#[derive(Debug, Default)]
pub struct Bus {
    pub mmio: MmioBus,
    pub pio: MmioBus,
}

impl Io for Bus {
    fn mmio_read(&self, addr: u64, data: &mut [u8]) {
        self.mmio.mmio_read(addr, data);
    }

    fn mmio_write(&self, addr: u64, data: &[u8]) {
        self.mmio.mmio_write(addr, data);
    }

    fn pio_read(&self, port: u16, data: &mut [u8]) {
        if !self.pio.read(u64::from(port), data) {
            debug!("unclaimed port read {port:#x} ({} bytes)", data.len());
        }
    }

    fn pio_write(&self, port: u16, data: &[u8]) {
        if !self.pio.write(u64::from(port), data) {
            debug!("unclaimed port write {port:#x} ({} bytes)", data.len());
        }
    }
}

impl Bus {
    pub fn pause(&self) {
        self.mmio.pause();
        self.pio.pause();
    }

    pub fn resume(&self) -> Result<(), String> {
        self.mmio.resume()?;
        self.pio.resume()
    }

    pub fn save(&self, w: &mut Writer) {
        self.mmio.save(w);
        self.pio.save(w);
    }
}

/// Nothing to complete after vCPU creation on x86.
pub type Finish = ();

/// A VM ready for its vCPUs. Field order is drop order: the VM goes before the memory
/// it maps.
#[derive(Debug)]
pub struct Machine {
    pub vm: hv::Vm,
    pub memory: Arc<GuestMemory>,
    pub bus: Bus,
    pub serial: Arc<Serial>,
    pub control: Arc<Control>,
    /// Power-off (ACPI sleep control) and reset (i8042).
    pub power: Arc<Power>,
    pub finish: Finish,
    pub start: Start,
    pub config: MachineConfig,
}

/// A level-sensitive IOAPIC pin, driven as the device's line is.
#[derive(Debug)]
struct LevelLine {
    irqs: hv::Irqs,
    gsi: u32,
}

impl Interrupt for LevelLine {
    fn set_level(&self, level: bool) {
        if let Err(e) = self.irqs.set(self.gsi, level) {
            warn!("GSI {}: {e}", self.gsi);
        }
    }
}

/// An edge-triggered IOAPIC pin: each raise is one pulse.
#[derive(Debug)]
struct EdgeLine {
    irqs: hv::Irqs,
    gsi: u32,
}

impl Interrupt for EdgeLine {
    fn set_level(&self, level: bool) {
        if level && let Err(e) = self.irqs.pulse(self.gsi) {
            warn!("GSI {}: {e}", self.gsi);
        }
    }
}

pub fn build(cfg: &Config) -> Result<Machine, String> {
    if cfg.memory_mib < MIN_MEMORY_MIB || !cfg.memory_mib.is_multiple_of(2) {
        return Err(format!(
            "guest memory must be an even number of MiB, at least {MIN_MEMORY_MIB}"
        ));
    }
    let ram = cfg
        .memory_mib
        .checked_mul(MIB)
        .ok_or_else(|| format!("{} MiB of guest memory", cfg.memory_mib))?;
    let ranges = boot::ram_ranges(ram)
        .into_iter()
        .map(|(gpa, len)| usize::try_from(len).map(|l| (gpa, l)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("{ram} bytes of guest memory"))?;
    let memory = Arc::new(GuestMemory::anonymous(&ranges).map_err(|e| format!("guest RAM: {e}"))?);
    let low_ram_end = ram.min(layout::MMIO_GAP);

    let kernel_file = File::open(&cfg.kernel).map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, low_ram_end).map_err(|e| e.to_string())?;
    debug!("kernel loaded");

    let vm = hv::Vm::new(hv::VmConfig { vcpus: cfg.vcpus }).map_err(|e| e.to_string())?;
    for (gpa, host, len) in memory.regions() {
        // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
        unsafe { vm.map_ram(host, gpa, len) }.map_err(|e| e.to_string())?;
    }
    // Device memory (pmem regions) goes above 4 GiB, after any high RAM.
    let mut regions = Vec::with_capacity(cfg.pmem.len());
    let mut next = (layout::MMIO_GAP_END + ram.saturating_sub(layout::MMIO_GAP)).next_multiple_of(GIB);
    for path in &cfg.pmem {
        let region = Arc::new(pmem::Region::open(path)?);
        let gpa = next;
        next = gpa
            .checked_add(region.len() as u64)
            .ok_or("pmem regions overflow the address space")?;
        // SAFETY: the region outlives the VM: its device sits on the bus, which
        // `Machine` and `Running` drop after the VM.
        unsafe { vm.map_device_memory(region.host(), gpa, region.len(), false) }
            .map_err(|e| e.to_string())?;
        regions.push((region, gpa));
    }
    let irqs = vm.irqs();
    debug!("VM created and RAM mapped");

    let slots = cfg.disks.len() + regions.len() + usize::from(cfg.vsock.is_some());
    if slots as u64 > layout::VIRTIO_MMIO_MAX {
        return Err(format!(
            "at most {} virtio devices are supported",
            layout::VIRTIO_MMIO_MAX
        ));
    }
    let mut bus = Bus::default();
    let mut virtio = Vec::with_capacity(slots);
    // Each virtio device takes the next MMIO window and GSI, in the guest's probe order.
    let mut add_virtio = |bus: &mut Bus, device: Box<dyn VirtioDevice>| -> Result<(), String> {
        let i = virtio.len();
        let gsi = layout::GSI_VIRTIO + i as u32;
        let base = layout::VIRTIO_MMIO + i as u64 * layout::VIRTIO_MMIO_STRIDE;
        let line = Arc::new(EdgeLine {
            irqs: irqs.clone(),
            gsi,
        });
        let transport = virtio_mmio::MmioTransport::new(device, memory.clone(), line);
        bus.mmio.insert(base, virtio_mmio::WINDOW, Arc::new(transport))?;
        virtio.push(acpi::MmioDevice {
            base,
            size: virtio_mmio::WINDOW,
            gsi,
        });
        Ok(())
    };
    for (i, disk) in cfg.disks.iter().enumerate() {
        let block = Block::open(&disk.path, disk.read_only, &format!("shards-disk{i}"))?;
        add_virtio(&mut bus, Box::new(block))?;
    }
    for (region, gpa) in regions {
        add_virtio(&mut bus, Box::new(pmem::Pmem::new(region, gpa)))?;
    }
    if let Some(path) = &cfg.vsock {
        add_virtio(&mut bus, Box::new(vsock::Vsock::new(path, vsock::GUEST_CID)?))?;
    }
    let control = Arc::new(Control::default());
    bus.mmio.insert(layout::CONTROL, 0x1000, control.clone())?;

    let out: Box<dyn Write + Send> = match cfg.console {
        Console::Stdout => Box::new(platform::stdout_file().map_err(|e| format!("console: {e}"))?),
        Console::Discard => Box::new(std::io::sink()),
    };
    let serial = Arc::new(Serial::new(
        out,
        Arc::new(LevelLine {
            irqs: irqs.clone(),
            gsi: layout::GSI_COM1,
        }),
    ));
    let power = Arc::new(Power::default());
    bus.pio.insert(u64::from(layout::COM1), 8, serial.clone())?;
    bus.pio
        .insert(u64::from(layout::I8042), 5, Arc::new(I8042::new(power.clone())))?;
    bus.pio.insert(
        u64::from(layout::ACPI_SLEEP),
        2,
        Arc::new(AcpiSleep::new(power.clone())),
    )?;

    for (addr, bytes) in acpi::build(cfg.vcpus, &virtio)?.blobs {
        memory.write(addr, &bytes).map_err(|e| e.to_string())?;
    }

    let read = |p: &PathBuf| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let initrd_bytes = match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => return Err("--initrd and --init are mutually exclusive".into()),
        (Some(p), None) => Some(read(p)?),
        (None, Some(p)) => Some(initramfs::with_init(&read(p)?)),
        (None, None) => None,
    };
    let initrd = match initrd_bytes {
        None => None,
        Some(bytes) => Some(
            boot::load_initrd(&memory, &bytes, kernel.end, low_ram_end.min(layout::MMIO_GAP))
                .map_err(|e| e.to_string())?,
        ),
    };
    let cmdline = format!("{} {MACHINE_CMDLINE}", cfg.cmdline);
    let boot_state = boot::write_boot_state(
        &memory,
        kernel.entry,
        &cmdline,
        &boot::ZeroPage {
            ram,
            initrd,
            rsdp: layout::RSDP,
        },
    )
    .map_err(|e| e.to_string())?;
    debug!("kernel entry {:#x} end {:#x}", kernel.entry, kernel.end);

    Ok(Machine {
        vm,
        memory,
        bus,
        serial,
        control,
        power,
        finish: (),
        start: Start::Boot(boot_state),
        config: MachineConfig {
            vcpus: cfg.vcpus,
            memory_mib: cfg.memory_mib,
            disks: cfg.disks.iter().map(|d| (d.path.clone(), d.read_only)).collect(),
            pmem: cfg.pmem.clone(),
            vsock: cfg.vsock.is_some(),
        },
    })
}

const NO_SNAPSHOTS: &str = "snapshots are not supported on x86_64 yet";

pub fn restore(
    snap: &Snapshot,
    _memory_file: &File,
    _console: Console,
    vsock: Option<&Path>,
) -> Result<Machine, String> {
    super::check_vsock(snap, vsock)?;
    Err(NO_SNAPSHOTS.into())
}

/// Creates vCPU `index`; the boot vCPU gets the boot protocol's registers.
pub fn setup_vcpu(vm: &hv::Vm, index: usize, start: &Start) -> Result<hv::Vcpu, String> {
    let mut vcpu = vm.create_vcpu(index).map_err(|e| e.to_string())?;
    let Start::Boot(boot) = start;
    if index == 0 {
        vcpu.boot(boot).map_err(|e| e.to_string())?;
    }
    Ok(vcpu)
}

pub fn finish(_vm: &hv::Vm, _bus: &Bus, _finish: &Finish, _start: &Start) -> Result<(), String> {
    Ok(())
}

pub fn release_offset(_start: &Start) -> Option<u64> {
    None
}

pub fn set_counter_offset(_vcpu: &mut hv::Vcpu, _offset: u64) -> Result<(), String> {
    Err(NO_SNAPSHOTS.into())
}

/// No x86 vCPU state can be captured yet, so there is no value of this type.
#[derive(Debug)]
pub enum Captured {}

pub fn capture(_vcpu: &hv::Vcpu, _index: usize) -> Result<Captured, String> {
    Err(NO_SNAPSHOTS.into())
}

pub fn encode_state(_vm: &hv::Vm, captured: Vec<Captured>) -> Result<Vec<u8>, String> {
    match captured.into_iter().next() {
        Some(c) => match c {},
        None => Err(NO_SNAPSHOTS.into()),
    }
}
