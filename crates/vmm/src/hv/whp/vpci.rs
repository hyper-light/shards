//! A host's PCI function assigned to a partition (D154): Discrete Device Assignment through
//! WHP's virtual PCI devices (Microsoft Learn, "Windows Hypervisor Platform API
//! Definitions", its "Virtual PCI (VPCI) Devices" and each function's page, 2026-06-08), as
//! Microsoft's own VMM drives them (OpenVMM's vm/whp/src/lib.rs, 29ff7a10). The function,
//! dismounted from the host by its administrator (Dismount-VMHostAssignableDevice), is
//! allocated by the instance ID the host gives it then (WHvAllocateVpciResource, with no
//! provider and its WHV_SRIOV_RESOURCE_DESCRIPTOR's PnpInstanceId, as for a discretely
//! assignable physical function; PM M401 allocated one so), and made a physically backed
//! device of a partition allowed device assignment (WHvCreateVpciDevice), which programs the
//! host's IOMMU to remap the function's DMA to the partition's addresses. The machine sees
//! it through [`crate::devices::pci::assigned`].
//!
//! Both of WHP's backends' (D209): each function's page gives it x64 and Arm64 alike, and
//! WinHvPlatform.h declares them, and WinHvPlatformDefs.h their types, outside its
//! architectures' branches (Windows SDK 10.0.26100). What differs is an MSI's target
//! (`Assigned::target`): on x86_64 the vCPUs its destination names, as the local APICs
//! resolve it (WHvGetInterruptTargetVpSet, which the header gives x64 alone); on arm64 the
//! SPI the GICv2m frame the VMM keeps names, at one vCPU.

use std::ffi::c_void;
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

use super::ffi::*;
use super::{Error, Partition, Result, Vm, check};
#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::ioapic::Message;
use crate::devices::pci::assigned::{Device, Range, Space};

/// DEVICE_POWER_STATE's D0 (wdm.h): D`n` is 1 + n.
const POWER_D0: i32 = 1;

/// A function made a virtual PCI device of a partition, by its logical device ID there:
/// deleted, and let go of by the partition, as it drops.
#[derive(Debug)]
pub struct Assigned {
    partition: Arc<Partition>,
    #[cfg(target_arch = "x86_64")]
    vcpus: u32,
    id: u64,
    name: String,
}

impl Vm {
    /// The host's function `name`, its instance ID once dismounted from the host
    /// (`PCIP\...`, as Get-VMHostAssignableDevice lists it), made device `id` of this VM's
    /// partition, which [`Vm::assigning`] made.
    pub fn assign(&self, id: u64, name: &str) -> Result<Assigned> {
        let mut descriptor = WHV_SRIOV_RESOURCE_DESCRIPTOR::default();
        let wide: Vec<u16> = name.encode_utf16().collect();
        // Its instance ID NUL-terminated in the PnP manager's 200 characters
        // (WHV_MAX_DEVICE_ID_SIZE_IN_CHARS).
        if wide.len() >= descriptor.PnpInstanceId.len() {
            return Err(Error::Guest(format!(
                "{name}: an instance ID past the PnP manager's {} characters",
                descriptor.PnpInstanceId.len() - 1
            )));
        }
        for (d, c) in descriptor.PnpInstanceId.iter_mut().zip(wide) {
            *d = c;
        }
        let mut resource: HANDLE = std::ptr::null_mut();
        // SAFETY: no provider, a descriptor of its own size, and a handle for the call to
        // fill.
        check("WHvAllocateVpciResource", unsafe {
            WHvAllocateVpciResource(
                std::ptr::null(),
                WHvAllocateVpciResourceFlagNone,
                (&raw const descriptor).cast(),
                size_of::<WHV_SRIOV_RESOURCE_DESCRIPTOR>() as u32,
                &raw mut resource,
            )
        })?;
        // SAFETY: the partition, set up, and the resource just allocated, of no
        // notification event.
        let created = check("WHvCreateVpciDevice", unsafe {
            WHvCreateVpciDevice(
                self.partition.0,
                id,
                resource,
                WHvCreateVpciDeviceFlagPhysicallyBacked,
                std::ptr::null_mut(),
            )
        });
        // The device holds what it needs of the resource: OpenVMM closes it once the device
        // is made (lib.rs, create_device, which takes it by value).
        // SAFETY: the handle the allocation gave, closed once.
        unsafe { CloseHandle(resource) };
        created?;
        Ok(Assigned {
            partition: self.partition.clone(),
            #[cfg(target_arch = "x86_64")]
            vcpus: self.vcpus,
            id,
            name: name.to_string(),
        })
    }
}

