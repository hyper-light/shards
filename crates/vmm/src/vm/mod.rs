//! A microVM: configuration, lifecycle, and exit reasons, independent of the backend
//! that runs it.

use std::path::PathBuf;
#[cfg(hv)]
use std::sync::Arc;

#[cfg(hv = "hvf")]
use crate::devices::virtio::gpu;
#[cfg(hv)]
use crate::devices::virtio::{mem, pmem, vsock};

#[cfg(all(hv, target_arch = "aarch64"))]
mod aarch64;
#[cfg(all(hv, target_arch = "aarch64"))]
use aarch64 as machine;
#[cfg(all(hv, target_arch = "x86_64"))]
mod x86_64;
#[cfg(all(hv, target_arch = "x86_64"))]
use x86_64 as machine;
/// Whether this build's VMs can be snapshotted and restored: HVF's and x86_64 KVM's can,
/// arm64 KVM's and WHP's not yet (D140).
pub const SNAPSHOTS: bool = cfg!(any(hv = "hvf", all(hv = "kvm", target_arch = "x86_64")));
/// Whether a VM resumed from its snapshot records its working set, for restores to
/// prefetch: HVF's at stage 2 (hv::hvf::Watch), KVM's from the pages its host maps
/// (vm::x86_64::recorder).
pub const WORKING_SETS: bool = SNAPSHOTS;
/// Which run records it. HVF's: the one that saves the template, whose command recording
/// slows sixfold (PM M30), on a run that boots anyway. KVM's: the first warm restore
/// without one, for which recording costs nothing, and whose pages later restores touch,
/// where the saving run's they barely do (PM M33).
pub const RESTORES_RECORD: bool = cfg!(hv = "kvm");

#[cfg(any(hv, test))]
mod barrier;
#[cfg(hv)]
mod runtime;
#[cfg(hv)]
pub use runtime::{
    Handle, Running, accept_working_set, check_host, max_vcpus, restore, restore_from, start,
    working_set_limit,
};

#[cfg(not(hv))]
mod unsupported;
#[cfg(not(hv))]
pub use unsupported::{
    Handle, Running, accept_working_set, check_host, max_vcpus, restore, restore_from, start,
    working_set_limit,
};

/// The host sides of a VM's devices that live outside it.
#[cfg(hv)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hosts<'a> {
    pub vsock: Option<&'a VsockHost>,
    pub net: Option<&'a crate::devices::virtio::net::NetHost>,
    /// Each virtio-fs device's slot, in order (D38).
    #[cfg(unix)]
    pub shares: &'a [crate::devices::virtio::fs::Share],
    /// The join disk's ranges, which its VM process gives joining containers' images
    /// (D119).
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
}

/// A snapshot's vsock and network devices, and the restore's host sides for them, come
/// together or not at all.
#[cfg(hv)]
fn check_hosts(snap: &crate::snapshot::Snapshot, hosts: Hosts<'_>) -> Result<(), String> {
    match (snap.config.vsock, hosts.vsock) {
        (true, None) => {
            return Err(
                "the snapshot has a vsock device: give the restored VM its own socket with --vsock PATH"
                    .into(),
            );
        }
        (false, Some(_)) => return Err("the snapshot has no vsock device for --vsock".into()),
        _ => {}
    }
    match (snap.config.net, hosts.net) {
        (Some(_), None) => {
            return Err(
                "the snapshot has a network device: give the restored VM its own network process".into(),
            );
        }
        (None, Some(_)) => return Err("the snapshot has no network device for a network process".into()),
        _ => {}
    }
    #[cfg(unix)]
    if snap.config.shares as usize != hosts.shares.len() {
        return Err(format!(
            "the snapshot has {} shared directories; the restore gives {}",
            snap.config.shares,
            hosts.shares.len()
        ));
    }
    match (snap.config.join, hosts.join) {
        (true, None) => return Err("the snapshot has a join disk: give the restored VM its own".into()),
        (false, Some(_)) => return Err("the snapshot has no join disk for the restore's".into()),
        _ => {}
    }
    #[cfg(unix)]
    match (snap.config.join_share, hosts.join_share) {
        (true, None) => return Err("the snapshot has a join share: give the restored VM its own".into()),
        (false, Some(_)) => return Err("the snapshot has no join share for the restore's".into()),
        _ => {}
    }
    Ok(())
}

/// pmem regions, each with the guest address it maps at.
#[cfg(hv)]
type Regions = Vec<(Arc<pmem::Region>, u64)>;

/// `config`'s pmem files, opened, each at the guest address it maps at: from `first`, in
/// order, each right after the one before; and the address past the last. Opened before
/// the VM, so that on every early return they outlive it.
#[cfg(hv)]
fn pmem_regions(config: &crate::snapshot::MachineConfig, first: u64) -> Result<(Regions, u64), String> {
    let mut regions = Vec::with_capacity(config.pmem.len());
    let mut next = first;
    for path in &config.pmem {
        let region = Arc::new(pmem::Region::open(path)?);
        let gpa = next;
        next = gpa
            .checked_add(region.len() as u64)
            .ok_or("pmem regions overflow the address space")?;
        regions.push((region, gpa));
    }
    Ok((regions, next))
}

/// The memory `config` may have plugged (D133), `(most, region)` in bytes: its
/// `hotplug_mib`, whole blocks of [`mem::BLOCK`], and the region that holds it, whole
/// blocks of Linux's memory ([`mem::ALIGN`]); none where it has none.
#[cfg(hv)]
fn hotplug(config: &crate::snapshot::MachineConfig) -> Result<Option<(u64, u64)>, String> {
    if config.hotplug_mib == 0 {
        return Ok(None);
    }
    let too_much = || format!("{} MiB of memory to plug", config.hotplug_mib);
    let most = config.hotplug_mib.checked_mul(1 << 20).ok_or_else(too_much)?;
    if !most.is_multiple_of(mem::BLOCK) {
        return Err(format!(
            "memory to plug must be whole blocks of {} MiB",
            mem::BLOCK >> 20
        ));
    }
    let region = most.checked_next_multiple_of(mem::ALIGN).ok_or_else(too_much)?;
    Ok(Some((most, region)))
}

