//! One instance of the in-VM server: the agent or harness it serves reaches it by a
//! socket only that domain sees, `/run/shards/server.sock`. Two layers say who calls:
//!
//! - that socket, which init made visible to its domain alone: an identity the kernel
//!   makes, which no agent can forge;
//! - mutual TLS 1.3 over it, with a CA this instance makes as it starts: no key is shared
//!   with another agent's instance, and init holds none. The agent's certificate and key
//!   lie beside the socket, the key readable by the agent's group alone, and a connection
//!   must present that certificate.
//!
//! Started by init as `shards-server LABEL DIR GID READY FILTER PEERS`, already in namespaces
//! and a uid of its own with no capability (init's `domains`): it makes its files in DIR,
//! listens, says so on descriptor READY, then confines itself with the seccomp filter it
//! reads from descriptor FILTER, before it reads anything an agent sends.
//!
//! It speaks MCP over streamable HTTP (modelcontextprotocol.io, "Transports"): JSON
//! answers to JSON-RPC requests posted to `/mcp`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{broker, sandbox};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

#[path = "../../init/src/json.rs"]
#[allow(dead_code)]
pub(super) mod json;
use json::Value;

/// The name its certificate is for, which a client asks for.
pub const NAME: &str = "shards";
/// The MCP revisions it answers to, newest first (modelcontextprotocol.io, "Versioning").
/// The version it says it is, shards' (crates/shards/build.rs), or its own where it is
/// built alone.
const VERSION: &str = match option_env!("SHARDS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
/// The current revision of MCP, whose requests each say it in their `_meta` and stand
/// alone (modelcontextprotocol.io, 2026-07-28, "Versioning and Compatibility").
const MODERN: &str = "2026-07-28";
/// The revisions before it, newest first, which a session's `initialize` chooses among.
const REVISIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// Where a request of the current revision says it, and the capabilities it asks with.
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
/// The most of a request's head it holds: Node.js's `http.maxHeaderSize` (16 KiB, its
/// `--max-http-header-size` default; nodejs.org/api/cli.html), the head every MCP client
/// already fits, as MCP's own TypeScript SDK serves Streamable HTTP on Node's `http`. A
/// body is held as it comes, as long as its Content-Length says ([`request`]): the instance
/// counts against its caller's own memory (init's `domains`, D60), whose `memory.max` is the
/// bound of what a caller can make it hold.
const HEAD_MAX: usize = 16 * 1024;

/// An instance, made: its caller, the socket it listens on, and its TLS.
pub struct Prepared {
    /// `agent main`: what it says its caller is.
    label: String,
    listener: UnixListener,
    /// The certificate issued to its caller, which a connection must present.
    cert: CertificateDer<'static>,
    config: Arc<rustls::ServerConfig>,
    /// The MCP servers offered its caller (§12 answer 5): a JSON array.
    offered: String,
    /// Its caller's scratch, `/tmp` as the caller sees it, where the files it hands across
    /// lie (D139); none where its image has no `/tmp`.
    scratch: Option<OwnedFd>,
}

pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[OsString]) -> Result<(), String> {
    let arg = |i: usize, what: &str| {
        args.get(i).and_then(|a| a.to_str()).ok_or(format!(
            "usage: shards-server LABEL DIR GID READY FILTER (no {what})"
        ))
    };
    let label = arg(0, "LABEL")?;
    let dir = arg(1, "DIR")?;
    let gid: u32 = arg(2, "GID")?.parse().map_err(|_| "GID is no number")?;
    let fd = |i: usize, what: &str| -> Result<OwnedFd, String> {
        let n: RawFd = arg(i, what)?
            .parse()
            .map_err(|_| format!("{what} is no descriptor"))?;
        // SAFETY: init gives this process the descriptor named, for it alone.
        Ok(unsafe { OwnedFd::from_raw_fd(n) })
    };
    let (ready, filter) = (fd(3, "READY")?, fd(4, "FILTER")?);
    let mut peers = channels(args.get(5).and_then(|a| a.to_str()).unwrap_or_default())?;
    let mut p = prepare(dir, label, gid)?;
    // The MCP servers offered its caller, as init found them: a JSON array.
    p.offered = args
        .get(6)
        .and_then(|a| a.to_str())
        .filter(|a| json::parse(a.as_bytes()).is_ok())
        .unwrap_or("[]")
        .to_string();
    // Its caller's scratch, as init gave it (D139).
    p.scratch = match arg(7, "SCRATCH").ok().filter(|a| !a.is_empty()) {
        Some(_) => Some(fd(7, "SCRATCH")?),
        None => None,
    };
    let program = read_filter(filter)?;
    // Code mode's helper, while this process may still make one (D136): without it, code
    // mode says why, and the rest is served.
    let programs = Programs::new(spawn_helper());
    let mut ready = std::fs::File::from(ready);
    ready
        .write_all(b"r")
        .map_err(|e| format!("saying it listens: {e}"))?;
    drop(ready);
    confine(&program)?;
    serve(&p, &mut peers, programs);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct NativeIo {
    kind: io::ErrorKind,
    code: Option<i32>,
}

impl From<io::Error> for NativeIo {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
}

impl std::fmt::Display for NativeIo {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{:?} (OS {:?})", self.kind, self.code)
    }
}

#[derive(Clone, Copy, Debug)]
enum HelperOperation {
    Executable,
    Channel,
    Fork,
    ReadFlags,
    SetNonblocking,
    Receive,
}

#[derive(Clone, Copy, Debug)]
enum HelperFailure {
    Io {
        operation: HelperOperation,
        cause: NativeIo,
    },
    Ended,
}

impl HelperFailure {
    fn io(operation: HelperOperation, error: io::Error) -> Self {
        Self::Io {
            operation,
            cause: error.into(),
        }
    }
}

impl std::fmt::Display for HelperFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { operation, cause } => write!(out, "helper {operation:?}: {cause}"),
            Self::Ended => out.write_str("the helper ended"),
        }
    }
}

/// Starts code mode's helper (D136): this binary again, `shards-server sandbox`, its control
/// socket the one end of a pair at [`sandbox::CONTROL_FD`], and no other descriptor, no
/// variable of the environment and none of this process's memory; the other end, this
/// process's.
fn spawn_helper() -> Result<OwnedFd, HelperFailure> {
    let exe = std::fs::File::open("/proc/self/exe")
        .map_err(|error| HelperFailure::io(HelperOperation::Executable, error))?;
    let mut fds = [0 as RawFd; 2];
    // SAFETY: socketpair(2) into a two-element array.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(HelperFailure::io(
            HelperOperation::Channel,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: both were just made, and are owned here alone.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let argv: [*const libc::c_char; 3] = [c"shards-server".as_ptr(), c"sandbox".as_ptr(), std::ptr::null()];
    let envp: [*const libc::c_char; 1] = [std::ptr::null()];
    // SAFETY: fork(2) of this process, which runs no other thread yet; the child calls only
    // async-signal-safe functions before it execs or exits.
    match unsafe { libc::fork() } {
        -1 => Err(HelperFailure::io(
            HelperOperation::Fork,
            io::Error::last_os_error(),
        )),
        0 => {
            // SAFETY: the child moves its two descriptors past every other's, then to 3
            // and 4 (dup2 clears CLOEXEC), closes the rest, and execs its binary.
            unsafe {
                let control = libc::fcntl(theirs.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
                let binary = libc::fcntl(exe.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
                if control >= 0
                    && binary >= 0
                    && libc::dup2(control, sandbox::CONTROL_FD) == sandbox::CONTROL_FD
                    && libc::dup2(binary, sandbox::CONTROL_FD + 1) == sandbox::CONTROL_FD + 1
                {
                    libc::syscall(libc::SYS_close_range, sandbox::CONTROL_FD + 2, u32::MAX, 0);
                    libc::syscall(
                        libc::SYS_execveat,
                        sandbox::CONTROL_FD + 1,
                        c"".as_ptr(),
                        argv.as_ptr(),
                        envp.as_ptr(),
                        libc::AT_EMPTY_PATH,
                    );
                }
                libc::_exit(127)
            }
        }
        _ => {
            drop(theirs);
            Ok(ours)
        }
    }
}

/// Code mode's programs (D136): the helper's control socket, or why there is none; and each
/// program running, awaited by the connection that asked for it.
pub struct Programs {
    control: Result<OwnedFd, HelperFailure>,
    next: u64,
    running: Vec<Program>,
    /// Programs let go of, by their connection and request ID, whose connections are yet to
    /// be told.
    cancelled: Vec<(u64, String)>,
    /// Programs let go of whose STOP the helper's socket, full, has yet to take: each is sent
    /// as it has room ([`Programs::send_stops`]), lest its worker run on.
    unstopped: Vec<u64>,
}

/// A program running: its socket, what came of it and what goes to it, the connection that
/// awaits its answer, the session its request was made in and its ID there, and when it is
/// to end if it has not.
struct Program {
    id: u64,
    socket: UnixStream,
    read: Vec<u8>,
    write: Vec<u8>,
    conn: u64,
    session: Option<String>,
    rpc_id: String,
    /// Whether its request was of the current revision, which shapes its answer.
    modern: bool,
    deadline: Option<(Instant, u64)>,
}

#[derive(Debug)]
enum ProgramAdmission {
    Unavailable(HelperFailure),
    IdentityExhausted,
    Deadline,
    Expired,
    Capacity,
    Frame(sandbox::FrameFailure),
    Socket(NativeIo),
    Release(NativeIo),
}

impl std::fmt::Display for ProgramAdmission {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(cause) => write!(out, "code mode is not available here: {cause}"),
            Self::IdentityExhausted => out.write_str("code mode's execution identities are exhausted"),
            Self::Deadline => out.write_str("the program's deadline is not representable"),
            Self::Expired => out.write_str("the program's deadline expired before worker release"),
            Self::Capacity => out.write_str("the program's owned tracking storage is unavailable"),
            Self::Frame(why) => why.fmt(out),
            Self::Socket(why) => write!(out, "a program's socket: {why}"),
            Self::Release(why) => write!(out, "code mode's helper: {why}"),
        }
    }
}

struct PreparedProgram {
    program: Program,
    peer: UnixStream,
}

