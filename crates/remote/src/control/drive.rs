//! The controller's side of the agent-control endpoint (plan step 11, D238): the one connection
//! kiln or Hex holds to a microVM's endpoint, and the lanes it opens on it to the microVM's
//! members' stdio, so that neither writes this wire again ("a controller's side is Shards'
//! code", D238).
//!
//! - **Its lanes.** Each opens with step 5's subscription and carries step 5's frames: an
//!   output's records, one in flight, each acknowledged once its caller asks for the next
//!   ([`Output::next_record`]); a stdin's records, each within the room the endpoint said and taken
//!   before the next goes, then its end or abort ([`Input`]). Attaching to a member is this:
//!   its stdio as step 5 serves it.
//! - **Who it reaches.** No lane opens before the handshake took the microVM's pinned key
//!   ([`controller`](super::controller)), and no more open at once than the endpoint allows
//!   (MAX_STREAMS, RFC 9000 §4.6): the rest wait for its room.
//! - **Nothing retried.** A lane's refusal, its reset and the connection's end are typed
//!   ([`Fault`]). A caller that dials again opens each lane at its handle's cursor: the endpoint
//!   keeps an unacknowledged record and a stdin's place for the next subscription (step 5).
//! - **No value of its own** (R03). Every transport value is the endpoint's per-stream
//!   admission's, as the run's managed metadata gives it, summed over the lanes as the endpoint
//!   sums them ([`super::Endpoint::bind`]).

use std::mem::size_of;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::mpsc;
use std::time::Instant;

use hyper_quic::{ClientConfig, ConnectionError, StreamId};
use shards_abi::run::managed::InputEnd;

use super::{By, Ended, REFUSED_BYTES, controller, ended, read_refused};
use crate::frame::{self, Tag};
use crate::identity::{self, Fingerprint, Identity};
use crate::managed::input::{self, Said};
use crate::managed::{self, AdmissionError, Admitted, CONTROL_BYTES, DATA_HEAD, Reason, Scope, Windows};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
const OFFSET: usize = size_of::<u64>();
/// An output's acknowledgment: a frame's head and the offset it consumed to.
const ACK_BYTES: usize = FRAME_HEAD + OFFSET;

/// The member streams a controller drives on one connection, each with the admission its
/// endpoint announced for it (the run's managed metadata) and where its lane opens: an output at
/// the first byte its controller has not consumed, a stdin at the first it has not seen taken.
#[derive(Debug, Clone, Copy)]
pub struct Subscriptions<'a> {
    pub outputs: &'a [(Scope, Admitted, u64)],
    pub inputs: &'a [(input::Scope, Admitted, u64)],
}

/// Why a lane gives no more, typed: what callers match, never words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The endpoint refused this lane alone, as step 5's endpoints refuse a subscription.
    Refused(Reason),
    /// The endpoint abandoned this lane (RESET_STREAM): its stream was revoked or aborted.
    Revoked,
    /// The member's stdin has ended, having taken `written` bytes, as its init told it.
    InputEnded { written: u64, end: InputEnd },
    /// The connection ended ([`Ended`]): this side's own close is [`Ended::Local`].
    Lost(Ended),
}

/// What an output's lane gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Got {
    /// A record of the member's output, whose first byte is at offset `at`.
    Record { at: u64, bytes: Vec<u8> },
    /// The output finished: every byte of it consumed.
    Finished,
}

/// What the driver tells a lane's handle.
#[derive(Debug)]
enum Heard {
    Record { at: u64, bytes: Vec<u8> },
    Finished,
    Taken { accepted: u64, room: u32 },
    Fault(Fault),
}

/// What a lane's handle asks of the driver: at most one at a time each, as step 5 has one
/// record in flight each way.
#[derive(Debug)]
enum Ask {
    /// An output's records to `offset` consumed.
    Consumed { lane: usize, offset: u64 },
    /// A stdin's frame: a record, or its end or abort.
    Send { lane: usize, frame: Vec<u8> },
}

