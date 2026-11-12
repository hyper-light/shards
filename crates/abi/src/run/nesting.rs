//! The nesting grant (D234): a run's own command, the workload, makes boxes in its own
//! microVM, whose only principal it is. Its setup entries, as the daemon sends them and
//! shards-init reads them again against the image as built; the words both refuse it in;
//! and what init tells the host of the workload's readiness ([`kind::READY`]).
//!
//! [`kind::READY`]: super::kind::READY

use alloc::string::String;
use alloc::vec::Vec;

use crate::gpu::{MAX_PHYSICAL_GPUS, PhysicalGpu, PhysicalGpuError};

/// `nesting`, alone: the grant. Its command starts in a leaf of a writable cgroup subtree
/// of its own, with `NOTIFY_SOCKET`, and with its run's deadline and GPUs below.
pub const GRANT: &[u8] = b"nesting";
/// `ready-by=<ns>`: the caller's deadline, in nanoseconds since the Unix epoch on the
/// host's clock, which init moves onto its guest's CLOCK_BOOTTIME as the run arrives.
pub const READY_BY: &[u8] = b"ready-by=";
/// `gpus=<identity>[,<identity>…]`: the GPUs the run names (`--gpu-device`, D232), in
/// [`crate::gpu`]'s grammar, which init writes for its command to read.
pub const GPUS: &[u8] = b"gpus=";

/// A run's grant, as its setup entries give it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grant {
    /// [`READY_BY`]'s deadline, where the run gives one.
    pub ready_by: Option<u64>,
    /// [`GPUS`]'s selection, empty where the run names none.
    pub gpus: Vec<PhysicalGpu>,
}

/// Why a run's grant entries are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// [`READY_BY`] or [`GPUS`], the key named, in a run without [`GRANT`], whose alone
    /// they are.
    Ungranted(&'static str),
    /// An entry, the key named, given twice.
    Twice(&'static str),
    /// [`READY_BY`]'s value is no decimal count of nanoseconds.
    ReadyBy,
    /// [`GPUS`]'s value is no selection in [`crate::gpu`]'s grammar.
    Gpus(PhysicalGpuError),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Ungranted(key) => write!(f, "a {key} entry in a run without the nesting grant"),
            Error::Twice(key) => write!(f, "the nesting grant's {key} entry given twice"),
            Error::ReadyBy => f.write_str("a ready-by that is no count of nanoseconds since the Unix epoch"),
            Error::Gpus(e) => write!(f, "a GPU selection of the nesting grant: {e}"),
        }
    }
}

/// The grant `setup` carries: none where it holds no [`GRANT`].
pub fn read(setup: &[Vec<u8>]) -> Result<Option<Grant>, Error> {
    let (mut granted, mut ready_by, mut gpus) = (false, None, None);
    for entry in setup {
        if entry == GRANT {
            if granted {
                return Err(Error::Twice("nesting"));
            }
            granted = true;
        } else if let Some(at) = entry.strip_prefix(READY_BY) {
            if ready_by.is_some() {
                return Err(Error::Twice("ready-by="));
            }
            ready_by = Some(nanoseconds(at).ok_or(Error::ReadyBy)?);
        } else if let Some(list) = entry.strip_prefix(GPUS) {
            if gpus.is_some() {
                return Err(Error::Twice("gpus="));
            }
            gpus = Some(selection(list).map_err(Error::Gpus)?);
        }
    }
    if !granted {
        return match (ready_by, gpus) {
            (Some(_), _) => Err(Error::Ungranted("ready-by=")),
            (None, Some(_)) => Err(Error::Ungranted("gpus=")),
            (None, None) => Ok(None),
        };
    }
    Ok(Some(Grant {
        ready_by,
        gpus: gpus.unwrap_or_default(),
    }))
}

/// A decimal count, digits alone, within a u64.
fn nanoseconds(text: &[u8]) -> Option<u64> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(text).ok()?.parse().ok()
}

/// `list`, a [`GPUS`] entry's value, as its identities, each checked and none twice, as
/// D232 checks a selection: counted before any is kept, so that a list past
/// [`MAX_PHYSICAL_GPUS`] holds no memory for them.
pub fn selection(list: &[u8]) -> Result<Vec<PhysicalGpu>, PhysicalGpuError> {
    let text = core::str::from_utf8(list).map_err(|_| PhysicalGpuError::Syntax)?;
    let count = text.split(',').count();
    if count > MAX_PHYSICAL_GPUS {
        return Err(PhysicalGpuError::Count);
    }
    let mut gpus = Vec::new();
    gpus.try_reserve_exact(count)
        .map_err(|_| PhysicalGpuError::Allocation)?;
    for identity in text.split(',') {
        gpus.push(identity.parse()?);
    }
    crate::gpu::check_selection(&gpus)?;
    Ok(gpus)
}

