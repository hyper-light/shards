//! A host's PCI function, passed through by VFIO (Linux Documentation/driver-api/vfio.rst,
//! include/uapi/linux/vfio.h; D153): its group set in a container of its own, with the
//! type 1 IOMMU, all of guest RAM mapped for its DMA, or with none where the group is a
//! no-IOMMU one (`enable_unsafe_noiommu_mode`, for tests of functions that do no DMA);
//! its configuration space the device's, read and written through vfio-pci, which
//! already keeps the device's own registers from a guest, but for what the VMM keeps: its
//! BARs, its expansion ROM, its interrupt line and pin, and its MSI and MSI-X
//! capabilities; its memory BARs reached through the device's regions, but for its MSI-X
//! table and pending bits, the VMM's too; its interrupts signalled by VFIO on eventfds
//! that KVM's irqfds take into the guest (D154).

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, PoisonError};

use super::{Bar, BarKind, Header, Identity, Interrupts, Msi, MsiCap, MsixCap, MsixTable, PciFunction, reg};
use crate::devices::{MmioDevice, get_le};
use crate::memory::GuestMemory;
use crate::snapshot::codec::{self, Reader, Writer};

/// `_IO(';', 100 + n)`: VFIO's ioctls carry no size of their own.
const fn vfio_io(n: u64) -> u64 {
    ((b';' as u64) << 8) | (100 + n)
}

const VFIO_GET_API_VERSION: u64 = vfio_io(0);
const VFIO_CHECK_EXTENSION: u64 = vfio_io(1);
const VFIO_SET_IOMMU: u64 = vfio_io(2);
const VFIO_GROUP_GET_STATUS: u64 = vfio_io(3);
const VFIO_GROUP_SET_CONTAINER: u64 = vfio_io(4);
const VFIO_GROUP_GET_DEVICE_FD: u64 = vfio_io(6);
const VFIO_DEVICE_GET_INFO: u64 = vfio_io(7);
const VFIO_DEVICE_GET_REGION_INFO: u64 = vfio_io(8);
const VFIO_DEVICE_GET_IRQ_INFO: u64 = vfio_io(9);
const VFIO_DEVICE_SET_IRQS: u64 = vfio_io(10);
const VFIO_DEVICE_RESET: u64 = vfio_io(11);
const VFIO_IOMMU_MAP_DMA: u64 = vfio_io(13);
const VFIO_IOMMU_UNMAP_DMA: u64 = vfio_io(14);

/// Every VFIO ioctl request shards makes: what its seccomp filter allows.
pub const IOCTLS: &[u64] = &[
    VFIO_GET_API_VERSION,
    VFIO_CHECK_EXTENSION,
    VFIO_SET_IOMMU,
    VFIO_GROUP_GET_STATUS,
    VFIO_GROUP_SET_CONTAINER,
    VFIO_GROUP_GET_DEVICE_FD,
    VFIO_DEVICE_GET_INFO,
    VFIO_DEVICE_GET_REGION_INFO,
    VFIO_DEVICE_GET_IRQ_INFO,
    VFIO_DEVICE_SET_IRQS,
    VFIO_DEVICE_RESET,
    VFIO_IOMMU_MAP_DMA,
    VFIO_IOMMU_UNMAP_DMA,
];

const VFIO_API_VERSION: libc::c_int = 0;
const VFIO_TYPE1V2_IOMMU: u64 = 3;
const VFIO_NOIOMMU_IOMMU: u64 = 8;
const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
const VFIO_DEVICE_FLAGS_RESET: u32 = 1 << 0;
const VFIO_DEVICE_FLAGS_PCI: u32 = 1 << 1;
const VFIO_DMA_MAP_FLAG_READ: u32 = 1 << 0;
const VFIO_DMA_MAP_FLAG_WRITE: u32 = 1 << 1;
const VFIO_REGION_INFO_FLAG_READ: u32 = 1 << 0;
const VFIO_REGION_INFO_FLAG_WRITE: u32 = 1 << 1;
const VFIO_REGION_INFO_FLAG_MMAP: u32 = 1 << 2;
/// The configuration space's region (VFIO_PCI_CONFIG_REGION_INDEX).
const CONFIG_REGION: u32 = 7;
/// A function's interrupts as VFIO indexes them (VFIO_PCI_INTX_IRQ_INDEX, _MSI_, _MSIX_),
/// and how they are set (struct vfio_irq_set's flags) and signalled (vfio_irq_info's).
const IRQ_INTX: u32 = 0;
const IRQ_MSI: u32 = 1;
const IRQ_MSIX: u32 = 2;
const VFIO_IRQ_INFO_EVENTFD: u32 = 1 << 0;
const VFIO_IRQ_INFO_NORESIZE: u32 = 1 << 3;
const VFIO_IRQ_SET_DATA_NONE: u32 = 1 << 0;
const VFIO_IRQ_SET_DATA_EVENTFD: u32 = 1 << 2;
const VFIO_IRQ_SET_ACTION_UNMASK: u32 = 1 << 4;
const VFIO_IRQ_SET_ACTION_TRIGGER: u32 = 1 << 5;

#[repr(C)]
#[derive(Default)]
struct IrqInfo {
    argsz: u32,
    flags: u32,
    index: u32,
    count: u32,
}

