//! `shards daemon`: serves `shards run` from warm VMs (docs/design/architecture.md D26).
//!
//! One runs per SHARDS_HOME, holding the home's lock; `shards run` starts it when none
//! listens. For each template it has served, it keeps SHARDS_POOL warm VMs (default 2):
//! restored, resumed, connected, and waiting for a command (warm.rs). A run takes one, and
//! the pool refills. A run with no template yet boots a VM that saves one on the way, and
//! a run with its own kernel and init boots every time.
//!
//! Once a warm VM has taken a run, it serves the client directly, and tells the daemon
//! how the command ended. The daemon follows each run to that end, and can signal its
//! command meanwhile. It exits after SHARDS_DAEMON_IDLE seconds (default 900) without a
//! run or a run in progress. `shards daemon stop`, and a client of another build, first
//! end the runs in progress, as dockerd ends its containers when it shuts down; the next
//! daemon takes the home once this one has gone.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use shards_ipc::{Identity, Run, kind};
use shards_registry::http::Cancel;
use shards_vmm::platform::FileWatch;
use shards_vmm::vm::Config;

use crate::containers::{self, Container, Disk, Real, Registry, Removal, State as Life};

mod commands;
pub(crate) use commands::COPY_STEP;
mod builder_prune;
mod commit;
mod demand;
mod engine;
mod events;
mod files;
use crate::filters;
mod follow;
mod health;
mod hostnet;
pub(crate) mod images;
mod import;
mod info;
mod inspect;
mod inspect_doc;
mod load;
mod logs;
mod network;
mod networks;
mod packed;
mod parents;
mod processes;
mod ps;
mod publish;
mod pull;
mod push;
mod record;
mod refill;
mod relay;
mod remote;
mod restart;
mod rmi;
mod ship;
mod snapshots;
mod top;
mod update;
mod visit;
mod volume;
use crate::run::{Boot, Prepared};
use crate::segments::log_segment;
use crate::spec::{LogRetention, NOT_RUN};

const USAGE: &str = "usage: shards run daemon [--detached] [--listen ADDR:PORT]
       shards stop daemon
  Serves runs from warm microVMs; the first `shards run vm` starts it when none is running.
  It exits after SHARDS_DAEMON_IDLE seconds (default 900) without a run.
  --detached: run it in the background, writing messages to daemon.log in SHARDS_HOME, as
    a run starts it.
  --listen ADDR:PORT: also serve the machines this one enrolled (shards invite client),
    over QUIC on UDP ADDR:PORT; then it does not exit for being idle, as its clients find
    it there.
  stop daemon: end the daemon's runs, each microVM's stop signal first, then the daemon.
  SHARDS_POOL: the most warm microVMs kept for each image (default 2): as many as its
    runs have come at once, while it is used.
  SHARDS_POOL_KEEP: seconds an image's warm microVMs are kept after its last run (600).
  SHARDS_MAX_CLIENTS: the most clients served at once (default: as many as the threads
    the daemon may have hold).";

/// How long a VM may take to be ready: a restore takes milliseconds, a boot that saves a
/// template tens of them.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a VM's network process may take to say it has a run's published ports: a
/// round trip on a socket pair to a process polling it, the grace one is given to end
/// (`netproc::GRACE`).
const PUBLISH_PATIENCE: Duration = Duration::from_secs(1);
/// A template whose warm VMs fail this many times in a row is removed and saved again.
const MAX_FAILURES: u32 = 3;
const DEFAULT_POOL: usize = 2;
/// Warm VMs kept ahead of runs, all pools together, unless `SHARDS_WARM_MAX` says (audit
/// A13): a warm VM's own memory is 3.4 MiB, the rest of its RSS its template's pages it
/// shares, so the default holds about 55 MiB (PM M49).
const DEFAULT_WARM_MAX: usize = 16;
const DEFAULT_IDLE: Duration = Duration::from_secs(900);
/// How long a pool keeps warm VMs after its last claim unless `SHARDS_POOL_KEEP` says: as
/// AWS keeps an idle function, 10 minutes (Shahrad et al., "Serverless in the Wild",
/// USENIX ATC 2020, §1).
const DEFAULT_KEEP: Duration = Duration::from_secs(600);
/// Warm VMs a run may try: one can end while it waits, or before it has taken the run.
const HANDOFF_TRIES: usize = 3;
/// A stopped container's writable layer, in its directory (D37): kept whole, and being
/// written as its microVM stops.
pub(super) const LAYER: &str = "layer.tar";
/// The request a container was made by, in its directory, for `shards start`.
pub(super) const REQUEST: &str = "request";
const LAYER_NEW: &str = "layer.new";
/// A running container's swap (D159), in its directory until its microVM holds it.
const SWAP: &str = "swap";

/// A new, empty swap file of `bytes` at `at`, sparse, in place of any before it: only this
/// user's.
fn open_swap(at: &Path, bytes: u64) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(at)?;
    file.set_len(bytes)?;
    Ok(file)
}

/// A new file for a container's layer to be written to, in place of any half-written
/// one: only this user's.
fn open_layer(at: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(at)
}

/// How long a warm VM may take to say it has taken a run: it does so right after it
/// receives one.
const TAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The home's empty directory, every join share's root (D119): its server makes nothing in
/// it, and shows nothing of it.
const JOIN_ROOT: &str = "join-root";
/// The home's private directory, 0700: a build's ID file, and what a share process keeps of
/// a restored guest's files that no path reaches (D176).
const PRIVATE: &str = "tmp";
/// How long a daemon waits for the lock of one that neither listens nor ends its runs
/// (shards_ipc::exiting): one started at the same moment, which listens as soon as it
/// has the lock. One ending its runs is waited for as long as it takes.
const TAKEOVER: Duration = Duration::from_secs(20);
/// How long `shards daemon stop` lets a command end after its SIGTERM, before SIGKILL:
/// dockerd's default stop timeout (moby daemon/config/config_linux.go), which it gives
/// each container when it shuts down.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long a shutting-down daemon lets a command take to end after that SIGKILL, before
/// its VM goes too: dockerd gives up on its containers after the larger of its shutdown
/// timeout (15 s) and the stop timeout plus 5 s (moby daemon/daemon.go, ShutdownTimeout).
const SHUTDOWN_KILL: Duration = Duration::from_secs(5);
/// How often a listener out of descriptors looks for one again: nothing tells of one
/// closed anywhere in the process. And how often one whose watches could not be made
/// looks at its home.
const RETRY: Duration = Duration::from_millis(250);
/// How long a client may take to send its whole request (audit A07). One sends it as it
/// connects; one that has not by now is broken, or trickling it out.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The threads the daemon keeps besides its clients': its listener, and the followers',
/// the completer's, the files', the recorder's, the collector's, the refiller's and the
/// health checks'; and the system's, the dispatch worker a framework it calls keeps, one
/// with runs and without (PM M92, M98).
const OWN_THREADS: u64 = 9;
/// The threads a client in hand may hold: its own, and the watcher of a VM started for
/// its run, until that VM is ready.
const CLIENT_THREADS: u64 = 2;
/// The threads a process may have at the least, where the system says nothing: POSIX's
/// `_POSIX_THREAD_THREADS_MAX` (limits.h).
const POSIX_THREADS: u64 = 64;

/// The most clients in hand at once (audit A07, review 7.9): as many as the threads the
/// process may have hold, past the daemon's own, at [`CLIENT_THREADS`] each. A client is
/// in hand until its run is handed over or its command answered, or it waits long (`wait`,
/// `logs -f`, [`Daemon::waits_long`]); it holds up to six descriptors meanwhile (its
/// connection, its stdio, its container's log and a VM's socket), which the daemon waits
/// for room for where they run out. Past this many, connections wait in the listener's
/// backlog. Before, 256, chosen.
fn most_clients() -> usize {
    clients_for(shards_vmm::platform::thread_limit().unwrap_or(POSIX_THREADS))
}

/// The clients a process of `threads` threads may have in hand at once: at least one.
fn clients_for(threads: u64) -> usize {
    usize::try_from(threads.saturating_sub(OWN_THREADS) / CLIENT_THREADS)
        .unwrap_or(usize::MAX)
        .max(1)
}

pub fn daemon(args: impl Iterator<Item = OsString>) -> ExitCode {
    let all: Vec<OsString> = args.collect();
    // `--listen ADDR:PORT` (D180), wherever it is among the daemon's options.
    let mut listen = None;
    let mut args = Vec::with_capacity(all.len());
    let mut given = all.iter();
    while let Some(arg) = given.next() {
        let value = match arg.to_str() {
            Some("--listen") => given.next().and_then(|v| v.to_str()),
            Some(flag) if flag.starts_with("--listen=") => flag.strip_prefix("--listen="),
            _ => {
                args.push(arg.clone());
                continue;
            }
        };
        match value.and_then(|v| v.parse::<std::net::SocketAddr>().ok()) {
            Some(at) => listen = Some(at),
            None => {
                return fail(&format!(
                    "--listen: {value:?} is not an address and port (ADDR:PORT)\n{USAGE}"
                ));
            }
        }
    }
    let arg = |i: usize| args.get(i).and_then(|a| a.to_str());
    let result = match (args.len(), arg(0)) {
        (0, _) => serve(None, listen),
        (1, Some("--detached")) => detach(listen),
        // Its starter's (`detach`): the descriptor on which it says it serves.
        (2, Some("--ready")) => match arg(1).and_then(|fd| fd.parse::<i32>().ok()) {
            Some(fd) => ready_link(fd).and_then(|link| serve(Some(link), listen)),
            None => Err(format!("--ready: {:?} is not a descriptor", args.get(1))),
        },
        _ if crate::context::asks_help(
            &args.iter().map_while(|a| a.to_str()).collect::<Vec<_>>(),
            &["--ready"],
        ) =>
        {
            return crate::context::help_page("daemon", USAGE);
        }
        _ => Err(format!("unexpected arguments {args:?}\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// A container's membership of its microVM's switch as init says it (D181): its network,
/// its address and prefix there, its gateway and its link's hardware address, each a word;
/// none for what is not that.
fn engine_address_of(said: &[u8]) -> Option<crate::containers::EngineAddress> {
    let text = std::str::from_utf8(said).ok()?;
    let mut words = text.split(' ');
    let (network, cidr, gateway, mac) = (words.next()?, words.next()?, words.next()?, words.next()?);
    if words.next().is_some() || network.is_empty() {
        return None;
    }
    let (ip, prefix) = cidr.split_once('/')?;
    let mut octets = [0u8; 6];
    let mut parts = mac.split(':');
    for o in &mut octets {
        *o = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(crate::containers::EngineAddress {
        network: network.to_string(),
        ip: ip.parse().ok()?,
        prefix: prefix.parse().ok().filter(|p| *p <= 32)?,
        gateway: gateway.parse().ok()?,
        mac: octets,
    })
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}

/// A message in the daemon's log.
fn log(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr(), "shards daemon {}: {message}", std::process::id());
}

/// The host's hosts file, as a container on its network has it (D124): `/etc/hosts`, or
/// Windows' own; none where it cannot be read.
fn host_hosts() -> Vec<u8> {
    #[cfg(windows)]
    let path = std::env::var_os("SystemRoot")
        .map(|root| std::path::PathBuf::from(root).join(r"System32\drivers\etc\hosts"))
        .unwrap_or_default();
    #[cfg(not(windows))]
    let path = std::path::PathBuf::from("/etc/hosts");
    std::fs::read(path).unwrap_or_default()
}

/// The host's name as a guest's: at most Linux's 64 bytes (HOST_NAME_MAX), cut where a
/// character ends, as sethostname(2) takes no more.
fn guest_hostname(name: &str) -> String {
    let mut end = name.len().min(64);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.get(..end).unwrap_or_default().to_string()
}

/// A list of an Agentfile's grants as its labels write it, comma-separated (D59).
fn joined(list: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(&list.join(&b","[..])).into_owned()
}

/// A listener at `path` that a client finds listening once it finds the name, not blocking:
/// bound beside it and renamed there once it listens, where a client that found the name
/// between bind(2) and listen(2) was refused (ECONNREFUSED), as Docker's clients, which do
/// not ask again, would be as the daemon starts. The rename replaces what was there, in one
/// step.
fn listening_at(path: &Path) -> std::io::Result<UnixListener> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let unnamed = path.with_file_name(format!("~{name}"));
    let _ = std::fs::remove_file(&unnamed);
    let listener = UnixListener::bind(&unnamed)?;
    if let Err(e) = listener
        .set_nonblocking(true)
        .and_then(|()| std::fs::rename(&unnamed, path))
    {
        let _ = std::fs::remove_file(&unnamed);
        return Err(e);
    }
    Ok(listener)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A warm VM waiting for a run, and the socket it takes the run on.
struct Ready {
    vm: Arc<shards_ipc::Child>,
    socket: UnixStream,
    /// Its network process's control socket, if it has one: where its run's published
    /// ports go.
    net: Option<UnixStream>,
    /// The guest's MAC on its network, if it has one.
    mac: Option<[u8; 6]>,
    /// The pool it came from.
    pool: Option<PoolKey>,
    /// The template the working set it records goes with: its pool's, or the one it saves.
    /// The daemon writes it there; no VM may write a template (D30).
    records: Option<Records>,
    /// For a container joining another's network (D119), that container's ID: `vm` is its
    /// VM, which the joiner's run may never end, and `socket` the joiner's own connection
    /// to it.
    joined: Option<String>,
}

impl Ready {
    /// Ends the VM a run was not taken by: a joiner's (D119) is its provider's, which goes on
    /// serving its own run, and only the joiner's connection goes, with `self`.
    fn end(&self) {
        if self.joined.is_none() {
            let _ = self.vm.kill(libc::SIGKILL);
        }
    }
}

/// The template a VM's working set goes with, and the most bytes the set may take there
/// (`shards_vmm::vm::working_set_limit`): read where the VM was started, so that the
/// followers' loop, which gathers the set, reads no template.
#[derive(Debug, Clone)]
struct Records {
    dir: PathBuf,
    limit: u64,
}

/// Which pool warm VMs are of (D26): the template in `dir`, which each restores, and the
/// slice of the user's service manager they are made under (D183), empty for the
/// daemon's own place.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PoolKey {
    dir: PathBuf,
    parent: String,
}

impl PoolKey {
    fn new(dir: &Path, parent: &str) -> PoolKey {
        PoolKey {
            dir: dir.to_path_buf(),
            parent: parent.to_string(),
        }
    }
}

/// The warm VMs of one template, under one parent.
#[derive(Default)]
struct Pool {
    /// Its template's network device's MAC, if it has one, once read: each of its VMs
    /// needs a network process of its own.
    net: Option<Option<[u8; 6]>>,
    /// The most bytes a working set of its template may take, once read: none where the
    /// template could not be read, and its VMs' sets are not taken.
    working_set_limit: Option<Option<u64>>,
    /// The root filesystem its template was saved from, which the daemon knows from the
    /// run that made it: the one file a restore of it may be given (`--backing`), whatever
    /// the template's state, which a VM process wrote, names.
    rootfs: Option<PathBuf>,
    /// Whether its template was saved with the in-VM server's device (D60), once read:
    /// its restores are given the daemon's own, as its cold boots are.
    server: Option<bool>,
    /// Whether its template is a GPU's (D150), once read: its restores are the GPU's VM
    /// process's.
    gpu: Option<bool>,
    /// The emulator image's root its template was saved with (D179), once read: its
    /// restores are given it too, after the server's device.
    emulator: Option<Option<PathBuf>>,
    ready: VecDeque<Ready>,
    starting: usize,
    /// Warm VMs in a row that never became ready.
    failures: u32,
    /// Runs waiting for a VM of it.
    waiting: usize,
    /// Its runs' arrivals and its refills' times, which say how many VMs it keeps; its
    /// last claim orders eviction, least recent first.
    demand: demand::Demand,
}

/// Collections due, for the collector's thread ([`Daemon::collect_all`]).
#[derive(Default)]
struct Collecting {
    due: Mutex<bool>,
    /// One is due, the listener has descriptors again, or the thread is to return.
    changed: Condvar,
    /// The thread is to return: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

impl Collecting {
    /// Ends the collector's thread: a test's daemon's.
    #[cfg(test)]
    fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.due);
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct State {
    pools: HashMap<PoolKey, Pool>,
    /// Pooled warm VMs still starting, by pid, for the daemon to end when it exits.
    starting: HashMap<u32, Arc<shards_ipc::Child>>,
}

/// The spare container: none, one being made, or one made.
#[derive(Debug, Default)]
enum Spare {
    #[default]
    None,
    Making,
    Made(String, Log),
}

/// Who a starting VM is for.
enum For {
    Pool(PoolKey),
    Run(mpsc::Sender<Result<Ready, String>>),
}

/// A run, from its container's creation to its end (audit A06): what the container
/// commands and the daemon's shutdown consult, under `runs`' lock.
enum RunState {
    /// Created and not yet handed to a warm VM: `rm` may cancel it, and then it never
    /// starts.
    Pending { cancelled: bool },
    /// Being handed to a warm VM: it starts or fails, and commands wait to see which.
    /// `socket` is the VM's, which the handoff holds open until the run leaves this state
    /// (`start_run`): a VM says TAKEN on it before it starts anything.
    Handing { socket: RawFd },
    /// Handed over, and followed until it ends.
    Tracked(Tracked),
}

/// What a run was composed of, which each `exec` in its container starts from, and its
/// health check with the shell a `CMD-SHELL` one runs in.
struct Base {
    options: crate::spec::Options,
    health: Option<(shards_ipc::Health, Vec<String>)>,
    /// Its image's Agentfile (D109), which its execs are held to as its run was (D115).
    agentfile: Option<crate::agentfile::Agentfile>,
}

/// A run's own engine (D181): the token its init says first on each connection its
/// command makes to it, and its client's registries' environment, by which its engine
/// pulls, kept in memory alone, as a log driver's credentials are (D168).
#[derive(Clone)]
struct OwnEngine {
    token: [u8; shards_abi::run::TOKEN],
    registry_env: Vec<String>,
}

/// What a run's registration keeps of its request: a detached client, until it is told
/// whether its command started, and what each `exec` in its container starts from.
struct Keep<'a> {
    detached: Option<&'a UnixStream>,
    options: crate::spec::Options,
    /// Its health check, and the shell a `CMD-SHELL` one runs in.
    health: Option<(shards_ipc::Health, Vec<String>)>,
    /// Its published ports' host sockets: sent to its VM's network process, and held
    /// until that process has them (M24).
    published: Vec<publish::Listener>,
    /// The name a detached client gave the container it started again, said once its
    /// command starts, as `docker start` says it.
    named: Option<String>,
    /// Its start is a restart's (`shards restart vm`): said as an event of its own once it
    /// has started, for what follows microVMs by their events.
    restarted: bool,
    /// Its VM saves its writable layer as it stops (`RUN_LAYER_OUT`).
    layer_pending: bool,
    /// How many segments its log keeps (D161): its own, else the daemon's.
    log_files: u64,
    /// Its log's rotated segments are compressed (D162).
    log_compress: bool,
    /// A visit to a stopped container's files (visit.rs): no start, no end, no state of
    /// the container's changes with it.
    visit: bool,
    /// The ports its image's Agentfile grants its agents (D59), for its VM's network
    /// process: `NET_POLICY`'s payload.
    egress: Option<Vec<u8>>,
    /// Its image's Agentfile (D109), for its execs (D115).
    agentfile: Option<crate::agentfile::Agentfile>,
    /// Its own MAC (D46, PM M173), for its VM's network process (`NET_MAC`): none where it
    /// has no interface of its own.
    mac: Option<[u8; 6]>,
    /// Its link-local addresses (PM M175), for its network process and its peers'.
    link_local: Vec<std::net::IpAddr>,
    /// Its microVM's own engine (D181), where its run has one.
    engine: Option<OwnEngine>,
}

/// A run in progress: its VM's socket, to signal the command, and the VM itself.
///
/// Shared (`Arc`) because a run's lifetime is its own: its thread follows it while any
/// command may signal or settle it, and runs start and end independently of each other
/// and of the daemon. The alternatives cost more: one lock over every run's state would
/// serialize their record writes, and asking each run's thread would add a round trip per
/// run to every command (docs/audit/2026-09-30_arc.md).
/// A run's socket to its VM. A message bigger than the socket's buffer (8 KiB on macOS)
/// goes out in more than one write, and another sender's between them would spoil both,
/// and the VM's reading after them: senders take turns. A send waits for room at most
/// TAKE_TIMEOUT, as a live VM reads what it is sent at once: one that has stopped reading
/// fails it, rather than every sender after it.
struct RunSocket {
    stream: UnixStream,
    sending: Mutex<()>,
}

impl RunSocket {
    fn new(stream: UnixStream) -> RunSocket {
        if let Err(e) = stream.set_write_timeout(Some(TAKE_TIMEOUT)) {
            log(format!("a run's socket: {e}"));
        }
        RunSocket {
            stream,
            sending: Mutex::new(()),
        }
    }

    fn send(&self, kind: u8, payload: &[u8], fds: &[BorrowedFd<'_>]) -> std::io::Result<()> {
        let _turn = lock(&self.sending);
        shards_ipc::send(&self.stream, kind, payload, fds)
    }
}

struct Tracked {
    /// What its run was composed of: read, never changed, by each `exec` and health
    /// probe, on threads of their own, which share it rather than copy it each time.
    base: Arc<Base>,
    socket: Arc<RunSocket>,
    vm: Arc<shards_ipc::Child>,
    inbox: Arc<Mutex<Inbox>>,
    /// [`Keep::visit`]: the container is not running.
    visit: bool,
    /// Its guest's MAC on its network, for `inspect`.
    mac: Option<[u8; 6]>,
    /// Its network process's control socket, where peers on its network come (D46).
    net: Option<UnixStream>,
    /// For a container joining another's network (D119), that container's ID: `vm` is its
    /// VM, which this run never ends, pauses or reads as its own.
    joined: Option<String>,
}

/// What a run has told the daemon, and the socket it tells it on: read under this lock
/// alone, by the followers' loop as messages come (`follow`), and by every container
/// command before it answers (`settle`). A run tells the daemon of its start and end
/// before its client learns of them, so a command sees what any client has seen.
struct Inbox {
    socket: Arc<RunSocket>,
    /// The warm VM's process ID, for the log.
    pid: u32,
    started: bool,
    /// A detached run's client, until the daemon tells it whether its command started.
    detached: Option<UnixStream>,
    ended: bool,
    /// The template its working set goes with, and the parts of it that have come.
    records: Option<Records>,
    working_set: WorkingSet,
    /// The run's container's directory, and the log segment the VM writes there, which
    /// the daemon made.
    container: PathBuf,
    segment: u64,
    /// How many segments its log keeps (D161).
    log_files: u64,
    /// Its log's rotated segments are compressed (D162).
    log_compress: bool,
    /// Exec clients' connections handed to the VM and not yet taken, by number: held
    /// meanwhile, as XNU collects a socket in flight that no process holds (M24).
    execs_in_flight: Vec<(u64, UnixStream)>,
    /// Execs that have events, by number: each one's ID, for its `exec_die`.
    exec_ids: Vec<(u64, String)>,
    /// The container's writable layer is still to come, after its end (D37).
    layer_pending: bool,
    /// [`Keep::named`].
    named: Option<String>,
    /// [`Keep::restarted`].
    restarted: bool,
    /// What has come of the VM's next message: a VM that stops partway through one holds
    /// up no reader (`take_messages`).
    incoming: shards_ipc::Incoming,
    /// [`Keep::visit`].
    visit: bool,
    /// A joiner's exec's id in its provider's guest, from its start on (D119).
    guest: Option<u32>,
    /// On the host's network (D124): its guest's sockets of the host's, its joiners' too.
    host_net: Option<hostnet::HostNet>,
    /// How many containers joining its microVM's network run (D119): its VM, which runs on
    /// for them past its own command, is followed until it ends, for what their processes
    /// ask of the host's network and record (D123, D124).
    joiners: u32,
    /// A joiner's: its provider's run, whose microVM it runs in.
    provider: Option<Arc<Mutex<Inbox>>>,
    /// A save its VM was asked for and has yet to answer (D176).
    saving: Option<snapshots::Saving>,
    /// The snapshot it was saved as, where the save stops it: its end is the save's.
    saved_as: Option<String>,
    /// Its microVM's own engine (D181): the connections its VM hands over are its
    /// command's, served scoped to its microVM; a save keeps its token for its restore
    /// (D176), and its pulls are its client's.
    engine: Option<OwnEngine>,
}

/// A part of the working set run `id`'s VM recorded (`kind::WORKING_SET`), which the
/// daemon writes with the template it goes with once the last has come: only as much as
/// that template's guest can hold, and only if the template is still at the generation it
/// was recorded from (`shards_vmm::vm::accept_working_set`). Gathered here, in memory;
/// the whole set is returned, to be written on the files' thread. A VM that sends more,
/// or for no template, loses its prefetch and nothing else.
fn working_set_part(id: &str, inbox: &mut Inbox, payload: &[u8]) -> Option<files::Job> {
    let limit = inbox.records.as_ref()?.limit;
    match gather(&mut inbox.working_set, limit, payload) {
        Gathered::More => None,
        Gathered::Refused(why) => {
            log(format!("container {id}: its working set: {why}"));
            inbox.records = None;
            inbox.working_set = WorkingSet::default();
            None
        }
        Gathered::Whole(name, set) => inbox.records.take().map(|r| files::Job::WorkingSet {
            dir: r.dir,
            name,
            set,
        }),
    }
}

/// A working set coming in parts: the generation they name, and what has come of it.
#[derive(Debug, Default)]
struct WorkingSet {
    name: Option<String>,
    bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum Gathered {
    /// More parts are to come.
    More,
    /// The last has come: the generation, and the whole set.
    Whole(String, Vec<u8>),
    /// Malformed, past `limit` bytes, or naming another generation than the parts before.
    Refused(String),
}

/// Adds the `kind::WORKING_SET` message `payload` to `set`, which holds at most `limit`
/// bytes, each part reserved fallibly.
fn gather(set: &mut WorkingSet, limit: u64, payload: &[u8]) -> Gathered {
    let Some((flags, name, part)) = shards_ipc::working_set_part(payload) else {
        return Gathered::Refused("a malformed part".into());
    };
    match &set.name {
        Some(first) if first != name => return Gathered::Refused(format!("parts of {first} and {name}")),
        Some(_) => {}
        None => set.name = Some(name.to_string()),
    }
    let fits = (set.bytes.len() as u64).saturating_add(part.len() as u64) <= limit;
    if !fits || set.bytes.try_reserve(part.len()).is_err() {
        return Gathered::Refused(format!("more than its template's {limit} bytes"));
    }
    set.bytes.extend_from_slice(part);
    if flags & shards_ipc::WORKING_SET_LAST == 0 {
        return Gathered::More;
    }
    let taken = std::mem::take(set);
    Gathered::Whole(taken.name.unwrap_or_default(), taken.bytes)
}

/// One waiting for a container to end ([`Daemon::await_exit_held`]), by its number.
struct Waiter {
    number: u64,
    /// Written the exit code: its other end, which the waiter waits on in poll(2) beside
    /// its client's connection, then reads it.
    wake: UnixStream,
}

impl Waiter {
    /// Tells it the container's exit code.
    fn hear(&self, code: u8) {
        let _ = (&self.wake).write_all(&[code]);
    }
}

/// The pool `key`, of a template saved from `rootfs`, which it keeps as the VM saving the
/// template recorded it: resolved, links and all (vmm platform::input_path).
fn pool_of<'s>(state: &'s mut State, key: &PoolKey, rootfs: &Path) -> &'s mut Pool {
    let pool = state.pools.entry(key.clone()).or_default();
    pool.rootfs.get_or_insert_with(|| resolved(rootfs));
    pool
}

/// `rootfs` as the VM saving a template records it: resolved, links and all.
fn resolved(rootfs: &Path) -> PathBuf {
    std::fs::canonicalize(rootfs).unwrap_or_else(|_| rootfs.to_path_buf())
}

/// A run's stop as the daemon stops, once begun ([`Daemon::stop_each`]): SIGKILL at
/// `kill`, if ever, then its VM's end SHUTDOWN_KILL later.
struct Stop {
    socket: Arc<RunSocket>,
    /// Its VM, which its stop ends past its grace; none for a joiner, whose VM is its
    /// provider's, ended by the provider's own stop (D119).
    vm: Option<Arc<shards_ipc::Child>>,
    kill: Option<Instant>,
    killed: bool,
}

impl Stop {
    /// When its next step is due, if it has one.
    fn next(&self) -> Option<Instant> {
        let kill = self.kill?;
        if self.killed {
            kill.checked_add(SHUTDOWN_KILL)
        } else {
            Some(kill)
        }
    }
}

/// Why a template's pool gave no warm VM.
enum Claim {
    /// Its warm VMs keep failing: the template does not restore.
    Broken,
    Failed(String),
}

struct Daemon<D: Disk = Real> {
    home: PathBuf,
    /// Docker's default bridge, on which runs' guests are, elected as the daemon started;
    /// none if no subnet was free. Every template it saves on the bridge is named by its
    /// subnet, on the guest's command line (run.rs, `template`), so its pools' VMs are all
    /// on this one.
    bridge: Option<shards_net::bridge::Bridge>,
    /// shards-vm, beside this binary: each VM runs in a process of its own.
    vm: PathBuf,
    identity: Identity,
    /// The socket, relative to the working directory, the home.
    socket: &'static Path,
    /// The most warm VMs a template's pool keeps ([`demand`]).
    target: usize,
    /// Warm VMs kept ahead of runs, all pools together ([`DEFAULT_WARM_MAX`]).
    warm_max: usize,
    idle: Duration,
    /// How long a pool keeps warm VMs after its last claim ([`DEFAULT_KEEP`]).
    keep: Duration,
    /// How much of each container's output its log keeps.
    logs: LogRetention,
    /// How long a client may take to send its request ([`REQUEST_TIMEOUT`]).
    request_timeout: Duration,
    state: Mutex<State>,
    changed: Condvar,
    /// Clients in hand: connected, and not yet handed over, answered, or waiting long.
    busy: AtomicUsize,
    /// The most clients in hand at once ([`most_clients`]).
    max_clients: usize,
    /// The clients that wait long, by number, in hand no more ([`Daemon::waits_long`]).
    long_waits: Mutex<HashSet<u64>>,
    /// The connections of clients in hand whose threads the daemon's shutdown ends, by
    /// number: those still sending their request, and those of container commands. A run's
    /// connection is its command's once its request is read, and leaves here then.
    clients: Mutex<HashMap<u64, Arc<UnixStream>>>,
    /// The clients whose request no thread has taken yet: one is answered either by its
    /// thread, which takes it from here once it has read the request, or, as the daemon
    /// steps aside first, with RESTART (`end_clients`), never both nor neither.
    unread: Mutex<std::collections::HashSet<u64>>,
    next_client: AtomicU64,
    /// Written to wake the listener: a client left, a run ended (`wake_listener`). Its
    /// other end, which the listener waits on, read.
    listener_wake: (UnixStream, UnixStream),
    /// What runs still being prepared are downloading, by client: a shutdown cancels it.
    preparing: Mutex<HashMap<u64, Cancel>>,
    last: Mutex<Instant>,
    stopping: AtomicBool,
    /// The socket is gone: no client arrives from here on.
    closed: AtomicBool,
    /// Held as the socket is removed by [`close`](Self::close) and as the listener binds
    /// it again ([`relisten`](Self::relisten)): a daemon's own removal is never undone.
    socket_at: Mutex<()>,
    /// The connections of `shards daemon stop`, held open until this daemon has let go
    /// of its home, which is how they learn it has exited.
    stoppers: Mutex<Vec<UnixStream>>,
    /// Every run whose container exists and has not ended, by the container's ID.
    runs: Mutex<HashMap<String, RunState>>,
    /// A run's start was decided: it runs, or never will.
    resolved: Condvar,
    /// `shards daemon stop` asked this daemon to end its runs.
    ending: AtomicBool,
    /// Every run's container.
    containers: Mutex<Registry>,
    /// Where the containers' records are kept: the host's file system, or a test's. Each
    /// of the registry's writes, and each record or removal finished outside its lock,
    /// borrows it.
    disk: D,
    /// A reserved container was let be seen, or taken out; or a container taken out was
    /// set aside, or put back.
    arrived: Condvar,
    /// The containers' records to write, and the thread that writes them (record.rs).
    recording: record::Recording,
    /// Who waits for each running container to end, for its exit code: `shards wait`,
    /// `stop`, `kill`, `rm -f`. Told under the containers' lock, as the record changes.
    /// Each waiter has a number, by which it goes if it stops waiting first.
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    next_waiter: AtomicU64,
    /// Numbers each exec handed to a VM (`kind::EXEC_RUN`).
    next_exec: AtomicU64,
    /// Each running container's health, if it has a check: in memory, as dockerd keeps it.
    health: Mutex<HashMap<String, health::State>>,
    /// The host addresses runs publish, from their binding until the network process
    /// that took them has gone, and told as each goes.
    ports_held: Mutex<Vec<publish::Held>>,
    ports_freed: Condvar,
    /// The containers `shards rm` is removing.
    removing: Mutex<HashSet<String>>,
    /// What has happened, for `shards events`.
    events: events::Events,
    /// Containers whose writable layer their stopped VM is still saving (D37), and when
    /// one is no more.
    settling: Mutex<HashSet<String>>,
    settled: Condvar,
    /// Each container's restart manager, as its runs end (restart.rs).
    restarts: Mutex<HashMap<String, restart::Manager>>,
    /// The containers the daemon itself is starting again, for their policies: such a
    /// start keeps its restart count, where `shards start` resets it.
    restarting_now: Mutex<HashSet<String>>,
    /// Stopped containers whose files a VM is visiting (visit.rs), and its end's wakeup:
    /// one visit at a time each, and no start meanwhile.
    visiting: Mutex<HashSet<String>>,
    visited: Condvar,
    /// The containers `shards pause` froze: their VM processes stopped (SIGSTOP), until
    /// `unpause`, or a stop or kill, lets them go on.
    paused: Mutex<HashSet<String>>,
    /// Each running microVM's join share process (D119), by the ID of the container whose
    /// run it is: the daemon's connection to it, on which each joiner's link to it goes.
    /// Made as the first container joining that microVM's network brings volumes.
    join_shares: Mutex<HashMap<String, UnixStream>>,
    /// Each joiner's link to its microVM's join share, by its ID, with the ID of the
    /// container whose microVM it is: its volumes are served until the link closes, as it
    /// ends.
    joiner_links: Mutex<HashMap<String, (String, UnixStream)>>,
    /// Each user network's members, by its ID: the endpoints of its running containers
    /// (D46).
    members: Mutex<std::collections::BTreeMap<String, Vec<networks::Member>>>,
    /// A container's ID, directory and log, made ahead of the run that takes them.
    spare: Mutex<Spare>,
    /// Numbers the templates a run saves before they become the template.
    saved: AtomicU64,
    /// Collections, run by the collector's thread: one due at start, and once a pull has
    /// moved a reference (audit A13).
    collecting: Collecting,
    /// The listener is out of descriptors: no collection starts, whose files would take
    /// what its clients wait for.
    starving: AtomicBool,
    /// The home is gone or going ([`Daemon::home_going`]): the daemon steps aside, and
    /// removes what is left of it as it exits.
    home_gone: AtomicBool,
    /// Every run followed to its end by one thread (follow.rs).
    followers: follow::Followers,
    /// The removals of `--rm` containers that ended, made durable together (follow.rs).
    completing: follow::Completing,
    /// The files runs' VMs ask for, made on a thread of their own (files.rs).
    files: files::Files,
    /// The rotated log segments to compress, on the packer's thread (packed.rs, D162).
    packing: packed::Packing,
    /// The logs being sent where their drivers say, a thread each (ship.rs, D163).
    shipping: ship::Shipping,
    /// The Engine API's calls in hand, and what it keeps between them (engine.rs, D170).
    engine: engine::Engine,
    /// The pools to refill and the spare to make, on the refiller's thread (refill.rs).
    refills: refill::Refills,
    /// Each `--cgroup-parent`'s spawner, placed once as its first run asks (D183).
    parents: parents::Parents,
    /// When each running container's health check is next due (health.rs).
    checks: health::Checks,
    /// Runs waiting for a VM booted for them, by number: told as the daemon stops.
    booting: Mutex<HashMap<u64, mpsc::Sender<Result<Ready, String>>>>,
    next_cold: AtomicU64,
    /// The home's lock, held while this daemon lives.
    home_lock: File,
    /// The home's device and inode as this daemon took it: the directory it removes what
    /// is left of as its removal goes, and no other made at its path.
    home_id: Option<(u64, u64)>,
}

/// The daemon's threads: each borrows the daemon and is joined before it goes, so none
/// outlives it (docs/audit/2026-09-30_arc.md).
type Threads<'s, 'e> = std::thread::Scope<'s, 'e>;

/// A client in hand, by its number: counted until its run is handed over or refused, or
/// its command answered.
struct Busy<'a, D: Disk>(&'a Daemon<D>, u64);

impl<D: Disk> Drop for Busy<'_, D> {
    fn drop(&mut self) {
        *lock(&self.0.last) = Instant::now();
        let mut clients = lock(&self.0.clients);
        clients.remove(&self.1);
        // One that waited long counts no more already.
        if !lock(&self.0.long_waits).remove(&self.1) {
            self.0.busy.fetch_sub(1, Ordering::SeqCst);
        }
        drop(clients);
        self.0.wake_listener();
    }
}

/// Takes the descriptor `fd` its starter left this process to say on that it serves.
fn ready_link(fd: i32) -> Result<File, String> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl(2) asks whether the descriptor is open.
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(format!("--ready: {fd} is not a descriptor of its own"));
    }
    // SAFETY: an open descriptor its starter left this process alone, owned from here
    // on, and closed on exec.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    // SAFETY: as above.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Starts the daemon in the background, and returns once it serves: in a session of its
/// own, writing to the log in its home, and orphaned as this process exits, so that init
/// adopts it (or the nearest subreaper) and reaps it when it exits. A daemon left the
/// child of the `shards run` that started it would stay a zombie after it exits, for as
/// long as that client lives (APUE 13.3; XNU proc_exit reparents orphans to launchd). One
/// that ends before it serves has found another daemon serving its home, or failed,
/// which this says with its log's path, rather than leave its client to wait for it
/// (review 8.14).
fn detach(listen: Option<std::net::SocketAddr>) -> Result<(), String> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    // Settings it cannot keep are its starter's to hear, at once.
    settings()?;
    let home = shards_ipc::home()?;
    shards_vmm::platform::create_private_dir(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    let path = shards_ipc::log(&home);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let (mut ready, says) = io::pipe().map_err(|e| format!("a pipe for the daemon's start: {e}"))?;
    let listening = listen.map(|at| at.to_string());
    let mut argv: Vec<&OsStr> = vec!["daemon".as_ref(), "--ready".as_ref(), "3".as_ref()];
    if let Some(at) = &listening {
        argv.extend(["--listen".as_ref(), OsStr::new(at)]);
    }
    let daemon = shards_ipc::spawn(
        &exe,
        &argv,
        &[
            (null.as_fd(), 0),
            (log.as_fd(), 1),
            (log.as_fd(), 2),
            (says.as_fd(), 3),
        ],
        true,
    )
    .map_err(|e| format!("starting the daemon {}: {e}", exe.display()))?;
    drop(says);
    let mut byte = [0u8; 1];
    let read = loop {
        match ready.read(&mut byte) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            read => break read,
        }
    };
    if matches!(read, Ok(1)) {
        return Ok(());
    }
    // It ended without serving: its status says whether it found another daemon serving.
    match daemon.wait() {
        Ok(0) => Ok(()),
        Ok(status) => Err(format!(
            "the daemon exited with status {status} before it served; see {}",
            path.display()
        )),
        Err(e) => Err(format!("the daemon: {e}; see {}", path.display())),
    }
}

/// Raises this process's descriptor limit as far as it may go (`platform::
/// raise_descriptor_limit`): the daemon holds a socket for each warm VM and each run, and
/// each client in hand holds several more. Returns the limit it has.
fn raise_descriptor_limit() -> Option<u64> {
    match shards_vmm::platform::raise_descriptor_limit() {
        Ok(limit) => Some(limit),
        Err(e) => {
            log(format!("raising the descriptor limit: {e}"));
            shards_vmm::platform::descriptor_limit()
        }
    }
}

/// A count from the setting `name`, or `default`: a malformed one is refused, not taken
/// for the default (audit A14).
fn count(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{name}: {v:?} is not a count")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("{name}: {e}")),
    }
}

/// How much of a container's output its log keeps unless the settings say otherwise: as
/// Docker's `local` log driver keeps it, five files of 20 MiB (its `max-size` of 20m and
/// `max-file` of 5, docs.docker.com engine/logging/drivers/local).
const DEFAULT_LOGS: LogRetention = LogRetention {
    size: 20 << 20,
    files: 5,
};

/// A daemon's settings.
#[derive(Debug, Clone, Copy)]
struct Settings {
    /// Warm VMs kept ahead of runs for each template: `SHARDS_POOL`. With 0 each run
    /// restores its own.
    pool: usize,
    /// Warm VMs kept ahead of runs, all pools together: `SHARDS_WARM_MAX`.
    warm_max: usize,
    /// How long it stays with nothing to do: `SHARDS_DAEMON_IDLE`, in seconds.
    idle: Duration,
    /// How long a template's pool keeps warm VMs after its last claim:
    /// `SHARDS_POOL_KEEP`, in seconds.
    keep: Duration,
    /// How much of each container's output its log keeps: `SHARDS_LOG_MAX_SIZE` and
    /// `SHARDS_LOG_MAX_FILE`.
    logs: LogRetention,
    /// Clients in hand at once: `SHARDS_MAX_CLIENTS`, at most [`most_clients`].
    max_clients: usize,
    /// Docker's default bridge, as [`elect_bridge`] found it: none if no subnet is free.
    bridge: Option<shards_net::bridge::Bridge>,
}

/// The daemon's settings, checked before it serves: a malformed or excessive one stops it
/// (audit A14).
fn settings() -> Result<Settings, String> {
    let as_usize = |name: &str, n: u64| usize::try_from(n).map_err(|_| format!("{name}: {n} is too many"));
    let most = most_clients();
    let max_clients = as_usize("SHARDS_MAX_CLIENTS", count("SHARDS_MAX_CLIENTS", most as u64)?)?;
    if max_clients == 0 {
        return Err("SHARDS_MAX_CLIENTS: the daemon serves at least one client".into());
    }
    if max_clients > most {
        return Err(format!(
            "SHARDS_MAX_CLIENTS: {max_clients} is more than the {most} the threads the daemon may have hold"
        ));
    }
    let warm_max = as_usize(
        "SHARDS_WARM_MAX",
        count("SHARDS_WARM_MAX", DEFAULT_WARM_MAX as u64)?,
    )?;
    // Each warm VM takes a thread of the daemon while it starts, as a client does.
    if warm_max > most {
        return Err(format!("SHARDS_WARM_MAX: {warm_max} is more than {most}"));
    }
    let pool = as_usize("SHARDS_POOL", count("SHARDS_POOL", DEFAULT_POOL as u64)?)?;
    if pool > warm_max {
        return Err(format!(
            "SHARDS_POOL: {pool} is more than the {warm_max} warm VMs all pools may keep (SHARDS_WARM_MAX)"
        ));
    }
    let idle = Duration::from_secs(count("SHARDS_DAEMON_IDLE", DEFAULT_IDLE.as_secs())?);
    let keep = Duration::from_secs(count("SHARDS_POOL_KEEP", DEFAULT_KEEP.as_secs())?);
    let logs = LogRetention {
        size: count("SHARDS_LOG_MAX_SIZE", DEFAULT_LOGS.size)?,
        files: count("SHARDS_LOG_MAX_FILE", DEFAULT_LOGS.files)?,
    };
    if logs.size == 0 {
        return Err("SHARDS_LOG_MAX_SIZE: a log keeps at least a byte".into());
    }
    if logs.files == 0 {
        return Err("SHARDS_LOG_MAX_FILE: a log keeps at least one file".into());
    }
    Ok(Settings {
        pool,
        warm_max,
        idle,
        keep,
        logs,
        max_clients,
        bridge: elect_bridge(),
    })
}

/// Docker's default bridge, elected as the daemon starts and kept for its life, as dockerd
/// keeps the one it makes as it starts (shards_net::bridge::elected_here); what it is, in the log.
fn elect_bridge() -> Option<shards_net::bridge::Bridge> {
    let bridge = shards_net::bridge::elected_here(&mut |note| log(note));
    match &bridge {
        Some(b) => log(format!("the default bridge is {b}")),
        None => log(shards_net::bridge::NO_SUBNET),
    }
    bridge
}

fn serve(ready: Option<File>, listen: Option<std::net::SocketAddr>) -> Result<(), String> {
    let mut settings = settings()?;
    // A daemon its clients reach does not end for having had nothing to do (D180).
    if listen.is_some() {
        settings.idle = Duration::MAX;
    }
    let descriptors = raise_descriptor_limit();
    let home = shards_ipc::home()?;
    shards_vmm::platform::create_private_dir(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    // The socket's name is relative to the home (shards_ipc::SOCKET). The home is the
    // daemon's own, so this holds no client's directory busy.
    std::env::set_current_dir(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    let socket = Path::new(shards_ipc::SOCKET);
    let Some(home_lock) = take_lock(&home, socket)? else {
        // Another daemon serves this home.
        return Ok(());
    };
    let pid_file = home.join("daemon.pid");
    std::fs::write(&pid_file, format!("{}\n", std::process::id()))
        .map_err(|e| format!("{}: {e}", pid_file.display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    // Every child the daemon makes, its spawner makes, made now, before the daemon holds a
    // run's listener or a client's stdio: what the daemon lets go of, no child holds, as
    // one made here would until its exec, a port bound past its run's end (PM M158). It
    // works in the home, with the daemon's limits, as the children it makes do.
    shards_ipc::start_spawner(&exe, &[OsStr::new("spawner")])
        .map_err(|e| format!("the daemon's spawner: {e}"))?;
    // Its daemon is gone, since this one holds the lock.
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(crate::context::LISTENING);
    let _ = std::fs::remove_file(home.join(shards_ipc::STOPPING));
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", home.join(socket).display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("{}: {e}", home.join(socket).display()))?;
    // The Engine API's (D170), beside it: one that cannot be made is said, and the daemon
    // serves its CLI without it.
    let _ = std::fs::remove_file(engine::SOCKET);
    let api = match listening_at(Path::new(engine::SOCKET)) {
        Ok(api) => Some(api),
        Err(e) => {
            log(format!(
                "{}: {e}; the Engine API is not served",
                home.join(engine::SOCKET).display()
            ));
            None
        }
    };
    let identity = Identity::of_build(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let vm = crate::helpers::vm()?;
    let containers = Registry::open(&home, &mut |note| log(note))
        .map_err(|e| format!("{}: {e}", home.join("containers").display()))?;
    // What a daemon's end cut short of a snapshot's save or removal (D176), before any
    // save of this one's; and the files a share process made for a restored guest, which
    // a crash left before it removed them.
    for c in containers.all() {
        snapshots::remove_unsaved(&containers.dir(&c.id), &mut |note| log(note));
    }
    snapshots::remove_ghosts(&home.join(PRIVATE), &mut |note| log(note));
    let daemon = Daemon::new(home, vm, identity, settings, containers, Real, home_lock)
        .map_err(|e| format!("following runs: {e}"))?;
    daemon.make_spare();
    log(format!(
        "serving {} on {}, with up to {} descriptors open",
        daemon.home.display(),
        daemon.home.join(daemon.socket).display(),
        descriptors.map_or_else(|| "an unknown number of".to_string(), |n| n.to_string())
    ));
    // Other machines' clients (D180), before it says it serves: an address it cannot bind
    // is its starter's to hear.
    if let Some(at) = listen {
        remote::listen(daemon.home.clone(), exe.clone(), at)?;
    }
    // Its starter returns once it hears this: clients connect now.
    if let Some(mut ready) = ready {
        let _ = ready.write_all(&[1]);
    }
    // The daemon exits from within, so its threads are never waited for here.
    std::thread::scope(|threads| {
        daemon.start_completer(threads);
        daemon.start_recorder(threads);
        daemon.start_packer(threads);
        // What a daemon before this one left uncompressed, and unsent, as it ended.
        daemon.pack_left();
        daemon.ship_left(threads);
        daemon.start_collector(threads);
        daemon.restart_at_start(threads);
        daemon.listen(threads, listener, api);
    });
    Ok(())
}

/// Whether this process could open another descriptor now: one duplicated and closed.
/// Whether `why`, an error's text, says a process or the system was out of descriptors:
/// std writes an OS error's code after its text, `(os error N)`, whatever the C library
/// calls it, and the image store passes its errors on as text.
fn short_of_descriptors(why: &str) -> bool {
    [libc::EMFILE, libc::ENFILE]
        .iter()
        .any(|code| why.ends_with(&format!("(os error {code})")))
}

fn descriptor_free(any: &impl AsRawFd) -> bool {
    // SAFETY: fcntl(2) duplicating a descriptor we hold; the copy is closed at once.
    let copy = unsafe { libc::fcntl(any.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return false;
    }
    // SAFETY: closing the copy just made, which nothing else holds.
    unsafe { libc::close(copy) };
    true
}

/// Whether `conn`'s peer goes before `ended`'s does: its end read before anything it
/// sends. Nothing is taken from `conn`: it is peeked at.
fn hung_up(conn: &UnixStream, ended: &UnixStream) -> bool {
    let mut polled = [conn.as_raw_fd(), ended.as_raw_fd()].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // SAFETY: poll(2) on two pollfds of descriptors this thread holds open.
        if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        let [client, own] = polled;
        if own.revents != 0 {
            return false;
        }
        if client.revents == 0 {
            continue;
        }
        let mut byte = 0u8;
        // SAFETY: recv(2) of at most one byte into a local, with MSG_PEEK: the byte stays
        // for whoever reads the connection.
        let peeked = unsafe {
            libc::recv(
                conn.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match peeked {
            0 => return true,
            1.. => return false,
            _ => match io::Error::last_os_error().kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {}
                _ => return true,
            },
        }
    }
}

/// Whether `socket` has something to read now: a message, or its end.
fn readable(socket: &UnixStream) -> bool {
    readable_fd(socket.as_raw_fd())
}

/// Whether `fd` has something to read, or has ended, now.
fn readable_fd(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) on one valid pollfd, without waiting.
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
}

/// The home's lock, or `None` if another daemon serves the home. One that finds no
/// daemon listening waits a while for the lock: the last daemon may be exiting.
fn take_lock(home: &Path, socket: &Path) -> Result<Option<File>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = home.join("daemon.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut deadline = Instant::now() + TAKEOVER;
    loop {
        // SAFETY: flock(2) on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("{}: {e}", path.display()));
        }
        if UnixStream::connect(socket).is_ok() {
            return Ok(None);
        }
        if shards_ipc::exiting(home) {
            deadline = Instant::now() + TAKEOVER;
        } else if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl<D: Disk> Daemon<D> {
    /// A daemon of `home`, whose lock it holds, serving no run yet.
    fn new(
        home: PathBuf,
        vm: PathBuf,
        identity: Identity,
        settings: Settings,
        containers: Registry,
        disk: D,
        home_lock: File,
    ) -> io::Result<Daemon<D>> {
        use std::os::unix::fs::MetadataExt as _;
        let home_id = std::fs::symlink_metadata(&home).ok().map(|m| (m.dev(), m.ino()));
        Ok(Daemon {
            home,
            bridge: settings.bridge,
            vm,
            identity,
            socket: Path::new(shards_ipc::SOCKET),
            target: settings.pool,
            warm_max: settings.warm_max,
            idle: settings.idle,
            keep: settings.keep,
            logs: settings.logs,
            request_timeout: REQUEST_TIMEOUT,
            state: Mutex::default(),
            changed: Condvar::new(),
            busy: AtomicUsize::new(0),
            max_clients: settings.max_clients,
            long_waits: Mutex::default(),
            clients: Mutex::default(),
            unread: Mutex::default(),
            next_client: AtomicU64::new(0),
            listener_wake: {
                let (tell, heard) = UnixStream::pair()?;
                tell.set_nonblocking(true)?;
                heard.set_nonblocking(true)?;
                (tell, heard)
            },
            preparing: Mutex::default(),
            last: Mutex::new(Instant::now()),
            stopping: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            socket_at: Mutex::new(()),
            stoppers: Mutex::default(),
            runs: Mutex::default(),
            resolved: Condvar::new(),
            ending: AtomicBool::new(false),
            containers: Mutex::new(containers),
            disk,
            arrived: Condvar::new(),
            recording: record::Recording::default(),
            waiters: Mutex::default(),
            next_waiter: AtomicU64::new(0),
            next_exec: AtomicU64::new(0),
            health: Mutex::new(HashMap::new()),
            ports_held: Mutex::default(),
            ports_freed: Condvar::new(),
            removing: Mutex::default(),
            paused: Mutex::default(),
            join_shares: Mutex::default(),
            joiner_links: Mutex::default(),
            members: Mutex::default(),
            events: events::Events::default(),
            settling: Mutex::default(),
            settled: Condvar::new(),
            restarts: Mutex::default(),
            restarting_now: Mutex::default(),
            visiting: Mutex::default(),
            visited: Condvar::new(),
            spare: Mutex::default(),
            saved: AtomicU64::new(0),
            collecting: Collecting {
                due: Mutex::new(true),
                ..Collecting::default()
            },
            starving: AtomicBool::new(false),
            home_gone: AtomicBool::new(false),
            followers: follow::Followers::new()?,
            completing: follow::Completing::default(),
            files: files::Files::default(),
            packing: packed::Packing::default(),
            shipping: ship::Shipping::default(),
            engine: engine::Engine::default(),
            refills: refill::Refills::default(),
            parents: parents::Parents::default(),
            checks: health::Checks::default(),
            booting: Mutex::default(),
            next_cold: AtomicU64::new(0),
            home_lock,
            home_id,
        })
    }

    /// Removes the socket, once: no client arrives from here on.
    fn close(&self) {
        let _at = lock(&self.socket_at);
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = std::fs::remove_file(self.socket);
            let _ = std::fs::remove_file(engine::SOCKET);
        }
    }

    /// Binds the socket again where someone removed it while the daemon serves.
    ///
    /// `closing` is what the listener read as its pass began. A stop since, handled on
    /// another thread, has closed the daemon and removed the socket itself, so whether it
    /// has is asked again under `socket_at`, which [`close`](Self::close) holds. Read once,
    /// a stopping daemon took its own removal for another's and listened again. Its stop's
    /// next client then reached it as it exited, and its run failed: "the command's
    /// microVM stopped before the command ended" (CI's guest-memory runs, PM M188).
    fn relisten(&self, closing: bool, listener: &mut UnixListener) {
        if closing || std::fs::symlink_metadata(self.socket).is_ok() {
            return;
        }
        let _at = lock(&self.socket_at);
        if self.closed.load(Ordering::SeqCst) || std::fs::symlink_metadata(self.socket).is_ok() {
            return;
        }
        match UnixListener::bind(self.socket).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
            Ok(l) => {
                log(format!(
                    "{} was removed; listening there again",
                    self.home.join(self.socket).display()
                ));
                *listener = l;
            }
            Err(e) => log(format!("{}: {e}", self.home.join(self.socket).display())),
        }
    }

    /// Binds the Engine API's socket again where someone removed it while the daemon
    /// serves, as [`relisten`](Self::relisten) binds the daemon's.
    fn relisten_api(&self, closing: bool, api: &mut Option<UnixListener>) {
        let path = Path::new(engine::SOCKET);
        if closing || api.is_none() || std::fs::symlink_metadata(path).is_ok() {
            return;
        }
        let _at = lock(&self.socket_at);
        if self.closed.load(Ordering::SeqCst) || std::fs::symlink_metadata(path).is_ok() {
            return;
        }
        match listening_at(path) {
            Ok(l) => {
                log(format!(
                    "{} was removed; listening there again",
                    self.home.join(path).display()
                ));
                *api = Some(l);
            }
            Err(e) => log(format!("{}: {e}", self.home.join(path).display())),
        }
    }

    /// Accepts clients until it is time to exit: when stopped, or idle for `idle`. Then it
    /// removes the socket, so no client arrives after, serves any that already did, and
    /// exits.
    fn listen<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        mut listener: UnixListener,
        mut api: Option<UnixListener>,
    ) {
        // Out of descriptors, since when: said once, not every retry.
        let mut starved_since: Option<Instant> = None;
        // The home, whose removal and its socket's the listener must see, and its images,
        // where a pull leaves a collection due (pull.rs, `collect_due`). Where a watch
        // cannot be made, the listener looks again every RETRY.
        let watch = |dir: &Path| match File::open(dir).and_then(|d| FileWatch::new(&d)) {
            Ok(w) => Some(w),
            Err(e) => {
                log(format!("watching {}: {e}", dir.display()));
                None
            }
        };
        let images = self.home.join("images");
        // The contexts this home joined, whose engines it serves (relay.rs) as each comes
        // and goes.
        let contexts = self.home.join("contexts");
        for dir in [&images, &contexts] {
            if let Err(e) = shards_vmm::platform::create_private_dir(dir) {
                log(format!("{}: {e}", dir.display()));
            }
        }
        let watches = [watch(&self.home), watch(&images), watch(&contexts)];
        let mut relays = relay::Relays::default();
        let mut contexts_changed = true;
        loop {
            let closing = self.closed.load(Ordering::SeqCst);
            // A daemon whose home is gone has nothing left to serve, and its runs' containers
            // are gone with it, so no command could reach them: they end as `daemon stop`
            // ends them, rather than run on unseen.
            if !closing && self.home_going() {
                self.home_gone.store(true, Ordering::SeqCst);
                self.step_aside(threads, &format!("{} is gone", self.home.display()));
                continue;
            }
            // Someone may remove the socket while the daemon runs; a removal of the home,
            // which takes the lock too, is seen above once it has.
            self.relisten(closing, &mut listener);
            self.relisten_api(closing, &mut api);
            // The engines of the contexts this home joined (relay.rs): served while the
            // daemon is open, and their sockets removed as it closes, the clients that
            // reached one by then relayed.
            let relays_ready = if closing {
                self.close_relays(threads, &mut relays);
                true
            } else {
                self.relay_contexts(&mut relays, contexts_changed || watches[2].is_none());
                self.relay_waiting(threads, &relays)
            };
            // The Engine API's clients, each served on a thread of its own as long as it
            // keeps its connection: not counted among the clients in hand, whose cap is
            // for requests, as each of its calls is counted as it is put to the daemon.
            // One that cannot be accepted now (out of descriptors) waits for the retry.
            let mut api_ready = true;
            if let Some(api) = &api {
                loop {
                    match api.accept() {
                        Ok((conn, _)) => self.take_engine(threads, conn, None),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            log(format!("accepting an API client: {e}"));
                            api_ready = false;
                            break;
                        }
                    }
                }
            }
            // What runs' commands asked of their own microVMs' engines (D181), each served
            // scoped to its microVM.
            for (provider, conn) in self.engine.take_arrived() {
                self.take_engine(threads, conn, Some(engine::Scope { provider }));
            }
            let mut starved = false;
            while self.busy.load(Ordering::SeqCst) < self.max_clients {
                // An accept that fails for want of a descriptor drops its client on macOS
                // (xnu-11417.101.15 bsd/kern/uipc_syscalls.c, accept_nocancel), and leaves
                // it queued on Linux (net/socket.c, __sys_accept4_file): on macOS it
                // accepts only when a descriptor is free, each time, and on Linux once it
                // has found none, until it has room again. Looking once starved alone
                // dropped a second client on macOS: the first accepted after room came
                // took the last descriptor, and the next found none.
                if (cfg!(target_vendor = "apple") || starved_since.is_some()) && !descriptor_free(&listener) {
                    if starved_since.is_none() {
                        log("accepting: no descriptor is free; clients wait until the daemon has room");
                        starved_since = Some(Instant::now());
                        self.starving.store(true, Ordering::SeqCst);
                    }
                    starved = true;
                    break;
                }
                match listener.accept() {
                    Ok((conn, _)) => {
                        if let Some(since) = starved_since.take() {
                            log(format!("accepting again, after {:?}", since.elapsed()));
                            self.have_room();
                        }
                        self.take(threads, conn);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // Out of descriptors or memory: the listener stays readable, so it waits
                    // for room (below) rather than spin.
                    Err(e)
                        if matches!(
                            e.raw_os_error(),
                            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                        ) =>
                    {
                        if starved_since.is_none() {
                            log(format!("accepting: {e}; clients wait until the daemon has room"));
                            starved_since = Some(Instant::now());
                            self.starving.store(true, Ordering::SeqCst);
                        }
                        starved = true;
                        break;
                    }
                    Err(e) => {
                        log(format!("accepting: {e}"));
                        break;
                    }
                }
            }
            self.age_pools();
            self.mark_collection();
            let quiet = self.busy.load(Ordering::SeqCst) == 0
                && lock(&self.runs).is_empty()
                && self.engine.open.load(Ordering::SeqCst) == 0;
            let idle = quiet && lock(&self.last).elapsed() >= self.idle;
            if !self.closed.load(Ordering::SeqCst) && (self.stopping.load(Ordering::SeqCst) || idle) {
                self.close();
                // A client may have connected before the socket went.
                continue;
            }
            if self.closed.load(Ordering::SeqCst) && quiet {
                // Closed by another thread since this pass began: the contexts' sockets go
                // first, on the next.
                if !relays.is_empty() {
                    continue;
                }
                self.exit();
            }
            // At the cap, or starved, the listener would be readable at once: it waits for
            // a client to leave instead.
            let accepting = !starved && self.busy.load(Ordering::SeqCst) < self.max_clients;
            let looking = starved || !api_ready || !relays_ready || watches.iter().any(Option::is_none);
            let timeout = if looking {
                Some(RETRY.min(self.next_duty(quiet).unwrap_or(RETRY)))
            } else {
                self.next_duty(quiet)
            };
            contexts_changed = self.wait_for_work(
                accepting.then_some(&listener),
                api.as_ref().filter(|_| api_ready),
                relays_ready.then_some(&relays),
                &watches,
                timeout,
            );
        }
    }

    /// Whether the home is gone, or going: removed, or its lock unlinked, which none but a
    /// removal of the home does, and after which a new daemon would take the home as free.
    /// A removal takes a directory's names before the directory: one that took the
    /// socket, which the listener made again, and then the lock, is seen once it has.
    fn home_going(&self) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::symlink_metadata(&self.home).is_err()
            || self.home_lock.metadata().is_ok_and(|m| m.nlink() == 0)
    }

    /// Whether the home this daemon took is being removed where it is: its lock unlinked,
    /// and its path still the directory this daemon started in.
    fn removed_here(&self) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        self.home_lock.metadata().is_ok_and(|m| m.nlink() == 0)
            && self.home_id.is_some()
            && std::fs::symlink_metadata(&self.home)
                .ok()
                .map(|m| (m.dev(), m.ino()))
                == self.home_id
    }

    /// Wakes the listener: something it waits for may have happened.
    fn wake_listener(&self) {
        // A full buffer is a wakeup already pending.
        let _ = (&self.listener_wake.0).write(&[0]);
    }

    /// How long until the listener has something to do that no client asks for: the idle
    /// exit, once nothing runs (`quiet`), or a pool's keep-alive running out.
    fn next_duty(&self, quiet: bool) -> Option<Duration> {
        let now = Instant::now();
        let idle = quiet
            .then(|| lock(&self.last).checked_add(self.idle))
            .flatten()
            .map(|at| at.saturating_duration_since(now));
        let aging = lock(&self.state)
            .pools
            .values()
            .filter(|p| p.waiting == 0 && !p.ready.is_empty())
            // Never claimed from, it has expired already (`Demand::expired`).
            .filter_map(|p| {
                p.demand
                    .last()
                    .map_or(Some(now), |last| last.checked_add(self.keep))
            })
            .map(|at| at.saturating_duration_since(now))
            .min();
        idle.into_iter().chain(aging).min()
    }

    /// Waits until a client arrives (`listener`, when given), the listener is woken, a
    /// watch sees a change, or `timeout` passes (for ever if `None`).
    /// Waits for a client at one of the daemon's sockets, or a context's (`relays`), a wake,
    /// a change a watch sees, or `timeout`: whether the contexts the home joined may have
    /// changed.
    fn wait_for_work(
        &self,
        listener: Option<&UnixListener>,
        api: Option<&UnixListener>,
        relays: Option<&relay::Relays>,
        watches: &[Option<FileWatch>; 3],
        timeout: Option<Duration>,
    ) -> bool {
        let pollfd = |fd: RawFd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let watched = |w: &Option<FileWatch>| w.as_ref().map_or(-1, |w| w.fd().as_raw_fd());
        let mut polled: Vec<libc::pollfd> = [
            self.listener_wake.1.as_raw_fd(),
            watched(&watches[0]),
            watched(&watches[1]),
            watched(&watches[2]),
            listener.map_or(-1, AsRawFd::as_raw_fd),
            api.map_or(-1, AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .chain(relays.into_iter().flat_map(relay::Relays::fds))
        .map(pollfd)
        .collect();
        // Rounded up: a wait of 0 ms would return at once, and spin.
        let ms = timeout.map_or(-1, |t| {
            libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
        });
        let n = libc::nfds_t::try_from(polled.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: poll(2) on pollfds of descriptors this daemon holds open (negative ones
        // are ignored), `n` of them.
        unsafe { libc::poll(polled.as_mut_ptr(), n, ms) };
        let ready = |i: usize| polled.get(i).is_some_and(|p| p.revents != 0);
        if ready(0) {
            let mut drained = [0u8; 64];
            while matches!((&self.listener_wake.1).read(&mut drained), Ok(n) if n > 0) {}
        }
        for (i, watch) in watches.iter().enumerate() {
            if ready(i + 1)
                && let Some(watch) = watch
            {
                watch.clear();
            }
        }
        ready(3)
    }

    pub(super) fn take<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, conn: UnixStream) {
        // Only this user's processes: a home in a shared directory would let others'
        // reach the socket, and a run acts as this user.
        // SAFETY: geteuid(2) cannot fail.
        let ours = unsafe { libc::geteuid() };
        match shards_ipc::peer_uid(&conn) {
            Ok(uid) if uid == ours => {}
            Ok(uid) => {
                log(format!("refused a client of user {uid}"));
                return;
            }
            Err(e) => {
                log(format!("a client's credentials: {e}"));
                return;
            }
        }
        if let Err(e) = conn.set_nonblocking(false) {
            log(format!("a client's connection: {e}"));
            return;
        }
        let number = self.next_client.fetch_add(1, Ordering::Relaxed);
        // Shared rather than duplicated: a descriptor short, the daemon would drop
        // clients it could otherwise take.
        let conn = Arc::new(conn);
        lock(&self.unread).insert(number);
        lock(&self.clients).insert(number, conn.clone());
        self.busy.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("run".into())
            .spawn_scoped(threads, move || {
                // A run handed over is registered before the client stops counting as
                // busy, so shutdown never sees neither (audit A06).
                let handed = {
                    let _busy = Busy(self, number);
                    self.handle(threads, conn, number)
                };
                // The client's descriptors are closed by now: its run goes on in the VM.
                if let Some((id, inbox)) = handed {
                    // Its health checks, as dockerd's monitor runs them.
                    self.watch_health(threads, &id);
                    self.follow(threads, id, inbox);
                }
            });
        if let Err(e) = spawned {
            lock(&self.unread).remove(&number);
            lock(&self.clients).remove(&number);
            self.busy.fetch_sub(1, Ordering::SeqCst);
            log(format!("a client's thread: {e}"));
        }
    }

    /// Client `number` waits long, for a container's end or its log's growth (`wait`,
    /// `logs -f`): it counts among the clients in hand no more, so that waiters shut out
    /// no client however many there are (review 7.9). Its connection stays the daemon's
    /// to end as it stops, and its thread holds on.
    pub(super) fn waits_long(&self, number: u64) {
        let in_hand = lock(&self.clients).contains_key(&number);
        if in_hand && lock(&self.long_waits).insert(number) {
            self.busy.fetch_sub(1, Ordering::SeqCst);
            self.wake_listener();
        }
    }

    /// Client `number`'s connection is no longer the daemon's to end: a run's, or one the
    /// daemon answers as it steps aside.
    fn release_client(&self, number: u64) {
        lock(&self.clients).remove(&number);
    }

    /// Ends the clients in hand that a shutdown would otherwise wait for: shut down, their
    /// connections fail every read and write, and their threads return (audit A07).
    fn end_clients(&self) {
        self.end_engine();
        for (number, conn) in lock(&self.clients).iter() {
            // One whose request no thread has taken asks again, of the daemon that comes
            // next, as it would have had its request been the first read: shut out, it
            // would take the closed connection for its command's end.
            if lock(&self.unread).remove(number) {
                let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
            }
            shards_ipc::end(conn);
        }
        for cancel in lock(&self.preparing).values() {
            cancel.cancel();
        }
    }

    /// Runs `work` with a cancel that a shutdown sets (`end_clients`), and that the
    /// client's going sets: its connection ending while `work` runs. Nothing the client
    /// sends meanwhile is read here: it stays for what reads the connection next. Says
    /// too whether the client went.
    fn cancellable<T>(&self, client: u64, conn: &UnixStream, work: impl FnOnce(&Cancel) -> T) -> (T, bool) {
        let cancel = Cancel::new();
        lock(&self.preparing).insert(client, cancel.clone());
        if self.stopping.load(Ordering::SeqCst) {
            cancel.cancel();
        }
        // The watch ends once `done` goes: its peer then reads its end.
        let watch = (|| -> io::Result<_> {
            let (done, ended) = UnixStream::pair()?;
            let watched = conn.try_clone()?;
            let cancel = cancel.clone();
            let thread = std::thread::Builder::new().name("hangup".into()).spawn(move || {
                let gone = hung_up(&watched, &ended);
                if gone {
                    cancel.cancel();
                }
                gone
            })?;
            Ok((done, thread))
        })();
        let watch = watch
            .map_err(|e| log(format!("watching a client for its end: {e}")))
            .ok();
        let out = work(&cancel);
        lock(&self.preparing).remove(&client);
        let gone = watch.is_some_and(|(done, thread)| {
            drop(done);
            thread.join().unwrap_or(false)
        });
        (out, gone)
    }

    /// Ends the VMs still waiting, lets go of the home, and exits ([`finish`](Self::finish)).
    fn exit(&self) -> ! {
        self.finish();
        std::process::exit(0)
    }

    /// All [`exit`](Self::exit) does but end the process: ends the VMs still waiting, waits
    /// for what it last knew to be written, and lets go of the home. `shards daemon stop`
    /// learns of it from its connection closing, which is done here, after the rest: the
    /// kernel would close it on exit, but in no order this could rely on (XNU closes a
    /// process's descriptors from the highest down, kern_descrip.c fdt_invalidate).
    fn finish(&self) {
        let _ = std::fs::remove_file(self.home.join("daemon.pid"));
        let _ = std::fs::remove_file(self.home.join(shards_ipc::STOPPING));
        let state = lock(&self.state);
        let waiting = state.pools.values().flat_map(|p| p.ready.iter().map(|r| &r.vm));
        for vm in waiting.chain(state.starting.values()) {
            let _ = vm.kill(libc::SIGTERM);
        }
        // What it last knew of its containers is what the next daemon reads, and the
        // files its runs asked for are made.
        self.await_recorded();
        self.await_made();
        log("exiting");
        // A home being removed is removed whole, once nothing more is written in it: what
        // this daemon made in it as the removal went kept it before (rmdir(2): ENOTEMPTY),
        // the socket its listener made again, its records, its networks' files, its log.
        // Only the directory it started in, and only once its lock is unlinked, which none
        // but a removal of the home does: a home moved away, or one made anew at its
        // path, stays as it is.
        if self.home_gone.load(Ordering::SeqCst) && self.removed_here() {
            let _ = std::fs::remove_dir_all(&self.home);
        }
        // SAFETY: flock(2) on the lock's own descriptor.
        unsafe { libc::flock(self.home_lock.as_raw_fd(), libc::LOCK_UN) };
        lock(&self.stoppers).clear();
    }

    /// Serves one client's request. A run it hands to a warm VM, registered, comes back
    /// with its container's ID, for the caller to follow once the client's descriptors
    /// here are closed.
    fn handle<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: Arc<UnixStream>,
        number: u64,
    ) -> Option<(String, Arc<Mutex<Inbox>>)> {
        let conn = &*conn;
        let message = match shards_ipc::recv_by(conn, Instant::now() + self.request_timeout) {
            Ok(Some(m)) => m,
            Ok(None) => return None,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                log(format!("a client sent no request in {:?}", self.request_timeout));
                return None;
            }
            Err(e) => {
                log(format!("a client's request: {e}"));
                return None;
            }
        };
        // Answered already, with RESTART, as the daemon stepped aside: it asks the next.
        // `unread`'s lock is let go first: `end_clients` takes `clients`' before it.
        let taken = lock(&self.unread).remove(&number);
        if !taken {
            self.release_client(number);
            return None;
        }
        match message.kind {
            // Its connection is its run's from here, and passes to the VM.
            kind::START => self.release_client(number),
            kind::STOP => {
                self.release_client(number);
                match conn.try_clone() {
                    Ok(held) => lock(&self.stoppers).push(held),
                    Err(e) => log(format!("holding a stopper's connection: {e}")),
                }
                self.step_aside(threads, "a client asked it to stop");
                return None;
            }
            kind::ATTACH => {
                self.release_client(number);
                if let Some(attach) = shards_ipc::Attach::decode(&message.payload)
                    && attach.daemon != self.identity
                {
                    self.step_aside(threads, "an attach of another build");
                    let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
                    return None;
                }
                self.attach(&message, conn);
                return None;
            }
            kind::EXEC => {
                self.release_client(number);
                if let Some(exec) = shards_ipc::Exec::decode(&message.payload)
                    && exec.daemon != self.identity
                {
                    self.step_aside(threads, "an exec of another build");
                    let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
                    return None;
                }
                self.exec(&message, conn);
                return None;
            }
            kind::CONTAINER => {
                let Some(command) = shards_ipc::Command::decode(&message.payload) else {
                    log("a malformed container command");
                    return None;
                };
                if command.daemon != self.identity {
                    self.release_client(number);
                    self.step_aside(threads, "a command of another build");
                    let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
                    return None;
                }
                let asker = commands::Asker {
                    client: number,
                    registry_env: command.registry_env,
                    east_asian: command.east_asian,
                    now: command.now,
                    utc_offset: command.utc_offset,
                    terminal: command.terminal,
                    width: command.width,
                    color: command.color,
                    files: message.fds,
                };
                // A stopped container's files are read in a VM booted over them (visit.rs).
                let visit = self.visit_for(threads, &command.argv);
                let status = self.command(&command.argv, &asker, &commands::Reply(conn));
                if let Some(id) = visit {
                    self.end_visit(&id);
                }
                let _ = shards_ipc::send(conn, kind::END, &[status], &[]);
                return None;
            }
            other => {
                log(format!("a client sent message kind {other}"));
                return None;
            }
        }
        let Ok([stdin, stdout, stderr]) = <[OwnedFd; 3]>::try_from(message.fds) else {
            log("a run without the client's stdio");
            return None;
        };
        let Ok(err) = stderr.try_clone().map(File::from) else {
            return None;
        };
        let say = |line: &str| {
            let _ = writeln!(&err, "{line}");
        };
        // As `docker run` reports what its daemon refused (spec::not_run).
        let refuse = |said: &str| {
            let (text, status) = crate::spec::not_run(said);
            say(&text);
            let _ = shards_ipc::send(conn, kind::EXIT, &[status], &[]);
        };
        let Some(mut run) = Run::decode(&message.payload) else {
            say("shards: a malformed request");
            let _ = shards_ipc::send(conn, kind::EXIT, &[NOT_RUN], &[]);
            return None;
        };
        if run.daemon != self.identity {
            // Another build asks: its own daemon serves it once this one has gone. The
            // socket goes first, so that the client's next connection cannot reach this
            // daemon.
            self.step_aside(threads, "a run of another build");
            let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
            return None;
        }
        // `shards create` says the daemon's refusal as it is, and exits 1: runCreate
        // returns createContainer's error unwrapped (docker/cli create.go).
        let creating = run.create;
        let refuse = |said: &str| {
            if creating {
                say(&format!("shards: {said}"));
                let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
            } else {
                refuse(said);
            }
        };
        // `shards start`: the container as it was made, attached as this client asks
        // (D37). One running already is left so, and named, as `docker start` names it.
        let mut again = None;
        // Set aside as the run becomes the one the container was made by (`again_as`).
        let restarting = run.restart;
        if let Some(given) = run.again.clone() {
            if run.restart
                && let Err(e) = self.stop_for_restart(&given, run.stop_signal.as_deref(), run.stop_timeout)
            {
                say(&e);
                let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
                return None;
            }
            match self.again(&given) {
                Ok(Some((id, log, stored))) => {
                    run = again_as(stored, &run, &id);
                    again = Some((given, id, log));
                }
                // A running one is left so, and named; a snapshot restores only a stopped
                // one (D176).
                Ok(None) if run.resume.is_some() => {
                    say(&format!(
                        "shards: {given} is running: a snapshot restores a stopped microVM"
                    ));
                    let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
                    return None;
                }
                Ok(None) => {
                    let _ = shards_ipc::send(conn, kind::OUT, format!("{given}\n").as_bytes(), &[]);
                    let _ = shards_ipc::send(conn, kind::EXIT, &[0], &[]);
                    return None;
                }
                Err(e) => {
                    say(&e);
                    let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
                    return None;
                }
            }
        }
        // A container started again is refused in `docker start`'s words, the client
        // naming it after, or `docker restart`'s (moby daemon/restart.go,
        // ContainerRestart), not `docker run`'s.
        let asked_again = again.as_ref().map(|(given, _, _)| given.clone());
        let refuse = |said: &str| match &asked_again {
            Some(given) => {
                if restarting {
                    say(&format!("shards: cannot restart {given}: {said}"));
                } else {
                    say(&format!("shards: {said}"));
                }
                let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
            }
            None => refuse(said),
        };
        // One refused before it starts waits no more to be, as one that does not start
        // (`abandon`).
        let refused = |said: &str| {
            refuse(said);
            if let Some((_, id, _)) = &again {
                self.not_again(id);
            }
        };
        // `shards restore vm` (D176): the container resumed from a snapshot of its own, its
        // command going on where it was saved, in a VM restoring it. Its guest's interface
        // is as it was: its MAC, and its address on its network asked for again, as `--ip`
        // asks for one, which a restore finding it another's fails on, as a start does.
        let resuming = match (&again, run.resume.as_deref()) {
            (Some((given, id, _)), Some(asked)) => match self.resumable(id, given, asked) {
                Ok(r) => Some(r),
                Err(e) => {
                    refused(&e);
                    return None;
                }
            },
            _ => None,
        };
        if let Some(r) = &resuming {
            snapshots::resume_interface(&mut run, &r.resume);
        }
        // Its microVM's own engine (D181), where its command finds it, as the docker CLI and
        // Docker's SDKs look for one: over its image's, unless its options name another.
        if run.use_api_socket && again.is_none() && !run.env.iter().any(|e| e.starts_with("DOCKER_HOST=")) {
            run.env
                .push(format!("DOCKER_HOST=unix://{}", shards_abi::run::ENGINE_SOCKET));
        }
        // And the token its init says first on each connection its command makes to it: a
        // resumed run's, its save's, which its guest holds already (D176).
        let engine = match &resuming {
            Some(r) => r.engine,
            None if run.use_api_socket => {
                let mut token = [0u8; shards_abi::run::TOKEN];
                if let Err(e) = shards_vmm::platform::fill_random(&mut token) {
                    refused(&format!("--use-api-socket: its engine's token: {e}"));
                    return None;
                }
                Some(token)
            }
            None => None,
        };
        // dockerd's create route first, before its daemon checks anything (moby
        // handleSysctlBC): an interface's sysctl given as the container's own is refused.
        if again.is_none()
            && let Err(e) = network::interface_sysctl(&run)
        {
            refused(&e);
            return None;
        }
        // Its networks as dockerd checks them before it makes the container; what fails
        // as it starts fails once the container is made.
        // Connected to one user network from the default bridge: on it alone (D46).
        if let Some(only) = self.connected_network(&run) {
            run.network.clone_from(&only);
            run.endpoints.retain(|e| e.network == only);
        }
        let user_subnets = |term: &str| {
            self.user_network(term).map(|n| {
                let v4 = n
                    .pools
                    .iter()
                    .map(|p| (std::net::IpAddr::V4(p.subnet.0), p.subnet.1));
                let v6 = n
                    .pools6
                    .iter()
                    .filter_map(|p| crate::networks::prefix6(&p.subnet))
                    .map(|(net, bits)| (std::net::IpAddr::V6(net), bits));
                v4.chain(v6).collect()
            })
        };
        // Another container's namespaces are kept by its ID as the container is made, so
        // that they hold as that one is renamed, as dockerd keeps them (moby
        // daemon/daemon_unix.go, adaptSharedNamespaceContainer); a name no container has
        // is kept as given, and fails as it starts.
        if again.is_none() {
            let by_id = |mode: &str| {
                mode.strip_prefix("container:")
                    .filter(|name| !name.is_empty())
                    .and_then(|name| self.resolve(name).ok())
                    .map(|id| format!("container:{id}"))
            };
            if let Some(network) = by_id(&run.network) {
                for e in &mut run.endpoints {
                    if e.network == run.network {
                        e.network.clone_from(&network);
                    }
                }
                run.network = network;
            }
            if let Some(pid) = by_id(&run.pid) {
                run.pid = pid;
            }
            if let Some(ipc) = by_id(&run.ipc) {
                run.ipc = ipc;
            }
        }
        // `--pid container:NAME`: a run in a microVM of its own, whose PID namespace no
        // other microVM's process can join (D115), is joined by a run joining its microVM
        // (D119), or one of its joiners'.
        if again.is_none()
            && let Some(id) = run.pid.strip_prefix("container:")
            && let Err(e) = self.in_joined_microvm(&run, id, "--pid", "PID namespace")
        {
            refused(&e);
            return None;
        }
        // Its IPC, hostname and writable layer, as its microVM has them (D156).
        if again.is_none()
            && let Err(e) = microvm_namespaces(&run).and_then(|()| self.shared_ipc(&run))
        {
            refused(&e);
            return None;
        }
        // Its log (D161): shards' store, as its options keep it, or none.
        let kept = match crate::logging::kept(&run, self.logs) {
            Ok(kept) => kept,
            Err(e) => {
                refused(&e);
                return None;
            }
        };
        let mut start = match network::check(
            &run,
            self.bridge,
            |name| self.resolve(name).is_ok(),
            &user_subnets,
        ) {
            Ok(start) => start,
            Err(e) => {
                refused(&e);
                return None;
            }
        };
        // Its isolation, which is its microVM's (D155).
        if again.is_none()
            && let Err(e) = isolation(&run)
        {
            refused(&e);
            return None;
        }
        // Where its microVM's processes go on the host (D183).
        if again.is_none()
            && let Err(e) = parents::check(&run)
        {
            refused(&e);
            return None;
        }
        // A joiner's processes are its provider's microVM's (D119), under that microVM's
        // parent: it has none of its own to place.
        if again.is_none()
            && !run.cgroup_parent.is_empty()
            && let network::Start::Join(name) = &start
            && let Some(theirs) = self.parent_of(name)
            && theirs != run.cgroup_parent
        {
            let where_ = if theirs.is_empty() {
                "none".to_string()
            } else {
                theirs
            };
            refused(&format!(
                "--cgroup-parent {}: a run joining {name}'s network runs in its microVM, whose processes are under its parent ({where_})",
                run.cgroup_parent
            ));
            return None;
        }
        // The container's ID first: it names the command's host unless the run does
        // (moby daemon/container.go).
        let (again, id, container_log) = match again {
            Some((given, id, log)) => (Some(given), id, log),
            None => match self.new_container() {
                Ok((id, log)) => (None, id, log),
                Err(e) => {
                    refuse(&e);
                    return None;
                }
            },
        };
        // What a run that does not start leaves: a new container's directory goes, and a
        // spare is made; a container started again only waits no more to be (`again`).
        let abandon = |id: &str| {
            if again.is_some() {
                self.not_again(id);
            } else {
                self.discard(id);
                self.make_spare();
            }
        };
        // A joiner of a container on the host's network is on it too (D124), as dockerd's
        // is, in the host's network namespace.
        let joins_host = matches!(&start, network::Start::Join(name) if self.joins_hosts_network(name));
        // A joiner's names are its provider's, as dockerd gives a joiner the other's
        // (initializeNetworking): its hostname and domain name; but one its microVM's
        // engine made on that engine's network has a namespace and names of its own (D181).
        if let network::Start::Join(name) = &start
            && run.engine_net.is_empty()
            && let Some((hostname, domainname)) = self.names_of(name)
        {
            run.hostname = Some(hostname);
            run.domainname = domainname;
        }
        // On the host's network, or asked for (`--uts host`, D156): the host's hostname.
        if run.hostname.is_none()
            && (start == network::Start::Attach(network::Net::Host) || run.uts == "host")
        {
            run.hostname = info::host_name().map(|n| guest_hostname(&n));
        }
        if run.hostname.is_none() {
            run.hostname = Some(id.get(..12).unwrap_or(&id).to_string());
        }
        // The next run's spare is made once this one is answered, off its path. What it
        // downloads, a shutdown cancels, and its client's going. A client that went gets no
        // container, as a Docker CLI interrupted as it pulls gets none, and hears nothing
        // more: its stderr, which this daemon holds, may be a terminal it gave back.
        let (prepared, gone) = self.cancellable(number, conn, |cancel| {
            let heard = |line: &str| {
                if !cancel.is_cancelled() {
                    say(line);
                }
            };
            crate::run::prepare(&run, &self.home, &heard, cancel)
        });
        if gone {
            abandon(&id);
            return None;
        }
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                refuse(if self.stopping.load(Ordering::SeqCst) {
                    "the daemon is shutting down"
                } else {
                    &e
                });
                abandon(&id);
                return None;
            }
        };
        // The host's resolvers as dockerd gives a container them, on its bridge or on
        // none (the legacy transform, neither with IPv6), read as the run starts; with
        // them the command still within a frame (review 1.y), as spec.rs measured it
        // without.
        // A joiner's: an Agentfile's agents are not yet taken by a container joining
        // another's network (D119), as the guest refuses them.
        if let network::Start::Join(_) = &start
            && prepared.agentfile.is_some()
        {
            refuse("an Agentfile's image is not supported in a container joining another's network yet");
            abandon(&id);
            return None;
        }
        // A joiner runs beside the agents and harnesses of the microVM it joins, as that
        // microVM's own run does, and is held to what reaches them (D115); init refuses it
        // again as it starts the joiner.
        let beside = match &start {
            network::Start::Join(name) => self.microvm_agentfile(name),
            _ => prepared.agentfile.clone(),
        };
        if let network::Start::Join(_) = &start
            && let Some(a) = &beside
            && let Err(e) = a.refuse(&run)
        {
            refuse(&e);
            abandon(&id);
            return None;
        }
        // A joiner's files that name it are its provider's (D119): none of its own.
        if !matches!(start, network::Start::Join(_))
            && let Err(e) = self.name_guest(&run, &mut prepared.spec)
        {
            refuse(&e);
            abandon(&id);
            return None;
        }
        // Its limits, a joiner's too, held by its own cgroup in its microVM.
        prepared.spec.cgroup = crate::resources::cgroup(&run.resources, crate::resources::host_cpus());
        // A user network's endpoint (D46): its address, as the microVM starts, or why
        // the start fails, the microVM kept.
        let mut network_setup = Vec::new();
        let mut user_net = None;
        if start == network::Start::Attach(network::Net::User) {
            let term = run.network.clone();
            let endpoint = run.endpoints.iter().find(|e| e.network == term);
            let ip = endpoint.map(|e| e.ipv4.clone()).unwrap_or_default();
            let ip6 = endpoint.map(|e| e.ipv6.clone()).unwrap_or_default();
            let aliases = endpoint.map(|e| e.aliases.clone()).unwrap_or_default();
            let hostname = run.hostname.clone().unwrap_or_default();
            match self.join_network(&id, &term, &ip, &ip6, &aliases, &hostname) {
                Ok((network, member)) => {
                    let before = prepared.spec.setup.len();
                    self.network_guest(&network, &member, &run, &mut prepared.spec);
                    network_setup = prepared.spec.setup.split_off(before);
                    user_net = Some(network);
                }
                Err(e) => start = network::Start::Fails(e),
            }
        }
        if let Err(e) = crate::spec::fits(&prepared.spec) {
            refuse(&e);
            abandon(&id);
            return None;
        }
        // Its published ports, bound now so that its record lists them; a binding that
        // fails, fails the start once the container is made, as dockerd's does. On none,
        // dockerd publishes nothing, and says nothing of it.
        let (bindings, alone) = publish::wanted(&run, &prepared.exposed);
        // A port its image's Agentfile declares `AS egress`: a destination, not a listener,
        // so none is published (AGENTFILE_ARCH.md §12 answer 6).
        if let Some(a) = prepared
            .agentfile
            .as_ref()
            .filter(|a| !a.grants.egress_declared.is_empty())
        {
            let declared = shards_net::Ports::parse(&joined(&a.grants.egress_declared)).unwrap_or_default();
            let proto = |p: &str| match p {
                "udp" => shards_net::Proto::Udp,
                _ => shards_net::Proto::Tcp,
            };
            if let Some(b) = bindings.iter().find(|b| declared.has(proto(&b.proto), b.port)) {
                refuse(&format!(
                    "cannot publish port {}/{}: the image's Agentfile declares it AS egress, a port its agents reach out to, not one they listen on",
                    b.port, b.proto
                ));
                abandon(&id);
                return None;
            }
        }
        let bound = match start {
            network::Start::Attach(network::Net::Bridge | network::Net::User) => {
                if let Some(e) = publish::unsupported(&bindings) {
                    refuse(&e);
                    abandon(&id);
                    return None;
                }
                // A routed network's members are reached at their own addresses, with no
                // host port bound for what they publish (dockerd's `gateway_mode` routed);
                // a microVM's address is reached through its network process alone, so
                // its ports could only be published on the host's, which the mode keeps
                // closed.
                if let Some(n) = user_net.as_ref().filter(|n| n.routed() && !bindings.is_empty()) {
                    refuse(&format!(
                        "cannot publish ports on network {}: its gateway mode is routed, which reaches its members at their own addresses with no host port bound, and a microVM is reached through its network process alone; publish them on a network in nat mode",
                        n.name
                    ));
                    abandon(&id);
                    return None;
                }
                let default = user_net.as_ref().and_then(crate::networks::Network::host_binding);
                publish::bind(
                    &bindings,
                    &alone,
                    default,
                    publish::v6_listenable(),
                    |at, proto| self.in_use(at, proto),
                )
            }
            // A container its microVM's engine made, a member of that microVM's switch (D181):
            // its ports the microVM's, which init publishes, none of the host's.
            network::Start::Join(_) if !run.engine_nets.is_empty() => {
                publish::engine_ports(&bindings, &alone, self.engine_host_binding(&run)).map(|ports| {
                    publish::Bound {
                        listeners: Vec::new(),
                        ports,
                    }
                })
            }
            _ => Ok(publish::Bound::default()),
        };
        let (ports, published, unbound) = match bound {
            Ok(b) => {
                self.hold_ports(&id, &b.listeners);
                (b.ports, b.listeners, None)
            }
            Err(e) => (Vec::new(), Vec::new(), Some(e)),
        };
        // The run's container, before anything starts: its name must be free. Its record
        // is written while a VM is found for it.
        let made = match &again {
            // Its record stays, with the ports it has now. A restart by its policy keeps
            // its last exit code until it runs (`run_started`), as dockerd's SetRunning
            // clears it with Restarting, so that it shows `Restarting (N)` meanwhile; one
            // asked for (`start`) has none from here, so that a `wait` asked after it waits
            // for this run's, as `docker start` returns once its container runs.
            Some(_) => {
                let by_policy = lock(&self.restarting_now).contains(&id);
                let mut registry = lock(&self.containers);
                let name = registry.get(&id).map(|c| c.name.clone()).unwrap_or_default();
                match registry.change(&id, |c| {
                    c.ports = ports;
                    if !by_policy {
                        c.exit_code = None;
                    }
                }) {
                    Ok(()) => Ok(name),
                    Err(e) => Err(format!("container {id}: {e}")),
                }
            }
            None => self.create(&run, &prepared, &id, ports),
        };
        let name = match made {
            // A new container's warnings, as the CLI prints ContainerCreate's.
            Ok(name) if again.is_none() => {
                // verifyContainerSettings: before the platform's (moby daemon/container.go).
                if start == network::Start::Attach(network::Net::Host) && !run.publish.is_empty() {
                    say("WARNING: Published ports are discarded when using host network mode");
                }
                let warned = crate::resources::verify(&run.resources, crate::resources::host_cpus(), false);
                for w in warned.unwrap_or_default() {
                    say(&format!("WARNING: {w}"));
                }
                // What the run withholds of Docker's beside its domains, never silently (D115).
                for (cap, what) in prepared.agentfile.iter().flat_map(|a| a.withheld(&run)) {
                    let name = shards_abi::run::CAP_NAMES
                        .get(cap as usize)
                        .copied()
                        .unwrap_or("a capability");
                    say(&format!(
                        "WARNING: {}",
                        shards_abi::run::beside::withheld(name, what)
                    ));
                }
                name
            }
            Ok(name) => name,
            Err(e) => {
                self.free_ports(Some(&id), None);
                refuse(&e);
                abandon(&id);
                return None;
            }
        };
        // The host's network, said each time it is given (D124; the user's, 2026-10-11): a
        // joiner's too, which dockerd says nothing of.
        if start == network::Start::Attach(network::Net::Host) {
            say(&format!("WARNING: {}", shards_abi::run::NETWORK_HOST));
        } else if joins_host {
            say(&format!("WARNING: {}", shards_abi::run::NETWORK_HOST_JOINED));
        }
        // Its name is a member's name on its network (DNSNames' first).
        self.name_member(&id, &name);
        // A driver that ships its log to a cloud finds its credentials with this client's
        // environment, kept in memory alone (D168).
        self.shipping.give(&id, &run);
        // A new container keeps the request it was made by, for `shards start` (D37): but
        // its client's environments, its registries' and its cloud's, which hold
        // credentials and are no container's, and which each client that starts it gives.
        if again.is_none() {
            let dir = lock(&self.containers).dir(&id);
            let kept = Run {
                registry_env: Vec::new(),
                cloud_env: Vec::new(),
                ..run.clone()
            };
            // As it was made: a create's a create, whose Config says the streams its command
            // line gave (AttachStdout); a start says how it attaches then (`again_as`).
            if let Err(e) = std::fs::write(dir.join(REQUEST), kept.encode()) {
                log(format!(
                    "container {id}: keeping its request: {e}; it cannot be started again"
                ));
            }
        }
        // `shards create`: made, not started, its ports not yet bound (moby
        // daemon/create.go makes no network endpoint).
        if run.create {
            self.free_ports(Some(&id), None);
            lock(&self.runs).remove(&id);
            self.record_arrival(threads, &id);
            self.await_arrival(&id);
            let _ = shards_ipc::send(conn, kind::CREATED, id.as_bytes(), &[]);
            let _ = shards_ipc::send(conn, kind::OUT, format!("{id}\n").as_bytes(), &[]);
            let _ = shards_ipc::send(conn, kind::EXIT, &[0], &[]);
            self.make_spare();
            return None;
        }
        // From here, its client passes signals on to the command; it has the ID for
        // `--cidfile`.
        let _ = shards_ipc::send(conn, kind::CREATED, id.as_bytes(), &[]);
        // Its mount points, opened and served (D38): the guest mounts each as its setup
        // says. One that cannot be fails the start, as a mount runc cannot make does.
        let (mut points, first) = lock(&self.containers)
            .made(&id)
            .map(|c| (c.mounts.clone(), c.started.is_none()))
            .unwrap_or_default();
        // CDI's mounts (D146), as the run starts: one that cannot be fails the start too.
        let cdi_points = match &prepared.cdi {
            Ok(Some(e)) => crate::volumes::cdi_points(&e.mounts),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(e.clone()),
        };
        // Its own MAC where it has an interface of its own (D46, PM M173): given
        // (`--mac-address`, its endpoint's `mac-address`), which its VM takes as the run
        // comes; else made for it, as dockerd makes each endpoint's, so that no two runs of
        // one template share its MAC: its VM's own, made with the VM, which the VM's guest
        // took before the run came (D31, E1).
        let run_mac: Option<Vec<u8>> = match start {
            network::Start::Attach(network::Net::Bridge | network::Net::User) => {
                match crate::setup::own_endpoint(&run)
                    .map(|e| e.mac.trim())
                    .filter(|m| !m.is_empty())
                {
                    Some(given) => match shards_cmdline::network::mac_octets(given) {
                        Some(octets) => Some(octets),
                        None => {
                            refuse(&format!("{given} is not a valid mac address"));
                            abandon(&id);
                            return None;
                        }
                    },
                    None => None,
                }
            }
            _ => None,
        };
        // Its link-local addresses where it has an interface of its own (PM M175), as
        // libnetwork adds them: an IPv4-mapped one as IPv4, a zone gone.
        let link_local: Vec<std::net::IpAddr> = match start {
            network::Start::Attach(network::Net::Bridge | network::Net::User) => {
                crate::setup::own_endpoint(&run)
                    .map(|e| {
                        e.link_local
                            .iter()
                            .filter_map(|s| shards_cmdline::network::parse_addr(s).ok())
                            .map(|a| a.unmap().ip)
                            .collect()
                    })
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        // A joiner's shared directories (D119), each with the name it goes by in its microVM's
        // join share, given to the share as it joins (`join_vm`).
        let mut join_volumes = Vec::new();
        let joining = matches!(start, network::Start::Join(_));
        // A container its microVM's engine made, a member of that microVM's switch (D181):
        // the network it joins there, and its name in its engine's scope. Init gives it its
        // address as it starts, and says it (`MEMBER`).
        let member = match (&run.engine_made, joining) {
            (Some((_, name)), true) if !run.engine_nets.is_empty() => Some(name.clone()),
            _ => None,
        };
        let shared = cdi_points
            .and_then(|more| {
                points.extend(more);
                crate::volumes::open(&points, first, &crate::volumes::Store::new(&self.home))
            })
            .and_then(|opened| {
                let cdi = prepared.cdi.as_ref().ok().and_then(Option::as_ref);
                prepared.spec.setup = crate::setup::setup(&run, &opened.mounts, cdi)?;
                // CDI's variables, over the workload's whole (AddMultipleProcessEnv, D146).
                if let Some(e) = cdi {
                    crate::build::cdi::merge_env(&mut prepared.spec.env, &e.env);
                }
                // `--pid container:` of a joiner (D119): the PID namespace its run took.
                if let Some(other) = run.pid.strip_prefix("container:")
                    && self.joined_run(other)
                {
                    let entry = self.joiners_pid(other)?;
                    for e in prepared.spec.setup.iter_mut().filter(|e| *e == b"pid=workload") {
                        e.clone_from(&entry);
                    }
                }
                // `--ipc container:` (D156): the IPC namespace of the run it shares.
                if let Some(other) = run.ipc.strip_prefix("container:") {
                    prepared.spec.setup.push(self.shared_ipc_entry(other)?);
                }
                // `--network container:` of a container its microVM's engine gave a network
                // namespace of its own (D181): that one's, not its provider's.
                if run.engine_made.is_some()
                    && let Some(other) = run.network.strip_prefix("container:")
                    && let Some(entry) = self.joiners_net(other)?
                {
                    prepared.spec.setup.push(entry);
                }
                // Beside agents whose flows past the microVM cross the command's network
                // namespace, Docker's default CAP_NET_RAW is not the command's (D115): its
                // own microVM's, or the one it joins.
                if let Some(a) = beside.as_ref().filter(|a| a.domains > 0 && a.uplink) {
                    prepared.spec.setup.retain(|e| !e.starts_with(b"caps="));
                    prepared
                        .spec
                        .setup
                        .push(format!("caps={}", a.caps(&run)).into_bytes());
                }
                // Its MAC first, which init gives eth0 before its addresses.
                if let Some(mac) = &run_mac {
                    prepared
                        .spec
                        .setup
                        .push(format!("mac={}", shards_cmdline::network::mac_string(mac)).into_bytes());
                }
                prepared.spec.setup.extend(network_setup.iter().cloned());
                // Its link-local addresses, each with its family's prefix, /16 or /64, which
                // libnetwork gives after its addresses (setInterfaceLinkLocalIPs).
                prepared.spec.setup.extend(link_local.iter().map(|a| {
                    let bits = if a.is_ipv4() { 16 } else { 64 };
                    format!("link-local={a}/{bits}").into_bytes()
                }));
                // On the host's network (D124): the host's hosts file, and eth0's IPv6, by
                // which what it sends past its loopback is the host's.
                if start == network::Start::Attach(network::Net::Host) {
                    let mut hosts = b"hosts-file=".to_vec();
                    hosts.extend_from_slice(&host_hosts());
                    prepared.spec.setup.push(hosts);
                    let (guest6, gateway6) = self.host_ipv6();
                    prepared
                        .spec
                        .setup
                        .push(format!("address6={guest6}/64,{gateway6}").into_bytes());
                }
                // Its microVM's own engine (D181), whose socket init makes for its command.
                if let Some(token) = &engine {
                    let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
                    prepared.spec.setup.push(format!("engine={hex}").into_bytes());
                }
                // Made through a microVM's own engine: it ends as that microVM's command does,
                // and binds that command's files, which its init copies into its own (D181).
                if run.engine_made.is_some() {
                    prepared.spec.setup.push(b"engine-made".to_vec());
                }
                if !run.own_binds.is_empty() && !joining {
                    return Err("a bind of a microVM's own files is a container's that joins it".into());
                }
                // Its membership of its microVM's switch, and what it publishes to the
                // microVM's command, init's to make (D181). `--network none`: a namespace
                // of its own with its loopback alone, with nothing to publish to.
                if run.engine_made.is_some() && run.engine_net == "none" {
                    prepared.spec.setup.push(b"own-net-none".to_vec());
                }
                if let Some(name) = &member {
                    // Each network of its engine's it is on: the address its client asked
                    // for there, and its names there, the one its client gave it first.
                    for net in &run.engine_nets {
                        let mut parts = net.split('\0');
                        let network = parts.next().unwrap_or_default();
                        let addr = parts.next().unwrap_or_default();
                        let mut entry = format!("member={network}\0{addr}");
                        let mut names: Vec<&str> = Vec::new();
                        for n in std::iter::once(name.as_str()).chain(parts) {
                            if !n.is_empty() && !names.contains(&n) {
                                names.push(n);
                                entry.push('\0');
                                entry.push_str(n);
                            }
                        }
                        prepared.spec.setup.push(entry.into_bytes());
                    }
                    // The ports it declares, which its peers may reach: those it publishes,
                    // and those it exposes alone.
                    let declared: Vec<String> = bindings
                        .iter()
                        .map(|b| (b.port, b.proto.as_str()))
                        .chain(alone.iter().map(|(port, proto)| (*port, proto.as_str())))
                        .map(|(port, proto)| format!("{}/{port}", if proto == "udp" { 17 } else { 6 }))
                        .collect();
                    if !declared.is_empty() {
                        prepared
                            .spec
                            .setup
                            .push(format!("member-accepts={}", declared.join(",")).into_bytes());
                    }
                    // Each at the address its binding names, or its network's for its
                    // members' ports where it names none.
                    let default_host = self.engine_host_binding(&run).map(|a| a.to_string());
                    for b in &bindings {
                        let port = b
                            .host_port
                            .split_once('-')
                            .map_or(b.host_port.as_str(), |(lo, _)| lo);
                        let port = if port.is_empty() { "0" } else { port };
                        let host = match b.host_ip.trim_matches(|c| c == '[' || c == ']') {
                            "" => default_host.as_deref().unwrap_or_default(),
                            given => given,
                        };
                        prepared.spec.setup.push(
                            format!("own-publish={}\0{host}\0{port}\0{}", b.proto, b.port).into_bytes(),
                        );
                    }
                }
                prepared.spec.setup.extend(
                    run.own_binds
                        .iter()
                        .map(|b| [b"own-bind=".as_slice(), b.as_bytes()].concat()),
                );
                // Its endpoint's sysctls, on its interface once it has its addresses (PM M171).
                prepared.spec.setup.extend(
                    crate::setup::endpoint_sysctls(&run)
                        .into_iter()
                        .map(|kv| format!("endpoint-sysctl={kv}").into_bytes()),
                );
                // The version of the kernel it boots, which its seccomp profile is read by.
                let booted = prepared
                    .vfio_kernel
                    .as_deref()
                    .or(prepared.gpu.as_deref())
                    .unwrap_or_else(|| crate::run::kernel_of(&prepared.boot));
                let kernel = crate::guest::version_of(booted)?;
                prepared.spec.setup.extend(crate::setup::security_setup(
                    &run,
                    kernel,
                    prepared.cdi.as_ref().ok().and_then(Option::as_ref),
                    prepared.vfio.as_deref().unwrap_or_default(),
                )?);
                // A joiner of a container on the host's network: its binds the host's too,
                // by its provider's process for them (D124).
                if joins_host {
                    prepared.spec.setup.extend(crate::setup::binds(&run, kernel)?);
                }
                // An Agentfile's image: the filter its domains run under (D59). Its init
                // starts no domain without one.
                if prepared.agentfile.is_some() {
                    prepared.spec.setup.extend(crate::setup::domain_seccomp(kernel)?);
                }
                // A joiner's are volumes of its microVM's join share, each named so that no
                // other container can guess it (D119), as no run's own shares are its.
                if joining {
                    let names = opened
                        .dirs
                        .iter()
                        .map(|_| containers::new_id().map_err(|e| format!("a volume's name: {e}")))
                        .collect::<Result<Vec<_>, _>>()?;
                    crate::volumes::joined(&mut prepared.spec.setup, &names)?;
                    crate::spec::fits(&prepared.spec)?;
                    join_volumes = opened.dirs.into_iter().zip(names).collect();
                    return Ok(None);
                }
                crate::spec::fits(&prepared.spec)?;
                let link = self.start_shares(threads, &opened.dirs, &prepared.parent)?;
                prepared.shares = u32::try_from(opened.dirs.len()).map_err(|_| "too many shares")?;
                Ok(link)
            });
        let shares = match shared {
            Ok(link) => link,
            Err(e) => {
                start = network::Start::Fails(e);
                None
            }
        };
        if let Some(e) = unbound {
            start = network::Start::Fails(format!("-p: publishing {name}'s ports: {e}"));
        }
        match &again {
            // `docker start` names what it started, once it has (container/start.go).
            Some(_) => self.record_soon(&id, Vec::new()),
            None => {
                self.record_arrival(threads, &id);
                // `docker run -d` prints the ID once the container exists, before it
                // starts.
                if run.detach {
                    self.await_arrival(&id);
                    let _ = shards_ipc::send(conn, kind::OUT, format!("{id}\n").as_bytes(), &[]);
                }
            }
        }
        // Its output kept where it has a log (D161).
        let mut flags = if kept.is_some() { shards_ipc::RUN_LOG } else { 0 };
        if prepared.interactive {
            flags |= shards_ipc::RUN_INTERACTIVE;
        }
        if run.timing {
            flags |= shards_ipc::RUN_TIMING;
        }
        if !published.is_empty() {
            flags |= shards_ipc::RUN_PUBLISHED;
        }
        // A detached run's output goes only to its log; it reads nothing.
        let mut fds = if run.detach {
            flags |= shards_ipc::RUN_DETACHED;
            vec![stdin.as_fd()]
        } else {
            vec![conn.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()]
        };
        if kept.is_some() {
            fds.extend([container_log.log.as_fd(), container_log.index.as_fd()]);
        }
        // A container that stays keeps its writable layer once it stops (D37): written
        // beside its log, and kept once whole (`run_ended`); a joiner's (D119) as any.
        let layer_out = if run.remove {
            None
        } else {
            let at = lock(&self.containers).dir(&id).join(LAYER_NEW);
            match open_layer(&at) {
                Ok(file) => Some(file),
                Err(e) => {
                    log(format!(
                        "container {id}: its writable layer will not be kept: {e}"
                    ));
                    None
                }
            }
        };
        // What it changed before, to put back (D37); a resumed one's is in its memory.
        let layer_in = again
            .as_ref()
            .filter(|_| resuming.is_none())
            .and_then(|_| File::open(lock(&self.containers).dir(&id).join(LAYER)).ok());
        if let Some(file) = &layer_in {
            flags |= shards_ipc::RUN_LAYER_IN;
            fds.push(file.as_fd());
        }
        if let Some(file) = &layer_out {
            flags |= shards_ipc::RUN_LAYER_OUT;
            fds.push(file.as_fd());
        }
        // Its swap (D159): a sparse file of what its limits allow, made anew for each run,
        // which its microVM maps into its swap window; unlinked once the microVM has it,
        // so that what it swaps out is nowhere once the microVM has ended.
        let swap = match (&resuming, crate::resources::swap_bytes(&run.resources)) {
            // A resumed run's: a copy of the swap its save kept, as its guest swapped to it
            // (D176), the snapshot's own kept for its other restores.
            (Some(r), _) => {
                let at = lock(&self.containers).dir(&id).join(SWAP);
                match snapshots::copy_swap(&r.generation, &at) {
                    Ok(copied) => copied.map(|file| (file, at)),
                    Err(e) => {
                        refuse(&format!("{}: its swap: {e}", id.get(..12).unwrap_or(&id)));
                        abandon(&id);
                        return None;
                    }
                }
            }
            (None, Some(bytes)) => {
                let at = lock(&self.containers).dir(&id).join(SWAP);
                let vcpus = crate::resources::vcpus(&run.resources, crate::resources::host_cpus());
                match open_swap(&at, crate::resources::swap_device_bytes(bytes, vcpus)) {
                    Ok(file) => Some((file, at)),
                    Err(e) => {
                        refuse(&format!(
                            "{}: its swap, {}: {e}",
                            id.get(..12).unwrap_or(&id),
                            at.display()
                        ));
                        abandon(&id);
                        return None;
                    }
                }
            }
            (None, None) => None,
        };
        if let Some((file, _)) = &swap {
            flags |= shards_ipc::RUN_SWAP;
            fds.push(file.as_fd());
        }
        if let Some(link) = &shares {
            fds.push(link.as_fd());
        }
        // An image whose Agentfile grants its agents anything past the microVM: init keeps
        // the run's own processes from those grants (eth0's own subnet alone, of both
        // versions) before its command starts, as its VM's network process holds the
        // grants for the whole microVM from before the run (D59, D99).
        let grants = prepared
            .agentfile
            .as_ref()
            .map(|a| &a.grants)
            .filter(|g| !g.egress.is_empty() || !g.mcp.is_empty() || g.dns);
        if grants.is_some() {
            prepared.spec.setup.push(b"confine-eth0".to_vec());
        }
        // Its head, then the spec, in one allocation (audit D10).
        let mut payload =
            Vec::with_capacity(shards_ipc::RunHead::LEN + prepared.spec.encoded_len().unwrap_or(0));
        let retention = kept.map_or(self.logs, |k| LogRetention {
            size: k.size,
            files: k.files,
        });
        shards_ipc::RunHead {
            flags,
            log_size: retention.size,
            log_files: retention.files,
            first: container_log.seq,
            // The most memory its guest may plug, in bytes: its own (D148).
            plug: prepared
                .memory_max
                .saturating_sub(prepared.size.1)
                .saturating_mul(1 << 20),
            // Non-blocking, its queue (D162).
            queue: kept.and_then(|k| k.queue),
            // Its microVM's I/O limits, its VMM's to keep (D178).
            io_bytes: run.resources.io_maximum_bandwidth,
            io_requests: run.resources.io_maximum_iops,
        }
        .encode_into(&mut payload);
        // A resumed run's command runs already: its request brings its save's tokens instead,
        // with which its guest dials the host again (D176).
        match &resuming {
            Some(r) => {
                payload.extend_from_slice(&r.tokens);
                // Its engine's, which its guest's relay says (D181).
                if let Some(token) = &r.engine {
                    payload.extend_from_slice(token);
                }
            }
            None => prepared.spec.encode_into(&mut payload),
        }
        let detached = run.detach.then_some(conn);
        // An Agentfile's egress grants and remote MCP servers (D59), as the daemon read
        // them from the image's own normalized Agentfile, checked against its digest
        // (D109), never from its labels: its VM's network process's policy.
        let egress = match grants {
            // The host's network (D124), which an Agentfile's agents refuse, whose grants
            // these would be.
            None if start == network::Start::Attach(network::Net::Host) => {
                Some(shards_net::HOST_POLICY.to_vec())
            }
            None => None,
            Some(g) => {
                let ports = if g.egress.is_empty() {
                    Ok(shards_net::Ports::default())
                } else {
                    shards_net::Ports::parse(&joined(&g.egress))
                };
                let ports = match ports {
                    Ok(p) => p,
                    Err(e) => {
                        refuse(&format!("the image's egress grants: {e}"));
                        abandon(&id);
                        return None;
                    }
                };
                let named: Vec<(String, u16)> = g
                    .mcp
                    .iter()
                    .filter_map(|e| {
                        let (host, port) = std::str::from_utf8(e).ok()?.rsplit_once(':')?;
                        Some((host.to_string(), port.parse().ok()?))
                    })
                    .collect();
                Some(shards_net::encode_policy(&ports, &named, g.dns))
            }
        };
        let started = self.start_run(
            threads,
            &id,
            &payload,
            &fds,
            Keep {
                detached,
                options: prepared.options.clone(),
                health: prepared.health.clone().map(|h| (h, prepared.shell.clone())),
                published,
                named: again.clone().filter(|_| run.detach && !run.silent),
                restarted: restarting,
                layer_pending: layer_out.is_some(),
                log_files: kept.map_or(self.logs.files, |k| k.files),
                log_compress: kept.is_some_and(|k| k.compress),
                visit: false,
                egress,
                agentfile: prepared.agentfile.clone(),
                mac: run_mac.as_deref().and_then(|m| m.first_chunk::<6>().copied()),
                link_local: link_local.clone(),
                engine: engine.map(|token| OwnEngine {
                    token,
                    registry_env: run.registry_env.clone(),
                }),
            },
            || match &start {
                // A joiner's I/O is the microVM's it joins (D119), whose run's limits it takes
                // (D178).
                network::Start::Join(name)
                    if run.resources.io_maximum_bandwidth != 0 || run.resources.io_maximum_iops != 0 =>
                {
                    Err(format!(
                        "--network container:{name}: a joining container's I/O is that microVM's, \
                         and takes its limits: set --io-maxbandwidth and --io-maxiops on {name}"
                    ))
                }
                network::Start::Join(name) => self.join_vm(threads, &id, name, &prepared, &join_volumes),
                // A resumed run's VM restores its snapshot (D176), on the network its start
                // set up, or not, for why it failed, as `warm_for` says it.
                network::Start::Fails(why) if resuming.is_some() => {
                    Err(match resuming.as_ref().and_then(|r| r.resume.network.as_ref()) {
                        Some(saved) => format!("{why}: the snapshot's address, {}", saved.ipv4),
                        None => why.clone(),
                    })
                }
                _ => match &resuming {
                    Some(r) => self.resumed_vm(
                        threads,
                        r,
                        run_mac.as_deref().and_then(|m| m.first_chunk::<6>().copied()),
                        &prepared.parent,
                    ),
                    None => self.warm_for(threads, &prepared, &start, &say),
                },
            },
        );
        // Its VM has its root filesystem, or never will; and its swap, off the run's path.
        drop(prepared.lease.take());
        if let Some((_, at)) = &swap {
            let _ = std::fs::remove_file(at);
        }
        match started {
            Ok(inbox) => Some((id, inbox)),
            Err(said) => {
                self.free_ports(Some(&id), None);
                // A joiner's volumes go from its microVM's join share (D119).
                lock(&self.joiner_links).remove(&id);
                refuse(&said);
                None
            }
        }
    }

    /// Hands the run of container `id`, just created, to a warm VM from `acquire` (audit
    /// A06). Until a VM is committed to, the run is pending, and `rm` may cancel it; from
    /// then on commands wait to see whether the VM took it. A VM that surely did not gives
    /// way to another; one that may have is followed as it is, so no run starts twice.
    /// Returns the run, registered, for [`follow`](Self::follow), or what its client is
    /// told.
    fn start_run<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
        keep: Keep<'_>,
        mut acquire: impl FnMut() -> Result<Ready, String>,
    ) -> Result<Arc<Mutex<Inbox>>, String> {
        let mut keep = keep;
        // A visit that cannot start leaves the container as it was.
        let visit = keep.visit;
        let failed = |why: &str| {
            if visit {
                lock(&self.runs).remove(id);
                self.resolved.notify_all();
                why.to_string()
            } else {
                self.not_started(id, why)
            }
        };
        for _ in 0..HANDOFF_TRIES {
            let mut ready = acquire().map_err(|e| failed(&e))?;
            // Every way on leaves `Handing` while `ready` holds the socket it names.
            if let Err(said) = self.commit(id, ready.socket.as_raw_fd()) {
                // A joiner's VM is its provider's, in no pool: only its connection goes.
                if ready.joined.is_none() {
                    self.give_back(threads, ready);
                }
                return Err(said);
            }
            // Its VM's network process told, before the VM has the run, all it takes for
            // it, in one round trip (E1): its own MAC (D46), what its VM's network process
            // frames to and from it from here on, as init gives eth0 it; on the host's
            // network (D124), its IPv6 link and its IPv4 one the bridge's, as its template
            // booted on; its published ports, so that none of their connections wait on
            // its start, which are that process's alone once taken, closed as the run
            // ends; on a network of its own, its address, peers and names; and its agents'
            // egress grants (D59). Committed first: a VM given ports never goes back to its
            // pool. One whose process does not take them all goes, and another is tried,
            // rather than a run whose network answers nobody.
            if let Err(e) = self.tell_net(id, &ready, &keep) {
                log(format!("warm VM {}'s {e}", ready.vm.id()));
                ready.end();
                self.uncommit(id);
                continue;
            }
            if ready.net.is_some() && keep.mac.is_some() {
                ready.mac = keep.mac;
            }
            let handed = hand_over(&ready.socket, payload, fds);
            // Only now, so that starting its successor delays no run: on the refiller's
            // thread.
            self.refill_soon(threads, ready.pool.as_ref());
            match handed {
                // The warm VM serves the client from here, and ours close. A detached
                // client waits for the daemon to say whether its command started.
                // The network process has the ports: the daemon's copies go (M24).
                Ok(()) => {
                    keep.published.clear();
                    return Ok(self.register(ready, id, keep));
                }
                Err(Untaken::Surely(e)) => {
                    log(format!("warm VM {} did not take a run: {e}", ready.vm.id()));
                    ready.end();
                    self.uncommit(id);
                }
                // What it sent before it ended tells what it did, as for any run.
                Err(Untaken::Unknown(e)) => {
                    log(format!(
                        "warm VM {} may have taken a run: {e}; ending it",
                        ready.vm.id()
                    ));
                    ready.end();
                    return Ok(self.register(ready, id, keep));
                }
            }
        }
        Err(failed("no warm VM took the run"))
    }

    /// A new container's ID, and its log: the spare's, made ahead, or made now.
    fn new_container(&self) -> Result<(String, Log), String> {
        {
            let mut spare = lock(&self.spare);
            if let Spare::Made(..) = *spare
                && let Spare::Made(id, log) = std::mem::replace(&mut *spare, Spare::None)
            {
                return Ok((id, log));
            }
        }
        let id = containers::new_id().map_err(|e| format!("a container ID: {e}"))?;
        let dir = lock(&self.containers).dir(&id);
        let log = new_log(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok((id, log))
    }

    /// Holds what container `id` publishes at `listeners`' addresses.
    fn hold_ports(&self, id: &str, listeners: &[publish::Listener]) {
        if listeners.is_empty() {
            return;
        }
        let at = listeners.iter().map(|l| (l.at, l.proto)).collect();
        lock(&self.ports_held).push(publish::Held {
            container: id.to_string(),
            vm: None,
            at,
            listeners: Vec::new(),
        });
    }

    /// Lets go of what container `id`'s run, or VM `vm`'s network process, held: the
    /// daemon's copies of its listening sockets close, and their ports are free.
    fn free_ports(&self, id: Option<&str>, vm: Option<u32>) {
        let mut held = lock(&self.ports_held);
        let before = held.len();
        held.retain(|h| Some(h.container.as_str()) != id && (vm.is_none() || h.vm != vm));
        if held.len() != before {
            self.ports_freed.notify_all();
        }
    }

    /// Whose host address `at`, found in use, is: the host's; a running container's of
    /// this daemon, as dockerd's allocator knows its own; or a run's that ended, waited
    /// for until its network process has gone (its grace, `netproc::GRACE`, and as long
    /// again). A run's VM says DONE before it tells its client (warm.rs `finish`), whose
    /// next program may bind the port at once: what the holder's run has sent is taken
    /// first, as a name's holder's is (`create`), so that a run its client has seen end
    /// holds the port no longer, as dockerd's has let go of it by the time `docker run`
    /// returns.
    fn in_use(&self, at: std::net::SocketAddr, proto: u8) -> publish::InUse {
        let deadline = Instant::now() + crate::netproc::GRACE.saturating_mul(2);
        let mut held = lock(&self.ports_held);
        // A run of this daemon's held it: gone from every run, it was freed, while this
        // waited or while it took the holder's messages without the lock.
        let mut ours = false;
        let mut taken: Option<String> = None;
        loop {
            let Some(holder) = held
                .iter()
                .find(|h| h.at.iter().any(|&(a, p)| p == proto && publish::overlaps(a, at)))
                .map(|h| h.container.clone())
            else {
                return if ours {
                    publish::InUse::Freed
                } else {
                    publish::InUse::Host
                };
            };
            ours = true;
            if taken.as_ref() != Some(&holder) {
                // Without the ports' lock: a handoff, seen through first, registers its
                // run's ports under it, and the followers' loop, which frees ports, may be
                // taking the holder's messages too.
                drop(held);
                if let Some(inbox) = self.inbox_of(&holder) {
                    self.take_messages(&holder, &inbox);
                }
                held = lock(&self.ports_held);
                taken = Some(holder);
                continue;
            }
            // Running, or starting: dockerd's allocator holds a port from the container's
            // start, not from its command's.
            let running = lock(&self.containers)
                .get(&holder)
                .is_some_and(|c| c.state == Life::Running);
            if running || lock(&self.runs).contains_key(&holder) {
                return publish::InUse::Allocated;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return publish::InUse::Host;
            };
            held = self
                .ports_freed
                .wait_timeout(held, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// What a run's guest knows of names (`run`'s `--dns*`, `--add-host` and
    /// `--domainname`): its `/etc/resolv.conf`, the host's made over as dockerd makes it
    /// for its bridge (the legacy transform, without IPv6), the run's resolvers in place
    /// of the host's; `/etc/hosts`' extra lines, `host-gateway` the bridge's gateway, as
    /// dockerd's HostGatewayIPs default to its bridge's; and the domain name.
    pub(super) fn name_guest(&self, run: &Run, spec: &mut shards_abi::run::Spec) -> Result<(), String> {
        // On the host's network (D124, PM M177): the host's resolvers as they are, loopback
        // ones too, which are the host's own there; its hosts file, which `--add-host`
        // adds nothing to, as dockerd's.
        if run.network == "host" {
            spec.resolv = Some(crate::build::step::resolv_host(
                &crate::build::step::host_resolv(),
                &run.dns,
                &run.dns_search,
                &run.dns_options,
            ));
            spec.hosts = Vec::new();
            spec.domainname = run.domainname.clone().into_bytes();
            spec.setup = crate::setup::setup(run, &[], crate::run::cdi_edits(run).ok().flatten().as_ref())?;
            return Ok(());
        }
        spec.resolv = Some(crate::build::step::resolv_with(
            &crate::build::step::host_resolv(),
            false,
            &run.dns,
            &run.dns_search,
            &run.dns_options,
        ));
        spec.hosts = run
            .add_hosts
            .iter()
            .map(|h| {
                let (name, ip) = h.split_once(':').unwrap_or((h.as_str(), ""));
                let ip = if ip == "host-gateway" {
                    self.bridge
                        .as_ref()
                        .map(|b| b.gateway().to_string())
                        .ok_or("unable to derive the IP value for host-gateway")?
                } else {
                    ip.to_string()
                };
                Ok(format!("{ip}\t{name}").into_bytes())
            })
            .collect::<Result<_, &str>>()?;
        spec.domainname = run.domainname.clone().into_bytes();
        spec.setup = crate::setup::setup(run, &[], crate::run::cdi_edits(run).ok().flatten().as_ref())?;
        Ok(())
    }

    /// Container `given`, to be started again (`shards start`): its ID, its log's newest
    /// segment, and the request it was made by; none if it runs, or is starting, already.
    /// From here until its run is handed over or abandoned, it is starting.
    fn again(&self, given: &str) -> Result<Option<(String, Log, Run)>, String> {
        // What any client has seen of its last run first, as every command takes it
        // (`settle`): a `run` that has just returned its status may have sent a DONE not
        // yet taken, and the container would read as running still.
        self.settle();
        let id = self.resolve(given)?;
        if lock(&self.removing).contains(&id) {
            return Err(format!("shards: {given} is being removed: it cannot start"));
        }
        // A visit to its files (visit.rs) ends first; it starts only where no run is.
        loop {
            self.await_visit(&id);
            let mut runs = lock(&self.runs);
            if lock(&self.visiting).contains(&id) {
                continue;
            }
            if runs.contains_key(&id) {
                drop(runs);
                // A frozen one is neither started nor left so, as dockerd's is not
                // (daemon/start.go, validateState).
                if lock(&self.paused).contains(&id) {
                    return Err("shards: cannot start a paused container, try unpause instead".into());
                }
                return Ok(None);
            }
            runs.insert(id.clone(), RunState::Pending { cancelled: false });
            break;
        }
        // Its files as its last run left them, once its VM has saved them.
        self.await_settled(&id);
        let dir = lock(&self.containers).dir(&id);
        let found = std::fs::read(dir.join(REQUEST))
            .ok()
            .and_then(|b| Run::decode(&b))
            .ok_or_else(|| format!("shards: {given} was not made to be started again"))
            .and_then(|stored| {
                newest_log(&dir)
                    .map(|log| (stored, log))
                    .map_err(|e| format!("shards: {given}: its log: {e}"))
            });
        match found {
            Ok((stored, log)) => Ok(Some((id, log, stored))),
            Err(e) => {
                self.not_again(&id);
                Err(e)
            }
        }
    }

    /// Container `id`, which [`again`](Self::again) was to start, does not start: it
    /// waits no more to be, and whoever waits for its start (`await_start`) is woken to
    /// see so.
    fn not_again(&self, id: &str) {
        lock(&self.runs).remove(id);
        self.resolved.notify_all();
    }

    /// Waits until container `id`'s writable layer is whole where it is kept, if its VM
    /// is still saving it (D37).
    pub(super) fn await_settled(&self, id: &str) {
        let mut settling = lock(&self.settling);
        while settling.contains(id) {
            settling = self
                .settled
                .wait(settling)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Removes what was made for container `id`, which will not be created.
    fn discard(&self, id: &str) {
        let dir = lock(&self.containers).dir(id);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            log(format!("{}: {e}", dir.display()));
        }
    }

    /// The mount points of a container `run` makes (moby daemon/volumes.go,
    /// registerMountPoints). Anonymous volumes it made go again if a later one fails, as
    /// dockerd's cleanup removes them with the container it could not make.
    fn register_mounts(
        &self,
        run: &Run,
        prepared: &Prepared,
    ) -> Result<Vec<(crate::volumes::MountPoint, bool)>, String> {
        let store = crate::volumes::Store::new(&self.home);
        let from = |id: &str| -> Result<Vec<crate::volumes::MountPoint>, String> {
            let (registry, found) = self.resolve_held(lock(&self.containers), id);
            let found = found.map_err(|e| e.strip_prefix("shards: ").unwrap_or(&e).to_string())?;
            Ok(registry.get(&found).map(|c| c.mounts.clone()).unwrap_or_default())
        };
        let image_volumes: Vec<String> = prepared
            .image_volumes
            .iter()
            .chain(run.volumes.iter())
            .cloned()
            .collect();
        // A microVM's own engine's run: its volumes are named after that microVM (D181).
        let prefix = run
            .engine_made
            .as_ref()
            .and_then(|(of, _)| lock(&self.containers).get(of).map(|c| c.name.clone()))
            .unwrap_or_default();
        let points = crate::volumes::register(
            &store,
            run,
            &image_volumes,
            prepared.agentfile.as_ref().map(|a| a.volumes.as_slice()),
            &from,
            &prefix,
        )?;
        Ok(points)
    }

    /// The home's private directory (`tmp`, 0700), made if it is not there: where a share
    /// process keeps the files a restored guest holds that no path reaches (D176).
    fn private_dir(&self) -> Result<File, String> {
        use std::os::unix::fs::DirBuilderExt as _;
        let at = self.home.join(PRIVATE);
        match std::fs::DirBuilder::new().mode(0o700).create(&at) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", at.display()));
            }
            _ => {}
        }
        File::open(&at).map_err(|e| format!("{}: {e}", at.display()))
    }

    /// Starts the share process (share.rs) serving `dirs`: the VM's connection to it, none
    /// for none. It is reaped on the followers' loop, and ends as its VM's connections
    /// close.
    fn start_shares<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        dirs: &[(OwnedFd, bool, OsString)],
        parent: &str,
    ) -> Result<Option<UnixStream>, String> {
        if dirs.is_empty() {
            return Ok(None);
        }
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let (vm, share) = UnixStream::pair().map_err(|e| format!("the share process's connection: {e}"))?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("share")];
        for (_, read_only, only) in dirs {
            args.push(OsStr::new(if *read_only { "ro" } else { "rw" }));
            args.push(only.as_os_str());
        }
        let mut fds = vec![(share.as_fd(), 3)];
        for (i, (dir, _, _)) in dirs.iter().enumerate() {
            fds.push((dir.as_fd(), 4 + i32::try_from(i).map_err(|_| "too many shares")?));
        }
        let ghosts = self.private_dir()?;
        fds.push((
            ghosts.as_fd(),
            4 + i32::try_from(dirs.len()).map_err(|_| "too many shares")?,
        ));
        // Of its microVM's processes: under its parent (D183).
        let child = shards_ipc::spawn_in_group(self.group_of(parent)?, &exe, &args, &fds, false, &[])
            .map_err(|e| format!("starting the share process: {e}"))?;
        self.follow_share(threads, child);
        Ok(Some(vm))
    }

    /// Container `id` of `run`, with the name the run gave, or one made for it, as dockerd
    /// names containers (moby daemon/names.go). Reserved, its name held, and seen once its
    /// record is written ([`record_arrival`](Self::record_arrival)), so that it outlives a
    /// crash of the daemon (audit A15).
    fn create(
        &self,
        run: &Run,
        prepared: &Prepared,
        id: &str,
        ports: Vec<containers::PortRecord>,
    ) -> Result<String, String> {
        // Its settings, checked before its name is taken (moby daemon/create.go,
        // verifyContainerSettings).
        let stop_signal = prepared
            .stop_signal
            .as_deref()
            .map(commands::parse_signal)
            .transpose()?;
        // Its log as its options keep it, checked as its run was (D161).
        let kept = crate::logging::kept(run, self.logs).ok().flatten();
        if let Some(h) = &prepared.health {
            // dockerd's floor for each duration set (daemon/container.go, translate:
            // containertypes.MinimumDuration), and no negative retries.
            const MINIMUM_NS: i64 = 1_000_000;
            for (ns, what) in [(h.interval, "Interval"), (h.timeout, "Timeout")] {
                if ns != 0 && ns < MINIMUM_NS {
                    return Err(format!("{what} in Healthcheck cannot be less than 1ms"));
                }
            }
            if h.retries < 0 {
                return Err("Retries in Healthcheck cannot be negative".into());
            }
            for (ns, what) in [
                (h.start_period, "StartPeriod"),
                (h.start_interval, "StartInterval"),
            ] {
                if ns != 0 && ns < MINIMUM_NS {
                    return Err(format!("{what} in Healthcheck cannot be less than 1ms"));
                }
            }
        }
        // Each annotation keyed (D155).
        if let Some(given) = run
            .annotations
            .iter()
            .find(|kv| kv.split_once('=').map_or(kv.as_str(), |(k, _)| k).is_empty())
        {
            return Err(format!("--annotation {given}: an annotation needs a key"));
        }
        // Its resources (verifyPlatformContainerResources), whose warnings it says once
        // it is made.
        crate::resources::verify(&run.resources, crate::resources::host_cpus(), false)?;
        crate::setup::verify(run)?;
        validate_restart_policy(&run.restart_policy)?;
        // Then shards' own: beside its image's agents and harnesses, nothing that reaches
        // them (D115); its init refuses each again as it starts the command.
        if let Some(a) = &prepared.agentfile {
            a.refuse(run)?;
        }
        // A name held by a container that ended with `--rm`, its end not yet taken or its
        // removal not yet durable, is free once that is done, as dockerd's is by the time
        // `docker run --rm` returns: its end is taken, and its removal waited for.
        if let Some(given) = &run.name {
            let name = given.strip_prefix('/').unwrap_or(given);
            let holder = lock(&self.containers).name_taken(name).map(|c| c.id.clone());
            if let Some(holder) = holder {
                if let Some(inbox) = self.inbox_of(&holder) {
                    self.take_messages(&holder, &inbox);
                }
                self.await_released(name);
            }
        }
        // dockerd's name reservation comes first (newContainer, then the mount points).
        if let Some(given) = &run.name
            && !containers::valid_name(given)
        {
            return Err(format!(
                "Invalid container name ({given}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
            ));
        }
        // Its mount points (registerMountPoints): volumes made as they are named, binds
        // checked; an image's volumes, and `-v DEST`'s, anonymous. No volume is removed
        // from here until the container that mounts it is seen.
        let _volumes = crate::volumes::lock();
        let points = self.register_mounts(run, prepared)?;
        let unmade = |e: String| {
            let store = crate::volumes::Store::new(&self.home);
            for (p, made) in &points {
                if *made && store.get(&p.name).is_some_and(|v| v.is_anonymous()) {
                    let _ = store.remove(&p.name);
                }
            }
            e
        };
        let mut registry = lock(&self.containers);
        let named = match &run.name {
            Some(given) => {
                let name = given.strip_prefix('/').unwrap_or(given);
                match registry.name_taken(name) {
                    Some(holder) => Err(format!(
                        "--name {name}: the name is in use by microVM {}: remove or rename it to use the name",
                        holder.id.get(..12).unwrap_or(&holder.id)
                    )),
                    None => Ok(name.to_string()),
                }
            }
            None => crate::names::generate(id, |name| registry.name_taken(name).is_some())
                .map_err(|e| format!("a container name: {e}")),
        };
        let name = match named {
            Ok(name) => name,
            Err(e) => {
                drop(registry);
                return Err(unmade(e));
            }
        };
        let mounts = points.iter().map(|(p, _)| p.clone()).collect();
        registry.reserve(Container {
            id: id.to_string(),
            name: name.clone(),
            image: run.image.clone(),
            image_id: Some(prepared.image_id.clone()),
            command: prepared
                .spec
                .argv
                .iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
            created: containers::now(),
            state: Life::Created,
            started: None,
            finished: None,
            exit_code: None,
            error: String::new(),
            auto_remove: run.remove,
            log_lost: 0,
            log_dropped: 0,
            log_compress: kept.is_some_and(|k| k.compress),
            log_ships: kept.is_some_and(|k| k.ships),
            stop_signal,
            stop_timeout: run.stop_timeout,
            ports,
            labels: prepared.labels.clone(),
            annotations: annotations(run),
            oom_killed: false,
            mounts,
            restart: containers::Restart {
                policy: run.restart_policy.0.clone(),
                max: run.restart_policy.1,
                ..Default::default()
            },
            size_rw: None,
            saved: None,
            engine_images: Vec::new(),
            engine_networks: Vec::new(),
            // Made through a microVM's own engine (D181): its name there, or the one it was
            // given here where that engine named none.
            engine_made: run
                .engine_made
                .as_ref()
                .map(|(of, there)| containers::EngineMade {
                    addresses: Vec::new(),
                    of: of.clone(),
                    name: if there.is_empty() {
                        name.clone()
                    } else {
                        there.clone()
                    },
                }),
        });
        self.event_for(id, &name, &run.image, &prepared.labels, "create", &[]);
        // Its run is owned from the moment the container is visible.
        lock(&self.runs).insert(id.to_string(), RunState::Pending { cancelled: false });
        Ok(name)
    }

    /// Writes the record of reserved container `id`, again while it changes as it is
    /// written, then lets it be seen. One whose record cannot be written is seen, its
    /// record behind, which is said: it exists, and its run may have started.
    fn arrive(&self, id: &str) -> io::Result<()> {
        loop {
            let Some((recorder, c)) = lock(&self.containers).arrival(id) else {
                return Ok(());
            };
            let written = recorder.write(&self.disk, &c);
            let mut registry = lock(&self.containers);
            let seen = match &written {
                Ok(()) => registry.admit(id, &c),
                Err(_) => {
                    registry.admit_behind(id);
                    true
                }
            };
            drop(registry);
            if seen {
                self.arrived.notify_all();
                return written;
            }
        }
    }

    /// Waits until reserved container `id` is seen.
    fn await_arrival(&self, id: &str) {
        let mut registry = lock(&self.containers);
        while registry.is_arriving(id) {
            registry = self
                .arrived
                .wait(registry)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Makes a spare container, if there is none and none is being made, for the next run
    /// to take: making it costs a directory and a file that no run should wait for. One
    /// maker at a time, so no spare is made only to be dropped (audit A20).
    fn make_spare(&self) {
        {
            let mut spare = lock(&self.spare);
            if !matches!(*spare, Spare::None) {
                return;
            }
            *spare = Spare::Making;
        }
        let made = containers::new_id().and_then(|id| {
            // The lock only for the name: the directory and file are made without it,
            // which a run's record, and `ps`, would otherwise wait for.
            let dir = lock(&self.containers).dir(&id);
            let log = new_log(&dir)?;
            Ok((id, log))
        });
        *lock(&self.spare) = match made {
            Ok((id, log)) => Spare::Made(id, log),
            Err(e) => {
                log(format!("a spare container: {e}"));
                Spare::None
            }
        };
    }

    /// The run of container `id` will not start, for `why`: with `--rm` its container
    /// goes, and otherwise it stays created, with the exit code dockerd gives it (moby
    /// daemon/start.go, daemon/errors.go). A run `rm` cancelled has no container left, and
    /// its client hears what `docker run` hears of a container removed before it could
    /// start it. Those waiting for the container hear its code. Returns what the client is
    /// told.
    fn not_started(&self, id: &str, why: &str) -> String {
        let mut registry = lock(&self.containers);
        let cancelled = matches!(
            lock(&self.runs).get(id),
            Some(RunState::Pending { cancelled: true })
        );
        let mut removal = None;
        let (said, code) = if cancelled {
            (format!("No such container: {id}"), 0)
        } else {
            let (said, code) = shards_cmdline::commands::start_failed(why);
            removal = self.end_container(&mut registry, id, |c| {
                c.exit_code = Some(code);
                c.error.clone_from(&said);
            });
            (said, code)
        };
        lock(&self.runs).remove(id);
        self.resolved.notify_all();
        for waiter in lock(&self.waiters).remove(id).unwrap_or_default() {
            waiter.hear(code);
        }
        drop(registry);
        if let Some(removal) = removal
            && self.set_aside(&removal).is_ok()
        {
            let _ = self.complete(&removal);
        }
        said
    }

    /// The end of container `id`'s run, as `end` records it, kept at once: in sight, or
    /// in its reservation, whose record then holds it. A `--rm` container is taken out of
    /// sight, for [`set_aside`](Self::set_aside) and [`complete`](Self::complete) to
    /// remove; any other's record is written on the recorder's thread. Waits on no write:
    /// the followers' loop ends every run, and one waiting there holds up every other
    /// run's messages, by 81 ms at a loaded host's p90 (PM M96). Failures go to the log.
    fn end_container(
        &self,
        registry: &mut Registry,
        id: &str,
        end: impl FnOnce(&mut Container),
    ) -> Option<Removal> {
        if let Err(e) = registry.change(id, |c| {
            end(c);
            // Its addresses on its microVM's switch go with its run (D181).
            if let Some(m) = c.engine_made.as_mut() {
                m.addresses.clear();
            }
        }) {
            log(format!("container {id}: its end is lost: {e}"));
            return None;
        }
        if registry
            .made(id)
            .is_some_and(|c| c.auto_remove && c.saved.is_none())
        {
            let removal = registry.take_out(id);
            // One still arriving is so no more.
            self.arrived.notify_all();
            return removal;
        }
        self.record_soon(id, Vec::new());
        None
    }

    /// Sets aside the directory of the container `removal` took out of sight, out of the
    /// registry's lock: once it is, no crash of the daemon brings the container back. One
    /// that cannot be set aside is put back in sight as it was, its record written again,
    /// and the error is returned. Commands waiting for it to be set aside look again.
    pub(super) fn set_aside(&self, removal: &Removal) -> io::Result<()> {
        let id = &removal.container.id;
        let set = removal.set_aside(&self.disk);
        {
            let mut registry = lock(&self.containers);
            match &set {
                Ok(()) => registry.aside(id),
                Err(_) => registry.bring_back(id),
            }
        }
        if set.is_ok() {
            let c = &removal.container;
            self.shipping.forget(id);
            self.event_for(id, &c.name, &c.image, &c.labels, "destroy", &[]);
        }
        self.arrived.notify_all();
        if let Err(e) = &set {
            log(format!("container {id}: removing it: {e}"));
            self.record_soon(id, Vec::new());
        }
        set
    }

    /// Logs container event `action` (moby daemon/events.go,
    /// LogContainerEventWithAttributes): `extra`, the container's labels over them, its
    /// image and its name.
    pub(super) fn container_event(&self, id: &str, action: impl Into<String>, extra: &[(&str, String)]) {
        let (name, image, labels) = lock(&self.containers)
            .made(id)
            .map(|c| (c.name.clone(), c.image.clone(), c.labels.clone()))
            .unwrap_or_default();
        self.event_for(id, &name, &image, &labels, action, extra);
    }

    /// [`container_event`](Self::container_event), of a container named `name` made
    /// from `image`, with `labels`: what clients that follow events filter them by, as
    /// Compose follows its project's (`label=com.docker.compose.project=P`). Its own
    /// attributes first, its labels over them, then its image and name, in moby's order
    /// (copyAttributes).
    pub(super) fn event_for(
        &self,
        id: &str,
        name: &str,
        image: &str,
        labels: &std::collections::BTreeMap<String, String>,
        action: impl Into<String>,
        extra: &[(&str, String)],
    ) {
        let mut attributes: std::collections::BTreeMap<String, String> =
            extra.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
        attributes.extend(labels.iter().map(|(k, v)| (k.clone(), v.clone())));
        if !image.is_empty() {
            attributes.insert("image".into(), image.to_string());
        }
        attributes.insert("name".into(), name.trim_start_matches('/').to_string());
        self.events.log("container", action, id, attributes);
    }

    /// Makes `removal`, set aside, durable out of the registry's lock, then lets its name
    /// go and deletes what it set aside. Until it is durable the name stays held, since a
    /// power loss could bring the container back. Failures go to the log, and whether it
    /// is durable is returned.
    pub(super) fn complete(&self, removal: &Removal) -> io::Result<()> {
        let id = &removal.container.id;
        let synced = removal.sync(&self.disk);
        match &synced {
            Ok(()) => lock(&self.containers).release(id),
            Err(e) => log(format!(
                "container {id}: its removal may not outlast a crash, and its name stays held: {e}"
            )),
        }
        if let Err(e) = removal.delete(&self.disk) {
            log(format!(
                "container {id}: deleting its files: {e}; the next start deletes them"
            ));
        }
        if removal.container.auto_remove {
            self.drop_anonymous(&removal.container);
        }
        self.remove_engine_made(id);
        // Its own engine's volumes, as it goes, those nothing mounts; the rest as the last
        // container that mounts them goes after it (D181).
        self.remove_own_volumes(id);
        if let Some(made) = &removal.container.engine_made
            && lock(&self.containers).get(&made.of).is_none()
        {
            self.remove_own_volumes(&made.of);
        }
        synced
    }

    /// Removes what microVM `provider`'s own engine made (D181) as `provider` goes, as a
    /// nested engine's containers go with its files: each ended at once, and each still
    /// running as it ends, which it does with its microVM. None is left behind.
    fn remove_engine_made(&self, provider: &str) {
        let made: Vec<String> = lock(&self.containers)
            .all()
            .filter(|c| c.engine_made.as_ref().is_some_and(|m| m.of == provider))
            .map(|c| c.id.clone())
            .collect();
        for id in made {
            if self.is_restarting(&id) {
                self.exit_on_next(&id, 9);
            }
            let removal = match self.cancel_start(&id) {
                Some(removal) => Some(removal),
                // Its state as its end records it, under the same lock: one running is
                // removed as that end comes (`end_container`).
                None => {
                    let mut registry = lock(&self.containers);
                    match registry.made(&id).map(|c| c.state) {
                        Some(Life::Running) => {
                            if let Err(e) = registry.change(&id, |c| c.auto_remove = true) {
                                log(format!("container {id}: to go with its microVM {provider}: {e}"));
                            }
                            None
                        }
                        Some(_) => registry.take_out(&id),
                        None => None,
                    }
                }
            };
            match removal {
                Some(removal) => {
                    if self.set_aside(&removal).is_ok() {
                        let _ = self.complete(&removal);
                    }
                }
                None => self.record_soon(&id, Vec::new()),
            }
        }
    }

    /// Removes the anonymous volumes `removed` mounted that no other container mounts, as
    /// dockerd does with a container removed with its volumes (`rm -v`, `run --rm`; moby
    /// daemon/delete.go, removeMountPoints): a volume another holds stays, as dockerd's
    /// does, in use.
    pub(super) fn drop_anonymous(&self, removed: &Container) {
        let store = crate::volumes::Store::new(&self.home);
        let _volumes = crate::volumes::lock();
        for m in removed.mounts.iter().filter(|m| m.kind == "volume") {
            if !store.get(&m.name).is_some_and(|v| v.anonymous) {
                continue;
            }
            let held = lock(&self.containers).all().any(|c| {
                c.id != removed.id && c.mounts.iter().any(|o| o.kind == "volume" && o.name == m.name)
            });
            if held {
                continue;
            }
            if let Err(e) = store.remove(&m.name) {
                log(format!("container {}: its volume {}: {e}", removed.id, m.name));
            }
        }
    }

    /// [`set_aside`](Self::set_aside), then [`complete`](Self::complete), of a batch: one
    /// sync of their directory makes every removal set aside before it durable.
    fn complete_batch(&self, batch: &[Removal]) {
        let aside: Vec<&Removal> = batch.iter().filter(|r| self.set_aside(r).is_ok()).collect();
        let Some(first) = aside.first() else {
            return;
        };
        let synced = first.sync(&self.disk);
        for removal in aside {
            let id = &removal.container.id;
            match &synced {
                Ok(()) => lock(&self.containers).release(id),
                Err(e) => log(format!(
                    "container {id}: its removal may not outlast a crash, and its name stays held: {e}"
                )),
            }
            if let Err(e) = removal.delete(&self.disk) {
                log(format!(
                    "container {id}: deleting its files: {e}; the next start deletes them"
                ));
            }
            if removal.container.auto_remove {
                self.drop_anonymous(&removal.container);
            }
        }
    }

    /// What warm VM `ready`'s network process takes before the VM has run `id` (D31, E1),
    /// asked in one round trip; where one is not taken, which it was. Its published ports
    /// go at most a message's descriptors at a time, the daemon keeping its copies until
    /// the run has started (M24).
    fn tell_net(&self, id: &str, ready: &Ready, keep: &Keep<'_>) -> Result<(), String> {
        let Some(net) = &ready.net else {
            if !keep.published.is_empty() {
                return Err("published ports: no network process".into());
            }
            if keep.egress.is_some() {
                return Err("egress grants: no network process".into());
            }
            return Ok(());
        };
        let mut asks = networks::Asks::new(net)?;
        if let Some(mac) = keep.mac {
            asks.ask("MAC", kind::NET_MAC, &mac, &[])?;
        }
        if keep.egress.as_deref() == Some(&shards_net::HOST_POLICY[..])
            && let Some(bridge) = self.bridge
        {
            let (guest6, gateway6) = self.host_ipv6();
            let mut address = bridge.guest().octets().to_vec();
            address.push(bridge.subnet().1);
            address.extend_from_slice(&bridge.gateway().octets());
            address.extend_from_slice(&guest6.octets());
            address.push(64);
            address.extend_from_slice(&gateway6.octets());
            asks.ask("addresses", kind::NET_ADDRESS, &address, &[])?;
        }
        for batch in keep.published.chunks(shards_ipc::MAX_FDS) {
            let ports: Vec<u8> = batch
                .iter()
                .flat_map(|l| {
                    let [hi, lo] = l.guest_port.to_be_bytes();
                    [hi, lo, l.proto]
                })
                .collect();
            let fds: Vec<BorrowedFd<'_>> = batch.iter().map(|l| l.fd.as_fd()).collect();
            asks.ask("published ports", kind::PUBLISH, &ports, &fds)?;
        }
        self.give_network(id, &mut asks, keep.mac.or(ready.mac), &keep.link_local)?;
        if let Some(ports) = &keep.egress {
            asks.ask("egress grants", kind::NET_POLICY, ports, &[])?;
        }
        asks.answered()
    }

    /// Commits the run of container `id` to the warm VM in hand, unless `rm` cancelled it
    /// or the daemon is stopping: then it never starts, and the error is what its client
    /// is told. `stop_runs` sets `ending` under the same lock, so a run either commits
    /// before and is stopped once it runs, or sees it here.
    fn commit(&self, id: &str, socket: RawFd) -> Result<(), String> {
        {
            let mut runs = lock(&self.runs);
            match runs.get_mut(id) {
                Some(state) if matches!(state, RunState::Pending { cancelled: false }) => {
                    if !self.ending.load(Ordering::SeqCst) {
                        *state = RunState::Handing { socket };
                        return Ok(());
                    }
                }
                Some(RunState::Pending { cancelled: true }) => {}
                _ => log(format!(
                    "container {id}: its run was not pending when a VM came for it"
                )),
            }
        }
        Err(self.not_started(id, "the daemon is shutting down"))
    }

    /// The warm VM committed to surely did not take run `id`: the run is pending again,
    /// for another VM, and `rm` may cancel it meanwhile.
    fn uncommit(&self, id: &str) {
        if let Some(state) = lock(&self.runs).get_mut(id) {
            *state = RunState::Pending { cancelled: false };
        }
        self.resolved.notify_all();
    }

    /// A warm VM no run took goes back to its pool, unless it has ended, or was booted
    /// for the run; then it goes. The run took the spare container: another is made.
    fn give_back<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, ready: Ready) {
        // A warm VM says nothing until it has a run: one with something to read has ended.
        match ready.pool.clone() {
            Some(dir) if !readable(&ready.socket) => {
                lock(&self.state)
                    .pools
                    .entry(dir)
                    .or_default()
                    .ready
                    .push_front(ready);
                self.changed.notify_all();
            }
            _ => {
                let _ = ready.vm.kill(libc::SIGKILL);
            }
        }
        self.refill_soon(threads, None);
    }

    /// Waits while the run of container `id` is being started: until it runs, or never
    /// will. What `stop` and `kill` then find is what a client that saw `run` return would
    /// find.
    pub(super) fn await_start(&self, id: &str) {
        let mut runs = lock(&self.runs);
        while matches!(
            runs.get(id),
            Some(RunState::Pending { cancelled: false } | RunState::Handing { .. })
        ) {
            runs = self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// For `rm`: takes container `id` out of sight with its run if no VM has been
    /// committed to the run, which then never starts; those waiting for the container hear
    /// 0, the code of one that never ran. The removal, for [`set_aside`](Self::set_aside)
    /// and [`complete`](Self::complete), if it did. A run being handed over is seen through
    /// first, so that `rm` acts on whether it started.
    pub(super) fn cancel_start(&self, id: &str) -> Option<Removal> {
        loop {
            let mut registry = lock(&self.containers);
            let mut runs = lock(&self.runs);
            let handing = match runs.get_mut(id) {
                Some(RunState::Pending { cancelled }) => {
                    *cancelled = true;
                    false
                }
                Some(RunState::Handing { .. }) => true,
                Some(RunState::Tracked(_)) | None => return None,
            };
            if handing {
                drop(registry);
                drop(self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner));
                continue;
            }
            drop(runs);
            for waiter in lock(&self.waiters).remove(id).unwrap_or_default() {
                waiter.hear(0);
            }
            return registry.take_out(id);
        }
    }

    /// Registers the run of container `id`, just handed to `ready`'s VM: from here it
    /// runs, as commands see it. A run handed over while the daemon stops is stopped too:
    /// its registration wakes the stop thread (`stop_each`), which finds it. Returns the run's inbox, for [`follow`](Self::follow).
    fn register(&self, ready: Ready, id: &str, keep: Keep<'_>) -> Arc<Mutex<Inbox>> {
        let Keep {
            detached,
            options,
            health,
            published,
            named,
            restarted,
            layer_pending,
            log_files,
            log_compress,
            visit,
            egress,
            agentfile,
            mac: _,
            link_local: _,
            engine,
        } = keep;
        if layer_pending {
            lock(&self.settling).insert(id.to_string());
        }
        // Its ports are held until its VM's network process has gone, which frees them:
        // with the daemon's copies, if that process did not say it had them.
        if let Some(h) = lock(&self.ports_held).iter_mut().find(|h| h.container == id) {
            h.vm = Some(ready.vm.id());
            h.listeners = published.into_iter().map(|l| l.fd).collect();
        }
        // Runs last as long as their commands.
        let _ = ready.socket.set_read_timeout(None);
        let socket = Arc::new(RunSocket::new(ready.socket));
        // On the host's network (D124): its VM's network process, which takes the host's
        // sockets of its guest's binds as they listen.
        let host_net = if egress.as_deref() == Some(&shards_net::HOST_POLICY[..]) {
            ready
                .net
                .as_ref()
                .and_then(|net| {
                    net.try_clone()
                        .map_err(|e| log(format!("container {id}: its network process: {e}")))
                        .ok()
                })
                .map(|net| hostnet::HostNet::new(net, PUBLISH_PATIENCE))
        } else {
            None
        };
        let detached = detached.and_then(|conn| {
            conn.try_clone()
                .map_err(|e| log(format!("holding a detached client's connection: {e}")))
                .ok()
        });
        // A joiner's provider (D119), which follows its microVM on for it.
        let provider = ready.joined.as_deref().and_then(|p| self.inbox_of(p));
        if let Some(p) = &provider {
            lock(p).joiners += 1;
        }
        let inbox = Arc::new(Mutex::new(Inbox {
            socket: socket.clone(),
            pid: ready.vm.id(),
            started: false,
            detached,
            ended: false,
            records: ready.records,
            working_set: WorkingSet::default(),
            container: lock(&self.containers).dir(id),
            segment: 0,
            log_files,
            log_compress,
            execs_in_flight: Vec::new(),
            exec_ids: Vec::new(),
            layer_pending,
            named,
            restarted,
            incoming: shards_ipc::Incoming::default(),
            visit,
            guest: None,
            host_net,
            joiners: 0,
            provider,
            saving: None,
            saved_as: None,
            engine,
        }));
        let tracked = Tracked {
            base: Arc::new(Base {
                options,
                health,
                agentfile,
            }),
            socket,
            vm: ready.vm,
            inbox: inbox.clone(),
            visit,
            mac: ready.mac,
            net: ready.net,
            joined: ready.joined,
        };
        // Its VM is told how its run stops, so that it stops it so if this daemon dies.
        if !tracked.visit && tracked.joined.is_none() {
            let (signal, grace) = self.own_stop(id);
            let grace = grace.map_or(u64::MAX, |g| u64::try_from(g.as_millis()).unwrap_or(u64::MAX - 1));
            let mut order = signal.unwrap_or(0).to_be_bytes().to_vec();
            order.extend_from_slice(&grace.to_be_bytes());
            let _ = tracked.socket.send(kind::STOP_ORDER, &order, &[]);
        }
        // As the daemon stops, the stop thread, which this wakes, stops it.
        lock(&self.runs).insert(id.to_string(), RunState::Tracked(tracked));
        self.resolved.notify_all();
        inbox
    }

    /// The inbox of run `id` while it is followed; none where it is not. A run being
    /// handed over is seen through first: its VM may have run its command, said DONE, and
    /// told its client before the handoff registered the run, and the client's next
    /// command may already be here. The caller holds none of the locks a handoff takes.
    fn inbox_of(&self, id: &str) -> Option<Arc<Mutex<Inbox>>> {
        let mut runs = lock(&self.runs);
        loop {
            match runs.get(id) {
                Some(RunState::Tracked(t)) => return Some(t.inbox.clone()),
                Some(RunState::Handing { .. }) => {
                    runs = self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
                }
                _ => return None,
            }
        }
    }

    /// Takes the messages run `id` has sent and nobody has taken yet; whether it has
    /// ended.
    fn take_messages(&self, id: &str, inbox: &Mutex<Inbox>) -> bool {
        let mut guard = lock(inbox);
        // Followed past its end while its writable layer is still to come (D37), and while
        // containers joining its network run in its microVM (D119).
        while !guard.ended || guard.layer_pending || guard.joiners > 0 {
            let inbox = &mut *guard;
            let m = match inbox.incoming.take(&inbox.socket.stream) {
                Ok(shards_ipc::Took::Message(m)) => m,
                // What has come of the next is kept for the rest.
                Ok(shards_ipc::Took::Partial) => break,
                Ok(shards_ipc::Took::Ended) | Err(_) => {
                    // A save it never answered is not written (D176).
                    if let Some(saving) = inbox.saving.take() {
                        let _ = saving.told.send(Err("its microVM stopped".into()));
                    }
                    if !inbox.ended {
                        self.run_ended(id, inbox, None);
                    }
                    // Its VM has gone, and its joiners with it.
                    inbox.joiners = 0;
                    inbox.host_net = None;
                    if inbox.layer_pending {
                        self.settle_layer(id, inbox, false);
                    }
                    continue;
                }
            };
            match m.kind {
                kind::STARTED => {
                    inbox.guest = <[u8; 4]>::try_from(m.payload.as_slice())
                        .ok()
                        .map(u32::from_be_bytes);
                    self.run_started(id, inbox);
                }
                kind::DONE => self.run_ended(id, inbox, Some(&m.payload)),
                kind::LOST => self.log_lost(id, &m.payload),
                kind::OOM => self.oom_killed(id),
                kind::PROCESS => self.keep_processes(id, &m.payload),
                // Its guest's ask on the host's network (D124), answered at once: a bind
                // and a listen are each a system call's work, and what goes to the
                // network process is not waited for.
                kind::HOST_NET => {
                    let answer = inbox.host_net.as_mut().and_then(|h| h.ask(&m.payload));
                    if let Some(answer) = answer
                        && let Err(e) = inbox.socket.send(kind::HOST_BOUND, &answer, &[])
                    {
                        log(format!(
                            "container {id}: answering its bind on the host's network: {e}"
                        ));
                    }
                }
                // A container in its microVM ending (D124): its ports of the host's let go,
                // the network process's copies too, before its end is told.
                kind::HOST_RELEASE => {
                    if let (Some(h), Ok(owner)) =
                        (inbox.host_net.as_mut(), <[u8; 4]>::try_from(m.payload.as_slice()))
                    {
                        h.release(u32::from_be_bytes(owner));
                        h.settle();
                    }
                    if let Err(e) = inbox.socket.send(kind::HOST_RELEASE, &m.payload, &[]) {
                        log(format!(
                            "container {id}: answering its ports' release on the host's network: {e}"
                        ));
                    }
                }
                kind::WORKING_SET => {
                    if let Some(whole) = working_set_part(id, inbox, &m.payload) {
                        self.make_soon(whole);
                    }
                }
                kind::LOG_SEGMENT => self.ask_segment(id, inbox, &m.payload),
                // A container its microVM's engine made, a member of that microVM's switch
                // (D181): its address there as init gave it, kept while it runs.
                kind::MEMBER => match engine_address_of(&m.payload) {
                    Some(address) => {
                        let changed = lock(&self.containers).change(id, |c| {
                            if let Some(made) = c.engine_made.as_mut() {
                                made.addresses.retain(|a| a.network != address.network);
                                made.addresses.push(address.clone());
                            }
                        });
                        if changed.is_ok() {
                            self.record_soon(id, Vec::new());
                        }
                    }
                    None => log(format!(
                        "container {id}: a membership its guest said malformed: {:?}",
                        String::from_utf8_lossy(&m.payload)
                    )),
                },
                // The ports init gave what a container its microVM's engine made publishes
                // (D181), each binding's in its order.
                kind::PORTS => {
                    let (given, _) = m.payload.as_chunks::<2>();
                    let changed = lock(&self.containers).change(id, |c| {
                        for (p, port) in c.ports.iter_mut().filter(|p| p.ip.is_some()).zip(given) {
                            p.public = u16::from_be_bytes(*port);
                        }
                    });
                    if changed.is_ok() {
                        self.record_soon(id, Vec::new());
                    }
                }
                // A connection its command made to its own microVM's engine (D181): its VM's
                // copy goes once the daemon has it (M24), and the listener serves it, scoped
                // to this microVM. One from a run with no engine of its own is closed.
                kind::ENGINE_CONN => {
                    if let Err(e) = inbox.socket.send(kind::ENGINE_CONN, &m.payload, &[]) {
                        log(format!("container {id}: answering its engine connection: {e}"));
                    }
                    match (inbox.engine.is_some(), m.fds.into_iter().next()) {
                        (true, Some(fd)) => {
                            self.engine.arrive(id, UnixStream::from(fd));
                            self.wake_listener();
                        }
                        _ => log(format!(
                            "container {id}: an engine connection from a run with no engine of its own"
                        )),
                    }
                }
                // Its VM's answer to a save (D176), before anything after it: a microVM the
                // save stops ends as saved.
                kind::SAVED => snapshots::answered(inbox, &m.payload),
                kind::EXEC_TAKEN => {
                    if let Ok(n) = <[u8; 8]>::try_from(m.payload.as_slice()).map(u64::from_be_bytes) {
                        inbox.execs_in_flight.retain(|(held, _)| *held != n);
                    }
                }
                kind::LAYER_SAVED => {
                    self.settle_layer(id, inbox, true);
                    if let Ok(used) = <[u8; 8]>::try_from(m.payload.as_slice()) {
                        let used = u64::from_be_bytes(used);
                        if lock(&self.containers)
                            .change(id, |c| c.size_rw = Some(used))
                            .is_ok()
                        {
                            self.record_soon(id, Vec::new());
                        }
                    }
                }
                kind::EXEC_ENDED => {
                    if let Some((n, &[status])) = m.payload.split_first_chunk::<8>()
                        && let Some(at) = inbox
                            .exec_ids
                            .iter()
                            .position(|(e, _)| *e == u64::from_be_bytes(*n))
                    {
                        let (_, exec_id) = inbox.exec_ids.swap_remove(at);
                        // moby daemon/monitor.go, ProcessEvent's exit of an exec.
                        self.container_event(
                            id,
                            "exec_die",
                            &[("execID", exec_id), ("exitCode", status.to_string())],
                        );
                    }
                }
                _ => {}
            }
        }
        guard.ended && !guard.layer_pending && guard.joiners == 0
    }

    /// Container `id`'s writable layer, as its VM left it (D37): kept if it came `whole`;
    /// else what came of it goes, and the layer it had before stays.
    fn settle_layer(&self, id: &str, inbox: &mut Inbox, whole: bool) {
        inbox.layer_pending = false;
        let (new, kept) = (inbox.container.join(LAYER_NEW), inbox.container.join(LAYER));
        let settled = if whole {
            std::fs::rename(&new, &kept)
        } else {
            std::fs::remove_file(&new)
        };
        if let Err(e) = settled
            && e.kind() != io::ErrorKind::NotFound
        {
            log(format!("container {id}: its writable layer: {e}"));
        }
        lock(&self.settling).remove(id);
        self.settled.notify_all();
    }

    /// Has the log segment run `id`'s VM asks for made, the one after the last asked for,
    /// in its container, on the files' thread, which answers with its files, or with none
    /// where it cannot be made: the VM then keeps no more output (workload.rs, `Logger`).
    /// One asked for out of turn is answered with none here.
    fn ask_segment(&self, id: &str, inbox: &mut Inbox, payload: &[u8]) {
        let asked = <[u8; 8]>::try_from(payload).ok().map(u64::from_be_bytes);
        match asked {
            Some(seq) if Some(seq) == inbox.segment.checked_add(1) => {
                inbox.segment = seq;
                self.make_soon(files::Job::Segment {
                    id: id.to_string(),
                    dir: inbox.container.clone(),
                    seq,
                    files: inbox.log_files,
                    pack: inbox.log_compress,
                    socket: inbox.socket.clone(),
                });
            }
            _ => {
                log(format!(
                    "container {id}: a VM asked for a log segment out of turn"
                ));
                let seq = asked.unwrap_or_default();
                if let Err(e) = inbox.socket.send(kind::SEGMENT, &seq.to_be_bytes(), &[]) {
                    log(format!("container {id}: answering for its log: {e}"));
                }
            }
        }
    }

    /// Takes what every run has sent, so that what a command answers includes all that
    /// any client has seen of them.
    pub(super) fn settle(&self) {
        // A container is seen once its record is written, on the recorder's thread, while
        // its run goes on: a short run's client can have its status first. The answer
        // waits for every record being written, each a local write away, and for every
        // removal a crash would undo to be set aside.
        let mut registry = lock(&self.containers);
        // Records behind are written again, on the recorder's thread.
        let behind: Vec<String> = registry.behind().cloned().collect();
        while registry.any_arriving() || registry.any_setting_aside() {
            registry = self
                .arrived
                .wait(registry)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(registry);
        for id in behind {
            self.record_soon(&id, Vec::new());
        }
        // A run being handed over whose VM has said TAKEN may be running already, and its
        // client have seen it: what it sent is taken once it is registered, which its
        // handoff, reading TAKEN, does at once. One whose VM has not said it has started
        // nothing, and no command waits on a VM that has yet to answer.
        let mut states = lock(&self.runs);
        while states
            .values()
            .any(|r| matches!(r, RunState::Handing { socket } if readable_fd(*socket)))
        {
            states = self.resolved.wait(states).unwrap_or_else(PoisonError::into_inner);
        }
        let runs: Vec<(String, Arc<Mutex<Inbox>>)> = states
            .iter()
            .filter_map(|(id, r)| match r {
                RunState::Tracked(t) => Some((id.clone(), t.inbox.clone())),
                _ => None,
            })
            .collect();
        // Messages are taken without it: a run's end takes it again.
        drop(states);
        for (id, inbox) in runs {
            self.take_messages(&id, &inbox);
        }
        // Answered once what it answers with is recorded, or behind (audit A15).
        self.await_recorded();
    }

    /// Output of run `id` its log could not keep: counted on its container, for `logs` to
    /// say (audit A12).
    /// Run `id`'s guest killed a process of it for want of memory: as dockerd hears
    /// containerd's TaskOOM (daemon/monitor.go), State.OOMKilled until it runs again, and
    /// an `oom` event.
    fn oom_killed(&self, id: &str) {
        let named = lock(&self.containers)
            .made(id)
            .map(|c| (c.name.clone(), c.image.clone(), c.labels.clone()));
        match lock(&self.containers).change(id, |c| c.oom_killed = true) {
            Ok(()) => self.record_soon(id, Vec::new()),
            Err(e) => log(format!("container {id}: its OOM is not recorded: {e}")),
        }
        if let Some((name, image, labels)) = named {
            self.event_for(id, &name, &image, &labels, "oom", &[]);
        }
    }

    /// What run `id`'s log lost, then what it dropped, non-blocking (D162), as its VM
    /// counted them: `LOST`'s two counts, the second none from a VM that counts no drops.
    fn log_lost(&self, id: &str, payload: &[u8]) {
        let Some((lost, rest)) = payload.split_first_chunk::<8>() else {
            return;
        };
        let lost = u64::from_be_bytes(*lost);
        let dropped = rest.first_chunk::<8>().map_or(0, |b| u64::from_be_bytes(*b));
        if lost > 0 {
            log(format!(
                "container {id}: {lost} bytes of its output could not be kept in its log"
            ));
        }
        let changed = lock(&self.containers).change(id, |c| {
            c.log_lost = c.log_lost.saturating_add(lost);
            c.log_dropped = c.log_dropped.saturating_add(dropped);
        });
        match changed {
            Ok(()) => self.record_soon(id, Vec::new()),
            Err(e) => log(format!("container {id}: what its log lost is lost: {e}")),
        }
    }

    /// Run `id` started. Its record is written on the recorder's thread, outside the
    /// registry's lock; a detached run's client hears of the start once it is written, or
    /// why it is behind, as dockerd records a start before `docker run -d` hears of it
    /// (moby 0fed273 daemon/start.go, `containerStart`: `CheckpointTo` after
    /// `SetRunning`).
    fn run_started(&self, id: &str, inbox: &mut Inbox) {
        inbox.started = true;
        if inbox.visit {
            return;
        }
        let by_policy = self.restarted_by_policy(id);
        let changed = lock(&self.containers).change(id, |c| {
            c.state = Life::Running;
            c.started = Some(containers::now());
            // Running again, as a snapshot resumes it or not: no save stopped it (D176).
            c.saved = None;
            c.exit_code = None;
            c.oom_killed = false;
            c.error.clear();
            c.restart.restarting = false;
            if !by_policy {
                c.restart.count = 0;
                c.restart.manually_stopped = false;
            }
        });
        let told: Vec<UnixStream> = inbox.detached.take().into_iter().collect();
        if changed.is_ok() {
            self.container_event(id, "start", &[]);
            if inbox.restarted {
                self.container_event(id, "restart", &[]);
            }
        }
        if let Some(name) = inbox.named.take() {
            for client in &told {
                let _ = shards_ipc::send(client, kind::OUT, format!("{name}\n").as_bytes(), &[]);
            }
        }
        match changed {
            Ok(()) => self.record_soon(id, told),
            Err(e) => {
                log(format!("container {id}: its start is lost: {e}"));
                record::tell(told, id, &Err(e));
            }
        }
    }

    /// Run `id` ended: `done` is its DONE, or `None` for a VM that ended without one.
    fn run_ended(&self, id: &str, inbox: &mut Inbox, done: Option<&[u8]>) {
        self.leave_network(id);
        // A joiner's volumes go from its microVM's join share as its link closes; and a
        // microVM's join share ends with its VM, the daemon's connection to it let go (D119).
        lock(&self.joiner_links).remove(id);
        lock(&self.join_shares).remove(id);
        // On the host's network (D124): the host's ports of its guest's binds let go, but
        // its joiners', which go as each of them ends (D119).
        if inbox.joiners == 0 {
            inbox.host_net = None;
        } else if let Some(h) = inbox.host_net.as_mut() {
            h.release(0);
        }
        // A joiner's: its provider's microVM followed for it no more, and its ports there
        // let go.
        if let Some(provider) = inbox.provider.take() {
            let mut p = lock(&provider);
            p.joiners = p.joiners.saturating_sub(1);
            if let (Some(h), Some(guest)) = (p.host_net.as_mut(), inbox.guest) {
                h.release(guest);
            }
        }
        inbox.ended = true;
        // A visit's end is its own: the container stays as it was.
        if inbox.visit {
            lock(&self.runs).remove(id);
            self.resolved.notify_all();
            *lock(&self.last) = Instant::now();
            self.wake_listener();
            return;
        }
        let (pid, started) = (inbox.pid, inbox.started);
        // A microVM its save stopped (D176): its command did not end, and nothing of it
        // failed. It is not started again, by its policy or otherwise, nor removed with its
        // snapshots: `restore vm` resumes it.
        let saved = inbox.saved_as.take();
        // DONE carries the status, and for a command that never started what dockerd
        // would say and the code its container keeps (warm.rs finish).
        let (status, said) = match done.and_then(<[u8]>::split_first) {
            _ if saved.is_some() => (shards_ipc::SAVED_STATUS, None),
            Some((&status, said)) => (
                status,
                (!started).then(|| String::from_utf8_lossy(said).into_owned()),
            ),
            // A VM that ended without a word leaves its command's status unknown: 255, as
            // dockerd reports a container whose process it lost.
            None if started => {
                log(format!("VM {pid} ended before its command did"));
                (255, None)
            }
            None => {
                log(format!("VM {pid} ended before its command started"));
                let (said, code) = shards_cmdline::commands::start_failed(
                    "the container's microVM stopped before its command started",
                );
                (code, Some(said))
            }
        };
        // A run may end before its container's record is written: its end waits in its
        // reservation, for the record (`end_container`).
        // What `die` says of it (moby daemon/monitor.go): its status, and how long it ran
        // in whole seconds; a command that never started does not die.
        let (ran, name, image, labels) = lock(&self.containers)
            .made(id)
            .map(|c| {
                let ran = c.started.map(|s| containers::now().saturating_sub(s));
                (ran, c.name.clone(), c.image.clone(), c.labels.clone())
            })
            .unwrap_or_default();
        // Its policy's say (handleContainerExit): started again after a wait, or not; a
        // command that never started is never restarted.
        let restart_after = if started && saved.is_none() {
            self.should_restart(id, status, ran)
        } else {
            None
        };
        let removal = {
            let mut registry = lock(&self.containers);
            let removal = self.end_container(&mut registry, id, |c| {
                c.exit_code = Some(status);
                c.saved = saved;
                if let Some(why) = &said {
                    c.error.clone_from(why);
                }
                if started {
                    c.state = Life::Exited;
                    c.finished = Some(containers::now());
                }
                if restart_after.is_some() {
                    c.restart.restarting = true;
                    c.restart.count = c.restart.count.saturating_add(1);
                }
            });
            lock(&self.runs).remove(id);
            lock(&self.health).remove(id);
            lock(&self.paused).remove(id);
            // Logged before anyone waiting hears of the end, as dockerd logs it in the
            // exit's handling (monitor.go), ahead of `stop`'s own event.
            if started {
                let seconds = ran.map_or(0, |ns| ns / 1_000_000_000);
                self.event_for(
                    id,
                    &name,
                    &image,
                    &labels,
                    "die",
                    &[
                        ("exitCode", status.to_string()),
                        ("execDuration", seconds.to_string()),
                    ],
                );
            }
            self.resolved.notify_all();
            // One restarting runs on, to `wait` (State.SetRestarting).
            if restart_after.is_none() {
                for waiter in lock(&self.waiters).remove(id).unwrap_or_default() {
                    waiter.hear(status);
                }
            }
            removal
        };
        if let Some(removal) = removal {
            self.complete_soon(removal);
        }
        if let Some(wait) = restart_after {
            self.schedule_restart(id, wait);
        }
        // A detached command that never started: why, as `docker run -d` says it, or as
        // `docker start` does, without run's help (container/start.go).
        if let Some(client) = inbox.detached.take() {
            let said = said.as_deref().unwrap_or_default();
            let (text, exits) = if inbox.named.is_some() {
                (format!("shards: {said}"), 1)
            } else {
                crate::spec::not_run(said)
            };
            let _ = shards_ipc::send(&client, kind::ERR, format!("{text}\n").as_bytes(), &[]);
            let _ = shards_ipc::send(&client, kind::END, &[exits], &[]);
        }
        *lock(&self.last) = Instant::now();
        // The last run's end starts the idle clock.
        self.wake_listener();
    }

    /// Waits up to `limit` (for ever if `None`, or if it is too long to count) for the
    /// container with `id`, found in `registry`, held since, to stop running, and returns
    /// its exit code: 0 if it never ran. `None` if it still runs, or if `client`, the
    /// connection of the command that waits, has hung up. A waiter that stops waiting is
    /// forgotten (audit A07).
    pub(super) fn await_exit_held(
        &self,
        registry: MutexGuard<'_, Registry>,
        id: &str,
        limit: Option<Duration>,
        client: Option<&UnixStream>,
    ) -> Option<u8> {
        match self.register_waiter(registry, id, false)? {
            Err(code) => Some(code),
            Ok(waiter) => self.await_waiter(id, waiter, limit, client),
        }
    }

    /// A waiter for the end of the container with `id`, found in `registry`, held since,
    /// registered under its lock, under which ends are told, so that none goes unheard:
    /// its number and the socket its code comes on; or its code at once (`Err`) where it
    /// does not run, unless `next` asks for its next run's, though it has yet to start (the
    /// Engine API's `next-exit`, D170). `None` where no waiter could be made.
    pub(super) fn register_waiter(
        &self,
        registry: MutexGuard<'_, Registry>,
        id: &str,
        next: bool,
    ) -> Option<Result<(u64, UnixStream), u8>> {
        // A run `rm` cancelled will never run.
        if !next
            && matches!(
                lock(&self.runs).get(id),
                None | Some(RunState::Pending { cancelled: true })
            )
        {
            return Some(Err(registry.get(id).and_then(|c| c.exit_code).unwrap_or(0)));
        }
        let (told, wake) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                log(format!("container {id}: waiting for its end: {e}"));
                return None;
            }
        };
        let number = self.next_waiter.fetch_add(1, Ordering::Relaxed);
        lock(&self.waiters)
            .entry(id.to_string())
            .or_default()
            .push(Waiter { number, wake });
        Some(Ok((number, told)))
    }

    /// Waits up to `limit` (for ever if `None`) for the end the waiter `register_waiter`
    /// made for container `id` hears: its code, or `None` once `client` hangs up first.
    /// A waiter that stops waiting is forgotten (audit A07).
    pub(super) fn await_waiter(
        &self,
        id: &str,
        (number, told): (u64, UnixStream),
        limit: Option<Duration>,
        client: Option<&UnixStream>,
    ) -> Option<u8> {
        let deadline = limit.and_then(|l| Instant::now().checked_add(l));
        // Its code, its client's end (a command's client sends nothing after its request:
        // anything to read is its end), or the limit: nothing else wakes it.
        loop {
            let ms = deadline.map_or(-1, |d| {
                let left = d.saturating_duration_since(Instant::now());
                libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            });
            if ms == 0 {
                break;
            }
            let mut polled = [
                libc::pollfd {
                    fd: told.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: client.map_or(-1, |c| c.as_raw_fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll(2) on two pollfds of descriptors this function holds open (a
            // negative one is ignored).
            if unsafe { libc::poll(polled.as_mut_ptr(), 2, ms) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                break;
            }
            let [code, hung_up] = polled;
            if code.revents != 0 {
                let mut byte = [0u8; 1];
                if matches!((&told).read(&mut byte), Ok(1)) {
                    return Some(byte[0]);
                }
            }
            if hung_up.revents != 0 {
                break;
            }
        }
        self.forget_waiter(id, number);
        // Its code may have come as it gave up.
        let _ = told.set_nonblocking(true);
        let mut byte = [0u8; 1];
        matches!((&told).read(&mut byte), Ok(1)).then_some(byte[0])
    }

    /// For `logs -f`: a socket readable once run `id` ends, and its waiter's number, to
    /// forget it by; none if it does not run. Registered under the records' lock, under
    /// which a run's end is told, so no end goes unheard.
    pub(super) fn wake_at_end(&self, id: &str) -> io::Result<Option<(u64, UnixStream)>> {
        let _registry = lock(&self.containers);
        if !matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_))) {
            return Ok(None);
        }
        let (ours, theirs) = UnixStream::pair()?;
        let number = self.next_waiter.fetch_add(1, Ordering::Relaxed);
        lock(&self.waiters)
            .entry(id.to_string())
            .or_default()
            .push(Waiter { number, wake: theirs });
        Ok(Some((number, ours)))
    }

    /// Forgets waiter `number` of container `id`, which stops waiting (audit A07).
    pub(super) fn forget_waiter(&self, id: &str, number: u64) {
        let mut waiters = lock(&self.waiters);
        if let Some(list) = waiters.get_mut(id) {
            list.retain(|w| w.number != number);
            if list.is_empty() {
                waiters.remove(id);
            }
        }
    }

    /// Stops serving: removes the socket, ends the runs in progress, and exits once they
    /// have ended, as dockerd shuts down (`shards daemon stop`, or a client of another
    /// build, whose own daemon then takes the home).
    /// Stops serving, for `why`, which its log says: a daemon gone with no word of why
    /// left a test only its absence to go by (a_container_outlives_a_daemon_that_dies, dev
    /// 3c2e3cd3's CI).
    fn step_aside<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, why: &str) {
        log(format!("stepping aside: {why}"));
        // Before its socket goes: whoever then finds no daemon listening waits for this
        // one while it ends its runs, however long their stop timeouts.
        let stopping = self.home.join(shards_ipc::STOPPING);
        if let Err(e) = std::fs::write(&stopping, format!("{}\n", std::process::id())) {
            log(format!("{}: {e}", stopping.display()));
        }
        self.close();
        self.stopping.store(true, Ordering::SeqCst);
        for (_, booting) in lock(&self.booting).drain() {
            let _ = booting.send(Err("the daemon is shutting down".into()));
        }
        self.stop_runs(threads);
        self.end_clients();
        // Runs waiting for a warm VM look at `stopping` again; the lock orders the wakeup
        // after their last look.
        drop(lock(&self.state));
        self.changed.notify_all();
    }

    /// Ends the runs in progress as dockerd ends its containers when it shuts down (moby
    /// daemon/daemon.go Shutdown and shutdownContainer, daemon/stop.go containerStop): each
    /// command gets its container's stop signal, SIGKILL once its stop timeout is up,
    /// unless that is negative, and its VM goes too if the command outlives even that by
    /// SHUTDOWN_KILL. A run handed to a VM as the daemon stops is stopped as it registers.
    fn stop_runs<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        {
            // Under the runs' lock: a run still starting is either one the stop thread
            // sees, or sees `ending` when it commits (audit A06).
            let _runs = lock(&self.runs);
            if self.ending.swap(true, Ordering::SeqCst) {
                return;
            }
        }
        let stopping = std::thread::Builder::new()
            .name("stop".into())
            .spawn_scoped(threads, move || self.stop_each());
        if let Err(e) = stopping {
            log(format!("the stop thread: {e}"));
            self.stop_each();
        }
    }

    /// The stop thread's work ([`stop_runs`](Self::stop_runs)): each run, once seen, gets
    /// its stop signal, then the rest as their times come, until no run is left that may
    /// still start.
    fn stop_each(&self) {
        let mut stops: HashMap<String, Stop> = HashMap::new();
        loop {
            // What is new, and what is due, seen under the runs' lock; done once it is let
            // go of, as no send waits under it.
            let (new, due) = {
                let mut runs = lock(&self.runs);
                loop {
                    stops.retain(|id, _| matches!(runs.get(id), Some(RunState::Tracked(_))));
                    let new: Vec<_> = runs
                        .iter()
                        .filter_map(|(id, run)| match run {
                            RunState::Tracked(t) if !stops.contains_key(id) => Some((
                                id.clone(),
                                t.socket.clone(),
                                t.joined.is_none().then(|| t.vm.clone()),
                            )),
                            _ => None,
                        })
                        .collect();
                    let now = Instant::now();
                    let due: Vec<String> = stops
                        .iter()
                        .filter(|(_, stop)| stop.next().is_some_and(|at| at <= now))
                        .map(|(id, _)| id.clone())
                        .collect();
                    if !new.is_empty() || !due.is_empty() {
                        break (new, due);
                    }
                    if runs.values().all(|run| matches!(run, RunState::Pending { .. })) {
                        return;
                    }
                    runs = match stops.values().filter_map(Stop::next).min() {
                        Some(at) => {
                            self.resolved
                                .wait_timeout(runs, at.saturating_duration_since(now))
                                .unwrap_or_else(PoisonError::into_inner)
                                .0
                        }
                        None => self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner),
                    };
                }
            };
            for (id, socket, vm) in new {
                let (signal, grace) = self.own_stop(&id);
                let now = Instant::now();
                let kill = match signal {
                    Some(signal) => {
                        let _ = socket.send(kind::SIGNAL, &signal.to_be_bytes(), &[]);
                        grace.and_then(|grace| now.checked_add(grace))
                    }
                    // A signal Linux has no number for cannot be sent: SIGKILL at once.
                    None => Some(now),
                };
                stops.insert(
                    id,
                    Stop {
                        socket,
                        vm,
                        kill,
                        killed: false,
                    },
                );
            }
            for id in due {
                let Some(stop) = stops.get_mut(&id) else {
                    continue;
                };
                if stop.killed {
                    if let Some(vm) = &stop.vm {
                        let _ = vm.kill(libc::SIGKILL);
                    }
                    stop.kill = None;
                } else {
                    let _ = stop.socket.send(kind::SIGNAL, &9u32.to_be_bytes(), &[]);
                    stop.killed = true;
                }
            }
        }
    }

    /// The PID namespace of joiner `id` (D119), as another's `--pid container:` takes it,
    /// dockerd's `/proc/PID/ns/pid` of its process (moby daemon/oci_linux.go): the one its
    /// own run took, its own (by its exec's id in its provider's guest), its provider's
    /// (that of another joiner, followed), or the microVM's. As a setup entry.
    fn joiners_pid(&self, id: &str) -> Result<Vec<u8>, String> {
        let mut at = id.to_string();
        // Each names one made before it, so no chain loops; bounded all the same.
        for _ in 0..64 {
            let dir = lock(&self.containers).dir(&at);
            let run = std::fs::read(dir.join(REQUEST))
                .ok()
                .and_then(|b| Run::decode(&b))
                .ok_or_else(|| format!("container {at}: its request is lost"))?;
            if run.pid == "host" {
                return Ok(b"pid=host".to_vec());
            }
            match run.pid.strip_prefix("container:") {
                Some(other) if self.joined_run(other) => at = other.to_string(),
                Some(_) => return Ok(b"pid=workload".to_vec()),
                None => {
                    let guest = self.inbox_of(&at).and_then(|inbox| lock(&inbox).guest);
                    return guest
                        .map(|g| format!("pid=joiner={g}").into_bytes())
                        .ok_or_else(|| format!("container {at} is not running"));
                }
            }
        }
        Err(format!("container {id}: its PID namespace is too far removed"))
    }

    /// The network namespace of container `id` as another's `--network container:` takes it
    /// (D181): its own, where its microVM's engine gave it one (a member of the switch, or
    /// its loopback alone), as `net=joiner=ID` of its exec in its provider's guest; that of
    /// the one it shares, followed; none where it is its provider's, the command's.
    fn joiners_net(&self, id: &str) -> Result<Option<Vec<u8>>, String> {
        let mut at = id.to_string();
        // Each names one made before it, so no chain loops; bounded all the same.
        for _ in 0..64 {
            let dir = lock(&self.containers).dir(&at);
            let run = std::fs::read(dir.join(REQUEST))
                .ok()
                .and_then(|b| Run::decode(&b))
                .ok_or_else(|| format!("container {at}: its request is lost"))?;
            if run.engine_made.is_none() {
                return Ok(None);
            }
            if run.engine_net == "none" || !run.engine_nets.is_empty() {
                let guest = self.inbox_of(&at).and_then(|inbox| lock(&inbox).guest);
                return guest
                    .map(|g| Some(format!("net=joiner={g}").into_bytes()))
                    .ok_or_else(|| format!("container {at} is not running"));
            }
            match run.network.strip_prefix("container:") {
                Some(other) => at = other.to_string(),
                None => return Ok(None),
            }
        }
        Err(format!(
            "container {id}: its network namespace is too far removed"
        ))
    }

    /// `--ipc container:NAME` (D156): a run shares another's IPC namespace and /dev/shm
    /// inside that run's microVM, which it joins with its network; the other shares them
    /// only where it said so (`--ipc shareable`), or where they are the microVM's (`--ipc
    /// host`). One that shares a third's is followed to it.
    fn shared_ipc(&self, run: &Run) -> Result<(), String> {
        let Some(id) = run.ipc.strip_prefix("container:") else {
            return Ok(());
        };
        let name = |id: &str| {
            lock(&self.containers)
                .get(id)
                .map_or_else(|| id.to_string(), |c| c.name.trim_start_matches('/').to_string())
        };
        if self.resolve(id).is_err() {
            return Err(format!("--ipc container:{id}: no run is named {id}"));
        }
        let given = name(id);
        self.in_joined_microvm(run, id, "--ipc", "IPC")?;
        let mut at = id.to_string();
        // Each names one made before it, so no chain loops; bounded all the same.
        for _ in 0..64 {
            let dir = lock(&self.containers).dir(&at);
            let other = std::fs::read(dir.join(REQUEST))
                .ok()
                .and_then(|b| Run::decode(&b))
                .ok_or_else(|| format!("run {}: its request is lost", name(&at)))?;
            match other.ipc.as_str() {
                "shareable" | "host" => return Ok(()),
                ipc => match ipc.strip_prefix("container:") {
                    Some(next) => at = next.to_string(),
                    None => {
                        return Err(format!(
                            "--ipc container:{given}: {} keeps its IPC its own (--ipc {}); run it with --ipc shareable to share it",
                            name(&at),
                            if ipc.is_empty() { "private" } else { ipc }
                        ));
                    }
                },
            }
        }
        Err(format!(
            "--ipc container:{given}: its IPC namespace is too far removed"
        ))
    }

    /// The Agentfile of the microVM the run `name` names runs in, its own run's image's
    /// (D109): what a run joining it runs beside (D115).
    fn microvm_agentfile(&self, name: &str) -> Option<crate::agentfile::Agentfile> {
        let id = self.resolve(name).ok()?;
        let runs = lock(&self.runs);
        let owner = match runs.get(&id) {
            Some(RunState::Tracked(t)) => t.joined.clone().unwrap_or(id),
            _ => id,
        };
        match runs.get(&owner) {
            Some(RunState::Tracked(t)) => t.base.agentfile.clone(),
            _ => None,
        }
    }

    /// Whether the run `id` names is in the microVM `run` joins (D119): the one whose
    /// network it joins, or another joiner of that microVM. What `flag` (`--pid`, `--ipc`)
    /// shares of it, `what`, is shared there alone, each microVM's its own.
    fn in_joined_microvm(&self, run: &Run, id: &str, flag: &str, what: &str) -> Result<(), String> {
        let name = |id: &str| {
            lock(&self.containers)
                .get(id)
                .map_or_else(|| id.to_string(), |c| c.name.trim_start_matches('/').to_string())
        };
        if self.resolve(id).is_err() {
            return Err(format!("{flag} container:{id}: no run is named {id}"));
        }
        let given = name(id);
        let joined = match run.network.strip_prefix("container:") {
            Some(provider) => Some(self.microvm_of(provider)?),
            None => None,
        };
        if joined.as_deref() != Some(self.microvm_of(id)?.as_str()) {
            return Err(format!(
                "{flag} container:{given}: a run shares another's {what} inside that run's microVM, which it joins with --network container:{given}"
            ));
        }
        Ok(())
    }

    /// The run whose microVM run `id` runs in: itself, or, for a joiner (D119), the run
    /// whose network it joins, followed.
    fn microvm_of(&self, id: &str) -> Result<String, String> {
        let mut at = self.resolve(id)?;
        // Each names one made before it, so no chain loops; bounded all the same.
        for _ in 0..64 {
            let dir = lock(&self.containers).dir(&at);
            let run = std::fs::read(dir.join(REQUEST))
                .ok()
                .and_then(|b| Run::decode(&b))
                .ok_or_else(|| format!("container {at}: its request is lost"))?;
            match run.network.strip_prefix("container:") {
                Some(provider) => at = self.resolve(provider)?,
                None => return Ok(at),
            }
        }
        Err(format!("container {id}: its microVM is too far removed"))
    }

    /// Whose IPC namespace a run sharing `id`'s takes (D156), as a setup entry: the
    /// microVM's, the workload's, or a joiner's by its exec's id in its provider's guest.
    fn shared_ipc_entry(&self, id: &str) -> Result<Vec<u8>, String> {
        let mut at = id.to_string();
        for _ in 0..64 {
            let dir = lock(&self.containers).dir(&at);
            let run = std::fs::read(dir.join(REQUEST))
                .ok()
                .and_then(|b| Run::decode(&b))
                .ok_or_else(|| format!("container {at}: its request is lost"))?;
            if run.ipc == "host" {
                return Ok(b"ipc=host".to_vec());
            }
            if let Some(other) = run.ipc.strip_prefix("container:") {
                at = other.to_string();
                continue;
            }
            if !self.joined_run(&at) {
                return Ok(b"ipc=workload".to_vec());
            }
            let guest = self.inbox_of(&at).and_then(|inbox| lock(&inbox).guest);
            return guest
                .map(|g| format!("ipc=joiner={g}").into_bytes())
                .ok_or_else(|| format!("container {at} is not running"));
        }
        Err(format!("container {id}: its IPC namespace is too far removed"))
    }

    /// The hostname and domain name of the container `name` names, as its run gave them
    /// (its request), for a container joining its network (D119).
    fn names_of(&self, name: &str) -> Option<(String, String)> {
        let id = self.resolve(name).ok()?;
        let dir = lock(&self.containers).dir(&id);
        let run = Run::decode(&std::fs::read(dir.join(REQUEST)).ok()?)?;
        let hostname = run
            .hostname
            .unwrap_or_else(|| id.get(..12).unwrap_or(&id).to_string());
        Some((hostname, run.domainname))
    }

    /// Whether the container `name` names, whose network a joiner joins (D119), is on the
    /// host's network (D124): its microVM's run is, its provider's where it is a joiner
    /// itself, as [`Self::join_vm`] finds that microVM.
    fn joins_hosts_network(&self, name: &str) -> bool {
        let Ok(found) = self.resolve(name) else {
            return false;
        };
        let provider = match lock(&self.runs).get(&found) {
            Some(RunState::Tracked(t)) if !t.visit => t.joined.clone().unwrap_or_else(|| found.clone()),
            _ => found,
        };
        self.inbox_of(&provider)
            .is_some_and(|inbox| lock(&inbox).host_net.is_some())
    }

    /// The microVM container `id` joins (D119): that of the container `name` names, found
    /// as dockerd finds it as a joiner starts (moby daemon/container_operations.go
    /// getNetworkedContainer), in its words where it cannot be joined; a joiner's, its
    /// provider's. The VM is given the joiner's own connection, whose other end is the
    /// joiner's run's socket, and its image `prepared.rootfs`, which the VM gives a range
    /// of its join disk (`kind::JOIN`).
    /// Its shared directories, `volumes`, are given to that microVM's join share first,
    /// once however many times the joiner is handed over (`give_join_volumes`).
    fn join_vm<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        name: &str,
        prepared: &Prepared,
        volumes: &[((OwnedFd, bool, OsString), String)],
    ) -> Result<Ready, String> {
        let not_found = || format!("--network container:{name}: No such container: {name}");
        let found = self.resolve(name).map_err(|_| not_found())?;
        if found == id {
            return Err(format!(
                "--network container:{name}: a run cannot join its own microVM"
            ));
        }
        let (shown, restarting) = lock(&self.containers)
            .get(&found)
            .map(|c| (c.name.trim_start_matches('/').to_string(), c.restart.restarting))
            .ok_or_else(not_found)?;
        // A joiner's network is its provider's, in its provider's microVM.
        let provider = match lock(&self.runs).get(&found) {
            Some(RunState::Tracked(t)) if !t.visit => t.joined.clone().unwrap_or_else(|| found.clone()),
            _ => found.clone(),
        };
        let held = match lock(&self.runs).get(&provider) {
            Some(RunState::Tracked(t)) if !t.visit && t.joined.is_none() => {
                Some((t.socket.clone(), t.vm.clone(), t.inbox.clone()))
            }
            _ => None,
        };
        let Some((socket, vm, inbox)) = held.filter(|(_, _, inbox)| !lock(inbox).ended) else {
            let state = if restarting {
                "restarting"
            } else if lock(&self.containers)
                .get(&found)
                .is_some_and(|c| c.state == Life::Created)
            {
                "created"
            } else {
                "exited"
            };
            return Err(format!(
                "--network container:{shown}: {shown} is {state}, and a run joins a running one's microVM"
            ));
        };
        if lock(&self.paused).contains(&provider) || lock(&self.paused).contains(&found) {
            return Err(format!(
                "--network container:{shown}: {shown} is paused: its microVM is stopped until it is unpaused"
            ));
        }
        let given = lock(&self.joiner_links)
            .get(id)
            .is_some_and(|(to, _)| *to == provider);
        if !volumes.is_empty() && !given {
            self.give_join_volumes(threads, id, &provider, &socket, &inbox, volumes)?;
        }
        let image =
            File::open(&prepared.rootfs).map_err(|e| format!("{}: {e}", prepared.rootfs.display()))?;
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a joiner's connection: {e}"))?;
        // Held until the VM says it has it (`EXEC_TAKEN`), as an exec's client is (M24).
        let number = self.next_exec.fetch_add(1, Ordering::Relaxed);
        let held = theirs
            .try_clone()
            .map_err(|e| format!("a joiner's connection: {e}"))?;
        lock(&inbox).execs_in_flight.push((number, held));
        if let Err(e) = socket.send(
            kind::JOIN,
            &number.to_be_bytes(),
            &[theirs.as_fd(), image.as_fd()],
        ) {
            lock(&inbox).execs_in_flight.retain(|(n, _)| *n != number);
            return Err(format!("the microVM of container {shown}: {e}"));
        }
        Ok(Ready {
            vm,
            socket: ours,
            net: None,
            mac: None,
            pool: None,
            records: None,
            joined: Some(provider),
        })
    }

    /// Gives joiner `id`'s `volumes`, each a directory and the name it goes by, to the join
    /// share of `provider`'s microVM (D119), whose VM `socket` reaches: served until the
    /// joiner's link to the share closes, as it ends. The share's process is started with
    /// the first joiner to bring any, and its VM given the connection to it then, before
    /// that joiner's `JOIN`, so before its guest mounts the share.
    fn give_join_volumes<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        provider: &str,
        socket: &RunSocket,
        inbox: &Mutex<Inbox>,
        volumes: &[((OwnedFd, bool, OsString), String)],
    ) -> Result<(), String> {
        use std::os::unix::ffi::OsStrExt as _;
        let failed = |e: io::Error| format!("its microVM's join share: {e}");
        let (link, theirs) = UnixStream::pair().map_err(failed)?;
        {
            let mut shares = lock(&self.join_shares);
            if !shares.contains_key(provider) {
                // Of the provider's microVM's processes: under its parent (D183).
                let parent = self.parent_of(provider).unwrap_or_default();
                let door = self.start_join_share(threads, socket, inbox, &parent)?;
                shares.insert(provider.to_string(), door);
            }
            let sent = match shares.get(provider) {
                Some(door) => shards_ipc::send(door, kind::JOIN_LINK, &[], &[theirs.as_fd()]),
                None => Err(io::Error::other("it has none")),
            };
            if let Err(e) = sent {
                // Its process gone: the next joiner's starts another.
                shares.remove(provider);
                return Err(failed(e));
            }
        }
        link.set_read_timeout(Some(TAKE_TIMEOUT)).map_err(failed)?;
        link.set_write_timeout(Some(TAKE_TIMEOUT)).map_err(failed)?;
        for ((dir, read_only, only), name) in volumes {
            let mut payload = name.as_bytes().to_vec();
            payload.push(0);
            payload.extend_from_slice(if *read_only { b"ro" } else { b"rw" });
            payload.push(0);
            payload.extend_from_slice(only.as_bytes());
            shards_ipc::send(&link, kind::JOIN_VOLUME, &payload, &[dir.as_fd()]).map_err(failed)?;
            let answer = shards_ipc::recv(&link)
                .map_err(failed)?
                .ok_or("its microVM's join share ended")?;
            match answer.kind {
                kind::TAKEN => {}
                kind::ERR => {
                    return Err(format!(
                        "its microVM's join share: {}",
                        String::from_utf8_lossy(&answer.payload)
                    ));
                }
                other => return Err(format!("its microVM's join share said message kind {other}")),
            }
        }
        // Answered on it, the share has its end: ours may go (M24).
        drop(theirs);
        lock(&self.joiner_links).insert(id.to_string(), (provider.to_string(), link));
        Ok(())
    }

    /// Starts a microVM's join share process (D119), its root an empty directory of the
    /// home's, and gives the VM `socket` reaches its connection to it, held until the VM
    /// says it has it (M24). Returns the daemon's connection to the process, on which each
    /// joiner's link to it goes.
    fn start_join_share<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        socket: &RunSocket,
        inbox: &Mutex<Inbox>,
        parent: &str,
    ) -> Result<UnixStream, String> {
        use std::os::unix::fs::DirBuilderExt as _;
        let failed = |e: io::Error| format!("its microVM's join share: {e}");
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let (vms, served) = UnixStream::pair().map_err(failed)?;
        let (door, theirs) = UnixStream::pair().map_err(failed)?;
        let at = self.home.join(JOIN_ROOT);
        match std::fs::DirBuilder::new().mode(0o700).create(&at) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(failed(e)),
            _ => {}
        }
        let root = File::open(&at).map_err(failed)?;
        let ghosts = self.private_dir()?;
        let child = shards_ipc::spawn_in_group(
            self.group_of(parent)?,
            &exe,
            &[OsStr::new("share"), OsStr::new("--join")],
            &[
                (served.as_fd(), 3),
                (theirs.as_fd(), 4),
                (root.as_fd(), 5),
                (ghosts.as_fd(), 6),
            ],
            false,
            &[],
        )
        .map_err(|e| format!("starting its microVM's join share: {e}"))?;
        self.follow_share(threads, child);
        let number = self.next_exec.fetch_add(1, Ordering::Relaxed);
        let held = vms.try_clone().map_err(failed)?;
        lock(inbox).execs_in_flight.push((number, held));
        if let Err(e) = socket.send(kind::JOIN_SHARE, &number.to_be_bytes(), &[vms.as_fd()]) {
            lock(inbox).execs_in_flight.retain(|(n, _)| *n != number);
            // The process ends as the other end of its connection goes, with ours.
            return Err(failed(e));
        }
        door.set_write_timeout(Some(TAKE_TIMEOUT)).map_err(failed)?;
        Ok(door)
    }

    /// A warm VM for `prepared`: from its template's pool, or booted for it, saving the
    /// template on the way if there is none.
    fn warm_for<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        prepared: &Prepared,
        start: &network::Start,
        say: &dyn Fn(&str),
    ) -> Result<Ready, String> {
        // Docker's default bridge: the guest on a network of its own (D31), named on its
        // kernel command line, so that its templates are apart from those of guests on
        // none.
        let net = match start {
            network::Start::Attach(net) => *net,
            network::Start::Fails(why) => return Err(why.clone()),
            // A joiner's VM is its provider's (`join_vm`).
            network::Start::Join(_) => {
                return Err("a container joining another's network has no microVM of its own".into());
            }
        };
        let bridge = match net {
            // A user network's guest boots on the bridge's template, and takes its own
            // address as it starts (D46).
            network::Net::Bridge | network::Net::User | network::Net::Host => {
                Some(self.bridge.ok_or(shards_net::bridge::NO_SUBNET)?)
            }
            network::Net::None => None,
        };
        // An Agentfile's image: the in-VM server's device after its root filesystem
        // (D60), so that init starts each agent's instance from it.
        let server = if prepared.agentfile.is_some() {
            Some(crate::guest::server_device(&self.home)?)
        } else {
            None
        };
        // Sized as a run without a limit, with the host's memory to plug, which each run
        // asks for as its workload needs it within its own most (D148): one template for
        // every limit (run::template). Plugged memory comes online as the kernel's own, as
        // a builder's does (D133).
        let on_network = |cfg: &mut Config| {
            (cfg.vcpus, cfg.memory_mib) = prepared.size;
            cfg.hotplug_mib = crate::build::host_memory()
                .map_or(0, |bytes| ((bytes >> 20) & !1).saturating_sub(cfg.memory_mib));
            if cfg.hotplug_mib > 0 {
                cfg.cmdline.push_str(" memhp_default_state=online_kernel");
            }
            // And memory its workload frees given back to the host, as it reports it
            // free, as a container's is (D151).
            cfg.reporting = true;
            // And a swap window as long as the host's memory (D159), which a run with a
            // memory limit swaps to: as much as the most memory a run may have, the swap
            // that limit gives by default.
            cfg.swap_mib = crate::build::host_memory().map_or(0, |bytes| (bytes >> 20) & !1);
            cfg.pmem.extend(server.iter().cloned());
            // Another CPU's image (D179): the emulator image's root after them, and its init
            // told the device and the architecture to register, before its root is mounted.
            if let Some(e) = &prepared.emulator {
                cfg.pmem.push(e.root.clone());
                cfg.cmdline.push_str(&format!(
                    " shards_emulate=/dev/pmem{}:{}",
                    cfg.pmem.len(),
                    e.architecture
                ));
            }
            // A slot for each directory it shares (D38), which its run fills.
            cfg.shares = (0..prepared.shares).map(|_| Arc::default()).collect();
            if let Some(bridge) = &bridge {
                cfg.cmdline.push(' ');
                cfg.cmdline.push_str(&bridge.cmdline());
            }
        };
        // A run given a host's VFIO groups (D154): a microVM booted for it, their functions
        // behind its own IOMMU, with a kernel with VFIO; of no template, as a host's device
        // is no snapshot's. Where the microVM's MSIs are not isolated (vm::ISOLATED_MSIS:
        // x86_64 has no interrupt remapping yet), its guest's VFIO and iommufd are told to
        // take them anyway (vfio_iommu_type1.c; iommufd/device.c, iommufd_device_bind), as
        // they reach the microVM's own interrupt controller alone.
        let vfio = prepared.vfio.as_ref().map_err(String::clone)?;
        if let Some(kernel) = &prepared.vfio_kernel {
            let mut cfg = match &prepared.boot {
                Boot::Given(cfg) => (**cfg).clone(),
                Boot::Stored(guest) => Config::new(kernel.clone(), Some(guest.init.clone())),
            };
            on_network(&mut cfg);
            cfg.vfio = crate::run::vfio_functions(vfio);
            if !shards_vmm::vm::ISOLATED_MSIS {
                cfg.cmdline.push_str(
                    " vfio_iommu_type1.allow_unsafe_interrupts=1 iommufd.allow_unsafe_interrupts=1",
                );
            }
            // And its init told to keep a sysfs of its own (`shards_vfio`), through which
            // it gives the functions to vfio-pci.
            cfg.cmdline.push_str(" shards_vfio=1");
            return self.cold(threads, &cfg, &prepared.rootfs, None, &prepared.parent);
        }
        let guest = match &prepared.boot {
            Boot::Given(cfg) => {
                let mut cfg = (**cfg).clone();
                on_network(&mut cfg);
                cfg.gpu = prepared.gpu.is_some();
                return self.cold(threads, &cfg, &prepared.rootfs, None, &prepared.parent);
            }
            Boot::Stored(guest) => guest,
        };
        // A run given this Mac's GPU boots the pinned GPU kernel in the GPU's VM process
        // (D144), and restores a template of its own, saved before its guest made anything
        // of the GPU's, which its host driver would hold (D150).
        let gpu_guest = prepared.gpu.as_ref().map(|kernel| crate::guest::Guest {
            kernel: kernel.clone(),
            init: guest.init.clone(),
            kernel_digest: crate::kernel::KERNEL_GPU
                .map(|k| format!("sha256:{}", k.sha256))
                .unwrap_or_default(),
            init_digest: guest.init_digest.clone(),
        });
        let guest = gpu_guest.as_ref().unwrap_or(guest);
        let mut cfg = Config::new(guest.kernel.clone(), Some(guest.init.clone()));
        on_network(&mut cfg);
        cfg.gpu = prepared.gpu.is_some();
        if !shards_vmm::vm::SNAPSHOTS {
            return self.cold(threads, &cfg, &prepared.rootfs, None, &prepared.parent);
        }
        let dir = crate::run::template(&self.home, guest, &prepared.rootfs, &cfg);
        // Its pool: the template's warm VMs under its parent (D183).
        let key = PoolKey::new(&dir, &prepared.parent);
        if shards_vmm::snapshot::exists(&dir) {
            match self.claim(threads, &key, &prepared.rootfs) {
                Ok(ready) => return Ok(ready),
                Err(Claim::Failed(e)) => return Err(e),
                Err(Claim::Broken) => {
                    say(&format!(
                        "shards: template {} does not restore; booting instead",
                        dir.display()
                    ));
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
        // It becomes the template only once complete, and only if no other run's got there
        // first.
        let n = self.saved.fetch_add(1, Ordering::Relaxed);
        let fresh = dir.with_extension(format!("new-{}-{n}", std::process::id()));
        let mut ready = self.cold(threads, &cfg, &prepared.rootfs, Some(&fresh), &prepared.parent);
        if ready.is_ok()
            && let Err(e) = crate::run::Origin::of(
                guest,
                &prepared.rootfs,
                server.is_some(),
                cfg.gpu,
                prepared.emulator.as_ref().map(|e| e.root.as_path()),
            )
            .write(&dir)
        {
            log(format!("{}: its origin: {e}", dir.display()));
        }
        crate::run::settle(&fresh, &dir);
        if let Ok(r) = &mut ready
            && shards_vmm::snapshot::exists(&dir)
        {
            // What its run touches is the template's working set, which the daemon writes
            // once its last part has come, within what the template's guest can hold: read
            // here, where a boot is waited for anyway, and kept for the pool's VMs.
            let limit = shards_vmm::vm::working_set_limit(&dir)
                .map_err(|e| log(format!("{}: {e}", dir.display())))
                .ok();
            r.records = limit.map(|limit| Records {
                dir: dir.clone(),
                limit,
            });
            pool_of(&mut lock(&self.state), &key, &prepared.rootfs)
                .working_set_limit
                .get_or_insert(limit);
            self.refill_soon(threads, Some(&key));
        }
        ready
    }

    /// A warm VM of pool `key`, once one is ready: one waiting, or one started for this run
    /// where none is starting for it, so a run is served whatever its pool keeps, and a
    /// burst waits for no refill (audit A13, A14).
    fn claim<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        key: &PoolKey,
        rootfs: &Path,
    ) -> Result<Ready, Claim> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut state = lock(&self.state);
        let mut waiting = false;
        let leave = |state: &mut State, waiting: bool| {
            if waiting && let Some(pool) = state.pools.get_mut(key) {
                pool.waiting = pool.waiting.saturating_sub(1);
            }
        };
        loop {
            let pool = pool_of(&mut state, key, rootfs);
            if pool.failures >= MAX_FAILURES {
                state.pools.remove(key);
                return Err(Claim::Broken);
            }
            if !waiting {
                pool.demand.claimed(Instant::now(), self.target);
            }
            // Its pool refills once it has its run (handle).
            if let Some(ready) = pool.ready.pop_front() {
                leave(&mut state, waiting);
                return Ok(ready);
            }
            // A stopping daemon starts no run: no use waiting for a VM to start one.
            if self.stopping.load(Ordering::SeqCst) {
                leave(&mut state, waiting);
                return Err(Claim::Failed("the daemon is shutting down".into()));
            }
            if !waiting {
                pool.waiting += 1;
                waiting = true;
            }
            // Its VM, unless one is starting for it, started outside the pools' lock,
            // which no other claim then waits on (review 7.8). One that cannot be started
            // is said at once, to this run and to those waiting with it, which try again:
            // no VM comes for them otherwise.
            if let Some(planned) = self.plan_refill(&mut state, key, false) {
                drop(state);
                let started = self.start_planned(threads, planned);
                state = lock(&self.state);
                if let Err(e) = started
                    && state.pools.get(key).is_none_or(|p| p.starting < p.waiting)
                {
                    leave(&mut state, waiting);
                    drop(state);
                    self.changed.notify_all();
                    return Err(Claim::Failed(e));
                }
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                leave(&mut state, waiting);
                return Err(Claim::Failed(format!(
                    "no warm VM of {} was ready in {READY_TIMEOUT:?}",
                    key.dir.display()
                )));
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The listener has descriptors again: a collection due may start.
    fn have_room(&self) {
        self.starving.store(false, Ordering::SeqCst);
        let _guard = lock(&self.collecting.due);
        self.collecting.changed.notify_all();
    }

    /// Marks a collection due if a pull, here or by `shards pull`, has said one is
    /// (pull.rs, `collect_due`), for the collector's thread.
    fn mark_collection(&self) {
        let due = crate::pull::collect_due(&self.home);
        if std::fs::remove_file(&due).is_ok() {
            self.collect_soon();
        }
    }

    /// Has a collection run on the collector's thread: what a command left without a
    /// reference goes.
    pub(super) fn collect_soon(&self) {
        *lock(&self.collecting.due) = true;
        self.collecting.changed.notify_all();
    }

    /// Starts the collector's thread.
    fn start_collector<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        if let Err(e) = std::thread::Builder::new()
            .name("collector".into())
            .spawn_scoped(threads, move || self.collect_all())
        {
            log(format!("the collector's thread: {e}; nothing is collected"));
        }
    }

    /// Runs the collections due, one at a time, until [`Collecting::end`], off the
    /// listener, whose clients waited in its backlog while one ran (review 7.14). None
    /// starts while the listener is out of descriptors, which its files would take, and
    /// macOS drops a client whose accept finds none; nor once the daemon is stopping, its
    /// home perhaps gone. A collection marked due as one runs runs again after it.
    fn collect_all(&self) {
        let c = &self.collecting;
        loop {
            {
                let mut due = lock(&c.due);
                while !c.ended.load(Ordering::SeqCst)
                    && (!*due || self.starving.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst))
                {
                    due = c.changed.wait(due).unwrap_or_else(PoisonError::into_inner);
                }
                if c.ended.load(Ordering::SeqCst) {
                    return;
                }
                *due = false;
            }
            if let Err(e) = self.collect_garbage() {
                log(format!("collecting: {e}"));
                // Short of descriptors, which the clients the listener takes as room comes
                // may take back from it, it is due again: once there is room, and not before
                // a RETRY on, lest it spin while there is none (CI 3a640ba: one begun as room
                // came failed so, and none followed).
                if short_of_descriptors(&e) {
                    let due = lock(&c.due);
                    let (mut due, _) = c
                        .changed
                        .wait_timeout(due, RETRY)
                        .unwrap_or_else(PoisonError::into_inner);
                    *due = true;
                }
            }
        }
    }

    /// Removes what nothing needs (audit A13): the image store's content no reference
    /// needs (`Store::collect`), then the templates whose root filesystem has gone, that
    /// another guest saved, or that record no origin, ending their pools, and templates a
    /// daemon before this one left half saved. It waits for the store's lease, which a
    /// run being prepared holds.
    fn collect_garbage(&self) -> Result<(), String> {
        let began = Instant::now();
        // Opened, not made: a home without a store has nothing to collect.
        let root = self.home.join("images");
        if !root.is_dir() {
            return Ok(());
        }
        let store =
            shards_image::store::Store::open(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        // Held whole until the templates are done: no run begins meanwhile.
        let (collected, _whole) = store.collect().map_err(|e| e.to_string())?;
        if collected != shards_image::store::Collected::default() {
            log(format!(
                "collected {} blobs, {} root filesystems and {} files left in ingest/: {} bytes, in {:?}",
                collected.blobs,
                collected.rootfs,
                collected.ingest,
                collected.bytes,
                began.elapsed()
            ));
        }
        let guest = crate::guest::in_use(&self.home)?;
        let templates = self.home.join("templates");
        let entries = match std::fs::read_dir(&templates) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", templates.display())),
        };
        let ours = format!(".new-{}-", std::process::id());
        for entry in entries {
            let dir = entry.map_err(|e| format!("{}: {e}", templates.display()))?.path();
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // A template's record is beside it (run::Origin), and goes once it has gone.
            if let Some(of) = name.strip_suffix(".origin") {
                if !templates.join(of).exists() {
                    let _ = std::fs::remove_file(&dir);
                }
                continue;
            }
            let live = if name.contains(".new-") {
                // Being saved by this daemon, or left by one before it.
                name.contains(&ours)
            } else {
                crate::run::Origin::read(&dir).is_some_and(|o| o.live(guest.as_ref()))
            };
            if live {
                continue;
            }
            // Its pools under every parent go with it.
            let ended: Vec<Pool> = {
                let mut state = lock(&self.state);
                let of: Vec<PoolKey> = state.pools.keys().filter(|k| k.dir == dir).cloned().collect();
                of.iter().filter_map(|k| state.pools.remove(k)).collect()
            };
            for mut pool in ended {
                for ready in pool.ready.drain(..) {
                    let _ = ready.vm.kill(libc::SIGKILL);
                }
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    if let Some(origin) = crate::run::Origin::path(&dir) {
                        let _ = std::fs::remove_file(origin);
                    }
                    log(format!("collected template {}", dir.display()));
                }
                Err(e) => log(format!("collecting {}: {e}", dir.display())),
            }
        }
        Ok(())
    }

    /// Ends the ready VMs of pools unclaimed past their keep-alive, and forgets the pools
    /// that hold nothing (audit A13).
    fn age_pools(&self) {
        let now = Instant::now();
        let mut state = lock(&self.state);
        state.pools.retain(|key, pool| {
            if pool.waiting > 0 || !pool.demand.expired(now, self.keep) {
                return true;
            }
            for aged in pool.ready.drain(..) {
                log(format!(
                    "ending warm VM {} of {}: unclaimed for {:?} (SHARDS_POOL_KEEP)",
                    aged.vm.id(),
                    key.dir.display(),
                    self.keep
                ));
                let _ = aged.vm.kill(libc::SIGKILL);
            }
            pool.starting > 0
        });
    }

    /// Refills the pools claimed from more recently than `key`, whose VM has just become
    /// ready: one that found no room while `key`'s was starting, when only ready VMs can
    /// be ended, takes it now.
    fn rebalance<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, state: &State, key: &PoolKey) {
        let Some(since) = state.pools.get(key).map(|p| p.demand.last()) else {
            return;
        };
        let hotter: Vec<PoolKey> = state
            .pools
            .iter()
            .filter(|(k, p)| *k != key && p.demand.last() > since)
            .map(|(k, _)| k.clone())
            .collect();
        for hot in hotter {
            self.refill_soon(threads, Some(&hot));
        }
    }

    /// Room for up to `want` more warm VMs ahead of runs beside pool `key`'s: within
    /// `warm_max`, all pools together, made by ending the ready VMs of other pools, least
    /// recently claimed from first.
    fn room(&self, state: &mut State, key: &PoolKey, want: usize) -> usize {
        let held = |state: &State| {
            state
                .pools
                .values()
                .map(|p| p.ready.len() + p.starting)
                .sum::<usize>()
        };
        while held(state) + want > self.warm_max {
            let coldest = state
                .pools
                .iter_mut()
                .filter(|(k, p)| *k != key && !p.ready.is_empty())
                .min_by_key(|(_, p)| p.demand.last());
            let Some((cold, pool)) = coldest else {
                break;
            };
            if let Some(evicted) = pool.ready.pop_back() {
                log(format!(
                    "ending warm VM {} of {}: {} warm VMs are kept at most (SHARDS_WARM_MAX)",
                    evicted.vm.id(),
                    cold.dir.display(),
                    self.warm_max
                ));
                let _ = evicted.vm.kill(libc::SIGKILL);
            }
        }
        self.warm_max.saturating_sub(held(state)).min(want)
    }

    /// A VM booted for one run, from `cfg` into `rootfs`, saving a template to `save` on
    /// the way, made under `parent` (D183).
    /// `save` is where it saves the template, and where the template goes once saved.
    fn cold<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        cfg: &Config,
        rootfs: &Path,
        save: Option<&Path>,
        parent: &str,
    ) -> Result<Ready, String> {
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--kernel".into(),
            cfg.kernel.clone().into(),
            "--cmdline".into(),
            cfg.cmdline.clone().into(),
            "--cpus".into(),
            cfg.vcpus.to_string().into(),
            "--memory".into(),
            cfg.memory_mib.to_string().into(),
            "--rootfs".into(),
            rootfs.into(),
        ];
        if cfg.hotplug_mib > 0 {
            let max = cfg.memory_mib.saturating_add(cfg.hotplug_mib);
            args.extend(["--memory-max".into(), max.to_string().into()]);
        }
        if cfg.reporting {
            args.push("--reporting".into());
        }
        if cfg.swap_mib > 0 {
            args.extend(["--swap-window".into(), cfg.swap_mib.to_string().into()]);
        }
        if let Some(init) = &cfg.init {
            args.extend(["--init".into(), init.into()]);
        }
        for pmem in &cfg.pmem {
            args.extend(["--pmem".into(), pmem.into()]);
        }
        if let Some(fresh) = save {
            args.extend(["--snapshot-dir".into(), fresh.into()]);
        }
        if cfg.gpu {
            args.push("--gpu".into());
        }
        for f in &cfg.vfio {
            let flag = if f.iommu { "--vfio-iommu" } else { "--vfio" };
            // By its device node where it is taken by that (D154).
            let given: OsString = f
                .cdev
                .as_ref()
                .map_or_else(|| f.name.clone().into(), |c| c.clone().into_os_string());
            args.extend([flag.into(), given]);
        }
        if !cfg.shares.is_empty() {
            args.extend(["--shares".into(), cfg.shares.len().to_string().into()]);
        }
        // A join disk and a join share in every microVM, empty and unserved until a
        // container joins its network (D119): its templates have them, and so every restore
        // of them.
        args.extend(["--join".into(), "--join-share".into()]);
        args.extend(["--warm".into(), "3".into()]);
        // A guest on a network: a fresh MAC, which a template it saves keeps.
        let net = if cfg.cmdline.contains("shards_net=") {
            Some(shards_net::random_mac().map_err(|e| format!("a guest's MAC: {e}"))?)
        } else {
            None
        };
        self.start_for_run(threads, &args, net, parent)
    }

    /// Starts shards-vm with `args` for one run, under `parent` (D183), and waits until it is
    /// ready: a VM booted for it, or restoring a snapshot of its own (D176).
    fn start_for_run<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        args: &[OsString],
        net: Option<[u8; 6]>,
        parent: &str,
    ) -> Result<Ready, String> {
        let (tx, rx) = mpsc::channel();
        // Told by a shutdown too: registered, then checked, so a shutdown either finds it
        // or came before (`step_aside`).
        let waiting = self.next_cold.fetch_add(1, Ordering::Relaxed);
        lock(&self.booting).insert(waiting, tx.clone());
        if self.stopping.load(Ordering::SeqCst) {
            lock(&self.booting).remove(&waiting);
            return Err("the daemon is shutting down".into());
        }
        let started = self.start(threads, args, net, For::Run(tx), parent);
        // A VM given up on ends as its socket closes, the daemon's end dropped with it.
        let ready = started.and_then(|_| {
            rx.recv()
                .unwrap_or_else(|_| Err("the VM's watcher ended without a word".into()))
        });
        lock(&self.booting).remove(&waiting);
        ready
    }

    /// The parent container `name`'s microVM's processes are under (D183), as its request
    /// keeps it; none where it is no container whose request is kept.
    fn parent_of(&self, name: &str) -> Option<String> {
        let id = self.resolve(name).ok()?;
        let dir = lock(&self.containers).dir(&id);
        let kept = std::fs::read(dir.join(REQUEST)).ok()?;
        Run::decode(&kept).map(|r| r.cgroup_parent)
    }

    /// The spawner a microVM's processes under `parent` are made by (D183): none for the
    /// daemon's own place; the parent's, placed once, or again where the last has gone (its
    /// slice stopped, say).
    fn group_of(&self, parent: &str) -> Result<Option<shards_ipc::Group>, String> {
        if parent.is_empty() {
            return Ok(None);
        }
        let group = self.parents.group(parent, &self.home)?;
        if !shards_ipc::group_ended(group) {
            return Ok(Some(group));
        }
        log(format!("the spawner under {parent} has gone: placing another"));
        self.parents.forget(parent, group);
        self.parents.group(parent, &self.home).map(Some)
    }

    /// Starts shards-vm with `args` as a warm VM whose daemon socket is its descriptor 3,
    /// on a network of its own if `net` names its MAC, and watches it.
    fn start<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        args: &[OsString],
        net: Option<[u8; 6]>,
        dest: For,
        parent: &str,
    ) -> Result<(), String> {
        // Under its parent (D183): it and its network process made by the parent's
        // spawner, in its scope from their first instruction.
        let group = self.group_of(parent)?;
        // A run's microVM is on Docker's default bridge, and reaches nothing through it
        // until a grant opens it (AGENTFILE_ARCH.md §3, default deny); its published
        // ports' connections come in all the same.
        let network = match &net {
            Some(mac) => {
                let bridge = self.bridge.as_ref().ok_or(shards_net::bridge::NO_SUBNET)?;
                Some(crate::netproc::start(
                    shards_net::Policy::DenyAll,
                    mac,
                    bridge,
                    group,
                )?)
            }
            None => None,
        };
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a VM's socket: {e}"))?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let err = io::stderr();
        // On macOS the VM is in App Sandbox from its launch and asks the daemon, on a socket
        // of its own, for the files its arguments name (grant).
        let grants = if cfg!(target_os = "macos") {
            Some(UnixStream::pair().map_err(|e| format!("a VM's grants socket: {e}"))?)
        } else {
            None
        };
        let fds: Vec<_> = [
            (null.as_fd(), 0),
            (err.as_fd(), 1),
            (err.as_fd(), 2),
            (theirs.as_fd(), 3),
        ]
        .into_iter()
        .chain(grants.as_ref().map(|(_, granted)| (granted.as_fd(), 4)))
        .chain(network.iter().flat_map(|(_, side)| {
            let [r, s, w] = crate::netproc::VM_FDS;
            [
                (side.region.as_fd(), r),
                (side.sleeps.as_fd(), s),
                (side.rings.as_fd(), w),
                (side.release.as_fd(), crate::netproc::VM_RELEASE_FD),
            ]
        }))
        .collect();
        let release_fd = crate::netproc::VM_RELEASE_FD.to_string();
        let net_arg = net.map(|mac| OsString::from(crate::netproc::VmSide::arg(&mac)));
        let given = grants
            .iter()
            .flat_map(|_| [OsStr::new("--grants"), OsStr::new("4")])
            .chain(net_arg.iter().flat_map(|a| [OsStr::new("--net"), a.as_os_str()]))
            .chain(
                net_arg
                    .iter()
                    .flat_map(|_| [OsStr::new("--net-release"), OsStr::new(&release_fd)]),
            );
        let args: Vec<&OsStr> = args
            .iter()
            .take(1)
            .map(OsString::as_os_str)
            .chain(given)
            .chain(args.iter().skip(1).map(OsString::as_os_str))
            .collect();
        let mut env = crate::netproc::child_env();
        // A GPU's VM process is the GPU's build (D144), which says MoltenVK's errors alone.
        let gpu = args.iter().any(|a| *a == "--gpu");
        let binary = if gpu {
            crate::helpers::vm_gpu()?
        } else {
            self.vm.clone()
        };
        if gpu {
            env.push(("MVK_CONFIG_LOG_LEVEL", "1".into()));
        }
        let child = match shards_ipc::spawn_in_group(
            group,
            &binary,
            &args,
            &fds,
            false,
            &crate::netproc::env_pairs(&env),
        ) {
            Ok(child) => child,
            Err(e) => {
                if let Some((net, side)) = network {
                    drop(side);
                    crate::netproc::reap(&net);
                }
                return Err(format!("starting {}: {e}", binary.display()));
            }
        };
        drop(fds);
        // The VM holds its side of its network now; its network process goes with it.
        // The control socket stays the daemon's.
        let mac = net;
        let (net_process, net_control) = match network {
            Some((net, side)) => (Some(net), Some((side.control, mac))),
            None => (None, None),
        };
        let grants = grants.map(|(grants, _)| grants);
        let vm = Arc::new(child);
        // Among those the daemon ends as it exits from now on: before its watcher can find
        // it ready, which takes it out.
        let pooled = matches!(dest, For::Pool(_));
        if pooled {
            lock(&self.state).starting.insert(vm.id(), vm.clone());
        }
        let watched = vm.clone();
        let watching = std::thread::Builder::new()
            .name("warm vm".into())
            .spawn_scoped(threads, move || {
                self.watch(threads, watched, ours, grants, net_control, dest, net_process)
            });
        if let Err(e) = watching {
            if pooled {
                lock(&self.state).starting.remove(&vm.id());
            }
            let _ = vm.kill(libc::SIGKILL);
            return Err(format!("watching VM {}: {e}", vm.id()));
        }
        Ok(())
    }

    /// Waits for a VM to be ready and hands it to whoever it is for; then has its end, and
    /// its network process's, followed by the followers' loop (`follow_vm`), and returns:
    /// no thread waits out a VM's life. A pooled VM that ends while waiting leaves its
    /// pool, which refills.
    #[allow(clippy::too_many_arguments)]
    fn watch<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        child: Arc<shards_ipc::Child>,
        socket: UnixStream,
        grants: Option<UnixStream>,
        net: Option<(UnixStream, Option<[u8; 6]>)>,
        dest: For,
        net_process: Option<shards_ipc::Child>,
    ) {
        // Its network process's control socket, and the guest's MAC on it.
        let (net, mac) = match net {
            Some((control, mac)) => (Some(control), mac),
            None => (None, None),
        };
        let pid = child.id();
        let began = Instant::now();
        // What it asks to reach comes first: it opens nothing until it has it.
        let granted = match &grants {
            #[cfg(target_os = "macos")]
            Some(link) => grant(
                link,
                pid,
                match &dest {
                    For::Pool(key) => Some(key.dir.as_path()),
                    For::Run(_) => None,
                },
            ),
            _ => Ok::<(), GrantError>(()),
        };
        drop(grants);
        // A VM the host had not let run yet asked nothing and failed nothing of its own:
        // no fault of its template's (PM M166).
        let unstarted = matches!(&granted, Err(e) if !e.started);
        let ready = granted.map_err(|e| e.why).and_then(|()| ready(&socket, pid));
        match &dest {
            For::Pool(key) => {
                let mut state = lock(&self.state);
                state.starting.remove(&pid);
                let pool = state.pools.entry(key.clone()).or_default();
                pool.starting = pool.starting.saturating_sub(1);
                let came = ready.is_ok();
                match ready {
                    Ok(()) => {
                        pool.failures = 0;
                        pool.demand.refilled(began.elapsed());
                        // Its working set, where it records one, goes with its template,
                        // within what the refiller read the template's guest can hold.
                        let records = pool.working_set_limit.flatten().map(|limit| Records {
                            dir: key.dir.clone(),
                            limit,
                        });
                        pool.ready.push_back(Ready {
                            vm: child.clone(),
                            socket,
                            net,
                            mac,
                            pool: Some(key.clone()),
                            records,
                            joined: None,
                        });
                        self.rebalance(threads, &state, key);
                    }
                    Err(e) => {
                        log(&e);
                        if !unstarted {
                            pool.failures += 1;
                        }
                        let _ = child.kill(libc::SIGKILL);
                    }
                }
                self.changed.notify_all();
                drop(state);
                // A pool with a VM ready ages: a duty the listener, asleep since before,
                // would sleep past (`next_duty`).
                if came {
                    self.wake_listener();
                }
            }
            For::Run(tx) => match ready {
                Ok(()) => {
                    let _ = tx.send(Ok(Ready {
                        vm: child.clone(),
                        socket,
                        net,
                        mac,
                        pool: None,
                        records: None,
                        joined: None,
                    }));
                }
                Err(e) => {
                    let _ = child.kill(libc::SIGKILL);
                    let _ = tx.send(Err(e));
                }
            },
        }
        let pool = match dest {
            For::Pool(key) => Some(key),
            For::Run(_) => None,
        };
        self.follow_vm(threads, child, pool, net_process);
    }
}

/// Why a warm VM did not say it took a run.
#[derive(Debug, PartialEq, Eq)]
enum Untaken {
    /// It never had the run: another VM may take it.
    Surely(String),
    /// It may have: only what it sent before it ended can tell.
    Unknown(String),
}

/// Sends a run to a warm VM and waits for it to say it has taken it. This process keeps
/// its copies of the client's descriptors until then: macOS flushes a socket in flight
/// that no process holds (shards_ipc). A warm VM says TAKEN before it touches the client's
/// stdio or starts anything (warm.rs, receive), and if it cannot, it does neither: one
/// that ends without a word never started the run.
fn hand_over(vm: &UnixStream, payload: &[u8], fds: &[BorrowedFd<'_>]) -> Result<(), Untaken> {
    // A live VM reads its run at once: one that has stopped reading fails the send once a
    // write of it has waited TAKE_TIMEOUT for room, as a run's socket does (`RunSocket`).
    // macOS refuses options on a socket its peer has closed (EINVAL): the send then finds
    // the end at once.
    match vm.set_write_timeout(Some(TAKE_TIMEOUT)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
        Err(e) => return Err(Untaken::Surely(e.to_string())),
    }
    // A request cut short is no request: the VM never had all of it.
    shards_ipc::send(vm, kind::RUN, payload, fds).map_err(|e| Untaken::Surely(e.to_string()))?;
    taken(vm)
}

/// Waits for a warm VM sent a run to say it has taken it.
fn taken(vm: &UnixStream) -> Result<(), Untaken> {
    // macOS refuses options on a socket its peer has closed (EINVAL, xnu sosetoptlock):
    // the read then finds the end at once.
    match vm.set_read_timeout(Some(TAKE_TIMEOUT)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
        Err(e) => return Err(Untaken::Unknown(e.to_string())),
    }
    match shards_ipc::recv(vm) {
        Ok(Some(m)) if m.kind == kind::TAKEN => Ok(()),
        Ok(Some(m)) => Err(Untaken::Unknown(format!(
            "it said message kind {} instead of TAKEN",
            m.kind
        ))),
        Ok(None) => Err(Untaken::Surely("it ended first".into())),
        Err(e) => Err(Untaken::Unknown(e.to_string())),
    }
}

/// The first segment of a container's log (spec.rs, `LOG_STDOUT`), its log and index,
/// which its run's VM appends to; the daemon makes the rest as the VM asks
/// (`kind::LOG_SEGMENT`).
#[derive(Debug)]
struct Log {
    log: File,
    index: File,
    /// Its segment's number: 0 for a new container's.
    seq: u64,
}

/// The newest segment of the log in container directory `dir`, to go on writing: the
/// last whose index is there (segments.rs).
fn newest_log(dir: &Path) -> io::Result<Log> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut seq = 0;
    while dir.join(log_segment(seq + 1).1).exists() {
        seq += 1;
    }
    let (log, index) = log_segment(seq);
    let open = |name: String| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(name))
    };
    Ok(Log {
        log: open(log)?,
        index: open(index)?,
        seq,
    })
}

/// The request container `id` was made by, as a client asks to start it again: attached
/// as that client asks, its stdin only if it was made to read one, never pulled (moby
/// daemon/start.go; docker/cli container/start.go).
fn again_as(stored: Run, client: &Run, id: &str) -> Run {
    Run {
        detach: client.detach,
        interactive: stored.interactive && client.interactive,
        tty: stored.tty.map(|made| client.tty.unwrap_or(made)),
        timing: client.timing,
        pull: shards_ipc::Pull::Never,
        registry_env: client.registry_env.clone(),
        cloud_env: client.cloud_env.clone(),
        daemon: client.daemon,
        again: Some(id.to_string()),
        resume: client.resume.clone(),
        silent: client.silent,
        create: false,
        restart: false,
        ..stored
    }
}

/// A run's IPC, UTS and writable layer as its microVM has them (D156): a /dev/shm sized
/// where it is the run's own alone; a hostname given once; `--storage-opt` its writable
/// layer's size alone, no more than the memory that layer is held in.
fn microvm_namespaces(run: &Run) -> Result<(), String> {
    if run.shm_size != 0 {
        let whose = match run.ipc.as_str() {
            "none" => Some("--ipc none gives it none".to_string()),
            "host" => Some("it is the microVM's (--ipc host), half the microVM's memory".to_string()),
            ipc => ipc
                .strip_prefix("container:")
                .map(|other| format!("it is that of the run it shares it with (--ipc container:{other})")),
        };
        if let Some(whose) = whose {
            return Err(format!("--shm-size sizes a run's own /dev/shm, and {whose}"));
        }
    }
    if run.uts == "host" && run.hostname.is_some() {
        return Err("--hostname and --uts host each name the microVM's host: give one".into());
    }
    if let Some(size) = crate::setup::storage_size(run)? {
        let (limit, of) = match run.resources.memory {
            0 => (
                crate::build::host_memory().unwrap_or(u64::MAX),
                "its host's memory, which the microVM grows to".to_string(),
            ),
            m => (
                u64::try_from(m).unwrap_or(0),
                format!("its memory (--memory {m})"),
            ),
        };
        if size > limit {
            return Err(format!(
                "--storage-opt size={size}: a run's writable layer is held in its microVM's memory, and {size} bytes is more than {of}, {limit} bytes"
            ));
        }
    }
    Ok(())
}

/// A run's `--isolation` (D155): every run is isolated in a microVM of its own, with a
/// kernel of its own, so `default` and `hyperv` (a VM's isolation) ask for what it has, in
/// any case; `process`, a kernel shared with the host, is what no run has.
fn isolation(run: &Run) -> Result<(), String> {
    let given = run.isolation.to_ascii_lowercase();
    match given.as_str() {
        "" | "default" | "hyperv" => Ok(()),
        "process" => Err(format!(
            "--isolation {}: every run is isolated in a microVM of its own, which shares no kernel with its host or another run: its isolation is default, or hyperv, a VM's",
            run.isolation
        )),
        _ => Err(format!(
            "--isolation {}: every run is isolated in a microVM of its own: its isolation is default, or hyperv, a VM's",
            run.isolation
        )),
    }
}

/// A run's annotations as dockerd keeps them (HostConfig.Annotations, which docker/cli's
/// MapOpts makes): each key's last value, a key alone given the empty one.
pub(crate) fn annotations(run: &Run) -> std::collections::BTreeMap<String, String> {
    run.annotations
        .iter()
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

/// ValidateRestartPolicy (moby api/types/container/hostconfig.go): a known policy's name,
/// a retry count only `on-failure`'s and never negative; none given passes, as from a CLI
/// before dockerd v25.
pub(super) fn validate_restart_policy((name, max): &(String, i64)) -> Result<(), String> {
    match name.as_str() {
        "always" | "unless-stopped" | "no" if *max != 0 => {
            let mut msg =
                "invalid restart policy: maximum retry count can only be used with 'on-failure'".to_string();
            if *max < 0 {
                msg.push_str(" and cannot be negative");
            }
            Err(msg)
        }
        "on-failure" if *max < 0 => {
            Err("invalid restart policy: maximum retry count cannot be negative".into())
        }
        "always" | "unless-stopped" | "no" | "on-failure" | "" => Ok(()),
        other => Err(format!(
            "invalid restart policy: unknown policy '{other}'; use one of 'no', 'always', 'on-failure', or 'unless-stopped'"
        )),
    }
}

/// A new container's directory `dir`, with its log's first segment and that segment's
/// index, for this user alone and written by appends.
fn new_log(dir: &Path) -> io::Result<Log> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    // In the home's `containers`, which is there while the home is: a daemon whose home
    // was removed never makes it again.
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    // Its log, then its index, which says the segment is there (segments.rs).
    let (log, index) = log_segment(0);
    let made = |name: String| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(name))
    };
    let log = made(log)?;
    Ok(Log {
        log,
        index: made(index)?,
        seq: 0,
    })
}

/// How long a VM process may take to send its first request for access before its stacks
/// are sampled: a diagnostic's trigger, not a limit. A sixth of [`READY_TIMEOUT`], so that
/// the sample is taken while a stalled VM is still stalled, and four thousand times the
/// VM process's measured launch (2.5 ms, PM M113). The stall it catches, a VM that asks
/// nothing for the whole wait, is the open p99 flake that only its stacks while stalled
/// can explain.
#[cfg(target_os = "macos")]
const GRANT_STALL: Duration = Duration::from_secs(10);

/// Answers what VM `pid` asks to reach until it has all it needs and closes the link,
/// each request within [`READY_TIMEOUT`] (macOS, grant). A VM that has said nothing for
/// [`GRANT_STALL`], before its first request or between two, is sampled once (Apple's
/// sample(1), 3 s of its threads' stacks) into the home, beside daemon.log, as it goes on
/// being waited for; a wait that then fails says where.
#[cfg(target_os = "macos")]
fn grant(link: &UnixStream, pid: u32, template: Option<&Path>) -> Result<(), GrantError> {
    use std::os::fd::AsRawFd as _;
    let started = |why: String| GrantError { why, started: true };
    link.set_read_timeout(Some(READY_TIMEOUT))
        .map_err(|e| started(format!("VM {pid}: {e}")))?;
    // A VM the host has not let run yet: its stacks would hold nothing (PM M166).
    let unstarted = || shards_vmm::platform::launched(pid) == Some(false);
    let mut sampled: Option<String> = None;
    let served = crate::grant_answer::serve_waiting(link, template, &mut |sent| {
        if sampled.is_some() {
            return;
        }
        let mut next = libc::pollfd {
            fd: link.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let stall = i32::try_from(GRANT_STALL.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: poll(2) of one pollfd of ours.
        if unsafe { libc::poll(&raw mut next, 1, stall) } != 0 {
            return;
        }
        if sent == 0 && unstarted() {
            log(format!(
                "VM {pid}: not started by the host in {GRANT_STALL:?}: it has run nothing yet, \
                 as macOS admits each launch after it has assessed the executables launched \
                 before it (PM M166)"
            ));
            return;
        }
        let file = format!("vm-{pid}.sample");
        log(format!(
            "VM {pid}: nothing said in {GRANT_STALL:?} after {sent} messages; sampling its \
             stacks to {file}"
        ));
        // Made as every child of the daemon is, by its spawner (PM M158).
        let spawned = File::open("/dev/null").and_then(|null| {
            let pid = pid.to_string();
            let args = [pid.as_str(), "3", "-mayDie", "-file", file.as_str()].map(OsStr::new);
            let stdio = [0, 1, 2].map(|n| (null.as_fd(), n));
            shards_ipc::spawn(Path::new("/usr/bin/sample"), &args, &stdio, false)
        });
        match spawned {
            // Reaped where it ends, on a thread of its own, so that no zombie is left.
            Ok(child) => {
                let _ = std::thread::Builder::new()
                    .name("sample".into())
                    .spawn(move || child.wait());
            }
            Err(e) => log(format!("VM {pid}: sampling it: {e}")),
        }
        sampled = Some(file);
    });
    served.map_err(|e| {
        if unstarted() {
            return GrantError {
                why: format!(
                    "VM {pid}: not started by the host in {:?}: it has run nothing (PM M166): {e}",
                    GRANT_STALL + READY_TIMEOUT
                ),
                started: false,
            };
        }
        started(match &sampled {
            Some(file) => {
                format!("VM {pid}: {e} (its stacks once quiet {GRANT_STALL:?}: {file} in the home)")
            }
            None => format!("VM {pid}: {e}"),
        })
    })
}

/// Why a VM was not granted what it asked: `started` false for one the host had not let
/// run at all, which no template is at fault for (PM M166). Only macOS grants a VM its
/// files.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct GrantError {
    why: String,
    started: bool,
}

/// Waits for a starting VM to say it is ready.
fn ready(socket: &UnixStream, pid: u32) -> Result<(), String> {
    socket
        .set_read_timeout(Some(READY_TIMEOUT))
        .map_err(|e| format!("VM {pid}: {e}"))?;
    let said = shards_ipc::recv(socket);
    let _ = socket.set_read_timeout(None);
    match said {
        Ok(Some(m)) if m.kind == kind::READY => Ok(()),
        Ok(Some(m)) => Err(format!("VM {pid} said message kind {} instead of READY", m.kind)),
        Ok(None) => Err(format!("VM {pid} ended before it was ready")),
        Err(e) => Err(format!("VM {pid} was not ready: {e}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::io::{Read, Write};

    /// Senders take turns on a run's socket: messages bigger than its buffer, sent from
    /// four threads at once, each arrive whole and as sent. Without the turns, a send's
    /// writes and another's interleave.
    #[test]
    fn senders_on_a_run_socket_take_turns() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let socket = super::RunSocket::new(ours);
        std::thread::scope(|scope| {
            for sender in 0u8..4 {
                let socket = &socket;
                scope.spawn(move || {
                    let message = vec![sender; 64 << 10];
                    for _ in 0..16 {
                        socket.send(kind::SIGNAL, &message, &[]).unwrap();
                    }
                });
            }
            for _ in 0..64 {
                let m = shards_ipc::recv(&theirs).unwrap().unwrap();
                assert_eq!(m.kind, kind::SIGNAL);
                assert_eq!(m.payload.len(), 64 << 10);
                assert!(
                    m.payload.iter().all(|&b| b == m.payload[0]),
                    "a message mixed with another"
                );
            }
        });
    }

    use crate::containers::{Disk, Real};

    use super::*;

    /// The pool of the template in `dir`, in the daemon's own place.
    fn own(dir: impl AsRef<Path>) -> PoolKey {
        PoolKey::new(dir.as_ref(), "")
    }

    /// A client's end is seen; what it sent first is not taken, and is no end; and the
    /// watch ends when told to, though the client stays.
    #[test]
    fn a_clients_end_is_seen_and_nothing_it_sent_is_taken() {
        use std::io::Read as _;
        let (_done, ended) = UnixStream::pair().unwrap();
        let (conn, client) = UnixStream::pair().unwrap();
        drop(client);
        assert!(hung_up(&conn, &ended));
        let (conn, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"x").unwrap();
        assert!(!hung_up(&conn, &ended));
        let mut byte = [0u8; 1];
        (&conn).read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        let (conn, _client) = UnixStream::pair().unwrap();
        let (done, ended) = UnixStream::pair().unwrap();
        drop(done);
        assert!(!hung_up(&conn, &ended));
    }

    /// A client's connection handed over while collections of in-flight descriptors run
    /// still carries the client's bytes: the daemon's copy keeps it reachable until the
    /// warm VM has it. Without that copy, macOS flushes it (shards_ipc; M24).
    #[test]
    fn a_handed_over_connection_survives_until_taken() {
        // Every freed Unix socket starts a collection on macOS.
        let done = AtomicBool::new(false);
        /// Stops the collections however the test ends, so that its scope can.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    drop(UnixStream::pair().unwrap());
                    std::thread::sleep(Duration::from_micros(50));
                }
            });
            let _stop = Stop(&done);
            for _ in 0..20 {
                let (daemon, vm) = UnixStream::pair().unwrap();
                let (mut client, conn) = UnixStream::pair().unwrap();
                let warm = std::thread::spawn(move || {
                    // Slow to take it: collections run while it is in flight.
                    std::thread::sleep(Duration::from_millis(2));
                    let run = shards_ipc::recv(&vm).unwrap().unwrap();
                    assert_eq!(run.kind, kind::RUN);
                    shards_ipc::send(&vm, kind::TAKEN, &[], &[]).unwrap();
                    run.fds
                });
                hand_over(&daemon, b"run", &[conn.as_fd()]).unwrap();
                drop(conn);
                let mut fds = warm.join().unwrap();
                let mut taken = UnixStream::from(fds.pop().unwrap());
                client.write_all(b"x").unwrap();
                let mut got = [0u8; 1];
                taken.read_exact(&mut got).unwrap();
                assert_eq!(&got, b"x");
            }
        });
    }

    #[test]
    fn a_vm_that_ends_before_taking_a_run_fails_the_hand_over() {
        let (daemon, vm) = UnixStream::pair().unwrap();
        let (_client, conn) = UnixStream::pair().unwrap();
        let warm = std::thread::spawn(move || drop(shards_ipc::recv(&vm)));
        let e = hand_over(&daemon, b"run", &[conn.as_fd()]).unwrap_err();
        assert_eq!(e, Untaken::Surely("it ended first".into()));
        warm.join().unwrap();
    }

    /// A VM that says anything but TAKEN may have the run: whether it does is left to what
    /// it sent before it ended.
    #[test]
    fn a_vm_that_answers_otherwise_may_have_taken_the_run() {
        let (daemon, vm) = UnixStream::pair().unwrap();
        let (_client, conn) = UnixStream::pair().unwrap();
        let warm = std::thread::spawn(move || {
            drop(shards_ipc::recv(&vm));
            shards_ipc::send(&vm, kind::STARTED, &[], &[]).unwrap();
            vm
        });
        let e = hand_over(&daemon, b"run", &[conn.as_fd()]).unwrap_err();
        assert!(matches!(e, Untaken::Unknown(_)), "{e:?}");
        drop(warm.join().unwrap());
        // So does a VM whose socket is gone before the request is whole.
        let (daemon, vm) = UnixStream::pair().unwrap();
        drop(vm);
        let e = hand_over(&daemon, b"run", &[conn.as_fd()]).unwrap_err();
        assert!(matches!(e, Untaken::Surely(_)), "{e:?}");
    }

    /// How long a test waits for what must happen before it fails instead of hanging.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A thread's handle, scoped or not, for [`joined`].
    trait Joinable<T> {
        fn finished(&self) -> bool;
        fn join_now(self) -> std::thread::Result<T>;
    }

    impl<T> Joinable<T> for std::thread::JoinHandle<T> {
        fn finished(&self) -> bool {
            self.is_finished()
        }
        fn join_now(self) -> std::thread::Result<T> {
            self.join()
        }
    }

    impl<T> Joinable<T> for std::thread::ScopedJoinHandle<'_, T> {
        fn finished(&self) -> bool {
            self.is_finished()
        }
        fn join_now(self) -> std::thread::Result<T> {
            self.join()
        }
    }

    /// What thread `h` returned, once it has, within [`PATIENCE`].
    /// The `kind::WORKING_SET` messages that carry `set`, each copied out.
    fn working_set_parts(name: &str, set: &[u8]) -> Vec<Vec<u8>> {
        let mut parts = Vec::new();
        let _ = shards_ipc::working_set_parts::<()>(name, set, |p| {
            parts.push(p.to_vec());
            Ok(())
        });
        parts
    }

    /// Whether thread `h` finishes within [`PATIENCE`]; it is not joined.
    fn finishes<T>(h: &impl Joinable<T>) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while !h.finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        h.finished()
    }

    fn joined<T>(h: impl Joinable<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        while !h.finished() {
            assert!(Instant::now() < deadline, "a thread did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        h.join_now().unwrap()
    }

    /// What a test's disk lets through as its test ends, failing or not, so that no
    /// daemon thread waits on it past its test ([`Ending`]).
    trait Opens {
        fn open(&self) {}
    }

    impl Opens for Real {}

    impl Opens for Held {
        fn open(&self) {
            self.let_through();
        }
    }

    std::thread_local! {
        /// What this thread last panicked with, and where: for the watchdog to say, as
        /// libtest's capture of a failing test's output goes with the tests it aborts.
        static PANICKED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    /// Has every thread's panic kept in [`PANICKED`], then said as before.
    fn keep_panics() {
        static KEEPING: std::sync::Once = std::sync::Once::new();
        KEEPING.call_once(|| {
            let before = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let what = info.payload_as_str().unwrap_or("a panic");
                let at = info.location().map(|l| format!(" at {l}")).unwrap_or_default();
                let _ = PANICKED.try_with(|p| *p.borrow_mut() = Some(format!("{what}{at}")));
                before(info);
            }));
        });
    }

    /// A daemon of a home of its own, which starts no VM itself: its tests play the warm
    /// VMs, over socket pairs, and hold each step of a run's start as long as they like
    /// (audit A06).
    struct Test<D: Disk + Opens = Real> {
        daemon: Daemon<D>,
        home: shards_testdir::TempDir,
        /// The processes of the warm VMs played, ended with the test.
        vms: Mutex<Vec<Arc<shards_ipc::Child>>>,
    }

    impl Test {
        fn new(tag: &str) -> Test {
            Test::on(tag, Real)
        }
    }

    impl<D: Disk + Opens> Test<D> {
        /// A daemon whose containers are kept on `disk`, which the test reaches as
        /// `t.daemon.disk`.
        fn on(tag: &str, disk: D) -> Test<D> {
            keep_panics();
            // A home of its own: a daemon's refill thread may still make a spare container
            // as its test ends, and must not make it in another test's home.
            static HOMES: AtomicUsize = AtomicUsize::new(0);
            let n = HOMES.fetch_add(1, Ordering::Relaxed);
            let home = shards_testdir::TempDir::new(&format!("daemon-{tag}-{n}")).unwrap();
            let containers = Registry::open_on(home.join("containers"), &disk, &mut |note| {
                panic!("noted: {note}")
            })
            .unwrap();
            let home_lock = File::create(home.join("daemon.lock")).unwrap();
            let daemon = Daemon::new(
                home.to_path_buf(),
                PathBuf::from("shards-vm"),
                Identity::default(),
                Settings {
                    pool: 0,
                    warm_max: DEFAULT_WARM_MAX,
                    idle: DEFAULT_IDLE,
                    keep: DEFAULT_KEEP,
                    logs: DEFAULT_LOGS,
                    max_clients: most_clients(),
                    bridge: shards_net::bridge::Bridge::elect(&[]),
                },
                containers,
                disk,
                home_lock,
            )
            .unwrap();
            Test {
                daemon,
                home,
                vms: Mutex::default(),
            }
        }

        /// Runs `body` with the daemon's threads in a scope of its own. As the body ends,
        /// failing or not, the daemon's threads are made to end, and all are joined before
        /// this returns; a thread that still has not ended after [`PATIENCE`] aborts the
        /// tests rather than hang them.
        fn run<'e, R>(&'e self, body: impl for<'s> FnOnce(&In<'s, 'e, D>) -> R) -> R {
            let (joined, joining) = mpsc::channel::<()>();
            let r = std::thread::scope(|threads| {
                let _ending = Ending {
                    test: self,
                    joining: Some(joining),
                };
                body(&In { t: self, threads })
            });
            drop(joined);
            r
        }

        /// A warm VM the test plays: what the daemon holds of it, and the test's end of
        /// its socket. Its process is a `sleep`, for the daemon to end.
        fn warm_vm(&self, pool: Option<&str>) -> (Ready, UnixStream) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            theirs.set_read_timeout(Some(PATIENCE)).unwrap();
            let sleep = shards_ipc::spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], false).unwrap();
            let vm = Arc::new(sleep);
            lock(&self.vms).push(vm.clone());
            let ready = Ready {
                vm,
                socket: ours,
                net: None,
                mac: None,
                pool: pool.map(own),
                records: None,
                joined: None,
            };
            (ready, theirs)
        }

        /// `shards ARGS`, as its client asks the daemon: status, stdout and stderr.
        fn ask(&self, args: &[&str]) -> (u8, String, String) {
            ask(&self.daemon, args)
        }

        /// Waits until what `f` finds of the daemon holds.
        fn until(&self, what: &str, f: impl Fn(&Daemon<D>) -> bool) {
            let deadline = Instant::now() + PATIENCE;
            while !f(&self.daemon) {
                assert!(Instant::now() < deadline, "{what}");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn record(&self, id: &str) -> Option<Container> {
            lock(&self.daemon.containers).get(id).cloned()
        }
    }

    impl<D: Disk + Opens> Drop for Test<D> {
        fn drop(&mut self) {
            for vm in lock(&self.vms).drain(..) {
                let _ = vm.kill(libc::SIGKILL);
                let _ = vm.wait();
            }
            // A spare being made as the home is removed fails its removal (ENOTEMPTY); once
            // the home is gone, none is made in it, as a daemon never recreates its home.
            for _ in 0..1000 {
                match std::fs::remove_dir_all(&self.home) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => std::thread::yield_now(),
                    _ => break,
                }
            }
        }
    }

    /// A test's daemon while its threads may run ([`Test::run`]): the test, and the
    /// scope its daemon's threads run in.
    struct In<'s, 'e, D: Disk + Opens> {
        t: &'e Test<D>,
        threads: &'s Threads<'s, 'e>,
    }

    impl<'e, D: Disk + Opens> std::ops::Deref for In<'_, 'e, D> {
        type Target = Test<D>;
        fn deref(&self) -> &Test<D> {
            self.t
        }
    }

    impl<'s, 'e, D: Disk + Opens> In<'s, 'e, D> {
        /// Creates container `name` as a client's run does, in a directory of its own; its
        /// ID, once it is seen.
        fn create(&self, name: &str) -> String {
            let id = self.reserve(name);
            self.t.daemon.await_arrival(&id);
            id
        }

        /// Reserves container `name` as a client's run does, its record being written; its
        /// ID.
        fn reserve(&self, name: &str) -> String {
            self.reserve_with(name, None, false)
        }

        /// [`reserve`](Self::reserve), with the container's own stop signal, and `--rm`.
        fn reserve_with(&self, name: &str, stop_signal: Option<&str>, remove: bool) -> String {
            let (id, _log) = self.t.daemon.new_container().unwrap();
            let run = Run {
                image: "test".into(),
                name: Some(name.into()),
                remove,
                ..Run::default()
            };
            let prepared = Prepared {
                boot: Boot::Given(Box::new(Config::new(PathBuf::from("kernel"), None))),
                gpu: None,
                vfio: Ok(Vec::new()),
                vfio_kernel: None,
                cdi: Ok(None),
                rootfs: PathBuf::new(),
                spec: shards_abi::run::Spec {
                    argv: vec![b"exit".to_vec(), b"7".to_vec()],
                    ..Default::default()
                },
                interactive: false,
                lease: None,
                stop_signal: stop_signal.map(String::from),
                options: crate::spec::Options::default(),
                health: None,
                shell: Vec::new(),
                labels: Default::default(),
                parent: String::new(),
                exposed: Vec::new(),
                image_id: String::new(),
                size: (1, shards_vmm::vm::MEMORY_MIB),
                memory_max: shards_vmm::vm::MEMORY_MIB,
                image_volumes: Vec::new(),
                agentfile: None,
                shares: 0,
                emulator: None,
            };
            self.t.daemon.create(&run, &prepared, &id, Vec::new()).unwrap();
            self.t.daemon.record_arrival(self.threads, &id);
            id
        }

        /// Starts the run of container `id` on a thread of its own, as a client's run is
        /// started, then follows it; the warm VMs sent on the channel returned are the
        /// ones it may take, and each it asks for is said on its other channel. The
        /// thread returns what the client was told, if the run did not start.
        fn start(&self, id: &str) -> Starting<'s> {
            self.start_with(id, None)
        }

        /// [`start`](Self::start), detached: `client` is the client's connection, told
        /// whether the command started.
        fn start_with(&self, id: &str, client: Option<UnixStream>) -> Starting<'s> {
            let (warm, offered) = mpsc::channel::<Ready>();
            let (ask, asks) = mpsc::channel::<()>();
            let (daemon, threads, id) = (&self.t.daemon, self.threads, id.to_string());
            let run = threads.spawn(move || {
                let null = File::open("/dev/null").unwrap();
                let acquire = || {
                    let _ = ask.send(());
                    offered
                        .recv_timeout(PATIENCE)
                        .map_err(|_| "no warm VM".to_string())
                };
                let inbox = daemon.start_run(
                    threads,
                    &id,
                    b"run",
                    &[null.as_fd()],
                    Keep {
                        detached: client.as_ref(),
                        options: crate::spec::Options::default(),
                        health: None,
                        published: Vec::new(),
                        named: None,
                        restarted: false,
                        layer_pending: false,
                        log_files: DEFAULT_LOGS.files,
                        log_compress: false,
                        visit: false,
                        egress: None,
                        agentfile: None,
                        mac: None,
                        link_local: Vec::new(),
                        engine: None,
                    },
                    acquire,
                )?;
                daemon.follow(threads, id.clone(), inbox);
                until_ended(daemon, &id);
                Ok(())
            });
            Starting {
                warm,
                asks,
                asked: std::cell::Cell::new(0),
                run,
            }
        }

        /// `shards ARGS` on a thread of its own.
        fn asking(&self, args: &[&str]) -> std::thread::ScopedJoinHandle<'s, (u8, String, String)> {
            let daemon = &self.t.daemon;
            let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            self.threads.spawn(move || {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                ask(daemon, &args)
            })
        }
    }

    /// Ends a test's daemon's threads as its body ends ([`Test::run`]): the recorder's
    /// queue closes, clients are let go, and the VMs played end, so that every thread the
    /// daemon started returns; a watchdog aborts the tests if one does not in time.
    struct Ending<'e, D: Disk + Opens> {
        test: &'e Test<D>,
        joining: Option<mpsc::Receiver<()>>,
    }

    impl<D: Disk + Opens> Drop for Ending<'_, D> {
        fn drop(&mut self) {
            let daemon = &self.test.daemon;
            // What the test held, a failing one too, is let through.
            daemon.disk.open();
            daemon.recording.end();
            daemon.followers.end();
            // A run's thread waiting for its end, which no follower takes now, returns.
            drop(lock(&daemon.runs));
            daemon.resolved.notify_all();
            daemon.completing.end();
            daemon.files.end();
            daemon.packing.end();
            daemon.shipping.end();
            daemon.checks.end();
            daemon.refills.end();
            daemon.collecting.end();
            daemon.end_clients();
            for vm in lock(&self.test.vms).iter() {
                let _ = vm.kill(libc::SIGKILL);
            }
            // And those the daemon started, as it ends them as it exits, whose watchers
            // return once they have gone.
            for vm in lock(&daemon.state).starting.values() {
                let _ = vm.kill(libc::SIGKILL);
            }
            if let Some(joining) = self.joining.take() {
                // Named, as libtest names the test's thread: the test's own output, and
                // its failure, are captured, and lost as the tests abort.
                let test = std::thread::current().name().unwrap_or("a test").to_string();
                let how = if std::thread::panicking() {
                    let why = PANICKED.with(|p| p.borrow_mut().take());
                    format!("failed: {}", why.as_deref().unwrap_or("its panic unknown"))
                } else {
                    "returned".into()
                };
                // Its own thread ends as the scope does: the sender goes once all are joined.
                let _ = std::thread::Builder::new()
                    .name("watchdog".into())
                    .spawn(move || {
                        if let Err(mpsc::RecvTimeoutError::Timeout) = joining.recv_timeout(PATIENCE) {
                            let _ = writeln!(
                                io::stderr(),
                                "a daemon thread outlived its test by {PATIENCE:?}: {test}, which {how}"
                            );
                            std::process::abort();
                        }
                    });
            }
        }
    }

    /// Waits until run `id` is followed no more: it has ended, or its test is ending,
    /// and no follower takes its end.
    fn until_ended<D: Disk>(daemon: &Daemon<D>, id: &str) {
        let mut runs = lock(&daemon.runs);
        while matches!(runs.get(id), Some(RunState::Tracked(_))) && !daemon.followers.ending() {
            runs = daemon.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
        }
    }

    struct Starting<'s> {
        warm: mpsc::Sender<Ready>,
        asks: mpsc::Receiver<()>,
        asked: std::cell::Cell<usize>,
        run: std::thread::ScopedJoinHandle<'s, Result<(), String>>,
    }

    impl Starting<'_> {
        /// How many warm VMs the run has asked for so far.
        fn asked(&self) -> usize {
            count_asks(&self.asks, &self.asked)
        }
    }

    /// The asks `asks` has said, with those counted already in `seen`: for a run whose
    /// thread has been joined, and so taken from its `Starting`.
    fn count_asks(asks: &mpsc::Receiver<()>, seen: &std::cell::Cell<usize>) -> usize {
        seen.set(seen.get() + asks.try_iter().count());
        seen.get()
    }

    fn ask<D: Disk>(daemon: &Daemon<D>, args: &[&str]) -> (u8, String, String) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let asker = commands::Asker {
            client: 0,
            registry_env: Vec::new(),
            east_asian: false,
            now: 0,
            utc_offset: 0,
            terminal: false,
            width: 0,
            color: false,
            files: Vec::new(),
        };
        let status = daemon.command(&argv, &asker, &commands::Reply(&ours));
        drop(ours);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        // A listing's rows, laid out as the client lays them out.
        let asked = crate::cli::listing::Asked {
            format: args
                .iter()
                .position(|a| *a == "--format")
                .and_then(|i| args.get(i + 1))
                .map_or(String::new(), |f| (*f).to_string()),
            quiet: args.iter().any(|a| matches!(*a, "-q" | "--quiet" | "-aq")),
            trunc: !args.contains(&"--no-trunc"),
            digests: args.contains(&"--digests"),
            human: !args.contains(&"--human=false") && !args.contains(&"-H=false"),
            verbose: args.iter().any(|a| matches!(*a, "-v" | "--verbose")),
            size: args.iter().any(|a| matches!(*a, "-s" | "--size")),
            filtered: args
                .iter()
                .any(|a| *a == "--filter" || a.starts_with("--filter=")),
        };
        let clock = shards_cmdline::format::Clock {
            now: i128::try_from(containers::now()).unwrap(),
            zone: &shards_cmdline::format::utc,
        };
        while let Ok(Some(m)) = shards_ipc::recv(&theirs) {
            match m.kind {
                kind::OUT => out.extend(m.payload),
                kind::ERR => err.extend(m.payload),
                kind::SHEET => {
                    let sheet = shards_ipc::Sheet::decode(&m.payload).unwrap();
                    if let Some(text) = crate::cli::listing::render(&sheet, &asked, &clock).unwrap() {
                        out.extend(text.into_bytes());
                    }
                }
                _ => {}
            }
        }
        (
            status,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// The next message the daemon sends a warm VM: its kind and payload.
    /// The next message a VM hears of the daemon, past its run's stop order, which a VM
    /// keeps for its daemon's going (`STOP_ORDER`).
    fn heard(vm: &UnixStream) -> (u8, Vec<u8>) {
        let m = heard_whole(vm);
        (m.kind, m.payload)
    }

    /// [`heard`], the message whole, its descriptors with it.
    fn heard_whole(vm: &UnixStream) -> shards_ipc::Message {
        loop {
            let m = shards_ipc::recv(vm).unwrap().expect("the daemon hung up");
            if m.kind != kind::STOP_ORDER {
                return m;
            }
        }
    }

    fn say(vm: &UnixStream, what: u8, payload: &[u8]) {
        shards_ipc::send(vm, what, payload, &[]).unwrap();
    }

    /// [`say`] of each message, in one write: the daemon finds them all there as soon as
    /// it finds the first.
    fn say_together(vm: &UnixStream, messages: &[(u8, &[u8])]) {
        let mut bytes = Vec::new();
        for (what, payload) in messages {
            bytes.push(*what);
            bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
            bytes.extend_from_slice(payload);
        }
        (&mut &*vm).write_all(&bytes).unwrap();
    }

    fn signal(n: u32) -> (u8, Vec<u8>) {
        (kind::SIGNAL, n.to_be_bytes().to_vec())
    }

    /// Plays a warm VM that takes its run, starts it, and ends it with `status`.
    fn serve(vm: &UnixStream, status: u8) {
        assert_eq!(heard(vm).0, kind::RUN);
        say(vm, kind::TAKEN, &[]);
        say(vm, kind::STARTED, &[]);
        say(vm, kind::DONE, &[status]);
    }

    /// `rm` of a container whose run no VM has been committed to removes it, tells those
    /// waiting for it 0, and the run never starts: its warm VM hears nothing and goes back
    /// to its pool, and its client hears what `docker run` hears of a container removed
    /// before it was started (audit A06).
    #[test]
    fn rm_cancels_a_run_no_vm_was_committed_to() {
        let t = Test::new("rm-pending");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            t.until("the run asked for a warm VM", |_| starting.asked() == 1);
            let waiting = t.asking(&["wait", "racer"]);
            t.until("wait waits", |d| lock(&d.waiters).contains_key(&id));
            assert_eq!(
                t.ask(&["rm", "racer"]),
                (
                    0,
                    "racer
"
                    .into(),
                    String::new()
                )
            );
            assert_eq!(
                joined(waiting),
                (
                    0,
                    "0
"
                    .into(),
                    String::new()
                )
            );
            assert!(t.record(&id).is_none());

            let (ready, vm) = t.warm_vm(Some("pool"));
            let pid = ready.vm.id();
            starting.warm.send(ready).unwrap();
            assert_eq!(joined(starting.run), Err(format!("No such container: {id}")));
            assert!(!readable(&vm), "the warm VM heard of the cancelled run");
            let state = lock(&t.daemon.state);
            let pooled = &state.pools.get(&own("pool")).unwrap().ready;
            assert_eq!(pooled.iter().map(|r| r.vm.id()).collect::<Vec<_>>(), [pid]);
            drop(state);
            assert!(lock(&t.daemon.runs).is_empty());
            assert_eq!(t.ask(&["ps", "-a", "-q"]), (0, String::new(), String::new()));
        });
    }

    /// `start` of a container whose last run told its client it ended, though the daemon
    /// has not yet taken that run's DONE, starts it again: `again` first takes what every
    /// run has sent, as `docker start` right after `docker run` finds the container exited.
    #[test]
    fn start_takes_the_end_a_run_s_client_has_seen() {
        let t = Test::new("start-after-done");
        t.run(|t| {
            let id = t.create("again");
            let dir = lock(&t.daemon.containers).dir(&id);
            std::fs::write(dir.join(REQUEST), Run::default().encode()).unwrap();
            let (ready, vm) = t.warm_vm(None);
            let (warm, offered) = mpsc::channel::<Ready>();
            warm.send(ready).unwrap();
            let (daemon, threads, id2) = (&t.t.daemon, t.threads, id.clone());
            // Its run started, and not followed: nothing takes what its VM sends but the
            // command that asks.
            let running = threads.spawn(move || {
                let null = File::open("/dev/null").unwrap();
                daemon
                    .start_run(
                        threads,
                        &id2,
                        b"run",
                        &[null.as_fd()],
                        Keep {
                            detached: None,
                            options: crate::spec::Options::default(),
                            health: None,
                            published: Vec::new(),
                            named: None,
                            restarted: false,
                            layer_pending: false,
                            log_files: DEFAULT_LOGS.files,
                            log_compress: false,
                            visit: false,
                            egress: None,
                            agentfile: None,
                            mac: None,
                            link_local: Vec::new(),
                            engine: None,
                        },
                        || {
                            offered
                                .recv_timeout(PATIENCE)
                                .map_err(|_| "no warm VM".to_string())
                        },
                    )
                    .map(|_| ())
            });
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            running.join().unwrap().unwrap();
            say_together(&vm, &[(kind::STARTED, &[]), (kind::DONE, &[0])]);
            match t.daemon.again("again") {
                Ok(Some((again, _, _))) => assert_eq!(again, id),
                Ok(None) => panic!("its ended run read as running"),
                Err(e) => panic!("{e}"),
            }
        });
    }

    /// `rm` of a container whose run is being handed over waits to learn whether it
    /// started, and acts on that: a run that started is running, and is not removed
    /// without `-f`; once it has ended it is.
    #[test]
    fn rm_waits_for_a_run_being_handed_over() {
        let t = Test::new("rm-handing");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            // The request reaches the VM once the daemon has committed the run to it.
            assert_eq!(heard(&vm).0, kind::RUN);
            let removing = t.asking(&["rm", "racer"]);
            t.until("rm began", |d| lock(&d.removing).contains(&id));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!removing.is_finished(), "rm did not wait for the handoff");
            say(&vm, kind::TAKEN, &[]);
            let (status, out, err) = joined(removing);
            assert_eq!((status, out.as_str()), (1, ""));
            assert_eq!(
                err,
                "shards: cannot remove racer: it is running: stop it first, or remove it with -f\n"
            );
            say(&vm, kind::STARTED, &[]);
            say(&vm, kind::DONE, &[7]);
            joined(starting.run).unwrap();
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Exited, Some(7)));
            assert_eq!(
                t.ask(&["rm", "racer"]),
                (
                    0,
                    "racer
"
                    .into(),
                    String::new()
                )
            );
        });
    }

    /// `rm -f` of a container whose run is being handed over kills the run once it has
    /// started, then removes it.
    #[test]
    fn rm_force_kills_a_run_being_handed_over_once_it_runs() {
        let t = Test::new("rm-force");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            let removing = t.asking(&["rm", "-f", "racer"]);
            t.until("rm began", |d| lock(&d.removing).contains(&id));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!removing.is_finished(), "rm -f did not wait for the handoff");
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            assert_eq!(heard(&vm), signal(9));
            say(&vm, kind::DONE, &[137]);
            assert_eq!(
                joined(removing),
                (
                    0,
                    "racer
"
                    .into(),
                    String::new()
                )
            );
            joined(starting.run).unwrap();
            assert!(t.record(&id).is_none());
        });
    }

    /// `wait`, `stop` and `kill` of a container still starting act once its run has
    /// started, as they would had its client seen `run` return: `wait` hears the run's own
    /// code, and the signals reach its command.
    #[test]
    fn wait_stop_and_kill_see_a_start_through() {
        for (args, sent, status, answer) in [
            (
                &["wait", "racer"][..],
                None,
                7,
                "7
",
            ),
            (
                &["stop", "racer"][..],
                Some(15),
                143,
                "racer
",
            ),
            (
                &["kill", "racer"][..],
                Some(9),
                137,
                "racer
",
            ),
            (
                &["kill", "-s", "USR1", "racer"][..],
                Some(10),
                0,
                "racer
",
            ),
        ] {
            let t = Test::new("start-through");
            t.run(|t| {
                let id = t.create("racer");
                let starting = t.start(&id);
                t.until("the run asked for a warm VM", |_| starting.asked() == 1);
                let asked = t.asking(args);
                if args[0] == "wait" {
                    t.until("wait waits", |d| lock(&d.waiters).contains_key(&id));
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert!(!asked.is_finished(), "{args:?} did not wait for the start");
                let (ready, vm) = t.warm_vm(None);
                starting.warm.send(ready).unwrap();
                assert_eq!(heard(&vm).0, kind::RUN);
                say(&vm, kind::TAKEN, &[]);
                say(&vm, kind::STARTED, &[]);
                if let Some(n) = sent {
                    assert_eq!(heard(&vm), signal(n), "{args:?}");
                }
                say(&vm, kind::DONE, &[status]);
                assert_eq!(joined(asked), (0, answer.into(), String::new()), "{args:?}");
                joined(starting.run).unwrap();
            });
        }
    }

    /// `stop` ends every container it names at once, from one loop: 60 that ignore their
    /// SIGTERM all hear it before any hears SIGKILL, and all are stopped one grace after,
    /// where docker/cli's 50 at a time would take two; the answers in the order asked.
    #[test]
    fn stop_ends_every_container_at_once() {
        const N: usize = 60;
        let t = Test::new("stop-all");
        t.run(|t| {
            let names: Vec<String> = (0..N).map(|i| format!("c{i}")).collect();
            let vms: Vec<_> = names.iter().map(|n| running(t, n)).collect();
            let mut args = vec!["stop", "-t", "1"];
            args.extend(names.iter().map(String::as_str));
            let t0 = std::time::Instant::now();
            let asked = t.asking(&args);
            for (_, _, vm) in &vms {
                assert_eq!(heard(vm), signal(15));
            }
            let terms = t0.elapsed();
            for (_, _, vm) in &vms {
                assert_eq!(heard(vm), signal(9));
                say(vm, kind::DONE, &[137]);
            }
            let (status, out, err) = joined(asked);
            let took = t0.elapsed();
            assert_eq!((status, err.as_str()), (0, ""));
            assert_eq!(out, names.iter().map(|n| format!("{n}\n")).collect::<String>());
            assert!(
                terms < Duration::from_millis(900),
                "SIGTERM to all took {terms:?}"
            );
            assert!(
                took >= Duration::from_secs(1) && took < Duration::from_millis(1900),
                "one grace, not two: {took:?}"
            );
            for (_, starting, _) in vms {
                let _ = joined(starting.run);
            }
        });
    }

    /// A daemon told to stop starts no run still pending: its warm VM hears nothing, its
    /// container keeps the code of a start that failed, and those waiting for it hear
    /// that code.
    #[test]
    fn a_stopping_daemon_starts_no_pending_run() {
        let t = Test::new("stop-pending");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            t.until("the run asked for a warm VM", |_| starting.asked() == 1);
            let waiting = t.asking(&["wait", "racer"]);
            t.until("wait waits", |d| lock(&d.waiters).contains_key(&id));
            t.t.daemon.stop_runs(t.threads);
            let (ready, vm) = t.warm_vm(None);
            let child = ready.vm.clone();
            starting.warm.send(ready).unwrap();
            assert_eq!(joined(starting.run), Err("the daemon is shutting down".into()));
            assert_eq!(heard_nothing(&vm), 0, "the warm VM heard of the run");
            assert_eq!(child.wait().unwrap(), 128 + libc::SIGKILL);
            assert_eq!(joined(waiting), (0, "128\n".into(), String::new()));
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Created, Some(128)));
            assert!(lock(&t.daemon.runs).is_empty());
        });
    }

    /// A run being handed over as the daemon is told to stop is stopped once it runs:
    /// its command hears SIGTERM once.
    #[test]
    fn a_stopping_daemon_stops_a_run_being_handed_over() {
        let t = Test::new("stop-handing");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            t.t.daemon.stop_runs(t.threads);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            assert_eq!(heard(&vm), signal(15));
            say(&vm, kind::DONE, &[143]);
            joined(starting.run).unwrap();
            // Nothing more: the run had ended by the time any SIGKILL was due.
            assert_eq!(heard_nothing(&vm), 0);
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Exited, Some(143)));
        });
    }

    /// A client that has sent container command `args`: the daemon's end of its
    /// connection, for [`Daemon::take`], and its own.
    fn commanding(args: &[&str]) -> (UnixStream, UnixStream) {
        let (daemon, client) = UnixStream::pair().unwrap();
        let command = shards_ipc::Command {
            argv: args.iter().map(|a| (*a).to_string()).collect(),
            registry_env: Vec::new(),
            east_asian: false,
            now: 0,
            utc_offset: 0,
            terminal: false,
            width: 0,
            color: false,
            daemon: Identity::default(),
        };
        shards_ipc::send(&client, kind::CONTAINER, &command.encode(), &[]).unwrap();
        (daemon, client)
    }

    /// A stop's escalation ends as soon as no run is left to escalate against, not after
    /// its grace periods: a run that ends at SIGTERM lets every thread the stop started
    /// return well before STOP_GRACE.
    #[test]
    fn a_stops_escalation_ends_with_the_last_run() {
        let t = Test::new("stop-ends");
        let began = Instant::now();
        t.run(|t| {
            let (_, starting, vm) = running(t, "racer");
            t.t.daemon.stop_runs(t.threads);
            assert_eq!(heard(&vm), signal(15));
            say(&vm, kind::DONE, &[143]);
            joined(starting.run).unwrap();
        });
        // Every thread is joined by now, the stop's included.
        assert!(
            began.elapsed() < STOP_GRACE / 2,
            "the stop's thread waited {:?}",
            began.elapsed()
        );
    }

    /// Starts container `name`'s run on a warm VM the test plays, and has it running.
    fn running<'s>(t: &In<'s, '_, Real>, name: &str) -> (String, Starting<'s>, UnixStream) {
        let id = t.create(name);
        let starting = t.start(&id);
        let (ready, vm) = t.warm_vm(None);
        starting.warm.send(ready).unwrap();
        assert_eq!(heard(&vm).0, kind::RUN);
        say(&vm, kind::TAKEN, &[]);
        say(&vm, kind::STARTED, &[]);
        t.until("the run started", |d| {
            lock(&d.containers)
                .get(&id)
                .is_some_and(|c| c.state == Life::Running)
        });
        (id, starting, vm)
    }

    /// A daemon told to stop ends the clients it would otherwise wait for: one that never
    /// sends its request, in hand, and container commands waiting long on a run that
    /// ignores its SIGTERM (review 7.9), whose connections are shut down. The audit's
    /// reproduction (A07): before, the daemon waited for the first until its client
    /// closed.
    #[test]
    fn a_stopping_daemon_ends_the_clients_it_would_wait_for() {
        let t = Test::new("stop-clients");
        t.run(|t| {
            let (id, starting, vm) = running(t, "racer");
            let (idle, idle_client) = UnixStream::pair().unwrap();
            t.t.daemon.take(t.threads, idle);
            let (waiting, waiting_client) = commanding(&["wait", "racer"]);
            t.t.daemon.take(t.threads, waiting);
            let (following, following_client) = commanding(&["logs", "-f", "racer"]);
            t.t.daemon.take(t.threads, following);
            t.until("one client in hand, two waiting long", |d| {
                d.busy.load(Ordering::SeqCst) == 1
                    && lock(&d.long_waits).len() == 2
                    && lock(&d.waiters).contains_key(&id)
            });
            let t0 = Instant::now();
            t.t.daemon.step_aside(t.threads, "a test asked it to stop");
            // The run hears its SIGTERM, and goes on regardless.
            assert_eq!(heard(&vm), signal(15));
            t.until("clients still in hand", |d| {
                d.busy.load(Ordering::SeqCst) == 0 && lock(&d.long_waits).is_empty()
            });
            assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
            for client in [&idle_client, &waiting_client, &following_client] {
                // macOS refuses options on a socket shut down both ways (EINVAL): this one's
                // end has nothing more to wait for anyway.
                let _ = client.set_read_timeout(Some(PATIENCE));
                // One that asked nothing yet is told to ask the next daemon first.
                let mut got = shards_ipc::recv(client);
                if matches!(&got, Ok(Some(m)) if m.kind == kind::RESTART) {
                    got = shards_ipc::recv(client);
                }
                assert!(matches!(got, Ok(None)), "a client was not let go");
            }
            assert!(!lock(&t.daemon.waiters).contains_key(&id), "a waiter left behind");
            say(&vm, kind::DONE, &[143]);
            joined(starting.run).unwrap();
        });
    }

    /// A pool that found no room while a colder pool's VM was starting takes it once that
    /// VM is ready: only ready VMs can be ended, and the colder one's then is (audit A13).
    #[test]
    fn a_colder_pools_vm_ready_gives_way_to_a_hotter_pool() {
        let mut t = Test::new("rebalance");
        {
            let daemon = &mut t.daemon;
            daemon.target = 1;
            daemon.warm_max = 1;
        }
        t.run(|t| {
            // Templates of their own, so their pools refill.
            let (cold, hot) = (t.home.join("cold"), t.home.join("hot"));
            for dir in [&cold, &hot] {
                template(dir);
            }
            assert!(
                shards_vmm::snapshot::exists(&cold),
                "not a template to the daemon"
            );
            let t0 = Instant::now();
            let (ready, _theirs) = t.warm_vm(Some(cold.to_str().unwrap()));
            let vm = ready.vm.clone();
            {
                let mut state = lock(&t.daemon.state);
                let pool = state.pools.entry(own(&cold)).or_default();
                pool.demand.claimed(t0, 1);
                pool.ready.push_back(ready);
                state
                    .pools
                    .entry(own(&hot))
                    .or_default()
                    .demand
                    .claimed(t0 + Duration::from_millis(1), 1);
            }
            t.t.daemon
                .rebalance(t.threads, &lock(&t.daemon.state), &own(&cold));
            // On the refiller's thread.
            t.until("the colder pool kept the room", |d| {
                lock(&d.state).pools[&own(&cold)].ready.is_empty()
            });
            // Ended: its wait returns, as it would not for a sleep of 600 s.
            assert_eq!(vm.wait().unwrap(), 128 + libc::SIGKILL);
        });
    }

    /// A collection due when the home has been removed makes nothing again: not the
    /// home, not its store.
    #[test]
    fn a_collection_never_makes_a_removed_home_again() {
        let t = Test::new("collect-removed");
        t.run(|t| {
            std::fs::remove_dir_all(&t.home).unwrap();
            assert_eq!(t.daemon.collect_garbage(), Ok(()));
            assert!(!t.home.exists(), "the home was made again");
        });
    }

    /// A collection removes templates a daemon before this one left half saved, and ones
    /// that record no origin; it keeps the one this daemon is saving, and does nothing
    /// while a run is being prepared (audit A13).
    /// A working set comes whole from its parts, however many; parts past the template's
    /// bound, parts of two generations, or a malformed one, are refused.
    #[test]
    fn working_sets_are_gathered_whole_and_bounded() {
        let set: Vec<u8> = (0..(shards_ipc::MAX_PAYLOAD * 2 + 5)).map(|i| i as u8).collect();
        let parts = working_set_parts("g-1", &set);
        assert_eq!(parts.len(), 3);
        let mut gathering = WorkingSet::default();
        let limit = set.len() as u64;
        assert_eq!(gather(&mut gathering, limit, &parts[0]), Gathered::More);
        assert_eq!(gather(&mut gathering, limit, &parts[1]), Gathered::More);
        assert_eq!(
            gather(&mut gathering, limit, &parts[2]),
            Gathered::Whole("g-1".into(), set.clone())
        );
        // One byte short of room.
        let mut short = WorkingSet::default();
        let refused = parts
            .iter()
            .map(|p| gather(&mut short, limit - 1, p))
            .find(|g| *g != Gathered::More);
        assert!(matches!(refused, Some(Gathered::Refused(_))), "{refused:?}");
        // Another generation's part in the middle.
        let mut mixed = WorkingSet::default();
        let other = working_set_parts("g-2", &set);
        assert_eq!(gather(&mut mixed, limit, &parts[0]), Gathered::More);
        assert!(matches!(
            gather(&mut mixed, limit, &other[1]),
            Gathered::Refused(_)
        ));
        assert!(matches!(
            gather(&mut WorkingSet::default(), limit, &[1]),
            Gathered::Refused(_)
        ));
    }

    #[test]
    fn collections_remove_templates_nothing_can_use() {
        let t = Test::new("collect-templates");
        t.run(|t| {
            let templates = t.home.join("templates");
            let ours = templates.join(format!("abc.new-{}-0", std::process::id()));
            let left = templates.join("abc.new-1-0");
            let unknown = templates.join("def");
            for dir in [&ours, &left, &unknown] {
                std::fs::create_dir_all(dir).unwrap();
            }
            // It waits for the lease: a child another test spawns in this process holds
            // the lease's file too, for as long as its spawn copies descriptors before it
            // closes the close-on-exec ones, and a flock lasts as long as any holder.
            let lease = crate::pull::store(&t.home).unwrap().lease().unwrap();
            let daemon = &t.t.daemon;
            let collecting = t.threads.spawn(move || daemon.collect_garbage());
            std::thread::sleep(Duration::from_millis(100));
            assert!(!collecting.is_finished(), "collected under a lease");
            assert!(left.exists() && unknown.exists());
            drop(lease);
            assert_eq!(collecting.join().unwrap(), Ok(()));
            assert!(ours.exists(), "a template being saved was collected");
            assert!(!left.exists() && !unknown.exists());
        });
    }

    /// An error's text says it was out of descriptors as std writes an OS error, whatever
    /// the C library calls it; a path before it changes nothing, and other errors are not.
    #[test]
    fn errors_short_of_descriptors_are_known_by_their_code() {
        for code in [libc::EMFILE, libc::ENFILE] {
            let e = io::Error::from_raw_os_error(code);
            assert!(short_of_descriptors(&e.to_string()), "{e}");
            assert!(short_of_descriptors(&format!("/home/images: {e}")), "{e}");
        }
        assert!(!short_of_descriptors(
            &io::Error::from_raw_os_error(libc::ENOENT).to_string()
        ));
        assert!(!short_of_descriptors("a store record that names no blob"));
    }

    /// A socket someone removes while the daemon serves is bound again; one a stop removed
    /// is not, though the listener's pass began before the stop, `closing` read false: a
    /// daemon told to stop never listens again (PM M188).
    #[test]
    fn a_daemon_told_to_stop_never_listens_again() {
        let mut t = Test::new("rl");
        let path = t.home.join("daemon.sock");
        t.daemon.socket = Box::leak(path.clone().into_boxed_path());
        let mut listener = UnixListener::bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        t.daemon.relisten(false, &mut listener);
        assert!(path.exists(), "a removed socket was not bound again");
        assert!(UnixStream::connect(&path).is_ok());
        t.daemon.close();
        assert!(!path.exists());
        t.daemon.relisten(false, &mut listener);
        assert!(!path.exists(), "a daemon told to stop listened again");
    }

    /// A daemon that has closed starts no warm VM: a waiting one that ends as it exits is
    /// replaced by none, which would only end with it as the next daemon starts.
    #[test]
    fn a_closed_daemon_refills_nothing() {
        let mut t = Test::new("rf");
        t.daemon.socket = Box::leak(t.home.join("daemon.sock").into_boxed_path());
        t.run(|t| {
            t.daemon.close();
            t.t.daemon
                .refill_soon(t.threads, Some(&own(t.home.join("template"))));
            assert!(!t.daemon.refills.started.load(Ordering::SeqCst));
        });
    }

    /// No collection starts while the listener is out of descriptors, which its files
    /// would take; the one due starts once the listener has room again.
    #[test]
    fn no_collection_starts_while_the_listener_is_starved() {
        let t = Test::new("collect-starved");
        t.run(|t| {
            crate::pull::store(&t.home).unwrap();
            let left = t.home.join("images").join("ingest").join("left");
            std::fs::write(&left, b"left behind").unwrap();
            t.daemon.starving.store(true, Ordering::SeqCst);
            t.t.daemon.start_collector(t.threads);
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                left.exists(),
                "collected while the listener was out of descriptors"
            );
            t.daemon.have_room();
            t.until("not collected once the listener had room", |_| !left.exists());
        });
    }

    /// The clients in hand at once are what the threads a process may have hold, past the
    /// daemon's own, two a client: 8,187 of macOS's 16,384, 27 of POSIX's least, 64; and
    /// one at the least.
    #[test]
    fn clients_are_what_the_threads_hold() {
        assert_eq!(clients_for(16_384), 8_187);
        assert_eq!(clients_for(POSIX_THREADS), 27);
        assert_eq!(clients_for(0), 1);
    }

    /// A client in hand that waits long counts no more, once however often it says so,
    /// and its end then frees no room it did not hold; a command answered for no client
    /// in hand changes nothing (review 7.9).
    #[test]
    fn a_client_that_waits_long_counts_no_more_once() {
        let t = Test::new("waits-long");
        let busy = || t.daemon.busy.load(Ordering::SeqCst);
        t.daemon.waits_long(7);
        assert_eq!(busy(), 0);
        let (conn, _peer) = UnixStream::pair().unwrap();
        lock(&t.daemon.clients).insert(7, Arc::new(conn));
        t.daemon.busy.fetch_add(1, Ordering::SeqCst);
        t.daemon.waits_long(7);
        t.daemon.waits_long(7);
        assert_eq!(busy(), 0);
        drop(Busy(&t.daemon, 7));
        assert_eq!(busy(), 0);
        assert!(lock(&t.daemon.long_waits).is_empty());
        assert!(lock(&t.daemon.clients).is_empty());
    }

    /// A daemon ending its clients tells one whose request no thread has taken to ask
    /// the next daemon (RESTART), and that thread then answers nothing; one whose request
    /// a thread holds is shut out, as before. A client of another build that its peer's
    /// request made step aside so asks again, and takes no closed connection for its
    /// command's end (x86_64 CI, a gated run that never showed).
    #[test]
    fn a_daemon_stepping_aside_sends_unread_clients_on() {
        let t = Test::new("step-aside");
        let (unread, unread_peer) = UnixStream::pair().unwrap();
        let (taken, taken_peer) = UnixStream::pair().unwrap();
        lock(&t.daemon.clients).insert(1, Arc::new(unread));
        lock(&t.daemon.unread).insert(1);
        lock(&t.daemon.clients).insert(2, Arc::new(taken));
        t.daemon.end_clients();
        let told = shards_ipc::recv(&unread_peer).unwrap().map(|m| m.kind);
        assert_eq!(told, Some(kind::RESTART));
        assert!(shards_ipc::recv(&taken_peer).unwrap().is_none());
        // Its thread, reading the request after, finds it answered.
        assert!(!lock(&t.daemon.unread).remove(&1));
    }

    /// The listener's duty for a pool is when `age_pools` first finds it expired: its
    /// keep-alive after its last claim, or at once if it was never claimed from; none for
    /// a pool with no VM ready, or one a run waits on.
    #[test]
    fn a_pool_is_the_listeners_duty_once_it_would_expire() {
        let mut t = Test::new("duty");
        t.daemon.keep = Duration::from_secs(60);
        t.run(|t| {
            assert_eq!(t.daemon.next_duty(false), None);
            let claimed = PathBuf::from("claimed");
            lock(&t.daemon.state)
                .pools
                .entry(own(&claimed))
                .or_default()
                .demand
                .begin(Instant::now());
            assert_eq!(t.daemon.next_duty(false), None, "no VM ready");
            let (ready, _vm) = t.warm_vm(Some("claimed"));
            lock(&t.daemon.state)
                .pools
                .get_mut(&own(&claimed))
                .unwrap()
                .ready
                .push_back(ready);
            let due = t.daemon.next_duty(false).unwrap();
            assert!(
                due > Duration::from_secs(59) && due <= Duration::from_secs(60),
                "{due:?}"
            );
            lock(&t.daemon.state)
                .pools
                .get_mut(&own(&claimed))
                .unwrap()
                .waiting = 1;
            assert_eq!(t.daemon.next_duty(false), None, "a run waits");
            let (ready, _never_vm) = t.warm_vm(Some("never"));
            lock(&t.daemon.state)
                .pools
                .entry(own("never"))
                .or_default()
                .ready
                .push_back(ready);
            assert_eq!(t.daemon.next_duty(false), Some(Duration::ZERO));
        });
    }

    /// A warm VM coming ready wakes the listener: its pool ages from then on, a duty the
    /// listener, asleep since before, would sleep past. Its working set, if it records
    /// one, goes with its template, within what was read the template's guest can hold.
    #[test]
    fn a_warm_vm_coming_ready_wakes_the_listener() {
        let t = Test::new("ready-wakes");
        t.run(|t| {
            let (ready, theirs) = t.warm_vm(None);
            let Ready { vm, socket, .. } = ready;
            let dir = t.home.join("template");
            {
                let mut state = lock(&t.daemon.state);
                let pool = state.pools.entry(own(&dir)).or_default();
                pool.demand.begin(Instant::now());
                pool.working_set_limit = Some(Some(4096));
            }
            assert!(!readable(&t.daemon.listener_wake.1));
            let watched = vm.clone();
            let (daemon, threads) = (&t.t.daemon, t.threads);
            let watching = dir.clone();
            threads.spawn(move || {
                daemon.watch(
                    threads,
                    watched,
                    socket,
                    None,
                    None,
                    For::Pool(own(&watching)),
                    None,
                )
            });
            shards_ipc::send(&theirs, kind::READY, &[], &[]).unwrap();
            t.until("the listener woken", |d| readable(&d.listener_wake.1));
            assert!(t.daemon.next_duty(false).is_some());
            let records = lock(&t.daemon.state).pools.get(&own(&dir)).and_then(|p| {
                p.ready
                    .front()
                    .and_then(|r| r.records.as_ref().map(|r| (r.dir.clone(), r.limit)))
            });
            assert_eq!(records, Some((dir.clone(), 4096)));
            let _ = vm.kill(libc::SIGKILL);
        });
    }

    /// A pool unclaimed past its keep-alive ends its ready VMs and is forgotten; one
    /// claimed within it, or with a run waiting, keeps them (audit A13).
    #[test]
    fn pools_unclaimed_past_their_keep_alive_end_their_vms() {
        let mut t = Test::new("aging");
        t.daemon.keep = Duration::from_millis(100);
        t.run(|t| {
            let t0 = Instant::now();
            let mut vms = Vec::new();
            {
                let mut state = lock(&t.daemon.state);
                for (dir, waiting) in [("cold", 0), ("warm", 0), ("waited", 1)] {
                    let (ready, theirs) = t.warm_vm(Some(dir));
                    vms.push((dir, ready.vm.clone(), theirs));
                    let pool = state.pools.entry(own(dir)).or_default();
                    pool.demand.begin(t0);
                    pool.waiting = waiting;
                    pool.ready.push_back(ready);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
            lock(&t.daemon.state)
                .pools
                .get_mut(&own("warm"))
                .unwrap()
                .demand
                .claimed(Instant::now(), 2);
            t.daemon.age_pools();
            let state = lock(&t.daemon.state);
            assert!(
                !state.pools.contains_key(&own("cold")),
                "an aged pool is forgotten"
            );
            assert_eq!(state.pools[&own("warm")].ready.len(), 1);
            assert_eq!(state.pools[&own("waited")].ready.len(), 1);
            drop(state);
            // Whether a VM has ended, without reaping it.
            let ended = |vm: &shards_ipc::Child| {
                // SAFETY: an all-zero siginfo_t is valid; waitid(2) fills it for our child.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
                // SAFETY: as above, for a child of this process.
                assert_eq!(unsafe { libc::waitid(libc::P_PID, vm.id(), &mut info, flags) }, 0);
                // SAFETY: waitid filled `info`, whose pid is 0 while the child runs.
                let pid = unsafe { info.si_pid() };
                pid != 0
            };
            let deadline = Instant::now() + PATIENCE;
            while !ended(&vms[0].1) {
                assert!(Instant::now() < deadline, "the aged VM goes on");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!ended(&vms[1].1) && !ended(&vms[2].1), "a kept VM ended");
        });
    }

    /// A warm VM that ends before it says TAKEN surely never had the run, even once its
    /// socket is closed before the daemon waits, when macOS refuses the wait's timeout
    /// (EINVAL); one that said TAKEN and ended has it, and one that said anything else may
    /// (audit A06).
    #[test]
    fn a_vm_gone_before_the_wait_surely_never_took_the_run() {
        let (daemon_end, vm) = UnixStream::pair().unwrap();
        drop(vm);
        assert_eq!(taken(&daemon_end), Err(Untaken::Surely("it ended first".into())));

        let (daemon_end, vm) = UnixStream::pair().unwrap();
        say(&vm, kind::TAKEN, &[]);
        drop(vm);
        assert_eq!(taken(&daemon_end), Ok(()));

        let (daemon_end, vm) = UnixStream::pair().unwrap();
        say(&vm, kind::STARTED, &[]);
        drop(vm);
        assert!(matches!(taken(&daemon_end), Err(Untaken::Unknown(_))));
    }

    /// A client that sends no request, or trickles one out, is let go once its time is up,
    /// however many bytes it sends meanwhile (audit A07).
    #[test]
    fn a_client_is_let_go_if_its_request_is_late() {
        let mut t = Test::new("late-request");
        t.daemon.request_timeout = Duration::from_millis(200);
        t.run(|t| {
            let (silent, silent_client) = UnixStream::pair().unwrap();
            let (trickling, trickling_client) = UnixStream::pair().unwrap();
            let t0 = Instant::now();
            t.t.daemon.take(t.threads, silent);
            t.t.daemon.take(t.threads, trickling);
            let trickle = std::thread::spawn(move || {
                for byte in [kind::CONTAINER, 0, 0, 0, 200].into_iter().chain([0u8; 200]) {
                    if (&trickling_client).write_all(&[byte]).is_err() {
                        return trickling_client;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                trickling_client
            });
            t.until("clients still in hand", |d| d.busy.load(Ordering::SeqCst) == 0);
            let took = t0.elapsed();
            assert!(
                took >= Duration::from_millis(200) && took < Duration::from_secs(3),
                "{took:?}"
            );
            // Disconnected, macOS refuses the option (EINVAL), and the read ends at once anyway.
            let _ = silent_client.set_read_timeout(Some(PATIENCE));
            assert!(matches!(shards_ipc::recv(&silent_client), Ok(None)));
            drop(joined(trickle));
        });
    }

    /// A waiter that stops waiting is forgotten: one whose time is up, and a `wait` or a
    /// `logs -f` whose client hangs up, which ends them as soon as it does (audit A07).
    #[test]
    fn a_waiter_that_stops_waiting_is_forgotten() {
        let t = Test::new("waiters");
        t.run(|t| {
            let (id, starting, vm) = running(t, "racer");
            assert_eq!(
                t.daemon.await_exit_held(
                    lock(&t.daemon.containers),
                    &id,
                    Some(Duration::from_millis(20)),
                    None
                ),
                None
            );
            assert!(!lock(&t.daemon.waiters).contains_key(&id));
            for args in [&["wait", "racer"][..], &["logs", "-f", "racer"]] {
                let (ours, theirs) = UnixStream::pair().unwrap();
                let daemon = &t.t.daemon;
                let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
                let asking = t.threads.spawn(move || {
                    let asker = commands::Asker {
                        client: 0,
                        registry_env: Vec::new(),
                        east_asian: false,
                        now: 0,
                        utc_offset: 0,
                        terminal: false,
                        width: 0,
                        color: false,
                        files: Vec::new(),
                    };
                    daemon.command(&argv, &asker, &commands::Reply(&ours))
                });
                if args[0] == "wait" {
                    t.until("wait waits", |d| lock(&d.waiters).contains_key(&id));
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                }
                assert!(!asking.is_finished(), "{args:?} did not wait");
                let t0 = Instant::now();
                drop(theirs);
                joined(asking);
                assert!(
                    t0.elapsed() < Duration::from_secs(2),
                    "{args:?}: {:?}",
                    t0.elapsed()
                );
                assert!(
                    !lock(&t.daemon.waiters).contains_key(&id),
                    "{args:?}: a waiter left behind"
                );
            }
            say(&vm, kind::DONE, &[0]);
            joined(starting.run).unwrap();
        });
    }

    /// `shards ARGS` as its client asks the daemon, reading as the daemon answers: status,
    /// stdout and stderr, as bytes.
    fn ask_bytes<D: Disk>(daemon: &Daemon<D>, args: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            while let Ok(Some(m)) = shards_ipc::recv(&theirs) {
                match m.kind {
                    kind::OUT => out.extend(m.payload),
                    kind::ERR => err.extend(m.payload),
                    _ => {}
                }
            }
            (out, err)
        });
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let asker = commands::Asker {
            client: 0,
            registry_env: Vec::new(),
            east_asian: false,
            now: 0,
            utc_offset: 0,
            terminal: false,
            width: 0,
            color: false,
            files: Vec::new(),
        };
        let status = daemon.command(&argv, &asker, &commands::Reply(&ours));
        drop(ours);
        let (out, err) = joined(reader);
        (status, out, err)
    }

    use crate::spec::{LOG_STDERR, LOG_STDOUT};

    /// A container's log line: its stream, when its first byte came, and its bytes.
    type Logged = (u8, u64, Vec<u8>);

    /// Writes `lines` to container `id`'s log as a workload's `Logger` writes it
    /// (spec.rs, `LOG_STDOUT`): records of at most 16 KiB, each its stream, its time in
    /// big-endian nanoseconds, its length in a big-endian u32, then its bytes, and each
    /// record's entry in the index.
    fn write_log(t: &Test, id: &str, lines: &[Logged]) {
        use crate::spec::{INDEX_LINE, INDEX_STDERR};
        let dir = lock(&t.daemon.containers).dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut log, mut index) = (Vec::new(), Vec::new());
        for (stream, at, bytes) in lines {
            for (i, piece) in bytes.chunks(16 << 10).enumerate() {
                let mut entry = log.len() as u64;
                if *stream == LOG_STDERR {
                    entry |= INDEX_STDERR;
                }
                if piece.last() == Some(&b'\n') {
                    entry |= INDEX_LINE;
                }
                index.extend(entry.to_be_bytes());
                log.push(*stream);
                log.extend((at + i as u64).to_be_bytes());
                log.extend(u32::try_from(piece.len()).unwrap().to_be_bytes());
                log.extend(piece);
            }
        }
        std::fs::write(dir.join(logs::LOG), log).unwrap();
        std::fs::write(dir.join(logs::INDEX), index).unwrap();
    }

    /// A line of `len` bytes numbered `n`, ending in a newline if `whole`.
    fn line_of(n: u8, len: usize, whole: bool) -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..len)
            .map(|i| match (i * 7 + usize::from(n)) % 251 {
                10 => b'x',
                b => u8::try_from(b).unwrap(),
            })
            .collect();
        if whole && let Some(last) = bytes.last_mut() {
            *last = b'\n';
        }
        bytes
    }

    /// Lines of every length about the largest message the daemon may send reach
    /// `shards logs` whole, byte for byte, on their own streams, with and without their
    /// prefixes, and so do lines several times that, and the last line a container left
    /// unfinished. A client that hangs up is told nothing, and the command fails: its
    /// output did not all arrive (audit A08: a line past 1 MiB was dropped, status 0).
    #[test]
    fn logs_deliver_lines_of_any_length() {
        let t = Test::new("long-lines");
        t.run(|t| {
            let id = t.create("racer");
            let cap = shards_ipc::MAX_PAYLOAD;
            let lines: Vec<Logged> = vec![
                (LOG_STDOUT, 1_000, line_of(1, 6, true)),
                (LOG_STDOUT, 2_000_000_001, line_of(2, cap - 1, true)),
                (LOG_STDOUT, 3_000_000_002, line_of(3, cap, true)),
                (LOG_STDERR, 4_000_000_003, line_of(4, 3 * cap + 5, true)),
                (LOG_STDOUT, 5_000_000_004, line_of(5, cap + 1, true)),
                (LOG_STDOUT, 6_000_000_005, line_of(6, cap + 7, false)),
            ];
            write_log(t, &id, &lines);
            let shown = |stamps: bool, details: bool, which: u8, from: usize| -> Vec<u8> {
                let mut all = Vec::new();
                for (stream, at, bytes) in lines.iter().skip(from) {
                    if *stream != which {
                        continue;
                    }
                    if stamps {
                        all.extend(commands::rfc3339_nano(*at).into_bytes());
                        all.push(b' ');
                    }
                    if details {
                        all.push(b' ');
                    }
                    all.extend(bytes);
                }
                all
            };
            for (args, stamps, details, from) in [
                (&["logs", "racer"][..], false, false, 0),
                (&["logs", "-t", "racer"], true, false, 0),
                (&["logs", "--details", "racer"], false, true, 0),
                (&["logs", "-t", "--details", "racer"], true, true, 0),
                (&["logs", "--tail", "2", "racer"], false, false, 4),
                (&["logs", "-f", "racer"], false, false, 0),
            ] {
                let (status, out, err) = ask_bytes(&t.daemon, args);
                assert_eq!(status, 0, "{args:?}: {}", String::from_utf8_lossy(&err));
                assert!(
                    out == shown(stamps, details, LOG_STDOUT, from),
                    "{args:?}: stdout differs"
                );
                assert!(
                    err == shown(stamps, details, LOG_STDERR, from),
                    "{args:?}: stderr differs"
                );
            }
            // A client that hangs up after one message.
            let (ours, theirs) = UnixStream::pair().unwrap();
            let daemon = &t.t.daemon;
            let asking = t.threads.spawn(move || {
                let argv = vec!["logs".to_string(), "racer".to_string()];
                let asker = commands::Asker {
                    client: 0,
                    registry_env: Vec::new(),
                    east_asian: false,
                    now: 0,
                    utc_offset: 0,
                    terminal: false,
                    width: 0,
                    color: false,
                    files: Vec::new(),
                };
                daemon.command(&argv, &asker, &commands::Reply(&ours))
            });
            drop(shards_ipc::recv(&theirs));
            drop(theirs);
            assert_eq!(joined(asking), 1, "undelivered logs answered as delivered");
        });
    }

    /// How many messages the daemon sends a warm VM before it lets go of its socket.
    fn heard_nothing(vm: &UnixStream) -> usize {
        let mut n = 0;
        while let Ok(Some(_)) = shards_ipc::recv(vm) {
            n += 1;
        }
        n
    }

    /// A VM that surely did not take the run gives way to another; one that may have is
    /// followed as it is, and the run goes to no other, so it never starts twice.
    #[test]
    fn a_handoff_is_retried_only_where_the_run_surely_did_not_start() {
        let t = Test::new("handoff");
        t.run(|t| {
            let id = t.create("racer");
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            let first = ready.vm.clone();
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            // It ends without a word.
            drop(vm);
            assert_eq!(first.wait().unwrap(), 128 + libc::SIGKILL);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            serve(&vm, 0);
            joined(starting.run).unwrap();
            assert_eq!(count_asks(&starting.asks, &starting.asked), 2);
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Exited, Some(0)));

            // One that answers with anything but TAKEN may have started the run: it ends, and
            // the run with it, as a run whose VM ended before its command started.
            let id = t.create("other");
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            let child = ready.vm.clone();
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::OUT, b"?");
            assert_eq!(child.wait().unwrap(), 128 + libc::SIGKILL);
            drop(vm);
            joined(starting.run).unwrap();
            assert_eq!(count_asks(&starting.asks, &starting.asked), 1);
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Created, Some(128)));
        });
    }

    /// A command waits for a run being handed over only once its VM has said TAKEN, after
    /// which the VM may have started it and its client seen that: the answer waits for it
    /// to be registered. One whose VM has not answered is waited for by nothing.
    #[test]
    fn a_command_waits_for_a_handoff_only_once_the_vm_has_taken_it() {
        let t = Test::new("settle-handing");
        t.run(|t| {
            let handing = |socket: &UnixStream| RunState::Handing {
                socket: socket.as_raw_fd(),
            };
            let (quiet, _unanswered) = UnixStream::pair().unwrap();
            lock(&t.daemon.runs).insert("quiet".into(), handing(&quiet));
            let (answered, settled) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.daemon.settle();
                    answered.send(()).unwrap();
                });
                let waited = settled.recv_timeout(Duration::from_secs(2));
                // Resolved either way, so that the scope ends.
                lock(&t.daemon.runs).remove("quiet");
                t.daemon.resolved.notify_all();
                assert!(waited.is_ok(), "waited on a VM that has not answered");
            });

            let (taken, vm) = UnixStream::pair().unwrap();
            say(&vm, kind::TAKEN, &[]);
            lock(&t.daemon.runs).insert("taken".into(), handing(&taken));
            let (answered, settled) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.daemon.settle();
                    answered.send(()).unwrap();
                });
                assert!(
                    settled.recv_timeout(Duration::from_millis(200)).is_err(),
                    "answered while a taken run was being handed over"
                );
                // Its handoff resolves it.
                lock(&t.daemon.runs).insert("taken".into(), RunState::Pending { cancelled: false });
                t.daemon.resolved.notify_all();
                settled.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            lock(&t.daemon.runs).clear();
        });
    }

    /// A command answers once every container being recorded is seen: its run may have
    /// ended, and its client have its status, before its record is written.
    #[test]
    fn a_command_sees_a_container_whose_record_is_being_written() {
        let t = Test::on("settle-arriving", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve("racer");
            let (answered, settled) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.daemon.settle();
                    answered.send(()).unwrap();
                });
                let early = settled.recv_timeout(Duration::from_millis(200));
                // Written either way, so that the scope ends.
                held.let_through();
                assert!(early.is_err(), "answered while a record was being written");
                settled.recv_timeout(PATIENCE).unwrap();
            });
            assert!(lock(&t.daemon.containers).get(&id).is_some());
        });
    }

    /// A container is found from its creation, as dockerd's is: `exec` right after an
    /// attached run starts, while its record is being written, finds it, not "No such
    /// container" (seen 2026-10-02 under the parallel suite).
    #[test]
    fn a_container_whose_record_is_being_written_is_found() {
        let t = Test::on("resolve-arriving", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve("racer");
            let (answered, found) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    answered.send(t.daemon.resolve("racer")).unwrap();
                });
                let early = found.recv_timeout(Duration::from_millis(200));
                held.let_through();
                assert!(
                    early.is_err(),
                    "answered while its record was being written: {early:?}"
                );
                assert_eq!(found.recv_timeout(PATIENCE).unwrap(), Ok(id.clone()));
            });
            assert_eq!(t.daemon.resolve(id.get(..12).unwrap()), Ok(id.clone()));
        });
    }

    /// A run that ends before its container's record is written ends at once, holding up
    /// no other run's messages on the followers' loop (PM M96), and keeps its end: the
    /// record says how it exited, and `wait` hears its code. Before, the end was dropped,
    /// the container stayed running in sight, and `wait` said 0 (seen 2026-10-02 under
    /// parallel E2E runs, which slow the record's write); then the loop waited for the
    /// write.
    #[test]
    fn a_run_that_ends_before_its_record_is_written_keeps_its_end() {
        let t = Test::on("end-arriving", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve("racer");
            // Held before the run starts, on the recorder's thread, which a loaded host
            // may start late.
            t.until("the record's write is held", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            serve(&vm, 3);
            let ended = finishes(&starting.run);
            let arriving = lock(&t.daemon.containers).is_arriving(&id);
            held.let_through();
            assert!(ended, "its run ended only once its record was written");
            joined(starting.run).unwrap();
            assert!(arriving, "its record was written before its run ended");
            t.daemon.await_arrival(&id);
            let record = t.record(&id).unwrap();
            assert_eq!((record.state, record.exit_code), (Life::Exited, Some(3)));
            assert_eq!(t.ask(&["wait", "racer"]), (0, "3\n".into(), String::new()));
        });
    }

    /// A `--rm` run that ends before its container's record is written ends at once too:
    /// its container is out of sight, its name held, and removed once the record's write
    /// is done, never seen; that write, which finds the directory set aside, fails for
    /// nothing, and is neither logged nor kept as a record behind.
    #[test]
    fn a_rm_run_that_ends_before_its_record_is_written_is_removed() {
        let t = Test::on("rm-arriving", Held::default());
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve_with("racer", None, true);
            // Held before the run starts: a run that ended first would leave no record
            // to write (seen on CI, its recorder's thread started late).
            t.until("the record's write is held", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            // Its command never starts: nothing asks for its record after the first.
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::DONE, &[127]);
            let ended = finishes(&starting.run);
            // Set aside before the record's write goes on, which then finds no directory.
            let dir = t.home.join("containers").join(&id);
            let deadline = Instant::now() + PATIENCE;
            while dir.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let set_aside = !dir.exists();
            let (arriving, seen, holder) = {
                let registry = lock(&t.daemon.containers);
                (
                    registry.is_arriving(&id),
                    registry.get(&id).is_some(),
                    registry.name_taken("racer").map(|c| c.id.clone()),
                )
            };
            held.let_through();
            assert!(ended, "its run ended only once its record was written");
            joined(starting.run).unwrap();
            assert!(
                set_aside,
                "its directory was set aside only once its record was written"
            );
            assert_eq!((arriving, seen), (false, false), "in sight, or arriving");
            assert_eq!(holder, Some(id.clone()), "its name let go before its removal");
            t.until("its name was never let go", |d| {
                lock(&d.containers).name_taken("racer").is_none()
            });
            assert!(t.record(&id).is_none());
            assert!(!t.home.join("containers").join(&id).exists());
            let aside = t.home.join("containers").join(format!(".{id}.removing"));
            t.until("what was set aside was not deleted", |_| !aside.exists());
            // Its record's write, which found its directory set aside, failed for nothing.
            t.daemon.await_recorded();
            assert_eq!(
                t.daemon.recording.failed(),
                0,
                "a failure kept for a container gone"
            );
        });
    }

    /// A `--rm` container whose directory cannot be set aside as its run ends is back in
    /// sight, as it ended, its record written as such with no command asking, its name
    /// its own.
    #[test]
    fn a_rm_container_that_cannot_be_set_aside_is_back_as_it_ended() {
        let t = Test::on("rm-refused", Held::default());
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let held = &t.daemon.disk;
            held.let_through();
            let id = t.reserve_with("racer", None, true);
            t.t.daemon.await_arrival(&id);
            held.failing_set_asides.store(true, Ordering::SeqCst);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            serve(&vm, 5);
            joined(starting.run).unwrap();
            let on_disk = || {
                let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).ok()?;
                serde_json::from_slice::<Container>(&bytes).ok()
            };
            t.until("its end was not recorded", |_| {
                on_disk().is_some_and(|c| c.state == Life::Exited && c.exit_code == Some(5))
            });
            assert_eq!(
                t.record(&id).map(|c| (c.state, c.exit_code)),
                Some((Life::Exited, Some(5)))
            );
            assert_eq!(
                lock(&t.daemon.containers).named("racer").map(|c| c.id.clone()),
                Some(id.clone())
            );
        });
    }

    /// Where the completer's thread does not run, a `--rm` container's removal is
    /// completed where its run ends.
    #[test]
    fn a_removal_completes_where_no_completer_runs() {
        let t = Test::new("rm-alone");
        t.run(|t| {
            let id = t.reserve_with("racer", None, true);
            t.t.daemon.await_arrival(&id);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            serve(&vm, 0);
            joined(starting.run).unwrap();
            t.until("its name was never let go", |d| {
                lock(&d.containers).name_taken("racer").is_none()
            });
            assert!(!t.home.join("containers").join(&id).exists());
            let aside = t.home.join("containers").join(format!(".{id}.removing"));
            t.until("what was set aside was not deleted", |_| !aside.exists());
        });
    }

    /// A command waiting for a container's record to be written sees the container taken
    /// out once its `--rm` run ends first, and answers once what was asked before it is
    /// written: nothing else says the container has arrived until its removal is set
    /// aside, which may wait on the filesystem.
    #[test]
    fn a_command_waiting_for_an_arrival_sees_it_taken_out() {
        let t = Test::on("taken-out-arriving", Held::default());
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let held = &t.daemon.disk;
            let other = t.create("other");
            held.holding_writes.store(true, Ordering::SeqCst);
            held.holding_set_asides.store(true, Ordering::SeqCst);
            // The recorder, held on another's record: the arrival waits its turn.
            lock(&t.daemon.containers)
                .change(&other, |c| c.exit_code = Some(1))
                .unwrap();
            t.daemon.record_soon(&other, Vec::new());
            t.until("the other's record is not held", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            let id = t.reserve_with("racer", None, true);
            let starting = t.start(&id);
            let (answered, settled) = mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.daemon.settle();
                    answered.send(()).unwrap();
                });
                let (ready, vm) = t.warm_vm(None);
                starting.warm.send(ready).unwrap();
                serve(&vm, 0);
                let ended = finishes(&starting.run);
                held.let_writes_through();
                let early = settled.recv_timeout(Duration::from_secs(2));
                held.let_through();
                assert!(ended, "its run ended only once its record was written");
                assert!(early.is_ok(), "answered only once its removal was set aside");
            });
            joined(starting.run).unwrap();
        });
    }

    /// `rm` sets its container's directory aside out of the registry's lock, which every
    /// run's end takes: while the rename waits on the filesystem, the lock is free, and a
    /// command, which could say the container is gone, waits until it is set aside, since
    /// a crash before then would bring it back (audit A15). One that cannot be set aside is
    /// back as it was, and `rm` says why.
    #[test]
    fn rm_sets_aside_out_of_the_registrys_lock() {
        let t = Test::on("rm-aside", Held::default());
        t.run(|t| {
            let going = t.create("going");
            let held = &t.daemon.disk;
            held.holding_set_asides.store(true, Ordering::SeqCst);
            let removing = t.asking(&["rm", "going"]);
            t.until("the set-aside is not held", |d| {
                d.disk.setting_aside.load(Ordering::SeqCst)
            });
            let free = t.daemon.containers.try_lock().is_ok();
            let listing = t.asking(&["ps", "-a"]);
            std::thread::sleep(Duration::from_millis(100));
            let early = listing.is_finished();
            held.let_through();
            assert!(free, "the registry's lock held through the rename");
            assert!(!early, "answered before the removal was set aside");
            assert_eq!(joined(removing), (0, "going\n".into(), String::new()));
            let (code, out, err) = joined(listing);
            assert_eq!(code, 0, "{err}");
            assert!(!out.contains("going"), "{out}");
            assert!(!t.home.join("containers").join(&going).exists());

            t.create("staying");
            held.failing.store(true, Ordering::SeqCst);
            let refused = t.ask(&["rm", "staying"]);
            held.failing.store(false, Ordering::SeqCst);
            assert_eq!(
                refused,
                (
                    1,
                    String::new(),
                    "shards: cannot remove staying: a failing disk\n".into()
                )
            );
            let (_, out, _) = t.ask(&["ps", "-a"]);
            assert!(out.contains("staying"), "{out}");
            assert!(lock(&t.daemon.containers).name_taken("staying").is_some());
        });
    }

    /// Container references as the Docker CLI's client sends them (moby client utils.go,
    /// trimID), measured on Docker 29.3.1: their spaces trimmed, and an empty one refused
    /// in its words; `rm`'s own refusal comes first. Found once trimmed, each is said as
    /// given.
    #[test]
    fn references_to_microvms_are_trimmed_and_checked() {
        let t = Test::new("references");
        t.run(|t| {
            t.create("racer");
            let empty = || {
                (
                    1,
                    String::new(),
                    "invalid container name or ID: value is empty\n".to_string(),
                )
            };
            for args in [
                &["stop", ""][..],
                &["kill", " "],
                &["wait", "\t"],
                &["logs", ""],
                &["port", " "],
                &["rm", " "],
            ] {
                assert_eq!(t.ask(args), empty(), "{args:?}");
            }
            assert_eq!(
                t.ask(&["rm", ""]),
                (1, String::new(), "container name cannot be empty\n".into())
            );
            // Resolved: a container with no ports says none.
            assert_eq!(t.ask(&["port", "  racer  "]), (0, String::new(), String::new()));
            assert_eq!(t.ask(&["rm", " racer "]), (0, " racer \n".into(), String::new()));
            // A reference is named as the client sent it: trimmed.
            let (_, starting, vm) = running(t, "busy");
            assert_eq!(
                t.ask(&["rm", " busy "]),
                (
                    1,
                    String::new(),
                    "shards: cannot remove busy: it is running: stop it first, or remove it with -f\n".into()
                )
            );
            say(&vm, kind::DONE, &[0]);
            joined(starting.run).unwrap();
            assert_eq!(
                t.ask(&["kill", " busy "]),
                (
                    1,
                    String::new(),
                    "shards: cannot kill busy: it is not running\n".to_string()
                )
            );
        });
    }

    /// A container command answers once every record being written is (`settle`): `ps`
    /// lists a container whose record a thread is still writing. An image command reads
    /// no container, and waits for no record.
    #[test]
    fn container_commands_wait_for_records_and_image_commands_do_not() {
        let t = Test::on("settle-commands", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            t.reserve("arriving");
            t.until("its record's write is held", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            let (ps_said, ps) = mpsc::channel();
            let (inspect_said, inspect) = mpsc::channel();
            std::thread::scope(|s| {
                s.spawn(move || {
                    let _ = ps_said.send(t.ask(&["ps", "-a"]));
                });
                s.spawn(move || {
                    let _ = inspect_said.send(t.ask(&["image", "inspect", "nothing"]));
                });
                let inspected = inspect.recv_timeout(Duration::from_secs(2));
                let early = ps.recv_timeout(Duration::from_millis(200));
                held.let_through();
                assert!(
                    inspected.is_ok(),
                    "an image command waited for a container's record"
                );
                assert!(early.is_err(), "ps answered before the record was written");
                let (status, listed, _) = ps.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(status, 0);
                assert!(listed.contains("arriving"), "{listed}");
            });
        });
    }

    /// A VM that stops partway through a message holds up no command: what has come of it
    /// waits for the rest, which is then taken as if it had come whole.
    #[test]
    fn a_vm_stopped_partway_through_a_message_holds_up_no_command() {
        let t = Test::new("partial");
        t.run(|t| {
            let (id, starting, vm) = running(t, "halting");
            // DONE, status 5, cut after its kind and part of its length.
            let frame = [kind::DONE, 0, 0, 0, 1, 5];
            (&vm).write_all(&frame[..3]).unwrap();
            let (asked, answer) = mpsc::channel();
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _ = asked.send(t.ask(&["ps"]));
                });
                let listed = answer.recv_timeout(Duration::from_secs(5));
                // The rest, which a waiting command would have been waiting for.
                (&vm).write_all(&frame[3..]).unwrap();
                let (status, listed, _) = listed.unwrap();
                assert_eq!(status, 0);
                assert!(listed.contains("halting"), "{listed}");
            });
            joined(starting.run).unwrap();
            assert_eq!(t.record(&id).unwrap().exit_code, Some(5));
        });
    }

    /// A warm VM that has stopped reading fails a run's handoff, surely untaken, once a
    /// write waits TAKE_TIMEOUT for room, where the send waited for ever: the run then
    /// tries another VM. (A write cut short by the timeout returns what it wrote; the next
    /// waits again: two timeouts in all.)
    #[test]
    fn a_vm_that_reads_nothing_fails_its_handoff() {
        let (daemon, vm) = UnixStream::pair().unwrap();
        let (done, finished) = mpsc::channel();
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let payload = vec![0u8; shards_ipc::MAX_PAYLOAD];
            let _ = done.send(hand_over(&daemon, &payload, &[]));
        });
        let handed = finished.recv_timeout(TAKE_TIMEOUT * 3).unwrap();
        assert!(matches!(handed, Err(Untaken::Surely(_))));
        assert!(t0.elapsed() >= TAKE_TIMEOUT, "{:?}", t0.elapsed());
        drop(vm);
    }

    /// A container being removed, not running, is listed so until it is gone, as dockerd
    /// lists one (moby daemon/container/state.go, RemovalInProgress).
    #[test]
    fn a_container_being_removed_is_listed_so() {
        let t = Test::new("ps-removing");
        t.run(|t| {
            let id = t.create("going");
            lock(&t.daemon.removing).insert(id.clone());
            let row = || {
                t.ask(&["ps", "-a"])
                    .1
                    .lines()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string()
            };
            assert!(row().contains("   Removal In Progress   "), "{}", row());
            lock(&t.daemon.removing).remove(&id);
            assert!(row().contains("   Created   "), "{}", row());
        });
    }

    /// A run whose warm VM cannot be started is told why at once, where it waited
    /// READY_TIMEOUT for one; and the template is not counted broken for it: the host
    /// failed, not the template. (A test's daemon names a VM binary that is not there.)
    #[test]
    fn a_run_whose_vm_cannot_start_is_told_why_at_once() {
        let t = Test::new("unstartable");
        t.run(|t| {
            let dir = t.home.join("template");
            std::fs::create_dir_all(dir.join("g-1")).unwrap();
            std::fs::write(dir.join("current"), b"g-1\n").unwrap();
            std::fs::write(dir.join("g-1").join("state"), b"").unwrap();
            let t0 = Instant::now();
            let claimed = t.t.daemon.claim(t.threads, &own(&dir), Path::new("/rootfs"));
            assert!(
                matches!(&claimed, Err(Claim::Failed(why)) if why.starts_with("starting a warm VM of ")),
                "not refused as unstartable"
            );
            assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
            let state = lock(&t.daemon.state);
            assert_eq!(
                state
                    .pools
                    .get(&own(&dir))
                    .map(|p| (p.failures, p.waiting, p.starting)),
                Some((0, 0, 0))
            );
            // What its VMs' working sets may take was read before any started, once:
            // nothing, of a state that is no snapshot.
            assert_eq!(
                state.pools.get(&own(&dir)).map(|p| p.working_set_limit),
                Some(Some(None))
            );
        });
    }

    /// Whether process `pid` is gone, reaped: one that has ended unreaped still answers a
    /// signal of 0.
    fn reaped(pid: u32) -> bool {
        // SAFETY: kill(2) with signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) != 0 }
    }

    /// A warm VM that ends while it waits is reaped, and taken out of its pool, by the
    /// followers' loop: no thread waits out its life (PM M90).
    #[test]
    fn a_warm_vm_ending_as_it_waits_is_reaped_and_leaves_its_pool() {
        let t = Test::new("vm-end");
        t.run(|t| {
            // A template, so that its pool stays as it refills.
            let dir = t.home.join("template");
            template(&dir);
            let (ready, _theirs) = t.warm_vm(Some(dir.to_str().unwrap()));
            let vm = ready.vm.clone();
            lock(&t.daemon.state)
                .pools
                .entry(own(&dir))
                .or_default()
                .ready
                .push_back(ready);
            t.t.daemon.follow_vm(t.threads, vm.clone(), Some(own(&dir)), None);
            let _ = vm.kill(libc::SIGKILL);
            t.until("the VM did not leave its pool", |d| {
                lock(&d.state)
                    .pools
                    .get(&own(&dir))
                    .is_some_and(|p| p.ready.is_empty())
            });
            t.until("the VM was not reaped", |_| reaped(vm.id()));
        });
    }

    /// A run's VM says DONE before it tells its client, whose next program may then bind
    /// the run's port: found in use, the port is the run's no more once what the run sent
    /// is taken, though nothing has taken it yet, and is free once the run's network
    /// process has gone. A run whose command goes on holds its port.
    #[test]
    fn a_port_of_a_run_that_has_said_done_is_not_allocated() {
        let t = Test::new("port-done");
        t.run(|t| {
            let id = t.create("ended");
            let at = std::net::SocketAddr::from(([0, 0, 0, 0], 1));
            lock(&t.daemon.ports_held).push(publish::Held {
                container: id.clone(),
                vm: None,
                at: vec![(at, publish::TCP)],
                listeners: Vec::new(),
            });
            let (ready, vm) = t.warm_vm(None);
            // Followed by nothing: what it sends waits until it is taken.
            let keep = Keep {
                detached: None,
                options: crate::spec::Options::default(),
                health: None,
                published: Vec::new(),
                named: None,
                restarted: false,
                layer_pending: false,
                log_files: DEFAULT_LOGS.files,
                log_compress: false,
                visit: false,
                egress: None,
                agentfile: None,
                mac: None,
                link_local: Vec::new(),
                engine: None,
            };
            let _inbox = t.daemon.register(ready, &id, keep);
            say(&vm, kind::STARTED, &[]);
            assert_eq!(t.daemon.in_use(at, publish::TCP), publish::InUse::Allocated);
            say(&vm, kind::DONE, &[0]);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.until("the run did not end", |d| {
                        !matches!(lock(&d.runs).get(&id), Some(RunState::Tracked(_)))
                    });
                    t.daemon.free_ports(Some(&id), None);
                });
                assert_eq!(t.daemon.in_use(at, publish::TCP), publish::InUse::Freed);
            });
            assert_eq!(t.record(&id).unwrap().state, Life::Exited);
        });
    }

    /// An address a run asked for on its network is the next run's to ask for once the run
    /// has said its end, though nothing has taken that yet, as dockerd's is once `run
    /// --rm` has returned (PM M182); while it runs, it is refused in dockerd's words.
    #[test]
    fn an_asked_address_is_free_once_its_holder_has_said_its_end() {
        let t = Test::new("ip-held");
        t.run(|t| {
            let made = t.ask(&["network", "create", "--subnet", "10.78.3.0/24", "n"]);
            assert_eq!(made.0, 0, "{made:?}");
            let id = t.create("holder");
            t.daemon
                .join_network(&id, "n", "10.78.3.9", "", &[], "h")
                .unwrap();
            let (ready, vm) = t.warm_vm(None);
            // Followed by nothing: what it sends waits until it is taken.
            let keep = Keep {
                detached: None,
                options: crate::spec::Options::default(),
                health: None,
                published: Vec::new(),
                named: None,
                restarted: false,
                layer_pending: false,
                log_files: DEFAULT_LOGS.files,
                log_compress: false,
                visit: false,
                egress: None,
                agentfile: None,
                mac: None,
                link_local: Vec::new(),
                engine: None,
            };
            let _inbox = t.daemon.register(ready, &id, keep);
            say(&vm, kind::STARTED, &[]);
            let asker = t.create("asker");
            let ask = || {
                t.daemon
                    .join_network(&asker, "n", "10.78.3.9", "", &[], "a")
                    .map(|_| ())
            };
            assert_eq!(
                ask(),
                Err("failed to set up container networking: Address already in use".to_string())
            );
            say(&vm, kind::DONE, &[0]);
            assert_eq!(ask(), Ok(()));
        });
    }

    /// A run that publishes nothing, as one on `--network none` never does, holds nothing:
    /// a record of no addresses, one a run, would only grow (review 2.11, 2.18).
    #[test]
    fn a_run_that_publishes_nothing_holds_nothing() {
        let t = Test::new("hold-nothing");
        t.run(|t| {
            let id = t.create("quiet");
            t.daemon.hold_ports(&id, &[]);
            assert!(lock(&t.daemon.ports_held).is_empty());
            let (fd, port) = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                (OwnedFd::from(l), port)
            };
            let at = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            t.daemon.hold_ports(
                &id,
                &[publish::Listener {
                    fd,
                    at,
                    guest_port: 80,
                    proto: publish::TCP,
                }],
            );
            assert_eq!(t.daemon.in_use(at, publish::TCP), publish::InUse::Allocated);
            t.daemon.free_ports(Some(&id), None);
            assert!(lock(&t.daemon.ports_held).is_empty());
        });
    }

    /// A port whose run is being handed over is seen through the handoff, as a name is:
    /// the run's client may have heard its end before the handoff registered it.
    #[test]
    fn a_port_of_a_run_being_handed_over_is_seen_through_its_handoff() {
        let t = Test::new("port-handing");
        t.run(|t| {
            let id = t.create("handing");
            let at = std::net::SocketAddr::from(([0, 0, 0, 0], 1));
            lock(&t.daemon.ports_held).push(publish::Held {
                container: id.clone(),
                vm: None,
                at: vec![(at, publish::TCP)],
                listeners: Vec::new(),
            });
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            std::thread::scope(|scope| {
                let asked = scope.spawn(|| t.daemon.in_use(at, publish::TCP));
                std::thread::sleep(Duration::from_millis(100));
                assert!(!asked.is_finished(), "answered before the handoff was through");
                // In one write, as the name's test says them: three writes let the asker
                // take the run's messages between them, and find it running.
                say_together(
                    &vm,
                    &[(kind::TAKEN, &[]), (kind::STARTED, &[]), (kind::DONE, &[0])],
                );
                t.until("the run did not end", |d| {
                    !matches!(lock(&d.runs).get(&id), Some(RunState::Tracked(_)))
                });
                t.daemon.free_ports(Some(&id), None);
                assert_eq!(asked.join().unwrap(), publish::InUse::Freed);
            });
            joined(starting.run).unwrap();
        });
    }

    /// A VM's network process is given its grace once the VM has ended, then ended, and
    /// the VM's ports are freed once both have gone, not before.
    #[test]
    fn a_network_process_is_ended_past_its_grace_and_the_ports_freed() {
        let t = Test::new("net-end");
        t.run(|t| {
            let (ready, _theirs) = t.warm_vm(None);
            let vm = ready.vm.clone();
            let net = shards_ipc::spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], false).unwrap();
            let net_pid = net.id();
            lock(&t.daemon.ports_held).push(publish::Held {
                container: "c".into(),
                vm: Some(vm.id()),
                at: Vec::new(),
                listeners: Vec::new(),
            });
            t.t.daemon.follow_vm(t.threads, vm.clone(), None, Some(net));
            let ended = Instant::now();
            let _ = vm.kill(libc::SIGKILL);
            t.until("the ports were not freed", |d| lock(&d.ports_held).is_empty());
            assert!(ended.elapsed() >= crate::netproc::GRACE, "freed within the grace");
            assert!(reaped(net_pid), "the network process was not reaped");
            // The VM is reaped once its ports are freed (follow.rs), which the wait above
            // could see first.
            t.until("the VM was not reaped", |_| reaped(vm.id()));
        });
    }

    /// A network process that ends before its VM is reaped, and the VM's ports stay held
    /// until the VM ends too: then they are freed at once.
    #[test]
    fn ports_are_freed_as_the_vm_ends_after_its_network_process() {
        let t = Test::new("net-first");
        t.run(|t| {
            let (ready, _theirs) = t.warm_vm(None);
            let vm = ready.vm.clone();
            let net = shards_ipc::spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], false).unwrap();
            let net_pid = net.id();
            lock(&t.daemon.ports_held).push(publish::Held {
                container: "c".into(),
                vm: Some(vm.id()),
                at: Vec::new(),
                listeners: Vec::new(),
            });
            t.t.daemon.follow_vm(t.threads, vm.clone(), None, Some(net));
            // SAFETY: kill(2) of the test's own child.
            unsafe { libc::kill(net_pid as libc::pid_t, libc::SIGKILL) };
            t.until("the network process was not reaped", |_| reaped(net_pid));
            assert_eq!(lock(&t.daemon.ports_held).len(), 1, "freed with the VM running");
            let ended = Instant::now();
            let _ = vm.kill(libc::SIGKILL);
            t.until("the ports were not freed", |d| lock(&d.ports_held).is_empty());
            assert!(ended.elapsed() < crate::netproc::GRACE, "{:?}", ended.elapsed());
        });
    }

    /// A VM that ends while its network process has its grace is not reaped until its
    /// ports are freed: its pid, by which they are held, names no new process meanwhile,
    /// whose ports the freeing would take (review 2.25).
    #[test]
    fn a_vms_pid_is_kept_until_its_ports_are_freed() {
        let t = Test::new("vm-pid-kept");
        t.run(|t| {
            let (ready, _theirs) = t.warm_vm(None);
            let vm = ready.vm.clone();
            let net = shards_ipc::spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], false).unwrap();
            let net_pid = net.id();
            lock(&t.daemon.ports_held).push(publish::Held {
                container: "c".into(),
                vm: Some(vm.id()),
                at: Vec::new(),
                listeners: Vec::new(),
            });
            t.t.daemon.follow_vm(t.threads, vm.clone(), None, Some(net));
            let _ = vm.kill(libc::SIGKILL);
            vm.ended().unwrap();
            // Its end handled: the network process given its grace, which a deadline holds.
            t.until("the VM's end was not handled", |d| {
                !lock(&d.followers.deadlines).is_empty()
            });
            assert!(!reaped(vm.id()), "reaped while its ports were held");
            assert_eq!(lock(&t.daemon.ports_held).len(), 1);
            // SAFETY: kill(2) of the test's own child.
            unsafe { libc::kill(net_pid as libc::pid_t, libc::SIGKILL) };
            t.until("the ports were not freed", |d| lock(&d.ports_held).is_empty());
            t.until("the VM was not reaped", |_| reaped(vm.id()));
        });
    }

    /// A VM that has ended before it is followed is reaped at once, and its network
    /// process given its grace: macOS watches only ends to come, and refuses one past, so
    /// the follower finds it, not the loop, which it then wakes to keep the grace.
    #[test]
    fn a_vm_ended_before_it_is_followed_is_reaped() {
        let t = Test::new("vm-gone");
        t.run(|t| {
            // The loop, started and asleep, with nothing due.
            let (ready, _theirs) = t.warm_vm(None);
            t.t.daemon.follow_vm(t.threads, ready.vm.clone(), None, None);
            std::thread::sleep(Duration::from_millis(100));
            let vm =
                Arc::new(shards_ipc::spawn(Path::new("/bin/sleep"), &["0".as_ref()], &[], false).unwrap());
            vm.ended().unwrap();
            let net = shards_ipc::spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], false).unwrap();
            let net_pid = net.id();
            lock(&t.daemon.ports_held).push(publish::Held {
                container: "c".into(),
                vm: Some(vm.id()),
                at: Vec::new(),
                listeners: Vec::new(),
            });
            t.t.daemon.follow_vm(t.threads, vm.clone(), None, Some(net));
            t.until("the VM was not reaped", |_| reaped(vm.id()));
            t.until("the ports were not freed", |d| lock(&d.ports_held).is_empty());
            assert!(reaped(net_pid));
        });
    }

    /// A run's start and end are recorded off the registry's lock (review 7.7): while the
    /// start's record waits on the filesystem, the lock is free and the run's end is taken
    /// at once; a command waits only to answer with what is recorded (audit A15), and
    /// then answers with the end, which is the record's last.
    #[test]
    fn a_records_write_holds_up_only_the_answers_that_need_it() {
        let t = Test::on("record-held", Held::default());
        t.run(|t| {
            let id = t.create("held");
            t.daemon.disk.holding_writes.store(true, Ordering::SeqCst);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            t.until("the start's record is not being written", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            say(&vm, kind::DONE, &[3]);
            joined(starting.run).unwrap();
            assert_eq!(
                t.record(&id).map(|c| (c.state, c.exit_code)),
                Some((Life::Exited, Some(3)))
            );
            let asking = t.asking(&["ps", "-a"]);
            std::thread::sleep(Duration::from_millis(100));
            assert!(!asking.is_finished(), "answered before its record was written");
            t.daemon.disk.let_through();
            let (code, out, err) = asking.join().unwrap();
            assert_eq!(code, 0, "{err}");
            assert!(out.contains(" Exited (3) "), "{out}");
            let on_disk = || {
                let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).ok()?;
                serde_json::from_slice::<Container>(&bytes).ok()
            };
            t.until("the run's end is not recorded", |_| {
                on_disk().is_some_and(|c| c.state == Life::Exited && c.exit_code == Some(3))
            });
        });
    }

    /// A run's end, come while the record of its start is written, is recorded as it stands
    /// once that write is done, with no command asking: the start's write, as the run
    /// stood, leaves the record behind, and the end asked for its own.
    #[test]
    fn a_change_while_its_record_is_written_is_written_after() {
        let t = Test::on("record-after", Held::default());
        t.run(|t| {
            let id = t.create("after");
            t.daemon.disk.holding_writes.store(true, Ordering::SeqCst);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            t.until("the start's record is not being written", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            say(&vm, kind::DONE, &[4]);
            joined(starting.run).unwrap();
            t.daemon.disk.let_through();
            let on_disk = || {
                let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).ok()?;
                serde_json::from_slice::<Container>(&bytes).ok()
            };
            t.until("the run's end is not recorded", |_| {
                on_disk().is_some_and(|c| c.state == Life::Exited && c.exit_code == Some(4))
            });
            t.until("the record is behind still", |d| {
                lock(&d.containers).behind().count() == 0
            });
        });
    }

    /// A detached run's client hears of its start once the start is recorded, or why it
    /// is not, its record behind; and the record is written again once it can be.
    #[test]
    fn a_detached_runs_client_hears_its_start_once_recorded_or_why_not() {
        let t = Test::on("record-detached", Held::default());
        t.run(|t| {
            let id = t.create("detached");
            t.daemon.disk.failing.store(true, Ordering::SeqCst);
            let (client, ours) = UnixStream::pair().unwrap();
            let starting = t.start_with(&id, Some(ours));
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            client.set_read_timeout(Some(PATIENCE)).unwrap();
            let warning = shards_ipc::recv(&client).unwrap().unwrap();
            assert_eq!(warning.kind, kind::ERR);
            let said = String::from_utf8_lossy(&warning.payload).into_owned();
            assert_eq!(
                said,
                format!("WARNING: container {id}: its record is behind: a failing disk\n")
            );
            let end = shards_ipc::recv(&client).unwrap().unwrap();
            assert_eq!((end.kind, end.payload.as_slice()), (kind::END, &[0u8][..]));
            t.daemon.disk.failing.store(false, Ordering::SeqCst);
            // The next command has it written again.
            t.ask(&["ps"]);
            t.until("the record is behind still", |d| {
                lock(&d.containers).behind().count() == 0
            });
            say(&vm, kind::DONE, &[0]);
            joined(starting.run).unwrap();
        });
    }

    /// A template of a test's own, for its daemon to plan warm VMs of: a snapshot to the
    /// daemon, which no VM could restore.
    fn template(dir: &Path) {
        std::fs::create_dir_all(dir.join("g-1")).unwrap();
        std::fs::write(dir.join("current"), b"g-1\n").unwrap();
        std::fs::write(dir.join("g-1").join("state"), b"").unwrap();
        let rootfs = dir.with_extension("erofs");
        std::fs::write(&rootfs, b"").unwrap();
        let origin = serde_json::json!({"rootfs": rootfs, "kernel_digest": "", "init_digest": ""});
        std::fs::write(crate::run::Origin::path(dir).unwrap(), origin.to_string()).unwrap();
    }

    /// A pool forgotten before its refill comes, made before its first claim and so
    /// unclaimed past any keep-alive, is made again by the refill, which gives its VMs
    /// the root filesystem its template records; a template that records none has no warm
    /// VM restore it with whatever its saving VM wrote.
    #[test]
    fn a_pool_made_again_restores_its_templates_root_filesystem() {
        let mut t = Test::new("made-again");
        t.daemon.target = 2;
        t.daemon.warm_max = 8;
        t.run(|t| {
            let dir = t.home.join("template");
            template(&dir);
            let rootfs = dir.with_extension("erofs");
            pool_of(&mut lock(&t.daemon.state), &own(&dir), &rootfs);
            t.daemon.age_pools();
            assert!(
                !lock(&t.daemon.state).pools.contains_key(&own(&dir)),
                "the pool kept"
            );
            let planned =
                t.t.daemon
                    .plan_refill(&mut lock(&t.daemon.state), &own(&dir), true);
            let args = planned.expect("VMs planned").args;
            let mut backing = std::fs::canonicalize(&rootfs).unwrap().into_os_string();
            backing.push(":ro");
            assert!(
                args.windows(2).any(|w| w[0] == "--backing" && w[1] == backing),
                "{args:?}"
            );

            let bare = t.home.join("bare");
            template(&bare);
            std::fs::remove_file(crate::run::Origin::path(&bare).unwrap()).unwrap();
            assert!(
                t.daemon
                    .plan_refill(&mut lock(&t.daemon.state), &own(&bare), true)
                    .is_none()
            );
        });
    }

    /// The warm VMs a refill plans count as starting at once, before any is started, so
    /// that a claim or refill planning meanwhile starts none of them again; outside the
    /// pools' lock, those that cannot start count no more. A pool never claimed from keeps
    /// one (demand.rs).
    #[test]
    fn a_refills_vms_count_as_starting_once_planned() {
        let mut t = Test::new("planned");
        t.daemon.target = 2;
        t.daemon.warm_max = 8;
        t.run(|t| {
            let dir = t.home.join("template");
            template(&dir);
            let planned =
                t.t.daemon
                    .plan_refill(&mut lock(&t.daemon.state), &own(&dir), true);
            let planned = planned.expect("VMs planned");
            assert_eq!(lock(&t.daemon.state).pools[&own(&dir)].starting, 1);
            assert!(
                t.daemon
                    .plan_refill(&mut lock(&t.daemon.state), &own(&dir), true)
                    .is_none(),
                "planned again"
            );
            // The test's daemon names a VM binary that is not there.
            assert!(t.t.daemon.start_planned(t.threads, planned).is_err());
            assert_eq!(lock(&t.daemon.state).pools[&own(&dir)].starting, 0);
        });
    }

    /// Refills are the refiller's, which starts what a pool keeps ahead of its runs, no
    /// more however often it is asked, and makes the spare container.
    #[test]
    fn the_refiller_starts_what_a_pool_keeps_and_the_spare() {
        let mut t = Test::new("refiller");
        t.daemon.target = 2;
        t.daemon.warm_max = 8;
        // A VM that never says it is ready.
        let vm = t.home.join("vm");
        std::fs::write(&vm, b"#!/bin/sh\nexec /bin/sleep 600\n").unwrap();
        std::fs::set_permissions(&vm, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        t.daemon.vm = vm;
        t.run(|t| {
            let dir = t.home.join("template");
            template(&dir);
            for _ in 0..5 {
                t.t.daemon.refill_soon(t.threads, Some(&own(&dir)));
            }
            t.until("the pool's VM started and the spare made", |d| {
                lock(&d.state).starting.len() == 1 && matches!(*lock(&d.spare), Spare::Made(..))
            });
            assert_eq!(lock(&t.daemon.state).pools[&own(&dir)].starting, 1);
        });
    }

    /// A connection a run's command made to its own microVM's engine (D181), which its VM
    /// hands over, is answered as taken (M24), and served scoped to that microVM; one from
    /// a run with no engine of its own is answered too, and closed unserved.
    #[test]
    fn an_engine_connection_is_served_for_a_run_with_its_own_engine_alone() {
        use std::io::{Read as _, Write as _};
        let t = Test::new("engine-conn");
        t.run(|t| {
            let own = OwnEngine {
                token: [7u8; shards_abi::run::TOKEN],
                registry_env: Vec::new(),
            };
            for (name, engine) in [("own", Some(own)), ("none", None)] {
                let id = t.create(name);
                let (ready, vm) = t.warm_vm(None);
                let keep = Keep {
                    detached: None,
                    options: crate::spec::Options::default(),
                    health: None,
                    published: Vec::new(),
                    named: None,
                    restarted: false,
                    layer_pending: false,
                    log_files: DEFAULT_LOGS.files,
                    log_compress: false,
                    visit: false,
                    egress: None,
                    agentfile: None,
                    mac: None,
                    link_local: Vec::new(),
                    engine: engine.clone(),
                };
                let _inbox = t.daemon.register(ready, &id, keep);
                say(&vm, kind::STARTED, &[]);
                let (ours, theirs) = UnixStream::pair().unwrap();
                // Before its far end may close: XNU sets no option on a socket closed both
                // ways (sosetoptlock).
                ours.set_read_timeout(Some(PATIENCE)).unwrap();
                shards_ipc::send(&vm, kind::ENGINE_CONN, &5u64.to_be_bytes(), &[theirs.as_fd()]).unwrap();
                drop(theirs);
                t.daemon.settle();
                assert_eq!(heard(&vm), (kind::ENGINE_CONN, 5u64.to_be_bytes().to_vec()));
                let arrived = t.daemon.engine.take_arrived();
                match engine {
                    Some(_) => {
                        let [(provider, conn)] = <[(String, UnixStream); 1]>::try_from(arrived).unwrap();
                        assert_eq!(provider, id);
                        t.t.daemon
                            .take_engine(t.threads, conn, Some(engine::Scope { provider }));
                        (&ours)
                            .write_all(b"GET /_ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                            .unwrap();
                        let mut answer = String::new();
                        (&ours).read_to_string(&mut answer).unwrap();
                        assert!(
                            answer.starts_with("HTTP/1.1 200 OK\r\n") && answer.ends_with("\r\n\r\nOK"),
                            "{answer}"
                        );
                    }
                    None => {
                        assert!(arrived.is_empty());
                        let mut answer = Vec::new();
                        (&ours).read_to_end(&mut answer).unwrap();
                        assert_eq!(answer, b"");
                    }
                }
                say(&vm, kind::DONE, &[0]);
            }
        });
    }

    /// As the daemon stops, a run hears its container's own stop signal, though the
    /// container's record is not written yet: what it was made with is read where it is.
    #[test]
    fn a_stop_signals_a_run_its_own_way_before_its_record_is_written() {
        let t = Test::on("stop-arriving", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve_with("racer", Some("USR1"), false);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            t.until("the record's write is held", |d| {
                d.disk.writing.load(Ordering::SeqCst)
            });
            t.t.daemon.stop_runs(t.threads);
            // Its VM was told its stop as its run was tracked, its own signal in it, so that
            // it stops its run so should this daemon die (STOP_ORDER); then the stop's
            // signal. Linux numbers SIGUSR1 10.
            let order = shards_ipc::recv(&vm).unwrap().unwrap();
            let grace = u64::try_from(STOP_GRACE.as_millis()).unwrap();
            assert_eq!(
                (order.kind, order.payload),
                (
                    kind::STOP_ORDER,
                    [10u32.to_be_bytes().as_slice(), &grace.to_be_bytes()].concat()
                )
            );
            assert_eq!(heard(&vm), signal(10));
            held.let_through();
            say(&vm, kind::DONE, &[128 + 10]);
            joined(starting.run).unwrap();
        });
    }

    /// A daemon compresses, as it starts, the rotated segments a daemon before it left
    /// as written (D162): of each container whose log compresses them, every segment but
    /// the newest, and one a crash left half compressed; none of any other container.
    #[test]
    fn rotated_segments_left_as_written_are_compressed_at_start() {
        let t = Test::new("pack-left");
        t.run(|t| {
            // As `run` makes them: compressed but with json-file's options.
            let packs = t.create("packs");
            let keeps = t.create("keeps");
            assert!(t.record(&packs).unwrap().log_compress);
            lock(&t.daemon.containers)
                .change(&keeps, |c| c.log_compress = false)
                .unwrap();
            let lines: Vec<u8> = (0..5000)
                .flat_map(|i| format!("line {i}\n").into_bytes())
                .collect();
            for id in [&packs, &keeps] {
                let dir = t.home.join("containers").join(id);
                for seq in 0..3 {
                    let (log, index) = crate::segments::log_segment(seq);
                    std::fs::write(dir.join(log), &lines).unwrap();
                    std::fs::write(dir.join(index), []).unwrap();
                }
                // Half compressed as a daemon ended.
                std::fs::write(dir.join("log.1.zst"), b"half").unwrap();
            }
            t.daemon.pack_left();
            let names = |id: &str| {
                let mut names: Vec<String> = std::fs::read_dir(t.home.join("containers").join(id))
                    .unwrap()
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| n.starts_with("log") && !n.ends_with(".idx"))
                    .collect();
                names.sort();
                names
            };
            assert_eq!(names(&packs), ["log.1.zst", "log.2", "log.zst"]);
            assert_eq!(names(&keeps), ["log", "log.1", "log.1.zst", "log.2"]);
            let dir = shards_vmm::platform::open_dir(&t.home.join("containers").join(&packs)).unwrap();
            for seq in [0, 1] {
                let log = packed::open_log(&dir, seq).unwrap();
                let mut back = vec![0u8; lines.len()];
                log.read_exact_at(&mut back, 0).unwrap();
                assert_eq!(back, lines, "segment {seq}");
            }
        });
    }

    /// The host's filesystem, but its directory syncs wait until the test lets them
    /// through, and while `failing` its renames fail.
    #[derive(Debug, Default)]
    struct Held {
        through: Mutex<bool>,
        turn: std::sync::Condvar,
        syncing: AtomicBool,
        /// Syncs begun.
        syncs: AtomicUsize,
        failing: AtomicBool,
        /// Its writes too wait until let through, while this is set.
        holding_writes: AtomicBool,
        writing: AtomicBool,
        /// Its renames that set a removed container's directory aside (`.ID.removing`)
        /// too wait until let through, while this is set: on a gate of their own, which
        /// [`let_writes_through`](Self::let_writes_through) leaves shut.
        holding_set_asides: AtomicBool,
        setting_aside: AtomicBool,
        /// Its set-asides fail, while this is set.
        failing_set_asides: AtomicBool,
        set_asides_through: Mutex<bool>,
        set_asides_turn: std::sync::Condvar,
    }

    impl Held {
        fn let_through(&self) {
            *lock(&self.set_asides_through) = true;
            self.set_asides_turn.notify_all();
            self.let_writes_through();
        }

        /// Lets its writes and syncs through, not its renames.
        fn let_writes_through(&self) {
            *lock(&self.through) = true;
            self.turn.notify_all();
        }
    }

    impl Disk for Held {
        fn create_dir(&self, dir: &Path) -> io::Result<()> {
            Real.create_dir(dir)
        }
        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            Real.list(dir)
        }
        fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>> {
            Real.read(path, max)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.holding_writes.load(Ordering::SeqCst) {
                self.writing.store(true, Ordering::SeqCst);
                let through = lock(&self.through);
                drop(self.turn.wait_while(through, |t| !*t).unwrap());
            }
            Real.write(path, bytes)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(io::Error::other("a failing disk"));
            }
            let aside = to
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".removing"));
            if aside && self.failing_set_asides.load(Ordering::SeqCst) {
                return Err(io::Error::other("a failing set-aside"));
            }
            if aside && self.holding_set_asides.load(Ordering::SeqCst) {
                self.setting_aside.store(true, Ordering::SeqCst);
                let through = lock(&self.set_asides_through);
                drop(self.set_asides_turn.wait_while(through, |t| !*t).unwrap());
            }
            Real.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            Real.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            Real.remove_dir_all(path)
        }
        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            self.syncing.store(true, Ordering::SeqCst);
            let through = lock(&self.through);
            drop(self.turn.wait_while(through, |t| !*t).unwrap());
            Real.sync_dir(dir)
        }
    }

    /// The name a `--rm` container held is free for the next container as soon as its run
    /// has ended: its end is taken, by the followers or by the next container's making,
    /// and its removal, which the completer makes durable, waited for; as dockerd's name
    /// is free by the time `docker run --rm` returns.
    #[test]
    fn a_rm_containers_name_is_free_once_its_run_ends() {
        let t = Test::new("rm-name");
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let id = t.reserve_with("reused", None, true);
            t.t.daemon.await_arrival(&id);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            say(&vm, kind::DONE, &[0]);
            let again = t.reserve("reused");
            assert_ne!(again, id);
            joined(starting.run).unwrap();
            assert!(t.record(&id).is_none());
        });
    }

    /// A name whose `--rm` holder's run is being handed over is seen through the handoff:
    /// its VM may run the command, say DONE and tell its client before the handoff has
    /// registered the run, and the client's next run may want the name at once. Said in
    /// one write, as the client's next run comes after DONE: the VM says it before it
    /// tells its client (warm.rs). Written apart, the run could be registered between
    /// TAKEN and DONE, when its name is rightly still held.
    #[test]
    fn a_name_held_by_a_run_being_handed_over_is_seen_through_its_handoff() {
        let t = Test::new("rm-handing");
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let id = t.reserve_with("handing", None, true);
            t.t.daemon.await_arrival(&id);
            let starting = t.start(&id);
            let (ready, vm) = t.warm_vm(None);
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            std::thread::scope(|scope| {
                let again = scope.spawn(|| t.reserve("handing"));
                std::thread::sleep(Duration::from_millis(100));
                assert!(!again.is_finished(), "answered before the handoff was through");
                say_together(
                    &vm,
                    &[(kind::TAKEN, &[]), (kind::STARTED, &[]), (kind::DONE, &[0])],
                );
                assert_ne!(again.join().unwrap(), id);
            });
            joined(starting.run).unwrap();
            assert!(t.record(&id).is_none());
        });
    }

    /// `--rm` containers that end together are made durable together: the removals that
    /// queue while one sync waits take one more, for them all.
    #[test]
    fn removals_ending_together_take_one_sync() {
        let t = Test::on("rm-batch", Held::default());
        t.run(|t| {
            t.t.daemon.start_completer(t.threads);
            let runs: Vec<_> = (0..3)
                .map(|i| {
                    let id = t.reserve_with(&format!("batch{i}"), None, true);
                    t.t.daemon.await_arrival(&id);
                    let starting = t.start(&id);
                    let (ready, vm) = t.warm_vm(None);
                    starting.warm.send(ready).unwrap();
                    assert_eq!(heard(&vm).0, kind::RUN);
                    say(&vm, kind::TAKEN, &[]);
                    say(&vm, kind::STARTED, &[]);
                    (starting, vm)
                })
                .collect();
            say(&runs[0].1, kind::DONE, &[0]);
            t.until("the first removal's sync waits", |d| {
                d.disk.syncing.load(Ordering::SeqCst)
            });
            say(&runs[1].1, kind::DONE, &[0]);
            say(&runs[2].1, kind::DONE, &[0]);
            t.until("the others queued", |d| lock(&d.completing.pending).len() == 2);
            t.daemon.disk.let_through();
            for (starting, _) in runs {
                joined(starting.run).unwrap();
            }
            t.until("every name let go", |d| {
                let registry = lock(&d.containers);
                (0..3).all(|i| registry.name_taken(&format!("batch{i}")).is_none())
            });
            assert_eq!(t.daemon.disk.syncs.load(Ordering::SeqCst), 2);
        });
    }

    /// The followers let a run go once it has ended, whoever took its end: the loop, or a
    /// command, after which its VM's end does.
    #[test]
    fn ended_runs_are_followed_no_more() {
        let t = Test::new("followed-no-more");
        t.run(|t| {
            let (_, starting, vm) = running(t, "taken");
            assert_eq!(lock(&t.daemon.followers.runs).len(), 1);
            say(&vm, kind::DONE, &[0]);
            t.t.daemon.settle();
            joined(starting.run).unwrap();
            drop(vm);
            t.until("followed no more", |d| lock(&d.followers.runs).is_empty());
        });
    }

    /// A removed container's name is let go only once its removal is durable: a power
    /// loss before then could bring the container back, next to one that took its name
    /// (audit A15).
    #[test]
    fn a_name_is_let_go_only_once_its_removal_is_durable() {
        let t = Test::on("name-held", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            let id = t.create("racer");
            let removal = lock(&t.daemon.containers).take_out(&id).unwrap();
            t.daemon.set_aside(&removal).unwrap();
            std::thread::scope(|scope| {
                let completing = scope.spawn(|| t.daemon.complete(&removal));
                let deadline = Instant::now() + PATIENCE;
                while !held.syncing.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let syncing = held.syncing.load(Ordering::SeqCst);
                let seen = lock(&t.daemon.containers).get(&id).is_some();
                let holder = lock(&t.daemon.containers)
                    .name_taken("racer")
                    .map(|c| c.id.clone());
                // Through before anything is asserted, so that the scope can end.
                held.let_through();
                assert!(syncing, "the removal was never synced");
                assert!(!seen, "out of sight");
                assert_eq!(
                    holder,
                    Some(id.clone()),
                    "its name was let go before the removal was durable"
                );
                completing.join().unwrap().unwrap();
            });
            assert!(lock(&t.daemon.containers).name_taken("racer").is_none());
            assert!(!t.home.join("containers").join(&id).exists());
        });
    }

    /// A record that could not be written is written again before a command is answered
    /// (audit A15).
    #[test]
    fn a_record_behind_is_written_before_a_command_is_answered() {
        let t = Test::on("behind", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            let id = t.create("racer");
            held.failing.store(true, Ordering::SeqCst);
            let e = lock(&t.daemon.containers).update(&t.daemon.disk, &id, |c| c.exit_code = Some(9));
            assert!(e.is_err());
            held.failing.store(false, Ordering::SeqCst);
            let on_disk = || {
                let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).unwrap();
                serde_json::from_slice::<Container>(&bytes).unwrap().exit_code
            };
            assert_eq!(on_disk(), None, "behind");
            t.daemon.settle();
            assert_eq!(on_disk(), Some(9));
        });
    }

    /// What happens to a container while its record is written is written too, before it
    /// is seen (audit A15).
    #[test]
    fn a_change_while_a_record_is_written_is_written_before_it_is_seen() {
        let t = Test::on("arriving", Held::default());
        t.run(|t| {
            let held = &t.daemon.disk;
            held.holding_writes.store(true, Ordering::SeqCst);
            let id = t.reserve("racer");
            let deadline = Instant::now() + PATIENCE;
            while !held.writing.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "the record was never written");
                std::thread::sleep(Duration::from_millis(1));
            }
            lock(&t.daemon.containers)
                .update(&t.daemon.disk, &id, |c| c.exit_code = Some(5))
                .unwrap();
            assert!(lock(&t.daemon.containers).get(&id).is_none(), "not seen yet");
            held.let_through();
            let deadline = Instant::now() + PATIENCE;
            while lock(&t.daemon.containers).get(&id).is_none() {
                assert!(Instant::now() < deadline, "never seen");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                lock(&t.daemon.containers).get(&id).and_then(|c| c.exit_code),
                Some(5)
            );
            let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).unwrap();
            assert_eq!(
                serde_json::from_slice::<Container>(&bytes).unwrap().exit_code,
                Some(5),
                "its record is the change's"
            );
        });
    }

    /// Makers of the spare container at once leave one spare, and no directory besides
    /// (audit A20).
    #[test]
    fn makers_at_once_leave_one_spare() {
        let t = Test::new("spares");
        t.run(|t| {
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(|| {
                        start.wait();
                        t.daemon.make_spare();
                    });
                }
            });
            assert!(matches!(*lock(&t.daemon.spare), Spare::Made(..)));
            let dirs = std::fs::read_dir(t.home.join("containers")).unwrap().count();
            assert_eq!(dirs, 1, "spares made to be dropped");
            let (id, _log) = t.daemon.new_container().unwrap();
            assert!(t.home.join("containers").join(&id).is_dir(), "the spare taken");
            assert!(matches!(*lock(&t.daemon.spare), Spare::None));
        });
    }

    /// Output a run's log could not keep is said by `logs`, which fails: the log is not
    /// all of it; and a log that is gone is said too, not shown as nothing (audit A12).
    #[test]
    fn logs_say_what_they_do_not_hold() {
        let t = Test::new("lost");
        t.run(|t| {
            let (id, starting, vm) = running(t, "racer");
            say(&vm, kind::LOST, &7u64.to_be_bytes());
            say(&vm, kind::DONE, &[0]);
            drop(vm);
            let _ = starting.run.join();
            let (status, _, err) = ask(&t.daemon, &["logs", "racer"]);
            assert_eq!(status, 1, "{err}");
            assert!(err.contains("7 bytes of racer's output"), "{err}");
            assert_eq!(lock(&t.daemon.containers).get(&id).map(|c| c.log_lost), Some(7));

            let gone = t.create("gone");
            std::fs::remove_file(lock(&t.daemon.containers).dir(&gone).join(logs::LOG)).unwrap();
            let (status, _, err) = ask(&t.daemon, &["logs", "gone"]);
            assert_eq!(status, 1, "{err}");
            assert!(err.contains("its log"), "{err}");
        });
    }

    /// The daemon exits once the files its runs asked for are made, those queued and the
    /// one being made: a template's working set queued as it stops is written, not lost
    /// with it (PM M98).
    #[test]
    fn the_files_asked_for_are_made_before_the_daemon_exits() {
        let t = Test::new("files-drained");
        t.run(|t| {
            t.t.daemon.start_files(t.threads);
            let (free_first, first) = mpsc::channel();
            let (free_last, last) = mpsc::channel();
            let (ours, vm) = UnixStream::pair().unwrap();
            vm.set_read_timeout(Some(PATIENCE)).unwrap();
            let dir = t.home.join("containers").join("made");
            std::fs::create_dir_all(&dir).unwrap();
            t.daemon.make_soon(files::Job::Hold(first));
            t.daemon.make_soon(files::Job::Segment {
                id: "made".into(),
                dir: dir.clone(),
                seq: 1,
                files: DEFAULT_LOGS.files,
                pack: false,
                socket: Arc::new(RunSocket::new(ours)),
            });
            t.daemon.make_soon(files::Job::Hold(last));
            let (finished, waited) = mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    t.daemon.finish();
                    finished.send(()).unwrap();
                });
                let queued = waited.recv_timeout(Duration::from_millis(200));
                free_first.send(()).unwrap();
                t.until("the last job was not taken", |d| d.files.queued() == 0);
                let working = waited.recv_timeout(Duration::from_millis(200));
                free_last.send(()).unwrap();
                assert!(queued.is_err(), "done with jobs queued");
                assert!(working.is_err(), "done with a job being made");
                waited.recv_timeout(PATIENCE).unwrap();
            });
            assert!(dir.join(crate::segments::log_segment(1).0).is_file());
            let m = heard_whole(&vm);
            assert_eq!((m.kind, m.fds.len()), (kind::SEGMENT, 2));
        });
    }

    /// The files a run's VM asks for are made on the files' thread, never on the
    /// followers' loop (PM M98): while that thread is busy, the loop goes on taking the
    /// run's messages, its end among them, and leaves the segment and the working set it
    /// asked for queued, the first set alone; a segment asked for out of turn is refused
    /// at once.
    #[test]
    fn a_runs_files_are_made_off_the_followers_loop() {
        let t = Test::new("files-off-loop");
        t.run(|t| {
            let id = t.create("chatty");
            let starting = t.start(&id);
            let (mut ready, vm) = t.warm_vm(None);
            ready.records = Some(Records {
                dir: t.home.join("template"),
                limit: 1 << 20,
            });
            starting.warm.send(ready).unwrap();
            assert_eq!(heard(&vm).0, kind::RUN);
            say(&vm, kind::TAKEN, &[]);
            say(&vm, kind::STARTED, &[]);
            // Following the run started the files' thread.
            t.until("the files' thread was not started", |d| d.files.started());
            let (free, held) = mpsc::channel();
            t.daemon.make_soon(files::Job::Hold(held));
            // Taken, so that what is queued after is the run's alone: a thread a loaded host
            // had yet to run left it queued, and counted (CI, 77cefa9's aarch64 glibc run).
            t.until("the files' thread did not take its hold", |d| {
                d.files.queued() == 0
            });
            say(&vm, kind::LOG_SEGMENT, &1u64.to_be_bytes());
            say(&vm, kind::LOG_SEGMENT, &5u64.to_be_bytes());
            // Twice: one set is taken of a run, the first.
            for _ in 0..2 {
                for part in working_set_parts("g-1", &[7; 64]) {
                    say(&vm, kind::WORKING_SET, &part);
                }
            }
            let refused = heard_whole(&vm);
            say(&vm, kind::DONE, &[0]);
            let ended = finishes(&starting.run);
            let queued = t.daemon.files.queued();
            free.send(()).unwrap();
            assert_eq!(
                (refused.kind, refused.payload.as_slice(), refused.fds.len()),
                (kind::SEGMENT, &5u64.to_be_bytes()[..], 0)
            );
            assert!(ended, "its end waited for its files");
            assert_eq!(queued, 2, "its files made on the loop");
            joined(starting.run).unwrap();
            let made = shards_ipc::recv(&vm).unwrap().unwrap();
            assert_eq!(
                (made.kind, made.payload.as_slice(), made.fds.len()),
                (kind::SEGMENT, &1u64.to_be_bytes()[..], 2)
            );
            assert!(
                t.home
                    .join("containers")
                    .join(&id)
                    .join(crate::segments::log_segment(1).0)
                    .is_file()
            );
        });
    }

    /// A run's VM is given each log segment it asks for, in turn, made in its container,
    /// and nothing for one out of turn; past the retention the oldest goes. No VM reaches a
    /// container's directory (D30): the daemon makes its segments.
    #[test]
    fn a_runs_log_segments_are_made_in_turn() {
        use std::io::Write as _;
        let t = Test::new("segments");
        t.run(|t| {
            let (id, starting, vm) = running(t, "rotating");
            let dir = lock(&t.daemon.containers).dir(&id);
            vm.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let ask = |seq: u64| {
                say(&vm, kind::LOG_SEGMENT, &seq.to_be_bytes());
                let m = heard_whole(&vm);
                assert_eq!(m.kind, kind::SEGMENT);
                assert_eq!(m.payload, seq.to_be_bytes(), "the answer names its segment");
                m.fds
            };
            assert!(ask(2).is_empty(), "segment 2 before 1");
            let files = t.daemon.logs.files;
            for seq in 1..=files + 1 {
                let fds = ask(seq);
                assert_eq!(fds.len(), 2, "segment {seq}");
                let mut fds = fds.into_iter();
                let (mut log, mut index) = (File::from(fds.next().unwrap()), File::from(fds.next().unwrap()));
                log.write_all(b"record").unwrap();
                index.write_all(&[0; 8]).unwrap();
                let (l, i) = log_segment(seq);
                assert_eq!(
                    std::fs::read(dir.join(l)).unwrap(),
                    b"record",
                    "segment {seq}'s log"
                );
                assert_eq!(
                    std::fs::read(dir.join(i)).unwrap(),
                    [0; 8],
                    "segment {seq}'s index"
                );
                assert!(ask(seq).is_empty(), "segment {seq} again");
            }
            let (l, i) = log_segment(1);
            assert!(
                !dir.join(l).exists() && !dir.join(i).exists(),
                "the oldest past the retention"
            );
            say(&vm, kind::DONE, &[0]);
            drop(vm);
            let _ = starting.run.join();
        });
    }
}
