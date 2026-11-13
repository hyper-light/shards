//! The agent-control endpoint (plan step 11, D238): one QUIC connection from a microVM's
//! controller (kiln, Hex) to the microVM, its lanes the streams the controller opens on it.
//!
//! - **Who reaches it.** TLS 1.3 under [`tls`]'s provider, TLS13_AES_256_GCM_SHA384 alone both
//!   ways, early data off. The endpoint presents the microVM's own key, an Ed25519 key made where
//!   TLS ends that never leaves there (RFC 7250's raw public key, as D180's homes present
//!   theirs), and takes only the key its controller enrolled; the controller takes only the key
//!   it pinned for that microVM. Either side refuses any other key at the handshake, TLS's
//!   access_denied (RFC 8446 §6.2, the alert this crate's verifiers raise), before QUIC gives
//!   the controller any stream the endpoint would read (RFC 9001 §5.7: no 1-RTT packet of the
//!   client's is processed before the handshake completes): a peer without the enrolled key
//!   reaches nothing.
//! - **Its lanes.** Each lane is a bidirectional stream the controller opens, named by its
//!   first frame ([`lane`]); the endpoint opens none. A member's stdin, stdout and stderr are a
//!   lane each, opened by step 5's subscription, byte for byte (`managed::subscribe`,
//!   `managed::input::subscribe`), and carrying step 5's frames after it, so that a stream
//!   stalled on one member's reader holds up no other lane (RFC 9000 §2.2, §4.1). A lane the
//!   endpoint will not serve is refused alone ([`refused`]): the connection and its other
//!   lanes go on.
//! - **Its controller's side** ([`Controller`], `drive.rs`): the connection kiln or Hex dials and
//!   the lanes it opens, step 5's client of each stream, so that neither writes this wire again.

use std::mem::size_of;
use std::net::SocketAddr;

use hyper_quic::rustls::AlertDescription;
use hyper_quic::{
    ClientConfig, ConnectionError, ServerConfig, StreamId, TransportConfig, TransportErrorCode,
};
use shards_abi::run::kind;
use shards_abi::run::managed::Channel;

use crate::frame::{self, Tag};
use crate::identity::{self, Fingerprint, Identity};
use crate::managed::{
    self, AdmissionError, Admitted, Gone, Producer, ProtocolError, Reason, RunId, Scope, Stop, Windows, input,
};
use crate::net::{Close, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use crate::tls;

mod drive;
pub mod exec;
pub mod unix;
pub use crate::tls::Negotiated;
pub use drive::{
    Controller, Dialed, Exec, ExecControl, Execs, Fault, Got, Hangup, Input, Link, Linked, McpConnection,
    Output, Socket, SocketReader, SocketWriter, Sockets, Subscriptions,
};
pub use unix::Admission as SocketAdmission;

/// The application protocol of a control connection (RFC 7301), apart from a context's
/// (`shards/1`) and the managed output's: a peer offering another is refused at its handshake.
pub const ALPN: &[u8] = b"shards-control/1";

/// The endpoint's side: the microVM's own key presented, and `controller`'s, the key its
/// controller enrolled, the only one taken (D180's `owner_server`).
pub fn endpoint(
    vm: &Identity,
    controller: Fingerprint,
    transport: TransportConfig,
) -> Result<ServerConfig, crate::Error> {
    tls::owner_server(vm, controller, transport, ALPN)
}

/// The controller's side: its own key presented, and the microVM's only where its key's SHA-256
/// is `vm`, the one the controller pinned for it.
pub fn controller(
    own: &Identity,
    vm: Fingerprint,
    transport: TransportConfig,
) -> Result<ClientConfig, crate::Error> {
    tls::client_with(own, vm, transport, ALPN)
}

/// Which side ended a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// This side.
    This,
    /// Its peer.
    Peer,
}

/// Why a control connection ended, read from the codes QUIC carries (RFC 9000 §20, RFC 9001
/// §4.8, which puts a TLS alert in CRYPTO_ERROR's range): what callers match, never its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// A key that is not the one enrolled or pinned: access_denied, which this crate's verifiers
    /// raise for any key but the pinned one (hyper-tls error.rs: `ApplicationVerificationFailure`).
    NotEnrolled(By),
    /// Nothing in common at the handshake, TLS13_AES_256_GCM_SHA384 among the rest
    /// (handshake_failure, RFC 8446 §4.1.1).
    Handshake(By),
    /// Another application protocol (no_application_protocol, RFC 7301 §3.2).
    Protocol(By),
    /// Closed with a reason of the endpoint's protocol.
    Closed(By, Reason),
    /// Its peer unheard for its idle timeout (RFC 9000 §10.1).
    Silent,
    /// Closed by this side ([`hyper_quic::ConnectionError::LocallyClosed`]).
    Local,
    /// Anything else its transport said: a code of no meaning here, a reset, a version.
    Transport(By),
}

/// What `error` says of why its connection ended.
pub fn ended(error: &ConnectionError) -> Ended {
    let (code, by) = match error {
        ConnectionError::TransportError(e) => (e.code, By::This),
        ConnectionError::ConnectionClosed(close) => (close.error_code, By::Peer),
        ConnectionError::ApplicationClosed(close) => {
            return u32::try_from(close.error_code.into_inner())
                .ok()
                .and_then(|code| Reason::try_from(code).ok())
                .map_or(Ended::Transport(By::Peer), |reason| {
                    Ended::Closed(By::Peer, reason)
                });
        }
        ConnectionError::TimedOut => return Ended::Silent,
        ConnectionError::LocallyClosed => return Ended::Local,
        ConnectionError::VersionMismatch | ConnectionError::Reset => return Ended::Transport(By::Peer),
        ConnectionError::CidsExhausted => return Ended::Transport(By::This),
    };
    let alert = |alert: AlertDescription| TransportErrorCode::crypto(alert.into());
    if code == alert(AlertDescription::AccessDenied) {
        Ended::NotEnrolled(by)
    } else if code == alert(AlertDescription::HandshakeFailure) {
        Ended::Handshake(by)
    } else if code == alert(AlertDescription::NoApplicationProtocol) {
        Ended::Protocol(by)
    } else {
        Ended::Transport(by)
    }
}

/// A member's stream a lane carries: the run protocol's channel numbers (abi `run::kind`), as
/// step 5's subscriptions name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stdio {
    Stdin = kind::STDIN,
    Stdout = kind::STDOUT,
    Stderr = kind::STDERR,
}

/// What a lane is for, as its first frame says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// A member's stdin, stdout or stderr, from `offset`: the first byte the controller has not
    /// seen taken (stdin) or consumed (output), as step 5's subscription says it.
    Stdio {
        run: RunId,
        generation: u64,
        member: u32,
        stream: Stdio,
        offset: u64,
    },
}

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
/// Step 5's subscription after its frame head: the run (32), its generation (u64), the member
/// (u32), the channel (u8) and the offset (u64).
const SUBSCRIPTION: usize =
    size_of::<RunId>() + size_of::<u64>() + size_of::<u32>() + size_of::<u8>() + size_of::<u64>();
/// The most a lane's first frame takes: the endpoint reads no more of a lane before it knows
/// what the lane is.
pub const LANE_MOST: usize = FRAME_HEAD + SUBSCRIPTION;

/// The lane the frame at the front of `bytes` opens, and how many bytes that frame took; none
/// until it has come whole. A frame of another kind or length, or of a channel no member has,
/// is the controller's error ([`Reason::Protocol`]).
pub fn lane(bytes: &[u8]) -> Result<Option<(Lane, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    if tag != Tag::Exec || len != SUBSCRIPTION {
        return Err(Reason::Protocol);
    }
    let Some(payload) = bytes.get(FRAME_HEAD..LANE_MOST) else {
        return Ok(None);
    };
    let (run, rest) = payload.split_first_chunk::<32>().ok_or(Reason::Protocol)?;
    let (generation, rest) = rest.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
    let (member, rest) = rest.split_first_chunk::<4>().ok_or(Reason::Protocol)?;
    let (channel, rest) = rest.split_first().ok_or(Reason::Protocol)?;
    let offset = rest.first_chunk::<8>().ok_or(Reason::Protocol)?;
    let stream = match *channel {
        kind::STDIN => Stdio::Stdin,
        kind::STDOUT => Stdio::Stdout,
        kind::STDERR => Stdio::Stderr,
        _ => return Err(Reason::Protocol),
    };
    let lane = Lane::Stdio {
        run: RunId(*run),
        generation: u64::from_be_bytes(*generation),
        member: u32::from_be_bytes(*member),
        stream,
        offset: u64::from_be_bytes(*offset),
    };
    Ok(Some((lane, LANE_MOST)))
}

/// The endpoint's refusal of one lane, whole: a `Refused` frame of its reason's code (a
/// big-endian u32, `managed::Reason`'s), then the lane's end. The connection and its other lanes
/// go on.
pub const REFUSED_BYTES: usize = FRAME_HEAD + size_of::<u32>();

/// `reason`'s refusal frame.
pub fn refused(reason: Reason) -> [u8; REFUSED_BYTES] {
    let mut out = [0; REFUSED_BYTES];
    let code = (reason as u32).to_be_bytes();
    let len = (size_of::<u32>() as u32).to_be_bytes();
    for (slot, byte) in out
        .iter_mut()
        .zip(std::iter::once(&(Tag::Refused as u8)).chain(&len).chain(&code))
    {
        *slot = *byte;
    }
    out
}

/// The control lane's ask, which opens it: its tag alone (`Tag::Control`).
const CONTROL_ASK: [u8; FRAME_HEAD] = [Tag::Control as u8, 0, 0, 0, 0];

/// The endpoint's answer on the control lane, whole: a `Negotiated` frame of its own TLS's
/// suite and group (u16 each, [`Negotiated`]), then the lane's end.
const NEGOTIATED_BYTES: usize = FRAME_HEAD + 2 * size_of::<u16>();

/// The most an answer that ends a lane takes: a refusal or the control lane's report.
const ANSWER_MOST: usize = if REFUSED_BYTES > NEGOTIATED_BYTES {
    REFUSED_BYTES
} else {
    NEGOTIATED_BYTES
};

/// `negotiated`'s report frame.
fn negotiated_frame(negotiated: Negotiated) -> [u8; NEGOTIATED_BYTES] {
    let mut out = [0; NEGOTIATED_BYTES];
    let len = ((2 * size_of::<u16>()) as u32).to_be_bytes();
    let (suite, group) = (negotiated.suite.to_be_bytes(), negotiated.group.to_be_bytes());
    for (slot, byte) in out.iter_mut().zip(
        std::iter::once(&(Tag::Negotiated as u8))
            .chain(&len)
            .chain(&suite)
            .chain(&group),
    ) {
        *slot = *byte;
    }
    out
}

/// The endpoint's report at the front of `bytes`, once it has come whole; none before. Another
/// frame there is no report: `Err` with what it is, as [`read_refused`] has it.
fn read_negotiated(bytes: &[u8]) -> Result<Option<Negotiated>, Option<Tag>> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| None)? else {
        return Ok(None);
    };
    if tag != Tag::Negotiated || len != 2 * size_of::<u16>() {
        return Err(Some(tag));
    }
    let Some((suite, group)) = bytes
        .get(FRAME_HEAD..NEGOTIATED_BYTES)
        .and_then(|b| b.split_first_chunk::<2>())
        .and_then(|(suite, rest)| Some((*suite, *rest.first_chunk::<2>()?)))
    else {
        return Ok(None);
    };
    Ok(Some(Negotiated {
        suite: u16::from_be_bytes(suite),
        group: u16::from_be_bytes(group),
    }))
}

/// The reason of the refusal frame at the front of `bytes`, once it has come whole; none
/// before. Another frame there is no refusal: `Err` with what it is, for the lane's own reader.
pub fn read_refused(bytes: &[u8]) -> Result<Option<Reason>, Option<Tag>> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| None)? else {
        return Ok(None);
    };
    if tag != Tag::Refused || len != size_of::<u32>() {
        return Err(Some(tag));
    }
    let Some(code) = bytes
        .get(FRAME_HEAD..REFUSED_BYTES)
        .and_then(|c| <[u8; 4]>::try_from(c).ok())
    else {
        return Ok(None);
    };
    Reason::try_from(u32::from_be_bytes(code))
        .map(Some)
        .map_err(|_| None)
}

/// How many connections of its controller the endpoint holds: the one it serves, and the one
/// that replaces it. A newer connection's handshake closes the older ([`Lanes::connected`]),
/// which the driver keeps until its close has drained (RFC 9000 §10.2); a third, until then,
/// is refused at its first packet, and its controller dials again.
const CONNECTIONS: usize = 2;

/// What one microVM's endpoint serves: each member stream, with what its caller admitted for
/// it, as for step 5's endpoint of one stream (`managed::Endpoint`, `managed::input::Endpoint`).
#[derive(Debug, Clone, Copy)]
pub struct Streams<'a> {
    pub outputs: &'a [(Scope, Admitted)],
    pub inputs: &'a [(input::Scope, Admitted)],
    /// What each exec's streams are admitted, where its host runs execs in the members
    /// ([`exec`]); none, and an ask is refused.
    pub execs: Option<exec::Admissions>,
    /// What the run admits of lanes to its own command's sockets ([`unix`], D251); none, and
    /// a socket lane is refused.
    pub sockets: Option<unix::Admission>,
}

