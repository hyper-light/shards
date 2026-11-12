//! Linux guests booted through the `shards` binary, for programs that run their own PID 1
//! in one and reach it over vsock (docs/design/architecture.md D215): a [`GuestSpec`] in, a
//! [`Guest`] out, whose vsock ports give [`Stream`]s, and every refusal typed ([`Refusal`]).
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
pub mod physical;
mod said;
pub mod sim;
mod stream;
#[cfg(unix)]
mod taken;
#[cfg(test)]
mod while_runs;

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../docs/research/measurements/managed-proc-main/allocation.rs"]
mod proc_allocation;

pub use client::{
    ManagedGuest, Shards, attach_managed, boot_managed, connect_borrowed, connect_prepared, prepare_boot,
    recorded_console_bytes,
};
pub use physical::{GpuProbe, GpuProbeError, PhysicalGpu, PhysicalGpuError, ResolvedGpu};

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
    /// A GPU (`--gpu`): Venus over the host's own Vulkan driver, rendered by the GPU's VM
    /// process (D144, D201, D202, D208); a selection below implies it.
    pub gpu: bool,
    /// The host GPUs its Venus contexts render on, each named by its exact native identity
    /// (`--gpu-device`, D231), never a model name or an ordinal; none, those the renderer
    /// finds, as `gpu` alone gives.
    pub gpu_devices: Vec<PhysicalGpu>,
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

/// A pure checked boot plan. Its caller owns the private socket directory,
/// native process/envelope, output drain and terminal receipt. Preparing never
/// spawns, watches, creates files or assumes a native resource grant.
#[derive(Debug)]
pub struct PreparedBoot {
    binary: PathBuf,
    arguments: Vec<std::ffi::OsString>,
    vsock: PathBuf,
    ports: Vec<u32>,
    output_bytes: usize,
}
impl PreparedBoot {
    pub fn binary(&self) -> &std::path::Path {
        &self.binary
    }
    pub fn arguments(&self) -> &[std::ffi::OsString] {
        &self.arguments
    }
    pub fn vsock(&self) -> &std::path::Path {
        &self.vsock
    }
    pub fn ports(&self) -> &[u32] {
        &self.ports
    }
    pub fn output_bytes(&self) -> usize {
        self.output_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchError {
    Binary,
    Socket,
    Port,
    Count,
    Allocation,
    Observer,
    NativeRefused,
}
impl std::fmt::Display for LaunchError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Binary => "managed boot requires an explicit absolute native binary path",
            Self::Socket => {
                "managed boot socket is not an absolute bounded native path or disagrees with identity"
            }
            Self::Port => "managed boot connection port is outside the exact prepared set",
            Self::Count => "managed boot argument/output count exceeds its native representation",
            Self::Allocation => "managed boot preparation storage is unavailable",
            Self::Observer => "native observation has no positive terminal receipt",
            Self::NativeRefused => "the canonical native owner refused the managed operation",
        })
    }
}
impl std::error::Error for LaunchError {}

/// Stdio created by the API's existing bounded drain path. The native owner
/// consumes only these output handles; stdin is its native null device. No extra
/// inherited handles, output socket export or post-spawn placement is permitted.
#[derive(Debug)]
pub struct LaunchIo {
    pub stdout: std::io::PipeWriter,
    pub stderr: std::io::PipeWriter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedStatus {
    Code(i32),
    Signal(i32),
    Unknown,
}

/// A positive native fact, independent of the presentation of an End. Released
/// requires the complete owned VM envelope, including selected GPU/helper lifetime;
/// process exit alone is only Process. An observation failure returns Err.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalScope {
    Process,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedEnd {
    pub status: ManagedStatus,
    pub scope: TerminalScope,
}

/// An existing native receipt, not another PID opener/reaper or holder ledger.
/// Its immutable identity is established under the canonical Creating turn before
/// launch returns. wait_for subscribes to that receipt's sole terminal authority;
/// stop can run concurrently without waiting behind its observation lock.
pub trait ManagedProcess: Send + Sync {
    /// A scope can exist before controller publication. Missing observation is
    /// Observer, never pid/start zero; stop/wait/terminal still own cleanup.
    fn identity(&self) -> Result<&Identity, Refusal>;
    /// Wait for the complete native scope. Some must carry Released; a controller
    /// exit with live helpers remains None until the deadline (terminal exposes
    /// that partial Process fact). Duration::MAX subscribes until an event/error.
    fn wait_for(&self, patience: Duration) -> Result<Option<ManagedEnd>, Refusal>;
    fn stop(&self) -> Result<(), Refusal>;
    fn terminal(&self) -> Result<Option<ManagedEnd>, Refusal>;
}

/// The SAME fallibly allocated native scope owner retained by its canonical slot.
/// Unsizing below transfers a reference; it allocates no wrapper or owner.
pub type ManagedReceipt = triomphe::Arc<dyn ManagedProcess>;

pub fn managed_receipt<T: ManagedProcess + 'static>(owner: triomphe::Arc<T>) -> ManagedReceipt {
    use unsize::CoerceUnsize;
    owner.unsize(unsize::Coercion!(to dyn ManagedProcess))
}

