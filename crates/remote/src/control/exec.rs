//! Exec lanes (plan step 11, D238; ROOT's design (B), 2026-11-13): a command run in one of a
//! microVM's members is a lane of its own, which asks for it, steers it and hears it end, and
//! its stdout, stderr and stdin are step 5's streams on lanes of their own, as a member's are.
//!
//! - **The ask.** An exec lane's first frame (`Tag::MemberExec`) names the run, its
//!   generation and the member, then carries step 6's member exec as the run protocol carries
//!   it, byte for byte: a `run::Spec` (D243). Its length is the run protocol's own bound, as
//!   large as Linux execs (`run::MAX_PAYLOAD`).
//! - **Its streams.** The endpoint numbers each exec, and its stdout, stderr (none on a
//!   terminal) and stdin (where it reads one) are step 5's streams under a run of their own
//!   ([`exec_run`]): the run's identity and the exec's number, hashed, so that the
//!   controller's subscriptions name them as they name a member's.
//! - **Its steering.** After `Started`, the lane carries the controller's resizes and
//!   signals, D180's frames (`Tag::Resize`, `Tag::Signal`), one in flight to the endpoint's
//!   host at a time, as step 5 has one record in flight; the endpoint tells its exit
//!   (`Tag::Exit`) and finishes it.
//! - **Its host.** The endpoint starts nothing itself: it hands its host the ask and its
//!   streams' handles ([`Start`]), and the host, which charges the run's budget for each exec,
//!   says how it went ([`Replies`]).

use std::mem::size_of;
use std::sync::mpsc;

use sha2::{Digest as _, Sha256};
use shards_abi::run::{self, Size, Spec};

use crate::frame::{self, Tag};
use crate::managed::{self, Producer, Reason, RunId, input};
use crate::net::Sender;

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
/// An ask's scope before its spec: the run (32), its generation (u64) and the member (u32).
const SCOPE: usize = size_of::<RunId>() + size_of::<u64>() + size_of::<u32>();
/// The most an ask's frame carries: its scope and the largest spec the run protocol carries,
/// as large as Linux execs (`run::MAX_PAYLOAD`).
pub const ASK_MOST: usize = SCOPE + run::MAX_PAYLOAD as usize;

/// What a controller asks to run in a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub run: RunId,
    pub generation: u64,
    pub member: u32,
    /// Step 6's member exec: its argv, environment, working directory, terminal and stdin. Its
    /// user and setup are the member's own (D243): the host refuses others.
    pub spec: Spec,
}

impl Ask {
    /// Its first frame, whole.
    pub fn frame(&self) -> Result<Vec<u8>, managed::Error> {
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(SCOPE.saturating_add(self.spec.encoded_len().unwrap_or(0)))
            .map_err(|_| managed::Error::Admission(managed::AdmissionError::Allocation))?;
        payload.extend_from_slice(&self.run.0);
        payload.extend_from_slice(&self.generation.to_be_bytes());
        payload.extend_from_slice(&self.member.to_be_bytes());
        self.spec.encode_into(&mut payload);
        if payload.len() > ASK_MOST {
            return Err(managed::Error::Protocol(managed::ProtocolError::Length));
        }
        let mut out = Vec::new();
        frame::put(&mut out, Tag::MemberExec, &payload)?;
        Ok(out)
    }

    /// The ask a whole first frame's `payload` carries: a protocol error for one cut short or a
    /// spec the run protocol would not take.
    pub fn decode(payload: &[u8]) -> Result<Ask, Reason> {
        let (run, rest) = payload.split_first_chunk::<32>().ok_or(Reason::Protocol)?;
        let (generation, rest) = rest.split_first_chunk::<8>().ok_or(Reason::Protocol)?;
        let (member, spec) = rest.split_first_chunk::<4>().ok_or(Reason::Protocol)?;
        Ok(Ask {
            run: RunId(*run),
            generation: u64::from_be_bytes(*generation),
            member: u32::from_be_bytes(*member),
            spec: Spec::decode(spec).ok_or(Reason::Protocol)?,
        })
    }
}

/// The run an exec's streams are scoped to: the run's identity and the exec's number, hashed,
/// so that no stream of one exec is another's or a member's (FIPS 180-4).
pub fn exec_run(run: RunId, exec: u32) -> RunId {
    let mut hash = Sha256::new();
    hash.update(b"shards-control/1 exec ");
    hash.update(run.0);
    hash.update(exec.to_be_bytes());
    RunId(hash.finalize().into())
}

/// What the controller steers a running exec with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steer {
    /// Its terminal's size: rows, then columns (step 6's `EXEC_RESIZE`).
    Resize(Size),
    /// A signal, by its Linux number, `1..=run::NSIG` (step 6's `EXEC_SIGNAL`).
    Signal(u32),
}

