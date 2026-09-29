//! A microVM: configuration, lifecycle, and exit reasons, independent of the backend
//! that runs it.

use std::path::PathBuf;

#[cfg(all(hv, target_arch = "aarch64"))]
mod aarch64;
#[cfg(all(hv, target_arch = "aarch64"))]
use aarch64 as machine;
#[cfg(all(hv, target_arch = "x86_64"))]
mod x86_64;
#[cfg(all(hv, target_arch = "x86_64"))]
use x86_64 as machine;
/// Whether this build's VMs can be snapshotted and restored: every backend's can.
pub const SNAPSHOTS: bool = cfg!(hv);
/// Whether a VM resumed from its snapshot records its working set, for restores to
/// prefetch: every backend's can, HVF's at stage 2 (hv::hvf::Watch), KVM's from the
/// pages its host maps (vm::x86_64::recorder).
pub const WORKING_SETS: bool = cfg!(hv);
/// Which run records it. HVF's: the one that saves the template, whose command recording
/// slows sixfold (PM M30), on a run that boots anyway. KVM's: the first warm restore
/// without one, for which recording costs nothing, and whose pages later restores touch,
/// where the saving run's they barely do (PM M33).
pub const RESTORES_RECORD: bool = cfg!(hv = "kvm");

#[cfg(hv)]
mod runtime;
#[cfg(hv)]
pub use runtime::{Handle, Running, check_host, max_vcpus, restore, start};

#[cfg(not(hv))]
mod unsupported;
#[cfg(not(hv))]
pub use unsupported::{Handle, Running, check_host, max_vcpus, restore, start};

/// A snapshot's vsock device and the restore's socket path come together or not at all.
#[cfg(hv)]
fn check_vsock(snap: &crate::snapshot::Snapshot, path: Option<&std::path::Path>) -> Result<(), String> {
    match (snap.config.vsock, path) {
        (true, None) => Err(
            "the snapshot has a vsock device: give the restored VM its own socket with --vsock PATH".into(),
        ),
        (false, Some(_)) => Err("the snapshot has no vsock device for --vsock".into()),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    /// A prebuilt initramfs image.
    pub initrd: Option<PathBuf>,
    /// A guest executable to run as PID 1 from a generated initramfs (exclusive with
    /// `initrd`).
    pub init: Option<PathBuf>,
    pub cmdline: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Where guest console (UART) output goes.
    pub console: Console,
    /// virtio-blk disks, in the order the guest enumerates them (vda, vdb, ...).
    pub disks: Vec<Disk>,
    /// What to do when the guest asks for a snapshot; without it, requests are ignored.
    pub snapshot: Option<SnapshotPolicy>,
    /// Read-only virtio-pmem devices backed by these files, in guest order (pmem0, ...).
    pub pmem: Vec<PathBuf>,
    /// A virtio-vsock device whose host side listens at this Unix socket path.
    pub vsock: Option<PathBuf>,
}

impl Config {
    /// A VM with shards' defaults: 1 CPU, 256 MiB, and a console on stdout.
    pub fn new(kernel: PathBuf, init: Option<PathBuf>) -> Config {
        Config {
            kernel,
            initrd: None,
            init,
            cmdline: "console=ttyS0 earlycon panic=-1".into(),
            vcpus: 1,
            memory_mib: 256,
            console: Console::Stdout,
            disks: Vec::new(),
            snapshot: None,
            pmem: Vec::new(),
            vsock: None,
        }
    }
}

/// Starts a VM from a snapshot instead of booting one.
#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// The snapshot directory.
    pub dir: PathBuf,
    pub console: Console,
    /// For snapshots the restored guest asks for.
    pub snapshot: Option<SnapshotPolicy>,
    /// Prepare everything, then wait for [`Handle::release`]: a warm VM whose start
    /// request costs only the release.
    pub hold: bool,
    /// Where this VM's vsock device listens. A snapshot with a vsock device needs one:
    /// the original VM may still hold its own path.
    pub vsock: Option<PathBuf>,
    /// Prefetch the snapshot's working set, if it has one, before the guest runs: for a
    /// restore ahead of its request, which it moves off the request's path (PM M30).
    pub prefetch: bool,
    /// Record a working set for the snapshot, where it has none and this backend records
    /// restores' ([`RESTORES_RECORD`]), until [`Handle::save_working_set`] saves it.
    pub record: bool,
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    /// Where to write the snapshot when the guest asks for one.
    pub dir: PathBuf,
    pub then: AfterSnapshot,
    /// Once resumed from its snapshot, record the pages the guest touches, until
    /// [`Handle::save_working_set`] saves them with the snapshot.
    pub working_set: bool,
}

/// What the VM does once its snapshot is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSnapshot {
    Resume,
    Stop,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stdout,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest powered off (PSCI SYSTEM_OFF on arm64).
    PowerOff,
    /// The guest requested a reset (Linux does on reboot and, with `panic=-1`, on panic).
    Reset,
    /// Stopped by the host.
    Stopped,
    /// Stopped after writing a snapshot ([`AfterSnapshot::Stop`]).
    Snapshotted,
    Error(String),
}

/// Boots a VM and waits for it to exit.
pub fn run(cfg: &Config) -> Result<ExitReason, String> {
    let (handle, running) = start(cfg)?;
    Ok(running.wait(handle))
}
