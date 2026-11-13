//! The controller's side of the agent-control endpoint (plan step 11, D238): the one connection
//! kiln or Hex holds to a microVM's endpoint, and the lanes it opens on it to the microVM's
//! members' stdio and to the commands it runs in them, so that neither writes this wire again
//! ("a controller's side is Shards' code", D238).
//!
//! - **Its lanes.** Each opens with step 5's subscription and carries step 5's frames: an
//!   output's records, one in flight, each acknowledged once its caller asks for the next
//!   ([`Output::next_record`]); a stdin's records, each within the room the endpoint said and taken
//!   before the next goes, then its end or abort ([`Input`]). Attaching to a member is this:
//!   its stdio as step 5 serves it.
//! - **Its execs** ([`Execs::exec`]). An exec's lane asks for step 6's member exec; once it has
//!   started, its stdout, stderr and stdin are lanes as a member's are, and its lane carries its
//!   resizes and signals and tells its exit ([`ExecControl`]).
//! - **Who it reaches.** No lane opens before the handshake took the microVM's pinned key
//!   ([`controller`](super::controller)), and no more open at once than the endpoint allows
//!   (MAX_STREAMS, RFC 9000 §4.6): the rest wait for its room.
//! - **Nothing retried.** A lane's refusal, its reset and the connection's end are typed
//!   ([`Fault`]). A caller that dials again opens each lane at its handle's cursor: the endpoint
//!   keeps an unacknowledged record and a stdin's place for the next subscription (step 5).
//! - **No value of its own** (R03). Every transport value is the endpoint's per-stream
//!   admission's, as the run's managed metadata gives it, summed over the lanes as the endpoint
//!   sums them ([`super::Endpoint::bind`]), an exec's while it runs.

use std::mem::size_of;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::mpsc;
use std::time::Instant;

use hyper_quic::{ClientConfig, ConnectionError, StreamId};
use shards_abi::run::managed::{Channel, InputEnd};
use shards_abi::run::{Size, Spec};

use super::exec::{self, Limits, Steer, Told};
use super::{By, Ended, REFUSED_BYTES, controller, ended, read_refused};
use crate::frame::{self, Tag};
use crate::identity::{self, Fingerprint, Identity};
use crate::managed::input::{self, Said};
use crate::managed::{
    self, AdmissionError, Admitted, CONTROL_BYTES, DATA_HEAD, Reason, RunId, Scope, Windows,
};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
const OFFSET: usize = size_of::<u64>();
/// An output's acknowledgment: a frame's head and the offset it consumed to.
const ACK_BYTES: usize = FRAME_HEAD + OFFSET;
/// A steering frame: a frame's head and a u32.
const STEER_BYTES: usize = FRAME_HEAD + size_of::<u32>();

/// The member streams a controller drives on one connection, each with the admission its
/// endpoint announced for it (the run's managed metadata) and where its lane opens: an output at
/// the first byte its controller has not consumed, a stdin at the first it has not seen taken;
/// and what each exec's streams are admitted, where the endpoint runs execs.
#[derive(Debug, Clone, Copy)]
pub struct Subscriptions<'a> {
    pub outputs: &'a [(Scope, Admitted, u64)],
    pub inputs: &'a [(input::Scope, Admitted, u64)],
    pub execs: Option<exec::Admissions>,
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
    /// An exec did not start ([`exec::Unstarted`]).
    Unstarted(exec::Unstarted),
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
    Record {
        at: u64,
        bytes: Vec<u8>,
    },
    Finished,
    Taken {
        accepted: u64,
        room: u32,
    },
    Fault(Fault),
    /// An exec started: its stdio's handles.
    Started(Box<Started>),
    /// An exec ended, with this status.
    Exit(i32),
}

/// An exec's stdio's handles, as its lane's start makes them.
#[derive(Debug)]
struct Started {
    lane: u64,
    stdout: Output,
    stderr: Option<Output>,
    stdin: Option<Input>,
}

/// What a handle asks of the driver: a lane's at most one at a time, as step 5 has one record in
/// flight each way, and an exec's asks as its controller makes them.
#[derive(Debug)]
enum Ask {
    /// An output's records to `offset` consumed.
    Consumed { lane: u64, offset: u64 },
    /// A stdin's frame (a record, or its end or abort), or an exec's steering.
    Send { lane: u64, frame: Vec<u8> },
    /// An exec's lane to open, its ask its first frame.
    Exec(Box<Asked>),
}

