//! A warm VM: restored from a template, resumed and connected, waiting for the daemon to
//! hand it one request (docs/design/architecture.md D26). `shards restore DIR --warm FD`
//! runs one, FD being its Unix socket to the daemon.
//!
//! It tells the daemon `READY` once the guest waits for its command. The daemon answers
//! with `RUN`: the command, the client's connection, and the client's stdin, stdout and
//! stderr, which become this process's, so the workload's stdio is the client's. `TAKEN`
//! then lets the daemon close its copies. The client's signals arrive on its connection
//! as `SIGNAL`, and `EXIT` tells it the command's status as soon as the command ends
//! (shards_ipc::kind).
//!
//! The VM lets go of the client's stdio before `EXIT`, so that a pipeline reading it ends
//! with the client, and when the client hangs up, so that a command outliving its client
//! writes nowhere, as a container's output stops reaching a `docker run` that has gone.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use shards_abi::run::Spec;
use shards_ipc::kind;

use crate::spec::NOT_RUN;
use crate::workload::{self, ToGuest};

/// A warm VM's side of the daemon: the socket its request arrives on, and /dev/null, to
/// replace the client's stdio when the VM lets go of the client.
pub struct Link {
    daemon: UnixStream,
    null: File,
}

impl Link {
    /// The daemon's socket at `fd`, which must be an open Unix socket other than stdio.
    pub fn new(fd: RawFd) -> Result<Link, String> {
        let daemon = daemon_socket(fd)?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        Ok(Link { daemon, null })
    }
}

/// The VM's socket to its network process, on which its run's published ports close as
/// the run ends (`--net-release`).
static RELEASE: std::sync::OnceLock<UnixStream> = std::sync::OnceLock::new();
/// The run publishes ports (`RUN_PUBLISHED`): only then is there anything to close.
static PUBLISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The VM's network device's flush (`--net`), asked as the run ends.
static FLUSH: std::sync::OnceLock<shards_vmm::devices::virtio::net::TxFlush> = std::sync::OnceLock::new();

/// Takes `flush` as [`FLUSH`].
pub fn adopt_flush(flush: shards_vmm::devices::virtio::net::TxFlush) {
    let _ = FLUSH.set(flush);
}

/// Takes the socket at `fd` as [`RELEASE`].
pub fn adopt_release(fd: RawFd) -> Result<(), String> {
    let socket = inherited_socket("--net-release", fd)?;
    RELEASE
        .set(socket)
        .map_err(|_| "--net-release given twice".to_string())
}

/// Has the network process close the run's published ports, and waits until it has, so
/// that they are free once the run's end is told, as dockerd frees a container's before
/// its exit is (`docker run --rm -p 80 …; docker run -p 80 …` finds it free). A network
/// process that does not answer within a second is ended with the VM, as the daemon ends
/// one past its grace (`netproc::GRACE`), and frees them then.
///
/// First, what the guest sent before its command ended goes into the network process's
/// ring, which it takes before UNPUBLISH: a datagram a command sends just before it exits
/// reaches its peer, as a container's does, rather than going with the run's ports or its
/// VM (the published UDP flake of 2026-10-06 and 2026-10-10: the guest answered its last
/// datagram and exited, and the answer never came).
fn release_ports() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    if let Some(flush) = FLUSH.get() {
        flush.flush(deadline);
    }
    if !PUBLISHED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(release) = RELEASE.get() else { return };
    if shards_ipc::send(release, kind::UNPUBLISH, &[], &[]).is_err() {
        return;
    }
    // Bounded by poll(2), not a socket option: the VM process's seccomp filter refuses
    // setsockopt(2) (confine.rs).
    let _ = shards_ipc::recv_by(release, deadline);
}

fn daemon_socket(fd: RawFd) -> Result<UnixStream, String> {
    inherited_socket("--warm", fd)
}

/// The socket a spawner left this process at descriptor `fd`, named by option `flag`:
/// owned from here on, and closed on exec.
pub fn inherited_socket(flag: &str, fd: RawFd) -> Result<UnixStream, String> {
    if fd < 3 {
        return Err(format!("{flag} {fd}: not a descriptor of its own"));
    }
    // SAFETY: fstat(2) into a zeroed stat buffer; any descriptor number is safe to ask about.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(format!("{flag} {fd}: {}", io::Error::last_os_error()));
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(format!("{flag} {fd}: not a socket"));
    }
    // SAFETY: an open socket the spawner left for this process alone, owned from here on.
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fcntl(2) on a descriptor we own.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(format!("{flag} {fd}: {}", io::Error::last_os_error()));
    }
    Ok(socket)
}

