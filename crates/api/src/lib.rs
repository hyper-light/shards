//! Linux guests booted through the `shards` binary, for programs that run their own PID 1
//! in one and reach it over vsock (Hex's guest node, its design `docs/design/shards-guest.md`
//! §2 and Appendix A): a [`GuestSpec`] in, a [`Guest`] out, whose vsock ports give
//! [`Stream`]s, and every refusal typed ([`Refusal`]).
//!
//! - [`Shards`], the real one, is a client of the `shards` binary, not of `shards-vmm`: on
//!   macOS the process that calls Hypervisor.framework must be shards' own signed VM process
//!   (D30), so no other program links the VMM. It boots with `shards run --kernel`, the
//!   only interface that boots a caller's own kernel and PID 1: the daemon and its Engine
//!   API boot images under shards-init, from warm pools, as containers (D25-D27), and give
//!   no vsock port of a guest's own. Its vsock is the VM's (`--vsock`, Firecracker's
//!   `CONNECT <port>`, muxer.rs), its life the process's.
//! - [`sim::SimGuests`] stands in for it in tests: a closure as the guest, over in-process
//!   socket pairs, with the same bounds and refusals.
//! - [`conformance`] holds both to one behaviour.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::time::Duration;

mod client;
pub mod conformance;
pub mod sim;
mod stream;

pub use client::Shards;

/// A guest to boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSpec {
    /// Its kernel: an arm64 `Image` or an x86_64 `vmlinux`.
    pub kernel: PathBuf,
    /// What its kernel runs first.
    pub init: Init,
    /// Its kernel's command line.
    pub cmdline: String,
    pub vcpus: u32,
    /// Its memory, in MiB: even, at least [`min_memory_mib`].
    pub memory_mib: u64,
    /// Its disks, `/dev/vda` on, in order.
    pub disks: Vec<Disk>,
    /// Host directories it sees as virtio-fs `shards0`, `shards1`, ..., in order, fixed at
    /// boot.
    pub shares: Vec<Share>,
    /// The guest's vsock ports the host may connect to; any other is refused as
    /// [`Refusal::Channel`].
    pub ports: Vec<u32>,
    /// The `shards` binary to boot it with: else `SHARDS_BIN`, else `shards` on `PATH`. On
    /// macOS a signed shards as installed, run where it is: its VM process holds the
    /// hypervisor entitlement under shards' signature.
    pub binary: Option<PathBuf>,
}

/// What a guest's kernel runs first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Init {
    /// An initramfs (`--initrd`).
    Initrd(PathBuf),
    /// An executable, run as PID 1 from an initramfs shards makes of it (`--init`).
    Executable(PathBuf),
}

/// A disk of a guest's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

/// A host directory a guest sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub dir: PathBuf,
    pub read_only: bool,
}

/// What boots guests on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Kvm,
    HypervisorFramework,
    WindowsHypervisorPlatform,
}

/// How a guest ended: shards-vmm's `ExitReason`, but `Snapshotted`, which no guest booted
/// here asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// It powered off.
    PowerOff,
    /// It asked for a reset (a kernel's panic, with `panic=-1`).
    Reset,
    /// [`Guest::stop`] stopped it.
    Stopped,
    /// Its VM failed, as shards said.
    Failed(String),
}

