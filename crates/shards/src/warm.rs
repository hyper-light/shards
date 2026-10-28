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
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::mpsc;

use shards_abi::run::Spec;
// The daemon's link and its clients' connections: Unix sockets on Unix, shards' own pipes
// on Windows (D195). The guest's connections are the VMM's (workload.rs).
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::{AsFd as _, OwnedFd};
use shards_ipc::kind;

use crate::spec::NOT_RUN;
use crate::workload::{self, ToGuest};

/// A warm VM's side of the daemon: the socket its request arrives on. The null device that
/// replaces the client's stdio when the VM lets go of the client is held beside it
/// ([`workload::null`]).
pub struct Link {
    daemon: UnixStream,
}

impl Link {
    /// The daemon's link, `--warm`'s `daemon`.
    pub fn new(daemon: UnixStream) -> Link {
        Link { daemon }
    }

    /// The daemon's socket at `fd`, which must be an open Unix socket other than stdio.
    #[cfg(unix)]
    pub fn from_fd(fd: RawFd) -> Result<Link, String> {
        Ok(Link::new(daemon_socket(fd)?))
    }
}

/// The stream a message's handle `fd` is an end of, owned from here on: a socket's
/// descriptor on Unix; on Windows a pipe's end the daemon gave this process.
fn stream(fd: OwnedFd) -> Result<UnixStream, String> {
    #[cfg(unix)]
    return Ok(UnixStream::from(fd));
    #[cfg(windows)]
    return UnixStream::given(fd).map_err(|e| format!("a connection: {e}"));
}

/// One message at a time to the daemon, whichever of the VM's threads sends it, on a
/// connection of the daemon's or a clone of one: as the daemon sends to a VM (its
/// `RunSocket`). Linux queues a stream message past one buffer's worth in pieces
/// (net/unix/af_unix.c `unix_stream_sendmsg`), between which another thread's message can
/// land: a run's working set reached its daemon cut through by another message, which it
/// refused as pages neither aligned nor distinct, and the stream's framing broken there
/// took the VM for ended before its command (PM M319).
static TO_DAEMON: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sends the daemon a message on `daemon` ([`TO_DAEMON`]).
fn to_daemon(
    daemon: &UnixStream,
    kind: u8,
    payload: &[u8],
    fds: &[shards_ipc::fd::BorrowedFd<'_>],
) -> io::Result<()> {
    let _turn = TO_DAEMON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shards_ipc::send(daemon, kind, payload, fds)
}

/// The VM's socket to its network process, on which its run's published ports close as
/// the run ends (`--net-release`; on Windows the AF_UNIX socket given it as `net-release`,
/// which that process waits on with its own, PM M213).
static RELEASE: std::sync::OnceLock<shards_vmm::platform::UnixStream> = std::sync::OnceLock::new();
/// The run publishes ports (`RUN_PUBLISHED`): only then is there anything to close.
static PUBLISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the VM holds a host's devices, VFIO groups (D154): whose run is over for its
/// client only once they are free, as `docker run` returns once its container's
/// processes, which held its devices, have gone ([`finish`]).
pub static HOST_DEVICES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This process's pidfd (pidfd_open(2)), which polls readable once it has ended, its
/// files, a host's devices among them, closed before (exit_files runs before
/// exit_notify). None where it cannot be had.
#[cfg(target_os = "linux")]
fn own_pidfd() -> Option<OwnedFd> {
    // SAFETY: pidfd_open(2) of this process, no flags; a fresh descriptor on success.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    let fd = RawFd::try_from(fd).ok().filter(|&fd| fd >= 0)?;
    // SAFETY: the descriptor pidfd_open just made, owned from here on.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn own_pidfd() -> Option<OwnedFd> {
    None
}

/// The VM's network device's flush (`--net`), asked as the run ends.
static FLUSH: std::sync::OnceLock<shards_vmm::devices::virtio::net::TxFlush> = std::sync::OnceLock::new();

/// On the host's network (D124): the daemon's connection the guest's asks go on, the
/// containers whose processes asked for a port of the host's, and those whose ports the
/// daemon has said it let go of (`HOST_RELEASE`).
static HOST: Host = Host {
    daemon: std::sync::OnceLock::new(),
    asked: std::sync::Mutex::new(Vec::new()),
    released: std::sync::Mutex::new(Vec::new()),
    said: std::sync::Condvar::new(),
};

struct Host {
    daemon: std::sync::OnceLock<UnixStream>,
    asked: std::sync::Mutex<Vec<u32>>,
    released: std::sync::Mutex<Vec<u32>>,
    said: std::sync::Condvar,
}

/// Takes `flush` as [`FLUSH`].
pub fn adopt_flush(flush: shards_vmm::devices::virtio::net::TxFlush) {
    let _ = FLUSH.set(flush);
}

/// Takes the socket at `fd` as [`RELEASE`].
#[cfg(unix)]
pub fn adopt_release(fd: RawFd) -> Result<(), String> {
    let socket = inherited_socket("--net-release", fd)?;
    RELEASE
        .set(socket)
        .map_err(|_| "--net-release given twice".to_string())
}

/// Takes `socket`, its spawner's `net-release`, as [`RELEASE`] (Windows).
#[cfg(windows)]
pub fn adopt_release(socket: shards_vmm::platform::UnixStream) -> Result<(), String> {
    RELEASE
        .set(socket)
        .map_err(|_| "net-release given twice".to_string())
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
    #[cfg(unix)]
    {
        if shards_ipc::send(release, kind::UNPUBLISH, &[], &[]).is_err() {
            return;
        }
        // Bounded by poll(2), not a socket option: the VM process's seccomp filter refuses
        // setsockopt(2) (confine.rs).
        let _ = shards_ipc::recv_by(release, deadline);
    }
    #[cfg(windows)]
    {
        if shards_ipc::write_message(release, kind::UNPUBLISH, &[]).is_err() {
            return;
        }
        // Bounded by a poll before the read (shards_afunix's read timeout).
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !left.is_zero() && release.set_read_timeout(Some(left)).is_ok() {
            let _ = shards_ipc::read_message(release);
        }
    }
}

#[cfg(unix)]
fn daemon_socket(fd: RawFd) -> Result<UnixStream, String> {
    inherited_socket("--warm", fd)
}

/// The socket a spawner left this process at descriptor `fd`, named by option `flag`:
/// owned from here on, and closed on exec.
#[cfg(unix)]
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
    /// The most memory its guest may plug, in bytes: the run's own (D148).
    pub plug: u64,
    /// Its microVM's I/O limits (D178): bytes and requests a second, 0 for none.
    pub io: (u64, u64),
    /// The CPUs whose time its microVM's vCPUs may use of the host's together (D191), 0
    /// for every one.
    pub cpus: u32,
    /// Its swap (D159), a sparse file of what its limits allow, which the VM maps into its
    /// guest's swap window; or, for a run its microVM was restored from a save of, a copy of
    /// the swap the save kept (D176).
    pub swap: Option<File>,
    /// Its group's CPU budget (D183, slice 3), which its microVM's vCPUs are held by too,
    /// every microVM of the group charging it.
    pub group: Option<File>,
    /// A run its microVM was restored from a save of (D176): the save's tokens, which its
    /// guest dials the host again with; its request brings them in place of a command.
    pub tokens: Option<Vec<u8>>,
}