#[derive(Debug)]
struct Asked {
    frame: Vec<u8>,
    member: u32,
    stderr: bool,
    stdin: bool,
    tell: mpsc::Sender<Heard>,
}

/// The handle's view of a lane gone with its driver: the driver ended, which this side did.
fn gone() -> Fault {
    Fault::Lost(Ended::Local)
}

/// Where a handle asks the driver, and wakes it: one unit wake, a full handoff a wake queued
/// already, whose turn reads every ask.
#[derive(Debug)]
struct Asks {
    tx: mpsc::Sender<Ask>,
    wake: Sender<()>,
}

impl Asks {
    /// Hands `ask` over. A driver that ended told the lane why first, which its handle hears.
    fn hand(&self, ask: Ask) {
        if self.tx.send(ask).is_ok() {
            let _ = self.wake.try_send(Command::Handler(()));
        }
    }

    fn try_clone(&self) -> Result<Asks, crate::Error> {
        Ok(Asks {
            tx: self.tx.clone(),
            wake: self.wake.try_clone()?,
        })
    }
}

/// A member's stdout or stderr, or an exec's, as its controller reads it.
#[derive(Debug)]
pub struct Output {
    lane: u64,
    asks: Asks,
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
            self.asks.hand(Ask::Consumed {
                lane: self.lane,
                offset,
            });
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
            Ok(_) | Err(_) => (Err(gone()), Some(Err(gone()))),
        };
        self.over = over;
        got
    }

    /// The first byte not yet acknowledged: where a later dial's lane opens.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