/// The handle's view of a lane gone with its driver: the driver ended, which this side did.
fn gone() -> Fault {
    Fault::Lost(Ended::Local)
}

/// Hands `ask` to the driver. A driver that ended told the lane why first, which its handle
/// hears next; a full handoff cannot be, with one ask in flight per lane and a slot for each.
fn hand(asks: &Sender<Ask>, ask: Ask) -> Result<(), Fault> {
    match asks.try_send(Command::Handler(ask)) {
        Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => Ok(()),
        Err(mpsc::TrySendError::Full(_)) => Err(gone()),
    }
}

/// A member's stdout or stderr, as its controller reads it.
#[derive(Debug)]
pub struct Output {
    lane: usize,
    asks: Sender<Ask>,
    heard: mpsc::Receiver<Heard>,
    cursor: u64,
    /// The end of the record handed out and not yet acknowledged.
    held: Option<u64>,
    over: Option<Result<(), Fault>>,
}

impl Output {
    /// The output's next record, the one handed out before it acknowledged first: asking for
    /// the next says the caller has the last whole (step 5 acknowledges only that). Waits as
    /// long as the member writes nothing. [`Got::Finished`] once the output has finished, then
    /// again; a [`Fault`] once the lane is over, then again.
    pub fn next_record(&mut self) -> Result<Got, Fault> {
        if let Some(offset) = self.held.take() {
            self.cursor = offset;
            let ask = Ask::Consumed {
                lane: self.lane,
                offset,
            };
            if let Err(fault) = hand(&self.asks, ask) {
                self.over = Some(Err(fault));
            }
        }
        if let Some(over) = self.over {
            return over.map(|()| Got::Finished);
        }
        let (got, over) = match self.heard.recv() {
            Ok(Heard::Record { at, bytes }) => {
                // The driver took it only at this lane's next offset, of an admitted length.
                self.held = at.checked_add(bytes.len() as u64);
                (Ok(Got::Record { at, bytes }), None)
            }
            Ok(Heard::Finished) => (Ok(Got::Finished), Some(Ok(()))),
            Ok(Heard::Fault(fault)) => (Err(fault), Some(Err(fault))),
            Ok(Heard::Taken { .. }) | Err(_) => (Err(gone()), Some(Err(gone()))),
        };
        self.over = over;
        got
    }

    /// The first byte not yet acknowledged: where a later dial's lane opens.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

/// A member's stdin, as its controller writes it.
#[derive(Debug)]
pub struct Input {
    lane: usize,
    asks: Sender<Ask>,
    heard: mpsc::Receiver<Heard>,
    cursor: u64,
    /// Where the endpoint said the next record starts and how long it may be.
    room: Option<(u64, u32)>,
    over: Option<Fault>,
}

impl Input {
    /// Every byte before it is taken: where this stdin's next bytes start, and where a later
    /// dial's lane opens.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Writes the first bytes of `bytes`, which start at [`Input::cursor`], as one record, as
    /// many as the endpoint has room for, and waits until it has taken them: how many it took.
    /// Bytes an earlier connection's record gave the member already are counted, not sent again
    /// (step 5: a new subscription is told where to go on, which may be past its cursor).
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, Fault> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let (accepted, room) = self.room()?;
        if accepted > self.cursor {
            let taken = usize::try_from(accepted.saturating_sub(self.cursor))
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            self.cursor = self.cursor.saturating_add(taken as u64);
            return Ok(taken);
        }
        let len = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
        let mut frame = Vec::new();
        input::record(accepted, bytes.get(..len).unwrap_or_default(), &mut frame)
            .map_err(|_| self.fail(gone()))?;
        self.send(frame)?;
        self.room = None;
        // The driver took that taken only where this record ends (`Pace`).
        let (next, _) = self.room()?;
        self.cursor = next;
        Ok(len)
    }

    /// Ends the member's stdin once all before it is taken (step 5's end, errno 0), and waits
    /// for the member's init to tell how it went: how many bytes its stdin took.
    pub fn end(&mut self) -> Result<(u64, InputEnd), Fault> {
        let (accepted, _) = self.room()?;
        self.send(input::end(accepted, 0).to_vec())?;
        self.ended()
    }