/// Tells the daemon of the files a share's DAX window maps (D189), for it to watch them
/// for other processes cutting them short (`kind::WATCH`, `kind::UNWATCH`): Linux's alone,
/// whose x86 KVM spins on a guest's access past a file's end where HVF and arm64's KVM end
/// its run, which the window answers (PM M273).
#[cfg(target_os = "linux")]
struct Watching {
    daemon: UnixStream,
    share: u32,
}

#[cfg(target_os = "linux")]
impl shards_vmm::devices::virtio::fs::window::Watch for Watching {
    fn watch(&self, node: u64, file: std::os::fd::BorrowedFd<'_>, size: u64) {
        let mut said = self.share.to_be_bytes().to_vec();
        said.extend_from_slice(&node.to_be_bytes());
        said.extend_from_slice(&size.to_be_bytes());
        if let Err(e) = to_daemon(&self.daemon, kind::WATCH, &said, &[file]) {
            shards_vmm::warn!("a share's DAX window: telling the daemon of a file it maps: {e}");
        }
    }

    fn unwatch(&self, node: u64) {
        let mut said = self.share.to_be_bytes().to_vec();
        said.extend_from_slice(&node.to_be_bytes());
        if let Err(e) = to_daemon(&self.daemon, kind::UNWATCH, &said, &[]) {
            shards_vmm::debug!("a share's DAX window: telling the daemon of a file it maps no more: {e}");
        }
    }
}

/// Has the daemon watch what each of `slots`' DAX windows maps ([`Watching`]).
#[cfg(target_os = "linux")]
fn watch_windows(daemon: &UnixStream, slots: &[shards_vmm::devices::virtio::fs::Share]) {
    for (share, slot) in slots.iter().enumerate() {
        let (Some(window), Ok(share)) = (slot.window(), u32::try_from(share)) else {
            continue;
        };
        match daemon.try_clone() {
            Ok(daemon) => window.watch_with(Box::new(Watching { daemon, share })),
            Err(e) => shards_vmm::warn!("a share's DAX window: the daemon's connection: {e}"),
        }
    }
}

/// Takes a connection for each of `slots` from the run's share process, in order, and
/// says it has them (`kind::SHARE_ENDS`).
#[cfg(unix)]
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
    // Each takes the tables of the server its guest was served by, where it was restored
    // from a save of its run (D176), before the run's guest runs.
    for (slot, end) in slots.iter().zip(ends) {
        slot.attach(end)?;
    }
    Ok(())
}

/// Tells the daemon this VM is ready, then waits for its request, and tells the daemon it
/// has taken it. Until then the daemon holds its own copies of the client's descriptors,
/// and gives the request to another VM if this one fails. Then makes the client's stdio
/// this process's, and starts passing the client's signals to the workload through `to`.
/// A `RUN` request as a warm VM's run and a joiner's (D119) read it: its head
/// (`shards_ipc::RunHead`), then its command's encoded spec.
struct Head<'a> {
    flags: u8,
    retention: crate::spec::LogRetention,
    first: u64,
    plug: u64,
    /// Non-blocking: the most bytes of output queued for its log.
    queue: Option<u64>,
    /// Its microVM's I/O limits (D178): bytes and requests a second, 0 for none.
    io: (u64, u64),
    /// The CPUs its microVM is held to (D191), 0 for every one.
    cpus: u32,
    /// A group's CPU budget holds it too (D183): its file comes after its swap.
    group: bool,
    spec: &'a [u8],
}

/// Tells the daemon what the run's log lost, and what its log dropped, non-blocking
/// (D162): `LOST`, the two counts, where either is some.
fn tell_lost(daemon: &UnixStream, ended: &workload::Ended) {
    if ended.lost > 0 || ended.dropped > 0 {
        let mut counts = ended.lost.to_be_bytes().to_vec();
        counts.extend(ended.dropped.to_be_bytes());
        let _ = to_daemon(daemon, kind::LOST, &counts, &[]);
    }
}

fn head(payload: &[u8]) -> Result<Head<'_>, String> {
    let (head, spec) = shards_ipc::RunHead::decode(payload)?;
    let retention = crate::spec::LogRetention {
        size: head.log_size,
        files: head.log_files,
    };
    if retention.size == 0 || retention.files == 0 {
        return Err("a request whose log keeps nothing".into());
    }
    Ok(Head {
        flags: head.flags,
        retention,
        first: head.first,
        plug: head.plug,
        queue: head.queue,
        io: (head.io_bytes, head.io_requests),
        cpus: u32::try_from(head.cpus).unwrap_or(u32::MAX),
        group: head.group,
        spec,
    })
}