/// Borrowed synchronous launch/attach callbacks, usually over the caller's stack
/// transaction. The returned owner comes from the native slot, never a second
/// process-box record/manager. The API owns draining/presentation, while that same
/// native receipt alone owns observation/reaping.
pub trait ManagedLauncher {
    fn launch(&mut self, plan: &PreparedBoot, io: LaunchIo) -> Result<ManagedReceipt, Refusal>;
    /// Reopen only this recorded identity/envelope, without creating a process or
    /// replacing old stdio. Recovery has no historical output to manufacture.
    fn attach(&mut self, identity: &Identity) -> Result<ManagedReceipt, Refusal>;
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

/// Bounds seeded from records of a host class's runs: macOS-HVF, Linux-KVM and
/// Windows-WHP are recorded apart and chosen as this crate is built; a class with no record
/// seeds from the slowest of any, and every bound rises with what its node sees.
pub mod record {
    /// This host's class.
    pub const CLASS: &str = if cfg!(target_os = "macos") {
        "macOS-HVF"
    } else if cfg!(target_os = "linux") {
        "Linux-KVM"
    } else if cfg!(windows) {
        "Windows-WHP"
    } else {
        "no backend"
    };

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// A bound on a wait that can't wait on a fact alone, from the records of its waits by
    /// host class (`docs/research/measurements/shards-api-waits/run.sh`): twice (the
    /// margin) this class's slowest record; where this class has none, twice the slowest of
    /// any class's, as an unrecorded model seeds from the highest on record; and either
    /// rising to twice the slowest this node has seen ([`Learned::saw`]), never falling.
    #[derive(Debug)]
    pub struct Learned {
        name: &'static str,
        records: &'static [(&'static str, Duration)],
        /// The slowest seen here, in ns.
        seen: AtomicU64,
    }

    impl Learned {
        pub const fn new(name: &'static str, records: &'static [(&'static str, Duration)]) -> Learned {
            Learned {
                name,
                records,
                seen: AtomicU64::new(0),
            }
        }

        /// What it bounds.
        pub fn name(&self) -> &'static str {
            self.name
        }

        /// The bound now: twice the slowest of this class's records, else of any class's,
        /// or of what this node has seen, whichever is longer.
        pub fn get(&self) -> Duration {
            let own = self.records.iter().filter(|(class, _)| *class == CLASS);
            let slowest = |r: &mut dyn Iterator<Item = &(&str, Duration)>| r.map(|(_, d)| *d).max();
            let recorded = slowest(&mut own.clone())
                .or_else(|| slowest(&mut self.records.iter()))
                .unwrap_or_default();
            let seen = Duration::from_nanos(self.seen.load(Ordering::Relaxed));
            recorded.max(seen).saturating_mul(2)
        }

        /// A wait it bounds took `took`: the bound rises to twice it where it is the
        /// slowest yet.
        pub fn saw(&self, took: Duration) {
            let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
            self.seen.fetch_max(ns, Ordering::Relaxed);
        }
    }
}

/// The native operation that could not observe a process identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOperation {
    PidfdOpen,
    KqueueCreate,
    RegisterExit,
    OpenStat,
    ReadStat,
    ProcPidRusage,
    PollExit,
    KeventExit,
    OpenProcess,
    GetProcessTimes,
    WaitForExit,
    GetExitCode,
    QueryScope,
    StopScope,
}

