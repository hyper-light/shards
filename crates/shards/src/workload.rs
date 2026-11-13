//! Runs a command in a microVM booted into an image: the host half of the run protocol
//! (docs/design/architecture.md D16). The guest dials in once the image is mounted, and
//! shards sends the workload, relays its stdio, and returns its exit status as
//! `docker run` does.

use std::fs;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use shards_abi::run::{self, Size, Spec, kind};
// A connection the guest dialed on the vsock device, as the VMM hands it over: a Unix
// socket, Winsock's AF_UNIX on Windows (D195). A client's and the daemon's are
// `shards_ipc::Stream`s, which are Unix sockets too on Unix.
use shards_abi::ring;
use shards_vmm::devices::virtio::vsock::ring::RingHandle;
use shards_vmm::platform::UnixStream;

#[path = "workload/capture.rs"]
pub mod capture;
#[path = "workload/hellos.rs"]
mod hellos;
pub use crate::log_writer::NextSegment;
use crate::log_writer::Writer;
pub use hellos::exec_port;
use hellos::register;

use crate::spec::{LOG_STDERR, LOG_STDOUT, LogRetention};

/// Where the guest's connections to a host port this process serves arrive
/// ([`shards_vmm::vm::VsockHost::ports`]).
pub type Port = std::sync::mpsc::Receiver<UnixStream>;

/// The engine's port (D181): each guest connection, with the handle a ring its relay
/// offers is attached through (D230).
pub type EnginePort = std::sync::mpsc::Receiver<(UnixStream, RingHandle)>;

/// The workload's control (D16): its signals, its terminal's sizes, its execs and the host's
/// answers on its network, as frames on the guest's signal connection ([`Link`]).
#[derive(Debug, Default)]
pub struct Signals {
    link: Link,
}

impl Signals {
    pub const fn new() -> Signals {
        Signals { link: Link::Idle }
    }
}

/// Where the workload's control stands, and so what a frame for it meets.
#[derive(Debug, Default)]
enum Link {
    /// No workload runs, nor is sure to, or its run has ended: a frame has no one to go to.
    #[default]
    Idle,
    /// The workload runs, or is sure to, and its guest has yet to dial (a restored guest
    /// dials again, D176): frames wait for it, one for each thing pending ([`Pending`]).
    Awaiting(Pending),
    /// The guest's connection, of which its sender ([`send_frames`]) owns the sending and
    /// [`watch`] the reading: frames wait here for the sender, one for each thing pending,
    /// and it sends them in order with the lock let go, so that a guest that reads slowly
    /// or not at all (its vCPUs held: a pause, a save) holds that thread alone.
    Connected(Pending),
    /// It failed while the run went on: frames are refused. The guest dials the signal port
    /// once, so that a workload dialing it finds it taken (AGENTFILE_ARCH.md §9.7), and only
    /// a copy restored from a save dials again, in a VM process of its own (D176): nothing
    /// reaches this workload's control again.
    Lost(Lost),
}

/// Why a frame for the workload's control did not go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsent {
    /// No workload runs, nor is sure to, or its run has ended.
    NotRunning,
    /// Linux has no signal of this number: the guest would drop it.
    NoSuchSignal(u32),
    /// No memory to hold it until the guest dials.
    Unheld,
    /// The guest's signal connection failed.
    Lost(Lost),
}

impl std::fmt::Display for Unsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => f.write_str("the container is not running"),
            Self::NoSuchSignal(n) => write!(f, "Linux has no signal {n}"),
            Self::Unheld => f.write_str("no memory to hold it until the guest dials"),
            Self::Lost(lost) => write!(f, "the guest's signal connection is lost: {lost}"),
        }
    }
}

impl std::error::Error for Unsent {}

/// How the guest's signal connection failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    /// The guest closed it without saying its relay's end ([`kind::SIGNALS_END`]).
    Closed,
    /// The guest sent on it what it never sends.
    Protocol,
    /// The host could not take it as a restored guest dialed it again (D176).
    Untaken,
    /// A send on it, a read of it, or a copy of it to read failed: the error's kind, and
    /// the OS's code where there was one.
    Failed { kind: io::ErrorKind, os: Option<i32> },
}

impl Lost {
    fn of(e: &io::Error) -> Lost {
        Lost::Failed {
            kind: e.kind(),
            os: e.raw_os_error(),
        }
    }
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Closed => f.write_str("the guest closed it before its end"),
            Self::Protocol => f.write_str("the guest sent on it what it never sends"),
            Self::Untaken => f.write_str("its restored guest's dial could not be taken"),
            Self::Failed { os: Some(code), .. } => write!(f, "{}", io::Error::from_raw_os_error(code)),
            Self::Failed { kind, os: None } => write!(f, "{}", io::Error::from(kind)),
        }
    }
}

/// Frames waiting for the guest's dial, or for its connection's sender, in the order they
/// came, one for each thing pending: each signal once, as Linux holds a standard signal
/// pending once (Linux 6.18 kernel/signal.c l. 1037, `legacy_queue`); the workload's
/// terminal's latest size; each exec's start, and its signals and its terminal's latest size;
/// the latest answer to each of the guest's asks on the host's network (D124). So they are
/// never more than [`run::NSIG`] signals and a size for the workload and for each exec, its
/// start, and an answer for each ask; and the sender holds one batch of them at a time.
#[derive(Debug, Default)]
struct Pending(Vec<(u8, Vec<u8>)>);

impl Pending {
    /// Holds frame `which`: in the place of the one waiting that it stands for
    /// ([`stands_for`]), or after those waiting. Before the guest dials (`dialed` false), an
    /// exec's signal or size waits only where its start does: no exec of this workload is
    /// in its guest yet.
    fn hold(&mut self, which: u8, payload: &[u8], dialed: bool) -> Result<(), Unsent> {
        if !dialed && matches!(which, kind::EXEC_SIGNAL | kind::EXEC_RESIZE) {
            let id = payload.get(..4).ok_or(Unsent::NotRunning)?;
            if !self
                .0
                .iter()
                .any(|(w, p)| *w == kind::EXEC && exec_id(p) == Some(id))
            {
                return Err(Unsent::NotRunning);
            }
        }
        let held = stands_for(which, payload);
        let mut copy = Vec::new();
        copy.try_reserve_exact(payload.len())
            .map_err(|_| Unsent::Unheld)?;
        copy.extend_from_slice(payload);
        match self
            .0
            .iter_mut()
            .find(|(w, p)| *w == which && stands_for(*w, p) == held)
        {
            Some((_, waiting)) => *waiting = copy,
            None => {
                self.0.try_reserve(1).map_err(|_| Unsent::Unheld)?;
                self.0.push((which, copy));
            }
        }
        Ok(())
    }
}

/// What a frame waiting for the guest's dial stands for, of which one waits: the workload's
/// terminal for its size; an exec's id for its start and its size; an ask's number for its
/// answer; a signal's own bytes, its exec's id and its number.
fn stands_for(which: u8, payload: &[u8]) -> &[u8] {
    match which {
        kind::RESIZE => &[],
        kind::EXEC => exec_id(payload).unwrap_or(payload),
        kind::EXEC_RESIZE | kind::BOUND => payload.get(..4).unwrap_or(payload),
        _ => payload,
    }
}

/// The id an exec's frame carries after its token ([`exec_frame`]).
fn exec_id(payload: &[u8]) -> Option<&[u8]> {
    payload.get(run::TOKEN..run::TOKEN + 4)
}

/// Where signals for the workload go: its link, and what its connection's sender waits on.
/// A VM process serves one workload, so its own is a `static` that the threads relaying
/// signals, which live as long as the process, share.
#[derive(Debug)]
pub struct ToGuest {
    state: Mutex<Signals>,
    /// Told as frames come for the sender, and as the link leaves `Connected`.
    changed: Condvar,
}

impl ToGuest {
    pub const fn new(signals: Signals) -> ToGuest {
        ToGuest {
            state: Mutex::new(signals),
            changed: Condvar::new(),
        }
    }
}

fn lock(to: &ToGuest) -> std::sync::MutexGuard<'_, Signals> {
    to.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// When a request arrived and when it was answered, on the VMM's clock (for
/// `SHARDS_TIMING`).
#[derive(Debug, Default)]
pub struct Timing {
    pub request_us: std::sync::OnceLock<u128>,
    pub answered_us: std::sync::OnceLock<u128>,
    /// The request asked for the timing line (a warm VM's client set `SHARDS_TIMING`).
    pub asked: std::sync::atomic::AtomicBool,
}

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
pub enum Request<'a> {
    /// Known before the guest connects.
    Now { spec: Box<Spec>, interactive: bool },
    /// Asked for once the guest is connected and waiting: a warm VM's request.
    Later(&'a dyn Fn() -> Result<Asked<'a>, String>),
}

/// A request, as served: the command, whether it reads stdin, the microVM's log if its
/// output is kept, and what to call once the command runs.
pub struct Asked<'a> {
    pub spec: Spec,
    pub capture: Option<capture::Given>,
    pub interactive: bool,
    /// Its client does not stay: its stdin, if it reads one, is for `shards attach`.
    pub detached: bool,
    pub log: Option<Logger>,
    pub started: Option<&'a (dyn Fn() + Sync)>,
    /// The microVM's writable layer from before, to put back before the command runs
    /// (D37); and where its layer goes once it stops.
    pub layer_in: Option<fs::File>,
    /// Its microVM's layer device has the microVM's layer kept in its file readied for
    /// it (D190): the guest is told to go on over it ([`kind::OVER`]) in a put-back layer's
    /// place.
    pub layer_over: bool,
    pub layer_out: Option<fs::File>,
    /// Told how the command ended, before its layer is saved: a run's client waits for
    /// nothing it does not need.
    pub told: Option<&'a Told<'a>>,
    /// Told the layer is saved whole.
    pub saved: Option<&'a (dyn Fn(Option<u64>) + Sync)>,
    /// Told the layer stays in its own file (D190), its guest's journal written in place:
    /// where the VM's layer is one and no joiner of its network outlives the
    /// run, in place of a save.
    pub kept: Option<&'a (dyn Fn(Option<u64>) + Sync)>,
    /// Given the records of the processes the run's members start and end (D123).
    pub processes: Option<&'a Records<'a>>,
    /// Given each member's end, where the run asked to hear them (D231): its
    /// `MEMBER_ENDED` payload.
    pub members: Option<&'a Records<'a>>,
    /// Given each move of its command's readiness under the nesting grant (D234): its
    /// `READY` payload.
    pub readiness: Option<&'a Records<'a>>,
    /// Given its guest's asks of the host on the host's network (D124): each frame's kind,
    /// then its payload.
    pub host_net: Option<&'a Records<'a>>,
    /// A run its microVM was restored from a save of (D176): no command to send, its
    /// command running already, and its connections the ones its guest dials again, by the
    /// save's tokens ([`await_resumed`]).
    pub resumed: Option<Resumed>,
}

/// The connections of a run its microVM was restored from a save of (D176), as its guest
/// dials them again: the workload's, then the signals'.
pub struct Resumed {
    run: std::sync::mpsc::Receiver<Result<UnixStream, String>>,
    signals: std::sync::mpsc::Receiver<Result<UnixStream, String>>,
}

/// Waits for the connections a restored guest dials again with `tokens` (D176), the
/// workload's and the signals', as an exec's connection is waited for: by its HELLO on the
/// exec port, which a workload dialing it learns nothing of a token from.
pub fn await_resumed(tokens: &[u8]) -> Result<Resumed, String> {
    let (Some((run, rest)), true) = (
        tokens.split_first_chunk::<{ run::TOKEN }>(),
        tokens.len() == 2 * run::TOKEN,
    ) else {
        return Err(format!("a resumed run's tokens are {} bytes", tokens.len()));
    };
    let signal: [u8; run::TOKEN] = rest.try_into().map_err(|_| "a resumed run's signals token")?;
    let wait = |token: [u8; run::TOKEN]| -> Result<_, String> {
        let (to, from) = std::sync::mpsc::sync_channel(1);
        register(token, Target::Resumed(to))?;
        Ok(from)
    };
    let run_token = run;
    let run = wait(*run_token)?;
    let signals = match wait(signal) {
        Ok(signals) => signals,
        Err(e) => {
            hellos::forget(run_token);
            return Err(e);
        }
    };
    Ok(Resumed { run, signals })
}

#[derive(Debug)]
pub enum QuiesceError {
    InvalidRequest,
    Ended,
    Busy,
    NotStarted,
    Transport(io::Error),
    Protocol(run::quiesce::DecodeError),
    Refused(run::quiesce::Refused),
}

impl std::fmt::Display for QuiesceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest => f.write_str("a save's restore tokens have the wrong length"),
            Self::Ended => f.write_str("its command has ended"),
            Self::Busy => f.write_str("a save of its command is already pending"),
            Self::NotStarted => f.write_str("its command has not started"),
            Self::Transport(error) => write!(f, "asking its guest: {error}"),
            Self::Protocol(error) => write!(f, "its guest sent an invalid save response: {error}"),
            Self::Refused(refusal) => write!(f, "its guest refused the save: {refusal}"),
        }
    }
}

impl std::error::Error for QuiesceError {}

/// The run relay's checkpoint request and its typed terminal reply (D176).
enum Quiescing {
    Idle,
    Waiting([u8; run::quiesce::REQUEST_BYTES]),
    Quiet,
    Refused(run::quiesce::Refused),
    Ended {
        protocol: Option<run::quiesce::DecodeError>,
    },
}

impl Quiescing {
    fn begin(&mut self, request: [u8; run::quiesce::REQUEST_BYTES]) -> Result<(), QuiesceError> {
        match self {
            Self::Idle => {
                *self = Self::Waiting(request);
                Ok(())
            }
            Self::Ended { .. } => Err(QuiesceError::Ended),
            _ => Err(QuiesceError::Busy),
        }
    }

    fn cancel(&mut self, request: &[u8; run::quiesce::REQUEST_BYTES]) {
        if matches!(self, Self::Waiting(held) if held == request) {
            *self = Self::Idle;
        }
    }

    fn quiet(&mut self) {
        if matches!(self, Self::Waiting(_)) {
            *self = Self::Quiet;
        }
    }

    fn refuse(&mut self, refusal: run::quiesce::Refused) {
        if matches!(self, Self::Waiting(request) if refusal.matches(request)) {
            *self = Self::Refused(refusal);
        }
    }

    fn end(&mut self) {
        if !matches!(self, Self::Ended { .. }) {
            *self = Self::Ended { protocol: None };
        }
    }

    fn protocol_failed(&mut self, error: run::quiesce::DecodeError) {
        *self = Self::Ended {
            protocol: Some(error),
        };
    }

    fn outcome(&mut self) -> Option<Result<(), QuiesceError>> {
        match self {
            Self::Idle | Self::Waiting(_) => None,
            Self::Ended { protocol } => Some(Err(match *protocol {
                Some(error) => QuiesceError::Protocol(error),
                None => QuiesceError::Ended,
            })),
            Self::Quiet => {
                *self = Self::Idle;
                Some(Ok(()))
            }
            Self::Refused(refusal) => {
                let error = QuiesceError::Refused(*refusal);
                *self = Self::Idle;
                Some(Err(error))
            }
        }
    }
}

static QUIESCING: Mutex<Quiescing> = Mutex::new(Quiescing::Idle);
static QUIET: std::sync::Condvar = std::sync::Condvar::new();

fn quiescing() -> std::sync::MutexGuard<'static, Quiescing> {
    QUIESCING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits for the guest's cut or a correlated refusal before the VMM saves it (D176).
/// The two restore tokens remain private to this control path.
pub fn quiesce(tokens: &[u8]) -> Result<(), QuiesceError> {
    let request = tokens.try_into().map_err(|_| QuiesceError::InvalidRequest)?;
    quiescing().begin(request)?;
    let sent = match lock_stdin().as_mut() {
        Some(conn) => send(conn, kind::QUIESCE, tokens).map_err(QuiesceError::Transport),
        None => Err(QuiesceError::NotStarted),
    };
    if let Err(error) = sent {
        quiescing().cancel(&request);
        return Err(error);
    }
    let mut q = quiescing();
    loop {
        if let Some(outcome) = q.outcome() {
            return outcome;
        }
        q = QUIET.wait(q).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Lets the guest go on after a save, taken or not (D176).
pub fn resume_guest() {
    if let Some(conn) = lock_stdin().as_mut() {
        let _ = send(conn, kind::RESUME, &[]);
    }
}

/// What hears how a served command ended (`Asked::told`).
pub type Told<'a> = dyn Fn(&Result<Ended, String>) + Sync + 'a;

/// What is given a run's records of processes as they come (`Asked::processes`, D123).
pub type Records<'a> = dyn Fn(&[u8]) + Sync + 'a;

/// How a served command ended: its exit status, and if it never ran, why not, in the
/// guest's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: u8,
    pub not_run: Option<String>,
    /// Bytes of its output its log could not keep.
    pub lost: u64,
    /// Bytes of its output a non-blocking log dropped, as they came while its queue was
    /// full (D162).
    pub dropped: u64,
    /// What its init said as the kernel killed a process of it for want of memory: what
    /// its cgroup held and was allowed, and the kernel's words (D159); none where none was.
    pub oom: Option<Vec<u8>>,
    /// What its init said as the kernel refused it a process or thread past its pids
    /// limit: what its cgroup was allowed and held, and how many (D193); none where none
    /// was.
    pub pids: Option<Vec<u8>>,
}

/// Keeps a microVM's output in its log ([`Writer`], log_writer.rs): each record at once,
/// or, non-blocking (D162), through a queue of its own ([`Queue`]).
pub struct Logger {
    writer: Writer,
    /// Non-blocking (D162): the most bytes of output queued for it ([`Queue`]); none
    /// where the run waits for its log.
    queue: Option<u64>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("writer", &self.writer)
            .field("queue", &self.queue)
            .finish()
    }
}

