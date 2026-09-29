//! The x86_64 machine on any backend: RAM around the 32-bit MMIO gap, the in-kernel
//! IOAPIC, ACPI tables, COM1, the i8042 and ACPI sleep ports the guest resets and powers
//! off through (docs/research/kvm-x86_64-ground-truth.md §3-§6), and the VM generation ID
//! that makes each restored copy reseed its RNG.

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
use crate::devices::vmgenid::VmGenId;
use crate::devices::{Interrupt, MmioBus};
use crate::hv::{self, Io};
use crate::memory::GuestMemory;
use crate::snapshot::codec::{Reader, Writer};
use crate::snapshot::{MachineConfig, Snapshot};
use crate::{debug, initramfs, platform, warn};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Smallest guest that holds the kernel (loaded at 16 MiB), its bss and early allocations.
const MIN_MEMORY_MIB: u64 = 128;
/// Kernel parameters the machine depends on: restart through the i8042 (the device the
/// VMM emulates), and no PCI bus (there is none yet).
const MACHINE_CMDLINE: &str = "reboot=k pci=off";

/// How the vCPUs start.
#[derive(Debug)]
pub enum Start {
    /// The boot vCPU's registers; application processors wait for INIT/SIPI in the kernel.
    Boot(x86_64::Boot),
    Restore(Restored),
}

#[derive(Debug)]
pub struct Restored {
    pub vcpus: Vec<hv::VcpuState>,
    /// The interrupt controllers and kvmclock, applied once every vCPU exists ([`finish`]).
    pub vm: hv::VmState,
    /// The device buses' saved state, applied by [`finish`].
    pub devices: Vec<u8>,
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

    fn restore(&self, r: &mut Reader<'_>) -> Result<(), String> {
        self.mmio.restore(r).map_err(|e| e.to_string())?;
        self.pio.restore(r).map_err(|e| e.to_string())
    }
}

/// What [`finish`] needs besides the VM and bus: the VM generation ID, new for each
/// restore.
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

/// What boot and restore share: the VM with its RAM, and the devices, laid out alike so a
/// restore's device state lands where it was saved.
struct Assembled {
    vm: hv::Vm,
    bus: Bus,
    serial: Arc<Serial>,
    control: Arc<Control>,
    power: Arc<Power>,
    vmgenid: VmGenId,
    virtio: Vec<acpi::MmioDevice>,
}