#[repr(C)]
#[derive(Default)]
struct GroupStatus {
    argsz: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct DeviceInfo {
    argsz: u32,
    flags: u32,
    num_regions: u32,
    num_irqs: u32,
    cap_offset: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RegionInfo {
    argsz: u32,
    flags: u32,
    index: u32,
    cap_offset: u32,
    size: u64,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct DmaMap {
    argsz: u32,
    flags: u32,
    vaddr: u64,
    iova: u64,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct DmaUnmap {
    argsz: u32,
    flags: u32,
    iova: u64,
    size: u64,
}

/// ioctl(2) of `request` with `arg`, its result where it did not fail.
///
/// # Safety
/// `arg` is what `request` reads and writes, valid for it.
unsafe fn ioctl(fd: &impl std::os::fd::AsRawFd, request: u64, arg: usize) -> io::Result<libc::c_int> {
    // SAFETY: as the caller vouches.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r)
}

/// A host's PCI function, as VFIO hands it to this process.
#[derive(Debug)]
pub struct Device {
    /// The host's name of it (`0000:01:00.0`).
    pub name: String,
    /// Its descriptor, whose regions are reached at their offsets.
    file: File,
    regions: Vec<RegionInfo>,
    /// Its group and their container, held open as long as it is (vfio.rst).
    _group: Arc<OwnedFd>,
    _container: Arc<OwnedFd>,
}

/// VFIO's container of a VM's passed-through functions (vfio.rst, "VFIO Usage
/// Example"): the IOMMU their groups share, each group set in it once, however many of
/// its functions the VM is given, and guest RAM mapped for their DMA once, where the IOMMU
/// is type 1. A no-IOMMU group shares no container with groups that have an IOMMU.
#[derive(Debug)]
pub struct Container {
    fd: Arc<OwnedFd>,
    noiommu: bool,
    groups: Vec<(PathBuf, Arc<OwnedFd>)>,
    /// Whether its first group maps guest RAM: not where what it maps follows a function's
    /// endpoint behind the guest's own IOMMU ([`Container::followed`]).
    maps_ram: bool,
}

impl Container {
    pub fn open(noiommu: bool) -> Result<Container, String> {
        let err = |what: &str, e: io::Error| format!("VFIO's {what}: {e}");
        let fd = open(Path::new("/dev/vfio/vfio")).map_err(|e| err("/dev/vfio/vfio", e))?;
        // SAFETY: VFIO_GET_API_VERSION takes no argument.
        let version = unsafe { ioctl(&fd, VFIO_GET_API_VERSION, 0) }.map_err(|e| err("version", e))?;
        if version != VFIO_API_VERSION {
            return Err(format!(
                "VFIO API {version}, where shards speaks {VFIO_API_VERSION}"
            ));
        }
        // SAFETY: VFIO_CHECK_EXTENSION takes an integer.
        if unsafe { ioctl(&fd, VFIO_CHECK_EXTENSION, Self::iommu(noiommu) as usize) }
            .map_err(|e| err("IOMMU", e))?
            <= 0
        {
            return Err(match noiommu {
                true => "this host's VFIO has no no-IOMMU mode (vfio.enable_unsafe_noiommu_mode)".into(),
                false => "this host's VFIO has no type 1 IOMMU".into(),
            });
        }
        Ok(Container {
            fd: Arc::new(fd),
            noiommu,
            groups: Vec::new(),
            maps_ram: true,
        })
    }

    /// A container of its own for a group whose function sits behind the guest's own
    /// IOMMU (D154): what it maps follows the function's endpoint there ([`Followed`]), as
    /// a type 1 container maps one set of I/O addresses for all of its groups (Cloud
    /// Hypervisor's device_manager.rs, add_vfio_device: a VfioOps of its own for each
    /// device attached to its virtio-iommu, "a limitation of the legacy VFIO interface").
    pub fn followed() -> Result<Container, String> {
        Ok(Container {
            maps_ram: false,
            ..Container::open(false)?
        })
    }

    /// What follows the endpoint of this container's function into its mappings, of guest
    /// RAM `memory`.
    pub fn follower(&self, memory: Arc<GuestMemory>) -> Followed {
        Followed {
            fd: self.fd.clone(),
            memory,
            mapped: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn iommu(noiommu: bool) -> u64 {
        if noiommu {
            VFIO_NOIOMMU_IOMMU
        } else {
            VFIO_TYPE1V2_IOMMU
        }
    }

    /// The group at `path`, set in this container and told of to `host`'s hypervisor: the
    /// first sets its IOMMU and maps guest RAM, `memory`, for their DMA; a group set
    /// already is the same.
    fn group(&mut self, path: &Path, memory: &GuestMemory, host: &dyn Host) -> Result<Arc<OwnedFd>, String> {
        if let Some((_, fd)) = self.groups.iter().find(|(p, _)| p == path) {
            return Ok(fd.clone());
        }
        let err = |what: &str, e: io::Error| format!("{}: {what}: {e}", path.display());
        let group = open(path).map_err(|e| err("opening it", e))?;
        host.add_group(group.as_raw_fd())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut status = GroupStatus {
            argsz: size_of::<GroupStatus>() as u32,
            flags: 0,
        };
        // SAFETY: VFIO_GROUP_GET_STATUS fills a `vfio_group_status` it is given.
        unsafe { ioctl(&group, VFIO_GROUP_GET_STATUS, (&raw mut status) as usize) }
            .map_err(|e| err("its status", e))?;
        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            return Err(format!(
                "{}: the IOMMU group is not viable: each of its functions must be bound to vfio-pci, or to no driver",
                path.display()
            ));
        }
        let container = self.fd.as_raw_fd();
        // SAFETY: VFIO_GROUP_SET_CONTAINER reads the container's descriptor it points at.
        unsafe { ioctl(&group, VFIO_GROUP_SET_CONTAINER, (&raw const container) as usize) }
            .map_err(|e| err("setting its container", e))?;
        if self.groups.is_empty() {
            // SAFETY: VFIO_SET_IOMMU takes an integer.
            unsafe { ioctl(&*self.fd, VFIO_SET_IOMMU, Self::iommu(self.noiommu) as usize) }.map_err(|e| {
                // Type 1 refuses a group whose MSIs nothing isolates (vfio_iommu_type1.c,
                // vfio_iommu_type1_attach_group), the host's administrator's to allow.
                let hint = match e.raw_os_error() {
                    Some(libc::EPERM) => ": the host's IOMMU does not isolate its functions' MSIs (no interrupt remapping), which the host's VFIO then refuses, unless its administrator allows it (vfio_iommu_type1.allow_unsafe_interrupts=1)",
                    _ => "",
                };
                format!("{}: setting its IOMMU: {e}{hint}", path.display())
            })?;
            if !self.noiommu && self.maps_ram {
                self.map(memory)?;
            }
        }
        let group = Arc::new(group);
        self.groups.push((path.to_path_buf(), group.clone()));
        Ok(group)
    }

    /// Maps all of guest RAM for the DMA of the functions in this container, at the
    /// addresses the guest knows it by: type 1 pins it, against the process's
    /// locked-memory limit.
    fn map(&self, memory: &GuestMemory) -> Result<(), String> {
        for (gpa, host, len) in memory.regions() {
            let map = DmaMap {
                argsz: size_of::<DmaMap>() as u32,
                flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
                vaddr: host as u64,
                iova: gpa,
                size: len as u64,
            };
            // SAFETY: VFIO_IOMMU_MAP_DMA reads a `vfio_iommu_type1_dma_map`; the memory it
            // maps is guest RAM, this process's as long as the machine is.
            if let Err(e) = unsafe { ioctl(&*self.fd, VFIO_IOMMU_MAP_DMA, (&raw const map) as usize) } {
                let hint = match e.raw_os_error() {
                    Some(libc::ENOMEM) => format!(
                        ": VFIO pins guest RAM for a device's DMA, against this process's locked-memory limit (RLIMIT_MEMLOCK, `ulimit -l`), {}",
                        memlock()
                    ),
                    _ => String::new(),
                };
                return Err(format!("mapping guest RAM {gpa:#x}+{len:#x} for DMA: {e}{hint}"));
            }
        }
        Ok(())
    }
}

/// A followed container's mappings (D154): what the guest's IOMMU shows its function's
/// endpoint ([`crate::devices::virtio::iommu::Follower`]), by type 1's map and unmap of
/// I/O addresses. Each mapping of guest RAM pins it, against the process's locked-memory
/// limit, as [`Container`]'s whole-RAM mapping does, and guest memory keeps what it pins
/// from being given back under the device ([`GuestMemory::pin`]).
#[derive(Debug)]
pub struct Followed {
    fd: Arc<OwnedFd>,
    memory: Arc<GuestMemory>,
    /// What it maps now: each mapping by its I/O address, its guest-physical address and
    /// length.
    mapped: Mutex<std::collections::BTreeMap<u64, (u64, u64)>>,
}

impl Followed {
    fn mapped(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<u64, (u64, u64)>> {
        self.mapped.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn dma_map(&self, iova: u64, host: u64, len: u64, flags: u32) -> io::Result<()> {
        let map = DmaMap {
            argsz: size_of::<DmaMap>() as u32,
            flags,
            vaddr: host,
            iova,
            size: len,
        };
        // SAFETY: VFIO_IOMMU_MAP_DMA reads a `vfio_iommu_type1_dma_map`; what it maps is
        // guest RAM, this process's as long as the machine is.
        unsafe { ioctl(&*self.fd, VFIO_IOMMU_MAP_DMA, (&raw const map) as usize) }.map(drop)
    }

    fn dma_unmap(&self, iova: u64, len: u64) -> io::Result<()> {
        let mut unmap = DmaUnmap {
            argsz: size_of::<DmaUnmap>() as u32,
            flags: 0,
            iova,
            size: len,
        };
        // SAFETY: VFIO_IOMMU_UNMAP_DMA reads and updates a `vfio_iommu_type1_dma_unmap`.
        unsafe { ioctl(&*self.fd, VFIO_IOMMU_UNMAP_DMA, (&raw mut unmap) as usize) }.map(drop)
    }
}

impl crate::devices::virtio::iommu::Follower for Followed {
    fn identity(&self) -> io::Result<()> {
        self.clear()?;
        let mut mapped = self.mapped();
        for (gpa, host, len) in self.memory.regions() {
            let len = len as u64;
            self.dma_map(
                gpa,
                host as u64,
                len,
                VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            )?;
            self.memory.pin(gpa, len);
            mapped.insert(gpa, (gpa, len));
        }
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        let mut mapped = self.mapped();
        while let Some((&iova, &(gpa, len))) = mapped.first_key_value() {
            self.dma_unmap(iova, len)?;
            self.memory.unpin(gpa, len);
            mapped.remove(&iova);
        }
        Ok(())
    }

    fn map(&self, iova: u64, gpa: u64, len: u64, read: bool, write: bool) -> io::Result<()> {
        let not_ram = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{gpa:#x}+{len:#x} is not guest RAM"),
            )
        };
        let size = usize::try_from(len).map_err(|_| not_ram())?;
        let host = self.memory.host_ptr(gpa, size).map_err(|_| not_ram())?;
        let flags =
            if read { VFIO_DMA_MAP_FLAG_READ } else { 0 } | if write { VFIO_DMA_MAP_FLAG_WRITE } else { 0 };
        let mut mapped = self.mapped();
        self.dma_map(iova, host as u64, len, flags)?;
        self.memory.pin(gpa, len);
        mapped.insert(iova, (gpa, len));
        Ok(())
    }

    fn unmap(&self, iova: u64, len: u64) -> io::Result<()> {
        let mut mapped = self.mapped();
        self.dma_unmap(iova, len)?;
        if let Some((gpa, len)) = mapped.remove(&iova) {
            self.memory.unpin(gpa, len);
        }
        Ok(())
    }
}

/// A passed-through function's interrupts as they go behind the guest's own IOMMU (D154):
/// an MSI it aims at an I/O address lands where its endpoint's domain maps that address
/// for a write, or in the MSI window, untranslated
/// ([`crate::devices::virtio::iommu::Translation::msi_target`]); where its domain maps
/// nothing there, the write faults and goes nowhere. Each route's removal names where it
/// was routed, whatever the domain maps by then.
#[derive(Debug)]
pub struct Translated {
    translation: crate::devices::virtio::iommu::Shared,
    endpoint: u32,
    route: Option<Arc<dyn Route>>,
    send: Option<Arc<dyn Msi>>,
    routed: Mutex<std::collections::BTreeMap<RawFd, u64>>,
}

impl Translated {
    pub fn new(
        translation: crate::devices::virtio::iommu::Shared,
        endpoint: u32,
        route: Option<Arc<dyn Route>>,
        send: Option<Arc<dyn Msi>>,
    ) -> Translated {
        Translated {
            translation,
            endpoint,
            route,
            send,
            routed: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn target(&self, address: u64) -> Option<u64> {
        self.translation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .msi_target(self.endpoint, address)
    }
}

impl Route for Translated {
    fn intx(&self, trigger: RawFd, resample: RawFd) -> Result<(), String> {
        self.route.as_ref().ok_or("no route")?.intx(trigger, resample)
    }

    fn intx_off(&self, trigger: RawFd) -> Result<(), String> {
        self.route.as_ref().ok_or("no route")?.intx_off(trigger)
    }

    fn msi(&self, event: RawFd, address: u64, data: u32) -> Result<(), String> {
        let route = self.route.as_ref().ok_or("no route")?;
        let to = self
            .target(address)
            .ok_or_else(|| format!("an MSI to {address:#x}, which its domain does not map"))?;
        route.msi(event, to, data)?;
        self.routed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(event, to);
        Ok(())
    }

    fn msi_off(&self, event: RawFd, address: u64, data: u32) -> Result<(), String> {
        let route = self.route.as_ref().ok_or("no route")?;
        let routed = self
            .routed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&event);
        match routed {
            Some(to) => route.msi_off(event, to, data),
            None => Err(format!("no MSI to {address:#x} routed by that eventfd")),
        }
    }
}

impl Msi for Translated {
    fn send(&self, address: u64, data: u32) {
        match (self.target(address), &self.send) {
            (Some(to), Some(send)) => send.send(to, data),
            _ => crate::debug!("an MSI to {address:#x}, which its domain does not map"),
        }
    }
}

/// The process's locked-memory limit, as `ulimit -l` would put it.
fn memlock() -> String {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) fills the rlimit it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut limit) } != 0 {
        return "which could not be read".into();
    }
    match limit.rlim_cur {
        libc::RLIM_INFINITY => "unlimited".into(),
        bytes => format!("{} KiB", bytes / 1024),
    }
}

impl std::fmt::Debug for RegionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "region {} {:#x}@{:#x}", self.index, self.size, self.offset)
    }
}

/// The VFIO group of the host's function `name`: `/dev/vfio/N`, or `/dev/vfio/noiommu-N`
/// for a no-IOMMU group, N its IOMMU group's number (/sys/bus/pci/devices/NAME/iommu_group).
/// Its errors say what of that is missing; the caller names the function.
pub fn group_of(name: &str) -> io::Result<(PathBuf, bool)> {
    let link = std::fs::read_link(format!("/sys/bus/pci/devices/{name}/iommu_group")).map_err(|e| match e
        .kind()
    {
        io::ErrorKind::NotFound => io::Error::new(
            e.kind(),
            "no such PCI function in an IOMMU group: a host's function is passed through bound to vfio-pci, \
                 its group a no-IOMMU one where the host has no IOMMU (vfio.enable_unsafe_noiommu_mode)",
        ),
        _ => e,
    })?;
    let number = link
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("an IOMMU group without a number"))?
        .to_string();
    let plain = PathBuf::from(format!("/dev/vfio/{number}"));
    if plain.exists() {
        return Ok((plain, false));
    }
    let noiommu = PathBuf::from(format!("/dev/vfio/noiommu-{number}"));
    if noiommu.exists() {
        return Ok((noiommu, true));
    }
    Err(io::Error::other(format!(
        "no VFIO group {number}: is the function bound to vfio-pci?"
    )))
}

