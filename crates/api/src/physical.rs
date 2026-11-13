//! A host GPU named by its exact native identity (D232), the type shards-abi shares with the VM
//! process; here, its `--gpu-device` argument.

use std::fmt;

pub(crate) mod report;

pub use shards_abi::gpu::{
    GPU_NAME_BYTES, GpuName, GpuProbe, GpuProbeError, GpuRefusal, GpuReport, JoinError, MAX_PHYSICAL_GPUS,
    PhysicalGpu, PhysicalGpuError, ResolvedGpu, check_selection, probe_bytes, report_bytes,
};

/// The refusal a probe's VM process printed in place of its receipt and failed of (D232),
/// where what it said, `said`, is that record alone.
pub(crate) fn refused(said: &[u8]) -> Option<GpuProbeError> {
    match GpuProbe::parse(std::str::from_utf8(said).ok()?) {
        Err(why @ GpuProbeError::Refused(_)) => Some(why),
        _ => None,
    }
}

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

/// Render only the four GPU-report option words from the writer owned by the
/// FINAL native launcher, after any SCM_RIGHTS/handle transfer or duplication.
/// It reserves/appends these to its prepared base before foreign execution.
#[cfg(unix)]
pub fn report_arguments(
    scope: [u8; 16],
    writer: impl std::os::fd::AsFd,
) -> Result<[std::ffi::OsString; 4], crate::Refusal> {
    use std::os::fd::AsRawFd as _;
    let fd = writer.as_fd().as_raw_fd();
    if fd < 3 {
        return Err(crate::Refusal::Launch(crate::LaunchError::Observer));
    }
    report_words(
        scope,
        usize::try_from(fd).map_err(|_| crate::Refusal::Launch(crate::LaunchError::Count))?,
    )
}

#[cfg(windows)]
pub fn report_arguments(
    scope: [u8; 16],
    writer: impl std::os::windows::io::AsHandle,
) -> Result<[std::ffi::OsString; 4], crate::Refusal> {
    use std::os::windows::io::AsRawHandle as _;
    let handle = writer.as_handle().as_raw_handle().addr();
    if handle == 0 || handle == usize::MAX {
        return Err(crate::Refusal::Launch(crate::LaunchError::Observer));
    }
    report_words(scope, handle)
}

/// Only the final native launcher's owned, transferred read end is encoded.
/// The helper retains the sole write end; this is a lifecycle fence, not physical
/// selection authority or a whole-scope termination receipt.
#[cfg(unix)]
pub fn lifeline_arguments(reader: impl std::os::fd::AsFd) -> Result<[std::ffi::OsString; 2], crate::Refusal> {
    use std::os::fd::AsRawFd as _;
    let fd = reader.as_fd().as_raw_fd();
    if fd < 3 {
        return Err(crate::Refusal::Launch(crate::LaunchError::Observer));
    }
    let mut native = alloc_text("2147483647".len())?; // The positive i32 descriptor width.
    fmt::write(&mut native, format_args!("{fd}"))
        .map_err(|_| crate::Refusal::Launch(crate::LaunchError::Allocation))?;
    Ok([os_text("--host-lifeline")?, os_text(&native)?])
}

fn report_words(scope: [u8; 16], writer: usize) -> Result<[std::ffi::OsString; 4], crate::Refusal> {
    use core::fmt::Write as _;
    if scope == [0; 16] {
        return Err(crate::Refusal::GpuProbe(GpuProbeError::Scope));
    }
    let allocation = || crate::Refusal::Launch(crate::LaunchError::Allocation);
    let mut native = alloc_text("18446744073709551615".len())?;
    write!(&mut native, "{writer}").map_err(|_| allocation())?;
    let mut scope_text = alloc_text(
        scope
            .len()
            .checked_mul(2)
            .ok_or(crate::Refusal::Launch(crate::LaunchError::Count))?,
    )?;
    for byte in scope {
        write!(&mut scope_text, "{byte:02x}").map_err(|_| allocation())?;
    }
    Ok([
        os_text("--gpu-report")?,
        os_text(&native)?,
        os_text("--gpu-report-scope")?,
        os_text(&scope_text)?,
    ])
}

fn alloc_text(bytes: usize) -> Result<String, crate::Refusal> {
    let mut text = String::new();
    text.try_reserve_exact(bytes)
        .map_err(|_| crate::Refusal::Launch(crate::LaunchError::Allocation))?;
    Ok(text)
}

fn os_text(text: &str) -> Result<std::ffi::OsString, crate::Refusal> {
    let mut word = std::ffi::OsString::new();
    word.try_reserve_exact(text.len())
        .map_err(|_| crate::Refusal::Launch(crate::LaunchError::Allocation))?;
    word.push(text);
    Ok(word)
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

    /// D232: what a refused probe said is its refusal where it is that record alone; a
    /// receipt, another failure's words, or a record torn or trailed by more, none.
    #[test]
    fn a_probes_refusal_is_its_record_alone() {
        let refusal = GpuRefusal::Join(JoinError::Missing);
        let record = refusal.encode().unwrap();
        assert_eq!(refused(record.as_bytes()), Some(GpuProbeError::Refused(refusal)));
        let probe = GpuProbe {
            devices: vec![ResolvedGpu {
                physical: PhysicalGpu::MetalRegistry(7),
                uuid: [7; 16],
                pci: None,
                luid: Some(7u64.to_be_bytes()),
                name: None,
                device_local_bytes: None,
            }],
            host_visible_bytes: 4096,
        };
        for said in [
            probe.encode().unwrap(),
            "shards: a selected native GPU has no exact live Vulkan identity\n".into(),
            format!("{record}shards: and more\n"),
            record.replace("missing\n", "missing"),
            String::new(),
        ] {
            assert_eq!(refused(said.as_bytes()), None, "{said:?}");
        }
        assert_eq!(refused(&[0xff, b'\n']), None);
    }

    /// D232: a transferred/duplicated writer can have a different raw value;
    /// the final-launch suffix must follow the actual owned writer, not its sender.
    #[test]
    fn managed_report_argument_follows_the_final_owned_writer() {
        let (reader, writer) = std::io::pipe().unwrap();
        let transferred = writer.try_clone().unwrap();
        let original = report_arguments([3; 16], &writer).unwrap();
        let final_words = report_arguments([3; 16], &transferred).unwrap();
        assert_eq!(final_words[0], "--gpu-report");
        assert_eq!(final_words[2], "--gpu-report-scope");
        assert_eq!(final_words[3], std::ffi::OsString::from("03".repeat(16)));
        assert_ne!(original[1], final_words[1]);
        assert!(report_arguments([0; 16], &transferred).is_err());
        drop((reader, writer, transferred));
    }

    #[cfg(unix)]
    #[test]
    fn managed_lifeline_argument_follows_the_final_transferred_reader() {
        let (reader, writer) = std::io::pipe().unwrap();
        let final_reader = reader.try_clone().unwrap();
        let original = lifeline_arguments(&reader).unwrap();
        let final_words = lifeline_arguments(&final_reader).unwrap();
        assert_eq!(final_words[0], "--host-lifeline");
        assert_ne!(original[1], final_words[1]);
        drop((reader, writer, final_reader));
    }
}