/// A request, as the daemon hands it over.
pub struct Request {
    /// None for a detached run.
    pub client: Option<UnixStream>,
    pub spec: Spec,
    /// The command reads the client's stdin.
    pub interactive: bool,
    /// The run is detached: its stdin, if it reads one, is for `shards attach`.
    pub detached: bool,
    /// The client asked for the timing line.
    pub timing: bool,
    /// The container's log, if its output is kept.
    pub log: Option<workload::Logger>,
    /// The container's writable layer from before, to put back (D37); and where its
    /// layer goes once it stops, for a container that stays.
    pub layer_in: Option<File>,
    pub layer_out: Option<File>,
}

/// Takes a connection for each of `slots` from the run's share process, in order, and
/// says it has them (`kind::SHARE_ENDS`).
fn shared(link: &UnixStream, slots: &[shards_vmm::devices::virtio::fs::Share]) -> Result<(), String> {
    let mut ends = Vec::with_capacity(slots.len());
    while ends.len() < slots.len() {
        let m = shards_ipc::recv(link)
            .map_err(|e| format!("the share process: {e}"))?
            .ok_or("the share process ended first")?;
        if m.kind != kind::SHARE_ENDS {
            return Err(format!("the share process said message kind {}", m.kind));
        }
        ends.extend(m.fds.into_iter().map(UnixStream::from));
    }
    if ends.len() != slots.len() {
        return Err(format!(
            "the share process gave {} connections for {} shares",
            ends.len(),
            slots.len()
        ));
    }
    shards_ipc::send(link, kind::TAKEN, &[], &[]).map_err(|e| format!("the share process: {e}"))?;
    for (slot, end) in slots.iter().zip(ends) {
        slot.attach(end);
    }
    Ok(())
}