impl Logger {
    /// Writes a microVM's log from its segment `seq`, `log` and `index`, which its
    /// daemon made with the microVM (or its newest, for one started again), going on in
    /// the segments `next` makes.
    pub fn new(
        log: fs::File,
        index: fs::File,
        seq: u64,
        retention: LogRetention,
        next: NextSegment,
    ) -> io::Result<Logger> {
        Ok(Logger {
            writer: Writer::new(log, index, seq, retention, next)?,
            queue: None,
        })
    }

    /// Non-blocking where `queue` is some: the most bytes of output queued for it.
    pub fn with_queue(mut self, queue: Option<u64>) -> Logger {
        self.queue = queue;
        self
    }

    fn keep(&mut self, stream: u8, bytes: &[u8]) {
        self.writer.keep(stream, bytes);
    }

    /// Bytes of output not kept.
    pub fn lost(&self) -> u64 {
        self.writer.lost()
    }
}

/// Where a run's output goes to be kept: its log, at once; or a non-blocking log's queue.
enum Sink<'a> {
    Now(&'a mut Logger),
    Queued(&'a Queue),
}

impl Sink<'_> {
    fn keep(&mut self, stream: u8, bytes: &[u8]) {
        match self {
            Sink::Now(log) => log.keep(stream, bytes),
            Sink::Queued(queue) => queue.push(stream, bytes),
        }
    }
}

/// A non-blocking log's queue (`--log-opt mode=non-blocking`, D162): the run's output as
/// it comes, at most `max` bytes of it, for the log's own thread to keep, so that a log
/// that falls behind never holds up its run. What comes while it is full is dropped and
/// counted, as moby's ring logger drops it (daemon/logger/ring.go, `Enqueue`): output that
/// would pass `max` is taken only into an empty queue.
struct Queue {
    max: u64,
    state: Mutex<Queued>,
    changed: Condvar,
}

#[derive(Default)]
struct Queued {
    items: std::collections::VecDeque<(u8, Vec<u8>)>,
    bytes: u64,
    closed: bool,
    dropped: u64,
}

