//! A microVM: configuration, lifecycle, and exit reasons, independent of the backend
//! that runs it.

use std::path::PathBuf;

#[cfg(all(hv, target_arch = "aarch64"))]
mod aarch64;
#[cfg(all(hv, target_arch = "aarch64"))]
use aarch64 as machine;
#[cfg(hv)]
mod runtime;
#[cfg(hv)]
pub use runtime::{Handle, Running, check_host, max_vcpus, restore, start};

#[cfg(not(hv))]
mod unsupported;
#[cfg(not(hv))]
pub use unsupported::{Handle, Running, check_host, max_vcpus, restore, start};

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
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    /// Where to write the snapshot when the guest asks for one.
    pub dir: PathBuf,
    pub then: AfterSnapshot,
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
