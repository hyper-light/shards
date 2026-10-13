//! A host's PCI function, passed through by VFIO (Linux Documentation/driver-api/vfio.rst,
//! include/uapi/linux/vfio.h; D153): its group set in a container of its own, with the
//! type 1 IOMMU, all of guest RAM mapped for its DMA, or with none where the group is a
//! no-IOMMU one (`enable_unsafe_noiommu_mode`, for tests of functions that do no DMA);
//! its configuration space the device's, read and written through vfio-pci, which
//! already keeps the device's own registers from a guest, but for its BARs, its expansion
//! ROM and its interrupt line and pin, which the VMM keeps; its memory BARs reached
//! through the device's regions.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Bar, BarKind, Header, Identity, PciFunction, reg};
use crate::devices::MmioDevice;
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
const VFIO_DEVICE_RESET: u64 = vfio_io(11);
const VFIO_IOMMU_MAP_DMA: u64 = vfio_io(13);

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
    VFIO_DEVICE_RESET,
    VFIO_IOMMU_MAP_DMA,
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
/// The configuration space's region (VFIO_PCI_CONFIG_REGION_INDEX).
const CONFIG_REGION: u32 = 7;

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
        })
    }

    fn iommu(noiommu: bool) -> u64 {
        if noiommu {
            VFIO_NOIOMMU_IOMMU
        } else {
            VFIO_TYPE1V2_IOMMU
        }
    }

    /// The group at `path`, set in this container: the first sets its IOMMU and maps guest
    /// RAM, `memory`, for their DMA; a group set already is the same.
    fn group(&mut self, path: &Path, memory: &GuestMemory) -> Result<Arc<OwnedFd>, String> {
        if let Some((_, fd)) = self.groups.iter().find(|(p, _)| p == path) {
            return Ok(fd.clone());
        }
        let err = |what: &str, e: io::Error| format!("{}: {what}: {e}", path.display());
        let group = open(path).map_err(|e| err("opening it", e))?;
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
            unsafe { ioctl(&*self.fd, VFIO_SET_IOMMU, Self::iommu(self.noiommu) as usize) }
                .map_err(|e| err("setting its IOMMU", e))?;
            if !self.noiommu {
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
    ) -> Result<Device, String> {
        let err = |what: &str, e: io::Error| format!("{name}: {what}: {e}");
        let group = container
            .group(group, memory)
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

/// A passed-through function: its configuration space the device's but for what the VMM
/// keeps ([`Header`]'s): its BARs, placed by the VMM; its expansion ROM, none; and its
/// interrupt line and pin, none yet routed.
#[derive(Debug)]
pub struct VfioFunction {
    device: Arc<Device>,
    kept: Header,
}

impl VfioFunction {
    pub fn new(device: Arc<Device>, bars: Vec<Bar>) -> Result<VfioFunction, String> {
        let kept = Header::new(
            Identity {
                vendor: 0,
                device: 0,
                class: 0,
                revision: 0,
                subsystem_vendor: 0,
                subsystem: 0,
            },
            bars,
        )?;
        Ok(VfioFunction { device, kept })
    }

    /// Whether `offset` is a register the VMM keeps.
    fn kept(offset: u16) -> bool {
        matches!(offset, 0x10..=0x27 | 0x30..=0x33 | 0x3c..=0x3f)
    }
}

impl PciFunction for VfioFunction {
    fn config_read(&self, offset: u16, data: &mut [u8]) {
        if Self::kept(offset) {
            self.kept.read(offset, data);
        } else {
            self.device.read(CONFIG_REGION as usize, u64::from(offset), data);
        }
    }

    fn config_write(&self, offset: u16, data: &[u8]) {
        if Self::kept(offset) {
            self.kept.write(offset, data);
        } else {
            self.device.write(CONFIG_REGION as usize, u64::from(offset), data);
        }
    }
}

/// A memory BAR of a passed-through function, each access the device's region's.
#[derive(Debug)]
pub struct VfioBar {
    device: Arc<Device>,
    index: usize,
}

impl VfioBar {
    pub fn new(device: Arc<Device>, index: usize) -> VfioBar {
        VfioBar { device, index }
    }
}

impl MmioDevice for VfioBar {
    fn read(&self, offset: u64, data: &mut [u8]) {
        self.device.read(self.index, offset, data);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        self.device.write(self.index, offset, data);
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

    /// The VMM keeps the BARs, the expansion ROM and the interrupt line and pin (with
    /// PCIe's hardwired-zero Min_Gnt and Max_Lat beside them); every other register is the
    /// device's, through vfio-pci.
    #[test]
    fn the_vmm_keeps_the_bars_rom_and_interrupt_registers() {
        for offset in [0x10, 0x14, 0x24, 0x27, 0x30, 0x33, 0x3c, 0x3d, 0x3e, 0x3f] {
            assert!(VfioFunction::kept(offset), "{offset:#x}");
        }
        for offset in [0x00, 0x04, 0x06, 0x0f, 0x28, 0x2c, 0x34, 0x38, 0x40, 0x100] {
            assert!(!VfioFunction::kept(offset), "{offset:#x}");
        }
    }
}