/// Guest RAM of `ranges`, and the memory `config` may have plugged at `at`, withheld until
/// plugged (D133).
#[cfg(hv)]
fn guest_memory(
    ranges: &[(u64, usize)],
    config: &crate::snapshot::MachineConfig,
    at: u64,
) -> Result<crate::memory::GuestMemory, String> {
    let memory = match hotplug(config)? {
        None => crate::memory::GuestMemory::anonymous(ranges),
        Some((_, region)) => {
            let len = usize::try_from(region).map_err(|_| format!("{region} bytes of memory to plug"))?;
            crate::memory::GuestMemory::with_hotplug(ranges, at, len, mem::BLOCK)
        }
    };
    memory.map_err(|e| format!("guest RAM: {e}"))
}

/// Guest RAM of `ranges` from a snapshot's memory `file`, and, as boot made it, the memory
/// `config` may have plugged at `at`, none of it plugged: a snapshot keeps none (D148).
#[cfg(hv)]
fn restored_memory(
    ranges: &[(u64, usize)],
    file: &std::fs::File,
    config: &crate::snapshot::MachineConfig,
    at: u64,
) -> Result<crate::memory::GuestMemory, String> {
    let memory =
        crate::memory::GuestMemory::from_file(ranges, file).map_err(|e| format!("snapshot memory: {e}"))?;
    match hotplug(config)? {
        None => Ok(memory),
        Some((_, region)) => {
            let len = usize::try_from(region).map_err(|_| format!("{region} bytes of memory to plug"))?;
            memory
                .with_plug_region(at, len, mem::BLOCK)
                .map_err(|e| format!("snapshot memory: {e}"))
        }
    }
}

#[cfg(hv)]
impl mem::Mapper for crate::hv::Stage2 {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::Stage2::map(self, host, gpa, len) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::Stage2::unmap(self, gpa, len).map_err(|e| e.to_string())
    }
}

/// The virtio-mem device of `memory`'s memory to plug, and the sizes the control page
/// shares with it (D133); none where it has none. The region is the VM's from here on,
/// mapped as the hypervisor maps it ([`crate::hv::Vm::add_hotplug`]).
#[cfg(hv)]
#[expect(
    clippy::type_complexity,
    reason = "the device and the sizes the control page shares with it, made together"
)]
fn hotplug_device(
    vm: &crate::hv::Vm,
    memory: &crate::memory::GuestMemory,
    config: &crate::snapshot::MachineConfig,
) -> Result<Option<(Box<dyn crate::devices::virtio::VirtioDevice>, Arc<mem::Sizes>)>, String> {
    let (Some((gpa, host, len)), Some((most, _))) = (memory.hotplug(), hotplug(config)?) else {
        return Ok(None);
    };
    // SAFETY: `memory` outlives the VM: `Machine` and `Running` drop the VM first.
    let stage2 = unsafe { vm.add_hotplug(host, gpa, len) }.map_err(|e| e.to_string())?;
    let sizes = Arc::new(mem::Sizes::new(gpa, len as u64, most));
    Ok(Some((Box::new(mem::Mem::new(sizes.clone(), stage2)), sizes)))
}

/// The virtio-balloon device that gives the host back the memory the guest reports free
/// (D151), where `config` has one; it takes guest RAM from the guest and gives it back
/// through the VM's stage 2.
#[cfg(hv)]
fn reporting_device(
    vm: &crate::hv::Vm,
    config: &crate::snapshot::MachineConfig,
) -> Option<Box<dyn crate::devices::virtio::VirtioDevice>> {
    config.reporting.then(|| {
        Box::new(crate::devices::virtio::balloon::Balloon::new(vm.stage2()))
            as Box<dyn crate::devices::virtio::VirtioDevice>
    })
}

