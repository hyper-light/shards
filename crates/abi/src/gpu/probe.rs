//! Bounded receipt from the carried GPU process after its exact context filter ACK.

use super::{MAX_PHYSICAL_GPUS, PhysicalGpu, PhysicalGpuError, check_selection};
use alloc::{string::String, vec::Vec};
use core::fmt::{self, Write};

const HEADER: &str = "shards-gpu-selection 2";
const LINE_BYTES: usize =
    "metal:ffffffffffffffff\tffffffffffffffffffffffffffffffff\tpci:ffffffff:ff:1f.7\tffffffffffffffff\n"
        .len();
const FIXED_BYTES: usize =
    "shards-gpu-selection 2\nhost-visible 18446744073709551615\ndevices 65536\nend\n".len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedGpu {
    pub physical: PhysicalGpu,
    pub uuid: [u8; 16],
    /// Complete per-device PCI properties, never an ADL assumed segment.
    pub pci: Option<PhysicalGpu>,
    /// Exact native byte order from VkPhysicalDeviceIDProperties when valid.
    pub luid: Option<[u8; 8]>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GpuProbe {
    pub devices: Vec<ResolvedGpu>,
    pub host_visible_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuProbeError {
    Selection(PhysicalGpuError),
    Empty,
    Syntax,
    Count,
    Identity,
    Memory,
    Allocation,
    Unavailable,
    Pending,
}

pub fn probe_bytes(count: usize) -> Result<usize, GpuProbeError> {
    if count == 0 || count > MAX_PHYSICAL_GPUS {
        return Err(GpuProbeError::Count);
    }
    count
        .checked_mul(LINE_BYTES)
        .and_then(|n| n.checked_add(FIXED_BYTES))
        .ok_or(GpuProbeError::Count)
}

pub fn uuid_from_hex(text: &str) -> Result<[u8; 16], GpuProbeError> {
    fixed_hex(text)
}

fn fixed_hex<const N: usize>(text: &str) -> Result<[u8; N], GpuProbeError> {
    if N.checked_mul(2) != Some(text.len()) {
        return Err(GpuProbeError::Syntax);
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => byte.checked_sub(b'0'),
        b'a'..=b'f' => byte.checked_sub(b'a').and_then(|b| b.checked_add(10)),
        b'A'..=b'F' => byte.checked_sub(b'A').and_then(|b| b.checked_add(10)),
        _ => None,
    };
    let mut uuid = [0; N];
    // The checked exact 2*N length above rules out a trailing byte.
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (slot, pair) in uuid.iter_mut().zip(pairs) {
        let high = pair
            .first()
            .copied()
            .and_then(nibble)
            .ok_or(GpuProbeError::Syntax)?;
        let low = pair
            .get(1)
            .copied()
            .and_then(nibble)
            .ok_or(GpuProbeError::Syntax)?;
        *slot = (high << 4) | low;
    }
    Ok(uuid)
}

impl GpuProbe {
    pub fn check(&self, selected: &[PhysicalGpu]) -> Result<(), GpuProbeError> {
        check_selection(selected).map_err(GpuProbeError::Selection)?;
        if selected.is_empty() || self.devices.len() != selected.len() {
            return Err(GpuProbeError::Empty);
        }
        if self.host_visible_bytes == 0 {
            return Err(GpuProbeError::Memory);
        }
        let mut uuids = Vec::new();
        uuids
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        for (resolved, selected) in self.devices.iter().zip(selected) {
            if resolved.physical != *selected || resolved.uuid == [0; 16] {
                return Err(GpuProbeError::Identity);
            }
            if let Some(pci) = resolved.pci
                && (!matches!(pci, PhysicalGpu::Pci { .. }) || pci.check().is_err())
            {
                return Err(GpuProbeError::Identity);
            }
            let matches = match resolved.physical {
                PhysicalGpu::Pci { .. } => resolved.pci == Some(resolved.physical),
                PhysicalGpu::MetalRegistry(id) => {
                    resolved.luid.is_some_and(|raw| u64::from_be_bytes(raw) == id)
                }
                PhysicalGpu::Luid(id) => resolved.luid.is_some_and(|raw| u64::from_le_bytes(raw) == id),
            };
            if !matches {
                return Err(GpuProbeError::Identity);
            }
            uuids.push(resolved.uuid);
        }
        uuids.sort_unstable();
        if uuids.windows(2).any(|pair| pair.first() == pair.last()) {
            return Err(GpuProbeError::Identity);
        }
        Ok(())
    }

    pub fn selectors(&self) -> Result<Vec<PhysicalGpu>, GpuProbeError> {
        probe_bytes(self.devices.len())?;
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        selected.extend(self.devices.iter().map(|device| device.physical));
        self.check(&selected)?;
        Ok(selected)
    }

    pub fn try_clone(&self) -> Result<Self, GpuProbeError> {
        self.selectors()?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| GpuProbeError::Allocation)?;
        devices.extend_from_slice(&self.devices);
        Ok(Self {
            devices,
            host_visible_bytes: self.host_visible_bytes,
        })
    }

    fn render(&self, out: &mut impl Write) -> fmt::Result {
        writeln!(out, "{HEADER}")?;
        writeln!(out, "host-visible {}", self.host_visible_bytes)?;
        writeln!(out, "devices {}", self.devices.len())?;
        for device in &self.devices {
            write!(out, "{}\t", device.physical)?;
            for byte in device.uuid {
                write!(out, "{byte:02x}")?;
            }
            out.write_char('\t')?;
            match device.pci {
                Some(pci) => write!(out, "{pci}")?,
                None => out.write_char('-')?,
            };
            out.write_char('\t')?;
            match device.luid {
                Some(luid) => {
                    for byte in luid {
                        write!(out, "{byte:02x}")?;
                    }
                }
                None => out.write_char('-')?,
            };
            out.write_char('\n')?;
        }
        writeln!(out, "end")
    }

    pub fn encode(&self) -> Result<String, GpuProbeError> {
        self.selectors()?;
        struct Count {
            bytes: usize,
            bound: usize,
        }
        impl Write for Count {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                self.bytes = self
                    .bytes
                    .checked_add(text.len())
                    .filter(|n| *n <= self.bound)
                    .ok_or(fmt::Error)?;
                Ok(())
            }
        }
        let mut count = Count {
            bytes: 0,
            bound: probe_bytes(self.devices.len())?,
        };
        self.render(&mut count).map_err(|_| GpuProbeError::Count)?;
        let mut text = String::new();
        text.try_reserve_exact(count.bytes)
            .map_err(|_| GpuProbeError::Allocation)?;
        self.render(&mut text).map_err(|_| GpuProbeError::Count)?;
        Ok(text)
    }

    pub fn parse(text: &str) -> Result<Self, GpuProbeError> {
        if text.len() > probe_bytes(MAX_PHYSICAL_GPUS)? || !text.ends_with('\n') {
            return Err(GpuProbeError::Syntax);
        }
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(GpuProbeError::Syntax);
        }
        fn field<'a>(line: Option<&'a str>, prefix: &str) -> Result<&'a str, GpuProbeError> {
            line.and_then(|line| line.strip_prefix(prefix))
                .ok_or(GpuProbeError::Syntax)
        }
        let number = |text: &str| {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(GpuProbeError::Syntax);
            }
            text.parse::<u64>().map_err(|_| GpuProbeError::Count)
        };
        let host_visible_bytes = number(field(lines.next(), "host-visible ")?)?;
        let count =
            usize::try_from(number(field(lines.next(), "devices ")?)?).map_err(|_| GpuProbeError::Count)?;
        probe_bytes(count)?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(count)
            .map_err(|_| GpuProbeError::Allocation)?;
        for _ in 0..count {
            let mut fields = lines.next().ok_or(GpuProbeError::Syntax)?.split('\t');
            let native = fields.next().ok_or(GpuProbeError::Syntax)?;
            let uuid = fields.next().ok_or(GpuProbeError::Syntax)?;
            let pci = fields.next().ok_or(GpuProbeError::Syntax)?;
            let luid = fields.next().ok_or(GpuProbeError::Syntax)?;
            if fields.next().is_some() {
                return Err(GpuProbeError::Syntax);
            }
            devices.push(ResolvedGpu {
                physical: native.parse().map_err(GpuProbeError::Selection)?,
                uuid: uuid_from_hex(uuid)?,
                pci: if pci == "-" {
                    None
                } else {
                    Some(pci.parse().map_err(GpuProbeError::Selection)?)
                },
                luid: if luid == "-" { None } else { Some(fixed_hex(luid)?) },
            });
        }
        if lines.next() != Some("end") || lines.next().is_some() {
            return Err(GpuProbeError::Syntax);
        }
        let probe = Self {
            devices,
            host_visible_bytes,
        };
        probe.selectors()?;
        Ok(probe)
    }
}

