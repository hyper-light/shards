//! Socket lanes (D251; Hex's step 39): a lane to an AF_UNIX stream socket of a microVM's run's
//! own command, by its file name in the one directory the run grants, as OpenSSH connects a
//! channel to a socket by its path (`direct-streamlocal@openssh.com`, PROTOCOL §2.4).
//!
//! - **The ask.** The lane's first frame (`Tag::UnixConnect`) names the run, its generation and
//!   the socket's name. A name that is no file name is the controller's error, refused
//!   `Protocol` before the endpoint's host is asked; another run or generation, or an endpoint
//!   that serves no socket lane, `Scope`; a lane past the run's admitted most, `Capacity`, never
//!   queued.
//! - **Its host.** The endpoint connects nothing itself: it hands its host the ask
//!   ([`Connect`]), and the host answers with the connected socket or the errno its connect
//!   failed with ([`Reply`]): `Tag::Connected`, then the socket's bytes, or `Tag::Unreached`
//!   and the lane's end.
//! - **Its bytes.** After `Connected` the lane is the socket, byte for byte, both ways: each
//!   way's end is that way's half-close (a FIN; the socket's `shutdown(SHUT_WR)`), and an abort
//!   either way is the other's (RESET_STREAM or STOP_SENDING; the socket ended both ways).
//!   Each way holds at most the run's admitted window in flight: the endpoint reads no more of
//!   the controller than its host's writer has room for, and its reader reads no more of the
//!   socket than the controller has room for, so a stalled reader holds up its own lane alone
//!   (RFC 9000 §2.2, §4.1).
//! - **Its scope.** An endpoint that serves its run's member streams knows its run and
//!   generation, which an ask must name; one that serves sockets alone knows only the keys its
//!   connection pinned, and an ask names the zero run and generation 0 there.

use std::io::Write as _;
use std::mem::size_of;
use std::num::NonZeroU16;
use std::sync::mpsc;

use crate::frame::{self, Tag};
use crate::managed::{self, AdmissionError, Reason, RunId};
use crate::net::{Command, ConnKey, Io, Read, Sender};

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
/// An ask's scope before its name: the run (32) and its generation (u64).
const SCOPE: usize = size_of::<RunId>() + size_of::<u64>();
/// The longest file name a lane names: Linux's `NAME_MAX` (include/uapi/linux/limits.h).
pub const NAME_MOST: usize = shards_abi::run::members::NAME_MOST;
/// The most an ask's frame takes: its head, its scope and the longest name.
pub const ASK_MOST: usize = FRAME_HEAD + SCOPE + NAME_MOST;
/// The endpoint's answer, whole: `Connected`'s head, or `Unreached`'s head and its errno.
pub const CONNECTED_BYTES: usize = FRAME_HEAD;
pub const UNREACHED_BYTES: usize = FRAME_HEAD + size_of::<u32>();

/// TLS 1.3's largest handshake message: a uint24 length and its four-byte head (RFC 8446 §4),
/// the most a handshake's crypto buffer holds of one.
pub const HANDSHAKE_MOST: usize = (1 << 24) - 1 + 4;

/// The link of a connection that carries socket lanes and no member stream, which would set it
/// otherwise: its handshake within TLS 1.3's largest message ([`HANDSHAKE_MOST`]), its datagrams
/// D180's ([`managed::DATAGRAM_BYTES`]), and its patience D180's own bound of silence
/// (`tls::QUIET`, RFC 1122 §4.2.3.5's R2), as the daemon's endpoints take them.
pub(super) fn link() -> managed::Admitted {
    managed::Admitted {
        record_bytes: 0,
        transport_bytes: 0,
        handshake_bytes: HANDSHAKE_MOST,
        datagram_bytes: managed::DATAGRAM_BYTES,
        patience: crate::tls::QUIET,
    }
}

/// What a run admits of socket lanes (R03: its caller's numbers): how many may be open at once,
/// and each lane's bytes in flight each way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    pub lanes: u32,
    pub window: u32,
}

impl Admission {
    /// Admitted at all: a lane and a byte at least.
    pub fn check(self) -> Result<Admission, managed::Error> {
        if self.lanes == 0 || self.window == 0 {
            return Err(managed::Error::Admission(AdmissionError::Window));
        }
        Ok(self)
    }
}

/// Whether `name` names one file in the run's directory: nonempty, neither `.` nor `..`, no
/// `/` or NUL, within `NAME_MAX`.
pub fn name_ok(name: &str) -> bool {
    shards_abi::run::socket::name_ok(name.as_bytes())
}

