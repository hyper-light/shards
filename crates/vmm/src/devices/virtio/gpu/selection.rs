//! Exact native identity -> live Vulkan device UUID correlation. The renderer owns
//! immutable UUID selection; this module does not choose devices or own holders.

pub use shards_abi::gpu::{JoinError, MAX_PHYSICAL_GPUS, PhysicalGpu, PhysicalGpuError, check_selection};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Properties {
    pub uuid: [u8; 16],
    pub luid: [u8; 8],
    pub domain: u32,
    pub bus: u32,
    pub device: u32,
    pub function: u32,
    pub luid_valid: u32,
    pub pci_valid: u32,
    pub host_memory_valid: u32,
    pub name: [u8; shards_abi::gpu::GPU_NAME_BYTES],
}

// resources/gpu/native-identity.c's `struct shards_gpu_properties`, whose 308 bytes its own
// _Static_assert holds and the bridge writes through its pointer: checked on every target
// this builds for, not only where its tests run.
const _: () = assert!(std::mem::size_of::<Properties>() == 308);

impl Default for Properties {
    fn default() -> Self {
        Self {
            uuid: [0; 16],
            luid: [0; 8],
            domain: 0,
            bus: 0,
            device: 0,
            function: 0,
            luid_valid: 0,
            pci_valid: 0,
            host_memory_valid: 0,
            name: [0; shards_abi::gpu::GPU_NAME_BYTES],
        }
    }
}

/// The same immutable native properties used to resolve the selector, retained
/// with its context whitelist. PCI always includes the domain; LUID stays bytes.
pub fn resolved(
    physical: PhysicalGpu,
    props: &Properties,
    metal: bool,
) -> Result<shards_abi::gpu::ResolvedGpu, JoinError> {
    physical.check().map_err(JoinError::Selection)?;
    if props.uuid == [0; 16] {
        return Err(JoinError::InvalidUuid);
    }
    let pci = if props.pci_valid != 0 {
        let pci = PhysicalGpu::Pci {
            domain: props.domain,
            bus: u8::try_from(props.bus).map_err(|_| JoinError::InvalidNative)?,
            device: u8::try_from(props.device).map_err(|_| JoinError::InvalidNative)?,
            function: u8::try_from(props.function).map_err(|_| JoinError::InvalidNative)?,
        };
        pci.check().map_err(|_| JoinError::InvalidNative)?;
        Some(pci)
    } else {
        None
    };
    let luid = (props.luid_valid != 0).then_some(props.luid);
    let exact = match physical {
        PhysicalGpu::Pci { .. } => pci == Some(physical),
        PhysicalGpu::MetalRegistry(id) => metal && luid == Some(id.to_be_bytes()),
        PhysicalGpu::Luid(id) => !metal && luid == Some(id.to_le_bytes()),
    };
    if !exact {
        return Err(JoinError::Missing);
    }
    Ok(shards_abi::gpu::ResolvedGpu {
        physical,
        uuid: props.uuid,
        pci,
        luid,
        name: Some(shards_abi::gpu::GpuName::from_buffer(&props.name).map_err(|_| JoinError::InvalidNative)?),
        // The actual memory query fills this independently after exact identity resolution.
        device_local_bytes: None,
    })
}

/// Vulkan DEVICE_LOCAL heaps are distinct entries, not memory types (which may
/// alias a heap). The caller passes each queried heap once; overflow refuses.
pub fn local_heap_bytes(mut heaps: impl Iterator<Item = (u64, bool)>) -> Result<u64, JoinError> {
    heaps.try_fold(0u64, |total, (size, local)| {
        if local {
            total.checked_add(size).ok_or(JoinError::Memory)
        } else {
            Ok(total)
        }
    })
}

fn native(
    props: &Properties,
    metal: bool,
    keys: &mut Vec<(PhysicalGpu, usize)>,
    index: usize,
) -> Result<(), JoinError> {
    if props.pci_valid != 0 {
        let id = PhysicalGpu::Pci {
            domain: props.domain,
            bus: u8::try_from(props.bus).map_err(|_| JoinError::InvalidNative)?,
            device: u8::try_from(props.device).map_err(|_| JoinError::InvalidNative)?,
            function: u8::try_from(props.function).map_err(|_| JoinError::InvalidNative)?,
        };
        id.check().map_err(|_| JoinError::InvalidNative)?;
        keys.push((id, index));
    }
    if props.luid_valid != 0 {
        let id = if metal {
            PhysicalGpu::MetalRegistry(u64::from_be_bytes(props.luid))
        } else {
            PhysicalGpu::Luid(u64::from_le_bytes(props.luid))
        };
        id.check().map_err(|_| JoinError::InvalidNative)?;
        keys.push((id, index));
    }
    Ok(())
}

