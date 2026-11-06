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
//! [`TAIL`] bytes are kept for a refusal to quote. The guest is ready when a `CONNECT` is
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
use crate::{End, Guest, GuestSpec, Guests, Init, Refusal, STOP_GRACE, Stream, allowed, backend, check};

/// The guests the `shards` binary boots: [`GuestSpec::binary`], else `SHARDS_BIN`, else
/// `shards` on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Shards;

impl Shards {
    pub fn new() -> Shards {
        Shards
    }
}

/// What shards-vmm keeps of a guest's console, its last bytes, and a reset's report
/// quotes: `SAID` (`crates/vmm/src/devices/serial.rs:57`).
const CONSOLE_SAID: usize = 4096;
/// The most bytes shards' report writes for one byte of the console: it escapes each control
/// character (`vm_run.rs:2750` `last_said`, `char::escape_default`): a C0 one or DEL to at
/// most six (`\u{1f}`, `\u{7f}`), a C1 one (two bytes) to six, a byte that is no UTF-8 to
/// U+FFFD's three.
const ESCAPED: usize = "\\u{7f}".len();
/// How much of what shards says on its stdout and stderr is kept, its last bytes: the longest
/// report it writes as it ends, a reset's, with `--no-console` its guest's last console page
/// escaped (`crates/shards/src/vm_run.rs:2730-2737`, `report` at `:1021`), so a refusal quotes it
/// whole. Its other reports are one error each.
const TAIL: usize = "shards: guest requested a reset, its console's last lines:\n".len()
    + CONSOLE_SAID * ESCAPED
    + "\n".len();
/// The longest answer a handshake reads, `OK <host port>\n` with the port a u32: Firecracker's
/// hybrid vsock answer (firecracker `docs/vsock.md`, "Host-Initiated Connections": `OK
/// <assigned_hostside_port>\n`), as shards' muxer writes it
/// (`crates/vmm/src/devices/virtio/vsock/conn.rs:372`).
const ANSWER: usize = "OK ".len() + "4294967295".len() + "\n".len();

impl Guests for Shards {
    fn backend(&self) -> Result<crate::Backend, Refusal> {
        backend()
    }

    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        Ok(Box::new(start(spec)?))
    }
}

/// Boots `spec`'s guest through shards.
fn start(spec: &GuestSpec) -> Result<ShardsGuest, Refusal> {
    check(spec)?;
    backend()?;
    let dir = PrivateDir::new()?;
    let vsock = dir.0.join("v");
    let args = arguments(spec, &vsock)?;
    let binary = spec
        .binary
        .clone()
        .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("shards"));
    // One pipe for both, read in the order shards wrote; this process keeps a writer of
    // its own to say, after shards' end, where shards' words end.
    let piped = |e: io::Error| Refusal::Boot(format!("a pipe for shards' output: {e}"));
    let (output, writer) = io::pipe().map_err(piped)?;
    let (stdout, stderr) = (
        writer.try_clone().map_err(piped)?,
        writer.try_clone().map_err(piped)?,
    );
    let mut child = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|e| Refusal::Boot(format!("{}: {e}", binary.display())))?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            exited: false,
            stopping: false,
            ready: false,
            end: None,
            said: String::new(),
            ended_as: String::new(),
        }),
        changed: Condvar::new(),
        tail: Mutex::new(Tail {
            bytes: VecDeque::new(),
            drained: false,
            output: None,
        }),
        drained: Condvar::new(),
        end_mark: end_mark(),
    });
    let started = start_threads(&shared, &child, output, writer);
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Refusal::Boot(format!("a thread to watch shards: {e}")));
    }
    Ok(ShardsGuest {
        ports: spec.ports.clone(),
        vsock,
        #[cfg(unix)]
        pid: child.id(),
        child: Mutex::new(child),
        shared,
        _dir: dir,
    })
}

