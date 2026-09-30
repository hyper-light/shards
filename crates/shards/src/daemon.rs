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
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use shards_ipc::{Identity, Run, kind};
use shards_registry::http::Cancel;
use shards_vmm::vm::Config;

use crate::containers::{self, Container, Registry, Removal, State as Life};

mod commands;
mod demand;
mod logs;
use crate::run::{Boot, Prepared};
use crate::spec::{LogRetention, NOT_RUN, log_segment};

const USAGE: &str = "usage: shards daemon [--detached | stop]
  Serves `shards run` from warm microVMs; `shards run` starts one when none is running.
  It exits after SHARDS_DAEMON_IDLE seconds (default 900) without a run.
  --detached: run it in the background, writing messages to daemon.log in SHARDS_HOME, as
    `shards run` starts it.
  stop: have the running daemon end its runs, as dockerd ends containers, and exit.
  SHARDS_POOL: the most warm microVMs kept for each image (default 2): as many as its
    runs have come at once, while it is used.
  SHARDS_POOL_KEEP: seconds an image's warm microVMs are kept after its last run (600).";

/// How long a VM may take to be ready: a restore takes milliseconds, a boot that saves a
/// template tens of them.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// A template whose warm VMs fail this many times in a row is removed and saved again.
const MAX_FAILURES: u32 = 3;
const DEFAULT_POOL: usize = 2;
/// Warm VMs kept ahead of runs, all pools together, unless `SHARDS_WARM_MAX` says (audit
/// A13): a warm VM's own memory is 3.4 MiB, the rest of its RSS its template's pages it
/// shares, so the default holds about 55 MiB (PM M49).
const DEFAULT_WARM_MAX: usize = 16;
/// The most `SHARDS_WARM_MAX` may be: each warm VM holds a thread and descriptors of the
/// daemon, whose clients are capped at `MAX_CLIENTS` for the same reason.
const MAX_WARM_MAX: usize = MAX_CLIENTS;
const DEFAULT_IDLE: Duration = Duration::from_secs(900);
/// How long a pool keeps warm VMs after its last claim unless `SHARDS_POOL_KEEP` says: as
/// AWS keeps an idle function, 10 minutes (Shahrad et al., "Serverless in the Wild",
/// USENIX ATC 2020, §1).
const DEFAULT_KEEP: Duration = Duration::from_secs(600);
/// Warm VMs a run may try: one can end while it waits, or before it has taken the run.
const HANDOFF_TRIES: usize = 3;
/// How long a warm VM may take to say it has taken a run: it does so right after it
/// receives one.
const TAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a daemon waits for the lock of one that is exiting: one ending its runs takes
/// up to STOP_GRACE + SHUTDOWN_KILL, then its VMs end.
const TAKEOVER: Duration = Duration::from_secs(20);
/// How long `shards daemon stop` lets a command end after its SIGTERM, before SIGKILL:
/// dockerd's default stop timeout (moby daemon/config/config_linux.go), which it gives
/// each container when it shuts down.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long a shutting-down daemon lets a command take to end after that SIGKILL, before
/// its VM goes too: dockerd gives up on its containers after the larger of its shutdown
/// timeout (15 s) and the stop timeout plus 5 s (moby daemon/daemon.go, ShutdownTimeout).
const SHUTDOWN_KILL: Duration = Duration::from_secs(5);
/// How often the daemon looks at its clock when no client arrives.
const TICK: libc::c_int = 250;
/// [`TICK`], as a duration: how often a waiting command looks at its client.
const TICK_TIME: Duration = Duration::from_millis(TICK.unsigned_abs() as u64);
/// How long a client may take to send its whole request (audit A07). One sends it as it
/// connects; one that has not by now is broken, or trickling it out.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Clients in hand at once (audit A07). Each holds a thread and up to six descriptors
/// (its connection, its stdio, its container's log and a VM's socket) until its run is
/// handed over or its command answered; beyond this many, connections wait in the
/// listener's backlog.
const MAX_CLIENTS: usize = 256;

pub fn daemon(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.collect();
    let arg = |i: usize| args.get(i).and_then(|a| a.to_str());
    let result = match (args.len(), arg(0)) {
        (0, _) => serve(),
        (1, Some("--detached")) => detach(),
        (1, Some("-h" | "--help")) => {
            let _ = writeln!(io::stdout(), "{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => Err(format!("unexpected arguments {args:?}\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {message}");
    ExitCode::FAILURE
}

/// A message in the daemon's log.
fn log(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr(), "shards daemon {}: {message}", std::process::id());
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A warm VM waiting for a run, and the socket it takes the run on.
struct Ready {
    vm: Arc<shards_ipc::Child>,
    socket: UnixStream,
    /// The template whose pool it came from.
    pool: Option<PathBuf>,
}

/// The warm VMs of one template.
#[derive(Default)]
struct Pool {
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

#[derive(Default)]
struct State {
    pools: HashMap<PathBuf, Pool>,
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
    Pool(PathBuf),
    Run(mpsc::Sender<Result<Ready, String>>),
}

/// A run, from its container's creation to its end (audit A06): what the container
/// commands and the daemon's shutdown consult, under `runs`' lock.
enum RunState {
    /// Created and not yet handed to a warm VM: `rm` may cancel it, and then it never
    /// starts.
    Pending { cancelled: bool },
    /// Being handed to a warm VM: it starts or fails, and commands wait to see which.
    Handing,
    /// Handed over, and followed until it ends.
    Tracked(Tracked),
}

/// A run in progress: its VM's socket, to signal the command, and the VM itself.
struct Tracked {
    socket: Arc<UnixStream>,
    vm: Arc<shards_ipc::Child>,
    inbox: Arc<Mutex<Inbox>>,
}

/// What a run has told the daemon, and the socket it tells it on: read under this lock
/// alone, by the run's own thread as messages come (`follow`), and by every container
/// command before it answers (`settle`). A run tells the daemon of its start and end
/// before its client learns of them, so a command sees what any client has seen.
struct Inbox {
    socket: Arc<UnixStream>,
    /// The warm VM's process ID, for the log.
    pid: u32,
    started: bool,
    /// A detached run's client, until the daemon tells it whether its command started.
    detached: Option<UnixStream>,
    ended: bool,
}

/// One waiting for a container to end ([`Daemon::await_exit`]), by its number.
struct Waiter {
    number: u64,
    tell: mpsc::Sender<u8>,
    /// Written to as it is told, for one that waits in poll(2): `logs -f`.
    wake: Option<UnixStream>,
}

impl Waiter {
    /// Tells it the container's exit code.
    fn hear(&self, code: u8) {
        let _ = self.tell.send(code);
        if let Some(wake) = &self.wake {
            let _ = (&*wake).write_all(&[code]);
        }
    }
}

/// The runs handed over, of `runs`.
fn tracked(runs: &HashMap<String, RunState>) -> impl Iterator<Item = &Tracked> {
    runs.values().filter_map(|r| match r {
        RunState::Tracked(t) => Some(t),
        _ => None,
    })
}

/// Why a template's pool gave no warm VM.
enum Claim {
    /// Its warm VMs keep failing: the template does not restore.
    Broken,
    Failed(String),
}

struct Daemon {
    home: PathBuf,
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
    /// Clients connected and not yet handed over.
    busy: AtomicUsize,
    /// The connections of clients in hand whose threads the daemon's shutdown ends, by
    /// number: those still sending their request, and those of container commands. A run's
    /// connection is its command's once its request is read, and leaves here then.
    clients: Mutex<HashMap<u64, Arc<UnixStream>>>,
    next_client: AtomicU64,
    /// A client left: another may be taken.
    admitted: Condvar,
    /// What runs still being prepared are downloading, by client: a shutdown cancels it.
    preparing: Mutex<HashMap<u64, Cancel>>,
    last: Mutex<Instant>,
    stopping: AtomicBool,
    /// The socket is gone: no client arrives from here on.
    closed: AtomicBool,
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
    /// A reserved container was let be seen.
    arrived: Condvar,
    /// Where reserved containers' records go to be written, in order, by one thread, once
    /// the first run has started it.
    recorder: Mutex<Option<mpsc::Sender<String>>>,
    /// Who waits for each running container to end, for its exit code: `shards wait`,
    /// `stop`, `kill`, `rm -f`. Told under the containers' lock, as the record changes.
    /// Each waiter has a number, by which it goes if it stops waiting first.
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    next_waiter: AtomicU64,
    /// The containers `shards rm` is removing.
    removing: Mutex<HashSet<String>>,
    /// A container's ID, directory and log, made ahead of the run that takes them.
    spare: Mutex<Spare>,
    /// Numbers the templates a run saves before they become the template.
    saved: AtomicU64,
    /// A collection is due: at start, and once a pull has moved a reference (audit A13).
    collect: AtomicBool,
    /// The home's lock, held while this daemon lives.
    home_lock: File,
}

/// A client in hand, by its number: counted until its run is handed over or refused, or
/// its command answered.
struct Busy<'a>(&'a Daemon, u64);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        *lock(&self.0.last) = Instant::now();
        // Under the lock the listener waits with, so its wakeup is not lost.
        let mut clients = lock(&self.0.clients);
        clients.remove(&self.1);
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
        drop(clients);
        self.0.admitted.notify_all();
    }
}

/// Starts the daemon in the background: in a session of its own, writing to the log in
/// its home, and orphaned as this process exits, so that init adopts it (or the nearest
/// subreaper) and reaps it when it exits. A daemon left the child of the `shards run` that
/// started it would stay a zombie after it exits, for as long as that client lives
/// (APUE 13.3; XNU proc_exit reparents orphans to launchd).
fn detach() -> Result<(), String> {
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
    shards_ipc::spawn(
        &exe,
        &["daemon".as_ref()],
        &[(null.as_fd(), 0), (log.as_fd(), 1), (log.as_fd(), 2)],
        true,
    )
    .map(drop)
    .map_err(|e| format!("starting the daemon {}: {e}", exe.display()))
}

/// Raises this process's soft limit on open descriptors to its hard limit, as Go's runtime
/// raises its own (go1.25.0 src/syscall/rlimit.go, after go.dev/issue/46279): the daemon
/// holds a socket for each warm VM and each run, and each client in hand holds several
/// more, where macOS starts a process with a soft limit of 256. macOS refuses more than
/// `kern.maxfilesperproc` (src/syscall/rlimit_darwin.go). The hard limit is for whoever
/// starts the daemon to set. Returns the limit it has.
fn raise_descriptor_limit() -> Option<libc::rlim_t> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return None;
    }
    if lim.rlim_cur >= lim.rlim_max {
        return Some(lim.rlim_cur);
    }
    let raised = lim.rlim_max;
    #[cfg(target_os = "macos")]
    let raised = raised.min(max_files_per_process().unwrap_or(raised));
    if raised > lim.rlim_cur {
        let before = lim.rlim_cur;
        lim.rlim_cur = raised;
        // SAFETY: setrlimit(2) from a local.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
            log(format!(
                "raising the descriptor limit to {raised}: {}",
                io::Error::last_os_error()
            ));
            return Some(before);
        }
    }
    Some(lim.rlim_cur)
}