impl Programs {
    fn new(control: Result<OwnedFd, HelperFailure>) -> Programs {
        let control = control.and_then(|control| {
            // SAFETY: the descriptor remains owned throughout native flag setup.
            let flags = unsafe { libc::fcntl(control.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(HelperFailure::io(
                    HelperOperation::ReadFlags,
                    io::Error::last_os_error(),
                ));
            }
            // SAFETY: preserve native status flags while enabling readiness-based control.
            if unsafe { libc::fcntl(control.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(HelperFailure::io(
                    HelperOperation::SetNonblocking,
                    io::Error::last_os_error(),
                ));
            }
            Ok(control)
        });
        Programs {
            control,
            next: 0,
            running: Vec::new(),
            cancelled: Vec::new(),
            unstopped: Vec::new(),
        }
    }

    /// Preparation owns every retained allocation before RUN transfers the program socket.
    fn prepare(
        &mut self,
        code: &str,
        timeout: Option<u64>,
        conn: u64,
        session: Option<&str>,
        rpc_id: String,
        modern: bool,
    ) -> Result<PreparedProgram, ProgramAdmission> {
        let began = Instant::now();
        let deadline = timeout
            .map(|ms| {
                began
                    .checked_add(Duration::from_millis(ms))
                    .map(|at| (at, ms))
                    .ok_or(ProgramAdmission::Deadline)
            })
            .transpose()?;
        let id = self
            .next
            .checked_add(1)
            .ok_or(ProgramAdmission::IdentityExhausted)?;
        self.running
            .try_reserve(1)
            .map_err(|_| ProgramAdmission::Capacity)?;
        // Each live request may need one owned cancellation result, and one STOP the helper's
        // socket cannot yet take, before another admission.
        let cancellations = self
            .running
            .len()
            .checked_add(1)
            .ok_or(ProgramAdmission::Capacity)?;
        self.cancelled
            .try_reserve(cancellations)
            .map_err(|_| ProgramAdmission::Capacity)?;
        self.unstopped
            .try_reserve(cancellations)
            .map_err(|_| ProgramAdmission::Capacity)?;
        let session = session
            .map(|source| {
                let mut owned = String::new();
                owned
                    .try_reserve_exact(source.len())
                    .map_err(|_| ProgramAdmission::Capacity)?;
                owned.push_str(source);
                Ok(owned)
            })
            .transpose()?;
        let write =
            sandbox::prepare_frame(sandbox::PROGRAM, code.as_bytes()).map_err(ProgramAdmission::Frame)?;
        let (socket, peer) = UnixStream::pair().map_err(|error| ProgramAdmission::Socket(error.into()))?;
        socket
            .set_nonblocking(true)
            .map_err(|error| ProgramAdmission::Socket(error.into()))?;
        Ok(PreparedProgram {
            program: Program {
                id,
                socket,
                read: Vec::new(),
                write,
                conn,
                session,
                rpc_id,
                modern,
                deadline,
            },
            peer,
        })
    }

    /// A successful RUN is local helper admission, not proof of worker completion.
    fn start(
        &mut self,
        code: &str,
        timeout: Option<u64>,
        conn: u64,
        session: Option<&str>,
        rpc_id: String,
        modern: bool,
    ) -> Result<u64, ProgramAdmission> {
        let control = self
            .control
            .as_ref()
            .map_err(|cause| ProgramAdmission::Unavailable(*cause))?
            .as_raw_fd();
        let prepared = self.prepare(code, timeout, conn, session, rpc_id, modern)?;
        if prepared
            .program
            .deadline
            .is_some_and(|(at, _)| Instant::now() >= at)
        {
            return Err(ProgramAdmission::Expired);
        }
        let id = prepared.program.id;
        // Issued identities are never reused, including after a refused native transfer.
        self.next = id;
        let mut run = [0; 1 + size_of::<u64>()];
        for (slot, byte) in run
            .iter_mut()
            .zip(std::iter::once(sandbox::RUN).chain(id.to_be_bytes()))
        {
            *slot = byte;
        }
        sandbox::send_control(control, &run, Some(prepared.peer.as_raw_fd()))
            .map_err(|error| ProgramAdmission::Release(error.into()))?;
        // prepare reserved this slot, with no intervening mutation of the owned vector.
        self.running.push(prepared.program);
        Ok(id)
    }

    /// Ends the worker of program `id`, just taken out of those running: its STOP sent now,
    /// or, while the helper's socket is full, once it has room. With no helper there is no
    /// worker to end: each ended with it (PR_SET_PDEATHSIG), or as it let go of its socket.
    fn stop(&mut self, id: u64) {
        if self.control.is_err() {
            return;
        }
        // Admission reserved this place: a program is let go of once (prepare).
        self.unstopped.push(id);
        self.send_stops();
    }

    /// Sends the helper the STOPs it is owed, as many as its socket takes now.
    fn send_stops(&mut self) {
        let Ok(c) = &self.control else {
            self.unstopped.clear();
            return;
        };
        while let Some(&id) = self.unstopped.last() {
            let mut stop = [0; 1 + size_of::<u64>()];
            for (slot, byte) in stop
                .iter_mut()
                .zip(std::iter::once(sandbox::STOP).chain(id.to_be_bytes()))
            {
                *slot = byte;
            }
            match sandbox::send_control(c.as_raw_fd(), &stop, None) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // Sent; or refused for a cause room would not cure, let go of as before.
                _ => {
                    self.unstopped.pop();
                }
            }
        }
    }
}

/// A channel to another domain's instance, which init made for what the Agentfile grants
/// (D60): its label, whether this caller may send it requests and answers its, and what is
/// in flight. Each message is one sequenced packet: a kind (1 a request, 2 an answer), an
/// ID of the asker's, 8 bytes big-endian, and its text.
pub struct Peer {
    label: String,
    fd: OwnedFd,
    send: bool,
    answer: bool,
    /// The most one message to it holds: its socket's send buffer, less what the kernel
    /// keeps of it (Linux 7.2 net/unix/af_unix.c `unix_dgram_sendmsg`, l. 2122, which a
    /// sequenced packet is sent by: `len > sk_sndbuf - 32` is EMSGSIZE).
    most: usize,
    /// What was read from it and not yet taken: one at a time, so that what it sends past
    /// that waits in the kernel's buffer, whose bound is the kernel's.
    held: Option<Message>,
    /// Requests sent it, awaiting its answers: an answer to anything else is dropped.
    sent: Vec<u64>,
    /// The last ID given a request sent it.
    last: u64,
    /// Its requests taken and not yet answered: an answer to anything else is refused.
    asked: Vec<u64>,
    gone: bool,
}

/// A message read from a peer.
struct Message {
    request: bool,
    id: u64,
    body: Body,
}

/// What a message carries: a text, or a file handed across, its name and its bytes.
enum Body {
    Text(String),
    File(String, Vec<u8>),
}

pub(super) const REQUEST: u8 = 1;
pub(super) const ANSWER: u8 = 2;
/// A request, and an answer, that carry a file: its name's length (2 bytes, big-endian),
/// its name, and its bytes.
pub(super) const REQUEST_FILE: u8 = 3;
pub(super) const ANSWER_FILE: u8 = 4;

/// The channels init gave it: one a line, `FD\tLABEL\tSEND\tANSWER`.
fn channels(spec: &str) -> Result<Vec<Peer>, String> {
    let mut out = Vec::new();
    for line in spec.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let [fd, label, send, answer] = f.as_slice() else {
            return Err(format!("a channel it cannot read: {line:?}"));
        };
        let fd: RawFd = fd
            .parse()
            .map_err(|_| format!("a channel's descriptor: {fd:?}"))?;
        // SAFETY: init gives this process the descriptor named, for it alone.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let most = message_most(&fd).map_err(|e| format!("{label}'s channel: {e}"))?;
        out.push(Peer {
            label: label.to_string(),
            fd,
            send: *send == "true",
            answer: *answer == "true",
            most,
            held: None,
            sent: Vec::new(),
            last: 0,
            asked: Vec::new(),
            gone: false,
        });
    }
    Ok(out)
}

/// What kernel keeps of a sequenced packet's send buffer: a message on `fd` holds its
/// SO_SNDBUF less this (net/unix/af_unix.c `unix_dgram_sendmsg`, l. 2122).
const SNDBUF_KEPT: usize = 32;

/// The most one message sent on `fd` holds: its SO_SNDBUF (socket(7)), less [`SNDBUF_KEPT`].
fn message_most(fd: &OwnedFd) -> io::Result<usize> {
    let mut size: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) of an int into one of ours, its length given.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&raw mut size).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(size).unwrap_or(0).saturating_sub(SNDBUF_KEPT))
}

/// Reads what `peer` sent, if nothing of its is held: a request only where this caller
/// answers its, an answer only to a request sent it; anything else is dropped, whatever
/// the other instance is made to send. Its message is read into as many bytes as it holds,
/// which the kernel says first (recv(2), MSG_TRUNC, of a sequenced packet since Linux 3.4)
/// and bounds by its sender's buffer.
fn take(peer: &mut Peer) {
    if peer.held.is_some() || peer.gone {
        return;
    }
    // SAFETY: recv(2) of no bytes, peeking: the next message's length, which stays.
    let next = unsafe {
        libc::recv(
            peer.fd.as_raw_fd(),
            std::ptr::null_mut(),
            0,
            libc::MSG_DONTWAIT | libc::MSG_PEEK | libc::MSG_TRUNC,
        )
    };
    let Ok(next) = usize::try_from(next) else {
        return;
    };
    let mut buf = Vec::new();
    if buf.try_reserve_exact(next).is_err() {
        return;
    }
    buf.resize(next, 0);
    // SAFETY: recv(2) into a buffer it owns.
    let n = unsafe {
        libc::recv(
            peer.fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n == 0 {
        peer.gone = true;
        return;
    }
    let Some(frame) = usize::try_from(n).ok().and_then(|n| buf.get(..n)) else {
        return;
    };
    let Some((&kind, rest)) = frame.split_first() else {
        return;
    };
    let Some((id, text)) = rest.split_first_chunk::<8>() else {
        return;
    };
    let id = u64::from_be_bytes(*id);
    let (request, file) = match kind {
        REQUEST | REQUEST_FILE if peer.answer => (true, kind == REQUEST_FILE),
        ANSWER | ANSWER_FILE if peer.sent.contains(&id) => (false, kind == ANSWER_FILE),
        _ => return,
    };
    let body = if file {
        let Some((len, rest)) = text.split_first_chunk::<2>() else {
            return;
        };
        let len = usize::from(u16::from_be_bytes(*len));
        let (Some(name), Some(bytes)) = (rest.get(..len), rest.get(len..)) else {
            return;
        };
        Body::File(String::from_utf8_lossy(name).into_owned(), bytes.to_vec())
    } else {
        Body::Text(String::from_utf8_lossy(text).into_owned())
    };
    if !request {
        peer.sent.retain(|s| *s != id);
    }
    peer.held = Some(Message { request, id, body });
}

/// A message's head: its kind, and its ID, 8 bytes.
pub(super) const MESSAGE_HEAD: usize = 1 + 8;

/// What came of sending a peer a message.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum Put {
    Sent,
    /// Its buffer is full: what was sent it before is not yet taken.
    Full,
    /// Longer than a message to it holds: at most this much of the message's text.
    Long(usize),
}

/// Sends `peer` a message of `kind`.
#[cfg(test)]
fn put(peer: &Peer, kind: u8, id: u64, text: &str) -> Put {
    put_bytes(peer, kind, id, text.as_bytes())
}

/// What a file's message holds beside its bytes: its name's length (2 bytes) and its name.
#[cfg(test)]
fn file_head(name: &str) -> usize {
    2usize.saturating_add(name.len())
}

/// Sends `peer` a file handed across, named `name`, as `kind`.
#[cfg(test)]
fn put_file(peer: &Peer, kind: u8, id: u64, name: &str, bytes: &[u8]) -> Put {
    let Ok(len) = u16::try_from(name.len()) else {
        return Put::Long(peer.most.saturating_sub(MESSAGE_HEAD + file_head(name)));
    };
    put_bytes(
        peer,
        kind,
        id,
        &[&len.to_be_bytes()[..], name.as_bytes(), bytes].concat(),
    )
}

#[cfg(test)]
fn put_bytes(peer: &Peer, kind: u8, id: u64, payload: &[u8]) -> Put {
    if payload.len() > peer.most.saturating_sub(MESSAGE_HEAD) {
        return Put::Long(peer.most.saturating_sub(MESSAGE_HEAD));
    }
    let frame = [&[kind][..], &id.to_be_bytes(), payload].concat();
    // SAFETY: send(2) of a buffer it owns, on a socket of its own.
    let n = unsafe {
        libc::send(
            peer.fd.as_raw_fd(),
            frame.as_ptr().cast(),
            frame.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if usize::try_from(n).is_ok_and(|n| n == frame.len()) {
        Put::Sent
    } else {
        Put::Full
    }
}

/// The seccomp filter init passed: its flags, 4 bytes little-endian, then its
/// `struct sock_filter`s (include/uapi/linux/filter.h), as init's setup carries it.
fn read_filter(from: OwnedFd) -> Result<(u32, Vec<libc::sock_filter>), String> {
    let mut bytes = Vec::new();
    std::fs::File::from(from)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("its seccomp filter: {e}"))?;
    let (flags, bytes) = bytes
        .split_first_chunk::<4>()
        .ok_or("its seccomp filter: no flags")?;
    let (program, rest) = bytes.as_chunks::<FILTER_LEN>();
    if program.is_empty() || !rest.is_empty() {
        return Err("its seccomp filter: no whole program".to_string());
    }
    Ok((
        u32::from_le_bytes(*flags),
        program
            .iter()
            .map(|&[c0, c1, jt, jf, k0, k1, k2, k3]| libc::sock_filter {
                code: u16::from_ne_bytes([c0, c1]),
                jt,
                jf,
                k: u32::from_ne_bytes([k0, k1, k2, k3]),
            })
            .collect(),
    ))
}

/// A `struct sock_filter`'s size: a u16, two u8s and a u32.
const FILTER_LEN: usize = 8;
const _: () = assert!(std::mem::size_of::<libc::sock_filter>() == FILTER_LEN);

/// Loads `program` with `flags`; `no_new_privs` init set before the exec.
fn confine((flags, program): &(u32, Vec<libc::sock_filter>)) -> Result<(), String> {
    let prog = libc::sock_fprog {
        len: u16::try_from(program.len()).map_err(|_| "its seccomp filter is too long")?,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: seccomp(2) with a sock_fprog over a program that outlives the call.
    if unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            *flags,
            &raw const prog,
        )
    } != 0
    {
        return Err(format!("its seccomp filter: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn err(what: &str) -> impl Fn(rcgen::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Makes its CA, its own certificate, and its caller's certificate and key in `dir`
/// (`ca.pem`, `cert.pem`, `key.pem`, readable by group `gid`, its caller's, alone for the
/// key), and listens on `dir/server.sock`, which that group may connect to.
pub fn prepare(dir: &str, label: &str, gid: u32) -> Result<Prepared, String> {
    // TLS13_AES_256_GCM_SHA384 alone, as Ada directed for every link an agent's control
    // crosses (2026-11-11; D60). TLS 1.3 negotiates one suite for both directions, each keyed
    // apart (RFC 8446 §4.1.3, §7.3): a client offering no AES-256-GCM is refused at its
    // handshake.
    let provider = Arc::new(rustls::crypto::CryptoProvider {
        cipher_suites: vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],
        ..rustls::crypto::aws_lc_rs::default_provider()
    });
    let new_key = || KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(err("a key"));
    let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(err("its CA"))?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.distinguished_name
        .push(DnType::CommonName, format!("shards server of {label}"));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    let ca_key = new_key()?;
    let ca_cert = ca.self_signed(&ca_key).map_err(err("its CA"))?;
    let issuer = Issuer::new(ca, ca_key);
    let leaf = |names: Vec<String>, cn: &str, usage: ExtendedKeyUsagePurpose| {
        let mut p = CertificateParams::new(names).map_err(err("a certificate"))?;
        p.distinguished_name.push(DnType::CommonName, cn);
        p.extended_key_usages = vec![usage];
        let key = new_key()?;
        let cert = p.signed_by(&key, &issuer).map_err(err("a certificate"))?;
        Ok::<_, String>((cert, key))
    };
    let (server, server_key) = leaf(vec![NAME.to_string()], NAME, ExtendedKeyUsagePurpose::ServerAuth)?;
    let (cert, key) = leaf(Vec::new(), label, ExtendedKeyUsagePurpose::ClientAuth)?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca_cert.der().clone())
        .map_err(|e| format!("its CA: {e}"))?;
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|e| format!("its client verifier: {e}"))?;
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS 1.3: {e}"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .map_err(|e| format!("its certificate: {e}"))?;
    let fail = |what: &str, e: io::Error| format!("{dir}/{what}: {e}");
    let write = |name: &str, text: &str, mode: u32| -> Result<(), String> {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, text).map_err(|e| fail(name, e))?;
        std::os::unix::fs::chown(&path, None, Some(gid)).map_err(|e| fail(name, e))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).map_err(|e| fail(name, e))
    };
    write("ca.pem", &ca_cert.pem(), 0o444)?;
    write("cert.pem", &cert.pem(), 0o444)?;
    write("key.pem", &key.serialize_pem(), 0o440)?;
    let sock = format!("{dir}/server.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| fail("server.sock", e))?;
    std::os::unix::fs::chown(&sock, None, Some(gid)).map_err(|e| fail("server.sock", e))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| fail("server.sock", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| fail("server.sock", e))?;
    Ok(Prepared {
        label: label.to_string(),
        listener,
        cert: cert.der().clone(),
        config: Arc::new(config),
        scratch: None,
        offered: "[]".into(),
    })
}

/// A connection, and what it has sent.
struct Conn {
    id: u64,
    stream: UnixStream,
    tls: rustls::ServerConnection,
    checked: bool,
    plain: Vec<u8>,
    closing: bool,
    /// The program whose answer it awaits before it reads another request (D136), and what
    /// it does once that answer is written.
    waiting: Option<(u64, AfterAnswer)>,
}

/// What a connection does once the program it awaits has answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterAnswer {
    /// Reads its next request.
    Read,
    /// Closes, as its request asked (`Connection: close`): the answer is its last (RFC 9112
    /// §9.6, a server closes after the response to the request that said close).
    Close,
}

