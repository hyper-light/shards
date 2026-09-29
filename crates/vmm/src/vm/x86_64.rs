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
    /// The snapshot's working set, to map before the guest runs; empty for none.
    pub working_set: Vec<hv::Touch>,
    /// How many of its pages were mapped ahead.
    pub prefetched: std::sync::OnceLock<usize>,
}

/// MMIO devices and port I/O devices.
#[derive(Debug, Default)]
pub struct Bus {
    pub mmio: MmioBus,
    pub pio: MmioBus,
    /// Each pmem device's region and guest address, which working sets record.
    pub pmem: Vec<(u64, Arc<pmem::Region>)>,
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
    let mut bus = Bus {
        pmem: regions
            .iter()
            .map(|(region, gpa)| (*gpa, region.clone()))
            .collect(),
        ..Bus::default()
    };
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

/// A machine that resumes `snap`, with guest RAM mapped copy-on-write from `memory_file`,
/// that maps `working_set` before the guest runs: the pages the guest wrote are copied
/// here, and vCPU 0 maps them all into the stage-2 tables ([`setup_vcpu`]).
pub fn restore(
    snap: &Snapshot,
    memory_file: &File,
    console: Console,
    vsock: Option<&Path>,
    working_set: Vec<hv::Touch>,
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
    debug!("diag: memory mapped");
    let a = assemble(&memory, &snap.config, console, vsock)?;
    debug!("diag: assembled");
    if let Err(e) = copy_written(&memory, &working_set) {
        warn!("{e}; the guest copies the pages it writes as it writes them");
    }
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
            working_set,
            prefetched: std::sync::OnceLock::new(),
        }),
        config: snap.config.clone(),
    })
}

/// Makes the private copies of RAM that the guest wrote while its working set was
/// recorded, as its writes would make them: KVM then maps these pages writable ahead,
/// where a write to a page mapped read-only would fault, copy, and fault again.
fn copy_written(memory: &GuestMemory, working_set: &[hv::Touch]) -> Result<(), String> {
    let written = working_set.iter().filter(|t| t.written).map(|t| t.gpa);
    for (gpa, len) in runs(written) {
        let at = |e: &dyn std::fmt::Display| format!("copying written pages at {gpa:#x}+{len:#x}: {e}");
        let len = usize::try_from(len).map_err(|e| at(&e))?;
        let host = memory.host_ptr(gpa, len).map_err(|e| at(&e))?;
        platform::populate_writable(host, len).map_err(|e| at(&e))?;
    }
    Ok(())
}

/// The runs of consecutive pages among `pages`, as `(gpa, len)`, in order.
fn runs(pages: impl Iterator<Item = u64>) -> Vec<(u64, u64)> {
    let mut pages: Vec<u64> = pages.map(|gpa| gpa & !(PAGE - 1)).collect();
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for gpa in pages {
        match runs.last_mut() {
            Some((start, len)) if start.checked_add(*len) == Some(gpa) => *len += PAGE,
            _ => runs.push((gpa, PAGE)),
        }
    }
    runs
}

/// Maps every page of `working_set` into the stage-2 tables; how many it mapped. A page
/// that cannot be mapped ahead is only a fault later.
fn pre_fault(vcpu: &hv::Vcpu, working_set: &[hv::Touch]) -> usize {
    let mut mapped = 0;
    for (gpa, len) in runs(working_set.iter().map(|t| t.gpa)) {
        match vcpu.pre_fault(gpa, len) {
            Ok(true) => mapped += (len / PAGE) as usize,
            Ok(false) => {
                debug!("KVM cannot map memory ahead here (Linux 6.10+, with two-dimensional paging)");
                break;
            }
            Err(e) => {
                warn!("mapping the working set ahead at {gpa:#x}+{len:#x}: {e}");
                break;
            }
        }
    }
    mapped
}

/// Stage-2 pages, as working sets record them: the host's pages.
pub const PAGE: u64 = 4 << 10;

/// What records a working set: a restored guest's memory, whose host mappings start
/// empty, kept mapped until the recording ends, even if the VM stops first.
#[derive(Debug)]
pub struct Recorder {
    memory: Arc<GuestMemory>,
    pmem: Vec<(u64, Arc<pmem::Region>)>,
}

/// None: on KVM, the run that saves a template records nothing; its first warm restore
/// does ([`recorder`], `vm::RESTORES_RECORD`).
pub fn record(_vm: &hv::Vm) -> Result<Option<Recorder>, String> {
    Ok(None)
}

/// Records what restored machine `m` touches: its RAM is mapped copy-on-write from the
/// snapshot's file and its pmem from its images, afresh, so the host maps a page only once
/// the guest (or a device) touches it; what the host maps when the recording ends is
/// what the guest touched, and a private copy of RAM is a page it wrote. None where the
/// host's pages are not the working set's.
pub fn recorder(m: &Machine) -> Option<Recorder> {
    let page = platform::page_size().ok()? as u64;
    (page == PAGE).then(|| Recorder {
        memory: m.memory.clone(),
        pmem: m.bus.pmem.clone(),
    })
}

/// The pages touched since the restore: those the host maps, written where it holds a
/// private copy of RAM. pmem is read-only, so a page there that is not the file's is the
/// shared zero page, past the end of the file.
pub fn recorded(recorder: &Recorder) -> Result<Vec<hv::Touch>, String> {
    let regions = recorder
        .memory
        .regions()
        .map(|(gpa, host, len)| (gpa, host, len, true))
        .chain(
            recorder
                .pmem
                .iter()
                .map(|(gpa, r)| (*gpa, r.host(), r.len(), false)),
        );
    let mut touched = Vec::new();
    for (gpa, host, len, writable) in regions {
        let mapped =
            platform::mapped_pages(host, len).map_err(|e| format!("the pages mapped at {gpa:#x}: {e}"))?;
        touched.extend(mapped.into_iter().map(|(i, copied)| hv::Touch {
            gpa: gpa + i as u64 * PAGE,
            written: copied && writable,
        }));
    }
    Ok(touched)
}

/// How many pages of its working set a restored machine mapped ahead.
pub fn prefetched(start: &Start) -> usize {
    match start {
        Start::Restore(r) => r.prefetched.get().copied().unwrap_or(0),
        Start::Boot(_) => 0,
    }
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
            // After the state, which sets the paging mode KVM maps for.
            if index == 0 && !r.working_set.is_empty() {
                let t0 = crate::log::uptime_us();
                let n = pre_fault(&vcpu, &r.working_set);
                debug!(
                    "mapped {n} of {} working-set pages ahead in {} us",
                    r.working_set.len(),
                    crate::log::uptime_us().saturating_sub(t0)
                );
                let _ = r.prefetched.set(n);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_join_consecutive_pages_once_each() {
        let last = !(PAGE - 1);
        let pages = [0x3000, 0x1000, 0x2000, 0x2000, 0x2fff, 0x9000, 0x8000, last];
        assert_eq!(
            runs(pages.into_iter()),
            [(0x1000, 0x3000), (0x8000, 0x2000), (last, PAGE)]
        );
        assert_eq!(runs(std::iter::empty()), []);
    }
}
