//! The real [`Guests`]: a client of the `shards` binary. `boot` starts
//! `shards run --kernel … --vsock <socket> --no-console`, a VM process of shards' own (on
//! macOS signed, sandboxed and granted only what its broker grants, D30; on Windows in an
//! AppContainer and a Job that ends with it, D141), with the guest's vsock device's host side
//! at a socket in a directory of this process's own, 0700. `connect` dials that socket and
//! speaks Firecracker's `CONNECT <port>` / `OK <host port>` (`devices/virtio/vsock/muxer.rs`,
//! `conn.rs`). The guest's life is the process's: it exits 0 as the guest powers off, 3 as it
//! asks for a reset (`vm_run.rs` `supervise`), and as it is signalled or killed.
//!
//! What shards writes to its stdout and stderr is diagnostics, never a protocol: its last
//! bytes are kept for a refusal to quote, as many as its longest report takes ([`tail`]). The guest is ready when a `CONNECT` is
//! answered `OK`; until it listens, the handshake is refused or closed and asked again.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::hash::{BuildHasher as _, RandomState};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::stream::{self, Socket};
use crate::{
    BorrowFailure, End, Guest, GuestSpec, Guests, HandshakeError, HandshakePhase, Identity, Init, Refusal,
    ReplyError, RuntimePaths, Stream, allowed, backend, check,
};

/// The guests the `shards` binary boots: [`GuestSpec::binary`], else `SHARDS_BIN`, else
/// `shards` on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Shards;

impl Shards {
    pub fn new() -> Shards {
        Shards
    }

    /// A client's explicit runtime directories (`SHARDS_CACHE`, `SHARDS_HOME`), also given its
    /// managed launcher's VM processes (D232). This provider never inherits the environment.
    pub fn configured(cache: PathBuf, home: PathBuf) -> Result<ConfiguredShards, Refusal> {
        Ok(ConfiguredShards {
            paths: RuntimePaths::new(cache, home)?,
        })
    }
}

impl RuntimePaths {
    pub fn new(cache: PathBuf, home: PathBuf) -> Result<Self, Refusal> {
        for path in [&cache, &home] {
            if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
                return Err(Refusal::Bounds {
                    what: "runtime directories",
                    why: "SHARDS_CACHE and SHARDS_HOME require explicit absolute paths without NUL".into(),
                });
            }
        }
        Ok(Self { cache, home })
    }
    pub fn cache(&self) -> &Path {
        &self.cache
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        Self::new(
            PathBuf::from(argument_copy(self.cache.as_os_str())?),
            PathBuf::from(argument_copy(self.home.as_os_str())?),
        )
    }
}

/// The same client/drain/watch protocol, with exactly its client's explicit helper cache and
/// runtime home (D232). This stores no inventory or holders.
#[derive(Debug)]
pub struct ConfiguredShards {
    paths: RuntimePaths,
}

impl ConfiguredShards {
    pub fn cache(&self) -> &Path {
        self.paths.cache()
    }
    pub fn home(&self) -> &Path {
        self.paths.home()
    }
    pub fn runtime_paths(&self) -> &RuntimePaths {
        &self.paths
    }

    fn environment(&self, command: &mut Command) {
        command
            .env_clear()
            .env("SHARDS_CACHE", self.paths.cache())
            .env("SHARDS_HOME", self.paths.home());
    }
}

/// The most bytes shards' report writes for one byte of the console: it escapes each control
/// character (`vm_run.rs` `last_said`, `char::escape_default`): a C0 one or DEL to at
/// most six (`\u{1f}`, `\u{7f}`), a C1 one (two bytes) to six, a byte that is no UTF-8 to
/// U+FFFD's three.
const ESCAPED: usize = "\\u{7f}".len();
/// How much of what shards says on its stdout and stderr is kept, its last bytes, for a guest
/// that keeps `said` bytes of its console (`--console-said`, D12): the longest report it
/// writes as it ends, a reset's, with `--no-console` its guest's last console bytes escaped
/// and their first said lost (`crates/shards/src/vm_run.rs` `supervise`, `last_said`), so a
/// refusal quotes it whole. Its other reports are one error each.
pub(crate) fn tail(said: usize) -> usize {
    "shards: guest requested a reset, its console's last lines, those before them lost past the "
        .len()
        .saturating_add(usize::MAX.ilog10() as usize + 1)
        .saturating_add(" bytes kept:\n".len())
        .saturating_add(said.saturating_mul(ESCAPED))
        .saturating_add("\n".len())
}
/// The longest answer a handshake reads, `OK <host port>\n` with the port a u32: Firecracker's
/// hybrid vsock answer (firecracker `docs/vsock.md`, "Host-Initiated Connections": `OK
/// <assigned_hostside_port>\n`), as shards' muxer writes it
/// (`crates/vmm/src/devices/virtio/vsock/conn.rs:372`).
const ANSWER: usize = "OK ".len() + "4294967295".len() + "\n".len();
const REQUEST: usize = "CONNECT ".len() + "4294967295".len() + "\n".len();

impl Guests for Shards {
    fn probe_gpu(&self, spec: &GuestSpec, patience: Duration) -> Result<crate::GpuProbe, Refusal> {
        probe_gpu(spec, patience, None)
    }
    fn backend(&self) -> Result<crate::Backend, Refusal> {
        backend()
    }

    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        Ok(Box::new(start(spec)?))
    }

    fn attach(&self, identity: &Identity, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        check(spec)?;
        backend()?;
        Ok(Box::new(take_back(identity, spec)?))
    }
}

impl Guests for ConfiguredShards {
    fn probe_gpu(&self, spec: &GuestSpec, patience: Duration) -> Result<crate::GpuProbe, Refusal> {
        probe_gpu(spec, patience, Some(self))
    }
    fn backend(&self) -> Result<crate::Backend, Refusal> {
        backend()
    }
    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        Ok(Box::new(launch(spec, None, Some(self))?))
    }
    fn attach(&self, identity: &Identity, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        check(spec)?;
        backend()?;
        Ok(Box::new(take_back(identity, spec)?))
    }
}

fn probe_gpu(
    spec: &GuestSpec,
    patience: Duration,
    environment: Option<&ConfiguredShards>,
) -> Result<crate::GpuProbe, Refusal> {
    let deadline = Instant::now()
        .checked_add(patience)
        .filter(|_| !patience.is_zero() && patience != Duration::MAX)
        .ok_or(Refusal::GpuProbe(crate::GpuProbeError::Pending))?;
    let bound = crate::physical::probe_bytes(spec.gpu_devices.len()).map_err(Refusal::GpuProbe)?;
    let guest = launch(spec, Some(bound), environment)?;
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Refusal::GpuProbe(crate::GpuProbeError::Pending))?;
    match guest.wait_for(left)? {
        Some(End::PowerOff) => {}
        Some(end) => return Err(Refusal::Ended(end)),
        None => {
            guest.stop()?;
            return Err(Refusal::GpuProbe(crate::GpuProbeError::Pending));
        }
    }
    if Instant::now() >= deadline || !guest.process_exited()? {
        return Err(Refusal::GpuProbe(crate::GpuProbeError::Pending));
    }
    let mut tail = lock(&guest.shared.tail);
    if let Some(why) = tail.probe_error {
        return Err(Refusal::GpuProbe(why));
    }
    if tail.bytes.len() > bound {
        return Err(Refusal::GpuProbe(crate::GpuProbeError::Count));
    }
    let text = std::str::from_utf8(tail.bytes.make_contiguous())
        .map_err(|_| Refusal::GpuProbe(crate::GpuProbeError::Syntax))?;
    let probe = crate::GpuProbe::parse(text).map_err(Refusal::GpuProbe)?;
    probe.check(&spec.gpu_devices).map_err(Refusal::GpuProbe)?;
    if Instant::now() >= deadline {
        return Err(Refusal::GpuProbe(crate::GpuProbeError::Pending));
    }
    Ok(probe)
}

/// How a guest taken back by [`Guests::attach`] ends where this client did not stop it: its
/// VM process is another's child, whose exit status is its parent's alone.
#[cfg(unix)]
const NOT_ITS_PARENT: &str = "it was taken back, its exit status its parent's, not this client's";

/// The guest `identity` names, taken back: its VM process held by its pid and start
/// (`taken::Held`, from before its start is read), watched to its end on a thread of its own,
/// and dialed at its socket. Booted by the client that lost it, it is ready; it has no output
/// here (its first client read it); and its socket directory is removed as it drops, where
/// it is one this client makes (`PrivateDir::adopt`).
#[cfg(unix)]
fn take_back(identity: &Identity, spec: &GuestSpec) -> Result<ShardsGuest, Refusal> {
    let process =
        crate::taken::Held::find(identity.pid, identity.started).map_err(|failure| Refusal::Process {
            pid: identity.pid,
            expected_started: identity.started,
            failure,
        })?;
    // Shared: its watcher thread waits on it for the guest's life, past this call, as the guest kills it.
    let process = Arc::new(process);
    // Shared: the guest and its watcher thread, which outlives this call, each hold it while either lives.
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            exited: false,
            observed_exited: false,
            stopping: false,
            ready: true,
            end: None,
            said: String::new(),
            ended_as: String::new(),
            status: None,
            managed: None,
            managed_ports: Vec::new(),
        }),
        changed: Condvar::new(),
        tail: Mutex::new(Tail {
            bytes: VecDeque::new(),
            scratch: None,
            probe_error: None,
            keep: 0,
            drained: true,
            output: None,
        }),
        drained: Condvar::new(),
        end_mark: end_mark(),
        probe: false,
        managed_owners: std::sync::atomic::AtomicUsize::new(0),
    });
    let (watched, watcher) = (process.clone(), shared.clone());
    std::thread::Builder::new()
        .name("shards-api-taken".into())
        .spawn(move || {
            let awaited = watched.exited();
            let observed_exited = awaited.is_ok();
            let mut state = lock(&watcher.state);
            state.status = Some(Err(match awaited {
                Ok(()) => NOT_ITS_PARENT.into(),
                Err(e) => format!("waiting for it: {e}"),
            }));
            state.observed_exited = observed_exited;
            state.exited = true;
            watcher.changed.notify_all();
        })
        .map_err(|e| Refusal::Boot(format!("taking it back, a thread to watch it: {e}")))?;
    Ok(ShardsGuest {
        ports: spec.ports.clone(),
        vsock: identity.vsock.clone(),
        pid: identity.pid,
        taken: Some(Taken {
            identity: identity.clone(),
            process,
        }),
        shared,
        _dir: PrivateDir::adopt(&identity.vsock),
    })
}

/// On Windows a guest's VM is in a job that ends with the process that booted it (D141), so
/// no guest outlives its client to be taken back.
#[cfg(windows)]
fn take_back(identity: &Identity, _spec: &GuestSpec) -> Result<ShardsGuest, Refusal> {
    Err(Refusal::Gone {
        pid: identity.pid,
        why: "on Windows a guest's VM ends with the process that booted it (D141): none outlives \
              its client to be taken back"
            .into(),
    })
}

/// Boots `spec`'s guest through shards.
fn start(spec: &GuestSpec) -> Result<ShardsGuest, Refusal> {
    launch(spec, None, None)
}

/// The same bounded output/watcher ownership as a guest; Some is an exact probe
/// byte budget and selects the no-VM CLI mode, never a second process manager.
fn launch(
    spec: &GuestSpec,
    probe: Option<usize>,
    environment: Option<&ConfiguredShards>,
) -> Result<ShardsGuest, Refusal> {
    check(spec)?;
    let configured_binary = if environment.is_some() {
        Some(
            spec.binary
                .as_ref()
                .filter(|binary| binary.is_absolute() && !binary.as_os_str().as_encoded_bytes().contains(&0))
                .ok_or(Refusal::Launch(crate::LaunchError::Binary))?,
        )
    } else {
        None
    };
    backend()?;
    let dir = PrivateDir::new()?;
    let vsock = dir.0.join("v");
    // What its console keeps to quote, as its kernel's records say (D12).
    let said = probe.unwrap_or_else(|| crate::said::kept(&spec.kernel));
    let args = if probe.is_some() {
        probe_arguments(spec)?
    } else {
        arguments(spec, &vsock, said)?
    };
    let binary = if let Some(binary) = configured_binary {
        PathBuf::from(argument_copy(binary.as_os_str())?)
    } else {
        spec.binary
            .clone()
            .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("shards"))
    };
    // One pipe for both, read in the order shards wrote; this process keeps a writer of
    // its own to say, after shards' end, where shards' words end.
    let piped = |e: io::Error| Refusal::Boot(format!("a pipe for shards' output: {e}"));
    let (output, writer) = io::pipe().map_err(piped)?;
    let (stdout, stderr) = (
        writer.try_clone().map_err(piped)?,
        writer.try_clone().map_err(piped)?,
    );
    let marker = end_mark();
    let probe_keep = probe
        .map(|bound| {
            bound
                .checked_add(marker.len())
                .ok_or(Refusal::GpuProbe(crate::GpuProbeError::Count))
        })
        .transpose()?;
    let mut probe_scratch = None;
    let mut retained = VecDeque::new();
    if let Some(bound) = probe_keep {
        let room = bound
            .checked_mul(2)
            .ok_or(Refusal::GpuProbe(crate::GpuProbeError::Count))?;
        retained
            .try_reserve_exact(room)
            .map_err(|_| Refusal::GpuProbe(crate::GpuProbeError::Allocation))?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(bound)
            .map_err(|_| Refusal::GpuProbe(crate::GpuProbeError::Allocation))?;
        scratch.resize(bound, 0);
        probe_scratch = Some(scratch);
    }
    let mut command = Command::new(&binary);
    if let Some(environment) = environment {
        environment.environment(&mut command);
    }
    let child = command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|e| Refusal::Boot(format!("{}: {e}", binary.display())))?;
    // Shared: the guest, its drain and its watcher, threads that outlive this call, each hold it.
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            exited: false,
            observed_exited: false,
            stopping: false,
            ready: false,
            end: None,
            said: String::new(),
            ended_as: String::new(),
            status: None,
            managed: None,
            managed_ports: Vec::new(),
        }),
        changed: Condvar::new(),
        tail: Mutex::new(Tail {
            bytes: retained,
            scratch: probe_scratch,
            probe_error: None,
            keep: probe_keep.unwrap_or_else(|| tail(said)),
            drained: false,
            output: None,
        }),
        drained: Condvar::new(),
        end_mark: marker,
        probe: probe.is_some(),
        managed_owners: std::sync::atomic::AtomicUsize::new(0),
    });
    #[cfg(unix)]
    let pid = child.id();
    // Shared: the watcher, which outlives this call, reaps it; on Windows the guest kills it too.
    let child = Arc::new(Mutex::new(child));
    let started = start_threads(&shared, &child, output, writer);
    if let Err(e) = started {
        let mut child = lock(&child);
        let _ = child.kill();
        let _ = child.wait();
        return Err(Refusal::Boot(format!("a thread to watch shards: {e}")));
    }
    Ok(ShardsGuest {
        ports: spec.ports.clone(),
        vsock,
        #[cfg(unix)]
        pid,
        #[cfg(windows)]
        child,
        #[cfg(unix)]
        taken: None,
        shared,
        _dir: Some(dir),
    })
}