/// Tells the daemon this VM is ready, then waits for its request, and tells the daemon it
/// has taken it. Until then the daemon holds its own copies of the client's descriptors,
/// and gives the request to another VM if this one fails. Then makes the client's stdio
/// this process's, and starts passing the client's signals to the workload through `to`.
pub fn receive(
    link: &Link,
    to: &'static ToGuest,
    may_take: &dyn Fn() -> Result<(), String>,
) -> Result<Request, String> {
    let daemon = &link.daemon;
    // The relays' threads start while the VM is idle, so that none starts on a request's
    // way to the guest (review 8.13).
    let daemon_relay = Relay::start("daemon-signals", to)?;
    let client_relay = Relay::start("client-signals", to)?;
    shards_ipc::send(daemon, kind::READY, &[], &[]).map_err(|e| format!("telling the daemon: {e}"))?;
    let request = shards_ipc::recv(daemon)
        .map_err(|e| format!("waiting for a request: {e}"))?
        .ok_or("the daemon closed without a request")?;
    if request.kind != kind::RUN {
        return Err(format!("expected a request, got message kind {}", request.kind));
    }
    let (flags, rest) = request.payload.split_first().ok_or("an empty request")?;
    let (size, rest) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's retention")?;
    let (files, rest) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's retention")?;
    let (first, spec) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's segment")?;
    let first = u64::from_be_bytes(*first);
    let retention = crate::spec::LogRetention {
        size: u64::from_be_bytes(*size),
        files: u64::from_be_bytes(*files),
    };
    if retention.size == 0 || retention.files == 0 {
        return Err("a request whose log keeps nothing".into());
    }
    let spec = Spec::decode(spec).ok_or("a malformed command")?;
    let (detached, logged) = (
        flags & shards_ipc::RUN_DETACHED != 0,
        flags & shards_ipc::RUN_LOG != 0,
    );
    PUBLISHED.store(
        flags & shards_ipc::RUN_PUBLISHED != 0,
        std::sync::atomic::Ordering::Relaxed,
    );
    let count = request.fds.len();
    let mut fds = request.fds.into_iter();
    let mut next = || {
        fds.next()
            .ok_or(format!("a request brings too few descriptors: {count}"))
    };
    let client = if detached {
        None
    } else {
        Some(UnixStream::from(next()?))
    };
    let stdin = next()?;
    // A detached run's output goes only to its log.
    let (stdout, stderr) = if detached {
        let null = || link.null.try_clone().map_err(|e| format!("/dev/null: {e}"));
        (null()?, null()?)
    } else {
        (File::from(next()?), File::from(next()?))
    };
    // The log's first segment; the daemon makes the rest, as this VM reaches no
    // container's directory (D30), and its answers arrive with its signals.
    let (segments, answers) = mpsc::channel();
    let log = if detached || logged {
        let (log, index) = (File::from(next()?), File::from(next()?));
        let asker = daemon
            .try_clone()
            .map_err(|e| format!("the daemon's connection: {e}"))?;
        let next = segments_from(asker, answers);
        Some(
            workload::Logger::new(log, index, first, retention, next)
                .map_err(|e| format!("the container's log: {e}"))?,
        )
    } else {
        None
    };
    let layer_in = if flags & shards_ipc::RUN_LAYER_IN != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    let layer_out = if flags & shards_ipc::RUN_LAYER_OUT != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    // Its share process (D38), where this VM was made with shared directories.
    let slots = crate::vm_run::SHARES.get().map(Vec::as_slice).unwrap_or_default();
    let share_link = if slots.is_empty() {
        None
    } else {
        Some(UnixStream::from(next()?))
    };
    if fds.next().is_some() {
        return Err(format!("a request brings too many descriptors: {count}"));
    }
    // What must hold for this VM to take a run, checked while the daemon may still give
    // it to another.
    may_take()?;
    // TAKEN before anything of the client's is touched or anything starts: a VM that ends
    // without it never started the run, which the daemon may then hand to another VM
    // (daemon.rs, hand_over). A daemon gone by now has no copies left to close, and hands
    // the run to no other: the command runs. Any other failure leaves the daemon unable to
    // tell whether this VM has the run, so it does not.
    if let Err(e) = shards_ipc::send(daemon, kind::TAKEN, &[], &[])
        && !matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        return Err(format!("telling the daemon the request is taken: {e}"));
    }
    if let Some(link) = share_link {
        shared(&link, slots)?;
    }
    // The VM's own log stays where it was, its daemon's: the client's stderr carries the
    // command's alone, as a container's does (review 8.10).
    if let Ok(log) = std::os::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned() {
        shards_vmm::log::to(File::from(log));
    }
    for (fd, target) in [
        (stdin.as_raw_fd(), 0),
        (stdout.as_raw_fd(), 1),
        (stderr.as_raw_fd(), 2),
    ] {
        // SAFETY: dup2(2) onto this process's standard descriptors, which nothing else in
        // this process holds open as its own.
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(format!(
                "taking the client's stdio: {}",
                io::Error::last_os_error()
            ));
        }
    }
    // The command will run: a signal from here on waits for it rather than being lost.
    workload::will_run(to);
    let from_daemon = daemon
        .try_clone()
        .map_err(|e| format!("the daemon's connection: {e}"))?;
    daemon_relay.relay(from_daemon, From::Daemon(segments));
    // A detached run's client has no relay: its thread ends as its handle goes.
    if let Some(client) = &client {
        let signals = client
            .try_clone()
            .map_err(|e| format!("the client's connection: {e}"))?;
        client_relay.relay(signals, From::Client);
    }
    Ok(Request {
        client,
        spec,
        interactive: flags & shards_ipc::RUN_INTERACTIVE != 0,
        detached,
        timing: flags & shards_ipc::RUN_TIMING != 0,
        log,
        layer_in,
        layer_out,
    })
}

/// A log's next segments, asked of the daemon on `daemon`, whose answers the daemon's relay
/// passes to `answers`: none once the daemon has gone.
fn segments_from(daemon: UnixStream, answers: mpsc::Receiver<Segment>) -> workload::NextSegment {
    Box::new(move |seq| {
        shards_ipc::send(&daemon, kind::LOG_SEGMENT, &seq.to_be_bytes(), &[])?;
        loop {
            let (answered, fds) = answers
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the daemon has gone"))?;
            // One ask is answered at a time; an answer to another is not this one's.
            if answered != seq {
                continue;
            }
            let mut fds = fds.into_iter();
            return match (fds.next(), fds.next(), fds.next()) {
                (Some(log), Some(index), None) => Ok((File::from(log), File::from(index))),
                _ => Err(io::Error::other(format!("the daemon made no log segment {seq}"))),
            };
        }
    })
}

/// The daemon's answer to a `LOG_SEGMENT`: the segment's number, and its log and index.
type Segment = (u64, Vec<OwnedFd>);

/// A relay's thread, waiting for the connection it is to read: started ahead of the
/// request that brings it.
struct Relay(mpsc::Sender<(UnixStream, From<'static>)>);

impl Relay {
    fn start(name: &'static str, to: &'static ToGuest) -> Result<Relay, String> {
        let (relay, told) = mpsc::channel::<(UnixStream, From<'static>)>();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Ok((conn, from)) = told.recv() {
                    relay_signals(&conn, to, from);
                }
            })
            .map_err(|e| format!("{name} thread: {e}"))?;
        Ok(Relay(relay))
    }

    /// Has its thread pass the signals that arrive on `conn`, which `from` says whose it is.
    fn relay(self, conn: UnixStream, from: From<'static>) {
        let _ = self.0.send((conn, from));
    }
}