/// The sizes of function `name`'s 64-bit memory BARs, in their order, as the host's PCI
/// core gives them (sysfs `resource`: each BAR's start, end and flags, IORESOURCE_MEM and
/// IORESOURCE_MEM_64; drivers/pci/pci-sysfs.c, resource_show): what the machine's 64-bit
/// window holds, known before its VM is.
pub fn bars64_of(name: &str) -> io::Result<Vec<u64>> {
    const MEM: u64 = 0x0000_0200;
    const MEM_64: u64 = 0x0010_0000;
    let path = format!("/sys/bus/pci/devices/{name}/resource");
    let text = std::fs::read_to_string(&path)?;
    let mut sizes = Vec::new();
    // The six BARs come first; a 64-bit BAR's second register is a line of zeros.
    for line in text.lines().take(6) {
        let mut fields = line
            .split_whitespace()
            .map(|f| u64::from_str_radix(f.trim_start_matches("0x"), 16));
        let (Some(Ok(start)), Some(Ok(end)), Some(Ok(flags))) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(io::Error::other(format!("{path}: {line:?}")));
        };
        if flags & MEM != 0 && flags & MEM_64 != 0 && end > start {
            sizes.push((end - start + 1).next_power_of_two());
        }
    }
    Ok(sizes)
}