/// The endpoint's handles to what it serves, in the order [`Streams`] named them: each output's
/// producer and control, each input's relay and control, as step 5's endpoints give them. A
/// stream's control revokes its lane alone.
#[derive(Debug)]
pub struct Served {
    pub outputs: Vec<(Producer, managed::Control)>,
    pub inputs: Vec<(input::Relay, input::Control)>,
    pub revoke: Revoke,
    /// Each exec a controller asks for, for the host to start ([`exec::Start`]), where
    /// [`Streams::execs`] admitted them.
    pub execs: Option<std::sync::mpsc::Receiver<exec::Start>>,
    /// Each socket lane a controller asks for, for the host to connect ([`unix::Connect`]),
    /// where [`Streams::sockets`] admitted them.
    pub sockets: Option<std::sync::mpsc::Receiver<unix::Connect>>,
}

/// Ends the endpoint's driver, its connections and every lane: the microVM's control is over.
/// Dropping it does so too.
#[derive(Debug)]
pub struct Revoke {
    sender: Sender<()>,
}

impl Revoke {
    pub fn revoke(&self) {
        self.sender.request_stop();
    }
}

impl Drop for Revoke {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// One microVM's agent-control endpoint: its controller's lanes to its members' stdio, on one
/// connection at a time. Bound before the members start; run its driver on a thread of its
/// own, and keep its [`Revoke`].
pub struct Endpoint {
    driver: Driver<Lanes>,
    pub address: SocketAddr,
    /// The SHA-256 of the microVM's key, which its controller pins.
    pub fingerprint: Fingerprint,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

impl Endpoint {
    /// Binds at `at` the endpoint of the microVM whose key is `vm`, which `controller` alone
    /// reaches, serving `streams`. Every value of its transport is its streams' (D238): each
    /// lane's windows as its endpoint of one stream's, summed, the connection's patience the
    /// least they admitted and its handshake and datagrams the most.
    pub fn bind(
        vm: &Identity,
        controller: Fingerprint,
        at: SocketAddr,
        streams: Streams<'_>,
    ) -> Result<(Endpoint, Served), managed::Error> {
        let (link, mut windows) = admitted(streams)?;
        let window = || managed::Error::Admission(AdmissionError::Window);
        // The control lane its controller opens on each connection: its ask read through a
        // stream's window, and its report or a refusal sent.
        windows.streams = windows.streams.checked_add(1).ok_or_else(window)?;
        windows.stream = windows.stream.max(CONTROL_ASK.len());
        windows.connection = windows
            .connection
            .checked_add(CONTROL_ASK.len())
            .ok_or_else(window)?;
        windows.send = windows
            .send
            .checked_add(u64::try_from(NEGOTIATED_BYTES + REFUSED_BYTES).map_err(|_| window())?)
            .ok_or_else(window)?;
        if streams.execs.is_some() {
            // One ask's lane at a time, read through a stream's window, and what it is told.
            windows.streams = windows.streams.checked_add(1).ok_or_else(window)?;
            windows.connection = windows
                .connection
                .checked_add(windows.stream)
                .ok_or_else(window)?;
            windows.send = windows
                .send
                .checked_add(u64::try_from(exec::TOLD_BYTES + REFUSED_BYTES).map_err(|_| window())?)
                .ok_or_else(window)?;
        }
        if let Some(admission) = streams.sockets {
            // Each socket lane, and one more to refuse past the run's most, with what each
            // reads and sends (unix.rs `added`); and each lane's window through one stream's.
            let window = || managed::Error::Admission(AdmissionError::Window);
            let (lanes, receive, send) = unix::added(admission.check()?).ok_or_else(window)?;
            windows.streams = windows.streams.checked_add(lanes).ok_or_else(window)?;
            windows.connection = usize::try_from(receive)
                .ok()
                .and_then(|r| windows.connection.checked_add(r))
                .ok_or_else(window)?;
            windows.send = windows.send.checked_add(send).ok_or_else(window)?;
            windows.stream = usize::try_from(admission.window)
                .ok()
                .map(|w| windows.stream.max(w).max(unix::ASK_MOST))
                .ok_or_else(window)?;
        }
        let mut config = endpoint(vm, controller, managed::transport_of(&link, windows)?)?;
        config
            .max_incoming(1)
            .incoming_buffer_size(0)
            .incoming_buffer_size_total(0);
        let base = exec::Limits {
            streams: u64::from(windows.streams),
            receive: u64::try_from(windows.connection).unwrap_or(u64::MAX),
            send: windows.send,
        };
        let (execs, handed) = exec::Execs::new(streams.execs, base);
        let (sockets, connects) = unix::Carried::new(streams.sockets);
        let lanes = Lanes::new(controller, streams, windows.streams, execs, sockets)?;
        let (mut driver, sender) = Driver::bind_bounded(
            at,
            Some(config),
            lanes,
            DriverLimits {
                connections: CONNECTIONS,
                // A wake is the same unit whoever sends it, and its turn serves every stream
                // (Lanes::turn): one pending covers them all.
                command_slots: 1,
                queued_datagram_bytes: link.datagram_bytes,
                handshake_patience: link.patience,
            },
        )?;
        let address = driver.local()?;
        let lanes = driver.handler_mut();
        lanes.execs.wake = Some(sender.try_clone()?);
        lanes.sockets.wake = Some(sender.try_clone()?);
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(lanes.outputs.len())
            .map_err(|_| managed::Error::Admission(AdmissionError::Allocation))?;
        for output in &lanes.outputs {
            outputs.push(output.handles(sender.try_clone()?, Stop::Lane)?);
        }
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(lanes.inputs.len())
            .map_err(|_| managed::Error::Admission(AdmissionError::Allocation))?;
        for input in &lanes.inputs {
            inputs.push(input.handles(sender.try_clone()?, Stop::Lane)?);
        }
        Ok((
            Endpoint {
                driver,
                address,
                fingerprint: vm.fingerprint(),
            },
            Served {
                outputs,
                inputs,
                revoke: Revoke { sender },
                execs: handed,
                sockets: connects,
            },
        ))
    }

    /// The kernel's buffers of its socket, which its caller's budget holds, as for step 5's
    /// endpoints (`managed::Endpoint::kernel_buffer_bytes`).
    pub fn kernel_buffer_bytes(&self) -> Result<u64, managed::Error> {
        self.driver
            .kernel_buffer_bytes()
            .map_err(managed::Error::Transport)
    }

    /// Serves until revoked; then every output not finished is aborted and every input not
    /// ended let go, as step 5's endpoints leave theirs.
    pub fn run(mut self) -> Result<(), managed::Error> {
        let result = self.driver.run();
        let lanes = self.driver.handler_mut();
        for output in &mut lanes.outputs {
            output.close();
        }
        for input in &mut lanes.inputs {
            input.close();
        }
        result.map_err(managed::Error::Transport)
    }
}

/// The connection's admission and windows from its streams': each stream's own admission
/// checked as its endpoint of one stream checks it; the most its controller sends on a lane
/// before it is read, each lane's own and all of them, as each endpoint of one stream bounds
/// its owner's (an output's subscription and acknowledgment, an input's record and its end);
/// what the endpoint sends unacknowledged, each lane's admitted window and a refusal's frame
/// for each stream the controller may open; one stream each.
fn admitted(streams: Streams<'_>) -> Result<(Admitted, Windows), managed::Error> {
    let window = || managed::Error::Admission(AdmissionError::Window);
    let mut link: Option<Admitted> = None;
    let mut windows = Windows {
        stream: 0,
        connection: 0,
        send: 0,
        streams: 0,
    };
    let lanes = streams
        .outputs
        .iter()
        .map(|(_, admitted)| Ok((admitted, managed::CONTROL_BYTES)))
        .chain(streams.inputs.iter().map(|(_, admitted)| {
            input::receive_bytes(admitted.record_bytes).map(|receive| (admitted, receive))
        }));
    for lane in lanes {
        let (admitted, receive) = lane?;
        managed::admissible(admitted)?;
        windows.stream = windows.stream.max(receive);
        windows.connection = windows.connection.checked_add(receive).ok_or_else(window)?;
        windows.send = windows
            .send
            .checked_add(admitted.transport_bytes)
            .and_then(|n| n.checked_add(REFUSED_BYTES as u64))
            .ok_or_else(window)?;
        windows.streams = windows.streams.checked_add(1).ok_or_else(window)?;
        link = Some(match link {
            None => *admitted,
            Some(link) => Admitted {
                handshake_bytes: link.handshake_bytes.max(admitted.handshake_bytes),
                datagram_bytes: link.datagram_bytes.max(admitted.datagram_bytes),
                patience: link.patience.min(admitted.patience),
                ..link
            },
        });
    }
    // An endpoint of socket lanes alone takes their link (unix.rs `link`).
    let link = match link {
        Some(link) => link,
        None if streams.sockets.is_some() => unix::link(),
        None => return Err(managed::Error::Admission(AdmissionError::Capacity)),
    };
    Ok((link, windows))
}

/// A lane's first frame, as it comes.
#[derive(Debug, Clone, Copy)]
struct Opening {
    conn: ConnKey,
    stream: StreamId,
    head: [u8; LANE_MOST],
    len: usize,
}

/// A lane's last frame, its refusal or the control lane's report, as much of it as has gone.
#[derive(Debug, Clone, Copy)]
struct Answering {
    conn: ConnKey,
    stream: StreamId,
    frame: [u8; ANSWER_MOST],
    len: usize,
    written: usize,
}

/// Which stream a lane serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serves {
    Output(usize),
    Input(usize),
}

/// The endpoint's handler. A lane is `Opening` while its first frame comes, then subscribed
/// to its stream (step 5's states, one lane per stream), or `Answering` while its last frame
/// goes: a refusal, or the control lane's report. A stream no lane holds is read no more. Its
/// connections are its controller's alone, the newest last.
struct Lanes {
    controller: Fingerprint,
    outputs: Vec<managed::Service>,
    inputs: Vec<input::Service>,
    opening: Vec<Opening>,
    answering: Vec<Answering>,
    conns: Vec<ConnKey>,
    execs: exec::Execs,
    sockets: unix::Carried,
}

/// Abandons `stream` both ways: what this side would send is let go (RESET_STREAM) and its
/// peer is asked to send no more (STOP_SENDING).
fn end(io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
    io.stop(conn, stream);
    io.reset(conn, stream);
}

impl Lanes {
    /// Every stream once, each `Opening` and `Answering` held to what the controller's
    /// connections may have open at once (QUIC's stream limit, RFC 9000 §4.6), so that nothing
    /// grows as the endpoint serves.
    fn new(
        controller: Fingerprint,
        streams: Streams<'_>,
        open: u32,
        execs: exec::Execs,
        sockets: unix::Carried,
    ) -> Result<Lanes, managed::Error> {
        let allocation = managed::Error::Admission(AdmissionError::Allocation);
        let held = usize::try_from(open)
            .ok()
            .and_then(|n| n.checked_mul(CONNECTIONS))
            .ok_or(allocation)?;
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(streams.outputs.len())
            .map_err(|_| managed::Error::Admission(AdmissionError::Allocation))?;
        for (scope, admitted) in streams.outputs {
            if outputs.iter().any(|o: &managed::Service| {
                o.scope().member == scope.member && o.scope().channel == scope.channel
            }) {
                return Err(managed::Error::Protocol(ProtocolError::Scope));
            }
            outputs.push(managed::Service::new(*scope, controller, admitted.record_bytes)?);
        }
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(streams.inputs.len())
            .map_err(|_| managed::Error::Admission(AdmissionError::Allocation))?;
        for (scope, admitted) in streams.inputs {
            if inputs
                .iter()
                .any(|i: &input::Service| i.scope().member == scope.member)
            {
                return Err(managed::Error::Protocol(ProtocolError::Scope));
            }
            inputs.push(input::Service::new(*scope, controller, admitted.record_bytes)?);
        }
        let mut lanes = Lanes {
            controller,
            outputs,
            inputs,
            opening: Vec::new(),
            answering: Vec::new(),
            conns: Vec::new(),
            execs,
            sockets,
        };
        let reserve = |held: usize| {
            managed::Error::Admission(if held == 0 {
                AdmissionError::Capacity
            } else {
                AdmissionError::Allocation
            })
        };
        lanes.opening.try_reserve_exact(held).map_err(|_| reserve(held))?;
        lanes
            .answering
            .try_reserve_exact(held)
            .map_err(|_| reserve(held))?;
        lanes
            .conns
            .try_reserve_exact(CONNECTIONS)
            .map_err(|_| reserve(CONNECTIONS))?;
        Ok(lanes)
    }

    /// The stream the lane `(conn, stream)` serves.
    fn serves(&self, conn: ConnKey, stream: StreamId) -> Option<Serves> {
        let lane = Some((conn, stream));
        if let Some(at) = self.outputs.iter().position(|o| o.subscriber() == lane) {
            return Some(Serves::Output(at));
        }
        self.inputs
            .iter()
            .position(|i| i.subscriber() == lane)
            .map(Serves::Input)
    }