/// `config`'s virtio devices in the guest's probe order, which each machine gives the
/// next of its MMIO windows and interrupt lines: its disks, its join disk, its pmem
/// `regions`, its vsock device, its network device, its shared directories, its join share,
/// and the machine's own devices, `last`: its GPU, its memory to plug, then its balloon (last of
/// all: no other device's window moves for them); at most `max` of them. The devices share the regions:
/// `regions`, made before the VM, holds them until after it on every early return, the
/// hypervisor mapping them until its destroy.
#[cfg(hv)]
fn virtio_devices(
    config: &crate::snapshot::MachineConfig,
    regions: &[(Arc<pmem::Region>, u64)],
    hosts: Hosts<'_>,
    last: Vec<Box<dyn crate::devices::virtio::VirtioDevice>>,
    max: u64,
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>, String> {
    let slots = config.disks.len()
        + usize::from(config.join)
        + regions.len()
        + usize::from(config.vsock)
        + usize::from(config.net.is_some())
        + config.shares as usize
        + usize::from(config.join_share)
        + last.len();
    if slots as u64 > max {
        return Err(format!("at most {max} virtio devices are supported"));
    }
    let mut devices: Vec<Box<dyn crate::devices::virtio::VirtioDevice>> = Vec::with_capacity(slots);
    for (i, (path, read_only)) in config.disks.iter().enumerate() {
        devices.push(Box::new(crate::devices::virtio::block::Block::open(
            path,
            *read_only,
            &format!("shards-disk{i}"),
        )?));
    }
    // Its join disk, the virtio-blk device after its disks, which init finds by its
    // serial (D119).
    if config.join {
        let disk = hosts
            .join
            .ok_or("the machine has a join disk but no host side for it")?;
        devices.push(Box::new(crate::devices::virtio::block::Block::join(
            disk,
            shards_abi::JOIN_DISK_SERIAL,
        )));
    }
    for (region, gpa) in regions {
        devices.push(Box::new(pmem::Pmem::new(region.clone(), *gpa)));
    }
    if config.vsock {
        let host = hosts
            .vsock
            .ok_or("the machine has a vsock device but no host side for it")?;
        devices.push(Box::new(vsock::Vsock::new(host.clone(), vsock::GUEST_CID)?));
    }
    if let Some(mac) = config.net {
        let mut host = hosts
            .net
            .ok_or("the machine has a network device but no network process for it")?
            .clone();
        // The machine's own MAC, a snapshot's included: its guest was set up with it.
        host.mac = mac;
        devices.push(Box::new(crate::devices::virtio::net::Net::new(host)?));
    }
    // Then its shared directories, each mounted by its tag, `shards0` and on.
    #[cfg(unix)]
    for i in 0..config.shares as usize {
        let slot = hosts
            .shares
            .get(i)
            .ok_or("the machine has a shared directory but no slot for it")?
            .clone();
        devices.push(Box::new(crate::devices::virtio::fs::Fs::new(
            &format!("shards{i}"),
            slot,
        )?));
    }
    // Its join share, after them, which init mounts by its tag once a joiner brings
    // volumes (D119).
    #[cfg(unix)]
    if config.join_share {
        let slot = hosts
            .join_share
            .ok_or("the machine has a join share but no slot for it")?;
        devices.push(Box::new(crate::devices::virtio::fs::Fs::join(
            shards_abi::JOIN_SHARE_TAG,
            slot,
        )?));
    }
    devices.extend(last);
    Ok(devices)
}

/// A GPU's renderer, and the length of the window its blobs are mapped into: what the
/// host's GPU says it has of memory the host can map, which Venus tells the guest, the most
/// a guest can have mapped at once (D144).
#[cfg(all(hv = "hvf", feature = "gpu"))]
fn gpu_renderer() -> Result<(Box<dyn gpu::Renderer>, u64), String> {
    let venus = gpu::virgl::Venus::new()?;
    let len = venus.host_visible();
    Ok((Box::new(venus), len))
}

#[cfg(all(hv = "hvf", not(feature = "gpu")))]
fn gpu_renderer() -> Result<(Box<dyn gpu::Renderer>, u64), String> {
    Err("this VM process renders no GPU: shards' GPU VM process does, on macOS (D144)".into())
}

#[cfg(hv = "hvf")]
impl gpu::Window for crate::hv::Window {
    unsafe fn map(&self, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::Window::map(self, host, gpa, len) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, gpa: u64, len: usize) -> Result<(), String> {
        crate::hv::Window::unmap(self, gpa, len).map_err(|e| e.to_string())
    }
}

/// Where the serial console's output goes.
#[cfg(hv)]
fn console_out(console: Console) -> Result<Box<dyn std::io::Write + Send>, String> {
    Ok(match console {
        Console::Stdout => Box::new(crate::platform::stdout_file().map_err(|e| format!("console: {e}"))?),
        Console::Discard => Box::new(std::io::sink()),
    })
}

/// `cfg`'s machine, its backing files resolved once to absolute paths: a snapshot of it
/// names the files the machine had, wherever it is restored from (audit A18).
#[cfg(hv)]
fn machine_config(cfg: &Config) -> Result<crate::snapshot::MachineConfig, String> {
    let resolve = |path: &std::path::Path| {
        crate::platform::input_path(path).map_err(|e| format!("{}: {e}", path.display()))
    };
    Ok(crate::snapshot::MachineConfig {
        vcpus: cfg.vcpus,
        memory_mib: cfg.memory_mib,
        disks: cfg
            .disks
            .iter()
            .map(|d| Ok((resolve(&d.path)?, d.read_only)))
            .collect::<Result<_, String>>()?,
        pmem: cfg
            .pmem
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<_, String>>()?,
        vsock: cfg.vsock.is_some(),
        net: cfg.net.as_ref().map(|n| n.mac),
        #[cfg(unix)]
        shares: u32::try_from(cfg.shares.len()).map_err(|_| "too many shared directories")?,
        #[cfg(not(unix))]
        shares: 0,
        join: cfg.join.is_some(),
        #[cfg(unix)]
        join_share: cfg.join_share.is_some(),
        #[cfg(not(unix))]
        join_share: false,
        nested: cfg.nested,
        hotplug_mib: cfg.hotplug_mib,
        reporting: cfg.reporting,
        // Its window's length, once the machine has its renderer (aarch64.rs `build`).
        gpu: None,
    })
}

/// `working_set` if every page of it, `page` bytes long, lies in one of `ranges`, the
/// `(guest address, length)` of the guest's RAM and pmem. Else nothing: the restore goes
/// without it rather than prefetch what the guest does not have (audit A16).
#[cfg(hv)]
fn usable_working_set(
    working_set: Vec<crate::hv::Touch>,
    page: u64,
    ranges: &[(u64, u64)],
) -> Vec<crate::hv::Touch> {
    let inside = |gpa: u64| {
        gpa.checked_add(page).is_some_and(|end| {
            ranges
                .iter()
                .any(|&(start, len)| gpa >= start && start.checked_add(len).is_some_and(|stop| end <= stop))
        })
    };
    match working_set.iter().find(|t| !inside(t.gpa)) {
        None => working_set,
        Some(t) => {
            crate::warn!(
                "the working set names {:#x}, outside the guest's memory; restoring without prefetching it",
                t.gpa
            );
            Vec::new()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    /// A prebuilt initramfs image.
    pub initrd: Option<PathBuf>,
    /// A guest executable to run as PID 1 from a generated initramfs (exclusive with
    /// `initrd`).
    pub init: Option<PathBuf>,
    pub cmdline: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Where guest console (UART) output goes.
    pub console: Console,
    /// virtio-blk disks, in the order the guest enumerates them (vda, vdb, ...).
    pub disks: Vec<Disk>,
    /// What to do when the guest asks for a snapshot; without it, requests are ignored.
    pub snapshot: Option<SnapshotPolicy>,
    /// Read-only virtio-pmem devices backed by these files, in guest order (pmem0, ...).
    pub pmem: Vec<PathBuf>,
    /// A virtio-vsock device, and where its host side is.
    pub vsock: Option<VsockHost>,
    /// A virtio-net device, and the network process's side of it (D31).
    pub net: Option<crate::devices::virtio::net::NetHost>,
    /// virtio-fs devices, each a slot its directory's server is put in (D38).
    #[cfg(unix)]
    pub shares: Vec<crate::devices::virtio::fs::Share>,
    /// A join disk, after the disks: empty until the VM process gives a joining
    /// container's image a range of it (D119).
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// Virtualization for the guest itself (nested): on arm64 its vCPUs have EL2, so
    /// that its kernel may be a hypervisor (KVM). Refused where the host cannot give it,
    /// and its snapshot refused, its EL2 state not saved.
    pub nested: bool,
    /// Memory the guest may have plugged beyond `memory_mib`, in MiB, as it asks for it
    /// through the control page: a virtio-mem device (D133), last of its devices; 0 for
    /// none. Its snapshot keeps none of it plugged (D148).
    pub hotplug_mib: u64,
    /// A virtio-balloon device that gives the host back the memory the guest reports free
    /// (D151), after the memory to plug: last of its devices.
    pub reporting: bool,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
    /// A virtio-gpu device, Vulkan through Venus (D144), rendered in this process: a VM
    /// process built with the renderer (`shards-vm-gpu`) has one. Its snapshot is refused
    /// once the guest has made a context or a resource, which are its host driver's and no
    /// snapshot holds; a template is saved before any (D150).
    pub gpu: bool,
    /// A PCI bus with a test function on it, QEMU's edu's identification and liveness
    /// registers (D153): what tests read to show that the bus reaches the VMM. A machine
    /// with a PCI bus saves no snapshot.
    pub pci_test: bool,
    /// Host PCI functions passed through by VFIO, Linux's (D154), on the PCI bus after the
    /// test function. A machine given any has no memory to plug and no balloon: the
    /// functions' DMA reaches guest RAM through mappings the host's IOMMU holds pinned,
    /// which neither may take from under them (as QEMU's `ram_block_discard_disable`).
    pub vfio: Vec<Vfio>,
}

/// A host PCI function a VM is given (D154): its name as the host has it (`0000:01:00.0`)
/// and its IOMMU group's VFIO device, found before the VM process is confined
/// ([`Vfio::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vfio {
    pub name: String,
    pub group: PathBuf,
    /// Whether the group has no IOMMU (vfio's `enable_unsafe_noiommu_mode`): its function
    /// may make no DMA, and guest RAM is mapped for none.
    pub noiommu: bool,
}

impl Vfio {
    /// The host's function `name`, by the VFIO group it is in: it must be bound to
    /// vfio-pci, as must every other function in its IOMMU group.
    pub fn of(name: &str) -> Result<Vfio, String> {
        #[cfg(target_os = "linux")]
        {
            let (group, noiommu) =
                crate::devices::pci::vfio::group_of(name).map_err(|e| format!("{name}: {e}"))?;
            Ok(Vfio {
                name: name.to_string(),
                group,
                noiommu,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Err(format!(
            "{name}: a host's PCI devices are passed through by VFIO, Linux's (D154)"
        ))
    }
}

/// The PCI functions a machine is given (D153, D154).
#[cfg(hv)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Pci<'a> {
    pub test: bool,
    pub vfio: &'a [Vfio],
}

/// How a machine takes its PCI functions' interrupts (D154): the line of each device's
/// INTx pin, which its firmware tables route there, and where the MSIs of the functions
/// the VMM emulates go.
#[cfg(hv)]
pub(crate) trait PciInterrupts {
    /// The INTx line of device `device` on bus 0.
    fn intx(&self, device: u8) -> Arc<dyn crate::devices::Interrupt>;
    fn msi(&self) -> Arc<dyn crate::devices::pci::Msi>;
    /// How a function passed through on device `device` interrupts the guest, by irqfds;
    /// none where the machine routes it no interrupt.
    #[cfg(target_os = "linux")]
    fn route(&self, device: u8) -> Option<Arc<dyn crate::devices::pci::vfio::Route>>;
}

/// A machine's PCI bus (D153, D154): each device that raises INTx, and its pin, for the
/// machine's firmware tables to route to [`PciInterrupts::intx`]'s line.
#[cfg(hv)]
#[derive(Debug, Default)]
pub(crate) struct PciBus {
    pub intx: Vec<(u8, u8)>,
}

/// What a machine gives its PCI bus: its VM, and its guest memory, which passed-through
/// functions reach by DMA; and how its functions' interrupts reach the guest, where it
/// routes them.
#[cfg(hv)]
pub(crate) struct PciMachine<'a> {
    #[cfg_attr(
        not(target_os = "linux"),
        expect(dead_code, reason = "read by VFIO, Linux's alone")
    )]
    pub vm: &'a crate::hv::Vm,
    #[cfg_attr(
        not(target_os = "linux"),
        expect(dead_code, reason = "read by VFIO, Linux's alone")
    )]
    pub memory: &'a crate::memory::GuestMemory,
    pub irqs: Option<&'a dyn PciInterrupts>,
}

