//! The x86_64 machine on any backend: RAM around the 32-bit MMIO gap, the in-kernel
//! IOAPIC, ACPI tables, COM1, the i8042 and ACPI sleep ports the guest resets and powers
//! off through (docs/research/kvm-x86_64-ground-truth.md §3-§6), and the VM generation ID
//! that makes each restored copy reseed its RNG.

use std::fs::File;
use std::io::Write;
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
use crate::{debug, platform, warn};

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
}

/// What vCPU `index` sets itself up from ([`split`]): moved to its thread, its own alone.
#[derive(Debug)]
pub enum VcpuStart {
    /// The boot vCPU's registers; `None` for the others, which wait for INIT/SIPI.
    Boot(Option<x86_64::Boot>),
    Restore {
        state: hv::VcpuState,
        /// vCPU 0's: the working set it maps ahead.
        working_set: Option<Vec<hv::Touch>>,
    },
}

/// What a restore's start leaves the machine once the vCPUs have theirs ([`split`]):
/// applied by [`finish`] once every vCPU exists.
#[derive(Debug)]
pub struct Remainder {
    vm: hv::VmState,
    devices: Vec<u8>,
    /// Where the snapshot has vCPU 0's TSC start.
    tsc_start: Option<hv::TscStart>,
}

/// What vCPU 0 found as it set up, for the machine to keep.
#[derive(Debug, Default)]
pub struct SetUp {
    /// Pages of the working set it mapped ahead.
    prefetched: usize,
    /// Its TSC, which the release starts ([`release_offset`]), where the restore held the
    /// vCPUs' TSCs back.
    tsc: Option<hv::Tsc>,
}

/// Divides `start` among `vcpus` vCPUs and the machine: each vCPU takes only its own
/// part, and none is shared.
pub fn split(start: Start, vcpus: usize) -> Result<(Vec<VcpuStart>, Option<Remainder>), String> {
    match start {
        Start::Boot(boot) => Ok((
            (0..vcpus)
                .map(|index| VcpuStart::Boot((index == 0).then_some(boot)))
                .collect(),
            None,
        )),
        Start::Restore(r) => {
            if r.vcpus.len() < vcpus {
                return Err(format!("the snapshot has {} vCPUs, not {vcpus}", r.vcpus.len()));
            }
            let tsc_start = r.vcpus.first().and_then(hv::VcpuState::tsc_start);
            let mut working_set = Some(r.working_set);
            let starts = r
                .vcpus
                .into_iter()
                .take(vcpus)
                .map(|state| VcpuStart::Restore {
                    state,
                    working_set: working_set.take(),
                })
                .collect();
            Ok((
                starts,
                Some(Remainder {
                    vm: r.vm,
                    devices: r.devices,
                    tsc_start,
                }),
            ))
        }
    }
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
    hosts: super::Hosts<'_>,
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

    let slots =
        config.disks.len() + regions.len() + usize::from(config.vsock) + usize::from(config.net.is_some());
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
        let host = hosts
            .vsock
            .ok_or("the machine has a vsock device but no host side for it")?;
        add_virtio(
            &mut bus,
            Box::new(vsock::Vsock::new(host.clone(), vsock::GUEST_CID)?),
        )?;
    }
    #[cfg(unix)]
    if config.net.is_some() {
        let host = hosts
            .net
            .ok_or("the machine has a network device but no network process for it")?;
        let mut host = host.clone();
        // The machine's own MAC, a snapshot's included: its guest was set up with it.
        if let Some(mac) = config.net {
            host.mac = mac;
        }
        add_virtio(&mut bus, Box::new(crate::devices::virtio::net::Net::new(host)?))?;
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

    let kernel_file = crate::platform::open_input(&cfg.kernel, false)
        .map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, low_ram_end).map_err(|e| e.to_string())?;
    debug!("kernel loaded");

    let config = super::machine_config(cfg)?;
    let hosts = super::Hosts {
        vsock: cfg.vsock.as_ref(),
        #[cfg(unix)]
        net: cfg.net.as_ref(),
    };
    let a = assemble(&memory, &config, cfg.console, hosts)?;
    let tables = acpi::build(cfg.vcpus, &a.virtio)?.blobs;
    let access = memory.access().map_err(|e| e.to_string())?;
    for (addr, bytes) in tables {
        access.write(addr, &bytes).map_err(|e| e.to_string())?;
    }
    drop(access);
    a.vmgenid.write_new_id()?;

    // Between the kernel, at the next 2 MiB, and the end of low RAM (boot.rs, load_initrd).
    let room = low_ram_end
        .min(layout::MMIO_GAP)
        .saturating_sub(kernel.end.next_multiple_of(2 << 20));
    let initrd_bytes = super::initrd(cfg, room)?;
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
    hosts: super::Hosts<'_>,
    working_set: Vec<hv::Touch>,
) -> Result<Machine, String> {
    super::check_vsock(snap, hosts.vsock)?;
    #[cfg(unix)]
    if snap.config.net.is_some() != hosts.net.is_some() {
        return Err(match snap.config.net {
            Some(_) => {
                "the snapshot has a network device: give the restored VM its own network process".into()
            }
            None => "the snapshot has no network device for a network process".into(),
        });
    }
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
    let a = assemble(&memory, &snap.config, console, hosts)?;
    let guest: Vec<(u64, u64)> = memory
        .regions()
        .map(|(gpa, _, len)| (gpa, len as u64))
        .chain(a.bus.pmem.iter().map(|(gpa, region)| (*gpa, region.len() as u64)))
        .collect();
    let working_set = super::usable_working_set(working_set, PAGE, &guest);
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
/// What a machine keeps of its [`Start`] once its vCPUs are set up, for its release and
/// its diagnostics; the rest is dropped (audit D03).
#[derive(Debug)]
pub struct Kept {
    /// A restore's: vCPU 0's TSC, and where the snapshot has it start.
    tsc: Option<(hv::Tsc, Option<hv::TscStart>)>,
    prefetched: usize,
}

/// What the start leaves the running machine, once every vCPU is set up.
pub fn keep(rest: Option<&Remainder>, first: &SetUp) -> Kept {
    Kept {
        tsc: rest.and_then(|r| first.tsc.clone().map(|tsc| (tsc, r.tsc_start))),
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
        r.vcpus.capacity() * std::mem::size_of::<hv::VcpuState>(),
        r.devices.capacity(),
        r.working_set.capacity() * std::mem::size_of::<hv::Touch>(),
    ])
}

