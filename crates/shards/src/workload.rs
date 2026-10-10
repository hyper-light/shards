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
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    Now { spec: Box<Spec>, interactive: bool },
    /// Asked for once the guest is connected and waiting: a warm VM's request.
    Later(&'a dyn Fn() -> Result<Asked<'a>, String>),
}

/// A request, as served: the command, whether it reads stdin, the container's log if its
/// output is kept, and what to call once the command runs.
#[cfg(unix)]
pub struct Asked<'a> {
    pub spec: Spec,
    pub interactive: bool,
    /// Its client does not stay: its stdin, if it reads one, is for `shards attach`.
    pub detached: bool,
    pub log: Option<Logger>,
    pub started: Option<&'a (dyn Fn() + Sync)>,
    /// The container's writable layer from before, to put back before the command runs
    /// (D37); and where its layer goes once it stops.
    pub layer_in: Option<fs::File>,
    pub layer_out: Option<fs::File>,
    /// Told how the command ended, before its layer is saved: a run's client waits for
    /// nothing it does not need.
    pub told: Option<&'a Told<'a>>,
    /// Told the layer is saved whole.
    pub saved: Option<&'a (dyn Fn(Option<u64>) + Sync)>,
}

/// What hears how a served command ended (`Asked::told`).
#[cfg(unix)]
pub type Told<'a> = dyn Fn(&Result<Ended, String>) + Sync + 'a;