/// An ask's first frame, whole: the run, its generation and the name. A name that is no file
/// name is refused here, as the endpoint would.
pub fn ask_frame(run: RunId, generation: u64, name: &str) -> Result<Vec<u8>, Reason> {
    if !name_ok(name) {
        return Err(Reason::Protocol);
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(SCOPE.saturating_add(name.len()))
        .map_err(|_| Reason::Closed)?;
    payload.extend_from_slice(&run.0);
    payload.extend_from_slice(&generation.to_be_bytes());
    payload.extend_from_slice(name.as_bytes());
    let mut out = Vec::new();
    frame::put(&mut out, Tag::UnixConnect, &payload).map_err(|_| Reason::Protocol)?;
    Ok(out)
}

/// The ask a whole first frame's `payload` carries: a protocol error for one cut short, or a
/// name that is no file name.
fn read_ask(payload: &[u8]) -> Result<(RunId, u64, String), Reason> {
    let (run, rest) = payload.split_first_chunk::<32>().ok_or(Reason::Protocol)?;
    let (generation, name) = rest.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
    let name = std::str::from_utf8(name)
        .ok()
        .filter(|n| name_ok(n))
        .ok_or(Reason::Protocol)?;
    Ok((RunId(*run), u64::from_be_bytes(*generation), name.to_owned()))
}

/// `Unreached`'s frame for `errno`.
pub fn unreached(errno: NonZeroU16) -> [u8; UNREACHED_BYTES] {
    let mut out = [0; UNREACHED_BYTES];
    let len = (size_of::<u32>() as u32).to_be_bytes();
    let errno = u32::from(errno.get()).to_be_bytes();
    for (slot, byte) in out
        .iter_mut()
        .zip(std::iter::once(&(Tag::Unreached as u8)).chain(&len).chain(&errno))
    {
        *slot = *byte;
    }
    out
}

/// `Connected`'s frame.
pub fn connected() -> [u8; CONNECTED_BYTES] {
    [Tag::Connected as u8, 0, 0, 0, 0]
}

/// What the endpoint's answer at the front of `bytes` says, and its length: connected, or the
/// errno its host's connect failed with; none until it has come whole. Anything else is no
/// answer of a socket lane's.
pub fn read_answer(bytes: &[u8]) -> Result<Option<(Result<(), NonZeroU16>, usize)>, ()> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| ())? else {
        return Ok(None);
    };
    match (tag, len) {
        (Tag::Connected, 0) => Ok(Some((Ok(()), CONNECTED_BYTES))),
        (Tag::Unreached, 4) => {
            let Some(errno) = bytes.get(FRAME_HEAD..UNREACHED_BYTES) else {
                return Ok(None);
            };
            let errno = u32::from_be_bytes(errno.try_into().map_err(|_| ())?);
            let errno = u16::try_from(errno).ok().and_then(NonZeroU16::new).ok_or(())?;
            Ok(Some((Err(errno), UNREACHED_BYTES)))
        }
        _ => Err(()),
    }
}

/// How the host answers an ask: the socket, connected, whose bytes the lane carries from here
/// on; or the errno its connect failed with (`ENOENT`, `ECONNREFUSED`, ...).
#[derive(Debug)]
pub enum Reply {
    Connected(shards_ipc::Stream),
    Unreached(NonZeroU16),
}

/// The endpoint has ended: nothing waits for the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

/// One lane's ask, as the endpoint hands it to its host.
#[derive(Debug)]
pub struct Connect {
    name: String,
    lane: u64,
    replies: mpsc::Sender<(u64, Reply)>,
    wake: Sender<()>,
}

impl Connect {
    /// The socket's file name in the run's directory, checked ([`name_ok`]).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Answers the ask: the endpoint tells its controller, and carries the socket's bytes.
    pub fn reply(self, reply: Reply) -> Result<(), Gone> {
        self.replies.send((self.lane, reply)).map_err(|_| Gone)?;
        let _ = self.wake.try_send(Command::Handler(()));
        Ok(())
    }
}

/// What the controller's side of a lane sends the socket's writer.
#[derive(Debug)]
enum Up {
    Bytes(Vec<u8>),
    /// The controller's FIN: the socket's write side is shut, once what came before is written.
    End,
}

/// What the socket's writer and reader tell the endpoint.
#[derive(Debug)]
enum Said {
    /// The writer wrote so many bytes: room for as many more.
    Wrote(usize),
    /// The reader read these.
    Read(Vec<u8>),
    /// The socket's own end of its writes: its FIN.
    Ended,
    /// A write or read failed, or the socket closed while the controller still sent: the lane
    /// is reset, never ended in order.
    Failed,
}

/// A lane's way, as the endpoint drives it (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    Open,
    /// Its end came: the rest goes, then its FIN.
    Ending,
    Done,
}

/// A connected lane's pump: its socket's writer and reader on threads of their own, and what
/// each way holds at the endpoint.
#[derive(Debug)]
struct Pump {
    socket: shards_ipc::Stream,
    to_writer: mpsc::Sender<Up>,
    /// Room the writer has, of the window: what the endpoint may read of the controller.
    writer_room: usize,
    said: mpsc::Receiver<Said>,
    /// Room back to the reader as the controller takes what it read.
    to_reader: mpsc::Sender<usize>,
    /// What goes to the controller, `sent` of it gone; its answer first.
    out: Vec<u8>,
    sent: usize,
    /// How much of `out` is the answer's, which the reader's window does not count.
    answer: usize,
    up: Way,
    down: Way,
}

impl Pump {
    /// The socket ended both ways at once: its writer and reader end, and the lane with them.
    fn abort(&self) {
        shards_ipc::end(&self.socket);
    }
}

/// Where a socket lane stands (R04).
#[derive(Debug)]
enum Stage {
    /// Its ask, as it comes: `whole` its frame's length.
    Asking {
        frame: Vec<u8>,
        whole: usize,
    },
    /// Handed to the host, which has yet to answer.
    Asked,
    Carrying(Box<Pump>),
}

#[derive(Debug)]
struct Lane {
    conn: ConnKey,
    stream: hyper_quic::StreamId,
    id: u64,
    stage: Stage,
}