/// A failure of the concrete process-identity producers. Observation failure is
/// not proof that a process has ended or that its PID has been reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessFailure {
    /// The exact native error code; its operation supplies the observation stage.
    Os {
        operation: ProcessOperation,
        code: i32,
    },
    PidOutOfRange {
        operation: ProcessOperation,
    },
    DescriptorOutOfRange {
        returned: i64,
    },
    IdentifierOutOfRange,
    PathBounds,
    /// The reader could not reserve capacity; this does not establish host exhaustion.
    CapacityUnavailable,
    BufferBounds,
    InvalidStartRecord,
    DifferentStart {
        observed: u64,
    },
    Exited,
    UnexpectedWait {
        returned: u32,
    },
    /// An unexpected I/O representation with no native code. Its payload is not
    /// represented here; the observation remains uncertain.
    UnrepresentedIo {
        operation: ProcessOperation,
        kind: io::ErrorKind,
    },
    Poisoned {
        what: &'static str,
    },
}

impl std::fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Os { operation, code } => write!(f, "{operation:?}: OS error {code}"),
            Self::PidOutOfRange { operation } => write!(f, "{operation:?}: PID is not representable"),
            Self::DescriptorOutOfRange { returned } => {
                write!(f, "descriptor {returned} is not representable")
            }
            Self::IdentifierOutOfRange => f.write_str("process event identifier is not representable"),
            Self::PathBounds => f.write_str("process stat pathname bounds"),
            Self::CapacityUnavailable => f.write_str("process stat capacity unavailable"),
            Self::BufferBounds => f.write_str("process stat buffer bounds"),
            Self::InvalidStartRecord => f.write_str("process stat has no valid starttime"),
            Self::DifferentStart { observed } => {
                write!(f, "process started at {observed}: another process's since")
            }
            Self::Exited => f.write_str("the held process has ended"),
            Self::UnexpectedWait { returned } => write!(f, "unexpected process wait result {returned}"),
            Self::UnrepresentedIo { operation, kind } => {
                write!(f, "{operation:?}: unrepresented I/O cause ({kind:?})")
            }
            Self::Poisoned { what } => write!(f, "{what}: poisoned native scope state"),
        }
    }
}

impl std::error::Error for ProcessFailure {}

/// The phase of the shared Firecracker CONNECT handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePhase {
    Dial,
    Write,
    Read,
    ClearTimeouts,
}

/// A reply that does not satisfy the existing CONNECT/OK grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    TooLong,
    NotOk,
}

/// A handshake's phase and observed I/O cause, without allocating error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    Expired {
        phase: HandshakePhase,
    },
    Io {
        phase: HandshakePhase,
        kind: io::ErrorKind,
        code: Option<i32>,
    },
    Closed,
    Reply(ReplyError),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired { phase } => write!(f, "{phase:?}: handshake deadline expired"),
            Self::Io { phase, kind, code } => write!(f, "{phase:?}: {kind:?} (OS {code:?})"),
            Self::Closed => f.write_str("closed unanswered (the guest does not listen there)"),
            Self::Reply(ReplyError::TooLong) => f.write_str("answered past the longest OK, no line"),
            Self::Reply(ReplyError::NotOk) => f.write_str("answered not OK"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Borrowed-connect failures. Process observations use [`Refusal::Process`];
/// unrepresentable patience uses [`Refusal::Bounds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorrowFailure {
    Expired,
    /// Retained for callers of earlier revisions; new patience overflow is Bounds.
    DeadlineOverflow,
    Channel(HandshakeError),
    CapacityUnavailable,
}

impl std::fmt::Display for BorrowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => f.write_str("borrowed connection deadline expired"),
            Self::DeadlineOverflow => f.write_str("borrowed connection deadline is not representable"),
            Self::Channel(error) => error.fmt(f),
            Self::CapacityUnavailable => f.write_str("borrowed connection storage unavailable"),
        }
    }
}

impl std::error::Error for BorrowFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // Its Display is the handshake error's own: the cause after it is that error's.
            Self::Channel(error) => std::error::Error::source(error),
            _ => None,
        }
    }
}