    /// `conn` is gone: its lanes leave their streams for the next subscription, as step 5's
    /// endpoints let a lost owner's go.
    fn forget(&mut self, conn: ConnKey) {
        for output in &mut self.outputs {
            if output.subscriber().is_some_and(|(c, _)| c == conn) {
                output.disconnect();
            }
        }
        for input in &mut self.inputs {
            if input.subscriber().is_some_and(|(c, _)| c == conn) {
                input.disconnect();
            }
        }
        self.opening.retain(|o| o.conn != conn);
        self.answering.retain(|r| r.conn != conn);
        self.conns.retain(|c| *c != conn);
        self.forget_execs(conn);
        self.forget_sockets(conn);
    }

    /// The controller sent what a lane's protocol refuses: its connection is closed with why,
    /// as step 5's endpoints close their owner's.
    fn refuse_connection(&mut self, io: &mut Io<'_>, conn: ConnKey, reason: Reason) {
        io.close(conn, reason as u32, "agent control: a lane's frame refused");
        self.forget(conn);
    }

    /// Refuses the lane `(conn, stream)` alone: its refusal frame, then its end; nothing more of
    /// it is read. The connection and its other lanes go on.
    fn refuse_lane(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId, reason: Reason) {
        self.answer(io, conn, stream, &refused(reason));
    }

    /// Answers the control lane `(conn, stream)` with what this side's own TLS negotiated, then
    /// its end: nothing more of it is read. A connection names both once its handshake is done,
    /// which every lane follows; one that names none has its lane abandoned.
    fn report(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        match io.negotiated(conn) {
            Some(negotiated) => self.answer(io, conn, stream, &negotiated_frame(negotiated)),
            None => end(io, conn, stream),
        }
    }

    /// Ends the lane `(conn, stream)` with `last`, at most [`ANSWER_MOST`] bytes, as far as its
    /// stream takes it now, and the rest as it has room ([`Lanes::turn`]); nothing more of it is
    /// read. One with no room held for it is reset.
    fn answer(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId, last: &[u8]) {
        io.stop(conn, stream);
        let mut frame = [0; ANSWER_MOST];
        let Some(slot) = frame.get_mut(..last.len()) else {
            io.reset(conn, stream);
            return;
        };
        slot.copy_from_slice(last);
        let mut answering = Answering {
            conn,
            stream,
            frame,
            len: last.len(),
            written: 0,
        };
        if self.answer_more(io, &mut answering) {
            return;
        }
        if self.answering.len() < self.answering.capacity() {
            self.answering.push(answering);
        } else {
            io.reset(conn, stream);
        }
    }

    /// Writes what is left of a lane's last frame; whether it has all gone, its stream finished.
    fn answer_more(&self, io: &mut Io<'_>, answering: &mut Answering) -> bool {
        let frame = answering.frame.get(..answering.len).unwrap_or_default();
        while let Some(rest) = frame.get(answering.written..).filter(|r| !r.is_empty()) {
            match io.write(answering.conn, answering.stream, rest) {
                Ok(0) => return false,
                Ok(n) => answering.written = answering.written.saturating_add(n),
                // The stream is gone: nothing more to say on it.
                Err(_) => return true,
            }
        }
        let _ = io.finish(answering.conn, answering.stream);
        true
    }

    /// Reads a lane's first frame as it comes, and subscribes the lane to the stream it names
    /// once it is whole, or refuses it.
    fn open(&mut self, io: &mut Io<'_>, at: usize) {
        let Some(opening) = self.opening.get_mut(at) else {
            return;
        };
        let (conn, stream) = (opening.conn, opening.stream);
        let room = LANE_MOST.saturating_sub(opening.len);
        let (bytes, ended) = match io.read_within(conn, stream, room) {
            Ok(Read::Bytes(bytes)) => (bytes, false),
            Ok(Read::End(bytes)) => (bytes, true),
            Ok(Read::Blocked) => return,
            Ok(Read::Reset) | Err(_) => {
                self.opening.swap_remove(at);
                end(io, conn, stream);
                return;
            }
        };
        let end_at = opening.len.saturating_add(bytes.len());
        let Some(slot) = opening.head.get_mut(opening.len..end_at) else {
            self.opening.swap_remove(at);
            self.refuse_lane(io, conn, stream, Reason::Protocol);
            return;
        };
        slot.copy_from_slice(&bytes);
        opening.len = end_at;
        let head = opening.head.get(..opening.len).unwrap_or_default();
        // The control lane's ask, its tag alone: answered with this side's report.
        if let Ok(Some((Tag::Control, len))) = frame::peek(head) {
            let asked = len == 0 && head.len() == CONTROL_ASK.len();
            self.opening.swap_remove(at);
            if asked {
                self.report(io, conn, stream);
            } else {
                self.refuse_lane(io, conn, stream, Reason::Protocol);
            }
            return;
        }
        // A socket lane's ask: read on as long as it is (unix.rs).
        if let Ok(Some((Tag::UnixConnect, len))) = frame::peek(head) {
            let mut came = [0; LANE_MOST];
            let came = came.get_mut(..head.len()).map(|c| {
                c.copy_from_slice(head);
                &*c
            });
            self.opening.swap_remove(at);
            self.begin_socket(io, conn, stream, came.unwrap_or_default(), len, ended);
            return;
        }
        // An exec's ask, or an MCP connection's (D254): read on as long as it is (exec.rs).
        if let Ok(Some((Tag::MemberExec | Tag::Mcp, len))) = frame::peek(head) {
            let mut came = [0; LANE_MOST];
            let came = came.get_mut(..head.len()).map(|c| {
                c.copy_from_slice(head);
                &*c
            });
            self.opening.swap_remove(at);
            self.begin_ask(io, conn, stream, came.unwrap_or_default(), len, ended);
            return;
        }
        let first = lane(opening.head.get(..opening.len).unwrap_or_default());
        if matches!(first, Ok(None)) && !ended {
            return;
        }
        self.opening.swap_remove(at);
        match first {
            // Its controller ended it before it said what it is for.
            Ok(None) => end(io, conn, stream),
            Ok(Some(_)) if ended => end(io, conn, stream),
            Ok(Some((first, _))) => match self.subscribe(conn, stream, first) {
                // What came after its first frame, and what it is to be sent.
                Ok(()) => self.readable(io, conn, stream),
                Err(reason) => self.refuse_lane(io, conn, stream, reason),
            },
            Err(reason) => self.refuse_lane(io, conn, stream, reason),
        }
    }

    /// Subscribes `(conn, stream)` to the member stream its first frame named.
    fn subscribe(&mut self, conn: ConnKey, stream: StreamId, first: Lane) -> Result<(), Reason> {
        let Lane::Stdio {
            run,
            generation,
            member,
            stream: which,
            offset,
        } = first;
        let channel = match which {
            Stdio::Stdin => {
                let input = self
                    .inputs
                    .iter_mut()
                    .find(|i| i.scope().run == run && i.scope().member == member)
                    .ok_or(Reason::Scope)?;
                let scope = input::Scope {
                    run,
                    generation,
                    member,
                };
                return input.lane(conn, stream, scope, offset);
            }
            Stdio::Stdout => Channel::Stdout,
            Stdio::Stderr => Channel::Stderr,
        };
        let output = self
            .outputs
            .iter_mut()
            .find(|o| o.scope().run == run && o.scope().member == member && o.scope().channel == channel)
            .ok_or(Reason::Scope)?;
        let scope = Scope {
            run,
            generation,
            member,
            channel,
        };
        output.lane(conn, stream, scope, offset)
    }

    /// A turn of the endpoint's own: a revoked stream's lane is abandoned alone, every other
    /// lane says what it has to, and a lane done (its output finished, its input's end told)
    /// leaves its stream for the next subscription. Every refusal goes on.
    fn turn(&mut self, io: &mut Io<'_>) {
        for output in &mut self.outputs {
            let Some((conn, stream)) = output.subscriber() else {
                continue;
            };
            if output.aborted() {
                output.disconnect();
                end(io, conn, stream);
                continue;
            }
            output.flush(io);
            // Its last record acknowledged, the flush finished the lane, and nothing more of it
            // is read: its controller's side is stopped too (STOP_SENDING), so that the stream
            // closes both ways and its controller may open another (RFC 9000 §3.5, §4.6).
            if output.finished() {
                output.disconnect();
                io.stop(conn, stream);
            }
        }
        for input in &mut self.inputs {
            let Some((conn, stream)) = input.subscriber() else {
                continue;
            };
            if input.revoked() {
                input.disconnect();
                end(io, conn, stream);
                continue;
            }
            input.flush(io);
            if input.said_its_end() {
                input.disconnect();
                io.stop(conn, stream);
            }
        }
        let mut answering = std::mem::take(&mut self.answering);
        answering.retain_mut(|r| !self.answer_more(io, r));
        self.answering = answering;
        // After the streams' own: an exec's finished outputs have ended their lanes before it
        // is let go.
        self.exec_turn(io);
        self.socket_turn(io);
    }

    /// The lane `(conn, stream)` serving `which` is over: its stream waits for its next
    /// subscription.
    fn leave(&mut self, which: Serves) {
        match which {
            Serves::Output(at) => {
                if let Some(output) = self.outputs.get_mut(at) {
                    output.disconnect();
                }
            }
            Serves::Input(at) => {
                if let Some(input) = self.inputs.get_mut(at) {
                    input.disconnect();
                }
            }
        }
    }
}

/// What a lane's reading leaves it: open; ended by its controller (its FIN or its reset), the
/// lane abandoned both ways; over, its input having ended, so that what its controller sends
/// is let go while the end is told; or refused, its controller having sent what the lane's
/// protocol refuses, which closes its connection as step 5's endpoints close their owner's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Open,
    Ended,
    Over,
    Refused(Reason),
}

/// What `read` leaves a lane whose input `over` has ended, or not. A frame refused once its
/// input has ended raced the end, which the member's init tells on its own: the controller
/// broke nothing, and the lane is over.
fn after(read: Result<(), Gone>, over: bool) -> After {
    match read {
        Err(Gone::Ended | Gone::Reset) => After::Ended,
        Ok(()) | Err(Gone::Refused(_)) if over => After::Over,
        Ok(()) => After::Open,
        Err(Gone::Refused(reason)) => After::Refused(reason),
    }
}