impl Assigned {
    /// What its BAR registers read once all ones were written to each
    /// (WHvVpciDevicePropertyCodeProbedBARs), which give their sizes.
    pub fn probed_bars(&self) -> Result<[u32; 6]> {
        let mut bars = WHV_VPCI_PROBED_BARS::default();
        let mut written = 0u32;
        // SAFETY: a buffer of the property's size, which the call fills.
        check("WHvGetVpciDeviceProperty(ProbedBARs)", unsafe {
            WHvGetVpciDeviceProperty(
                self.partition.0,
                self.id,
                WHvVpciDevicePropertyCodeProbedBARs,
                (&raw mut bars).cast(),
                size_of::<WHV_VPCI_PROBED_BARS>() as u32,
                &raw mut written,
            )
        })?;
        Ok(bars.Value)
    }

    /// The register `len` bytes long at `offset` of `space`.
    fn register(
        space: Space,
        offset: u64,
        len: usize,
    ) -> std::result::Result<WHV_VPCI_DEVICE_REGISTER, String> {
        let location = match space {
            Space::Config => WHvVpciConfigSpace,
            Space::Bar(n) => i32::try_from(n)
                .ok()
                .filter(|n| (WHvVpciBar0..=WHvVpciBar5).contains(n))
                .ok_or_else(|| format!("BAR {n}: not one of a function's six"))?,
        };
        Ok(WHV_VPCI_DEVICE_REGISTER {
            Location: location,
            SizeInBytes: u32::try_from(len).map_err(|e| e.to_string())?,
            OffsetInBytes: offset,
        })
    }
}

#[cfg(target_arch = "x86_64")]
impl Assigned {
    /// The guest's MSI of `address` and `data` (Intel SDM Vol. 3A §11.11) as WHP takes it:
    /// its vector, and the vCPUs its destination names, as the partition's local APICs
    /// resolve it (WHvGetInterruptTargetVpSet); all of them where its delivery is fixed,
    /// multicast, and the first where it is lowest priority, which goes to one.
    fn target(&self, address: u64, data: u32) -> std::result::Result<(u32, Vec<u32>), String> {
        let m = Message::of_msi(address, data);
        if m.delivery > 1 {
            return Err(format!(
                "an MSI of delivery mode {}: an assigned function's are fixed or lowest priority",
                m.delivery
            ));
        }
        let mode = if m.logical {
            WHvX64InterruptDestinationModeLogical
        } else {
            WHvX64InterruptDestinationModePhysical
        };
        let mut vps = vec![0u32; self.vcpus as usize];
        let mut written = 0u32;
        // SAFETY: a buffer of `vcpus` indexes, which the call fills.
        check("WHvGetInterruptTargetVpSet", unsafe {
            WHvGetInterruptTargetVpSet(
                self.partition.0,
                u64::from(m.destination),
                mode,
                vps.as_mut_ptr(),
                self.vcpus,
                &raw mut written,
            )
        })
        .map_err(|e| e.to_string())?;
        vps.truncate(written as usize);
        if m.delivery == 1 {
            vps.truncate(1);
        }
        if vps.is_empty() {
            return Err(format!(
                "an MSI to destination {:#x}, which names no vCPU",
                m.destination
            ));
        }
        Ok((u32::from(m.vector), vps))
    }
}

#[cfg(target_arch = "aarch64")]
impl Assigned {
    /// The guest's MSI of `address` and `data` as WHP takes it on arm64: as its vector the
    /// SPI, by INTID, that the GICv2m frame the VMM keeps names (its doorbell the address,
    /// the INTID the data: `boot::v2m_spi`), as Hyper-V's own virtual PCI takes an SPI's
    /// INTID as the vector there (Linux's drivers/pci/controller/pci-hyperv.c at d4872371,
    /// hv_msi_get_int_vector: the GIC's hwirq, from HV_PCI_MSI_SPI_START; OpenVMM's
    /// vmm_core/virt_whp/src/device.rs at ba105dd7 hands WHP the vector its guest asks for,
    /// on either architecture); aimed at one vCPU, as WHP aims each MSI at one on Arm64
    /// (WHvMapVpciDeviceInterrupt: E_INVALIDARG for more). The boot vCPU, VP 0: an SPI goes
    /// where the GIC's distributor routes it, which the hypervisor's GIC keeps, as
    /// pci-hyperv.c says of Hyper-V's (hv_arch_irq_unmask: "SPIs is managed via GICD
    /// registers which Hyper-V already supports").
    fn target(&self, address: u64, data: u32) -> std::result::Result<(u32, Vec<u32>), String> {
        let frame = self
            .partition
            .msi_frame()
            .ok_or("its machine's GIC has no GICv2m frame, which an assigned function's MSIs name")?;
        let intid =
            crate::arch::aarch64::boot::v2m_spi(frame.base, frame.first_intid, frame.count, address, data)?;
        Ok((intid, vec![0]))
    }
}