/// `kern.maxfilesperproc`: how many descriptors macOS lets one process open.
#[cfg(target_os = "macos")]
fn max_files_per_process() -> Option<libc::rlim_t> {
    let mut per_process: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname(3) reading one int into a local of its size.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut per_process).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    libc::rlim_t::try_from(per_process).ok()
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
}

/// The daemon's settings, checked before it serves: a malformed or excessive one stops it
/// (audit A14).
fn settings() -> Result<Settings, String> {
    let as_usize = |name: &str, n: u64| usize::try_from(n).map_err(|_| format!("{name}: {n} is too many"));
    let warm_max = as_usize(
        "SHARDS_WARM_MAX",
        count("SHARDS_WARM_MAX", DEFAULT_WARM_MAX as u64)?,
    )?;
    if warm_max > MAX_WARM_MAX {
        return Err(format!("SHARDS_WARM_MAX: {warm_max} is more than {MAX_WARM_MAX}"));
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
    })
}

fn serve() -> Result<(), String> {
    let settings = settings()?;
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
    // Its daemon is gone, since this one holds the lock.
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", home.join(socket).display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("{}: {e}", home.join(socket).display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let identity = Identity::of_build(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let vm = shards_ipc::vm_binary(&exe);
    let containers = Registry::open(&home, &mut |note| log(note))
        .map_err(|e| format!("{}: {e}", home.join("containers").display()))?;
    let daemon = Arc::new(Daemon::new(home, vm, identity, settings, containers, home_lock));
    daemon.make_spare();
    log(format!(
        "serving {} on {}, with up to {} descriptors open",
        daemon.home.display(),
        daemon.home.join(daemon.socket).display(),
        descriptors.map_or_else(|| "an unknown number of".to_string(), |n| n.to_string())
    ));
    daemon.listen(listener);
    Ok(())
}

/// Whether this process could open another descriptor now: one duplicated and closed.
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