impl Queue {
    fn new(max: u64) -> Queue {
        Queue {
            max,
            state: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queued> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, stream: u8, bytes: &[u8]) {
        let len = bytes.len() as u64;
        let mut q = self.lock();
        if q.bytes.saturating_add(len) > self.max && !q.items.is_empty() {
            q.dropped = q.dropped.saturating_add(len);
            return;
        }
        q.items.push_back((stream, bytes.to_vec()));
        q.bytes = q.bytes.saturating_add(len);
        drop(q);
        self.changed.notify_one();
    }

    /// No more comes: its thread returns once it has kept what is queued.
    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// Keeps what is queued in `log`, in turn, until it is closed and empty.
    fn keep_into(&self, log: &mut Logger) {
        loop {
            let (stream, bytes) = {
                let mut q = self.lock();
                loop {
                    if let Some(item) = q.items.pop_front() {
                        q.bytes = q.bytes.saturating_sub(item.1.len() as u64);
                        break item;
                    }
                    if q.closed {
                        return;
                    }
                    q = self.changed.wait(q).unwrap_or_else(PoisonError::into_inner);
                }
            };
            log.keep(stream, &bytes);
        }
    }

    fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

/// Runs `relay` with the run's output kept through `log`'s sink: the log itself, or, where
/// it is non-blocking, its queue, which a thread of its own keeps in it, all of it kept
/// before this returns. What `relay` returned, then the bytes the log lost and dropped.
fn with_sink<T>(
    log: Option<&mut Logger>,
    relay: impl FnOnce(Option<&mut Sink<'_>>) -> T,
) -> Result<(T, u64, u64), String> {
    let Some(log) = log else {
        return Ok((relay(None), 0, 0));
    };
    let Some(max) = log.queue else {
        let out = relay(Some(&mut Sink::Now(&mut *log)));
        return Ok((out, log.lost(), 0));
    };
    let queue = Queue::new(max);
    std::thread::scope(|s| {
        let queue = &queue;
        let writer = std::thread::Builder::new()
            .name("log".into())
            .spawn_scoped(s, move || {
                queue.keep_into(log);
                log.lost()
            })
            .map_err(|e| format!("the log's thread: {e}"))?;
        let out = relay(Some(&mut Sink::Queued(queue)));
        queue.close();
        let lost = writer
            .join()
            .map_err(|_| "the log's thread ended in a panic".to_string())?;
        Ok((out, lost, queue.dropped()))
    })
}

/// Serves the guest: sends the command, relays stdio, and returns how the workload ended.
/// Signals go through `to`, on the connection the guest makes to `signals`. The command
/// goes only to a guest whose init announced this build's contract, `guest_abi`
/// (shards_abi::control::ABI): an init built for another shards could misread it.
/// A run its microVM was restored from a save of (D176) has no ports of its own: its
/// guest dials the exec port again instead ([`Asked::resumed`]).
pub fn serve(
    ports: Option<(&Port, Port)>,
    request: Request<'_>,
    to: &'static ToGuest,
    timing: &Timing,
    guest_abi: &dyn Fn() -> Option<u64>,
) -> Result<Ended, String> {
    let (listener, signals) = match ports {
        Some((listener, signals)) => (Some(listener), Some(signals)),
        None => (None, None),
    };
    let first = listener
        .map(|l| {
            l.recv()
                .map_err(|_| "the microVM ended before its guest connected".to_string())
        })
        .transpose()?;
    let Asked {
        mut spec,
        capture,
        interactive,
        detached,
        log,
        started,
        layer_in,
        layer_over,
        layer_out,
        told,
        saved,
        kept,
        processes,
        members,
        readiness,
        host_net,
        resumed,
    } = match request {
        Request::Now { spec, interactive } => Asked {
            spec: *spec,
            capture: None,
            interactive,
            detached: false,
            log: None,
            started: None,
            layer_in: None,
            layer_over: false,
            layer_out: None,
            told: None,
            saved: None,
            kept: None,
            processes: None,
            members: None,
            readiness: None,
            host_net: None,
            resumed: None,
        },
        Request::Later(ask) => ask()?,
    };
    // A restored run's connections, as its guest dials them again (D176): the workload's
    // now, the signals' on their own thread, below. Its command runs already.
    let (mut conn, resumed_signals) = match (first, resumed) {
        (Some(conn), None) => (conn, None),
        (None, Some(Resumed { run, signals })) => {
            let conn = run
                .recv()
                .map_err(|_| "the microVM ended before its guest dialed again".to_string())??;
            if let Some(started) = started {
                started();
            }
            (conn, Some(signals))
        }
        _ => return Err("a run with no connection to come".into()),
    };
    let resuming = resumed_signals.is_some();
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
    let capture_control = match capture {
        Some(capture) => {
            if resuming {
                return Err("managed output restore generation is not admitted".into());
            }
            Some(capture::start(&mut spec, capture)?)
        }
        None => None,
    };
    if !resuming {
        if let Some(layer) = layer_in {
            send_layer(&mut conn, layer).map_err(|e| format!("sending the container's files: {e}"))?;
        } else if layer_over {
            send(&mut conn, kind::OVER, &[]).map_err(|e| format!("sending the container's layer: {e}"))?;
        }
        send(&mut conn, kind::SPEC, &spec.encode()).map_err(|e| format!("sending the command: {e}"))?;
    }
    // The command's stdin, which its client, then those attached, write (`attach`); a
    // detached run's stays open for them, an attached run's closes with its client's
    // (Docker's StdinOnce).
    *lock_stdin() = Some(conn.try_clone().map_err(|e| e.to_string())?);
    // moby's CopyStreams: a client's stdin ending closes the command's where it is
    // StdinOnce and has no terminal; with one, it is the client leaving, by its detach
    // keys or not, and the command keeps its stdin.
    STDIN_ONCE.store(!detached && spec.tty.is_none(), Ordering::Relaxed);
    STDIN_OPEN.store(interactive, Ordering::Relaxed);
    if interactive && !detached {
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(forward_stdin)
            .map_err(|e| format!("stdin: {e}"))?;
    } else if !interactive && !resuming {
        send(&mut conn, kind::STDIN, &[]).map_err(|e| format!("closing stdin: {e}"))?;
    }
    will_run(to);
    let boundary = capture_control
        .as_ref()
        .map(|_| conn.try_clone())
        .transpose()
        .map_err(|e| format!("managed control boundary: {e}"))?;
    std::thread::Builder::new()
        .name("signal-conn".into())
        .spawn(move || {
            // Ends with the microVM, if the guest never dials.
            let came = match (signals, resumed_signals) {
                (Some(port), _) => port.recv().ok(),
                (None, Some(again)) => match again.recv() {
                    Ok(Ok(conn)) => Some(conn),
                    Ok(Err(_)) => return lose(to, Lost::Untaken),
                    Err(_) => None,
                },
                (None, None) => None,
            };
            if let Some(conn) = came {
                take_signals(to, conn, capture_control.zip(boundary));
            }
        })
        .map_err(|e| format!("signal connection: {e}"))?;
    let mut log = log;
    // One reader for all the guest says on the run's connection, so that what it read
    // past one part is the next part's (review 8.20's reads, which take what has come).
    let mut reader = io::BufReader::with_capacity(run::BUFFERED, shards_ipc::Polled(&conn));
    let records = RunRecords {
        processes,
        members,
        readiness,
        host_net,
    };
    let status = relay(&mut reader, timing, log.as_mut(), started, records);
    // A save waiting for the guest to go quiet waits no more: its command has ended.
    quiescing().end();
    QUIET.notify_all();
    if let Some(told) = told {
        told(&status);
    }
    // What the guest says past its command's end, as the relay passes it on: the records
    // of the processes its joiners start and end (D123), and their binds on the host's
    // network (D124), which go on past it (D119).
    let mut beyond = |which: u8, payload: &[u8]| match which {
        kind::PROCESS => {
            if let Some(processes) = processes {
                processes(payload);
            }
            true
        }
        kind::BIND | kind::LISTEN | kind::UNBIND => {
            if let Some(host_net) = host_net {
                host_net(&[&[which][..], payload].concat());
            }
            true
        }
        // An exec its joiners' microVM runs, which went without a connection.
        kind::EXEC_FAILED => {
            exec_failed(payload);
            true
        }
        _ => false,
    };
    // A microVM that stays keeps its files, as they are now its command has ended:
    // once its end is told, so that nothing waits for them that does not need them. Where
    // they are an ext4 file system in a file of their own (D190), they stay there, its
    // journal written in place, for its host to read once the microVM has gone, unless
    // joiners of its network go on in it past the run (D119): their files are
    // its, and a microVM that goes on keeps it in use.
    if let (Ok(_), Some(out)) = (&status, layer_out) {
        if crate::vm_run::layered() && !crate::warm::joining() {
            match keep_layer_from(&conn, &mut reader, &mut beyond) {
                Ok(used) => {
                    if let Some(kept) = kept {
                        kept(used);
                    }
                }
                Err(e) => shards_vmm::debug!("keeping the container's files: {e}"),
            }
        } else {
            match receive_layer_from(&conn, &mut reader, out, &mut beyond) {
                Ok(used) => {
                    if let Some(saved) = saved {
                        saved(used);
                    }
                }
                Err(e) => shards_vmm::debug!("saving the container's files: {e}"),
            }
        }
    }
    // Containers joined to its network go on past it (D119): the guest's signal
    // connection, and what waits for the guest, stay theirs; and what the guest says on
    // the run's connection is passed on until it closes it, as it powers off. The host
    // says it asks no more of it (no layer, or its layer had), and the guest takes that
    // as its end of asking.
    let joined = crate::warm::joining();
    if joined {
        let _ = conn.shutdown(std::net::Shutdown::Write);
        drain(&mut reader, &mut beyond);
    }
    if !joined {
        *lock(to) = Signals::default();
        to.changed.notify_all();
    }
    *lock_stdin() = None;
    STDIN_OPEN.store(false, Ordering::Relaxed);
    lock_attached().clear();
    // Execs still waiting for the guest will never start.
    if !joined {
        hellos::close();
    }
    // The guest powers off once the host closes the run connection: shutting it down
    // closes every copy.
    end_guest(&conn);
    status
}

/// Ends the guest's connection `conn` both ways, every copy of it: as `shards_ipc::end`
/// ends a socket on Unix (M236); on Windows its Winsock socket shut down both ways.
fn end_guest(conn: &UnixStream) {
    #[cfg(unix)]
    shards_ipc::end(conn);
    #[cfg(windows)]
    let _ = conn.shutdown(std::net::Shutdown::Both);
}

/// Copies the guest's frames to shards' stdout and stderr until the exit status, and to
/// `log` if the microVM's output is kept. Why a command never ran is not its output,
/// and stays out of the log: `logs` holds what the command wrote.
fn relay(
    conn: &mut io::BufReader<shards_ipc::Polled<&UnixStream>>,
    timing: &Timing,
    log: Option<&mut Logger>,
    started: Option<&(dyn Fn() + Sync)>,
    records: RunRecords<'_>,
) -> Result<Ended, String> {
    let (ended, lost, dropped) = with_sink(log, |sink| relay_frames(conn, timing, sink, started, records))?;
    ended.map(|e| Ended { lost, dropped, ..e })
}

/// Where a run's records go as its guest sends them, each its frame's payload: [`Asked`]'s
/// of the same names.
#[derive(Clone, Copy)]
struct RunRecords<'a> {
    processes: Option<&'a Records<'a>>,
    members: Option<&'a Records<'a>>,
    readiness: Option<&'a Records<'a>>,
    host_net: Option<&'a Records<'a>>,
}

/// [`relay`]'s frames, its output kept through `log`; what its log lost is said by
/// [`with_sink`].
fn relay_frames(
    conn: &mut io::BufReader<shards_ipc::Polled<&UnixStream>>,
    timing: &Timing,
    mut log: Option<&mut Sink<'_>>,
    started: Option<&(dyn Fn() + Sync)>,
    records: RunRecords<'_>,
) -> Result<Ended, String> {
    // Grown to the longest frame yet, at most MAX_PAYLOAD, and never shrunk: a frame
    // longer than any before zeroes only its new bytes before they are read over (audit
    // D08).
    let mut frame = Vec::new();
    let mut not_run = None;
    let mut oom = None;
    let mut pids = None;
    // As much as the guest sends at once, in one read, whatever the frames it holds
    // (review 8.20): the caller's reader, whose next part reads what this one read past
    // its end.
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
            // A closed stdout drops output, as a closed pipe does for `docker run`.
            kind::STDOUT => {
                let _ = write_fd(1, payload);
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDOUT, payload);
                }
                fan_out(payload, false);
            }
            kind::STDERR => {
                let _ = write_fd(2, payload);
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDERR, payload);
                }
                fan_out(payload, true);
            }
            kind::STARTED => {
                if let Some(started) = started {
                    started();
                }
            }
            kind::SYSTEM_ERR => not_run = Some(String::from_utf8_lossy(payload).into_owned()),
            kind::EXEC_FAILED => exec_failed(payload),
            kind::SOCKET_REFUSED => socket_refused(payload),
            // All the guest held is here, the save it was asked for waits on it (D176).
            kind::QUIESCED => {
                quiescing().quiet();
                QUIET.notify_all();
            }
            kind::QUIESCE_REFUSED => {
                let refusal = match run::quiesce::Refused::decode(payload) {
                    Ok(refusal) => refusal,
                    Err(error) => {
                        quiescing().protocol_failed(error);
                        QUIET.notify_all();
                        return Err("the guest sent a malformed quiesce refusal".into());
                    }
                };
                quiescing().refuse(refusal);
                QUIET.notify_all();
            }
            kind::OOM => oom = Some(payload.to_vec()),
            kind::PIDS => pids = Some(payload.to_vec()),
            kind::PROCESS => {
                if let Some(processes) = records.processes {
                    processes(payload);
                }
            }
            // A member's end (D231), well formed or the guest's protocol broken.
            kind::MEMBER_ENDED => {
                run::members::Ended::decode(payload).ok_or("the guest sent a malformed member end")?;
                if let Some(members) = records.members {
                    members(payload);
                }
            }
            // Its command's readiness under the nesting grant (D234), as init moves it:
            // well formed, or the guest's protocol broken.
            kind::READY => {
                run::nesting::Ready::decode(payload).ok_or("the guest sent a malformed readiness")?;
                if let Some(readiness) = records.readiness {
                    readiness(payload);
                }
            }
            kind::BIND | kind::LISTEN | kind::UNBIND => {
                if let Some(host_net) = records.host_net {
                    let mut frame = Vec::with_capacity(1 + payload.len());
                    frame.push(which);
                    frame.extend_from_slice(payload);
                    host_net(&frame);
                }
            }
            kind::EXIT => {
                let _ = timing.answered_us.set(shards_vmm::log::uptime_us());
                let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                let status = u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX);
                // Those attached exit with the command, as `docker attach` does.
                for a in lock_attached().drain(..) {
                    let _ = shards_ipc::send(&a.client, shards_ipc::kind::EXIT, &[status], &[]);
                }
                return Ok(Ended {
                    status,
                    not_run,
                    lost: 0,
                    dropped: 0,
                    oom,
                    pids,
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// Writes all of `bytes` to this process's descriptor `fd` as it is now: standard output
/// or error, which a warm VM takes from its client once it starts. A frame is one write
/// where the descriptor takes it all, where std's line-buffered stdout made one of each
/// line's end and another of the rest (review 8.20).
#[cfg(unix)]
fn write_fd(fd: libc::c_int, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: write(2) of a live buffer, of its length.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        match usize::try_from(n) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// [`write_fd`] on Windows: to this process's standard output (1) or error (2) handle as
/// it is now (GetStdHandle), its bytes as they are, where std's console writer would take
/// them for UTF-8.
#[cfg(windows)]
fn write_fd(fd: i32, mut bytes: &[u8]) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let which = if fd == 2 {
        STD_ERROR_HANDLE
    } else {
        STD_OUTPUT_HANDLE
    };
    // SAFETY: no pointers; the handle is the process's, not ours to close.
    let handle = unsafe { GetStdHandle(which) };
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(io::ErrorKind::NotConnected.into());
    }
    while !bytes.is_empty() {
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let mut n = 0u32;
        // SAFETY: a live buffer of at least `len` bytes, and a count to write.
        if unsafe { WriteFile(handle, bytes.as_ptr(), len, &raw mut n, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = bytes.get(n as usize..).unwrap_or_default();
    }
    Ok(())
}

/// Sends `layer` as [`kind::LAYER`] frames, then an empty one.
fn send_layer(conn: &mut UnixStream, mut layer: fs::File) -> io::Result<()> {
    let mut buf = vec![0u8; run::CHUNK];
    loop {
        let n = layer.read(&mut buf)?;
        send(conn, kind::LAYER, buf.get(..n).unwrap_or_default())?;
        if n == 0 {
            return Ok(());
        }
    }
}

/// Asks the guest for the microVM's writable layer ([`kind::SAVE`]) and writes its
/// [`kind::LAYER`] frames to `out` until the empty one; an error if it did not come
/// whole: the guest's reason, where it says why ([`kind::SYSTEM_ERR`]).
pub(crate) fn receive_layer(conn: &mut UnixStream, out: fs::File) -> io::Result<Option<u64>> {
    let reading = conn.try_clone()?;
    receive_layer_from(
        conn,
        &mut io::BufReader::new(shards_ipc::Polled(&reading)),
        out,
        &mut |_, _| false,
    )
}

/// [`receive_layer`], its frames read from `reader`, `conn`'s; each other frame the guest
/// sends meanwhile given to `beyond`, an error where that takes none of it.
fn receive_layer_from(
    conn: &UnixStream,
    reader: &mut impl Read,
    mut out: fs::File,
    beyond: &mut dyn FnMut(u8, &[u8]) -> bool,
) -> io::Result<Option<u64>> {
    let mut w = conn;
    w.write_all(&run::header(kind::SAVE, 0))?;
    let mut frame = Vec::new();
    let mut used = None;
    loop {
        let mut h = [0u8; run::HEADER];
        reader.read_exact(&mut h)?;
        let (which, len) =
            run::parse_header(h).ok_or_else(|| io::Error::other("the guest sent a malformed frame"))?;
        let len = len as usize;
        match which {
            kind::LAYER => {}
            // Not all of it could be packed: the layer it had stays.
            kind::SYSTEM_ERR => {
                let mut why = vec![0u8; len];
                reader.read_exact(&mut why)?;
                return Err(io::Error::other(String::from_utf8_lossy(&why).into_owned()));
            }
            // What the layer uses, before it (kind::USAGE).
            kind::USAGE if len == 8 && used.is_none() => {
                let mut n = [0u8; 8];
                reader.read_exact(&mut n)?;
                used = Some(u64::from_be_bytes(n));
                continue;
            }
            _ => {
                frame.resize(len, 0);
                reader.read_exact(&mut frame)?;
                if beyond(which, &frame) {
                    continue;
                }
                return Err(io::Error::other("the guest sent another frame amid its files"));
            }
        }
        if len == 0 {
            return out.sync_all().map(|()| used);
        }
        frame.resize(len, 0);
        reader.read_exact(&mut frame)?;
        out.write_all(&frame)?;
    }
}

/// Asks the guest to keep the microVM's writable layer in its own file ([`kind::KEEP`],
/// D190) and reads its answer: what the layer uses ([`kind::USAGE`]) and an empty
/// [`kind::LAYER`] frame; each other frame given to `beyond`, as
/// [`receive_layer_from`]'s.
fn keep_layer_from(
    conn: &UnixStream,
    reader: &mut impl Read,
    beyond: &mut dyn FnMut(u8, &[u8]) -> bool,
) -> io::Result<Option<u64>> {
    let mut w = conn;
    w.write_all(&run::header(kind::KEEP, 0))?;
    let mut frame = Vec::new();
    let mut used = None;
    loop {
        let mut h = [0u8; run::HEADER];
        reader.read_exact(&mut h)?;
        let (which, len) =
            run::parse_header(h).ok_or_else(|| io::Error::other("the guest sent a malformed frame"))?;
        let len = len as usize;
        frame.resize(len, 0);
        reader.read_exact(&mut frame)?;
        match which {
            kind::LAYER if len == 0 => return Ok(used),
            kind::USAGE if len == 8 && used.is_none() => {
                used = <[u8; 8]>::try_from(frame.as_slice()).ok().map(u64::from_be_bytes);
            }
            kind::SYSTEM_ERR => return Err(io::Error::other(String::from_utf8_lossy(&frame).into_owned())),
            _ if beyond(which, &frame) => {}
            _ => {
                return Err(io::Error::other(
                    "the guest sent another frame as it kept its files",
                ));
            }
        }
    }
}

/// Passes each frame `reader` has to `beyond` until it ends: what the guest says on a
/// run's connection once its command's end and layer are had, its joiners going on.
fn drain(reader: &mut impl Read, beyond: &mut dyn FnMut(u8, &[u8]) -> bool) {
    let mut frame = Vec::new();
    loop {
        let mut h = [0u8; run::HEADER];
        if reader.read_exact(&mut h).is_err() {
            return;
        }
        let Some((which, len)) = run::parse_header(h) else {
            return;
        };
        frame.resize(len as usize, 0);
        if reader.read_exact(&mut frame).is_err() {
            return;
        }
        beyond(which, &frame);
    }
}

fn send(conn: &mut UnixStream, which: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    conn.write_all(&run::header(which, len))?;
    conn.write_all(payload)
}

/// What waits for a guest's connection to the exec port, and where it goes, or why there is
/// none: an exec's, by the id its guest's [`kind::EXEC_FAILED`] names; a restored run's
/// (D176); a capture's.
enum Target {
    Exec {
        id: u32,
        to: std::sync::mpsc::SyncSender<Result<UnixStream, String>>,
    },
    Resumed(std::sync::mpsc::SyncSender<Result<UnixStream, String>>),
    Capture(std::sync::mpsc::SyncSender<Result<UnixStream, String>>),
    /// A member's input connection (capture.rs `start`), by the member's index.
    Input {
        member: u32,
        to: std::sync::mpsc::SyncSender<(u32, UnixStream)>,
    },
    /// A lane to a socket of the run's own command (D251), by the id its guest's
    /// [`kind::SOCKET_REFUSED`] names.
    Socket {
        id: u32,
        to: std::sync::mpsc::SyncSender<Result<UnixStream, Unopened>>,
    },
}

impl Target {
    fn id(&self) -> Option<u32> {
        match self {
            Self::Exec { id, .. } | Self::Socket { id, .. } => Some(*id),
            _ => None,
        }
    }
    fn deliver(self, result: Result<UnixStream, String>) {
        let to = match self {
            Self::Exec { to, .. } | Self::Resumed(to) | Self::Capture(to) => to,
            Self::Socket { to, .. } => {
                let _ = to.try_send(result.map_err(Unopened::Ended));
                return;
            }
            // An input that never comes is its init's to tell (`managed::InputEnded`), as the
            // connection it dialed fails there: nothing waits on it here.
            Self::Input { member, to } => {
                if let Ok(conn) = result {
                    let _ = to.try_send((member, conn));
                }
                return;
            }
        };
        let _ = to.try_send(result);
    }
}

static NEXT_EXEC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Lane `id` (D251) will have no connection: its guest refused it before connecting
/// anything, as it said.
fn socket_refused(payload: &[u8]) {
    let Some((id, errno)) = run::socket::read_refused(payload) else {
        return;
    };
    if let Some(Target::Socket { to, .. }) = hellos::fail(id) {
        let _ = to.try_send(Err(Unopened::Refused(errno)));
    }
}

/// Exec `id` will have no connection, for `why`, as the guest said: it fails with it.
fn exec_failed(payload: &[u8]) {
    let Some((id, why)) = payload.split_first_chunk::<4>() else {
        return;
    };
    if let Some(target) = hellos::fail(u32::from_be_bytes(*id)) {
        target.deliver(Err(String::from_utf8_lossy(why).into_owned()));
    }
}

/// Takes every guest connection to `port`, each of which says a [`kind::HELLO`] first:
/// one whose hello has come whole, saying `token` (checked in constant time), goes to
/// `heard` with the ring its relay offered after the token, if it offered one (D230); any
/// other is closed. An offer is read only past a token that holds, so that a process of the
/// microVM's that dials the port itself has the VM hold no more of its hello than its
/// header and token. Connections not yet heard from are read by one thread, polling them
/// all (review 8.8): one that says nothing costs a descriptor, not a thread, and holds up no
/// other; the muxer bounds how many there are. They go when the workload's process does.
pub fn accept_hellos(
    port: EnginePort,
    name: &'static str,
    token: [u8; run::TOKEN],
    heard: impl Fn(UnixStream, RingHandle, Hello) + Send + 'static,
) {
    let Ok((wake, woken)) = UnixStream::pair() else {
        return;
    };
    let (arrived, arrivals) = std::sync::mpsc::channel::<(UnixStream, RingHandle)>();
    if std::thread::Builder::new()
        .name(name.into())
        .spawn(move || hellos(&arrivals, &woken, &token, &heard))
        .is_err()
    {
        return;
    }
    for conn in port {
        if arrived.send(conn).is_err() {
            return;
        }
        let _ = (&wake).write(&[0]);
    }
}

/// What a connection's hello came to, its token the run's.
#[derive(Debug)]
pub enum Hello {
    /// No ring offered.
    Plain,
    /// A ring offered: the slot its bytes say, or why they say none.
    Offered(Result<ring::Offer, ring::Malformed>),
}

/// A connection to the engine's port, and as much of its hello as has come: its header and
/// token at first, then, where its header says an offer follows and its token holds, the
/// offer.
struct Unheard {
    conn: UnixStream,
    ring: RingHandle,
    hello: Vec<u8>,
    got: usize,
}

/// What a read of a hello came to.
enum Heard {
    /// Not whole yet.
    More,
    Whole(Hello),
    /// Ended, failed, not a hello, or not the run's token: closed.
    Refused,
}

impl Unheard {
    fn new(conn: UnixStream, ring: RingHandle) -> Unheard {
        Unheard {
            conn,
            ring,
            hello: vec![0; run::HEADER + run::TOKEN],
            got: 0,
        }
    }

    fn read(&mut self, token: &[u8; run::TOKEN]) -> Heard {
        let Some(room) = self.hello.get_mut(self.got..) else {
            return Heard::Refused;
        };
        match (&self.conn).read(room) {
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                return Heard::More;
            }
            Ok(n @ 1..) => self.got += n,
            _ => return Heard::Refused,
        }
        if self.got < run::HEADER {
            return Heard::More;
        }
        // A header that is no hello's, or says more than a token and an offer, is closed
        // as soon as it has come.
        let len = match self
            .hello
            .first_chunk::<{ run::HEADER }>()
            .copied()
            .and_then(run::parse_header)
        {
            Some((kind::HELLO, len)) => len as usize,
            _ => return Heard::Refused,
        };
        if !(run::TOKEN..=run::TOKEN + ring::MAX_OFFER).contains(&len) {
            return Heard::Refused;
        }
        if self.got < self.hello.len() {
            return Heard::More;
        }
        if self.hello.len() == run::HEADER + run::TOKEN {
            let said = self.hello.last_chunk::<{ run::TOKEN }>();
            if !said.is_some_and(|said| same_token(token, said)) {
                return Heard::Refused;
            }
            if len == run::TOKEN {
                return Heard::Whole(Hello::Plain);
            }
            // The token holds: room for the offer that follows it.
            if self.hello.try_reserve_exact(len - run::TOKEN).is_err() {
                return Heard::Refused;
            }
            self.hello.resize(run::HEADER + len, 0);
            return Heard::More;
        }
        let offer = self.hello.get(run::HEADER + run::TOKEN..).unwrap_or_default();
        Heard::Whole(Hello::Offered(ring::Offer::parse(offer)))
    }
}

/// Reads the hellos of the connections that `arrivals` brings, each told of on `woken`,
/// until the acceptor goes: whole, each goes to `heard`, or is closed.
fn hellos(
    arrivals: &std::sync::mpsc::Receiver<(UnixStream, RingHandle)>,
    woken: &UnixStream,
    token: &[u8; run::TOKEN],
    heard: &dyn Fn(UnixStream, RingHandle, Hello),
) {
    let mut unheard: Vec<Unheard> = Vec::new();
    let mut byte = [0u8; 64];
    loop {
        let Ok(polled) = await_hellos(woken, unheard.iter().map(|u| &u.conn)) else {
            return;
        };
        let Some((&wake, rest)) = polled.split_first() else {
            return;
        };
        if wake {
            match (&*woken).read(&mut byte) {
                // The acceptor is gone, and the workload with it.
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(_) => return,
            }
            while let Ok((conn, ring)) = arrivals.try_recv() {
                if conn.set_nonblocking(true).is_ok() {
                    unheard.push(Unheard::new(conn, ring));
                }
            }
        }
        // From the last, so that each removal leaves the indices before it as they were.
        let ready: Vec<usize> = rest
            .iter()
            .enumerate()
            .filter(|&(_, &r)| r)
            .map(|(i, _)| i)
            .collect();
        for i in ready.into_iter().rev() {
            let Some(u) = unheard.get_mut(i) else { continue };
            match u.read(token) {
                Heard::More => {}
                Heard::Refused => {
                    unheard.swap_remove(i);
                }
                Heard::Whole(hello) => {
                    let u = unheard.swap_remove(i);
                    heard(u.conn, u.ring, hello);
                }
            }
        }
    }
}

/// Waits until `woken` or one of `unheard`'s connections has something to read, or has
/// ended: which have, `woken` first. poll(2) on Unix; WSAPoll on Windows, whose sockets
/// they are (`shards_vmm::platform::wait_readable`).
fn await_hellos<'a>(
    woken: &UnixStream,
    connections: impl ExactSizeIterator<Item = &'a UnixStream>,
) -> io::Result<Vec<bool>> {
    use shards_vmm::platform::AsRawSocket as _;
    let count = connections
        .len()
        .checked_add(1)
        .ok_or_else(|| io::Error::other("HELLO poll count overflow"))?;
    let mut sockets = Vec::new();
    sockets.try_reserve_exact(count).map_err(io::Error::other)?;
    sockets.push(woken.raw_socket());
    sockets.extend(connections.map(|c| c.raw_socket()));
    shards_vmm::platform::wait_readable(&sockets, None)
}

/// Whether tokens `a` and `b` are the same, compared whole, in constant time: a workload
/// that dials a port learns nothing of a token from how fast it is refused.
pub fn same_token(a: &[u8; run::TOKEN], b: &[u8; run::TOKEN]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The null device, which stands in for the stdio of a client this VM lets go of: held from
/// its start ([`hold_null`]), /dev/null, or on Windows the NUL its spawner gave it, as its
/// AppContainer opens no file by name, NUL among them on Windows Server (PM M351).
static NULL: std::sync::OnceLock<fs::File> = std::sync::OnceLock::new();

/// Holds `null` as this VM's null device ([`null`]), which it keeps once held.
pub fn hold_null(null: fs::File) {
    let _ = NULL.set(null);
}

/// This VM's null device, a copy of its own.
pub fn null() -> io::Result<fs::File> {
    NULL.get()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no null device is held"))?
        .try_clone()
}

/// Why a lane to a socket of the run's own command (D251) did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unopened {
    /// Its guest refused it, with Linux's errno for why (`shards_abi::run::socket`): its
    /// connect's own, or init's for a run that grants no sockets, a name that is no file name
    /// in its directory, or a command not running.
    Refused(std::num::NonZeroU32),
    /// It could not be asked of its guest, or its connection will not come: the workload
    /// has ended, or its connections failed. Why.
    Ended(String),
}

impl std::fmt::Display for Unopened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(errno) => write!(f, "refused, errno {errno}"),
            Self::Ended(why) => f.write_str(why),
        }
    }
}