/// `shards run --kernel …` for `spec`, its vsock at `vsock`, its console's last `said` bytes
/// kept to quote. `--kernel` comes first, as the command line knows a direct boot by it
/// (`cli/mod.rs`).
fn argument_copy(value: &std::ffi::OsStr) -> Result<OsString, Refusal> {
    let mut copy = OsString::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    copy.push(value);
    Ok(copy)
}

fn argument_push(args: &mut Vec<OsString>, value: impl AsRef<std::ffi::OsStr>) -> Result<(), Refusal> {
    args.push(argument_copy(value.as_ref())?);
    Ok(())
}

fn decimal(value: impl std::fmt::Display) -> Result<OsString, Refusal> {
    let mut text = String::new();
    // Every supplied value is a u32/u64 or a usize on the supported 64-bit hosts.
    text.try_reserve_exact("18446744073709551615".len())
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    std::fmt::write(&mut text, format_args!("{value}"))
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    argument_copy(std::ffi::OsStr::new(&text))
}

/// The renderer query opens no workload, shares, console recorder or VM. Kernel
/// is retained solely for the existing VM parser's required run grammar.
fn probe_arguments(spec: &GuestSpec) -> Result<Vec<OsString>, Refusal> {
    let count = spec
        .gpu_devices
        .len()
        .checked_mul(2)
        .and_then(|devices| devices.checked_add(4))
        .ok_or(Refusal::Launch(crate::LaunchError::Count))?;
    let mut args = Vec::new();
    args.try_reserve_exact(count)
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    argument_push(&mut args, "run")?;
    argument_push(&mut args, "--kernel")?;
    argument_push(&mut args, &spec.kernel)?;
    argument_push(&mut args, "--gpu-probe")?;
    for device in &spec.gpu_devices {
        argument_push(&mut args, "--gpu-device")?;
        args.push(crate::physical::argument(*device).map_err(Refusal::Gpu)?);
    }
    Ok(args)
}

fn arguments(spec: &GuestSpec, vsock: &Path, said: usize) -> Result<Vec<OsString>, Refusal> {
    let enabled = spec.gpu || !spec.gpu_devices.is_empty();
    // Existing fixed block: run/kernel, init, cmdline/cpus/memory and vsock/console.
    let count = spec
        .disks
        .len()
        .checked_add(spec.shares.len())
        .and_then(|n| n.checked_add(spec.gpu_devices.len()))
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(16))
        .and_then(|n| n.checked_add(usize::from(enabled)))
        .ok_or(Refusal::Launch(crate::LaunchError::Count))?;
    let mut args = Vec::new();
    args.try_reserve_exact(count)
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    argument_push(&mut args, "run")?;
    argument_push(&mut args, "--kernel")?;
    argument_push(&mut args, &spec.kernel)?;
    match &spec.init {
        Init::Initrd(path) => {
            argument_push(&mut args, "--initrd")?;
            argument_push(&mut args, path)?;
        }
        Init::Executable(path) => {
            argument_push(&mut args, "--init")?;
            argument_push(&mut args, path)?;
        }
    }
    argument_push(&mut args, "--cmdline")?;
    argument_push(&mut args, &spec.cmdline)?;
    argument_push(&mut args, "--cpus")?;
    args.push(decimal(spec.vcpus)?);
    argument_push(&mut args, "--memory")?;
    args.push(decimal(spec.memory_mib)?);
    for disk in &spec.disks {
        argument_push(&mut args, "--disk")?;
        args.push(with_mode("disk", &disk.path, disk.read_only)?);
    }
    for share in &spec.shares {
        argument_push(&mut args, "--share")?;
        args.push(with_mode("share", &share.dir, share.read_only)?);
    }
    argument_push(&mut args, "--vsock")?;
    argument_push(&mut args, vsock)?;
    argument_push(&mut args, "--no-console")?;
    argument_push(&mut args, "--console-said")?;
    args.push(decimal(said)?);
    // Keep GPU flags contiguous AFTER the fixed CPU block and its byte golden.
    if enabled {
        argument_push(&mut args, "--gpu")?;
    }
    for device in &spec.gpu_devices {
        argument_push(&mut args, "--gpu-device")?;
        args.push(crate::physical::argument(*device).map_err(Refusal::Gpu)?);
    }
    Ok(args)
}

/// Read the existing kernel console measurements separately from pure preparation.
pub fn recorded_console_bytes(kernel: &Path) -> usize {
    crate::said::kept(kernel)
}

pub fn prepare_boot(
    spec: &GuestSpec,
    binary: &Path,
    vsock: &Path,
    console_bytes: usize,
) -> Result<crate::PreparedBoot, Refusal> {
    check(spec)?;
    if !binary.is_absolute() || binary.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(Refusal::Launch(crate::LaunchError::Binary));
    }
    if !vsock.is_absolute()
        || vsock.as_os_str().len() > SOCKET_PATH
        || vsock.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(Refusal::Launch(crate::LaunchError::Socket));
    }
    u32::try_from(spec.ports.len()).map_err(|_| Refusal::Launch(crate::LaunchError::Count))?;
    let mut ports = Vec::new();
    ports
        .try_reserve_exact(spec.ports.len())
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    ports.extend_from_slice(&spec.ports);
    let output_bytes = console_bytes
        .checked_mul(ESCAPED)
        .and_then(|n| n.checked_add(tail(0)))
        .ok_or(Refusal::Launch(crate::LaunchError::Count))?;
    let mut arguments = arguments(spec, vsock, console_bytes)?;
    arguments
        .try_reserve_exact(1)
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    // Only prepared native launches keep the existing helpers in their original
    // scope. This marker grants no physical identity/device authority.
    argument_push(&mut arguments, "--host-managed")?;
    Ok(crate::PreparedBoot {
        binary: PathBuf::from(argument_copy(binary.as_os_str())?),
        arguments,
        vsock: PathBuf::from(argument_copy(vsock.as_os_str())?),
        ports,
        output_bytes,
        runtime_paths: None,
    })
}

/// Connect only after the native owner has established readiness. This is the
/// existing one-shot borrowed connector; it starts no watcher/reaper or retry loop.
pub fn connect_prepared(
    plan: &crate::PreparedBoot,
    identity: &Identity,
    port: u32,
    patience: Duration,
) -> Result<Box<dyn Stream>, Refusal> {
    if identity.vsock != plan.vsock {
        return Err(Refusal::Launch(crate::LaunchError::Socket));
    }
    if !plan.ports.contains(&port) {
        return Err(Refusal::Launch(crate::LaunchError::Port));
    }
    connect_borrowed(identity, port, patience)
}

/// `path`, with `:ro` where read-only. One that ends in `:ro` itself is refused: shards
/// would read it as read-only, and the path as without it.
fn with_mode(what: &'static str, path: &Path, read_only: bool) -> Result<OsString, Refusal> {
    if path.as_os_str().as_encoded_bytes().ends_with(b":ro") {
        return Err(Refusal::Unsupported {
            what,
            why: format!("{} ends in :ro, which shards reads as read-only", path.display()),
        });
    }
    let mut arg = argument_copy(path.as_os_str())?;
    if read_only {
        arg.try_reserve_exact(":ro".len())
            .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
        arg.push(":ro");
    }
    Ok(arg)
}

/// Where a guest's socket directory goes: `tmp`, where `<tmp>/shards-api-<nonce>-<n>/v` is
/// short enough to bind; else on Unix /tmp; on Windows, which has no such fallback, a
/// refusal naming the bound (review 16).
fn socket_dir(tmp: PathBuf) -> Result<PathBuf, Refusal> {
    let longest = "/shards-api-ffffffffffffffff-18446744073709551615/v".len();
    if tmp.as_os_str().len().saturating_add(longest) <= SOCKET_PATH {
        return Ok(tmp);
    }
    #[cfg(unix)]
    return Ok(PathBuf::from("/tmp"));
    #[cfg(windows)]
    Err(Refusal::Boot(format!(
        "{}: too long for a guest's socket beneath it, {SOCKET_PATH} bytes at most",
        tmp.display()
    )))
}

/// A directory of this process's own for a guest's vsock socket, 0700, removed as it drops.
#[derive(Debug)]
struct PrivateDir(PathBuf);

/// The longest path a Unix socket may have, less its NUL: macOS's `sun_path` holds 104 bytes
/// (xnu `bsd/sys/un.h`, `char sun_path[104]`), Linux's 108 (unix(7), `sun_path[108]`),
/// Windows' 108 (afunix.h, `UNIX_PATH_MAX`): the least of them.
const SOCKET_PATH: usize = 103;

impl PrivateDir {
    fn new() -> Result<PrivateDir, Refusal> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        #[expect(
            clippy::disallowed_methods,
            reason = "a guest's socket directory, made 0700 and removed with its guest, no test's"
        )]
        let tmp = socket_dir(std::env::temp_dir())?;
        // Unique in this process by its counter, and apart from every other process's by
        // the process's nonce: 64 bits of std's `RandomState`, seeded from the OS's random
        // source, which no earlier process of the same pid shares and no other user can
        // guess and take first. So one that exists is refused, not passed over.
        static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let nonce = NONCE.get_or_init(|| RandomState::new().hash_one(std::process::id()));
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = tmp.join(format!("shards-api-{nonce:016x}-{n}"));
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        // Windows: the user's own temp folder, whose ACL is the user's (GetTempPath2W).
        #[cfg(windows)]
        let builder = std::fs::DirBuilder::new();
        builder
            .create(&dir)
            .map(|()| PrivateDir(dir.clone()))
            .map_err(|e| Refusal::Boot(format!("{}: {e}", dir.display())))
    }
}

#[cfg(unix)]
impl PrivateDir {
    /// The directory a guest's socket `vsock` is in, where it is one this client makes (`new`'s
    /// name, in `new`'s directory): a guest taken back removes it as it drops, as the guest it
    /// was made for would have. Else none, and none is removed.
    fn adopt(vsock: &Path) -> Option<PrivateDir> {
        let dir = vsock.parent()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "the directory `new` makes guests' socket directories in, no test's"
        )]
        let tmp = socket_dir(std::env::temp_dir()).ok()?;
        let (nonce, n) = dir
            .file_name()?
            .to_str()?
            .strip_prefix("shards-api-")?
            .split_once('-')?;
        let ours = vsock.file_name()? == "v"
            && dir.parent()? == tmp.as_path()
            && nonce.len() == 16
            && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit());
        ours.then(|| PrivateDir(dir.to_path_buf()))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a guest's watchers and callers share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// Signalled as the process ends.
    changed: Condvar,
    tail: Mutex<Tail>,
    /// Signalled as its output is drained.
    drained: Condvar,
    /// What this process writes into shards' output once shards has ended ([`end_mark`]).
    end_mark: Vec<u8>,
    probe: bool,
    /// API lifetime handles only, distinct from worker Arc refs and native holders.
    managed_owners: std::sync::atomic::AtomicUsize,
}

#[derive(Debug)]
struct State {
    /// The watcher completed; a failed native observation can also complete it.
    exited: bool,
    /// Positive process-exit observation/reap, distinct from an End diagnostic.
    observed_exited: bool,
    /// `stop` or a drop is ending it: its end is `Stopped`.
    stopping: bool,
    /// A `CONNECT` has been answered `OK`.
    ready: bool,
    end: Option<End>,
    /// What shards said last, once it has exited.
    said: String,
    /// How shards ended, with its last words, once reaped.
    ended_as: String,
    /// Its exit status, as the watcher reaped it.
    status: Option<Result<std::process::ExitStatus, String>>,
    /// Pending None -> native completion or failed observer; never a release ledger.
    managed: Option<Result<crate::ManagedEnd, Refusal>>,
    managed_ports: Vec<u32>,
}

