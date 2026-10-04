//! Runs a command in a microVM booted into an image: the host half of the run protocol
//! (docs/design/architecture.md D16). The guest dials in once the image is mounted, and
//! shards sends the workload, relays its stdio, and returns its exit status as
//! `docker run` does.

// Where shards has no vsock yet (Windows), only the options are parsed.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::{Mutex, PoisonError};

#[cfg(unix)]
use shards_abi::run::{self, Size, Spec, kind};

#[cfg(unix)]
use crate::spec::{
    INDEX_LINE, INDEX_START, INDEX_STDERR, LOG_HEAD, LOG_STDERR, LOG_STDOUT, LogRetention, now,
};

#[cfg(unix)]
/// Where the guest's connections to a host port this process serves arrive
/// ([`shards_vmm::vm::VsockHost::ports`]).
pub type Port = std::sync::mpsc::Receiver<UnixStream>;

/// Signals and terminal sizes on their way to the workload, as frames: sent once the guest
/// has dialed the signal port, queued before then while the workload runs or is sure to,
/// and otherwise not sent.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Signals {
    conn: Option<UnixStream>,
    queued: Vec<(u8, Vec<u8>)>,
    /// The workload runs, or is sure to.
    running: bool,
}

#[cfg(unix)]
impl Signals {
    pub const fn new() -> Signals {
        Signals {
            conn: None,
            queued: Vec::new(),
            running: false,
        }
    }
}

/// Where signals for the workload go. A VM process serves one workload, so its own is a
/// `static` that the threads relaying signals, which live as long as the process, share.
#[cfg(unix)]
pub type ToGuest = Mutex<Signals>;

#[cfg(unix)]
fn lock(to: &ToGuest) -> std::sync::MutexGuard<'_, Signals> {
    to.lock().unwrap_or_else(PoisonError::into_inner)
}

