//! A host GPU named by its exact native identity (D231), the type shards-abi shares with the VM
//! process; here, its `--gpu-device` argument.

use std::fmt;

pub use shards_abi::gpu::{MAX_PHYSICAL_GPUS, PhysicalGpu, PhysicalGpuError, check_selection};

/// Canonical ASCII grammar has a fixed maximum encoded length for each
/// native integer width; reserve both buffers before formatting/copying.
pub fn argument(device: PhysicalGpu) -> Result<std::ffi::OsString, PhysicalGpuError> {
    device.check()?;
    let bound = match device {
        PhysicalGpu::Pci { .. } => "pci:ffffffff:ff:1f.7".len(),
        PhysicalGpu::MetalRegistry(_) => "metal:ffffffffffffffff".len(),
        PhysicalGpu::Luid(_) => "luid:ffffffffffffffff".len(),
    };
    let mut text = String::new();
    text.try_reserve_exact(bound)
        .map_err(|_| PhysicalGpuError::Allocation)?;
    fmt::write(&mut text, format_args!("{device}")).map_err(|_| PhysicalGpuError::Allocation)?;
    let mut argument = std::ffi::OsString::new();
    argument
        .try_reserve_exact(text.len())
        .map_err(|_| PhysicalGpuError::Allocation)?;
    argument.push(text);
    Ok(argument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_argument_preserves_every_native_width() {
        for (id, expected) in [
            (PhysicalGpu::MetalRegistry(u64::MAX), "metal:ffffffffffffffff"),
            (PhysicalGpu::Luid(u64::MAX), "luid:ffffffffffffffff"),
            (
                PhysicalGpu::Pci {
                    domain: u32::MAX,
                    bus: 255,
                    device: 31,
                    function: 7,
                },
                "pci:ffffffff:ff:1f.7",
            ),
        ] {
            assert_eq!(argument(id).unwrap(), expected);
        }
    }
}