/// What an exec lane's steering frame at the front of `bytes` says, and its length; none until
/// it has come whole. Another frame, or a signal no guest has, is the controller's error.
pub fn steer(bytes: &[u8]) -> Result<Option<(Steer, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Resize | Tag::Signal => size_of::<u32>(),
        _ => return Err(Reason::Protocol),
    };
    if len != expected {
        return Err(Reason::Protocol);
    }
    let Some(payload) = bytes.get(FRAME_HEAD..FRAME_HEAD.saturating_add(len)) else {
        return Ok(None);
    };
    let steer = if tag == Tag::Resize {
        let (rows, cols) = frame::read_size(payload).ok_or(Reason::Protocol)?;
        Steer::Resize(Size { rows, cols })
    } else {
        let signal = u32::from_be_bytes(<[u8; 4]>::try_from(payload).map_err(|_| Reason::Protocol)?);
        if !(1..=run::NSIG).contains(&signal) {
            return Err(Reason::Protocol);
        }
        Steer::Signal(signal)
    };
    Ok(Some((steer, FRAME_HEAD.saturating_add(len))))
}

/// A steering frame: `steer`, as the controller sends it.
pub fn steering(steer: Steer) -> Result<Vec<u8>, managed::Error> {
    let mut out = Vec::new();
    match steer {
        Steer::Resize(size) => frame::put(&mut out, Tag::Resize, &frame::size(size.rows, size.cols))?,
        Steer::Signal(signal) => frame::put(&mut out, Tag::Signal, &signal.to_be_bytes())?,
    }
    Ok(out)
}

/// Why an exec did not start, typed: what its controller matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Unstarted {
    /// The run's budget has no room for another exec's streams (the host charges each).
    Capacity = 16,
    /// The member refused it, as its init refuses step 6's member exec (D243): a member that
    /// has ended or is unknown, a user or privilege the member has not, or its confinement.
    Refused = 17,
    /// It could not be started: its host could not run it.
    Failed = 18,
}

impl TryFrom<u32> for Unstarted {
    type Error = ();
    fn try_from(code: u32) -> Result<Self, ()> {
        match code {
            16 => Ok(Unstarted::Capacity),
            17 => Ok(Unstarted::Refused),
            18 => Ok(Unstarted::Failed),
            _ => Err(()),
        }
    }
}

/// What the endpoint tells an exec lane after its ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// It started, its streams numbered `exec` ([`exec_run`]).
    Started(u32),
    /// It did not start.
    Unstarted(Unstarted),
    /// It ended, with this status: its command's, or 128 plus the signal that ended it.
    Exit(i32),
}

pub(super) const TOLD_BYTES: usize = FRAME_HEAD + size_of::<u32>();

/// `told`'s frame.
pub fn telling(told: Told) -> [u8; TOLD_BYTES] {
    let (tag, word) = match told {
        Told::Started(exec) => (Tag::Started, exec),
        Told::Unstarted(why) => (Tag::Refused, why as u32),
        Told::Exit(status) => (Tag::Exit, u32::from_be_bytes(status.to_be_bytes())),
    };
    let mut out = [0; TOLD_BYTES];
    let len = (size_of::<u32>() as u32).to_be_bytes();
    for (slot, byte) in out.iter_mut().zip(
        std::iter::once(&(tag as u8))
            .chain(&len)
            .chain(&word.to_be_bytes()),
    ) {
        *slot = *byte;
    }
    out
}

/// What the endpoint told at the front of `bytes`, and its length; none until it has come
/// whole. A lane's own refusal (`Reason`) is the lane's, read before ([`super::read_refused`]).
pub fn told(bytes: &[u8]) -> Result<Option<(Told, usize)>, Reason> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Reason::Protocol)? else {
        return Ok(None);
    };
    if len != size_of::<u32>() {
        return Err(Reason::Protocol);
    }
    let Some(word) = bytes
        .get(FRAME_HEAD..TOLD_BYTES)
        .and_then(|w| <[u8; 4]>::try_from(w).ok())
        .map(u32::from_be_bytes)
    else {
        return Ok(None);
    };
    let told = match tag {
        Tag::Started => Told::Started(word),
        Tag::Refused => Told::Unstarted(Unstarted::try_from(word).map_err(|()| Reason::Protocol)?),
        Tag::Exit => Told::Exit(i32::from_be_bytes(word.to_be_bytes())),
        _ => return Err(Reason::Protocol),
    };
    Ok(Some((told, TOLD_BYTES)))
}

/// What the host says of an exec it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Started,
    Unstarted(Unstarted),
    Exited(i32),
}

/// Where a host says how an exec went: to the endpoint that handed it, which it wakes.
#[derive(Debug)]
pub struct Replies {
    exec: u32,
    tx: mpsc::Sender<(u32, Reply)>,
    wake: Sender<()>,
}

impl Replies {
    pub(super) fn new(exec: u32, tx: mpsc::Sender<(u32, Reply)>, wake: Sender<()>) -> Replies {
        Replies { exec, tx, wake }
    }