/// How long [`Guest::stop`] lets a guest's process end after SIGTERM before SIGKILL: what
/// `shards daemon stop` gives a run, Shards' own `STOP_GRACE`
/// (`crates/shards/src/daemon.rs:228-231`), dockerd's default stop timeout (moby
/// `daemon/config/config_linux.go`). `stop` returns within it and the kill's reaping.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// What can't be done, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// This host has no backend shards boots Linux with.
    #[error("shards boots no guest on {os} {arch}: it has no backend there")]
    NoBackend { os: &'static str, arch: &'static str },
    /// A spec past this host's bounds, refused before anything boots.
    #[error("{what}: {why}")]
    Bounds { what: &'static str, why: String },
    /// Its kernel, its init or a disk; or shards, which ended before the guest was ready.
    #[error("the guest did not boot: {0}")]
    Boot(String),
    /// What this client does not do yet, by name.
    #[error("{what}: {why}")]
    Unsupported { what: &'static str, why: String },
    /// `CONNECT <port>` refused, or unanswered within its patience.
    #[error("vsock port {port}: {why}")]
    Channel { port: u32, why: String },
    /// The guest is gone.
    #[error("the guest has ended: {0:?}")]
    Ended(End),
}

/// A connection to a guest's vsock port: a Unix stream socket, on Windows shards' own
/// overlapped one (`shards_afunix`), whose one thread's write never waits for another's
/// read (PM M207). Timeouts are polled for before each read or write on every host, never
/// a socket's own receive timeout, which leaves a Winsock connection indeterminate (PM M207);
/// and a read on macOS waits in poll(2), which sees a peer's half-close that a waiting
/// read can sleep through (PM M236).
pub trait Stream: Read + Write + Send + Sync {
    /// Another handle to the same connection.
    fn try_clone(&self) -> io::Result<Box<dyn Stream>>;
    /// Ends one way or both; both, one way at a time (D175).
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
    /// A read waits at most this long (`None`: for ever), then fails as std's would.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// A write waits at most this long for room (`None`: for ever).
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

/// What boots guests.
pub trait Guests: Send + Sync {
    /// What this host boots with, or the refusal naming the backend it lacks.
    fn backend(&self) -> Result<Backend, Refusal>;
    /// Boots a guest; refused before boot past this host's bounds. It is the caller's
    /// until it drops, which ends it.
    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal>;
}

/// A booted guest. Dropping it ends it.
pub trait Guest: Send + Sync {
    /// A stream to its vsock `port` (`CONNECT <port>`), within `patience`: until the guest
    /// listens there it is refused, and asked again, so the first that connects says the
    /// guest is ready. One dial is always made, so a patience of zero is that one alone;
    /// `Duration::MAX` is no clock, until the guest listens or ends. A guest that has ended
    /// is refused as soon as it has, whatever patience is left.
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal>;
    /// Stops it: its [`wait_for`](Guest::wait_for) then returns [`End::Stopped`].
    fn stop(&self) -> Result<(), Refusal>;
    /// Its end, once it has come, waiting at most `timeout` (`Duration::MAX`: until it
    /// comes).
    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal>;
}

/// The backend shards boots Linux guests with on `os` and `arch` (`std::env::consts`'):
/// KVM on Linux, Hypervisor.framework on arm64 macOS, WHP on x86_64 Windows. macOS on x86_64
/// is out of shards' scope, and Windows on arm64 not yet done (shards' plan step 10).
pub fn backend_of(os: &'static str, arch: &'static str) -> Result<Backend, Refusal> {
    match (os, arch) {
        ("linux", "x86_64" | "aarch64") => Ok(Backend::Kvm),
        ("macos", "aarch64") => Ok(Backend::HypervisorFramework),
        ("windows", "x86_64") => Ok(Backend::WindowsHypervisorPlatform),
        _ => Err(Refusal::NoBackend { os, arch }),
    }
}

/// This host's backend.
pub fn backend() -> Result<Backend, Refusal> {
    backend_of(std::env::consts::OS, std::env::consts::ARCH)
}

/// The least memory a guest of `arch` boots in, in MiB (shards-vmm's `MIN_MEMORY_MIB`,
/// `vm/aarch64.rs` and `vm/x86_64.rs`).
pub fn min_memory_mib(arch: &str) -> u64 {
    if arch == "x86_64" { 128 } else { 64 }
}

/// The most vCPUs a guest has here: the host's CPUs, as std reads them (a cgroup's or
/// affinity's bound included). shards-vmm then holds a guest to its hypervisor's own limit,
/// which is no lower on any backend it has.
pub fn max_vcpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
}

/// The most vsock ports a spec may allow: the connections shards' muxer holds at once
/// (`MAX_CONNECTIONS`, Firecracker's, `devices/virtio/vsock/muxer.rs`).
pub const MAX_PORTS: usize = 1023;

/// Checks `spec` against this host's bounds, as shards-vmm does as it starts a VM
/// (`vm/runtime.rs` `check_vcpus`; `vm/{aarch64,x86_64}.rs`' memory), so that both the
/// client and the simulator refuse it before anything boots.
pub fn check(spec: &GuestSpec) -> Result<(), Refusal> {
    let max = max_vcpus();
    if spec.vcpus == 0 {
        return Err(Refusal::Bounds {
            what: "vcpus",
            why: "at least one vCPU is required".into(),
        });
    }
    if spec.vcpus > max {
        return Err(Refusal::Bounds {
            what: "vcpus",
            why: format!("{} vCPUs requested; this host has {max}", spec.vcpus),
        });
    }
    let least = min_memory_mib(std::env::consts::ARCH);
    if spec.memory_mib < least || !spec.memory_mib.is_multiple_of(2) {
        return Err(Refusal::Bounds {
            what: "memory_mib",
            why: format!(
                "{} MiB: guest memory must be an even number of MiB, at least {least}",
                spec.memory_mib
            ),
        });
    }
    if spec.ports.len() > MAX_PORTS {
        return Err(Refusal::Bounds {
            what: "ports",
            why: format!(
                "{} ports; a guest's vsock holds at most {MAX_PORTS}",
                spec.ports.len()
            ),
        });
    }
    Ok(())
}

/// Refuses `port` as [`Refusal::Channel`] unless `spec` allows it.
fn allowed(ports: &[u32], port: u32) -> Result<(), Refusal> {
    if ports.contains(&port) {
        Ok(())
    } else {
        Err(Refusal::Channel {
            port,
            why: "not among the guest's allowed ports".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs: one vCPU and 256 MiB, within every host's bounds; the cases below step
    /// past each bound by one (257, odd; `least - 2`; `MAX_PORTS + 1` ports).
    fn spec() -> GuestSpec {
        GuestSpec {
            kernel: "k".into(),
            init: Init::Initrd("i".into()),
            cmdline: String::new(),
            vcpus: 1,
            memory_mib: 256,
            disks: vec![],
            shares: vec![],
            ports: vec![1],
            binary: None,
        }
    }

    /// A spec past the host's bounds is refused, naming which: no vCPU, more than the host
    /// has, odd memory, too little, too many ports (Hex HEX-GUEST-9).
    #[test]
    fn bounds_are_refused_by_name() {
        assert!(check(&spec()).is_ok());
        let refused = |s: GuestSpec| match check(&s) {
            Err(Refusal::Bounds { what, .. }) => what,
            other => panic!("{other:?}"),
        };
        assert_eq!(refused(GuestSpec { vcpus: 0, ..spec() }), "vcpus");
        assert_eq!(
            refused(GuestSpec {
                vcpus: max_vcpus() + 1,
                ..spec()
            }),
            "vcpus"
        );
        assert_eq!(
            refused(GuestSpec {
                memory_mib: 257,
                ..spec()
            }),
            "memory_mib"
        );
        let least = min_memory_mib(std::env::consts::ARCH);
        assert_eq!(
            refused(GuestSpec {
                memory_mib: least - 2,
                ..spec()
            }),
            "memory_mib"
        );
        assert!(
            check(&GuestSpec {
                memory_mib: least,
                ..spec()
            })
            .is_ok()
        );
        let ports = (0..=MAX_PORTS as u32).collect();
        assert_eq!(refused(GuestSpec { ports, ..spec() }), "ports");
    }

    /// The backend each host boots with, and the two hosts without one refused naming
    /// them (Hex HEX-GUEST-9).
    #[test]
    fn backends_are_named_per_host() {
        assert_eq!(backend_of("linux", "x86_64"), Ok(Backend::Kvm));
        assert_eq!(backend_of("linux", "aarch64"), Ok(Backend::Kvm));
        assert_eq!(backend_of("macos", "aarch64"), Ok(Backend::HypervisorFramework));
        assert_eq!(
            backend_of("windows", "x86_64"),
            Ok(Backend::WindowsHypervisorPlatform)
        );
        assert_eq!(
            backend_of("macos", "x86_64"),
            Err(Refusal::NoBackend {
                os: "macos",
                arch: "x86_64"
            })
        );
        assert_eq!(
            backend_of("windows", "aarch64"),
            Err(Refusal::NoBackend {
                os: "windows",
                arch: "aarch64"
            })
        );
        assert_eq!(min_memory_mib("x86_64"), 128);
        assert_eq!(min_memory_mib("aarch64"), 64);
    }
}