/// Opens a lane to the socket `name` in the run's own command's socket directory (D251): asks
/// the guest for it, waits for its connection to the exec port, and reads its status.
/// Connected, the connection, its bytes the socket's each way from here on; else why not.
pub fn open_socket(to: &'static ToGuest, name: &[u8]) -> Result<UnixStream, Unopened> {
    let mut token = [0u8; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token)
        .map_err(|e| Unopened::Ended(format!("a lane's token: {e}")))?;
    let id = NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let payload = run::socket::Ask { token, id, name }.encode();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    register(token, Target::Socket { id, to: tx }).map_err(Unopened::Ended)?;
    if let Err(unsent) = to_guest(to, kind::SOCKET, &payload) {
        hellos::forget(&token);
        return Err(Unopened::Ended(unsent.to_string()));
    }
    let conn = rx
        .recv()
        .map_err(|_| Unopened::Ended("the run's command ended before the lane opened".into()))??;
    let mut status = [0u8; run::socket::STATUS];
    conn.set_nonblocking(false)
        .and_then(|()| Read::read_exact(&mut shards_ipc::Polled(&conn), &mut status))
        .map_err(|e| Unopened::Ended(format!("the lane's status: {e}")))?;
    match std::num::NonZeroU32::new(u32::from_be_bytes(status)) {
        None => Ok(conn),
        Some(errno) => Err(Unopened::Refused(errno)),
    }
}

/// A command to run beside the workload (`docker exec`), and its client's: the
/// connection it is answered on, and its stdio.
pub struct ExecRequest {
    pub spec: Spec,
    pub interactive: bool,
    pub detached: bool,
    /// Its client is told its start, or why it never started (`EXEC_TOLD`, D238).
    pub told: bool,
    /// The daemon's number for it, and its connection, told of its end (`EXEC_ENDED`).
    pub number: u64,
    pub daemon: Option<shards_ipc::Stream>,
    pub client: shards_ipc::Stream,
    pub stdin: fs::File,
    pub stdout: fs::File,
    pub stderr: fs::File,
}

/// Runs `req` beside the workload, on a thread of its own, answering its client as
/// `docker exec` answers: the command's output, or why it did not start, then `EXIT`
/// with its status (`-d`: 0 once it starts).
pub fn exec(to: &'static ToGuest, req: ExecRequest) -> Result<(), String> {
    std::thread::Builder::new()
        .name("exec".into())
        .spawn(move || {
            let mut req = req;
            let status = match exec_session(to, &mut req) {
                Ok(status) => status,
                Err(e) => {
                    let _ = writeln!(req.stderr, "shards: {e}");
                    1
                }
            };
            // Its end, to its client: a detached exec's, which heard its start, too, for a
            // client that stays to keep it (the Engine API's, D170); shards' CLI has left.
            let _ = shards_ipc::send(&req.client, shards_ipc::kind::EXIT, &[status], &[]);
            if let Some(daemon) = &req.daemon {
                let ended = [&req.number.to_be_bytes()[..], &[status]].concat();
                let _ = shards_ipc::send(daemon, shards_ipc::kind::EXEC_ENDED, &ended, &[]);
            }
        })
        .map(drop)
        .map_err(|e| format!("an exec's thread: {e}"))
}

/// The [`kind::EXEC`] frame's payload for exec `id`: its token, its id and its spec, within
/// what a frame may carry (review 8.9). The guest reads no longer frame, and drops the
/// connection one came on, which carries every signal and exec of the workload.
fn exec_frame(token: &[u8; run::TOKEN], id: u32, spec: &Spec) -> Result<Vec<u8>, String> {
    let len = spec
        .encoded_len()
        .and_then(|n| n.checked_add(run::TOKEN + 4))
        .filter(|&n| n <= run::MAX_PAYLOAD as usize)
        .ok_or("the command and its environment are too large")?;
    let mut payload = Vec::with_capacity(len);
    payload.extend_from_slice(token);
    payload.extend_from_slice(&id.to_be_bytes());
    spec.encode_into(&mut payload);
    Ok(payload)
}