/// `shards run --kernel …` for `spec`, its vsock at `vsock`. `--kernel` comes first, as the
/// command line knows a direct boot by it (`cli/mod.rs`).
fn arguments(spec: &GuestSpec, vsock: &Path) -> Result<Vec<OsString>, Refusal> {
    let mut args: Vec<OsString> = vec!["run".into(), "--kernel".into(), spec.kernel.clone().into()];
    match &spec.init {
        Init::Initrd(path) => args.extend(["--initrd".into(), path.clone().into()]),
        Init::Executable(path) => args.extend(["--init".into(), path.clone().into()]),
    }
    args.extend([
        "--cmdline".into(),
        spec.cmdline.clone().into(),
        "--cpus".into(),
        spec.vcpus.to_string().into(),
        "--memory".into(),
        spec.memory_mib.to_string().into(),
    ]);
    for disk in &spec.disks {
        args.extend(["--disk".into(), with_mode("disk", &disk.path, disk.read_only)?]);
    }
    for share in &spec.shares {
        args.extend(["--share".into(), with_mode("share", &share.dir, share.read_only)?]);
    }
    args.extend(["--vsock".into(), vsock.into(), "--no-console".into()]);
    Ok(args)
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
    let mut arg = OsString::from(path);
    if read_only {
        arg.push(":ro");
    }
    Ok(arg)
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
        let tmp = std::env::temp_dir();
        // `<tmp>/shards-api-<nonce>-<n>/v`, short enough to bind, else under /tmp.
        let longest = "/shards-api-ffffffffffffffff-18446744073709551615/v".len();
        #[cfg(unix)]
        let tmp = if tmp.as_os_str().len().saturating_add(longest) <= SOCKET_PATH {
            tmp
        } else {
            PathBuf::from("/tmp")
        };
        #[cfg(not(unix))]
        let _ = (longest, SOCKET_PATH);
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
}

#[derive(Debug)]
struct State {
    /// The process has exited (and is reaped, or about to be under this lock).
    exited: bool,
    /// `stop` or a drop is ending it: its end is `Stopped`.
    stopping: bool,
    /// A `CONNECT` has been answered `OK`.
    ready: bool,
    end: Option<End>,
    /// What shards said last, once it has exited.
    said: String,
    /// How shards ended, with its last words, once reaped.
    ended_as: String,
}