impl fmt::Display for GpuProbeError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection(why) => why.fmt(out),
            Self::Empty => out.write_str("GPU probe does not contain the exact requested nonempty set"),
            Self::Syntax => out.write_str("GPU probe receipt is malformed or truncated"),
            Self::Count => {
                out.write_str("GPU probe count or byte budget overflows its physical address space")
            }
            Self::Identity => {
                out.write_str("GPU probe native identities or UUIDs disagree, repeat or are unavailable")
            }
            Self::Memory => out.write_str("selected GPU has no host-visible memory for Venus"),
            Self::Allocation => out.write_str("GPU probe storage is unavailable"),
            Self::Unavailable => out.write_str("GPU probe capability has not been initialized"),
            Self::Pending => {
                out.write_str("GPU probe has no completed native result within the caller deadline")
            }
        }
    }
}
impl core::error::Error for GpuProbeError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn completed_receipt_preserves_exact_identity_and_refuses_forgery_or_torn_output() {
        let native = PhysicalGpu::Pci {
            domain: u32::MAX,
            bus: 196,
            device: 0,
            function: 0,
        };
        let probe = GpuProbe {
            devices: vec![ResolvedGpu {
                physical: native,
                uuid: [7; 16],
                pci: Some(native),
                luid: Some(9u64.to_le_bytes()),
            }],
            host_visible_bytes: 4096,
        };
        let text = probe.encode().unwrap();
        assert_eq!(GpuProbe::parse(&text).unwrap(), probe);
        assert_eq!(
            probe.check(&[PhysicalGpu::Luid(196)]),
            Err(GpuProbeError::Identity)
        );
        for end in 0..text.len() {
            assert!(GpuProbe::parse(text.get(..end).unwrap()).is_err());
        }
        for bad in [
            text.replace("devices 1", "devices +1"),
            text.replace("devices 1", "devices 65537"),
            text.replace("host-visible 4096", "host-visible 0"),
            text.replace(
                "07070707070707070707070707070707",
                "00000000000000000000000000000000",
            ),
        ] {
            assert!(GpuProbe::parse(&bad).is_err());
        }
    }
}