/// One exec, from asking the guest for it to its status: a detached one's client is told
/// once it starts, and it is followed to its end, its output going nowhere. What never
/// started is said on `req.stderr`.
fn exec_session(to: &'static ToGuest, req: &mut ExecRequest) -> Result<u8, String> {
    let mut token = [0u8; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("an exec's token: {e}"))?;
    let id = NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let payload = exec_frame(&token, id, &req.spec)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    register(token, Target::Exec { id, to: tx })?;
    if let Err(unsent) = to_guest(to, kind::EXEC, &payload) {
        hellos::forget(&token);
        return Err(unsent.to_string());
    }
    let mut conn = rx
        .recv()
        .map_err(|_| "the container's process ended before the command started".to_string())??;
    // A detached exec attaches nothing, `-i` or not (docker/cli exec.go, parseExec).
    if req.interactive && !req.detached {
        let mut input = conn.try_clone().map_err(|e| e.to_string())?;
        let stdin = req.stdin.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("exec-stdin".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = shards_ipc::read(&stdin, &mut buf) {
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
                // A size or signal refused leaves the exec as it is, its output and end
                // coming on its own connection; a lost link is the VM's log's to say (`lose`).
                while let Ok(Some(m)) = shards_ipc::recv(&client) {
                    match m.kind {
                        shards_ipc::kind::RESIZE if Size::decode(&m.payload).is_some() => {
                            let _ = resize_exec(to, id, &m.payload);
                        }
                        // The daemon's, for a health check past its timeout.
                        shards_ipc::kind::SIGNAL => {
                            if let Ok(signal) = <[u8; 4]>::try_from(m.payload.as_slice()) {
                                let _ = signal_exec(to, id, u32::from_be_bytes(signal));
                            }
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| format!("an exec's client: {e}"))?;
    }
    let mut frame = Vec::new();
    let mut not_started: Option<(u8, Vec<u8>)> = None;
    let mut conn = io::BufReader::with_capacity(run::BUFFERED, shards_ipc::Polled(&conn));
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
                // Its client's stdio is let go of, as `docker exec -d` holds none of it once
                // the command has started: what reads the client's output to its end is not
                // held until the command ends.
                if let Ok(null) = null() {
                    for slot in [&mut req.stdin, &mut req.stdout, &mut req.stderr] {
                        if let Ok(null) = null.try_clone() {
                            *slot = null;
                        }
                    }
                }
            }
            // Its start, to a client that types it (the agent-control endpoint's host, D238).
            kind::STARTED if req.told => {
                let _ = shards_ipc::send(&req.client, shards_ipc::kind::STARTED, &[], &[]);
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
                not_started = Some((*class, why.to_vec()));
            }
            kind::EXIT => {
                let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                let status = u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX);
                // Ended, every copy, its stdin's too: the VMM's end on Windows is held until
                // this one ends (PM M358).
                end_guest(conn.get_ref().0);
                // Why it never started, by its class, to a client that types it (D238): the
                // guest's own failure, within what its frame carried.
                if let (true, Some((class, why))) = (req.told, &not_started) {
                    let said = [&[*class][..], why.as_slice()].concat();
                    let _ = shards_ipc::send(&req.client, shards_ipc::kind::EXEC_UNSTARTED, &said, &[]);
                }
                return Ok(match not_started {
                    None => status,
                    Some((run::exec_failed::DAEMON, why)) => {
                        let _ = writeln!(req.stderr, "shards: {}", String::from_utf8_lossy(&why));
                        1
                    }
                    // Refused in a member, by type (D243): `docker exec`'s 126 for what its
                    // runtime could not start, its 1 for what its daemon refuses.
                    Some((run::exec_failed::MEMBER, said)) => {
                        let (refusal, why) = run::members::Refusal::decode(&said)
                            .ok_or("the guest's refusal of a member's command is malformed")?;
                        let _ = writeln!(req.stderr, "shards: {}", String::from_utf8_lossy(why));
                        if refusal.not_started() { 126 } else { 1 }
                    }
                    Some((_, why)) => {
                        let why = String::from_utf8_lossy(&why);
                        // The status of what init could not start (126 or 127, as moby
                        // daemon/errors.go sets them), and why.
                        let (_, code) = shards_cmdline::commands::start_failed(&why);
                        let _ = writeln!(req.stderr, "shards: cannot start the command: {why}");
                        code
                    }
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// A joiner of the workload's network (D119): its command, as a run's spec says
/// it, the join entry naming its image's range of the join disk among its setup; its
/// client's connection and stdio, as a run's, none for a detached one; its log.
pub struct JoinRequest {
    pub spec: Spec,
    pub interactive: bool,
    pub detached: bool,
    pub client: Option<shards_ipc::Stream>,
    pub stdin: fs::File,
    pub stdout: Option<fs::File>,
    pub stderr: Option<fs::File>,
    pub log: Option<Logger>,
}

/// Clients attached to joiners (D119), each with its joiner's id in the guest, as
/// [`ATTACHED`] holds the workload's: where a joiner's output goes besides its own client
/// and log, and whose connections hear its status.
static JOIN_ATTACHED: Mutex<Vec<(u32, Attacher)>> = Mutex::new(Vec::new());

/// Each running joiner's way in (D119), by its id in the guest: a writer of its connection
/// to the guest, for its attached clients' stdin; and a socket pair whose first end closes
/// as the joiner ends, which each copy of an attached stdin waits on with its stdin.
static JOIN_INPUTS: Mutex<Vec<(u32, UnixStream, UnixStream, UnixStream)>> = Mutex::new(Vec::new());

fn lock_join_attached() -> std::sync::MutexGuard<'static, Vec<(u32, Attacher)>> {
    JOIN_ATTACHED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_join_inputs() -> std::sync::MutexGuard<'static, Vec<(u32, UnixStream, UnixStream, UnixStream)>> {
    JOIN_INPUTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Attaches `req`'s client to joiner `id` in the guest (D119), as `docker attach` attaches
/// to a container: its output from now, its stdin where the joiner reads one, its signals
/// and terminal sizes; told its status as it ends ([`end_joiner`]).
pub fn attach_joiner(to: &'static ToGuest, id: u32, req: AttachRequest) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    let AttachRequest {
        client,
        stdin,
        stdout,
        stderr,
        reads_stdin,
    } = req;
    let listened = client
        .try_clone()
        .map_err(|e| format!("the client's connection: {e}"))?;
    // Its way in, taken before it is attached: none once the joiner has ended.
    let input = if reads_stdin {
        lock_join_inputs()
            .iter()
            .find(|(j, ..)| *j == id)
            .and_then(|(_, input, _, wake)| Some((input.try_clone().ok()?, wake.try_clone().ok()?)))
    } else {
        None
    };
    lock_join_attached().push((
        id,
        Attacher {
            id: number,
            out: stdout,
            err: stderr,
            client,
        },
    ));
    std::thread::Builder::new()
        .name("join-attached".into())
        .spawn(move || {
            while let Ok(Some(m)) = shards_ipc::recv(&listened) {
                match m.kind {
                    // Its INTERRUPT is the command's signal (D26).
                    shards_ipc::kind::SIGNAL | shards_ipc::kind::INTERRUPT => {
                        if let Ok(signal) = <[u8; 4]>::try_from(m.payload.as_slice()) {
                            let _ = signal_exec(to, id, u32::from_be_bytes(signal));
                        }
                    }
                    shards_ipc::kind::RESIZE if Size::decode(&m.payload).is_some() => {
                        let _ = resize_exec(to, id, &m.payload);
                    }
                    _ => {}
                }
            }
            lock_join_attached().retain(|(_, a)| a.id != number);
        })
        .map_err(|e| format!("an attached client's thread: {e}"))?;
    if let Some((input, wake)) = input {
        std::thread::Builder::new()
            .name("join-attached-stdin".into())
            .spawn(move || copy_attached_stdin(stdin, input, &wake))
            .map_err(|e| format!("an attached client's stdin: {e}"))?;
    }
    Ok(())
}

/// Copies an attached client's `stdin` to a joiner's, on its connection `input`, until
/// either ends or the joiner does, which closes `wake`'s peer.
#[cfg(unix)]
fn copy_attached_stdin(mut stdin: fs::File, mut input: UnixStream, wake: &UnixStream) {
    use std::os::fd::AsRawFd as _;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut polled = [stdin.as_raw_fd(), wake.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: poll(2) on two pollfds of descriptors this thread holds open.
        if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let [from, woken] = polled;
        if woken.revents != 0 {
            return;
        }
        if from.revents != 0 {
            match stdin.read(&mut buf) {
                Ok(n @ 1..) => {
                    if send(&mut input, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    }
}

/// [`copy_attached_stdin`] on Windows, where a client's stdin, a pipe or a file, is waited
/// on beside no socket: each read blocks, and the copy ends at the stdin's end, or at the
/// first write the joiner's end refuses. Its client, told the joiner's end, closes it.
#[cfg(windows)]
fn copy_attached_stdin(mut stdin: fs::File, mut input: UnixStream, _wake: &UnixStream) {
    let mut buf = vec![0u8; 64 * 1024];
    while let Ok(n @ 1..) = stdin.read(&mut buf) {
        if send(&mut input, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
            return;
        }
    }
}

/// Joiner `id`'s output to each client attached to it; one gone is let go.
fn fan_out_joined(id: u32, payload: &[u8], stderr: bool) {
    lock_join_attached().retain_mut(|(j, a)| {
        if *j != id {
            return true;
        }
        let to = if stderr { &mut a.err } else { &mut a.out };
        to.write_all(payload).is_ok()
    });
}

/// Joiner `id` has ended with `status` (D119): each client attached to it hears it and is
/// let go, its connection shut so that its thread ends; its way in goes, which ends the
/// copies of their stdin.
pub fn end_joiner(id: u32, status: u8) {
    lock_join_inputs().retain(|(j, ..)| *j != id);
    let attached: Vec<Attacher> = {
        let mut all = lock_join_attached();
        let (ended, others): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *all).into_iter().partition(|(j, _)| *j == id);
        *all = others;
        ended.into_iter().map(|(_, a)| a).collect()
    };
    for a in attached {
        let _ = shards_ipc::send(&a.client, shards_ipc::kind::EXIT, &[status], &[]);
        shards_ipc::end(&a.client);
    }
}

/// Sends Linux signal `linux` to the guest's exec, or joiner, `id` (D119), as
/// [`signal_guest`] signals a microVM's command.
pub fn signal_exec(to: &ToGuest, id: u32, linux: u32) -> Result<(), Unsent> {
    is_signal(linux)?;
    to_guest(
        to,
        kind::EXEC_SIGNAL,
        &[&id.to_be_bytes()[..], &linux.to_be_bytes()].concat(),
    )
}

/// Runs joiner `req` in the guest to its end, as a run's command is served: what kept it
/// from starting, or its output to its client and its log, its client's stdin, terminal
/// sizes and signals, and its status. Its id in the guest goes to `on_id` as soon as it
/// has one, and `started` hears when its command runs; `told` hears what init says of a
/// joiner its microVM's engine made (D181), by its frame's kind: its membership of the
/// microVM's switch (`MEMBER`) and the ports it was given (`PORTS`).
pub fn join(
    to: &'static ToGuest,
    req: JoinRequest,
    on_id: &dyn Fn(u32),
    started: &dyn Fn(),
    told: &dyn Fn(u8, &[u8]),
) -> Result<(Ended, UnixStream), String> {
    let JoinRequest {
        spec,
        interactive,
        detached,
        client,
        stdin,
        mut stdout,
        mut stderr,
        mut log,
    } = req;
    let mut token = [0u8; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("a joiner's token: {e}"))?;
    let id = NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    on_id(id);
    let payload = exec_frame(&token, id, &spec)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    register(token, Target::Exec { id, to: tx })?;
    if let Err(unsent) = to_guest(to, kind::EXEC, &payload) {
        hellos::forget(&token);
        return Err(match unsent {
            Unsent::NotRunning => "the container whose network it joins is not running".into(),
            other => other.to_string(),
        });
    }
    let stream = rx
        .recv()
        .map_err(|_| "the container whose network it joins ended first".to_string())??;
    let mut input = stream.try_clone().map_err(|e| e.to_string())?;
    // Its way in, for the clients that attach to it, until it ends (`end_joiner`).
    let (wake, watched) = UnixStream::pair().map_err(|e| format!("a joiner's attach: {e}"))?;
    lock_join_inputs().push((id, stream.try_clone().map_err(|e| e.to_string())?, wake, watched));
    // Its stdin as a run's (`serve`): its client's, closed as the client's ends where it has
    // no terminal (Docker's StdinOnce); a detached one's open for those who attach; none
    // without `-i`.
    let once = spec.tty.is_none();
    if interactive && !detached {
        std::thread::Builder::new()
            .name("join-stdin".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = shards_ipc::read(&stdin, &mut buf) {
                    if send(&mut input, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
                        return;
                    }
                }
                if once {
                    let _ = send(&mut input, kind::STDIN, &[]);
                }
            })
            .map_err(|e| format!("a joiner's stdin: {e}"))?;
    } else if !interactive {
        let _ = send(&mut input, kind::STDIN, &[]);
    }
    if let Some(client) = &client {
        // The client's terminal sizes and signals, for the joiner's own.
        let client = client.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("join-client".into())
            .spawn(move || {
                while let Ok(Some(m)) = shards_ipc::recv(&client) {
                    match m.kind {
                        shards_ipc::kind::RESIZE if Size::decode(&m.payload).is_some() => {
                            let _ = resize_exec(to, id, &m.payload);
                        }
                        // Its INTERRUPT is the command's signal once its run is made (D26).
                        shards_ipc::kind::SIGNAL | shards_ipc::kind::INTERRUPT => {
                            if let Ok(signal) = <[u8; 4]>::try_from(m.payload.as_slice()) {
                                let _ = signal_exec(to, id, u32::from_be_bytes(signal));
                            }
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| format!("a joiner's client: {e}"))?;
    }
    let mut frame = Vec::new();
    let mut not_run: Option<String> = None;
    let mut oom: Option<Vec<u8>> = None;
    let mut pids: Option<Vec<u8>> = None;
    let mut conn = io::BufReader::with_capacity(run::BUFFERED, shards_ipc::Polled(&stream));
    let (ended, lost, dropped) = with_sink(log.as_mut(), |mut log| {
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
                kind::STARTED => started(),
                // Its membership of its microVM's switch, and the ports it was given, as a
                // joiner its microVM's engine made (D181).
                kind::MEMBER | kind::PORTS => told(which, payload),
                kind::STDOUT | kind::STDERR => {
                    let (out, stream) = if which == kind::STDOUT {
                        (&mut stdout, crate::spec::LOG_STDOUT)
                    } else {
                        (&mut stderr, crate::spec::LOG_STDERR)
                    };
                    if let Some(log) = &mut log {
                        log.keep(stream, payload);
                    }
                    fan_out_joined(id, payload, which == kind::STDERR);
                    // A detached joiner's output goes to its log alone, as a detached run's.
                    if !detached && let Some(o) = out {
                        let _ = o.write_all(payload);
                    }
                }
                kind::SYSTEM_ERR => {
                    let (_, why) = payload.split_first().ok_or("an empty failure")?;
                    not_run = Some(String::from_utf8_lossy(why).into_owned());
                }
                // A process of it the kernel killed for want of memory (D159).
                kind::OOM => oom = Some(payload.to_vec()),
                kind::PIDS => pids = Some(payload.to_vec()),
                // Its end, and its connection, on which the host may ask for its writable
                // layer (`receive_layer`) before it closes it: the guest sends nothing more
                // until then.
                kind::EXIT => {
                    let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                    return Ok(Ended {
                        status: u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX),
                        not_run: not_run.take(),
                        lost: 0,
                        dropped: 0,
                        oom: oom.take(),
                        pids: pids.take(),
                    });
                }
                _ => return Err(format!("the guest sent an unknown frame kind {which}")),
            }
        }
    })?;
    Ok((
        Ended {
            lost,
            dropped,
            ..ended?
        },
        stream,
    ))
}

/// Copies shards' stdin to the workload's, then closes it, where its end is the
/// workload's ([`STDIN_ONCE`]).
fn forward_stdin() {
    // Looked at before each read on Unix, as macOS can sleep through a peer's end (M236,
    // `shards_ipc::read`); read as it comes on Windows, a pipe, a file or a console.
    #[cfg(unix)]
    {
        let stdin = io::stdin();
        copy_stdin(&mut |buf| shards_ipc::read(&stdin, buf));
    }
    #[cfg(windows)]
    copy_stdin(&mut |buf| io::stdin().read(buf));
    if STDIN_ONCE.load(Ordering::Relaxed) {
        close_stdin();
    }
}

/// Copies what `read` reads to the workload's stdin until its end.
fn copy_stdin(read: &mut dyn FnMut(&mut [u8]) -> io::Result<usize>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let data = buf.get(..n).unwrap_or_default();
                let sent = lock_stdin()
                    .as_mut()
                    .is_some_and(|c| send(c, kind::STDIN, data).is_ok());
                if !sent {
                    return;
                }
            }
        }
    }
}

/// Closes the workload's stdin, once.
fn close_stdin() {
    if STDIN_OPEN.swap(false, Ordering::Relaxed)
        && let Some(c) = lock_stdin().as_mut()
    {
        let _ = send(c, kind::STDIN, &[]);
    }
}

/// The run connection's copy that carries the workload's stdin, which every writer of it
/// shares: its client's, and those attached.
static STDIN: Mutex<Option<UnixStream>> = Mutex::new(None);
/// The workload reads a stdin, still open.
static STDIN_OPEN: AtomicBool = AtomicBool::new(false);
/// Its stdin closes as a client's ends: the run was attached (StdinOnce), without a
/// terminal.
static STDIN_ONCE: AtomicBool = AtomicBool::new(false);

fn lock_stdin() -> std::sync::MutexGuard<'static, Option<UnixStream>> {
    STDIN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A client attached to the workload (`shards attach`): where its output goes, and its
/// connection, told the workload's status.
struct Attacher {
    id: u64,
    out: fs::File,
    err: fs::File,
    client: shards_ipc::Stream,
}

/// Those attached, which the output's relay writes to: a lock of its own, apart from the
/// stdin's, so that a guest that has stopped reading stdin stalls no output.
static ATTACHED: Mutex<Vec<Attacher>> = Mutex::new(Vec::new());

fn lock_attached() -> std::sync::MutexGuard<'static, Vec<Attacher>> {
    ATTACHED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The workload's output to each client attached, as it comes, waited for as the run's own
/// client is. One that has gone is let go.
fn fan_out(payload: &[u8], stderr: bool) {
    lock_attached().retain_mut(|a| {
        let to = if stderr { &mut a.err } else { &mut a.out };
        to.write_all(payload).is_ok()
    });
}

/// A client to attach (`kind::ATTACH_RUN`): its connection and stdio, and whether its stdin
/// is the workload's.
pub struct AttachRequest {
    pub client: shards_ipc::Stream,
    pub stdin: fs::File,
    pub stdout: fs::File,
    pub stderr: fs::File,
    pub reads_stdin: bool,
}

/// Attaches `req`'s client to the workload, as `docker attach` attaches: its output from
/// now, its stdin to the workload's where the workload has one open, its signals and its
/// terminal's size; told the workload's status as it ends. Its stdin's end closes the
/// workload's where the run's own client's would ([`STDIN_ONCE`]); where it does not, its
/// output goes on to the end, where dockerd's attach ends with its stdin and loses it.
pub fn attach(to: &'static ToGuest, req: AttachRequest) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let AttachRequest {
        client,
        stdin,
        stdout,
        stderr,
        reads_stdin,
    } = req;
    let listened = client
        .try_clone()
        .map_err(|e| format!("the client's connection: {e}"))?;
    lock_attached().push(Attacher {
        id,
        out: stdout,
        err: stderr,
        client,
    });
    std::thread::Builder::new()
        .name("attached".into())
        .spawn(move || {
            while let Ok(Some(m)) = shards_ipc::recv(&listened) {
                match m.kind {
                    // Its INTERRUPT is the command's signal (D26).
                    shards_ipc::kind::SIGNAL | shards_ipc::kind::INTERRUPT => {
                        if let Ok(sig) = <[u8; 4]>::try_from(m.payload.as_slice()) {
                            let _ = signal_guest(to, u32::from_be_bytes(sig));
                        }
                    }
                    shards_ipc::kind::RESIZE => {
                        if let Some(size) = Size::decode(&m.payload) {
                            let _ = resize_guest(to, size);
                        }
                    }
                    _ => {}
                }
            }
            lock_attached().retain(|a| a.id != id);
        })
        .map_err(|e| format!("an attached client's thread: {e}"))?;
    if reads_stdin && STDIN_OPEN.load(Ordering::Relaxed) {
        std::thread::Builder::new()
            .name("attached-stdin".into())
            .spawn(move || {
                #[cfg(unix)]
                copy_stdin(&mut |buf| shards_ipc::read(&stdin, buf));
                #[cfg(windows)]
                copy_stdin(&mut |buf| (&stdin).read(buf));
                if STDIN_ONCE.load(Ordering::Relaxed) {
                    close_stdin();
                }
            })
            .map_err(|e| format!("an attached client's stdin thread: {e}"))?;
    }
    Ok(())
}

/// Forwards the signals shards receives to the workload, through `to`, even those shards
/// was started ignoring: a run a script started in the background still passes them on
/// (shards_ipc::take_forwarded). It blocks them in the calling thread, which every thread
/// started later inherits, so call it before the VM starts: then only the forwarder's
/// `sigwait` receives them. A signal that would end shards, arriving before the workload
/// runs, ends shards as it would have, unless it was ignored. With `reads_terminal`, the
/// terminal's job control applies to shards (shards_ipc::forwarded).
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
                // Not forwarded where its workload's control is lost too: a signal that would
                // end shards ends it, its microVM with it, as no other way is left to end it.
                let forwarded = signal_guest(to, linux).is_ok();
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

/// [`forward_signals`] on Windows, which sends a console process no signal but its
/// console's events, taken as Go's os/signal takes them: Ctrl+C
/// and Ctrl+Break as SIGINT; the console's close, a logoff and a shutdown as SIGTERM. One
/// that comes before the workload runs goes on to the default handler, which ends shards,
/// as Unix's signal would.
#[cfg(windows)]
pub fn forward_signals(to: &'static ToGuest, _reads_terminal: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };
    static TO: std::sync::OnceLock<&'static ToGuest> = std::sync::OnceLock::new();
    unsafe extern "system" fn handler(event: u32) -> windows_sys::core::BOOL {
        let linux = match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => 2,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => 15,
            _ => return 0,
        };
        i32::from(TO.get().is_some_and(|to| signal_guest(to, linux).is_ok()))
    }
    if TO.set(to).is_err() {
        return Err("signals forwarded twice".into());
    }
    // SAFETY: a handler of this process's that lives as long as it does.
    if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
        return Err(format!(
            "taking the console's events: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// From here on the workload will run, so frames for its control wait for the guest's dial
/// rather than being refused: a warm VM calls it once it has taken a request.
pub fn will_run(to: &ToGuest) {
    let mut state = lock(to);
    if matches!(state.link, Link::Idle) {
        state.link = Link::Awaiting(Pending::default());
    }
}

/// Sends Linux signal `linux` to the workload through `to`, or holds it until the guest
/// dials ([`Link`]); or says why it cannot.
pub fn signal_guest(to: &ToGuest, linux: u32) -> Result<(), Unsent> {
    is_signal(linux)?;
    to_guest(to, kind::SIGNAL, &linux.to_be_bytes())
}

/// Linux signal `linux` is one the guest delivers: `1..=NSIG`, as init checks it.
fn is_signal(linux: u32) -> Result<(), Unsent> {
    if (1..=run::NSIG).contains(&linux) {
        Ok(())
    } else {
        Err(Unsent::NoSuchSignal(linux))
    }
}

/// Gives the guest the host's answer to its ask on the host's network (D124), as
/// [`signal_guest`] sends a signal.
pub fn bound_guest(to: &ToGuest, bound: &[u8]) -> Result<(), Unsent> {
    to_guest(to, kind::BOUND, bound)
}

/// Sizes the workload's terminal, if it has one, as [`signal_guest`] sends a signal.
pub fn resize_guest(to: &ToGuest, size: Size) -> Result<(), Unsent> {
    to_guest(to, kind::RESIZE, &size.encode())
}

/// Sizes exec, or joiner, `id`'s terminal (D119), as [`signal_guest`] sends a signal.
fn resize_exec(to: &ToGuest, id: u32, size: &[u8]) -> Result<(), Unsent> {
    to_guest(to, kind::EXEC_RESIZE, &[&id.to_be_bytes()[..], size].concat())
}

/// Holds frame `which` for the workload's control, for its guest's dial or its connection's
/// sender; or says why it cannot, at once. It never waits on the guest: no send happens
/// under the link's lock ([`send_frames`]).
fn to_guest(to: &ToGuest, which: u8, payload: &[u8]) -> Result<(), Unsent> {
    let mut state = lock(to);
    match &mut state.link {
        Link::Idle => Err(Unsent::NotRunning),
        Link::Lost(lost) => Err(Unsent::Lost(*lost)),
        Link::Awaiting(pending) => pending.hold(which, payload, false),
        Link::Connected(pending) => {
            pending.hold(which, payload, true)?;
            to.changed.notify_one();
            Ok(())
        }
    }
}

/// The guest has dialed the signal port with `conn`, or a restored guest has dialed again
/// (D176): what waited for it waits now for the connection's sender, first. Its two halves
/// come back, the sender's and the one to [`watch`]; none where its run is over, or no copy
/// can be had to watch it, which loses it ([`lose`]): a connection whose end no one reads
/// could keep an exec waiting for a frame the guest never read.
fn dialed(to: &ToGuest, conn: UnixStream) -> Option<(UnixStream, UnixStream)> {
    let mut state = lock(to);
    let Link::Awaiting(pending) = &mut state.link else {
        // Its run over before it dialed (or a dial past the one its port takes): the guest
        // reads this one's end.
        return None;
    };
    let watched = match conn.try_clone() {
        Ok(watched) => watched,
        Err(e) => {
            drop(state);
            lose(to, Lost::of(&e));
            return None;
        }
    };
    state.link = Link::Connected(std::mem::take(pending));
    Some((conn, watched))
}

/// Takes the guest's signal connection as it comes, `conn`: its sender starts on a thread of
/// its own ([`send_frames`]), what waited going first, a managed run's control boundary is
/// held on it (`capture::Control`), and its end is watched from then on ([`watch`]).
fn take_signals(to: &'static ToGuest, conn: UnixStream, managed: Option<(capture::Control, UnixStream)>) {
    let sender = dialed(to, conn).map(|(sending, watched)| {
        let started = std::thread::Builder::new()
            .name("signal-send".into())
            .spawn(move || send_frames(to, sending));
        (started, watched)
    });
    let watched = match sender {
        Some((Ok(_), watched)) => watched,
        Some((Err(e), _)) => {
            lose(to, Lost::of(&e));
            return end_managed(managed);
        }
        None => return end_managed(managed),
    };
    if let Some((control, boundary)) = managed
        && let Err(e) = control.supervise(&watched, &boundary)
    {
        shards_vmm::debug!("managed control boundary ended: {e}");
        return lose(to, Lost::of(&e));
    }
    watch(to, &watched);
}

/// A managed run's control boundary, which nothing reads or sends on: ended, as it cannot
/// hold.
fn end_managed(managed: Option<(capture::Control, UnixStream)>) {
    if let Some((_, boundary)) = managed {
        end_guest(&boundary);
    }
}

/// Sends what waits for the guest on its signal connection, `conn`, in order: each batch
/// taken under the link's lock, and sent with it let go, so that what holds a send (a guest
/// that reads slowly, or whose vCPUs are held for a pause or a save) holds this thread
/// alone, never a frame's caller, nor the relay that brings the pause's end. Ends as the link
/// leaves `Connected`. A send that fails refuses every frame after it, the first cause
/// standing. Where its error says the guest's end has gone, whether the guest had said its
/// end first, or the connection is lost, is [`watch`]'s to find, which reads that end at
/// once; any other error leaves an end the watch may never read, and loses the connection
/// here ([`lose`]).
fn send_frames(to: &ToGuest, mut conn: UnixStream) {
    let mut state = lock(to);
    loop {
        let batch = match &mut state.link {
            Link::Connected(pending) if !pending.0.is_empty() => std::mem::take(pending),
            Link::Connected(_) => {
                state = to.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            Link::Idle | Link::Awaiting(_) | Link::Lost(_) => return,
        };
        drop(state);
        let sent = batch
            .0
            .iter()
            .try_for_each(|(which, payload)| send(&mut conn, *which, payload));
        state = lock(to);
        if let Err(e) = sent {
            let lost = Lost::of(&e);
            if matches!(state.link, Link::Connected(_)) {
                state.link = Link::Lost(lost);
            }
            drop(state);
            let gone = matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
            );
            if !gone {
                lose(to, lost);
            }
            return;
        }
    }
}

/// Reads the guest's signal connection, `conn`, to its end. The guest sends nothing on it but
/// its managed control, read before this (`capture::Control`), and last its relay's end
/// ([`kind::SIGNALS_END`]): that said, the run is over ([`ended`]); the connection closed or
/// failed without it, or anything else said on it, the workload's control is lost
/// ([`lose`]). Blocked in a read, it costs no wake until one of those.
fn watch(to: &ToGuest, conn: &UnixStream) {
    let mut header = [0u8; run::HEADER];
    match shards_ipc::Polled(conn).read_exact(&mut header) {
        Ok(()) if run::parse_header(header) == Some((kind::SIGNALS_END, 0)) => ended(to),
        Ok(()) => lose(to, Lost::Protocol),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => lose(to, Lost::Closed),
        Err(e) => lose(to, Lost::of(&e)),
    }
}

/// The guest's relay has said its end on the signal connection: the workload and every exec
/// of it are over, and nothing reads what the host would send. Frames are refused as an
/// ended run's from now on, a send that failed meanwhile having met that end, and execs that
/// still wait fail so: nothing is left in the guest to start them.
fn ended(to: &ToGuest) {
    {
        let mut state = lock(to);
        if !matches!(state.link, Link::Connected(_) | Link::Lost(_)) {
            return;
        }
        state.link = Link::Idle;
    }
    to.changed.notify_all();
    hellos::fail_execs(&Unsent::NotRunning.to_string());
}

/// The guest's signal connection has failed, as `lost` says, while its run goes on: frames are
/// refused from now on with why, the first cause found standing; execs that wait for
/// connections they asked the guest for on it, which it may never have read, fail with it;
/// and the VM process's log, its daemon's for a warm VM, says it.
fn lose(to: &ToGuest, lost: Lost) {
    let lost = {
        let mut state = lock(to);
        match &state.link {
            Link::Idle => return,
            Link::Lost(first) => *first,
            Link::Awaiting(_) | Link::Connected(_) => {
                state.link = Link::Lost(lost);
                lost
            }
        }
    };
    to.changed.notify_all();
    let why = Unsent::Lost(lost);
    let failed = hellos::fail_execs(&why.to_string());
    shards_vmm::warn!(
        "{why}: its workload takes no more signals, terminal sizes, answers or execs, its \
         command going on; execs that waited and failed with it: {failed}"
    );
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::log_writer::{log_head, write_parts};
    use crate::segments::{log_segment, new_segment};
    use crate::spec::{INDEX_LINE, INDEX_STDERR};

    /// The exec port's tests one at a time, each with a port of its own.
    fn exec_port_test() -> (
        std::sync::MutexGuard<'static, ()>,
        shards_vmm::vm::ServedPort,
        hellos::Acceptor,
    ) {
        let serial = hellos::TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        hellos::reopen();
        let (port, acceptor) = exec_port().unwrap();
        (serial, port, acceptor)
    }

    #[test]
    fn quiesce_replies_resolve_only_the_waiting_request() {
        use run::quiesce::{REQUEST_BYTES, Reason, Refused};
        let request = [0x5a; REQUEST_BYTES];
        let refusal = Refused::new(request, 7, Reason::Quarantined, 21).unwrap();
        let mut q = Quiescing::Idle;
        q.begin(request).unwrap();
        assert!(matches!(q.begin(request), Err(QuiesceError::Busy)));
        q.refuse(Refused::new([0x5b; REQUEST_BYTES], 7, Reason::Quarantined, 21).unwrap());
        assert!(q.outcome().is_none());
        q.refuse(refusal);
        let error = q.outcome().unwrap().unwrap_err();
        assert!(matches!(&error, QuiesceError::Refused(found) if *found == refusal));
        assert_eq!(
            error.to_string(),
            "its guest refused the save: tenant 7 is quarantined with unknown population; errno 21"
        );
        assert!(matches!(q, Quiescing::Idle));
        q.quiet();
        assert!(q.outcome().is_none());
        q.begin(request).unwrap();
        q.quiet();
        assert!(q.outcome().unwrap().is_ok());
        q.refuse(refusal);
        assert!(q.outcome().is_none());
    }

    #[test]
    fn quiesce_send_failure_and_command_end_keep_their_ownership() {
        let request = [0; run::quiesce::REQUEST_BYTES];
        let mut q = Quiescing::Idle;
        q.begin(request).unwrap();
        q.cancel(&[1; run::quiesce::REQUEST_BYTES]);
        assert!(matches!(q, Quiescing::Waiting(_)));
        q.cancel(&request);
        assert!(matches!(q, Quiescing::Idle));
        q.begin(request).unwrap();
        q = Quiescing::Ended { protocol: None };
        q.cancel(&request);
        q.quiet();
        assert!(matches!(q.begin(request), Err(QuiesceError::Ended)));
        assert!(matches!(q.outcome(), Some(Err(QuiesceError::Ended))));
        assert!(matches!(q, Quiescing::Ended { .. }));
    }

    #[test]
    fn quiesce_bad_response_keeps_its_typed_error_when_the_relay_ends() {
        let mut q = Quiescing::Idle;
        q.begin([0; run::quiesce::REQUEST_BYTES]).unwrap();
        let error = run::quiesce::DecodeError::UnknownReason(u8::MAX);
        q.protocol_failed(error);
        q.end();
        assert!(matches!(q.outcome(), Some(Err(QuiesceError::Protocol(found))) if found == error));
        assert!(matches!(q, Quiescing::Ended { .. }));
    }

    /// A directory of its own, removed when dropped, whether its test passes or panics.
    fn temp(tag: &str) -> shards_testdir::TempDir {
        shards_testdir::TempDir::new(&format!("logger-{tag}")).unwrap()
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
    /// gets its own. Connections that say nothing are admitted while anything waits, and
    /// held no more than wait: the one silent longest goes first.
    #[test]
    fn exec_connections_go_only_to_the_exec_they_name() {
        let (_serial, port, acceptor) = exec_port_test();
        let token = [7u8; run::TOKEN];
        let capture_token = [9u8; run::TOKEN];
        let resumed_token = [10u8; run::TOKEN];
        let (to, arrived) = std::sync::mpsc::sync_channel(1);
        let (capture_to, capture_arrived) = std::sync::mpsc::sync_channel(1);
        let (resumed_to, resumed_arrived) = std::sync::mpsc::sync_channel(1);
        register(token, Target::Exec { id: 1, to }).unwrap();
        register(capture_token, Target::Capture(capture_to)).unwrap();
        register(resumed_token, Target::Resumed(resumed_to)).unwrap();
        let mut idle_peers = Vec::new();
        for _ in 0..4 {
            let (peer, candidate) = UnixStream::pair().unwrap();
            assert_eq!(port.accept(candidate), shards_vmm::vm::Admission::Accepted);
            peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            idle_peers.push(peer);
        }
        let server = std::thread::spawn(move || acceptor.run());
        // Four held where three wait: the first, silent as long as any, goes.
        assert!(matches!(idle_peers[0].read(&mut [0]), Ok(0)));
        let hello = |token: &[u8]| {
            let (mut guest, host) = UnixStream::pair().unwrap();
            guest
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            guest
                .write_all(&run::header(kind::HELLO, token.len() as u32))
                .unwrap();
            guest.write_all(token).unwrap();
            assert_eq!(port.accept(host), shards_vmm::vm::Admission::Accepted);
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
        let mut capture_guest = hello(&capture_token);
        let mut capture_host = capture_arrived
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut duplicate = hello(&capture_token);
        assert!(matches!(duplicate.read(&mut [0]), Ok(0) | Err(_)));
        capture_guest.write_all(b"kept").unwrap();
        let mut kept = [0; 4];
        capture_host.read_exact(&mut kept).unwrap();
        assert_eq!(&kept, b"kept");
        let _resumed = hello(&resumed_token);
        assert!(
            resumed_arrived
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .is_ok()
        );
        let _ours = hello(&token);
        assert!(matches!(
            arrived.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(Ok(_))
        ));
        assert!(hellos::fail(1).is_none(), "its wait is over");
        // Nothing waits now: every connection that never spoke has gone.
        for mut idle in idle_peers {
            assert!(matches!(idle.read(&mut [0]), Ok(0)));
        }
        hellos::close();
        drop(port);
        server.join().unwrap();
    }

    /// An exec's frame carries its token and id beside its spec, and is refused past what
    /// a frame may carry, where the guest would drop the signal connection (review 8.9).
    #[test]
    fn an_exec_frame_stays_within_what_a_frame_carries() {
        let token = [5u8; run::TOKEN];
        let base = Spec {
            argv: vec![b"x".to_vec()],
            ..Spec::default()
        };
        let room = run::MAX_PAYLOAD as usize - run::TOKEN - 4 - base.encoded_len().unwrap();
        let mut fits = base.clone();
        fits.argv[0] = vec![b'x'; 1 + room];
        let frame = exec_frame(&token, 9, &fits).unwrap();
        assert_eq!(frame.len(), run::MAX_PAYLOAD as usize);
        assert_eq!(&frame[..run::TOKEN], &token);
        assert_eq!(&frame[run::TOKEN..run::TOKEN + 4], &9u32.to_be_bytes());
        assert!(run::parse_header(run::header(kind::EXEC, frame.len() as u32)).is_some());
        let mut over = fits;
        over.argv[0].push(b'x');
        assert_eq!(
            exec_frame(&token, 9, &over).unwrap_err(),
            "the command and its environment are too large"
        );
    }

    /// An exec's connection, admitted before the acceptor took it, stays its own as another
    /// exec stops waiting: it had waited in that exec's slot, and went with it.
    #[test]
    fn an_admitted_connection_outlives_another_execs_wait() {
        let (_serial, port, acceptor) = exec_port_test();
        let (first, _first_gets) = std::sync::mpsc::sync_channel(1);
        let (second, second_gets) = std::sync::mpsc::sync_channel(1);
        register([41; run::TOKEN], Target::Exec { id: 41, to: first }).unwrap();
        register([42; run::TOKEN], Target::Exec { id: 42, to: second }).unwrap();
        let (mut guest, host) = UnixStream::pair().unwrap();
        guest
            .write_all(&run::header(kind::HELLO, run::TOKEN as u32))
            .unwrap();
        guest.write_all(&[42; run::TOKEN]).unwrap();
        assert_eq!(port.accept(host), shards_vmm::vm::Admission::Accepted);
        // The first exec's guest says it has no connection, before the acceptor has run.
        exec_failed(&[&41u32.to_be_bytes()[..], b"refused"].concat());
        let server = std::thread::spawn(move || acceptor.run());
        let got = second_gets.recv_timeout(std::time::Duration::from_secs(5));
        hellos::close();
        drop(port);
        server.join().unwrap();
        assert!(
            matches!(got, Ok(Ok(_))),
            "the second exec lost its connection: {got:?}"
        );
    }

    /// Execs at once, as ROOT's 64 (adacachy, 2026-11-12), each registering as the others'
    /// guests connect: every one is admitted and gets its own connection.
    #[test]
    fn execs_at_once_each_get_their_own_connection() {
        let (_serial, port, acceptor) = exec_port_test();
        let server = std::thread::spawn(move || acceptor.run());
        let results: Vec<_> = std::thread::scope(|s| {
            let port = &port;
            let execs: Vec<_> = (0..64u32)
                .map(|i| {
                    s.spawn(move || {
                        let mut token = [0x5a; run::TOKEN];
                        token[..4].copy_from_slice(&i.to_be_bytes());
                        let (to, gets) = std::sync::mpsc::sync_channel(1);
                        register(token, Target::Exec { id: 1000 + i, to }).unwrap();
                        let (mut guest, host) = UnixStream::pair().unwrap();
                        guest
                            .write_all(&run::header(kind::HELLO, run::TOKEN as u32))
                            .unwrap();
                        guest.write_all(&token).unwrap();
                        let admitted = port.accept(host);
                        let got = gets
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .map(|r| r.is_ok());
                        (i, admitted, got)
                    })
                })
                .collect();
            execs.into_iter().map(|e| e.join().unwrap()).collect()
        });
        hellos::close();
        drop(port);
        server.join().unwrap();
        let failed: Vec<_> = results
            .iter()
            .filter(|(_, admitted, got)| *admitted != shards_vmm::vm::Admission::Accepted || *got != Ok(true))
            .collect();
        assert!(failed.is_empty(), "{} of 64 execs: {failed:?}", failed.len());
    }

    /// An exec the guest could make no connection for fails with what the guest said,
    /// rather than wait for one (review 8.8); another exec waits on.
    #[test]
    fn an_exec_the_guest_cannot_connect_fails_with_why() {
        let (_serial, _port, _acceptor) = exec_port_test();
        let (to, failed) = std::sync::mpsc::sync_channel(1);
        let (other, waits) = std::sync::mpsc::sync_channel(1);
        register([3; run::TOKEN], Target::Exec { id: 41, to }).unwrap();
        register([4; run::TOKEN], Target::Exec { id: 42, to: other }).unwrap();
        exec_failed(
            &[
                &41u32.to_be_bytes()[..],
                b"connecting to the host for the command: refused",
            ]
            .concat(),
        );
        assert_eq!(
            failed.try_recv().unwrap().unwrap_err(),
            "connecting to the host for the command: refused"
        );
        assert!(waits.try_recv().is_err(), "another exec failed with it");
        exec_failed(&[0, 0]);
        assert!(hellos::fail(42).is_some(), "it still waits");
        hellos::close();
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
        let mut logger = Logger::new(append(&log), append(&index), 0, keep, in_dir(&dir, keep)).unwrap();
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
            let mut logger = Logger::new(l, i, 0, keep, in_dir(&dir, keep)).unwrap();
            logger.keep(LOG_STDOUT, b"lost\n");
            assert_eq!(logger.lost(), 5, "{broken}");
            assert_eq!(
                fs::read(&log).unwrap(),
                bytes,
                "{broken}: a record left in the log"
            );
            assert_eq!(fs::read(&index).unwrap(), entries, "{broken}: an entry left");
        }
        let mut logger = Logger::new(append(&log), append(&index), 0, keep, in_dir(&dir, keep)).unwrap();
        logger.keep(LOG_STDOUT, b"c\n");
        assert_eq!(fs::read(&index).unwrap().len(), 24);
        assert_eq!(
            u64::from_be_bytes(fs::read(&index).unwrap()[16..24].try_into().unwrap()),
            bytes.len() as u64 | INDEX_LINE,
            "kept whole after the loss"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A log's records each in a segment within its size: whole where it fits, in the next
    /// segment where the one written has no room, cut into segments' worth where it is
    /// longer than one (D161); only its retention's newest segments kept; a segment that
    /// cannot be started costs the records that would go in it, and leaves nothing of
    /// itself (audit A12).
    #[test]
    fn a_log_rotates_and_keeps_its_newest_segments() {
        let dir = temp("rotate");
        let retention = LogRetention { size: 64, files: 3 };
        let first = || (append(&dir.join("log")), append(&dir.join("log.idx")));
        let (log, index) = first();
        let mut logger = Logger::new(log, index, 0, retention, in_dir(&dir, retention)).unwrap();
        // 70 bytes, past a segment's 64: 51 of them filling the first, 19 the second.
        logger.keep(LOG_STDOUT, &[b'_'; 70]);
        assert_eq!(fs::read(dir.join("log")).unwrap().len(), 64);
        assert_eq!(fs::read(dir.join("log.1")).unwrap().len(), 13 + 19);
        for f in ["log", "log.1"] {
            fs::remove_file(dir.join(f)).unwrap();
            fs::remove_file(dir.join(format!("{f}.idx"))).unwrap();
        }
        let (log, index) = first();
        let mut logger = Logger::new(log, index, 0, retention, in_dir(&dir, retention)).unwrap();
        // Ten of 20 bytes, then 100: each segment filled to its 64 bytes before the next,
        // a record cut where it ends. Every third segment repeats the first's: 20 and 18
        // of two; 2, 20 and 3; 17 and 20, the 1 byte left too few for a head and a byte.
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
            ["log.5", "log.5.idx", "log.6", "log.6.idx", "log.7", "log.7.idx"]
        );
        for (seq, records) in [
            (5, vec![(LOG_STDOUT, b'i', 17), (LOG_STDOUT, b'j', 20)]),
            (6, vec![(LOG_STDERR, b'z', 51)]),
            (7, vec![(LOG_STDERR, b'z', 49)]),
        ] {
            let (l, i) = log_segment(seq);
            let bytes = fs::read(dir.join(l)).unwrap();
            let mut at = 0;
            for &(stream, byte, len) in &records {
                assert_eq!(bytes[at], stream, "segment {seq}");
                assert_eq!(
                    &bytes[at + 13..at + 13 + len],
                    &vec![byte; len][..],
                    "segment {seq}"
                );
                at += 13 + len;
            }
            assert_eq!(at, bytes.len(), "segment {seq}");
            assert_eq!(
                fs::read(dir.join(i)).unwrap().len(),
                8 * records.len(),
                "segment {seq}"
            );
        }

        // The next segment's log there already, then its index: neither is taken, and
        // what is there stays.
        for taken in [0, 1] {
            let name = if taken == 0 { "log.8" } else { "log.8.idx" };
            fs::write(dir.join(name), b"taken").unwrap();
            logger.keep(LOG_STDOUT, &[b'q'; 40]);
            assert_eq!(logger.lost(), 40 * (taken + 1));
            assert_eq!(fs::read(dir.join(name)).unwrap(), b"taken");
            let other = if taken == 0 { "log.8.idx" } else { "log.8" };
            assert!(!dir.join(other).exists(), "{other} left of a segment not started");
            fs::remove_file(dir.join(name)).unwrap();
        }
        logger.keep(LOG_STDOUT, &[b'r'; 40]);
        assert_eq!(logger.lost(), 80);
        assert_eq!(fs::read(dir.join("log.8")).unwrap().len(), 53);
        assert!(!dir.join("log.5.idx").exists() && !dir.join("log.5").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The records of the log at `path`: each its stream and its bytes.
    fn records(path: &Path) -> Vec<(u8, Vec<u8>)> {
        let log = fs::read(path).unwrap();
        let mut records = Vec::new();
        let mut at = 0;
        while at < log.len() {
            let len = u32::from_be_bytes(log[at + 9..at + 13].try_into().unwrap()) as usize;
            records.push((log[at], log[at + 13..at + 13 + len].to_vec()));
            at += 13 + len;
        }
        records
    }

    /// A non-blocking log's queue (D162) keeps what it holds, in order, and drops what
    /// comes while it is full, counting it, as moby's ring logger does: output that would
    /// pass its size goes only into an empty queue. Through its own thread, a run's output
    /// that never fills it is kept whole, in order, before the run's relay returns.
    #[test]
    fn a_non_blocking_log_drops_only_what_comes_while_its_queue_is_full() {
        let dir = temp("queue");
        let retention = LogRetention {
            size: 1 << 20,
            files: 2,
        };
        let open = || {
            let (log, index) = (append(&dir.join("log")), append(&dir.join("log.idx")));
            Logger::new(log, index, 0, retention, in_dir(&dir, retention)).unwrap()
        };
        let queue = Queue::new(100);
        queue.push(LOG_STDOUT, &[b'a'; 60]);
        // 110 past 100: dropped; 100 is not past it.
        queue.push(LOG_STDOUT, &[b'b'; 50]);
        queue.push(LOG_STDERR, &[b'c'; 40]);
        queue.push(LOG_STDOUT, b"d");
        assert_eq!(queue.dropped(), 51);
        queue.close();
        let mut logger = open();
        queue.keep_into(&mut logger);
        assert_eq!(
            records(&dir.join("log")),
            [(LOG_STDOUT, vec![b'a'; 60]), (LOG_STDERR, vec![b'c'; 40])]
        );
        // Into an empty queue, however large; so a queue of 0 bytes holds one at a time.
        let none = Queue::new(0);
        none.push(LOG_STDOUT, b"x");
        none.push(LOG_STDOUT, b"y");
        assert_eq!(none.dropped(), 1);
        for f in ["log", "log.idx"] {
            fs::remove_file(dir.join(f)).unwrap();
        }
        let mut logger = open().with_queue(Some(1 << 20));
        let (relayed, lost, dropped) = with_sink(Some(&mut logger), |sink| {
            let sink = sink.unwrap();
            for i in 0..1000u32 {
                sink.keep(LOG_STDOUT, format!("line {i}\n").as_bytes());
            }
            "relayed"
        })
        .unwrap();
        assert_eq!((relayed, lost, dropped), ("relayed", 0, 0));
        let kept: Vec<u8> = records(&dir.join("log"))
            .into_iter()
            .flat_map(|(_, b)| b)
            .collect();
        let sent: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        assert_eq!(kept, sent.into_bytes());
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

    /// A connection's hello as the engine's reader takes it (D181, D230): the token checked
    /// before an offer is read, nothing read past the hello.
    #[cfg(unix)]
    mod engine_hellos {
        use super::*;
        use shards_abi::ring::{Extent, MAX_OFFER, Offer, slot_len};
        use std::sync::Arc;

        const TOKEN: [u8; run::TOKEN] = [7; run::TOKEN];

        fn unheard() -> (Unheard, UnixStream) {
            let (ours, guest) = UnixStream::pair().unwrap();
            ours.set_nonblocking(true).unwrap();
            let memory = Arc::new(shards_vmm::memory::GuestMemory::anonymous(&[(0, 1 << 20)]).unwrap());
            (Unheard::new(ours, RingHandle::new(memory)), guest)
        }

        fn hello(token: &[u8; run::TOKEN], offer: &[u8]) -> Vec<u8> {
            let len = (run::TOKEN + offer.len()) as u32;
            [&run::header(kind::HELLO, len)[..], token, offer].concat()
        }

        fn read_all(u: &mut Unheard) -> Heard {
            loop {
                match u.read(&TOKEN) {
                    Heard::More => {}
                    other => return other,
                }
            }
        }

        fn offer() -> Offer {
            Offer {
                ring: 1 << 16,
                extents: vec![Extent {
                    gpa: 1 << 20,
                    len: slot_len(1 << 16),
                }],
            }
        }

        #[test]
        fn a_plain_hello_is_heard_and_what_follows_it_left_unread() {
            let (mut u, mut guest) = unheard();
            guest
                .write_all(&[hello(&TOKEN, &[]), b"GET /_ping".to_vec()].concat())
                .unwrap();
            assert!(matches!(read_all(&mut u), Heard::Whole(Hello::Plain)));
            let mut rest = [0u8; 10];
            (&u.conn).read_exact(&mut rest).unwrap();
            assert_eq!(&rest, b"GET /_ping");
        }

        #[test]
        fn an_offer_after_the_token_is_read_whole() {
            let (mut u, mut guest) = unheard();
            let o = offer();
            guest
                .write_all(&[hello(&TOKEN, &o.encode()), b"after".to_vec()].concat())
                .unwrap();
            match read_all(&mut u) {
                Heard::Whole(Hello::Offered(Ok(got))) => assert_eq!(got, o),
                _ => panic!("no offer heard"),
            }
            let mut rest = [0u8; 5];
            (&u.conn).read_exact(&mut rest).unwrap();
            assert_eq!(&rest, b"after");
        }

        #[test]
        fn a_wrong_token_is_refused_before_its_offer_is_read() {
            let (mut u, mut guest) = unheard();
            let offer = offer().encode();
            guest.write_all(&hello(&[8; run::TOKEN], &offer)).unwrap();
            assert!(matches!(read_all(&mut u), Heard::Refused));
            // Its buffer never grew past a header and a token.
            assert_eq!(u.hello.len(), run::HEADER + run::TOKEN);
        }

        #[test]
        fn a_hello_longer_than_a_token_and_an_offer_is_refused() {
            let (mut u, mut guest) = unheard();
            let len = (run::TOKEN + MAX_OFFER + 1) as u32;
            guest.write_all(&run::header(kind::HELLO, len)).unwrap();
            assert!(matches!(read_all(&mut u), Heard::Refused));
        }

        #[test]
        fn an_offer_whose_bytes_say_no_slot_is_heard_as_malformed() {
            let (mut u, mut guest) = unheard();
            guest
                .write_all(&hello(&TOKEN, b"not an offer, 32 bytes long....."))
                .unwrap();
            assert!(matches!(read_all(&mut u), Heard::Whole(Hello::Offered(Err(_)))));
        }
    }

    /// Its command's readiness under the nesting grant (D234) is handed on as the guest
    /// sends it, each move's payload whole; a READY that reads as no readiness breaks the
    /// run's protocol.
    #[test]
    fn a_runs_readiness_is_handed_on_as_it_comes() {
        use run::nesting::Ready;
        let frame = |which: u8, payload: &[u8]| {
            let mut f = run::header(which, u32::try_from(payload.len()).unwrap()).to_vec();
            f.extend_from_slice(payload);
            f
        };
        let heard = Mutex::new(Vec::new());
        let readiness = |payload: &[u8]| heard.lock().unwrap().push(payload.to_vec());
        let relayed = |frames: &[Vec<u8>]| {
            let (mut guest, host) = UnixStream::pair().unwrap();
            for f in frames {
                guest.write_all(f).unwrap();
            }
            drop(guest);
            let mut reader = io::BufReader::new(shards_ipc::Polled(&host));
            let records = RunRecords {
                processes: None,
                members: None,
                readiness: Some(&readiness),
                host_net: None,
            };
            relay(&mut reader, &Timing::new(), None, None, records)
        };
        let moves = [Ready::Passed(3).encode(), Ready::Late(5).encode()];
        let ended = relayed(&[
            frame(kind::READY, &moves[0]),
            frame(kind::READY, &moves[1]),
            frame(kind::EXIT, &0u32.to_be_bytes()),
        ])
        .unwrap();
        assert_eq!(ended.status, 0);
        assert_eq!(*heard.lock().unwrap(), moves.map(|m| m.to_vec()));
        let broken = relayed(&[frame(kind::READY, &[0; 9])]);
        assert_eq!(
            broken.err().as_deref(),
            Some("the guest sent a malformed readiness")
        );
    }

    /// An exec's request as the daemon brings it, its stdio the null device's.
    fn exec_request() -> ExecRequest {
        let (client, _peer) = shards_ipc::Stream::pair().unwrap();
        let null = || {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap()
        };
        ExecRequest {
            spec: Spec {
                argv: vec![b"true".to_vec()],
                ..Spec::default()
            },
            interactive: false,
            detached: false,
            told: false,
            number: 1,
            daemon: None,
            client,
            stdin: null(),
            stdout: null(),
            stderr: null(),
        }
    }

    /// `to`'s workload runs, its guest's signal connection dialed: the connection's two
    /// halves, its sender's and the one to watch, and the guest's end.
    fn connected(to: &ToGuest) -> ((UnixStream, UnixStream), UnixStream) {
        will_run(to);
        let (host, guest) = pair();
        (dialed(to, host).unwrap(), guest)
    }

    /// A connection's host end and its guest's, whose reads give up after 5 s, a bound on a
    /// frame that comes at once or never.
    fn pair() -> (UnixStream, UnixStream) {
        let (host, guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        (host, guest)
    }

    /// `to`'s workload runs, its guest's signal connection taken as `serve` takes it, its
    /// sender and its watch on threads of their own ([`take_signals`]): the guest's end, and
    /// the watch's thread.
    fn taken(to: &'static ToGuest) -> (UnixStream, std::thread::JoinHandle<()>) {
        will_run(to);
        let (host, guest) = pair();
        (guest, std::thread::spawn(move || take_signals(to, host, None)))
    }

    /// An exec asked for through `to` on a thread of its own: how it ends.
    fn exec_ends(to: &'static ToGuest) -> std::sync::mpsc::Receiver<Result<u8, String>> {
        let (said, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = said.send(exec_session(to, &mut exec_request()));
        });
        heard
    }

    /// Why the exec `heard` tells of failed, within 5 s: a bound on an event that comes at
    /// once or never.
    fn failure(heard: &std::sync::mpsc::Receiver<Result<u8, String>>) -> String {
        let ended = heard.recv_timeout(std::time::Duration::from_secs(5));
        assert!(
            matches!(ended, Ok(Err(_))),
            "the exec did not fail at once: {ended:?}"
        );
        ended.unwrap().unwrap_err()
    }

    /// The frame `guest` reads next on the signal connection.
    fn next_frame(guest: &mut UnixStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; run::HEADER];
        guest.read_exact(&mut header).unwrap();
        let (which, len) = run::parse_header(header).unwrap();
        let mut payload = vec![0; len as usize];
        guest.read_exact(&mut payload).unwrap();
        (which, payload)
    }

    /// Waits for what `guest` has yet to read: a frame on its way.
    fn readable(guest: &UnixStream) {
        let mut came = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(guest),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) of one pollfd of a socket this test holds open.
        assert_eq!(unsafe { libc::poll(&mut came, 1, 5000) }, 1, "no frame came");
    }

    /// A send on the signal connection that fails, the guest's end of it gone, refuses every
    /// frame after it at once with why: none is held for a sender that cannot send it (at
    /// d35c4cf79 each was told it went, and the later ones queued for ever).
    #[test]
    fn a_send_that_fails_refuses_every_later_frame_at_once() {
        let to = ToGuest::new(Signals::new());
        let ((sending, _watched), guest) = connected(&to);
        drop(guest);
        signal_guest(&to, 15).unwrap();
        send_frames(&to, sending);
        let failed = signal_guest(&to, 15).unwrap_err();
        assert!(matches!(failed, Unsent::Lost(Lost::Failed { .. })), "{failed:?}");
        assert!(
            failed
                .to_string()
                .starts_with("the guest's signal connection is lost: "),
            "{failed}"
        );
        for _ in 0..3 {
            assert_eq!(signal_guest(&to, 15), Err(failed), "a later frame");
        }
        assert_eq!(resize_guest(&to, Size { rows: 1, cols: 1 }), Err(failed));
        assert!(matches!(lock(&to).link, Link::Lost(_)), "nothing waits");
    }

    /// A send that fails with the guest's end still there, which the watch would never read
    /// an end of, loses the connection at once: an exec its frame asked for fails with why,
    /// rather than wait for an end that may never come.
    #[test]
    fn a_send_that_fails_with_its_guest_still_there_loses_the_connection() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        let ((sending, _watched), _guest) = connected(&TO);
        // A send that cannot wait fails as soon as the guest reads no more, its end open.
        sending.set_nonblocking(true).unwrap();
        let heard = exec_ends(&TO);
        while !matches!(&lock(&TO).link, Link::Connected(p) if !p.0.is_empty()) {
            std::thread::yield_now();
        }
        to_guest(&TO, kind::BOUND, &vec![0x5a; 4 << 20]).unwrap();
        send_frames(&TO, sending);
        let why = failure(&heard);
        hellos::close();
        assert!(
            why.starts_with("the guest's signal connection is lost: "),
            "{why}"
        );
    }

    /// An exec whose frame cannot be sent ends at once with why, its wait over (at d35c4cf79
    /// it was told the frame went, and waited for the workload's end).
    #[test]
    fn an_exec_whose_frame_cannot_be_sent_ends_at_once_with_why() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        let (guest, watching) = taken(&TO);
        drop(guest);
        let why = failure(&exec_ends(&TO));
        watching.join().unwrap();
        assert!(
            why.starts_with("the guest's signal connection is lost: "),
            "{why}"
        );
        assert_eq!(hellos::fail_execs("none"), 0, "its wait is over");
        hellos::close();
    }

    /// An exec whose frame went on the signal connection, the guest's end of it closing
    /// before the guest read it and with its end unsaid, fails as the watch reads that
    /// close, and so does every frame after it (at d35c4cf79 nothing read the connection's
    /// end, and the exec waited for the workload's).
    #[test]
    fn an_exec_whose_frame_the_guest_never_read_fails_as_its_connection_closes() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        let (guest, watching) = taken(&TO);
        let heard = exec_ends(&TO);
        // Its frame has come, unread: the exec waits for its connection.
        readable(&guest);
        drop(guest);
        let why = failure(&heard);
        watching.join().unwrap();
        hellos::close();
        assert!(
            why.starts_with("the guest's signal connection is lost: "),
            "{why}"
        );
        assert!(
            matches!(signal_guest(&TO, 15), Err(Unsent::Lost(_))),
            "a later frame"
        );
    }

    /// A guest that says its relay's end before closing the signal connection has ended its
    /// run: what waits fails as an ended run's, and no loss is said, a send that met the
    /// connection's end first having met that end.
    #[test]
    fn a_guest_that_says_its_end_has_ended_its_run() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        let ((sending, watched), mut guest) = connected(&TO);
        let sender = std::thread::spawn(move || send_frames(&TO, sending));
        let heard = exec_ends(&TO);
        assert_eq!(next_frame(&mut guest).0, kind::EXEC);
        guest.write_all(&run::header(kind::SIGNALS_END, 0)).unwrap();
        drop(guest);
        // Held for the sender, whose send meets the connection's end.
        signal_guest(&TO, 15).unwrap();
        sender.join().unwrap();
        assert!(
            matches!(signal_guest(&TO, 15), Err(Unsent::Lost(_))),
            "a send failed"
        );
        watch(&TO, &watched);
        assert_eq!(failure(&heard), "the container is not running");
        assert_eq!(signal_guest(&TO, 15), Err(Unsent::NotRunning));
        hellos::close();
    }

    /// An exec whose client types its start (`EXEC_TOLD`, D238) is told `STARTED` as its guest
    /// starts it, and of one its guest never starts, why, by its class and the guest's words
    /// whole, before its end; an exec not so asked tells neither. The client's `EXIT` is
    /// `exec`'s, sent here as it sends it.
    #[test]
    fn a_told_exec_hears_its_start_or_why_it_never_started() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, port, acceptor) = exec_port_test();
        let server = std::thread::spawn(move || acceptor.run());
        let ((sending, watched), mut guest) = connected(&TO);
        let sender = std::thread::spawn(move || send_frames(&TO, sending));
        let frame =
            |which: u8, payload: &[u8]| [&run::header(which, payload.len() as u32)[..], payload].concat();
        let ended = run::members::Refusal::Ended.encode(b"member 0 has ended");
        let refused = [&[run::exec_failed::MEMBER][..], &ended[..]].concat();
        for (told, refusal) in [
            (true, None),
            (true, Some(&refused)),
            (false, None),
            (false, Some(&refused)),
        ] {
            let (client, peer) = shards_ipc::Stream::pair().unwrap();
            let mut req = ExecRequest {
                told,
                client,
                ..exec_request()
            };
            let session = std::thread::spawn(move || {
                let status = exec_session(&TO, &mut req);
                let code = *status.as_ref().unwrap_or(&1);
                shards_ipc::send(&req.client, shards_ipc::kind::EXIT, &[code], &[]).unwrap();
                status
            });
            // Its guest takes its frame and dials its connection with its token.
            let (which, asked) = next_frame(&mut guest);
            assert_eq!(which, kind::EXEC);
            let (mut conn, host) = UnixStream::pair().unwrap();
            conn.write_all(&frame(kind::HELLO, &asked[..run::TOKEN])).unwrap();
            assert_eq!(port.accept(host), shards_vmm::vm::Admission::Accepted);
            match refusal {
                None => conn.write_all(&frame(kind::STARTED, &[])).unwrap(),
                Some(why) => conn.write_all(&frame(kind::SYSTEM_ERR, why)).unwrap(),
            }
            conn.write_all(&frame(kind::EXIT, &0u32.to_be_bytes())).unwrap();
            let status = session.join().unwrap().unwrap();
            let mut heard = Vec::new();
            loop {
                let said = shards_ipc::recv(&peer).unwrap().unwrap();
                let last = said.kind == shards_ipc::kind::EXIT;
                heard.push((said.kind, said.payload));
                if last {
                    break;
                }
            }
            let mut expected = match (told, refusal) {
                (true, None) => vec![(shards_ipc::kind::STARTED, Vec::new())],
                (true, Some(why)) => vec![(shards_ipc::kind::EXEC_UNSTARTED, why.clone())],
                (false, _) => Vec::new(),
            };
            expected.push((shards_ipc::kind::EXIT, vec![status]));
            assert_eq!(heard, expected, "told {told}, refused {}", refusal.is_some());
            // A member that has ended refuses it: `docker exec`'s 1, as the daemon refuses.
            assert_eq!(status, if refusal.is_some() { 1 } else { 0 });
        }
        guest.write_all(&run::header(kind::SIGNALS_END, 0)).unwrap();
        drop(guest);
        signal_guest(&TO, 15).unwrap();
        sender.join().unwrap();
        watch(&TO, &watched);
        hellos::close();
        drop(port);
        server.join().unwrap();
    }

    /// The signal connection is taken as it comes: what waited goes on it, and its end is
    /// watched from then on.
    #[test]
    fn a_signal_connection_is_watched_from_its_dial() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        will_run(&TO);
        signal_guest(&TO, 15).unwrap();
        let (host, mut guest) = pair();
        let taking = std::thread::spawn(move || take_signals(&TO, host, None));
        assert_eq!(
            next_frame(&mut guest),
            (kind::SIGNAL, 15u32.to_be_bytes().to_vec())
        );
        drop(guest);
        taking.join().unwrap();
        assert_eq!(signal_guest(&TO, 15), Err(Unsent::Lost(Lost::Closed)));
    }

    /// The signal connection's watch takes anything but the guest's end as its loss: bytes
    /// the guest never sends, or a read that fails.
    #[test]
    fn a_signal_connection_saying_anything_else_is_lost() {
        let (_serial, _port, _acceptor) = exec_port_test();
        let to = ToGuest::new(Signals::new());
        let ((_sending, watched), mut guest) = connected(&to);
        guest.write_all(&run::header(kind::SIGNAL, 0)).unwrap();
        watch(&to, &watched);
        assert_eq!(signal_guest(&to, 15), Err(Unsent::Lost(Lost::Protocol)));
        // Once its run is over, its connection's end is no loss.
        let to = ToGuest::new(Signals::new());
        let ((_sending, watched), guest) = connected(&to);
        *lock(&to) = Signals::default();
        drop(guest);
        watch(&to, &watched);
        assert_eq!(signal_guest(&to, 15), Err(Unsent::NotRunning));
    }

    /// A frame whose send waits on its guest (more than the connection's sockets hold, its
    /// guest reading nothing, as a paused microVM's reads nothing) holds no other frame's
    /// caller: theirs are answered at once, held behind it, and every frame goes in order as
    /// the guest reads again (at e8d19fc68 that send held the link's lock until the guest
    /// read it, and every other caller waited for the lock: the daemon's relay among them,
    /// and with it the unpause it would read next).
    #[test]
    fn a_send_its_guest_has_yet_to_read_holds_no_other_frames_caller() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        let (mut guest, watching) = taken(&TO);
        let big = vec![0x5a; 4 << 20];
        let first = big.clone();
        let held = std::thread::spawn(move || to_guest(&TO, kind::BOUND, &first));
        // Its send is under way.
        readable(&guest);
        let (said, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = said.send(signal_guest(&TO, 28));
        });
        let answered = heard.recv_timeout(std::time::Duration::from_secs(5));
        // The guest reads again: both go, in order.
        assert_eq!(next_frame(&mut guest), (kind::BOUND, big));
        assert_eq!(
            next_frame(&mut guest),
            (kind::SIGNAL, 28u32.to_be_bytes().to_vec())
        );
        assert_eq!(held.join().unwrap(), Ok(()));
        drop(guest);
        watching.join().unwrap();
        hellos::close();
        assert_eq!(answered, Ok(Ok(())), "a signal waited on another frame's send");
    }

    /// Frames for the workload's control wait for its guest's dial, as a restored guest's
    /// dial again too (D176), one for each thing pending: a signal once, the workload's
    /// terminal's latest size, an exec's start, and its signals and latest size once it is on
    /// its way, an ask's latest answer. Then they go on the connection in the order they came.
    /// Before the workload runs there is no one to send them to; a number that is no Linux
    /// signal is refused.
    #[test]
    fn frames_wait_for_the_dial_one_for_each_thing_pending() {
        let to = ToGuest::new(Signals::new());
        assert_eq!(signal_guest(&to, 15), Err(Unsent::NotRunning));
        will_run(&to);
        assert_eq!(signal_guest(&to, 0), Err(Unsent::NoSuchSignal(0)));
        assert_eq!(signal_guest(&to, 65), Err(Unsent::NoSuchSignal(65)));
        for _ in 0..3 {
            signal_guest(&to, 15).unwrap();
        }
        signal_guest(&to, 2).unwrap();
        let (small, large) = (Size { rows: 24, cols: 80 }, Size { rows: 50, cols: 132 });
        resize_guest(&to, small).unwrap();
        resize_guest(&to, large).unwrap();
        assert_eq!(
            signal_exec(&to, 7, 15),
            Err(Unsent::NotRunning),
            "no exec 7 waits"
        );
        let spec = Spec {
            argv: vec![b"x".to_vec()],
            ..Spec::default()
        };
        let exec = exec_frame(&[1; run::TOKEN], 7, &spec).unwrap();
        to_guest(&to, kind::EXEC, &exec).unwrap();
        for _ in 0..2 {
            signal_exec(&to, 7, 15).unwrap();
        }
        resize_exec(&to, 7, &small.encode()).unwrap();
        resize_exec(&to, 7, &large.encode()).unwrap();
        let answer = |request: u32, errno: i32| {
            run::HostBound {
                request,
                errno,
                port: 0,
                number: 0,
            }
            .encode()
        };
        bound_guest(&to, &answer(1, 98)).unwrap();
        bound_guest(&to, &answer(1, 0)).unwrap();
        bound_guest(&to, &answer(2, 0)).unwrap();
        let ((sending, _watched), mut guest) = {
            let (host, guest) = pair();
            (dialed(&to, host).unwrap(), guest)
        };
        std::thread::scope(|s| {
            s.spawn(|| send_frames(&to, sending));
            let exec_and = |rest: &[u8]| [&7u32.to_be_bytes()[..], rest].concat();
            let expected = [
                (kind::SIGNAL, 15u32.to_be_bytes().to_vec()),
                (kind::SIGNAL, 2u32.to_be_bytes().to_vec()),
                (kind::RESIZE, large.encode().to_vec()),
                (kind::EXEC, exec),
                (kind::EXEC_SIGNAL, exec_and(&15u32.to_be_bytes())),
                (kind::EXEC_RESIZE, exec_and(&large.encode())),
                (kind::BOUND, answer(1, 0).to_vec()),
                (kind::BOUND, answer(2, 0).to_vec()),
            ];
            for (i, frame) in expected.into_iter().enumerate() {
                assert_eq!(next_frame(&mut guest), frame, "frame {i}");
            }
            // Once dialed, an exec's signal goes whether or not its start waits with it.
            signal_guest(&to, 1).unwrap();
            signal_exec(&to, 9, 15).unwrap();
            assert_eq!(
                next_frame(&mut guest),
                (kind::SIGNAL, 1u32.to_be_bytes().to_vec())
            );
            assert_eq!(
                next_frame(&mut guest),
                (
                    kind::EXEC_SIGNAL,
                    [&9u32.to_be_bytes()[..], &15u32.to_be_bytes()].concat()
                )
            );
            // The sender ends with the link.
            *lock(&to) = Signals::default();
            to.changed.notify_all();
        });
    }

    /// A lane to a socket of the run's own command (D251): its ask reaches the guest on the
    /// signal connection, a fresh token and the lane's name; the guest's connection whose
    /// HELLO says that token is the lane once its status says connected, its bytes the
    /// socket's from there; a status of an errno is the lane's refusal; and the guest's
    /// SOCKET_REFUSED, by the lane's id, fails a lane that waits.
    #[test]
    fn a_lane_is_the_connection_its_token_names_once_its_status_says_connected() {
        use std::num::NonZeroU32;
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, port, acceptor) = exec_port_test();
        let server = std::thread::spawn(move || acceptor.run());
        will_run(&TO);
        let ((sending, _watched), mut guest) = {
            let (host, guest) = pair();
            (dialed(&TO, host).unwrap(), guest)
        };
        let dial = |token: &[u8], status: u32, then: &[u8]| {
            let (mut theirs, host) = UnixStream::pair().unwrap();
            theirs
                .write_all(&run::header(kind::HELLO, run::TOKEN as u32))
                .unwrap();
            theirs.write_all(token).unwrap();
            theirs.write_all(&status.to_be_bytes()).unwrap();
            theirs.write_all(then).unwrap();
            assert_eq!(port.accept(host), shards_vmm::vm::Admission::Accepted);
            theirs
        };
        std::thread::scope(|s| {
            s.spawn(|| send_frames(&TO, sending));
            let opened = s.spawn(|| open_socket(&TO, b"control.sock"));
            let (which, asked) = next_frame(&mut guest);
            assert_eq!(which, kind::SOCKET);
            let ask = run::socket::Ask::decode(&asked).unwrap();
            assert_eq!(ask.name, b"control.sock");
            let mut theirs = dial(&ask.token, 0, b"after");
            let mut lane = opened.join().unwrap().unwrap();
            let mut got = [0u8; 5];
            Read::read_exact(&mut lane, &mut got).unwrap();
            assert_eq!(&got, b"after", "the bytes past the status are the socket's");
            lane.write_all(b"back").unwrap();
            let mut back = [0u8; 4];
            theirs.read_exact(&mut back).unwrap();
            assert_eq!(&back, b"back");

            let unreached = s.spawn(|| open_socket(&TO, b"absent.sock"));
            let (_, asked) = next_frame(&mut guest);
            let ask = run::socket::Ask::decode(&asked).unwrap();
            let _theirs = dial(&ask.token, 2, b"");
            assert_eq!(
                unreached.join().unwrap().unwrap_err(),
                Unopened::Refused(NonZeroU32::new(2).unwrap())
            );

            let early = s.spawn(|| open_socket(&TO, b"x.sock"));
            let (_, asked) = next_frame(&mut guest);
            let ask = run::socket::Ask::decode(&asked).unwrap();
            socket_refused(&run::socket::refused(ask.id, run::socket::errno::EACCES));
            assert_eq!(
                early.join().unwrap().unwrap_err(),
                Unopened::Refused(NonZeroU32::new(run::socket::errno::EACCES).unwrap())
            );
            // A lane that waits fails as the signal connection goes, as an exec does.
            let waiting = s.spawn(|| open_socket(&TO, b"y.sock"));
            let _ = next_frame(&mut guest);
            while hellos::fail_execs("lost") == 0 {
                std::thread::yield_now();
            }
            assert_eq!(
                waiting.join().unwrap().unwrap_err(),
                Unopened::Ended("lost".into())
            );
            *lock(&TO) = Signals::default();
            TO.changed.notify_all();
        });
        hellos::close();
        drop(port);
        server.join().unwrap();
    }

    /// What waits for a dial whose sends fail is lost with it: an exec it asked for fails
    /// with why as the watch reads the connection's end.
    #[test]
    fn an_exec_waiting_for_a_dial_that_fails_fails_with_it() {
        static TO: ToGuest = ToGuest::new(Signals::new());
        let (_serial, _port, _acceptor) = exec_port_test();
        will_run(&TO);
        let heard = exec_ends(&TO);
        // Its frame waits for the dial.
        while !matches!(&lock(&TO).link, Link::Awaiting(p) if !p.0.is_empty()) {
            std::thread::yield_now();
        }
        let (host, guest) = UnixStream::pair().unwrap();
        drop(guest);
        take_signals(&TO, host, None);
        let why = failure(&heard);
        hellos::close();
        assert!(
            why.starts_with("the guest's signal connection is lost: "),
            "{why}"
        );
    }

    /// Execs that wait fail with why as the signal connection is lost or ended; a restored
    /// run's waits and a capture's, which ask nothing on it, wait on.
    #[test]
    fn only_execs_fail_as_the_signal_connection_goes() {
        let (_serial, _port, _acceptor) = exec_port_test();
        let (exec, failed) = std::sync::mpsc::sync_channel(1);
        let (resumed, waits) = std::sync::mpsc::sync_channel(1);
        let (capture, captured) = std::sync::mpsc::sync_channel(1);
        register([11; run::TOKEN], Target::Exec { id: 11, to: exec }).unwrap();
        register([12; run::TOKEN], Target::Resumed(resumed)).unwrap();
        register([13; run::TOKEN], Target::Capture(capture)).unwrap();
        assert_eq!(hellos::fail_execs("lost"), 1);
        assert_eq!(failed.try_recv().unwrap().unwrap_err(), "lost");
        assert!(waits.try_recv().is_err() && captured.try_recv().is_err());
        assert_eq!(hellos::fail_execs("lost"), 0);
        hellos::close();
    }
}
