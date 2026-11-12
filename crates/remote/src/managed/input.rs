//! A member's stdin, written by its owner over the managed endpoint's transport (plan step 5;
//! D231 decision v1 §3.3, docs/research/runtime-image-boot-decision-v1.md): the owner pinned by
//! its key as an output channel's is ([`super::Endpoint`]), one bidirectional stream, and one
//! record in flight. The owner subscribes naming the run, its generation, the member and the
//! stdin channel, and the first of its bytes it has not seen taken; the endpoint says where the
//! next record starts and how long it may be ([`Said::Taken`]) once the record before it is in
//! the member's input connection, so that the member's reading paces its owner's writing and
//! the endpoint holds one record at most. A record is taken whole or not at all: one cut short
//! by a lost connection is let go, and its owner sends it again from the offset it is told on
//! its next subscription, which may be past what it last heard. The owner's end, and an abort
//! of its own at any time, are frames of their own; the input's end, as the member's init
//! tells it, comes back ([`Said::Ended`]) with how many bytes the member's stdin took.
//!
//! An owner's connection lost or closed takes nothing from the member: what it sent whole goes
//! on, and the member's stdin stays open for its next subscription.
//!
//! Frames, each a [`Tag`], a big-endian u32 length and its payload (crate::frame):
//!
//! | From | Tag | Payload |
//! |---|---|---|
//! | owner | `Exec` | subscribe: run (32), generation (u64), member (u32), channel 0 (u8), offset (u64) |
//! | owner | `Stdin` | a record: its offset (u64), then 1 to `room` bytes |
//! | owner | `StdinEnd` | its offset (u64), errno (i32): 0 its end, more its abort |
//! | endpoint | `Stdin` | taken: the next record's offset (u64), its room (u32) |
//! | endpoint | `Exit` | ended: bytes the member's stdin took (u64), errno (i32, 0 its owner's end) |

use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use hyper_quic::{ClientConfig, StreamId};
use shards_abi::run::managed::InputEnd;

use super::{AdmissionError, Admitted, Error, ProtocolError, Reason, RunId};
use crate::frame::{self, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use crate::tls;

/// The channel an input's subscription names: the run protocol's stdin.
pub const CHANNEL: u8 = shards_abi::run::kind::STDIN;

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
const OFFSET: usize = size_of::<u64>();
const ROOM: usize = size_of::<u32>();
const ERRNO: usize = size_of::<i32>();
const SUBSCRIBE: usize = size_of::<RunId>() + size_of::<u64>() + size_of::<u32>() + size_of::<u8>() + OFFSET;
/// A subscription's frame, whole.
pub const SUBSCRIBE_BYTES: usize = FRAME_HEAD + SUBSCRIBE;
/// A record's frame before its bytes.
pub const RECORD_HEAD: usize = FRAME_HEAD + OFFSET;
/// The owner's end's frame, whole.
pub const END_BYTES: usize = FRAME_HEAD + OFFSET + ERRNO;
/// The endpoint's taken frame, whole.
pub const TAKEN_BYTES: usize = FRAME_HEAD + OFFSET + ROOM;
/// The endpoint's ended frame, whole.
pub const ENDED_BYTES: usize = FRAME_HEAD + OFFSET + ERRNO;
/// The most an owner's frame head takes before its record's bytes: a subscription's.
const HEAD_MOST: usize = SUBSCRIBE_BYTES;

/// The member whose input a subscription is to: its run and its management generation, as
/// [`super::Scope`] names an output's, on the stdin channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub run: RunId,
    pub generation: u64,
    pub member: u32,
}

fn put(out: &mut [u8], at: &mut usize, part: &[u8]) {
    let end = at.saturating_add(part.len());
    if let Some(slot) = out.get_mut(*at..end) {
        slot.copy_from_slice(part);
    }
    *at = end;
}

/// The owner's subscription: `offset` is the first of its bytes it has not seen taken.
pub fn subscribe(scope: Scope, offset: u64) -> [u8; SUBSCRIBE_BYTES] {
    let mut out = [0; SUBSCRIBE_BYTES];
    let mut at = 0;
    for part in [
        &[Tag::Exec as u8][..],
        &(SUBSCRIBE as u32).to_be_bytes(),
        &scope.run.0,
        &scope.generation.to_be_bytes(),
        &scope.member.to_be_bytes(),
        &[CHANNEL],
        &offset.to_be_bytes(),
    ] {
        put(&mut out, &mut at, part);
    }
    out
}

/// A record of the owner's at `offset`, appended to `out`: its bytes as they are.
pub fn record(offset: u64, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    if bytes.is_empty() {
        return Err(Error::Protocol(ProtocolError::Length));
    }
    let len = OFFSET
        .checked_add(bytes.len())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Error::Protocol(ProtocolError::Length))?;
    out.try_reserve(FRAME_HEAD.saturating_add(len as usize))
        .map_err(|_| Error::Admission(AdmissionError::Allocation))?;
    out.push(Tag::Stdin as u8);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// The owner's end at `offset` (`errno` 0), or its abort (`errno` above 0).
pub fn end(offset: u64, errno: i32) -> [u8; END_BYTES] {
    let mut out = [0; END_BYTES];
    let mut at = 0;
    for part in [
        &[Tag::StdinEnd as u8][..],
        &((OFFSET + ERRNO) as u32).to_be_bytes(),
        &offset.to_be_bytes(),
        &errno.to_be_bytes(),
    ] {
        put(&mut out, &mut at, part);
    }
    out
}

fn taken(at: u64, room: u32) -> [u8; TAKEN_BYTES] {
    let mut out = [0; TAKEN_BYTES];
    let mut cursor = 0;
    for part in [
        &[Tag::Stdin as u8][..],
        &((OFFSET + ROOM) as u32).to_be_bytes(),
        &at.to_be_bytes(),
        &room.to_be_bytes(),
    ] {
        put(&mut out, &mut cursor, part);
    }
    out
}

fn ended(written: u64, end: InputEnd) -> [u8; ENDED_BYTES] {
    let errno = match end {
        InputEnd::Closed => 0,
        InputEnd::Aborted(errno) => errno,
    };
    let mut out = [0; ENDED_BYTES];
    let mut at = 0;
    for part in [
        &[Tag::Exit as u8][..],
        &((OFFSET + ERRNO) as u32).to_be_bytes(),
        &written.to_be_bytes(),
        &errno.to_be_bytes(),
    ] {
        put(&mut out, &mut at, part);
    }
    out
}