impl Device for Assigned {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(&self, space: Space, offset: u64, data: &mut [u8]) -> std::result::Result<(), String> {
        let register = Assigned::register(space, offset, data.len())?;
        // SAFETY: the register, and a buffer of its size.
        check("WHvReadVpciDeviceRegister", unsafe {
            WHvReadVpciDeviceRegister(
                self.partition.0,
                self.id,
                &raw const register,
                data.as_mut_ptr().cast::<c_void>(),
            )
        })
        .map_err(|e| e.to_string())
    }

    fn write(&self, space: Space, offset: u64, data: &[u8]) -> std::result::Result<(), String> {
        let register = Assigned::register(space, offset, data.len())?;
        // SAFETY: the register, and a buffer of its size.
        check("WHvWriteVpciDeviceRegister", unsafe {
            WHvWriteVpciDeviceRegister(
                self.partition.0,
                self.id,
                &raw const register,
                data.as_ptr().cast::<c_void>(),
            )
        })
        .map_err(|e| e.to_string())
    }

    fn power(&self, state: u8) -> std::result::Result<(), String> {
        // SAFETY: the device, and a power state of D0 to D3.
        check("WHvSetVpciDevicePowerState", unsafe {
            WHvSetVpciDevicePowerState(self.partition.0, self.id, POWER_D0 + i32::from(state & 3))
        })
        .map_err(|e| e.to_string())
    }

    fn map_ranges(&self) -> std::result::Result<Vec<Range>, String> {
        let mut count = 0u32;
        let mut mappings: *mut WHV_VPCI_MMIO_MAPPING = std::ptr::null_mut();
        // SAFETY: a count and an array pointer for the call to fill.
        check("WHvMapVpciDeviceMmioRanges", unsafe {
            WHvMapVpciDeviceMmioRanges(self.partition.0, self.id, &raw mut count, &raw mut mappings)
        })
        .map_err(|e| e.to_string())?;
        if mappings.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: the array of `count` mappings the call returned, valid until they are
        // unmapped, the device powered off or deleted; copied out here.
        let all = unsafe { std::slice::from_raw_parts(mappings, count as usize) };
        Ok(all
            .iter()
            .filter_map(|m| {
                Some(Range {
                    bar: usize::try_from(m.Location).ok()?,
                    offset: m.OffsetInBytes,
                    len: m.SizeInBytes,
                    host: m.VirtualAddress as usize,
                    read: m.Flags & WHvVpciMmioRangeFlagReadAccess != 0,
                    write: m.Flags & WHvVpciMmioRangeFlagWriteAccess != 0,
                })
            })
            .collect())
    }

    fn unmap_ranges(&self) -> std::result::Result<(), String> {
        // SAFETY: the device, whose ranges nothing reaches past this.
        check("WHvUnmapVpciDeviceMmioRanges", unsafe {
            WHvUnmapVpciDeviceMmioRanges(self.partition.0, self.id)
        })
        .map_err(|e| e.to_string())
    }

    /// The guest's MSI of `address` and `data` mapped by WHP as interrupt `index`, to the
    /// vector and vCPUs [`Assigned::target`] gives: the message WHP returns, which the
    /// device is to send (WHvMapVpciDeviceInterrupt).
    fn map_interrupt(&self, index: u32, address: u64, data: u32) -> std::result::Result<(u64, u32), String> {
        let (vector, vps) = self.target(address, data)?;
        let flags = if vps.len() > 1 {
            WHvVpciInterruptTargetFlagMulticast
        } else {
            WHvVpciInterruptTargetFlagNone
        };
        let count = u32::try_from(vps.len()).map_err(|e| e.to_string())?;
        // WHV_VPCI_INTERRUPT_TARGET: its vector, flags and processor count, then as many
        // processors, each a u32.
        let mut target = vec![vector, flags as u32, count];
        target.extend(&vps);
        let (mut sent, mut with) = (0u64, 0u32);
        // SAFETY: a target of its processor count's length, 4-aligned as the structure is,
        // and an address and data for the call to fill.
        check("WHvMapVpciDeviceInterrupt", unsafe {
            WHvMapVpciDeviceInterrupt(
                self.partition.0,
                self.id,
                index,
                1,
                target.as_ptr().cast::<WHV_VPCI_INTERRUPT_TARGET>(),
                &raw mut sent,
                &raw mut with,
            )
        })
        .map_err(|e| e.to_string())?;
        Ok((sent, with))
    }

    fn unmap_interrupt(&self, index: u32) -> std::result::Result<(), String> {
        // SAFETY: the device's interrupt `index`.
        check("WHvUnmapVpciDeviceInterrupt", unsafe {
            WHvUnmapVpciDeviceInterrupt(self.partition.0, self.id, index)
        })
        .map_err(|e| e.to_string())
    }
}

impl Drop for Assigned {
    fn drop(&mut self) {
        // SAFETY: the device this value made, which nothing reaches past this: its ranges,
        // where they are mapped, then the device.
        unsafe {
            WHvUnmapVpciDeviceMmioRanges(self.partition.0, self.id);
            WHvDeleteVpciDevice(self.partition.0, self.id);
        }
    }
}
