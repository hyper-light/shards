//! A host GPU named by its exact native identity (D232): its PCI address, its Metal registry
//! ID, or its current boot's DXGI LUID; never a model name, an ordinal or an environment
//! filter. One checked grammar, `--gpu-device`'s, for shards-api's client and the VM process.

mod probe;
pub use probe::{
    GPU_NAME_BYTES, GpuName, GpuProbe, GpuProbeError, GpuRefusal, GpuReport, ResolvedGpu, fixed_name,
    probe_bytes, report_bytes, uuid_from_hex,
};

use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

/// The bytes one GPU of a selection takes on its way to the renderer: its Vulkan deviceUUID
/// (`VK_UUID_SIZE`, 16 bytes) as 32 hex digits and their NUL (`VIRGL_DEVICE_UUID_STRIDE`,
/// resources/gpu/physical-transport-*.patch).
pub const SELECTION_STRIDE: usize = 16 * 2 + 1;

/// The most GPUs a selection names (devices-09, D232): as many identities as the renderer's
/// transport carries, its payload's length a `uint32_t` (`virgl_device_selection_payload_size`)
/// of [`SELECTION_STRIDE`] bytes each, 130,150,524. The renderer's patches hold a context's
/// selection to the same (`VIRGL_DEVICE_UUID_MAX_COUNT`, `VKR_PHYSICAL_DEVICE_FILTER_MAX_COUNT`),
/// so nothing here takes a selection the renderer refuses. A host's Vulkan devices are counted
/// by Vulkan's own `uint32_t` instead ([`MAX_HOST_GPUS`]), and a selection names each at most
/// once. Below it a count is bounded by what carries it (a receipt's lines, a snapshot's bytes,
/// a command line's arguments) and by the memory reserved for it, fallibly. A consumer's own
/// index space, Hex's scheduler's u16 device index, is that consumer's to hold its selections to.
pub const MAX_PHYSICAL_GPUS: usize = u32::MAX as usize / SELECTION_STRIDE;

/// The most GPUs a host's Vulkan lists: its counts are `uint32_t`
/// (`vkEnumeratePhysicalDevices`'s `pPhysicalDeviceCount`).
pub const MAX_HOST_GPUS: usize = u32::MAX as usize;

/// Whether a selection of `count` GPUs fits the renderer's transport ([`MAX_PHYSICAL_GPUS`]).
pub fn selection_fits(count: usize) -> Result<(), PhysicalGpuError> {
    match count.checked_mul(SELECTION_STRIDE) {
        Some(bytes) if bytes <= u32::MAX as usize => Ok(()),
        _ => Err(PhysicalGpuError::Count),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PhysicalGpu {
    Pci {
        domain: u32,
        bus: u8,
        device: u8,
        function: u8,
    },
    MetalRegistry(u64),
    /// Windows' current-boot DXGI LUID; durable PCI identity should be used where
    /// present, and rejoined to the live LUID/UUID rather than persisted as PCI.
    Luid(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalGpuError {
    Syntax,
    PciAddress,
    MetalRegistry,
    Count,
    Duplicate,
    Allocation,
}

impl PhysicalGpu {
    pub fn check(self) -> Result<(), PhysicalGpuError> {
        match self {
            Self::Pci { device, function, .. } if device >= 32 || function >= 8 => {
                Err(PhysicalGpuError::PciAddress)
            }
            Self::MetalRegistry(0) => Err(PhysicalGpuError::MetalRegistry),
            _ => Ok(()),
        }
    }
}

pub fn check_selection(devices: &[PhysicalGpu]) -> Result<(), PhysicalGpuError> {
    selection_fits(devices.len())?;
    let mut sorted = Vec::new();
    sorted
        .try_reserve_exact(devices.len())
        .map_err(|_| PhysicalGpuError::Allocation)?;
    for device in devices {
        device.check()?;
        sorted.push(*device);
    }
    sorted.sort_unstable();
    let mut previous = None;
    for device in sorted {
        if previous == Some(device) {
            return Err(PhysicalGpuError::Duplicate);
        }
        previous = Some(device);
    }
    Ok(())
}

impl FromStr for PhysicalGpu {
    type Err = PhysicalGpuError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let hex64 = |value: &str| {
            if value.is_empty() || value.len() > 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(PhysicalGpuError::Syntax);
            }
            u64::from_str_radix(value, 16).map_err(|_| PhysicalGpuError::Syntax)
        };
        let device = if let Some(value) = text.strip_prefix("metal:") {
            Self::MetalRegistry(hex64(value)?)
        } else if let Some(value) = text.strip_prefix("luid:") {
            Self::Luid(hex64(value)?)
        } else if let Some(value) = text.strip_prefix("pci:") {
            let mut parts = value.split(':');
            let domain = parts.next().ok_or(PhysicalGpuError::Syntax)?;
            let bus = parts.next().ok_or(PhysicalGpuError::Syntax)?;
            let df = parts.next().ok_or(PhysicalGpuError::Syntax)?;
            if parts.next().is_some() || domain.is_empty() || domain.len() > 8 || bus.len() != 2 {
                return Err(PhysicalGpuError::Syntax);
            }
            let mut df = df.split('.');
            let device = df.next().ok_or(PhysicalGpuError::Syntax)?;
            let function = df.next().ok_or(PhysicalGpuError::Syntax)?;
            if df.next().is_some() || device.len() != 2 || function.len() != 1 {
                return Err(PhysicalGpuError::Syntax);
            }
            if [domain, bus, device, function]
                .iter()
                .any(|part| !part.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Err(PhysicalGpuError::Syntax);
            }
            Self::Pci {
                domain: u32::from_str_radix(domain, 16).map_err(|_| PhysicalGpuError::PciAddress)?,
                bus: u8::from_str_radix(bus, 16).map_err(|_| PhysicalGpuError::PciAddress)?,
                device: u8::from_str_radix(device, 16).map_err(|_| PhysicalGpuError::PciAddress)?,
                function: u8::from_str_radix(function, 16).map_err(|_| PhysicalGpuError::PciAddress)?,
            }
        } else {
            return Err(PhysicalGpuError::Syntax);
        };
        device.check()?;
        Ok(device)
    }
}

impl fmt::Display for PhysicalGpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci {
                domain,
                bus,
                device,
                function,
            } => write!(f, "pci:{domain:08x}:{bus:02x}:{device:02x}.{function:x}"),
            Self::MetalRegistry(id) => write!(f, "metal:{id:016x}"),
            Self::Luid(id) => write!(f, "luid:{id:016x}"),
        }
    }
}