/// Whether `socket` has something to read now: a message, or its end.
fn readable(socket: &UnixStream) -> bool {
    let mut pfd = libc::pollfd {
        fd: socket.as_raw_fd(),
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
    let deadline = Instant::now() + TAKEOVER;
    loop {
        // SAFETY: flock(2) on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("{}: {e}", path.display()));
        }
        if UnixStream::connect(socket).is_ok() || Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Daemon {
    /// A daemon of `home`, whose lock it holds, serving no run yet.
    fn new(
        home: PathBuf,
        vm: PathBuf,
        identity: Identity,
        settings: Settings,
        containers: Registry,
        home_lock: File,
    ) -> Daemon {
        Daemon {
            home,
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
            clients: Mutex::default(),
            next_client: AtomicU64::new(0),
            admitted: Condvar::new(),
            preparing: Mutex::default(),
            last: Mutex::new(Instant::now()),
            stopping: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            stoppers: Mutex::default(),
            runs: Mutex::default(),
            resolved: Condvar::new(),
            ending: AtomicBool::new(false),
            containers: Mutex::new(containers),
            arrived: Condvar::new(),
            recorder: Mutex::default(),
            waiters: Mutex::default(),
            next_waiter: AtomicU64::new(0),
            removing: Mutex::default(),
            spare: Mutex::default(),
            saved: AtomicU64::new(0),
            collect: AtomicBool::new(true),
            home_lock,
        }
    }

    /// Removes the socket, once: no client arrives from here on.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = std::fs::remove_file(self.socket);
        }
    }

    /// Accepts clients until it is time to exit: when stopped, or idle for `idle`. Then it
    /// removes the socket, so no client arrives after, serves any that already did, and
    /// exits.
    fn listen(self: &Arc<Self>, mut listener: UnixListener) {
        // Out of descriptors, since when: said once, not every tick.
        let mut starved_since: Option<Instant> = None;
        loop {
            let closing = self.closed.load(Ordering::SeqCst);
            // A daemon whose home is gone has nothing left to serve.
            if !closing && std::fs::symlink_metadata(&self.home).is_err() {
                log(format!("{} is gone", self.home.display()));
                self.stopping.store(true, Ordering::SeqCst);
            }
            // Someone may remove the socket while the daemon runs.
            if !closing && std::fs::symlink_metadata(self.socket).is_err() {
                match UnixListener::bind(self.socket).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
                    Ok(l) => {
                        log(format!(
                            "{} was removed; listening there again",
                            self.home.join(self.socket).display()
                        ));
                        listener = l;
                    }
                    Err(e) => log(format!("{}: {e}", self.home.join(self.socket).display())),
                }
            }
            let mut starved = false;
            while self.busy.load(Ordering::SeqCst) < MAX_CLIENTS {
                // Once starved, it accepts only when a descriptor is free: an accept that
                // fails for want of one leaves the client queued on Linux (net/socket.c,
                // __sys_accept4_file) but drops it on macOS (xnu-11417.101.15
                // bsd/kern/uipc_syscalls.c, accept_nocancel).
                if starved_since.is_some() && !descriptor_free(&listener) {
                    starved = true;
                    break;
                }
                match listener.accept() {
                    Ok((conn, _)) => {
                        if let Some(since) = starved_since.take() {
                            log(format!("accepting again, after {:?}", since.elapsed()));
                        }
                        self.take(conn);
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
            // A collection opens files: not while clients wait for descriptors, whose
            // accept macOS drops if one is taken meanwhile. On this thread, so no accept
            // runs beside it: a client arriving waits in the backlog.
            if starved_since.is_none() {
                self.collect_if_due();
            }
            let quiet = self.busy.load(Ordering::SeqCst) == 0 && lock(&self.runs).is_empty();
            let idle = quiet && lock(&self.last).elapsed() >= self.idle;
            if !self.closed.load(Ordering::SeqCst) && (self.stopping.load(Ordering::SeqCst) || idle) {
                self.close();
                // A client may have connected before the socket went.
                continue;
            }
            if self.closed.load(Ordering::SeqCst) && quiet {
                self.exit();
            }
            // At the cap, or starved, the listener would be readable at once: wait for a
            // client to leave instead, or a tick.
            let clients = lock(&self.clients);
            if starved || self.busy.load(Ordering::SeqCst) >= MAX_CLIENTS {
                drop(
                    self.admitted
                        .wait_timeout(clients, TICK_TIME)
                        .unwrap_or_else(PoisonError::into_inner),
                );
                continue;
            }
            drop(clients);
            let mut pfd = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) on one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, TICK) };
        }
    }

    fn take(self: &Arc<Self>, conn: UnixStream) {
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
        lock(&self.clients).insert(number, conn.clone());
        self.busy.fetch_add(1, Ordering::SeqCst);
        let daemon = self.clone();
        let spawned = std::thread::Builder::new().name("run".into()).spawn(move || {
            // A run handed over is registered before the client stops counting as busy,
            // so shutdown never sees neither (audit A06).
            let handed = {
                let _busy = Busy(&daemon, number);
                daemon.handle(conn, number)
            };
            // The client's descriptors are closed by now: its run goes on in the VM.
            if let Some((id, inbox)) = handed {
                daemon.follow(&id, &inbox);
            }
        });
        if let Err(e) = spawned {
            lock(&self.clients).remove(&number);
            self.busy.fetch_sub(1, Ordering::SeqCst);
            log(format!("a client's thread: {e}"));
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
        for conn in lock(&self.clients).values() {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        for cancel in lock(&self.preparing).values() {
            cancel.cancel();
        }
    }

    /// Ends the VMs still waiting, lets go of the home, and exits. `shards daemon stop`
    /// learns of it from its connection closing, which is done here, after the rest: the
    /// kernel would close it on exit, but in no order this could rely on (XNU closes a
    /// process's descriptors from the highest down, kern_descrip.c fdt_invalidate).
    fn exit(&self) -> ! {
        let _ = std::fs::remove_file(self.home.join("daemon.pid"));
        let state = lock(&self.state);
        let waiting = state.pools.values().flat_map(|p| p.ready.iter().map(|r| &r.vm));
        for vm in waiting.chain(state.starting.values()) {
            let _ = vm.kill(libc::SIGTERM);
        }
        log("exiting");
        // SAFETY: flock(2) on the lock's own descriptor.
        unsafe { libc::flock(self.home_lock.as_raw_fd(), libc::LOCK_UN) };
        lock(&self.stoppers).clear();
        std::process::exit(0)
    }

    /// Serves one client's request. A run it hands to a warm VM, registered, comes back
    /// with its container's ID, for the caller to follow once the client's descriptors
    /// here are closed.
    fn handle(self: &Arc<Self>, conn: Arc<UnixStream>, number: u64) -> Option<(String, Arc<Mutex<Inbox>>)> {
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
        match message.kind {
            // Its connection is its run's from here, and passes to the VM.
            kind::START => self.release_client(number),
            kind::STOP => {
                self.release_client(number);
                match conn.try_clone() {
                    Ok(held) => lock(&self.stoppers).push(held),
                    Err(e) => log(format!("holding a stopper's connection: {e}")),
                }
                self.step_aside();
                return None;
            }
            kind::CONTAINER => {
                let Some(command) = shards_ipc::Command::decode(&message.payload) else {
                    log("a malformed container command");
                    return None;
                };
                if command.daemon != self.identity {
                    self.release_client(number);
                    self.step_aside();
                    let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
                    return None;
                }
                let asker = commands::Asker {
                    east_asian: command.east_asian,
                    now: command.now,
                    utc_offset: command.utc_offset,
                };
                let status = self.command(&command.argv, &asker, &commands::Reply(conn));
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
            self.step_aside();
            let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
            return None;
        }
        // The container's ID first: it names the command's host unless the run does
        // (moby daemon/container.go).
        let (id, container_log) = match self.new_container() {
            Ok(new) => new,
            Err(e) => {
                refuse(&e);
                return None;
            }
        };
        if run.hostname.is_none() {
            run.hostname = Some(id.get(..12).unwrap_or(&id).to_string());
        }
        // The next run's spare is made once this one is answered, off its path. What it
        // downloads, a shutdown cancels: registered, then checked, so a shutdown either
        // finds it or came before (`end_clients`).
        let cancel = Cancel::new();
        lock(&self.preparing).insert(number, cancel.clone());
        if self.stopping.load(Ordering::SeqCst) {
            cancel.cancel();
        }
        let prepared = crate::run::prepare(&run, &self.home, &say, &cancel);
        lock(&self.preparing).remove(&number);
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                refuse(if cancel.is_cancelled() {
                    "the daemon is shutting down"
                } else {
                    &e
                });
                self.discard(&id);
                self.make_spare();
                return None;
            }
        };
        // The run's container, before anything starts: its name must be free. Its record
        // is written while a VM is found for it.
        if let Err(e) = self.create(&run, &prepared, &id) {
            refuse(&e);
            self.discard(&id);
            self.make_spare();
            return None;
        }
        self.record_arrival(&id);
        // `docker run -d` prints the ID once the container exists, before it starts.
        if run.detach {
            self.await_arrival(&id);
            let _ = shards_ipc::send(conn, kind::OUT, format!("{id}\n").as_bytes(), &[]);
        }
        let mut flags = shards_ipc::RUN_LOG;
        if prepared.interactive {
            flags |= shards_ipc::RUN_INTERACTIVE;
        }
        if run.timing {
            flags |= shards_ipc::RUN_TIMING;
        }
        // A detached run's output goes only to its log; it reads nothing.
        let mut fds = if run.detach {
            flags |= shards_ipc::RUN_DETACHED;
            vec![stdin.as_fd()]
        } else {
            vec![conn.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()]
        };
        fds.push(container_log.dir.as_fd());
        let mut payload = vec![flags];
        payload.extend(self.logs.size.to_be_bytes());
        payload.extend(self.logs.files.to_be_bytes());
        payload.extend(prepared.spec.encode());
        let detached = run.detach.then_some(conn);
        let started = self.start_run(&id, &payload, &fds, detached, || self.warm_for(&prepared, &say));
        // Its VM has its root filesystem, or never will.
        drop(prepared.lease.take());
        match started {
            Ok(inbox) => Some((id, inbox)),
            Err(said) => {
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
    fn start_run(
        self: &Arc<Self>,
        id: &str,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
        detached: Option<&UnixStream>,
        mut acquire: impl FnMut() -> Result<Ready, String>,
    ) -> Result<Arc<Mutex<Inbox>>, String> {
        for _ in 0..HANDOFF_TRIES {
            let ready = acquire().map_err(|e| self.not_started(id, &e))?;
            if let Err(said) = self.commit(id) {
                self.give_back(ready);
                return Err(said);
            }
            let handed = hand_over(&ready.socket, payload, fds);
            // Only now, so that starting its successor delays no run, and on a thread of
            // its own, so that the run's own messages are read as they come (`follow`).
            self.replace(ready.pool.clone());
            match handed {
                // The warm VM serves the client from here, and ours close. A detached
                // client waits for the daemon to say whether its command started.
                Ok(()) => return Ok(self.register(ready, id, detached)),
                Err(Untaken::Surely(e)) => {
                    log(format!("warm VM {} did not take a run: {e}", ready.vm.id()));
                    let _ = ready.vm.kill(libc::SIGKILL);
                    self.uncommit(id);
                }
                // What it sent before it ended tells what it did, as for any run.
                Err(Untaken::Unknown(e)) => {
                    log(format!(
                        "warm VM {} may have taken a run: {e}; ending it",
                        ready.vm.id()
                    ));
                    let _ = ready.vm.kill(libc::SIGKILL);
                    return Ok(self.register(ready, id, detached));
                }
            }
        }
        Err(self.not_started(id, "no warm VM took the run"))
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

    /// Removes what was made for container `id`, which will not be created.
    fn discard(&self, id: &str) {
        let dir = lock(&self.containers).dir(id);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            log(format!("{}: {e}", dir.display()));
        }
    }

    /// Container `id` of `run`, with the name the run gave, or one made for it, as dockerd
    /// names containers (moby daemon/names.go). Reserved, its name held, and seen once its
    /// record is written ([`record_arrival`](Self::record_arrival)), so that it outlives a
    /// crash of the daemon (audit A15).
    fn create(&self, run: &Run, prepared: &Prepared, id: &str) -> Result<(), String> {
        let mut registry = lock(&self.containers);
        let name = match &run.name {
            Some(given) => {
                if !containers::valid_name(given) {
                    return Err(format!(
                        "Invalid container name ({given}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
                    ));
                }
                let name = given.strip_prefix('/').unwrap_or(given);
                if let Some(holder) = registry.name_taken(name) {
                    return Err(format!(
                        "Conflict. The container name \"/{name}\" is already in use by container \"{}\". You have to remove (or rename) that container to be able to reuse that name.",
                        holder.id
                    ));
                }
                name.to_string()
            }
            None => crate::names::generate(id, |name| registry.name_taken(name).is_some())
                .map_err(|e| format!("a container name: {e}"))?,
        };
        registry.reserve(Container {
            id: id.to_string(),
            name,
            image: run.image.clone(),
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
            auto_remove: run.remove,
            log_lost: 0,
        });
        // Its run is owned from the moment the container is visible.
        lock(&self.runs).insert(id.to_string(), RunState::Pending { cancelled: false });
        Ok(())
    }

    /// Has the record of reserved container `id` written, on the recorder's thread, started
    /// here if it is not yet; here, if it cannot be.
    fn record_arrival(self: &Arc<Self>, id: &str) {
        let mut recorder = lock(&self.recorder);
        if recorder.is_none() {
            let (send, receive) = mpsc::channel::<String>();
            let daemon = Arc::downgrade(self);
            let spawned = std::thread::Builder::new()
                .name("recorder".into())
                .spawn(move || {
                    for id in receive {
                        match daemon.upgrade() {
                            Some(daemon) => daemon.arrive(&id),
                            None => return,
                        }
                    }
                });
            match spawned {
                Ok(_) => *recorder = Some(send),
                Err(e) => log(format!("the recorder's thread: {e}; recording here")),
            }
        }
        let sent = recorder.as_ref().is_some_and(|r| r.send(id.to_string()).is_ok());
        drop(recorder);
        if !sent {
            self.arrive(id);
        }
    }

    /// Writes the record of reserved container `id`, again while it changes as it is
    /// written, then lets it be seen. One whose record cannot be written is seen, its
    /// record behind: it exists, and its run may have started.
    fn arrive(&self, id: &str) {
        loop {
            let Some((recorder, c)) = lock(&self.containers).arrival(id) else {
                return;
            };
            let written = recorder.write(&c);
            let mut registry = lock(&self.containers);
            let seen = match written {
                Ok(()) => registry.admit(id, &c),
                Err(e) => {
                    log(format!("container {id}: its record is behind: {e}"));
                    registry.admit_behind(id);
                    true
                }
            };
            drop(registry);
            if seen {
                self.arrived.notify_all();
                return;
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

    /// Starts the successor of a warm VM of `pool` that has just taken a run, and the next
    /// run's spare container, on a thread of their own: a spawn and a file take
    /// milliseconds that the run's start and end should not wait for.
    fn replace(self: &Arc<Self>, pool: Option<PathBuf>) {
        let refill = |daemon: &Arc<Self>, pool: Option<&Path>| {
            if let Some(dir) = pool {
                daemon.refill(&mut lock(&daemon.state), dir);
            }
            daemon.make_spare();
        };
        let (daemon, theirs) = (self.clone(), pool.clone());
        let spawned = std::thread::Builder::new()
            .name("refill".into())
            .spawn(move || refill(&daemon, theirs.as_deref()));
        if let Err(e) = spawned {
            log(format!("a refill thread: {e}; refilling here"));
            refill(self, pool.as_deref());
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
        self.await_arrival(id);
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
            removal = self.end_container(&mut registry, id, |c| c.exit_code = Some(code));
            (said, code)
        };
        lock(&self.runs).remove(id);
        self.resolved.notify_all();
        for waiter in lock(&self.waiters).remove(id).unwrap_or_default() {
            waiter.hear(code);
        }
        drop(registry);
        if let Some(removal) = removal {
            let _ = self.complete(&removal);
        }
        said
    }

    /// The end of container `id`'s run, as `end` records it: a `--rm` container is taken
    /// out of sight, for [`complete`](Self::complete) to remove, and one whose removal
    /// fails, or any other, is recorded. Failures go to the log.
    fn end_container(
        &self,
        registry: &mut Registry,
        id: &str,
        end: impl FnOnce(&mut Container),
    ) -> Option<Removal> {
        if registry.get(id)?.auto_remove {
            match registry.remove(id) {
                Ok(removal) => return removal,
                Err(e) => log(format!("container {id}: removing it: {e}")),
            }
        }
        if let Err(e) = registry.update(id, end) {
            log(format!("container {id}: its record is behind: {e}"));
        }
        None
    }

    /// Makes `removal` durable out of the registry's lock, then lets its name go and
    /// deletes what it set aside. Until it is durable the name stays held, since a crash
    /// could bring the container back. Failures go to the log, and whether it is durable
    /// is returned.
    pub(super) fn complete(&self, removal: &Removal) -> io::Result<()> {
        let id = &removal.container.id;
        let synced = removal.sync();
        match &synced {
            Ok(()) => lock(&self.containers).release(id),
            Err(e) => log(format!(
                "container {id}: its removal may not outlast a crash, and its name stays held: {e}"
            )),
        }
        if let Err(e) = removal.delete() {
            log(format!(
                "container {id}: deleting its files: {e}; the next start deletes them"
            ));
        }
        synced
    }

    /// Commits the run of container `id` to the warm VM in hand, unless `rm` cancelled it
    /// or the daemon is stopping: then it never starts, and the error is what its client
    /// is told. `stop_runs` sets `ending` under the same lock, so a run either commits
    /// before and is stopped once it runs, or sees it here.
    fn commit(&self, id: &str) -> Result<(), String> {
        {
            let mut runs = lock(&self.runs);
            match runs.get_mut(id) {
                Some(state) if matches!(state, RunState::Pending { cancelled: false }) => {
                    if !self.ending.load(Ordering::SeqCst) {
                        *state = RunState::Handing;
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
    fn give_back(self: &Arc<Self>, ready: Ready) {
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
        self.replace(None);
    }

    /// Waits while the run of container `id` is being started: until it runs, or never
    /// will. What `stop` and `kill` then find is what a client that saw `run` return would
    /// find.
    pub(super) fn await_start(&self, id: &str) {
        let mut runs = lock(&self.runs);
        while matches!(
            runs.get(id),
            Some(RunState::Pending { cancelled: false } | RunState::Handing)
        ) {
            runs = self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// For `rm`: takes container `id` out of sight with its run if no VM has been
    /// committed to the run, which then never starts; those waiting for the container hear
    /// 0, the code of one that never ran. The removal, for [`complete`](Self::complete), if
    /// it did. A run being handed over is seen through first, so that `rm` acts on whether
    /// it started.
    pub(super) fn cancel_start(&self, id: &str) -> io::Result<Option<Removal>> {
        loop {
            let mut registry = lock(&self.containers);
            let mut runs = lock(&self.runs);
            let handing = match runs.get_mut(id) {
                Some(RunState::Pending { cancelled }) => {
                    *cancelled = true;
                    false
                }
                Some(RunState::Handing) => true,
                Some(RunState::Tracked(_)) | None => return Ok(None),
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
            return registry.remove(id);
        }
    }

    /// Registers the run of container `id`, just handed to `ready`'s VM: from here it
    /// runs, as commands see it. A run handed over while the daemon stops is stopped too:
    /// `stop_runs` signals the runs it finds, under the same lock, and this one if it came
    /// too late to be found. Returns the run's inbox, for [`follow`](Self::follow).
    fn register(&self, ready: Ready, id: &str, detached: Option<&UnixStream>) -> Arc<Mutex<Inbox>> {
        // Runs last as long as their commands.
        let _ = ready.socket.set_read_timeout(None);
        let socket = Arc::new(ready.socket);
        let detached = detached.and_then(|conn| {
            conn.try_clone()
                .map_err(|e| log(format!("holding a detached client's connection: {e}")))
                .ok()
        });
        let inbox = Arc::new(Mutex::new(Inbox {
            socket: socket.clone(),
            pid: ready.vm.id(),
            started: false,
            detached,
            ended: false,
        }));
        let tracked = Tracked {
            socket,
            vm: ready.vm,
            inbox: inbox.clone(),
        };
        let mut runs = lock(&self.runs);
        if self.ending.load(Ordering::SeqCst) {
            let _ = shards_ipc::send(&tracked.socket, kind::SIGNAL, &15u32.to_be_bytes(), &[]);
        }
        runs.insert(id.to_string(), RunState::Tracked(tracked));
        drop(runs);
        self.resolved.notify_all();
        inbox
    }

    /// Follows a run to its end, and keeps its container's record: running once the VM
    /// says STARTED, exited at its DONE, or at the VM's end if the VM dies first. A command
    /// that never started leaves its container created, with the status that says why
    /// (moby daemon/start.go). A detached run's client learns whether its command started,
    /// and if not, why not, as `docker run -d` does.
    fn follow(&self, id: &str, inbox: &Mutex<Inbox>) {
        // Open while `inbox` holds the socket.
        let fd = lock(inbox).socket.as_raw_fd();
        // A command may have taken the run's end already; then this learns it within a
        // tick.
        while !self.take_messages(id, inbox) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) on one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, TICK) };
        }
    }

    /// Takes the messages run `id` has sent and nobody has taken yet; whether it has
    /// ended.
    fn take_messages(&self, id: &str, inbox: &Mutex<Inbox>) -> bool {
        let mut inbox = lock(inbox);
        while !inbox.ended && readable(&inbox.socket) {
            match shards_ipc::recv(&inbox.socket) {
                Ok(Some(m)) if m.kind == kind::STARTED => self.run_started(id, &mut inbox),
                Ok(Some(m)) if m.kind == kind::DONE => self.run_ended(id, &mut inbox, Some(&m.payload)),
                Ok(Some(m)) if m.kind == kind::LOST => self.log_lost(id, &m.payload),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => self.run_ended(id, &mut inbox, None),
            }
        }
        inbox.ended
    }

    /// Takes what every run has sent, so that what a command answers includes all that
    /// any client has seen of them.
    pub(super) fn settle(&self) {
        for id in lock(&self.containers).catch_up() {
            log(format!("container {id}: its record is written again"));
        }
        let runs: Vec<(String, Arc<Mutex<Inbox>>)> = lock(&self.runs)
            .iter()
            .filter_map(|(id, r)| match r {
                RunState::Tracked(t) => Some((id.clone(), t.inbox.clone())),
                _ => None,
            })
            .collect();
        for (id, inbox) in runs {
            self.take_messages(&id, &inbox);
        }
    }

    /// Output of run `id` its log could not keep: counted on its container, for `logs` to
    /// say (audit A12).
    fn log_lost(&self, id: &str, payload: &[u8]) {
        let Some(lost) = payload.first_chunk::<8>().map(|b| u64::from_be_bytes(*b)) else {
            return;
        };
        log(format!(
            "container {id}: {lost} bytes of its output could not be kept in its log"
        ));
        if let Err(e) = lock(&self.containers).update(id, |c| c.log_lost = c.log_lost.saturating_add(lost)) {
            log(format!("container {id}: its record is behind: {e}"));
        }
    }

    fn run_started(&self, id: &str, inbox: &mut Inbox) {
        inbox.started = true;
        let recorded = lock(&self.containers).update(id, |c| {
            c.state = Life::Running;
            c.started = Some(containers::now());
        });
        let behind = recorded.err().map(|e| {
            let said = format!("container {id}: its record is behind: {e}");
            log(&said);
            said
        });
        if let Some(client) = inbox.detached.take() {
            if let Some(said) = behind {
                let warning = format!("WARNING: {said}\n");
                let _ = shards_ipc::send(&client, kind::ERR, warning.as_bytes(), &[]);
            }
            let _ = shards_ipc::send(&client, kind::END, &[0], &[]);
        }
    }

    /// Run `id` ended: `done` is its DONE, or `None` for a VM that ended without one.
    fn run_ended(&self, id: &str, inbox: &mut Inbox, done: Option<&[u8]>) {
        inbox.ended = true;
        let (pid, started) = (inbox.pid, inbox.started);
        // DONE carries the status, and for a command that never started what dockerd
        // would say and the code its container keeps (warm.rs finish).
        let (status, said) = match done.and_then(<[u8]>::split_first) {
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
        let removal = {
            let mut registry = lock(&self.containers);
            let removal = self.end_container(&mut registry, id, |c| {
                c.exit_code = Some(status);
                if started {
                    c.state = Life::Exited;
                    c.finished = Some(containers::now());
                }
            });
            lock(&self.runs).remove(id);
            self.resolved.notify_all();
            for waiter in lock(&self.waiters).remove(id).unwrap_or_default() {
                waiter.hear(status);
            }
            removal
        };
        if let Some(removal) = removal {
            let _ = self.complete(&removal);
        }
        // A detached command that never started: why, as `docker run -d` says it.
        if let Some(client) = inbox.detached.take() {
            let (text, exits) = crate::spec::not_run(said.as_deref().unwrap_or_default());
            let _ = shards_ipc::send(&client, kind::ERR, format!("{text}\n").as_bytes(), &[]);
            let _ = shards_ipc::send(&client, kind::END, &[exits], &[]);
        }
        *lock(&self.last) = Instant::now();
    }

    /// Waits up to `limit` (for ever if `None`, or if it is too long to count) for the
    /// container with `id` to stop running, and returns its exit code: 0 if it never ran.
    /// `None` if it still runs, or if `client`, the connection of the command that waits,
    /// has hung up. A waiter that stops waiting is forgotten (audit A07).
    fn await_exit(&self, id: &str, limit: Option<Duration>, client: Option<&UnixStream>) -> Option<u8> {
        let (number, told) = {
            let registry = lock(&self.containers);
            // A run `rm` cancelled will never run.
            if matches!(
                lock(&self.runs).get(id),
                None | Some(RunState::Pending { cancelled: true })
            ) {
                return Some(registry.get(id).and_then(|c| c.exit_code).unwrap_or(0));
            }
            let (tell, told) = mpsc::channel();
            let number = self.next_waiter.fetch_add(1, Ordering::Relaxed);
            lock(&self.waiters)
                .entry(id.to_string())
                .or_default()
                .push(Waiter {
                    number,
                    tell,
                    wake: None,
                });
            (number, told)
        };
        let deadline = limit.and_then(|l| Instant::now().checked_add(l));
        loop {
            let step = deadline.map_or(TICK_TIME, |d| {
                d.saturating_duration_since(Instant::now()).min(TICK_TIME)
            });
            match told.recv_timeout(step) {
                Ok(code) => return Some(code),
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let over = deadline.is_some_and(|d| Instant::now() >= d);
                    // A command's client sends nothing after its request: anything to
                    // read is its end.
                    if over || client.is_some_and(readable) {
                        break;
                    }
                }
            }
        }
        self.forget_waiter(id, number);
        // Its code may have come as it gave up.
        told.try_recv().ok()
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
            .push(Waiter {
                number,
                tell: mpsc::channel().0,
                wake: Some(theirs),
            });
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
    fn step_aside(self: &Arc<Self>) {
        self.close();
        self.stopping.store(true, Ordering::SeqCst);
        self.stop_runs();
        self.end_clients();
        // Runs waiting for a warm VM look at `stopping` again; the lock orders the wakeup
        // after their last look.
        drop(lock(&self.state));
        self.changed.notify_all();
    }

    /// Ends the runs in progress as dockerd ends its containers when it shuts down: SIGTERM
    /// to each command, SIGKILL to any still running after STOP_GRACE, and the VM itself
    /// if its command outlives even that (moby daemon/daemon.go Shutdown, daemon/stop.go).
    fn stop_runs(self: &Arc<Self>) {
        let signal = |runs: &HashMap<String, RunState>, linux: u32| {
            for t in tracked(runs) {
                let _ = shards_ipc::send(&t.socket, kind::SIGNAL, &linux.to_be_bytes(), &[]);
            }
        };
        {
            // Under the runs' lock: a run still starting is either here to be signalled,
            // or sees `ending` when it commits or registers (audit A06).
            let runs = lock(&self.runs);
            if self.ending.swap(true, Ordering::SeqCst) {
                return;
            }
            signal(&runs, 15);
        }
        let daemon = self.clone();
        let escalating = std::thread::Builder::new().name("stop".into()).spawn(move || {
            std::thread::sleep(STOP_GRACE);
            signal(&lock(&daemon.runs), 9);
            std::thread::sleep(SHUTDOWN_KILL);
            for t in tracked(&lock(&daemon.runs)) {
                let _ = t.vm.kill(libc::SIGKILL);
            }
        });
        if let Err(e) = escalating {
            log(format!("the stop thread: {e}"));
        }
    }

    /// A warm VM for `prepared`: from its template's pool, or booted for it, saving the
    /// template on the way if there is none.
    fn warm_for(self: &Arc<Self>, prepared: &Prepared, say: &dyn Fn(&str)) -> Result<Ready, String> {
        let guest = match &prepared.boot {
            Boot::Given(cfg) => return self.cold(cfg, &prepared.rootfs, None),
            Boot::Stored(guest) => guest,
        };
        let cfg = Config::new(guest.kernel.clone(), Some(guest.init.clone()));
        if !shards_vmm::vm::SNAPSHOTS {
            return self.cold(&cfg, &prepared.rootfs, None);
        }
        let dir = crate::run::template(&self.home, guest, &prepared.rootfs, &cfg);
        if shards_vmm::snapshot::exists(&dir) {
            match self.claim(&dir) {
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
        let ready = self.cold(&cfg, &prepared.rootfs, Some(&fresh));
        if ready.is_ok()
            && let Err(e) = crate::run::Origin::of(guest, &prepared.rootfs).write(&fresh)
        {
            log(format!("{}: {e}", fresh.display()));
        }
        crate::run::settle(&fresh, &dir);
        if ready.is_ok() && shards_vmm::snapshot::exists(&dir) {
            self.refill(&mut lock(&self.state), &dir);
        }
        ready
    }

    /// A warm VM of the template in `dir`, once one is ready: one waiting, or one started
    /// for this run where none is starting for it, so a run is served whatever its pool
    /// keeps, and a burst waits for no refill (audit A13, A14).
    fn claim(self: &Arc<Self>, dir: &Path) -> Result<Ready, Claim> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut state = lock(&self.state);
        let mut waiting = false;
        let leave = |state: &mut State, waiting: bool| {
            if waiting && let Some(pool) = state.pools.get_mut(dir) {
                pool.waiting = pool.waiting.saturating_sub(1);
            }
        };
        loop {
            let pool = state.pools.entry(dir.to_path_buf()).or_default();
            if pool.failures >= MAX_FAILURES {
                state.pools.remove(dir);
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
            self.refill(&mut state, dir);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                leave(&mut state, waiting);
                return Err(Claim::Failed(format!(
                    "no warm VM of {} was ready in {READY_TIMEOUT:?}",
                    dir.display()
                )));
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Starts warm VMs of the template in `dir`: one for each run waiting that none is
    /// starting for, and until its pool will hold `target`, as far as `warm_max` allows,
    /// all pools together, after evicting the ready VMs of the pools least recently
    /// claimed from.
    fn refill(self: &Arc<Self>, state: &mut State, dir: &Path) {
        // A template collected is restored no more.
        if !shards_vmm::snapshot::exists(dir) {
            state.pools.remove(dir);
            return;
        }
        let (for_runs, ahead) = {
            let pool = state.pools.entry(dir.to_path_buf()).or_default();
            if pool.failures >= MAX_FAILURES {
                return;
            }
            let now = Instant::now();
            pool.demand.begin(now);
            let target = pool.demand.target(now, self.target, self.keep);
            let have = pool.ready.len() + pool.starting;
            let for_runs = pool.waiting.saturating_sub(pool.starting);
            let ahead = target.saturating_sub(have + for_runs);
            (for_runs, ahead)
        };
        let ahead = ahead.min(self.room(state, dir, ahead));
        let State { pools, starting } = state;
        let Some(pool) = pools.get_mut(dir) else {
            return;
        };
        let args: Vec<OsString> = vec!["restore".into(), dir.into(), "--warm".into(), "3".into()];
        for _ in 0..for_runs + ahead {
            match self.start(&args, For::Pool(dir.to_path_buf())) {
                Ok(vm) => {
                    pool.starting += 1;
                    starting.insert(vm.id(), vm);
                }
                Err(e) => {
                    log(format!("starting a warm VM of {}: {e}", dir.display()));
                    pool.failures += 1;
                    return;
                }
            }
        }
    }

    /// Collects if a collection is due: it stays due until one has run, which it cannot
    /// while any run is being prepared.
    fn collect_if_due(&self) {
        // A pull, here or by `shards pull`, says so (pull.rs, `collect_due`).
        let due = crate::pull::collect_due(&self.home);
        if std::fs::remove_file(&due).is_ok() {
            self.collect.store(true, Ordering::SeqCst);
        }
        if !self.collect.load(Ordering::SeqCst) {
            return;
        }
        match self.collect_garbage() {
            Ok(true) => self.collect.store(false, Ordering::SeqCst),
            Ok(false) => {}
            Err(e) => {
                log(format!("collecting: {e}"));
                self.collect.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Removes what nothing needs (audit A13): the image store's content no reference
    /// needs (`Store::collect`), then the templates whose root filesystem has gone, that
    /// another guest saved, or that record no origin, ending their pools, and templates a
    /// daemon before this one left half saved. Whether it ran: not while a run is being
    /// prepared, which holds the store's lease.
    fn collect_garbage(&self) -> Result<bool, String> {
        let began = Instant::now();
        let store = crate::pull::store(&self.home)?;
        // Held whole until the templates are done: no run begins meanwhile.
        let Some((collected, _whole)) = store.collect().map_err(|e| e.to_string())? else {
            return Ok(false);
        };
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
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(format!("{}: {e}", templates.display())),
        };
        let ours = format!(".new-{}-", std::process::id());
        for entry in entries {
            let dir = entry.map_err(|e| format!("{}: {e}", templates.display()))?.path();
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let live = if name.contains(".new-") {
                // Being saved by this daemon, or left by one before it.
                name.contains(&ours)
            } else {
                crate::run::Origin::read(&dir).is_some_and(|o| o.live(guest.as_ref()))
            };
            if live {
                continue;
            }
            if let Some(mut pool) = lock(&self.state).pools.remove(&dir) {
                for ready in pool.ready.drain(..) {
                    let _ = ready.vm.kill(libc::SIGKILL);
                }
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => log(format!("collected template {}", dir.display())),
                Err(e) => log(format!("collecting {}: {e}", dir.display())),
            }
        }
        Ok(true)
    }

    /// Ends the ready VMs of pools unclaimed past their keep-alive, and forgets the pools
    /// that hold nothing (audit A13).
    fn age_pools(&self) {
        let now = Instant::now();
        let mut state = lock(&self.state);
        state.pools.retain(|dir, pool| {
            if pool.waiting > 0 || !pool.demand.expired(now, self.keep) {
                return true;
            }
            for aged in pool.ready.drain(..) {
                log(format!(
                    "ending warm VM {} of {}: unclaimed for {:?} (SHARDS_POOL_KEEP)",
                    aged.vm.id(),
                    dir.display(),
                    self.keep
                ));
                let _ = aged.vm.kill(libc::SIGKILL);
            }
            pool.starting > 0
        });
    }

    /// Refills the pools claimed from more recently than `dir`'s, whose VM has just become
    /// ready: one that found no room while `dir`'s was starting, when only ready VMs can
    /// be ended, takes it now.
    fn rebalance(self: &Arc<Self>, state: &mut State, dir: &Path) {
        let Some(since) = state.pools.get(dir).map(|p| p.demand.last()) else {
            return;
        };
        let hotter: Vec<PathBuf> = state
            .pools
            .iter()
            .filter(|(d, p)| d.as_path() != dir && p.demand.last() > since)
            .map(|(d, _)| d.clone())
            .collect();
        for hot in hotter {
            self.refill(state, &hot);
        }
    }

    /// Room for up to `want` more warm VMs ahead of runs beside `dir`'s: within
    /// `warm_max`, all pools together, made by ending the ready VMs of other pools, least
    /// recently claimed from first.
    fn room(&self, state: &mut State, dir: &Path, want: usize) -> usize {
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
                .filter(|(d, p)| d.as_path() != dir && !p.ready.is_empty())
                .min_by_key(|(_, p)| p.demand.last());
            let Some((cold, pool)) = coldest else {
                break;
            };
            if let Some(evicted) = pool.ready.pop_back() {
                log(format!(
                    "ending warm VM {} of {}: {} warm VMs are kept at most (SHARDS_WARM_MAX)",
                    evicted.vm.id(),
                    cold.display(),
                    self.warm_max
                ));
                let _ = evicted.vm.kill(libc::SIGKILL);
            }
        }
        self.warm_max.saturating_sub(held(state)).min(want)
    }

    /// A VM booted for one run, from `cfg` into `rootfs`, saving a template to `save` on
    /// the way.
    fn cold(self: &Arc<Self>, cfg: &Config, rootfs: &Path, save: Option<&Path>) -> Result<Ready, String> {
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
        if let Some(init) = &cfg.init {
            args.extend(["--init".into(), init.into()]);
        }
        if let Some(dir) = save {
            args.extend(["--snapshot-dir".into(), dir.into()]);
        }
        args.extend(["--warm".into(), "3".into()]);
        let (tx, rx) = mpsc::channel();
        self.start(&args, For::Run(tx))?;
        // A VM given up on ends as its socket closes, the daemon's end dropped with it.
        loop {
            match rx.recv_timeout(TICK_TIME) {
                Ok(ready) => return ready,
                Err(mpsc::RecvTimeoutError::Timeout) if self.stopping.load(Ordering::SeqCst) => {
                    return Err("the daemon is shutting down".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the VM's watcher ended without a word".into());
                }
            }
        }
    }

    /// Starts shards-vm with `args` as a warm VM whose daemon socket is its descriptor 3,
    /// and watches it.
    fn start(self: &Arc<Self>, args: &[OsString], dest: For) -> Result<Arc<shards_ipc::Child>, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a VM's socket: {e}"))?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let err = io::stderr();
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let child = shards_ipc::spawn(
            &self.vm,
            &args,
            &[
                (null.as_fd(), 0),
                (err.as_fd(), 1),
                (err.as_fd(), 2),
                (theirs.as_fd(), 3),
            ],
            false,
        )
        .map_err(|e| format!("starting {}: {e}", self.vm.display()))?;
        let vm = Arc::new(child);
        let (daemon, watched) = (self.clone(), vm.clone());
        let watching = std::thread::Builder::new()
            .name("warm vm".into())
            .spawn(move || daemon.watch(&watched, ours, dest));
        if let Err(e) = watching {
            let _ = vm.kill(libc::SIGKILL);
            return Err(format!("watching VM {}: {e}", vm.id()));
        }
        Ok(vm)
    }

    /// Waits for a VM to be ready and hands it to whoever it is for; then waits for its
    /// end. A pooled VM that ends while waiting leaves its pool, which refills.
    fn watch(self: &Arc<Self>, child: &Arc<shards_ipc::Child>, socket: UnixStream, dest: For) {
        let pid = child.id();
        let began = Instant::now();
        let ready = ready(&socket, pid);
        match &dest {
            For::Pool(dir) => {
                let mut state = lock(&self.state);
                state.starting.remove(&pid);
                let pool = state.pools.entry(dir.clone()).or_default();
                pool.starting = pool.starting.saturating_sub(1);
                match ready {
                    Ok(()) => {
                        pool.failures = 0;
                        pool.demand.refilled(began.elapsed());
                        pool.ready.push_back(Ready {
                            vm: child.clone(),
                            socket,
                            pool: Some(dir.clone()),
                        });
                        self.rebalance(&mut state, dir);
                    }
                    Err(e) => {
                        log(&e);
                        pool.failures += 1;
                        let _ = child.kill(libc::SIGKILL);
                    }
                }
                self.changed.notify_all();
            }
            For::Run(tx) => match ready {
                Ok(()) => {
                    let _ = tx.send(Ok(Ready {
                        vm: child.clone(),
                        socket,
                        pool: None,
                    }));
                }
                Err(e) => {
                    let _ = child.kill(libc::SIGKILL);
                    let _ = tx.send(Err(e));
                }
            },
        }
        let status = child.wait();
        if let For::Pool(dir) = dest {
            let mut state = lock(&self.state);
            let pool = state.pools.get_mut(&dir);
            if let Some(pool) = pool
                && let Some(i) = pool.ready.iter().position(|r| r.vm.id() == pid)
            {
                pool.ready.remove(i);
                log(format!("warm VM {pid} ended while it waited ({status:?})"));
                self.refill(&mut state, &dir);
                return;
            }
        }
        // A VM that served exits 0, whatever its command's status: the client has that.
        if !matches!(status, Ok(0)) {
            log(format!("VM {pid} ended with {status:?}"));
        }
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

/// A container's directory, where its run's VM writes its log (spec.rs, `LOG_STDOUT`).
#[derive(Debug)]
struct Log {
    dir: File,
}

/// A new container's directory `dir`, with its log's first segment and that segment's
/// index, for this user alone and written by appends.
fn new_log(dir: &Path) -> io::Result<Log> {
    use std::os::unix::fs::OpenOptionsExt;
    shards_vmm::platform::create_private_dir(dir)?;
    // Its log, then its index, which says the segment is there (spec.rs, `log_segment`).
    let (log, index) = log_segment(0);
    for name in [log, index] {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(name))?;
    }
    Ok(Log {
        dir: File::open(dir)?,
    })
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

    use crate::containers::{Disk, Real};

    use super::*;

    /// A client's connection handed over while collections of in-flight descriptors run
    /// still carries the client's bytes: the daemon's copy keeps it reachable until the
    /// warm VM has it. Without that copy, macOS flushes it (shards_ipc; M24).
    #[test]
    fn a_handed_over_connection_survives_until_taken() {
        // Every freed Unix socket starts a collection on macOS.
        let done = Arc::new(AtomicBool::new(false));
        let collections = {
            let done = done.clone();
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    drop(UnixStream::pair().unwrap());
                    std::thread::sleep(Duration::from_micros(50));
                }
            })
        };
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
        done.store(true, Ordering::Relaxed);
        collections.join().unwrap();
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

    /// What thread `h` returned, once it has, within [`PATIENCE`].
    fn joined<T>(h: std::thread::JoinHandle<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        while !h.is_finished() {
            assert!(Instant::now() < deadline, "a thread did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        h.join().unwrap()
    }

    /// A daemon of a home of its own, which starts no VM itself: its tests play the warm
    /// VMs, over socket pairs, and hold each step of a run's start as long as they like
    /// (audit A06).
    struct Test {
        daemon: Arc<Daemon>,
        home: PathBuf,
        /// The processes of the warm VMs played, ended with the test.
        vms: Mutex<Vec<Arc<shards_ipc::Child>>>,
    }

    impl Test {
        fn new(tag: &str) -> Test {
            Test::on(tag, Arc::new(Real))
        }

        /// A daemon whose containers are kept on `disk`.
        fn on(tag: &str, disk: Arc<dyn Disk>) -> Test {
            let home = std::env::temp_dir().join(format!("shards-daemon-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let containers =
                Registry::open_on(home.join("containers"), disk, &mut |note| panic!("noted: {note}"))
                    .unwrap();
            let home_lock = File::create(home.join("daemon.lock")).unwrap();
            let daemon = Daemon::new(
                home.clone(),
                PathBuf::from("shards-vm"),
                Identity::default(),
                Settings {
                    pool: 0,
                    warm_max: DEFAULT_WARM_MAX,
                    idle: DEFAULT_IDLE,
                    keep: DEFAULT_KEEP,
                    logs: DEFAULT_LOGS,
                },
                containers,
                home_lock,
            );
            Test {
                daemon: Arc::new(daemon),
                home,
                vms: Mutex::default(),
            }
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
                pool: pool.map(PathBuf::from),
            };
            (ready, theirs)
        }

        /// Creates container `name` as a client's run does, in a directory of its own; its
        /// ID, once it is seen.
        fn create(&self, name: &str) -> String {
            let id = self.reserve(name);
            self.daemon.await_arrival(&id);
            id
        }

        /// Reserves container `name` as a client's run does, its record being written; its
        /// ID.
        fn reserve(&self, name: &str) -> String {
            let (id, _log) = self.daemon.new_container().unwrap();
            let run = Run {
                image: "test".into(),
                name: Some(name.into()),
                ..Run::default()
            };
            let prepared = Prepared {
                boot: Boot::Given(Config::new(PathBuf::from("kernel"), None)),
                rootfs: PathBuf::new(),
                spec: shards_abi::run::Spec {
                    argv: vec![b"exit".to_vec(), b"7".to_vec()],
                    ..Default::default()
                },
                interactive: false,
                lease: None,
            };
            self.daemon.create(&run, &prepared, &id).unwrap();
            self.daemon.record_arrival(&id);
            id
        }

        /// Starts the run of container `id` on a thread of its own, as a client's run is
        /// started, then follows it; the warm VMs sent on the channel returned are the
        /// ones it may take, and the count how many it asked for. The thread returns what
        /// the client was told, if the run did not start.
        fn start(&self, id: &str) -> Starting {
            let (warm, offered) = mpsc::channel::<Ready>();
            let asked = Arc::new(AtomicUsize::new(0));
            let (daemon, id, counted) = (self.daemon.clone(), id.to_string(), asked.clone());
            let run = std::thread::spawn(move || {
                let null = File::open("/dev/null").unwrap();
                let acquire = || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    offered
                        .recv_timeout(PATIENCE)
                        .map_err(|_| "no warm VM".to_string())
                };
                let inbox = daemon.start_run(&id, b"run", &[null.as_fd()], None, acquire)?;
                daemon.follow(&id, &inbox);
                Ok(())
            });
            Starting { warm, asked, run }
        }

        /// `shards ARGS`, as its client asks the daemon: status, stdout and stderr.
        fn ask(&self, args: &[&str]) -> (u8, String, String) {
            ask(&self.daemon, args)
        }

        /// `shards ARGS` on a thread of its own.
        fn asking(&self, args: &[&str]) -> std::thread::JoinHandle<(u8, String, String)> {
            let daemon = self.daemon.clone();
            let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            std::thread::spawn(move || {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                ask(&daemon, &args)
            })
        }

        /// Waits until what `f` finds of the daemon holds.
        fn until(&self, what: &str, f: impl Fn(&Daemon) -> bool) {
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

    impl Drop for Test {
        fn drop(&mut self) {
            for vm in lock(&self.vms).drain(..) {
                let _ = vm.kill(libc::SIGKILL);
                let _ = vm.wait();
            }
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    struct Starting {
        warm: mpsc::Sender<Ready>,
        asked: Arc<AtomicUsize>,
        run: std::thread::JoinHandle<Result<(), String>>,
    }

    fn ask(daemon: &Arc<Daemon>, args: &[&str]) -> (u8, String, String) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let asker = commands::Asker {
            east_asian: false,
            now: 0,
            utc_offset: 0,
        };
        let status = daemon.command(&argv, &asker, &commands::Reply(&ours));
        drop(ours);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        while let Ok(Some(m)) = shards_ipc::recv(&theirs) {
            match m.kind {
                kind::OUT => out.extend(m.payload),
                kind::ERR => err.extend(m.payload),
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
    fn heard(vm: &UnixStream) -> (u8, Vec<u8>) {
        let m = shards_ipc::recv(vm).unwrap().expect("the daemon hung up");
        (m.kind, m.payload)
    }

    fn say(vm: &UnixStream, what: u8, payload: &[u8]) {
        shards_ipc::send(vm, what, payload, &[]).unwrap();
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
        let id = t.create("racer");
        let starting = t.start(&id);
        t.until("the run asked for a warm VM", |_| {
            starting.asked.load(Ordering::SeqCst) == 1
        });
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
        let pooled = &state.pools.get(Path::new("pool")).unwrap().ready;
        assert_eq!(pooled.iter().map(|r| r.vm.id()).collect::<Vec<_>>(), [pid]);
        drop(state);
        assert!(lock(&t.daemon.runs).is_empty());
        assert_eq!(t.ask(&["ps", "-a", "-q"]), (0, String::new(), String::new()));
    }

    /// `rm` of a container whose run is being handed over waits to learn whether it
    /// started, and acts on that: a run that started is running, and is not removed
    /// without `-f`; once it has ended it is.
    #[test]
    fn rm_waits_for_a_run_being_handed_over() {
        let t = Test::new("rm-handing");
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
            "Error response from daemon: cannot remove container \"racer\": container is running: stop the container before removing or force remove\n"
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
    }

    /// `rm -f` of a container whose run is being handed over kills the run once it has
    /// started, then removes it.
    #[test]
    fn rm_force_kills_a_run_being_handed_over_once_it_runs() {
        let t = Test::new("rm-force");
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
            let id = t.create("racer");
            let starting = t.start(&id);
            t.until("the run asked for a warm VM", |_| {
                starting.asked.load(Ordering::SeqCst) == 1
            });
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
        }
    }

    /// A daemon told to stop starts no run still pending: its warm VM hears nothing, its
    /// container keeps the code of a start that failed, and those waiting for it hear
    /// that code.
    #[test]
    fn a_stopping_daemon_starts_no_pending_run() {
        let t = Test::new("stop-pending");
        let id = t.create("racer");
        let starting = t.start(&id);
        t.until("the run asked for a warm VM", |_| {
            starting.asked.load(Ordering::SeqCst) == 1
        });
        let waiting = t.asking(&["wait", "racer"]);
        t.until("wait waits", |d| lock(&d.waiters).contains_key(&id));
        t.daemon.stop_runs();
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
    }

    /// A run being handed over as the daemon is told to stop is stopped once it runs:
    /// its command hears SIGTERM once.
    #[test]
    fn a_stopping_daemon_stops_a_run_being_handed_over() {
        let t = Test::new("stop-handing");
        let id = t.create("racer");
        let starting = t.start(&id);
        let (ready, vm) = t.warm_vm(None);
        starting.warm.send(ready).unwrap();
        assert_eq!(heard(&vm).0, kind::RUN);
        t.daemon.stop_runs();
        say(&vm, kind::TAKEN, &[]);
        say(&vm, kind::STARTED, &[]);
        assert_eq!(heard(&vm), signal(15));
        say(&vm, kind::DONE, &[143]);
        joined(starting.run).unwrap();
        // Nothing more: the run had ended by the time any SIGKILL was due.
        assert_eq!(heard_nothing(&vm), 0);
        let record = t.record(&id).unwrap();
        assert_eq!((record.state, record.exit_code), (Life::Exited, Some(143)));
    }

    /// A client that has sent container command `args`: the daemon's end of its
    /// connection, for [`Daemon::take`], and its own.
    fn commanding(args: &[&str]) -> (UnixStream, UnixStream) {
        let (daemon, client) = UnixStream::pair().unwrap();
        let command = shards_ipc::Command {
            argv: args.iter().map(|a| (*a).to_string()).collect(),
            east_asian: false,
            now: 0,
            utc_offset: 0,
            daemon: Identity::default(),
        };
        shards_ipc::send(&client, kind::CONTAINER, &command.encode(), &[]).unwrap();
        (daemon, client)
    }

    /// Starts container `name`'s run on a warm VM the test plays, and has it running.
    fn running(t: &Test, name: &str) -> (String, Starting, UnixStream) {
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
    /// sends its request, and container commands waiting on a run that ignores its
    /// SIGTERM, whose connections are shut down. The audit's reproduction (A07): before,
    /// the daemon waited for the first until its client closed.
    #[test]
    fn a_stopping_daemon_ends_the_clients_it_would_wait_for() {
        let t = Test::new("stop-clients");
        let (id, starting, vm) = running(&t, "racer");
        let (idle, idle_client) = UnixStream::pair().unwrap();
        t.daemon.take(idle);
        let (waiting, waiting_client) = commanding(&["wait", "racer"]);
        t.daemon.take(waiting);
        let (following, following_client) = commanding(&["logs", "-f", "racer"]);
        t.daemon.take(following);
        t.until("three clients in hand, one waiting", |d| {
            d.busy.load(Ordering::SeqCst) == 3 && lock(&d.waiters).contains_key(&id)
        });
        let t0 = Instant::now();
        t.daemon.step_aside();
        // The run hears its SIGTERM, and goes on regardless.
        assert_eq!(heard(&vm), signal(15));
        t.until("clients still in hand", |d| d.busy.load(Ordering::SeqCst) == 0);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        for client in [&idle_client, &waiting_client, &following_client] {
            // macOS refuses options on a socket shut down both ways (EINVAL): this one's
            // end has nothing more to wait for anyway.
            let _ = client.set_read_timeout(Some(PATIENCE));
            assert!(
                matches!(shards_ipc::recv(client), Ok(None)),
                "a client was not let go"
            );
        }
        assert!(!lock(&t.daemon.waiters).contains_key(&id), "a waiter left behind");
        say(&vm, kind::DONE, &[143]);
        joined(starting.run).unwrap();
    }

    /// A pool that found no room while a colder pool's VM was starting takes it once that
    /// VM is ready: only ready VMs can be ended, and the colder one's then is (audit A13).
    #[test]
    fn a_colder_pools_vm_ready_gives_way_to_a_hotter_pool() {
        let mut t = Test::new("rebalance");
        {
            let daemon = Arc::get_mut(&mut t.daemon).unwrap();
            daemon.target = 1;
            daemon.warm_max = 1;
        }
        // Templates of their own, so their pools refill.
        let (cold, hot) = (t.home.join("cold"), t.home.join("hot"));
        for dir in [&cold, &hot] {
            std::fs::create_dir_all(dir.join("g-1")).unwrap();
            std::fs::write(dir.join("current"), b"g-1\n").unwrap();
            std::fs::write(dir.join("g-1").join("state"), b"").unwrap();
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
            let pool = state.pools.entry(cold.clone()).or_default();
            pool.demand.claimed(t0, 1);
            pool.ready.push_back(ready);
            state
                .pools
                .entry(hot.clone())
                .or_default()
                .demand
                .claimed(t0 + Duration::from_millis(1), 1);
        }
        t.daemon.rebalance(&mut lock(&t.daemon.state), &cold);
        let state = lock(&t.daemon.state);
        assert!(
            state.pools[&cold].ready.is_empty(),
            "the colder pool kept the room"
        );
        drop(state);
        // Ended: its wait returns, as it would not for a sleep of 600 s.
        assert_eq!(vm.wait().unwrap(), 128 + libc::SIGKILL);
    }

    /// A collection removes templates a daemon before this one left half saved, and ones
    /// that record no origin; it keeps the one this daemon is saving, and does nothing
    /// while a run is being prepared (audit A13).
    #[test]
    fn collections_remove_templates_nothing_can_use() {
        let t = Test::new("collect-templates");
        let templates = t.home.join("templates");
        let ours = templates.join(format!("abc.new-{}-0", std::process::id()));
        let left = templates.join("abc.new-1-0");
        let unknown = templates.join("def");
        for dir in [&ours, &left, &unknown] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let lease = crate::pull::store(&t.home).unwrap().lease().unwrap();
        assert_eq!(t.daemon.collect_garbage(), Ok(false), "collected under a lease");
        assert!(left.exists() && unknown.exists());
        drop(lease);
        assert_eq!(t.daemon.collect_garbage(), Ok(true));
        assert!(ours.exists(), "a template being saved was collected");
        assert!(!left.exists() && !unknown.exists());
    }

    /// A pool unclaimed past its keep-alive ends its ready VMs and is forgotten; one
    /// claimed within it, or with a run waiting, keeps them (audit A13).
    #[test]
    fn pools_unclaimed_past_their_keep_alive_end_their_vms() {
        let mut t = Test::new("aging");
        Arc::get_mut(&mut t.daemon).unwrap().keep = Duration::from_millis(100);
        let t0 = Instant::now();
        let mut vms = Vec::new();
        {
            let mut state = lock(&t.daemon.state);
            for (dir, waiting) in [("cold", 0), ("warm", 0), ("waited", 1)] {
                let (ready, theirs) = t.warm_vm(Some(dir));
                vms.push((dir, ready.vm.clone(), theirs));
                let pool = state.pools.entry(PathBuf::from(dir)).or_default();
                pool.demand.begin(t0);
                pool.waiting = waiting;
                pool.ready.push_back(ready);
            }
        }
        std::thread::sleep(Duration::from_millis(150));
        lock(&t.daemon.state)
            .pools
            .get_mut(Path::new("warm"))
            .unwrap()
            .demand
            .claimed(Instant::now(), 2);
        t.daemon.age_pools();
        let state = lock(&t.daemon.state);
        assert!(
            !state.pools.contains_key(Path::new("cold")),
            "an aged pool is forgotten"
        );
        assert_eq!(state.pools[Path::new("warm")].ready.len(), 1);
        assert_eq!(state.pools[Path::new("waited")].ready.len(), 1);
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
        Arc::get_mut(&mut t.daemon).unwrap().request_timeout = Duration::from_millis(200);
        let (silent, silent_client) = UnixStream::pair().unwrap();
        let (trickling, trickling_client) = UnixStream::pair().unwrap();
        let t0 = Instant::now();
        t.daemon.take(silent);
        t.daemon.take(trickling);
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
    }

    /// A waiter that stops waiting is forgotten: one whose time is up, and a `wait` or a
    /// `logs -f` whose client hangs up, which ends them as soon as it does (audit A07).
    #[test]
    fn a_waiter_that_stops_waiting_is_forgotten() {
        let t = Test::new("waiters");
        let (id, starting, vm) = running(&t, "racer");
        assert_eq!(
            t.daemon.await_exit(&id, Some(Duration::from_millis(20)), None),
            None
        );
        assert!(!lock(&t.daemon.waiters).contains_key(&id));
        for args in [&["wait", "racer"][..], &["logs", "-f", "racer"]] {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let daemon = t.daemon.clone();
            let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            let asking = std::thread::spawn(move || {
                let asker = commands::Asker {
                    east_asian: false,
                    now: 0,
                    utc_offset: 0,
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
    }

    /// `shards ARGS` as its client asks the daemon, reading as the daemon answers: status,
    /// stdout and stderr, as bytes.
    fn ask_bytes(daemon: &Arc<Daemon>, args: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
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
            east_asian: false,
            now: 0,
            utc_offset: 0,
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
        write_log(&t, &id, &lines);
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
        let daemon = t.daemon.clone();
        let asking = std::thread::spawn(move || {
            let argv = vec!["logs".to_string(), "racer".to_string()];
            let asker = commands::Asker {
                east_asian: false,
                now: 0,
                utc_offset: 0,
            };
            daemon.command(&argv, &asker, &commands::Reply(&ours))
        });
        drop(shards_ipc::recv(&theirs));
        drop(theirs);
        assert_eq!(joined(asking), 1, "undelivered logs answered as delivered");
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
        assert_eq!(starting.asked.load(Ordering::SeqCst), 2);
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
        assert_eq!(starting.asked.load(Ordering::SeqCst), 1);
        let record = t.record(&id).unwrap();
        assert_eq!((record.state, record.exit_code), (Life::Created, Some(128)));
    }

    /// The host's filesystem, but its directory syncs wait until the test lets them
    /// through, and while `failing` its renames fail.
    #[derive(Debug, Default)]
    struct Held {
        through: Mutex<bool>,
        turn: std::sync::Condvar,
        syncing: AtomicBool,
        failing: AtomicBool,
        /// Its writes too wait until let through, while this is set.
        holding_writes: AtomicBool,
        writing: AtomicBool,
    }

    impl Held {
        fn let_through(&self) {
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
            Real.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            Real.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            Real.remove_dir_all(path)
        }
        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.syncing.store(true, Ordering::SeqCst);
            let through = lock(&self.through);
            drop(self.turn.wait_while(through, |t| !*t).unwrap());
            Real.sync_dir(dir)
        }
    }

    /// A removed container's name is let go only once its removal is durable: a power
    /// loss before then could bring the container back, next to one that took its name
    /// (audit A15).
    #[test]
    fn a_name_is_let_go_only_once_its_removal_is_durable() {
        let held = Arc::new(Held::default());
        let t = Test::on("name-held", held.clone());
        let id = t.create("racer");
        let removal = lock(&t.daemon.containers).remove(&id).unwrap().unwrap();
        let daemon = t.daemon.clone();
        let completing = std::thread::spawn(move || daemon.complete(&removal));
        let deadline = Instant::now() + PATIENCE;
        while !held.syncing.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the removal was never synced");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(lock(&t.daemon.containers).get(&id).is_none(), "out of sight");
        assert_eq!(
            lock(&t.daemon.containers)
                .name_taken("racer")
                .map(|c| c.id.clone()),
            Some(id.clone()),
            "its name was let go before the removal was durable"
        );
        held.let_through();
        completing.join().unwrap().unwrap();
        assert!(lock(&t.daemon.containers).name_taken("racer").is_none());
        assert!(!t.home.join("containers").join(&id).exists());
    }

    /// A record that could not be written is written again before a command is answered
    /// (audit A15).
    #[test]
    fn a_record_behind_is_written_before_a_command_is_answered() {
        let held = Arc::new(Held::default());
        let t = Test::on("behind", held.clone());
        let id = t.create("racer");
        held.failing.store(true, Ordering::SeqCst);
        let e = lock(&t.daemon.containers).update(&id, |c| c.exit_code = Some(9));
        assert!(e.is_err());
        held.failing.store(false, Ordering::SeqCst);
        let on_disk = || {
            let bytes = std::fs::read(t.home.join("containers").join(&id).join("config.json")).unwrap();
            serde_json::from_slice::<Container>(&bytes).unwrap().exit_code
        };
        assert_eq!(on_disk(), None, "behind");
        t.daemon.settle();
        assert_eq!(on_disk(), Some(9));
    }

    /// What happens to a container while its record is written is written too, before it
    /// is seen (audit A15).
    #[test]
    fn a_change_while_a_record_is_written_is_written_before_it_is_seen() {
        let held = Arc::new(Held::default());
        let t = Test::on("arriving", held.clone());
        held.holding_writes.store(true, Ordering::SeqCst);
        let id = t.reserve("racer");
        let deadline = Instant::now() + PATIENCE;
        while !held.writing.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the record was never written");
            std::thread::sleep(Duration::from_millis(1));
        }
        lock(&t.daemon.containers)
            .update(&id, |c| c.exit_code = Some(5))
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
    }

    /// Makers of the spare container at once leave one spare, and no directory besides
    /// (audit A20).
    #[test]
    fn makers_at_once_leave_one_spare() {
        let t = Test::new("spares");
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
    }

    /// Output a run's log could not keep is said by `logs`, which fails: the log is not
    /// all of it; and a log that is gone is said too, not shown as nothing (audit A12).
    #[test]
    fn logs_say_what_they_do_not_hold() {
        let t = Test::new("lost");
        let (id, starting, vm) = running(&t, "racer");
        say(&vm, kind::LOST, &7u64.to_be_bytes());
        say(&vm, kind::DONE, &[0]);
        drop(vm);
        let _ = starting.run.join();
        let (status, _, err) = ask(&t.daemon, &["logs", "racer"]);
        assert_eq!(status, 1, "{err}");
        assert!(err.contains("7 bytes of container"), "{err}");
        assert_eq!(lock(&t.daemon.containers).get(&id).map(|c| c.log_lost), Some(7));

        let gone = t.create("gone");
        std::fs::remove_file(lock(&t.daemon.containers).dir(&gone).join(logs::LOG)).unwrap();
        let (status, _, err) = ask(&t.daemon, &["logs", "gone"]);
        assert_eq!(status, 1, "{err}");
        assert!(err.contains("its log"), "{err}");
    }
}
