//! The arm64 machine on any backend: memory map, in-kernel GICv3, devicetree, devices.
//! It is built either to boot a kernel or to resume a snapshot.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Config, Console};
use crate::arch::aarch64::state::{MachineState, VcpuState};
use crate::arch::aarch64::{self, Entry, boot, layout};
use crate::devices::control::Control;
use crate::devices::power::Power;
use crate::devices::rtc::Pl031;
use crate::devices::serial::Serial;
use crate::devices::virtio::{VirtioDevice, block::Block, mmio as virtio_mmio, pmem, vsock};
use crate::devices::vmgenid::VmGenId;
use crate::devices::{Interrupt, MmioBus};
use crate::hv::{self, Gic, GicLayout};
use crate::memory::GuestMemory;
use crate::snapshot::codec::{Reader, Writer};
use crate::snapshot::{MachineConfig, Snapshot};
use crate::{debug, initramfs, platform, warn};

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
    /// GIC distributor registers, applied once every vCPU exists (see [`finish`]).
    pub dist: Vec<(u32, u64)>,
    /// The device bus's saved state, applied by [`finish`].
    pub devices: Vec<u8>,
    /// The snapshot's working set, to prefetch before the guest runs; empty for none.
    pub working_set: Vec<hv::Touch>,
    /// How many of its pages were prefetched.
    pub prefetched: std::sync::OnceLock<usize>,
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

/// The smallest architected IPA width that covers guest RAM, within the host's limit.
fn ipa_bits(end: u64) -> Result<u32, String> {
    let needed = 64 - end.saturating_sub(1).leading_zeros();
    let max = hv::max_ipa_bits().map_err(|e| e.to_string())?;
    [36, 40, 42, 44, 48]
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
}