impl fmt::Display for PhysicalGpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax => "expected pci:<domain>:<bus>:<device>.<function>, metal:<hex64> or luid:<hex64>",
            Self::PciAddress => "GPU PCI identity exceeds the native address fields",
            Self::MetalRegistry => "GPU Metal registry identity is unavailable",
            Self::Count => "physical GPU count exceeds what the renderer's selection carries",
            Self::Duplicate => "physical GPU selection contains a repeated native identity",
            Self::Allocation => "physical GPU selection storage is unavailable",
        })
    }
}

impl core::error::Error for PhysicalGpuError {}

/// Why a selection's identities join no exact live Vulkan device of their host's (D232): the
/// VM process's join (shards-vmm's `gpu_selection`), which its client reads typed in its
/// refusal ([`GpuRefusal::Join`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    Selection(PhysicalGpuError),
    Count,
    Allocation,
    Missing,
    Ambiguous,
    Aliased,
    InvalidUuid,
    InvalidNative,
    Memory,
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection(error) => error.fmt(f),
            Self::Count => f.write_str("Vulkan device count exceeds Vulkan's uint32_t count"),
            Self::Allocation => f.write_str("native GPU join storage is unavailable"),
            Self::Missing => f.write_str("a selected native GPU has no exact live Vulkan identity"),
            Self::Ambiguous => f.write_str("native GPU or Vulkan UUID correspondence is ambiguous"),
            Self::Aliased => f.write_str("multiple native selectors resolve to the same physical GPU"),
            Self::InvalidUuid => f.write_str("selected Vulkan GPU has no nonzero device UUID"),
            Self::InvalidNative => {
                f.write_str("native Vulkan GPU properties exceed their identity representation")
            }
            Self::Memory => f.write_str("selected device-local Vulkan heap sizes overflow"),
        }
    }
}

impl core::error::Error for JoinError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn native_identity_roundtrips_without_narrowing() {
        for id in [
            PhysicalGpu::Pci {
                domain: u32::MAX,
                bus: u8::MAX,
                device: 31,
                function: 7,
            },
            PhysicalGpu::MetalRegistry(u64::MAX),
            PhysicalGpu::Luid(u64::MAX),
            PhysicalGpu::Luid(0),
        ] {
            assert_eq!(id.to_string().parse(), Ok(id));
        }
        for bad in [
            "0",
            "NVIDIA",
            "pci:0000:01:20.0",
            "pci:0000:01:00.8",
            "pci:100000000:01:00.0",
            "metal:0",
            "luid:10000000000000000",
            "luid:-1",
            "metal: 1",
            "pci:+fff:01:00.0",
            "pci:0000:+1:00.0",
            "pci:0000:01:+0.0",
        ] {
            assert!(bad.parse::<PhysicalGpu>().is_err(), "{bad}");
        }
    }

    #[test]
    fn duplicate_and_excessive_selections_are_refused() {
        let id = PhysicalGpu::MetalRegistry(7);
        assert_eq!(check_selection(&[id, id]), Err(PhysicalGpuError::Duplicate));
    }

    /// devices-09: a selection fits the renderer's transport, a `uint32_t` of bytes at 33 a
    /// GPU, and no more: its largest count's bytes are within it and one more's past it, and
    /// a count whose bytes overflow is refused rather than wrapped.
    #[test]
    fn a_selection_fits_the_renderers_transport_and_no_more() {
        assert_eq!(MAX_PHYSICAL_GPUS, 130_150_524);
        assert!(MAX_PHYSICAL_GPUS * SELECTION_STRIDE <= u32::MAX as usize);
        assert!((MAX_PHYSICAL_GPUS + 1) * SELECTION_STRIDE > u32::MAX as usize);
        assert_eq!(selection_fits(MAX_PHYSICAL_GPUS), Ok(()));
        assert_eq!(
            selection_fits(MAX_PHYSICAL_GPUS + 1),
            Err(PhysicalGpuError::Count)
        );
        assert_eq!(selection_fits(usize::MAX), Err(PhysicalGpuError::Count));
        // Past Hex's u16 index space, which bounded it before, a selection still fits.
        assert_eq!(selection_fits((1 << u16::BITS) + 1), Ok(()));
    }

    /// devices-09: 65,537 distinct GPUs, one past the u16 index space a selection was held
    /// to, are a selection the renderer's transport carries.
    #[test]
    fn a_selection_past_the_u16_index_space_is_taken() {
        let many: Vec<_> = (1..=(1u64 << u16::BITS) + 1)
            .map(PhysicalGpu::MetalRegistry)
            .collect();
        assert_eq!(check_selection(&many), Ok(()));
    }
}