/// A member's stdin, or an exec's, as its controller writes it.
#[derive(Debug)]
pub struct Input {
    lane: u64,
    asks: Asks,
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
        self.send(frame);
        self.room = None;
        // The driver took that taken only where this record ends (`Pace`).
        let (next, _) = self.room()?;
        self.cursor = next;
        Ok(len)
    }

    /// Ends the stdin once all before it is taken (step 5's end, errno 0), and waits for the
    /// member's init to tell how it went: how many bytes its stdin took.
    pub fn end(&mut self) -> Result<(u64, InputEnd), Fault> {
        let (accepted, _) = self.room()?;
        self.send(input::end(accepted, 0).to_vec());
        self.ended()
    }

    /// Aborts the stdin with `errno`, whatever is in flight, and waits for the member's init to
    /// tell how it went. An errno is positive and below Linux's MAX_ERRNO, 4095
    /// (include/linux/err.h), which a u16 holds.
    pub fn abort(&mut self, errno: NonZeroU16) -> Result<(u64, InputEnd), Fault> {
        if let Some(fault) = self.over {
            return ended_or(fault);
        }
        self.send(input::end(self.cursor, i32::from(errno.get())).to_vec());
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
                Ok(Heard::Fault(fault)) => self.over = Some(fault),
                Ok(_) | Err(_) => self.over = Some(gone()),
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

    fn send(&self, frame: Vec<u8>) {
        self.asks.hand(Ask::Send {
            lane: self.lane,
            frame,
        });
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

/// Where a controller asks for execs in the microVM's members, on its one connection.
#[derive(Debug)]
pub struct Execs {
    asks: Asks,
    run: RunId,
    generation: u64,
}

impl Execs {
    /// Runs `spec` in member `member` (step 6's member exec, D243: as the member runs, its user
    /// and setup the member's own) and waits until it has started: its stdio's handles and its
    /// control. Why it did not start, typed: the endpoint's refusal of its lane, the host's
    /// [`Fault::Unstarted`], or the connection's end.
    pub fn exec(&self, member: u32, spec: Spec) -> Result<Exec, Fault> {
        let (stderr, stdin) = (spec.tty.is_none(), spec.stdin);
        let ask = exec::Ask {
            run: self.run,
            generation: self.generation,
            member,
            spec,
        };
        let frame = ask.frame().map_err(|_| Fault::Refused(Reason::Protocol))?;
        let (tell, heard) = mpsc::channel();
        self.asks.hand(Ask::Exec(Box::new(Asked {
            frame,
            member,
            stderr,
            stdin,
            tell,
        })));
        let started = match heard.recv() {
            Ok(Heard::Started(started)) => started,
            Ok(Heard::Fault(fault)) => return Err(fault),
            Ok(_) | Err(_) => return Err(gone()),
        };
        let Started {
            lane,
            stdout,
            stderr,
            stdin,
        } = *started;
        Ok(Exec {
            stdout,
            stderr,
            stdin,
            control: ExecControl {
                lane,
                asks: self.asks.try_clone().map_err(|_| gone())?,
                heard,
                exit: None,
            },
        })
    }
}

/// A command running in a member: its stdout, its stderr (none on a terminal, whose output is
/// its stdout), its stdin (where it reads one), and its control.
#[derive(Debug)]
pub struct Exec {
    pub stdout: Output,
    pub stderr: Option<Output>,
    pub stdin: Option<Input>,
    pub control: ExecControl,
}

/// An exec's lane: its resizes and signals, and its exit.
#[derive(Debug)]
pub struct ExecControl {
    lane: u64,
    asks: Asks,
    heard: mpsc::Receiver<Heard>,
    exit: Option<Result<i32, Fault>>,
}

impl ExecControl {
    /// Sizes its terminal (step 6's `EXEC_RESIZE`).
    pub fn resize(&self, size: Size) -> Result<(), Fault> {
        self.steer(Steer::Resize(size))
    }

    /// Signals it with `signal`, a Linux signal's number, `1..=run::NSIG`
    /// (step 6's `EXEC_SIGNAL`).
    pub fn signal(&self, signal: u32) -> Result<(), Fault> {
        if !(1..=shards_abi::run::NSIG).contains(&signal) {
            return Err(Fault::Refused(Reason::Protocol));
        }
        self.steer(Steer::Signal(signal))
    }

    fn steer(&self, steer: Steer) -> Result<(), Fault> {
        let frame = exec::steering(steer).map_err(|_| gone())?;
        self.asks.hand(Ask::Send {
            lane: self.lane,
            frame,
        });
        Ok(())
    }

    /// Waits for it to end: its status, its command's or 128 plus the signal that ended it. Its
    /// endpoint tells it once its outputs have finished, every record consumed.
    pub fn wait(&mut self) -> Result<i32, Fault> {
        if let Some(exit) = self.exit {
            return exit;
        }
        let exit = match self.heard.recv() {
            Ok(Heard::Exit(status)) => Ok(status),
            Ok(Heard::Fault(fault)) => Err(fault),
            Ok(_) | Err(_) => Err(gone()),
        };
        self.exit = Some(exit);
        exit
    }
}

/// Ends the controller's connection and every lane on it: the microVM's control is over here.
/// Dropping it does so too.
#[derive(Debug)]
pub struct Hangup {
    sender: Sender<()>,
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

/// The handles of what a controller dialed, in the order its [`Subscriptions`] named them, and
/// where it asks for execs, where its subscriptions admit them.
#[derive(Debug)]
pub struct Dialed {
    pub outputs: Vec<Output>,
    pub inputs: Vec<Input>,
    pub execs: Option<Execs>,
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
        let mut id = 0u64;
        for (scope, admitted, from) in subscriptions.outputs {
            let (tell, heard) = mpsc::channel();
            lanes.push(output_lane(id, *scope, admitted, *from, tell)?);
            receivers.push((id, heard, *from));
            id = id.saturating_add(1);
        }
        for (scope, admitted, from) in subscriptions.inputs {
            let (tell, heard) = mpsc::channel();
            lanes.push(input_lane(id, *scope, admitted, *from, tell)?);
            receivers.push((id, heard, *from));
            id = id.saturating_add(1);
        }
        let run = subscriptions
            .outputs
            .first()
            .map(|(scope, _, _)| (scope.run, scope.generation));
        let (asks_tx, asks) = mpsc::channel();
        let drive = Drive {
            to,
            vm,
            config: Some(config),
            since: Instant::now(),
            conn: None,
            cause: None,
            lanes,
            over: false,
            asks,
            asks_tx: asks_tx.clone(),
            wake: None,
            next_id: id,
            run,
            admissions: subscriptions.execs,
            base: Limits {
                // The endpoint opens no lane (D238).
                streams: 0,
                receive: u64::try_from(windows.connection).unwrap_or(u64::MAX),
                send: windows.send,
            },
        };
        let any: SocketAddr = if to.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        let (mut driver, sender) = Driver::bind_bounded(
            any,
            None,
            drive,
            DriverLimits {
                connections: 1,
                // A wake is one unit whoever sends it, and its turn reads every ask.
                command_slots: 1,
                queued_datagram_bytes: link.datagram_bytes,
                handshake_patience: link.patience,
            },
        )?;
        driver.handler_mut().wake = Some(sender.try_clone()?);
        let asks = |sender: &Sender<()>| -> Result<Asks, managed::Error> {
            Ok(Asks {
                tx: asks_tx.clone(),
                wake: sender.try_clone()?,
            })
        };
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(subscriptions.outputs.len())
            .map_err(|_| allocation())?;
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(subscriptions.inputs.len())
            .map_err(|_| allocation())?;
        for (at, (lane, heard, from)) in receivers.into_iter().enumerate() {
            if at < subscriptions.outputs.len() {
                outputs.push(Output {
                    lane,
                    asks: asks(&sender)?,
                    heard,
                    cursor: from,
                    held: None,
                    over: None,
                });
            } else {
                inputs.push(Input {
                    lane,
                    asks: asks(&sender)?,
                    heard,
                    cursor: from,
                    room: None,
                    over: None,
                });
            }
        }
        let execs = match (subscriptions.execs, run) {
            (Some(_), Some((run, generation))) => Some(Execs {
                asks: asks(&sender)?,
                run,
                generation,
            }),
            _ => None,
        };
        Ok((
            Controller { driver },
            Dialed {
                outputs,
                inputs,
                execs,
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

/// An output's lane, subscribed at `from`: what it sends (its subscription, one acknowledgment)
/// and the largest frame it reads (a record and its head, or a refusal).
fn output_lane(
    id: u64,
    scope: Scope,
    admitted: &Admitted,
    from: u64,
    tell: mpsc::Sender<Heard>,
) -> Result<Lane, managed::Error> {
    let most = admitted
        .record_bytes
        .checked_add(DATA_HEAD)
        .ok_or(managed::Error::Admission(AdmissionError::Window))?;
    Lane::new(
        id,
        Carries::Output {
            most,
            next: from,
            held: false,
        },
        &managed::subscribe(scope, from),
        CONTROL_BYTES.saturating_add(ACK_BYTES),
        most.max(REFUSED_BYTES),
        tell,
    )
}

/// A stdin's lane, subscribed at `from`.
fn input_lane(
    id: u64,
    scope: input::Scope,
    admitted: &Admitted,
    from: u64,
    tell: mpsc::Sender<Heard>,
) -> Result<Lane, managed::Error> {
    Lane::new(
        id,
        Carries::Input {
            pace: Pace::First { from },
        },
        &input::subscribe(scope, from),
        input_sends(admitted)?,
        input_receives(),
        tell,
    )
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

/// The largest frame the endpoint sends on an exec's lane: what it tells, or a refusal.
fn exec_receives() -> usize {
    exec::TOLD_BYTES.max(REFUSED_BYTES)
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
/// least patience, the largest handshake and datagrams. An exec's lanes add theirs while it
/// runs ([`Drive::exec_started`]).
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
    if let Some(execs) = subscriptions.execs {
        // An exec's stdio lanes read a record each through one stream's window.
        let receive = execs
            .output
            .record_bytes
            .checked_add(DATA_HEAD)
            .ok_or_else(window)?;
        windows.stream = windows.stream.max(receive).max(exec_receives());
    }
    let link = link.ok_or(managed::Error::Admission(AdmissionError::Capacity))?;
    Ok((link, windows))
}

/// What an exec's lanes add to the controller's connection while it runs: what the endpoint
/// sends on each before it is read (a record and its head on each output, a stdin's word, what
/// the exec's lane is told) and what the controller sends on each (each output's subscription
/// and acknowledgment, a stdin's subscription, record and end, a steer), as its dialed lanes'.
fn exec_limits(admissions: exec::Admissions, stderr: bool, stdin: bool) -> Option<Limits> {
    let outputs = 1u64.checked_add(u64::from(stderr))?;
    let record = u64::try_from(admissions.output.record_bytes.checked_add(DATA_HEAD)?).ok()?;
    let mut receive = u64::try_from(exec_receives())
        .ok()?
        .checked_add(record.checked_mul(outputs)?)?;
    let mut send = u64::try_from(
        STEER_BYTES.checked_add(
            CONTROL_BYTES
                .checked_add(ACK_BYTES)?
                .checked_mul(usize::from(stderr) + 1)?,
        )?,
    )
    .ok()?;
    if stdin {
        receive = receive.checked_add(u64::try_from(input_receives()).ok()?)?;
        send = send.checked_add(u64::try_from(input_sends(&admissions.input).ok()?).ok()?)?;
    }
    Some(Limits {
        streams: 0,
        receive,
        send,
    })
}

/// What a lane carries.
#[derive(Debug, Clone, Copy)]
enum Carries {
    /// A member's output: its largest record frame, where its next record starts, and whether
    /// one is handed out and not yet acknowledged (one in flight, step 5).
    Output { most: usize, next: u64, held: bool },
    /// A member's stdin: what its next taken may say.
    Input { pace: Pace },
    /// An exec's lane: the member it runs in and which stdio it has, its number once started,
    /// and what its lanes add to the connection while it runs.
    Exec {
        member: u32,
        stderr: bool,
        stdin: bool,
        started: Option<u32>,
        added: Limits,
    },
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
    /// Its handle told its end; nothing more of it is read, and it is let go.
    Over,
}

struct Lane {
    /// Its number for its handle's asks: a lane let go answers none.
    id: u64,
    carries: Carries,
    stage: Stage,
    /// What it has to send, `sent` of it gone: its first frame first.
    out: Vec<u8>,
    sent: usize,
    /// The endpoint's next frame as it comes, its largest at most.
    inbound: Vec<u8>,
    tell: mpsc::Sender<Heard>,
}

impl Lane {
    fn new(
        id: u64,
        carries: Carries,
        first: &[u8],
        sends: usize,
        receives: usize,
        tell: mpsc::Sender<Heard>,
    ) -> Result<Lane, managed::Error> {
        let allocation = || managed::Error::Admission(AdmissionError::Allocation);
        let mut out = Vec::new();
        out.try_reserve_exact(sends.max(first.len()))
            .map_err(|_| allocation())?;
        out.extend_from_slice(first);
        let mut inbound = Vec::new();
        inbound.try_reserve_exact(receives).map_err(|_| allocation())?;
        Ok(Lane {
            id,
            carries,
            stage: Stage::Waiting,
            out,
            sent: 0,
            inbound,
            tell,
        })
    }

    /// Tells its handle `heard`, which ends the lane for a fault, an output's finish or an
    /// exec's exit. A handle dropped hears nothing.
    fn tell(&mut self, heard: Heard) {
        if matches!(heard, Heard::Fault(_) | Heard::Finished | Heard::Exit(_)) {
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

    /// Adds `frame` to what it has to send: within what was reserved for it, or, for an exec's
    /// steering, which its controller sends as it likes, in room made for it.
    fn queue(&mut self, frame: &[u8]) -> bool {
        let fits = self
            .out
            .len()
            .checked_add(frame.len())
            .is_some_and(|n| n <= self.out.capacity());
        let room = fits
            || (matches!(self.carries, Carries::Exec { .. }) && self.out.try_reserve(frame.len()).is_ok());
        if room {
            self.out.extend_from_slice(frame);
        }
        room
    }

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

/// What a lane found wrong: the endpoint broke its protocol, or this side had no memory or room
/// for what came or was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    Protocol,
    Local,
}

/// What a lane's frame says: something for its handle, or, on an exec's lane, that the exec
/// started under this number.
#[derive(Debug)]
enum Came {
    Heard(Heard),
    Started(u32),
}

/// One frame of the endpoint's at the front of `bytes` for a lane carrying `carries`, `heard`
/// whether one came before (a refusal comes first or not at all), and its length; none until it
/// has come whole. A frame of another kind or of a length its lane does not admit is broken, as
/// soon as its head says so.
fn frame_of(bytes: &[u8], carries: Carries, heard: bool) -> Result<Option<(Came, usize)>, Broken> {
    match read_refused(bytes) {
        Ok(None) => return Ok(None),
        Ok(Some(reason)) if !heard => {
            let refused = Heard::Fault(Fault::Refused(reason));
            return Ok(Some((Came::Heard(refused), REFUSED_BYTES)));
        }
        Ok(Some(_)) => return Err(Broken::Protocol),
        // An exec's lane tells why its exec did not start in a refusal of its own codes.
        Err(None) if !matches!(carries, Carries::Exec { .. }) => return Err(Broken::Protocol),
        Err(_) => {}
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
            Ok(Some((Came::Heard(Heard::Record { at, bytes }), whole)))
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
                Ok(Some((Came::Heard(Heard::Taken { accepted, room }), n)))
            }
            Some((Said::Ended { written, end }, n)) => {
                let ended = Heard::Fault(Fault::InputEnded { written, end });
                Ok(Some((Came::Heard(ended), n)))
            }
        },
        Carries::Exec { started, .. } => match exec::told(bytes).map_err(|_| Broken::Protocol)? {
            None => Ok(None),
            Some((Told::Started(exec), n)) if started.is_none() && !heard => {
                Ok(Some((Came::Started(exec), n)))
            }
            Some((Told::Unstarted(why), n)) if started.is_none() && !heard => {
                Ok(Some((Came::Heard(Heard::Fault(Fault::Unstarted(why))), n)))
            }
            Some((Told::Exit(status), n)) if started.is_some() => {
                Ok(Some((Came::Heard(Heard::Exit(status)), n)))
            }
            Some(_) => Err(Broken::Protocol),
        },
    }
}

/// The controller's handler: its one connection, and its lanes.
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
    asks: mpsc::Receiver<Ask>,
    asks_tx: mpsc::Sender<Ask>,
    wake: Option<Sender<()>>,
    next_id: u64,
    /// The run and generation an exec is asked for in, from its dialed outputs.
    run: Option<(RunId, u64)>,
    admissions: Option<exec::Admissions>,
    /// The connection's windows with no exec running.
    base: Limits,
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
    /// or room for it (`Closed`).
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

    /// Lanes over let go, and the connection's windows those of the execs still running.
    fn sweep(&mut self, io: &mut Io<'_>) {
        let before = self.lanes.len();
        self.lanes.retain(|l| l.stage != Stage::Over);
        if let (Some(conn), true) = (self.conn, self.lanes.len() != before) {
            let limits = self.lanes.iter().try_fold(self.base, |sum, l| match l.carries {
                Carries::Exec { added, .. } => sum.plus(added),
                _ => Some(sum),
            });
            if let Some(limits) = limits {
                let _ = io.limit(conn, limits.streams, limits.receive, limits.send);
            }
        }
    }

    /// Reads lane `at` as far as it has come: each whole frame told, its stream's end and reset
    /// as its protocol has them. An exec that started stops it there: its lanes are made before
    /// anything after is told ([`Drive::exec_started`]).
    fn read(&mut self, io: &mut Io<'_>, conn: ConnKey, at: usize) -> Result<Option<u32>, Broken> {
        let Some(lane) = self.lanes.get_mut(at) else {
            return Ok(None);
        };
        loop {
            let Stage::Open { stream, heard } = lane.stage else {
                return Ok(None);
            };
            let mut heard = heard;
            while let Some((came, len)) = frame_of(&lane.inbound, lane.carries, heard)? {
                lane.inbound.drain(..len.min(lane.inbound.len()));
                heard = true;
                lane.stage = Stage::Open { stream, heard };
                let said = match came {
                    Came::Started(exec) => return Ok(Some(exec)),
                    Came::Heard(said) => said,
                };
                match (&said, &mut lane.carries) {
                    (Heard::Record { at, bytes }, Carries::Output { next, held, .. }) => {
                        *next = at.checked_add(bytes.len() as u64).ok_or(Broken::Protocol)?;
                        *held = true;
                    }
                    (Heard::Taken { .. }, Carries::Input { pace }) => *pace = Pace::Told,
                    _ => {}
                }
                lane.tell(said);
                if lane.stage == Stage::Over {
                    let _ = io.finish(conn, stream);
                    io.stop(conn, stream);
                    return Ok(None);
                }
            }
            let room = lane.inbound.capacity().saturating_sub(lane.inbound.len());
            let (bytes, end) = match io.read_within(conn, stream, room).map_err(|_| Broken::Local)? {
                Read::Bytes(bytes) => (bytes, false),
                Read::End(bytes) => (bytes, true),
                Read::Blocked if room == 0 => return Err(Broken::Protocol),
                Read::Blocked => return Ok(None),
                Read::Reset => {
                    lane.tell(Heard::Fault(Fault::Revoked));
                    io.stop(conn, stream);
                    return Ok(None);
                }
            };
            if !lane.queue_inbound(&bytes) {
                return Err(Broken::Protocol);
            }
            if end {
                // Its frames first: then an output's lane is finished once every record is
                // consumed, and any other only after its last word or a refusal (told).
                while let Some((came, len)) = frame_of(&lane.inbound, lane.carries, heard)? {
                    lane.inbound.drain(..len.min(lane.inbound.len()));
                    heard = true;
                    lane.stage = Stage::Open { stream, heard };
                    match came {
                        Came::Started(exec) => return Ok(Some(exec)),
                        Came::Heard(said) => {
                            if let (Heard::Record { at, bytes }, Carries::Output { next, held, .. }) =
                                (&said, &mut lane.carries)
                            {
                                *next = at.checked_add(bytes.len() as u64).ok_or(Broken::Protocol)?;
                                *held = true;
                            }
                            lane.tell(said);
                            if lane.stage == Stage::Over {
                                return Ok(None);
                            }
                        }
                    }
                }
                return match lane.carries {
                    Carries::Output { held: false, .. } if lane.inbound.is_empty() => {
                        lane.tell(Heard::Finished);
                        let _ = io.finish(conn, stream);
                        Ok(None)
                    }
                    _ => Err(Broken::Protocol),
                };
            }
        }
    }

    /// Exec lane `at` started exec `exec`: its stdio's lanes made and opened, as dialed lanes
    /// are, at its streams ([`exec::exec_run`]) from their first bytes; the connection's windows
    /// raised by them; and their handles told to the exec's caller.
    fn exec_started(&mut self, io: &mut Io<'_>, conn: ConnKey, at: usize, exec: u32) -> Result<(), Broken> {
        let (Some((run, generation)), Some(admissions)) = (self.run, self.admissions) else {
            return Err(Broken::Protocol);
        };
        let Some(lane) = self.lanes.get(at) else {
            return Ok(());
        };
        let Carries::Exec {
            member,
            stderr,
            stdin,
            started: None,
            ..
        } = lane.carries
        else {
            return Err(Broken::Protocol);
        };
        let (id, tell) = (lane.id, lane.tell.clone());
        let wake = self
            .wake
            .as_ref()
            .ok_or(Broken::Local)?
            .try_clone()
            .map_err(|_| Broken::Local)?;
        let streams = exec::exec_run(run, exec);
        let local = |_| Broken::Local;
        let mut made = Vec::new();
        made.try_reserve_exact(3).map_err(|_| Broken::Local)?;
        let mut output = |channel| -> Result<Output, Broken> {
            let lane = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            let (tell, heard) = mpsc::channel();
            let scope = Scope {
                run: streams,
                generation,
                member,
                channel,
            };
            made.push(output_lane(lane, scope, &admissions.output, 0, tell).map_err(local)?);
            Ok(Output {
                lane,
                asks: Asks {
                    tx: self.asks_tx.clone(),
                    wake: wake.try_clone().map_err(|_| Broken::Local)?,
                },
                heard,
                cursor: 0,
                held: None,
                over: None,
            })
        };
        let stdout = output(Channel::Stdout)?;
        let stderr = match stderr {
            true => Some(output(Channel::Stderr)?),
            false => None,
        };
        let stdin = match stdin {
            true => {
                let lane = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
                let (tell, heard) = mpsc::channel();
                let scope = input::Scope {
                    run: streams,
                    generation,
                    member,
                };
                made.push(input_lane(lane, scope, &admissions.input, 0, tell).map_err(local)?);
                Some(Input {
                    lane,
                    asks: Asks {
                        tx: self.asks_tx.clone(),
                        wake: wake.try_clone().map_err(|_| Broken::Local)?,
                    },
                    heard,
                    cursor: 0,
                    room: None,
                    over: None,
                })
            }
            false => None,
        };
        self.lanes.try_reserve(made.len()).map_err(|_| Broken::Local)?;
        let added = exec_limits(admissions, stderr.is_some(), stdin.is_some()).ok_or(Broken::Local)?;
        if let Some(Carries::Exec {
            started, added: kept, ..
        }) = self.lanes.get_mut(at).map(|l| &mut l.carries)
        {
            *started = Some(exec);
            *kept = added;
        }
        self.lanes.extend(made);
        let limits = self
            .lanes
            .iter()
            .try_fold(self.base, |sum, l| match l.carries {
                Carries::Exec { added, .. } => sum.plus(added),
                _ => Some(sum),
            })
            .ok_or(Broken::Local)?;
        let _ = io.limit(conn, limits.streams, limits.receive, limits.send);
        let _ = tell.send(Heard::Started(Box::new(Started {
            lane: id,
            stdout,
            stderr,
            stdin,
        })));
        self.open(io);
        Ok(())
    }

    /// Reads lane `at`, an exec that started there given its lanes before its lane reads on.
    fn read_on(&mut self, io: &mut Io<'_>, conn: ConnKey, at: usize) -> Result<(), Broken> {
        while let Some(exec) = self.read(io, conn, at)? {
            self.exec_started(io, conn, at, exec)?;
        }
        Ok(())
    }

    /// Acts on what a handle asked.
    fn ask(&mut self, io: &mut Io<'_>, ask: Ask) -> Result<(), Broken> {
        let (lane, frame) = match ask {
            Ask::Exec(asked) => {
                let Asked {
                    frame,
                    member,
                    stderr,
                    stdin,
                    tell,
                } = *asked;
                let id = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
                let carries = Carries::Exec {
                    member,
                    stderr,
                    stdin,
                    started: None,
                    added: Limits::default(),
                };
                match Lane::new(id, carries, &frame, frame.len(), exec_receives(), tell.clone()) {
                    Ok(lane) if self.lanes.try_reserve(1).is_ok() => self.lanes.push(lane),
                    _ => {
                        let _ = tell.send(Heard::Fault(Fault::Lost(Ended::Local)));
                        return Ok(());
                    }
                }
                self.open(io);
                return Ok(());
            }
            Ask::Consumed { lane, offset } => (lane, managed::consumed(offset).to_vec()),
            Ask::Send { lane, frame } => (lane, frame),
        };
        let Some(conn) = self.conn else {
            return Ok(());
        };
        let Some(lane) = self
            .lanes
            .iter_mut()
            .find(|l| l.id == lane && l.stage != Stage::Over)
        else {
            return Ok(());
        };
        match &mut lane.carries {
            // An acknowledgment of the record handed out, whole, and nothing else.
            Carries::Output { next, held, .. } => {
                let consumed = frame
                    .get(FRAME_HEAD..)
                    .and_then(|o| <[u8; OFFSET]>::try_from(o).ok())
                    .map(u64::from_be_bytes);
                if !*held || consumed != Some(*next) {
                    return Ok(());
                }
                *held = false;
            }
            Carries::Input { pace } => {
                if let Some(next) = record_end(&frame) {
                    *pace = Pace::Sent { next };
                }
            }
            Carries::Exec { started, .. } => {
                // Steering goes once its exec has started.
                if started.is_none() {
                    return Ok(());
                }
            }
        }
        if !lane.queue(&frame) {
            return Err(Broken::Local);
        }
        lane.flush(io, conn);
        Ok(())
    }
}

impl Handler for Drive {
    type Command = ();

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
        if let Err(broken) = self.read_on(io, conn, at) {
            self.broke(io, conn, broken);
        }
        self.sweep(io);
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

    fn command(&mut self, io: &mut Io<'_>, (): ()) {
        while let Ok(ask) = self.asks.try_recv() {
            if let Err(broken) = self.ask(io, ask) {
                if let Some(conn) = self.conn {
                    self.broke(io, conn, broken);
                }
                break;
            }
        }
        self.sweep(io);
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