#[derive(Debug)]
struct Tail {
    bytes: VecDeque<u8>,
    scratch: Option<Vec<u8>>,
    probe_error: Option<crate::GpuProbeError>,
    /// How many of its last bytes are kept ([`tail`]).
    keep: usize,
    /// Read to the end mark, or to an end or error before it.
    drained: bool,
    /// The read end, once drained: kept open, so the end mark's write meets no closed pipe
    /// (no SIGPIPE).
    output: Option<io::PipeReader>,
}

/// The line that ends shards' words in its output: written by this process after shards has
/// exited, so after everything shards wrote (a process's writes are in the pipe before it
/// can exit), and unguessable by what writes there, a guest's console included: 64 bits of
/// std's `RandomState`, seeded from the OS's random source (std `RandomState::new`).
fn end_mark() -> Vec<u8> {
    let nonce = RandomState::new().hash_one(std::process::id());
    format!("\nshards-api: shards ended {nonce:016x}\n").into_bytes()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the thread that reads shards' output into the tail, and the one that waits for it
/// to exit.
fn start_threads(
    shared: &Arc<Shared>,
    child: &Arc<Mutex<Child>>,
    output: io::PipeReader,
    writer: io::PipeWriter,
) -> io::Result<()> {
    let exit = Exit::of(child)?;
    let drainer = shared.clone();
    std::thread::Builder::new()
        .name("shards-api-drain".into())
        .spawn(move || drain(&drainer, output))?;
    let shared = shared.clone();
    std::thread::Builder::new()
        .name("shards-api-wait".into())
        .spawn(move || exit.watch(&shared, writer))?;
    Ok(())
}

/// Reads `output` to the end mark, keeping its last bytes before it, as many as its tail
/// keeps ([`Tail::keep`]). What follows the
/// mark is not shards': a child it left holding its output (macOS's grants broker, until it
/// exits) holds no refusal back.
fn drain(shared: &Shared, mut output: io::PipeReader) {
    let mark = shared.end_mark.as_slice();
    let keep = lock(&shared.tail).keep;
    // A read never returns more than the tail keeps (never nothing: [`tail`]'s report).
    let scratch = { lock(&shared.tail).scratch.take() };
    let mut buf = match scratch {
        Some(buffer) => buffer,
        None if shared.probe => {
            let mut tail = lock(&shared.tail);
            tail.probe_error = Some(crate::GpuProbeError::Allocation);
            tail.drained = true;
            shared.drained.notify_all();
            return;
        }
        None => vec![0u8; keep],
    };
    loop {
        match output.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut tail = lock(&shared.tail);
                tail.bytes.extend(buf.get(..n).unwrap_or_default());
                // The mark may have begun in an earlier read.
                let from = tail.bytes.len().saturating_sub(n.saturating_add(mark.len()));
                let at = tail
                    .bytes
                    .make_contiguous()
                    .get(from..)
                    .and_then(|b| b.windows(mark.len()).position(|w| w == mark));
                if let Some(at) = at {
                    tail.bytes.truncate(from.saturating_add(at));
                }
                let over = tail.bytes.len().saturating_sub(keep);
                if shared.probe && over != 0 {
                    tail.probe_error = Some(crate::GpuProbeError::Count);
                }
                tail.bytes.drain(..over);
                if at.is_some() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let mut tail = lock(&shared.tail);
    tail.drained = true;
    tail.output = Some(output);
    shared.drained.notify_all();
}

/// The process's exit, awaited without reaping it, so that its pid stays its own (and a
/// signal sent to it reaches no other process) until [`State::exited`] says it has gone.
struct Exit {
    #[cfg(unix)]
    pid: libc::id_t,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    /// The process, reaped once it has exited, under the state's lock.
    child: Arc<Mutex<Child>>,
}

impl Exit {
    fn of(child: &Arc<Mutex<Child>>) -> io::Result<Exit> {
        let process = lock(child);
        #[cfg(unix)]
        return Ok(Exit {
            pid: process.id(),
            child: child.clone(),
        });
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle as _;
            Ok(Exit {
                handle: process.as_handle().try_clone_to_owned()?,
                child: child.clone(),
            })
        }
    }

    /// Waits for the process to exit.
    fn awaited(&self) -> io::Result<()> {
        #[cfg(unix)]
        loop {
            // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of our own child into `info`; WNOWAIT leaves it unreaped.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
            // SAFETY: a process handle this value owns.
            if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    /// Waits for the process to exit, then for its last words, and says how it ended. Its
    /// words end at the end mark this writes after its exit: the drain reaches the mark
    /// through what is in the pipe ahead of it, which is finite, with no clock.
    fn watch(self, shared: &Shared, writer: io::PipeWriter) {
        let awaited = self.awaited();
        let mut tail = finish_drain(shared, writer);
        let said = if shared.probe {
            String::new()
        } else {
            String::from_utf8_lossy(tail.bytes.make_contiguous())
                .trim()
                .to_string()
        };
        drop(tail);
        let mut state = lock(&shared.state);
        if let Err(e) = awaited {
            state.end = Some(End::Failed(format!("waiting for shards: {e}")));
            state.ended_as = format!("waiting for shards: {e}");
        }
        // Reaped under the state's lock as `exited` is set, so no signal sent while it is
        // false reaches another process, and none is left a zombie, dropped or not.
        state.status = Some(lock(&self.child).wait().map_err(|e| e.to_string()));
        state.observed_exited = state.status.as_ref().is_some_and(Result::is_ok);
        state.said = said;
        state.exited = true;
        shared.changed.notify_all();
    }
}

fn finish_drain(shared: &Shared, mut writer: io::PipeWriter) -> MutexGuard<'_, Tail> {
    let drained = lock(&shared.tail).drained;
    // Where the drain has ended (an error), the pipe holds the mark unread, as its
    // reader is kept open.
    if !drained && writer.write_all(&shared.end_mark).is_err() {
        lock(&shared.tail).drained = true;
    }
    drop(writer);
    let mut tail = lock(&shared.tail);
    while !tail.drained {
        tail = shared.drained.wait(tail).unwrap_or_else(PoisonError::into_inner);
    }
    tail
}

/// A guest shards booted, or one taken back by [`Guests::attach`].
struct ShardsGuest {
    ports: Vec<u32>,
    vsock: PathBuf,
    #[cfg(unix)]
    pid: u32,
    /// Ended through it on Windows, which sends a process no signal; the watcher reaps it.
    #[cfg(windows)]
    child: Arc<Mutex<Child>>,
    /// What it was taken back by, where it was: its VM process is not this one's child.
    #[cfg(unix)]
    taken: Option<Taken>,
    shared: Arc<Shared>,
    /// Its socket's directory, removed as it drops: none where a guest taken back was given
    /// a socket in a directory this client does not make.
    _dir: Option<PrivateDir>,
}

/// A guest taken back: the identity it was taken back by, and its VM process, held.
#[cfg(unix)]
struct Taken {
    identity: Identity,
    process: Arc<crate::taken::Held>,
}

impl ShardsGuest {
    /// Its end, once the process has exited and its watcher reaped it.
    fn end(&self, state: &mut State) -> Option<End> {
        if !state.exited {
            return None;
        }
        if state.end.is_none() {
            let status = state
                .status
                .clone()
                .unwrap_or_else(|| Err("no status reaped".into()));
            state.ended_as = match &status {
                Ok(status) => said(&format!("shards {status}"), &state.said),
                Err(e) => format!("shards: {e}"),
            };
            state.end = Some(match status {
                _ if state.stopping => End::Stopped,
                Ok(status) if status.code() == Some(0) => End::PowerOff,
                Ok(status) if status.code() == Some(3) => End::Reset,
                _ => End::Failed(state.ended_as.clone()),
            });
        }
        state.end.clone()
    }

    /// The refusal for a guest that has ended: `Boot`, with shards' exit and last words,
    /// where it failed or reset before any `CONNECT` was answered, else `Ended`.
    fn gone(&self, state: &mut State) -> Option<Refusal> {
        let end = self.end(state)?;
        // A failure or a reset (a kernel's panic) before any answer is the boot's; a power-off
        // or a stop is a normal end.
        if !state.ready && matches!(end, End::Failed(_) | End::Reset) {
            // shards-vmm's own refusal of the vCPUs its hypervisor won't run
            // (`crates/vmm/src/vm/runtime.rs:1065`), as shards reports it.
            if state.said.contains("vCPUs requested; this host supports") {
                return Some(Refusal::Bounds {
                    what: "vcpus",
                    why: state.ended_as.clone(),
                });
            }
            return Some(Refusal::Boot(format!("before it was ready, {}", state.ended_as)));
        }
        Some(Refusal::Ended(end))
    }

    /// Kills the process at once, while it is unreaped (or, taken back, while its exit has
    /// not been seen).
    fn signal(&self, state: &State) {
        if state.exited {
            return;
        }
        #[cfg(unix)]
        if let Some(taken) = &self.taken {
            taken.process.kill();
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            // SAFETY: kill(2) of our own child, unreaped while `exited` is false.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        // Windows sends a process no signal: shards' daemon ends a VM's process as its
        // Job's (shards_ipc `Child::kill`), and `run --kernel`'s VM, in a Job that ends with
        // it (cli/mod.rs `vm`), ends as it does.
        #[cfg(windows)]
        {
            let _ = lock(&self.child).kill();
        }
    }
}

/// When `child` was created: GetProcessTimes' creation time, a FILETIME (100 ns since
/// 1601), as one number.
#[cfg(windows)]
fn created(child: &Child) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle as _;
    created_handle(child.as_raw_handle())
}

#[cfg(windows)]
fn created_handle(handle: std::os::windows::io::RawHandle) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the caller holds this process handle open; FILETIMEs are ours.
    let ok = unsafe {
        GetProcessTimes(
            handle,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// `what`, with what shards said last where it said anything.
fn said(what: &str, said: &str) -> String {
    if said.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {said}")
    }
}

/// The wait before dialing again, `p = sqrt(2 c w)`: pacing that balances one dial's cost `c`
/// against the waiting so far `w`, both measured as this connect runs.
///
/// - `c` is the last refused dial's own work, its connect and its ask, measured each dial;
///   not its wait for the guest's answer, which costs this thread nothing (a muxer holding a
///   request for a guest whose vsock is not up yet waits seconds). On HVF (M464) 5.5 µs at
///   p50 to a socket not bound yet.
/// - `w` is how long this connect has waited so far, and stands for the wait still to come.
/// - Pausing `p` between dials, a wait of `w` takes `w / p` dials, `c w / p` of dialing; and a
///   guest that starts to listen at a uniform moment within a pause is reached `p / 2` late on
///   average. Both are seconds, of this thread and of its caller.
/// - Their sum `f(p) = c w / p + p / 2` is least where `f'(p) = -c w / p² + 1 / 2 = 0`, that
///   is `p² = 2 c w`: `p = sqrt(2 c w)`.
///
/// So a few dials go out early and fewer as the wait grows: at 150 ms waited, 1.3 ms with a
/// 5.5 µs dial and 3.6 ms with a 44 µs one, which dialing holds 0.4% and 1.2% of. A dial to a
/// VM that listens waits for the guest's own answer, so the guest paces it. At most `left`.
fn pause(dialed: Duration, waited: Duration, left: Duration) -> Duration {
    let product = dialed
        .as_nanos()
        .checked_mul(waited.as_nanos())
        .and_then(|p| p.checked_mul(2))
        .unwrap_or(u128::MAX);
    let nanos = u64::try_from(product.isqrt()).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos).min(left)
}

/// Why a handshake failed: worth asking again, or not.
enum Attempt {
    Again(String),
    Refused(String),
    Allocation,
}

/// The existing owned-client String boundary, with fallible growth and no
/// allocating fallback. Borrowed errors never pass through this boundary.
fn message(args: std::fmt::Arguments<'_>) -> Result<String, ()> {
    struct Text(String);
    impl std::fmt::Write for Text {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.try_reserve(text.len()).map_err(|_| std::fmt::Error)?;
            self.0.push_str(text);
            Ok(())
        }
    }
    let mut text = Text(String::new());
    std::fmt::write(&mut text, args).map_err(|_| ())?;
    Ok(text.0)
}

impl HandshakeError {
    fn io(phase: HandshakePhase, source: io::Error) -> Self {
        Self::Io {
            phase,
            kind: source.kind(),
            code: source.raw_os_error(),
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::Expired { .. } | Self::Closed => true,
            Self::Io {
                phase: HandshakePhase::Dial,
                kind,
                code,
            } => {
                matches!(
                    kind,
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) || {
                    #[cfg(windows)]
                    {
                        *code == Some(windows_sys::Win32::Networking::WinSock::WSAENOBUFS)
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = code;
                        false
                    }
                }
            }
            Self::Io {
                phase: HandshakePhase::Write | HandshakePhase::Read,
                ..
            } => true,
            Self::Io { .. } | Self::Reply(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HandshakeWait {
    Ready,
    Until(Instant),
    Unbounded,
}

fn remaining(wait: HandshakeWait, phase: HandshakePhase) -> Result<Option<Duration>, HandshakeError> {
    match wait {
        HandshakeWait::Until(at) => {
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                Err(HandshakeError::Expired { phase })
            } else {
                Ok(Some(left))
            }
        }
        HandshakeWait::Ready | HandshakeWait::Unbounded => Ok(None),
    }
}

/// Existing owned-client adapter: zero and unrepresentable patience keep their
/// documented one-dial/no-clock behavior. Borrowed calls use a finite deadline directly.
fn handshake(
    vsock: &Path,
    port: u32,
    wait: Option<Duration>,
    cost: &mut Duration,
) -> Result<Socket, Attempt> {
    let deadline = wait
        .map(|wait| Instant::now().checked_add(wait).ok_or(()))
        .transpose()
        .map_err(
            |()| match message(format_args!("{}", BorrowFailure::DeadlineOverflow)) {
                Ok(why) => Attempt::Refused(why),
                Err(()) => Attempt::Allocation,
            },
        )?;
    let wait = deadline.map_or(HandshakeWait::Unbounded, HandshakeWait::Until);
    handshake_until(vsock, port, wait, cost).map_err(|error| {
        if matches!(
            error,
            HandshakeError::Io {
                kind: io::ErrorKind::OutOfMemory,
                ..
            }
        ) {
            return Attempt::Allocation;
        }
        let why = match message(format_args!("{error}")) {
            Ok(why) => why,
            Err(()) => return Attempt::Allocation,
        };
        if error.retryable() {
            Attempt::Again(why)
        } else {
            Attempt::Refused(why)
        }
    })
}

/// One shared CONNECT/OK exchange. Each partial I/O uses the original absolute
/// deadline; reading only through the newline leaves subsequent payload untouched.
fn handshake_until(
    vsock: &Path,
    port: u32,
    wait: HandshakeWait,
    cost: &mut Duration,
) -> Result<Socket, HandshakeError> {
    use HandshakePhase::{ClearTimeouts, Dial, Read, Write};
    let began = Instant::now();
    remaining(wait, Dial)?;
    let connected = match wait {
        HandshakeWait::Ready => stream::connect_nowait(vsock),
        HandshakeWait::Until(at) => stream::connect_until(vsock, Some(at)),
        HandshakeWait::Unbounded => stream::connect_until(vsock, None),
    };
    let mut s = connected.map_err(|source| {
        *cost = began.elapsed();
        HandshakeError::io(Dial, source)
    })?;
    let mut request = [0; REQUEST];
    let mut unused = request.as_mut_slice();
    writeln!(&mut unused, "CONNECT {port}").map_err(|source| HandshakeError::io(Write, source))?;
    let used = REQUEST.saturating_sub(unused.len());
    let mut ask = request
        .get(..used)
        .ok_or(HandshakeError::Reply(ReplyError::NotOk))?;
    while !ask.is_empty() {
        let sent = match wait {
            HandshakeWait::Ready => s.write_nowait(ask),
            _ => {
                Stream::set_write_timeout(&s, remaining(wait, Write)?)
                    .map_err(|source| HandshakeError::io(Write, source))?;
                s.write(ask)
            }
        };
        *cost = began.elapsed();
        remaining(wait, Write)?;
        match sent {
            Ok(0) => return Err(HandshakeError::io(Write, io::ErrorKind::WriteZero.into())),
            Ok(n) => ask = ask.get(n..).ok_or(HandshakeError::Reply(ReplyError::NotOk))?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !matches!(wait, HandshakeWait::Ready) => {}
            Err(source) => return Err(HandshakeError::io(Write, source)),
        }
    }
    let mut line = [0; ANSWER];
    let mut used = 0usize;
    loop {
        let byte = line
            .get_mut(used..used.saturating_add(1))
            .ok_or(HandshakeError::Reply(ReplyError::TooLong))?;
        let read = match wait {
            HandshakeWait::Ready => s.read_nowait(byte),
            _ => {
                Stream::set_read_timeout(&s, remaining(wait, Read)?)
                    .map_err(|source| HandshakeError::io(Read, source))?;
                s.read(byte)
            }
        };
        remaining(wait, Read)?;
        match read {
            Ok(0) => return Err(HandshakeError::Closed),
            Ok(_) => {
                used = used.saturating_add(1);
                if byte.first() == Some(&b'\n') {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted && !matches!(wait, HandshakeWait::Ready) => {}
            Err(source) => return Err(HandshakeError::io(Read, source)),
        }
    }
    let ok = line
        .get(..used)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|answer| answer.strip_prefix("OK "))
        .is_some_and(|n| n.trim_end().parse::<u32>().is_ok());
    if !ok {
        return Err(HandshakeError::Reply(ReplyError::NotOk));
    }
    Stream::set_write_timeout(&s, None)
        .and_then(|()| Stream::set_read_timeout(&s, None))
        .map_err(|source| HandshakeError::io(ClearTimeouts, source))?;
    remaining(wait, ClearTimeouts)?;
    Ok(s)
}

/// Connect to a running guest without acquiring its lifetime.
///
/// Zero patience makes one readiness-only attempt: connect and every handshake
/// operation must complete without waiting. Pending or interrupted I/O returns
/// its typed phase/cause and closes the connection. Positive patience starts one
/// deadline before identity validation; unrepresentable patience returns Bounds
/// before identity or dial. There is one dial and no retry. Dropping the returned stream
/// closes only that connection. The supplied identity/socket remain trusted input;
/// matching a PID/start is not authentication of the socket's peer.
///
/// Synchronous OS calls and scheduling are not hard real-time bounded. Readiness
/// waits use the original deadline, and late completion is refused.
pub fn connect_borrowed(
    identity: &Identity,
    port: u32,
    patience: Duration,
) -> Result<Box<dyn Stream>, Refusal> {
    let began = Instant::now();
    let refused = |failure| Refusal::Borrowed { port, failure };
    let wait = if patience.is_zero() {
        HandshakeWait::Ready
    } else {
        let deadline = began
            .checked_add(patience)
            .filter(|_| patience != Duration::MAX)
            .ok_or_else(|| {
                message(format_args!("{}", BorrowFailure::DeadlineOverflow))
                    .map(|why| Refusal::Bounds {
                        what: "patience",
                        why,
                    })
                    .unwrap_or(Refusal::Allocation {
                        what: "connection diagnostic",
                    })
            })?;
        HandshakeWait::Until(deadline)
    };
    let process_failure = |failure| Refusal::Process {
        pid: identity.pid,
        expected_started: identity.started,
        failure,
    };
    #[cfg(unix)]
    let held = crate::taken::Held::find(identity.pid, identity.started).map_err(process_failure)?;
    #[cfg(windows)]
    let held = borrowed_windows::Held::find(identity.pid, identity.started).map_err(process_failure)?;
    let mut cost = Duration::ZERO;
    let stream = handshake_until(&identity.vsock, port, wait, &mut cost).map_err(|cause| {
        refused(match cause {
            HandshakeError::Io {
                kind: io::ErrorKind::OutOfMemory,
                ..
            } => BorrowFailure::CapacityUnavailable,
            cause => BorrowFailure::Channel(cause),
        })
    })?;
    held.check().map_err(process_failure)?;
    if matches!(wait, HandshakeWait::Until(at) if Instant::now() >= at) {
        return Err(refused(BorrowFailure::Expired));
    }
    let stream = stream
        .boxed()
        .map_err(|_| refused(BorrowFailure::CapacityUnavailable))?;
    if matches!(wait, HandshakeWait::Until(at) if Instant::now() >= at) {
        return Err(refused(BorrowFailure::Expired));
    }
    Ok(stream)
}

#[cfg(test)]
#[path = "../../../docs/research/measurements/managed-borrowed-nowait/focused.rs"]
mod borrowed_tests;

#[cfg(windows)]
mod borrowed_windows {
    use crate::{ProcessFailure, ProcessOperation};
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    fn failure(operation: ProcessOperation, source: io::Error) -> ProcessFailure {
        match source.raw_os_error() {
            Some(code) => ProcessFailure::Os { operation, code },
            None => ProcessFailure::UnrepresentedIo {
                operation,
                kind: source.kind(),
            },
        }
    }

    pub(super) struct Held(OwnedHandle);

    impl Held {
        pub(super) fn find(pid: u32, expected: u64) -> Result<Self, ProcessFailure> {
            // SAFETY: only query/synchronize access; non-inheritable and no termination right.
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                return Err(failure(ProcessOperation::OpenProcess, io::Error::last_os_error()));
            }
            // SAFETY: successful OpenProcess transfers its sole handle ownership here;
            // all return paths close it, without a watcher or a Guest lifetime owner.
            let held = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
            let observed = super::created_handle(held.0.as_raw_handle())
                .map_err(|source| failure(ProcessOperation::GetProcessTimes, source))?;
            if observed != expected {
                return Err(ProcessFailure::DifferentStart { observed });
            }
            held.check()?;
            Ok(held)
        }

        pub(super) fn check(&self) -> Result<(), ProcessFailure> {
            // SAFETY: a zero-time state observation of the held synchronize handle,
            // not a lifecycle wait or periodic process probe.
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                WAIT_TIMEOUT => Ok(()),
                WAIT_OBJECT_0 => Err(ProcessFailure::Exited),
                WAIT_FAILED => Err(failure(ProcessOperation::WaitForExit, io::Error::last_os_error())),
                returned => Err(ProcessFailure::UnexpectedWait { returned }),
            }
        }
    }
}

fn owned_text(text: &str) -> Result<String, Refusal> {
    let mut copy = String::new();
    copy.try_reserve_exact(text.len())
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    copy.push_str(text);
    Ok(copy)
}
fn managed_io(operation: &'static str, why: io::Error) -> Refusal {
    Refusal::ManagedIo {
        operation,
        kind: why.kind(),
        code: why.raw_os_error(),
    }
}
fn managed_marker() -> Result<Vec<u8>, Refusal> {
    let nonce = RandomState::new().hash_one(std::process::id());
    message(format_args!("\nshards-api: shards ended {nonce:016x}\n"))
        .map(String::into_bytes)
        .map_err(|()| Refusal::Launch(crate::LaunchError::Allocation))
}

/// The existing API protocol around a receipt the native owner already owns.
pub struct ManagedGuest {
    process: crate::ManagedReceipt,
    shared: triomphe::Arc<Shared>,
}

impl std::fmt::Debug for ManagedGuest {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("ManagedGuest").finish_non_exhaustive()
    }
}

fn managed_shared(keep: usize, recovered: bool) -> Result<triomphe::Arc<Shared>, Refusal> {
    let mut bytes = VecDeque::new();
    bytes
        .try_reserve_exact(
            keep.checked_mul(2)
                .ok_or(Refusal::Launch(crate::LaunchError::Count))?,
        )
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(keep)
        .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    scratch.resize(keep, 0);
    triomphe::Arc::try_new(Shared {
        state: Mutex::new(State {
            exited: false,
            observed_exited: false,
            stopping: false,
            ready: recovered,
            end: None,
            said: String::new(),
            ended_as: String::new(),
            status: None,
            managed: None,
            managed_ports: Vec::new(),
        }),
        changed: Condvar::new(),
        tail: Mutex::new(Tail {
            bytes,
            scratch: Some(scratch),
            probe_error: None,
            keep,
            drained: recovered,
            output: None,
        }),
        drained: Condvar::new(),
        end_mark: managed_marker()?,
        probe: false,
        managed_owners: std::sync::atomic::AtomicUsize::new(1),
    })
    .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))
}