/// What the endpoint says to its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// Every byte before `accepted` is taken: the next record starts there, `room` bytes at
    /// most.
    Taken { accepted: u64, room: u32 },
    /// The input has ended, the member's stdin having taken `written` bytes.
    Ended { written: u64, end: InputEnd },
}

/// The endpoint's first whole frame at the front of `bytes`, for an owner, and its length;
/// none until it has come whole.
pub fn said(bytes: &[u8]) -> Result<Option<(Said, usize)>, Error> {
    let protocol = |_| Error::Protocol(ProtocolError::ControlKind);
    let Some((tag, len)) = frame::peek(bytes).map_err(protocol)? else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Stdin => OFFSET + ROOM,
        Tag::Exit => OFFSET + ERRNO,
        _ => return Err(Error::Protocol(ProtocolError::ControlKind)),
    };
    if len != expected {
        return Err(Error::Protocol(ProtocolError::Length));
    }
    let end = FRAME_HEAD + len;
    let Some(payload) = bytes.get(FRAME_HEAD..end) else {
        return Ok(None);
    };
    let (offset, rest) = payload
        .split_first_chunk::<OFFSET>()
        .ok_or(Error::Protocol(ProtocolError::Length))?;
    let offset = u64::from_be_bytes(*offset);
    let said = if tag == Tag::Stdin {
        let room = <[u8; ROOM]>::try_from(rest).map_err(|_| Error::Protocol(ProtocolError::Length))?;
        Said::Taken {
            accepted: offset,
            room: u32::from_be_bytes(room),
        }
    } else {
        let errno = <[u8; ERRNO]>::try_from(rest).map_err(|_| Error::Protocol(ProtocolError::Length))?;
        let end = match i32::from_be_bytes(errno) {
            0 => InputEnd::Closed,
            errno @ 1.. => InputEnd::Aborted(errno),
            _ => return Err(Error::Protocol(ProtocolError::CloseReason)),
        };
        Said::Ended { written: offset, end }
    };
    Ok(Some((said, end)))
}

/// Where the input's one record is. `at` is always the offset of the next byte the owner has
/// not yet seen taken that the endpoint holds or waits for.
#[derive(Debug)]
enum Slot {
    /// Waiting for the owner's next record, at `at`; `buffer` holds it as it comes.
    Free { at: u64, buffer: Vec<u8> },
    /// A record at `at` of `len` bytes coming, `buffer` what has come.
    Coming { at: u64, len: usize, buffer: Vec<u8> },
    /// Whole, for the relay.
    Whole { at: u64, bytes: Vec<u8> },
    /// The relay is writing it to the member's input connection.
    Relaying { at: u64, len: usize },
    /// The owner's end (errno 0) or its abort, for the relay.
    Ending { at: u64, errno: i32 },
    /// The relay has the end or abort; the member's init is to tell how it went.
    Closing { at: u64, errno: i32 },
    /// As the member's init told it.
    Ended { written: u64, end: InputEnd },
    /// Its control let it go: the run's management scope ended.
    Revoked,
}

impl Slot {
    /// Where the owner's next record would start, past what the endpoint holds.
    fn next_start(&self) -> Option<u64> {
        match self {
            Self::Free { at, .. }
            | Self::Coming { at, .. }
            | Self::Ending { at, .. }
            | Self::Closing { at, .. } => Some(*at),
            Self::Whole { at, bytes } => at.checked_add(bytes.len() as u64),
            Self::Relaying { at, len } => at.checked_add(*len as u64),
            Self::Ended { .. } | Self::Revoked => None,
        }
    }
}

/// An observation of the input, for its owner's records and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Taking records; `at` the next one's offset.
    Open {
        at: u64,
    },
    /// Its owner's end or abort is on its way to the member.
    Ending {
        at: u64,
        errno: i32,
    },
    Ended {
        written: u64,
        end: InputEnd,
    },
    Revoked,
}

type Closer = Box<dyn Fn() + Send + Sync>;

struct Shared {
    slot: Mutex<Slot>,
    changed: Condvar,
    /// Ends the member's input connection, once it has one: an abort of its owner's, or
    /// its revocation, cannot wait for a record the member is not reading.
    closer: Mutex<Option<Closer>>,
}

fn lock(shared: &Shared) -> MutexGuard<'_, Slot> {
    shared
        .slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn close(&self) {
        let closer = self
            .closer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(close) = closer.as_ref() {
            close();
        }
    }
}

/// What the relay is given next.
#[derive(Debug)]
pub enum Next {
    /// A record whole, at `at`: for the member's input connection, then [`Relay::delivered`].
    Record { at: u64, bytes: Vec<u8> },
    /// The owner's end: every record before it delivered.
    End,
    /// The owner's abort, with its errno.
    Abort(i32),
    /// The input has ended or was let go: nothing more.
    Gone,
}

/// The daemon's side of the input: what it takes to the member's connection.
pub struct Relay {
    shared: Arc<Shared>,
    sender: Sender<()>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay").finish_non_exhaustive()
    }
}

