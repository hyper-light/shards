//! The arm64 machine on any backend: memory map, in-kernel GICv3, devicetree, devices.
//! It is built either to boot a kernel or to resume a snapshot.

use std::fs::File;
use std::io::Write;
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
    /// Each pmem region's guest address and length.
    pmem: Vec<(u64, u64)>,
}

/// `vsock` is the host side of the vsock device `config` asks for.
fn assemble(
    memory: &Arc<GuestMemory>,
    config: &MachineConfig,
    console: Console,
    vsock: Option<&super::VsockHost>,
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
    let pmem: Vec<(u64, u64)> = regions
        .iter()
        .map(|(region, gpa)| (*gpa, region.len() as u64))
        .collect();
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
        let host = vsock.ok_or("the machine has a vsock device but no host side for it")?;
        add_virtio(
            &mut bus,
            Box::new(vsock::Vsock::new(host.clone(), vsock::GUEST_CID)?),
        )?;
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
        pmem,
    })
}

/// A machine that boots `cfg.kernel`.
pub fn build(cfg: &Config) -> Result<Machine, String> {
    let ram = ram_bytes(cfg.memory_mib)?;
    let memory = Arc::new(GuestMemory::anonymous(&ram_ranges(ram)?).map_err(|e| format!("guest RAM: {e}"))?);
    // A bad kernel fails the start before any hypervisor state exists. The image is copied,
    // not mapped: a mapped image stalled boots for up to 1 s (platform-measurements M16).
    let kernel_file = crate::platform::open_input(&cfg.kernel, false)
        .map_err(|e| format!("{}: {e}", cfg.kernel.display()))?;
    let kernel = boot::load_kernel(&memory, &kernel_file, ram).map_err(|e| e.to_string())?;
    debug!("kernel loaded");

    let config = super::machine_config(cfg)?;
    let a = assemble(&memory, &config, cfg.console, cfg.vsock.as_ref())?;
    a.vmgenid.write_new_id()?;

    let fdt_addr = layout::DRAM_BASE + ram - boot::FDT_MAX;
    // Between the kernel, at the next 2 MiB, and the device tree (boot.rs, load_initrd).
    let room = fdt_addr.saturating_sub(kernel.end.next_multiple_of(2 << 20));
    let initrd_bytes = super::initrd(cfg, room)?;
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
/// What a machine keeps of its [`Start`] once its vCPUs are set up, for its release and
/// its diagnostics; the rest is dropped (audit D03).
#[derive(Debug)]
pub struct Kept {
    /// A restore's guest counter at its snapshot.
    counter: Option<u64>,
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
    vsock: Option<&super::VsockHost>,
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

/// Completes the machine once every vCPU exists and before any runs. A restore applies
/// the GIC's state only now, as Hypervisor.framework requires (hv_gic.h): HVF routes an
/// SPI to its vCPU, and routing to a CPU that does not exist yet loses the SPI for good.
/// Found by the snapshot E2E test; SPIs were pending but never delivered. The state is
/// HVF's own serialization, not the distributor's registers, which lose an interrupt HVF
/// has passed on toward a vCPU that has yet to take it (found by the storm E2E test: a
/// request completed, the guest never told). Devices go after the GIC, because they
/// re-raise their interrupt lines as they restore. Last, the restored guest gets a new
/// generation ID, so it reseeds its RNG before it runs anything that uses it.
pub fn finish(vm: &hv::Vm, bus: &MmioBus, vmgenid: &VmGenId, rest: Option<&Remainder>) -> Result<(), String> {
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
                vcpu.boot(entry);
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