fn lossy_output(bytes: &[u8]) -> Result<String, Refusal> {
    let mut text = String::new();
    // Each invalid byte can require U+FFFD's three UTF-8 bytes.
    text.try_reserve_exact(
        bytes
            .len()
            .checked_mul("\u{fffd}".len())
            .ok_or(Refusal::Launch(crate::LaunchError::Count))?,
    )
    .map_err(|_| Refusal::Launch(crate::LaunchError::Allocation))?;
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                break;
            }
            Err(why) => {
                let valid = rest
                    .get(..why.valid_up_to())
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .ok_or(Refusal::Launch(crate::LaunchError::Observer))?;
                text.push_str(valid);
                text.push('\u{fffd}');
                let skip = why
                    .error_len()
                    .unwrap_or_else(|| rest.len().saturating_sub(why.valid_up_to()));
                let used = why
                    .valid_up_to()
                    .checked_add(skip)
                    .filter(|n| *n > 0)
                    .ok_or(Refusal::Launch(crate::LaunchError::Count))?;
                rest = rest
                    .get(used..)
                    .ok_or(Refusal::Launch(crate::LaunchError::Observer))?;
            }
        }
    }
    Ok(text)
}

fn watch_managed(
    shared: triomphe::Arc<Shared>,
    process: crate::ManagedReceipt,
    writer: Option<io::PipeWriter>,
) {
    // This subscribes to the existing native receipt; it opens/reaps no PID.
    let observed = match process.wait_for(Duration::MAX) {
        Ok(Some(end)) if end.scope == crate::TerminalScope::Released => Ok(end),
        Ok(_) => Err(Refusal::Launch(crate::LaunchError::Observer)),
        Err(why) => Err(why),
    };
    let said = match writer {
        Some(writer) => lossy_output(finish_drain(&shared, writer).bytes.make_contiguous()),
        None => Ok(String::new()),
    };
    let mut state = lock(&shared.state);
    state.observed_exited = observed.is_ok();
    state.managed = Some(match said {
        Ok(said) => {
            state.said = said;
            observed
        }
        Err(why) => Err(why),
    });
    state.exited = true;
    shared.changed.notify_all();
}