/// How many connections it holds at once: what this process may open
/// (`RLIMIT_NOFILE`), less its standard descriptors and its listener.
fn room() -> usize {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a struct it owns.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
        return 0;
    }
    usize::try_from(lim.rlim_cur)
        .unwrap_or(usize::MAX)
        .saturating_sub(4)
}

pub fn serve(p: &Prepared, peers: &mut [Peer], mut programs: Programs) {
    let room = room();
    let mut conns: Vec<Conn> = Vec::new();
    let mut next_conn = 0u64;
    let mut set: Vec<libc::pollfd> = Vec::new();
    loop {
        set.clear();
        set.push(libc::pollfd {
            fd: p.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        // A peer's channel only while what it sends is wanted: one gone, or whose message
        // is held, left in the set says POLLHUP to every poll once its end has closed,
        // which poll(2) reports whatever it was asked, and the loop would spin.
        for peer in peers.iter() {
            let wanted = peer.held.is_none() && !peer.gone;
            set.push(libc::pollfd {
                fd: if wanted { peer.fd.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for c in &conns {
            let out = if c.tls.wants_write() { libc::POLLOUT } else { 0 };
            set.push(libc::pollfd {
                fd: c.stream.as_raw_fd(),
                events: libc::POLLIN | out,
                revents: 0,
            });
        }
        // The helper's socket, if there is one, and room in it while it is owed STOPs.
        let control_at = programs.control.as_ref().ok().map(|control| {
            let at = set.len();
            let out = if programs.unstopped.is_empty() {
                0
            } else {
                libc::POLLOUT
            };
            set.push(libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN | out,
                revents: 0,
            });
            at
        });
        for prog in &programs.running {
            let out = if prog.write.is_empty() { 0 } else { libc::POLLOUT };
            set.push(libc::pollfd {
                fd: prog.socket.as_raw_fd(),
                events: libc::POLLIN | out,
                revents: 0,
            });
        }
        // Until the nearest of the programs' deadlines, if one has any.
        let wait = programs
            .running
            .iter()
            .filter_map(|prog| prog.deadline.map(|(at, _)| at))
            .min()
            .map_or(-1, |at| {
                i32::try_from(
                    at.saturating_duration_since(Instant::now())
                        .as_millis()
                        .saturating_add(1),
                )
                .unwrap_or(i32::MAX)
            });
        // SAFETY: poll(2) of pollfds of ours.
        if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, wait) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return;
        }
        // Only what poll said is ready: an accept(2) with no connection waiting makes a
        // socket, a file and a dentry and frees them after an RCU grace period
        // (net/socket.c, `do_accept`), charged to this domain's memory meanwhile, so that
        // a loop of them took it past the agents' share.
        let ready = |i: usize| set.get(i).is_some_and(|s| s.revents != 0);
        if control_at
            .and_then(|i| set.get(i))
            .is_some_and(|s| s.revents & libc::POLLOUT != 0)
        {
            programs.send_stops();
        }
        let listening = ready(0);
        while listening && let Ok((stream, _)) = p.listener.accept() {
            if conns.len() >= room || stream.set_nonblocking(true).is_err() {
                continue;
            }
            if let Ok(tls) = rustls::ServerConnection::new(p.config.clone()) {
                next_conn = next_conn.wrapping_add(1);
                conns.push(Conn {
                    id: next_conn,
                    stream,
                    tls,
                    checked: false,
                    plain: Vec::new(),
                    closing: false,
                    waiting: None,
                });
            }
        }
        // The listener's pollfd, then each peer's, in order.
        for (i, peer) in peers.iter_mut().enumerate() {
            if ready(i + 1) {
                take(peer);
            }
        }
        let mut answers: Vec<(u64, Vec<u8>)> = Vec::new();
        run_programs(p, peers, &mut programs, &mut answers);
        for (conn, rpc_id) in programs.cancelled.drain(..) {
            let said = format!(
                r#"{{"jsonrpc":"2.0","id":{rpc_id},"error":{{"code":-32800,"message":"request cancelled"}}}}"#
            );
            answers.push((conn, http(200, "application/json", said.as_bytes())));
        }
        for (conn, answer) in answers {
            if let Some(c) = conns.iter_mut().find(|c| c.id == conn) {
                let _ = c.tls.writer().write_all(&answer);
                if let Some((_, AfterAnswer::Close)) = c.waiting.take() {
                    c.closing = true;
                    c.tls.send_close_notify();
                }
            }
        }
        for c in &mut conns {
            step(p, peers, &mut programs, c);
        }
        // A connection gone ends the program it awaited, if that program still runs.
        for c in conns.iter().filter(|c| c.closing) {
            if let Some((id, _)) = c.waiting
                && let Some(at) = programs.running.iter().position(|prog| prog.id == id)
            {
                // Its socket closes once its STOP is sent or owed, as on every other path.
                let ended = programs.running.swap_remove(at);
                programs.stop(id);
                drop(ended);
            }
        }
        conns.retain(|c| !(c.closing && !c.tls.wants_write()));
    }
}

/// The JSON-RPC answer to request `rpc_id`: tool result `result`.
fn answered(rpc_id: &str, result: &str) -> Vec<u8> {
    http(
        200,
        "application/json",
        format!(r#"{{"jsonrpc":"2.0","id":{rpc_id},"result":{result}}}"#).as_bytes(),
    )
}

/// What each program came to: its calls answered, and, for each that ended, the answer its
/// connection awaits, into `answers`: its value, why it failed, or that it ran past its
/// time; the helper's word of a worker that ended without a word of its own.
fn run_programs(
    p: &Prepared,
    peers: &mut [Peer],
    programs: &mut Programs,
    answers: &mut Vec<(u64, Vec<u8>)>,
) {
    let mut ended: Vec<(u64, String)> = Vec::new();
    // The helper's: workers that ended.
    if let Ok(control) = &programs.control {
        loop {
            match sandbox::recv_control(control.as_raw_fd()) {
                Ok(Some((msg, _))) => {
                    let id = msg
                        .get(1..9)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_be_bytes);
                    let code = msg
                        .get(9..13)
                        .and_then(|b| b.try_into().ok())
                        .map(i32::from_be_bytes);
                    let status = msg
                        .get(13..17)
                        .and_then(|b| b.try_into().ok())
                        .map(i32::from_be_bytes);
                    if let (Some(&sandbox::ENDED), Some(id), Some(code), Some(status)) =
                        (msg.first(), id, code, status)
                    {
                        ended.push((id, worker_end(code, status)));
                    }
                }
                Ok(None) => {
                    programs.control = Err(HelperFailure::Ended);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    programs.control = Err(HelperFailure::io(HelperOperation::Receive, e));
                    break;
                }
            }
        }
    }
    if let Err(why) = &programs.control {
        for prog in &programs.running {
            ended.push((prog.id, format!("code mode ended: {why}")));
        }
    }
    let now = Instant::now();
    let mut finished: Vec<(u64, String, bool)> = Vec::new();
    for prog in &mut programs.running {
        // Its frames: each call answered, its end.
        let mut buf = [0u8; 64 * 1024];
        loop {
            match prog.socket.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let got = buf.get(..n).unwrap_or_default();
                    if prog.read.try_reserve(got.len()).is_err() {
                        break;
                    }
                    prog.read.extend_from_slice(got);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
            // A whole frame read is answered before more is: the worker waits for its
            // answer, and a frame being read is read on.
            if !matches!(sandbox::parse_frame(&prog.read), Ok(None)) {
                break;
            }
        }
        loop {
            let parsed =
                sandbox::parse_frame(&prog.read).map(|f| f.map(|(k, b, used)| (k, b.to_vec(), used)));
            match parsed {
                Ok(Some((sandbox::CALL, bytes, used))) => {
                    prog.read.drain(..used.min(prog.read.len()));
                    let reply = call_for_program(p, peers, &bytes);
                    let framed = sandbox::frame(sandbox::REPLY, reply.as_bytes()).or_else(|why| {
                        let refused = format!(r#"{{"error":{}}}"#, quote(&format!("its answer: {why}")));
                        sandbox::frame(sandbox::REPLY, refused.as_bytes())
                    });
                    match framed {
                        Ok(f) => prog.write.extend(f),
                        Err(why) => {
                            finished.push((prog.id, why, true));
                            break;
                        }
                    }
                }
                Ok(Some((sandbox::DONE, bytes, _))) => {
                    finished.push((prog.id, String::from_utf8_lossy(&bytes).into_owned(), false));
                    break;
                }
                Ok(Some((sandbox::FAILED, bytes, _))) => {
                    finished.push((prog.id, String::from_utf8_lossy(&bytes).into_owned(), true));
                    break;
                }
                Ok(Some((other, _, _))) => {
                    finished.push((prog.id, format!("its worker sent frame {other}"), true));
                    break;
                }
                Ok(None) => break,
                Err(e) => {
                    finished.push((prog.id, e, true));
                    break;
                }
            }
        }
        while !prog.write.is_empty() {
            match prog.socket.write(&prog.write) {
                Ok(0) => break,
                Ok(n) => {
                    prog.write.drain(..n.min(prog.write.len()));
                }
                Err(_) => break,
            }
        }
        if let Some((at, ms)) = prog.deadline
            && now >= at
        {
            finished.push((
                prog.id,
                format!("the program ran past its {ms} ms, and was ended"),
                true,
            ));
        }
    }
    for (id, why) in ended {
        if !finished.iter().any(|(f, _, _)| *f == id) {
            finished.push((id, why, true));
        }
    }
    for (id, said, failed) in finished {
        let Some(at) = programs.running.iter().position(|prog| prog.id == id) else {
            continue;
        };
        let prog = programs.running.swap_remove(at);
        if failed {
            programs.stop(id);
        }
        let result = text(&said, failed);
        let result = if prog.modern {
            completed(&result, "")
        } else {
            result
        };
        answers.push((prog.conn, answered(&prog.rpc_id, &result)));
    }
}

/// How a worker ended, as `waitid` said it (`CLD_*`, include/uapi/asm-generic/siginfo.h).
fn worker_end(code: i32, status: i32) -> String {
    const CLD_EXITED: i32 = 1;
    match code {
        CLD_EXITED => format!("the program ended without a word, its worker's status {status}"),
        sandbox::END_UNKNOWN => {
            format!("the program ended without a word, how is not known (waitid: OS {status})")
        }
        _ if status == libc::SIGSYS => {
            "the program's worker made a system call its sandbox refuses, and was ended".to_string()
        }
        _ if status == libc::SIGKILL => "the program was ended".to_string(),
        -1 => "the program's worker could not be made".to_string(),
        _ => format!("the program's worker was ended by signal {status}"),
    }
}

/// A program's call, a frame's JSON (`{"name": ..., "args": {...}}`), made as the caller's
/// MCP call of that tool is: its answer, JSON with the tool's `value` or its `error`.
fn call_for_program(p: &Prepared, peers: &mut [Peer], bytes: &[u8]) -> broker::Reply {
    let Ok(msg) = json::parse(bytes) else {
        return broker::Reply::Literal(r#"{"error":"a call that is no JSON"}"#);
    };
    let name = msg.get("name").and_then(Value::str).unwrap_or_default();
    match call(p, peers, name, msg.get("args"), broker::Format::Program) {
        Some(Dispatched::Effect(reply)) => reply,
        Some(Dispatched::Value(Ok(value))) => format!(r#"{{"value":{value}}}"#).into(),
        Some(Dispatched::Value(Err(why))) => format!(r#"{{"error":{}}}"#, quote(&why)).into(),
        None => format!(r#"{{"error":{}}}"#, quote(&format!("no tool {name}"))).into(),
    }
}

/// Reads what a connection sent, answers each whole request, and writes what is due.
fn step(p: &Prepared, peers: &mut [Peer], programs: &mut Programs, c: &mut Conn) {
    loop {
        match c.tls.read_tls(&mut c.stream) {
            Ok(0) => {
                c.closing = true;
                break;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => {
                c.closing = true;
                break;
            }
        }
        if c.tls.process_new_packets().is_err() {
            c.closing = true;
            break;
        }
    }
    // The certificate presented must be the one issued for this socket's caller.
    if !c.checked && !c.tls.is_handshaking() {
        let presented = c.tls.peer_certificates().and_then(<[_]>::first);
        if presented != Some(&p.cert) {
            c.closing = true;
            c.tls.send_close_notify();
        }
        c.checked = true;
    }
    if c.checked && !c.closing {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match c.tls.reader().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let got = buf.get(..n).unwrap_or_default();
                    // Past what the instance can hold, the connection ends.
                    if c.plain.try_reserve(got.len()).is_err() {
                        c.closing = true;
                        break;
                    }
                    c.plain.extend_from_slice(got);
                }
            }
        }
        while c.waiting.is_none()
            && let Some((answer, used, close)) = request(p, peers, programs, c.id, &c.plain)
        {
            c.plain.drain(..used.min(c.plain.len()));
            match answer {
                Reply::Now(answer) => {
                    let _ = c.tls.writer().write_all(&answer);
                    if close {
                        c.closing = true;
                        c.tls.send_close_notify();
                        break;
                    }
                }
                // A program's: its answer comes once it ends, before the next request, and
                // the close its request asked for comes after that answer.
                Reply::Later(id) => {
                    let after = if close {
                        AfterAnswer::Close
                    } else {
                        AfterAnswer::Read
                    };
                    c.waiting = Some((id, after));
                }
            }
        }
    }
    while c.tls.wants_write() {
        match c.tls.write_tls(&mut c.stream) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => {
                c.closing = true;
                return;
            }
        }
    }
}

/// A request's answer: now, or, for a program's, once the program ends (D136).
enum Reply {
    Now(Vec<u8>),
    Later(u64),
}

/// The first whole request in `plain`, connection `conn`'s, answered: the answer, the bytes
/// it took, and whether the connection then closes. None until one is whole.
fn request(
    p: &Prepared,
    peers: &mut [Peer],
    programs: &mut Programs,
    conn: u64,
    plain: &[u8],
) -> Option<(Reply, usize, bool)> {
    let now = |answer: Vec<u8>, used: usize, close: bool| Some((Reply::Now(answer), used, close));
    let end = plain.windows(4).position(|w| w == b"\r\n\r\n");
    let Some(end) = end else {
        return (plain.len() > HEAD_MAX)
            .then(|| (Reply::Now(status(431, "header too large")), plain.len(), true));
    };
    if end > HEAD_MAX {
        return now(status(431, "header too large"), plain.len(), true);
    }
    let head = String::from_utf8_lossy(plain.get(..end)?);
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?, first.next()?);
    let mut length = 0usize;
    let mut close = false;
    let mut head = Head::default();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = Some(v.trim().to_string());
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => match v.as_deref().map(str::parse) {
                Some(Ok(n)) => length = n,
                _ => return now(status(400, "bad content-length"), plain.len(), true),
            },
            "connection" => close = v.is_some_and(|v| v.eq_ignore_ascii_case("close")),
            "mcp-session-id" => head.session = v,
            "mcp-protocol-version" => head.version = v,
            "mcp-method" => head.method = v,
            "mcp-name" => head.name = v,
            "origin" => head.origin = v,
            _ => {}
        }
    }
    // A body is held as its length says; one no memory addresses is refused.
    let Some(used) = end.checked_add(4).and_then(|n| n.checked_add(length)) else {
        return now(status(413, "body too large"), plain.len(), true);
    };
    let body = plain.get(end + 4..used)?;
    // No web page may reach it (Streamable HTTP, "Security"): its callers are agents, over
    // a socket of their own, which no browser opens; an origin named is none it serves.
    if head.origin.is_some() {
        let said = format!(
            r#"{{"jsonrpc":"2.0","error":{{"code":-32600,"message":{}}}}}"#,
            quote("this server answers no web origin")
        );
        return now(http(403, "application/json", said.as_bytes()), used, close);
    }
    if path != "/mcp" {
        return now(status(404, "not found"), used, close);
    }
    // POST alone: no stream of the server's own to GET, no session to DELETE (2026-07-28
    // has neither; before it, a server may offer neither).
    if method != "POST" {
        return now(
            reply(405, "text/plain", "Allow: POST\r\n", b"method not allowed"),
            used,
            close,
        );
    }
    let answer = match json::parse(body) {
        Ok(msg) => match era(p, peers, programs, conn, &head, &msg) {
            Err(refused) => refused,
            Ok(Answered::Now(result)) => http(200, "application/json", result.as_bytes()),
            // A session begun (MCP 2025-03-26 to 2025-11-25, Streamable HTTP, "Session
            // Management"): its ID, which the client's requests in it carry.
            Ok(Answered::Begun(result, session)) => reply(
                200,
                "application/json",
                &format!("Mcp-Session-Id: {session}\r\n"),
                result.as_bytes(),
            ),
            Ok(Answered::Later(id)) => return Some((Reply::Later(id), used, close)),
            // A notification is accepted, and answered with nothing.
            Ok(Answered::Nothing) => http(202, "", b""),
        },
        Err(_) => http(
            400,
            "application/json",
            error(&Value::Null, -32700, "parse error").as_bytes(),
        ),
    };
    now(answer, used, close)
}

/// A tool's result: its one text item, a JSON value, and whether it is an error the caller
/// may act on (MCP's `isError`).
fn text(value: &str, is_error: bool) -> String {
    format!(
        r#"{{"content":[{{"type":"text","text":{}}}],"isError":{is_error}}}"#,
        quote(value)
    )
}

/// What it offers: who the caller is, those it may message, and messaging them.
const TOOLS: &str = r#"{"tools":[
{"name":"whoami","description":"Who this server knows the caller as: the agent or harness whose socket it called on.","inputSchema":{"type":"object","properties":{}}},
{"name":"mcp","description":"The MCP servers offered the caller: a remote one by its URL, one spoken to over stdio by its directory and the command that runs it, which the caller runs itself, in its own confinement.","inputSchema":{"type":"object","properties":{}}},
{"name":"peers","description":"The agents and harnesses the caller may message, as its Agentfile grants: whether it may send each requests (send), and whether it answers theirs (answer).","inputSchema":{"type":"object","properties":{}}},
{"name":"send","description":"Sends a peer the caller may send requests a request; its ID, which the peer's answer carries.","inputSchema":{"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]}},
{"name":"receive","description":"What peers have sent the caller and it has not taken: their requests, and their answers to its own, each a text or a file (its name, its size, and its bytes in Base64).","inputSchema":{"type":"object","properties":{}}},
{"name":"answer","description":"Answers a request a peer sent the caller, by its ID.","inputSchema":{"type":"object","properties":{"to":{"type":"string"},"id":{"type":"integer"},"text":{"type":"string"}},"required":["to","id","text"]}},
{"name":"hand","description":"Hands a peer a file of the caller's scratch, /tmp: a request where the caller may send the peer requests, or, with id, the answer to its request of that ID. The file is a regular one, which the caller's group may read, of at most 1 MiB less its name; its server opens it beneath /tmp, through no link, and reads it whole.","inputSchema":{"type":"object","properties":{"to":{"type":"string"},"path":{"type":"string"},"id":{"type":"integer"}},"required":["to","path"]}},
{"name":"run","description":"Runs a JavaScript program (ECMAScript, QuickJS-NG) that calls the tools above, many in one round trip, in a sandbox with the caller's grants and no more: no files, no network, no processes. The program is the body of an async function, in strict mode: await the tools, and return a value, which comes back as JSON; a tool's refusal is thrown as an Error, and an error the program does not catch comes back with its stack, at the program's own lines and columns. send, answer and hand also give the broker's typed receipt of what they did: as the cause of the Error they throw, and as the broker property of the value they return. timeout_ms, if given, ends it past that many milliseconds.\n\ndeclare const shards: {\n  whoami(): Promise<string>;\n  mcp(): Promise<Array<{ name: string; remote: true; url: string } | { name: string; remote: false; dir: string; command: string[] }>>;\n  peers(): Promise<Array<{ name: string; send: boolean; answer: boolean }>>;\n  send(to: string, text: string): Promise<{ id: number }>;\n  receive(): Promise<Array<{ from: string; kind: \"request\" | \"answer\"; id: number; text?: string; file?: { name: string; size: number; data: string } }>>;\n  answer(to: string, id: number, text: string): Promise<{}>;\n  hand(to: string, path: string, id?: number): Promise<{ id: number } | {}>;\n};","inputSchema":{"type":"object","properties":{"code":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}},"required":["code"]}}
]}"#;

/// What a JSON-RPC message came to: an answer now, an answer that begins a session and that
/// session's ID, a program's once it ends, or nothing, for a notification.
enum Answered {
    Now(String),
    Begun(String, String),
    Later(u64),
    Nothing,
}

/// Tool `name` called with `args` by the caller, for its MCP call or a program's (D136):
/// its value, as JSON, or why it failed; None for no such tool.
enum Dispatched {
    Value(Result<String, String>),
    Effect(broker::Reply),
}

fn call(
    p: &Prepared,
    peers: &mut [Peer],
    name: &str,
    args: Option<&Value>,
    format: broker::Format,
) -> Option<Dispatched> {
    if let Some(request) = broker::Request::receive(name, args) {
        let request = match request {
            Ok(request) => request,
            Err(error) => return Some(Dispatched::Effect(format.refused(error, &p.label, ""))),
        };
        let to = request.destination();
        let peer = peers
            .iter_mut()
            .find(|peer| peer.label == to)
            .map(|peer| broker::Peer {
                label: &peer.label,
                fd: std::os::fd::AsFd::as_fd(&peer.fd),
                send: peer.send,
                answer: peer.answer,
                most: peer.most,
                last: &mut peer.last,
                sent: &mut peer.sent,
                asked: &mut peer.asked,
            });
        let context = broker::Context {
            principal: &p.label,
            scratch: p.scratch.as_ref().map(std::os::fd::AsFd::as_fd),
            evidence: broker::Evidence::MemoryOnly,
        };
        let terminal = match request
            .authorize(context, peer)
            .and_then(|authorized| authorized.reserve(format))
        {
            Ok(reserved) => reserved.execute().present(),
            Err(error) => format.refused(error, &p.label, to),
        };
        return Some(Dispatched::Effect(terminal));
    }
    let value = match name {
        "whoami" => Ok(quote(&p.label)),
        "mcp" => Ok(p.offered.clone()),
        "peers" => {
            let list: Vec<String> = peers
                .iter()
                .map(|q| {
                    format!(
                        r#"{{"name":{},"send":{},"answer":{}}}"#,
                        quote(&q.label),
                        q.send,
                        q.answer
                    )
                })
                .collect();
            Ok(format!("[{}]", list.join(",")))
        }
        "receive" => {
            let mut got = Vec::new();
            for q in peers.iter_mut() {
                if let Some(m) = q.held.take() {
                    if m.request {
                        q.asked.push(m.id);
                    }
                    let body = match &m.body {
                        Body::Text(text) => format!(r#""text":{}"#, quote(text)),
                        Body::File(name, bytes) => format!(
                            r#""file":{{"name":{},"size":{},"data":"{}"}}"#,
                            quote(name),
                            bytes.len(),
                            base64(bytes)
                        ),
                    };
                    got.push(format!(
                        r#"{{"from":{},"kind":"{}","id":{},{body}}}"#,
                        quote(&q.label),
                        if m.request { "request" } else { "answer" },
                        m.id,
                    ));
                }
            }
            Ok(format!("[{}]", got.join(",")))
        }
        _ => return None,
    };
    Some(Dispatched::Value(value))
}

#[cfg(test)]
fn handed(p: &Prepared, path: &str, message: usize) -> Result<(String, Vec<u8>), broker::Refusal> {
    broker::read_for_test(p.scratch.as_ref().map(std::os::fd::AsFd::as_fd), path, message)
}

/// openat2(2)'s `resolve` flags (include/uapi/linux/openat2.h).
pub(super) const RESOLVE_NO_XDEV: u64 = 0x01;
pub(super) const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub(super) const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub(super) const RESOLVE_BENEATH: u64 = 0x08;

/// `bytes` in Base64, the standard alphabet with padding (RFC 4648 §4).
fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                let digit = DIGITS.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'A');
                out.push(char::from(digit));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// What a request's head says beside its length: the MCP headers, and the page it came
/// from, if a browser sent it.
#[derive(Debug, Default)]
struct Head {
    session: Option<String>,
    version: Option<String>,
    method: Option<String>,
    name: Option<String>,
    origin: Option<String>,
}

/// A request answered by the revision it is of: the current one where its `_meta` says
/// so, each request standing alone; those before it otherwise, a session's. Or its
/// refusal, an answer of its own, where its headers or its revision do not hold.
fn era(
    p: &Prepared,
    peers: &mut [Peer],
    programs: &mut Programs,
    conn: u64,
    head: &Head,
    msg: &Value,
) -> Result<Answered, Vec<u8>> {
    let meta = msg.get("params").and_then(|params| params.get("_meta"));
    let id = msg.get("id").unwrap_or(&Value::Null);
    let refuse = |status: u16, code: i32, message: &str, data: Option<String>| {
        let mut said = format!(
            r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":{code},"message":{}"#,
            write(id),
            quote(message)
        );
        if let Some(data) = data {
            said.push_str(&format!(r#","data":{data}"#));
        }
        said.push_str("}}");
        http(status, "application/json", said.as_bytes())
    };
    let unsupported = |asked: &str| {
        let supported: Vec<String> = std::iter::once(MODERN).chain(REVISIONS).map(quote).collect();
        refuse(
            400,
            -32022,
            "Unsupported protocol version",
            Some(format!(
                r#"{{"supported":[{}],"requested":{}}}"#,
                supported.join(","),
                quote(asked)
            )),
        )
    };
    let Some(version) = meta.and_then(|m| m.get(VERSION_KEY)).and_then(Value::str) else {
        // A revision before the current one: its header, once it has one, one of them.
        return match head.version.as_deref() {
            None => Ok(rpc(p, peers, programs, conn, head.session.as_deref(), msg)),
            Some(v) if REVISIONS.contains(&v) => {
                Ok(rpc(p, peers, programs, conn, head.session.as_deref(), msg))
            }
            // The current revision's request without its `_meta`, which it must have.
            Some(MODERN) => Err(refuse(
                400,
                -32602,
                &format!("Invalid params: no {VERSION_KEY} in _meta"),
                None,
            )),
            Some(v) => Err(unsupported(v)),
        };
    };
    // Its headers say what its body says (Streamable HTTP, "Server Validation").
    let method = msg.get("method").and_then(Value::str).unwrap_or_default();
    let mismatch = |header: &str, said: Option<&str>, body: &str| {
        refuse(
            400,
            -32020,
            &match said {
                Some(said) => format!(
                    "Header mismatch: {header} header value {said:?} does not match body value {body:?}"
                ),
                None => format!("Header mismatch: no {header} header for body value {body:?}"),
            },
            None,
        )
    };
    if head.version.as_deref() != Some(version) {
        return Err(mismatch("MCP-Protocol-Version", head.version.as_deref(), version));
    }
    if head.method.as_deref() != Some(method) {
        return Err(mismatch("Mcp-Method", head.method.as_deref(), method));
    }
    if method == "tools/call" {
        let name = msg
            .get("params")
            .and_then(|params| params.get("name"))
            .and_then(Value::str)
            .unwrap_or_default();
        let said = head.name.as_deref().map(header_value);
        if said.as_ref().is_none_or(|said| said.as_deref() != Some(name)) {
            return Err(mismatch("Mcp-Name", head.name.as_deref(), name));
        }
    }
    if version != MODERN {
        return Err(unsupported(version));
    }
    if !meta
        .and_then(|m| m.get(CAPABILITIES_KEY))
        .is_some_and(|c| matches!(c, Value::Object(_)))
    {
        return Err(refuse(
            400,
            -32602,
            &format!("Invalid params: no {CAPABILITIES_KEY} in _meta"),
            None,
        ));
    }
    // No notification of the client's is defined over HTTP: one is accepted, and does
    // nothing; a request's cancellation is its connection closing.
    let Some(id) = msg.get("id") else {
        return Ok(Answered::Nothing);
    };
    let result = match method {
        "server/discover" => {
            let supported: Vec<String> = std::iter::once(MODERN).chain(REVISIONS).map(quote).collect();
            completed(
                &format!(
                    r#"{{"supportedVersions":[{}],"capabilities":{{"tools":{{}}}}}}"#,
                    supported.join(",")
                ),
                CACHED,
            )
        }
        "tools/list" => completed(TOOLS, CACHED),
        "tools/call" => match tool(p, peers, programs, conn, None, id, msg, true) {
            Called::Result(result) => completed(&result, ""),
            Called::Later(program) => return Ok(Answered::Later(program)),
            Called::Refused(answer) => return Ok(Answered::Now(answer)),
        },
        _ => return Err(refuse(404, -32601, "Method not found", None)),
    };
    Ok(Answered::Now(format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        write(id)
    )))
}

/// What a list's freshness is (2026-07-28, "Caching"): the same for every caller, and
/// stale at once, as asking again costs a caller tens of microseconds (PM M196), and a
/// cache kept past this instance, by a client that keeps one, would not see a newer
/// server's tools.
const CACHED: &str = r#""ttlMs":0,"cacheScope":"public""#;

/// `result`, a JSON object, as the current revision answers: `more` fields beside it,
/// its type, complete, and who answers.
fn completed(result: &str, more: &str) -> String {
    let fields = format!(
        r#"{more}{}"resultType":"complete","_meta":{{"io.modelcontextprotocol/serverInfo":{{"name":"shards","version":{}}}}}"#,
        if more.is_empty() { "" } else { "," },
        quote(VERSION)
    );
    match result.trim_end().strip_suffix('}') {
        Some(head) if head.trim_end().ends_with('{') => format!("{head}{fields}}}"),
        Some(head) => format!("{head},{fields}}}"),
        None => result.to_string(),
    }
}

/// A header's value as its sender meant it: plain, or Base64 between `=?base64?` and `?=`
/// (Streamable HTTP, "Value Encoding"); None for Base64 that is no UTF-8 or no Base64.
fn header_value(said: &str) -> Option<String> {
    let Some(encoded) = said.strip_prefix("=?base64?").and_then(|r| r.strip_suffix("?=")) else {
        return Some(said.to_string());
    };
    let digit = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let body = encoded.trim_end_matches('=');
    if encoded.len() % 4 != 0 || encoded.len() - body.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in body.bytes() {
        acc = (acc << 6) | u32::from(digit(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    String::from_utf8(out).ok()
}

/// What a `tools/call` came to: its tool's result, a program's answer to come, or a
/// refusal, its whole answer.
enum Called {
    Result(broker::Reply),
    Later(u64),
    Refused(String),
}

/// `tools/call`, request `id`, its tool called as the caller's: a program started for
/// `run`, in `session`, its answer shaped by its revision (`modern`).
#[expect(
    clippy::too_many_arguments,
    reason = "a call's whole context, both revisions'"
)]
fn tool(
    p: &Prepared,
    peers: &mut [Peer],
    programs: &mut Programs,
    conn: u64,
    session: Option<&str>,
    id: &Value,
    msg: &Value,
    modern: bool,
) -> Called {
    let params = msg.get("params");
    let args = params.and_then(|p| p.get("arguments"));
    Called::Result(match params.and_then(|p| p.get("name")).and_then(Value::str) {
        Some("run") => {
            let code = args.and_then(|a| a.get("code")).and_then(Value::str);
            let timeout = args.and_then(|a| a.get("timeout_ms")).and_then(Value::u64);
            match code {
                Some(code) => {
                    match programs.start(code, timeout.filter(|&t| t > 0), conn, session, write(id), modern) {
                        Ok(id) => return Called::Later(id),
                        Err(why) => {
                            return Called::Result(
                                broker::Format::Mcp.display_refusal(format_args!("{why}")),
                            );
                        }
                    }
                }
                None => text("run takes a program, its code", true),
            }
        }
        .into(),
        Some(name) => match call(p, peers, name, args, broker::Format::Mcp) {
            Some(Dispatched::Effect(reply)) => reply,
            Some(Dispatched::Value(Ok(_))) if name == "whoami" => text(&p.label, false).into(),
            Some(Dispatched::Value(Ok(value))) => text(&value, false).into(),
            Some(Dispatched::Value(Err(why))) => text(&why, true).into(),
            None => return Called::Refused(error(id, -32602, "no such tool")),
        },
        None => return Called::Refused(error(id, -32602, "no such tool")),
    })
}

/// A JSON-RPC message's answer, sent on connection `conn` in `session`, of a revision
/// before the current one.
fn rpc(
    p: &Prepared,
    peers: &mut [Peer],
    programs: &mut Programs,
    conn: u64,
    session: Option<&str>,
    msg: &Value,
) -> Answered {
    let method = msg.get("method").and_then(Value::str);
    // A program's request let go of (modelcontextprotocol.io, "Cancellation"): its worker
    // ended, and its connection told so, as nothing else is to come of it. A request's ID
    // names it within its session, which two clients of one caller do not share.
    if method == Some("notifications/cancelled") {
        let asked = msg.get("params").and_then(|p| p.get("requestId")).map(write);
        if let Some(asked) = asked
            && let Some(at) = programs
                .running
                .iter()
                .position(|prog| prog.rpc_id == asked && prog.session.as_deref() == session)
        {
            let prog = programs.running.swap_remove(at);
            programs.stop(prog.id);
            programs.cancelled.push((prog.conn, prog.rpc_id));
        }
        return Answered::Nothing;
    }
    let Some(id) = msg.get("id") else {
        return Answered::Nothing;
    };
    let result = match method {
        Some("initialize") => {
            let asked = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::str);
            let version = REVISIONS
                .iter()
                .find(|r| Some(**r) == asked)
                .unwrap_or(&REVISIONS[0]);
            format!(
                r#"{{"protocolVersion":{},"capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"shards","version":{}}}}}"#,
                quote(version),
                quote(VERSION)
            )
        }
        Some("ping") => "{}".to_string(),
        Some("tools/list") => TOOLS.to_string(),
        Some("tools/call") => match tool(p, peers, programs, conn, session, id, msg, false) {
            Called::Result(result) => {
                return Answered::Now(format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
                    write(id)
                ));
            }
            Called::Later(program) => return Answered::Later(program),
            Called::Refused(answer) => return Answered::Now(answer),
        },
        _ => return Answered::Now(error(id, -32601, "no such method")),
    };
    let answer = format!(r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#, write(id));
    if method != Some("initialize") {
        return Answered::Now(answer);
    }
    match session_id() {
        Ok(session) => Answered::Begun(answer, session),
        Err(why) => Answered::Now(error(id, -32603, &format!("no session ID: {why}"))),
    }
}

/// A new session's ID: 128 bits from the kernel's CSPRNG, in hex, as the specification
/// asks one be, "globally unique and cryptographically secure", of visible ASCII.
fn session_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    // SAFETY: getrandom(2) into a buffer of ours, of its length.
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if usize::try_from(n).ok() != Some(bytes.len()) {
        return Err(format!("getrandom: {}", io::Error::last_os_error()));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn error(id: &Value, code: i32, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":{code},"message":{}}}}}"#,
        write(id),
        quote(message)
    )
}

/// An ID as it came: a string, a number, or null.
fn write(v: &Value) -> String {
    match v {
        Value::String(s) => quote(s),
        Value::Number(n) => n.clone(),
        _ => "null".to_string(),
    }
}

/// `s` as a JSON string (RFC 8259 §7).
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn http(code: u16, kind: &str, body: &[u8]) -> Vec<u8> {
    reply(code, kind, "", body)
}

/// An answer with header lines `more` (each ending CRLF) beside its length and type.
fn reply(code: u16, kind: &str, more: &str, body: &[u8]) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "",
    };
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\n", body.len());
    if !kind.is_empty() {
        head.push_str(&format!("Content-Type: {kind}\r\n"));
    }
    head.push_str(more);
    head.push_str("\r\n");
    [head.as_bytes(), body].concat()
}