/// What can't be done, and why.
///
/// Exhaustive consumers must handle [`Refusal::Process`], [`Refusal::Borrowed`]
/// and [`Refusal::Allocation`] as well as the existing variants. `Clone`,
/// `PartialEq` and `Eq` remain available; match typed failures rather than text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// This host has no backend shards boots Linux with.
    NoBackend { os: &'static str, arch: &'static str },
    /// A spec past this host's bounds, refused before anything boots.
    Bounds { what: &'static str, why: String },
    /// Its kernel, its init or a disk; or shards, which ended before the guest was ready.
    Boot(String),
    /// What this client does not do yet, by name.
    Unsupported { what: &'static str, why: String },
    /// `CONNECT <port>` refused, or unanswered within its patience.
    Channel { port: u32, why: String },
    /// The guest is gone.
    Ended(End),
    /// No guest runs as the [`Identity`] names: its VM's process has ended, or its pid now names
    /// another process (its start time differs).
    Gone { pid: u32, why: String },
    /// A native process identity could not be held or observed. The failure keeps
    /// its stage and cause; only DifferentStart and Exited assert those outcomes.
    /// Other failures leave observation uncertain; an OS code alone does not
    /// establish a terminal guest state.
    Process {
        pid: u32,
        expected_started: u64,
        failure: ProcessFailure,
    },
    /// A finite borrowed connection failed without acquiring the guest's lifetime.
    Borrowed { port: u32, failure: BorrowFailure },
    /// A post-boot connection resource could not be allocated.
    Allocation { what: &'static str },
    /// The exact native GPU request is malformed or exceeds its protocol bound.
    Gpu(PhysicalGpuError),
    /// The carried renderer has no completed exact native selection receipt.
    GpuProbe(GpuProbeError),
    /// Preparation failed before any native owner launches a process.
    Launch(LaunchError),
    /// Managed output/observer setup failed without allocating diagnostic text.
    ManagedIo {
        operation: &'static str,
        kind: io::ErrorKind,
        code: Option<i32>,
    },
    /// Native scope observation or cleanup failed before a controller identity
    /// was published. This never fabricates a process identity.
    Scope(ProcessFailure),
}

/// What names a booted guest beyond its client's process, so a client that restarts (crashed, say)
/// takes it back with [`Guests::attach`]: its VM's process, by pid and the start time that tells it
/// from a process given that pid since, and the socket its vsock is reached at. A client records it
/// durably once the guest is booted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    /// The VM process's start, as its OS counts it: Linux's `/proc/<pid>/stat` starttime (clock
    /// ticks since boot, proc(5)); macOS's `proc_pid_rusage` `ri_proc_start_abstime` (Mach absolute
    /// time); Windows' `GetProcessTimes` creation time (a FILETIME, 100 ns since 1601).
    pub started: u64,
    /// Where its vsock is reached (`--vsock`'s socket).
    pub vsock: PathBuf,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Scope(failure) => write!(f, "managed scope: {failure}"),
            Refusal::ManagedIo {
                operation,
                kind,
                code,
            } => write!(f, "managed {operation}: {kind:?} (OS {code:?})"),
            Refusal::Launch(why) => write!(f, "managed boot: {why}"),
            Refusal::GpuProbe(why) => write!(f, "GPU probe: {why}"),
            Refusal::Gpu(why) => write!(f, "physical GPU: {why}"),
            Refusal::NoBackend { os, arch } => {
                write!(f, "shards boots no guest on {os} {arch}: it has no backend there")
            }
            Refusal::Bounds { what, why } | Refusal::Unsupported { what, why } => write!(f, "{what}: {why}"),
            Refusal::Boot(why) => write!(f, "the guest did not boot: {why}"),
            Refusal::Channel { port, why } => write!(f, "vsock port {port}: {why}"),
            Refusal::Ended(end) => write!(f, "the guest has ended: {end:?}"),
            Refusal::Gone { pid, why } => write!(f, "no guest runs as pid {pid}: {why}"),
            Refusal::Process {
                pid,
                expected_started,
                failure,
            } => {
                write!(f, "process {pid}, expected start {expected_started}: {failure}")
            }
            Refusal::Borrowed { port, failure } => write!(f, "vsock port {port}: {failure}"),
            Refusal::Allocation { what } => write!(f, "cannot allocate {what}"),
        }
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // Each Display already ends with its failure: the cause after it is the failure's.
            Self::Process { failure, .. } | Self::Scope(failure) => std::error::Error::source(failure),
            Self::Borrowed { failure, .. } => std::error::Error::source(failure),
            _ => None,
        }
    }
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
    /// The OS handle under it, for a caller that waits in its own event loop (epoll, kqueue,
    /// IOCP) rather than in a read: readable when a read won't wait, writable when a write
    /// won't, both once the connection has ended. Borrowed: it stays the stream's, open for as
    /// long as the borrow, so a caller waits on it without `unsafe` and never closes it; one that
    /// adds it to an event set takes it out before the stream drops.
    fn handle(&self) -> Handle<'_>;
    /// With `on`, a read or a write that would wait fails `WouldBlock` at once, for a caller
    /// that waits on [`handle`](Stream::handle); else each waits to its timeout, as by
    /// default. Shared by its clones, as its timeouts are.
    fn set_nonblocking(&self, on: bool) -> io::Result<()>;
    /// Ends one way or both; both, one way at a time (D175).
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
    /// The connection's send and receive buffer sizes in bytes (SO_SNDBUF, SO_RCVBUF), as
    /// the OS reports them: getsockopt(2) of SOL_SOCKET's options (socket(7)); on Windows,
    /// Winsock's getsockopt of the same (learn.microsoft.com, "SOL_SOCKET socket options").
    /// They are the OS's own numbers, not bytes a write is guaranteed to move; the send
    /// figure is the unit one write hands the OS. A zero or negative report is an error, not
    /// a size. Seen: Windows x86_64 (jupiter, 2026-11-06, a probe outside every crate)
    /// 65536 and 65536; macOS 8192 and 8192, its `net.local.stream.sendspace` and
    /// `recvspace` (Darwin 25.4 on adas-macbook-pro-2 and 25.2 on adas-mac-studio-2,
    /// 2026-11-07); Linux twice the value set (socket(7), SO_SNDBUF), from
    /// `net.core.wmem_default` and `rmem_default` where none is set, its measured value
    /// pending.
    fn buffer_sizes(&self) -> io::Result<(usize, usize)>;
    /// A read waits at most this long (`None`: for ever), then fails as std's would.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// A write waits at most this long for room (`None`: for ever).
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