/// When a request arrived and when it was answered, on the VMM's clock (for
/// `SHARDS_TIMING`).
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Timing {
    pub request_us: std::sync::OnceLock<u128>,
    pub answered_us: std::sync::OnceLock<u128>,
    /// The request asked for the timing line (a warm VM's client set `SHARDS_TIMING`).
    pub asked: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl Timing {
    pub const fn new() -> Timing {
        Timing {
            request_us: std::sync::OnceLock::new(),
            answered_us: std::sync::OnceLock::new(),
            asked: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// The command a served VM runs.
#[cfg(unix)]
pub enum Request<'a> {
    /// Known before the guest connects.
    Now { spec: Spec, interactive: bool },
    /// Asked for once the guest is connected and waiting: a warm VM's request.
    Later(&'a dyn Fn() -> Result<Asked<'a>, String>),
}

/// A request, as served: the command, whether it reads stdin, the container's log if its
/// output is kept, and what to call once the command runs.
#[cfg(unix)]
pub struct Asked<'a> {
    pub spec: Spec,
    pub interactive: bool,
    pub log: Option<Logger>,
    pub started: Option<&'a (dyn Fn() + Sync)>,
}

/// How a served command ended: its exit status, and if it never ran, why not, in the
/// guest's words.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: u8,
    pub not_run: Option<String>,
    /// Bytes of its output its log could not keep.
    pub lost: u64,
}

/// Keeps a container's output (spec.rs, `LOG_STDOUT`): each record appended to its log,
/// then its entry to the log's index once the record is whole, in segments kept as its
/// retention says (segments.rs). A record that cannot be kept, on a full disk
/// or a log removed, costs the log, not the run: it is taken back, so neither file holds
/// any of it, and its bytes are counted as lost (audit A12).
#[cfg(unix)]
pub struct Logger {
    /// Makes the next segment: in a VM, its daemon, since no VM reaches a container's
    /// directory (D30).
    next: NextSegment,
    retention: LogRetention,
    /// The segment written, and its files.
    seq: u64,
    log: fs::File,
    index: fs::File,
    /// The log's and the index's lengths, where a record that fails is cut back to.
    logged: u64,
    indexed: u64,
    lost: u64,
}

/// Makes a log's segment `seq`, its log and index to append to, and removes the oldest
/// past the log's retention ([`new_segment`]).
#[cfg(unix)]
pub type NextSegment = Box<dyn FnMut(u64) -> io::Result<(fs::File, fs::File)> + Send>;

#[cfg(unix)]
impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("seq", &self.seq)
            .field("logged", &self.logged)
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl Logger {
    /// Writes a container's log from its first segment, `log` and `index`, which its
    /// daemon made with the container, going on in the segments `next` makes.
    pub fn new(
        log: fs::File,
        index: fs::File,
        retention: LogRetention,
        next: NextSegment,
    ) -> io::Result<Logger> {
        let logged = log.metadata()?.len();
        let mut indexed = index.metadata()?.len();
        // A partial entry an earlier writer left is not one.
        if indexed % 8 != 0 {
            indexed -= indexed % 8;
            index.set_len(indexed)?;
        }
        Ok(Logger {
            next,
            retention,
            seq: 0,
            log,
            index,
            logged,
            indexed,
            lost: 0,
        })
    }

    fn keep(&mut self, stream: u8, bytes: &[u8]) {
        // Nothing is no output: a record of it would tell no stream's last byte.
        if bytes.is_empty() {
            return;
        }
        let head = log_head(stream, bytes);
        let record_len = LOG_HEAD + bytes.len() as u64;
        if self.logged > 0 && self.logged + record_len > self.retention.size && self.rotate().is_err() {
            // A log that cannot go on within its bound keeps nothing more.
            self.lost = self.lost.saturating_add(bytes.len() as u64);
            return;
        }
        let mut entry = self.logged & INDEX_START;
        if stream == LOG_STDERR {
            entry |= INDEX_STDERR;
        }
        if bytes.last() == Some(&b'\n') {
            entry |= INDEX_LINE;
        }
        let kept = write_parts(&self.log, &mut [io::IoSlice::new(&head), io::IoSlice::new(bytes)])
            .and_then(|()| (&self.index).write_all(&entry.to_be_bytes()));
        match kept {
            Ok(()) => {
                self.logged += record_len;
                self.indexed += 8;
            }
            Err(_) => {
                self.lost = self.lost.saturating_add(bytes.len() as u64);
                let _ = self.index.set_len(self.indexed);
                let _ = self.log.set_len(self.logged);
            }
        }
    }

    /// Starts the next segment.
    fn rotate(&mut self) -> io::Result<()> {
        let next = self.seq + 1;
        let (log, index) = (self.next)(next)?;
        (self.seq, self.log, self.index, self.logged, self.indexed) = (next, log, index, 0, 0);
        Ok(())
    }

    /// Bytes of output not kept.
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

/// Serves the guest: sends the command, relays stdio, and returns how the workload ended.
/// Signals go through `to`, on the connection the guest makes to `signals`. The command
/// goes only to a guest whose init announced this build's contract, `guest_abi`
/// (shards_abi::control::ABI): an init built for another shards could misread it.
#[cfg(unix)]
pub fn serve(
    listener: &Port,
    signals: Port,
    request: Request<'_>,
    to: &'static ToGuest,
    timing: &Timing,
    guest_abi: &dyn Fn() -> Option<u64>,
) -> Result<Ended, String> {
    let mut conn = listener
        .recv()
        .map_err(|_| "the microVM ended before its guest connected".to_string())?;
    let Asked {
        spec,
        interactive,
        log,
        started,
    } = match request {
        Request::Now { spec, interactive } => Asked {
            spec,
            interactive,
            log: None,
            started: None,
        },
        Request::Later(ask) => ask()?,
    };
    let _ = timing.request_us.set(shards_vmm::log::uptime_us());
    let abi = guest_abi();
    if abi != Some(shards_abi::IDENTITY) {
        let spoken = abi.map_or_else(|| "none".to_string(), |abi| format!("{abi:016x}"));
        return Err(format!(
            "the microVM's shards-init speaks another shards' protocol ({spoken}, where this \
             shards speaks {:016x}): boot the shards-init built with this shards",
            shards_abi::IDENTITY
        ));
    }
    send(&mut conn, kind::SPEC, &spec.encode()).map_err(|e| format!("sending the command: {e}"))?;
    if interactive {
        let mut input = conn.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || forward_stdin(&mut input))
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        send(&mut conn, kind::STDIN, &[]).map_err(|e| format!("closing stdin: {e}"))?;
    }
    lock(to).running = true;
    std::thread::Builder::new()
        .name("signal-conn".into())
        .spawn(move || {
            // Ends with the microVM, if the guest never dials.
            let Ok(mut c) = signals.recv() else {
                return;
            };
            let mut state = lock(to);
            if !state.running {
                return;
            }
            let queued = std::mem::take(&mut state.queued);
            if queued
                .iter()
                .all(|(which, payload)| send(&mut c, *which, payload).is_ok())
            {
                state.conn = Some(c);
            }
        })
        .map_err(|e| format!("signal connection: {e}"))?;
    let mut log = log;
    let status = relay(&mut conn, timing, log.as_mut(), started);
    {
        let mut state = lock(to);
        *state = Signals::default();
    }
    // Execs still waiting for the guest will never start.
    pending().clear();
    // The guest powers off once the host closes the run connection: shutting it down
    // closes every copy.
    let _ = conn.shutdown(std::net::Shutdown::Both);
    status
}

/// Copies the guest's frames to shards' stdout and stderr until the exit status, and to
/// `log` if the container's output is kept. Why a command never ran is not its output,
/// and stays out of the log, as dockerd keeps a failed start out of `docker logs`.
#[cfg(unix)]
fn relay(
    conn: &mut UnixStream,
    timing: &Timing,
    mut log: Option<&mut Logger>,
    started: Option<&(dyn Fn() + Sync)>,
) -> Result<Ended, String> {
    // Grown to the longest frame yet, at most MAX_PAYLOAD, and never shrunk: a frame
    // longer than any before zeroes only its new bytes before they are read over (audit
    // D08).
    let mut frame = Vec::new();
    let mut not_run = None;
    loop {
        let mut h = [0u8; run::HEADER];
        conn.read_exact(&mut h)
            .map_err(|e| format!("the guest stopped before the command ended: {e}"))?;
        let (which, len) = run::parse_header(h).ok_or("the guest sent a malformed frame")?;
        let len = len as usize;
        if frame.len() < len {
            frame.resize(len, 0);
        }
        let payload = frame.get_mut(..len).ok_or("a frame past its buffer")?;
        conn.read_exact(payload)
            .map_err(|e| format!("the guest stopped mid-frame: {e}"))?;
        let payload = &*payload;
        match which {
            kind::STDOUT => {
                let mut out = io::stdout().lock();
                // A closed stdout drops output, as a closed pipe does for `docker run`.
                let _ = out.write_all(payload).and_then(|()| out.flush());
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDOUT, payload);
                }
            }
            kind::STDERR => {
                let mut err = io::stderr().lock();
                let _ = err.write_all(payload).and_then(|()| err.flush());
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDERR, payload);
                }
            }
            kind::STARTED => {
                if let Some(started) = started {
                    started();
                }
            }
            kind::SYSTEM_ERR => not_run = Some(String::from_utf8_lossy(payload).into_owned()),
            kind::EXIT => {
                let _ = timing.answered_us.set(shards_vmm::log::uptime_us());
                let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                return Ok(Ended {
                    status: u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX),
                    not_run,
                    lost: log.as_deref().map_or(0, Logger::lost),
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// The head of a log record for `bytes` on `stream`, stamped now: its bytes follow it.
#[cfg(unix)]
fn log_head(stream: u8, bytes: &[u8]) -> [u8; LOG_HEAD as usize] {
    let [t0, t1, t2, t3, t4, t5, t6, t7] = u64::try_from(now()).unwrap_or(u64::MAX).to_be_bytes();
    let [l0, l1, l2, l3] = u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes();
    [stream, t0, t1, t2, t3, t4, t5, t6, t7, l0, l1, l2, l3]
}

/// Writes `parts` in order with as few writes as they take, a short or interrupted one
/// going on from where it stopped: a log record's head and bytes, without copying the
/// bytes after their head (audit D08).
#[cfg(unix)]
fn write_parts(mut w: impl Write, mut rest: &mut [io::IoSlice<'_>]) -> io::Result<()> {
    while !rest.is_empty() {
        match w.write_vectored(rest) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => io::IoSlice::advance_slices(&mut rest, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn send(conn: &mut UnixStream, which: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    conn.write_all(&run::header(which, len))?;
    conn.write_all(payload)
}

/// Execs waiting for the guest's connection: each one's token and where its connection
/// goes. Emptied when the workload ends, which tells each still waiting that it never
/// started.
#[cfg(unix)]
type Pending = Mutex<Vec<([u8; run::TOKEN], std::sync::mpsc::Sender<UnixStream>)>>;
#[cfg(unix)]
static PENDING: Pending = Mutex::new(Vec::new());
#[cfg(unix)]
static NEXT_EXEC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

#[cfg(unix)]
fn pending() -> std::sync::MutexGuard<'static, Vec<([u8; run::TOKEN], std::sync::mpsc::Sender<UnixStream>)>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes every guest connection to the exec port ([`run::EXEC_PORT`]): one whose
/// [`kind::HELLO`] names a pending exec's token becomes that exec's; any other is closed.
/// Each is read on a thread of its own, so that one that says nothing holds up no other;
/// the muxer bounds how many there are.
#[cfg(unix)]
pub fn accept_execs(port: Port) {
    for mut conn in port {
        let _ = std::thread::Builder::new()
            .name("exec-hello".into())
            .spawn(move || {
                let mut h = [0u8; run::HEADER];
                let mut token = [0u8; run::TOKEN];
                let hello = conn.read_exact(&mut h).ok().and_then(|()| run::parse_header(h));
                if hello != Some((kind::HELLO, run::TOKEN as u32)) || conn.read_exact(&mut token).is_err() {
                    return;
                }
                // Compared whole, in constant time: a workload that dials the port learns
                // nothing of a token from how fast it is refused.
                let same =
                    |t: &[u8; run::TOKEN]| t.iter().zip(&token).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0;
                let to = {
                    let mut waiting = pending();
                    waiting
                        .iter()
                        .position(|(t, _)| same(t))
                        .map(|i| waiting.swap_remove(i).1)
                };
                if let Some(to) = to {
                    let _ = to.send(conn);
                }
            });
    }
}

/// A command to run beside the workload (`docker exec`), and its client's: the
/// connection it is answered on, and its stdio.
#[cfg(unix)]
pub struct ExecRequest {
    pub spec: Spec,
    pub interactive: bool,
    pub detached: bool,
    pub client: UnixStream,
    pub stdin: fs::File,
    pub stdout: fs::File,
    pub stderr: fs::File,
}

/// Runs `req` beside the workload, on a thread of its own, answering its client as
/// `docker exec` answers: the command's output, or why it did not start, then `EXIT`
/// with its status (`-d`: 0 once it starts).
#[cfg(unix)]
pub fn exec(to: &'static ToGuest, req: ExecRequest) -> Result<(), String> {
    std::thread::Builder::new()
        .name("exec".into())
        .spawn(move || {
            let mut req = req;
            let status = match exec_session(to, &mut req) {
                Ok(Some(status)) => Some(status),
                Ok(None) => None,
                Err(e) => {
                    let _ = writeln!(req.stderr, "Error response from daemon: {e}");
                    Some(1)
                }
            };
            if let Some(status) = status {
                let _ = shards_ipc::send(&req.client, shards_ipc::kind::EXIT, &[status], &[]);
            }
        })
        .map(drop)
        .map_err(|e| format!("an exec's thread: {e}"))
}

/// One exec, from asking the guest for it to its status. `None` once a detached exec's
/// client has been told it started; what never started is said on `req.stderr`.
#[cfg(unix)]
fn exec_session(to: &'static ToGuest, req: &mut ExecRequest) -> Result<Option<u8>, String> {
    let mut token = [0u8; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("an exec's token: {e}"))?;
    let id = NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    pending().push((token, tx));
    let mut payload = Vec::with_capacity(run::TOKEN + 4 + req.spec.encoded_len().unwrap_or(0));
    payload.extend_from_slice(&token);
    payload.extend_from_slice(&id.to_be_bytes());
    req.spec.encode_into(&mut payload);
    if !to_guest(to, kind::EXEC, &payload) {
        pending().retain(|(t, _)| *t != token);
        return Err("the container is not running".into());
    }
    let mut conn = rx
        .recv()
        .map_err(|_| "the container's process ended before the command started".to_string())?;
    if req.interactive {
        let mut input = conn.try_clone().map_err(|e| e.to_string())?;
        let mut stdin = req.stdin.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("exec-stdin".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = stdin.read(&mut buf) {
                    if send(&mut input, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
                        return;
                    }
                }
                let _ = send(&mut input, kind::STDIN, &[]);
            })
            .map_err(|e| format!("an exec's stdin: {e}"))?;
    } else {
        // An exec that could not start may have said so and closed already: what it
        // said is still there to read.
        let _ = send(&mut conn, kind::STDIN, &[]);
    }
    if !req.detached {
        // The client's terminal sizes, for the exec's own terminal.
        let client = req.client.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("exec-client".into())
            .spawn(move || {
                while let Ok(Some(m)) = shards_ipc::recv(&client) {
                    match m.kind {
                        shards_ipc::kind::RESIZE if Size::decode(&m.payload).is_some() => {
                            let resize = [&id.to_be_bytes()[..], &m.payload].concat();
                            to_guest(to, kind::EXEC_RESIZE, &resize);
                        }
                        // The daemon's, for a health check past its timeout.
                        shards_ipc::kind::SIGNAL if m.payload.len() == 4 => {
                            let signal = [&id.to_be_bytes()[..], &m.payload].concat();
                            to_guest(to, kind::EXEC_SIGNAL, &signal);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| format!("an exec's client: {e}"))?;
    }
    let mut frame = Vec::new();
    let mut not_started: Option<(u8, String)> = None;
    loop {
        let mut h = [0u8; run::HEADER];
        conn.read_exact(&mut h)
            .map_err(|e| format!("the guest stopped before the command ended: {e}"))?;
        let (which, len) = run::parse_header(h).ok_or("the guest sent a malformed frame")?;
        let len = len as usize;
        if frame.len() < len {
            frame.resize(len, 0);
        }
        let payload = frame.get_mut(..len).ok_or("a frame past its buffer")?;
        conn.read_exact(payload)
            .map_err(|e| format!("the guest stopped mid-frame: {e}"))?;
        let payload = &*payload;
        match which {
            kind::STARTED if req.detached => {
                let _ = shards_ipc::send(&req.client, shards_ipc::kind::EXIT, &[0], &[]);
                return Ok(None);
            }
            kind::STARTED => {}
            // A detached exec's output goes nowhere, as `docker exec -d`'s does.
            kind::STDOUT if !req.detached => {
                let _ = req.stdout.write_all(payload);
            }
            kind::STDERR if !req.detached => {
                let _ = req.stderr.write_all(payload);
            }
            kind::STDOUT | kind::STDERR => {}
            kind::SYSTEM_ERR => {
                let (class, why) = payload.split_first().ok_or("an empty failure")?;
                not_started = Some((*class, String::from_utf8_lossy(why).into_owned()));
            }
            kind::EXIT => {
                let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                let status = u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX);
                return Ok(Some(match not_started {
                    None => status,
                    Some((run::exec_failed::DAEMON, why)) => {
                        let _ = writeln!(req.stderr, "Error response from daemon: {why}");
                        1
                    }
                    Some((_, why)) => {
                        // dockerd's code for what the runtime could not start (moby
                        // daemon/errors.go), in runc's words.
                        let (_, code) = shards_cmdline::commands::start_failed(&why);
                        let said = if why == "Cwd must be an absolute path" {
                            format!("OCI runtime exec failed: exec failed: {why}")
                        } else {
                            format!(
                                "OCI runtime exec failed: exec failed: unable to start container process: {why}"
                            )
                        };
                        let _ = writeln!(req.stderr, "{said}");
                        code
                    }
                }));
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// Copies shards' stdin to the workload's, then closes it.
#[cfg(unix)]
fn forward_stdin(conn: &mut UnixStream) {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if send(conn, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
        }
    }
    let _ = send(conn, kind::STDIN, &[]);
}

/// Forwards the signals shards receives to the workload, through `to`, even those shards
/// was started ignoring, as the Docker CLI does (shards_ipc::take_forwarded). It blocks
/// them in the calling thread, which every thread started later inherits, so call it
/// before the VM starts: then only the forwarder's `sigwait` receives them. A signal that
/// would end shards, arriving before the workload runs, ends shards as it would have,
/// unless it was ignored. With `reads_terminal`, the terminal's job control applies to
/// shards (shards_ipc::forwarded).
#[cfg(unix)]
pub fn forward_signals(to: &'static ToGuest, reads_terminal: bool) -> Result<(), String> {
    let (set, ignored) =
        shards_ipc::take_forwarded(reads_terminal).map_err(|e| format!("taking signals: {e}"))?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    return;
                }
                let Some(linux) = shards_ipc::linux_signal(sig) else {
                    continue;
                };
                let forwarded = signal_guest(to, linux);
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !forwarded && ends && !ignored.contains(&sig) {
                    // SAFETY: the default action of a terminating signal, on this process.
                    unsafe {
                        libc::signal(sig, libc::SIG_DFL);
                        let mut only: libc::sigset_t = std::mem::zeroed();
                        libc::sigemptyset(&mut only);
                        libc::sigaddset(&mut only, sig);
                        libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
                        libc::raise(sig);
                    }
                }
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    Ok(())
}

/// From here on the workload will run, so signals queue for it until the guest dials the
/// signal port rather than being refused: a warm VM calls it once it has taken a request.
#[cfg(unix)]
pub fn will_run(to: &ToGuest) {
    lock(to).running = true;
}

/// Sends Linux signal `linux` to the workload through `to`, or queues it until the guest
/// dials the signal port. Returns false before the workload runs, when there is no one to
/// send it to.
#[cfg(unix)]
pub fn signal_guest(to: &ToGuest, linux: u32) -> bool {
    to_guest(to, kind::SIGNAL, &linux.to_be_bytes())
}

/// Sizes the workload's terminal, if it has one, as [`signal_guest`] sends a signal.
#[cfg(unix)]
pub fn resize_guest(to: &ToGuest, size: Size) -> bool {
    to_guest(to, kind::RESIZE, &size.encode())
}

#[cfg(unix)]
fn to_guest(to: &ToGuest, which: u8, payload: &[u8]) -> bool {
    let mut guard = lock(to);
    let state = &mut *guard;
    match (&mut state.conn, state.running) {
        (Some(conn), _) => {
            if send(conn, which, payload).is_err() {
                state.conn = None;
            }
            true
        }
        (None, true) => {
            state.queued.push((which, payload.to_vec()));
            true
        }
        (None, false) => false,
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::segments::{log_segment, new_segment};

    /// A directory of its own, removed when dropped, whether its test passes or panics.
    struct Temp(std::path::PathBuf);

    impl std::ops::Deref for Temp {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for Temp {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(tag: &str) -> Temp {
        let dir = std::env::temp_dir().join(format!("shards-logger-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }

    /// Segments made in `dir`, as the daemon makes a run's.
    fn in_dir(dir: &Path, retention: LogRetention) -> NextSegment {
        let dir = fs::File::open(dir).unwrap();
        Box::new(move |seq| new_segment(&dir, seq, retention.files))
    }

    fn append(path: &Path) -> fs::File {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    }

    /// The exec port hands a connection only to the exec whose token its HELLO names:
    /// one naming another, or saying anything else first, is closed, and the exec still
    /// gets its own.
    #[test]
    fn exec_connections_go_only_to_the_exec_they_name() {
        let (port, conns) = std::sync::mpsc::channel();
        std::thread::spawn(move || accept_execs(conns));
        let token = [7u8; run::TOKEN];
        let (to, arrived) = std::sync::mpsc::channel();
        pending().push((token, to));
        let hello = |token: &[u8]| {
            let (mut guest, host) = UnixStream::pair().unwrap();
            guest
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            guest
                .write_all(&run::header(kind::HELLO, token.len() as u32))
                .unwrap();
            guest.write_all(token).unwrap();
            port.send(host).unwrap();
            guest
        };
        let mut wrong = [7u8; run::TOKEN];
        wrong[run::TOKEN - 1] = 8;
        let refused = hello(&wrong);
        let short = hello(&token[..run::TOKEN - 1]);
        for mut closed in [refused, short] {
            // Closed with bytes it never read, as the short one is, Linux resets it rather
            // than ending it (af_unix.c, unix_release_sock).
            match closed.read(&mut [0u8; 1]) {
                Ok(0) => {}
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("a connection naming no exec stays open: {other:?}"),
            }
        }
        assert!(
            arrived.try_recv().is_err(),
            "an exec took a connection not its own"
        );
        let _ours = hello(&token);
        assert!(arrived.recv_timeout(std::time::Duration::from_secs(5)).is_ok());
        assert!(pending().is_empty());
    }

    /// A logger keeps each record whole and indexes it, with its stream and whether it
    /// ends a line; a record that cannot be kept, in either file, is taken back from both
    /// and counted as lost, and the next is kept whole after it (audit A12).
    #[test]
    fn a_logger_keeps_records_whole_or_counts_them_lost() {
        let dir = temp("keep");
        let (log, index) = (dir.join("log"), dir.join("log.idx"));
        let keep = LogRetention {
            size: u64::MAX,
            files: 1,
        };
        let mut logger = Logger::new(append(&log), append(&index), keep, in_dir(&dir, keep)).unwrap();
        logger.keep(LOG_STDOUT, b"a\n");
        logger.keep(LOG_STDERR, b"b");
        logger.keep(LOG_STDOUT, b"");
        assert_eq!(logger.lost(), 0);
        let (bytes, entries) = (fs::read(&log).unwrap(), fs::read(&index).unwrap());
        assert_eq!(bytes.len(), 2 * 13 + 3, "no record of nothing");
        let entry = |i: usize| u64::from_be_bytes(entries[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!(entries.len(), 16);
        assert_eq!(entry(0), INDEX_LINE);
        assert_eq!(entry(1), 15 | INDEX_STDERR);

        // A log that cannot be written, then an index that cannot.
        for broken in ["log", "index"] {
            let (l, i) = if broken == "log" {
                (fs::File::open(&log).unwrap(), append(&index))
            } else {
                (append(&log), fs::File::open(&index).unwrap())
            };
            let mut logger = Logger::new(l, i, keep, in_dir(&dir, keep)).unwrap();
            logger.keep(LOG_STDOUT, b"lost\n");
            assert_eq!(logger.lost(), 5, "{broken}");
            assert_eq!(
                fs::read(&log).unwrap(),
                bytes,
                "{broken}: a record left in the log"
            );
            assert_eq!(fs::read(&index).unwrap(), entries, "{broken}: an entry left");
        }
        let mut logger = Logger::new(append(&log), append(&index), keep, in_dir(&dir, keep)).unwrap();
        logger.keep(LOG_STDOUT, b"c\n");
        assert_eq!(fs::read(&index).unwrap().len(), 24);
        assert_eq!(
            u64::from_be_bytes(fs::read(&index).unwrap()[16..24].try_into().unwrap()),
            bytes.len() as u64 | INDEX_LINE,
            "kept whole after the loss"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A log goes on in segments of at most its retention's size, a record too big for
    /// any alone in one, and keeps only its retention's newest files; a segment that
    /// cannot be started costs the records that would go in it, and leaves nothing of
    /// itself (audit A12).
    #[test]
    fn a_log_rotates_and_keeps_its_newest_segments() {
        let dir = temp("rotate");
        let retention = LogRetention { size: 64, files: 3 };
        let first = || (append(&dir.join("log")), append(&dir.join("log.idx")));
        let (log, index) = first();
        let mut logger = Logger::new(log, index, retention, in_dir(&dir, retention)).unwrap();
        // A first record past a segment's size is the first segment's, not an empty one's
        // successor's.
        logger.keep(LOG_STDOUT, &[b'_'; 70]);
        assert_eq!(fs::read(dir.join("log")).unwrap().len(), 83);
        assert!(!dir.join("log.1").exists());
        fs::remove_file(dir.join("log")).unwrap();
        fs::remove_file(dir.join("log.idx")).unwrap();
        let (log, index) = first();
        let mut logger = Logger::new(log, index, retention, in_dir(&dir, retention)).unwrap();
        // 13 + 20 bytes a record: one a segment, since a second would pass 64.
        for i in 0..10u8 {
            logger.keep(LOG_STDOUT, &[b'a' + i; 20]);
        }
        logger.keep(LOG_STDERR, &[b'z'; 100]);
        assert_eq!(logger.lost(), 0);
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["log.10", "log.10.idx", "log.8", "log.8.idx", "log.9", "log.9.idx"]
        );
        for (seq, byte, len) in [(8, b'i', 20), (9, b'j', 20), (10, b'z', 100)] {
            let (l, i) = log_segment(seq);
            let bytes = fs::read(dir.join(l)).unwrap();
            assert_eq!(bytes.len(), 13 + len, "segment {seq}");
            assert!(bytes[13..].iter().all(|&b| b == byte), "segment {seq}");
            assert_eq!(fs::read(dir.join(i)).unwrap().len(), 8, "segment {seq}");
        }

        // The next segment's log there already, then its index: neither is taken, and
        // what is there stays.
        for taken in [0, 1] {
            let name = if taken == 0 { "log.11" } else { "log.11.idx" };
            fs::write(dir.join(name), b"taken").unwrap();
            logger.keep(LOG_STDOUT, &[b'q'; 60]);
            assert_eq!(logger.lost(), 60 * (taken + 1));
            assert_eq!(fs::read(dir.join(name)).unwrap(), b"taken");
            let other = if taken == 0 { "log.11.idx" } else { "log.11" };
            assert!(!dir.join(other).exists(), "{other} left of a segment not started");
            fs::remove_file(dir.join(name)).unwrap();
        }
        logger.keep(LOG_STDOUT, &[b'r'; 60]);
        assert_eq!(logger.lost(), 120);
        assert_eq!(fs::read(dir.join("log.11")).unwrap().len(), 73);
        assert!(!dir.join("log.8.idx").exists() && !dir.join("log.8").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Takes at most five bytes a call, and is interrupted every third.
    struct Grudging {
        got: Vec<u8>,
        calls: usize,
    }

    impl Write for Grudging {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_vectored(&[io::IoSlice::new(buf)])
        }

        fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls.is_multiple_of(3) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let mut n = 0;
            for b in bufs {
                let take = b.len().min(5 - n);
                self.got.extend_from_slice(&b[..take]);
                n += take;
                if n == 5 {
                    break;
                }
            }
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A record's head and bytes go out whole and in order through short and interrupted
    /// writes, with its length in its head.
    #[test]
    fn records_are_written_whole_through_short_writes() {
        let bytes = b"one line of output\n";
        let head = log_head(LOG_STDERR, bytes);
        assert_eq!(head[0], LOG_STDERR);
        assert_eq!(head[9..], (bytes.len() as u32).to_be_bytes());
        let mut w = Grudging {
            got: Vec::new(),
            calls: 0,
        };
        write_parts(&mut w, &mut [io::IoSlice::new(&head), io::IoSlice::new(bytes)]).unwrap();
        assert_eq!(w.got, [&head[..], bytes].concat());
    }
}