fn managed_watch(
    shared: &triomphe::Arc<Shared>,
    process: &crate::ManagedReceipt,
    writer: Option<io::PipeWriter>,
    name: String,
) -> Result<(), Refusal> {
    let shared = shared.clone();
    let process = process.clone();
    std::thread::Builder::new()
        .name(name)
        .spawn(move || watch_managed(shared, process, writer))
        .map(|_| ())
        .map_err(|e| managed_io("observer thread", e))
}

pub fn boot_managed(
    plan: crate::PreparedBoot,
    launcher: &mut dyn crate::ManagedLauncher,
) -> Result<ManagedGuest, Refusal> {
    let shared = managed_shared(plan.output_bytes, false)?;
    let drain_name = owned_text("shards-api-managed-drain")?;
    let wait_name = owned_text("shards-api-managed-wait")?;
    let (output, writer) = io::pipe().map_err(|e| managed_io("output pipe", e))?;
    let stdout = writer
        .try_clone()
        .map_err(|e| managed_io("output writer clone", e))?;
    let stderr = writer
        .try_clone()
        .map_err(|e| managed_io("output writer clone", e))?;
    let process = launcher.launch(&plan, crate::LaunchIo { stdout, stderr })?;
    let identity = match process.identity() {
        Ok(identity) => identity,
        Err(why) => {
            process.stop()?;
            return Err(why);
        }
    };
    if identity.vsock != plan.vsock || identity.pid == 0 || identity.started == 0 {
        process.stop()?;
        return Err(Refusal::Launch(crate::LaunchError::Observer));
    }
    lock(&shared.state).managed_ports = plan.ports;
    let drainer = shared.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(drain_name)
        .spawn(move || drain(&drainer, output))
    {
        process.stop()?;
        return Err(managed_io("drain thread", error));
    }
    if let Err(why) = managed_watch(&shared, &process, Some(writer), wait_name) {
        process.stop()?;
        return Err(why);
    }
    Ok(ManagedGuest { process, shared })
}

pub fn attach_managed(
    plan: crate::PreparedBoot,
    identity: &Identity,
    launcher: &mut dyn crate::ManagedLauncher,
) -> Result<ManagedGuest, Refusal> {
    if identity.vsock != plan.vsock {
        return Err(Refusal::Launch(crate::LaunchError::Socket));
    }
    let shared = managed_shared(0, true)?;
    let wait_name = owned_text("shards-api-managed-wait")?;
    let process = launcher.attach(identity)?;
    if process.identity()? != identity {
        return Err(Refusal::Launch(crate::LaunchError::Observer));
    }
    lock(&shared.state).managed_ports = plan.ports;
    managed_watch(&shared, &process, None, wait_name)?;
    Ok(ManagedGuest { process, shared })
}

impl ManagedGuest {
    /// Shares the existing protocol/native receipt with checked handle counting.
    /// Worker references are not lifetime owners and cannot suppress last-drop stop.
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.shared
            .managed_owners
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| Refusal::Launch(crate::LaunchError::Count))?;
        Ok(Self {
            process: self.process.clone(),
            shared: self.shared.clone(),
        })
    }

    fn end(&self, state: &mut State) -> Result<Option<End>, Refusal> {
        let Some(result) = state.managed.as_mut() else {
            return Ok(None);
        };
        let completed = match result {
            Ok(end) => *end,
            Err(why) => {
                return Err(std::mem::replace(
                    why,
                    Refusal::Launch(crate::LaunchError::Observer),
                ));
            }
        };
        if state.stopping {
            return Ok(Some(End::Stopped));
        }
        Ok(Some(match completed.status {
            crate::ManagedStatus::Code(0) => End::PowerOff,
            crate::ManagedStatus::Code(3) => End::Reset,
            status => End::Failed(
                message(format_args!("native guest {status:?}: {}", state.said))
                    .map_err(|()| Refusal::Launch(crate::LaunchError::Allocation))?,
            ),
        }))
    }
}