    /// Tells the endpoint `reply`. An endpoint gone hears nothing; a full handoff is a wake
    /// queued already, whose turn reads every reply.
    pub fn tell(&self, reply: Reply) {
        if self.tx.send((self.exec, reply)).is_ok() {
            self.took();
        }
    }

    /// Says the host took a steer: the endpoint reads the exec's lane on.
    pub fn took(&self) {
        let _ = self.wake.try_send(crate::net::Command::Handler(()));
    }

    /// Another handle to the same exec's replies, for another of its host's threads.
    pub fn try_clone(&self) -> Result<Replies, crate::Error> {
        Ok(Replies {
            exec: self.exec,
            tx: self.tx.clone(),
            wake: self.wake.try_clone()?,
        })
    }
}

/// An exec the endpoint hands its host: the ask, the exec's number, its streams' handles as
/// step 5's endpoints give them, its steering and where to say how it went.
#[derive(Debug)]
pub struct Start {
    pub ask: Ask,
    pub exec: u32,
    pub stdout: (Producer, managed::Control),
    /// None on a terminal, whose output is its stdout alone.
    pub stderr: Option<(Producer, managed::Control)>,
    /// None for an exec that reads no stdin.
    pub stdin: Option<(input::Relay, input::Control)>,
    /// The controller's resizes and signals, one in flight: the endpoint reads its lane on as
    /// each is taken ([`Replies::took`]). It closes once the endpoint has let the exec go (its
    /// exit told, its outputs finished; or its connection gone, which hangs it up): a host
    /// keeps each stream's control until then, as a control dropped revokes its stream.
    pub steer: mpsc::Receiver<Steer>,
    pub replies: Replies,
}

/// The admission each exec's streams get: its outputs', and its stdin's, as the run's own
/// streams get theirs (`managed::Admitted`).
#[derive(Debug, Clone, Copy)]
pub struct Admissions {
    pub output: managed::Admitted,
    pub input: managed::Admitted,
}

/// What the endpoint holds of the connection's limits (RFC 9000 §4.1, §4.6): the streams its
/// controller may have open at once, its receive window and its send window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Limits {
    pub(super) streams: u64,
    pub(super) receive: u64,
    pub(super) send: u64,
}

impl Limits {
    fn plus(self, other: Limits) -> Option<Limits> {
        Some(Limits {
            streams: self.streams.checked_add(other.streams)?,
            receive: self.receive.checked_add(other.receive)?,
            send: self.send.checked_add(other.send)?,
        })
    }
}

/// Where an exec lane stands (R04).
#[derive(Debug)]
pub(super) enum Stage {
    /// Its ask coming: `frame` what has, of `whole` bytes.
    Asking { frame: Vec<u8>, whole: usize },
    /// Handed to the host, which has not said yet.
    Pending,
    /// Started: steering read, each taken before the next ([`Steer`]).
    Running,
    /// Its host said it ended: its exit is told once its outputs have finished.
    Exited(i32),
    /// Its last frame (its exit, or why it did not start) is queued: its lane finishes as it
    /// goes, and the exec's streams with it.
    Told,
}

/// One exec lane, and the exec it asked for.
#[derive(Debug)]
pub(super) struct Lane {
    pub(super) conn: crate::net::ConnKey,
    pub(super) stream: hyper_quic::StreamId,
    pub(super) stage: Stage,
    /// Its number, once asked for.
    pub(super) exec: Option<u32>,
    /// What its started exec added to the connection's limits.
    pub(super) added: Limits,
    /// Its steering, to the host; dropped, the host hangs the exec up.
    pub(super) steer: Option<mpsc::SyncSender<Steer>>,
    /// A steer the host has not taken yet: the lane is read no more until it has.
    pub(super) pending: Option<Steer>,
    /// Steering frames as they come.
    pub(super) inbound: Vec<u8>,
    /// What it says, as far as it has gone.
    pub(super) out: [u8; TOLD_BYTES],
    pub(super) written: usize,
    pub(super) queued: bool,
}

/// The endpoint's execs: what each exec's streams are admitted, where it hands its host each
/// exec, and what the host says.
#[derive(Debug)]
pub(super) struct Execs {
    pub(super) admissions: Option<Admissions>,
    pub(super) starts: Option<mpsc::Sender<Start>>,
    pub(super) replies_tx: mpsc::Sender<(u32, Reply)>,
    pub(super) replies: mpsc::Receiver<(u32, Reply)>,
    /// The endpoint's own sender, which each exec's handles wake it with.
    pub(super) wake: Option<Sender<()>>,
    pub(super) next: u32,
    pub(super) lanes: Vec<Lane>,
    /// The connection's limits without an exec running: its streams' and one ask's.
    pub(super) base: Limits,
}