/// Puts the PCI bus `pci` asks for on `bus` (D153, D154), where it asks for one: its ECAM
/// window at `ecam`, and its functions' BARs in the memory window `mem32`, each at the
/// next multiple of its size; QEMU's edu at 00:01.0 where `pci.test`, then each function
/// passed through, on the devices after. Their interrupts go where `machine.irqs` says;
/// with none, the functions the VMM emulates have none.
#[cfg(hv)]
fn pci_bus(
    bus: &mut crate::devices::MmioBus,
    ecam: u64,
    mem32: (u64, u64),
    pci: Pci<'_>,
    config: &crate::snapshot::MachineConfig,
    machine: PciMachine<'_>,
) -> Result<Option<PciBus>, String> {
    use crate::devices::pci;
    let irqs = machine.irqs;
    if !pci.test && pci.vfio.is_empty() {
        return Ok(None);
    }
    let mut routed = PciBus::default();
    let mut window = BarWindow {
        next: mem32.0,
        end: mem32
            .0
            .checked_add(mem32.1)
            .ok_or("the PCI window overflows the address space")?,
    };
    let mut functions: Vec<pci::Function> = Vec::new();
    let mut device: u8 = 1;
    if pci.test {
        let at = window.take(pci::edu::BAR_SIZE)?;
        let lines = irqs.map(|irqs| pci::Lines {
            intx: irqs.intx(device),
            msi: irqs.msi(),
        });
        if lines.is_some() {
            routed.intx.push((device, pci::edu::PIN));
        }
        let msix_at = window.take(pci::edu::MSIX_BAR_SIZE)?;
        let (edu, registers, msix) = pci::edu::Edu::new(at, msix_at, lines)?;
        bus.insert(at, pci::edu::BAR_SIZE, registers)?;
        bus.insert(msix_at, pci::edu::MSIX_BAR_SIZE, msix)?;
        functions.push((device << 3, edu));
        device += 1;
    }
    if !pci.vfio.is_empty() {
        if config.hotplug_mib > 0 || config.reporting {
            return Err("a VM given a host's PCI device has no memory to plug and no balloon: the device's DMA reaches guest RAM through mappings the host holds pinned (D154)".into());
        }
        functions.extend(passed_through(
            bus,
            &mut window,
            pci.vfio,
            device,
            &machine,
            &mut routed,
        )?);
    }
    bus.insert(ecam, pci::ECAM_SIZE, Arc::new(pci::Ecam::new(functions)))?;
    Ok(Some(routed))
}