    /// Aborts the member's stdin with `errno`, whatever is in flight, and waits for the member's
    /// init to tell how it went. An errno is positive and below Linux's MAX_ERRNO, 4095
    /// (include/linux/err.h), which a u16 holds.
    pub fn abort(&mut self, errno: NonZeroU16) -> Result<(u64, InputEnd), Fault> {
        if let Some(fault) = self.over {
            return ended_or(fault);
        }
        self.send(input::end(self.cursor, i32::from(errno.get())).to_vec())?;
        self.ended()
    }

    /// Waits for what the endpoint says next of the stdin: the room for its next record, or its
    /// end, which is a [`Fault::InputEnded`] here.
    fn room(&mut self) -> Result<(u64, u32), Fault> {
        loop {
            if let Some(fault) = self.over {
                return Err(fault);
            }
            if let Some(room) = self.room {
                return Ok(room);
            }
            match self.heard.recv() {
                Ok(Heard::Taken { accepted, room }) => self.room = Some((accepted, room)),
                Ok(Heard::Fault(fault)) => {
                    self.over = Some(fault);
                }
                Ok(Heard::Record { .. } | Heard::Finished) | Err(_) => {
                    self.over = Some(gone());
                }
            }
        }
    }

    /// Waits for the stdin's end as the member's init tells it.
    fn ended(&mut self) -> Result<(u64, InputEnd), Fault> {
        self.room = None;
        loop {
            match self.room() {
                Ok(_) => self.room = None,
                Err(fault) => return ended_or(fault),
            }
        }
    }

    fn send(&mut self, frame: Vec<u8>) -> Result<(), Fault> {
        let ask = Ask::Send {
            lane: self.lane,
            frame,
        };
        hand(&self.asks, ask).map_err(|fault| self.fail(fault))
    }

    fn fail(&mut self, fault: Fault) -> Fault {
        self.over = Some(fault);
        fault
    }
}

/// The stdin's end, or what ended its lane instead.
fn ended_or(fault: Fault) -> Result<(u64, InputEnd), Fault> {
    match fault {
        Fault::InputEnded { written, end } => Ok((written, end)),
        other => Err(other),
    }
}

/// Ends the controller's connection and every lane on it: the microVM's control is over here.
/// Dropping it does so too.
#[derive(Debug)]
pub struct Hangup {
    sender: Sender<Ask>,
}

impl Hangup {
    pub fn hangup(&self) {
        self.sender.request_stop();
    }
}

impl Drop for Hangup {
    fn drop(&mut self) {
        self.hangup();
    }
}

/// The handles of what a controller dialed, in the order its [`Subscriptions`] named them.
#[derive(Debug)]
pub struct Dialed {
    pub outputs: Vec<Output>,
    pub inputs: Vec<Input>,
    pub hangup: Hangup,
}

/// A controller's connection to one microVM's agent-control endpoint: run its driver on a
/// thread of its own, and keep its [`Hangup`].
pub struct Controller {
    driver: Driver<Drive>,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controller").finish_non_exhaustive()
    }
}