/// The endpoint's socket lanes: what a run admits, where it hands its host each ask, and what
/// the host says.
#[derive(Debug)]
pub(super) struct Carried {
    admission: Option<Admission>,
    asks: Option<mpsc::Sender<Connect>>,
    replies_tx: mpsc::Sender<(u64, Reply)>,
    replies: mpsc::Receiver<(u64, Reply)>,
    /// The endpoint's own sender, which each lane's threads and host wake it with.
    pub(super) wake: Option<Sender<()>>,
    next: u64,
    lanes: Vec<Lane>,
}

impl Carried {
    pub(super) fn new(admission: Option<Admission>) -> (Carried, Option<mpsc::Receiver<Connect>>) {
        let (replies_tx, replies) = mpsc::channel();
        let (asks, handed) = match admission {
            Some(_) => {
                let (tx, rx) = mpsc::channel();
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        (
            Carried {
                admission,
                asks,
                replies_tx,
                replies,
                wake: None,
                next: 0,
                lanes: Vec::new(),
            },
            handed,
        )
    }

    fn at(&self, conn: ConnKey, stream: hyper_quic::StreamId) -> Option<usize> {
        self.lanes
            .iter()
            .position(|l| l.conn == conn && l.stream == stream)
    }
}

/// The endpoint ended: every carried socket is ended at once, so that no writer or reader of
/// one outlives it.
impl Drop for Carried {
    fn drop(&mut self) {
        for lane in &self.lanes {
            if let Stage::Carrying(pump) = &lane.stage {
                pump.abort();
            }
        }
        while let Ok((_, reply)) = self.replies.try_recv() {
            if let Reply::Connected(socket) = reply {
                shards_ipc::end(&socket);
            }
        }
    }
}

/// What a run's socket lanes add to its endpoint's connection: a stream for each and one more,
/// on which a lane past its most is refused `Capacity`; what the endpoint reads of each before
/// its writer takes it (its ask, then its window), and what it sends unacknowledged (its
/// answer, then its window, or a refusal).
pub(super) fn added(admission: Admission) -> Option<(u32, u64, u64)> {
    let streams = admission.lanes.checked_add(1)?;
    let lanes = u64::from(admission.lanes);
    let window = u64::from(admission.window);
    let receive = lanes
        .checked_mul(window.checked_add(u64::try_from(ASK_MOST).ok()?)?)?
        .checked_add(u64::try_from(ASK_MOST).ok()?)?;
    let answer = u64::try_from(UNREACHED_BYTES.max(super::REFUSED_BYTES)).ok()?;
    let send = lanes
        .checked_mul(window.checked_add(answer)?)?
        .checked_add(answer)?;
    Some((streams, receive, send))
}

impl super::Lanes {
    /// A lane whose first frame is a socket's ask (`Tag::UnixConnect`) of `len` bytes, `bytes`
    /// what came of it: refused alone where the endpoint serves no socket or the ask is longer
    /// than a name allows; read on as it comes otherwise.
    pub(super) fn begin_socket(
        &mut self,
        io: &mut Io<'_>,
        conn: ConnKey,
        stream: hyper_quic::StreamId,
        bytes: &[u8],
        len: usize,
        ended: bool,
    ) {
        let whole = FRAME_HEAD.saturating_add(len);
        if self.sockets.admission.is_none() {
            self.refuse_lane(io, conn, stream, Reason::Scope);
            return;
        }
        if whole > ASK_MOST {
            self.refuse_lane(io, conn, stream, Reason::Protocol);
            return;
        }
        let mut frame = Vec::new();
        if frame.try_reserve_exact(whole).is_err() || self.sockets.lanes.try_reserve(1).is_err() {
            self.refuse_lane(io, conn, stream, Reason::Closed);
            return;
        }
        frame.extend_from_slice(bytes.get(..whole.min(bytes.len())).unwrap_or_default());
        let id = self.sockets.next;
        self.sockets.next = id.saturating_add(1);
        self.sockets.lanes.push(Lane {
            conn,
            stream,
            id,
            stage: Stage::Asking { frame, whole },
        });
        if ended {
            // Its controller ended its side with the ask: whole, the lane would carry nothing to
            // the socket, and the socket's end would race it; refused as a broken ask.
            let at = self.sockets.lanes.len().saturating_sub(1);
            self.sockets.lanes.swap_remove(at);
            self.refuse_lane(io, conn, stream, Reason::Protocol);
            return;
        }
        self.read_socket(io, conn, stream);
    }

    /// Reads the socket lane `(conn, stream)` as far as its stage reads: its ask, then, carried,
    /// what its socket's writer has room for. Whether it is one.
    pub(super) fn read_socket(
        &mut self,
        io: &mut Io<'_>,
        conn: ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self.sockets.at(conn, stream) else {
            return false;
        };
        let Some(lane) = self.sockets.lanes.get_mut(at) else {
            return true;
        };
        match &mut lane.stage {
            Stage::Asking { frame, whole } => {
                let room = whole.saturating_sub(frame.len());
                let (bytes, ended) = match io.read_within(conn, stream, room) {
                    Ok(Read::Bytes(bytes)) => (bytes, false),
                    Ok(Read::End(bytes)) => (bytes, true),
                    Ok(Read::Blocked) => return true,
                    Ok(Read::Reset) | Err(_) => {
                        self.sockets.lanes.swap_remove(at);
                        super::end(io, conn, stream);
                        return true;
                    }
                };
                frame.extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
                if frame.len() < *whole {
                    if ended {
                        self.sockets.lanes.swap_remove(at);
                        super::end(io, conn, stream);
                    }
                    return true;
                }
                let asked = frame.get(FRAME_HEAD..).map_or(Err(Reason::Protocol), read_ask);
                let asked = if ended { Err(Reason::Protocol) } else { asked };
                // The carried lanes' own news first, a reset its controller sent before this ask
                // among it, so that a lane it has ended counts against the run's most no more.
                self.socket_turn(io);
                let Some(at) = self.sockets.at(conn, stream) else {
                    return true;
                };
                match asked.and_then(|(run, generation, name)| self.admit_socket(run, generation, name, at)) {
                    Ok(()) => {}
                    Err(why) => {
                        self.sockets.lanes.swap_remove(at);
                        self.refuse_lane(io, conn, stream, why);
                    }
                }
            }
            // Its host has yet to answer: the controller is held back by flow control.
            Stage::Asked => {}
            Stage::Carrying(pump) => {
                if pump.up != Way::Open || pump.writer_room == 0 {
                    return true;
                }
                match io.read_within(conn, stream, pump.writer_room) {
                    Ok(Read::Bytes(bytes)) => {
                        pump.writer_room = pump.writer_room.saturating_sub(bytes.len());
                        let _ = pump.to_writer.send(Up::Bytes(bytes));
                    }
                    Ok(Read::End(bytes)) => {
                        pump.writer_room = pump.writer_room.saturating_sub(bytes.len());
                        if !bytes.is_empty() {
                            let _ = pump.to_writer.send(Up::Bytes(bytes));
                        }
                        let _ = pump.to_writer.send(Up::End);
                        pump.up = Way::Done;
                    }
                    Ok(Read::Blocked) => {}
                    // The controller abandoned its way (RESET_STREAM): the socket is ended at
                    // once, and the lane with it.
                    Ok(Read::Reset) | Err(_) => {
                        pump.abort();
                        self.sockets.lanes.swap_remove(at);
                        super::end(io, conn, stream);
                        return true;
                    }
                }
                self.socket_done(io, at);
            }
        }
        true
    }

    /// An ask read whole: refused alone for another run or generation, or past the run's
    /// admitted most; else handed to the host.
    fn admit_socket(&mut self, run: RunId, generation: u64, name: String, at: usize) -> Result<(), Reason> {
        let (ours, our_generation) = self
            .outputs
            .first()
            .map(|o| (o.scope().run, o.scope().generation))
            .unwrap_or((RunId([0; 32]), 0));
        if run != ours {
            return Err(Reason::Scope);
        }
        if generation != our_generation {
            return Err(Reason::Stale);
        }
        let admission = self.sockets.admission.ok_or(Reason::Scope)?;
        let open = self
            .sockets
            .lanes
            .iter()
            .filter(|l| !matches!(l.stage, Stage::Asking { .. }))
            .count();
        if u32::try_from(open).map_or(true, |open| open >= admission.lanes) {
            return Err(Reason::Capacity);
        }
        let wake = self.sockets.wake.as_ref().ok_or(Reason::Closed)?;
        let lane = self.sockets.lanes.get_mut(at).ok_or(Reason::Closed)?;
        let connect = Connect {
            name,
            lane: lane.id,
            replies: self.sockets.replies_tx.clone(),
            wake: wake.try_clone().map_err(|_| Reason::Closed)?,
        };
        let handed = self
            .sockets
            .asks
            .as_ref()
            .is_some_and(|asks| asks.send(connect).is_ok());
        if !handed {
            return Err(Reason::Closed);
        }
        lane.stage = Stage::Asked;
        Ok(())
    }

    /// The controller reads no more of lane `(conn, stream)` (STOP_SENDING): its socket is
    /// ended at once, and the lane with it. Whether it is one.
    pub(super) fn stop_socket(
        &mut self,
        io: &mut Io<'_>,
        conn: ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self.sockets.at(conn, stream) else {
            return false;
        };
        let lane = self.sockets.lanes.swap_remove(at);
        if let Stage::Carrying(pump) = &lane.stage {
            pump.abort();
        }
        super::end(io, conn, stream);
        true
    }

    /// A connection gone: every socket lane of it is ended at once.
    pub(super) fn forget_sockets(&mut self, conn: ConnKey) {
        self.sockets.lanes.retain(|l| {
            if l.conn != conn {
                return true;
            }
            if let Stage::Carrying(pump) = &l.stage {
                pump.abort();
            }
            false
        });
    }

    /// A turn for the socket lanes: each host's answer told, and each carried lane's bytes moved
    /// as far as either side has room.
    pub(super) fn socket_turn(&mut self, io: &mut Io<'_>) {
        while let Ok((id, reply)) = self.sockets.replies.try_recv() {
            self.answered(io, id, reply);
        }
        let mut at = 0;
        while at < self.sockets.lanes.len() {
            let before = self.sockets.lanes.len();
            let carried = self
                .sockets
                .lanes
                .get(at)
                .is_some_and(|l| matches!(l.stage, Stage::Carrying(_)));
            if carried {
                self.socket_said(io, at);
                if self.sockets.lanes.len() == before {
                    if let Some((conn, stream)) = self.sockets.lanes.get(at).map(|l| (l.conn, l.stream)) {
                        self.read_socket(io, conn, stream);
                    }
                }
            }
            if self.sockets.lanes.len() == before {
                at += 1;
            }
        }
    }

    /// The host's answer to lane `id`: `Unreached` told and the lane ended; or `Connected` told,
    /// its socket's writer and reader started, and the lane carried. One whose lane has gone
    /// meanwhile has its socket closed.
    fn answered(&mut self, io: &mut Io<'_>, id: u64, reply: Reply) {
        let Some(at) = self
            .sockets
            .lanes
            .iter()
            .position(|l| l.id == id && matches!(l.stage, Stage::Asked))
        else {
            if let Reply::Connected(socket) = reply {
                shards_ipc::end(&socket);
            }
            return;
        };
        let Some((conn, stream)) = self.sockets.lanes.get(at).map(|l| (l.conn, l.stream)) else {
            return;
        };
        let socket = match reply {
            Reply::Unreached(errno) => {
                self.sockets.lanes.swap_remove(at);
                self.answer(io, conn, stream, &unreached(errno));
                return;
            }
            Reply::Connected(socket) => socket,
        };
        let (Some(admission), Some(wake)) = (self.sockets.admission, self.sockets.wake.as_ref()) else {
            return;
        };
        match pump(socket, admission, wake) {
            Ok(pump) => {
                if let Some(lane) = self.sockets.lanes.get_mut(at) {
                    lane.stage = Stage::Carrying(Box::new(pump));
                }
                self.socket_said(io, at);
                self.read_socket(io, conn, stream);
            }
            Err(socket) => {
                shards_ipc::end(&socket);
                self.sockets.lanes.swap_remove(at);
                self.refuse_lane(io, conn, stream, Reason::Closed);
            }
        }
    }

    /// What lane `at`'s writer and reader said, taken: room for the writer, the reader's bytes
    /// queued and sent as far as the stream takes them, the socket's end its FIN once all has
    /// gone, and a failure the lane's reset.
    fn socket_said(&mut self, io: &mut Io<'_>, at: usize) {
        let Some(lane) = self.sockets.lanes.get_mut(at) else {
            return;
        };
        let (conn, stream) = (lane.conn, lane.stream);
        let Stage::Carrying(pump) = &mut lane.stage else {
            return;
        };
        let mut failed = false;
        while let Ok(said) = pump.said.try_recv() {
            match said {
                Said::Wrote(n) => pump.writer_room = pump.writer_room.saturating_add(n),
                Said::Read(bytes) => {
                    if pump.out.try_reserve(bytes.len()).is_err() {
                        failed = true;
                        break;
                    }
                    pump.out.extend_from_slice(&bytes);
                }
                Said::Ended => pump.down = Way::Ending,
                Said::Failed => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            pump.abort();
            self.sockets.lanes.swap_remove(at);
            super::end(io, conn, stream);
            return;
        }
        while let Some(rest) = pump.out.get(pump.sent..).filter(|r| !r.is_empty()) {
            match io.write(conn, stream, rest) {
                Ok(0) => break,
                Ok(n) => {
                    pump.sent = pump.sent.saturating_add(n);
                    // The answer's bytes go first; the rest were the reader's, taken by the
                    // stream: room for as many more.
                    let answer = n.min(pump.answer);
                    pump.answer = pump.answer.saturating_sub(answer);
                    let read = n.saturating_sub(answer);
                    if read > 0 {
                        let _ = pump.to_reader.send(read);
                    }
                }
                Err(_) => {
                    pump.abort();
                    self.sockets.lanes.swap_remove(at);
                    super::end(io, conn, stream);
                    return;
                }
            }
        }
        if pump.sent == pump.out.len() {
            pump.out.clear();
            pump.sent = 0;
            if pump.down == Way::Ending {
                let _ = io.finish(conn, stream);
                pump.down = Way::Done;
            }
        }
        self.socket_done(io, at);
    }

    /// Lane `at` let go once both its ways are done: what its writer still holds it writes,
    /// then its end; nothing more of the lane is read.
    fn socket_done(&mut self, io: &mut Io<'_>, at: usize) {
        let done = self.sockets.lanes.get(at).is_some_and(
            |l| matches!(&l.stage, Stage::Carrying(p) if p.up == Way::Done && p.down == Way::Done),
        );
        if done {
            let lane = self.sockets.lanes.swap_remove(at);
            io.stop(lane.conn, lane.stream);
        }
    }
}

/// `socket`'s pump: its writer and reader on threads of their own, each bounded by the window,
/// and `Connected` the first of what goes to the controller. The socket back where a thread
/// cannot be had.
fn pump(
    socket: shards_ipc::Stream,
    admission: Admission,
    wake: &Sender<()>,
) -> Result<Pump, shards_ipc::Stream> {
    let window = usize::try_from(admission.window).unwrap_or(usize::MAX);
    let (Ok(writing), Ok(reading), Ok(writer_wake), Ok(reader_wake)) = (
        socket.try_clone(),
        socket.try_clone(),
        wake.try_clone(),
        wake.try_clone(),
    ) else {
        return Err(socket);
    };
    let (to_writer, ups) = mpsc::channel();
    let (said_tx, said) = mpsc::channel();
    let (to_reader, room) = mpsc::channel();
    let writer_said = said_tx.clone();
    let writer = std::thread::Builder::new()
        .name("socket-lane-up".into())
        .spawn(move || write_up(&writing, &ups, &writer_said, &writer_wake));
    if writer.is_err() {
        return Err(socket);
    }
    let reader = std::thread::Builder::new()
        .name("socket-lane-down".into())
        .spawn(move || read_down(&reading, window, &room, &said_tx, &reader_wake));
    if reader.is_err() {
        // The writer ends as its sender goes.
        return Err(socket);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&connected());
    Ok(Pump {
        socket,
        to_writer,
        writer_room: window,
        said,
        to_reader,
        out,
        sent: 0,
        answer: CONNECTED_BYTES,
        up: Way::Open,
        down: Way::Open,
    })
}

fn tell(said: &mpsc::Sender<Said>, wake: &Sender<()>, what: Said) -> bool {
    let sent = said.send(what).is_ok();
    let _ = wake.try_send(Command::Handler(()));
    sent
}

/// The socket's writer: what the controller sent, written in order, each write's room told
/// back; its FIN the socket's write side shut. A write that fails fails the lane.
fn write_up(
    socket: &shards_ipc::Stream,
    ups: &mpsc::Receiver<Up>,
    said: &mpsc::Sender<Said>,
    wake: &Sender<()>,
) {
    let mut socket = socket;
    for up in ups {
        match up {
            Up::Bytes(bytes) => {
                if socket.write_all(&bytes).is_err() {
                    tell(said, wake, Said::Failed);
                    return;
                }
                if !tell(said, wake, Said::Wrote(bytes.len())) {
                    return;
                }
            }
            Up::End => {
                let _ = socket.shutdown(std::net::Shutdown::Write);
                return;
            }
        }
    }
}

/// The socket's reader: what the socket says, at most the window unsent at once, each read
/// told; its end the lane's FIN, and a read that fails its reset.
fn read_down(
    socket: &shards_ipc::Stream,
    window: usize,
    room: &mpsc::Receiver<usize>,
    said: &mpsc::Sender<Said>,
    wake: &Sender<()>,
) {
    let mut free = window;
    loop {
        while let Ok(n) = room.try_recv() {
            free = free.saturating_add(n).min(window);
        }
        if free == 0 {
            match room.recv() {
                Ok(n) => free = free.saturating_add(n).min(window),
                Err(_) => return,
            }
            continue;
        }
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(free).is_err() {
            tell(said, wake, Said::Failed);
            return;
        }
        buffer.resize(free, 0);
        match shards_ipc::read(socket, &mut buffer) {
            Ok(0) => {
                tell(said, wake, Said::Ended);
                return;
            }
            Ok(n) => {
                buffer.truncate(n);
                free = free.saturating_sub(n);
                if !tell(said, wake, Said::Read(buffer)) {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                tell(said, wake, Said::Failed);
                return;
            }
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    //! Hex's acceptance for the socket lane (joint plan step 39; the Hex lead's
    //! step11-a-socket-lane/red/control_unix_tests.rs), its names this tree's: the endpoint's host
    //! played by a test thread that connects a real AF_UNIX listener as shards-init's relay
    //! does, and the workload a listener in a test directory, as Hex's relay is in
    //! `/run/hex/channel`.

    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::num::NonZeroU16;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use super::{Admission, Connect, Reply};
    use crate::control::{
        Controller, Dialed, Ended, Endpoint, Fault, Revoke, Socket, Streams, Subscriptions,
    };
    use crate::identity::{Fingerprint, Identity};
    use crate::managed::{Reason, RunId};

    /// What the run's request admits: `lanes` at once, `window` bytes in flight each way per
    /// lane; the cases derive their byte counts from them.
    fn admission(lanes: u32) -> Admission {
        Admission {
            lanes,
            window: 64 * 1024,
        }
    }

    fn serving(
        vm: &Identity,
        controller: Fingerprint,
        sockets: Admission,
    ) -> (SocketAddr, mpsc::Receiver<Connect>, Revoke) {
        let (endpoint, served) = Endpoint::bind(
            vm,
            controller,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            Streams {
                outputs: &[],
                inputs: &[],
                execs: None,
                sockets: Some(sockets),
            },
        )
        .unwrap();
        let address = endpoint.address;
        std::thread::spawn(move || endpoint.run());
        (
            address,
            served.sockets.expect("socket lanes admitted"),
            served.revoke,
        )
    }

    /// The host as shards-init's relay plays it: each ask's name taken in `dir`, connected,
    /// answered with the stream or the connect's errno.
    fn host(dir: std::path::PathBuf, asks: mpsc::Receiver<Connect>) -> JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for ask in asks {
                seen.push(ask.name().to_owned());
                let reply = match UnixStream::connect(dir.join(ask.name())) {
                    Ok(stream) => Reply::Connected(stream),
                    Err(e) => Reply::Unreached(NonZeroU16::new(e.raw_os_error().unwrap() as u16).unwrap()),
                };
                ask.reply(reply).unwrap();
            }
            seen
        })
    }

    fn dial(own: &Identity, vm: Fingerprint, at: SocketAddr, sockets: Admission) -> Dialed {
        let (controller, dialed) = Controller::dial(
            own,
            vm,
            at,
            Subscriptions {
                outputs: &[],
                inputs: &[],
                execs: None,
                sockets: Some(sockets),
            },
        )
        .unwrap();
        std::thread::spawn(move || controller.run());
        dialed
    }

    /// Bytes no text holds, every byte value, filling `window` three times and one more, so
    /// that the lane's credit turns over several times and one record is cut short.
    fn pattern(window: u32) -> Vec<u8> {
        (0..=255u8).cycle().take(3 * window as usize + 1).collect()
    }

    /// The workload Hex runs: reads its connection to its end (the controller's half-close),
    /// writes back what it read, reversed, and ends its own half.
    fn echo_reversed(listener: UnixListener) -> JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            conn.read_to_end(&mut got).unwrap();
            let back: Vec<u8> = got.iter().rev().copied().collect();
            conn.write_all(&back).unwrap();
            conn.shutdown(std::net::Shutdown::Write).unwrap();
            got
        })
    }