pub fn prefetched(kept: &Kept) -> usize {
    kept.prefetched
}

/// Creates vCPU `index` and puts it where `start` says: the boot protocol's registers for
/// the boot vCPU, or its restored state.
pub fn setup_vcpu(vm: &hv::Vm, index: usize, start: VcpuStart) -> Result<(hv::Vcpu, SetUp), String> {
    let mut vcpu = vm.create_vcpu(index).map_err(|e| e.to_string())?;
    let mut set_up = SetUp::default();
    match start {
        VcpuStart::Boot(boot) => {
            if let Some(boot) = boot {
                vcpu.boot(&boot).map_err(|e| e.to_string())?;
            }
        }
        VcpuStart::Restore { state, working_set } => {
            vcpu.restore_state(&state).map_err(|e| e.to_string())?;
            if let Some(working_set) = working_set {
                set_up.tsc = vcpu.tsc();
                // After the state, which sets the paging mode KVM maps for.
                if !working_set.is_empty() {
                    let t0 = crate::log::uptime_us();
                    set_up.prefetched = pre_fault(&vcpu, &working_set);
                    debug!(
                        "mapped {} of {} working-set pages ahead in {} us",
                        set_up.prefetched,
                        working_set.len(),
                        crate::log::uptime_us().saturating_sub(t0)
                    );
                }
            }
        }
    }
    Ok((vcpu, set_up))
}

/// Completes a restore once every vCPU exists and before any runs: the interrupt
/// controllers and kvmclock, then the devices, which re-raise their lines as they
/// restore, then a new generation ID, so the guest reseeds its RNG before it runs
/// anything that uses it (as the arm64 machine orders them).
pub fn finish(vm: &hv::Vm, bus: &Bus, vmgenid: &Finish, rest: Option<&Remainder>) -> Result<(), String> {
    let Some(r) = rest else {
        return Ok(());
    };
    vm.restore_state(&r.vm).map_err(|e| e.to_string())?;
    let mut devices = Reader::new(&r.devices);
    bus.restore(&mut devices)?;
    devices.finish().map_err(|e| e.to_string())?;
    vmgenid.new_generation()
}

/// For a restore that held the vCPUs' TSCs back: starts vCPU 0's at the value it saved,
/// now, and returns what every vCPU adds to the offset it saved, so that each TSC goes
/// on from the snapshot with no jump, as far from the others' as it was (as KVM documents
/// restoring TSCs, Documentation/virt/kvm/devices/vcpu.rst §4, less the time between,
/// which a snapshot's guest does not see). kvmclock came back with the VM's state.
pub fn release_offset(kept: &Kept) -> Result<Option<u64>, String> {
    match &kept.tsc {
        Some((tsc, Some(start))) => tsc.restart(*start).map_err(|e| format!("the TSC: {e}")),
        Some((_, None)) => Err("the TSC: the snapshot kept no TSC to start from".into()),
        None => Ok(None),
    }
}

pub fn set_counter_offset(vcpu: &mut hv::Vcpu, offset: u64) -> Result<(), String> {
    vcpu.resume_tsc(offset).map_err(|e| format!("the TSC: {e}"))
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
        .seq(254, 1, hv::VcpuState::decode)
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