impl Handler for Lanes {
    type Command = ();

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        // The TLS handshake took no other key (`tls::owner_server`); this holds it so whatever
        // its configuration.
        if identity::fingerprint(peer) != self.controller {
            return Err(Close {
                code: Reason::Auth as u32,
                why: "agent control: not the enrolled controller",
            });
        }
        // Its TLS negotiated the suite the provider offers (D239), which its report says.
        if !io.negotiated(conn).is_some_and(|n| n.is_provided()) {
            return Err(Close {
                code: Reason::Auth as u32,
                why: "agent control: not TLS13_AES_256_GCM_SHA384",
            });
        }
        // Its newest connection is the one its controller uses: the older are let go, and the
        // streams their lanes held wait for the newer's.
        while let Some(old) = self.conns.pop() {
            io.close(
                old,
                Reason::Stale as u32,
                "agent control: superseded by its controller's newer connection",
            );
            self.forget(old);
        }
        self.conns.push(conn);
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if !self.conns.contains(&conn) || self.opening.len() >= self.opening.capacity() {
            end(io, conn, stream);
            return;
        }
        self.opening.push(Opening {
            conn,
            stream,
            head: [0; LANE_MOST],
            len: 0,
        });
        self.readable(io, conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some(at) = self
            .opening
            .iter()
            .position(|o| o.conn == conn && o.stream == stream)
        {
            self.open(io, at);
            return;
        }
        if self.read_socket(io, conn, stream) || self.read_exec(io, conn, stream) {
            self.turn(io);
            return;
        }
        let Some(which) = self.serves(conn, stream) else {
            // A lane refused or done: what still comes of it is not read.
            io.stop(conn, stream);
            return;
        };
        let read = match which {
            Serves::Output(at) => self
                .outputs
                .get_mut(at)
                .map(|output| after(output.read(io, conn, stream), false)),
            Serves::Input(at) => self.inputs.get_mut(at).map(|input| {
                let read = input.read(io, conn, stream);
                after(read, input.ended())
            }),
        };
        match read {
            Some(After::Open) | None => {}
            Some(After::Over) => io.stop(conn, stream),
            Some(After::Ended) => {
                self.leave(which);
                end(io, conn, stream);
            }
            Some(After::Refused(reason)) => {
                self.refuse_connection(io, conn, reason);
                return;
            }
        }
        self.turn(io);
    }

    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.turn(io);
    }

    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if self.stop_socket(io, conn, stream) || self.stop_exec(io, conn, stream) {
            return;
        }
        // Its controller reads no more of the lane, whose side here the driver has reset: the
        // stream waits for its next subscription, and nothing more of the lane is read.
        match self.serves(conn, stream) {
            Some(which) => self.leave(which),
            None => {
                self.opening.retain(|o| o.conn != conn || o.stream != stream);
                self.answering.retain(|r| r.conn != conn || r.stream != stream);
            }
        }
        io.stop(conn, stream);
    }

    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        self.forget(conn);
    }

    fn command(&mut self, io: &mut Io<'_>, (): ()) {
        self.turn(io);
    }

    fn done(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use hyper_quic::rustls::CipherSuite;

    use super::*;
    use crate::handshake::{Side, connect};

    /// How long a test waits for what it awaits: a patience of its own, shorter than
    /// [`tls::QUIET`].
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

    fn transport(serves: bool) -> TransportConfig {
        let mut transport = TransportConfig::default();
        transport.max_concurrent_uni_streams(hyper_quic::VarInt::from_u32(0));
        if !serves {
            transport.max_concurrent_bidi_streams(hyper_quic::VarInt::from_u32(0));
        }
        transport
    }

    /// A controller under `own` dialing an endpoint of `vm` that enrolled `enrolled`, the
    /// controller having pinned `pinned`.
    fn dial(vm: &Identity, enrolled: Fingerprint, own: &Identity, pinned: Fingerprint) -> (Side, Side) {
        connect(
            endpoint(vm, enrolled, transport(true)).unwrap(),
            controller(own, pinned, transport(false)).unwrap(),
        )
    }

    /// Why each side's connection ended, as [`ended`] reads it.
    fn why(side: &Side) -> Ended {
        ended(side.lost.as_ref().unwrap_or_else(|| panic!("still connected")))
    }

    /// The controller the microVM enrolled reaches it, and the microVM its controller pinned:
    /// TLS13_AES_256_GCM_SHA384 at both ends, on the control protocol, each side holding the
    /// other's own key.
    #[test]
    fn an_enrolled_controller_and_its_pinned_vm_connect_on_aes_256_gcm() {
        let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (endpoint, controller) = dial(&vm, kiln.fingerprint(), &kiln, vm.fingerprint());
        assert!(
            endpoint.connected && controller.connected,
            "{:?} {:?}",
            endpoint.lost,
            controller.lost
        );
        for side in [&endpoint, &controller] {
            assert_eq!(side.suite(), Some(CipherSuite::TLS13_AES_256_GCM_SHA384));
            assert_eq!(side.alpn().as_deref(), Some(ALPN));
        }
        assert_eq!(endpoint.peer_key().as_deref(), Some(kiln.spki()));
        assert_eq!(controller.peer_key().as_deref(), Some(vm.spki()));
    }

    /// A controller of another key than the one enrolled is refused at its handshake, typed at
    /// both ends; the endpoint never counts it connected, so no lane of it is ever read.
    #[test]
    fn a_controller_not_enrolled_is_refused_at_its_handshake() {
        let (vm, kiln, stranger) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (endpoint, controller) = dial(&vm, kiln.fingerprint(), &stranger, vm.fingerprint());
        assert!(!endpoint.connected, "the endpoint took a key it never enrolled");
        assert_eq!(why(&endpoint), Ended::NotEnrolled(By::This));
        assert_eq!(why(&controller), Ended::NotEnrolled(By::Peer));
    }

    /// An endpoint presenting another key than the microVM's pinned one, a stand-in's or
    /// another microVM's, is refused by the controller at its handshake, typed at both ends.
    #[test]
    fn an_endpoint_not_the_pinned_vm_is_refused_at_its_handshake() {
        let (vm, other, kiln) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (endpoint, controller) = dial(&other, kiln.fingerprint(), &kiln, vm.fingerprint());
        assert!(!controller.connected && !endpoint.connected);
        assert_eq!(why(&controller), Ended::NotEnrolled(By::This));
        assert_eq!(why(&endpoint), Ended::NotEnrolled(By::Peer));
    }

    /// A peer of another protocol, a context's client of the same keys, is refused at its
    /// handshake for its protocol, typed: no context command reaches a control endpoint.
    #[test]
    fn a_peer_of_another_protocol_is_refused() {
        let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (endpoint, client) = connect(
            endpoint(&vm, kiln.fingerprint(), transport(true)).unwrap(),
            tls::client(&kiln, vm.fingerprint(), tls::Rtt::Unmeasured).unwrap(),
        );
        assert!(!endpoint.connected && !client.connected);
        assert_eq!(why(&endpoint), Ended::Protocol(By::This));
        assert_eq!(why(&client), Ended::Protocol(By::Peer));
    }

    /// Codes a connection closes with read back as what they are.
    #[test]
    fn a_close_reads_as_its_cause() {
        use hyper_quic::{TransportError, VarInt};
        let local = |alert: AlertDescription| {
            ConnectionError::TransportError(TransportError {
                code: TransportErrorCode::crypto(alert.into()),
                frame: None,
                reason: String::new(),
            })
        };
        assert_eq!(
            ended(&local(AlertDescription::HandshakeFailure)),
            Ended::Handshake(By::This)
        );
        assert_eq!(
            ended(&local(AlertDescription::BadCertificate)),
            Ended::Transport(By::This)
        );
        let closed = |code: u32| {
            ConnectionError::ApplicationClosed(hyper_quic::ApplicationClose {
                error_code: VarInt::from_u32(code),
                reason: bytes::Bytes::new(),
            })
        };
        assert_eq!(
            ended(&closed(Reason::Stale as u32)),
            Ended::Closed(By::Peer, Reason::Stale)
        );
        assert_eq!(ended(&closed(99)), Ended::Transport(By::Peer));
        assert_eq!(ended(&ConnectionError::TimedOut), Ended::Silent);
        assert_eq!(ended(&ConnectionError::LocallyClosed), Ended::Local);
        assert_eq!(ended(&ConnectionError::Reset), Ended::Transport(By::Peer));
        assert_eq!(ended(&ConnectionError::CidsExhausted), Ended::Transport(By::This));
    }

    fn scope() -> (RunId, u64, u32) {
        (RunId([7; 32]), 3, 2)
    }

    /// Step 5's own subscriptions open a member's lanes, byte for byte: its stdin's and each
    /// output's, at their offsets; nothing is taken until the frame is whole.
    #[test]
    fn step_5s_subscriptions_open_a_members_lanes() {
        let (run, generation, member) = scope();
        let output = |channel| {
            managed::subscribe(
                Scope {
                    run,
                    generation,
                    member,
                    channel,
                },
                41,
            )
        };
        let input = managed::input::subscribe(
            managed::input::Scope {
                run,
                generation,
                member,
            },
            9,
        );
        for (bytes, stream, offset) in [
            (input, Stdio::Stdin, 9),
            (
                output(shards_abi::run::managed::Channel::Stdout),
                Stdio::Stdout,
                41,
            ),
            (
                output(shards_abi::run::managed::Channel::Stderr),
                Stdio::Stderr,
                41,
            ),
        ] {
            let opened = Lane::Stdio {
                run,
                generation,
                member,
                stream,
                offset,
            };
            assert_eq!(lane(&bytes), Ok(Some((opened, LANE_MOST))));
            let mut more = bytes.to_vec();
            more.extend_from_slice(b"what follows is the lane's");
            assert_eq!(lane(&more), Ok(Some((opened, LANE_MOST))));
            for cut in 0..bytes.len() {
                assert_eq!(lane(&bytes[..cut]), Ok(None), "cut at {cut}");
            }
        }
        assert_eq!(LANE_MOST, managed::CONTROL_BYTES);
        assert_eq!(LANE_MOST, managed::input::SUBSCRIBE_BYTES);
    }

    /// A first frame of another kind or length, or of no member's channel, is refused as
    /// the controller's error before the endpoint reads what follows it.
    #[test]
    fn a_lane_of_no_kind_is_refused() {
        let (run, generation, member) = scope();
        let good = managed::input::subscribe(
            managed::input::Scope {
                run,
                generation,
                member,
            },
            0,
        );
        let mut other_tag = good;
        other_tag[0] = Tag::Stdin as u8;
        assert_eq!(lane(&other_tag), Err(Reason::Protocol));
        let mut no_channel = good;
        no_channel[FRAME_HEAD + 32 + 8 + 4] = kind::SYSTEM_ERR;
        assert_eq!(lane(&no_channel), Err(Reason::Protocol));
        let mut longer = Vec::new();
        frame::put(&mut longer, Tag::Exec, &[0; SUBSCRIPTION + 1]).unwrap();
        assert_eq!(lane(&longer[..FRAME_HEAD]), Err(Reason::Protocol));
        assert_eq!(lane(&[0xee, 0, 0, 0, 0]), Err(Reason::Protocol));
    }

    /// A lane's refusal reads back as its reason, whole or not yet; anything else there is
    /// the lane's own frame.
    #[test]
    fn a_lanes_refusal_reads_back_as_its_reason() {
        for reason in [
            Reason::Protocol,
            Reason::Scope,
            Reason::Stale,
            Reason::Closed,
            Reason::Auth,
        ] {
            let frame = refused(reason);
            assert_eq!(read_refused(&frame), Ok(Some(reason)));
            for cut in 0..frame.len() {
                assert_eq!(read_refused(&frame[..cut]), Ok(None));
            }
        }
        let mut data = Vec::new();
        frame::put(&mut data, Tag::Stdout, b"x").unwrap();
        assert_eq!(read_refused(&data), Err(Some(Tag::Stdout)));
        let mut unknown = refused(Reason::Scope);
        unknown[REFUSED_BYTES - 1] = 0;
        assert_eq!(read_refused(&unknown), Err(None));
    }

    /// A lane's reading ends the lane alone, but for a frame its protocol refuses, which closes
    /// its connection; a frame refused once its input has ended raced the end, and only stops
    /// the lane's reading.
    #[test]
    fn a_lanes_reading_ends_it_alone_unless_its_controller_broke_its_protocol() {
        assert_eq!(after(Ok(()), false), After::Open);
        assert_eq!(after(Ok(()), true), After::Over);
        for over in [false, true] {
            assert_eq!(after(Err(Gone::Ended), over), After::Ended);
            assert_eq!(after(Err(Gone::Reset), over), After::Ended);
        }
        let refused = Err(Gone::Refused(Reason::Protocol));
        assert_eq!(after(refused, false), After::Refused(Reason::Protocol));
        assert_eq!(after(refused, true), After::Over);
    }

    /// The lanes' service over the real transport, on loopback: an endpoint of member 0's
    /// stdout, stderr and stdin, and controllers on drivers of their own.
    mod lanes {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::{Arc, mpsc};
        use std::thread::JoinHandle;

        use shards_abi::run::managed::InputEnd;

        use super::*;
        use crate::managed::input::{Next, Said};
        use crate::managed::{DATA_HEAD, State};
        use crate::net::Command;

        const RECORD: usize = 64;
        const GENERATION: u64 = 3;
        const STDOUT: usize = 0;
        const STDERR: usize = 1;
        const STDIN: usize = 2;

        fn run() -> RunId {
            RunId([7; 32])
        }

        fn output(channel: Channel) -> Scope {
            Scope {
                run: run(),
                generation: GENERATION,
                member: 0,
                channel,
            }
        }

        fn stdin() -> input::Scope {
            input::Scope {
                run: run(),
                generation: GENERATION,
                member: 0,
            }
        }

        fn admitted(record_bytes: usize, transport_bytes: usize) -> Admitted {
            Admitted {
                record_bytes,
                transport_bytes: transport_bytes as u64,
                // RFC 8446 §4: a handshake message's uint24 length and its four-byte head.
                handshake_bytes: (1 << 24) - 1 + 4,
                datagram_bytes: managed::DATAGRAM_BYTES,
                patience: PATIENCE,
            }
        }

        fn loopback() -> SocketAddr {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        }

        /// An endpoint serving member 0's three streams on a thread of its own; its stdin
        /// relayed into `records`, its end told back as the member's init would tell it.
        struct Serving {
            address: SocketAddr,
            outputs: Vec<(Producer, managed::Control)>,
            input: Arc<input::Control>,
            records: mpsc::Receiver<Vec<u8>>,
            revoke: Revoke,
            driver: Option<JoinHandle<Result<(), managed::Error>>>,
        }

        impl Serving {
            fn bind(vm: &Identity, controller: Fingerprint) -> Serving {
                Serving::bind_with(vm, controller, None).0
            }

            /// As [`Serving::bind`], its execs admitted as its streams are, and handed here.
            fn with_execs(vm: &Identity, controller: Fingerprint) -> (Serving, mpsc::Receiver<exec::Start>) {
                let execs = exec::Admissions {
                    output: admitted(RECORD, RECORD + DATA_HEAD),
                    input: admitted(RECORD, input::TAKEN_BYTES),
                };
                let (serving, starts) = Serving::bind_with(vm, controller, Some(execs));
                (serving, starts.unwrap())
            }

            fn bind_with(
                vm: &Identity,
                controller: Fingerprint,
                execs: Option<exec::Admissions>,
            ) -> (Serving, Option<mpsc::Receiver<exec::Start>>) {
                let out = admitted(RECORD, RECORD + DATA_HEAD);
                let (endpoint, served) = Endpoint::bind(
                    vm,
                    controller,
                    loopback(),
                    Streams {
                        outputs: &[(output(Channel::Stdout), out), (output(Channel::Stderr), out)],
                        inputs: &[(stdin(), admitted(RECORD, input::TAKEN_BYTES))],
                        execs,
                        sockets: None,
                    },
                )
                .unwrap();
                let address = endpoint.address;
                let driver = std::thread::Builder::new()
                    .name("control-endpoint".into())
                    .spawn(move || endpoint.run())
                    .unwrap();
                let Served {
                    outputs,
                    mut inputs,
                    revoke,
                    execs: starts,
                    sockets: _,
                } = served;
                let (relay, control) = inputs.pop().unwrap();
                let input = Arc::new(control);
                let (sink, records) = mpsc::channel();
                let told = Arc::clone(&input);
                std::thread::Builder::new()
                    .name("control-relay".into())
                    .spawn(move || {
                        let mut written = 0u64;
                        loop {
                            match relay.next() {
                                Next::Record { bytes, .. } => {
                                    written += bytes.len() as u64;
                                    let _ = sink.send(bytes.clone());
                                    relay.delivered(bytes);
                                }
                                Next::End => told.ended(written, InputEnd::Closed),
                                Next::Abort(errno) => told.ended(written, InputEnd::Aborted(errno)),
                                Next::Gone => return,
                            }
                        }
                    })
                    .unwrap();
                (
                    Serving {
                        address,
                        outputs,
                        input,
                        records,
                        revoke,
                        driver: Some(driver),
                    },
                    starts,
                )
            }

            /// Writes `bytes` as output `at`'s next record, once its subscriber has made room.
            fn submit(&self, at: usize, bytes: &[u8]) {
                let (producer, _) = &self.outputs[at];
                producer.wait_capacity().unwrap();
                let mut record = producer.take_buffer().unwrap().unwrap();
                record.extend_from_slice(bytes);
                producer.submit(record).unwrap();
            }
        }

        impl Drop for Serving {
            fn drop(&mut self) {
                self.revoke.revoke();
                if let Some(driver) = self.driver.take() {
                    let _ = driver.join();
                }
                self.input.abort();
            }
        }

        enum Ask {
            Connect(SocketAddr),
            /// Opens lane `n`, its first bytes these.
            Open(usize, Vec<u8>),
            /// Sends these bytes on lane `n`.
            Send(usize, Vec<u8>),
        }

        #[derive(Debug, PartialEq, Eq)]
        enum Heard {
            Connected,
            Bytes(usize, Vec<u8>),
            End(usize),
            Reset(usize),
            Lost(Option<u64>),
        }

        /// A controller as kiln's would be: its lanes, each a stream it opened, and what it
        /// has yet to send on each.
        struct Controller {
            config: Option<ClientConfig>,
            conn: Option<ConnKey>,
            lanes: Vec<Option<StreamId>>,
            pending: Vec<(usize, Vec<u8>)>,
            /// Lanes to open once the endpoint allows another stream (RFC 9000 §4.6).
            waiting: Vec<(usize, Vec<u8>)>,
            heard: mpsc::Sender<Heard>,
        }

        impl Controller {
            fn lane(&self, stream: StreamId) -> Option<usize> {
                self.lanes.iter().position(|s| *s == Some(stream))
            }

            /// Opens what lanes it may, in the order asked; the rest wait for room.
            fn open(&mut self, io: &mut Io<'_>) {
                let Some(conn) = self.conn else {
                    return;
                };
                while !self.waiting.is_empty() {
                    let Ok(stream) = io.open(conn) else {
                        return;
                    };
                    let (lane, bytes) = self.waiting.remove(0);
                    if self.lanes.len() <= lane {
                        self.lanes.resize(lane + 1, None);
                    }
                    self.lanes[lane] = Some(stream);
                    self.pending.push((lane, bytes));
                }
                self.flush(io);
            }

            fn flush(&mut self, io: &mut Io<'_>) {
                let Some(conn) = self.conn else {
                    return;
                };
                for (lane, bytes) in &mut self.pending {
                    let Some(Some(stream)) = self.lanes.get(*lane) else {
                        continue;
                    };
                    while !bytes.is_empty() {
                        match io.write(conn, *stream, bytes) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => drop(bytes.drain(..n)),
                        }
                    }
                }
                self.pending.retain(|(_, bytes)| !bytes.is_empty());
            }
        }

        impl Handler for Controller {
            type Command = Ask;
            fn connected(&mut self, _: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
                self.conn = Some(conn);
                let _ = self.heard.send(Heard::Connected);
                Ok(())
            }
            fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
                let Some(lane) = self.lane(stream) else {
                    return;
                };
                loop {
                    match io.read(conn, stream) {
                        Read::Bytes(bytes) => {
                            let _ = self.heard.send(Heard::Bytes(lane, bytes));
                        }
                        Read::End(bytes) => {
                            if !bytes.is_empty() {
                                let _ = self.heard.send(Heard::Bytes(lane, bytes));
                            }
                            let _ = self.heard.send(Heard::End(lane));
                            return;
                        }
                        Read::Reset => {
                            let _ = self.heard.send(Heard::Reset(lane));
                            return;
                        }
                        Read::Blocked => return,
                    }
                }
            }
            fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
                self.flush(io);
            }
            fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, code: Option<u64>) {
                let _ = self.heard.send(Heard::Lost(code));
            }
            fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
                match ask {
                    Ask::Connect(to) => {
                        if let Some(config) = self.config.take() {
                            io.connect(to, config).unwrap();
                        }
                    }
                    Ask::Open(lane, bytes) => {
                        self.waiting.push((lane, bytes));
                        self.open(io);
                    }
                    Ask::Send(lane, bytes) => {
                        self.pending.push((lane, bytes));
                        self.flush(io);
                    }
                }
            }
            fn available(&mut self, io: &mut Io<'_>, _: ConnKey) {
                self.open(io);
            }
            fn done(&self) -> bool {
                false
            }
        }

        /// A controller connected, and what it has heard on each lane.
        struct Live {
            asks: Sender<Ask>,
            heard: mpsc::Receiver<Heard>,
            driver: Option<JoinHandle<Result<(), crate::Error>>>,
            bytes: Vec<Vec<u8>>,
            ends: Vec<Option<Heard>>,
            lost: Option<Option<u64>>,
        }

        impl Live {
            /// A controller under `own`, which pinned `vm`, dialing `to`; once connected or
            /// refused.
            fn dial(own: &Identity, vm: Fingerprint, to: SocketAddr) -> Live {
                let mut transport = TransportConfig::default();
                transport.max_concurrent_bidi_streams(hyper_quic::VarInt::from_u32(0));
                transport.max_concurrent_uni_streams(hyper_quic::VarInt::from_u32(0));
                let (heard_tx, heard) = mpsc::channel();
                let (mut driver, asks) = Driver::bind_bounded(
                    loopback(),
                    None,
                    Controller {
                        config: Some(controller(own, vm, transport).unwrap()),
                        conn: None,
                        lanes: Vec::new(),
                        pending: Vec::new(),
                        waiting: Vec::new(),
                        heard: heard_tx,
                    },
                    DriverLimits {
                        connections: 1,
                        command_slots: 16,
                        queued_datagram_bytes: managed::DATAGRAM_BYTES,
                        handshake_patience: PATIENCE,
                    },
                )
                .unwrap();
                let driver = std::thread::Builder::new()
                    .name("controller".into())
                    .spawn(move || driver.run())
                    .unwrap();
                asks.send(Command::Handler(Ask::Connect(to))).unwrap();
                let mut live = Live {
                    asks,
                    heard,
                    driver: Some(driver),
                    bytes: vec![Vec::new(); 8],
                    ends: (0..8).map(|_| None).collect(),
                    lost: None,
                };
                live.hear();
                live
            }

            /// Takes the next thing heard into what is kept of it; whether it was the
            /// connection made.
            fn hear(&mut self) -> bool {
                match self.heard.recv_timeout(PATIENCE * 2).unwrap() {
                    Heard::Connected => return true,
                    Heard::Bytes(lane, bytes) => self.bytes[lane].extend_from_slice(&bytes),
                    Heard::End(lane) => self.ends[lane] = Some(Heard::End(lane)),
                    Heard::Reset(lane) => self.ends[lane] = Some(Heard::Reset(lane)),
                    Heard::Lost(code) => self.lost = Some(code),
                }
                false
            }

            fn open(&self, lane: usize, first: &[u8]) {
                self.asks
                    .send(Command::Handler(Ask::Open(lane, first.to_vec())))
                    .unwrap();
            }

            fn send(&self, lane: usize, bytes: &[u8]) {
                self.asks
                    .send(Command::Handler(Ask::Send(lane, bytes.to_vec())))
                    .unwrap();
            }

            /// Hears until lane `lane` holds what `take` reads off its front.
            fn take<T>(&mut self, lane: usize, take: fn(&[u8]) -> Option<(T, usize)>) -> T {
                loop {
                    if let Some((value, n)) = take(&self.bytes[lane]) {
                        self.bytes[lane].drain(..n);
                        return value;
                    }
                    assert!(
                        self.ends[lane].is_none(),
                        "lane {lane} ended: {:?}",
                        self.ends[lane]
                    );
                    self.hear();
                }
            }

            /// Hears until lane `lane` ends, all it carried taken.
            fn end(&mut self, lane: usize) -> Heard {
                while self.ends[lane].is_none() {
                    self.hear();
                }
                assert!(
                    self.bytes[lane].is_empty(),
                    "lane {lane} left {:?}",
                    self.bytes[lane]
                );
                self.ends[lane].take().unwrap()
            }

            /// Hears until its connection is lost: the code it was closed with.
            fn lost(&mut self) -> Option<u64> {
                while self.lost.is_none() {
                    self.hear();
                }
                self.lost.unwrap()
            }
        }

        impl Drop for Live {
            fn drop(&mut self) {
                self.asks.request_stop();
                if let Some(driver) = self.driver.take() {
                    let _ = driver.join();
                }
            }
        }

        /// An output's record frame: its offset and its bytes.
        fn record(bytes: &[u8]) -> Option<((u64, Vec<u8>), usize)> {
            let (tag, len) = frame::peek(bytes).unwrap()?;
            assert_eq!(tag, Tag::Stdout);
            let whole = FRAME_HEAD + len;
            let payload = bytes.get(FRAME_HEAD..whole)?;
            let (offset, record) = payload.split_first_chunk::<8>().unwrap();
            Some(((u64::from_be_bytes(*offset), record.to_vec()), whole))
        }

        fn said(bytes: &[u8]) -> Option<(Said, usize)> {
            input::said(bytes).unwrap()
        }

        fn refusal(bytes: &[u8]) -> Option<(Reason, usize)> {
            read_refused(bytes).unwrap().map(|reason| (reason, REFUSED_BYTES))
        }

        /// The enrolled controller drives member 0's stdin, stdout and stderr over one
        /// connection, each a lane: bytes no text holds come through exactly both ways, its
        /// stdin's end reaches the member and the member's end comes back, and an output that
        /// finishes ends its lane.
        #[test]
        fn a_controllers_lanes_carry_a_members_stdio_exactly_and_end_as_it_ends() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            live.open(STDOUT, &managed::subscribe(output(Channel::Stdout), 0));
            live.open(STDERR, &managed::subscribe(output(Channel::Stderr), 0));
            live.open(STDIN, &input::subscribe(stdin(), 0));
            let room = RECORD as u32;
            assert_eq!(live.take(STDIN, said), Said::Taken { accepted: 0, room });

            let out = b"\x00\xff\xfe not UTF-8 \x80, and no newline";
            serving.submit(STDOUT, out);
            assert_eq!(live.take(STDOUT, record), (0, out.to_vec()));
            live.send(STDOUT, &managed::consumed(out.len() as u64));
            serving.submit(STDERR, b"\x1b[31m");
            assert_eq!(live.take(STDERR, record), (0, b"\x1b[31m".to_vec()));
            live.send(STDERR, &managed::consumed(5));
            let more = b"\n{\"type\":\"result\"}";
            serving.submit(STDOUT, more);
            assert_eq!(live.take(STDOUT, record), (out.len() as u64, more.to_vec()));

            let line = b"{\"type\":\"user\"}\xff\x00 no newline";
            let mut frame = Vec::new();
            input::record(0, line, &mut frame).unwrap();
            live.send(STDIN, &frame);
            assert_eq!(serving.records.recv_timeout(PATIENCE).unwrap(), line.to_vec());
            let at = line.len() as u64;
            assert_eq!(live.take(STDIN, said), Said::Taken { accepted: at, room });
            live.send(STDIN, &input::end(at, 0));
            assert_eq!(
                live.take(STDIN, said),
                Said::Ended {
                    written: at,
                    end: InputEnd::Closed
                }
            );
            assert_eq!(live.end(STDIN), Heard::End(STDIN));

            let total = (out.len() + more.len()) as u64;
            serving.outputs[STDOUT].1.finish().unwrap();
            live.send(STDOUT, &managed::consumed(total));
            assert_eq!(live.end(STDOUT), Heard::End(STDOUT));
            assert_eq!(
                serving.outputs[STDOUT].1.state(),
                State::Finished(managed::Progress {
                    accepted: total,
                    consumed: total
                })
            );
            // A lane done leaves its stream: a later one of the finished output ends at once.
            live.open(3, &managed::subscribe(output(Channel::Stdout), total));
            assert_eq!(live.end(3), Heard::End(3));
            assert_eq!(live.lost, None, "a lane's end ends no other");
        }

        /// A controller of a key the microVM did not enroll is refused at its handshake. A TLS
        /// 1.3 client may count itself connected before its server has verified it (RFC 8446
        /// §4.4.4), so it tries a lane as a stranger would: the endpoint reads no stream of a
        /// connection whose handshake it never completed (RFC 9001 §5.7), and no stream of the
        /// microVM's is subscribed to.
        #[test]
        fn a_peer_without_the_enrolled_key_reaches_no_lane() {
            let (vm, kiln, stranger) = (
                Identity::generate().unwrap(),
                Identity::generate().unwrap(),
                Identity::generate().unwrap(),
            );
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&stranger, vm.fingerprint(), serving.address);
            live.open(STDOUT, &managed::subscribe(output(Channel::Stdout), 0));
            live.open(STDIN, &input::subscribe(stdin(), 0));
            assert_eq!(live.lost(), None, "refused by TLS, not by the application");
            let nothing = managed::Progress {
                accepted: 0,
                consumed: 0,
            };
            for (_, control) in &serving.outputs {
                assert_eq!(control.state(), State::Pending(nothing));
            }
            assert_eq!(serving.input.state(), input::State::Open { at: 0 });
            assert!(
                live.bytes.iter().all(Vec::is_empty),
                "a stranger heard {:?}",
                live.bytes
            );
        }

        /// Lanes the endpoint does not serve are refused alone, each typed: another member,
        /// another generation, another run, a member with no stdin, a frame of no kind, a
        /// cursor ahead of the stream's. The connection goes on, and a lane it serves works.
        #[test]
        fn a_lane_the_endpoint_does_not_serve_is_refused_alone_and_the_others_go_on() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            let stdout = output(Channel::Stdout);
            let refused_lanes: [(Vec<u8>, Reason); 6] = [
                (
                    managed::subscribe(Scope { member: 9, ..stdout }, 0).to_vec(),
                    Reason::Scope,
                ),
                (
                    managed::subscribe(
                        Scope {
                            generation: GENERATION + 1,
                            ..stdout
                        },
                        0,
                    )
                    .to_vec(),
                    Reason::Stale,
                ),
                (
                    managed::subscribe(
                        Scope {
                            run: RunId([8; 32]),
                            ..stdout
                        },
                        0,
                    )
                    .to_vec(),
                    Reason::Scope,
                ),
                (
                    input::subscribe(input::Scope { member: 1, ..stdin() }, 0).to_vec(),
                    Reason::Scope,
                ),
                (vec![0xee, 0, 0, 0, 0], Reason::Protocol),
                (managed::subscribe(stdout, 5).to_vec(), Reason::Protocol),
            ];
            for (lane, (first, _)) in refused_lanes.iter().enumerate() {
                live.open(lane, first);
            }
            for (lane, (_, reason)) in refused_lanes.iter().enumerate() {
                assert_eq!(live.take(lane, refusal), *reason, "lane {lane}");
                assert_eq!(live.end(lane), Heard::End(lane));
            }
            live.open(7, &managed::subscribe(stdout, 0));
            serving.submit(STDOUT, b"served");
            assert_eq!(live.take(7, record), (0, b"served".to_vec()));
            assert_eq!(live.lost, None);
        }

        /// A stream holds one lane: a second lane of an output or of a stdin is refused,
        /// `Scope` as an endpoint of one stream refuses a second owner, and the first goes on.
        #[test]
        fn a_second_lane_of_a_stream_is_refused_and_the_first_goes_on() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            live.open(0, &managed::subscribe(output(Channel::Stdout), 0));
            serving.submit(STDOUT, b"first");
            assert_eq!(live.take(0, record), (0, b"first".to_vec()));
            live.open(1, &managed::subscribe(output(Channel::Stdout), 0));
            assert_eq!(live.take(1, refusal), Reason::Scope);
            live.open(2, &input::subscribe(stdin(), 0));
            assert_eq!(
                live.take(2, said),
                Said::Taken {
                    accepted: 0,
                    room: RECORD as u32
                }
            );
            live.open(3, &input::subscribe(stdin(), 0));
            assert_eq!(live.take(3, refusal), Reason::Scope);
            live.send(0, &managed::consumed(5));
            serving.submit(STDOUT, b"second");
            assert_eq!(live.take(0, record), (5, b"second".to_vec()));
            assert_eq!(live.lost, None);
        }

        /// A controller's newer connection supersedes its older one, closed `Stale`, and its
        /// lanes go on where the older's left off: the record the older was sent and never
        /// acknowledged comes again, at its cursor.
        #[test]
        fn the_controllers_newer_connection_supersedes_the_older_and_resumes_at_its_cursor() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut older = Live::dial(&kiln, vm.fingerprint(), serving.address);
            older.open(0, &managed::subscribe(output(Channel::Stdout), 0));
            serving.submit(STDOUT, b"once");
            assert_eq!(older.take(0, record), (0, b"once".to_vec()));
            let mut newer = Live::dial(&kiln, vm.fingerprint(), serving.address);
            assert_eq!(older.lost(), Some(Reason::Stale as u64));
            newer.open(0, &managed::subscribe(output(Channel::Stdout), 0));
            assert_eq!(newer.take(0, record), (0, b"once".to_vec()));
            newer.send(0, &managed::consumed(4));
            serving.submit(STDOUT, b"twice");
            assert_eq!(newer.take(0, record), (4, b"twice".to_vec()));
            assert_eq!(newer.lost, None);
        }

        /// A stream revoked ends its lane alone, reset: the other lanes and the connection go
        /// on, and a new lane of the revoked stream is refused `Closed`.
        #[test]
        fn a_revoked_stream_ends_its_lane_alone() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            live.open(STDOUT, &managed::subscribe(output(Channel::Stdout), 0));
            live.open(STDERR, &managed::subscribe(output(Channel::Stderr), 0));
            serving.submit(STDOUT, b"before");
            assert_eq!(live.take(STDOUT, record), (0, b"before".to_vec()));
            serving.outputs[STDOUT].1.abort();
            assert_eq!(live.end(STDOUT), Heard::Reset(STDOUT));
            serving.submit(STDERR, b"still");
            assert_eq!(live.take(STDERR, record), (0, b"still".to_vec()));
            live.open(3, &managed::subscribe(output(Channel::Stdout), 0));
            assert_eq!(live.take(3, refusal), Reason::Closed);
            assert_eq!(live.lost, None);
        }

        /// A member's stdin that ends on its own (its reader gone, EPIPE) tells its lane so and
        /// ends it; what the controller sends on it then is let go, the connection and the other
        /// lanes go on, and a new lane of the ended stdin is told the end again.
        #[test]
        fn an_inputs_own_end_ends_its_lane_and_the_connection_goes_on() {
            const EPIPE: i32 = 32;
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            live.open(STDIN, &input::subscribe(stdin(), 0));
            live.open(STDOUT, &managed::subscribe(output(Channel::Stdout), 0));
            let room = RECORD as u32;
            assert_eq!(live.take(STDIN, said), Said::Taken { accepted: 0, room });
            serving.input.ended(0, InputEnd::Aborted(EPIPE));
            let mut frame = Vec::new();
            input::record(0, b"too late", &mut frame).unwrap();
            live.send(STDIN, &frame);
            let ended = Said::Ended {
                written: 0,
                end: InputEnd::Aborted(EPIPE),
            };
            assert_eq!(live.take(STDIN, said), ended);
            assert_eq!(live.end(STDIN), Heard::End(STDIN));
            serving.submit(STDOUT, b"on");
            assert_eq!(live.take(STDOUT, record), (0, b"on".to_vec()));
            live.open(4, &input::subscribe(stdin(), 0));
            assert_eq!(live.take(4, said), ended);
            assert_eq!(live.end(4), Heard::End(4));
            assert_eq!(live.lost, None);
            assert!(
                serving.records.try_recv().is_err(),
                "a record after the end reached the member"
            );
        }

        /// The endpoint's windows are its streams' own: each lane's as its endpoint of one
        /// stream bounds it, summed, the per-stream window the largest; one stream each; a
        /// refusal's frame for each stream; the least patience and the largest handshake.
        #[test]
        fn its_windows_are_its_streams_own_summed() {
            let out = admitted(100, 100 + DATA_HEAD);
            let slow = Admitted {
                patience: tls::QUIET,
                ..admitted(50, input::TAKEN_BYTES)
            };
            let (link, windows) = super::super::admitted(Streams {
                outputs: &[(output(Channel::Stdout), out), (output(Channel::Stderr), out)],
                inputs: &[(stdin(), slow)],
                execs: None,
                sockets: None,
            })
            .unwrap();
            let input_receive = input::receive_bytes(50).unwrap();
            assert_eq!(
                windows,
                Windows {
                    stream: input_receive,
                    connection: 2 * managed::CONTROL_BYTES + input_receive,
                    send: (2 * (100 + DATA_HEAD) + input::TAKEN_BYTES + 3 * REFUSED_BYTES) as u64,
                    streams: 3,
                }
            );
            assert_eq!(link.patience, PATIENCE);
            assert_eq!(link.handshake_bytes, out.handshake_bytes);
            assert!(matches!(
                super::super::admitted(Streams {
                    outputs: &[],
                    inputs: &[],
                    execs: None,
                    sockets: None,
                }),
                Err(managed::Error::Admission(AdmissionError::Capacity))
            ));
            let none = Admitted {
                record_bytes: 0,
                ..out
            };
            assert!(matches!(
                super::super::admitted(Streams {
                    outputs: &[(output(Channel::Stdout), none)],
                    inputs: &[],
                    execs: None,
                    sockets: None,
                }),
                Err(managed::Error::Admission(AdmissionError::Capacity))
            ));
            let twice = Endpoint::bind(
                &Identity::generate().unwrap(),
                [0; 32],
                loopback(),
                Streams {
                    outputs: &[(output(Channel::Stdout), out), (output(Channel::Stdout), out)],
                    inputs: &[],
                    execs: None,
                    sockets: None,
                },
            );
            assert!(matches!(
                twice,
                Err(managed::Error::Protocol(ProtocolError::Scope))
            ));
        }

        /// What an exec lane was told at its front.
        fn told(bytes: &[u8]) -> Option<(exec::Told, usize)> {
            exec::told(bytes).unwrap()
        }

        /// A record of `bytes` on an output whose subscriber has made room for it.
        fn submit_to((producer, _): &(Producer, managed::Control), bytes: &[u8]) {
            producer.wait_capacity().unwrap();
            let mut record = producer.take_buffer().unwrap().unwrap();
            record.extend_from_slice(bytes);
            producer.submit(record).unwrap();
        }

        /// A stand-in for the daemon's host of execs: says it started; echoes its stdin to its
        /// stdout; says each steer on its stderr, taking one at a time; and as its stdin ends,
        /// finishes its outputs and says it exited, keeping its streams' controls until its
        /// steering closes (`exec::Start::steer`).
        fn stand_in(start: exec::Start) {
            let exec::Start {
                stdout,
                stderr,
                stdin,
                steer,
                replies,
                ..
            } = start;
            replies.tell(exec::Reply::Started);
            let (stderr, stderr_control) = stderr.unwrap();
            let steered = replies.try_clone().unwrap();
            let steering = std::thread::spawn(move || {
                while let Ok(taken) = steer.recv() {
                    let said = match taken {
                        exec::Steer::Resize(size) => format!("resize {}x{}", size.rows, size.cols),
                        exec::Steer::Signal(n) => format!("signal {n}"),
                    };
                    if stderr.wait_capacity().is_ok() {
                        let mut record = stderr.take_buffer().unwrap().unwrap();
                        record.extend_from_slice(said.as_bytes());
                        let _ = stderr.submit(record);
                    }
                    steered.took();
                }
            });
            let (relay, input) = stdin.unwrap();
            let mut written = 0u64;
            loop {
                match relay.next() {
                    Next::Record { bytes, .. } => {
                        written += bytes.len() as u64;
                        submit_to(&stdout, &bytes);
                        relay.delivered(bytes);
                    }
                    Next::End => {
                        input.ended(written, InputEnd::Closed);
                        break;
                    }
                    Next::Abort(errno) => {
                        input.ended(written, InputEnd::Aborted(errno));
                        break;
                    }
                    Next::Gone => break,
                }
            }
            stdout.1.finish().unwrap();
            stderr_control.finish().unwrap();
            replies.tell(exec::Reply::Exited(0));
            steering.join().unwrap();
            drop((stdout, stderr_control, input));
        }

        fn exec_scope(channel: Channel) -> Scope {
            Scope {
                run: exec::exec_run(run(), 0),
                generation: GENERATION,
                member: 0,
                channel,
            }
        }

        /// An exec asked for on a lane of its own starts as its host says, and its stdio are
        /// step 5's streams on lanes of their own: what its controller writes reaches it
        /// exactly, what it writes comes back exactly, a resize and a signal reach its host one
        /// at a time, and its exit is told once its outputs have finished.
        #[test]
        fn an_exec_lane_starts_its_exec_and_its_lanes_carry_its_stdio_and_steering() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let (serving, starts) = Serving::with_execs(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            let spec = shards_abi::run::Spec {
                argv: vec![b"cat".to_vec()],
                stdin: true,
                ..shards_abi::run::Spec::default()
            };
            let ask = exec::Ask {
                run: run(),
                generation: GENERATION,
                member: 0,
                kind: exec::Kind::Exec(Box::new(spec.clone())),
            };
            live.open(0, &ask.frame().unwrap());
            let start = starts.recv_timeout(PATIENCE).unwrap();
            assert_eq!(
                (start.exec, &start.ask.kind),
                (0, &exec::Kind::Exec(Box::new(spec.clone())))
            );
            assert!(start.stderr.is_some() && start.stdin.is_some());
            let host = std::thread::spawn(move || stand_in(start));
            assert_eq!(live.take(0, told), exec::Told::Started(0));
            live.open(1, &managed::subscribe(exec_scope(Channel::Stdout), 0));
            live.open(2, &managed::subscribe(exec_scope(Channel::Stderr), 0));
            let stdin = input::Scope {
                run: exec::exec_run(run(), 0),
                generation: GENERATION,
                member: 0,
            };
            live.open(3, &input::subscribe(stdin, 0));
            let room = RECORD as u32;
            assert_eq!(live.take(3, said), Said::Taken { accepted: 0, room });
            let bytes = b"\x00\xff exec, no newline";
            let mut frame = Vec::new();
            input::record(0, bytes, &mut frame).unwrap();
            live.send(3, &frame);
            assert_eq!(live.take(1, record), (0, bytes.to_vec()));
            live.send(1, &managed::consumed(bytes.len() as u64));
            let at = bytes.len() as u64;
            assert_eq!(live.take(3, said), Said::Taken { accepted: at, room });

            let size = shards_abi::run::Size { rows: 24, cols: 80 };
            live.send(0, &exec::steering(exec::Steer::Resize(size)).unwrap());
            assert_eq!(live.take(2, record), (0, b"resize 24x80".to_vec()));
            live.send(2, &managed::consumed(12));
            live.send(0, &exec::steering(exec::Steer::Signal(15)).unwrap());
            assert_eq!(live.take(2, record), (12, b"signal 15".to_vec()));
            live.send(2, &managed::consumed(21));

            live.send(3, &input::end(at, 0));
            let ended = Said::Ended {
                written: at,
                end: InputEnd::Closed,
            };
            assert_eq!(live.take(3, said), ended);
            assert_eq!(live.end(1), Heard::End(1));
            assert_eq!(live.end(2), Heard::End(2));
            assert_eq!(live.take(0, told), exec::Told::Exit(0));
            assert_eq!(live.end(0), Heard::End(0));
            host.join().unwrap();
            assert_eq!(live.lost, None);
        }

        /// An ask for another member, generation or run is refused alone, typed, and the
        /// connection goes on; an exec its host does not start is told why, typed.
        #[test]
        fn an_exec_asked_amiss_is_refused_and_one_not_started_is_told_why() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let (serving, starts) = Serving::with_execs(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            let asked = |run, generation, member| {
                exec::Ask {
                    run,
                    generation,
                    member,
                    kind: exec::Kind::Exec(Box::default()),
                }
                .frame()
                .unwrap()
            };
            for (lane, (frame, why)) in [
                (asked(run(), GENERATION, 9), Reason::Scope),
                (asked(run(), GENERATION + 1, 0), Reason::Stale),
                (asked(RunId([8; 32]), GENERATION, 0), Reason::Scope),
            ]
            .into_iter()
            .enumerate()
            {
                live.open(lane, &frame);
                assert_eq!(live.take(lane, refusal), why, "lane {lane}");
                assert_eq!(live.end(lane), Heard::End(lane));
            }
            live.open(3, &asked(run(), GENERATION, 0));
            let start = starts.recv_timeout(PATIENCE).unwrap();
            start
                .replies
                .tell(exec::Reply::Unstarted(exec::Unstarted::Capacity));
            assert_eq!(
                live.take(3, told),
                exec::Told::Unstarted(exec::Unstarted::Capacity)
            );
            assert_eq!(live.end(3), Heard::End(3));
            assert_eq!(live.lost, None);
        }

        /// An endpoint whose host runs no exec refuses an ask alone, `Scope`.
        #[test]
        fn an_endpoint_that_runs_no_exec_refuses_an_ask() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            let ask = exec::Ask {
                run: run(),
                generation: GENERATION,
                member: 0,
                kind: exec::Kind::Exec(Box::default()),
            };
            live.open(0, &ask.frame().unwrap());
            assert_eq!(live.take(0, refusal), Reason::Scope);
            assert_eq!(live.end(0), Heard::End(0));
        }

        /// An exec whose controller's connection is gone is hung up: its host's steering ends.
        #[test]
        fn an_execs_connection_gone_hangs_it_up() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let (serving, starts) = Serving::with_execs(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            let ask = exec::Ask {
                run: run(),
                generation: GENERATION,
                member: 0,
                kind: exec::Kind::Exec(Box::default()),
            };
            live.open(0, &ask.frame().unwrap());
            let start = starts.recv_timeout(PATIENCE).unwrap();
            start.replies.tell(exec::Reply::Started);
            assert_eq!(live.take(0, told), exec::Told::Started(0));
            let newer = Live::dial(&kiln, vm.fingerprint(), serving.address);
            assert_eq!(
                start.steer.recv_timeout(PATIENCE),
                Err(mpsc::RecvTimeoutError::Disconnected)
            );
            drop(newer);
        }

        /// The production controller execs into a member: its stdin and stdout exact over their
        /// lanes, a resize and a signal reaching its host as it sends them, its stdin's end told,
        /// its outputs finished and its exit read; an exec its host does not start is said so,
        /// typed; a controller whose endpoint runs none is told none ran.
        #[test]
        fn the_controller_execs_into_a_member_and_drives_its_stdio_steering_and_exit() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let (serving, starts) = Serving::with_execs(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let admissions = exec::Admissions {
                output: out_admitted(),
                input: in_admitted(),
            };
            let (dialed, _driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: Some(admissions),
                    sockets: None,
                },
            );
            let execs = dialed.execs.as_ref().unwrap();
            // Its host: the first exec it runs, the second it says did not start.
            let host = std::thread::spawn(move || {
                stand_in(starts.recv_timeout(PATIENCE).unwrap());
                let refused = starts.recv_timeout(PATIENCE).unwrap();
                refused
                    .replies
                    .tell(exec::Reply::Unstarted(exec::Unstarted::Refused));
            });
            let spec = shards_abi::run::Spec {
                argv: vec![b"cat".to_vec()],
                stdin: true,
                ..shards_abi::run::Spec::default()
            };
            let Exec {
                mut stdout,
                stderr,
                stdin,
                mut control,
            } = execs.exec(0, spec.clone()).unwrap();
            let (mut stderr, mut stdin) = (stderr.unwrap(), stdin.unwrap());
            let line = b"\x00\xff typed, no newline";
            assert_eq!(stdin.write(line).unwrap(), line.len());
            assert_eq!(
                stdout.next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: line.to_vec()
                }
            );
            control
                .resize(shards_abi::run::Size { rows: 50, cols: 132 })
                .unwrap();
            assert_eq!(
                stderr.next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"resize 50x132".to_vec()
                }
            );
            control.signal(2).unwrap();
            assert_eq!(control.signal(0), Err(Fault::Refused(Reason::Protocol)));
            assert_eq!(
                stderr.next_record().unwrap(),
                Got::Record {
                    at: 13,
                    bytes: b"signal 2".to_vec()
                }
            );
            assert_eq!(stdin.end().unwrap(), (line.len() as u64, InputEnd::Closed));
            assert_eq!(stdout.next_record().unwrap(), Got::Finished);
            assert_eq!(stderr.next_record().unwrap(), Got::Finished);
            assert_eq!(control.wait(), Ok(0));
            assert_eq!(
                execs.exec(0, spec).map(|_| ()),
                Err(Fault::Unstarted(exec::Unstarted::Refused))
            );
            host.join().unwrap();
        }

        /// A stand-in for a host's MCP connection (D254): says it started as its harness
        /// connected, the harness echoing every byte it is sent; as what it is sent ends,
        /// finishes its output and says it ended, keeping its streams' controls until its
        /// steering closes (`exec::Start::steer`).
        fn mcp_stand_in(start: exec::Start) {
            let exec::Start {
                stdout,
                stdin,
                steer,
                replies,
                ..
            } = start;
            replies.tell(exec::Reply::Started);
            let (relay, input) = stdin.unwrap();
            let mut written = 0u64;
            loop {
                match relay.next() {
                    Next::Record { bytes, .. } => {
                        written += bytes.len() as u64;
                        submit_to(&stdout, &bytes);
                        relay.delivered(bytes);
                    }
                    Next::End => {
                        input.ended(written, InputEnd::Closed);
                        break;
                    }
                    Next::Abort(errno) => {
                        input.ended(written, InputEnd::Aborted(errno));
                        break;
                    }
                    Next::Gone => break,
                }
            }
            stdout.1.finish().unwrap();
            replies.tell(exec::Reply::Exited(0));
            while steer.recv().is_ok() {}
            drop((stdout, input));
        }

        /// An MCP connection (D254) is an exec in all but its start: the controller's ask is
        /// handed its host as an MCP ask, one stream each way and no stderr; what the
        /// controller sends reaches the harness exactly and what the harness sends comes back
        /// exactly; its end is told once its bytes are consumed; and a member whose run asked
        /// for no bridge refuses it, typed.
        #[test]
        fn the_controller_takes_a_members_mcp_connection_as_an_exec() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let (serving, starts) = Serving::with_execs(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let admissions = exec::Admissions {
                output: out_admitted(),
                input: in_admitted(),
            };
            let (dialed, _driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: Some(admissions),
                    sockets: None,
                },
            );
            let execs = dialed.execs.as_ref().unwrap();
            // Its host: the first, a harness that echoes; the second, a member with no bridge.
            let host = std::thread::spawn(move || {
                let start = starts.recv_timeout(PATIENCE).unwrap();
                assert_eq!((start.exec, &start.ask.kind), (0, &exec::Kind::Mcp));
                assert!(start.stderr.is_none() && start.stdin.is_some());
                mcp_stand_in(start);
                let refused = starts.recv_timeout(PATIENCE).unwrap();
                assert_eq!(refused.ask.kind, exec::Kind::Mcp);
                refused
                    .replies
                    .tell(exec::Reply::Unstarted(exec::Unstarted::Refused));
            });
            let McpConnection {
                mut from,
                mut to,
                mut end,
            } = execs.mcp(0).unwrap();
            let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
            assert_eq!(to.write(request).unwrap(), request.len());
            assert_eq!(
                from.next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: request.to_vec()
                }
            );
            assert_eq!(to.end().unwrap(), (request.len() as u64, InputEnd::Closed));
            assert_eq!(from.next_record().unwrap(), Got::Finished);
            assert_eq!(end.wait(), Ok(0));
            assert_eq!(
                execs.mcp(0).map(|_| ()),
                Err(Fault::Unstarted(exec::Unstarted::Refused))
            );
            host.join().unwrap();
        }

        /// The production controller (`drive.rs`), dialed at `to` for `subscriptions`, its
        /// driver on a thread of its own.
        fn drive(
            own: &Identity,
            vm: Fingerprint,
            to: SocketAddr,
            subscriptions: Subscriptions<'_>,
        ) -> (Dialed, JoinHandle<Result<(), managed::Error>>) {
            let (controller, dialed) = super::super::Controller::dial(own, vm, to, subscriptions).unwrap();
            let driver = std::thread::Builder::new()
                .name("controller".into())
                .spawn(move || controller.run())
                .unwrap();
            (dialed, driver)
        }

        fn out_admitted() -> Admitted {
            admitted(RECORD, RECORD + DATA_HEAD)
        }

        fn in_admitted() -> Admitted {
            admitted(RECORD, input::TAKEN_BYTES)
        }

        /// Member 0's two outputs and its stdin, each with its admission and cursor.
        type Member = ([(Scope, Admitted, u64); 2], [(input::Scope, Admitted, u64); 1]);

        /// Member 0's stdout, stderr and stdin, each from `from`.
        fn all(from: u64) -> Member {
            (
                [
                    (output(Channel::Stdout), out_admitted(), from),
                    (output(Channel::Stderr), out_admitted(), from),
                ],
                [(stdin(), in_admitted(), from)],
            )
        }

        /// kiln's side as Shards gives it: the controller drives member 0's stdin, stdout and
        /// stderr over one connection; bytes no text holds come through exactly both ways, in
        /// records of the endpoint's room under its pace, the stdin's end reaches the member
        /// and its end comes back, and an output that finishes says so once its last record is
        /// had.
        #[test]
        fn the_controller_drives_a_members_stdio_exactly_and_ends_as_it_ends() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let (mut dialed, driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            let out = b"\x00\xff\xfe not UTF-8 \x80, and no newline";
            serving.submit(STDOUT, out);
            assert_eq!(
                dialed.outputs[0].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: out.to_vec()
                }
            );
            serving.submit(STDERR, b"\x1b[31m");
            assert_eq!(
                dialed.outputs[1].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"\x1b[31m".to_vec()
                }
            );

            // More than one record's room: the stdin takes it a room at a time, each record
            // taken before the next goes.
            let line: Vec<u8> = (0..=255u8).cycle().take(3 * RECORD + 5).collect();
            let stdin = &mut dialed.inputs[0];
            let mut at = 0;
            while at < line.len() {
                let took = stdin.write(&line[at..]).unwrap();
                assert!((1..=RECORD).contains(&took), "{took}");
                at += took;
                assert_eq!(stdin.cursor(), at as u64);
            }
            let mut relayed = Vec::new();
            while relayed.len() < line.len() {
                let record = serving.records.recv_timeout(PATIENCE).unwrap();
                assert!(record.len() <= RECORD);
                relayed.extend_from_slice(&record);
            }
            assert_eq!(relayed, line);
            assert_eq!(stdin.end().unwrap(), (line.len() as u64, InputEnd::Closed));
            assert_eq!(
                stdin.write(b"after"),
                Err(Fault::InputEnded {
                    written: line.len() as u64,
                    end: InputEnd::Closed
                })
            );

            serving.outputs[STDOUT].1.finish().unwrap();
            assert_eq!(dialed.outputs[0].next_record().unwrap(), Got::Finished);
            assert_eq!(dialed.outputs[0].cursor(), out.len() as u64);
            assert_eq!(
                serving.outputs[STDOUT].1.state(),
                State::Finished(managed::Progress {
                    accepted: out.len() as u64,
                    consumed: out.len() as u64
                })
            );
            assert_eq!(dialed.outputs[0].next_record().unwrap(), Got::Finished);
            drop(dialed);
            driver.join().unwrap().unwrap();
        }

        /// The endpoint's report on the control lane: what its own TLS negotiated.
        fn report(bytes: &[u8]) -> Option<(Negotiated, usize)> {
            read_negotiated(bytes).unwrap().map(|n| (n, NEGOTIATED_BYTES))
        }

        /// A control lane's ask is answered with what the endpoint's own TLS negotiated, the
        /// suite the provider offers under a group it names, then the lane's end; an ask of
        /// more than its tag is refused alone, and the connection answers the next.
        #[test]
        fn the_control_lane_says_what_the_endpoints_own_tls_negotiated() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let mut live = Live::dial(&kiln, vm.fingerprint(), serving.address);
            live.open(0, &CONTROL_ASK);
            let said = live.take(0, report);
            assert_eq!(said.suite_name(), Some("TLS13_AES_256_GCM_SHA384"));
            assert!(said.is_provided());
            assert!(said.group_name().is_some(), "{said:?}");
            assert!(matches!(live.end(0), Heard::End(0)));

            let mut longer = CONTROL_ASK.to_vec();
            longer[FRAME_HEAD - 1] = 1;
            longer.push(0);
            live.open(1, &longer);
            assert_eq!(live.take(1, refusal), Reason::Protocol);
            assert!(matches!(live.end(1), Heard::End(1)));

            live.open(2, &CONTROL_ASK);
            assert_eq!(live.take(2, report), said);
            assert!(matches!(live.end(2), Heard::End(2)));
            assert_eq!(live.lost, None);
        }

        /// kiln's side as Shards gives it: the controller reads its own TLS's negotiation and
        /// hears the endpoint's on the control lane, the same suite and group, and again once
        /// heard; the member's lanes go on beside it.
        #[test]
        fn the_controller_has_both_ends_word_on_its_tls() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let (mut dialed, driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            let link = dialed.link.wait().unwrap();
            assert_eq!(link.controller, link.endpoint);
            assert_eq!(
                link.controller.suite,
                u16::from(CipherSuite::TLS13_AES_256_GCM_SHA384)
            );
            assert!(link.endpoint.is_provided());
            assert_eq!(dialed.link.wait(), Ok(link));
            serving.submit(STDOUT, b"after");
            assert_eq!(
                dialed.outputs[0].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"after".to_vec()
                }
            );
            drop(dialed);
            driver.join().unwrap().unwrap();
        }

        /// One record in flight, acknowledged only as the controller asks for the next: the
        /// endpoint holds a record handed out until then, and its next comes after, at its
        /// cursor.
        #[test]
        fn a_record_is_acknowledged_only_as_the_next_is_asked_for() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let (mut dialed, _driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            serving.submit(STDOUT, b"first");
            let mut stdout = dialed.outputs.remove(0);
            assert_eq!(
                stdout.next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"first".to_vec()
                }
            );
            let held = managed::Progress {
                accepted: 5,
                consumed: 0,
            };
            std::thread::sleep(PATIENCE / 10);
            assert_eq!(
                serving.outputs[STDOUT].1.progress(),
                held,
                "acknowledged while held"
            );
            assert_eq!(stdout.cursor(), 0);
            let reader = std::thread::spawn(move || {
                let next = stdout.next_record();
                (stdout, next)
            });
            let asked = std::time::Instant::now();
            while serving.outputs[STDOUT].1.progress().consumed != 5 {
                assert!(asked.elapsed() < PATIENCE, "never acknowledged");
                std::thread::sleep(PATIENCE / 100);
            }
            serving.submit(STDOUT, b"second");
            let (stdout, next) = reader.join().unwrap();
            assert_eq!(
                next.unwrap(),
                Got::Record {
                    at: 5,
                    bytes: b"second".to_vec()
                }
            );
            assert_eq!(stdout.cursor(), 5);
        }

        /// A lane the endpoint does not serve is refused alone, typed, and the others go on.
        #[test]
        fn a_lane_the_endpoint_refuses_is_refused_alone_and_typed() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let outputs = [
                (
                    Scope {
                        member: 9,
                        ..output(Channel::Stdout)
                    },
                    out_admitted(),
                    0,
                ),
                (
                    Scope {
                        generation: GENERATION + 1,
                        ..output(Channel::Stdout)
                    },
                    out_admitted(),
                    0,
                ),
                (output(Channel::Stdout), out_admitted(), 0),
            ];
            let inputs = [(input::Scope { member: 1, ..stdin() }, in_admitted(), 0)];
            let (mut dialed, _driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            assert_eq!(
                dialed.outputs[0].next_record(),
                Err(Fault::Refused(Reason::Scope))
            );
            assert_eq!(
                dialed.outputs[1].next_record(),
                Err(Fault::Refused(Reason::Stale))
            );
            assert_eq!(dialed.inputs[0].write(b"x"), Err(Fault::Refused(Reason::Scope)));
            serving.submit(STDOUT, b"served");
            assert_eq!(
                dialed.outputs[2].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"served".to_vec()
                }
            );
            assert_eq!(
                dialed.outputs[0].next_record(),
                Err(Fault::Refused(Reason::Scope))
            );
        }

        /// A controller under a key the microVM did not enroll reaches no lane: each says so,
        /// typed, `NotEnrolled`. So does one that pinned another microVM's key.
        #[test]
        fn a_controller_not_enrolled_or_not_pinned_reaches_no_lane() {
            let (vm, kiln, stranger) = (
                Identity::generate().unwrap(),
                Identity::generate().unwrap(),
                Identity::generate().unwrap(),
            );
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let subscriptions = Subscriptions {
                outputs: &outputs,
                inputs: &inputs,
                execs: None,
                sockets: None,
            };
            let (mut dialed, driver) = drive(&stranger, vm.fingerprint(), serving.address, subscriptions);
            let refused = dialed.outputs[0].next_record();
            assert!(
                matches!(refused, Err(Fault::Lost(Ended::NotEnrolled(_)))),
                "{refused:?}"
            );
            assert!(matches!(
                dialed.inputs[0].write(b"x"),
                Err(Fault::Lost(Ended::NotEnrolled(_)))
            ));
            driver.join().unwrap().unwrap();
            let (mut dialed, driver) = drive(&kiln, stranger.fingerprint(), serving.address, subscriptions);
            assert_eq!(
                dialed.outputs[0].next_record(),
                Err(Fault::Lost(Ended::NotEnrolled(By::This)))
            );
            driver.join().unwrap().unwrap();
            assert_eq!(serving.outputs[STDOUT].1.progress().consumed, 0);
        }

        /// The controller's newer connection supersedes its older, which is told `Stale`; the
        /// newer opens at the older's cursors and has again the record the older never
        /// acknowledged.
        #[test]
        fn a_newer_dial_supersedes_the_older_and_resumes_at_its_cursors() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let (mut older, _older_driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            serving.submit(STDOUT, b"once");
            assert_eq!(
                older.outputs[0].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"once".to_vec()
                }
            );
            assert_eq!(older.inputs[0].write(b"in").unwrap(), 2);
            let (outputs, inputs) = (
                [
                    (output(Channel::Stdout), out_admitted(), older.outputs[0].cursor()),
                    (output(Channel::Stderr), out_admitted(), older.outputs[1].cursor()),
                ],
                [(stdin(), in_admitted(), older.inputs[0].cursor())],
            );
            let (mut newer, _newer_driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            assert_eq!(
                newer.outputs[0].next_record().unwrap(),
                Got::Record {
                    at: 0,
                    bytes: b"once".to_vec()
                }
            );
            assert_eq!(
                older.outputs[0].next_record(),
                Err(Fault::Lost(Ended::Closed(By::Peer, Reason::Stale)))
            );
            assert_eq!(newer.inputs[0].write(b"more").unwrap(), 4);
            assert_eq!(serving.records.recv_timeout(PATIENCE).unwrap(), b"in");
            assert_eq!(serving.records.recv_timeout(PATIENCE).unwrap(), b"more");
            // Asking for the next acknowledges "once", which makes the endpoint's room for it.
            let mut stdout = newer.outputs.remove(0);
            let reader = std::thread::spawn(move || stdout.next_record());
            serving.submit(STDOUT, b"twice");
            assert_eq!(
                reader.join().unwrap().unwrap(),
                Got::Record {
                    at: 4,
                    bytes: b"twice".to_vec()
                }
            );
        }

        /// A revoked output ends its lane alone, `Revoked`; an aborted stdin ends with its
        /// errno as the member's init tells it; the connection and other lanes go on, until
        /// the controller hangs up, which every lane left hears as its own close.
        #[test]
        fn revocation_and_abort_end_their_lanes_alone_and_a_hangup_ends_the_rest() {
            let (vm, kiln) = (Identity::generate().unwrap(), Identity::generate().unwrap());
            let serving = Serving::bind(&vm, kiln.fingerprint());
            let (outputs, inputs) = all(0);
            let (mut dialed, driver) = drive(
                &kiln,
                vm.fingerprint(),
                serving.address,
                Subscriptions {
                    outputs: &outputs,
                    inputs: &inputs,
                    execs: None,
                    sockets: None,
                },
            );
            serving.submit(STDOUT, b"before");
            assert!(matches!(dialed.outputs[0].next_record(), Ok(Got::Record { .. })));
            serving.outputs[STDOUT].1.abort();
            assert_eq!(dialed.outputs[0].next_record(), Err(Fault::Revoked));
            let epipe = std::num::NonZeroU16::new(32).unwrap();
            assert_eq!(dialed.inputs[0].write(b"ab").unwrap(), 2);
            assert_eq!(dialed.inputs[0].abort(epipe).unwrap(), (2, InputEnd::Aborted(32)));
            serving.submit(STDERR, b"still");
            assert!(matches!(dialed.outputs[1].next_record(), Ok(Got::Record { .. })));
            dialed.hangup.hangup();
            assert_eq!(dialed.outputs[1].next_record(), Err(Fault::Lost(Ended::Local)));
            driver.join().unwrap().unwrap();
        }

        /// The controller's windows are its lanes' own, from the endpoint's admissions, the
        /// control lane's among them: what the endpoint sends on each before it is read,
        /// summed; what it sends on each, summed; no stream the endpoint may open; the least
        /// patience and the largest handshake.
        #[test]
        fn the_controllers_windows_are_its_lanes_own_summed() {
            let slow = Admitted {
                patience: tls::QUIET,
                ..admitted(50, input::TAKEN_BYTES)
            };
            let (link, windows) = super::super::drive::windows(Subscriptions {
                outputs: &[
                    (output(Channel::Stdout), out_admitted(), 0),
                    (output(Channel::Stderr), out_admitted(), 7),
                ],
                inputs: &[(stdin(), slow, 0)],
                execs: None,
                sockets: None,
            })
            .unwrap();
            let ack = 5 + 8;
            // The control lane: its report or a refusal read, its ask sent.
            let (report, ask) = (NEGOTIATED_BYTES.max(REFUSED_BYTES), CONTROL_ASK.len());
            assert_eq!(
                windows,
                Windows {
                    stream: RECORD + DATA_HEAD,
                    connection: 2 * (RECORD + DATA_HEAD) + input::TAKEN_BYTES + report,
                    send: (2 * (managed::CONTROL_BYTES + ack)
                        + input::SUBSCRIBE_BYTES
                        + input::RECORD_HEAD
                        + 50
                        + input::END_BYTES
                        + ask) as u64,
                    streams: 0,
                }
            );
            assert_eq!(link.patience, PATIENCE);
            assert!(matches!(
                super::super::drive::windows(Subscriptions {
                    outputs: &[],
                    inputs: &[],
                    execs: None,
                    sockets: None,
                }),
                Err(managed::Error::Admission(AdmissionError::Capacity))
            ));
        }
    }
}