fn open(path: &Path) -> io::Result<OwnedFd> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    // SAFETY: open(2) of a NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Device {
    /// The host's function `name`, from its group `group`, set in `container`; reset where
    /// it can be.
    pub fn open(
        name: &str,
        group: &Path,
        container: &mut Container,
        memory: &GuestMemory,
        host: &dyn Host,
    ) -> Result<Device, String> {
        let err = |what: &str, e: io::Error| format!("{name}: {what}: {e}");
        let group = container
            .group(group, memory, host)
            .map_err(|e| format!("{name}: {e}"))?;
        let cname = CString::new(name).map_err(|_| format!("{name}: a NUL in its name"))?;
        // SAFETY: VFIO_GROUP_GET_DEVICE_FD reads a NUL-terminated device name.
        let fd = unsafe { ioctl(&*group, VFIO_GROUP_GET_DEVICE_FD, cname.as_ptr() as usize) }
            .map_err(|e| err("its device", e))?;
        // SAFETY: the descriptor the ioctl just returned, ours alone.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let mut info = DeviceInfo {
            argsz: size_of::<DeviceInfo>() as u32,
            ..DeviceInfo::default()
        };
        // SAFETY: VFIO_DEVICE_GET_INFO fills a `vfio_device_info` it is given.
        unsafe { ioctl(&file, VFIO_DEVICE_GET_INFO, (&raw mut info) as usize) }
            .map_err(|e| err("its information", e))?;
        if info.flags & VFIO_DEVICE_FLAGS_PCI == 0 {
            return Err(format!("{name}: not a PCI device"));
        }
        let mut regions = Vec::new();
        for index in 0..info.num_regions.min(CONFIG_REGION + 1) {
            let mut region = RegionInfo {
                argsz: size_of::<RegionInfo>() as u32,
                index,
                ..RegionInfo::default()
            };
            // SAFETY: VFIO_DEVICE_GET_REGION_INFO fills a `vfio_region_info` it is given.
            unsafe { ioctl(&file, VFIO_DEVICE_GET_REGION_INFO, (&raw mut region) as usize) }
                .map_err(|e| err(&format!("its region {index}"), e))?;
            regions.push(region);
        }
        if regions.get(CONFIG_REGION as usize).is_none_or(|r| r.size < 256) {
            return Err(format!("{name}: no configuration space"));
        }
        if info.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
            // SAFETY: VFIO_DEVICE_RESET takes no argument.
            unsafe { ioctl(&file, VFIO_DEVICE_RESET, 0) }.map_err(|e| err("resetting it", e))?;
        }
        Ok(Device {
            name: name.to_string(),
            file,
            regions,
            _group: group,
            _container: container.fd.clone(),
        })
    }

    /// Region `index`'s size and offset, where it is one the device reads and writes.
    fn region(&self, index: usize) -> Option<(u64, u64)> {
        self.regions
            .get(index)
            .filter(|r| {
                r.size > 0 && r.flags & (VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE) != 0
            })
            .map(|r| (r.size, r.offset))
    }

    /// Region `index` mapped into this process, where vfio-pci lets it be
    /// (VFIO_REGION_INFO_FLAG_MMAP: a memory BAR of a page or more, or alone in its page;
    /// vfio_pci_core.c, vfio_pci_probe_mmaps).
    fn map(&self, index: usize) -> Option<io::Result<Mapping>> {
        let region = self.regions.get(index)?;
        if region.flags & VFIO_REGION_INFO_FLAG_MMAP == 0 || region.size == 0 {
            return None;
        }
        let (Ok(len), Ok(offset)) = (usize::try_from(region.size), libc::off_t::try_from(region.offset))
        else {
            return Some(Err(io::Error::other("a region past this process's addresses")));
        };
        // SAFETY: a new shared mapping of the device's region, unmapped as `Mapping` drops.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                offset,
            )
        };
        if host == libc::MAP_FAILED {
            return Some(Err(io::Error::last_os_error()));
        }
        Some(Ok(Mapping {
            host: host as usize,
            len,
        }))
    }

    /// Reads `data` at `offset` of region `index`; all ones where it cannot.
    fn read(&self, index: usize, offset: u64, data: &mut [u8]) {
        let ok = self.region(index).is_some_and(|(size, at)| {
            offset
                .checked_add(data.len() as u64)
                .is_some_and(|end| end <= size)
                && self.file.read_exact_at(data, at + offset).is_ok()
        });
        if !ok {
            data.fill(0xff);
        }
    }

    /// Writes `data` at `offset` of region `index`, where it can.
    fn write(&self, index: usize, offset: u64, data: &[u8]) {
        if let Some((size, at)) = self.region(index)
            && offset
                .checked_add(data.len() as u64)
                .is_some_and(|end| end <= size)
        {
            let _ = self.file.write_all_at(data, at + offset);
        }
    }

    /// How VFIO signals the function's interrupt `index`, its flags, where it signals it
    /// by eventfd at all.
    fn irq_flags(&self, index: u32) -> Option<u32> {
        let mut info = IrqInfo {
            argsz: size_of::<IrqInfo>() as u32,
            index,
            ..IrqInfo::default()
        };
        // SAFETY: VFIO_DEVICE_GET_IRQ_INFO fills a `vfio_irq_info` it is given.
        unsafe { ioctl(&self.file, VFIO_DEVICE_GET_IRQ_INFO, (&raw mut info) as usize) }.ok()?;
        (info.flags & VFIO_IRQ_INFO_EVENTFD != 0 && info.count > 0).then_some(info.flags)
    }

    fn signals(&self, index: u32) -> bool {
        self.irq_flags(index).is_some()
    }

    /// A `vfio_irq_set`: `flags` for interrupts `start..start + count` of `index`, `events`
    /// its data. Where it enables MSI or MSI-X with more vectors than the host gives, VFIO
    /// enables none and answers how many it could (vfio_pci_intrs.c, vfio_msi_enable).
    fn set_irqs(&self, index: u32, flags: u32, start: u32, count: u32, events: &[RawFd]) -> io::Result<()> {
        let mut set: Vec<u32> = vec![(20 + 4 * events.len()) as u32, flags, index, start, count];
        set.extend(events.iter().map(|&fd| fd as u32));
        // SAFETY: a vfio_irq_set whose argsz covers the eventfds after it.
        match unsafe { ioctl(&self.file, VFIO_DEVICE_SET_IRQS, set.as_ptr() as usize) }? {
            0 => Ok(()),
            could => Err(io::Error::other(format!(
                "the host has {could} of the {count} vectors asked"
            ))),
        }
    }

    /// Signals the function's interrupt `index` on `event` from now; with none, no more.
    fn trigger(&self, index: u32, event: Option<RawFd>) -> io::Result<()> {
        match event {
            Some(fd) => self.set_irqs(
                index,
                VFIO_IRQ_SET_ACTION_TRIGGER | VFIO_IRQ_SET_DATA_EVENTFD,
                0,
                1,
                &[fd],
            ),
            None => self.set_irqs(
                index,
                VFIO_IRQ_SET_ACTION_TRIGGER | VFIO_IRQ_SET_DATA_NONE,
                0,
                0,
                &[],
            ),
        }
    }

    /// Signals MSI-X vectors `start..` on `events`, -1 for one signalled on nothing.
    fn trigger_msix(&self, start: u32, events: &[RawFd]) -> io::Result<()> {
        self.set_irqs(
            IRQ_MSIX,
            VFIO_IRQ_SET_ACTION_TRIGGER | VFIO_IRQ_SET_DATA_EVENTFD,
            start,
            events.len() as u32,
            events,
        )
    }

    /// Unmasks INTx, which VFIO masks as it signals it, each time `event` is written.
    fn unmask_intx_on(&self, event: RawFd) -> io::Result<()> {
        self.set_irqs(
            IRQ_INTX,
            VFIO_IRQ_SET_ACTION_UNMASK | VFIO_IRQ_SET_DATA_EVENTFD,
            0,
            1,
            &[event],
        )
    }

    /// The function's own configuration register at `offset`, of `len` bytes (at most 4),
    /// as vfio-pci gives it.
    fn config(&self, offset: u16, len: usize) -> u32 {
        let mut raw = [0u8; 4];
        let bytes = raw.get_mut(..len.min(4)).unwrap_or_default();
        self.read(CONFIG_REGION as usize, u64::from(offset), bytes);
        get_le(bytes) as u32
    }

    /// Where its capability `id` is, found in its list (§6.7).
    fn cap(&self, id: u8) -> Option<u16> {
        if self.config(reg::STATUS, 2) as u16 & reg::STATUS_CAPABILITIES == 0 {
            return None;
        }
        let mut at = self.config(reg::CAPABILITIES, 1) as u16 & 0xfc;
        // The list lives in the header's 192 bytes past its first 64: 48 entries at most.
        for _ in 0..48 {
            if at < 0x40 {
                return None;
            }
            if self.config(at, 1) as u8 == id {
                return Some(at);
            }
            at = self.config(at + 1, 1) as u16 & 0xfc;
        }
        None
    }

    /// Its MSI capability (§6.8.1): where it is, the next, its addresses' width, and its
    /// length, with per-vector masking's registers where it has them.
    fn msi_cap(&self) -> Option<(MsiCap, u16)> {
        let at = self.cap(reg::CAP_MSI)?;
        let next = self.config(at + 1, 1) as u8 & 0xfc;
        let control = self.config(at + reg::MSI_CONTROL, 2) as u16;
        let is64 = control & reg::MSI_64BIT != 0;
        let masking = control & reg::MSI_MASKING != 0;
        let len = 10 + if is64 { 4 } else { 0 } + if masking { 10 } else { 0 };
        Some((MsiCap { at, next, is64 }, len))
    }

    /// Its MSI-X capability (§6.8.2): where it is, the next, its table's size, and its
    /// table's and pending bits' BARs and offsets.
    fn msix_cap(&self) -> Option<MsixCap> {
        let at = self.cap(reg::CAP_MSIX)?;
        let next = self.config(at + 1, 1) as u8 & 0xfc;
        let control = self.config(at + reg::MSIX_CONTROL, 2) as u16;
        let place = |register: u16| {
            let value = self.config(at + register, 4);
            ((value & 7) as u8, value & !7)
        };
        Some(MsixCap {
            at,
            next,
            size: (control & 0x7ff) + 1,
            table: place(reg::MSIX_TABLE),
            pba: place(reg::MSIX_PBA),
        })
    }

    /// The BARs the device has, by their sizes (its regions) and their kinds (its
    /// configuration space's BAR registers): a 64-bit BAR takes two registers, and an I/O
    /// BAR is left out, as this machine has no I/O window.
    pub fn bars(&self) -> Vec<(usize, u64, BarKind)> {
        let mut out = Vec::new();
        let mut index = 0;
        while index < 6 {
            let mut raw = [0u8; 4];
            self.read(
                CONFIG_REGION as usize,
                u64::from(reg::BAR0) + 4 * index as u64,
                &mut raw,
            );
            let raw = u32::from_le_bytes(raw);
            let kind = match (raw & 1, (raw >> 1) & 3) {
                (1, _) => BarKind::Io,
                (_, 2) => BarKind::Memory64,
                _ => BarKind::Memory32,
            };
            if let Some((size, _)) = self.region(index)
                && kind != BarKind::Io
            {
                out.push((index, size.next_power_of_two().max(16), kind));
            }
            index += if kind == BarKind::Memory64 { 2 } else { 1 };
        }
        out
    }
}

/// `data`, read at `offset` of a configuration, with each of `overlays`' bits in place of
/// the device's (a register's offset, the bits it reads, their mask).
fn overlay(overlays: &[(u16, u32, u32)], offset: u16, data: &mut [u8]) {
    for (at, byte) in (offset..).zip(data.iter_mut()) {
        let shift = u32::from(at & 3) * 8;
        for &(register, value, mask) in overlays {
            if register == at & !3 {
                let mask = (mask >> shift) as u8;
                *byte = (*byte & !mask) | ((value >> shift) as u8 & mask);
            }
        }
    }
}

/// The overlay that takes extended capability `id` out of the list a configuration whose
/// registers `read` gives holds from 0x100 (PCIe 6.0 §7.6.3: each header's ID in its bits
/// 15:0, the next one's offset in 31:20, none past one of 0): its predecessor's header
/// made to point past it, or, where it is the first, a header of ID 0 there in its place,
/// which a guest passes over (Linux's pci_find_next_ext_capability; as QEMU's vfio-pci
/// hides one, vfio_add_ext_cap). None where the list does not hold it.
fn ext_cap_taken_out(read: impl Fn(u16) -> u32, id: u32) -> Option<(u16, u32, u32)> {
    const FIRST: u16 = 0x100;
    let (mut at, mut before) = (FIRST, None);
    // No more than fit, each header 4 bytes, in the 4 KiB configuration past 0x100.
    for _ in 0..(4096 - usize::from(FIRST)) / 4 {
        let header = read(at);
        if header == 0 || header == u32::MAX {
            return None;
        }
        let next = (header >> 20) & 0xffc;
        if header & 0xffff == id {
            return Some(match before {
                Some(before) => (before, next << 20, 0xfff0_0000),
                None => (FIRST, next << 20, u32::MAX),
            });
        }
        before = Some(at);
        at = u16::try_from(next).ok().filter(|&next| next >= FIRST)?;
    }
    None
}