/// How a served command ended: its exit status, and if it never ran, why not, in the
/// guest's words.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: u8,
    pub not_run: Option<String>,
    /// Bytes of its output its log could not keep.
    pub lost: u64,
    /// The kernel killed a process of it for want of memory.
    pub oom: bool,
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
    /// Writes a container's log from its segment `seq`, `log` and `index`, which its
    /// daemon made with the container (or its newest, for one started again), going on in
    /// the segments `next` makes.
    pub fn new(
        log: fs::File,
        index: fs::File,
        seq: u64,
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
            seq,
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
        detached,
        log,
        started,
        layer_in,
        layer_out,
        told,
        saved,
    } = match request {
        Request::Now { spec, interactive } => Asked {
            spec: *spec,
            interactive,
            detached: false,
            log: None,
            started: None,
            layer_in: None,
            layer_out: None,
            told: None,
            saved: None,
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
    if let Some(layer) = layer_in {
        send_layer(&mut conn, layer).map_err(|e| format!("sending the container's files: {e}"))?;
    }
    send(&mut conn, kind::SPEC, &spec.encode()).map_err(|e| format!("sending the command: {e}"))?;
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
    } else if !interactive {
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
    if let Some(told) = told {
        told(&status);
    }
    // A container that stays keeps its files, as they are now its command has ended:
    // once its end is told, so that nothing waits for them that does not need them.
    if let (Ok(_), Some(out)) = (&status, layer_out) {
        match receive_layer(&mut conn, out) {
            Ok(used) => {
                if let Some(saved) = saved {
                    saved(used);
                }
            }
            Err(e) => shards_vmm::debug!("saving the container's files: {e}"),
        }
    }
    // Containers joined to its network go on past it (D119): the guest's signal
    // connection, and what waits for the guest, stay theirs.
    let joined = crate::warm::joining();
    if !joined {
        let mut state = lock(to);
        *state = Signals::default();
    }
    *lock_stdin() = None;
    STDIN_OPEN.store(false, Ordering::Relaxed);
    lock_attached().clear();
    // Execs still waiting for the guest will never start.
    if !joined {
        pending().clear();
    }
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
    let mut oom = false;
    // As much as the guest sends at once, in one read, whatever the frames it holds
    // (review 8.20).
    let mut conn = io::BufReader::with_capacity(run::BUFFERED, &*conn);
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
            kind::OOM => oom = true,
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
                    lost: log.as_deref().map_or(0, Logger::lost),
                    oom,
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

/// Sends `layer` as [`kind::LAYER`] frames, then an empty one.
#[cfg(unix)]
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

/// Asks the guest for the container's writable layer ([`kind::SAVE`]) and writes its
/// [`kind::LAYER`] frames to `out` until the empty one; an error if it did not come
/// whole.
#[cfg(unix)]
fn receive_layer(conn: &mut UnixStream, mut out: fs::File) -> io::Result<Option<u64>> {
    send(conn, kind::SAVE, &[])?;
    let mut frame = Vec::new();
    let mut used = None;
    loop {
        let mut h = [0u8; run::HEADER];
        conn.read_exact(&mut h)?;
        let len = match run::parse_header(h) {
            Some((kind::LAYER, len)) => len as usize,
            // What the layer uses, before it (kind::USAGE).
            Some((kind::USAGE, 8)) if used.is_none() => {
                let mut n = [0u8; 8];
                conn.read_exact(&mut n)?;
                used = Some(u64::from_be_bytes(n));
                continue;
            }
            _ => return Err(io::Error::other("the guest sent another frame amid its files")),
        };
        if len == 0 {
            return out.sync_all().map(|()| used);
        }
        frame.resize(len, 0);
        conn.read_exact(&mut frame)?;
        out.write_all(&frame)?;
    }
}

#[cfg(unix)]
fn send(conn: &mut UnixStream, which: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    conn.write_all(&run::header(which, len))?;
    conn.write_all(payload)
}

/// An exec waiting for the guest's connection: its token, its id, and where its
/// connection goes, or why there is none ([`kind::EXEC_FAILED`]).
#[cfg(unix)]
struct Waiting {
    token: [u8; run::TOKEN],
    id: u32,
    to: std::sync::mpsc::Sender<Result<UnixStream, String>>,
}

/// The execs waiting for their connections. Emptied when the workload ends, which tells
/// each still waiting that it never started.
#[cfg(unix)]
static PENDING: Mutex<Vec<Waiting>> = Mutex::new(Vec::new());
#[cfg(unix)]
static NEXT_EXEC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

#[cfg(unix)]
fn pending() -> std::sync::MutexGuard<'static, Vec<Waiting>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Exec `id` will have no connection, for `why`, as the guest said: it fails with it.
#[cfg(unix)]
fn exec_failed(payload: &[u8]) {
    let Some((id, why)) = payload.split_first_chunk::<4>() else {
        return;
    };
    let id = u32::from_be_bytes(*id);
    let waiting = {
        let mut waiting = pending();
        waiting
            .iter()
            .position(|w| w.id == id)
            .map(|i| waiting.swap_remove(i))
    };
    if let Some(w) = waiting {
        let _ = w.to.send(Err(String::from_utf8_lossy(why).into_owned()));
    }
}

/// Takes every guest connection to the exec port ([`run::EXEC_PORT`]): one whose
/// [`kind::HELLO`] names a pending exec's token becomes that exec's; any other is closed.
/// Connections not yet heard from are read by one thread, polling them all (review 8.8):
/// one that says nothing costs a descriptor, not a thread, and holds up no other; the
/// muxer bounds how many there are. They go when the workload's process does.
#[cfg(unix)]
pub fn accept_execs(port: Port) {
    let Ok((wake, woken)) = UnixStream::pair() else {
        return;
    };
    let (arrived, arrivals) = std::sync::mpsc::channel::<UnixStream>();
    if std::thread::Builder::new()
        .name("exec-hellos".into())
        .spawn(move || hellos(&arrivals, &woken))
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

/// A connection to the exec port, and as much of its hello as has come.
#[cfg(unix)]
struct Unheard {
    conn: UnixStream,
    hello: [u8; run::HEADER + run::TOKEN],
    got: usize,
}

/// Reads the hellos of the connections that `arrivals` brings, each told of on `woken`,
/// until the acceptor goes: whole, each goes to its exec, or is closed.
#[cfg(unix)]
fn hellos(arrivals: &std::sync::mpsc::Receiver<UnixStream>, woken: &UnixStream) {
    let mut unheard: Vec<Unheard> = Vec::new();
    let mut byte = [0u8; 64];
    loop {
        let mut polled: Vec<libc::pollfd> = std::iter::once(woken.as_raw_fd())
            .chain(unheard.iter().map(|u| u.conn.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let count = libc::nfds_t::try_from(polled.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: poll(2) on pollfds of descriptors this thread holds open.
        if unsafe { libc::poll(polled.as_mut_ptr(), count, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let Some((wake, rest)) = polled.split_first() else {
            return;
        };
        if wake.revents != 0 {
            match (&*woken).read(&mut byte) {
                // The acceptor is gone, and the workload with it.
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(_) => return,
            }
            while let Ok(conn) = arrivals.try_recv() {
                if conn.set_nonblocking(true).is_ok() {
                    unheard.push(Unheard {
                        conn,
                        hello: [0; run::HEADER + run::TOKEN],
                        got: 0,
                    });
                }
            }
        }
        // From the last, so that each removal leaves the indices before it as they were.
        let ready: Vec<usize> = rest
            .iter()
            .enumerate()
            .filter(|(_, p)| p.revents != 0)
            .map(|(i, _)| i)
            .collect();
        for i in ready.into_iter().rev() {
            let Some(u) = unheard.get_mut(i) else { continue };
            let Some(room) = u.hello.get_mut(u.got..) else {
                continue;
            };
            match (&u.conn).read(room) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                    continue;
                }
                Ok(n @ 1..) => u.got += n,
                // Ended, or failed, before its hello: closed.
                _ => {
                    unheard.swap_remove(i);
                    continue;
                }
            }
            // A header that is no hello's is closed as soon as it has come.
            let head = u.hello.first_chunk::<{ run::HEADER }>().copied();
            let hello = head.and_then(run::parse_header) == Some((kind::HELLO, run::TOKEN as u32));
            if u.got >= run::HEADER && !hello {
                unheard.swap_remove(i);
            } else if u.got == u.hello.len() {
                let u = unheard.swap_remove(i);
                heard(u.conn, &u.hello);
            }
        }
    }
}

/// A connection whose `hello`, its header checked, has come whole: to the exec its token
/// names, or closed.
#[cfg(unix)]
fn heard(conn: UnixStream, hello: &[u8; run::HEADER + run::TOKEN]) {
    let token = hello.get(run::HEADER..).unwrap_or_default();
    // Compared whole, in constant time: a workload that dials the port learns nothing of
    // a token from how fast it is refused.
    let same = |t: &[u8; run::TOKEN]| t.iter().zip(token).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0;
    let to = {
        let mut waiting = pending();
        waiting
            .iter()
            .position(|w| same(&w.token))
            .map(|i| waiting.swap_remove(i).to)
    };
    if let Some(to) = to
        && conn.set_nonblocking(false).is_ok()
    {
        let _ = to.send(Ok(conn));
    }
}

/// A command to run beside the workload (`docker exec`), and its client's: the
/// connection it is answered on, and its stdio.
#[cfg(unix)]
pub struct ExecRequest {
    pub spec: Spec,
    pub interactive: bool,
    pub detached: bool,
    /// The daemon's number for it, and its connection, told of its end (`EXEC_ENDED`).
    pub number: u64,
    pub daemon: Option<UnixStream>,
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
            let (status, answered) = match exec_session(to, &mut req) {
                Ok(ended) => ended,
                Err(e) => {
                    let _ = writeln!(req.stderr, "Error response from daemon: {e}");
                    (Some(1), false)
                }
            };
            if let (Some(status), false) = (status, answered) {
                let _ = shards_ipc::send(&req.client, shards_ipc::kind::EXIT, &[status], &[]);
            }
            if let (Some(status), Some(daemon)) = (status, &req.daemon) {
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
#[cfg(unix)]
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

/// One exec, from asking the guest for it to its status. `None` once a detached exec's
/// client has been told it started; what never started is said on `req.stderr`.
#[cfg(unix)]
/// Its status, if it ended, and whether its client was answered already: a detached one is
/// once it starts, then followed to its end, its output going nowhere.
fn exec_session(to: &'static ToGuest, req: &mut ExecRequest) -> Result<(Option<u8>, bool), String> {
    let mut token = [0u8; run::TOKEN];
    shards_vmm::platform::fill_random(&mut token).map_err(|e| format!("an exec's token: {e}"))?;
    let id = NEXT_EXEC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let payload = exec_frame(&token, id, &req.spec)?;
    let (tx, rx) = std::sync::mpsc::channel();
    pending().push(Waiting { token, id, to: tx });
    if !to_guest(to, kind::EXEC, &payload) {
        pending().retain(|w| w.token != token);
        return Err("the container is not running".into());
    }
    let mut conn = rx
        .recv()
        .map_err(|_| "the container's process ended before the command started".to_string())??;
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
    let mut answered = false;
    let mut conn = io::BufReader::with_capacity(run::BUFFERED, &conn);
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
                answered = true;
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
                return Ok((
                    Some(match not_started {
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
                    }),
                    answered,
                ));
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// A container joining the workload's network (D119): its command, as a run's spec says
/// it, the join entry naming its image's range of the join disk among its setup; its
/// client's connection and stdio, as a run's, none for a detached one; its log.
#[cfg(unix)]
pub struct JoinRequest {
    pub spec: Spec,
    pub interactive: bool,
    pub detached: bool,
    pub client: Option<UnixStream>,
    pub stdin: fs::File,
    pub stdout: Option<fs::File>,
    pub stderr: Option<fs::File>,
    pub log: Option<Logger>,
}

/// Clients attached to joiners (D119), each with its joiner's id in the guest, as
/// [`ATTACHED`] holds the workload's: where a joiner's output goes besides its own client
/// and log, and whose connections hear its status.
#[cfg(unix)]
static JOIN_ATTACHED: Mutex<Vec<(u32, Attacher)>> = Mutex::new(Vec::new());

/// Each running joiner's way in (D119), by its id in the guest: a writer of its connection
/// to the guest, for its attached clients' stdin; and a socket pair whose first end closes
/// as the joiner ends, which each copy of an attached stdin waits on with its stdin.
#[cfg(unix)]
static JOIN_INPUTS: Mutex<Vec<(u32, UnixStream, UnixStream, UnixStream)>> = Mutex::new(Vec::new());

#[cfg(unix)]
fn lock_join_attached() -> std::sync::MutexGuard<'static, Vec<(u32, Attacher)>> {
    JOIN_ATTACHED.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(unix)]
fn lock_join_inputs() -> std::sync::MutexGuard<'static, Vec<(u32, UnixStream, UnixStream, UnixStream)>> {
    JOIN_INPUTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Attaches `req`'s client to joiner `id` in the guest (D119), as `docker attach` attaches
/// to a container: its output from now, its stdin where the joiner reads one, its signals
/// and terminal sizes; told its status as it ends ([`end_joiner`]).
#[cfg(unix)]
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
                    shards_ipc::kind::SIGNAL if m.payload.len() == 4 => {
                        let signal = [&id.to_be_bytes()[..], &m.payload].concat();
                        to_guest(to, kind::EXEC_SIGNAL, &signal);
                    }
                    shards_ipc::kind::RESIZE if Size::decode(&m.payload).is_some() => {
                        let resize = [&id.to_be_bytes()[..], &m.payload].concat();
                        to_guest(to, kind::EXEC_RESIZE, &resize);
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

/// Joiner `id`'s output to each client attached to it; one gone is let go.
#[cfg(unix)]
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
#[cfg(unix)]
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
        let _ = a.client.shutdown(std::net::Shutdown::Both);
    }
}

/// Sends Linux signal `linux` to the guest's exec, or joiner, `id` (D119), as the daemon
/// signals a container: false where the guest has no connection to take it.
#[cfg(unix)]
pub fn signal_exec(to: &ToGuest, id: u32, linux: u32) -> bool {
    to_guest(
        to,
        kind::EXEC_SIGNAL,
        &[&id.to_be_bytes()[..], &linux.to_be_bytes()].concat(),
    )
}

/// Runs joiner `req` in the guest to its end, as a run's command is served: what kept it
/// from starting, or its output to its client and its log, its client's stdin, terminal
/// sizes and signals, and its status. Its id in the guest goes to `on_id` as soon as it
/// has one, and `started` hears when its command runs.
#[cfg(unix)]
pub fn join(
    to: &'static ToGuest,
    req: JoinRequest,
    on_id: &dyn Fn(u32),
    started: &dyn Fn(),
) -> Result<Ended, String> {
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
    let (tx, rx) = std::sync::mpsc::channel();
    pending().push(Waiting { token, id, to: tx });
    if !to_guest(to, kind::EXEC, &payload) {
        pending().retain(|w| w.token != token);
        return Err("the container whose network it joins is not running".into());
    }
    let conn = rx
        .recv()
        .map_err(|_| "the container whose network it joins ended first".to_string())??;
    let mut input = conn.try_clone().map_err(|e| e.to_string())?;
    // Its way in, for the clients that attach to it, until it ends (`end_joiner`).
    let (wake, watched) = UnixStream::pair().map_err(|e| format!("a joiner's attach: {e}"))?;
    lock_join_inputs().push((id, conn.try_clone().map_err(|e| e.to_string())?, wake, watched));
    // Its stdin as a run's (`serve`): its client's, closed as the client's ends where it has
    // no terminal (Docker's StdinOnce); a detached one's open for those who attach; none
    // without `-i`.
    let once = spec.tty.is_none();
    if interactive && !detached {
        let mut stdin = stdin;
        std::thread::Builder::new()
            .name("join-stdin".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = stdin.read(&mut buf) {
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
                            let resize = [&id.to_be_bytes()[..], &m.payload].concat();
                            to_guest(to, kind::EXEC_RESIZE, &resize);
                        }
                        shards_ipc::kind::SIGNAL if m.payload.len() == 4 => {
                            let signal = [&id.to_be_bytes()[..], &m.payload].concat();
                            to_guest(to, kind::EXEC_SIGNAL, &signal);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| format!("a joiner's client: {e}"))?;
    }
    let mut frame = Vec::new();
    let mut not_run: Option<String> = None;
    let mut conn = io::BufReader::with_capacity(run::BUFFERED, &conn);
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
            kind::EXIT => {
                let status: [u8; 4] = payload.try_into().map_err(|_| "malformed exit status")?;
                return Ok(Ended {
                    status: u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX),
                    not_run,
                    lost: log.as_ref().map_or(0, Logger::lost),
                    oom: false,
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// Copies shards' stdin to the workload's, then closes it, where its end is the
/// workload's ([`STDIN_ONCE`]).
#[cfg(unix)]
fn forward_stdin() {
    copy_stdin(&mut io::stdin().lock());
    if STDIN_ONCE.load(Ordering::Relaxed) {
        close_stdin();
    }
}

/// Copies `from` to the workload's stdin until its end.
#[cfg(unix)]
fn copy_stdin(from: &mut impl Read) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
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
#[cfg(unix)]
fn close_stdin() {
    if STDIN_OPEN.swap(false, Ordering::Relaxed)
        && let Some(c) = lock_stdin().as_mut()
    {
        let _ = send(c, kind::STDIN, &[]);
    }
}

/// The run connection's copy that carries the workload's stdin, which every writer of it
/// shares: its client's, and those attached.
#[cfg(unix)]
static STDIN: Mutex<Option<UnixStream>> = Mutex::new(None);
/// The workload reads a stdin, still open.
#[cfg(unix)]
static STDIN_OPEN: AtomicBool = AtomicBool::new(false);
/// Its stdin closes as a client's ends: the run was attached (StdinOnce), without a
/// terminal.
#[cfg(unix)]
static STDIN_ONCE: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn lock_stdin() -> std::sync::MutexGuard<'static, Option<UnixStream>> {
    STDIN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A client attached to the workload (`shards attach`): where its output goes, and its
/// connection, told the workload's status.
#[cfg(unix)]
struct Attacher {
    id: u64,
    out: fs::File,
    err: fs::File,
    client: UnixStream,
}

/// Those attached, which the output's relay writes to: a lock of its own, apart from the
/// stdin's, so that a guest that has stopped reading stdin stalls no output.
#[cfg(unix)]
static ATTACHED: Mutex<Vec<Attacher>> = Mutex::new(Vec::new());

#[cfg(unix)]
fn lock_attached() -> std::sync::MutexGuard<'static, Vec<Attacher>> {
    ATTACHED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The workload's output to each client attached, as dockerd's attach streams it: as it
/// comes, waited for as the run's own client is. One that has gone is let go.
#[cfg(unix)]
fn fan_out(payload: &[u8], stderr: bool) {
    lock_attached().retain_mut(|a| {
        let to = if stderr { &mut a.err } else { &mut a.out };
        to.write_all(payload).is_ok()
    });
}

/// A client to attach (`kind::ATTACH_RUN`): its connection and stdio, and whether its stdin
/// is the workload's.
#[cfg(unix)]
pub struct AttachRequest {
    pub client: UnixStream,
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
#[cfg(unix)]
pub fn attach(to: &'static ToGuest, req: AttachRequest) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let AttachRequest {
        client,
        mut stdin,
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
                    shards_ipc::kind::SIGNAL => {
                        if let Ok(sig) = <[u8; 4]>::try_from(m.payload.as_slice()) {
                            signal_guest(to, u32::from_be_bytes(sig));
                        }
                    }
                    shards_ipc::kind::RESIZE => {
                        if let Some(size) = Size::decode(&m.payload) {
                            resize_guest(to, size);
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
                copy_stdin(&mut stdin);
                if STDIN_ONCE.load(Ordering::Relaxed) {
                    close_stdin();
                }
            })
            .map_err(|e| format!("an attached client's stdin thread: {e}"))?;
    }
    Ok(())
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
        pending().push(Waiting { token, id: 1, to });
        // One that never says anything holds up none of the others.
        let (_idle, idle) = UnixStream::pair().unwrap();
        port.send(idle).unwrap();
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
        assert!(matches!(
            arrived.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(Ok(_))
        ));
        assert!(pending().is_empty());
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

    /// An exec the guest could make no connection for fails with what the guest said,
    /// rather than wait for one (review 8.8); another exec waits on.
    #[test]
    fn an_exec_the_guest_cannot_connect_fails_with_why() {
        let (to, failed) = std::sync::mpsc::channel();
        let (other, waits) = std::sync::mpsc::channel();
        pending().push(Waiting {
            token: [3; run::TOKEN],
            id: 41,
            to,
        });
        pending().push(Waiting {
            token: [4; run::TOKEN],
            id: 42,
            to: other,
        });
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
        pending().retain(|w| w.id != 42);
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
        let mut logger = Logger::new(log, index, 0, retention, in_dir(&dir, retention)).unwrap();
        // A first record past a segment's size is the first segment's, not an empty one's
        // successor's.
        logger.keep(LOG_STDOUT, &[b'_'; 70]);
        assert_eq!(fs::read(dir.join("log")).unwrap().len(), 83);
        assert!(!dir.join("log.1").exists());
        fs::remove_file(dir.join("log")).unwrap();
        fs::remove_file(dir.join("log.idx")).unwrap();
        let (log, index) = first();
        let mut logger = Logger::new(log, index, 0, retention, in_dir(&dir, retention)).unwrap();
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