/// `vsock` is the host socket path of the vsock device `config` asks for.
fn assemble(
    memory: &Arc<GuestMemory>,
    config: &MachineConfig,
    console: Console,
    vsock: Option<&Path>,
) -> Result<Assembled, String> {
    let ram = ram_bytes(config.memory_mib)?;
    // Device memory (pmem regions) starts at the first GiB boundary after RAM, and the
    // address space must reach its end.
    let mut top = layout::DRAM_BASE + ram;
    let mut regions = Vec::with_capacity(config.pmem.len());
    let mut next = top.next_multiple_of(GIB);
    for path in &config.pmem {
        let region = Arc::new(pmem::Region::open(path)?);
        let gpa = next;
        next = gpa
            .checked_add(region.len() as u64)
            .ok_or("pmem regions overflow the address space")?;
        top = next;
        regions.push((region, gpa));
    }
    let ipa_bits = ipa_bits(top)?;
    let mpidrs: Vec<u64> = (0..config.vcpus).map(aarch64::mpidr).collect();
    let vm = hv::Vm::new(hv::VmConfig {
        ipa_bits,
        mpidrs: mpidrs.clone(),
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
    debug!("VM created and RAM mapped");

    let gp = hv::gic_params().map_err(|e| e.to_string())?;
    let redist_total = gp.redist_size * u64::from(config.vcpus);
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
    debug!("GIC created");

    let slots = config.disks.len() + regions.len() + usize::from(config.vsock);
    if slots as u64 > layout::VIRTIO_MMIO_MAX {
        return Err(format!(
            "at most {} virtio devices are supported",
            layout::VIRTIO_MMIO_MAX
        ));
    }
    let mut bus = MmioBus::default();
    let mut virtio = Vec::with_capacity(slots);
    // Each virtio device takes the next MMIO window and SPI, in the guest's probe order.
    let mut add_virtio = |bus: &mut MmioBus, device: Box<dyn VirtioDevice>| -> Result<(), String> {
        let i = virtio.len();
        let spi = layout::SPI_VIRTIO_MMIO + i as u32;
        let base = layout::VIRTIO_MMIO + i as u64 * layout::VIRTIO_MMIO_STRIDE;
        let line = Arc::new(GicLine {
            gic,
            intid: SPI_INTID_BASE + spi,
        });
        let transport = virtio_mmio::MmioTransport::new(device, memory.clone(), line);
        bus.insert(base, virtio_mmio::WINDOW, Arc::new(transport))?;
        virtio.push(boot::MmioDevice {
            base,
            size: virtio_mmio::WINDOW,
            spi,
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
        let path = vsock.ok_or("the machine has a vsock device but no socket path for it")?;
        add_virtio(&mut bus, Box::new(vsock::Vsock::new(path, vsock::GUEST_CID)?))?;
    }
    let out: Box<dyn Write + Send> = match console {
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
    })
}

/// A machine that boots `cfg.kernel`.
pub fn build(cfg: &Config) -> Result<Machine, String> {
    let ram = ram_bytes(cfg.memory_mib)?;
    let memory = Arc::new(GuestMemory::anonymous(&ram_ranges(ram)?).map_err(|e| format!("guest RAM: {e}"))?);
    // A bad kernel fails the start before any hypervisor state exists. The image is copied,
    // not mapped: a mapped image stalled boots for up to 1 s (platform-measurements M16).
    let kernel_file = File::open(&cfg.kernel).map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, ram).map_err(|e| e.to_string())?;
    debug!("kernel loaded");

    let config = MachineConfig {
        vcpus: cfg.vcpus,
        memory_mib: cfg.memory_mib,
        disks: cfg.disks.iter().map(|d| (d.path.clone(), d.read_only)).collect(),
        pmem: cfg.pmem.clone(),
        vsock: cfg.vsock.is_some(),
    };
    let a = assemble(&memory, &config, cfg.console, cfg.vsock.as_deref())?;
    a.vmgenid.write_new_id()?;

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
    let fdt = boot::build_fdt(&boot::Machine {
        mpidrs: &a.mpidrs,
        ram_size: ram,
        cmdline: &cfg.cmdline,
        initrd,
        gic_dist: (layout::GIC_DIST, a.gic_dist_size),
        gic_redist: (layout::GIC_REDIST, a.redist_total),
        virtio: &a.virtio,
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
pub type Recorder = Arc<hv::Watch>;

/// Starts recording `vm`'s working set: its stage 2 watches RAM and pmem alike. Every
/// vCPU must be out of the guest.
pub fn record(vm: &hv::Vm) -> Result<Option<Recorder>, String> {
    let watch = vm.watch();
    watch.start().map_err(|e| e.to_string())?;
    Ok(Some(watch))
}

/// Stops recording; the pages touched since [`record`], in order.
pub fn recorded(recorder: &Recorder) -> Result<Vec<hv::Touch>, String> {
    Ok(recorder.stop())
}

/// None: on HVF, the run that saves a template records its working set ([`record`]).
pub fn recorder(_m: &Machine) -> Option<Recorder> {
    None
}

/// How many pages of its working set a restored machine prefetched.
pub fn prefetched(start: &Start) -> usize {
    match start {
        Start::Restore(r) => r.prefetched.get().copied().unwrap_or(0),
        Start::Boot(_) => 0,
    }
}

/// A machine that resumes `snap`, with guest RAM mapped copy-on-write from `memory_file`,
/// that prefetches `working_set` before the guest runs.
pub fn restore(
    snap: &Snapshot,
    memory_file: &File,
    console: Console,
    vsock: Option<&Path>,
    working_set: Vec<hv::Touch>,
) -> Result<Machine, String> {
    super::check_vsock(snap, vsock)?;
    let state = decode_state(&snap.arch)?;
    if state.vcpus.len() != snap.config.vcpus as usize {
        return Err(format!(
            "snapshot has state for {} vCPUs but configures {}",
            state.vcpus.len(),
            snap.config.vcpus
        ));
    }
    let ram = ram_bytes(snap.config.memory_mib)?;
    let memory = Arc::new(
        GuestMemory::from_file(&ram_ranges(ram)?, memory_file)
            .map_err(|e| format!("snapshot memory: {e}"))?,
    );
    let a = assemble(&memory, &snap.config, console, vsock)?;
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
            dist: state.dist,
            devices: snap.devices.clone(),
            working_set,
            prefetched: std::sync::OnceLock::new(),
        }),
        config: snap.config.clone(),
    })
}

/// Completes the machine once every vCPU exists and before any runs. A restore applies
/// the GIC distributor only now: HVF routes an SPI when its IROUTER is written, and
/// routing to a CPU that does not exist yet loses the SPI for good. Found by the snapshot
/// E2E test; SPIs were pending but never delivered. Devices go after the GIC, because
/// they re-raise their interrupt lines as they restore. Last, the restored guest gets a
/// new generation ID, so it reseeds its RNG before it runs anything that uses it.
pub fn finish(vm: &hv::Vm, bus: &MmioBus, vmgenid: &VmGenId, start: &Start) -> Result<(), String> {
    let Start::Restore(r) = start else {
        return Ok(());
    };
    vm.restore_gic(&r.dist).map_err(|e| e.to_string())?;
    let mut devices = Reader::new(&r.devices);
    bus.restore(&mut devices)
        .and_then(|()| devices.finish())
        .map_err(|e| e.to_string())?;
    vmgenid.new_generation()
}

/// Creates vCPU `index` and puts it where `start` says: the boot entry, parked for
/// PSCI CPU_ON, or its restored state.
pub fn setup_vcpu(vm: &hv::Vm, index: usize, start: &Start) -> Result<hv::Vcpu, String> {
    let e = |e: hv::Error| e.to_string();
    let mut vcpu = vm.create_vcpu(index).map_err(e)?;
    match start {
        Start::Boot(entry) => {
            if index == 0 {
                vcpu.boot(*entry);
            }
        }
        Start::Restore(r) => {
            if index == 0 {
                let here = vcpu.cpu_id().map_err(e)?;
                if here != r.cpu_id {
                    return Err(format!(
                        "this CPU is not the one the snapshot was taken on: {here:x?} vs {:x?}",
                        r.cpu_id
                    ));
                }
            }
            // Before the state: the prefetch runs on this vCPU. A failure is the VM's, since
            // its translations may outlive a loop that stopped short.
            if index == 0 && !r.working_set.is_empty() {
                let n = vcpu
                    .prefetch(vm, &r.working_set)
                    .map_err(|e| format!("prefetching the working set: {e}"))?;
                let _ = r.prefetched.set(n);
            }
            let state = r
                .vcpus
                .get(index)
                .ok_or_else(|| format!("the snapshot has no vCPU {index}"))?;
            vcpu.restore_state(state).map_err(e)?;
        }
    }
    Ok(vcpu)
}

/// For a restore, the counter offset every vCPU applies at release, taken now so the
/// guest counter continues from the snapshot.
pub fn release_offset(start: &Start) -> Option<u64> {
    match start {
        Start::Restore(r) => Some(hv::host_counter().wrapping_sub(r.counter)),
        Start::Boot(_) => None,
    }
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

/// The architecture state a snapshot records: every vCPU's, in index order, and the
/// distributor. The VM's counter is the latest any vCPU saw.
pub fn encode_state(vm: &hv::Vm, captured: Vec<Captured>) -> Result<Vec<u8>, String> {
    let counter = captured.iter().map(|c| c.counter).max().unwrap_or(0);
    let cpu_id = captured
        .iter()
        .find_map(|c| c.cpu_id.clone())
        .ok_or("vCPU 0 captured no CPU identity")?;
    let state = MachineState {
        counter,
        cpu_id,
        dist: vm.save_gic().map_err(|e| e.to_string())?,
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