/// Whose connection a relay reads.
enum From<'a> {
    /// The daemon's, which also brings its answers for the log, passed on here.
    Daemon(mpsc::Sender<Segment>),
    Client,
    /// A joiner's own connection to the daemon (D119), which brings its log's answers, its
    /// signals, for its id in the guest once it has one, execs into it, and clients to
    /// attach to it.
    Joiner(mpsc::Sender<Segment>, &'a std::sync::atomic::AtomicU32),
}

/// Passes the signals that arrive on `conn` to the workload until it closes: the client's,
/// or the daemon's (`shards stop`, `kill`), and the client's terminal sizes. When the
/// client hangs up, the VM lets go of its stdio. A workload outlives its client, as a
/// container outlives `docker run`'s.
fn relay_signals(conn: &UnixStream, to: &'static ToGuest, from: From<'_>) {
    let client = matches!(from, From::Client);
    let joiner = match &from {
        From::Joiner(_, id) => Some(*id),
        _ => None,
    };
    while let Ok(Some(message)) = shards_ipc::recv(conn) {
        match message.kind {
            kind::SEGMENT => {
                if let (From::Daemon(segments) | From::Joiner(segments, ..), Ok(seq)) =
                    (&from, <[u8; 8]>::try_from(message.payload.as_slice()))
                {
                    let _ = segments.send((u64::from_be_bytes(seq), message.fds));
                }
            }
            kind::SIGNAL => {
                if let Ok(signal) = <[u8; 4]>::try_from(message.payload.as_slice()) {
                    let signal = u32::from_be_bytes(signal);
                    match joiner.map(|id| id.load(std::sync::atomic::Ordering::SeqCst)) {
                        // A joiner's, once it has an id in the guest.
                        Some(0) => {}
                        Some(id) => {
                            workload::signal_exec(to, id, signal);
                        }
                        None => {
                            workload::signal_guest(to, signal);
                        }
                    }
                }
            }
            kind::EXEC_RUN if !client => {
                // The client's connection is this process's now: the daemon may let its
                // copy go.
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = shards_ipc::send(conn, kind::EXEC_TAKEN, number, &[]);
                }
                // An exec into a joiner runs in its namespaces (D119).
                let into = joiner.map(|id| id.load(std::sync::atomic::Ordering::SeqCst));
                if let Err(e) = exec_request(message, to, conn.try_clone().ok(), into) {
                    let _ = writeln!(io::stderr(), "shards: an exec: {e}");
                }
            }
            kind::JOIN if !client && joiner.is_none() => {
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = shards_ipc::send(conn, kind::EXEC_TAKEN, number, &[]);
                }
                if let Err(e) = join_request(message, to) {
                    let _ = writeln!(io::stderr(), "shards: a joining container: {e}");
                }
            }
            // The server of its join share (D119), from the first joiner to bring volumes:
            // before that joiner's `JOIN`, and so before its guest mounts the share.
            kind::JOIN_SHARE if !client && joiner.is_none() => {
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = shards_ipc::send(conn, kind::EXEC_TAKEN, number, &[]);
                }
                match (message.fds.into_iter().next(), crate::vm_run::join_share()) {
                    (Some(server), Some(slot)) => slot.attach(UnixStream::from(server)),
                    (None, _) => {
                        let _ = writeln!(io::stderr(), "shards: a join share without its server");
                    }
                    (Some(_), None) => {
                        let _ = writeln!(io::stderr(), "shards: a join share for a microVM without one");
                    }
                }
            }
            kind::ATTACH_RUN if !client => {
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = shards_ipc::send(conn, kind::EXEC_TAKEN, number, &[]);
                }
                // A joiner's clients attach to the joiner, never to the workload (D119).
                let into = joiner.map(|id| id.load(std::sync::atomic::Ordering::SeqCst));
                if let Err(e) = attach_request(message, to, into) {
                    let _ = writeln!(io::stderr(), "shards: an attach: {e}");
                }
            }
            kind::RESIZE if client => {
                if let Some(size) = shards_abi::run::Size::decode(&message.payload) {
                    workload::resize_guest(to, size);
                }
            }
            _ => {}
        }
    }
    if client && let Ok(null) = File::open("/dev/null") {
        let_go(&null);
    }
}