/// The OS handle under a [`Stream`], borrowed from it: a file descriptor on Unix, a Winsock socket
/// on Windows.
#[cfg(unix)]
pub type Handle<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Handle<'a> = std::os::windows::io::BorrowedSocket<'a>;

/// What boots guests.
pub trait Guests: Send + Sync {
    /// Query exact native identities and selected host-visible memory through the
    /// carried renderer's positive context filter ACK. No VM/workload is booted.
    /// Patience is the caller's original finite deadline, never a retry budget.
    fn probe_gpu(&self, spec: &GuestSpec, patience: Duration) -> Result<GpuProbe, Refusal>;
    /// What this host boots with, or the refusal naming the backend it lacks.
    fn backend(&self) -> Result<Backend, Refusal>;
    /// Boots a guest; refused before boot past this host's bounds. It is the caller's
    /// until it drops, which ends it.
    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal>;
    /// Takes back the guest `identity` names, booted from `spec` by a client whose process has
    /// gone since (a crash: nothing of it ran on, so nothing ended the guest), as [`boot`]'s
    /// own: the caller's until it drops, which ends it. On Unix, native identity observation
    /// failures return [`Refusal::Process`], preserving the requested PID/start and exact
    /// [`ProcessFailure`]. An uncertain observation is not proof the guest ended; inspect
    /// the typed cause rather than infer a terminal state from its OS code alone.
    /// A simulator may return [`Refusal::Gone`] or [`Refusal::Ended`]. On Windows a guest's
    /// VM is in a job object that ends with the process that booted it (D141), so no guest
    /// outlives its client there, and attach returns [`Refusal::Gone`].
    ///
    /// [`boot`]: Guests::boot
    fn attach(&self, identity: &Identity, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal>;
}

/// A booted guest. Dropping it ends it.
pub trait Guest: Send + Sync {
    /// Positive native process-exit observation, separate from wait_for's End.
    /// False includes a failed observation. This alone does not certify external
    /// renderer/helper resource reclamation.
    fn process_exited(&self) -> Result<bool, Refusal>;
    /// Only a managed native owner can strengthen process exit into Released.
    /// Legacy standalone observation remains Process even after successful wait.
    fn terminal_scope(&self) -> Result<Option<TerminalScope>, Refusal> {
        Ok(self.process_exited()?.then_some(TerminalScope::Process))
    }