impl Relay {
    /// Waits for the next thing to take to the member: a record, the owner's end or abort,
    /// or the input's end.
    pub fn next(&self) -> Next {
        let mut slot = lock(&self.shared);
        loop {
            match std::mem::replace(&mut *slot, Slot::Revoked) {
                Slot::Whole { at, bytes } => {
                    *slot = Slot::Relaying { at, len: bytes.len() };
                    return Next::Record { at, bytes };
                }
                Slot::Ending { at, errno } => {
                    *slot = Slot::Closing { at, errno };
                    return if errno == 0 { Next::End } else { Next::Abort(errno) };
                }
                old @ (Slot::Ended { .. } | Slot::Revoked) => {
                    *slot = old;
                    return Next::Gone;
                }
                old => {
                    *slot = old;
                    slot = self
                        .shared
                        .changed
                        .wait(slot)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
    }

    /// The record [`next`](Self::next) gave is in the member's input connection: the owner is
    /// told the next may come, and its buffer is used for it.
    pub fn delivered(&self, mut bytes: Vec<u8>) {
        {
            let mut slot = lock(&self.shared);
            if let Slot::Relaying { at, len } = *slot
                && len == bytes.len()
                && let Some(next) = at.checked_add(len as u64)
            {
                bytes.clear();
                *slot = Slot::Free {
                    at: next,
                    buffer: bytes,
                };
            }
        }
        // A full handoff already holds a wake, which flushes this too.
        let _ = self.sender.try_send(Command::Handler(()));
    }
}

/// Owns the input's service, never the member: dropping an observer stops no member.
pub struct Control {
    shared: Arc<Shared>,
    sender: Sender<()>,
    room: u32,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("room", &self.room)
            .finish_non_exhaustive()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.abort();
    }
}

impl Control {
    pub fn room(&self) -> u32 {
        self.room
    }

    /// The driver's two UDP sockets, counted once.
    pub fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        self.sender.kernel_buffer_bytes().map_err(Error::Transport)
    }

    pub fn state(&self) -> State {
        match &*lock(&self.shared) {
            Slot::Free { at, .. } | Slot::Coming { at, .. } => State::Open { at: *at },
            Slot::Whole { at, .. } | Slot::Relaying { at, .. } => State::Open { at: *at },
            Slot::Ending { at, errno } | Slot::Closing { at, errno } => State::Ending {
                at: *at,
                errno: *errno,
            },
            Slot::Ended { written, end } => State::Ended {
                written: *written,
                end: *end,
            },
            Slot::Revoked => State::Revoked,
        }
    }

    /// How to end the member's input connection, once it has one; called on the owner's
    /// abort and on revocation, outside the input's locks, and at once where either has come.
    pub fn on_close(&self, close: impl Fn() + Send + Sync + 'static) {
        *self
            .shared
            .closer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(close));
        let aborted = matches!(
            *lock(&self.shared),
            Slot::Revoked | Slot::Ending { errno: 1.., .. } | Slot::Closing { errno: 1.., .. }
        );
        if aborted {
            self.shared.close();
        }
    }

    /// The input's end as the member's init told it: its owner told it, every later record
    /// refused.
    pub fn ended(&self, written: u64, end: InputEnd) {
        {
            let mut slot = lock(&self.shared);
            if matches!(*slot, Slot::Revoked | Slot::Ended { .. }) {
                return;
            }
            *slot = Slot::Ended { written, end };
        }
        self.shared.changed.notify_all();
        let _ = self.sender.try_send(Command::Handler(()));
    }

    /// Lets the input go: its relay told, its connection to the member ended, its driver
    /// stopped. A member's stdin then reads its end.
    pub fn abort(&self) {
        {
            let mut slot = lock(&self.shared);
            if !matches!(*slot, Slot::Ended { .. }) {
                *slot = Slot::Revoked;
            }
        }
        self.shared.changed.notify_all();
        self.shared.close();
        self.sender.request_stop();
    }
}

#[derive(Debug, Clone, Copy)]
enum Subscription {
    Listening,
    Connecting(ConnKey),
    Subscribed { conn: ConnKey, stream: StreamId },
    Closed,
}

struct Service {
    scope: Scope,
    owner: Fingerprint,
    room: u32,
    shared: Arc<Shared>,
    state: Subscription,
    /// A frame head as it comes, up to a subscription's.
    head: [u8; HEAD_MOST],
    head_len: usize,
    /// What the endpoint is saying, and how much of it has gone: a taken or an ended frame.
    out: [u8; TAKEN_BYTES],
    out_len: usize,
    out_written: usize,
    /// The offset this subscription was last told is taken; whether it was told the end,
    /// and its stream finished after it.
    told: Option<u64>,
    told_end: bool,
    finished: bool,
}

const _: () = assert!(TAKEN_BYTES == ENDED_BYTES);

/// A bound input port, ready before the member starts: run its driver on a thread of its
/// own, keep its [`Control`], and give its [`Relay`] to what writes the member's connection.
pub struct Endpoint {
    driver: Driver<Service>,
    pub address: SocketAddr,
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

/// The owner's client configuration for an input: pinned to the endpoint's key, with room
/// for what the endpoint says.
pub fn client_config(
    identity: &Identity,
    server: Fingerprint,
    admitted: Admitted,
) -> Result<ClientConfig, Error> {
    // Its own sends: a subscription, or a record and its end.
    let sends = RECORD_HEAD
        .checked_add(admitted.record_bytes)
        .and_then(|n| n.checked_add(END_BYTES))
        .ok_or(Error::Admission(AdmissionError::Window))?
        .max(SUBSCRIBE_BYTES);
    let admitted = Admitted {
        transport_bytes: u64::try_from(sends).map_err(|_| Error::Admission(AdmissionError::Window))?,
        ..admitted
    };
    tls::client_with(
        identity,
        server,
        super::transport(admitted, TAKEN_BYTES)?,
        super::ALPN,
    )
    .map_err(Error::Transport)
}

impl Endpoint {
    /// Binds an input endpoint for `scope` that `owner` alone may reach. `admitted` is the
    /// caller's reservation, as an output endpoint's is: its `record_bytes` the most one
    /// record carries, of which the endpoint holds one.
    pub fn bind(
        identity: &Identity,
        owner: Fingerprint,
        scope: Scope,
        admitted: Admitted,
    ) -> Result<(Self, Relay, Control), Error> {
        let room =
            u32::try_from(admitted.record_bytes).map_err(|_| Error::Admission(AdmissionError::Capacity))?;
        if room == 0
            || admitted.record_bytes > super::RECORD_MOST
            || admitted.transport_bytes == 0
            || admitted.handshake_bytes == 0
            || admitted.patience.is_zero()
            || std::time::Instant::now().checked_add(admitted.patience).is_none()
        {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(admitted.record_bytes)
            .map_err(|_| Error::Admission(AdmissionError::Allocation))?;
        // The owner's stream holds a subscription, then one record and its end at most
        // before the endpoint reads them.
        let receive = RECORD_HEAD
            .checked_add(admitted.record_bytes)
            .and_then(|n| n.checked_add(END_BYTES))
            .ok_or(Error::Admission(AdmissionError::Window))?
            .max(SUBSCRIBE_BYTES);
        let transport = super::transport(admitted, receive)?;
        let mut config = tls::owner_server(identity, owner, transport, super::ALPN)?;
        config
            .max_incoming(1)
            .incoming_buffer_size(0)
            .incoming_buffer_size_total(0);
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::Free { at: 0, buffer }),
            changed: Condvar::new(),
            closer: Mutex::new(None),
        });
        let service = Service {
            scope,
            owner,
            room,
            shared: Arc::clone(&shared),
            state: Subscription::Listening,
            head: [0; HEAD_MOST],
            head_len: 0,
            out: [0; TAKEN_BYTES],
            out_len: 0,
            out_written: 0,
            told: None,
            told_end: false,
            finished: false,
        };
        let (driver, sender) = Driver::bind_bounded(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Some(config),
            service,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: admitted.datagram_bytes,
                handshake_patience: admitted.patience,
            },
        )?;
        let address = driver.local()?;
        if matches!(address.port(), 80 | 443) {
            return Err(Error::Admission(AdmissionError::ExcludedPort));
        }
        let relay = Relay {
            shared: Arc::clone(&shared),
            sender: sender.try_clone()?,
        };
        let control = Control { shared, sender, room };
        Ok((
            Self {
                driver,
                address,
                fingerprint: identity.fingerprint(),
            },
            relay,
            control,
        ))
    }

    pub fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        self.driver.kernel_buffer_bytes().map_err(Error::Transport)
    }

    pub fn run(mut self) -> Result<(), Error> {
        let result = self.driver.run();
        let service = self.driver.handler_mut();
        service.state = Subscription::Closed;
        {
            let mut slot = lock(&service.shared);
            if !matches!(*slot, Slot::Ended { .. }) {
                *slot = Slot::Revoked;
            }
        }
        service.shared.changed.notify_all();
        service.shared.close();
        result.map_err(Error::Transport)
    }
}