fn status(code: u16, why: &str) -> Vec<u8> {
    http(code, "text/plain", why.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::crypto::aws_lc_rs::cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    };

    /// An instance answers only the certificate it issued its caller: another instance's
    /// is refused, and so is none.
    #[test]
    fn an_instance_answers_its_own_caller_alone() {
        use rustls::pki_types::ServerName;
        use rustls::pki_types::pem::PemObject as _;
        let base_dir = shards_testdir::TempDir::new("server").unwrap();
        let base = base_dir.join("server");
        // SAFETY: getgid(2).
        let gid = unsafe { libc::getgid() };
        let dirs: Vec<String> = ["x", "y"]
            .iter()
            .map(|n| {
                let d = base.join(n);
                std::fs::create_dir_all(&d).unwrap();
                d.to_str().unwrap().to_string()
            })
            .collect();
        for (dir, label) in dirs.iter().zip(["agent x", "agent y"]) {
            let p = prepare(dir, label, gid).unwrap();
            std::thread::spawn(move || serve(&p, &mut Vec::new(), Programs::new(Err(HelperFailure::Ended))));
        }
        let ask = |socket: &str, creds: Option<&str>| -> Result<String, String> {
            let ca = CertificateDer::from_pem_file(format!("{socket}/ca.pem")).map_err(|e| e.to_string())?;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca).map_err(|e| e.to_string())?;
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots);
            let config = match creds {
                Some(d) => {
                    let cert =
                        CertificateDer::from_pem_file(format!("{d}/cert.pem")).map_err(|e| e.to_string())?;
                    let key =
                        PrivateKeyDer::from_pem_file(format!("{d}/key.pem")).map_err(|e| e.to_string())?;
                    builder
                        .with_client_auth_cert(vec![cert], key)
                        .map_err(|e| e.to_string())?
                }
                None => builder.with_no_client_auth(),
            };
            let name = ServerName::try_from(NAME).map_err(|e| e.to_string())?;
            let tls = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
            let sock = UnixStream::connect(format!("{socket}/server.sock")).map_err(|e| e.to_string())?;
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            let mut s = rustls::StreamOwned::new(tls, sock);
            let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"whoami"}}"#;
            let req = format!(
                "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
            let mut got = Vec::new();
            s.read_to_end(&mut got).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&got).into_owned();
            text.split_once(r#""text":""#)
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(t, _)| t.to_string())
                .ok_or(format!("no answer: {text:?}"))
        };
        let (x, y) = (dirs[0].as_str(), dirs[1].as_str());
        assert_eq!(ask(x, Some(x)), Ok("agent x".to_string()));
        assert_eq!(ask(y, Some(y)), Ok("agent y".to_string()));
        assert!(
            ask(x, Some(y)).is_err(),
            "y's certificate was answered by x's instance"
        );
        assert!(ask(x, None).is_err(), "no certificate was answered");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What another instance sends is taken only as the grant says, whatever it is made to
    /// send: a request only from a peer this one answers, an answer only to a request it
    /// sent.
    #[test]
    fn a_peer_is_heard_only_as_granted() {
        let pair = || {
            let mut fds = [0 as RawFd; 2];
            // SAFETY: socketpair(2) into a two-element array.
            let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
            assert_eq!(rc, 0);
            // SAFETY: both were just made, and are owned here alone.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        let peer = |fd: OwnedFd, answer: bool| Peer {
            label: "agent o".into(),
            most: message_most(&fd).unwrap(),
            fd,
            send: true,
            answer,
            held: None,
            sent: Vec::new(),
            last: 0,
            asked: Vec::new(),
            gone: false,
        };
        let other = |fd: &OwnedFd| peer(fd.try_clone().unwrap(), true);
        // A request from one it does not answer: dropped.
        let (mine, theirs) = pair();
        let mut p = peer(mine, false);
        assert_eq!(put(&other(&theirs), REQUEST, 1, "do this"), Put::Sent);
        take(&mut p);
        assert!(p.held.is_none());
        // From one it answers: held.
        let (mine, theirs) = pair();
        let mut p = peer(mine, true);
        assert_eq!(put(&other(&theirs), REQUEST, 1, "do this"), Put::Sent);
        take(&mut p);
        assert!(p.held.as_ref().is_some_and(|m| m.request && m.id == 1));
        // An answer to no request sent: dropped; to one sent, held, and no longer awaited.
        let (mine, theirs) = pair();
        let mut p = peer(mine, false);
        assert_eq!(put(&other(&theirs), ANSWER, 7, "unasked"), Put::Sent);
        take(&mut p);
        assert!(p.held.is_none());
        p.sent.push(7);
        assert_eq!(put(&other(&theirs), ANSWER, 7, "asked"), Put::Sent);
        take(&mut p);
        assert!(
            p.held
                .as_ref()
                .is_some_and(|m| !m.request && matches!(&m.body, Body::Text(t) if t == "asked"))
        );
        assert!(p.sent.is_empty());
        // A message is as long as its socket's buffer takes, whole: one past it refused,
        // saying how much it holds; one of it, sent and held whole.
        let (mine, theirs) = pair();
        let mut p = peer(mine, true);
        let o = other(&theirs);
        let most = o.most - MESSAGE_HEAD;
        assert_eq!(put(&o, REQUEST, 1, &"a".repeat(most + 1)), Put::Long(most));
        assert_eq!(put(&o, REQUEST, 1, &"a".repeat(most)), Put::Sent);
        take(&mut p);
        assert!(
            p.held
                .as_ref()
                .is_some_and(|m| matches!(&m.body, Body::Text(t) if t.len() == most))
        );
    }

    /// A file a peer hands is held as one, by the same grants as a text; one whose name runs
    /// past its message is dropped.
    #[test]
    fn a_peers_file_is_heard_as_granted() {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair(2) into a two-element array.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: both were just made, and are owned here alone.
        let (mine, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let peer = |fd: OwnedFd| Peer {
            label: "agent o".into(),
            most: message_most(&fd).unwrap(),
            fd,
            send: true,
            answer: false,
            held: None,
            sent: vec![3],
            last: 0,
            asked: Vec::new(),
            gone: false,
        };
        let other = peer(theirs);
        let mut p = peer(mine);
        assert_eq!(put_file(&other, REQUEST_FILE, 1, "x", b"y"), Put::Sent);
        take(&mut p);
        assert!(p.held.is_none(), "a request from one it does not answer");
        assert_eq!(put_bytes(&other, ANSWER_FILE, 3, &[0, 9, b'a']), Put::Sent);
        take(&mut p);
        assert!(p.held.is_none(), "a name past its message");
        assert_eq!(put_file(&other, ANSWER_FILE, 3, "out.txt", b"hello"), Put::Sent);
        take(&mut p);
        assert!(p.held.as_ref().is_some_and(|m| !m.request
            && m.id == 3
            && matches!(&m.body, Body::File(n, b) if n == "out.txt" && b == b"hello")));
    }

    /// Base64 as RFC 4648 §10's test vectors have it.
    #[test]
    fn base64_is_rfc_4648s() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
            assert_eq!(
                header_value(&format!("=?base64?{encoded}?=")).as_deref(),
                Some(plain)
            );
        }
    }

    /// A file is handed from beneath its caller's scratch, a regular one within what a
    /// message holds; a link, a FIFO, a directory, a giant file and a path past the scratch
    /// are refused, and the FIFO stalls nothing.
    #[test]
    fn only_a_regular_file_of_the_scratch_is_handed() {
        let mut p = prepare_for_test();
        let tmp = shards_testdir::TempDir::new("hand").unwrap();
        std::fs::write(tmp.join("out.txt"), "hello").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.join("key")).unwrap();
        let fifo = std::ffi::CString::new(tmp.join("pipe").to_str().unwrap()).unwrap();
        // SAFETY: mkfifo(3) of a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        std::fs::create_dir(tmp.join("d")).unwrap();
        // What a message to a peer holds: a channel's, as init makes them.
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair(2) into a two-element array.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: both were just made, and are owned here alone.
        let (one, _other) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let message = message_most(&one).unwrap();
        let most = message - MESSAGE_HEAD - file_head("big");
        std::fs::write(tmp.join("big"), vec![b'b'; most + 1]).unwrap();
        let dir = std::fs::File::open(&*tmp).unwrap();
        p.scratch = Some(OwnedFd::from(dir));
        assert_eq!(
            handed(&p, "/tmp/out.txt", message),
            Ok(("out.txt".to_string(), b"hello".to_vec()))
        );
        let refused = |path: &str| handed(&p, path, message).unwrap_err().reason;
        assert_eq!(refused("/tmp/key"), broker::Reason::FileIo(Some(libc::ELOOP)));
        assert_eq!(refused("/tmp/pipe"), broker::Reason::FileKind);
        assert_eq!(refused("/tmp/d"), broker::Reason::FileKind);
        assert_eq!(refused("/tmp/big"), broker::Reason::TooLong);
        assert_eq!(refused("/tmp/../x"), broker::Reason::FileIo(Some(libc::EXDEV)));
        assert_eq!(refused("/x"), broker::Reason::Path);
        p.scratch = None;
        assert_eq!(
            handed(&p, "/tmp/out.txt", message).unwrap_err().reason,
            broker::Reason::NoScratch
        );
    }

    #[test]
    fn strings_are_quoted_as_json_writes_them() {
        assert_eq!(quote("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
    }

    /// A head past its bound, a body longer than memory addresses, and a length that is no
    /// number each end the connection with an answer.
    #[test]
    fn requests_past_their_bounds_are_refused() {
        let p = prepare_for_test();
        let long = vec![b'a'; HEAD_MAX + 1];
        assert!(request(&p, &mut [], &mut no_programs(), 1, &long).is_some_and(
            |(a, _, close)| matches!(a, Reply::Now(a) if a.starts_with(b"HTTP/1.1 431")) && close
        ));
        let big = format!("POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n", usize::MAX);
        assert!(
            request(&p, &mut [], &mut no_programs(), 1, big.as_bytes()).is_some_and(
                |(a, _, close)| matches!(a, Reply::Now(a) if a.starts_with(b"HTTP/1.1 413")) && close
            )
        );
        let bad = b"POST /mcp HTTP/1.1\r\nContent-Length: x\r\n\r\n";
        assert!(request(&p, &mut [], &mut no_programs(), 1, bad).is_some_and(
            |(a, _, close)| matches!(a, Reply::Now(a) if a.starts_with(b"HTTP/1.1 400")) && close
        ));
    }

    fn no_programs() -> Programs {
        Programs::new(Err(HelperFailure::Ended))
    }

    /// A request's answer, which comes now, its head and body.
    fn asked(p: &Prepared, programs: &mut Programs, session: Option<&str>, body: &str) -> (String, String) {
        let session = session
            .map(|s| format!("Mcp-Session-Id: {s}\r\n"))
            .unwrap_or_default();
        asked_with(p, programs, &session, body)
    }

    /// A request with `headers` (each ending CRLF): its answer's head and body.
    fn asked_with(p: &Prepared, programs: &mut Programs, headers: &str, body: &str) -> (String, String) {
        let req = format!(
            "POST /mcp HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let Some((Reply::Now(answer), _, _)) = request(p, &mut [], programs, 1, req.as_bytes()) else {
            panic!("no answer now to {body}");
        };
        let answer = String::from_utf8(answer).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    /// `initialize` begins a session, its ID a new one each time: 128 bits, in hex.
    #[test]
    fn initialize_begins_a_session() {
        let p = prepare_for_test();
        let init =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let id = |head: &str| {
            head.lines()
                .find_map(|l| l.strip_prefix("Mcp-Session-Id: "))
                .map(str::to_string)
        };
        let (head, body) = asked(&p, &mut no_programs(), None, init);
        let first = id(&head).unwrap();
        assert!(
            first.len() == 32 && first.bytes().all(|b| b.is_ascii_hexdigit()),
            "{first}"
        );
        assert!(body.contains(r#""protocolVersion":"2025-06-18""#), "{body}");
        let (head, _) = asked(&p, &mut no_programs(), None, init);
        assert_ne!(id(&head), Some(first));
        let (head, _) = asked(
            &p,
            &mut no_programs(),
            None,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        assert_eq!(id(&head), None);
    }

    /// A request of MCP 2026-07-28: its body, its `_meta` naming `version`, and its
    /// headers as its client sends them.
    fn current(method: &str, params: &str, version: &str) -> (String, String) {
        let meta = format!(
            r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"{version}","io.modelcontextprotocol/clientCapabilities":{{}}}}"#
        );
        let params = if params.is_empty() {
            meta
        } else {
            format!("{params},{meta}")
        };
        let body = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{method}","params":{{{params}}}}}"#);
        let mut headers = format!("MCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n");
        if method == "tools/call" {
            let name = params
                .split(r#""name":""#)
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap();
            headers.push_str(&format!("Mcp-Name: {name}\r\n"));
        }
        (body, headers)
    }

    /// A request of the current revision is answered alone: its result complete, saying
    /// who answers; a list's with its caching hints; a tool's as before.
    #[test]
    fn the_current_revision_is_answered_request_by_request() {
        let p = prepare_for_test();
        let (body, headers) = current("tools/list", "", MODERN);
        let (head, said) = asked_with(&p, &mut no_programs(), &headers, &body);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            said.ends_with(&format!(
                r#""ttlMs":0,"cacheScope":"public","resultType":"complete","_meta":{{"io.modelcontextprotocol/serverInfo":{{"name":"shards","version":"{}"}}}}}}}}"#,
                VERSION
            )),
            "{said}"
        );
        let (body, headers) = current("tools/call", r#""name":"whoami","arguments":{}"#, MODERN);
        let (_, said) = asked_with(&p, &mut no_programs(), &headers, &body);
        assert!(
            said.starts_with(r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"agent t"}],"isError":false,"resultType":"complete","#),
            "{said}"
        );
        let (body, headers) = current("server/discover", "", MODERN);
        let (_, said) = asked_with(&p, &mut no_programs(), &headers, &body);
        assert!(
            said.contains(r#""supportedVersions":["2026-07-28","2025-11-25","2025-06-18","2025-03-26"],"capabilities":{"tools":{}},"ttlMs":0"#),
            "{said}"
        );
    }

    /// Headers that do not say what the body says are refused, 400 and HeaderMismatch; a
    /// name in Base64 is read as its sender meant it.
    #[test]
    fn headers_that_differ_from_their_body_are_refused() {
        let p = prepare_for_test();
        let (body, headers) = current("tools/call", r#""name":"whoami","arguments":{}"#, MODERN);
        for wrong in [
            headers.replace("Mcp-Method: tools/call", "Mcp-Method: tools/list"),
            headers.replace("Mcp-Name: whoami", "Mcp-Name: peers"),
            headers.replace("MCP-Protocol-Version: 2026-07-28\r\n", ""),
            headers.replace("Mcp-Name: whoami\r\n", ""),
        ] {
            let (head, said) = asked_with(&p, &mut no_programs(), &wrong, &body);
            assert!(head.starts_with("HTTP/1.1 400"), "{wrong:?}: {head}");
            assert!(said.contains(r#""code":-32020"#), "{wrong:?}: {said}");
        }
        let base64 = headers.replace("Mcp-Name: whoami", "Mcp-Name: =?base64?d2hvYW1p?=");
        let (head, _) = asked_with(&p, &mut no_programs(), &base64, &body);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(
            header_value("=?base64?SGVsbG8sIOS4lueVjA==?="),
            Some("Hello, 世界".into())
        );
        assert_eq!(header_value("=?base64?IHBhZGRlZCA=?="), Some(" padded ".into()));
        assert_eq!(header_value("=?base64?!!!!?="), None);
        assert_eq!(header_value("plain"), Some("plain".into()));
    }

    /// A revision it does not speak is refused with those it does; one missing its
    /// client's capabilities, or a method it lacks, as that revision says.
    #[test]
    fn what_the_current_revision_refuses() {
        let p = prepare_for_test();
        let (body, headers) = current("tools/list", "", "1900-01-01");
        let (head, said) = asked_with(&p, &mut no_programs(), &headers, &body);
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert_eq!(
            said,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2026-07-28","2025-11-25","2025-06-18","2025-03-26"],"requested":"1900-01-01"}}}"#
        );
        let (body, headers) = current("prompts/list", "", MODERN);
        let (head, said) = asked_with(&p, &mut no_programs(), &headers, &body);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        assert!(said.contains(r#""code":-32601"#), "{said}");
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let (head, said) = asked_with(
            &p,
            &mut no_programs(),
            "MCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list\r\n",
            body,
        );
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(said.contains(r#""code":-32602"#), "{said}");
    }

    /// The revisions before it are a session's, as before; a header naming none of them
    /// is refused. A browser's request is refused whatever it asks, and a method but POST.
    #[test]
    fn earlier_revisions_and_what_every_revision_refuses() {
        let p = prepare_for_test();
        let init =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#;
        let (head, said) = asked(&p, &mut no_programs(), None, init);
        assert!(head.contains("Mcp-Session-Id: "), "{head}");
        assert!(said.contains(r#""protocolVersion":"2025-11-25""#), "{said}");
        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let (head, _) = asked_with(
            &p,
            &mut no_programs(),
            "MCP-Protocol-Version: 2025-06-18\r\n",
            list,
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let (head, said) = asked_with(
            &p,
            &mut no_programs(),
            "MCP-Protocol-Version: 2024-11-05\r\n",
            list,
        );
        assert!(head.starts_with("HTTP/1.1 400"), "{head}");
        assert!(said.contains(r#""code":-32022"#), "{said}");
        let (head, said) = asked_with(&p, &mut no_programs(), "Origin: https://example.com\r\n", list);
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(!said.contains(r#""id""#), "{said}");
        let get = b"GET /mcp HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        let Some((Reply::Now(answer), _, _)) = request(&p, &mut [], &mut no_programs(), 1, get) else {
            panic!("no answer to GET");
        };
        let answer = String::from_utf8(answer).unwrap();
        assert!(
            answer.starts_with("HTTP/1.1 405") && answer.contains("\r\nAllow: POST\r\n"),
            "{answer}"
        );
    }

    /// A cancellation names a request within its session: another session's request of
    /// that ID runs on; its own is ended, its worker killed, and its connection told.
    #[test]
    fn a_cancellation_names_a_request_in_its_own_session() {
        let p = prepare_for_test();
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        helper.set_nonblocking(true).unwrap();
        let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
        programs
            .start("return 1;", None, 7, Some("s1"), "2".to_string(), false)
            .unwrap();
        programs
            .start("return 2;", None, 8, Some("s2"), "2".to_string(), false)
            .unwrap();
        let (run, _) = sandbox::recv_control(helper.as_raw_fd()).unwrap().unwrap();
        assert_eq!(run.first(), Some(&sandbox::RUN));
        let cancel = r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#;
        let (head, body) = asked(&p, &mut programs, Some("s3"), cancel);
        assert!(head.starts_with("HTTP/1.1 202"), "{head}");
        assert!(body.is_empty());
        assert_eq!(programs.running.len(), 2);
        assert!(programs.cancelled.is_empty());
        asked(&p, &mut programs, Some("s2"), cancel);
        let left: Vec<(u64, Option<&str>)> = programs
            .running
            .iter()
            .map(|prog| (prog.conn, prog.session.as_deref()))
            .collect();
        assert_eq!(left, [(7, Some("s1"))]);
        assert_eq!(programs.cancelled, [(8, "2".to_string())]);
        let mut stops = Vec::new();
        while let Ok(Some((msg, _))) = sandbox::recv_control(helper.as_raw_fd()) {
            if msg.first() == Some(&sandbox::STOP) {
                stops.push(msg);
            }
        }
        let mut want = vec![sandbox::STOP];
        want.extend_from_slice(&2u64.to_be_bytes());
        assert_eq!(stops, [want]);
    }

    #[test]
    fn an_exhausted_program_identity_cannot_release_another_worker() {
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        helper.set_nonblocking(true).unwrap();
        let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
        programs.next = u64::MAX;
        let result = programs.start("return 1;", None, 7, Some("owner"), "2".into(), false);
        let received = sandbox::recv_control(helper.as_raw_fd());
        let released = match &received {
            Ok(Some((bytes, passed))) => {
                eprintln!(
                    "actual control bytes={bytes:?} passed_descriptor={}",
                    passed.is_some()
                );
                true
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            other => panic!("unexpected native helper receipt: {other:?}"),
        };
        eprintln!(
            "start_refused={} worker_released={released} owned_programs={}",
            result.is_err(),
            programs.running.len()
        );
        assert!(
            !released,
            "an exhausted identity released a worker with a reused identity"
        );
        assert!(result.is_err());
        assert!(programs.running.is_empty());
        assert_eq!(programs.next, u64::MAX);
    }

    #[test]
    fn a_typed_program_refusal_formats_without_requesting_storage() {
        let failure = ProgramAdmission::Socket(io::Error::from_raw_os_error(libc::EMFILE).into());
        let mut storage = [0u8; 512]; // Caller-owned fixture display space, not a runtime bound.
        let mut out = io::Cursor::new(storage.as_mut_slice());
        let (result, allocations) =
            crate::program_admission_allocation::during(usize::MAX, || write!(out, "{failure}"));
        eprintln!(
            "actual_refusal_display_allocations={allocations} formatted={}",
            result.is_ok()
        );
        result.unwrap();
        assert_eq!(
            allocations, 0,
            "typed failure presentation requested heap storage"
        );
    }

    #[test]
    fn an_unusable_helper_control_is_not_admitted() {
        use std::os::unix::fs::OpenOptionsExt as _;
        let dir = shards_testdir::TempDir::new("helper-mode").unwrap();
        let control = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(&*dir)
            .unwrap();
        let programs = Programs::new(Ok(OwnedFd::from(control)));
        eprintln!("unusable_native_control_rejected={}", programs.control.is_err());
        assert!(
            programs.control.is_err(),
            "the native nonblocking setup error was discarded"
        );
    }

    #[test]
    fn a_program_is_reserved_before_its_helper_receives_authority() {
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        helper.set_nonblocking(true).unwrap();
        let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
        let prepared = programs
            .prepare("return 1;", None, 7, Some("owner"), "2".into(), false)
            .unwrap();
        assert_eq!(programs.next, 0);
        assert!(programs.running.is_empty());
        assert!(programs.running.capacity() > programs.running.len());
        assert!(programs.cancelled.capacity() > programs.running.len());
        assert_eq!(
            prepared.program.write,
            sandbox::frame(sandbox::PROGRAM, b"return 1;").unwrap()
        );
        assert!(
            matches!(sandbox::recv_control(helper.as_raw_fd()), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
        drop(prepared);
        let id = programs
            .start("return 2;", None, 7, Some("owner"), "3".into(), false)
            .unwrap();
        let (bytes, passed) = sandbox::recv_control(helper.as_raw_fd()).unwrap().unwrap();
        assert!(passed.is_some());
        assert_eq!(bytes, [vec![sandbox::RUN], id.to_be_bytes().to_vec()].concat());
        assert_eq!(programs.running.len(), 1);
    }

    #[test]
    fn a_refused_native_release_does_not_reuse_its_issued_identity() {
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
        drop(helper);
        assert!(matches!(
            programs.start("return 1;", None, 7, None, "1".into(), false),
            Err(ProgramAdmission::Release(_))
        ));
        assert_eq!(programs.next, 1);
        assert!(programs.running.is_empty());
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        programs.control = Ok(OwnedFd::from(instance));
        assert_eq!(
            programs
                .start("return 2;", None, 7, None, "2".into(), false)
                .unwrap(),
            2
        );
        let (bytes, passed) = sandbox::recv_control(helper.as_raw_fd()).unwrap().unwrap();
        assert_eq!(bytes, [vec![sandbox::RUN], 2u64.to_be_bytes().to_vec()].concat());
        assert!(passed.is_some());
    }

    #[test]
    fn an_expired_program_is_refused_before_native_release() {
        let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
        helper.set_nonblocking(true).unwrap();
        let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
        assert!(matches!(
            programs.start("return 1;", Some(0), 7, None, "1".into(), false),
            Err(ProgramAdmission::Expired)
        ));
        assert_eq!(programs.next, 0);
        assert!(programs.running.is_empty());
        assert!(
            matches!(sandbox::recv_control(helper.as_raw_fd()), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn each_refused_program_allocation_precedes_native_worker_release() {
        let attempt = |fail_at| {
            let (instance, helper) = std::os::unix::net::UnixDatagram::pair().unwrap();
            helper.set_nonblocking(true).unwrap();
            let mut programs = Programs::new(Ok(OwnedFd::from(instance)));
            let rpc = "2".to_string();
            let (result, count) = crate::program_admission_allocation::during(fail_at, || {
                programs.start("return 1;", None, 7, Some("owner"), rpc, false)
            });
            let received = sandbox::recv_control(helper.as_raw_fd());
            if fail_at == usize::MAX {
                assert!(result.is_ok());
                assert!(matches!(received, Ok(Some(_))));
            } else {
                eprintln!(
                    "refused_allocation={fail_at} attempts={count} typed_result={result:?} tracked={}",
                    programs.running.len()
                );
                assert!(matches!(
                    result,
                    Err(ProgramAdmission::Capacity | ProgramAdmission::Frame(sandbox::FrameFailure::Capacity))
                ));
                assert!(matches!(received, Err(e) if e.kind() == io::ErrorKind::WouldBlock));
                assert_eq!(programs.next, 0);
                assert!(programs.running.is_empty());
            }
            count
        };
        let count = attempt(usize::MAX);
        assert!(count > 0);
        for fail_at in 1..=count {
            attempt(fail_at);
        }
    }

    /// A program's call is made as its caller's MCP call is: its value, or why it failed.
    #[test]
    fn a_programs_call_is_its_callers() {
        let p = prepare_for_test();
        assert_eq!(
            call_for_program(&p, &mut [], br#"{"name":"whoami","args":{}}"#).as_str(),
            r#"{"value":"agent t"}"#
        );
        let refused = call_for_program(
            &p,
            &mut [],
            br#"{"name":"send","args":{"to":"agent x","text":"hi"}}"#,
        );
        let refused = json::parse(refused.as_bytes()).unwrap();
        assert_eq!(
            refused.get("error").and_then(Value::str),
            Some("agent t may not send agent x requests")
        );
        assert_eq!(
            refused
                .get("broker")
                .and_then(|v| v.get("cause"))
                .and_then(|v| v.get("kind"))
                .and_then(Value::str),
            Some("grant")
        );
        assert_eq!(
            call_for_program(&p, &mut [], br#"{"name":"spawn","args":{}}"#).as_str(),
            r#"{"error":"no tool spawn"}"#
        );
        assert_eq!(
            call_for_program(&p, &mut [], b"{").as_str(),
            r#"{"error":"a call that is no JSON"}"#
        );
    }

    #[test]
    fn broker_http_and_program_callers_share_the_actual_peer_admission() {
        let mut p = prepare_for_test();
        let mut fds = [-1; 2];
        assert_eq!(
            // SAFETY: socketpair writes the two supplied descriptors.
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: the successful descriptors are transferred here alone.
        let (mine, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let peer = |fd| Peer {
            label: "agent peer".into(),
            most: message_most(&fd).unwrap(),
            fd,
            send: true,
            answer: true,
            held: None,
            sent: vec![],
            last: 0,
            asked: vec![],
            gone: false,
        };
        let mut peers = [peer(mine)];
        let mut target = peer(theirs);
        let program = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"send","args":{"to":"agent peer","text":"program","principal":"forged"}}"#,
        );
        let program = json::parse(program.as_bytes()).unwrap();
        assert_eq!(
            program
                .get("value")
                .and_then(|v| v.get("id"))
                .and_then(Value::u64),
            Some(1)
        );
        assert_eq!(
            program
                .get("broker")
                .and_then(|v| v.get("principal"))
                .and_then(Value::str),
            Some("agent t")
        );
        take(&mut target);
        assert!(
            matches!(target.held.take(), Some(Message { id: 1, request: true, body: Body::Text(text) }) if text == "program")
        );
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"send","arguments":{"to":"agent peer","text":"http"}}}"#;
        let http = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let Some((Reply::Now(answer), _, _)) =
            request(&p, &mut peers, &mut no_programs(), 1, http.as_bytes())
        else {
            panic!("HTTP call did not answer");
        };
        let answer = String::from_utf8(answer).unwrap();
        assert!(answer.contains(r#""text":"{\"id\":2}""#), "{answer}");
        let body = answer.split_once("\r\n\r\n").unwrap().1;
        let body = json::parse(body.as_bytes()).unwrap();
        let metadata = body
            .get("result")
            .and_then(|v| v.get("structuredContent"))
            .unwrap();
        assert_eq!(metadata.get("operation").and_then(Value::str), Some("send"));
        assert_eq!(metadata.get("request").and_then(Value::u64), Some(2));
        assert_eq!(
            metadata
                .get("effect")
                .and_then(|v| v.get("kind"))
                .and_then(Value::str),
            Some("kernel_accepted")
        );
        assert_eq!(
            metadata
                .get("effect")
                .and_then(|v| v.get("bytes"))
                .and_then(Value::u64),
            Some((MESSAGE_HEAD + 4) as u64)
        );
        take(&mut target);
        assert!(
            matches!(target.held.take(), Some(Message { id: 2, request: true, body: Body::Text(text) }) if text == "http")
        );
        assert_eq!(peers[0].last, 2);
        assert_eq!(peers[0].sent, [1, 2]);

        let tmp = shards_testdir::TempDir::new("broker-call").unwrap();
        std::fs::write(tmp.join("out"), b"real-file").unwrap();
        p.scratch = Some(std::fs::File::open(&*tmp).unwrap().into());
        let reply = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"hand","args":{"to":"agent peer","path":"/tmp/out"}}"#,
        );
        let reply = json::parse(reply.as_bytes()).unwrap();
        assert_eq!(
            reply.get("value").and_then(|v| v.get("id")).and_then(Value::u64),
            Some(3)
        );
        assert_eq!(
            reply
                .get("broker")
                .and_then(|v| v.get("operation"))
                .and_then(Value::str),
            Some("hand_request")
        );
        take(&mut target);
        assert!(
            matches!(target.held.take(), Some(Message { id: 3, request: true, body: Body::File(name, bytes) }) if name == "out" && bytes == b"real-file")
        );

        peers[0].asked.push(9);
        target.sent.push(9);
        let reply = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"answer","args":{"to":"agent peer","text":"once","id":9}}"#,
        );
        let reply = json::parse(reply.as_bytes()).unwrap();
        assert!(matches!(reply.get("value"), Some(Value::Object(fields)) if fields.is_empty()));
        assert_eq!(
            reply
                .get("broker")
                .and_then(|v| v.get("request"))
                .and_then(Value::u64),
            Some(9)
        );
        take(&mut target);
        assert!(
            matches!(target.held.take(), Some(Message { id: 9, request: false, body: Body::Text(text) }) if text == "once")
        );
        let duplicate = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"answer","args":{"to":"agent peer","text":"twice","id":9}}"#,
        );
        assert!(duplicate.as_str().contains("has no request"));
        let malformed = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"hand","args":{"to":"agent peer","path":"/tmp/out","id":"wrong"}}"#,
        );
        assert!(malformed.as_str().contains("id is missing or has the wrong type"));
        take(&mut target);
        assert!(target.held.is_none());
        peers[0].last = u64::MAX;
        let exhausted = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"send","args":{"to":"agent peer","text":"no wrap"}}"#,
        );
        assert!(exhausted.as_str().contains("exhausted"));
        take(&mut target);
        assert!(target.held.is_none());
        assert_eq!(peers[0].last, u64::MAX);
        assert!(peers[0].asked.is_empty());
        eprintln!(
            "BROKER_CALLER program_id=1 http_id=2 hand_id=3 answer_id=9 duplicate=no_packet malformed=no_packet exhausted=no_packet local_kernel_acceptance_only=true"
        );
    }

    #[test]
    fn broker_exhausted_caller_identity_refuses_before_packet() {
        let p = prepare_for_test();
        let mut fds = [-1; 2];
        assert_eq!(
            // SAFETY: socketpair writes the two supplied descriptors.
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: the successful descriptors are transferred here alone.
        let (mine, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let peer = |fd| Peer {
            label: "agent peer".into(),
            most: message_most(&fd).unwrap(),
            fd,
            send: true,
            answer: true,
            held: None,
            sent: vec![],
            last: 0,
            asked: vec![],
            gone: false,
        };
        let mut peers = [peer(mine)];
        let mut target = peer(theirs);
        peers[0].last = u64::MAX;
        let reply = call_for_program(
            &p,
            &mut peers,
            br#"{"name":"send","args":{"to":"agent peer","text":"no wrap"}}"#,
        );
        take(&mut target);
        let observed_id = target.held.as_ref().map(|message| message.id);
        eprintln!(
            "BROKER_EXHAUSTED last={} actual_packet_id={observed_id:?} caller_reply={reply}",
            peers[0].last
        );
        assert_eq!(observed_id, None, "an exhausted identity emitted a packet");
        assert_eq!(peers[0].last, u64::MAX);
        assert!(reply.as_str().contains("exhausted"));
        assert!(peers[0].sent.is_empty());
    }

    fn prepare_for_test() -> Prepared {
        let d = shards_testdir::TempDir::new("server-bounds").unwrap();
        // SAFETY: getgid(2).
        prepare(d.to_str().unwrap(), "agent t", unsafe { libc::getgid() }).unwrap()
    }

    /// A control socket's two ends, as `spawn_helper` makes them: sequenced packets.
    fn control_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair(2) into a two-element array.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: both were just made, and are owned here alone.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    /// Whether `fd` is readable within `ms` milliseconds.
    fn readable(fd: &OwnedFd, ms: i32) -> bool {
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) of one pollfd of ours.
        unsafe { libc::poll(&raw mut p, 1, ms) == 1 }
    }

    /// An instance in `dir`, served on a thread of its own, code mode's helper played by the
    /// test: the helper's end of its control socket, and another handle on the instance's.
    fn served_with_helper(dir: &str) -> (OwnedFd, OwnedFd) {
        let (instance, helper) = control_pair();
        let also = instance.try_clone().unwrap();
        // SAFETY: getgid(2).
        let p = prepare(dir, "agent t", unsafe { libc::getgid() }).unwrap();
        let programs = Programs::new(Ok(instance));
        std::thread::spawn(move || serve(&p, &mut Vec::new(), programs));
        (helper, also)
    }

    /// A client of the instance in `dir`, with the certificate it issued its caller.
    fn client(dir: &str) -> rustls::StreamOwned<rustls::ClientConnection, UnixStream> {
        use rustls::pki_types::ServerName;
        use rustls::pki_types::pem::PemObject as _;
        let ca = CertificateDer::from_pem_file(format!("{dir}/ca.pem")).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let cert = CertificateDer::from_pem_file(format!("{dir}/cert.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_file(format!("{dir}/key.pem")).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)
            .unwrap();
        let tls =
            rustls::ClientConnection::new(Arc::new(config), ServerName::try_from(NAME).unwrap()).unwrap();
        let sock = UnixStream::connect(format!("{dir}/server.sock")).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        rustls::StreamOwned::new(tls, sock)
    }

    /// A `tools/call` of `run` with `arguments`, its head carrying `headers` (each ending CRLF).
    fn run_request(arguments: &str, headers: &str) -> String {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"run","arguments":{arguments}}}}}"#
        );
        format!(
            "POST /mcp HTTP/1.1\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// The helper's next control message, within five seconds, and the program ID it names.
    fn next_control(helper: &OwnedFd) -> (Vec<u8>, Option<OwnedFd>, u64) {
        assert!(readable(helper, 5000), "the helper was sent nothing");
        let (msg, fd) = sandbox::recv_control(helper.as_raw_fd()).unwrap().unwrap();
        let id = u64::from_be_bytes(msg[1..9].try_into().unwrap());
        (msg, fd, id)
    }

    /// The first frame a worker reads from its program's socket; None at the socket's end.
    fn first_frame(socket: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Ok(Some((kind, bytes, _))) = sandbox::parse_frame(&got) {
                return Some((kind, bytes.to_vec()));
            }
            match socket.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// One HTTP answer read from `client`, head and body, or what came before its end.
    fn response(client: &mut impl Read) -> String {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&got[..end]).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                if got.len() >= end + 4 + length {
                    break;
                }
            }
            match client.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    }

    /// Sends on `instance`, a handle on the instance's end of the control socket, until the
    /// socket takes no more: how many it took.
    fn fill(instance: &OwnedFd) -> usize {
        let mut taken = 0;
        loop {
            match sandbox::send_control(instance.as_raw_fd(), b"x", None) {
                Ok(()) => taken += 1,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return taken,
                Err(e) => panic!("filling the control socket: {e}"),
            }
        }
    }

    /// What the helper reads within five seconds, until a STOP: how many other messages,
    /// and the IDs its STOPs name.
    fn drain_until_stop(helper: &OwnedFd) -> (usize, Vec<u64>) {
        let (mut others, mut stops) = (0, Vec::new());
        let until = Instant::now() + Duration::from_secs(5);
        while stops.is_empty() && Instant::now() < until {
            if !readable(helper, 100) {
                continue;
            }
            let Some((msg, _)) = sandbox::recv_control(helper.as_raw_fd()).unwrap() else {
                break;
            };
            match msg.first() {
                Some(&sandbox::STOP) => stops.push(u64::from_be_bytes(msg[1..9].try_into().unwrap())),
                _ => others += 1,
            }
        }
        (others, stops)
    }

    /// A program's request that closes its connection (`Connection: close`, as a client
    /// without keep-alive sends each) is answered once the program ends, and then the
    /// connection closes; the program is not stopped for it.
    #[test]
    fn a_program_answers_a_request_that_closes_its_connection() {
        let tmp = shards_testdir::TempDir::new("close").unwrap();
        let dir = tmp.to_str().unwrap();
        let (helper, _instance) = served_with_helper(dir);
        let mut client = client(dir);
        let req = run_request(r#"{"code":"return 42;"}"#, "Connection: close\r\n");
        client.write_all(req.as_bytes()).unwrap();
        // The helper is asked to run it; its worker, played here, returns 42.
        let (run, program, id) = next_control(&helper);
        assert_eq!(run.first(), Some(&sandbox::RUN));
        let mut program = UnixStream::from(program.unwrap());
        program.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sent = first_frame(&mut program);
        let wrote = program.write_all(&sandbox::frame(sandbox::DONE, b"42").unwrap());
        let mut answer = Vec::new();
        let read = client.read_to_end(&mut answer);
        let answer = String::from_utf8_lossy(&answer).into_owned();
        let mut stops = Vec::new();
        while readable(&helper, 100) {
            let Some((msg, _)) = sandbox::recv_control(helper.as_raw_fd()).unwrap() else {
                break;
            };
            if msg.first() == Some(&sandbox::STOP) {
                stops.push(u64::from_be_bytes(msg[1..9].try_into().unwrap()));
            }
        }
        eprintln!(
            "CONNECTION_CLOSE program={id} program_frame_read={} worker_done_written={} client_read={read:?} answer_bytes={} stops={stops:?}",
            sent.is_some(),
            wrote.is_ok(),
            answer.len()
        );
        assert!(
            answer.contains(r#""text":"42""#),
            "no answer to the program's request: {answer:?}"
        );
        assert!(
            read.is_ok(),
            "the connection did not close after its answer: {read:?}"
        );
        assert!(stops.is_empty(), "its program was stopped: {stops:?}");
    }

    /// A program run past its time is answered as ended, and its worker's STOP reaches the
    /// helper though the helper's socket was full as it was due: once the helper has read
    /// what filled it.
    #[test]
    fn a_stop_the_helper_cannot_yet_take_reaches_it_once_it_can() {
        let tmp = shards_testdir::TempDir::new("stop").unwrap();
        let dir = tmp.to_str().unwrap();
        let (helper, instance) = served_with_helper(dir);
        let mut client = client(dir);
        let req = run_request(r#"{"code":"for (;;) {}","timeout_ms":500}"#, "");
        client.write_all(req.as_bytes()).unwrap();
        // Its worker, played here, runs on: its socket held, never answering.
        let (run, _program, id) = next_control(&helper);
        assert_eq!(run.first(), Some(&sandbox::RUN));
        let filled = fill(&instance);
        let answer = response(&mut client);
        let (drained, stops) = drain_until_stop(&helper);
        eprintln!(
            "LOST_STOP_DEADLINE program={id} control_messages_filling={filled} drained={drained} stops={stops:?} answered_ended={}",
            answer.contains("ran past its 500 ms, and was ended")
        );
        assert!(answer.contains("ran past its 500 ms, and was ended"), "{answer}");
        assert_eq!(
            stops,
            [id],
            "the worker answered as ended has no STOP: it runs on"
        );
    }

    /// A client gone ends the program it awaited: its worker's STOP reaches the helper though
    /// the helper's socket was full as the client went.
    #[test]
    fn a_gone_clients_program_is_stopped_though_the_helper_was_full() {
        let tmp = shards_testdir::TempDir::new("gone").unwrap();
        let dir = tmp.to_str().unwrap();
        let (helper, instance) = served_with_helper(dir);
        let mut client = client(dir);
        client
            .write_all(run_request(r#"{"code":"for (;;) {}"}"#, "").as_bytes())
            .unwrap();
        let (run, program, id) = next_control(&helper);
        assert_eq!(run.first(), Some(&sandbox::RUN));
        let mut program = UnixStream::from(program.unwrap());
        program.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sent = first_frame(&mut program);
        let filled = fill(&instance);
        drop(client);
        // The instance lets the program go: its end of the program's socket closes.
        let let_go = matches!(program.read(&mut [0u8; 1]), Ok(0));
        let (drained, stops) = drain_until_stop(&helper);
        eprintln!(
            "LOST_STOP_GONE program={id} program_frame_read={} control_messages_filling={filled} program_let_go={let_go} drained={drained} stops={stops:?}",
            sent.is_some()
        );
        assert!(let_go);
        assert_eq!(stops, [id], "the gone client's worker has no STOP: it runs on");
    }

    /// aws-lc-rs's provider offering `suites`, in that order: an agent's client.
    fn offering(suites: &[rustls::SupportedCipherSuite]) -> rustls::crypto::CryptoProvider {
        rustls::crypto::CryptoProvider {
            cipher_suites: suites.to_vec(),
            ..rustls::crypto::aws_lc_rs::default_provider()
        }
    }

    /// A handshake between `p`'s TLS and a client under `provider` presenting the certificate
    /// the instance in `dir` issued its caller, their records handed across in memory until
    /// neither has more to send: each side's connection, or the error that ended it.
    fn handshake(
        p: &Prepared,
        dir: &str,
        provider: rustls::crypto::CryptoProvider,
    ) -> (
        Result<rustls::Connection, rustls::Error>,
        Result<rustls::Connection, rustls::Error>,
    ) {
        use rustls::pki_types::ServerName;
        use rustls::pki_types::pem::PemObject as _;
        let ca = CertificateDer::from_pem_file(format!("{dir}/ca.pem")).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let cert = CertificateDer::from_pem_file(format!("{dir}/cert.pem")).unwrap();
        let key = PrivateKeyDer::from_pem_file(format!("{dir}/key.pem")).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)
            .unwrap();
        let name = ServerName::try_from(NAME).unwrap();
        let client = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        let mut client = rustls::Connection::Client(client);
        let server = rustls::ServerConnection::new(p.config.clone()).unwrap();
        let mut server = rustls::Connection::Server(server);
        let (mut server_ended, mut client_ended) = (None, None);
        for _ in 0..16 {
            let sent = pass(&mut client, &mut server, &mut server_ended);
            let answered = pass(&mut server, &mut client, &mut client_ended);
            if !sent && !answered {
                break;
            }
        }
        (
            server_ended.map_or(Ok(server), Err),
            client_ended.map_or(Ok(client), Err),
        )
    }

    /// Hands what `from` has to send to `to`; whether there was any. What ends `to` as it
    /// reads is kept in `ended`.
    fn pass(
        from: &mut rustls::Connection,
        to: &mut rustls::Connection,
        ended: &mut Option<rustls::Error>,
    ) -> bool {
        let mut bytes = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut bytes).unwrap();
        }
        let mut rest = bytes.as_slice();
        while !rest.is_empty() && ended.is_none() {
            to.read_tls(&mut rest).unwrap();
            if let Err(e) = to.process_new_packets() {
                *ended = Some(e);
            }
        }
        !bytes.is_empty()
    }

    fn suite(side: &rustls::Connection) -> Option<rustls::CipherSuite> {
        side.negotiated_cipher_suite().map(|s| s.suite())
    }

    /// The instance refused a client offering `only` for want of a suite in common, and the
    /// client was told so by TLS's handshake_failure (RFC 8446 §4.1.1).
    fn assert_refused_for_its_cipher(only: rustls::SupportedCipherSuite) {
        let tmp = shards_testdir::TempDir::new("cipher").unwrap();
        let dir = tmp.to_str().unwrap();
        // SAFETY: getgid(2).
        let p = prepare(dir, "agent t", unsafe { libc::getgid() }).unwrap();
        match handshake(&p, dir, offering(&[only])) {
            (
                Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::NoCipherSuitesInCommon)),
                Err(rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure)),
            ) => {}
            (server, client) => panic!(
                "the instance: {:?}, its client: {:?}",
                server.as_ref().map(suite),
                client.as_ref().map(suite)
            ),
        }
    }

    /// An instance and its caller are TLS13_AES_256_GCM_SHA384 at both ends, whatever order
    /// the caller's client prefers: aws-lc-rs's, AES-256-GCM first, and Go's crypto/tls with
    /// AES-GCM hardware, AES-128-GCM first, and without, ChaCha20-Poly1305 first (go1.26.1
    /// src/crypto/tls/defaults.go), whose first an instance chose before.
    #[test]
    fn an_instance_and_its_caller_are_aes_256_gcm_at_both_ends() {
        let tmp = shards_testdir::TempDir::new("cipher").unwrap();
        let dir = tmp.to_str().unwrap();
        // SAFETY: getgid(2).
        let p = prepare(dir, "agent t", unsafe { libc::getgid() }).unwrap();
        let (aes_256, aes_128, chacha20) = (
            TLS13_AES_256_GCM_SHA384,
            TLS13_AES_128_GCM_SHA256,
            TLS13_CHACHA20_POLY1305_SHA256,
        );
        for order in [
            [aes_256, aes_128, chacha20],
            [aes_128, aes_256, chacha20],
            [chacha20, aes_128, aes_256],
        ] {
            let (server, client) = handshake(&p, dir, offering(&order));
            let (server, client) = (server.unwrap(), client.unwrap());
            assert!(!server.is_handshaking() && !client.is_handshaking());
            let aes_256 = Some(rustls::CipherSuite::TLS13_AES_256_GCM_SHA384);
            assert_eq!(suite(&server), aes_256, "{order:?}");
            assert_eq!(suite(&client), aes_256, "{order:?}");
        }
    }

    /// A client limited to ChaCha20-Poly1305, which an instance answered before, is refused.
    #[test]
    fn a_client_limited_to_chacha20_poly1305_is_refused() {
        assert_refused_for_its_cipher(TLS13_CHACHA20_POLY1305_SHA256);
    }

    /// A client limited to AES-128-GCM, the one suite every TLS 1.3 peer must implement (RFC
    /// 8446 §9.1), which an instance answered before, is refused.
    #[test]
    fn a_client_limited_to_aes_128_gcm_is_refused() {
        assert_refused_for_its_cipher(TLS13_AES_128_GCM_SHA256);
    }
}