/// Starts the exec the daemon's `EXEC_RUN` asks for (workload::exec): its flags and spec,
/// with its client's connection, stdin, stdout and stderr; the daemon's connection hears
/// of its end.
fn exec_request(
    message: shards_ipc::Message,
    to: &'static ToGuest,
    daemon: Option<UnixStream>,
    into: Option<u32>,
) -> Result<(), String> {
    let (number, rest) = message
        .payload
        .split_first_chunk::<8>()
        .ok_or("an exec without its number")?;
    let number = u64::from_be_bytes(*number);
    let (flags, spec) = rest.split_first().ok_or("an empty exec")?;
    let mut spec = Spec::decode(spec).ok_or("a malformed exec")?;
    match into {
        // A joiner that has no id in the guest yet has no process to exec beside.
        Some(0) => return Err("the container is not running".into()),
        Some(id) => spec.setup.push(format!("in-join={id}").into_bytes()),
        None => {}
    }
    let mut fds = message.fds.into_iter();
    let (Some(client), Some(stdin), Some(stdout), Some(stderr), None) =
        (fds.next(), fds.next(), fds.next(), fds.next(), fds.next())
    else {
        return Err("an exec without its client's four descriptors".into());
    };
    workload::exec(
        to,
        workload::ExecRequest {
            spec,
            interactive: flags & shards_ipc::EXEC_INTERACTIVE != 0,
            detached: flags & shards_ipc::EXEC_DETACHED != 0,
            number,
            daemon,
            client: UnixStream::from(client),
            stdin: File::from(stdin),
            stdout: File::from(stdout),
            stderr: File::from(stderr),
        },
    )
}

/// The joiners this VM serves (D119), which it waits for before it ends: each tells the
/// daemon how it ended.
static JOINERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
static JOINER_ENDED: std::sync::Condvar = std::sync::Condvar::new();

/// Whether a container joined to this VM's network runs (D119).
pub fn joining() -> bool {
    *JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) > 0
}