pub fn resolve(selected: &[PhysicalGpu], host: &[Properties], metal: bool) -> Result<Vec<usize>, JoinError> {
    check_selection(selected).map_err(JoinError::Selection)?;
    if host.len() > MAX_PHYSICAL_GPUS {
        return Err(JoinError::Count);
    }
    let mut resolved = Vec::new();
    resolved
        .try_reserve_exact(selected.len())
        .map_err(|_| JoinError::Allocation)?;
    if selected.is_empty() {
        return Ok(resolved);
    }
    // Two identity-bearing native forms per Vulkan device: PCI and its OS LUID.
    // These per-query indexes are not inventory caches and never choose a device.
    let key_count = host.len().checked_mul(2).ok_or(JoinError::Count)?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(key_count)
        .map_err(|_| JoinError::Allocation)?;
    let mut uuids = Vec::new();
    uuids
        .try_reserve_exact(host.len())
        .map_err(|_| JoinError::Allocation)?;
    let mut used = Vec::new();
    used.try_reserve_exact(host.len())
        .map_err(|_| JoinError::Allocation)?;
    used.resize(host.len(), false);
    for (index, props) in host.iter().enumerate() {
        native(props, metal, &mut keys, index)?;
        uuids.push(props.uuid);
    }
    keys.sort_unstable();
    uuids.sort_unstable();
    for id in selected {
        let first = keys.partition_point(|(native, _)| native < id);
        let last = keys.partition_point(|(native, _)| native <= id);
        if first == last {
            return Err(JoinError::Missing);
        }
        if last.checked_sub(first) != Some(1) {
            return Err(JoinError::Ambiguous);
        }
        let (_, index) = keys.get(first).ok_or(JoinError::Missing)?;
        let seen = used.get_mut(*index).ok_or(JoinError::InvalidNative)?;
        if *seen {
            return Err(JoinError::Aliased);
        }
        let props = host.get(*index).ok_or(JoinError::InvalidNative)?;
        if props.uuid == [0; 16] {
            return Err(JoinError::InvalidUuid);
        }
        let first = uuids.partition_point(|uuid| *uuid < props.uuid);
        let last = uuids.partition_point(|uuid| *uuid <= props.uuid);
        if last.checked_sub(first) != Some(1) {
            return Err(JoinError::Ambiguous);
        }
        *seen = true;
        resolved.push(*index);
    }
    Ok(resolved)
}