/// A run its microVM was restored from a save of (`resumed`, D176) is asked for as any
/// other, its request bringing the save's tokens in place of a command.
pub fn receive(
    link: &Link,
    to: &'static ToGuest,
    may_take: &dyn Fn() -> Result<(), String>,
    resumed: bool,
) -> Result<Request, String> {
    let daemon = &link.daemon;
    // The relays' threads start while the VM is idle, so that none starts on a request's
    // way to the guest (review 8.13).
    let daemon_relay = Relay::start("daemon-signals", to)?;
    let client_relay = Relay::start("client-signals", to)?;
    to_daemon(daemon, kind::READY, &[], &[]).map_err(|e| format!("telling the daemon: {e}"))?;
    let request = shards_ipc::recv(daemon)
        .map_err(|e| format!("waiting for a request: {e}"))?
        .ok_or("the daemon closed without a request")?;
    if request.kind != kind::RUN {
        return Err(format!("expected a request, got message kind {}", request.kind));
    }
    let Head {
        flags,
        retention,
        first,
        plug,
        queue,
        io,
        cpus,
        group,
        spec,
    } = head(&request.payload)?;
    let (spec, tokens) = if resumed {
        (Spec::default(), Some(spec.to_vec()))
    } else {
        (Spec::decode(spec).ok_or("a malformed command")?, None)
    };
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
    let client = if detached { None } else { Some(stream(next()?)?) };
    let stdin = next()?;
    // A detached run's output goes only to its log.
    let (stdout, stderr) = if detached {
        let null = || workload::null().map_err(|e| format!("the null device: {e}"));
        (null()?, null()?)
    } else {
        (File::from(next()?), File::from(next()?))
    };
    // The log's first segment; the daemon makes the rest, as this VM reaches no
    // container's directory (D30), and its answers arrive with its signals.
    let (segments, answers) = mpsc::channel();
    let log = if logged {
        let (log, index) = (File::from(next()?), File::from(next()?));
        let asker = daemon
            .try_clone()
            .map_err(|e| format!("the daemon's connection: {e}"))?;
        let next = segments_from(asker, answers);
        Some(
            workload::Logger::new(log, index, first, retention, next)
                .map_err(|e| format!("the container's log: {e}"))?
                .with_queue(queue),
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
    // Its swap (D159), a sparse file of what its limits allow.
    let swap = if flags & shards_ipc::RUN_SWAP != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    // Its group's CPU budget (D183), which every microVM of the group charges.
    let group = if group { Some(File::from(next()?)) } else { None };
    // Its share process (D38), where this VM was made with shared directories: Unix's
    // alone as yet.
    #[cfg(unix)]
    let slots = crate::vm_run::SHARES.get().map(Vec::as_slice).unwrap_or_default();
    #[cfg(unix)]
    let share_link = if slots.is_empty() {
        None
    } else {
        Some(stream(next()?)?)
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
    if let Err(e) = to_daemon(daemon, kind::TAKEN, &[], &[])
        && !matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        return Err(format!("telling the daemon the request is taken: {e}"));
    }
    // Its windows' files watched by the daemon from their first mapping on (D189), a
    // restored run's among them, which its shares map again as they are given.
    #[cfg(target_os = "linux")]
    watch_windows(daemon, slots);
    #[cfg(unix)]
    if let Some(link) = share_link {
        shared(&link, slots)?;
    }
    // The VM's own log stays where it was, its daemon's: the client's stderr carries the
    // command's alone, as a container's does (review 8.10).
    if let Ok(log) = shards_ipc::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned() {
        shards_vmm::log::to(File::from(log));
    }
    set_stdio([stdin, OwnedFd::from(stdout), OwnedFd::from(stderr)])
        .map_err(|e| format!("taking the client's stdio: {e}"))?;
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
        plug,
        io,
        cpus,
        swap,
        group,
        spec,
        interactive: flags & shards_ipc::RUN_INTERACTIVE != 0,
        detached,
        timing: flags & shards_ipc::RUN_TIMING != 0,
        log,
        layer_in,
        layer_out,
        tokens,
    })
}