/// Waits, up to `within`, until every joiner this VM served has told the daemon its end:
/// as the VM ends, the guest has ended them all, and each says so on its own connection.
pub fn await_joiners(within: std::time::Duration) {
    let mut count = JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let deadline = std::time::Instant::now() + within;
    while *count > 0 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        count = JOINER_ENDED
            .wait_timeout(count, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// Takes the container the daemon's `JOIN` brings (D119): its own connection to the daemon,
/// and its image, which the VM's join disk gives a range; then serves its run on a thread
/// of its own, as a warm VM serves its daemon's.
fn join_request(message: shards_ipc::Message, to: &'static ToGuest) -> Result<(), String> {
    let mut fds = message.fds.into_iter();
    let (Some(link), Some(image), None) = (fds.next(), fds.next(), fds.next()) else {
        return Err("a join without its connection and image".into());
    };
    let link = UnixStream::from(link);
    // What fails here, the run says as its own start failing, once it has the run.
    let range = match crate::vm_run::join_disk() {
        Some(disk) => disk
            .attach(File::from(image))
            .map_err(|e| format!("its image: {e}")),
        None => Err("this microVM has no join disk".into()),
    };
    *JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
    let spawned = std::thread::Builder::new().name("joiner".into()).spawn(move || {
        if let Err(e) = joiner(&link, range, to) {
            let _ = writeln!(io::stderr(), "shards: a joining container: {e}");
        }
        *JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
        JOINER_ENDED.notify_all();
    });
    if let Err(e) = spawned {
        *JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
        return Err(format!("a joiner's thread: {e}"));
    }
    Ok(())
}

/// Serves a joiner's run on `link`, as a warm VM serves the run its daemon sends (D119):
/// `RUN`, `TAKEN`, `STARTED`, its log's segments, `DONE`; its command in the guest, its
/// image the join disk's `range`. Its range is let go as it ends.
fn joiner(link: &UnixStream, range: Result<(u64, u64), String>, to: &'static ToGuest) -> Result<(), String> {
    let request = shards_ipc::recv(link)
        .map_err(|e| format!("waiting for its run: {e}"))?
        .ok_or("the daemon closed without its run")?;
    if request.kind != kind::RUN {
        return Err(format!("expected its run, got message kind {}", request.kind));
    }
    let (flags, rest) = request.payload.split_first().ok_or("an empty request")?;
    let (size, rest) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's retention")?;
    let (files, rest) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's retention")?;
    let (first, spec) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's segment")?;
    let retention = crate::spec::LogRetention {
        size: u64::from_be_bytes(*size),
        files: u64::from_be_bytes(*files),
    };
    if retention.size == 0 || retention.files == 0 {
        return Err("a request whose log keeps nothing".into());
    }
    let mut spec = Spec::decode(spec).ok_or("a malformed command")?;
    let detached = flags & shards_ipc::RUN_DETACHED != 0;
    let logged = flags & shards_ipc::RUN_LOG != 0;
    let count = request.fds.len();
    let mut fds = request.fds.into_iter();
    let mut next = || {
        fds.next()
            .ok_or(format!("a request brings too few descriptors: {count}"))
    };
    let client = if detached {
        None
    } else {
        Some(UnixStream::from(next()?))
    };
    let stdin = File::from(next()?);
    let (stdout, stderr) = if detached {
        (None, None)
    } else {
        (Some(File::from(next()?)), Some(File::from(next()?)))
    };
    let (segments, answers) = mpsc::channel();
    let log = if detached || logged {
        let (log, index) = (File::from(next()?), File::from(next()?));
        let asker = link.try_clone().map_err(|e| format!("its connection: {e}"))?;
        Some(
            workload::Logger::new(
                log,
                index,
                u64::from_be_bytes(*first),
                retention,
                segments_from(asker, answers),
            )
            .map_err(|e| format!("its log: {e}"))?,
        )
    } else {
        None
    };
    // Its writable layer from before, to put back, and where to keep it as it stops (D37),
    // as a run's; a joiner has no shares, which the daemon does not send.
    let layer_in = if flags & shards_ipc::RUN_LAYER_IN != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    let layer_out = if flags & shards_ipc::RUN_LAYER_OUT != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    if fds.next().is_some() {
        return Err(format!("a request brings too many descriptors: {count}"));
    }
    if let Err(e) = shards_ipc::send(link, kind::TAKEN, &[], &[])
        && !matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        return Err(format!("telling the daemon its run is taken: {e}"));
    }
    let id = std::sync::atomic::AtomicU32::new(0);
    let attached = range.as_ref().ok().copied();
    // Its writable layer from before on the join disk beside its image, which its child
    // reads as it builds its root: let go of once its command has started.
    let layer = match layer_in {
        Some(file) => crate::vm_run::join_disk()
            .ok_or_else(|| "this microVM has no join disk".to_string())
            .and_then(|disk| disk.attach(file))
            .map(Some)
            .map_err(|e| format!("putting back the container's files: {e}")),
        None => Ok(None),
    };
    let layer_at = layer.as_ref().ok().copied().flatten();
    let let_go_of_layer = || {
        if let (Some((at, _)), Some(disk)) = (layer_at, crate::vm_run::join_disk()) {
            let _ = disk.detach(at);
        }
    };
    // Where what kept its command from starting is said: its client's own stderr.
    let said_to = stderr.as_ref().and_then(|e| e.try_clone().ok());
    std::thread::scope(|s| {
        // Its signals, its log's segments and execs into it, from the daemon, until its
        // connection goes.
        let relayed = link.try_clone().ok().map(|from| {
            let id = &id;
            s.spawn(move || relay_signals(&from, to, From::Joiner(segments, id)))
        });
        let joined = range.and_then(|(offset, len)| {
            spec.setup.push(format!("join={offset},{len}").into_bytes());
            if let Some((at, len)) = layer? {
                spec.setup.push(format!("join-layer={at},{len}").into_bytes());
            }
            workload::join(
                to,
                workload::JoinRequest {
                    spec,
                    interactive: flags & shards_ipc::RUN_INTERACTIVE != 0,
                    detached,
                    client: client.as_ref().and_then(|c| c.try_clone().ok()),
                    stdin,
                    stdout,
                    stderr,
                    log,
                },
                &|guest| id.store(guest, std::sync::atomic::Ordering::SeqCst),
                &|| {
                    let_go_of_layer();
                    let guest = id.load(std::sync::atomic::Ordering::SeqCst);
                    let _ = shards_ipc::send(link, kind::STARTED, &guest.to_be_bytes(), &[]);
                },
            )
        });
        let (ended, conn) = match joined {
            Ok((ended, conn)) => (Ok(ended), Some(conn)),
            Err(e) => (Err(e), None),
        };
        joined_finish(link, client.as_ref(), said_to, &ended);
        // Those attached hear its status as its own client does.
        workload::end_joiner(
            id.load(std::sync::atomic::Ordering::SeqCst),
            match &ended {
                Ok(e) => e.status,
                Err(_) => NOT_RUN,
            },
        );
        // Its writable layer, kept as it stops once its end is told, as a run's (D37):
        // where its command started, and so its root was built.
        if let (Ok(e), Some(mut conn), Some(out)) = (&ended, conn, layer_out)
            && e.not_run.is_none()
        {
            match workload::receive_layer(&mut conn, out) {
                Ok(used) => {
                    let payload = used.map(u64::to_be_bytes);
                    let _ = shards_ipc::send(
                        link,
                        kind::LAYER_SAVED,
                        payload.as_ref().map_or(&[][..], |p| p),
                        &[],
                    );
                }
                Err(e) => shards_vmm::debug!("saving a joiner's files: {e}"),
            }
        }
        // Its connection's reads end here, whatever the daemon does with its end.
        let _ = link.shutdown(std::net::Shutdown::Both);
        if let Some(r) = relayed {
            let _ = r.join();
        }
    });
    if let (Some((offset, _)), Some(disk)) = (attached, crate::vm_run::join_disk()) {
        let _ = disk.detach(offset);
    }
    let_go_of_layer();
    Ok(())
}

/// Tells the daemon how a joiner ended, then its client, as [`finish`] tells a run's: what
/// kept its command from starting goes to the client's stderr first. Unlike a run's, the
/// VM's own stdio is not the joiner's, and stays as it is.
fn joined_finish(
    link: &UnixStream,
    client: Option<&UnixStream>,
    stderr: Option<File>,
    served: &Result<workload::Ended, String>,
) {
    let (mut status, not_run) = match served {
        Ok(ended) => (ended.status, ended.not_run.clone()),
        Err(e) => (NOT_RUN, Some(e.clone())),
    };
    let failed = not_run.as_deref().map(shards_cmdline::commands::start_failed);
    let mut said_status = status;
    let mut text = None;
    if let Some((said, kept)) = &failed {
        let (t, exits) = crate::spec::not_run(said);
        text = Some(t);
        said_status = exits;
        status = *kept;
    }
    if let Ok(ended) = served
        && ended.lost > 0
    {
        let _ = shards_ipc::send(link, kind::LOST, &ended.lost.to_be_bytes(), &[]);
    }
    let mut done = vec![status];
    if let Some((said, _)) = &failed {
        done.extend_from_slice(said.as_bytes());
    }
    let _ = shards_ipc::send(link, kind::DONE, &done, &[]);
    if let Some(client) = client {
        // Its client's stderr is the one its request gave, not this process's.
        if let (Some(text), Some(mut stderr)) = (text, stderr) {
            let _ = writeln!(stderr, "{text}");
        }
        let _ = shards_ipc::send(client, kind::EXIT, &[said_status], &[]);
    }
}

/// Attaches the client the daemon's `ATTACH_RUN` brings (workload::attach): its flag, and
/// its connection, stdin, stdout and stderr.
fn attach_request(
    message: shards_ipc::Message,
    to: &'static ToGuest,
    joiner: Option<u32>,
) -> Result<(), String> {
    let flags = message
        .payload
        .get(8)
        .copied()
        .ok_or("an attach without its flags")?;
    let mut fds = message.fds.into_iter();
    let (Some(client), Some(stdin), Some(stdout), Some(stderr), None) =
        (fds.next(), fds.next(), fds.next(), fds.next(), fds.next())
    else {
        return Err("an attach without its client's four descriptors".into());
    };
    let req = workload::AttachRequest {
        client: UnixStream::from(client),
        stdin: File::from(stdin),
        stdout: File::from(stdout),
        stderr: File::from(stderr),
        reads_stdin: flags & shards_ipc::ATTACH_STDIN != 0,
    };
    match joiner {
        // One that has no id in the guest yet has nothing to attach to.
        Some(0) => Err("an attach to a joiner not yet started".into()),
        Some(id) => workload::attach_joiner(to, id, req),
        None => workload::attach(to, req),
    }
}

/// Tells the daemon the command runs, before the client has anything the command wrote.
pub fn started(link: &Link) {
    let _ = shards_ipc::send(&link.daemon, kind::STARTED, &[], &[]);
}

/// Tells the daemon how the command ended, then the client: `ps` then shows the
/// container exited, or gone for `--rm`, once `run` has returned, as with `docker run`
/// (docker/cli run.go waitExitOrRemoved; the daemon takes what its runs have sent before
/// it answers). What kept the command from running goes to the client's stderr first, as
/// `docker run` reports it, and the VM lets go of the client's stdio before the status
/// goes, so that both arrive before the client exits. A detached run's client is the
/// daemon's to tell: `DONE` carries the reason. With `timing`, the status carries the
/// VM's timing line for the client to print. A `working_set` the VM recorded, with the name
/// of its generation, goes to the daemon before `DONE`, which ends what it reads of the run.
pub fn finish(
    link: &Link,
    client: Option<&UnixStream>,
    served: &Result<workload::Ended, String>,
    timing: Option<&str>,
    working_set: Option<&(String, Vec<u8>)>,
) {
    let (mut status, not_run) = match served {
        Ok(ended) => (ended.status, ended.not_run.as_deref()),
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: {e}");
            (NOT_RUN, None)
        }
    };
    // A command that never started: what dockerd would say, and the exit code its
    // container keeps (shards_cmdline::commands::start_failed).
    let failed = not_run.map(shards_cmdline::commands::start_failed);
    let mut said_status = status;
    if let Some((said, kept)) = &failed {
        let (text, exits) = crate::spec::not_run(said);
        if client.is_some() {
            let _ = writeln!(io::stderr(), "{text}");
        }
        said_status = exits;
        status = *kept;
    }
    let_go(&link.null);
    let mut done = vec![status];
    if let Some((said, _)) = &failed {
        done.extend_from_slice(said.as_bytes());
    }
    if let Ok(ended) = &served
        && ended.lost > 0
    {
        let _ = shards_ipc::send(&link.daemon, kind::LOST, &ended.lost.to_be_bytes(), &[]);
    }
    if served.as_ref().is_ok_and(|ended| ended.oom) {
        let _ = shards_ipc::send(&link.daemon, kind::OOM, &[], &[]);
    }
    if let Some((name, set)) = working_set {
        let _ = shards_ipc::working_set_parts(name, set, |part| {
            shards_ipc::send(&link.daemon, kind::WORKING_SET, part, &[])
        });
    }
    release_ports();
    let _ = shards_ipc::send(&link.daemon, kind::DONE, &done, &[]);
    if let Some(client) = client {
        let mut payload = vec![said_status];
        payload.extend_from_slice(timing.unwrap_or_default().as_bytes());
        let _ = shards_ipc::send(client, kind::EXIT, &payload, &[]);
    }
}