    #[test]
    fn a_socket_lane_is_the_workloads_socket_exactly_both_ways_each_half_closed() {
        let dir = shards_testdir::TempDir::new("sl").unwrap();
        let workload = echo_reversed(UnixListener::bind(dir.path().join("control.sock")).unwrap());
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (at, asks, revoke) = serving(&vm, hex.fingerprint(), admission(4));
        let host = host(dir.path().to_owned(), asks);
        let dialed = dial(&hex, vm.fingerprint(), at, admission(4));
        let sockets = dialed.sockets.expect("socket lanes subscribed");

        let sent = pattern(admission(4).window);
        let mut lane: Socket = sockets.connect("control.sock").unwrap();
        lane.write_all(&sent).unwrap();
        lane.shutdown_write().unwrap();
        let mut back = Vec::new();
        assert_eq!(lane.read_to_end(&mut back).unwrap(), sent.len());
        assert_eq!(
            workload.join().unwrap(),
            sent,
            "the workload read the controller's bytes exactly"
        );
        let expected: Vec<u8> = sent.iter().rev().copied().collect();
        assert_eq!(back, expected, "the controller read the workload's bytes exactly");
        drop(sockets);
        drop(lane);
        // The endpoint's end ends its asks, and its host with them.
        drop(revoke);
        assert_eq!(host.join().unwrap(), ["control.sock"]);
    }