#[derive(Debug)]
struct Tail {
    bytes: VecDeque<u8>,
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
    child: &Child,
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

/// Reads `output` to the end mark, keeping its last [`TAIL`] bytes before it. What follows the
/// mark is not shards': a child it left holding its output (macOS's grants broker, until it
/// exits) holds no refusal back.
fn drain(shared: &Shared, mut output: io::PipeReader) {
    let mark = shared.end_mark.as_slice();
    // A read never returns more than the tail keeps.
    let mut buf = [0u8; TAIL];
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
                let over = tail.bytes.len().saturating_sub(TAIL);
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
}

impl Exit {
    fn of(child: &Child) -> io::Result<Exit> {
        #[cfg(unix)]
        return Ok(Exit { pid: child.id() });
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle as _;
            Ok(Exit {
                handle: child.as_handle().try_clone_to_owned()?,
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
    fn watch(self, shared: &Shared, mut writer: io::PipeWriter) {
        let awaited = self.awaited();
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
        let said = String::from_utf8_lossy(tail.bytes.make_contiguous())
            .trim()
            .to_string();
        drop(tail);
        let mut state = lock(&shared.state);
        if let Err(e) = awaited {
            state.end = Some(End::Failed(format!("waiting for shards: {e}")));
            state.ended_as = format!("waiting for shards: {e}");
        }
        state.said = said;
        state.exited = true;
        shared.changed.notify_all();
    }
}

/// A guest shards booted.
struct ShardsGuest {
    ports: Vec<u32>,
    vsock: PathBuf,
    #[cfg(unix)]
    pid: u32,
    child: Mutex<Child>,
    shared: Arc<Shared>,
    _dir: PrivateDir,
}

impl ShardsGuest {
    /// Its end, once the process has exited: reaped here, under its state's lock, once.
    fn end(&self, state: &mut State) -> Option<End> {
        if !state.exited {
            return None;
        }
        if state.end.is_none() {
            let status = lock(&self.child).wait();
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
    /// where no `CONNECT` was ever answered, else `Ended`.
    fn gone(&self, state: &mut State) -> Option<Refusal> {
        let end = self.end(state)?;
        if !state.ready && !state.stopping {
            return Some(Refusal::Boot(format!("before it was ready, {}", state.ended_as)));
        }
        Some(Refusal::Ended(end))
    }

    /// Signals the process, while it is unreaped.
    fn signal(&self, state: &State, kill: bool) {
        if state.exited {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            let signal = if kill { libc::SIGKILL } else { libc::SIGTERM };
            // SAFETY: kill(2) of our own child, unreaped while `exited` is false.
            unsafe { libc::kill(pid, signal) };
        }
        // Windows sends a process no signal: shards' daemon ends a VM's process as its
        // Job's (shards_ipc `Child::kill`), and `run --kernel`'s VM, in a Job that ends with
        // it (cli/mod.rs `vm`), ends as it does.
        #[cfg(windows)]
        {
            let _ = kill;
            let _ = lock(&self.child).kill();
        }
    }
}

/// `what`, with what shards said last where it said anything.
fn said(what: &str, said: &str) -> String {
    if said.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {said}")
    }
}

/// The wait before dialing again, learned from what each dial shows: `sqrt(2 c w)`, where
/// `c` is what the last refused dial took and `w` how long this connect has waited. Waiting
/// `p` between dials costs `c w / p` of dialing over a wait like the one so far and `p / 2`
/// of lateness on average once the guest listens; both are seconds, of this thread and of
/// its caller, and this `p` makes their sum least. So a few dials go out early, fewer as the
/// wait grows, and a guest that starts to listen is reached within a pause of it: with a
/// dial of 5 µs (a socket not bound yet) after 150 ms, 1.2 ms, which dialing holds 0.4% of.
/// A dial to a VM that listens waits for the guest's own answer, so the guest paces it. At
/// most `left`.
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
}

/// One `CONNECT <port>` over a new connection to `vsock`, its answer awaited at most `wait`
/// (never zero); with none, as long as the guest takes to answer, which shards' device
/// bounds: it ends a request the guest leaves unanswered at Firecracker's
/// `CONN_REQUEST_TIMEOUT_MS` (`crates/vmm/src/devices/virtio/vsock/conn.rs:26-27`), and the
/// connection with the VM's process.
fn handshake(vsock: &Path, port: u32, wait: Option<Duration>) -> Result<Socket, Attempt> {
    let mut s = stream::connect(vsock).map_err(|e| match e.kind() {
        // Not yet bound, or bound and not yet listening: the VM is starting.
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Attempt::Again(format!("dialing: {e}")),
        _ => Attempt::Refused(format!("{}: {e}", vsock.display())),
    })?;
    if wait.is_some() {
        let timeouts = Stream::set_write_timeout(&s, wait).and_then(|()| Stream::set_read_timeout(&s, wait));
        timeouts.map_err(|e| Attempt::Refused(format!("its timeouts: {e}")))?;
    }
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| Attempt::Again(format!("asking: {e}")))?;
    let mut line = Vec::with_capacity(ANSWER);
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= ANSWER {
            return Err(Attempt::Refused(format!(
                "answered {:?}, no line",
                String::from_utf8_lossy(&line)
            )));
        }
        match s.read(&mut byte) {
            // The guest did not accept: its device closes the connection.
            Ok(0) => {
                return Err(Attempt::Again(
                    "closed unanswered (the guest does not listen there)".into(),
                ));
            }
            Ok(_) => line.extend_from_slice(&byte),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Attempt::Again(format!("awaiting its answer: {e}"))),
        }
    }
    let answer = String::from_utf8_lossy(&line);
    let ok = answer
        .strip_prefix("OK ")
        .is_some_and(|n| n.trim_end().parse::<u32>().is_ok());
    if !ok {
        return Err(Attempt::Refused(format!("answered {answer:?}, not OK")));
    }
    let cleared = Stream::set_write_timeout(&s, None).and_then(|()| Stream::set_read_timeout(&s, None));
    cleared.map_err(|e| Attempt::Refused(format!("its timeouts: {e}")))?;
    Ok(s)
}

impl Guest for ShardsGuest {
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal> {
        allowed(&self.ports, port)?;
        // None: past `Instant`'s range (`Duration::MAX`), so no clock: until the guest
        // listens or ends.
        let began = Instant::now();
        let deadline = began.checked_add(patience);
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let last = loop {
            if let Some(refusal) = self.gone(&mut lock(&self.shared.state)) {
                return Err(refusal);
            }
            let dialed = Instant::now();
            // One dial is always made, its answer awaited for what patience has left, or,
            // with none left (a patience of zero) or no clock, for the guest's own answer.
            let wait = left().filter(|l| !l.is_zero());
            match handshake(&self.vsock, port, wait) {
                Ok(s) => {
                    lock(&self.shared.state).ready = true;
                    return Ok(Box::new(s));
                }
                Err(Attempt::Refused(why)) => return Err(Refusal::Channel { port, why }),
                Err(Attempt::Again(why)) if left().is_some_and(|l| l.is_zero()) => break why,
                Err(Attempt::Again(_)) => {}
            }
            // Waits out the pause, woken at once by the process's end.
            let state = lock(&self.shared.state);
            if !state.exited {
                let pause = pause(dialed.elapsed(), began.elapsed(), left().unwrap_or(Duration::MAX));
                let _ = self.shared.changed.wait_timeout(state, pause);
            }
        };
        if let Some(refusal) = self.gone(&mut lock(&self.shared.state)) {
            return Err(refusal);
        }
        Err(Refusal::Channel {
            port,
            why: format!("no OK within {patience:?}: {last}"),
        })
    }