/// Guest RAM of `memory_mib`, around the MMIO gap.
fn ram_ranges(memory_mib: u64) -> Result<(u64, Vec<(u64, usize)>), String> {
    if memory_mib < MIN_MEMORY_MIB || !memory_mib.is_multiple_of(2) {
        return Err(format!(
            "guest memory must be an even number of MiB, at least {MIN_MEMORY_MIB}"
        ));
    }
    let ram = memory_mib
        .checked_mul(MIB)
        .ok_or_else(|| format!("{memory_mib} MiB of guest memory"))?;
    let ranges = boot::ram_ranges(ram)
        .into_iter()
        .map(|(gpa, len)| usize::try_from(len).map(|l| (gpa, l)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("{ram} bytes of guest memory"))?;
    Ok((ram, ranges))
}

fn assemble(
    memory: &Arc<GuestMemory>,
    config: &MachineConfig,
    console: Console,
    vsock: Option<&Path>,
) -> Result<Assembled, String> {
    let ram = config
        .memory_mib
        .checked_mul(MIB)
        .ok_or_else(|| format!("{} MiB of guest memory", config.memory_mib))?;
    let vm = hv::Vm::new(hv::VmConfig { vcpus: config.vcpus }).map_err(|e| e.to_string())?;
    for (gpa, host, len) in memory.regions() {
        // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
        unsafe { vm.map_ram(host, gpa, len) }.map_err(|e| e.to_string())?;
    }
    // Device memory (pmem regions) goes above 4 GiB, after any high RAM.
    let mut regions = Vec::with_capacity(config.pmem.len());
    let mut next = (layout::MMIO_GAP_END + ram.saturating_sub(layout::MMIO_GAP)).next_multiple_of(GIB);
    for path in &config.pmem {
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

    let slots = config.disks.len() + regions.len() + usize::from(config.vsock);
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
    for (i, (path, read_only)) in config.disks.iter().enumerate() {
        let block = Block::open(path, *read_only, &format!("shards-disk{i}"))?;
        add_virtio(&mut bus, Box::new(block))?;
    }
    for (region, gpa) in regions {
        add_virtio(&mut bus, Box::new(pmem::Pmem::new(region, gpa)))?;
    }
    if config.vsock {
        let path = vsock.ok_or("the machine has a vsock device, but no socket path was given")?;
        add_virtio(&mut bus, Box::new(vsock::Vsock::new(path, vsock::GUEST_CID)?))?;
    }
    let control = Arc::new(Control::default());
    bus.mmio.insert(layout::CONTROL, 0x1000, control.clone())?;

    let out: Box<dyn Write + Send> = match console {
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
    let vmgenid = VmGenId::new(
        memory.clone(),
        layout::VMGENID,
        Arc::new(EdgeLine {
            irqs,
            gsi: layout::GSI_GED,
        }),
    );
    Ok(Assembled {
        vm,
        bus,
        serial,
        control,
        power,
        vmgenid,
        virtio,
    })
}

pub fn build(cfg: &Config) -> Result<Machine, String> {
    let (ram, ranges) = ram_ranges(cfg.memory_mib)?;
    let memory = Arc::new(GuestMemory::anonymous(&ranges).map_err(|e| format!("guest RAM: {e}"))?);
    let low_ram_end = ram.min(layout::MMIO_GAP);

    let kernel_file = File::open(&cfg.kernel).map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, low_ram_end).map_err(|e| e.to_string())?;
    debug!("kernel loaded");

    let config = MachineConfig {
        vcpus: cfg.vcpus,
        memory_mib: cfg.memory_mib,
        disks: cfg.disks.iter().map(|d| (d.path.clone(), d.read_only)).collect(),
        pmem: cfg.pmem.clone(),
        vsock: cfg.vsock.is_some(),
    };
    let a = assemble(&memory, &config, cfg.console, cfg.vsock.as_deref())?;
    for (addr, bytes) in acpi::build(cfg.vcpus, &a.virtio)?.blobs {
        memory.write(addr, &bytes).map_err(|e| e.to_string())?;
    }
    a.vmgenid.write_new_id()?;

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
        vm: a.vm,
        memory,
        bus: a.bus,
        serial: a.serial,
        control: a.control,
        power: a.power,
        finish: a.vmgenid,
        start: Start::Boot(boot_state),
        config,
    })
}

/// A machine that resumes `snap`, with guest RAM mapped copy-on-write from `memory_file`.
/// It prefetches no working set: recording one needs HVF's stage-2 protection today.
pub fn restore(
    snap: &Snapshot,
    memory_file: &File,
    console: Console,
    vsock: Option<&Path>,
    _working_set: Vec<hv::Touch>,
) -> Result<Machine, String> {
    super::check_vsock(snap, vsock)?;
    let (vm_state, vcpus) = decode_state(&snap.arch)?;
    if vcpus.len() != snap.config.vcpus as usize {
        return Err(format!(
            "snapshot has state for {} vCPUs but configures {}",
            vcpus.len(),
            snap.config.vcpus
        ));
    }
    let (_, ranges) = ram_ranges(snap.config.memory_mib)?;
    let memory =
        Arc::new(GuestMemory::from_file(&ranges, memory_file).map_err(|e| format!("snapshot memory: {e}"))?);
    let a = assemble(&memory, &snap.config, console, vsock)?;
    Ok(Machine {
        vm: a.vm,
        memory,
        bus: a.bus,
        serial: a.serial,
        control: a.control,
        power: a.power,
        finish: a.vmgenid,
        start: Start::Restore(Restored {
            vcpus,
            vm: vm_state,
            devices: snap.devices.clone(),
        }),
        config: snap.config.clone(),
    })
}