    #[test]
    fn a_lane_to_a_socket_not_listening_is_unreached_by_its_errno() {
        let dir = shards_testdir::TempDir::new("su").unwrap();
        // A socket file whose listener is gone: connect(2) on it is ECONNREFUSED.
        drop(UnixListener::bind(dir.path().join("gone.sock")).unwrap());
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (at, asks, _revoke) = serving(&vm, hex.fingerprint(), admission(4));
        let _host = host(dir.path().to_owned(), asks);
        let sockets = dial(&hex, vm.fingerprint(), at, admission(4)).sockets.unwrap();
        let errno = |e: i32| Err(Fault::Unreached(NonZeroU16::new(e as u16).unwrap()));
        assert_eq!(sockets.connect("absent.sock").map(|_| ()), errno(libc::ENOENT));
        assert_eq!(
            sockets.connect("gone.sock").map(|_| ()),
            errno(libc::ECONNREFUSED)
        );
    }

    #[test]
    fn a_name_that_is_no_file_name_is_refused_before_its_host_is_asked() {
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (at, asks, _revoke) = serving(&vm, hex.fingerprint(), admission(4));
        let sockets = dial(&hex, vm.fingerprint(), at, admission(4)).sockets.unwrap();
        let long = "x".repeat(256);
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "../control.sock",
            "/run/hex/channel/control.sock",
            "nul\0x",
            long.as_str(),
        ] {
            assert_eq!(
                sockets.connect(name).map(|_| ()),
                Err(Fault::Refused(Reason::Protocol)),
                "{name:?}"
            );
        }
        assert!(
            matches!(asks.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "its host was asked"
        );
    }