/// Raises this process's locked-memory limit as far as it may go unprivileged, its hard
/// limit: type 1 pins what each container maps against it (RLIMIT_MEMLOCK,
/// vfio_iommu_type1.c, vfio_lock_acct), and a host gives a user who may pass a device
/// through the limit such DMA needs as the hard one, which the soft one starts below
/// (systemd's DefaultLimitMEMLOCK, limits.conf). What it could not raise stays, and a
/// mapping past it says so ([`Container::map`]).
pub fn raise_memlock() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) fills the rlimit it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut limit) } != 0
        || limit.rlim_cur == limit.rlim_max
    {
        return;
    }
    limit.rlim_cur = limit.rlim_max;
    // SAFETY: setrlimit(2) reads the rlimit it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &raw const limit) } != 0 {
        crate::warn!(
            "raising the locked-memory limit for VFIO's DMA: {}",
            io::Error::last_os_error()
        );
    }
}

/// Whether `offset` is a register the VMM keeps of a function whose MSI and MSI-X
/// capabilities are at `caps`' places, of their lengths: the BARs, the expansion ROM, the
/// interrupt line and pin (with PCIe's hardwired-zero Min_Gnt and Max_Lat beside them),
/// and the capabilities.
fn kept(offset: u16, caps: &[(u16, u16)]) -> bool {
    matches!(offset, 0x10..=0x27 | 0x30..=0x33 | 0x3c..=0x3f)
        || caps.iter().any(|&(at, len)| (at..at + len).contains(&offset))
}

/// Whether `len` bytes at `place`, a BAR's index and an offset in it, lie in a memory BAR
/// of `bars`.
fn in_bars(bars: &[Bar], (index, offset): (u8, u32), len: u64) -> bool {
    bars.iter().any(|bar| {
        bar.index == usize::from(index) && bar.kind != BarKind::Io && u64::from(offset) + len <= bar.size
    })
}

/// What the machine gives the functions it passes through (D154): its hypervisor told of
/// their groups, and memory slots for their BARs.
pub trait Host {
    /// Tells the hypervisor of VFIO's group `group`, before any device of it is opened.
    fn add_group(&self, group: RawFd) -> Result<(), String>;
    /// `count` memory slots, of one function's own.
    fn slots(&self, count: u32) -> Box<dyn BarSlots>;
}

/// Memory slots of a passed-through function's own, which map its BARs into the guest.
pub trait BarSlots: Send + Sync {
    /// Maps `len` bytes at `host` at `gpa`, as slot `i`.
    ///
    /// # Safety
    /// `host..host+len` must stay mapped until slot `i` is unmapped or the VM is destroyed.
    unsafe fn map(&self, i: u32, host: *mut u8, gpa: u64, len: usize) -> Result<(), String>;
    fn unmap(&self, i: u32) -> Result<(), String>;
}

impl std::fmt::Debug for dyn BarSlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BarSlots")
    }
}

/// A region of the device mapped into this process (mmap(2) of the device's descriptor at
/// the region's offset): its place, as an address, and its length. Unmapped as it drops,
/// after the VM whose slots map it: the machine drops its VM before its bus.
#[derive(Debug)]
struct Mapping {
    host: usize,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping `Device::map` made, which nothing reaches past this.
        unsafe { libc::munmap(self.host as *mut libc::c_void, self.len) };
    }
}

/// Part of a BAR the guest reaches without an exit, while the function's memory decoding
/// is on: `len` bytes at `host` in this process, at `gpa` in the guest.
#[derive(Debug)]
struct Window {
    host: usize,
    gpa: u64,
    len: usize,
}

/// The parts of BAR `index`, `len` bytes of the device's region, that the guest can reach
/// without exits, each an offset and a length: all of its pages but those of the MSI-X
/// table and pending bits, which the VMM keeps; none where it is less than a page, as the
/// page it would take in the guest holds other BARs.
fn windows_of(index: usize, len: u64, msix: Option<MsixCap>, page: u64) -> Vec<(u64, u64)> {
    if len < page || !len.is_multiple_of(page) {
        return Vec::new();
    }
    let mut kept: Vec<(u64, u64)> = Vec::new();
    if let Some(cap) = msix {
        let size = u64::from(cap.size);
        for ((bar, offset), bytes) in [
            (cap.table, size * reg::MSIX_ENTRY),
            (cap.pba, size.div_ceil(64) * 8),
        ] {
            if usize::from(bar) == index {
                let start = u64::from(offset) / page * page;
                let end = (u64::from(offset) + bytes).div_ceil(page) * page;
                kept.push((start, end.min(len)));
            }
        }
    }
    kept.sort_unstable();
    let mut out = Vec::new();
    let mut at = 0;
    for (start, end) in kept {
        if start > at {
            out.push((at, start - at));
        }
        at = at.max(end);
    }
    if at < len {
        out.push((at, len - at));
    }
    out
}

/// How a passed-through function's interrupts reach the guest (D154): in the kernel, by
/// irqfds of the machine's, which the VMM never sees.
pub trait Route: Send + Sync {
    /// INTx on the function's line, level-triggered: raised as `trigger` is written, and
    /// lowered as the guest ends the interrupt, which writes `resample`.
    fn intx(&self, trigger: RawFd, resample: RawFd) -> Result<(), String>;
    fn intx_off(&self, trigger: RawFd) -> Result<(), String>;
    /// The MSI the guest aimed at `address` with `data`, raised as `event` is written.
    fn msi(&self, event: RawFd, address: u64, data: u32) -> Result<(), String>;
    fn msi_off(&self, event: RawFd, address: u64, data: u32) -> Result<(), String>;
}

impl std::fmt::Debug for dyn Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Route")
    }
}

/// An eventfd(2): a counter VFIO writes as the function interrupts, and KVM reads.
#[derive(Debug)]
struct EventFd(OwnedFd);

