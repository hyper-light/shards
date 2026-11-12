//! A host GPU named by its exact native identity (D231): its PCI address, its Metal registry
//! ID, or its current boot's DXGI LUID; never a model name, an ordinal or an environment
//! filter. One checked grammar, `--gpu-device`'s, for shards-api's client and the VM process.

use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

/// The most GPUs a selection names: a u16's every index, the bound the render server's
/// selection transport holds a context's identities to (D231).
pub const MAX_PHYSICAL_GPUS: usize = 1 << u16::BITS;

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
    if devices.len() > MAX_PHYSICAL_GPUS {
        return Err(PhysicalGpuError::Count);
    }
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
            Self::Count => "physical GPU count exceeds the u16 device-index space",
            Self::Duplicate => "physical GPU selection contains a repeated native identity",
            Self::Allocation => "physical GPU selection storage is unavailable",
        })
    }
}

impl core::error::Error for PhysicalGpuError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};

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
        assert_eq!(
            check_selection(&vec![id; MAX_PHYSICAL_GPUS + 1]),
            Err(PhysicalGpuError::Count)
        );
    }
}