impl Controller {
    /// Dials the endpoint at `to` of the microVM whose key's SHA-256 is `vm`, under the
    /// controller's own key `own`, for `subscriptions`; their lanes open once it is connected.
    pub fn dial(
        own: &Identity,
        vm: Fingerprint,
        to: SocketAddr,
        subscriptions: Subscriptions<'_>,
    ) -> Result<(Controller, Dialed), managed::Error> {
        let (link, windows) = windows(subscriptions)?;
        let config = controller(own, vm, managed::transport_of(&link, windows)?)?;
        let allocation = || managed::Error::Admission(AdmissionError::Allocation);
        let count = subscriptions
            .outputs
            .len()
            .checked_add(subscriptions.inputs.len())
            .ok_or_else(allocation)?;
        let mut lanes = Vec::new();
        lanes.try_reserve_exact(count).map_err(|_| allocation())?;
        let mut receivers = Vec::new();
        receivers.try_reserve_exact(count).map_err(|_| allocation())?;
        for (scope, admitted, from) in subscriptions.outputs {
            let most = admitted
                .record_bytes
                .checked_add(DATA_HEAD)
                .ok_or(managed::Error::Admission(AdmissionError::Window))?;
            let (lane, heard) = Lane::new(
                Carries::Output {
                    most,
                    next: *from,
                    held: false,
                },
                &managed::subscribe(*scope, *from),
                CONTROL_BYTES.saturating_add(ACK_BYTES),
                most.max(REFUSED_BYTES),
            )?;
            lanes.push(lane);
            receivers.push((heard, *from));
        }
        for (scope, admitted, from) in subscriptions.inputs {
            let sends = input_sends(admitted)?;
            let (lane, heard) = Lane::new(
                Carries::Input {
                    pace: Pace::First { from: *from },
                },
                &input::subscribe(*scope, *from),
                sends,
                input_receives(),
            )?;
            lanes.push(lane);
            receivers.push((heard, *from));
        }
        let drive = Drive {
            to,
            vm,
            config: Some(config),
            since: Instant::now(),
            conn: None,
            cause: None,
            lanes,
            over: false,
        };
        let any: SocketAddr = if to.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        let (driver, sender) = Driver::bind_bounded(
            any,
            None,
            drive,
            DriverLimits {
                connections: 1,
                // Each handle has one ask in flight at most (Ask).
                command_slots: count,
                queued_datagram_bytes: link.datagram_bytes,
                handshake_patience: link.patience,
            },
        )?;
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(subscriptions.outputs.len())
            .map_err(|_| allocation())?;
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(subscriptions.inputs.len())
            .map_err(|_| allocation())?;
        for (lane, (heard, from)) in receivers.into_iter().enumerate() {
            let asks = sender.try_clone()?;
            if lane < subscriptions.outputs.len() {
                outputs.push(Output {
                    lane,
                    asks,
                    heard,
                    cursor: from,
                    held: None,
                    over: None,
                });
            } else {
                inputs.push(Input {
                    lane,
                    asks,
                    heard,
                    cursor: from,
                    room: None,
                    over: None,
                });
            }
        }
        Ok((
            Controller { driver },
            Dialed {
                outputs,
                inputs,
                hangup: Hangup { sender },
            },
        ))
    }

    /// Drives the connection until it ends or is hung up; then every lane not over is told so.
    pub fn run(mut self) -> Result<(), managed::Error> {
        let result = self.driver.run();
        self.driver.handler_mut().lose(Ended::Local);
        result.map_err(managed::Error::Transport)
    }
}

/// What a stdin's lane sends at most at once: its subscription, one record of its admitted
/// length and its end (step 5's owner, `input::client_config`).
fn input_sends(admitted: &Admitted) -> Result<usize, managed::Error> {
    input::SUBSCRIBE_BYTES
        .checked_add(input::RECORD_HEAD)
        .and_then(|n| n.checked_add(admitted.record_bytes))
        .and_then(|n| n.checked_add(input::END_BYTES))
        .ok_or(managed::Error::Admission(AdmissionError::Window))
}

/// The largest frame the endpoint sends on a stdin's lane: its taken, its end, or a refusal.
fn input_receives() -> usize {
    input::TAKEN_BYTES.max(input::ENDED_BYTES).max(REFUSED_BYTES)
}

/// Where the stdin record `frame` ends, for one (`input::record`'s): its offset and length.
fn record_end(frame: &[u8]) -> Option<u64> {
    let (tag, len) = frame::peek(frame).ok().flatten()?;
    if tag != Tag::Stdin {
        return None;
    }
    let at = frame.get(FRAME_HEAD..input::RECORD_HEAD)?;
    let at = u64::from_be_bytes(<[u8; OFFSET]>::try_from(at).ok()?);
    at.checked_add(u64::try_from(len.checked_sub(OFFSET)?).ok()?)
}