    #[test]
    fn a_reset_either_way_reaches_the_other_side() {
        let dir = shards_testdir::TempDir::new("sr").unwrap();
        let listener = UnixListener::bind(dir.path().join("files.sock")).unwrap();
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (at, asks, _revoke) = serving(&vm, hex.fingerprint(), admission(4));
        let _host = host(dir.path().to_owned(), asks);
        let sockets = dial(&hex, vm.fingerprint(), at, admission(4)).sockets.unwrap();

        let closer = std::thread::spawn({
            let listener = listener.try_clone().unwrap();
            move || drop(listener.accept().unwrap())
        });
        let mut lane = sockets.connect("files.sock").unwrap();
        closer.join().unwrap();
        let sent = pattern(admission(4).window);
        let wrote = lane.write_all(&sent).and_then(|()| lane.flush());
        let mut rest = Vec::new();
        let read = lane.read_to_end(&mut rest);
        assert!(
            wrote.is_err() || read.is_err(),
            "a workload gone while the controller sent read as an orderly end: wrote {wrote:?}, read {read:?}"
        );

        let reader = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            conn.read_to_end(&mut got).map(|_| got.len())
        });
        let mut lane = sockets.connect("files.sock").unwrap();
        lane.write_all(b"partial").unwrap();
        lane.abort().unwrap();
        // The workload's read ends (EOF or ECONNRESET), whichever its socket says.
        let _ = reader.join().unwrap();
    }

    #[test]
    fn lanes_past_the_admitted_most_are_refused_until_one_ends() {
        let dir = shards_testdir::TempDir::new("sc").unwrap();
        let listener = UnixListener::bind(dir.path().join("stdio.sock")).unwrap();
        let holder = std::thread::spawn(move || {
            let held: Vec<UnixStream> = (0..3).map(|_| listener.accept().unwrap().0).collect();
            held
        });
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (at, asks, _revoke) = serving(&vm, hex.fingerprint(), admission(2));
        let _host = host(dir.path().to_owned(), asks);
        let sockets = dial(&hex, vm.fingerprint(), at, admission(2)).sockets.unwrap();
        let first = sockets.connect("stdio.sock").unwrap();
        let _second = sockets.connect("stdio.sock").unwrap();
        assert_eq!(
            sockets.connect("stdio.sock").map(|_| ()),
            Err(Fault::Refused(Reason::Capacity))
        );
        let mut first = first;
        first.abort().unwrap();
        drop(first);
        // Room comes back as the endpoint lets the aborted lane go: the next connect is taken.
        let _third = sockets.connect("stdio.sock").unwrap();
        drop(holder);
    }

    #[test]
    fn a_stranger_reaches_no_socket() {
        let (vm, hex, stranger) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (at, asks, _revoke) = serving(&vm, hex.fingerprint(), admission(4));
        let dialed = dial(&stranger, vm.fingerprint(), at, admission(4));
        let sockets = dialed.sockets.unwrap();
        assert!(matches!(
            sockets.connect("control.sock").map(|_| ()),
            Err(Fault::Lost(Ended::NotEnrolled(_)))
        ));
        assert!(matches!(asks.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    /// An endpoint that admits no socket lane refuses one by `Scope`, as a daemon's run's does,
    /// its connection and other lanes going on.
    #[test]
    fn an_endpoint_of_no_socket_lanes_refuses_one_by_scope() {
        use crate::managed::{Admitted, Scope};
        use shards_abi::run::managed::Channel;
        let (vm, hex) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let scope = Scope {
            run: RunId([7; 32]),
            generation: 1,
            member: 0,
            channel: Channel::Stdout,
        };
        let admitted = Admitted {
            record_bytes: 1024,
            transport_bytes: 64 * 1024,
            handshake_bytes: super::HANDSHAKE_MOST,
            datagram_bytes: crate::managed::DATAGRAM_BYTES,
            patience: crate::tls::QUIET,
        };
        let (endpoint, served) = Endpoint::bind(
            &vm,
            hex.fingerprint(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            Streams {
                outputs: &[(scope, admitted)],
                inputs: &[],
                execs: None,
                sockets: None,
            },
        )
        .unwrap();
        assert!(served.sockets.is_none());
        let at = endpoint.address;
        std::thread::spawn(move || endpoint.run());
        let (controller, dialed) = Controller::dial(
            &hex,
            vm.fingerprint(),
            at,
            Subscriptions {
                outputs: &[(scope, admitted, 0)],
                inputs: &[],
                execs: None,
                sockets: Some(admission(1)),
            },
        )
        .unwrap();
        std::thread::spawn(move || controller.run());
        let sockets = dialed.sockets.unwrap();
        assert_eq!(
            sockets.connect("control.sock").map(|_| ()),
            Err(Fault::Refused(Reason::Scope))
        );
        drop(served);
    }

    /// An ask and the endpoint's answers read as written; an ask whose name is no file name, or
    /// cut short, is a protocol error at the endpoint too, whoever framed it.
    #[test]
    fn asks_and_answers_read_as_written() {
        let run = RunId([3; 32]);
        let frame = super::ask_frame(run, 9, "stamps.sock").unwrap();
        let (tag, len) = crate::frame::peek(&frame).unwrap().unwrap();
        assert_eq!(tag, crate::frame::Tag::UnixConnect);
        assert_eq!(
            super::read_ask(&frame[5..5 + len]),
            Ok((run, 9, "stamps.sock".to_owned()))
        );
        assert!(frame.len() <= super::ASK_MOST);
        assert_eq!(super::ask_frame(run, 9, "a/b"), Err(Reason::Protocol));
        for name in [&b""[..], b".", b"..", b"a/b", b"n\0l", &[0xff]] {
            let payload = [&run.0[..], &9u64.to_be_bytes(), name].concat();
            assert_eq!(super::read_ask(&payload), Err(Reason::Protocol), "{name:?}");
        }
        assert_eq!(super::read_ask(&[0; 39]), Err(Reason::Protocol));
        assert_eq!(
            super::read_answer(&super::connected()),
            Ok(Some((Ok(()), super::CONNECTED_BYTES)))
        );
        let enoent = NonZeroU16::new(2).unwrap();
        assert_eq!(
            super::read_answer(&super::unreached(enoent)),
            Ok(Some((Err(enoent), super::UNREACHED_BYTES)))
        );
        assert_eq!(super::read_answer(&super::unreached(enoent)[..6]), Ok(None));
        assert_eq!(super::read_answer(&[40, 0, 0, 0, 1, 0]), Err(()));
        assert_eq!(super::read_answer(&[41, 0, 0, 0, 4, 0, 0, 0, 0]), Err(()));
        assert!(Admission { lanes: 0, window: 1 }.check().is_err());
        assert!(Admission { lanes: 1, window: 0 }.check().is_err());
        let (streams, receive, send) = super::added(admission(2)).unwrap();
        assert_eq!(streams, 3);
        assert!(receive >= 2 * 64 * 1024 && send >= 2 * 64 * 1024);
    }
}