/// Tells the daemon the container's writable layer is whole where it asked for it.
pub fn layer_saved(link: &Link, used: Option<u64>) {
    let payload = used.map(u64::to_be_bytes);
    let _ = shards_ipc::send(
        &link.daemon,
        kind::LAYER_SAVED,
        payload.as_ref().map_or(&[][..], |p| p),
        &[],
    );
}

/// Stops using the client's stdio: this process's standard descriptors become `null`.
fn let_go(null: &File) {
    for target in [0, 1, 2] {
        // SAFETY: dup2(2) onto this process's own standard descriptors. Should it fail,
        // the descriptor stays the client's until this process exits.
        unsafe { libc::dup2(null.as_raw_fd(), target) };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::fd::AsFd as _;

    /// A request that brings fewer descriptors than it needs, or more, is refused, in words
    /// that say which, before the VM takes it.
    #[test]
    fn a_request_with_the_wrong_descriptors_is_refused() {
        static TO_GUEST: ToGuest = ToGuest::new(workload::Signals::new());
        // A detached run's: its stdin, its log and its index.
        for (given, said) in [
            (1, "a request brings too few descriptors: 1"),
            (4, "a request brings too many descriptors: 4"),
        ] {
            let (ours, daemon) = UnixStream::pair().unwrap();
            let link = Link {
                daemon: ours,
                null: File::open("/dev/null").unwrap(),
            };
            let null = File::open("/dev/null").unwrap();
            let fds = vec![null.as_fd(); given];
            let refused = std::thread::scope(|s| {
                let receiving = s.spawn(|| receive(&link, &TO_GUEST, &|| Ok(())));
                assert_eq!(shards_ipc::recv(&daemon).unwrap().unwrap().kind, kind::READY);
                let mut payload = vec![shards_ipc::RUN_DETACHED | shards_ipc::RUN_LOG];
                payload.extend(1u64.to_be_bytes());
                payload.extend(1u64.to_be_bytes());
                // Its log's first segment.
                payload.extend(0u64.to_be_bytes());
                Spec::default().encode_into(&mut payload);
                shards_ipc::send(&daemon, kind::RUN, &payload, &fds).unwrap();
                receiving.join().unwrap().err()
            });
            assert_eq!(refused.as_deref(), Some(said));
            // Nothing taken.
            daemon.set_nonblocking(true).unwrap();
            assert!(shards_ipc::recv(&daemon).is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock));
        }
    }
}