/// What the owner's frame at the head buffer says, once enough of it has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    /// More of it is needed: this many bytes.
    More(usize),
    Subscribe(Scope, u8, u64),
    Record {
        offset: u64,
        len: usize,
    },
    End {
        offset: u64,
        errno: i32,
    },
}

impl Service {
    fn refuse(&mut self, io: &mut Io<'_>, conn: ConnKey, reason: Reason) {
        io.close(conn, reason as u32, "managed input subscription refused");
        self.disconnect();
    }

    /// The subscription gone, whatever took it: a record cut short is let go, its buffer kept
    /// for the next; what came whole goes on to the member.
    fn disconnect(&mut self) {
        self.state = Subscription::Listening;
        self.head_len = 0;
        self.out_len = 0;
        self.out_written = 0;
        self.told = None;
        self.told_end = false;
        self.finished = false;
        let mut slot = lock(&self.shared);
        if let Slot::Coming { at, buffer, .. } = &mut *slot {
            let (at, mut buffer) = (*at, std::mem::take(buffer));
            buffer.clear();
            *slot = Slot::Free { at, buffer };
        }
    }

    /// Reads the head buffer: how much more it needs, or what it says.
    fn head(&self) -> Result<Head, Reason> {
        let subscribed = matches!(self.state, Subscription::Subscribed { .. });
        let bytes = self.head.get(..self.head_len).unwrap_or_default();
        if bytes.len() < FRAME_HEAD {
            return Ok(Head::More(FRAME_HEAD - bytes.len()));
        }
        let Ok(Some((tag, len))) = frame::peek(bytes) else {
            return Err(Reason::Protocol);
        };
        let fixed = match (subscribed, tag) {
            (false, Tag::Exec) if len == SUBSCRIBE => SUBSCRIBE,
            (true, Tag::Stdin) if len > OFFSET && len - OFFSET <= self.room as usize => OFFSET,
            (true, Tag::StdinEnd) if len == OFFSET + ERRNO => OFFSET + ERRNO,
            _ => return Err(Reason::Protocol),
        };
        let whole = FRAME_HEAD + fixed;
        if bytes.len() < whole {
            return Ok(Head::More(whole - bytes.len()));
        }
        let payload = bytes.get(FRAME_HEAD..whole).ok_or(Reason::Protocol)?;
        if tag == Tag::Exec {
            let (run, rest) = payload
                .split_first_chunk::<{ size_of::<RunId>() }>()
                .ok_or(Reason::Protocol)?;
            let (generation, rest) = rest.split_first_chunk::<OFFSET>().ok_or(Reason::Protocol)?;
            let (member, rest) = rest
                .split_first_chunk::<{ size_of::<u32>() }>()
                .ok_or(Reason::Protocol)?;
            let (channel, offset) = rest.split_first().ok_or(Reason::Protocol)?;
            let offset = <[u8; OFFSET]>::try_from(offset).map_err(|_| Reason::Protocol)?;
            return Ok(Head::Subscribe(
                Scope {
                    run: RunId(*run),
                    generation: u64::from_be_bytes(*generation),
                    member: u32::from_be_bytes(*member),
                },
                *channel,
                u64::from_be_bytes(offset),
            ));
        }
        let (offset, rest) = payload.split_first_chunk::<OFFSET>().ok_or(Reason::Protocol)?;
        let offset = u64::from_be_bytes(*offset);
        Ok(if tag == Tag::Stdin {
            Head::Record {
                offset,
                len: len - OFFSET,
            }
        } else {
            let errno = <[u8; ERRNO]>::try_from(rest).map_err(|_| Reason::Protocol)?;
            Head::End {
                offset,
                errno: i32::from_be_bytes(errno),
            }
        })
    }

    /// How many bytes of the owner's stream to read now: a record's, while one is coming, else
    /// what its next frame's head still needs. A frame head is read whatever the slot holds,
    /// so that an abort is heard while a record waits for the member.
    fn want(&self) -> Result<usize, Reason> {
        if !matches!(
            self.state,
            Subscription::Connecting(_) | Subscription::Subscribed { .. }
        ) {
            return Ok(0);
        }
        if let Slot::Coming { len, buffer, .. } = &*lock(&self.shared) {
            return Ok(len.saturating_sub(buffer.len()));
        }
        match self.head()? {
            Head::More(n) => Ok(n),
            // A whole head is acted on as it comes ([`Service::heard`]).
            _ => Err(Reason::Protocol),
        }
    }

    /// Bytes read, at most what [`want`](Self::want) asked: a record's, or a head's.
    fn take(&mut self, bytes: &[u8], conn: ConnKey, stream: StreamId) -> Result<(), Reason> {
        {
            let mut slot = lock(&self.shared);
            if let Slot::Coming { at, len, buffer } = &mut *slot {
                if buffer.len().saturating_add(bytes.len()) > *len {
                    return Err(Reason::Protocol);
                }
                // Within the capacity reserved at bind: a record is at most its room.
                buffer.extend_from_slice(bytes);
                if buffer.len() == *len {
                    let (at, bytes) = (*at, std::mem::take(buffer));
                    *slot = Slot::Whole { at, bytes };
                    drop(slot);
                    self.shared.changed.notify_all();
                }
                return Ok(());
            }
        }
        let end = self
            .head_len
            .checked_add(bytes.len())
            .filter(|n| *n <= HEAD_MOST)
            .ok_or(Reason::Protocol)?;
        self.head
            .get_mut(self.head_len..end)
            .ok_or(Reason::Protocol)?
            .copy_from_slice(bytes);
        self.head_len = end;
        self.heard(conn, stream)
    }