/// Stage-2 pages, as working sets would record them.
pub const PAGE: u64 = 4 << 10;

/// Nothing records a working set on KVM yet.
#[derive(Debug)]
pub enum Recorder {}

/// None: recording a working set needs HVF's stage-2 protection, which KVM has no
/// counterpart of here yet.
pub fn record(_vm: &hv::Vm) -> Result<Option<Recorder>, String> {
    Ok(None)
}

pub fn recorded(recorder: &Recorder) -> Vec<hv::Touch> {
    match *recorder {}
}

pub fn prefetched(_start: &Start) -> usize {
    0
}

/// Creates vCPU `index` and puts it where `start` says: the boot protocol's registers for
/// the boot vCPU, or its restored state.
pub fn setup_vcpu(vm: &hv::Vm, index: usize, start: &Start) -> Result<hv::Vcpu, String> {
    let mut vcpu = vm.create_vcpu(index).map_err(|e| e.to_string())?;
    match start {
        Start::Boot(boot) => {
            if index == 0 {
                vcpu.boot(boot).map_err(|e| e.to_string())?;
            }
        }
        Start::Restore(r) => {
            let state = r
                .vcpus
                .get(index)
                .ok_or_else(|| format!("the snapshot has no vCPU {index}"))?;
            vcpu.restore_state(state).map_err(|e| e.to_string())?;
        }
    }
    Ok(vcpu)
}

/// Completes a restore once every vCPU exists and before any runs: the interrupt
/// controllers and kvmclock, then the devices, which re-raise their lines as they
/// restore, then a new generation ID, so the guest reseeds its RNG before it runs
/// anything that uses it (as the arm64 machine orders them).
pub fn finish(vm: &hv::Vm, bus: &Bus, vmgenid: &Finish, start: &Start) -> Result<(), String> {
    let Start::Restore(r) = start else {
        return Ok(());
    };
    vm.restore_state(&r.vm).map_err(|e| e.to_string())?;
    let mut devices = Reader::new(&r.devices);
    bus.restore(&mut devices)?;
    devices.finish().map_err(|e| e.to_string())?;
    vmgenid.new_generation()
}

/// The TSC and kvmclock come back with the state; nothing waits for the release.
pub fn release_offset(_start: &Start) -> Option<u64> {
    None
}

pub fn set_counter_offset(_vcpu: &mut hv::Vcpu, _offset: u64) -> Result<(), String> {
    Ok(())
}

/// One vCPU's contribution to a snapshot, captured on its own thread.
#[derive(Debug)]
pub struct Captured(hv::VcpuState);

pub fn capture(vcpu: &hv::Vcpu, _index: usize) -> Result<Captured, String> {
    vcpu.save_state().map(Captured).map_err(|e| e.to_string())
}

/// The VM's state, then each vCPU's, in index order.
pub fn encode_state(vm: &hv::Vm, captured: Vec<Captured>) -> Result<Vec<u8>, String> {
    let mut w = Writer::default();
    vm.save_state().map_err(|e| e.to_string())?.encode(&mut w);
    let states: Vec<hv::VcpuState> = captured.into_iter().map(|c| c.0).collect();
    w.seq(&states, |w, st| st.encode(w));
    Ok(w.into_bytes())
}

fn decode_state(bytes: &[u8]) -> Result<(hv::VmState, Vec<hv::VcpuState>), String> {
    let mut r = Reader::new(bytes);
    let vm = hv::VmState::decode(&mut r).map_err(|e| format!("snapshot VM state: {e}"))?;
    let vcpus = r
        .seq(254, hv::VcpuState::decode)
        .map_err(|e| format!("snapshot vCPU state: {e}"))?;
    r.finish().map_err(|e| format!("snapshot state: {e}"))?;
    Ok((vm, vcpus))
}