/// Where a [`kind::READY`](super::kind::READY) says the workload's readiness moved, as
/// init moves it (D234): ready, on time or late, or its ready-by passed; how far past it,
/// in nanoseconds of its guest's CLOCK_BOOTTIME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// `READY=1` came by the run's ready-by, or in a run that gave none.
    OnTime,
    /// `READY=1` came this far past the ready-by.
    Late(u64),
    /// The ready-by passed with no `READY=1`: init saw it this far past.
    Passed(u64),
}

/// [`Ready`]'s first byte: its state, ready on time, ready late, or ready-by passed.
const ON_TIME: u8 = 1;
const LATE: u8 = 2;
const PASSED: u8 = 3;

impl Ready {
    /// Its state's byte, then how far past, a big-endian u64: 0 for one on time.
    pub fn encode(self) -> [u8; 9] {
        let (state, past) = match self {
            Ready::OnTime => (ON_TIME, 0),
            Ready::Late(past) => (LATE, past),
            Ready::Passed(past) => (PASSED, past),
        };
        let [a, b, c, d, e, f, g, h] = past.to_be_bytes();
        [state, a, b, c, d, e, f, g, h]
    }

    /// What `bytes` say, or `None` unless they are one [`Ready`] encoded.
    pub fn decode(bytes: &[u8]) -> Option<Ready> {
        let [state, a, b, c, d, e, f, g, h] = <[u8; 9]>::try_from(bytes).ok()?;
        let past = u64::from_be_bytes([a, b, c, d, e, f, g, h]);
        match (state, past) {
            (ON_TIME, 0) => Some(Ready::OnTime),
            (LATE, past) => Some(Ready::Late(past)),
            (PASSED, past) => Some(Ready::Passed(past)),
            _ => None,
        }
    }
}

/// Refusing a run that asks for the grant (`--security-opt nesting=true`) of an image
/// whose Agentfile, as built, declares no `NESTING`: the daemon's words, and init's.
pub fn undeclared() -> String {
    "cannot give the command the nesting grant: the image's Agentfile declares no NESTING".into()
}

/// Refusing a run joining the network of one given the grant (`--network container:`,
/// D119): the daemon's words, and init's.
pub fn joiner() -> String {
    "cannot join the network of a container given the nesting grant: the grant makes its command its microVM's only principal".into()
}

/// Refusing the grant to a run joining another's network (D119), which is never its
/// microVM's only principal: init's words.
pub fn joined() -> String {
    "cannot give the nesting grant to a container joining another's network: the grant makes a command its microVM's only principal".into()
}

/// Refusing the grant with `--cgroupns host` (D156): the grant makes the command's own
/// cgroup namespace writable, and the microVM's holds the command's limits and every other
/// cgroup, which nsdelegate keeps init's only from a namespace of the command's own.
pub fn cgroupns_host() -> String {
    "cannot give the command the nesting grant in the microVM's cgroup namespace (--cgroupns host): the grant makes the command's cgroup namespace writable, and the microVM's holds the command's own limits and every other cgroup".into()
}