/// The connection's admission and windows from the subscriptions', as the endpoint sums its
/// own over the same streams: each stream's admission checked as its endpoint checks it; what
/// the endpoint sends on a lane before it is read (a whole record or a stdin's word, a refusal
/// at most otherwise), the largest per lane and all of them summed; what the controller sends
/// unacknowledged on each, summed; no stream the endpoint may open (it opens none, D238); the
/// least patience, the largest handshake and datagrams.
pub(super) fn windows(subscriptions: Subscriptions<'_>) -> Result<(Admitted, Windows), managed::Error> {
    let window = || managed::Error::Admission(AdmissionError::Window);
    let mut link: Option<Admitted> = None;
    let mut windows = Windows {
        stream: 0,
        connection: 0,
        send: 0,
        streams: 0,
    };
    let outputs = subscriptions.outputs.iter().map(|(_, admitted, _)| {
        let receive = admitted
            .record_bytes
            .checked_add(DATA_HEAD)
            .ok_or_else(window)?
            .max(REFUSED_BYTES);
        Ok::<_, managed::Error>((admitted, receive, CONTROL_BYTES.saturating_add(ACK_BYTES)))
    });
    let inputs = subscriptions
        .inputs
        .iter()
        .map(|(_, admitted, _)| Ok((admitted, input_receives(), input_sends(admitted)?)));
    for lane in outputs.chain(inputs) {
        let (admitted, receive, send) = lane?;
        managed::admissible(admitted)?;
        windows.stream = windows.stream.max(receive);
        windows.connection = windows.connection.checked_add(receive).ok_or_else(window)?;
        windows.send = u64::try_from(send)
            .ok()
            .and_then(|send| windows.send.checked_add(send))
            .ok_or_else(window)?;
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
    let link = link.ok_or(managed::Error::Admission(AdmissionError::Capacity))?;
    Ok((link, windows))
}

/// What a lane carries.
#[derive(Debug, Clone, Copy)]
enum Carries {
    /// A member's output: its largest record frame, where its next record starts, and whether
    /// one is handed out and not yet acknowledged (one in flight, step 5).
    Output { most: usize, next: u64, held: bool },
    /// A member's stdin: what its next taken may say.
    Input { pace: Pace },
}

/// What a stdin's next taken may say (step 5: one record in flight, taken whole).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// Its first, after the subscription at `from`: there, or past it, where an earlier
    /// connection's record was taken but its taken lost.
    First { from: u64 },
    /// A record was sent that ends at `next`: its taken says so.
    Sent { next: u64 },
    /// Said already, no record since: none.
    Told,
}

/// Where a lane stands (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Not open yet: waiting for the handshake, or for the endpoint's room for another stream.
    Waiting,
    /// Open on `stream`; `heard` once the endpoint has said anything on it, before which alone
    /// it may refuse the lane.
    Open { stream: StreamId, heard: bool },
    /// Its handle told its end; nothing more of it is read.
    Over,
}

struct Lane {
    carries: Carries,
    stage: Stage,
    /// What it has to send, `sent` of it gone: its subscription first. Within what was
    /// reserved as it was made, as one ask at a time goes.
    out: Vec<u8>,
    sent: usize,
    /// The endpoint's next frame as it comes, its largest at most.
    inbound: Vec<u8>,
    tell: mpsc::Sender<Heard>,
}

impl Lane {
    fn new(
        carries: Carries,
        subscription: &[u8],
        sends: usize,
        receives: usize,
    ) -> Result<(Lane, mpsc::Receiver<Heard>), managed::Error> {
        let allocation = || managed::Error::Admission(AdmissionError::Allocation);
        let mut out = Vec::new();
        out.try_reserve_exact(sends.max(subscription.len()))
            .map_err(|_| allocation())?;
        out.extend_from_slice(subscription);
        let mut inbound = Vec::new();
        inbound.try_reserve_exact(receives).map_err(|_| allocation())?;
        let (tell, heard) = mpsc::channel();
        Ok((
            Lane {
                carries,
                stage: Stage::Waiting,
                out,
                sent: 0,
                inbound,
                tell,
            },
            heard,
        ))
    }