/// What is left of a PCI memory window, BARs taken from its start.
#[cfg(hv)]
#[derive(Debug)]
struct BarWindow {
    next: u64,
    end: u64,
}

#[cfg(hv)]
impl BarWindow {
    /// The place of a BAR of `size` bytes, a power of two: the next multiple of its size
    /// (PCI Local Bus 3.0 §6.2.5.1).
    fn take(&mut self, size: u64) -> Result<u64, String> {
        let at = self
            .next
            .checked_next_multiple_of(size)
            .filter(|at| at.checked_add(size).is_some_and(|end| end <= self.end))
            .ok_or_else(|| {
                format!(
                    "a BAR of {size:#x} bytes: the PCI window has {:#x} left",
                    self.end.saturating_sub(self.next)
                )
            })?;
        self.next = at + size;
        Ok(at)
    }
}

/// Each function of `vfio` opened (its DMA mapped to the machine's memory), its BARs
/// placed in `window` and put on `bus`, on device `first` and those after it.
#[cfg(all(hv, target_os = "linux"))]
fn passed_through(
    bus: &mut crate::devices::MmioBus,
    window: &mut BarWindow,
    vfio: &[Vfio],
    first: u8,
    machine: &PciMachine<'_>,
    routed: &mut PciBus,
) -> Result<Vec<crate::devices::pci::Function>, String> {
    use crate::devices::pci::{self, vfio as v};
    let PciMachine { vm, memory, irqs } = *machine;
    let mut functions: Vec<pci::Function> = Vec::new();
    // One container for the functions whose groups have an IOMMU, and one for those
    // whose have none.
    let (mut iommu, mut noiommu) = (None, None);
    for (device, f) in (first..32).zip(vfio) {
        let slot = if f.noiommu { &mut noiommu } else { &mut iommu };
        let container = match slot {
            Some(c) => c,
            None => slot.insert(v::Container::open(f.noiommu).map_err(|e| format!("{}: {e}", f.name))?),
        };
        let opened = v::Device::open(&f.name, &f.group, container, memory, vm)?;
        let mut bars = Vec::new();
        for (index, size, kind) in opened.bars() {
            let addr = window
                .take(size)
                .map_err(|e| format!("{}: BAR {index}: {e}", f.name))?;
            bars.push(pci::Bar {
                index,
                addr,
                size,
                kind,
            });
        }
        let route = irqs.and_then(|irqs| irqs.route(device));
        let send = irqs.map(|irqs| irqs.msi());
        let function = Arc::new(v::VfioFunction::new(opened, bars, route, send, vm)?);
        for bar in function.bars() {
            bus.insert(
                bar.addr,
                bar.size,
                Arc::new(v::VfioBar::new(function.clone(), bar.index)),
            )?;
        }
        if function.pin() != 0 {
            routed.intx.push((device, function.pin()));
        }
        functions.push((device << 3, function));
    }
    if functions.len() < vfio.len() {
        return Err(format!(
            "{} PCI devices: bus 0 has room for {}",
            vfio.len(),
            32 - usize::from(first)
        ));
    }
    Ok(functions)
}

#[cfg(all(hv, not(target_os = "linux")))]
fn passed_through(
    _bus: &mut crate::devices::MmioBus,
    _window: &mut BarWindow,
    _vfio: &[Vfio],
    _first: u8,
    _machine: &PciMachine<'_>,
    _routed: &mut PciBus,
) -> Result<Vec<crate::devices::pci::Function>, String> {
    Err("a host's PCI devices are passed through by VFIO, Linux's (D154)".into())
}

/// KVM as the host of the functions VFIO passes through (D154): its VFIO device told of
/// their groups, and memory slots of each function's own for its BARs.
#[cfg(all(hv, target_os = "linux"))]
impl crate::devices::pci::vfio::Host for crate::hv::Vm {
    fn add_group(&self, group: std::os::fd::RawFd) -> Result<(), String> {
        self.add_vfio_group(group).map_err(|e| e.to_string())
    }

    fn slots(&self, count: u32) -> Box<dyn crate::devices::pci::vfio::BarSlots> {
        Box::new(self.device_slots(count))
    }
}

#[cfg(all(hv, target_os = "linux"))]
impl crate::devices::pci::vfio::BarSlots for crate::hv::DeviceSlots {
    unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize) -> Result<(), String> {
        // SAFETY: forwarded caller contract.
        unsafe { crate::hv::DeviceSlots::map(self, i, host, gpa, len) }.map_err(|e| e.to_string())
    }

    fn unmap(&self, i: u32) -> Result<(), String> {
        crate::hv::DeviceSlots::unmap(self, i).map_err(|e| e.to_string())
    }
}

