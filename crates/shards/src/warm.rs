//! A warm VM: restored from a template, resumed and connected, waiting for the daemon to
//! hand it one request (docs/design/architecture.md D26). `shards vm restore DIR --warm FD`
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
fn release_ports() {
    if !PUBLISHED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(release) = RELEASE.get() else { return };
    if shards_ipc::send(release, kind::UNPUBLISH, &[], &[]).is_err() {
        return;
    }
    let _ = release.set_read_timeout(Some(std::time::Duration::from_secs(1)));
    let _ = shards_ipc::recv(release);
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
    /// The client asked for the timing line.
    pub timing: bool,
    /// The container's log, if its output is kept.
    pub log: Option<workload::Logger>,
}

/// Tells the daemon this VM is ready, then waits for its request, and tells the daemon it
/// has taken it. Until then the daemon holds its own copies of the client's descriptors,
/// and gives the request to another VM if this one fails. Then makes the client's stdio
/// this process's, and starts passing the client's signals to the workload through `to`.
pub fn receive(link: &Link, to: &'static ToGuest) -> Result<Request, String> {
    let daemon = &link.daemon;
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
    let (files, spec) = rest
        .split_first_chunk::<8>()
        .ok_or("a request without its log's retention")?;
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
            .ok_or(format!("a request brings more than {count} descriptors"))
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
            workload::Logger::new(log, index, retention, next)
                .map_err(|e| format!("the container's log: {e}"))?,
        )
    } else {
        None
    };
    if fds.next().is_some() {
        return Err(format!("a request brings {count} descriptors, too many"));
    }
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
    let mut relays = vec![("daemon-signals", from_daemon, From::Daemon(segments))];
    if let Some(client) = &client {
        let signals = client
            .try_clone()
            .map_err(|e| format!("the client's connection: {e}"))?;
        relays.push(("client-signals", signals, From::Client));
    }
    for (name, conn, from) in relays {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || relay_signals(&conn, to, from))
            .map_err(|e| format!("{name} thread: {e}"))?;
    }
    Ok(Request {
        client,
        spec,
        interactive: flags & shards_ipc::RUN_INTERACTIVE != 0,
        timing: flags & shards_ipc::RUN_TIMING != 0,
        log,
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

/// Whose connection a relay reads.
enum From {
    /// The daemon's, which also brings its answers for the log, passed on here.
    Daemon(mpsc::Sender<Segment>),
    Client,
}

/// Passes the signals that arrive on `conn` to the workload until it closes: the client's,
/// or the daemon's (`shards stop`, `kill`), and the client's terminal sizes. When the
/// client hangs up, the VM lets go of its stdio. A workload outlives its client, as a
/// container outlives `docker run`'s.
fn relay_signals(conn: &UnixStream, to: &'static ToGuest, from: From) {
    let client = matches!(from, From::Client);
    while let Ok(Some(message)) = shards_ipc::recv(conn) {
        match message.kind {
            kind::SEGMENT => {
                if let (From::Daemon(segments), Ok(seq)) =
                    (&from, <[u8; 8]>::try_from(message.payload.as_slice()))
                {
                    let _ = segments.send((u64::from_be_bytes(seq), message.fds));
                }
            }
            kind::SIGNAL => {
                if let Ok(signal) = <[u8; 4]>::try_from(message.payload.as_slice()) {
                    workload::signal_guest(to, u32::from_be_bytes(signal));
                }
            }
            kind::EXEC_RUN if !client => {
                // The client's connection is this process's now: the daemon may let its
                // copy go.
                if let Some(number) = message.payload.first_chunk::<8>() {
                    let _ = shards_ipc::send(conn, kind::EXEC_TAKEN, number, &[]);
                }
                if let Err(e) = exec_request(message, to) {
                    let _ = writeln!(io::stderr(), "shards: an exec: {e}");
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
/// with its client's connection, stdin, stdout and stderr.
fn exec_request(message: shards_ipc::Message, to: &'static ToGuest) -> Result<(), String> {
    let (_, rest) = message
        .payload
        .split_first_chunk::<8>()
        .ok_or("an exec without its number")?;
    let (flags, spec) = rest.split_first().ok_or("an empty exec")?;
    let spec = Spec::decode(spec).ok_or("a malformed exec")?;
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
            client: UnixStream::from(client),
            stdin: File::from(stdin),
            stdout: File::from(stdout),
            stderr: File::from(stderr),
        },
    )
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
    if let Ok(ended) = served
        && ended.lost > 0
    {
        let _ = shards_ipc::send(&link.daemon, kind::LOST, &ended.lost.to_be_bytes(), &[]);
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

/// Stops using the client's stdio: this process's standard descriptors become `null`.
fn let_go(null: &File) {
    for target in [0, 1, 2] {
        // SAFETY: dup2(2) onto this process's own standard descriptors. Should it fail,
        // the descriptor stays the client's until this process exits.
        unsafe { libc::dup2(null.as_raw_fd(), target) };
    }
}