    fn stop(&self) -> Result<(), Refusal> {
        let mut state = lock(&self.shared.state);
        if state.exited {
            return Ok(());
        }
        state.stopping = true;
        self.signal(&state, false);
        let deadline = Instant::now().checked_add(STOP_GRACE);
        while !state.exited {
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                self.signal(&state, true);
                break;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
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
            self.signal(&state, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Disk, Share};

    /// Inputs, each only formatted into a flag: any values show the mapping.
    fn spec() -> GuestSpec {
        GuestSpec {
            kernel: "/k/Image".into(),
            init: Init::Executable("/k/init".into()),
            cmdline: "console=ttyS0 quiet".into(),
            vcpus: 2,
            memory_mib: 256,
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
    /// kernel first (Hex HEX-GUEST-9).
    #[test]
    fn a_spec_is_the_flags_shards_run_reads() {
        let args = arguments(&spec(), Path::new("/p/v")).unwrap();
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
                "--no-console"
            ]
        );
        let initrd = GuestSpec {
            init: Init::Initrd("/k/initrd".into()),
            ..spec()
        };
        let args = arguments(&initrd, Path::new("/p/v")).unwrap();
        assert_eq!(args.get(3..5).unwrap(), ["--initrd", "/k/initrd"]);
    }

    /// A path ending in `:ro`, which shards would read as a mode, is refused by name (Hex
    /// HEX-GUEST-9).
    #[test]
    fn a_path_ending_in_ro_is_refused() {
        let mut s = spec();
        s.disks.push(Disk {
            path: "/d/x:ro".into(),
            read_only: false,
        });
        assert!(matches!(
            arguments(&s, Path::new("/p/v")),
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

    /// The socket directory's name carries no pid, so a directory an earlier process of the
    /// same pid left, or one another user made to take it first, is never its name (Hex
    /// HEX-GUEST-9).
    #[test]
    fn the_socket_directory_is_named_apart_from_the_pid() {
        let dir = PrivateDir::new().unwrap();
        let name = dir.0.file_name().unwrap().to_str().unwrap().to_string();
        let pid = format!("shards-api-{}-", std::process::id());
        assert!(!name.starts_with(&pid), "{name}");
        let again = PrivateDir::new().unwrap();
        assert_ne!(dir.0, again.0);
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
        let got = handshake(&path, 5, None);
        drop(muxer.join().unwrap());
        got.err()
    }

    /// The longest answer Firecracker's protocol gives, `OK 4294967295\n`, is taken; one a
    /// byte longer is refused as no answer (Hex HEX-GUEST-9).
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
        use std::os::unix::fs::PermissionsExt as _;
        let dir = shards_testdir::TempDir::new("api-dials").unwrap();
        let bin = dir.path().join("shards");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 1000\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    /// A patience of zero is one dial, its answer awaited, and no other (Hex HEX-GUEST-9).
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
    /// (Hex HEX-GUEST-9).
    #[cfg(unix)]
    #[test]
    fn a_patience_of_max_dials_until_the_guest_listens() {
        let (_dir, guest, dials) = listening_after(3);
        assert!(guest.connect(1024, Duration::MAX).is_ok());
        assert_eq!(dials.load(Ordering::SeqCst), 4);
    }

    /// The wait between dials is `sqrt(2 c w)` of the last dial's time and the wait so far,
    /// none at first, never past what patience has left (Hex HEX-GUEST-9).
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
}