/// Refusing the grant with `--pid host` (D115): the command in init's PID namespace, whose
/// boxes then no longer end with it (pid_namespaces(7)), and where the grant's CAP_SYS_ADMIN
/// faces init's processes and their namespaces.
pub fn pid_host() -> String {
    "cannot give the command the nesting grant in the microVM's PID namespace (--pid host): its boxes would no longer end with it, and the grant's CAP_SYS_ADMIN would face init's processes and their namespaces".into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use alloc::vec;

    fn entries(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|e| e.as_bytes().to_vec()).collect()
    }

    /// The grant's entries as D234 writes them: none without `nesting`; its deadline and
    /// GPUs with it, the GPUs as D232 reads them; each refused where malformed, twice, or
    /// without the grant; and every other entry left to the rest of the run.
    #[test]
    fn the_grants_entries_are_read_as_d234_writes_them() {
        assert_eq!(read(&entries(&["readonly", "ulimit=nofile=1:2"])), Ok(None));
        assert_eq!(read(&entries(&["nesting"])), Ok(Some(Grant::default())));
        let full = read(&entries(&[
            "tmpfs=/run\0",
            "nesting",
            "ready-by=1794466320362000000",
            "gpus=pci:00000000:01:00.0,metal:00000001000002a1",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(full.ready_by, Some(1_794_466_320_362_000_000));
        assert_eq!(
            full.gpus,
            vec![
                PhysicalGpu::Pci {
                    domain: 0,
                    bus: 1,
                    device: 0,
                    function: 0
                },
                PhysicalGpu::MetalRegistry(0x1_0000_02a1),
            ]
        );
        assert_eq!(
            read(&entries(&["nesting", "ready-by=18446744073709551615"]))
                .unwrap()
                .unwrap()
                .ready_by,
            Some(u64::MAX)
        );
        for (setup, error) in [
            (&["ready-by=1"][..], Error::Ungranted("ready-by=")),
            (&["gpus=metal:1"][..], Error::Ungranted("gpus=")),
            (&["nesting", "nesting"][..], Error::Twice("nesting")),
            (
                &["nesting", "ready-by=1", "ready-by=2"][..],
                Error::Twice("ready-by="),
            ),
            (
                &["nesting", "gpus=metal:1", "gpus=metal:2"][..],
                Error::Twice("gpus="),
            ),
            (&["nesting", "ready-by="][..], Error::ReadyBy),
            (&["nesting", "ready-by=-1"][..], Error::ReadyBy),
            (&["nesting", "ready-by=+1"][..], Error::ReadyBy),
            (&["nesting", "ready-by=1.5"][..], Error::ReadyBy),
            (&["nesting", "ready-by=18446744073709551616"][..], Error::ReadyBy),
            (&["nesting", "gpus="][..], Error::Gpus(PhysicalGpuError::Syntax)),
            (
                &["nesting", "gpus=NVIDIA"][..],
                Error::Gpus(PhysicalGpuError::Syntax),
            ),
            (
                &["nesting", "gpus=metal:1,"][..],
                Error::Gpus(PhysicalGpuError::Syntax),
            ),
            (
                &["nesting", "gpus=metal:0"][..],
                Error::Gpus(PhysicalGpuError::MetalRegistry),
            ),
            (
                &["nesting", "gpus=metal:1,metal:01"][..],
                Error::Gpus(PhysicalGpuError::Duplicate),
            ),
        ] {
            assert_eq!(read(&entries(setup)), Err(error), "{setup:?}");
        }
        assert_eq!(
            read(&[b"nesting".to_vec(), b"gpus=\xff".to_vec()]),
            Err(Error::Gpus(PhysicalGpuError::Syntax))
        );
        // Past D232's bound, refused before any identity is kept.
        let mut many = b"gpus=metal:1".to_vec();
        for _ in 0..MAX_PHYSICAL_GPUS {
            many.extend_from_slice(b",metal:1");
        }
        assert_eq!(
            read(&[b"nesting".to_vec(), many]),
            Err(Error::Gpus(PhysicalGpuError::Count))
        );
    }

    /// READY's payload, each of its three moves read as written, how far past kept whole;
    /// anything else refused, an on-time ready with a lateness among it.
    #[test]
    fn ready_reads_as_written() {
        for ready in [
            Ready::OnTime,
            Ready::Late(1),
            Ready::Late(u64::MAX),
            Ready::Passed(0),
            Ready::Passed(7),
        ] {
            assert_eq!(Ready::decode(&ready.encode()), Some(ready));
        }
        assert_eq!(Ready::OnTime.encode(), [1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Ready::Late(0x0102).encode(), [2, 0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(Ready::Passed(3).encode(), [3, 0, 0, 0, 0, 0, 0, 0, 3]);
        assert_eq!(
            Ready::decode(&[1, 0, 0, 0, 0, 0, 0, 0, 1]),
            None,
            "on time, yet late"
        );
        assert_eq!(Ready::decode(&[0; 9]), None, "no state");
        assert_eq!(Ready::decode(&[4, 0, 0, 0, 0, 0, 0, 0, 0]), None, "no such state");
        assert_eq!(Ready::decode(&[1; 8]), None, "short");
        assert_eq!(Ready::decode(&[2; 10]), None, "long");
    }
}
