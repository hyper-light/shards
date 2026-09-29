//! `shards daemon`: serves `shards run` from warm VMs (docs/design/architecture.md D26).
//!
//! One runs per SHARDS_HOME, holding the home's lock; `shards run` starts it when none
//! listens. For each template it has served, it keeps SHARDS_POOL warm VMs (default 2):
//! restored, resumed, connected, and waiting for a command (warm.rs). A run takes one, and
//! the pool refills. A run with no template yet boots a VM that saves one on the way, and
//! a run with its own kernel and init boots every time.
//!
//! Handing a run over is the daemon's last part in it: the warm VM then serves the client
//! directly. So when the daemon exits, after SHARDS_DAEMON_IDLE seconds (default 900)
//! without a run or on `shards daemon stop`, it ends only the VMs still waiting. Runs in
//! progress go on, as containers outlive `docker run`.

use std::collections::{HashMap, VecDeque};
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
use shards_vmm::vm::Config;

use crate::run::{Boot, Prepared};
use crate::workload::NOT_RUN;

const USAGE: &str = "usage: shards daemon [--detached | stop]
  Serves `shards run` from warm microVMs; `shards run` starts one when none is running.
  It exits after SHARDS_DAEMON_IDLE seconds (default 900) without a run.
  --detached: write messages to daemon.log in SHARDS_HOME, as when `shards run` starts it.
  stop: have the running daemon exit once the runs in hand are handed over.
  SHARDS_POOL: warm microVMs kept for each image (default 2).";

/// How long a VM may take to be ready: a restore takes milliseconds, a boot that saves a
/// template tens of them.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// A template whose warm VMs fail this many times in a row is removed and saved again.
const MAX_FAILURES: u32 = 3;
const DEFAULT_POOL: usize = 2;
const DEFAULT_IDLE: Duration = Duration::from_secs(900);
/// Warm VMs a run may try: one can end while it waits, or before it has taken the run.
const HANDOFF_TRIES: usize = 3;
/// How long a warm VM may take to say it has taken a run: it does so right after it
/// receives one.
const TAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a daemon waits for the lock of one that is exiting.
const TAKEOVER: Duration = Duration::from_secs(5);
/// How often the daemon looks at its clock when no client arrives.
const TICK: libc::c_int = 250;