    /// Acts on the head once it has come whole.
    fn heard(&mut self, conn: ConnKey, stream: StreamId) -> Result<(), Reason> {
        let head = self.head()?;
        if matches!(head, Head::More(_)) {
            return Ok(());
        }
        self.head_len = 0;
        match head {
            Head::Subscribe(scope, channel, offset) => {
                if !matches!(self.state, Subscription::Connecting(c) if c == conn) {
                    return Err(Reason::Protocol);
                }
                if scope.run != self.scope.run || scope.member != self.scope.member || channel != CHANNEL {
                    return Err(Reason::Scope);
                }
                if scope.generation != self.scope.generation {
                    return Err(Reason::Stale);
                }
                {
                    let slot = lock(&self.shared);
                    match (slot.next_start(), &*slot) {
                        // An owner may be a record behind, the taken of its last lost with
                        // its connection; never ahead of what was taken.
                        (Some(next), _) if offset <= next => {}
                        (None, Slot::Ended { .. }) => {}
                        (None, _) => return Err(Reason::Closed),
                        (Some(_), _) => return Err(Reason::Protocol),
                    }
                }
                self.told = None;
                self.told_end = false;
                self.finished = false;
                self.state = Subscription::Subscribed { conn, stream };
                Ok(())
            }
            Head::Record { offset, len } => {
                let mut slot = lock(&self.shared);
                match std::mem::replace(&mut *slot, Slot::Revoked) {
                    Slot::Free { at, buffer } if at == offset => {
                        *slot = Slot::Coming { at, len, buffer };
                        Ok(())
                    }
                    // Before the last was taken, or not where the next starts.
                    old => {
                        *slot = old;
                        Err(Reason::Protocol)
                    }
                }
            }
            Head::End { offset, errno: 0 } => {
                let mut slot = lock(&self.shared);
                if !matches!(&*slot, Slot::Free { at, .. } if *at == offset) {
                    return Err(Reason::Protocol);
                }
                *slot = Slot::Ending { at: offset, errno: 0 };
                drop(slot);
                self.shared.changed.notify_all();
                Ok(())
            }
            Head::End { offset, errno } if errno > 0 => {
                let mut slot = lock(&self.shared);
                let at = match &*slot {
                    // Its input already ending or ended: nothing more to abort.
                    Slot::Ending { .. } | Slot::Closing { .. } | Slot::Ended { .. } | Slot::Revoked => {
                        return Ok(());
                    }
                    Slot::Free { at, .. }
                    | Slot::Coming { at, .. }
                    | Slot::Whole { at, .. }
                    | Slot::Relaying { at, .. } => *at,
                };
                if slot.next_start().is_none_or(|next| offset > next) {
                    return Err(Reason::Protocol);
                }
                *slot = Slot::Ending { at, errno };
                drop(slot);
                self.shared.changed.notify_all();
                // The relay may be held writing a record the member does not read.
                self.shared.close();
                Ok(())
            }
            Head::End { .. } | Head::More(_) => Err(Reason::Protocol),
        }
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if matches!(self.state, Subscription::Subscribed { conn: c, stream: s } if c != conn || s != stream) {
            self.refuse(io, conn, Reason::Protocol);
            return;
        }
        loop {
            let want = match self.want() {
                Ok(0) => break,
                Ok(n) => n,
                Err(reason) => {
                    self.refuse(io, conn, reason);
                    return;
                }
            };
            let (bytes, ended) = match io.read_within(conn, stream, want) {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => break,
                Ok(Read::Reset) | Err(_) => {
                    self.refuse(io, conn, Reason::Protocol);
                    return;
                }
            };
            if let Err(reason) = self.take(&bytes, conn, stream) {
                self.refuse(io, conn, reason);
                return;
            }
            // The owner's stream finished: its subscription ends, never its input. An owner keeps
            // its side open until it hears the input's end.
            if ended {
                self.refuse(io, conn, Reason::Closed);
                return;
            }
        }
        self.flush(io);
    }

    /// Says what the owner has to hear: where its next record goes once the last is taken,
    /// or the input's end, after which its stream is finished.
    fn flush(&mut self, io: &mut Io<'_>) {
        let Subscription::Subscribed { conn, stream } = self.state else {
            return;
        };
        loop {
            while self.out_written < self.out_len {
                let part = self.out.get(self.out_written..self.out_len).unwrap_or_default();
                match io.write(conn, stream, part) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => self.out_written = self.out_written.saturating_add(n).min(self.out_len),
                }
            }
            if self.told_end {
                if !self.finished {
                    self.finished = io.finish(conn, stream).is_ok();
                }
                return;
            }
            let said = match &*lock(&self.shared) {
                Slot::Free { at, .. } if self.told != Some(*at) => {
                    self.told = Some(*at);
                    taken(*at, self.room)
                }
                Slot::Ended { written, end } => {
                    self.told_end = true;
                    ended(*written, *end)
                }
                _ => return,
            };
            self.out = said;
            self.out_len = said.len();
            self.out_written = 0;
        }
    }
}