impl Guest for ManagedGuest {
    fn identity(&self) -> Result<Identity, Refusal> {
        let id = self.process.identity()?;
        Ok(Identity {
            pid: id.pid,
            started: id.started,
            vsock: PathBuf::from(argument_copy(id.vsock.as_os_str())?),
        })
    }
    fn process_exited(&self) -> Result<bool, Refusal> {
        Ok(self.process.terminal()?.is_some())
    }
    fn terminal_scope(&self) -> Result<Option<crate::TerminalScope>, Refusal> {
        Ok(self.process.terminal()?.map(|end| end.scope))
    }
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal> {
        allowed(&lock(&self.shared.state).managed_ports, port)?;
        connect_owned(
            &self.shared,
            &self.process.identity()?.vsock,
            port,
            patience,
            || {
                self.end(&mut lock(&self.shared.state))
                    .map(|end| end.map(Refusal::Ended))
            },
        )
    }
    fn stop(&self) -> Result<(), Refusal> {
        // Native stop may complete inline and wake a concurrent waiter. Publish
        // the request first; an uncertain native result cannot undo its effects.
        lock(&self.shared.state).stopping = true;
        self.process.stop()
    }
    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(end) = self.end(&mut state)? {
                return Ok(Some(end));
            }
            state = match deadline.map(|d| d.saturating_duration_since(Instant::now())) {
                Some(left) if left.is_zero() => return Ok(None),
                Some(left) => {
                    self.shared
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}
impl Drop for ManagedGuest {
    fn drop(&mut self) {
        match self
            .shared
            .managed_owners
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        {
            Ok(1) => {}
            Ok(_) => return,
            Err(_) => {
                lock(&self.shared.state).managed = Some(Err(Refusal::Launch(crate::LaunchError::Observer)));
                self.shared.changed.notify_all();
                return;
            }
        }
        if !matches!(
            self.process.terminal(),
            Ok(Some(crate::ManagedEnd {
                scope: crate::TerminalScope::Released,
                ..
            }))
        ) {
            // A failed stop cannot release the native owner's canonical record.
            let _ = self.process.stop();
        }
    }
}

fn connect_owned(
    shared: &Shared,
    vsock: &Path,
    port: u32,
    patience: Duration,
    mut gone: impl FnMut() -> Result<Option<Refusal>, Refusal>,
) -> Result<Box<dyn Stream>, Refusal> {
    // None: past `Instant`'s range (`Duration::MAX`), so no clock: until the guest
    // listens or ends.
    let began = Instant::now();
    let deadline = began.checked_add(patience);
    let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let last = loop {
        if let Some(refusal) = gone()? {
            return Err(refusal);
        }
        let mut cost = Duration::ZERO;
        // One dial is always made, its answer awaited for what patience has left, or,
        // with none left (a patience of zero) or no clock, for the guest's own answer.
        let wait = left().filter(|l| !l.is_zero());
        match handshake(vsock, port, wait, &mut cost) {
            Ok(s) => {
                lock(&shared.state).ready = true;
                return s.boxed().map_err(|_| Refusal::Allocation {
                    what: "connection storage",
                });
            }
            Err(Attempt::Allocation) => {
                return Err(Refusal::Allocation {
                    what: "connection storage",
                });
            }
            Err(Attempt::Refused(why)) => return Err(Refusal::Channel { port, why }),
            Err(Attempt::Again(why)) if left().is_some_and(|l| l.is_zero()) => break why,
            Err(Attempt::Again(_)) => {}
        }
        // Waits out the pause, woken at once by the process's end.
        let state = lock(&shared.state);
        if !state.exited {
            let pause = pause(cost, began.elapsed(), left().unwrap_or(Duration::MAX));
            let _ = shared.changed.wait_timeout(state, pause);
        }
    };
    if let Some(refusal) = gone()? {
        return Err(refusal);
    }
    Err(Refusal::Channel {
        port,
        why: message(format_args!("no OK within {patience:?}: {last}")).map_err(|()| {
            Refusal::Allocation {
                what: "connection diagnostic",
            }
        })?,
    })
}

impl Guest for ShardsGuest {
    fn process_exited(&self) -> Result<bool, Refusal> {
        Ok(lock(&self.shared.state).observed_exited)
    }
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal> {
        allowed(&self.ports, port)?;
        connect_owned(&self.shared, &self.vsock, port, patience, || {
            Ok(self.gone(&mut lock(&self.shared.state)))
        })
    }

    fn stop(&self) -> Result<(), Refusal> {
        let mut state = lock(&self.shared.state);
        if !state.exited {
            state.stopping = true;
            self.signal(&state);
        }
        Ok(())
    }

    /// Its VM process's pid and start, as the OS counts it, read while the process is this
    /// one's unreaped child (its watcher reaps it under the state's lock), and its socket; a
    /// guest taken back, the identity it was taken back by. One that has ended is refused as
    /// `gone` says: its pid may name another process since.
    fn identity(&self) -> Result<Identity, Refusal> {
        #[cfg(unix)]
        if let Some(taken) = &self.taken {
            return Ok(taken.identity.clone());
        }
        let mut state = lock(&self.shared.state);
        if let Some(refusal) = self.gone(&mut state) {
            return Err(refusal);
        }
        #[cfg(unix)]
        let (pid, started) = (self.pid, crate::taken::started(self.pid));
        #[cfg(windows)]
        let (pid, started) = {
            let child = lock(&self.child);
            (child.id(), created(&child))
        };
        drop(state);
        Ok(Identity {
            pid,
            started: started.map_err(|e| Refusal::Gone {
                pid,
                why: format!("its start: {e}"),
            })?,
            vsock: self.vsock.clone(),
        })
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal> {
        // None: past `Instant`'s range (`Duration::MAX`): until it ends.
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(end) = self.end(&mut state) {
                return Ok(Some(end));
            }
            state = match deadline.map(|d| d.saturating_duration_since(Instant::now())) {
                Some(left) if left.is_zero() => return Ok(None),
                Some(left) => {
                    self.shared
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

impl Drop for ShardsGuest {
    /// Ends the guest, if it has not ended: no VM outlives its owner.
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        if !state.exited {
            state.stopping = true;
            self.signal(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Disk, Share};

    #[derive(Default)]
    struct NativeState {
        end: Option<crate::ManagedEnd>,
        acknowledged: bool,
        io: Option<crate::LaunchIo>,
    }
    struct ModelNative {
        identity: Identity,
        state: Mutex<NativeState>,
        changed: Condvar,
        coordinate_stop: bool,
        stops: std::sync::atomic::AtomicUsize,
    }
    impl ModelNative {
        fn publish(&self, scope: crate::TerminalScope) {
            let mut state = self.state.lock().unwrap();
            state.end = Some(crate::ManagedEnd {
                status: crate::ManagedStatus::Code(0),
                scope,
            });
            if scope == crate::TerminalScope::Released {
                state.io.take();
            }
            self.changed.notify_all();
        }
    }
    impl crate::ManagedProcess for ModelNative {
        fn identity(&self) -> Result<&Identity, Refusal> {
            Ok(&self.identity)
        }
        fn terminal(&self) -> Result<Option<crate::ManagedEnd>, Refusal> {
            Ok(self.state.lock().unwrap().end)
        }
        fn wait_for(&self, patience: Duration) -> Result<Option<crate::ManagedEnd>, Refusal> {
            let mut state = self.state.lock().unwrap();
            if patience == Duration::MAX {
                while !state
                    .end
                    .is_some_and(|end| end.scope == crate::TerminalScope::Released)
                {
                    state = self.changed.wait(state).unwrap();
                }
            } else {
                state = self
                    .changed
                    .wait_timeout_while(state, patience, |state| {
                        !state
                            .end
                            .is_some_and(|end| end.scope == crate::TerminalScope::Released)
                    })
                    .unwrap()
                    .0;
            }
            Ok(state
                .end
                .filter(|end| end.scope == crate::TerminalScope::Released))
        }
        fn stop(&self) -> Result<(), Refusal> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.publish(crate::TerminalScope::Released);
            if self.coordinate_stop {
                let state = self.state.lock().unwrap();
                let (observed_state, timeout) = self
                    .changed
                    .wait_timeout_while(state, crate::conformance::PATIENCE.get(), |state| {
                        !state.acknowledged
                    })
                    .unwrap();
                drop(observed_state);
                if timeout.timed_out() {
                    return Err(Refusal::Launch(crate::LaunchError::Observer));
                }
            }
            Ok(())
        }
    }
    struct ModelLauncher(triomphe::Arc<ModelNative>);
    impl crate::ManagedLauncher for ModelLauncher {
        fn launch(
            &mut self,
            _: &crate::PreparedBoot,
            io: crate::LaunchIo,
        ) -> Result<crate::ManagedReceipt, Refusal> {
            self.0.state.lock().unwrap().io = Some(io);
            Ok(crate::managed_receipt(self.0.clone()))
        }
        fn attach(&mut self, identity: &Identity) -> Result<crate::ManagedReceipt, Refusal> {
            if identity != &self.0.identity {
                return Err(Refusal::Launch(crate::LaunchError::Observer));
            }
            Ok(crate::managed_receipt(self.0.clone()))
        }
    }
    fn managed_model(coordinate_stop: bool) -> (ManagedGuest, triomphe::Arc<ModelNative>) {
        let socket = Path::new("/model/native/v");
        let plan = prepare_boot(&spec(), Path::new("/model/never-executed"), socket, 0).unwrap();
        let native = triomphe::Arc::try_new(ModelNative {
            identity: Identity {
                pid: 42,
                started: 7,
                vsock: socket.into(),
            },
            state: Mutex::new(NativeState::default()),
            changed: Condvar::new(),
            coordinate_stop,
            stops: std::sync::atomic::AtomicUsize::new(0),
        })
        .unwrap();
        let guest = boot_managed(plan, &mut ModelLauncher(native.clone())).unwrap();
        (guest, native)
    }

    /// D215, D232: helper scope and handle ownership remain live after
    /// controller exit, and dropping a retained protocol handle cannot stop it.
    #[test]
    fn managed_scope_and_checked_clones_retain_native_lifetime() {
        let (guest, native) = managed_model(false);
        let retained = guest.try_clone().unwrap();
        drop(retained);
        assert_eq!(native.stops.load(Ordering::SeqCst), 0);
        native.publish(crate::TerminalScope::Process);
        assert!(guest.process_exited().unwrap());
        assert_eq!(
            guest.terminal_scope().unwrap(),
            Some(crate::TerminalScope::Process)
        );
        assert_eq!(guest.wait_for(Duration::ZERO).unwrap(), None);
        native.publish(crate::TerminalScope::Released);
        assert_eq!(
            guest.wait_for(crate::conformance::PATIENCE.get()).unwrap(),
            Some(End::PowerOff)
        );
        drop(guest);
        assert_eq!(native.stops.load(Ordering::SeqCst), 0);
    }

    /// D215, D232: a native stop can publish inline, before stop() returns;
    /// a concurrent waiter must observe Stopped rather than unsolicited poweroff.
    #[test]
    fn managed_inline_stop_completion_is_already_stopped_to_waiters() {
        let (guest, native) = managed_model(true);
        let waiting = guest.try_clone().unwrap();
        let observed = native.clone();
        let waiter = std::thread::spawn(move || {
            let result = waiting.wait_for(crate::conformance::PATIENCE.get());
            observed.state.lock().unwrap().acknowledged = true;
            observed.changed.notify_all();
            result
        });
        guest.stop().unwrap();
        assert_eq!(waiter.join().unwrap().unwrap(), Some(End::Stopped));
    }

    /// D215, D232: pure preparation launches nothing and preserves the exact
    /// native command/ports; wrong endpoint/port fail before borrowed observation.
    #[test]
    fn prepared_boot_is_pure_and_connections_stay_in_its_exact_grant() {
        let directory = shards_testdir::TempDir::new("api-prepared").unwrap();
        let binary = directory.join("never-started");
        let socket = directory.join("v");
        let mut spec = spec();
        spec.gpu = true;
        spec.gpu_devices = vec![crate::PhysicalGpu::MetalRegistry(7)];
        let plan = prepare_boot(&spec, &binary, &socket, 17).unwrap();
        assert_eq!(plan.binary(), binary);
        assert_eq!(plan.vsock(), socket);
        assert_eq!(plan.ports(), &[1024]);
        assert_eq!(
            std::fs::read_dir(&*directory).unwrap().count(),
            0,
            "no mkdir/socket/process side effects"
        );
        let words: Vec<_> = plan.arguments().iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            words.iter().rev().take(4).copied().collect::<Vec<_>>(),
            [
                "--host-managed",
                "metal:0000000000000007",
                "--gpu-device",
                "--gpu"
            ]
        );
        let identity = Identity {
            pid: 42,
            started: 7,
            vsock: socket.clone(),
        };
        assert!(matches!(
            connect_prepared(&plan, &identity, 1025, Duration::ZERO),
            Err(Refusal::Launch(crate::LaunchError::Port))
        ));
        let foreign = Identity {
            vsock: directory.join("foreign"),
            ..identity
        };
        assert!(matches!(
            connect_prepared(&plan, &foreign, 1024, Duration::ZERO),
            Err(Refusal::Launch(crate::LaunchError::Socket))
        ));
        assert!(matches!(
            prepare_boot(&spec, Path::new("relative"), &socket, 0),
            Err(Refusal::Launch(crate::LaunchError::Binary))
        ));
        assert!(matches!(
            prepare_boot(&spec, &binary, &socket, usize::MAX),
            Err(Refusal::Launch(crate::LaunchError::Count))
        ));
    }

    /// D232: native containment is explicit only for managed plans. The
    /// standalone CPU/GPU argument bytes stay exact, even for flag-like values.
    #[test]
    fn prepared_marker_preserves_standalone_arguments_and_flag_values() {
        let dir = shards_testdir::TempDir::new("api-managed-marker").unwrap();
        let binary = dir.join("never-started");
        let socket = dir.join("v");
        for gpu in [false, true] {
            for cmdline in ["--host-managed", "--", "console=ttyS0"] {
                let mut spec = spec();
                spec.gpu = gpu;
                spec.cmdline = cmdline.into();
                let standalone = arguments(&spec, &socket, 17).unwrap();
                let plan = prepare_boot(&spec, &binary, &socket, 17).unwrap();
                assert_eq!(
                    plan.arguments().get(..standalone.len()),
                    Some(standalone.as_slice())
                );
                assert_eq!(plan.arguments().last(), Some(&OsString::from("--host-managed")));
                assert_eq!(plan.arguments().len(), standalone.len() + 1);
            }
        }
        assert_eq!(std::fs::read_dir(&*dir).unwrap().count(), 0);
    }

    /// D231: renderer initialization cannot carry an inner workload or start
    /// its file/console helpers while resolving the exact selected identities.
    #[test]
    fn gpu_probe_arguments_carry_only_the_exact_native_query() {
        let mut spec = spec();
        spec.cmdline = "--host-managed --gpu-probe".into();
        spec.shares.push(crate::Share {
            dir: "/not-opened".into(),
            read_only: false,
        });
        spec.disks.push(crate::Disk {
            path: "/not-opened/disk".into(),
            read_only: false,
        });
        spec.gpu_devices = vec![crate::PhysicalGpu::MetalRegistry(7)];
        assert_eq!(
            probe_arguments(&spec).unwrap(),
            [
                OsString::from("run"),
                OsString::from("--kernel"),
                spec.kernel.into_os_string(),
                OsString::from("--gpu-probe"),
                OsString::from("--gpu-device"),
                OsString::from("metal:0000000000000007"),
            ]
        );
    }

    /// D231, D232: the configured probe uses its client's explicit pair, without a
    /// PATH/binary/environment fallback or additional guest/provider authority.
    #[test]
    fn configured_node_roots_are_explicit_and_the_command_environment_is_closed() {
        let dir = shards_testdir::TempDir::new("api-node-roots").unwrap();
        let cache = dir.join("cache");
        let home = dir.join("home");
        let node = Shards::configured(cache.clone(), home.clone()).unwrap();
        assert_eq!(node.cache(), cache);
        assert_eq!(node.home(), home);
        let plan = prepare_boot(&spec(), &dir.join("never-started"), &dir.join("v"), 17).unwrap();
        assert_eq!(plan.runtime_paths(), None);
        let plan = plan.with_runtime_paths(node.runtime_paths()).unwrap();
        assert_eq!(plan.runtime_paths(), Some(node.runtime_paths()));
        assert_eq!(node.runtime_paths().try_clone().unwrap(), *node.runtime_paths());
        assert!(Shards::configured("relative".into(), home.clone()).is_err());
        assert!(Shards::configured(cache.clone(), "".into()).is_err());
        let mut command = Command::new(dir.join("never-started"));
        command.env("FORBIDDEN_MODEL_INPUT", "value");
        node.environment(&mut command);
        assert_eq!(
            command
                .get_envs()
                .map(|(name, value)| (name.to_owned(), value.map(OsString::from)))
                .collect::<Vec<_>>(),
            [
                (OsString::from("SHARDS_CACHE"), Some(cache.into_os_string())),
                (OsString::from("SHARDS_HOME"), Some(home.into_os_string())),
            ]
        );
        let mut spec = spec();
        spec.binary = None;
        assert!(matches!(
            launch(&spec, Some(1), Some(&node)),
            Err(Refusal::Launch(crate::LaunchError::Binary))
        ));
        assert_eq!(std::fs::read_dir(&*dir).unwrap().count(), 0);
    }

    /// D215, D231: exercise real bounded pipe/watch/parse wiring with an owned
    /// protocol fixture process. This is not a renderer/native GPU witness.
    #[cfg(unix)]
    #[test]
    fn gpu_probe_protocol_requires_exact_completed_receipt() {
        use std::os::unix::fs::PermissionsExt;
        let directory = shards_testdir::TempDir::new("api-gpu-probe").unwrap();
        let binary = directory.join("fixture");
        let native = crate::PhysicalGpu::MetalRegistry(7);
        let expected = crate::GpuProbe {
            devices: vec![crate::ResolvedGpu {
                physical: native,
                uuid: [7; 16],
                pci: None,
                luid: Some(7u64.to_be_bytes()),
                name: None,
                device_local_bytes: None,
            }],
            host_visible_bytes: 4096,
        };
        let body = expected.encode().unwrap();
        let mut spec = spec();
        spec.binary = Some(binary.clone());
        spec.gpu = true;
        spec.gpu_devices = vec![native];
        for (text, valid) in [
            (body.clone(), true),
            (
                body.replace("metal:0000000000000007", "metal:0000000000000008"),
                false,
            ),
            (body.replace("end\n", ""), false),
        ] {
            // Canonical protocol fields contain no shell quoting characters.
            std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s' '{text}'\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result = Shards.probe_gpu(&spec, crate::conformance::PATIENCE.get());
            if valid {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert!(matches!(result, Err(Refusal::GpuProbe(_))), "{result:?}");
            }
        }
        assert!(matches!(
            Shards.probe_gpu(&spec, Duration::ZERO),
            Err(Refusal::GpuProbe(crate::GpuProbeError::Pending))
        ));
    }

    /// Inputs, each only formatted into a flag: any values show the mapping.
    fn spec() -> GuestSpec {
        GuestSpec {
            kernel: "/k/Image".into(),
            init: Init::Executable("/k/init".into()),
            cmdline: "console=ttyS0 quiet".into(),
            vcpus: 2,
            memory_mib: 256,
            gpu: false,
            gpu_devices: Vec::new(),
            disks: vec![
                Disk {
                    path: "/d/root".into(),
                    read_only: true,
                },
                Disk {
                    path: "/d/data".into(),
                    read_only: false,
                },
            ],
            shares: vec![Share {
                dir: "/s/ro".into(),
                read_only: true,
            }],
            ports: vec![1024],
            binary: None,
        }
    }

    /// A spec becomes `shards run --kernel` and its flags, as vm_run.rs reads them, the
    /// kernel first (D215).
    #[test]
    fn a_spec_is_the_flags_shards_run_reads() {
        let args = arguments(&spec(), Path::new("/p/v"), 2048).unwrap();
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "run",
                "--kernel",
                "/k/Image",
                "--init",
                "/k/init",
                "--cmdline",
                "console=ttyS0 quiet",
                "--cpus",
                "2",
                "--memory",
                "256",
                "--disk",
                "/d/root:ro",
                "--disk",
                "/d/data",
                "--share",
                "/s/ro:ro",
                "--vsock",
                "/p/v",
                "--no-console",
                "--console-said",
                "2048"
            ]
        );
        let initrd = GuestSpec {
            init: Init::Initrd("/k/initrd".into()),
            ..spec()
        };
        let args = arguments(&initrd, Path::new("/p/v"), 2048).unwrap();
        assert_eq!(args.get(3..5).unwrap(), ["--initrd", "/k/initrd"]);
    }

    /// A GPU adds `--gpu` to the flags and nothing else, whichever the init (D215).
    #[test]
    fn shared_venus_is_enabled_only_when_requested() {
        for init in [
            Init::Executable("/k/init".into()),
            Init::Initrd("/k/initrd".into()),
        ] {
            let cpu = GuestSpec { init, ..spec() };
            assert!(!cpu.gpu);
            let mut expected = arguments(&cpu, Path::new("/p/v"), 2048).unwrap();
            expected.push("--gpu".into());
            let gpu = GuestSpec { gpu: true, ..cpu };
            assert_eq!(arguments(&gpu, Path::new("/p/v"), 2048).unwrap(), expected);
        }
    }

    /// Exact native selection crosses the public process boundary verbatim; it
    /// implies the renderer without a model/index/environment-based fallback.
    #[test]
    fn physical_gpu_arguments_preserve_the_requested_native_set() {
        use crate::PhysicalGpu;
        let selected = GuestSpec {
            gpu_devices: vec![
                PhysicalGpu::Pci {
                    domain: u32::MAX,
                    bus: 196,
                    device: 0,
                    function: 0,
                },
                PhysicalGpu::MetalRegistry(u64::MAX),
            ],
            ..spec()
        };
        crate::check(&selected).unwrap();
        let args = arguments(&selected, Path::new("/p/v"), 2048).unwrap();
        let args: Vec<&str> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        let at = args.iter().position(|arg| *arg == "--gpu").unwrap();
        assert_eq!(
            args.get(at..at + 5),
            Some(
                [
                    "--gpu",
                    "--gpu-device",
                    "pci:ffffffff:c4:00.0",
                    "--gpu-device",
                    "metal:ffffffffffffffff"
                ]
                .as_slice()
            )
        );
        let repeated = GuestSpec {
            gpu_devices: vec![PhysicalGpu::Luid(7), PhysicalGpu::Luid(7)],
            ..spec()
        };
        assert_eq!(
            crate::check(&repeated),
            Err(Refusal::Gpu(crate::PhysicalGpuError::Duplicate))
        );
    }

    /// A path ending in `:ro`, which shards would read as a mode, is refused by name (D215).
    #[test]
    fn a_path_ending_in_ro_is_refused() {
        let mut s = spec();
        s.disks.push(Disk {
            path: "/d/x:ro".into(),
            read_only: false,
        });
        assert!(matches!(
            arguments(&s, Path::new("/p/v"), 2048),
            Err(Refusal::Unsupported { what: "disk", .. })
        ));
    }

    /// The socket's directory is private, short enough to bind, and goes with its value.
    #[cfg(unix)]
    #[test]
    fn the_socket_directory_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = PrivateDir::new().unwrap();
        let path = dir.0.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(path.join("v").as_os_str().len() <= SOCKET_PATH);
        drop(dir);
        assert!(!path.exists());
    }

    /// A temporary directory too long for a socket beneath it is passed over for /tmp on
    /// Unix, and refused by name on Windows, whose AF_UNIX paths hold 108 bytes (review 16).
    #[test]
    fn a_socket_directory_too_long_is_passed_over_or_refused() {
        let short = PathBuf::from("/t");
        assert_eq!(socket_dir(short.clone()).unwrap(), short);
        let long = PathBuf::from(format!("/{}", "d".repeat(SOCKET_PATH)));
        #[cfg(unix)]
        assert_eq!(socket_dir(long).unwrap(), PathBuf::from("/tmp"));
        #[cfg(windows)]
        assert!(matches!(socket_dir(long), Err(Refusal::Boot(why)) if why.contains("bytes at most")));
    }

    /// The socket directory's name carries no pid, so a directory an earlier process of the
    /// same pid left, or one another user made to take it first, is never its name (D215).
    #[test]
    fn the_socket_directory_is_named_apart_from_the_pid() {
        let dir = PrivateDir::new().unwrap();
        let name = dir.0.file_name().unwrap().to_str().unwrap().to_string();
        let pid = format!("shards-api-{}-", std::process::id());
        assert!(!name.starts_with(&pid), "{name}");
        let again = PrivateDir::new().unwrap();
        assert_ne!(dir.0, again.0);
    }

    /// A stand-in script at `bin`, `body` its text: written beside it and installed by
    /// install(1), so no descriptor of this process ever writes the file it executes (one a
    /// test beside it forked while open would fail its exec ETXTBSY, execve(2)).
    #[cfg(unix)]
    fn installed(bin: &Path, body: &str) {
        let source = bin.with_extension("sh");
        std::fs::write(&source, body).unwrap();
        let done = Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(bin)
            .status()
            .unwrap();
        assert!(done.success());
    }

    /// A guest's socket and its muxer's side of one connection, answering `answer` to the
    /// first `CONNECT` line; the refusal, where the dial is refused.
    #[cfg(unix)]
    fn answered(answer: &'static [u8]) -> Option<Attempt> {
        let dir = PrivateDir::new().unwrap();
        let path = dir.0.join("v");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let muxer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while line.last() != Some(&b'\n') {
                s.read_exact(&mut byte).unwrap();
                line.push(byte[0]);
            }
            assert_eq!(line, b"CONNECT 5\n");
            s.write_all(answer).unwrap();
            s
        });
        let mut cost = Duration::ZERO;
        let got = handshake(&path, 5, None, &mut cost);
        drop(muxer.join().unwrap());
        got.err()
    }

    /// The longest answer Firecracker's protocol gives, `OK 4294967295\n`, is taken; one a
    /// byte longer is refused as no answer (D215).
    #[cfg(unix)]
    #[test]
    fn an_answer_past_the_longest_ok_is_refused() {
        assert!(answered(b"OK 4294967295\n").is_none());
        assert!(matches!(answered(b"OK 04294967295\n"), Some(Attempt::Refused(_))));
    }

    /// A guest booted by a stand-in for shards that runs on until it is stopped, with a
    /// muxer at its vsock socket that closes its first `refusals` dials unanswered (as a
    /// guest that does not listen yet), answers the next `OK`, and counts them.
    #[cfg(unix)]
    fn listening_after(
        refusals: usize,
    ) -> (
        shards_testdir::TempDir,
        ShardsGuest,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let dir = shards_testdir::TempDir::new("api-dials").unwrap();
        let bin = dir.path().join("shards");
        installed(&bin, "#!/bin/sh\nexec sleep 1000\n");
        let guest = start(&GuestSpec {
            binary: Some(bin),
            ..spec()
        })
        .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&guest.vsock).unwrap();
        let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = dials.clone();
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let mut s = s.unwrap();
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                while line.last() != Some(&b'\n') && s.read_exact(&mut byte).is_ok() {
                    line.push(byte[0]);
                }
                // Counted before it is closed or answered, which the dial waits for.
                if counted.fetch_add(1, Ordering::SeqCst) >= refusals {
                    s.write_all(b"OK 1073741824\n").unwrap();
                    std::mem::forget(s);
                }
            }
        });
        (dir, guest, dials)
    }

    /// A stand-in for shards that ignores SIGTERM, booted, once it does.
    #[cfg(unix)]
    fn deaf() -> (shards_testdir::TempDir, ShardsGuest) {
        let dir = shards_testdir::TempDir::new("api-deaf-unit").unwrap();
        let (bin, deaf) = (dir.path().join("shards"), dir.path().join("deaf"));
        assert!(Command::new("mkfifo").arg(&deaf).status().unwrap().success());
        let body = format!(
            "#!/bin/sh\ntrap '' TERM; echo > '{}'; while :; do sleep 1; done\n",
            deaf.display()
        );
        installed(&bin, &body);
        let guest = start(&GuestSpec {
            binary: Some(bin),
            ..spec()
        })
        .unwrap();
        std::fs::read(&deaf).unwrap();
        (dir, guest)
    }

    /// A stop kills at once, though the guest ignores SIGTERM, returns `Ok`, and the guest
    /// ends `Stopped`: never refused (review B7).
    #[cfg(unix)]
    #[test]
    fn a_stop_kills_at_once() {
        let (_dir, guest) = deaf();
        guest.stop().unwrap();
        assert_eq!(guest.wait_for(Duration::MAX).unwrap(), Some(End::Stopped));
    }

    /// Whether `pid` names no process once `shared`'s watcher says it has exited: kill -0
    /// names a zombie as a process, and a reaped pid as none.
    #[cfg(unix)]
    fn reaped_once_exited(pid: u32, shared: &Shared) -> bool {
        let mut state = lock(&shared.state);
        while !state.exited {
            state = shared.changed.wait(state).unwrap();
        }
        drop(state);
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        !alive.success()
    }

    /// A guest leaves no zombie, dropped while its shards runs or after shards has exited
    /// with nobody asking its end: once its watcher says it has exited, its process is
    /// reaped, so its pid names nothing (D215; review B2; both of integrate's
    /// 80078b00 cases).
    #[cfg(unix)]
    #[test]
    fn a_dropped_guest_leaves_no_zombie() {
        let (_dir, guest, _) = listening_after(0);
        let (pid, shared) = (guest.pid, guest.shared.clone());
        drop(guest);
        assert!(
            reaped_once_exited(pid, &shared),
            "pid {pid} is still a process (a zombie)"
        );
        let dir = shards_testdir::TempDir::new("api-exits").unwrap();
        let bin = dir.path().join("shards");
        installed(&bin, "#!/bin/sh\nexit 0\n");
        let guest = start(&GuestSpec {
            binary: Some(bin),
            ..spec()
        })
        .unwrap();
        let (pid, shared) = (guest.pid, guest.shared.clone());
        assert!(
            reaped_once_exited(pid, &shared),
            "pid {pid}, exited unasked, is still a process (a zombie)"
        );
        drop(guest);
    }

    /// A patience of zero is one dial, its answer awaited, and no other (D215).
    #[cfg(unix)]
    #[test]
    fn a_patience_of_zero_dials_once() {
        let (_dir, guest, dials) = listening_after(usize::MAX);
        let refused = guest.connect(1024, Duration::ZERO);
        assert!(
            matches!(refused, Err(Refusal::Channel { port: 1024, .. })),
            "{:?}",
            refused.err()
        );
        assert_eq!(dials.load(Ordering::SeqCst), 1);
        let (_dir, guest, dials) = listening_after(0);
        assert!(guest.connect(1024, Duration::ZERO).is_ok());
        assert_eq!(dials.load(Ordering::SeqCst), 1);
    }

    /// A patience of `Duration::MAX` is no clock: dials go on until the guest listens
    /// (D215).
    #[cfg(unix)]
    #[test]
    fn a_patience_of_max_dials_until_the_guest_listens() {
        let (_dir, guest, dials) = listening_after(3);
        assert!(guest.connect(1024, Duration::MAX).is_ok());
        assert_eq!(dials.load(Ordering::SeqCst), 4);
    }

    /// Measures `c` of `pause`: what one refused dial takes, `SHARDS_API_DIALS_N` (default
    /// 1000) of each kind, printed as n, p50, p90, p99 and max
    /// (docs/research/measurements/shards-api-waits/run.sh): to a socket not bound yet
    /// (refused by this host's kernel), and, where `SHARDS_API_REAL=1` boots a real guest as
    /// tests/real.rs does, to a port it does not serve (refused by the guest).
    #[cfg(unix)]
    #[test]
    #[ignore = "a measurement, run by its harness"]
    fn the_cost_of_a_refused_dial() {
        let n: usize = std::env::var("SHARDS_API_DIALS_N")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(1000);
        let timed = |path: &Path| -> Vec<Duration> {
            let mut d: Vec<Duration> = (0..n)
                .map(|_| {
                    let mut cost = Duration::ZERO;
                    assert!(matches!(
                        handshake(path, 1236, None, &mut cost),
                        Err(Attempt::Again(_))
                    ));
                    cost
                })
                .collect();
            d.sort();
            d
        };
        let dir = PrivateDir::new().unwrap();
        let mut kinds = vec![("a socket not bound yet", timed(&dir.0.join("v")))];
        let real = std::env::var_os("SHARDS_API_REAL").is_some_and(|v| v == "1");
        let guest = real.then(|| {
            let var = |k: &str| PathBuf::from(std::env::var_os(k).unwrap());
            start(&GuestSpec {
                kernel: var("SHARDS_API_KERNEL"),
                init: Init::Executable(var("SHARDS_API_INIT")),
                cmdline: "console=ttyS0 quiet shards_test=echo".into(),
                vcpus: 1,
                memory_mib: crate::min_memory_mib(std::env::consts::ARCH).max(256),
                gpu: false,
                gpu_devices: Vec::new(),
                disks: vec![],
                shares: vec![],
                ports: vec![1234, 1236],
                binary: None,
            })
            .unwrap()
        });
        if let Some(guest) = &guest {
            drop(guest.connect(1234, Duration::MAX).unwrap());
            kinds.push(("a real guest's port it does not serve", timed(&guest.vsock)));
        }
        for (what, d) in kinds {
            let q = |p: usize| d[(n - 1) * p / 100];
            println!(
                "dial {what}: n={n} p50={:?} p90={:?} p99={:?} max={:?}",
                q(50),
                q(90),
                q(99),
                d[n - 1]
            );
        }
    }

    /// A refused dial's cost, `pause`'s `c`, is its own work, the connect and the ask, and
    /// not its wait for the guest's answer: a muxer that holds a request unanswered (a guest
    /// whose vsock is not up yet) costs this thread nothing while it waits, and a pause grown
    /// from that wait held a boot back seconds (review: a 15 s boot on HVF).
    #[cfg(unix)]
    #[test]
    fn a_dials_cost_is_its_work_not_its_wait_for_an_answer() {
        let dir = PrivateDir::new().unwrap();
        let path = dir.0.join("v");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // An input: an answer held long past any connect's and write's own time.
        let held = Duration::from_millis(200);
        let muxer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = [0u8; b"CONNECT 5\n".len()];
            s.read_exact(&mut line).unwrap();
            std::thread::sleep(held);
        });
        let mut cost = Duration::MAX;
        let start = Instant::now();
        assert!(matches!(
            handshake(&path, 5, None, &mut cost),
            Err(Attempt::Again(_))
        ));
        assert!(start.elapsed() >= held);
        assert!(cost < held, "{cost:?}");
        muxer.join().unwrap();
    }

    /// The wait between dials is `sqrt(2 c w)` of the last dial's time and the wait so far,
    /// none at first, never past what patience has left (D215).
    #[test]
    fn a_pause_weighs_the_dials_cost_against_lateness() {
        let (us, ms) = (Duration::from_micros, Duration::from_millis);
        assert_eq!(pause(us(5), Duration::ZERO, Duration::MAX), Duration::ZERO);
        assert_eq!(
            pause(us(5), ms(150), Duration::MAX),
            Duration::from_nanos(1_224_744)
        );
        assert_eq!(
            pause(us(50), ms(150), Duration::MAX),
            Duration::from_nanos(3_872_983)
        );
        assert_eq!(pause(us(5), ms(150), ms(1)), ms(1));
        assert_eq!(pause(Duration::MAX, Duration::MAX, ms(1)), ms(1));
    }

    /// A listener at `path` whose queue is full and which accepts none, with what holds it so:
    /// on Unix its backlog cut to none (listen(2) again) and one connection queued, which
    /// Linux and macOS each take past a backlog of none; on Windows a backlog of one and
    /// connections until afunix.sys refuses one (WSAENOBUFS, after five: PM M205).
    #[cfg(unix)]
    fn a_full_listener(path: &Path) -> Vec<Box<dyn std::any::Any + Send>> {
        use std::os::fd::AsRawFd as _;
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        // SAFETY: listen(2) again on the socket `listener` holds, its backlog cut to none.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let queued = std::os::unix::net::UnixStream::connect(path);
        vec![Box::new(listener), Box::new(queued)]
    }

    #[cfg(windows)]
    fn a_full_listener(path: &Path) -> Vec<Box<dyn std::any::Any + Send>> {
        let listener = shards_afunix::UnixListener::bound(path).unwrap();
        // SAFETY: listen of the bound socket `listener` holds, its backlog one.
        let listening = unsafe { windows_sys::Win32::Networking::WinSock::listen(listener.raw(), 1) };
        assert_eq!(listening, 0);
        let mut held: Vec<Box<dyn std::any::Any + Send>> = vec![Box::new(listener)];
        while let Ok(queued) = shards_afunix::UnixStream::connect(path) {
            held.push(Box::new(queued));
        }
        held
    }

    /// A dial to a socket whose listener's queue is full waits no longer than its patience
    /// and is asked again within it, refused `Again`, as a guest whose vsock takes no
    /// connection yet is: Linux's blocking connect(2) waited for room for ever, and Windows'
    /// WSAENOBUFS (PM M205) was taken for a refusal (D215).
    #[test]
    fn a_dial_to_a_full_listener_is_asked_again_within_its_patience() {
        let dir = PrivateDir::new().unwrap();
        let path = dir.0.join("v");
        let _full = a_full_listener(&path);
        // An input: any patience shows it kept.
        let patience = crate::conformance::UNSERVED_PATIENCE;
        let (worker, rx) = crate::while_runs::on_a_thread(move || {
            let mut cost = Duration::ZERO;
            match handshake(&path, 5, Some(patience), &mut cost) {
                Ok(_) => "connected".to_string(),
                Err(Attempt::Again(why)) => format!("again: {why}"),
                Err(Attempt::Refused(why)) => format!("refused: {why}"),
                Err(Attempt::Allocation) => "allocation unavailable".to_string(),
            }
        });
        let said = crate::while_runs::answer(&rx, worker, patience);
        assert!(said.starts_with("again: "), "{said}");
    }

    /// A muxer for `listener`'s guest until the listener fails: each connection's
    /// `CONNECT` answered `OK`, then what it is sent echoed back until it half-closes, as the
    /// test guest's echo mode does.
    #[cfg(unix)]
    fn serve_echo(listener: &std::os::unix::net::UnixListener) {
        for s in listener.incoming() {
            let Ok(mut s) = s else { return };
            std::thread::spawn(move || {
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                while line.last() != Some(&b'\n') && s.read_exact(&mut byte).is_ok() {
                    line.push(byte[0]);
                }
                if s.write_all(b"OK 1073741824\n").is_err() {
                    return;
                }
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = s.read(&mut buf) {
                    if s.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                let _ = s.shutdown(std::net::Shutdown::Write);
            });
        }
    }

    /// A guest booted by a stand-in for shards that runs on until it is ended, with an echoing
    /// muxer (`serve_echo`) at its vsock socket, and its spec.
    #[cfg(unix)]
    fn echoing() -> (shards_testdir::TempDir, ShardsGuest, GuestSpec) {
        let dir = shards_testdir::TempDir::new("api-attach").unwrap();
        let bin = dir.path().join("shards");
        installed(&bin, "#!/bin/sh\nexec sleep 1000\n");
        let spec = GuestSpec {
            binary: Some(bin),
            ..spec()
        };
        let guest = start(&spec).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&guest.vsock).unwrap();
        std::thread::spawn(move || serve_echo(&listener));
        (dir, guest, spec)
    }

    /// What `guest`'s port 1024 echoes of `back`, written and half-closed.
    #[cfg(unix)]
    fn echoed(guest: &dyn Guest) -> Vec<u8> {
        let mut conn = guest.connect(1024, Duration::MAX).unwrap();
        conn.write_all(b"back").unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = Vec::new();
        conn.read_to_end(&mut got).unwrap();
        got
    }

    /// A booted guest names itself by its VM process's pid, that process's start as its OS
    /// counts it, the same however often it is asked, and its vsock socket; once it has
    /// ended, its pid no longer names it, and it is refused `Ended` (D215; Hex's proposal).
    #[cfg(unix)]
    #[test]
    fn a_guest_names_itself_by_pid_start_and_socket() {
        let (_dir, guest, _) = echoing();
        let identity = guest.identity().unwrap();
        assert_eq!((identity.pid, &identity.vsock), (guest.pid, &guest.vsock));
        assert_eq!(echoed(&guest), b"back");
        assert_eq!(guest.identity().unwrap(), identity);
        guest.stop().unwrap();
        assert_eq!(guest.wait_for(Duration::MAX).unwrap(), Some(End::Stopped));
        assert_eq!(guest.identity(), Err(Refusal::Ended(End::Stopped)));
    }

    /// A guest whose handle its client lost without its drop (a crash, in one process) is
    /// taken back by its identity: it serves its port, names itself as before, stops as
    /// `Stopped`, and is then gone; an identity whose start differs (its pid since given
    /// another process) is refused with the observed start (D215; Hex's proposal).
    #[cfg(unix)]
    #[test]
    fn attach_takes_back_a_guest_whose_handle_was_lost() {
        let (_dir, guest, spec) = echoing();
        let identity = guest.identity().unwrap();
        std::mem::forget(guest);
        let other = Identity {
            started: identity.started.wrapping_add(1),
            ..identity.clone()
        };
        assert!(
            matches!(Shards.attach(&other, &spec), Err(Refusal::Process {
                pid, expected_started, failure: crate::ProcessFailure::DifferentStart { observed }
            }) if pid == other.pid && expected_started == other.started && observed == identity.started),
            "another start"
        );
        let back = Shards.attach(&identity, &spec).unwrap();
        assert_eq!(back.identity().unwrap(), identity);
        assert_eq!(echoed(&*back), b"back");
        back.stop().unwrap();
        assert_eq!(back.wait_for(Duration::MAX).unwrap(), Some(End::Stopped));
        let again = Shards.attach(&identity, &spec);
        assert!(
            matches!(again, Err(Refusal::Process {
            pid, expected_started, failure: crate::ProcessFailure::Exited | crate::ProcessFailure::Os {
                operation: crate::ProcessOperation::PidfdOpen | crate::ProcessOperation::RegisterExit | crate::ProcessOperation::ReadStat,
                code: libc::ESRCH,
            } | crate::ProcessFailure::Os {
                // Reaped between its pidfd's open and its stat's: procfs's lookup finds no
                // entry (ENOENT), or its permission check of one still cached no task (ESRCH,
                // fs/proc/base.c proc_pid_permission).
                operation: crate::ProcessOperation::OpenStat,
                code: libc::ENOENT | libc::ESRCH,
            }
        }) if pid == identity.pid && expected_started == identity.started),
            "{:?}",
            again.err()
        );
    }

    /// A guest taken back is its taker's until it drops, which ends it, and with it the
    /// socket directory its first client made; one that ends by itself ends `Failed`, as
    /// its exit status is its parent's alone (D215; Hex's proposal).
    #[cfg(unix)]
    #[test]
    fn a_guest_taken_back_ends_as_it_drops_or_by_itself() {
        let (_dir, guest, spec) = echoing();
        let (identity, shared) = (guest.identity().unwrap(), guest.shared.clone());
        std::mem::forget(guest);
        let back = Shards.attach(&identity, &spec).unwrap();
        let socket_dir = identity.vsock.parent().unwrap().to_path_buf();
        drop(back);
        assert!(
            reaped_once_exited(identity.pid, &shared),
            "pid {} runs on",
            identity.pid
        );
        assert!(!socket_dir.exists(), "{} is left", socket_dir.display());
        let (_dir, guest, spec) = echoing();
        let identity = guest.identity().unwrap();
        std::mem::forget(guest);
        let back = Shards.attach(&identity, &spec).unwrap();
        let killed = Command::new("kill")
            .args(["-KILL", &identity.pid.to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let end = back.wait_for(Duration::MAX).unwrap();
        assert!(
            matches!(&end, Some(End::Failed(why)) if why.contains("parent")),
            "{end:?}"
        );
    }

    /// Set in the client `attach_takes_back_a_guest_whose_client_crashed` runs: the stand-in
    /// for shards it boots with.
    #[cfg(unix)]
    const CRASHING_SHARDS: &str = "SHARDS_API_CRASHING_SHARDS";
    /// Set by that stand-in for the VM process it becomes: its `--vsock` socket.
    #[cfg(unix)]
    const STANDIN_VSOCK: &str = "SHARDS_API_STANDIN_VSOCK";

    /// The client `attach_takes_back_a_guest_whose_client_crashed` runs, in a process of its
    /// own: it boots a guest, takes its identity before its stand-in has become the VM
    /// process (so a start an exec changed would not be taken back), connects to it, says
    /// the identity, and ends holding it, its drop never run, as a crash ends a client.
    #[cfg(unix)]
    #[test]
    #[ignore = "a child of attach_takes_back_a_guest_whose_client_crashed"]
    fn child_a_client_that_crashes() {
        let Some(bin) = std::env::var_os(CRASHING_SHARDS) else {
            return;
        };
        let guest = start(&GuestSpec {
            binary: Some(bin.into()),
            ..spec()
        })
        .unwrap();
        let id = guest.identity().unwrap();
        drop(guest.connect(1024, Duration::MAX).unwrap());
        println!("identity {} {} {}", id.pid, id.started, id.vsock.display());
        std::process::exit(0);
    }

    /// The VM process the crashing client's stand-in for shards becomes: an echoing muxer
    /// (`serve_echo`) at the `--vsock` socket it was given, until it is killed.
    #[cfg(unix)]
    #[test]
    #[ignore = "a child of attach_takes_back_a_guest_whose_client_crashed"]
    fn child_a_vm_process_serving_its_vsock() {
        let Some(vsock) = std::env::var_os(STANDIN_VSOCK) else {
            return;
        };
        serve_echo(&std::os::unix::net::UnixListener::bind(vsock).unwrap());
    }

    /// A guest whose client crashed, ending with it held, is taken back by its identity from
    /// another process, whose child its VM process is not: it serves its port, stops as
    /// `Stopped`, and is then gone, and the crashed client's socket directory with it (D215;
    /// Hex's proposal).
    #[cfg(unix)]
    #[test]
    fn attach_takes_back_a_guest_whose_client_crashed() {
        let dir = shards_testdir::TempDir::new("api-crash").unwrap();
        let bin = dir.path().join("shards");
        let me = std::env::current_exe().unwrap();
        installed(
            &bin,
            &format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --vsock ]; then {STANDIN_VSOCK}=$2; \
                 export {STANDIN_VSOCK}; fi\n  shift\ndone\nexec '{}' --exact \
                 client::tests::child_a_vm_process_serving_its_vsock --ignored --test-threads=1 \
                 >/dev/null 2>&1\n",
                me.display()
            ),
        );
        let client = Command::new(&me)
            .args([
                "--exact",
                "client::tests::child_a_client_that_crashes",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CRASHING_SHARDS, &bin)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&client.stdout);
        assert!(client.status.success(), "{:?}: {said}", client.status);
        // libtest says the test's name before what the test prints, on the same line.
        let line = said
            .lines()
            .find_map(|l| l.split_once("identity ").map(|(_, id)| id))
            .unwrap_or_else(|| panic!("no identity said: {said}"));
        let mut words = line.splitn(3, ' ');
        let identity = Identity {
            pid: words.next().unwrap().parse().unwrap(),
            started: words.next().unwrap().parse().unwrap(),
            vsock: words.next().unwrap().into(),
        };
        let spec = GuestSpec {
            binary: Some(bin),
            ..spec()
        };
        let back = Shards.attach(&identity, &spec).unwrap();
        assert_eq!(echoed(&*back), b"back");
        back.stop().unwrap();
        assert_eq!(back.wait_for(Duration::MAX).unwrap(), Some(End::Stopped));
        let again = Shards.attach(&identity, &spec);
        assert!(
            matches!(again, Err(Refusal::Process {
            pid, expected_started, failure: crate::ProcessFailure::Exited | crate::ProcessFailure::Os {
                operation: crate::ProcessOperation::PidfdOpen | crate::ProcessOperation::RegisterExit | crate::ProcessOperation::ReadStat,
                code: libc::ESRCH,
            } | crate::ProcessFailure::Os {
                // Reaped between its pidfd's open and its stat's: procfs's lookup finds no
                // entry (ENOENT), or its permission check of one still cached no task (ESRCH,
                // fs/proc/base.c proc_pid_permission).
                operation: crate::ProcessOperation::OpenStat,
                code: libc::ENOENT | libc::ESRCH,
            }
        }) if pid == identity.pid && expected_started == identity.started),
            "{:?}",
            again.err()
        );
        let socket_dir = identity.vsock.parent().unwrap().to_path_buf();
        drop(back);
        assert!(!socket_dir.exists(), "{} is left", socket_dir.display());
    }

    /// On Windows a guest's VM ends with the process that booted it (D141), so none outlives
    /// its client to be taken back: attach is refused `Gone` (D215; Hex's proposal).
    #[cfg(windows)]
    #[test]
    fn attach_is_refused_gone_on_windows() {
        let identity = Identity {
            pid: std::process::id(),
            started: 1,
            vsock: "v".into(),
        };
        let refused = Shards.attach(&identity, &spec());
        assert!(
            matches!(refused, Err(Refusal::Gone { .. })),
            "{:?}",
            refused.err()
        );
    }
}