/// The host side of a VM's virtio-vsock device.
#[derive(Debug, Clone, Default)]
pub struct VsockHost {
    /// A Unix socket host clients dial (`CONNECT <port>`), and the prefix of the sockets
    /// `<path>_<port>` that guest connections to other host ports reach, as Firecracker's.
    pub path: Option<PathBuf>,
    /// Host ports this process serves itself. A guest connection to one arrives on its
    /// sender as one end of a socket pair, with no socket file anyone else could reach,
    /// dial first or need to be granted (D30). Each takes the guest's first connection
    /// only.
    pub ports: Vec<(u32, std::sync::mpsc::Sender<crate::platform::UnixStream>)>,
    /// Host ports this process serves for every guest connection, as [`ports`]' each
    /// first: their owner tells its own connections from any other by what each says
    /// first (shards' exec sessions, which name a token the guest's init alone was sent).
    ///
    /// [`ports`]: VsockHost::ports
    pub every: Vec<(u32, std::sync::mpsc::Sender<crate::platform::UnixStream>)>,
}

impl VsockHost {
    /// A host side at `path` alone.
    pub fn at(path: PathBuf) -> VsockHost {
        VsockHost {
            path: Some(path),
            ports: Vec::new(),
            every: Vec::new(),
        }
    }
}

/// The memory a VM has unless it is given more or less, in MiB.
pub const MEMORY_MIB: u64 = 256;

impl Config {
    /// A VM with shards' defaults: 1 CPU, [`MEMORY_MIB`], and a console on stdout.
    pub fn new(kernel: PathBuf, init: Option<PathBuf>) -> Config {
        Config {
            kernel,
            initrd: None,
            init,
            cmdline: "console=ttyS0 earlycon panic=-1".into(),
            vcpus: 1,
            memory_mib: MEMORY_MIB,
            console: Console::Stdout,
            disks: Vec::new(),
            snapshot: None,
            pmem: Vec::new(),
            vsock: None,
            net: None,
            #[cfg(unix)]
            shares: Vec::new(),
            join: None,
            #[cfg(unix)]
            join_share: None,
            nested: false,
            hotplug_mib: 0,
            reporting: false,
            gpu: false,
            pci_test: false,
            vfio: Vec::new(),
        }
    }
}

/// Starts a VM from a snapshot instead of booting one.
#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// The snapshot directory.
    pub dir: PathBuf,
    pub console: Console,
    /// For snapshots the restored guest asks for.
    pub snapshot: Option<SnapshotPolicy>,
    /// Prepare everything, then wait for [`Handle::release`]: a warm VM whose start
    /// request costs only the release.
    pub hold: bool,
    /// This VM's vsock device's host side. A snapshot with a vsock device needs one: the
    /// original VM may still hold its own path.
    pub vsock: Option<VsockHost>,
    /// This VM's network process's side of its network device. A snapshot with one needs
    /// one: a network process serves one VM alone.
    pub net: Option<crate::devices::virtio::net::NetHost>,
    /// The slots of the snapshot's shared directories, one each (D38).
    #[cfg(unix)]
    pub shares: Vec<crate::devices::virtio::fs::Share>,
    /// The restored VM's own join disk, where the snapshot has one (D119): empty, as the
    /// snapshot's was.
    pub join: Option<&'static crate::devices::virtio::block::Join>,
    /// The join share's slot (D119), which a server of its VM's joiners' volumes fills.
    #[cfg(unix)]
    pub join_share: Option<&'static crate::devices::virtio::fs::Slot>,
    /// Prefetch the snapshot's working set, if it has one, before the guest runs: for a
    /// restore ahead of its request, which it moves off the request's path (PM M30).
    pub prefetch: bool,
    /// Record a working set for the snapshot, where it has none and this backend records
    /// restores' ([`RESTORES_RECORD`]), until [`Handle::take_working_set`] takes it.
    pub record: bool,
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    /// Where to write the snapshot when the guest asks for one.
    pub dir: PathBuf,
    pub then: AfterSnapshot,
    /// Once resumed from its snapshot, record the pages the guest touches, until
    /// [`Handle::take_working_set`] takes them, for the snapshot.
    pub working_set: bool,
}

/// What the VM does once its snapshot is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSnapshot {
    Resume,
    Stop,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stdout,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest powered off (PSCI SYSTEM_OFF on arm64).
    PowerOff,
    /// The guest requested a reset (Linux does on reboot and, with `panic=-1`, on panic).
    Reset,
    /// Stopped by the host.
    Stopped,
    /// Stopped after writing a snapshot ([`AfterSnapshot::Stop`]).
    Snapshotted,
    Error(String),
}

/// Boots a VM and waits for it to exit.
pub fn run(cfg: &Config) -> Result<ExitReason, String> {
    let (_, running) = start(cfg)?;
    Ok(running.wait())
}