impl Handler for Service {
    type Command = ();
    fn connected(&mut self, _io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        let ours = crate::identity::fingerprint(peer) == self.owner;
        if !ours || !matches!(self.state, Subscription::Listening) {
            return Err(Close {
                code: if ours {
                    Reason::Scope as u32
                } else {
                    Reason::Auth as u32
                },
                why: "managed input owner refused",
            });
        }
        self.state = Subscription::Connecting(conn);
        Ok(())
    }
    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if !matches!(self.state, Subscription::Connecting(c) if c == conn) {
            self.refuse(io, conn, Reason::Protocol);
            return;
        }
        self.readable(io, conn, stream);
    }
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        Service::readable(self, io, conn, stream);
    }
    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, _stream: StreamId) {
        self.refuse(io, conn, Reason::Protocol);
    }
    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        if matches!(self.state, Subscription::Connecting(c) | Subscription::Subscribed { conn: c, .. } if c == conn)
        {
            self.disconnect();
        }
    }
    fn command(&mut self, io: &mut Io<'_>, (): ()) {
        // A record taken, or the input ended: what the owner has to hear.
        self.flush(io);
    }
    fn done(&self) -> bool {
        matches!(self.state, Subscription::Closed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const CONN: ConnKey = 1;
    /// The guest's errnos, Linux's (include/uapi/asm-generic/errno-base.h, errno.h), as its
    /// init reports them whatever the host's own.
    const EIO: i32 = 5;
    const EPIPE: i32 = 32;
    const ECONNRESET: i32 = 104;
    const ECANCELED: i32 = 125;
    const ROOM_TEST: usize = 32;

    fn scope() -> Scope {
        Scope {
            run: RunId([7; 32]),
            generation: 9,
            member: 2,
        }
    }

    fn admitted(patience: Duration) -> Admitted {
        Admitted {
            record_bytes: ROOM_TEST,
            transport_bytes: TAKEN_BYTES as u64,
            // RFC 8446: uint24 handshake length plus its four-byte header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: super::super::DATAGRAM_BYTES,
            patience,
        }
    }

    fn stream() -> StreamId {
        StreamId::from(hyper_quic::VarInt::from_u32(0))
    }

    /// Feeds `bytes` as the driver would, each read no more than the service asks for.
    fn feed(service: &mut Service, mut bytes: &[u8]) -> Result<(), Reason> {
        while !bytes.is_empty() {
            let n = service.want()?.min(bytes.len());
            if n == 0 {
                return Err(Reason::Protocol);
            }
            service.take(&bytes[..n], CONN, stream())?;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    fn bound() -> (Endpoint, Relay, Control) {
        let identity = Identity::generate().unwrap();
        Endpoint::bind(
            &identity,
            identity.fingerprint(),
            scope(),
            admitted(Duration::from_secs(1)),
        )
        .unwrap()
    }

    fn record_frame(offset: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        record(offset, bytes, &mut out).unwrap();
        out
    }

    #[test]
    fn frames_round_trip_and_an_owner_reads_what_the_endpoint_says() {
        let sub = subscribe(scope(), 9);
        assert_eq!(sub.len(), SUBSCRIBE_BYTES);
        assert_eq!(sub[0], Tag::Exec as u8);
        let t = taken(5, 64);
        assert_eq!(
            said(&t).unwrap(),
            Some((
                Said::Taken {
                    accepted: 5,
                    room: 64
                },
                TAKEN_BYTES
            ))
        );
        assert_eq!(
            said(&t[..TAKEN_BYTES - 1]).unwrap(),
            None,
            "not until it is whole"
        );
        for end in [InputEnd::Closed, InputEnd::Aborted(EPIPE)] {
            let e = ended(11, end);
            assert_eq!(
                said(&e).unwrap(),
                Some((Said::Ended { written: 11, end }, ENDED_BYTES))
            );
        }
        let mut negative = ended(1, InputEnd::Closed);
        negative[ENDED_BYTES - ERRNO..].copy_from_slice(&(-1i32).to_be_bytes());
        assert!(said(&negative).is_err());
        assert_eq!(record_frame(4, b"\0\xff").len(), RECORD_HEAD + 2);
        assert!(
            record(4, b"", &mut Vec::new()).is_err(),
            "a record has a byte at least"
        );
        assert!(said(&[Tag::Stdout as u8, 0, 0, 0, 0]).is_err());
        let mut long = t;
        long[4] = 13;
        assert!(said(&long).is_err());
        assert_eq!(end(3, 0)[0], Tag::StdinEnd as u8);
    }

    #[test]
    fn a_record_is_taken_whole_once_the_last_is_delivered_and_the_owners_end_closes_it() {
        let (mut endpoint, relay, control) = bound();
        let service = endpoint.driver.handler_mut();
        service.state = Subscription::Connecting(CONN);
        feed(service, &subscribe(scope(), 0)).unwrap();
        assert!(matches!(service.state, Subscription::Subscribed { .. }));
        let first = b"\0\xff\x80{\"a\":1}\n";
        feed(service, &record_frame(0, first)).unwrap();
        // Another before the first is taken is the owner's error, refused.
        assert_eq!(
            feed(service, &record_frame(first.len() as u64, b"x")),
            Err(Reason::Protocol)
        );
        let Next::Record { at: 0, bytes } = relay.next() else {
            panic!("no record")
        };
        assert_eq!(bytes, first);
        assert_eq!(control.state(), State::Open { at: 0 });
        relay.delivered(bytes);
        let at = first.len() as u64;
        assert_eq!(control.state(), State::Open { at });
        // One not where the next starts, and one past its room, are refused.
        assert_eq!(feed(service, &record_frame(at + 1, b"x")), Err(Reason::Protocol));
        service.head_len = 0;
        assert_eq!(
            feed(service, &record_frame(at, &[1; ROOM_TEST + 1])),
            Err(Reason::Protocol)
        );
        service.head_len = 0;
        // No newline, invalid UTF-8, exactly its room.
        let second = [0xc3u8; ROOM_TEST];
        feed(service, &record_frame(at, &second)).unwrap();
        let Next::Record { bytes, .. } = relay.next() else {
            panic!("no second record")
        };
        assert_eq!(bytes, second);
        relay.delivered(bytes);
        let at = at + ROOM_TEST as u64;
        // An end not where the next would start is refused; there, it goes to the relay.
        assert_eq!(feed(service, &end(at - 1, 0)), Err(Reason::Protocol));
        service.head_len = 0;
        feed(service, &end(at, 0)).unwrap();
        assert!(matches!(relay.next(), Next::End));
        assert_eq!(control.state(), State::Ending { at, errno: 0 });
        control.ended(at, InputEnd::Closed);
        assert_eq!(
            control.state(),
            State::Ended {
                written: at,
                end: InputEnd::Closed
            }
        );
        assert!(matches!(relay.next(), Next::Gone));
    }

    #[test]
    fn a_lost_subscription_lets_go_of_a_record_cut_short_and_a_resubscription_is_told_where_to_go_on() {
        let (mut endpoint, relay, control) = bound();
        let service = endpoint.driver.handler_mut();
        service.state = Subscription::Connecting(CONN);
        feed(service, &subscribe(scope(), 0)).unwrap();
        let frame = record_frame(0, b"cut-short");
        feed(service, &frame[..RECORD_HEAD + 3]).unwrap();
        service.disconnect();
        assert_eq!(control.state(), State::Open { at: 0 });
        // Taken whole on the next subscription, then lost before its taken was heard: the
        // owner comes back a record behind and is told where the next goes.
        service.state = Subscription::Connecting(CONN);
        feed(service, &subscribe(scope(), 0)).unwrap();
        feed(service, &frame).unwrap();
        service.disconnect();
        let Next::Record { at: 0, bytes } = relay.next() else {
            panic!("the whole record goes on without its owner")
        };
        relay.delivered(bytes);
        service.state = Subscription::Connecting(CONN);
        feed(service, &subscribe(scope(), 0)).unwrap();
        assert_eq!(service.told, None);
        // Never ahead of what was taken, never another run's, member's, channel's or
        // generation's.
        for (offered, offset, reason) in [
            (scope(), 10, Reason::Protocol),
            (Scope { member: 3, ..scope() }, 0, Reason::Scope),
            (
                Scope {
                    run: RunId([8; 32]),
                    ..scope()
                },
                0,
                Reason::Scope,
            ),
            (
                Scope {
                    generation: 8,
                    ..scope()
                },
                0,
                Reason::Stale,
            ),
        ] {
            service.disconnect();
            service.state = Subscription::Connecting(CONN);
            assert_eq!(
                feed(service, &subscribe(offered, offset)),
                Err(reason),
                "{offered:?}"
            );
        }
        service.disconnect();
        service.state = Subscription::Connecting(CONN);
        let mut stdout = subscribe(scope(), 0);
        stdout[SUBSCRIBE_BYTES - OFFSET - 1] = shards_abi::run::kind::STDOUT;
        assert_eq!(feed(service, &stdout), Err(Reason::Scope));
        drop(control);
    }

    #[test]
    fn an_abort_ends_a_record_the_member_does_not_read_and_its_connection_is_closed() {
        let (mut endpoint, relay, control) = bound();
        let (closes, closed) = mpsc::channel();
        control.on_close(move || {
            let _ = closes.send(());
        });
        let service = endpoint.driver.handler_mut();
        service.state = Subscription::Connecting(CONN);
        feed(service, &subscribe(scope(), 0)).unwrap();
        feed(service, &record_frame(0, b"unread")).unwrap();
        let Next::Record { .. } = relay.next() else {
            panic!("no record")
        };
        // Held by the member: the owner's abort is heard all the same.
        feed(service, &end(0, ECANCELED)).unwrap();
        closed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(relay.next(), Next::Abort(ECANCELED)));
        assert_eq!(
            control.state(),
            State::Ending {
                at: 0,
                errno: ECANCELED
            }
        );
        // The member's init tells how it went.
        control.ended(3, InputEnd::Aborted(ECONNRESET));
        assert_eq!(
            control.state(),
            State::Ended {
                written: 3,
                end: InputEnd::Aborted(ECONNRESET)
            }
        );
        // A later abort or end changes nothing.
        feed(service, &end(0, EIO)).unwrap();
        control.ended(0, InputEnd::Closed);
        assert!(matches!(control.state(), State::Ended { written: 3, .. }));
    }

    #[test]
    fn revocation_tells_the_relay_and_closes_the_members_connection_once_it_has_one() {
        let (_endpoint, relay, control) = bound();
        let (closes, closed) = mpsc::channel();
        control.abort();
        assert!(matches!(relay.next(), Next::Gone));
        // A closer given after the revocation runs at once.
        control.on_close(move || {
            let _ = closes.send(());
        });
        closed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(control.state(), State::Revoked);
    }

    /// What a test owner heard.
    #[derive(Debug)]
    enum Heard {
        Said(Said),
        End,
        Lost(String),
    }

    enum Ask {
        Connect(SocketAddr),
        Send(Vec<u8>),
        Vanish,
    }

    /// An owner as an orchestrator's would be, over the real transport on a driver of its
    /// own: it subscribes once its handshake is done, writes what it is asked to and tells
    /// what it hears.
    struct Owner {
        config: Option<ClientConfig>,
        offset: u64,
        stream: Option<(ConnKey, StreamId)>,
        pending: Vec<u8>,
        inbound: Vec<u8>,
        heard: mpsc::Sender<Heard>,
        vanished: bool,
    }

    impl Owner {
        fn flush(&mut self, io: &mut Io<'_>) {
            let Some((conn, stream)) = self.stream else {
                return;
            };
            while !self.pending.is_empty() {
                match io.write(conn, stream, &self.pending) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => drop(self.pending.drain(..n)),
                }
            }
        }
    }

    impl Handler for Owner {
        type Command = Ask;
        fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
            let stream = io.open(conn).map_err(|_| Close {
                code: 1,
                why: "no stream to subscribe on",
            })?;
            self.stream = Some((conn, stream));
            let mut first = subscribe(scope(), self.offset).to_vec();
            first.append(&mut self.pending);
            self.pending = first;
            self.flush(io);
            Ok(())
        }
        fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
            loop {
                let (bytes, ended) = match io.read(conn, stream) {
                    Read::Bytes(bytes) => (bytes, false),
                    Read::End(bytes) => (bytes, true),
                    Read::Blocked | Read::Reset => return,
                };
                self.inbound.extend_from_slice(&bytes);
                while let Ok(Some((said, n))) = said(&self.inbound) {
                    let _ = self.heard.send(Heard::Said(said));
                    self.inbound.drain(..n);
                }
                if ended {
                    let _ = self.heard.send(Heard::End);
                    return;
                }
            }
        }
        fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
            self.flush(io);
        }
        fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, why: &str, code: Option<u64>) {
            let _ = self.heard.send(Heard::Lost(format!("{why} ({code:?})")));
        }
        fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
            match ask {
                Ask::Connect(to) => {
                    if let Some(Err(e)) = self.config.take().map(|config| io.connect(to, config)) {
                        let _ = self.heard.send(Heard::Lost(e.to_string()));
                    }
                }
                Ask::Send(bytes) => {
                    self.pending.extend_from_slice(&bytes);
                    self.flush(io);
                }
                Ask::Vanish => self.vanished = true,
            }
        }
        fn done(&self) -> bool {
            self.vanished
        }
    }

    struct Live {
        owner: Sender<Ask>,
        driver: Option<std::thread::JoinHandle<Result<(), crate::Error>>>,
        heard: mpsc::Receiver<Heard>,
    }

    impl Live {
        fn connect(identity: &Identity, server: Fingerprint, to: SocketAddr, offset: u64) -> Live {
            let patience = tls::IDLE;
            let config = client_config(identity, server, admitted(patience)).unwrap();
            let (hears, heard) = mpsc::channel();
            let (mut driver, asks) = Driver::bind_bounded(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                None,
                Owner {
                    config: Some(config),
                    offset,
                    stream: None,
                    pending: Vec::new(),
                    inbound: Vec::new(),
                    heard: hears,
                    vanished: false,
                },
                DriverLimits {
                    connections: 1,
                    command_slots: 4,
                    queued_datagram_bytes: super::super::DATAGRAM_BYTES,
                    handshake_patience: patience,
                },
            )
            .unwrap();
            let driver = std::thread::Builder::new()
                .name("input-owner".into())
                .spawn(move || driver.run())
                .unwrap();
            asks.send(Command::Handler(Ask::Connect(to))).unwrap();
            Live {
                owner: asks,
                driver: Some(driver),
                heard,
            }
        }

        fn hear(&self) -> Heard {
            self.heard.recv_timeout(tls::IDLE).unwrap()
        }

        fn taken(&self) -> u64 {
            match self.hear() {
                Heard::Said(Said::Taken { accepted, room }) => {
                    assert_eq!(room as usize, ROOM_TEST);
                    accepted
                }
                other => panic!("no taken: {other:?}"),
            }
        }

        fn send(&self, bytes: Vec<u8>) {
            self.owner.send(Command::Handler(Ask::Send(bytes))).unwrap();
        }

        fn vanish(mut self) {
            self.owner.send(Command::Handler(Ask::Vanish)).unwrap();
            if let Some(driver) = self.driver.take() {
                let _ = driver.join();
            }
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.owner.request_stop();
            if let Some(driver) = self.driver.take() {
                let _ = driver.join();
            }
        }
    }

    /// An owner's subscription at `offset`, made again while the endpoint still holds a
    /// connection it has not let go of, told its first record goes at `at`.
    fn subscribed(owner: &Identity, server: Fingerprint, address: SocketAddr, offset: u64, at: u64) -> Live {
        let deadline = std::time::Instant::now() + tls::IDLE * 3;
        loop {
            let live = Live::connect(owner, server, address, offset);
            match live.hear() {
                Heard::Said(Said::Taken { accepted, .. }) => {
                    assert_eq!(accepted, at);
                    return live;
                }
                Heard::Lost(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                other => panic!("no subscription: {other:?}"),
            }
        }
    }

    /// The member's side, as the daemon's relay is: each record to `sink`, held until
    /// `gate` lets it go, the owner's end told back as the member's init would tell it.
    fn relay_into(relay: Relay, control: Arc<Control>, gate: mpsc::Receiver<()>) -> mpsc::Receiver<Vec<u8>> {
        let (sink, records) = mpsc::channel();
        std::thread::Builder::new()
            .name("input-relay".into())
            .spawn(move || {
                let mut written = 0u64;
                loop {
                    match relay.next() {
                        Next::Record { bytes, .. } => {
                            if gate.recv().is_err() {
                                return;
                            }
                            written += bytes.len() as u64;
                            let _ = sink.send(bytes.clone());
                            relay.delivered(bytes);
                        }
                        Next::End => control.ended(written, InputEnd::Closed),
                        Next::Abort(_) => control.ended(written, InputEnd::Aborted(ECONNRESET)),
                        Next::Gone => return,
                    }
                }
            })
            .unwrap();
        records
    }

    #[test]
    fn over_the_real_transport_input_goes_exactly_paced_resumed_ended_and_only_from_its_owner() {
        let server = Identity::generate().unwrap();
        let owner = Identity::generate().unwrap();
        let (endpoint, relay, control) =
            Endpoint::bind(&server, owner.fingerprint(), scope(), admitted(tls::IDLE)).unwrap();
        let address = endpoint.address;
        let control = Arc::new(control);
        let endpoint_driver = std::thread::Builder::new()
            .name("input-endpoint".into())
            .spawn(move || endpoint.run())
            .unwrap();
        let (gate, gated) = mpsc::channel();
        let records = relay_into(relay, Arc::clone(&control), gated);

        // A key that is not the owner's reaches nothing.
        let stranger = Live::connect(&Identity::generate().unwrap(), server.fingerprint(), address, 0);
        match stranger.hear() {
            Heard::Lost(why) => assert!(!why.is_empty()),
            other => panic!("a stranger's connection must be refused: {other:?}"),
        }
        drop(stranger);

        // The endpoint takes one connection at a time: the stranger's, refused, is let go
        // before the owner's is taken.
        let live = subscribed(&owner, server.fingerprint(), address, 0, 0);
        let first: &[u8] = b"\0\xff\x80{\"jsonrpc\":\"2.0\"}\n";
        live.send(record_frame(0, first));
        // Held by the member: no taken comes, so no second record may go.
        if let Ok(heard) = live.heard.recv_timeout(Duration::from_millis(300)) {
            panic!("heard while its record was held: {heard:?}");
        }
        gate.send(()).unwrap();
        assert_eq!(records.recv_timeout(tls::IDLE).unwrap(), first);
        let at = live.taken();
        assert_eq!(at, first.len() as u64);

        // Sent whole, then the owner gone before it heard its taken: the record goes on, and
        // its next subscription, a record behind, is told where to go on.
        let second: &[u8] = b"no-newline\xc3\x28";
        live.send(record_frame(at, second));
        gate.send(()).unwrap();
        assert_eq!(records.recv_timeout(tls::IDLE).unwrap(), second);
        live.vanish();
        let at = at + second.len() as u64;
        // The endpoint lets the vanished owner go within its patience; until then a second
        // connection is refused, as an output's is.
        let live = subscribed(&owner, server.fingerprint(), address, first.len() as u64, at);
        live.send(end(at, 0).to_vec());
        match live.hear() {
            Heard::Said(Said::Ended { written, end }) => {
                assert_eq!(written, at);
                assert_eq!(end, InputEnd::Closed);
            }
            other => panic!("no end: {other:?}"),
        }
        assert!(
            matches!(live.hear(), Heard::End),
            "its stream finished after the end"
        );
        drop(live);
        control.abort();
        let _ = endpoint_driver.join();
    }
}