pub fn copy_selection(selected: &[PhysicalGpu]) -> Result<Vec<PhysicalGpu>, PhysicalGpuError> {
    check_selection(selected)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(selected.len())
        .map_err(|_| PhysicalGpuError::Allocation)?;
    copy.extend_from_slice(selected);
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_heaps_are_counted_once_without_inventing_budget_or_usage() {
        assert_eq!(
            local_heap_bytes([(4, true), (7, false), (8, true)].into_iter()),
            Ok(12)
        );
        assert_eq!(local_heap_bytes([(0, true)].into_iter()), Ok(0));
        assert_eq!(
            local_heap_bytes([(u64::MAX, false), (8, true)].into_iter()),
            Ok(8)
        );
        assert_eq!(
            local_heap_bytes([(u64::MAX, true), (1, true)].into_iter()),
            Err(JoinError::Memory)
        );
    }

    #[test]
    fn native_report_keeps_renderer_name_with_the_same_complete_uuid_join() {
        let mut props = properties(7);
        let name = b"actual renderer GPU\0";
        props.name[..name.len()].copy_from_slice(name);
        let report = resolved(pci(7), &props, false).unwrap();
        assert_eq!(report.name.unwrap().as_str().unwrap(), "actual renderer GPU");
        assert_eq!(
            report.device_local_bytes, None,
            "memory is not inferred from an identity query"
        );
        props.name.fill(0xff);
        assert_eq!(resolved(pci(7), &props, false), Err(JoinError::InvalidNative));
    }

    fn pci(bus: u8) -> PhysicalGpu {
        PhysicalGpu::Pci {
            domain: u32::MAX,
            bus,
            device: 31,
            function: 7,
        }
    }
    fn properties(bus: u8) -> Properties {
        Properties {
            uuid: [bus; 16],
            domain: u32::MAX,
            bus: u32::from(bus),
            device: 31,
            function: 7,
            pci_valid: 1,
            ..Properties::default()
        }
    }

    #[test]
    fn exact_join_survives_catalog_and_enumeration_order_changes() {
        assert_eq!(
            resolve(&[pci(2)], &[properties(2), properties(1)], false),
            Ok(vec![0])
        );
        assert_eq!(
            resolve(&[pci(2)], &[properties(1), properties(2)], false),
            Ok(vec![1])
        );
        assert_eq!(
            resolve(&[pci(3)], &[properties(1), properties(2)], false),
            Err(JoinError::Missing)
        );
    }

    #[test]
    fn metal_and_windows_native_luids_use_the_documented_byte_order() {
        let id = 0x0102_0304_0506_0708u64;
        let metal = Properties {
            uuid: [7; 16],
            luid: id.to_be_bytes(),
            luid_valid: 1,
            ..Properties::default()
        };
        let windows = Properties {
            luid: id.to_le_bytes(),
            ..metal
        };
        assert_eq!(
            resolve(&[PhysicalGpu::MetalRegistry(id)], &[metal], true),
            Ok(vec![0])
        );
        assert_eq!(resolve(&[PhysicalGpu::Luid(id)], &[windows], false), Ok(vec![0]));
        assert_eq!(
            resolve(&[PhysicalGpu::MetalRegistry(id)], &[windows], false),
            Err(JoinError::Missing)
        );
        let receipt = resolved(PhysicalGpu::MetalRegistry(id), &metal, true).unwrap();
        assert_eq!(receipt.luid, Some(id.to_be_bytes()));
        assert_eq!(receipt.pci, None);
        assert!(resolved(PhysicalGpu::MetalRegistry(id), &windows, false).is_err());
        let receipt = resolved(PhysicalGpu::Luid(id), &windows, false).unwrap();
        assert_eq!(receipt.luid, Some(id.to_le_bytes()));
        assert!(resolved(PhysicalGpu::Luid(id), &metal, false).is_err());
    }

    #[test]
    fn probe_receipt_keeps_complete_pci_and_raw_live_luid_without_conflating_keys() {
        let mut props = properties(2);
        props.luid = 7u64.to_le_bytes();
        props.luid_valid = 1;
        let receipt = resolved(PhysicalGpu::Luid(7), &props, false).unwrap();
        assert_eq!(receipt.physical, PhysicalGpu::Luid(7));
        assert_eq!(receipt.pci, Some(pci(2)));
        assert_eq!(receipt.luid, Some(7u64.to_le_bytes()));
        assert_eq!(receipt.uuid, [2; 16]);
        props.domain = 0;
        assert_eq!(resolved(pci(2), &props, false), Err(JoinError::Missing));
        props.pci_valid = 0;
        let receipt = resolved(PhysicalGpu::Luid(7), &props, false).unwrap();
        assert_eq!(
            receipt.pci, None,
            "unknown domain cannot become an assumed PCI identity"
        );
        props.luid = 8u64.to_le_bytes();
        assert_eq!(
            resolved(PhysicalGpu::Luid(7), &props, false),
            Err(JoinError::Missing)
        );
    }

    #[test]
    fn missing_extension_alias_and_foreign_uuid_collisions_refuse() {
        let mut selected = properties(1);
        assert_eq!(
            resolve(&[pci(1)], &[selected, selected], false),
            Err(JoinError::Ambiguous)
        );
        let mut foreign = properties(2);
        foreign.uuid = selected.uuid;
        assert_eq!(
            resolve(&[pci(1)], &[selected, foreign], false),
            Err(JoinError::Ambiguous)
        );
        selected.pci_valid = 0;
        assert_eq!(resolve(&[pci(1)], &[selected], false), Err(JoinError::Missing));
        selected.pci_valid = 1;
        selected.luid_valid = 1;
        selected.luid = 7u64.to_le_bytes();
        assert_eq!(
            resolve(&[pci(1), PhysicalGpu::Luid(7)], &[selected], false),
            Err(JoinError::Aliased)
        );
    }
}

#[cfg(test)]
mod identity_controls {
    use super::*;

    #[test]
    fn pci_requires_complete_domain_and_stale_luid_is_unknown() {
        let selected = PhysicalGpu::Pci {
            domain: 0x1234,
            bus: 0xc4,
            device: 0,
            function: 0,
        };
        let mut props = Properties {
            uuid: [7; 16],
            domain: 0x1234,
            bus: 0xc4,
            pci_valid: 1,
            luid: 9u64.to_le_bytes(),
            luid_valid: 1,
            ..Properties::default()
        };
        assert_eq!(resolve(&[selected], &[props], false), Ok(vec![0]));
        props.domain = 0;
        assert_eq!(resolve(&[selected], &[props], false), Err(JoinError::Missing));
        assert_eq!(resolve(&[PhysicalGpu::Luid(9)], &[props], false), Ok(vec![0]));
        props.luid = 10u64.to_le_bytes();
        assert_eq!(
            resolve(&[PhysicalGpu::Luid(9)], &[props], false),
            Err(JoinError::Missing)
        );
        props.luid_valid = 0;
        assert_eq!(
            resolve(&[PhysicalGpu::Luid(10)], &[props], false),
            Err(JoinError::Missing)
        );
        props.bus = 256;
        assert_eq!(
            resolve(&[selected], &[props], false),
            Err(JoinError::InvalidNative)
        );
    }
}