impl EventFd {
    fn new() -> io::Result<EventFd> {
        // SAFETY: eventfd(2) with flags; a fresh descriptor on success.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor just made, ours alone.
        Ok(EventFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// What it counted since it was last read, zero from now: 0 where it counted
    /// nothing, which a read of it, nonblocking, answers with EAGAIN.
    fn take(&self) -> u64 {
        let mut count = [0u8; 8];
        // SAFETY: read(2) of an eventfd's eight bytes into eight of ours.
        let n = unsafe { libc::read(self.raw(), count.as_mut_ptr().cast(), count.len()) };
        if n == 8 { u64::from_ne_bytes(count) } else { 0 }
    }
}

/// A passed-through function's interrupts as they stand: whether the guest has reached
/// the function yet; INTx's eventfds where it is on; MSI's and the message it is routed
/// by where the guest enabled it; and MSI-X's table, the guest's, and its vectors at the
/// host while MSI-X is on there.
#[derive(Debug, Default)]
struct Irqs {
    reached: bool,
    intx: Option<(EventFd, EventFd)>,
    msi: Option<(EventFd, u64, u32)>,
    table: Option<MsixTable>,
    vectors: Option<Vec<Vector>>,
}

/// An MSI-X vector at the host: the eventfd VFIO signals it on, from the guest's first
/// unmasking it, and the message an irqfd routes it by while the guest leaves it unmasked.
#[derive(Debug, Default)]
struct Vector {
    event: Option<EventFd>,
    routed: Option<(u64, u32)>,
}

/// Where an access to a passed-through function's BAR lands: its MSI-X table or pending
/// bits, at an offset in them, which the VMM keeps; or the device.
enum Landing {
    Table(u64),
    Pending(u64),
    Device,
}

/// A passed-through function: its configuration space the device's but for what the VMM
/// keeps ([`Header`]'s): its BARs, placed by the VMM; its expansion ROM, none; its
/// interrupt line and pin; its MSI capability, which vfio-pci leaves a user to enable
/// through VFIO_DEVICE_SET_IRQS (vfio_pci_config.c, vfio_msi_config_write): one message,
/// at the device's place and of its length, without per-vector masking; and its MSI-X
/// capability, with its table and pending bits, which vfio-pci keeps a user from
/// (vfio_pci_rdwr.c, vfio_pci_bar_rw).
#[derive(Debug)]
pub struct VfioFunction {
    device: Device,
    kept: Header,
    /// Its MSI and MSI-X capabilities' places and lengths.
    caps: Vec<(u16, u16)>,
    route: Option<Arc<dyn Route>>,
    /// The machine's MSIs: what sends an MSI-X vector that pended while masked, as the
    /// guest unmasks it.
    send: Option<Arc<dyn Msi>>,
    /// Whether VFIO adds MSI-X vectors one at a time (vfio.h: no VFIO_IRQ_INFO_NORESIZE),
    /// as the host's MSI domains do where they allocate them dynamically
    /// (MSI_FLAG_PCI_MSIX_ALLOC_DYN: x86's), or takes them all at once (arm64's).
    grows: bool,
    irqs: Mutex<Irqs>,
    /// Its BARs' parts the guest reaches without exits, each a memory slot of `slots`,
    /// mapped while its memory decoding is on (`mapped`), in `_mappings` of its regions.
    windows: Vec<Window>,
    slots: Option<Box<dyn BarSlots>>,
    mapped: Mutex<bool>,
    _mappings: Vec<Mapping>,
    /// Bits of its configuration the guest reads as the VMM puts them, not as the device
    /// has them: each a register's offset (a multiple of 4), the bits it reads, and the
    /// mask of those bits.
    overlays: Vec<(u16, u32, u32)>,
}

/// The header type's bit 7 in the register at 0x0c: a multi-function device's function
/// (PCIe 6.0 §7.5.1.1.9).
const MULTI_FUNCTION: u32 = 1 << 23;
/// The register that holds the header type.
const HEADER_TYPE_REGISTER: u16 = reg::HEADER_TYPE & !3;
/// Access Control Services' extended capability (PCIe 6.0 §7.7.11).
const EXT_CAP_ACS: u32 = 0x000d;

impl VfioFunction {
    /// `device`, its BARs at `bars`, its interrupts reaching the guest by `route` and
    /// `send`, where the machine routes them: INTx from the guest's first look at the
    /// function ([`VfioFunction::reach`]), where it has a pin; MSI and MSI-X as the guest
    /// enables them. `together` where its device on the guest's bus holds other functions
    /// too, of its host IOMMU group (`vm::vfio_places`).
    pub fn new(
        device: Device,
        bars: Vec<Bar>,
        route: Option<Arc<dyn Route>>,
        send: Option<Arc<dyn Msi>>,
        host: &dyn Host,
        together: bool,
    ) -> Result<VfioFunction, String> {
        let routed = route.is_some() && send.is_some();
        let pin = if routed && device.signals(IRQ_INTX) {
            device.config(reg::INTERRUPT_PIN, 1) as u8
        } else {
            0
        };
        let msi = if routed && device.signals(IRQ_MSI) {
            device.msi_cap()
        } else {
            None
        };
        let msix_flags = if routed { device.irq_flags(IRQ_MSIX) } else { None };
        // MSI-X where its table and pending bits lie in the device's BARs, as they must
        // for the VMM to keep them.
        let msix = msix_flags.and_then(|_| device.msix_cap()).filter(|cap| {
            let size = u64::from(cap.size);
            let fits = in_bars(&bars, cap.table, size * reg::MSIX_ENTRY)
                && in_bars(&bars, cap.pba, size.div_ceil(64) * 8);
            if !fits {
                crate::warn!(
                    "{}: its MSI-X table or pending bits lie past its BARs",
                    device.name
                );
            }
            fits
        });
        // Each BAR vfio-pci maps, in the guest but for the pages the VMM keeps.
        let page = crate::platform::page_size().map_err(|e| e.to_string())? as u64;
        let (mut windows, mut mappings) = (Vec::new(), Vec::new());
        for bar in &bars {
            let mapping = match device.map(bar.index) {
                Some(Ok(mapping)) => mapping,
                Some(Err(e)) => {
                    crate::warn!("{}: BAR {} stays trapped: {e}", device.name, bar.index);
                    continue;
                }
                None => continue,
            };
            for (offset, len) in windows_of(bar.index, mapping.len as u64, msix, page) {
                windows.push(Window {
                    host: mapping.host + offset as usize,
                    gpa: bar.addr + offset,
                    len: len as usize,
                });
            }
            mappings.push(mapping);
        }
        let slots = (!windows.is_empty()).then(|| host.slots(windows.len() as u32));
        let kept = Header::with_interrupts(
            Identity {
                vendor: 0,
                device: 0,
                class: 0,
                revision: 0,
                subsystem_vendor: 0,
                subsystem: 0,
            },
            bars,
            Interrupts {
                pin,
                msi: msi.map(|(cap, _)| cap),
                msix,
            },
        )?;
        let mut caps: Vec<(u16, u16)> = msi.map(|(cap, len)| (cap.at, len)).into_iter().collect();
        caps.extend(msix.map(|cap| (cap.at, reg::MSIX_LEN)));
        // As the guest must see one host group's functions, which nothing keeps from each
        // other's DMA: where its device holds others, a multi-function device's function,
        // and one without ACS, whose controls would have the guest take it for isolated
        // from them (Linux's pci_acs_enabled; get_pci_function_alias_group); else a single
        // function, whose device has no others for the guest to look for.
        let mut overlays = vec![(
            HEADER_TYPE_REGISTER,
            if together { MULTI_FUNCTION } else { 0 },
            MULTI_FUNCTION,
        )];
        if together {
            overlays.extend(ext_cap_taken_out(|at| device.config(at, 4), EXT_CAP_ACS));
        }
        Ok(VfioFunction {
            device,
            kept,
            caps,
            route,
            send,
            grows: msix_flags.is_some_and(|flags| flags & VFIO_IRQ_INFO_NORESIZE == 0),
            irqs: Mutex::new(Irqs {
                table: msix.map(|cap| MsixTable::new(cap.size)),
                ..Irqs::default()
            }),
            windows,
            slots,
            mapped: Mutex::new(false),
            _mappings: mappings,
            overlays,
        })
    }

    /// Maps its BARs' windows into the guest (`on`), or takes them out: as the guest turns
    /// its memory decoding on, and before it turns it off, as vfio-pci then takes its own
    /// mappings away (vfio_pci_config.c, vfio_basic_config_write) and a vCPU touching one
    /// would fail its run (EFAULT). Out, they are trapped, as accesses to a device that
    /// does not decode them.
    fn map_windows(&self, on: bool) {
        let Some(slots) = &self.slots else {
            return;
        };
        let mut mapped = self.mapped.lock().unwrap_or_else(PoisonError::into_inner);
        if *mapped == on {
            return;
        }
        let mut done = 0;
        for (i, window) in (0u32..).zip(&self.windows) {
            let result = if on {
                // SAFETY: the window lies in one of `_mappings`, which outlive the VM.
                unsafe { slots.map(i, window.host as *mut u8, window.gpa, window.len) }
            } else {
                slots.unmap(i)
            };
            match result {
                Ok(()) => done += 1,
                Err(e) => crate::warn!("{}: a BAR's window at {:#x}: {e}", self.device.name, window.gpa),
            }
        }
        *mapped = on;
        crate::debug!(
            "{}: BAR windows {} the guest: {done}",
            self.device.name,
            if on { "mapped into" } else { "taken from" },
        );
    }

    /// Its BARs, where the VMM placed them.
    pub fn bars(&self) -> &[Bar] {
        self.kept.bars()
    }

    /// Its INTx pin, 0 for none: what the machine routes.
    pub fn pin(&self) -> u8 {
        let mut pin = [0u8];
        self.kept.read(reg::INTERRUPT_PIN, &mut pin);
        pin[0]
    }

    fn irqs(&self) -> std::sync::MutexGuard<'_, Irqs> {
        self.irqs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Routes INTx as the guest first reaches the function's configuration space: its
    /// vCPUs run by then, and KVM takes an irqfd only once the machine's interrupt
    /// controller is initialized, after every vCPU exists (virt/kvm/eventfd.c,
    /// kvm_irqfd_assign: EAGAIN before `kvm_arch_intc_initialized`).
    fn reach(&self) {
        let mut irqs = self.irqs();
        if !irqs.reached {
            irqs.reached = true;
            self.sync(&mut irqs);
        }
    }

    /// Brings the function's interrupts at the host to what the guest set (§6.8): MSI-X
    /// where it enabled MSI-X, else MSI where it enabled MSI, else INTx where the
    /// function has a pin. VFIO has one of them on at a time (vfio_pci_intrs.c,
    /// vfio_pci_set_msi_trigger), so what is no longer wanted goes first.
    fn sync(&self, irqs: &mut Irqs) {
        if !irqs.reached || self.route.is_none() {
            return;
        }
        let msix = self.kept.msix().is_some_and(|m| m.enabled);
        let msi = if msix { None } else { self.kept.msi() };
        if !msix {
            self.msix_off(irqs);
        }
        if irqs.msi.as_ref().map(|&(_, address, data)| (address, data)) != msi {
            self.msi_off(irqs);
        }
        if msix || msi.is_some() {
            self.intx_off(irqs);
        }
        if msix {
            if irqs.vectors.is_none() {
                self.msix_on(irqs);
            }
            for k in 0..irqs.vectors.as_ref().map_or(0, Vec::len) {
                self.vector(irqs, k);
            }
        } else if let Some((address, data)) = msi {
            if irqs.msi.is_none() {
                self.msi_on(irqs, address, data);
            }
        } else if self.pin() != 0
            && irqs.intx.is_none()
            && let Err(e) = self.intx_on(irqs)
        {
            crate::warn!("{}: INTx: {e}", self.device.name);
        }
    }

    /// INTx routed and signalled: VFIO masks it as it signals it, and unmasks it as the
    /// guest ends the interrupt, KVM writing the resample eventfd.
    fn intx_on(&self, irqs: &mut Irqs) -> Result<(), String> {
        let Some(route) = &self.route else {
            return Ok(());
        };
        let (trigger, resample) = (
            EventFd::new().map_err(|e| e.to_string())?,
            EventFd::new().map_err(|e| e.to_string())?,
        );
        route.intx(trigger.raw(), resample.raw())?;
        // Signalled first: vfio-pci takes INTx's unmask eventfd only for INTx it signals
        // (vfio_pci_intrs.c, vfio_pci_set_intx_unmask), and one written meanwhile is not
        // lost (vfio_virqfd_enable takes an event already pending).
        let on = self
            .device
            .trigger(IRQ_INTX, Some(trigger.raw()))
            .and_then(|()| self.device.unmask_intx_on(resample.raw()));
        if let Err(e) = on {
            let _ = route.intx_off(trigger.raw());
            return Err(e.to_string());
        }
        irqs.intx = Some((trigger, resample));
        Ok(())
    }

    fn intx_off(&self, irqs: &mut Irqs) {
        let (Some(route), Some((trigger, _))) = (&self.route, irqs.intx.take()) else {
            return;
        };
        if let Err(e) = self.device.trigger(IRQ_INTX, None) {
            crate::warn!("{}: INTx off: {e}", self.device.name);
        }
        if let Err(e) = route.intx_off(trigger.raw()) {
            crate::warn!("{}: INTx's route: {e}", self.device.name);
        }
    }

    /// MSI routed by the guest's message and signalled.
    fn msi_on(&self, irqs: &mut Irqs, address: u64, data: u32) {
        let Some(route) = &self.route else {
            return;
        };
        let on = EventFd::new().map_err(|e| e.to_string()).and_then(|event| {
            route.msi(event.raw(), address, data)?;
            if let Err(e) = self.device.trigger(IRQ_MSI, Some(event.raw())) {
                let _ = route.msi_off(event.raw(), address, data);
                return Err(e.to_string());
            }
            Ok(event)
        });
        match on {
            Ok(event) => irqs.msi = Some((event, address, data)),
            Err(e) => crate::warn!("{}: MSI to {address:#x} with {data:#x}: {e}", self.device.name),
        }
    }

    fn msi_off(&self, irqs: &mut Irqs) {
        let (Some(route), Some((event, address, data))) = (&self.route, irqs.msi.take()) else {
            return;
        };
        if let Err(e) = self.device.trigger(IRQ_MSI, None) {
            crate::warn!("{}: MSI off: {e}", self.device.name);
        }
        if let Err(e) = route.msi_off(event.raw(), address, data) {
            crate::warn!("{}: MSI's route: {e}", self.device.name);
        }
    }

    /// MSI-X on at the host, no vector signalled on anything (an eventfd of -1) until the
    /// guest unmasks it (vfio.h, VFIO_IRQ_INFO_NORESIZE): every vector of the table at
    /// once where VFIO cannot add one later, as a vector added by disabling MSI-X and
    /// enabling it again would lose the interrupts between; else the first alone, and
    /// each other as the guest first unmasks it, the host allocating only those in use.
    fn msix_on(&self, irqs: &mut Irqs) {
        let Some(table) = &irqs.table else {
            return;
        };
        let count = if self.grows { 1 } else { table.vectors() };
        match self.device.trigger_msix(0, &vec![-1; count]) {
            Ok(()) => irqs.vectors = Some((0..table.vectors()).map(|_| Vector::default()).collect()),
            Err(e) => crate::warn!("{}: MSI-X of {count} vectors: {e}", self.device.name),
        }
    }

    fn msix_off(&self, irqs: &mut Irqs) {
        let (Some(route), Some(vectors)) = (&self.route, irqs.vectors.take()) else {
            return;
        };
        if let Err(e) = self.device.trigger(IRQ_MSIX, None) {
            crate::warn!("{}: MSI-X off: {e}", self.device.name);
        }
        for vector in vectors {
            if let (Some(event), Some((address, data))) = (vector.event, vector.routed)
                && let Err(e) = route.msi_off(event.raw(), address, data)
            {
                crate::warn!("{}: an MSI-X vector's route: {e}", self.device.name);
            }
        }
    }

    /// MSI-X vector `k` at the host as the guest left it. Where it and the function are
    /// unmasked, its eventfd, made and given VFIO as the guest first unmasks it, is
    /// routed by its message through an irqfd; and the vector is sent where it pended
    /// masked: VFIO signals it masked or not, its eventfd counting what no irqfd took.
    /// KVM injects what an eventfd counted as its irqfd is assigned, but leaves the count
    /// (virt/kvm/eventfd.c, kvm_irqfd_assign), so the count is taken before, to be sent
    /// once, and after, not to be sent again. Where either is masked, the irqfd goes.
    fn vector(&self, irqs: &mut Irqs, k: usize) {
        let (Some(route), Some(table), Some(vectors)) = (&self.route, &mut irqs.table, &mut irqs.vectors)
        else {
            return;
        };
        let Some(vector) = vectors.get_mut(k) else {
            return;
        };
        let masked = self.kept.msix().is_none_or(|m| m.masked);
        let want = if masked { None } else { table.message(k) };
        if vector.routed == want {
            return;
        }
        if let (Some(event), Some((address, data))) = (&vector.event, vector.routed.take())
            && let Err(e) = route.msi_off(event.raw(), address, data)
        {
            crate::warn!("{}: MSI-X vector {k}'s route: {e}", self.device.name);
        }
        let Some((address, data)) = want else {
            return;
        };
        let event = match vector.event.take() {
            Some(event) => event,
            None => {
                let given = EventFd::new().and_then(|event| {
                    self.device.trigger_msix(k as u32, &[event.raw()])?;
                    Ok(event)
                });
                match given {
                    Ok(event) => event,
                    Err(e) => {
                        crate::warn!("{}: MSI-X vector {k}: {e}", self.device.name);
                        return;
                    }
                }
            }
        };
        if event.take() > 0 {
            table.set_pending(k, true);
        }
        if table.pending(k) {
            if let Some(send) = &self.send {
                send.send(address, data);
            }
            table.set_pending(k, false);
        }
        match route.msi(event.raw(), address, data) {
            Ok(()) => {
                vector.routed = Some((address, data));
                // What KVM injected as it assigned the irqfd.
                event.take();
            }
            Err(e) => crate::warn!(
                "{}: MSI-X vector {k} to {address:#x} with {data:#x}: {e}",
                self.device.name
            ),
        }
        vector.event = Some(event);
    }

    /// Where an access at `offset` of BAR `bar` lands.
    fn landing(&self, bar: usize, offset: u64) -> Landing {
        let Some(cap) = self.kept.msix_cap() else {
            return Landing::Device;
        };
        let size = u64::from(cap.size);
        let within = |(index, at): (u8, u32), len: u64| {
            (usize::from(index) == bar)
                .then(|| offset.checked_sub(u64::from(at)))
                .flatten()
                .filter(|&o| o < len)
        };
        if let Some(at) = within(cap.table, size * reg::MSIX_ENTRY) {
            return Landing::Table(at);
        }
        if let Some(at) = within(cap.pba, size.div_ceil(64) * 8) {
            return Landing::Pending(at);
        }
        Landing::Device
    }

    /// Reads BAR `bar` at `offset`. The pending bits are the device's own, for vectors
    /// VFIO signals on nothing yet, which the host leaves masked at the device, with each
    /// vector that pended while the guest masked it, as its eventfd counted.
    fn bar_read(&self, bar: usize, offset: u64, data: &mut [u8]) {
        match self.landing(bar, offset) {
            Landing::Device => self.device.read(bar, offset, data),
            Landing::Table(at) => {
                if let Some(table) = &self.irqs().table {
                    table.read(at, data);
                }
            }
            Landing::Pending(at) => {
                self.device.read(bar, offset, data);
                let mut irqs = self.irqs();
                let Irqs { table, vectors, .. } = &mut *irqs;
                let Some(table) = table else {
                    return;
                };
                for (k, vector) in vectors.iter().flatten().enumerate() {
                    if vector.routed.is_none() && vector.event.as_ref().is_some_and(|e| e.take() > 0) {
                        table.set_pending(k, true);
                    }
                }
                let mut ours = [0u8; 8];
                let ours = ours.get_mut(..data.len()).unwrap_or_default();
                table.read_pending(at, ours);
                for (d, o) in data.iter_mut().zip(ours.iter()) {
                    *d |= o;
                }
            }
        }
    }

    /// Writes BAR `bar` at `offset`: a write of a table entry brings its vector at the
    /// host to what the guest left in it; the pending bits are read-only.
    fn bar_write(&self, bar: usize, offset: u64, data: &[u8]) {
        match self.landing(bar, offset) {
            Landing::Device => self.device.write(bar, offset, data),
            Landing::Table(at) => {
                let mut irqs = self.irqs();
                if let Some(k) = irqs.table.as_mut().and_then(|table| table.write(at, data)) {
                    self.vector(&mut irqs, k);
                }
            }
            Landing::Pending(_) => {}
        }
    }
}

impl PciFunction for VfioFunction {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        self.reach();
        if kept(offset, &self.caps) {
            self.kept.read(offset, data);
        } else {
            self.device.read(CONFIG_REGION as usize, u64::from(offset), data);
        }
        overlay(&self.overlays, offset, data);
    }

    fn config_write(&self, offset: u16, data: &[u8]) {
        self.reach();
        if !kept(offset, &self.caps) {
            // The command register's memory decoding, where this writes it.
            let decoding = usize::from(reg::COMMAND)
                .checked_sub(usize::from(offset))
                .and_then(|i| data.get(i))
                .map(|&b| u16::from(b) & reg::COMMAND_MEMORY != 0);
            if decoding == Some(false) {
                self.map_windows(false);
            }
            self.device.write(CONFIG_REGION as usize, u64::from(offset), data);
            if decoding == Some(true) {
                self.map_windows(true);
            }
            return;
        }
        self.kept.write(offset, data);
        if self
            .caps
            .iter()
            .any(|&(at, len)| (at..at + len).contains(&offset))
        {
            self.sync(&mut self.irqs());
        }
    }
}

/// A memory BAR of a passed-through function: each access the device's region's, but for
/// the function's MSI-X table and pending bits, which the VMM keeps.
#[derive(Debug)]
pub struct VfioBar {
    function: Arc<VfioFunction>,
    index: usize,
}

impl VfioBar {
    pub fn new(function: Arc<VfioFunction>, index: usize) -> VfioBar {
        VfioBar { function, index }
    }
}

impl MmioDevice for VfioBar {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.function.bar_read(self.index, offset, data);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        self.function.bar_write(self.index, offset, data);
    }