impl Execs {
    pub(super) fn new(
        admissions: Option<Admissions>,
        base: Limits,
    ) -> (Execs, Option<mpsc::Receiver<Start>>) {
        let (replies_tx, replies) = mpsc::channel();
        let (starts, handed) = match admissions {
            Some(_) => {
                let (tx, rx) = mpsc::channel();
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        (
            Execs {
                admissions,
                starts,
                replies_tx,
                replies,
                wake: None,
                next: 0,
                lanes: Vec::new(),
                base,
            },
            handed,
        )
    }

    /// The connection's limits with every started exec of `conn` in.
    pub(super) fn limits(&self, conn: crate::net::ConnKey) -> Option<Limits> {
        self.lanes
            .iter()
            .filter(|l| l.conn == conn)
            .try_fold(self.base, |sum, l| sum.plus(l.added))
    }
}

/// What a started exec adds to its connection's limits: its lane stays open and another ask
/// may come, and each of its stdio lanes; what the endpoint reads of them before they are read
/// (an output's subscription and acknowledgment, a stdin's record and end, a steer) and what it
/// sends on them unacknowledged (each output's admitted window, a stdin's taken, a refusal
/// each, the lane's told frames), as the endpoint sums its own streams'
/// (`super::Endpoint::bind`).
pub(super) fn added(admissions: Admissions, stderr: bool, stdin: bool) -> Option<Limits> {
    let outputs = 1u64.checked_add(u64::from(stderr))?;
    let output = Limits {
        streams: 1,
        receive: u64::try_from(managed::CONTROL_BYTES).ok()?,
        send: admissions
            .output
            .transport_bytes
            .checked_add(u64::try_from(super::REFUSED_BYTES).ok()?)?,
    };
    let mut sum = Limits {
        // Its lane, kept open, and the next ask's.
        streams: 1,
        receive: u64::try_from(FRAME_HEAD.checked_add(size_of::<u32>())?).ok()?,
        send: u64::try_from(TOLD_BYTES).ok()?,
    };
    for _ in 0..outputs {
        sum = sum.plus(output)?;
    }
    if stdin {
        sum = sum.plus(Limits {
            streams: 1,
            receive: u64::try_from(input::receive_bytes(admissions.input.record_bytes).ok()?).ok()?,
            send: admissions
                .input
                .transport_bytes
                .checked_add(u64::try_from(super::REFUSED_BYTES).ok()?)?,
        })?;
    }
    Some(sum)
}

/// A steering frame's length: its head and its four bytes.
const STEER_BYTES: usize = FRAME_HEAD + size_of::<u32>();

impl super::Lanes {
    /// A lane whose first frame is an ask (`Tag::MemberExec`) of `len` bytes, `bytes` what came
    /// of it: refused alone where the endpoint serves no exec or the ask is longer than the run
    /// protocol carries; read on as it comes otherwise.
    pub(super) fn begin_ask(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
        bytes: &[u8],
        len: usize,
        ended: bool,
    ) {
        if self.execs.admissions.is_none() || len > ASK_MOST {
            let why = if len > ASK_MOST {
                Reason::Protocol
            } else {
                Reason::Scope
            };
            self.refuse_lane(io, conn, stream, why);
            return;
        }
        let mut frame = Vec::new();
        if frame.try_reserve(bytes.len()).is_err() || self.execs.lanes.try_reserve(1).is_err() {
            self.refuse_lane(io, conn, stream, Reason::Closed);
            return;
        }
        frame.extend_from_slice(bytes);
        self.execs.lanes.push(Lane {
            conn,
            stream,
            stage: Stage::Asking {
                frame,
                whole: FRAME_HEAD.saturating_add(len),
            },
            exec: None,
            added: Limits::default(),
            steer: None,
            pending: None,
            inbound: Vec::new(),
            out: [0; TOLD_BYTES],
            written: 0,
            queued: false,
        });
        if !ended {
            self.read_exec(io, conn, stream);
            return;
        }
        // Its controller ended its side with the ask: whole, the exec runs unsteered.
        let at = self.execs.lanes.len().saturating_sub(1);
        let whole = FRAME_HEAD.saturating_add(len);
        match bytes
            .get(FRAME_HEAD..)
            .filter(|_| bytes.len() == whole)
            .map(Ask::decode)
        {
            Some(Ok(ask)) => self.admit_exec(io, at, ask),
            Some(Err(why)) => {
                self.execs.lanes.swap_remove(at);
                self.refuse_lane(io, conn, stream, why);
            }
            None => {
                self.execs.lanes.swap_remove(at);
                super::end(io, conn, stream);
            }
        }
    }

    /// Reads the exec lane `(conn, stream)` as far as its stage reads: its ask, then its
    /// steering, each steer taken by the host before the next is read. Whether it is one.
    pub(super) fn read_exec(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self
            .execs
            .lanes
            .iter()
            .position(|l| l.conn == conn && l.stream == stream)
        else {
            return false;
        };
        loop {
            let Some(lane) = self.execs.lanes.get_mut(at) else {
                return true;
            };
            let room = match &lane.stage {
                Stage::Asking { frame, whole } => whole.saturating_sub(frame.len()),
                Stage::Running if lane.pending.is_none() => STEER_BYTES.saturating_sub(lane.inbound.len()),
                // Held: flow control holds its controller back meanwhile.
                _ => return true,
            };
            let (bytes, ended) = match io.read_within(conn, stream, room) {
                Ok(crate::net::Read::Bytes(bytes)) => (bytes, false),
                Ok(crate::net::Read::End(bytes)) => (bytes, true),
                Ok(crate::net::Read::Blocked) => return true,
                Ok(crate::net::Read::Reset) | Err(_) => {
                    // Its controller left it: an exec it started is hung up.
                    lane.steer = None;
                    if matches!(lane.stage, Stage::Asking { .. }) {
                        self.execs.lanes.swap_remove(at);
                    }
                    super::end(io, conn, stream);
                    return true;
                }
            };
            match &mut lane.stage {
                Stage::Asking { frame, whole } => {
                    if frame.try_reserve(bytes.len()).is_err() {
                        self.execs.lanes.swap_remove(at);
                        self.refuse_lane(io, conn, stream, Reason::Closed);
                        return true;
                    }
                    frame.extend_from_slice(&bytes);
                    if frame.len() < *whole {
                        if ended {
                            self.execs.lanes.swap_remove(at);
                            super::end(io, conn, stream);
                            return true;
                        }
                        continue;
                    }
                    let asked = frame.get(FRAME_HEAD..).map_or(Err(Reason::Protocol), Ask::decode);
                    match asked {
                        Ok(ask) => self.admit_exec(io, at, ask),
                        Err(why) => {
                            self.execs.lanes.swap_remove(at);
                            self.refuse_lane(io, conn, stream, why);
                        }
                    }
                    return true;
                }
                Stage::Running => {
                    lane.inbound.extend_from_slice(&bytes);
                    match steer(&lane.inbound) {
                        Ok(Some((taken, len))) => {
                            lane.inbound.drain(..len.min(lane.inbound.len()));
                            lane.pending = Some(taken);
                            Self::hand_steer(lane);
                        }
                        Ok(None) if !ended => {}
                        Ok(None) => {
                            // Its controller steers it no more; it runs on.
                            io.stop(conn, stream);
                            return true;
                        }
                        Err(why) => {
                            self.refuse_connection(io, conn, why);
                            return true;
                        }
                    }
                }
                _ => return true,
            }
        }
    }

    /// Hands a lane's pending steer to its host, where it has room for it.
    fn hand_steer(lane: &mut Lane) {
        let (Some(steer), Some(to)) = (lane.pending, &lane.steer) else {
            return;
        };
        match to.try_send(steer) {
            Ok(()) => lane.pending = None,
            Err(mpsc::TrySendError::Full(_)) => {}
            // Its host has let it go: what still comes is let go too.
            Err(mpsc::TrySendError::Disconnected(_)) => {
                lane.pending = None;
                lane.steer = None;
            }
        }
    }

    /// An ask read whole: refused alone for another run, generation or member than the
    /// endpoint serves, or where it numbers no more execs; else its streams are made, as step 5's
    /// endpoints make theirs, and the exec handed to the host.
    fn admit_exec(&mut self, io: &mut crate::net::Io<'_>, at: usize, ask: Ask) {
        let Some(lane) = self.execs.lanes.get(at) else {
            return;
        };
        let (conn, stream) = (lane.conn, lane.stream);
        match self.make_exec(&ask) {
            Ok((exec, start, steer)) => {
                let sent = self
                    .execs
                    .starts
                    .as_ref()
                    .is_some_and(|starts| starts.send(start).is_ok());
                if !sent {
                    self.drop_exec(io, exec);
                    self.execs.lanes.swap_remove(at);
                    self.refuse_lane(io, conn, stream, Reason::Closed);
                    return;
                }
                if let Some(lane) = self.execs.lanes.get_mut(at) {
                    lane.stage = Stage::Pending;
                    lane.exec = Some(exec);
                    lane.steer = Some(steer);
                }
            }
            Err(why) => {
                self.execs.lanes.swap_remove(at);
                self.refuse_lane(io, conn, stream, why);
            }
        }
    }

    /// The exec `ask` names, its streams made and served, and what its host is handed.
    fn make_exec(&mut self, ask: &Ask) -> Result<(u32, Start, mpsc::SyncSender<Steer>), Reason> {
        let first = self.outputs.first().map(|o| o.scope()).ok_or(Reason::Scope)?;
        if ask.run != first.run {
            return Err(Reason::Scope);
        }
        if ask.generation != first.generation {
            return Err(Reason::Stale);
        }
        if !self.outputs.iter().any(|o| o.scope().member == ask.member) {
            return Err(Reason::Scope);
        }
        let admissions = self.execs.admissions.ok_or(Reason::Scope)?;
        let wake = self.execs.wake.as_ref().ok_or(Reason::Closed)?;
        let exec = self.execs.next;
        let next = exec.checked_add(1).ok_or(Reason::Closed)?;
        let run = exec_run(ask.run, exec);
        let scope = |channel| managed::Scope {
            run,
            generation: ask.generation,
            member: ask.member,
            channel,
        };
        let closed = |_| Reason::Closed;
        let output = |channel| -> Result<(managed::Service, (Producer, managed::Control)), Reason> {
            let service =
                managed::Service::new(scope(channel), self.controller, admissions.output.record_bytes)
                    .map_err(closed)?;
            let handles = service
                .handles(wake.try_clone().map_err(|_| Reason::Closed)?, managed::Stop::Lane)
                .map_err(closed)?;
            Ok((service, handles))
        };
        use shards_abi::run::managed::Channel;
        let (stdout, stdout_handles) = output(Channel::Stdout)?;
        let stderr = match ask.spec.tty {
            None => Some(output(Channel::Stderr)?),
            Some(_) => None,
        };
        let stdin = match ask.spec.stdin {
            true => {
                let service = input::Service::new(
                    input::Scope {
                        run,
                        generation: ask.generation,
                        member: ask.member,
                    },
                    self.controller,
                    admissions.input.record_bytes,
                )
                .map_err(closed)?;
                let handles = service
                    .handles(wake.try_clone().map_err(|_| Reason::Closed)?, managed::Stop::Lane)
                    .map_err(closed)?;
                Some((service, handles))
            }
            false => None,
        };
        let replies = Replies::new(
            exec,
            self.execs.replies_tx.clone(),
            wake.try_clone().map_err(|_| Reason::Closed)?,
        );
        self.outputs.try_reserve(2).map_err(|_| Reason::Closed)?;
        self.inputs.try_reserve(1).map_err(|_| Reason::Closed)?;
        let (steer_tx, steer_rx) = mpsc::sync_channel(1);
        let (stderr_service, stderr_handles) = match stderr {
            Some((service, handles)) => (Some(service), Some(handles)),
            None => (None, None),
        };
        let (stdin_service, stdin_handles) = match stdin {
            Some((service, handles)) => (Some(service), Some(handles)),
            None => (None, None),
        };
        self.outputs.push(stdout);
        if let Some(service) = stderr_service {
            self.outputs.push(service);
        }
        if let Some(service) = stdin_service {
            self.inputs.push(service);
        }
        self.execs.next = next;
        Ok((
            exec,
            Start {
                ask: ask.clone(),
                exec,
                stdout: stdout_handles,
                stderr: stderr_handles,
                stdin: stdin_handles,
                steer: steer_rx,
                replies,
            },
            steer_tx,
        ))
    }

    /// Exec `exec`'s streams let go, and its handles told it is over: a lane on an output that
    /// has finished ends as the output's own would (its FIN), whichever thread finished it last;
    /// any other lane on one is abandoned.
    fn drop_exec(&mut self, io: &mut crate::net::Io<'_>, exec: u32) {
        if let Some(first) = self.outputs.first().map(|o| o.scope()) {
            let run = exec_run(first.run, exec);
            for output in self.outputs.iter_mut().filter(|o| o.scope().run == run) {
                if let (Some((conn, stream)), true) = (output.subscriber(), output.finished()) {
                    output.flush(io);
                    output.disconnect();
                    io.stop(conn, stream);
                }
            }
            for input in self.inputs.iter_mut().filter(|i| i.scope().run == run) {
                if let (Some((conn, stream)), true) = (input.subscriber(), input.said_its_end()) {
                    input.disconnect();
                    io.stop(conn, stream);
                }
            }
        }
        for (conn, stream) in self.let_exec_go(exec) {
            super::end(io, conn, stream);
        }
    }

    /// Exec `exec`'s streams let go, its handles told it is over: the lanes on them, to end.
    fn let_exec_go(&mut self, exec: u32) -> Vec<(crate::net::ConnKey, hyper_quic::StreamId)> {
        let mut lanes = Vec::new();
        let Some(first) = self.outputs.first().map(|o| o.scope()) else {
            return lanes;
        };
        let run = exec_run(first.run, exec);
        for output in self.outputs.iter_mut().filter(|o| o.scope().run == run) {
            if let Some(lane) = output.subscriber() {
                lanes.push(lane);
            }
            output.close();
        }
        for input in self.inputs.iter_mut().filter(|i| i.scope().run == run) {
            if let Some(lane) = input.subscriber() {
                lanes.push(lane);
            }
            input.close();
        }
        self.outputs.retain(|o| o.scope().run != run);
        self.inputs.retain(|i| i.scope().run != run);
        lanes
    }

    /// Whether exec `exec`'s outputs have all finished, every record consumed, or were let go.
    fn outputs_done(&self, exec: u32) -> bool {
        let Some(first) = self.outputs.first().map(|o| o.scope()) else {
            return true;
        };
        let run = exec_run(first.run, exec);
        self.outputs
            .iter()
            .filter(|o| o.scope().run == run)
            .all(|o| o.finished() || o.aborted())
    }

    /// A turn for the execs: what their host said, each steer it had no room for handed again,
    /// each exit told once its outputs have finished, each lane's frame written, and each exec
    /// told whole let go, the connection's limits with it.
    pub(super) fn exec_turn(&mut self, io: &mut crate::net::Io<'_>) {
        while let Ok((exec, reply)) = self.execs.replies.try_recv() {
            let Some(at) = self.execs.lanes.iter().position(|l| l.exec == Some(exec)) else {
                continue;
            };
            self.reply(io, at, exec, reply);
        }
        let mut at = 0;
        while at < self.execs.lanes.len() {
            let exec = self.execs.lanes.get(at).and_then(|l| l.exec);
            let done = exec.is_some_and(|e| self.outputs_done(e));
            let Some(lane) = self.execs.lanes.get_mut(at) else {
                break;
            };
            Self::hand_steer(lane);
            if let (Stage::Exited(status), true) = (&lane.stage, done) {
                lane.out = telling(Told::Exit(*status));
                lane.written = 0;
                lane.queued = true;
                lane.stage = Stage::Told;
            }
            if Self::write_told(io, lane) && matches!(lane.stage, Stage::Told) {
                let (conn, added) = (lane.conn, lane.added);
                let _ = io.finish(conn, lane.stream);
                io.stop(conn, lane.stream);
                let exec = lane.exec;
                self.execs.lanes.swap_remove(at);
                if let Some(exec) = exec {
                    self.drop_exec(io, exec);
                }
                if added != Limits::default() {
                    self.set_limits(io, conn);
                }
                continue;
            }
            at = at.saturating_add(1);
        }
    }

    /// Writes what is left of a lane's frame; whether nothing is left.
    fn write_told(io: &mut crate::net::Io<'_>, lane: &mut Lane) -> bool {
        if !lane.queued {
            return true;
        }
        while let Some(rest) = lane.out.get(lane.written..).filter(|r| !r.is_empty()) {
            match io.write(lane.conn, lane.stream, rest) {
                Ok(0) => return false,
                Ok(n) => lane.written = lane.written.saturating_add(n),
                Err(_) => break,
            }
        }
        lane.queued = false;
        true
    }

    /// Acts on what the host said of exec `exec`, on lane `at`.
    fn reply(&mut self, io: &mut crate::net::Io<'_>, at: usize, exec: u32, reply: Reply) {
        let admissions = self.execs.admissions;
        let Some(lane) = self.execs.lanes.get_mut(at) else {
            return;
        };
        match (reply, &lane.stage) {
            (Reply::Started, Stage::Pending) => {
                let first = self.outputs.first().map(|o| o.scope().run);
                let streams = first.map(|run| exec_run(run, exec));
                let stderr = streams
                    .is_some_and(|run| self.outputs.iter().filter(|o| o.scope().run == run).count() > 1);
                let stdin = streams.is_some_and(|run| self.inputs.iter().any(|i| i.scope().run == run));
                let Some(lane) = self.execs.lanes.get_mut(at) else {
                    return;
                };
                lane.added = admissions
                    .and_then(|a| added(a, stderr, stdin))
                    .unwrap_or_default();
                lane.stage = Stage::Running;
                lane.out = telling(Told::Started(exec));
                lane.written = 0;
                lane.queued = true;
                let conn = lane.conn;
                // Room for its lanes to open, kept as its connection's own.
                let streams = usize::try_from(lane.added.streams)
                    .ok()
                    .and_then(|n| n.checked_mul(super::CONNECTIONS))
                    .unwrap_or(0);
                let _ = self.opening.try_reserve(streams);
                let _ = self.refusing.try_reserve(streams);
                self.set_limits(io, conn);
                let (stream, conn) = (self.execs.lanes.get(at).map(|l| l.stream), conn);
                if let Some(stream) = stream {
                    self.read_exec(io, conn, stream);
                }
            }
            (Reply::Unstarted(why), Stage::Pending) => {
                lane.out = telling(Told::Unstarted(why));
                lane.written = 0;
                lane.queued = true;
                lane.stage = Stage::Told;
                lane.steer = None;
            }
            (Reply::Exited(status), Stage::Running | Stage::Pending) => {
                // Its steering stays open until the exec is let go (`Start::steer`).
                lane.stage = Stage::Exited(status);
            }
            _ => {}
        }
    }

    /// Sets `conn`'s limits to its streams' and its started execs'.
    fn set_limits(&mut self, io: &mut crate::net::Io<'_>, conn: crate::net::ConnKey) {
        if let Some(limits) = self.execs.limits(conn) {
            let _ = io.limit(conn, limits.streams, limits.receive, limits.send);
        }
    }

    /// `conn` is gone: each exec it asked for is hung up (its steering dropped) and let go,
    /// as a session's command ends with its connection (RFC 4254 §6.10's channel close).
    pub(super) fn forget_execs(&mut self, conn: crate::net::ConnKey) {
        let mut at = 0;
        while at < self.execs.lanes.len() {
            if self.execs.lanes.get(at).is_some_and(|l| l.conn == conn) {
                let exec = self.execs.lanes.swap_remove(at).exec;
                if let Some(exec) = exec {
                    // Its lanes were the connection's, gone with it.
                    let _ = self.let_exec_go(exec);
                }
                continue;
            }
            at = at.saturating_add(1);
        }
    }

    /// The controller reads no more of exec lane `(conn, stream)`: its exec is hung up and let
    /// go, its lanes ended. Whether it was one.
    pub(super) fn stop_exec(
        &mut self,
        io: &mut crate::net::Io<'_>,
        conn: crate::net::ConnKey,
        stream: hyper_quic::StreamId,
    ) -> bool {
        let Some(at) = self
            .execs
            .lanes
            .iter()
            .position(|l| l.conn == conn && l.stream == stream)
        else {
            return false;
        };
        let lane = self.execs.lanes.swap_remove(at);
        if let Some(exec) = lane.exec {
            self.drop_exec(io, exec);
        }
        if lane.added != Limits::default() {
            self.set_limits(io, conn);
        }
        io.stop(conn, stream);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ask's frame reads back as it was sent, step 6's spec byte for byte; one cut short,
    /// or of a spec the run protocol would not take, is the controller's error.
    #[test]
    fn an_ask_reads_back_as_it_was_sent() {
        let ask = Ask {
            run: RunId([4; 32]),
            generation: 9,
            member: 2,
            spec: Spec {
                argv: vec![b"sh".to_vec(), b"-c".to_vec(), b"printf \xff".to_vec()],
                env: vec![b"A=b".to_vec()],
                cwd: b"/work".to_vec(),
                tty: Some(Size { rows: 24, cols: 80 }),
                stdin: true,
                ..Spec::default()
            },
        };
        let frame = ask.frame().unwrap();
        let (tag, len) = frame::peek(&frame).unwrap().unwrap();
        assert_eq!((tag, len), (Tag::MemberExec, frame.len() - FRAME_HEAD));
        assert_eq!(Ask::decode(&frame[FRAME_HEAD..]), Ok(ask));
        assert_eq!(
            Ask::decode(&frame[FRAME_HEAD..FRAME_HEAD + SCOPE]),
            Err(Reason::Protocol)
        );
        assert_eq!(
            Ask::decode(&frame[FRAME_HEAD..FRAME_HEAD + 10]),
            Err(Reason::Protocol)
        );
    }

    /// Steering reads back; a signal no guest has, or another frame, is the controller's error;
    /// none until it has come whole.
    #[test]
    fn steering_reads_back_and_a_signal_no_guest_has_is_refused() {
        for sent in [
            Steer::Resize(Size { rows: 50, cols: 132 }),
            Steer::Signal(1),
            Steer::Signal(run::NSIG),
        ] {
            let frame = steering(sent).unwrap();
            assert_eq!(steer(&frame), Ok(Some((sent, STEER_BYTES))));
            assert_eq!(steer(&frame[..STEER_BYTES - 1]), Ok(None));
        }
        for signal in [0, run::NSIG + 1] {
            let mut frame = Vec::new();
            frame::put(&mut frame, Tag::Signal, &signal.to_be_bytes()).unwrap();
            assert_eq!(steer(&frame), Err(Reason::Protocol));
        }
        let mut stdin = Vec::new();
        frame::put(&mut stdin, Tag::Stdin, b"x").unwrap();
        assert_eq!(steer(&stdin), Err(Reason::Protocol));
    }

    /// What an exec lane is told reads back, its exit's sign kept; a refusal of a lane's own
    /// reason is no exec's word.
    #[test]
    fn what_an_exec_lane_is_told_reads_back() {
        for said in [
            Told::Started(7),
            Told::Unstarted(Unstarted::Capacity),
            Told::Unstarted(Unstarted::Refused),
            Told::Unstarted(Unstarted::Failed),
            Told::Exit(0),
            Told::Exit(137),
            Told::Exit(-1),
        ] {
            assert_eq!(told(&telling(said)), Ok(Some((said, TOLD_BYTES))));
        }
        assert_eq!(told(&super::super::refused(Reason::Scope)), Err(Reason::Protocol));
    }

    /// Each exec's streams are its own: no two execs', and no member's, share a run.
    #[test]
    fn each_execs_streams_are_scoped_apart() {
        let run = RunId([1; 32]);
        assert_ne!(exec_run(run, 0), exec_run(run, 1));
        assert_ne!(exec_run(run, 0), run);
        assert_ne!(exec_run(run, 0), exec_run(RunId([2; 32]), 0));
    }
}