pub fn daemon(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.collect();
    let arg = |i: usize| args.get(i).and_then(|a| a.to_str());
    let result = match (args.len(), arg(0)) {
        (0, _) => serve(false),
        (1, Some("--detached")) => serve(true),
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
}

#[derive(Default)]
struct State {
    pools: HashMap<PathBuf, Pool>,
    /// Pooled warm VMs still starting, by pid, for the daemon to end when it exits.
    starting: HashMap<u32, Arc<shards_ipc::Child>>,
}

/// Who a starting VM is for.
enum For {
    Pool(PathBuf),
    Run(mpsc::Sender<Result<Ready, String>>),
}

/// Why a template's pool gave no warm VM.
enum Claim {
    /// Its warm VMs keep failing: the template does not restore.
    Broken,
    Failed(String),
}

struct Daemon {
    home: PathBuf,
    exe: PathBuf,
    identity: Identity,
    /// The socket, relative to the working directory, the home.
    socket: &'static Path,
    target: usize,
    idle: Duration,
    state: Mutex<State>,
    changed: Condvar,
    /// Clients connected and not yet handed over.
    busy: AtomicUsize,
    last: Mutex<Instant>,
    stopping: AtomicBool,
    /// The socket is gone: no client arrives from here on.
    closed: AtomicBool,
    /// The connections of `shards daemon stop`, held open until this process exits, which
    /// is how they learn it has.
    stoppers: Mutex<Vec<UnixStream>>,
    /// Numbers the templates a run saves before they become the template.
    saved: AtomicU64,
    /// The home's lock, held while this daemon lives.
    _lock: File,
}

/// A client in hand: counted until its run is handed over or refused.
struct Busy<'a>(&'a Daemon);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        *lock(&self.0.last) = Instant::now();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve(detached: bool) -> Result<(), String> {
    let home = shards_ipc::home()?;
    shards_vmm::platform::create_private_dir(&home).map_err(|e| format!("{}: {e}", home.display()))?;
    if detached {
        log_to(&shards_ipc::log(&home))?;
    }
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
    let identity = Identity::of(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let setting = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    let daemon = Arc::new(Daemon {
        home,
        exe,
        identity,
        socket,
        target: setting("SHARDS_POOL").map_or(DEFAULT_POOL, |n| usize::try_from(n).unwrap_or(DEFAULT_POOL)),
        idle: setting("SHARDS_DAEMON_IDLE").map_or(DEFAULT_IDLE, Duration::from_secs),
        state: Mutex::default(),
        changed: Condvar::new(),
        busy: AtomicUsize::new(0),
        last: Mutex::new(Instant::now()),
        stopping: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        stoppers: Mutex::default(),
        saved: AtomicU64::new(0),
        _lock: home_lock,
    });
    log(format!(
        "serving {} on {}",
        daemon.home.display(),
        daemon.home.join(daemon.socket).display()
    ));
    daemon.listen(listener);
    Ok(())
}

/// Makes `path` this process's stdout and stderr.
fn log_to(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    for target in [1, 2] {
        // SAFETY: dup2(2) onto this process's own standard descriptors.
        if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
            return Err(format!("{}: {}", path.display(), io::Error::last_os_error()));
        }
    }
    Ok(())
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
            loop {
                match listener.accept() {
                    Ok((conn, _)) => self.take(conn),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log(format!("accepting: {e}"));
                        break;
                    }
                }
            }
            let quiet = self.busy.load(Ordering::SeqCst) == 0;
            let idle = quiet && lock(&self.last).elapsed() >= self.idle;
            if !self.closed.load(Ordering::SeqCst) && (self.stopping.load(Ordering::SeqCst) || idle) {
                self.close();
                // A client may have connected before the socket went.
                continue;
            }
            if self.closed.load(Ordering::SeqCst) && quiet {
                self.exit();
            }
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
        self.busy.fetch_add(1, Ordering::SeqCst);
        let daemon = self.clone();
        let spawned = std::thread::Builder::new().name("run".into()).spawn(move || {
            let _busy = Busy(&daemon);
            daemon.handle(&conn);
        });
        if let Err(e) = spawned {
            self.busy.fetch_sub(1, Ordering::SeqCst);
            log(format!("a client's thread: {e}"));
        }
    }

    /// Ends the VMs still waiting, and exits.
    fn exit(&self) -> ! {
        let _ = std::fs::remove_file(self.home.join("daemon.pid"));
        let state = lock(&self.state);
        let waiting = state.pools.values().flat_map(|p| p.ready.iter().map(|r| &r.vm));
        for vm in waiting.chain(state.starting.values()) {
            let _ = vm.kill(libc::SIGTERM);
        }
        log("exiting");
        std::process::exit(0)
    }

    fn handle(self: &Arc<Self>, conn: &UnixStream) {
        let message = match shards_ipc::recv(conn) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(e) => {
                log(format!("a client's request: {e}"));
                return;
            }
        };
        match message.kind {
            kind::START => {}
            kind::STOP => {
                self.close();
                self.stopping.store(true, Ordering::SeqCst);
                match conn.try_clone() {
                    Ok(held) => lock(&self.stoppers).push(held),
                    Err(e) => log(format!("holding a stopper's connection: {e}")),
                }
                return;
            }
            other => {
                log(format!("a client sent message kind {other}"));
                return;
            }
        }
        let Ok([stdin, stdout, stderr]) = <[OwnedFd; 3]>::try_from(message.fds) else {
            log("a run without the client's stdio");
            return;
        };
        let Ok(err) = stderr.try_clone().map(File::from) else {
            return;
        };
        let say = |line: &str| {
            let _ = writeln!(&err, "{line}");
        };
        let refuse = |e: &str| {
            say(&format!("shards: {e}"));
            let _ = shards_ipc::send(conn, kind::EXIT, &[NOT_RUN], &[]);
        };
        let Some(run) = Run::decode(&message.payload) else {
            return refuse("a malformed request");
        };
        if run.daemon != self.identity {
            // Another build asks: its own daemon serves it from here. The socket goes
            // first, so that the client's next connection cannot reach this daemon.
            self.close();
            self.stopping.store(true, Ordering::SeqCst);
            let _ = shards_ipc::send(conn, kind::RESTART, &[], &[]);
            return;
        }
        let prepared = match crate::run::prepare(&run, &self.home, &say) {
            Ok(prepared) => prepared,
            Err(e) => return refuse(&e),
        };
        let mut flags = 0;
        if prepared.interactive {
            flags |= shards_ipc::RUN_INTERACTIVE;
        }
        if run.timing {
            flags |= shards_ipc::RUN_TIMING;
        }
        let mut payload = vec![flags];
        payload.extend(prepared.spec.encode());
        let fds = [conn.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        for _ in 0..HANDOFF_TRIES {
            let ready = match self.warm_for(&prepared, &say) {
                Ok(ready) => ready,
                Err(e) => return refuse(&e),
            };
            let handed = hand_over(&ready.socket, &payload, &fds);
            // Only now, so that starting its successor delays no run.
            if let Some(dir) = &ready.pool {
                self.refill(&mut lock(&self.state), dir);
            }
            match handed {
                // The warm VM serves the client from here, and ours close.
                Ok(()) => return,
                Err(e) => {
                    log(format!("warm VM {} did not take a run: {e}", ready.vm.id()));
                    let _ = ready.vm.kill(libc::SIGKILL);
                }
            }
        }
        refuse("no warm VM took the run");
    }

    /// A warm VM for `prepared`: from its template's pool, or booted for it, saving the
    /// template on the way if there is none.
    fn warm_for(self: &Arc<Self>, prepared: &Prepared, say: &dyn Fn(&str)) -> Result<Ready, String> {
        let guest = match &prepared.boot {
            Boot::Given(cfg) => return self.cold(cfg, &prepared.rootfs, None),
            Boot::Recorded(guest) => guest,
        };
        let cfg = crate::vm_run::config(guest.kernel.clone(), Some(guest.init.clone()));
        if !shards_vmm::vm::SNAPSHOTS {
            return self.cold(&cfg, &prepared.rootfs, None);
        }
        let dir = crate::run::template(&self.home, guest, &prepared.rootfs, &cfg);
        if dir.join(shards_vmm::snapshot::STATE).is_file() {
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
        crate::run::settle(&fresh, &dir);
        if ready.is_ok() && dir.join(shards_vmm::snapshot::STATE).is_file() {
            self.refill(&mut lock(&self.state), &dir);
        }
        ready
    }

    /// A waiting warm VM of the template in `dir`, once one is ready.
    fn claim(self: &Arc<Self>, dir: &Path) -> Result<Ready, Claim> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut state = lock(&self.state);
        loop {
            let pool = state.pools.entry(dir.to_path_buf()).or_default();
            if pool.failures >= MAX_FAILURES {
                state.pools.remove(dir);
                return Err(Claim::Broken);
            }
            // Its pool refills once it has its run (handle).
            if let Some(ready) = pool.ready.pop_front() {
                return Ok(ready);
            }
            self.refill(&mut state, dir);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
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

    /// Starts warm VMs of the template in `dir` until its pool will hold `target`.
    fn refill(self: &Arc<Self>, state: &mut State, dir: &Path) {
        let State { pools, starting } = state;
        let pool = pools.entry(dir.to_path_buf()).or_default();
        let want = self.target.saturating_sub(pool.ready.len() + pool.starting);
        if pool.failures >= MAX_FAILURES {
            return;
        }
        let args: Vec<OsString> = vec![
            "vm".into(),
            "restore".into(),
            dir.into(),
            "--warm".into(),
            "3".into(),
        ];
        for _ in 0..want {
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

    /// A VM booted for one run, from `cfg` into `rootfs`, saving a template to `save` on
    /// the way.
    fn cold(self: &Arc<Self>, cfg: &Config, rootfs: &Path, save: Option<&Path>) -> Result<Ready, String> {
        let mut args: Vec<OsString> = vec![
            "vm".into(),
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
        rx.recv()
            .map_err(|_| "the VM's watcher ended without a word".to_string())?
    }

    /// Starts `exe args` as a warm VM whose daemon socket is its descriptor 3, and watches
    /// it.
    fn start(self: &Arc<Self>, args: &[OsString], dest: For) -> Result<Arc<shards_ipc::Child>, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a VM's socket: {e}"))?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let err = io::stderr();
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let child = shards_ipc::spawn(
            &self.exe,
            &args,
            &[
                (null.as_fd(), 0),
                (err.as_fd(), 1),
                (err.as_fd(), 2),
                (theirs.as_fd(), 3),
            ],
            false,
        )
        .map_err(|e| format!("starting {}: {e}", self.exe.display()))?;
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
                        pool.ready.push_back(Ready {
                            vm: child.clone(),
                            socket,
                            pool: Some(dir.clone()),
                        });
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

/// Sends a run to a warm VM and waits for it to say it has taken it. This process keeps
/// its copies of the client's descriptors until then: macOS flushes a socket in flight
/// that no process holds (shards_ipc).
fn hand_over(vm: &UnixStream, payload: &[u8], fds: &[BorrowedFd<'_>]) -> Result<(), String> {
    shards_ipc::send(vm, kind::RUN, payload, fds).map_err(|e| e.to_string())?;
    vm.set_read_timeout(Some(TAKE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    match shards_ipc::recv(vm) {
        Ok(Some(m)) if m.kind == kind::TAKEN => Ok(()),
        Ok(Some(m)) => Err(format!("it said message kind {} instead of TAKEN", m.kind)),
        Ok(None) => Err("it ended first".into()),
        Err(e) => Err(e.to_string()),
    }
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
        assert_eq!(e, "it ended first");
        warm.join().unwrap();
    }
}