    /// A stream to its vsock `port` (`CONNECT <port>`), within `patience`: until the guest
    /// listens there it is refused, and asked again, so the first that connects says the
    /// guest is ready. One dial is always made, so a patience of zero is that one alone;
    /// `Duration::MAX` is no clock, until the guest listens or ends. A guest that has ended
    /// is refused as soon as it has, whatever patience is left.
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal>;
    /// Ends it at once: its VM's process is killed (SIGKILL; on Windows, its process
    /// ended), and its [`wait_for`](Guest::wait_for) then returns [`End::Stopped`]. Never
    /// refused, as a VM left running is a leak. The VMM has no host-to-guest power-off
    /// (shards-vmm's `vm/runtime.rs`), so no signal asks the guest first: what the VMM has
    /// already written to a writable disk's file survives, and what the guest still held in
    /// its page cache is lost. A clean stop is the client's own: ask the guest, through its
    /// own channel, to sync and power off, `wait_for` its end ([`End::PowerOff`]), and stop
    /// only as the last resort.
    fn stop(&self) -> Result<(), Refusal>;
    /// Its end, once it has come, waiting at most `timeout` (`Duration::MAX`: until it
    /// comes).
    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal>;
    /// What names it for [`Guests::attach`].
    fn identity(&self) -> Result<Identity, Refusal>;
}

/// The backend shards boots Linux guests with on `os` and `arch` (`std::env::consts`'):
/// KVM on Linux, Hypervisor.framework on arm64 macOS, WHP on Windows (arm64's `hv::whp_arm64`,
/// D209), each the VMM's `hv::BACKEND` there; whether a host's hypervisor runs guests is its
/// VM process's `check_host`, as the guest boots. macOS on x86_64 is out of shards' scope.
pub fn backend_of(os: &'static str, arch: &'static str) -> Result<Backend, Refusal> {
    match (os, arch) {
        ("linux", "x86_64" | "aarch64") => Ok(Backend::Kvm),
        ("macos", "aarch64") => Ok(Backend::HypervisorFramework),
        ("windows", "x86_64" | "aarch64") => Ok(Backend::WindowsHypervisorPlatform),
        _ => Err(Refusal::NoBackend { os, arch }),
    }
}

/// This host's backend.
pub fn backend() -> Result<Backend, Refusal> {
    backend_of(std::env::consts::OS, std::env::consts::ARCH)
}

/// The least memory a guest of `arch` boots in, in MiB: shards-vmm's `MIN_MEMORY_MIB`
/// (`crates/vmm/src/vm/aarch64.rs:28`, `crates/vmm/src/vm/x86_64.rs:28`).
pub fn min_memory_mib(arch: &str) -> u64 {
    if arch == "x86_64" { 128 } else { 64 }
}

/// Checks `spec` against the bounds shards-vmm holds every host to, so that both the
/// client and the simulator refuse it before anything boots: at least one vCPU, and memory
/// even and at least the least (`vm/aarch64.rs:414`, `vm/x86_64.rs:361`). How many vCPUs a
/// host takes is its hypervisor's own limit, which shards reads as it starts the VM
/// (`vm/runtime.rs:1059` `check_vcpus`); its refusal comes back as [`Refusal::Bounds`].
pub fn check(spec: &GuestSpec) -> Result<(), Refusal> {
    physical::check_selection(&spec.gpu_devices).map_err(Refusal::Gpu)?;
    if spec.vcpus == 0 {
        return Err(Refusal::Bounds {
            what: "vcpus",
            why: "at least one vCPU is required".into(),
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
    /// past each bound by one (257, odd; `least - 2`).
    fn spec() -> GuestSpec {
        GuestSpec {
            kernel: "k".into(),
            init: Init::Initrd("i".into()),
            cmdline: String::new(),
            vcpus: 1,
            memory_mib: 256,
            gpu: false,
            gpu_devices: Vec::new(),
            disks: vec![],
            shares: vec![],
            ports: vec![1],
            binary: None,
        }
    }

    /// A spec past the bounds shards-vmm holds every host to is refused, naming which: no
    /// vCPU, odd memory, too little (D215).
    #[test]
    fn bounds_are_refused_by_name() {
        let refused = |s: GuestSpec| match check(&s) {
            Err(Refusal::Bounds { what, .. }) => what,
            other => panic!("{other:?}"),
        };
        for gpu in [false, true] {
            let base = GuestSpec { gpu, ..spec() };
            assert!(check(&base).is_ok());
            assert_eq!(
                refused(GuestSpec {
                    vcpus: 0,
                    ..base.clone()
                }),
                "vcpus"
            );
            assert_eq!(
                refused(GuestSpec {
                    memory_mib: 257,
                    ..base.clone()
                }),
                "memory_mib"
            );
            let least = min_memory_mib(std::env::consts::ARCH);
            assert_eq!(
                refused(GuestSpec {
                    memory_mib: least - 2,
                    ..base.clone()
                }),
                "memory_mib"
            );
            assert!(
                check(&GuestSpec {
                    memory_mib: least,
                    ..base
                })
                .is_ok()
            );
        }
    }

    /// More vCPUs than this process may run on, and more ports than the muxer's
    /// connections, pass the client's own check: the VMM bounds vCPUs by its hypervisor's
    /// own limit, and an allowlist names ports, not connections (review 6). Inputs: one past
    /// what std reads, and 1024 ports.
    #[test]
    fn the_hypervisors_bounds_are_its_own_to_refuse() {
        let many = std::thread::available_parallelism().map_or(1, |n| n.get());
        let vcpus = u32::try_from(many).unwrap() + 1;
        assert!(check(&GuestSpec { vcpus, ..spec() }).is_ok());
        let ports = (0..1024).collect();
        assert!(check(&GuestSpec { ports, ..spec() }).is_ok());
    }

    /// A bound is twice its own class's slowest record; a class with none seeds from twice
    /// the slowest of any class's; and either rises to twice the slowest this node has
    /// seen, never falling (review: the per-class rule).
    #[test]
    fn a_bound_seeds_from_the_records_and_rises_with_what_it_sees() {
        let ms = Duration::from_millis;
        static OWN: record::Learned = record::Learned::new(
            "own",
            &[
                (record::CLASS, Duration::from_millis(5)),
                ("another", Duration::from_millis(10)),
            ],
        );
        assert_eq!(OWN.get(), ms(10));
        static UNRECORDED: record::Learned = record::Learned::new(
            "unrecorded",
            &[
                ("one", Duration::from_millis(10)),
                ("another", Duration::from_millis(30)),
            ],
        );
        assert_eq!(UNRECORDED.get(), ms(60));
        UNRECORDED.saw(ms(40));
        assert_eq!(UNRECORDED.get(), ms(80));
        UNRECORDED.saw(ms(1));
        assert_eq!(UNRECORDED.get(), ms(80));
    }

    /// A refusal printed with its chain of causes (each `source()` after its Display, as error
    /// reporters print them) says each cause once: a refusal or failure whose Display already
    /// says its failure gives that failure's own source, not the failure again (D215).
    #[test]
    fn a_refusals_chain_says_each_cause_once() {
        let handshake = HandshakeError::Io {
            phase: HandshakePhase::Read,
            kind: io::ErrorKind::WouldBlock,
            code: Some(11),
        };
        let said = handshake.to_string();
        let refusals = [
            (
                Refusal::Borrowed {
                    port: 7,
                    failure: BorrowFailure::Channel(handshake),
                },
                said,
            ),
            (
                Refusal::Borrowed {
                    port: 7,
                    failure: BorrowFailure::Expired,
                },
                BorrowFailure::Expired.to_string(),
            ),
            (
                Refusal::Process {
                    pid: 1,
                    expected_started: 2,
                    failure: ProcessFailure::Exited,
                },
                ProcessFailure::Exited.to_string(),
            ),
            (
                Refusal::Scope(ProcessFailure::Exited),
                ProcessFailure::Exited.to_string(),
            ),
        ];
        for (refusal, failure) in refusals {
            let mut printed = refusal.to_string();
            let mut cause = std::error::Error::source(&refusal);
            while let Some(c) = cause {
                printed = format!("{printed}: {c}");
                cause = c.source();
            }
            assert_eq!(
                printed.matches(failure.as_str()).count(),
                1,
                "{refusal:?}: {printed}"
            );
        }
    }

    /// The backend each host boots with, the VMM's on each (WHP's arm64 one too, D209), and
    /// the host without one refused naming it (D215).
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
            Ok(Backend::WindowsHypervisorPlatform)
        );
        assert_eq!(min_memory_mib("x86_64"), 128);
        assert_eq!(min_memory_mib("aarch64"), 64);
    }
}