    fn pause(&self) {}

    fn resume(&self) -> Result<(), String> {
        Ok(())
    }

    fn save(&self, _: &mut Writer) {}

    fn restore(&self, _: &mut Reader<'_>) -> codec::Result<()> {
        Ok(())
    }

    fn unsaved(&self) -> Option<String> {
        Some("a passed-through device's state is its own".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VMM keeps the BARs, the expansion ROM, the interrupt line and pin (with PCIe's
    /// hardwired-zero Min_Gnt and Max_Lat beside them) and the MSI and MSI-X
    /// capabilities, all of them; every other register is the device's, through vfio-pci.
    #[test]
    fn the_vmm_keeps_the_bars_rom_interrupt_registers_msi_and_msix() {
        let caps = [(0x50, 14), (0x70, reg::MSIX_LEN)];
        for offset in [
            0x10, 0x14, 0x24, 0x27, 0x30, 0x33, 0x3c, 0x3d, 0x3e, 0x3f, 0x50, 0x5d, 0x70, 0x7b,
        ] {
            assert!(kept(offset, &caps), "{offset:#x}");
        }
        for offset in [
            0x00, 0x04, 0x06, 0x0f, 0x28, 0x2c, 0x34, 0x38, 0x40, 0x4f, 0x5e, 0x6f, 0x7c, 0x100,
        ] {
            assert!(!kept(offset, &caps), "{offset:#x}");
        }
        assert!(!kept(0x50, &[]));
    }

    /// What the guest reads of a function where the VMM puts bits of its own: those bits,
    /// in whichever bytes of a read hold them; and an extended capability taken out of its
    /// list, the first or one past it, the rest of the list as it was.
    #[test]
    fn the_guest_reads_overlaid_bits_and_a_list_without_acs() {
        let overlays = [(0x0c, MULTI_FUNCTION, MULTI_FUNCTION)];
        let mut header = [0x10, 0x00, 0x00, 0x00];
        overlay(&overlays, 0x0c, &mut header);
        assert_eq!(header, [0x10, 0x00, 0x80, 0x00]);
        let mut byte = [0x01];
        overlay(&overlays, 0x0e, &mut byte);
        assert_eq!(byte, [0x81]);
        let mut cleared = [0xff, 0xff];
        overlay(&[(0x0c, 0, MULTI_FUNCTION)], 0x0e, &mut cleared);
        assert_eq!(cleared, [0x7f, 0xff]);
        let mut elsewhere = [0xff; 4];
        overlay(&overlays, 0x10, &mut elsewhere);
        assert_eq!(elsewhere, [0xff; 4]);
        // AER at 0x100, ACS at 0x148, then SR-IOV at 0x160: AER points past ACS.
        let space =
            |list: Vec<(u16, u32)>| move |at: u16| list.iter().find(|(a, _)| *a == at).map_or(0, |&(_, h)| h);
        let header = |id: u32, next: u32| id | (1 << 16) | (next << 20);
        let three = vec![
            (0x100, header(0x0001, 0x148)),
            (0x148, header(0x000d, 0x160)),
            (0x160, header(0x0010, 0)),
        ];
        assert_eq!(
            ext_cap_taken_out(space(three.clone()), EXT_CAP_ACS),
            Some((0x100, 0x160 << 20, 0xfff0_0000))
        );
        let mut first = header(0x0001, 0x148).to_le_bytes();
        overlay(&[(0x100, 0x160 << 20, 0xfff0_0000)], 0x100, &mut first);
        assert_eq!(u32::from_le_bytes(first), header(0x0001, 0x160));
        // ACS first: a header of ID 0 pointing at the next.
        let acs_first = vec![(0x100, header(0x000d, 0x160)), (0x160, header(0x0010, 0))];
        assert_eq!(
            ext_cap_taken_out(space(acs_first), EXT_CAP_ACS),
            Some((0x100, 0x160 << 20, u32::MAX))
        );
        // ACS alone: no list at all.
        assert_eq!(
            ext_cap_taken_out(space(vec![(0x100, header(0x000d, 0))]), EXT_CAP_ACS),
            Some((0x100, 0, u32::MAX))
        );
        assert_eq!(
            ext_cap_taken_out(space(vec![(0x100, header(0x0001, 0))]), EXT_CAP_ACS),
            None
        );
        assert_eq!(
            ext_cap_taken_out(space(Vec::new()), EXT_CAP_ACS),
            None,
            "no extended space"
        );
        // A list that points back into the standard space, or at itself, ends.
        assert_eq!(
            ext_cap_taken_out(space(vec![(0x100, header(0x0001, 0x40))]), EXT_CAP_ACS),
            None
        );
        assert_eq!(
            ext_cap_taken_out(space(vec![(0x100, header(0x0001, 0x100))]), EXT_CAP_ACS),
            None
        );
    }

    /// A BAR's pages reach the guest without exits but for those of the MSI-X table and
    /// pending bits, which the VMM keeps; a BAR of less than a page stays trapped whole.
    #[test]
    fn a_bar_is_mapped_but_for_its_msix_pages() {
        let page = 0x1000;
        let msix = |table: (u8, u32), pba: (u8, u32), size: u16| {
            Some(MsixCap {
                at: 0x70,
                next: 0,
                size,
                table,
                pba,
            })
        };
        assert_eq!(windows_of(0, 0x10_0000, None, page), vec![(0, 0x10_0000)]);
        assert_eq!(windows_of(0, 0x800, None, page), vec![], "under a page");
        // Two vectors and their pending bits in one page: none of it mapped.
        assert_eq!(windows_of(2, 0x1000, msix((2, 0), (2, 0x800), 2), page), vec![]);
        // In another BAR: all of this one.
        assert_eq!(
            windows_of(0, 0x4000, msix((2, 0), (2, 0x800), 2), page),
            vec![(0, 0x4000)]
        );
        // A table of 512 vectors (8 KiB) at 0x2000, its pending bits at 0x6008: the
        // pages around each mapped.
        assert_eq!(
            windows_of(4, 0x10000, msix((4, 0x2000), (4, 0x6008), 512), page),
            vec![(0, 0x2000), (0x4000, 0x2000), (0x7000, 0x9000)]
        );
    }

    /// MSI-X's table and pending bits are the VMM's only where they lie in a memory BAR
    /// the function has.
    #[test]
    fn an_msix_table_is_kept_only_within_a_bar() {
        let bars = [
            Bar {
                index: 0,
                addr: 0x1000_0000,
                size: 0x4000,
                kind: BarKind::Memory32,
            },
            Bar {
                index: 2,
                addr: 0x1000,
                size: 0x100,
                kind: BarKind::Io,
            },
        ];
        assert!(in_bars(&bars, (0, 0x2000), 0x2000));
        assert!(!in_bars(&bars, (0, 0x2000), 0x2001));
        assert!(!in_bars(&bars, (1, 0), 16));
        assert!(!in_bars(&bars, (2, 0), 16), "an I/O BAR");
    }
}