    /// Tells its handle `heard`, which ends the lane for a fault or an output's finish. A handle
    /// dropped hears nothing.
    fn tell(&mut self, heard: Heard) {
        if matches!(heard, Heard::Fault(_) | Heard::Finished) {
            self.stage = Stage::Over;
        }
        let _ = self.tell.send(heard);
    }

    /// Sends what it has to, as far as its stream takes it.
    fn flush(&mut self, io: &mut Io<'_>, conn: ConnKey) {
        let Stage::Open { stream, .. } = self.stage else {
            return;
        };
        while let Some(rest) = self.out.get(self.sent..).filter(|r| !r.is_empty()) {
            match io.write(conn, stream, rest) {
                Ok(0) | Err(_) => return,
                Ok(n) => self.sent = self.sent.saturating_add(n),
            }
        }
        self.out.clear();
        self.sent = 0;
    }

    /// Adds `frame` to what it has to send, within what was reserved for it.
    fn queue(&mut self, frame: &[u8]) -> bool {
        let fits = self
            .out
            .len()
            .checked_add(frame.len())
            .is_some_and(|n| n <= self.out.capacity());
        if fits {
            self.out.extend_from_slice(frame);
        }
        fits
    }
}

/// What a lane found wrong: the endpoint broke its protocol, or this side had no memory or room
/// for what came or was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    Protocol,
    Local,
}