/// The initrd a machine boots: `--initrd`'s file, or an initramfs holding `--init`'s. Its
/// file is read only once its length is seen to fit the `room` bytes of RAM there are for
/// it, and an init is read straight into its place in the archive (audit D13).
#[cfg(hv)]
fn initrd(cfg: &Config, room: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read as _;
    let open = |p: &PathBuf| -> Result<(std::fs::File, usize), String> {
        let at = |e: std::io::Error| format!("{}: {e}", p.display());
        let file = crate::platform::open_input(p, false).map_err(at)?;
        let len = file.metadata().map_err(at)?.len();
        let len = usize::try_from(len).map_err(|_| format!("{}: {len} bytes", p.display()))?;
        Ok((file, len))
    };
    let fits = |p: &PathBuf, len: usize| {
        if len as u64 > room {
            return Err(format!(
                "{}: {len} bytes, more than the {room} the guest's RAM has room for",
                p.display()
            ));
        }
        Ok(())
    };
    match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => Err("--initrd and --init are mutually exclusive".into()),
        (Some(p), None) => {
            let (file, len) = open(p)?;
            fits(p, len)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            // Only the length that was seen to fit, though the file grow meanwhile.
            file.take(len as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            if bytes.len() != len {
                return Err(format!("{}: shorter than its {len} bytes", p.display()));
            }
            Ok(Some(bytes))
        }
        (None, Some(p)) => {
            let (mut file, len) = open(p)?;
            fits(p, crate::initramfs::archive_len(len))?;
            crate::initramfs::with_init_from(&mut file, len)
                .map(Some)
                .map_err(|e| format!("{}: {e}", p.display()))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(all(test, hv))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hv::Touch;
    use crate::snapshot::MachineConfig;
    #[cfg(unix)]
    use crate::snapshot::Snapshot;

    /// BARs are placed from the window's start, each at the next multiple of its size,
    /// and one past the window's end is refused, the window as it was.
    #[test]
    fn bars_are_placed_at_multiples_of_their_sizes() {
        let mut w = BarWindow {
            next: 0x1000_0000,
            end: 0x1080_0000,
        };
        assert_eq!(w.take(0x1000).unwrap(), 0x1000_0000);
        assert_eq!(w.take(0x10_0000).unwrap(), 0x1010_0000);
        assert_eq!(w.take(0x10).unwrap(), 0x1020_0000);
        assert_eq!(w.take(0x20_0000).unwrap(), 0x1040_0000);
        // 4 MiB fits from 0x10800000 alone, past the end.
        assert!(w.take(0x40_0000).is_err(), "{w:?}");
        assert_eq!(w.next, 0x1060_0000);
        assert_eq!(w.take(0x20_0000).unwrap(), 0x1060_0000);
        assert!(w.take(0x10).is_err(), "{w:?}");
    }

    fn machine(vsock: bool, net: Option<[u8; 6]>) -> MachineConfig {
        MachineConfig {
            vcpus: 1,
            memory_mib: 16,
            disks: Vec::new(),
            pmem: Vec::new(),
            vsock,
            net,
            shares: 0,
            join: false,
            join_share: false,
            nested: false,
            hotplug_mib: 0,
            reporting: false,
            gpu: None,
        }
    }

    #[cfg(unix)]
    fn net_host() -> (crate::devices::virtio::net::NetHost, [std::os::fd::OwnedFd; 2]) {
        let (net_waits, device_rings) = shards_netring::doorbell().unwrap();
        let (device_waits, net_rings) = shards_netring::doorbell().unwrap();
        let host = crate::devices::virtio::net::NetHost {
            region: Arc::new(shards_netring::memory().unwrap()),
            wake_me: Arc::new(device_waits),
            wake_peer: Arc::new(device_rings),
            mac: [2, 0, 0, 0, 0, 9],
            flush: crate::devices::virtio::net::TxFlush::default(),
        };
        (host, [net_waits, net_rings])
    }

    /// A restore's host sides match its snapshot's devices: a vsock device and a socket
    /// for it, a network device and a network process for it, together or not at all.
    #[cfg(unix)]
    #[test]
    fn a_restores_hosts_match_its_snapshots_devices() {
        let vsock = VsockHost::at(PathBuf::from("/nowhere"));
        let (net, _ends) = net_host();
        let snap = |vsock, net| Snapshot {
            config: machine(vsock, net),
            arch: Vec::new(),
            devices: Vec::new(),
        };
        let hosts = |v: bool, n: bool| Hosts {
            vsock: v.then_some(&vsock),
            net: n.then_some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
        };
        let mac = Some([2, 0, 0, 0, 0, 1]);
        for (has_vsock, has_net) in [(false, false), (true, false), (false, true), (true, true)] {
            for (give_vsock, give_net) in [(false, false), (true, false), (false, true), (true, true)] {
                let checked = check_hosts(
                    &snap(has_vsock, has_net.then_some(mac).flatten()),
                    hosts(give_vsock, give_net),
                );
                let matched = has_vsock == give_vsock && has_net == give_net;
                assert_eq!(
                    checked.is_ok(),
                    matched,
                    "{has_vsock} {has_net} {give_vsock} {give_net}: {checked:?}"
                );
            }
        }
        let e = check_hosts(&snap(true, None), hosts(false, false)).unwrap_err();
        assert!(e.contains("--vsock PATH"), "{e}");
        let e = check_hosts(&snap(false, mac), hosts(false, false)).unwrap_err();
        assert!(e.contains("its own network process"), "{e}");
    }

    /// pmem regions go from the first address, each right after the one before; none
    /// leaves the first address their end; one past the address space is refused.
    #[test]
    fn pmem_regions_go_one_after_another() {
        let dir = shards_testdir::TempDir::new("pmem-regions").unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, vec![0u8; 4096]).unwrap();
        std::fs::write(&b, vec![0u8; 3 << 20]).unwrap();
        let mut config = machine(false, None);
        let (none, end) = pmem_regions(&config, 1 << 32).unwrap();
        assert!(none.is_empty());
        assert_eq!(end, 1 << 32);
        config.pmem = vec![a, b];
        let (regions, end) = pmem_regions(&config, 1 << 32).unwrap();
        let at: Vec<(u64, u64)> = regions.iter().map(|(r, gpa)| (*gpa, r.len() as u64)).collect();
        let (first, second) = (at[0], at[1]);
        assert_eq!(first.0, 1 << 32);
        assert_eq!(second.0, first.0 + first.1);
        assert_eq!(end, second.0 + second.1);
        assert!(first.1 >= 4096 && second.1 >= 3 << 20);
        let e = pmem_regions(&config, u64::MAX - 4096).unwrap_err();
        assert_eq!(e, "pmem regions overflow the address space");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Maps nothing: the order test's memory to plug is never plugged.
    #[cfg(unix)]
    #[derive(Debug, Clone)]
    struct Unmapped;

    #[cfg(unix)]
    impl mem::Mapper for Unmapped {
        unsafe fn map(&self, _host: *mut u8, _gpa: u64, _len: usize) -> Result<(), String> {
            Ok(())
        }

        fn unmap(&self, _gpa: u64, _len: usize) -> Result<(), String> {
            Ok(())
        }
    }

    /// A machine's virtio devices come in the guest's probe order, disks, pmem, vsock,
    /// network and memory to plug last, no more than the machine has room for; the network
    /// device takes the machine's own MAC; a device without its host side is refused.
    #[cfg(unix)]
    #[test]
    fn virtio_devices_come_in_probe_order() {
        let dir = shards_testdir::TempDir::new("virtio-devices").unwrap();
        let disk = dir.join("disk");
        std::fs::write(&disk, vec![0u8; 8 * 512]).unwrap();
        let image = dir.join("image");
        std::fs::write(&image, vec![0u8; 4096]).unwrap();
        let mut config = machine(true, Some([2, 0, 0, 0, 0, 1]));
        config.disks = vec![(disk.clone(), true), (disk, false)];
        config.pmem = vec![image];
        let (regions, _) = pmem_regions(&config, 1 << 32).unwrap();
        let vsock = VsockHost::at(dir.join("vsock"));
        let (net, _ends) = net_host();
        let hosts = Hosts {
            vsock: Some(&vsock),
            net: Some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
        };
        let e = virtio_devices(&config, &regions, hosts, Vec::new(), 4)
            .err()
            .unwrap();
        assert_eq!(e, "at most 4 virtio devices are supported");
        let plugging = || {
            let sizes = Arc::new(mem::Sizes::new(1 << 32, mem::ALIGN, mem::ALIGN));
            vec![Box::new(mem::Mem::new(sizes, Unmapped)) as Box<dyn crate::devices::virtio::VirtioDevice>]
        };
        let e = virtio_devices(&config, &regions, hosts, plugging(), 5)
            .err()
            .unwrap();
        assert_eq!(e, "at most 5 virtio devices are supported");
        let ids: Vec<u32> = virtio_devices(&config, &regions, hosts, plugging(), 6)
            .unwrap()
            .iter()
            .map(|d| d.device_id())
            .collect();
        assert_eq!(ids, [2, 2, 27, 19, 1, mem::DEVICE_ID]);
        let devices = virtio_devices(&config, &regions, hosts, Vec::new(), 5).unwrap();
        let ids: Vec<u32> = devices.iter().map(|d| d.device_id()).collect();
        assert_eq!(ids, [2, 2, 27, 19, 1]);
        let mut mac = [0u8; 6];
        devices[4].read_config(0, &mut mac);
        assert_eq!(
            mac,
            [2, 0, 0, 0, 0, 1],
            "the machine's MAC, not its network process's"
        );
        drop(devices);
        let missing = Hosts {
            vsock: None,
            net: Some(&net),
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
        };
        let e = virtio_devices(&config, &regions, missing, Vec::new(), 5)
            .err()
            .unwrap();
        assert!(e.contains("no host side"), "{e}");
        let missing = Hosts {
            vsock: Some(&vsock),
            net: None,
            shares: &[],
            join: None,
            #[cfg(unix)]
            join_share: None,
        };
        let e = virtio_devices(&config, &regions, missing, Vec::new(), 5)
            .err()
            .unwrap();
        assert!(e.contains("no network process"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Backing files are resolved when the machine is built, so a snapshot of it names
    /// them wherever it is restored from (audit A18). Tests run in their package's
    /// directory, which holds `Cargo.toml`.
    #[test]
    fn a_machines_backing_files_resolve_to_absolute_paths() {
        let mut cfg = Config::new(PathBuf::from("kernel"), None);
        cfg.disks = vec![Disk {
            path: PathBuf::from("Cargo.toml"),
            read_only: true,
        }];
        cfg.pmem = vec![PathBuf::from("./src/../Cargo.toml")];
        let machine = machine_config(&cfg).unwrap();
        let absolute = std::fs::canonicalize("Cargo.toml").unwrap();
        assert!(absolute.is_absolute());
        assert_eq!(machine.disks, vec![(absolute.clone(), true)]);
        assert_eq!(machine.pmem, vec![absolute]);
        cfg.pmem = vec![PathBuf::from("no-such-file")];
        assert!(machine_config(&cfg).unwrap_err().starts_with("no-such-file: "));
    }

    /// An init or initrd is read only if it fits where it goes: one longer than the room
    /// is refused, saying so; one that fits becomes the archive, or the initrd, whole
    /// (audit D13). `Cargo.toml` stands in for either.
    #[test]
    fn an_initrd_is_read_only_if_it_fits() {
        let len = std::fs::metadata("Cargo.toml").unwrap().len() as usize;
        let mut cfg = Config::new(PathBuf::from("kernel"), Some(PathBuf::from("Cargo.toml")));
        let archive = crate::initramfs::archive_len(len) as u64;
        let e = initrd(&cfg, archive - 1).unwrap_err();
        assert!(e.contains("room for"), "{e}");
        let built = initrd(&cfg, archive).unwrap().unwrap();
        assert_eq!(
            built,
            crate::initramfs::with_init(&std::fs::read("Cargo.toml").unwrap())
        );
        cfg.init = None;
        cfg.initrd = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, len as u64 - 1).unwrap_err().contains("room for"));
        assert_eq!(initrd(&cfg, len as u64).unwrap().unwrap().len(), len);
        cfg.init = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, u64::MAX).is_err(), "both at once");
    }

    /// A working set naming a page the guest does not have is not prefetched at all
    /// (audit A16).
    #[test]
    fn a_working_set_outside_the_guest_is_dropped() {
        let page = 0x4000;
        let ranges = [(0x8000_0000, 0x10_0000), (0x1_0000_0000, 0x20_0000)];
        let touch = |gpa| Touch { gpa, written: false };
        let inside = vec![touch(0x8000_0000), touch(0x800f_c000), touch(0x1_001f_c000)];
        assert_eq!(usable_working_set(inside.clone(), page, &ranges), inside);
        for outside in [0x8010_0000, 0x7fff_c000, 0x1_0020_0000, !(page - 1)] {
            let set = vec![touch(0x8000_0000), touch(outside)];
            assert!(usable_working_set(set, page, &ranges).is_empty(), "{outside:#x}");
        }
    }
}