/// A log's next segments, asked of the daemon on `daemon`, whose answers the daemon's relay
/// passes to `answers`: none once the daemon has gone.
fn segments_from(daemon: UnixStream, answers: mpsc::Receiver<Segment>) -> workload::NextSegment {
    Box::new(move |seq| {
        to_daemon(&daemon, kind::LOG_SEGMENT, &seq.to_be_bytes(), &[])?;
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

/// A pause (true) or resume the daemon asked for: its number, and a copy of the daemon's
/// connection to answer on.
type Pause = (bool, [u8; 8], UnixStream);

/// The pauses and resumes the daemon asks of this VM (`PAUSE`), done in order on a thread
/// of their own, each answered (`PAUSED`) on its own copy of the daemon's connection as it
/// is done: a pause waits for every vCPU to be held, which a vCPU busy with an exit this
/// connection serves must not wait out.
fn pauses() -> Result<&'static mpsc::Sender<Pause>, String> {
    static PAUSES: std::sync::OnceLock<Result<mpsc::Sender<Pause>, String>> = std::sync::OnceLock::new();
    PAUSES
        .get_or_init(|| {
            let (asks, asked) = mpsc::channel::<Pause>();
            std::thread::Builder::new()
                .name("pauses".into())
                .spawn(move || {
                    for (frozen, number, answer) in asked {
                        let done = crate::vm_run::pause(frozen);
                        let said = done.as_ref().err().map(String::as_bytes).unwrap_or_default();
                        let _ = to_daemon(&answer, kind::PAUSED, &[&number[..], said].concat(), &[]);
                    }
                })
                .map(|_| asks)
                .map_err(|e| format!("its pauses' thread: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
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
    // How the run stops if its daemon goes (`STOP_ORDER`).
    let mut order: Option<(u32, Option<std::time::Duration>)> = None;
    while let Ok(Some(message)) = shards_ipc::recv(conn) {
        match message.kind {
            kind::STOP_ORDER if matches!(from, From::Daemon(_)) => {
                if let (Some(signal), Some(grace)) = (
                    message.payload.first_chunk::<4>(),
                    message.payload.get(4..).and_then(|g| g.first_chunk::<8>()),
                ) {
                    let grace = u64::from_be_bytes(*grace);
                    order = Some((
                        u32::from_be_bytes(*signal),
                        (grace != u64::MAX).then(|| std::time::Duration::from_millis(grace)),
                    ));
                }
            }
            kind::SEGMENT => {
                if let (From::Daemon(segments) | From::Joiner(segments, ..), Ok(seq)) =
                    (&from, <[u8; 8]>::try_from(message.payload.as_slice()))
                {
                    let _ = segments.send((u64::from_be_bytes(seq), message.fds));
                }
            }
            // An engine connection the daemon has (D181): this VM's copy goes (M24). On
            // Windows the daemon gives the pipe the connection is relayed onto, or none for
            // a connection it closes.
            kind::ENGINE_CONN if matches!(from, From::Daemon(_)) => {
                if let Ok(n) = <[u8; 8]>::try_from(message.payload.as_slice()) {
                    let held = engine_held().remove(&u64::from_be_bytes(n));
                    #[cfg(windows)]
                    if let (Some(guest), Some(pipe)) = (held, message.fds.into_iter().next()) {
                        relay_engine(guest, pipe);
                    }
                    #[cfg(unix)]
                    drop(held);
                }
            }
            // Its run's limit updated (D148): its guest's most to plug, before the exec that
            // writes its cgroup, which comes after this on the same link.
            kind::PLUG if matches!(from, From::Daemon(_)) => {
                if let Ok(bytes) = <[u8; 8]>::try_from(message.payload.as_slice())
                    && let Err(e) = crate::vm_run::set_plug_most(u64::from_be_bytes(bytes))
                {
                    let _ = writeln!(io::stderr(), "shards: its memory to plug: {e}");
                }
            }
            // Its run's CPU limits updated (D191): the CPUs its vCPUs are held to, before
            // the exec that writes its cgroup.
            kind::CPUS if matches!(from, From::Daemon(_)) => {
                if let Ok(bytes) = <[u8; 8]>::try_from(message.payload.as_slice())
                    && let Err(e) =
                        crate::vm_run::hold_cpus(u32::try_from(u64::from_be_bytes(bytes)).unwrap_or(u32::MAX))
                {
                    let _ = writeln!(io::stderr(), "shards: its CPUs' hold: {e}");
                }
            }
            // Paused where it is, or let go on (`pause`, `unpause`): in order, each
            // answered once done ([`pauses`]).
            kind::PAUSE if matches!(from, From::Daemon(_)) => {
                if let Some((&frozen, number)) = message.payload.split_first()
                    && let Ok(number) = <[u8; 8]>::try_from(number)
                {
                    let asked = conn.try_clone().map_err(|e| e.to_string()).and_then(|answer| {
                        pauses()?
                            .send((frozen == 1, number, answer))
                            .map_err(|_| "its pauses' thread has ended".to_string())
                    });
                    if let Err(e) = asked {
                        let _ = to_daemon(conn, kind::PAUSED, &[&number[..], e.as_bytes()].concat(), &[]);
                    }
                }
            }
            // A file a share's DAX window maps, cut short by another process (D189): mapped
            // as it is now, zeros where it has no pages, and a vCPU that spun there on its
            // guest's read past its end runs on (PM M273).
            #[cfg(unix)]
            kind::CUT if matches!(from, From::Daemon(_)) => {
                let named = message
                    .payload
                    .first_chunk::<4>()
                    .zip(message.payload.get(4..).and_then(|n| n.first_chunk::<8>()));
                if let Some((share, node)) = named {
                    let window = crate::vm_run::SHARES
                        .get()
                        .and_then(|s| s.get(u32::from_be_bytes(*share) as usize))
                        .and_then(|slot| slot.window());
                    if let Some(window) = window {
                        window.cut(u64::from_be_bytes(*node));
                    }
                }
            }
            // The host's answer to the guest's ask on the host's network (D124).
            kind::HOST_BOUND if matches!(from, From::Daemon(_)) => {
                workload::bound_guest(to, &message.payload);
            }
            // A container's ports of the host's, let go as it ends (`release_host`).
            kind::HOST_RELEASE if matches!(from, From::Daemon(_)) => {
                if let Ok(owner) = <[u8; 4]>::try_from(message.payload.as_slice()) {
                    HOST.released
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(u32::from_be_bytes(owner));
                    HOST.said.notify_all();
                }
            }
            // Saved on a thread of its own: as its guest goes quiet for it, the run's output
            // goes on to its log, which may ask the daemon for its next segment, whose
            // answer this relay brings. A microVM to stop after it stops once the daemon has
            // heard (`Saved`).
            kind::SAVE if matches!(from, From::Daemon(_)) => {
                let answer = conn.try_clone();
                // Before any message after this one: a JOIN behind it is refused, so that
                // no joiner starts in the guest as it goes quiet.
                SAVING.store(true, std::sync::atomic::Ordering::SeqCst);
                let saving = std::thread::Builder::new().name("save".into()).spawn(move || {
                    let saved = save_request(message);
                    let said = saved.as_ref().err().map(String::as_bytes).unwrap_or_default();
                    if let Ok(answer) = &answer {
                        let _ = to_daemon(answer, kind::SAVED, said, &[]);
                    }
                    SAVING.store(false, std::sync::atomic::Ordering::SeqCst);
                    drop(saved);
                });
                if let Err(e) = saving {
                    SAVING.store(false, std::sync::atomic::Ordering::SeqCst);
                    let _ = to_daemon(
                        conn,
                        kind::SAVED,
                        format!("its save's thread: {e}").as_bytes(),
                        &[],
                    );
                }
            }
            // A client's INTERRUPT is its command's signal once its run is made (D26).
            kind::SIGNAL | kind::INTERRUPT => {
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
                    let _ = to_daemon(conn, kind::EXEC_TAKEN, number, &[]);
                }
                // An exec into a joiner runs in its namespaces (D119).
                let into = joiner.map(|id| id.load(std::sync::atomic::Ordering::SeqCst));
                if let Err(e) = exec_request(message, to, conn.try_clone().ok(), into) {
                    let _ = writeln!(io::stderr(), "shards: an exec: {e}");
                }
            }
            kind::JOIN if !client && joiner.is_none() => {
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = to_daemon(conn, kind::EXEC_TAKEN, number, &[]);
                }
                if let Err(e) = join_request(message, to) {
                    let _ = writeln!(io::stderr(), "shards: a joining container: {e}");
                }
            }
            // The server of its join share (D119), from the first joiner to bring volumes:
            // before that joiner's `JOIN`, and so before its guest mounts the share.
            #[cfg(unix)]
            kind::JOIN_SHARE if !client && joiner.is_none() => {
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = to_daemon(conn, kind::EXEC_TAKEN, number, &[]);
                }
                match (message.fds.into_iter().next(), crate::vm_run::join_share()) {
                    (Some(server), Some(slot)) => {
                        if let Err(e) = slot.attach(UnixStream::from(server)) {
                            let _ = writeln!(io::stderr(), "shards: a join share: {e}");
                        }
                    }
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
                    let _ = to_daemon(conn, kind::EXEC_TAKEN, number, &[]);
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
    if client && let Ok(null) = workload::null() {
        let_go(&null);
    }
    // The daemon gone with the run going: its stop, its signal and then SIGKILL once its
    // grace is up, so that no run outlives its daemon unreachable. A run already over
    // takes no signal.
    if let (From::Daemon(_), Some((signal, grace))) = (&from, order) {
        let _ = writeln!(io::stderr(), "shards: the daemon is gone: the run stops");
        if signal != 0 {
            workload::signal_guest(to, signal);
            match grace {
                Some(grace) => std::thread::sleep(grace),
                None => return,
            }
        }
        workload::signal_guest(to, 9);
    }
}

/// The microVM's name and the snapshot it was saved as, where the save stopped it (D176):
/// its end is told as the save's.
static SAVED: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Saves this microVM as the daemon's `SAVE` asks (D176): into the generation's files it
/// brings, its memory, its state and the run's swap where it has one, then on or stopped.
fn save_request(message: shards_ipc::Message) -> Result<shards_vmm::vm::Saved, String> {
    // A joiner's processes are in its guest, its run its own container's (D119): a snapshot
    // would keep them, and a restore bring them back, without their containers.
    if *JOINERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) > 0 {
        return Err(
            "containers joined to it run in its microVM, which a snapshot would keep without their containers: save it once they end"
                .into(),
        );
    }
    let (flags, rest) = message.payload.split_first().ok_or("a save without its flags")?;
    let (tokens, names) = rest
        .split_at_checked(2 * shards_abi::run::TOKEN)
        .ok_or("a save without its tokens")?;
    let mut names = names.split(|&b| b == 0).map(|n| {
        std::str::from_utf8(n)
            .map(str::to_string)
            .map_err(|_| "a save's names are no text")
    });
    let (Some(name), Some(snapshot), Some(microvm), None) =
        (names.next(), names.next(), names.next(), names.next())
    else {
        return Err("a save without its generation's, snapshot's and microVM's names".into());
    };
    let (name, snapshot, microvm) = (name?, snapshot?, microvm?);
    let count = message.fds.len();
    let mut fds = message.fds.into_iter().map(File::from);
    let (Some(memory), Some(state)) = (fds.next(), fds.next()) else {
        return Err(format!(
            "a save brings {count} descriptors, without its memory and state"
        ));
    };
    let swap = fds.next();
    // Its writable layer's head and file system (D190), both or neither.
    let layer = match (fds.next(), fds.next()) {
        (Some(head), Some(data)) => Some((head, data)),
        (None, None) => None,
        _ => return Err("a save brings its writable layer's head without its file system".into()),
    };
    // Its DAX windows' heads (D189).
    let window_heads = fds.next();
    if fds.next().is_some() {
        return Err(format!("a save brings too many descriptors: {count}"));
    }
    let then = if flags & shards_ipc::SAVE_LEAVE_RUNNING != 0 {
        shards_vmm::vm::AfterSnapshot::Resume
    } else {
        shards_vmm::vm::AfterSnapshot::Stop
    };
    // Nothing of the guest's on its way to the host as it is saved: what it held is in the
    // run's log, and what comes after waits in it, for the microVM saved or restored.
    let asked = shards_vmm::log::uptime_us();
    crate::workload::quiesce(tokens)?;
    let quiet = shards_vmm::log::uptime_us();
    let saved = crate::vm_run::save(shards_vmm::vm::Save {
        name,
        memory,
        state,
        swap,
        layer,
        window_heads,
        then,
    });
    // The guest goes on where the microVM does: saved and left running, or not saved.
    if saved.is_err() || then == shards_vmm::vm::AfterSnapshot::Resume {
        crate::workload::resume_guest();
    }
    shards_vmm::info!(
        "a save: its guest quiet in {} us, the microVM saved in {} us more",
        quiet.saturating_sub(asked),
        shards_vmm::log::uptime_us().saturating_sub(quiet)
    );
    let saved = saved?;
    // Before the microVM stops, as the guard goes: its end is the save's.
    if then == shards_vmm::vm::AfterSnapshot::Stop {
        let _ = SAVED.set((microvm, snapshot));
    }
    Ok(saved)
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
            client: stream(client)?,
            stdin: File::from(stdin),
            stdout: File::from(stdout),
            stderr: File::from(stderr),
        },
    )
}

/// The joiners this VM serves (D119), which it waits for before it ends: each tells the
/// daemon how it ended.
static JOINERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
/// Whether this VM is being saved (D176), from its `SAVE` on: no container joins it then.
static SAVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
    let link = stream(link)?;
    // What fails here, the run says as its own start failing, once it has the run.
    let range = match crate::vm_run::join_disk() {
        // A joiner starting in a guest going quiet for its save would be in its snapshot
        // without its container (D176).
        _ if SAVING.load(std::sync::atomic::Ordering::SeqCst) => {
            Err("the microVM it joins is being saved: join it once it is saved".into())
        }
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
    // Its memory is its provider's microVM's, whose most stays the provider's run's (D148),
    // as the hold on its CPUs does (D191).
    let Head {
        flags,
        retention,
        first,
        queue,
        spec,
        ..
    } = head(&request.payload)?;
    let mut spec = Spec::decode(spec).ok_or("a malformed command")?;
    let detached = flags & shards_ipc::RUN_DETACHED != 0;
    let logged = flags & shards_ipc::RUN_LOG != 0;
    let count = request.fds.len();
    let mut fds = request.fds.into_iter();
    let mut next = || {
        fds.next()
            .ok_or(format!("a request brings too few descriptors: {count}"))
    };
    let client = if detached { None } else { Some(stream(next()?)?) };
    let stdin = File::from(next()?);
    let (stdout, stderr) = if detached {
        (None, None)
    } else {
        (Some(File::from(next()?)), Some(File::from(next()?)))
    };
    let (segments, answers) = mpsc::channel();
    let log = if logged {
        let (log, index) = (File::from(next()?), File::from(next()?));
        let asker = link.try_clone().map_err(|e| format!("its connection: {e}"))?;
        Some(
            workload::Logger::new(log, index, first, retention, segments_from(asker, answers))
                .map_err(|e| format!("its log: {e}"))?
                .with_queue(queue),
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
    // Its swap (D159), which the microVM maps where none of its members' is yet: they
    // share one.
    let swap = if flags & shards_ipc::RUN_SWAP != 0 {
        Some(File::from(next()?))
    } else {
        None
    };
    if fds.next().is_some() {
        return Err(format!("a request brings too many descriptors: {count}"));
    }
    if let Err(e) = to_daemon(link, kind::TAKEN, &[], &[])
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
            if let Some(file) = swap
                && let Some(len) = crate::vm_run::attach_swap(file).map_err(|e| format!("its swap: {e}"))?
            {
                spec.setup.push(format!("swap={len}").into_bytes());
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
                    let _ = to_daemon(link, kind::STARTED, &guest.to_be_bytes(), &[]);
                },
                &|which, said| {
                    let to = if which == shards_abi::run::kind::MEMBER {
                        kind::MEMBER
                    } else {
                        kind::PORTS
                    };
                    let _ = to_daemon(link, to, said, &[]);
                },
            )
        });
        let (ended, conn) = match joined {
            Ok((ended, conn)) => (Ok(ended), Some(conn)),
            Err(e) => (Err(e), None),
        };
        // Its ports of the host's let go before its end is told (D124): a joiner's that
        // started, whose id is never the run's own, 0.
        let started = id.load(std::sync::atomic::Ordering::SeqCst);
        if started != 0 {
            release_host(started);
        }
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
                    let _ = to_daemon(
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
        shards_ipc::end(link);
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
    if let Ok(ended) = served {
        tell_lost(link, ended);
        // A joiner's process killed for want of memory, said as a run's is (D119, D159),
        // and what its cgroup was refused past its pids limit (D193).
        if let Some(facts) = &ended.oom {
            let _ = to_daemon(link, kind::OOM, facts, &[]);
        }
        if let Some(facts) = &ended.pids {
            let _ = to_daemon(link, kind::PIDS, facts, &[]);
        }
    }
    let mut done = vec![status];
    if let Some((said, _)) = &failed {
        done.extend_from_slice(said.as_bytes());
    }
    let _ = to_daemon(link, kind::DONE, &done, &[]);
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
        client: stream(client)?,
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

/// The engine connections handed to the daemon and not yet taken, by number: held
/// meanwhile, as XNU collects a socket in flight that no process holds (M24); on Windows
/// until the daemon gives the pipe each is relayed onto.
static ENGINE_HELD: std::sync::Mutex<std::collections::BTreeMap<u64, GuestConn>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// A guest's connection to a port this VM serves, as its vsock device makes it.
type GuestConn = shards_vmm::platform::UnixStream;

fn engine_held() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<u64, GuestConn>> {
    ENGINE_HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The run's engine token (D181), its spec's `engine=` entry, where it has one.
pub fn engine_token(spec: &Spec) -> Option<[u8; shards_abi::run::TOKEN]> {
    let hex = spec.setup.iter().find_map(|e| e.strip_prefix(b"engine="))?;
    let (pairs, []) = hex.as_chunks::<2>() else {
        return None;
    };
    let mut token = [0u8; shards_abi::run::TOKEN];
    if pairs.len() != token.len() {
        return None;
    }
    for (byte, pair) in token.iter_mut().zip(pairs) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(token)
}

/// Serves the run's own engine port (D181) on a thread of its own: each guest connection
/// whose hello says `token`, which only the guest's init was given, is the daemon's, on
/// this VM's link (`ENGINE_CONN`), held here until the daemon says it has it; any other is
/// closed. The daemon serves it with its Engine API, scoped to this microVM. On Windows,
/// where a VM gives its daemon no handle, the daemon gives a pipe for it instead, which
/// the connection is relayed onto ([`relay_engine`]).
pub fn serve_engine(
    link: &Link,
    token: [u8; shards_abi::run::TOKEN],
    port: workload::Port,
) -> Result<(), String> {
    let daemon = link
        .daemon
        .try_clone()
        .map_err(|e| format!("the daemon's connection: {e}"))?;
    let next = std::sync::atomic::AtomicU64::new(0);
    let heard = move |conn: GuestConn, said: &[u8; shards_abi::run::TOKEN]| {
        if !workload::same_token(&token, said) || conn.set_nonblocking(false).is_err() {
            return;
        }
        let number = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Held before it goes, as the daemon's answer may come before the send returns.
        #[cfg(unix)]
        let (Ok(held), handed) = (conn.try_clone(), [std::os::fd::AsFd::as_fd(&conn)]) else {
            return;
        };
        #[cfg(windows)]
        let (held, handed) = (conn, []);
        engine_held().insert(number, held);
        if to_daemon(&daemon, kind::ENGINE_CONN, &number.to_be_bytes(), &handed).is_err() {
            engine_held().remove(&number);
        }
    };
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || workload::accept_hellos(port, "engine-hellos", heard))
        .map(drop)
        .map_err(|e| format!("its engine's thread: {e}"))
}

/// Relays a guest's connection to its run's engine onto the pipe its daemon gave for it
/// (Windows, where a VM gives its daemon no handle): each way on a thread of its own, the
/// end of each side's writes said to the other (the pipe's message of no bytes, the
/// socket's half-close), as the daemon serves a connection passed whole elsewhere; a side
/// that can take no more ends the other's, and each is closed once both ways have ended.
#[cfg(windows)]
fn relay_engine(guest: GuestConn, pipe: OwnedFd) {
    let Ok(pipe) = UnixStream::given(pipe) else {
        return;
    };
    let (Ok(guest_down), Ok(pipe_down)) = (guest.try_clone(), pipe.try_clone()) else {
        return;
    };
    let down = std::thread::Builder::new()
        .name("engine-down".into())
        .spawn(move || {
            pass(
                &pipe_down,
                &guest_down,
                |guest| guest.shutdown(std::net::Shutdown::Write),
                shards_ipc::end,
            )
        });
    if down.is_err() {
        return;
    }
    let up = std::thread::Builder::new().name("engine-up".into()).spawn({
        let (guest, pipe) = (guest.try_clone(), pipe.try_clone());
        move || {
            if let (Ok(guest), Ok(pipe)) = (guest, pipe) {
                pass(
                    &guest,
                    &pipe,
                    |pipe| pipe.shutdown(std::net::Shutdown::Write),
                    |guest| {
                        let _ = guest.shutdown(std::net::Shutdown::Both);
                    },
                );
            }
        }
    });
    if up.is_err() {
        shards_ipc::end(&pipe);
        let _ = guest.shutdown(std::net::Shutdown::Both);
    }
}

/// Copies what `from` reads to `to` until `from` ends, then ends `to`'s writes
/// (`ended`); where `to` takes no more, ends `from` (`refused`), so that its writer stops.
#[cfg(windows)]
fn pass<F: ?Sized, T: ?Sized>(
    from: &F,
    to: &T,
    ended: impl FnOnce(&T) -> io::Result<()>,
    refused: impl FnOnce(&F),
) where
    for<'a> &'a F: io::Read,
    for<'a> &'a T: Write,
{
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match shards_ipc::read(from, &mut buf) {
            Ok(0) | Err(_) => {
                let _ = ended(to);
                return;
            }
            Ok(n) => {
                let mut to = to;
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    refused(from);
                    return;
                }
            }
        }
    }
}

/// Tells the daemon the command runs, before the client has anything the command wrote.
pub fn started(link: &Link) {
    let _ = to_daemon(&link.daemon, kind::STARTED, &[], &[]);
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
/// Where the VM holds a host's devices ([`HOST_DEVICES`]), the status goes with this
/// process's pidfd, which its client waits on before it is done: the devices are free
/// once this process has ended.
pub fn finish(
    link: &Link,
    client: Option<&UnixStream>,
    served: &Result<workload::Ended, String>,
    timing: Option<&str>,
    working_set: Option<&(String, Vec<u8>)>,
) {
    // A microVM its save stopped (D176): its command goes on once it is restored, and its
    // client hears it was saved, with the status a shell gives a job it stopped.
    let saved = SAVED.get();
    let (mut status, not_run) = match (saved, served) {
        (Some((microvm, snapshot)), _) => {
            if client.is_some() {
                let _ = writeln!(
                    io::stderr(),
                    "shards: {microvm} was saved as {snapshot}, and stopped"
                );
            }
            (shards_ipc::SAVED_STATUS, None)
        }
        (None, Ok(ended)) => (ended.status, ended.not_run.as_deref()),
        (None, Err(e)) => {
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
    if let Ok(null) = workload::null() {
        let_go(&null);
    }
    let mut done = vec![status];
    if let Some((said, _)) = &failed {
        done.extend_from_slice(said.as_bytes());
    }
    if let Ok(ended) = &served {
        tell_lost(&link.daemon, ended);
    }
    if let Some(facts) = served.as_ref().ok().and_then(|ended| ended.oom.as_deref()) {
        let _ = to_daemon(&link.daemon, kind::OOM, facts, &[]);
    }
    // A process or thread its pids limit refused it (D193), said as its OOM is.
    if let Some(facts) = served.as_ref().ok().and_then(|ended| ended.pids.as_deref()) {
        let _ = to_daemon(&link.daemon, kind::PIDS, facts, &[]);
    }
    if let Some((name, set)) = working_set {
        let _ = shards_ipc::working_set_parts(name, set, |part| {
            to_daemon(&link.daemon, kind::WORKING_SET, part, &[])
        });
    }
    release_ports();
    release_host(0);
    let _ = to_daemon(&link.daemon, kind::DONE, &done, &[]);
    if let Some(client) = client {
        let mut payload = vec![said_status];
        payload.extend_from_slice(timing.unwrap_or_default().as_bytes());
        let pidfd = HOST_DEVICES
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(own_pidfd)
            .flatten();
        let fds: Vec<shards_ipc::fd::BorrowedFd<'_>> = pidfd.iter().map(|p| p.as_fd()).collect();
        let _ = shards_ipc::send(client, kind::EXIT, &payload, &fds);
    }
}

/// Gives the daemon records of the processes the run's members started and ended (D123),
/// as they come.
pub fn processes(link: &Link, records: &[u8]) {
    let _ = to_daemon(&link.daemon, kind::PROCESS, records, &[]);
}

/// Gives the daemon its guest's ask of the host on the host's network (D124): the frame's
/// kind, then its payload. A port let go (`UNBIND`) goes after what the guest sent before
/// it, put in the network process's ring first, which it takes before the daemon's word to
/// close the port: a server's last datagram, sent as it closed its socket, reaches its
/// peer, as `release_ports` has a run's last reach it.
pub fn host_net(link: &Link, frame: &[u8]) {
    if frame.first() == Some(&shards_abi::run::kind::UNBIND)
        && let Some(flush) = FLUSH.get()
    {
        flush.flush(std::time::Instant::now() + std::time::Duration::from_secs(1));
    }
    // Whose processes have ports of the host's, to be let go as each ends.
    if let Some((&shards_abi::run::kind::BIND, bind)) = frame.split_first()
        && let Some(bind) = shards_abi::run::HostBind::decode(bind)
    {
        if HOST.daemon.get().is_none()
            && let Ok(daemon) = link.daemon.try_clone()
        {
            let _ = HOST.daemon.set(daemon);
        }
        let mut asked = HOST
            .asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !asked.contains(&bind.owner) {
            asked.push(bind.owner);
        }
    }
    let _ = to_daemon(&link.daemon, kind::HOST_NET, frame, &[]);
}

/// Has the daemon let go of container `owner`'s ports of the host's (D124) as it ends,
/// before its end is told, waiting up to a second for it to say it has: the daemon's
/// copies and its network process's closed, so that the host may bind them again once
/// the end is told, as dockerd's may (PM M181). Nothing to do for one that asked for none.
fn release_host(owner: u32) {
    {
        let mut asked = HOST
            .asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(at) = asked.iter().position(|&o| o == owner) else {
            return;
        };
        asked.swap_remove(at);
    }
    let Some(daemon) = HOST.daemon.get() else {
        return;
    };
    if to_daemon(daemon, kind::HOST_RELEASE, &owner.to_be_bytes(), &[]).is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut released = HOST
        .released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if let Some(at) = released.iter().position(|&o| o == owner) {
            released.swap_remove(at);
            return;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        released = HOST
            .said
            .wait_timeout(released, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// Tells the daemon the container's writable layer is whole where it asked for it.
pub fn layer_saved(link: &Link, used: Option<u64>) {
    let payload = used.map(u64::to_be_bytes);
    let _ = to_daemon(
        &link.daemon,
        kind::LAYER_SAVED,
        payload.as_ref().map_or(&[][..], |p| p),
        &[],
    );
}

/// The daemon's word that the container's writable layer stays in its own file (D190),
/// held until nothing of the machine writes the file ([`tell_kept`]): its link's own
/// socket, and what the layer used in the guest.
static KEPT: std::sync::Mutex<Option<(UnixStream, Option<u64>)>> = std::sync::Mutex::new(None);

/// Holds the word that the container's writable layer stays in its own file (D190), its
/// guest's journal written in place, for [`tell_kept`] once the machine is down. An error
/// where its link's socket cannot be held: the daemon then keeps the layer as the VM goes.
pub fn hold_kept(link: &Link, used: Option<u64>) -> std::io::Result<()> {
    let daemon = link.daemon.try_clone()?;
    *KEPT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((daemon, used));
    Ok(())
}

/// Tells the daemon what [`hold_kept`] held, if anything: nothing of the machine writes the
/// layer's file any more, which is the container's from now on (`Running::wait_then`).
pub fn tell_kept() {
    let held = KEPT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some((daemon, used)) = held {
        let payload = used.map(u64::to_be_bytes);
        let _ = to_daemon(
            &daemon,
            kind::LAYER_KEPT,
            payload.as_ref().map_or(&[][..], |p| p),
            &[],
        );
    }
}

/// Stops using the client's stdio: this process's standard descriptors become `null`.
/// Should that fail, they stay the client's until this process exits.
fn let_go(null: &File) {
    let nulls = [null.try_clone(), null.try_clone(), null.try_clone()];
    if let [Ok(a), Ok(b), Ok(c)] = nulls {
        let _ = set_stdio([a.into(), b.into(), c.into()]);
    }
}

/// Makes `stdio` this process's standard input, output and error, each its own from here
/// on: dup2(2) onto 0, 1 and 2 on Unix, which closes what they were; on Windows its
/// standard handles (SetStdHandle), those they were closed after, each once. A thread may
/// be reading the standard input it was, a client's that says nothing: on Windows that
/// read is cancelled (CancelIoEx) and the handle closed on a thread of its own, since a
/// close of a handle a synchronous read waits on waits for that read, as dup2 never does.
fn set_stdio(stdio: [OwnedFd; 3]) -> io::Result<()> {
    #[cfg(unix)]
    for (fd, target) in stdio.iter().zip([0, 1, 2]) {
        // SAFETY: dup2(2) onto this process's standard descriptors, which nothing else in
        // this process holds open as its own.
        if unsafe { libc::dup2(fd.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::IntoRawHandle as _;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        let which = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
        // SAFETY: no pointers: the process's standard handles as they are.
        let before = which.map(|w| unsafe { GetStdHandle(w) });
        let mut now = Vec::with_capacity(3);
        for (handle, w) in stdio.into_iter().zip(which) {
            let raw = handle.into_raw_handle();
            // SAFETY: a handle of this process's, its standard one from here on.
            if unsafe { SetStdHandle(w, raw) } == 0 {
                let e = io::Error::last_os_error();
                // SAFETY: the handle just let go of, which nothing took.
                unsafe { CloseHandle(raw) };
                return Err(e);
            }
            now.push(raw);
        }
        let mut closed = Vec::with_capacity(3);
        // Its output and error first, which its client waits to see end; its input last.
        for (i, old) in before.into_iter().enumerate().rev() {
            if old.is_null() || old == INVALID_HANDLE_VALUE || now.contains(&old) || closed.contains(&old) {
                continue;
            }
            closed.push(old);
            if i == 0 {
                close_input(old);
                continue;
            }
            // SAFETY: a standard handle this process had, none of its own from here on.
            unsafe { CloseHandle(old) };
        }
    }
    Ok(())
}

/// Closes `old`, the standard input this process had, waiting for no read of it: one under
/// way is cancelled, and the handle closed on a thread of its own, which a read cancel does
/// not end waits for.
#[cfg(windows)]
fn close_input(old: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::IO::CancelIoEx;
    // SAFETY: a handle this process holds; no OVERLAPPED: every request on it.
    unsafe { CancelIoEx(old, std::ptr::null()) };
    let raw = old as usize;
    let closing = std::thread::Builder::new()
        .name("stdin-close".into())
        .spawn(move || {
            // SAFETY: the standard handle let go of above, closed here alone.
            unsafe { CloseHandle(raw as windows_sys::Win32::Foundation::HANDLE) };
        });
    // Where no thread starts, it stays open until this process exits.
    drop(closing);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The null device's path.
    const NULL: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

    /// A request that brings fewer descriptors than it needs, or more, is refused, in words
    /// that say which, before the VM takes it.
    #[test]
    fn a_request_with_the_wrong_descriptors_is_refused() {
        static TO_GUEST: ToGuest = ToGuest::new(workload::Signals::new());
        workload::hold_null(File::open(NULL).unwrap());
        // A file, as a log and its index are: CI's Windows Server reads no metadata of NUL
        // (ERROR_INVALID_FUNCTION), which a log's is read first.
        let dir = shards_testdir::TempDir::new("warm-fds").unwrap();
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("f"))
            .unwrap();
        // A detached run's: its stdin, its log and its index.
        for (given, said) in [
            (1, "a request brings too few descriptors: 1"),
            (4, "a request brings too many descriptors: 4"),
        ] {
            let (ours, daemon) = UnixStream::pair().unwrap();
            let link = Link::new(ours);
            let fds = vec![file.as_fd(); given];
            let refused = std::thread::scope(|s| {
                let receiving = s.spawn(|| receive(&link, &TO_GUEST, &|| Ok(()), false));
                assert_eq!(shards_ipc::recv(&daemon).unwrap().unwrap().kind, kind::READY);
                let mut payload = Vec::new();
                // Its log's first segment, and memory to plug: none (D148).
                shards_ipc::RunHead {
                    flags: shards_ipc::RUN_DETACHED | shards_ipc::RUN_LOG,
                    log_size: 1,
                    log_files: 1,
                    ..Default::default()
                }
                .encode_into(&mut payload);
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

    /// What the VM's threads send the daemon arrives whole, one message after another,
    /// however they send at once on clones of its connection: a working set's message cut
    /// through by another's broke the stream's framing (PM M319). Many large messages and
    /// many small ones, each sent from a thread of its own, are each read back as sent; a
    /// stream whose framing broke, which would leave its reader waiting for bytes no message
    /// brings, is said so within 30 s.
    #[test]
    fn what_the_vms_threads_send_the_daemon_arrives_whole() {
        const LARGE: usize = 400 << 10;
        const LARGES: usize = 40;
        const SMALLS: usize = 4000;
        let (ours, daemon) = UnixStream::pair().unwrap();
        daemon
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let other = ours.try_clone().unwrap();
        let read = std::thread::scope(|s| {
            // Each sender stops at its first refused send, its reader's end gone.
            s.spawn(|| {
                for i in 0..LARGES {
                    if to_daemon(&ours, kind::WORKING_SET, &vec![i as u8; LARGE], &[]).is_err() {
                        return;
                    }
                }
            });
            s.spawn(|| {
                for i in 0..SMALLS {
                    if to_daemon(&other, kind::PROCESS, &(i as u64).to_be_bytes(), &[]).is_err() {
                        return;
                    }
                }
            });
            let daemon = daemon;
            let read = (|| -> Result<(), String> {
                let (mut larges, mut smalls) = (0, 0);
                while larges < LARGES || smalls < SMALLS {
                    let m = shards_ipc::recv(&daemon)
                        .map_err(|e| format!("after {larges} large and {smalls} small messages: {e}"))?
                        .ok_or("the stream ended early")?;
                    match m.kind {
                        kind::WORKING_SET
                            if m.payload.len() == LARGE && m.payload.iter().all(|&b| b == larges as u8) =>
                        {
                            larges += 1;
                        }
                        kind::PROCESS if m.payload == (smalls as u64).to_be_bytes() => smalls += 1,
                        kind => {
                            return Err(format!(
                                "after {larges} large and {smalls} small messages, one of kind {kind} and {} bytes: one cut through another",
                                m.payload.len()
                            ));
                        }
                    }
                }
                Ok(())
            })();
            // Its end gone, a sender waiting on a stream full of what will not be read is let go.
            drop(daemon);
            read
        });
        assert_eq!(read, Ok(()));
    }
}