/// One frame of the endpoint's at the front of `bytes` for a lane carrying `carries`, `heard`
/// whether one came before (a refusal comes first or not at all), and its length; none until it
/// has come whole. A frame of another kind or of a length its lane does not admit is broken, as
/// soon as its head says so.
fn frame_of(bytes: &[u8], carries: Carries, heard: bool) -> Result<Option<(Heard, usize)>, Broken> {
    match read_refused(bytes) {
        Ok(None) => return Ok(None),
        Ok(Some(reason)) if !heard => return Ok(Some((Heard::Fault(Fault::Refused(reason)), REFUSED_BYTES))),
        Ok(Some(_)) | Err(None) => return Err(Broken::Protocol),
        Err(Some(_)) => {}
    }
    match carries {
        Carries::Output { most, next, held } => {
            let Some((tag, len)) = frame::peek(bytes).map_err(|_| Broken::Protocol)? else {
                return Ok(None);
            };
            let whole = FRAME_HEAD.checked_add(len).ok_or(Broken::Protocol)?;
            if tag != Tag::Stdout || len < OFFSET || whole > most || held {
                return Err(Broken::Protocol);
            }
            let Some(payload) = bytes.get(FRAME_HEAD..whole) else {
                return Ok(None);
            };
            let (at, record) = payload.split_first_chunk::<OFFSET>().ok_or(Broken::Protocol)?;
            let at = u64::from_be_bytes(*at);
            if at != next {
                return Err(Broken::Protocol);
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(record.len()).map_err(|_| Broken::Local)?;
            bytes.extend_from_slice(record);
            Ok(Some((Heard::Record { at, bytes }, whole)))
        }
        Carries::Input { pace } => match input::said(bytes).map_err(|_| Broken::Protocol)? {
            None => Ok(None),
            Some((Said::Taken { accepted, room }, n)) => {
                let paced = match pace {
                    Pace::First { from } => accepted >= from,
                    Pace::Sent { next } => accepted == next,
                    Pace::Told => false,
                };
                if !paced || room == 0 {
                    return Err(Broken::Protocol);
                }
                Ok(Some((Heard::Taken { accepted, room }, n)))
            }
            Some((Said::Ended { written, end }, n)) => {
                Ok(Some((Heard::Fault(Fault::InputEnded { written, end }), n)))
            }
        },
    }
}

/// The controller's handler: its one connection, and its lanes in the order dialed.
struct Drive {
    to: SocketAddr,
    vm: Fingerprint,
    config: Option<ClientConfig>,
    /// When it was made: its connection is begun at its first turn.
    since: Instant,
    conn: Option<ConnKey>,
    /// Why its connection ended, as QUIC told it ([`Handler::cause`]).
    cause: Option<Ended>,
    lanes: Vec<Lane>,
    over: bool,
}

impl Drive {
    /// The connection is over: every lane not over is told why, once, and the driver may end.
    fn lose(&mut self, why: Ended) {
        self.over = true;
        for lane in &mut self.lanes {
            if lane.stage != Stage::Over {
                lane.tell(Heard::Fault(Fault::Lost(why)));
            }
        }
    }

    /// Closes the connection for what a lane's reading found: the endpoint broke its protocol
    /// (`Protocol`, as step 5's endpoints close a broken owner's), or this side has no memory
    /// for it (`Closed`).
    fn broke(&mut self, io: &mut Io<'_>, conn: ConnKey, broken: Broken) {
        let (reason, why) = match broken {
            Broken::Protocol => (
                Reason::Protocol,
                "agent control: the endpoint broke a lane's protocol",
            ),
            Broken::Local => (
                Reason::Closed,
                "agent control: no memory or room for a lane's frame",
            ),
        };
        io.close(conn, reason as u32, why);
        self.lose(match broken {
            Broken::Protocol => Ended::Closed(By::This, Reason::Protocol),
            Broken::Local => Ended::Local,
        });
    }

    /// Opens the lanes still waiting, in order, as far as the endpoint has room.
    fn open(&mut self, io: &mut Io<'_>) {
        let Some(conn) = self.conn.filter(|_| !self.over) else {
            return;
        };
        for lane in &mut self.lanes {
            if lane.stage != Stage::Waiting {
                continue;
            }
            let Ok(stream) = io.open(conn) else {
                return;
            };
            lane.stage = Stage::Open { stream, heard: false };
            lane.flush(io, conn);
        }
    }

    fn lane_of(&self, stream: StreamId) -> Option<usize> {
        self.lanes
            .iter()
            .position(|l| matches!(l.stage, Stage::Open { stream: s, .. } if s == stream))
    }

    /// Reads lane `at` as far as it has come: each whole frame told, its stream's end and reset
    /// as its protocol has them.
    fn read(&mut self, io: &mut Io<'_>, conn: ConnKey, at: usize) -> Result<(), Broken> {
        let Some(lane) = self.lanes.get_mut(at) else {
            return Ok(());
        };
        loop {
            let Stage::Open { stream, heard } = lane.stage else {
                return Ok(());
            };
            let room = lane.inbound.capacity().saturating_sub(lane.inbound.len());
            let (bytes, end) = match io.read_within(conn, stream, room).map_err(|_| Broken::Local)? {
                Read::Bytes(bytes) => (bytes, false),
                Read::End(bytes) => (bytes, true),
                Read::Blocked if room == 0 => return Err(Broken::Protocol),
                Read::Blocked => return Ok(()),
                Read::Reset => {
                    lane.tell(Heard::Fault(Fault::Revoked));
                    io.stop(conn, stream);
                    return Ok(());
                }
            };
            if !lane.queue_inbound(&bytes) {
                return Err(Broken::Protocol);
            }
            let mut heard = heard;
            while let Some((said, len)) = frame_of(&lane.inbound, lane.carries, heard)? {
                lane.inbound.drain(..len.min(lane.inbound.len()));
                heard = true;
                match (&said, &mut lane.carries) {
                    (Heard::Record { at, bytes }, Carries::Output { next, held, .. }) => {
                        *next = at.checked_add(bytes.len() as u64).ok_or(Broken::Protocol)?;
                        *held = true;
                    }
                    (Heard::Taken { .. }, Carries::Input { pace }) => *pace = Pace::Told,
                    _ => {}
                }
                lane.stage = Stage::Open { stream, heard };
                lane.tell(said);
                if lane.stage == Stage::Over {
                    let _ = io.finish(conn, stream);
                    io.stop(conn, stream);
                    return Ok(());
                }
            }
            if end {
                // The endpoint finishes an output's lane once every record is consumed; a stdin's
                // only after its end, and either after a refusal (told above).
                return match lane.carries {
                    Carries::Output { held: false, .. } if lane.inbound.is_empty() => {
                        lane.tell(Heard::Finished);
                        let _ = io.finish(conn, stream);
                        Ok(())
                    }
                    _ => Err(Broken::Protocol),
                };
            }
        }
    }
}

impl Lane {
    /// Adds bytes read to the frame coming, within its largest.
    fn queue_inbound(&mut self, bytes: &[u8]) -> bool {
        let fits = self
            .inbound
            .len()
            .checked_add(bytes.len())
            .is_some_and(|n| n <= self.inbound.capacity());
        if fits {
            self.inbound.extend_from_slice(bytes);
        }
        fits
    }
}

impl Handler for Drive {
    type Command = Ask;

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        // The TLS handshake took no other key (`tls::client_with`); this holds it so whatever
        // its configuration.
        if identity::fingerprint(peer) != self.vm {
            self.lose(Ended::Closed(By::This, Reason::Auth));
            return Err(Close {
                code: Reason::Auth as u32,
                why: "agent control: not the pinned microVM",
            });
        }
        self.conn = Some(conn);
        self.open(io);
        Ok(())
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        // The endpoint opens no lane (D238).
        io.stop(conn, stream);
        io.reset(conn, stream);
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        let Some(at) = self.lane_of(stream) else {
            io.stop(conn, stream);
            return;
        };
        if let Err(broken) = self.read(io, conn, at) {
            self.broke(io, conn, broken);
        }
    }

    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some(lane) = self.lane_of(stream).and_then(|at| self.lanes.get_mut(at)) {
            lane.flush(io, conn);
        }
    }

    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        // The endpoint reads no more of a lane it refused, ended or finished, whose side here
        // the driver has reset: its own frames, read as they come, say how it ended.
    }

    fn cause(&mut self, conn: ConnKey, error: &ConnectionError) {
        if self.conn.is_none_or(|c| c == conn) {
            self.cause = Some(ended(error));
        }
    }

    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        if self.conn.is_some_and(|c| c != conn) {
            return;
        }
        // No cause: its handshake ran out of the endpoint's patience, unheard.
        let why = self.cause.take().unwrap_or(Ended::Silent);
        self.lose(why);
    }

    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        // A handle asks only after its lane heard something, so on the connection made.
        let Some(conn) = self.conn else {
            return;
        };
        let at = match ask {
            Ask::Consumed { lane, .. } | Ask::Send { lane, .. } => lane,
        };
        let Some(lane) = self.lanes.get_mut(at).filter(|l| l.stage != Stage::Over) else {
            return;
        };
        let queued = match (ask, &mut lane.carries) {
            // An acknowledgment of the record handed out, whole, and nothing else.
            (Ask::Consumed { offset, .. }, Carries::Output { next, held, .. }) => {
                if !*held || offset != *next {
                    return;
                }
                *held = false;
                lane.queue(&managed::consumed(offset))
            }
            (Ask::Send { frame, .. }, Carries::Input { pace }) => {
                if let Some(next) = record_end(&frame) {
                    *pace = Pace::Sent { next };
                }
                lane.queue(&frame)
            }
            _ => return,
        };
        if queued {
            lane.flush(io, conn);
        } else {
            self.broke(io, conn, Broken::Local);
        }
    }

    fn available(&mut self, io: &mut Io<'_>, _conn: ConnKey) {
        self.open(io);
    }

    fn done(&self) -> bool {
        self.over
    }

    fn due(&self) -> Option<Instant> {
        self.config.as_ref().map(|_| self.since)
    }

    fn timeout(&mut self, io: &mut Io<'_>, _now: Instant) {
        let Some(config) = self.config.take() else {
            return;
        };
        // Its key is set as it connects (`connected`): no lane opens before the handshake has
        // taken the microVM's pinned key.
        if io.connect(self.to, config).is_err() {
            self.lose(Ended::Transport(By::This));
        }
    }
}
