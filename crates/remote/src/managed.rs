//! Owner-authenticated, zero-retention managed output on the existing QUIC driver.

use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use hyper_quic::{ClientConfig, StreamId, TransportConfig, VarInt};
use shards_abi::run::managed::Channel;

use crate::frame::{self, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use crate::tls;

/// A member's stdin, written by its owner: one record in flight, its end and abort typed.
pub mod input;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    ControlKind,
    Length,
    Scope,
    Channel,
    Cursor,
    CloseReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    Capacity,
    Allocation,
    Window,
    ExcludedPort,
}

#[derive(Debug)]
pub enum Error {
    Protocol(ProtocolError),
    Admission(AdmissionError),
    Closed,
    Transport(crate::Error),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Transport(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(reason) => write!(out, "managed output protocol: {reason:?}"),
            Self::Admission(reason) => write!(out, "managed output admission: {reason:?}"),
            Self::Closed => out.write_str("managed output is closed"),
            Self::Transport(error) => std::fmt::Display::fmt(error, out),
        }
    }
}
impl std::error::Error for Error {}

/// Stable application close reasons; text is presentation, never a control key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Reason {
    Protocol = 1,
    Scope = 2,
    Stale = 3,
    Closed = 4,
    Auth = 5,
    /// More lanes than the run admits at once: refused, never queued (D251).
    Capacity = 6,
}

impl TryFrom<u32> for Reason {
    type Error = ProtocolError;
    fn try_from(code: u32) -> Result<Self, Self::Error> {
        match code {
            1 => Ok(Self::Protocol),
            2 => Ok(Self::Scope),
            3 => Ok(Self::Stale),
            4 => Ok(Self::Closed),
            5 => Ok(Self::Auth),
            6 => Ok(Self::Capacity),
            _ => Err(ProtocolError::CloseReason),
        }
    }
}

pub const ALPN: &[u8] = b"shards-managed-output/1";
pub const DATAGRAM_BYTES: usize = crate::net::DATAGRAM;

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
const OFFSET_BYTES: usize = size_of::<u64>();
const SCOPE_BYTES: usize = size_of::<RunId>() + size_of::<u64>() + size_of::<u32>() + size_of::<u8>();
const SUBSCRIBE_BYTES: usize = SCOPE_BYTES + OFFSET_BYTES;
pub const CONTROL_BYTES: usize = FRAME_HEAD + SUBSCRIBE_BYTES;
pub const DATA_HEAD: usize = FRAME_HEAD + OFFSET_BYTES;
/// The most bytes one record takes: its data frame carries the record's offset, then the
/// record whole, within one frame's [`frame::MAX_FRAME`]. The endpoint admits no window
/// past it.
pub const RECORD_MOST: usize = frame::MAX_FRAME - OFFSET_BYTES;

/// A run and its current management generation, minted by the trusted owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunId(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub run: RunId,
    pub generation: u64,
    pub member: u32,
    pub channel: Channel,
}

/// The fixed subscription message; the cursor names the first unconsumed record byte.
pub fn subscribe(scope: Scope, offset: u64) -> [u8; CONTROL_BYTES] {
    let mut out = [0u8; CONTROL_BYTES];
    let mut at = 0usize;
    for part in [
        &[Tag::Exec as u8][..],
        &(SUBSCRIBE_BYTES as u32).to_be_bytes(),
        &scope.run.0,
        &scope.generation.to_be_bytes(),
        &scope.member.to_be_bytes(),
        &[scope.channel as u8],
        &offset.to_be_bytes(),
    ] {
        let end = at.saturating_add(part.len());
        if let Some(head) = out.get_mut(at..end) {
            head.copy_from_slice(part);
        }
        at = end;
    }
    out
}

/// Sent only after the owner application consumes the complete record.
pub fn consumed(offset: u64) -> [u8; FRAME_HEAD + OFFSET_BYTES] {
    let mut out = [0u8; FRAME_HEAD + OFFSET_BYTES];
    if let Some((tag, rest)) = out.split_first_mut() {
        *tag = Tag::Stdin as u8;
        if let Some((len, bytes)) = rest.split_first_chunk_mut::<{ size_of::<u32>() }>() {
            len.copy_from_slice(&(OFFSET_BYTES as u32).to_be_bytes());
            bytes.copy_from_slice(&offset.to_be_bytes());
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    Subscribe(Scope, u64),
    Consumed(u64),
}

fn input(bytes: &[u8]) -> Result<Option<(Input, usize)>, Error> {
    let Some((tag, len)) = frame::peek(bytes).map_err(|_| Error::Protocol(ProtocolError::ControlKind))?
    else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Exec => SUBSCRIBE_BYTES,
        Tag::Stdin => OFFSET_BYTES,
        _ => return Err(Error::Protocol(ProtocolError::ControlKind)),
    };
    if len != expected {
        return Err(Error::Protocol(ProtocolError::Length));
    }
    let end = FRAME_HEAD
        .checked_add(len)
        .ok_or(Error::Protocol(ProtocolError::Length))?;
    let Some(payload) = bytes.get(FRAME_HEAD..end) else {
        return Ok(None);
    };
    let value = if tag == Tag::Exec {
        let (run, rest) = payload
            .split_first_chunk::<{ size_of::<RunId>() }>()
            .ok_or(Error::Protocol(ProtocolError::Scope))?;
        let (generation, rest) = rest
            .split_first_chunk::<OFFSET_BYTES>()
            .ok_or(Error::Protocol(ProtocolError::Scope))?;
        let (member, rest) = rest
            .split_first_chunk::<{ size_of::<u32>() }>()
            .ok_or(Error::Protocol(ProtocolError::Scope))?;
        let (channel, rest) = rest
            .split_first()
            .ok_or(Error::Protocol(ProtocolError::Channel))?;
        let channel = match channel {
            1 => Channel::Stdout,
            2 => Channel::Stderr,
            _ => return Err(Error::Protocol(ProtocolError::Channel)),
        };
        let offset =
            <[u8; OFFSET_BYTES]>::try_from(rest).map_err(|_| Error::Protocol(ProtocolError::Cursor))?;
        Input::Subscribe(
            Scope {
                run: RunId(*run),
                generation: u64::from_be_bytes(*generation),
                member: u32::from_be_bytes(*member),
                channel,
            },
            u64::from_be_bytes(offset),
        )
    } else {
        let offset =
            <[u8; OFFSET_BYTES]>::try_from(payload).map_err(|_| Error::Protocol(ProtocolError::Cursor))?;
        Input::Consumed(u64::from_be_bytes(offset))
    };
    Ok(Some((value, end)))
}

/// The caller retains a host resource reservation covering these allowances and
/// the endpoint/QUIC/kernel state. A buffer size alone is not such a reservation.
#[derive(Debug, Clone, Copy)]
pub struct Admitted {
    pub record_bytes: usize,
    pub transport_bytes: u64,
    pub handshake_bytes: usize,
    pub datagram_bytes: usize,
}

#[derive(Debug)]
enum Slot {
    Waiting {
        at: u64,
        bytes: Vec<u8>,
    },
    Ready {
        at: u64,
        bytes: Vec<u8>,
    },
    Capturing(u64),
    Record {
        start: u64,
        bytes: Vec<u8>,
        written: usize,
    },
    AwaitingAck {
        start: u64,
        bytes: Vec<u8>,
    },
    Finished(u64),
    Aborted {
        accepted: u64,
        consumed: u64,
    },
}

#[derive(Debug, Clone, Copy)]
enum Ending {
    Open,
    Finishing,
    Aborted,
}

struct Shared {
    slot: Mutex<(Slot, Ending)>,
    changed: Condvar,
    record_bytes: AtomicUsize,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Endpoint record bytes accepted and explicitly consumed; these are distinct
/// from bytes still in guest pipes, whose tail is unknown after owner revocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub accepted: u64,
    pub consumed: u64,
}

/// An observation of the authoritative slot, not a second terminal ledger.
/// Ready and Finished can have the same cursors as a revoked slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending(Progress),
    Ready(Progress),
    Capturing(Progress),
    Finished(Progress),
    Aborted(Progress),
}

impl State {
    pub fn progress(self) -> Progress {
        match self {
            Self::Pending(at)
            | Self::Ready(at)
            | Self::Capturing(at)
            | Self::Finished(at)
            | Self::Aborted(at) => at,
        }
    }
}

fn progress(slot: &Slot) -> Progress {
    match slot {
        Slot::Waiting { at, .. } | Slot::Ready { at, .. } | Slot::Capturing(at) | Slot::Finished(at) => {
            Progress {
                accepted: *at,
                consumed: *at,
            }
        }
        Slot::Record { start, bytes, .. } | Slot::AwaitingAck { start, bytes } => Progress {
            accepted: start.saturating_add(bytes.len() as u64),
            consumed: *start,
        },
        Slot::Aborted { accepted, consumed } => Progress {
            accepted: *accepted,
            consumed: *consumed,
        },
    }
}

impl Shared {
    fn state(&self) -> State {
        let state = lock(self);
        let at = progress(&state.0);
        match state.0 {
            Slot::Aborted { .. } => State::Aborted(at),
            Slot::Finished(_) => State::Finished(at),
            Slot::Ready { .. } if matches!(state.1, Ending::Open) => State::Ready(at),
            Slot::Capturing(_) if matches!(state.1, Ending::Open) => State::Capturing(at),
            _ => State::Pending(at),
        }
    }

    fn finish(&self) -> Result<(), Error> {
        {
            let mut state = lock(self);
            if matches!(state.0, Slot::Aborted { .. }) || matches!(state.1, Ending::Aborted) {
                return Err(Error::Closed);
            }
            state.1 = Ending::Finishing;
            if let Slot::Ready { at, .. } | Slot::Waiting { at, .. } = state.0 {
                state.0 = Slot::Finished(at);
            }
        }
        self.notify();
        Ok(())
    }

    fn notify(&self) {
        self.changed.notify_all();
        let wake = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

fn lock(shared: &Shared) -> MutexGuard<'_, (Slot, Ending)> {
    shared
        .slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The one owned producer. Only checked managed records enter this endpoint.
pub struct Producer {
    shared: Arc<Shared>,
    sender: Sender<()>,
}

impl std::fmt::Debug for Producer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Producer")
            .field("record_bytes", &self.shared.record_bytes)
            .finish()
    }
}

pub enum Rejected {
    Full(Vec<u8>),
    Closed(Vec<u8>),
    Invalid(Vec<u8>),
}

impl std::fmt::Debug for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, bytes) = match self {
            Self::Full(b) => ("Full", b),
            Self::Closed(b) => ("Closed", b),
            Self::Invalid(b) => ("Invalid", b),
        };
        f.debug_struct(kind)
            .field("bytes", &bytes.len())
            .finish_non_exhaustive()
    }
}

fn record_buffer(capacity: usize) -> Result<Vec<u8>, Error> {
    if capacity == 0 || capacity > RECORD_MOST {
        return Err(Error::Admission(AdmissionError::Capacity));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Admission(AdmissionError::Allocation))?;
    if bytes.capacity() > RECORD_MOST {
        return Err(Error::Admission(AdmissionError::Capacity));
    }
    Ok(bytes)
}

impl Producer {
    /// Prepares backing when extending the selected record window.
    /// The returned actual capacity includes allocator-reported excess and must
    /// be accounted before that credit is published. QUIC's window is unchanged.
    pub fn admit(&self, record_bytes: usize) -> Result<usize, Error> {
        if record_bytes == 0 || record_bytes > RECORD_MOST {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
        let current = {
            let state = lock(&self.shared);
            if !matches!(state.1, Ending::Open) || matches!(state.0, Slot::Finished(_) | Slot::Aborted { .. })
            {
                return Err(Error::Closed);
            }
            self.storage_capacity()
        };
        if record_bytes <= current {
            return Ok(current);
        }
        let bytes = record_buffer(record_bytes)?;
        let capacity = bytes.capacity();
        let mut state = lock(&self.shared);
        let open = matches!(state.1, Ending::Open);
        match &mut state.0 {
            Slot::Waiting { bytes: old, .. } | Slot::Ready { bytes: old, .. } if open => {
                if capacity > old.capacity() {
                    *old = bytes;
                    self.shared.record_bytes.store(capacity, Ordering::Release);
                }
                Ok(old.capacity())
            }
            Slot::Finished(_) | Slot::Aborted { .. } => Err(Error::Closed),
            _ => Err(Error::Admission(AdmissionError::Capacity)),
        }
    }

    /// The selected allocation capacity, including backing lent to capture.
    /// This is not a live-usage observation or a parent entitlement.
    pub fn storage_capacity(&self) -> usize {
        self.shared.record_bytes.load(Ordering::Acquire)
    }

    /// Transfers the prepared allocation to the one capture-credit owner. It
    /// must submit it or abort its Control on every cancellation/error/drop.
    /// The Capturing state prevents a second borrower issuing duplicate credit.
    pub fn take_buffer(&self) -> Result<Option<Vec<u8>>, Error> {
        let mut state = lock(&self.shared);
        if !matches!(state.1, Ending::Open) {
            return Ok(None);
        }
        let Slot::Ready { at, ref bytes } = state.0 else {
            return Ok(None);
        };
        // Existing submit(Vec) callers can replace the prepared Vec with smaller
        // owned storage. It is valid input, but cannot back the selected window.
        // Refusal preserves their owned-submit path and the Ready state.
        if bytes.capacity() < self.shared.record_bytes.load(Ordering::Acquire) {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
        let old = std::mem::replace(&mut state.0, Slot::Capturing(at));
        if let Slot::Ready { at, bytes } = old {
            state.0 = Slot::Capturing(at);
            Ok(Some(bytes))
        } else {
            state.0 = old;
            Ok(None)
        }
    }

    pub fn capacity(&self) -> usize {
        let state = lock(&self.shared);
        if matches!(state.0, Slot::Ready { .. }) && matches!(state.1, Ending::Open) {
            self.shared.record_bytes.load(Ordering::Acquire)
        } else {
            0
        }
    }

    /// The wake must be nonblocking; it is invoked after state locks are released.
    pub fn on_credit(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self
            .shared
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(wake));
        self.shared.notify();
    }

    /// Waits only on this data path; owner abort wakes it independently of data.
    pub fn wait_capacity(&self) -> Result<usize, Error> {
        let mut state = lock(&self.shared);
        loop {
            match state.0 {
                Slot::Ready { .. } if matches!(state.1, Ending::Open) => {
                    return Ok(self.shared.record_bytes.load(Ordering::Acquire));
                }
                Slot::Finished(_) | Slot::Aborted { .. } => {
                    return Err(Error::Closed);
                }
                _ if !matches!(state.1, Ending::Open) => return Err(Error::Closed),
                _ => {
                    state = self
                        .shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                }
            }
        }
    }

    /// Returns the owned record on refusal. Capacity, not only length, is charged:
    /// a short slice of a larger allocation cannot hide retained memory.
    pub fn submit(&self, bytes: Vec<u8>) -> Result<(), Rejected> {
        if bytes.is_empty() || bytes.capacity() > self.shared.record_bytes.load(Ordering::Acquire) {
            return Err(Rejected::Invalid(bytes));
        }
        {
            let mut state = lock(&self.shared);
            let start = match state.0 {
                Slot::Ready { at, .. } | Slot::Capturing(at) => at,
                Slot::Finished(_) | Slot::Aborted { .. } => return Err(Rejected::Closed(bytes)),
                _ => return Err(Rejected::Full(bytes)),
            };
            if !matches!(state.1, Ending::Open) {
                return Err(Rejected::Closed(bytes));
            }
            if start.checked_add(bytes.len() as u64).is_none() {
                return Err(Rejected::Invalid(bytes));
            }
            state.0 = Slot::Record {
                start,
                bytes,
                written: 0,
            };
        }
        // A full handoff already holds a wake, whose turn flushes this record too: the wake is
        // the same unit whoever sent it, and its handler flushes every slot it serves.
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) =
            self.sender.try_send(Command::Handler(()))
        {
            let mut state = lock(&self.shared);
            let old = std::mem::replace(
                &mut state.0,
                Slot::Aborted {
                    accepted: 0,
                    consumed: 0,
                },
            );
            drop(state);
            self.shared.notify();
            if let Slot::Record { bytes, .. } = old {
                return Err(Rejected::Closed(bytes));
            }
        }
        Ok(())
    }
}

/// Owns the output service, never the VM. Dropping an observer cannot stop a VM.
pub struct Control {
    shared: Arc<Shared>,
    sender: Sender<()>,
    stop: Stop,
}

/// What a stream's revocation stops: the driver it alone has (an endpoint of its own), or its
/// lane alone on an endpoint that serves others too (the agent-control endpoint, D238).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Driver,
    Lane,
}

impl Stop {
    /// Tells the driver of a revocation: its own stop flag, which no full handoff holds up, or
    /// a wake of its handler, whose turn ends the revoked stream's lane and no other. A full
    /// handoff already holds such a wake.
    pub(crate) fn revoke(self, sender: &Sender<()>) {
        match self {
            Stop::Driver => sender.request_stop(),
            Stop::Lane => {
                let _ = sender.try_send(Command::Handler(()));
            }
        }
    }
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.abort();
    }
}

impl Control {
    /// The driver's two UDP sockets, counted once despite cloned wake handles.
    pub fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        self.sender.kernel_buffer_bytes().map_err(Error::Transport)
    }
    pub fn progress(&self) -> Progress {
        progress(&lock(&self.shared).0)
    }

    pub fn state(&self) -> State {
        self.shared.state()
    }

    /// Ends normally after the admitted record is application-acknowledged.
    pub fn finish(&self) -> Result<(), Error> {
        self.shared.finish()?;
        // Shared owns finishing; a queued unit wake will flush that state too.
        // Only this endpoint's producer/control enqueue these unit notifications.
        match self.sender.try_send(Command::Handler(())) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Error::Transport(crate::Error::new(
                "the connection's driver has ended",
            ))),
        }
    }

    /// Revocation uses the driver's separate stop flag, even with a full handoff, where the
    /// driver is this stream's alone ([`Stop`]).
    pub fn abort(&self) {
        {
            let mut state = lock(&self.shared);
            let at = progress(&state.0);
            state.0 = Slot::Aborted {
                accepted: at.accepted,
                consumed: at.consumed,
            };
            state.1 = Ending::Aborted;
        }
        self.shared.notify();
        self.stop.revoke(&self.sender);
    }
}

#[derive(Debug, Clone, Copy)]
enum Subscription {
    Listening,
    Connecting(ConnKey),
    Subscribed { conn: ConnKey, stream: StreamId },
    Closed,
}

/// Why a subscriber's stream stopped being read: its owner ended its side (FIN), reset it, or
/// sent what the stream's protocol refuses. An endpoint of one stream closes its connection
/// for each ([`Gone::reason`]); the agent-control endpoint ends the lane alone for the first
/// two (D238).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gone {
    Ended,
    Reset,
    Refused(Reason),
}

impl Gone {
    /// The code an endpoint of one stream closes its connection with.
    pub(crate) fn reason(self) -> Reason {
        match self {
            Gone::Ended => Reason::Closed,
            Gone::Reset => Reason::Protocol,
            Gone::Refused(reason) => reason,
        }
    }
}

/// One output's subscriber, on one stream at a time: its subscription, the acknowledgments it
/// reads and the record it writes. The connection it is on is its endpoint's to admit.
pub(crate) struct Service {
    scope: Scope,
    owner: Fingerprint,
    shared: Arc<Shared>,
    state: Subscription,
    control: [u8; CONTROL_BYTES],
    control_len: usize,
}

/// A bound port, ready before the actor is released. The caller runs its driver
/// on an owned thread and retains `Control` for stop/revocation.
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

/// Each side's transport, with no idle timeout and no keep-alive of its own (D180's liveness
/// at use): a quiet channel is no idle connection, as its owner awaits its output however long
/// a harness says nothing, and its driver judges each send by what its peer acknowledges
/// (`net::Liveness`), a peer gone found so as the next record waits a whole window unanswered
/// past its probe timeout.
fn transport(admitted: Admitted, receive_bytes: usize) -> Result<TransportConfig, Error> {
    transport_of(
        &admitted,
        Windows {
            stream: receive_bytes,
            connection: receive_bytes,
            send: admitted.transport_bytes,
            streams: 1,
        },
    )
}

/// What a connection holds of its streams: the most its peer sends on one stream and on all
/// of them before this side reads, the most this side sends unacknowledged, and how many
/// streams its peer may have open at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Windows {
    pub(crate) stream: usize,
    pub(crate) connection: usize,
    pub(crate) send: u64,
    pub(crate) streams: u32,
}

/// The transport of a connection that carries `windows`, `admitted`'s handshake and no
/// datagrams, as [`transport`] has it for one stream.
pub(crate) fn transport_of(admitted: &Admitted, windows: Windows) -> Result<TransportConfig, Error> {
    let window = |bytes: usize| {
        VarInt::from_u64(u64::try_from(bytes).map_err(|_| Error::Admission(AdmissionError::Window))?)
            .map_err(|_| Error::Admission(AdmissionError::Window))
    };
    let mut transport = TransportConfig::default();
    transport
        .stream_receive_window(window(windows.stream)?)
        .receive_window(window(windows.connection)?)
        .send_window(windows.send)
        .max_concurrent_bidi_streams(VarInt::from_u32(windows.streams))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .crypto_buffer_size(admitted.handshake_bytes)
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0)
        .max_idle_timeout(None)
        .keep_alive_interval(None);
    Ok(transport)
}

/// Uses the same pinned-key TLS verification and explicit transport allowances as
/// the endpoint, with the managed-output ALPN rather than remote-command ALPN.
pub fn client_config(
    identity: &Identity,
    server: Fingerprint,
    admitted: Admitted,
) -> Result<ClientConfig, Error> {
    let receive = admitted
        .record_bytes
        .checked_add(DATA_HEAD)
        .ok_or(Error::Admission(AdmissionError::Window))?;
    tls::client_with(identity, server, transport(admitted, receive)?, ALPN).map_err(Error::Transport)
}

impl Endpoint {
    pub fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        self.driver.kernel_buffer_bytes().map_err(Error::Transport)
    }
    pub fn bind(
        identity: &Identity,
        owner: Fingerprint,
        scope: Scope,
        admitted: Admitted,
    ) -> Result<(Self, Producer, Control), Error> {
        admissible(&admitted)?;
        let transport = transport(admitted, CONTROL_BYTES)?;
        let mut config = tls::owner_server(identity, owner, transport, ALPN)?;
        config
            .max_incoming(1)
            .incoming_buffer_size(0)
            .incoming_buffer_size_total(0);
        let handler = Service::new(scope, owner, admitted.record_bytes)?;
        let (mut driver, sender) = Driver::bind_bounded(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Some(config),
            handler,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: admitted.datagram_bytes,
            },
        )?;
        let address = driver.local()?;
        if matches!(address.port(), 80 | 443) {
            return Err(Error::Admission(AdmissionError::ExcludedPort));
        }
        let (producer, control) = driver.handler_mut().handles(sender, Stop::Driver)?;
        Ok((
            Self {
                driver,
                address,
                fingerprint: identity.fingerprint(),
            },
            producer,
            control,
        ))
    }

    pub fn run(mut self) -> Result<(), Error> {
        let result = self.driver.run();
        self.driver.handler_mut().close();
        result.map_err(Error::Transport)
    }
}

/// Whether one stream's admission can hold its protocol: a record of one byte or more and no
/// more than a frame carries, and a window and a handshake to send.
pub(crate) fn admissible(admitted: &Admitted) -> Result<(), Error> {
    if admitted.record_bytes == 0
        || admitted.record_bytes > RECORD_MOST
        || admitted.transport_bytes == 0
        || admitted.handshake_bytes == 0
    {
        return Err(Error::Admission(AdmissionError::Capacity));
    }
    Ok(())
}

impl Service {
    /// The output of `scope`, its one record `record_bytes` at most, subscribed to by `owner`'s
    /// streams alone.
    pub(crate) fn new(scope: Scope, owner: Fingerprint, record_bytes: usize) -> Result<Service, Error> {
        let bytes = record_buffer(record_bytes)?;
        let capacity = bytes.capacity();
        Ok(Service {
            scope,
            owner,
            shared: Arc::new(Shared {
                slot: Mutex::new((Slot::Waiting { at: 0, bytes }, Ending::Open)),
                changed: Condvar::new(),
                record_bytes: AtomicUsize::new(capacity),
                waker: Mutex::new(None),
            }),
            state: Subscription::Listening,
            control: [0; CONTROL_BYTES],
            control_len: 0,
        })
    }

    /// Its producer and its control, waking the driver that serves it through `sender`.
    pub(crate) fn handles(&self, sender: Sender<()>, stop: Stop) -> Result<(Producer, Control), Error> {
        let producer = Producer {
            shared: Arc::clone(&self.shared),
            sender: sender.try_clone()?,
        };
        let control = Control {
            shared: Arc::clone(&self.shared),
            sender,
            stop,
        };
        Ok((producer, control))
    }

    /// Its driver has ended: an output not finished is aborted, its producer and control told.
    pub(crate) fn close(&mut self) {
        self.state = Subscription::Closed;
        let mut state = lock(&self.shared);
        if !matches!(state.0, Slot::Finished(_)) {
            let at = progress(&state.0);
            state.0 = Slot::Aborted {
                accepted: at.accepted,
                consumed: at.consumed,
            };
        }
        drop(state);
        self.shared.notify();
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    /// The stream that subscribes to it now.
    pub(crate) fn subscriber(&self) -> Option<(ConnKey, StreamId)> {
        match self.state {
            Subscription::Subscribed { conn, stream } => Some((conn, stream)),
            _ => None,
        }
    }

    /// Its output was revoked: no stream subscribes to it again.
    pub(crate) fn aborted(&self) -> bool {
        matches!(lock(&self.shared).0, Slot::Aborted { .. })
    }

    /// Its output finished, its last record acknowledged: its subscriber's stream is finished
    /// at its next flush.
    pub(crate) fn finished(&self) -> bool {
        matches!(lock(&self.shared).0, Slot::Finished(_))
    }

    /// A subscription read by the endpoint its stream came to, which admitted the connection
    /// (the agent-control endpoint, D238): `(conn, stream)` serves this output from `offset`
    /// on, as an endpoint of one stream subscribes its owner ([`Service::controls`]).
    pub(crate) fn lane(
        &mut self,
        conn: ConnKey,
        stream: StreamId,
        scope: Scope,
        offset: u64,
    ) -> Result<(), Reason> {
        if !matches!(self.state, Subscription::Listening) {
            return Err(Reason::Scope);
        }
        self.check_scope(scope)?;
        if self.aborted() {
            return Err(Reason::Closed);
        }
        if !self.subscribe(offset) {
            return Err(Reason::Protocol);
        }
        self.state = Subscription::Subscribed { conn, stream };
        Ok(())
    }

    fn refuse(&mut self, io: &mut Io<'_>, conn: ConnKey) {
        self.refuse_reason(io, conn, Reason::Protocol);
    }

    fn refuse_reason(&mut self, io: &mut Io<'_>, conn: ConnKey, reason: Reason) {
        io.close(conn, reason as u32, "managed output subscription refused");
        self.disconnect();
    }

    fn check_scope(&self, offered: Scope) -> Result<(), Reason> {
        if offered.run != self.scope.run
            || offered.member != self.scope.member
            || offered.channel != self.scope.channel
        {
            return Err(Reason::Scope);
        }
        if offered.generation != self.scope.generation {
            return Err(Reason::Stale);
        }
        Ok(())
    }

    /// Its subscriber's stream is gone: the record it was sent is kept whole for the next
    /// subscription to the same cursor.
    pub(crate) fn disconnect(&mut self) {
        self.state = Subscription::Listening;
        self.control_len = 0;
        let mut state = lock(&self.shared);
        match &mut state.0 {
            Slot::Ready { at, bytes } => {
                let (at, bytes) = (*at, std::mem::take(bytes));
                state.0 = Slot::Waiting { at, bytes };
            }
            Slot::Record { written, .. } => *written = 0,
            Slot::AwaitingAck { .. } => {
                if let Slot::AwaitingAck { start, bytes } = std::mem::replace(
                    &mut state.0,
                    Slot::Aborted {
                        accepted: 0,
                        consumed: 0,
                    },
                ) {
                    state.0 = Slot::Record {
                        start,
                        bytes,
                        written: 0,
                    };
                }
            }
            _ => {}
        }
        drop(state);
        self.shared.notify();
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        match self.read(io, conn, stream) {
            Ok(()) => self.flush(io),
            Err(gone) => self.refuse_reason(io, conn, gone.reason()),
        }
    }

    /// Reads what its subscriber sent: its subscription, then its acknowledgments, as far as
    /// they have come.
    pub(crate) fn read(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) -> Result<(), Gone> {
        loop {
            let room = CONTROL_BYTES.saturating_sub(self.control_len);
            let (bytes, ended) = match io.read_within(conn, stream, room) {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => return Ok(()),
                Ok(Read::Reset) => return Err(Gone::Reset),
                Err(_) => return Err(Gone::Refused(Reason::Protocol)),
            };
            let target = self
                .control
                .get_mut(self.control_len..self.control_len.saturating_add(bytes.len()))
                .ok_or(Gone::Refused(Reason::Protocol))?;
            target.copy_from_slice(&bytes);
            self.control_len = self.control_len.saturating_add(bytes.len());
            self.controls(io, conn, stream).map_err(Gone::Refused)?;
            if ended {
                return Err(Gone::Ended);
            }
        }
    }

    fn controls(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) -> Result<(), Reason> {
        loop {
            let bytes = self.control.get(..self.control_len).unwrap_or_default();
            let (message, end) = match input(bytes) {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(()),
                Err(_) => return Err(Reason::Protocol),
            };
            match (self.state, message) {
                (Subscription::Connecting(c), Input::Subscribe(scope, offset)) if c == conn => {
                    self.check_scope(scope)?;
                    if !self.subscribe(offset) {
                        return Err(Reason::Protocol);
                    }
                    self.state = Subscription::Subscribed { conn, stream };
                }
                (Subscription::Subscribed { conn: c, stream: s }, Input::Consumed(offset))
                    if c == conn && s == stream =>
                {
                    if !self.consumed(offset) {
                        return Err(Reason::Protocol);
                    }
                }
                _ => return Err(Reason::Protocol),
            }
            self.control.copy_within(end..self.control_len, 0);
            self.control_len -= end;
            self.flush(io);
            if self.control_len == 0 {
                return Ok(());
            }
        }
    }

    fn subscribe(&self, offset: u64) -> bool {
        let mut state = lock(&self.shared);
        match &mut state.0 {
            Slot::Waiting { at, bytes } if *at == offset => {
                let bytes = std::mem::take(bytes);
                state.0 = Slot::Ready { at: offset, bytes };
            }
            Slot::Capturing(at) if *at == offset => {}
            Slot::Record { start, .. } | Slot::AwaitingAck { start, .. } if *start == offset => {}
            Slot::Finished(at) if *at == offset => {}
            _ => return false,
        }
        drop(state);
        self.shared.notify();
        true
    }

    fn consumed(&self, offset: u64) -> bool {
        let mut state = lock(&self.shared);
        match &state.0 {
            Slot::AwaitingAck { start, bytes } if start.checked_add(bytes.len() as u64) == Some(offset) => {
                let old = std::mem::replace(&mut state.0, Slot::Finished(offset));
                if !matches!(state.1, Ending::Finishing)
                    && let Slot::AwaitingAck { mut bytes, .. } = old
                {
                    bytes.clear();
                    state.0 = Slot::Ready { at: offset, bytes };
                }
                drop(state);
                self.shared.notify();
                true
            }
            Slot::Ready { at, .. } if *at == offset => true,
            _ => false,
        }
    }

    pub(crate) fn flush(&mut self, io: &mut Io<'_>) {
        let Subscription::Subscribed { conn, stream } = self.state else {
            return;
        };
        let mut state = lock(&self.shared);
        let Slot::Record {
            start,
            bytes,
            written,
        } = &mut state.0
        else {
            if matches!(state.0, Slot::Finished(_)) {
                let _ = io.finish(conn, stream);
            }
            return;
        };
        let Some(len) = bytes
            .len()
            .checked_add(OFFSET_BYTES)
            .and_then(|n| u32::try_from(n).ok())
        else {
            return;
        };
        let mut head = [0u8; DATA_HEAD];
        let Some((tag, rest)) = head.split_first_mut() else {
            return;
        };
        *tag = Tag::Stdout as u8;
        if let Some((size, at)) = rest.split_first_chunk_mut::<{ size_of::<u32>() }>() {
            size.copy_from_slice(&len.to_be_bytes());
            at.copy_from_slice(&start.to_be_bytes());
        }
        let total = DATA_HEAD.saturating_add(bytes.len());
        while *written < total {
            let part = if *written < DATA_HEAD {
                head.get(*written..).unwrap_or_default()
            } else {
                bytes.get(written.saturating_sub(DATA_HEAD)..).unwrap_or_default()
            };
            match io.write(conn, stream, part) {
                Ok(0) | Err(_) => return,
                Ok(n) => *written = written.saturating_add(n).min(total),
            }
        }
        if let Slot::Record { start, bytes, .. } = std::mem::replace(
            &mut state.0,
            Slot::Aborted {
                accepted: 0,
                consumed: 0,
            },
        ) {
            state.0 = Slot::AwaitingAck { start, bytes };
        }
    }
}

impl Handler for Service {
    type Command = ();
    fn connected(&mut self, _io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        if crate::identity::fingerprint(peer) != self.owner || !matches!(self.state, Subscription::Listening)
        {
            return Err(Close {
                code: if crate::identity::fingerprint(peer) != self.owner {
                    Reason::Auth as u32
                } else {
                    Reason::Scope as u32
                },
                why: "managed output owner refused",
            });
        }
        self.state = Subscription::Connecting(conn);
        Ok(())
    }
    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if !matches!(self.state, Subscription::Connecting(c) if c == conn) {
            self.refuse(io, conn);
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
        self.refuse(io, conn);
    }
    fn lost(&mut self, _io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        if matches!(self.state, Subscription::Connecting(c) if c == conn)
            || matches!(self.state, Subscription::Subscribed { conn: c, .. } if c == conn)
        {
            self.disconnect();
        }
    }
    fn command(&mut self, io: &mut Io<'_>, (): ()) {
        self.flush(io);
    }
    fn done(&self) -> bool {
        matches!(self.state, Subscription::Closed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// How long a test waits for what it awaits: a patience of its own.
    const PATIENCE: Duration = Duration::from_secs(10);

    fn waiting(at: u64) -> Slot {
        Slot::Waiting {
            at,
            bytes: record_buffer(16).unwrap(),
        }
    }

    fn ready(at: u64) -> Slot {
        Slot::Ready {
            at,
            bytes: record_buffer(16).unwrap(),
        }
    }

    fn admitted() -> Admitted {
        Admitted {
            record_bytes: CONTROL_BYTES,
            transport_bytes: u64::try_from(CONTROL_BYTES + DATA_HEAD).unwrap(),
            // RFC 8446: uint24 handshake length plus its four-byte header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: DATAGRAM_BYTES,
        }
    }

    fn scope() -> Scope {
        Scope {
            run: RunId([7; 32]),
            generation: 9,
            member: 0,
            channel: Channel::Stdout,
        }
    }

    fn native_endpoint() -> (Endpoint, Producer, Control) {
        let identity = Identity::generate().unwrap();
        // Finite setup admission only; the driver is never run by these tests.
        Endpoint::bind(&identity, identity.fingerprint(), scope(), admitted()).unwrap()
    }

    #[test]
    fn prepared_capture_storage_is_transferred_and_recycled_after_ack() {
        let (mut endpoint, producer, control) = native_endpoint();
        let service = endpoint.driver.handler_mut();
        let (pointer, capacity) = match &lock(&service.shared).0 {
            Slot::Waiting { bytes, .. } => (bytes.as_ptr(), bytes.capacity()),
            other => panic!("{other:?}"),
        };
        assert!(capacity >= CONTROL_BYTES);
        assert_eq!(producer.storage_capacity(), capacity);
        assert!(producer.take_buffer().unwrap().is_none());
        assert!(service.subscribe(0));
        let mut bytes = producer.take_buffer().unwrap().unwrap();
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(bytes.capacity(), capacity);
        assert_eq!(
            control.state(),
            State::Capturing(Progress {
                accepted: 0,
                consumed: 0
            })
        );
        assert!(producer.take_buffer().unwrap().is_none());
        assert_eq!(producer.capacity(), 0);
        bytes.extend_from_slice(b"a\0\xff");
        producer.submit(bytes).unwrap();
        let mut state = lock(&service.shared);
        let old = std::mem::replace(&mut state.0, Slot::Finished(0));
        let Slot::Record { start, bytes, .. } = old else {
            panic!("record not submitted")
        };
        assert_eq!(bytes.as_ptr(), pointer);
        state.0 = Slot::AwaitingAck { start, bytes };
        drop(state);
        assert!(service.consumed(3));
        let recycled = producer.take_buffer().unwrap().unwrap();
        assert_eq!(recycled.as_ptr(), pointer);
        assert_eq!(recycled.capacity(), capacity);
        assert!(recycled.is_empty());
        // A capture borrower cancelling instead of submitting must terminalize
        // its owner before dropping the detached allocation.
        control.abort();
        drop(recycled);
        assert!(matches!(control.state(), State::Aborted(_)));
        assert!(producer.take_buffer().unwrap().is_none());
    }

    #[test]
    fn capture_reconnect_preserves_the_borrowed_record_and_cursor() {
        let (mut endpoint, producer, control) = native_endpoint();
        let service = endpoint.driver.handler_mut();
        assert!(service.subscribe(0));
        let mut bytes = producer.take_buffer().unwrap().unwrap();
        let pointer = bytes.as_ptr();
        service.disconnect();
        assert!(!service.subscribe(1));
        assert!(service.subscribe(0));
        assert!(producer.take_buffer().unwrap().is_none());
        bytes.extend_from_slice(b"payload");
        producer.submit(bytes).unwrap();
        match &lock(&service.shared).0 {
            Slot::Record { start: 0, bytes, .. } => assert_eq!(bytes.as_ptr(), pointer),
            other => panic!("{other:?}"),
        }
        control.abort();
    }

    #[test]
    fn growth_prepares_real_storage_and_refusal_preserves_the_previous_window() {
        let (mut endpoint, producer, control) = native_endpoint();
        let before = producer.storage_capacity();
        let grown = producer.admit(before.checked_mul(2).unwrap()).unwrap();
        assert!(grown > before);
        assert_eq!(producer.storage_capacity(), grown);
        assert!(matches!(
            producer.admit(usize::MAX),
            Err(Error::Admission(AdmissionError::Capacity))
        ));
        assert_eq!(producer.storage_capacity(), grown);
        assert!(endpoint.driver.handler_mut().subscribe(0));
        let bytes = producer.take_buffer().unwrap().unwrap();
        assert_eq!(bytes.capacity(), grown);
        assert!(matches!(
            producer.admit(grown.checked_mul(2).unwrap()),
            Err(Error::Admission(AdmissionError::Capacity))
        ));
        assert_eq!(producer.storage_capacity(), grown);
        control.abort();
        assert!(matches!(producer.submit(bytes), Err(Rejected::Invalid(_))));
    }

    #[test]
    fn finish_coalesces_the_queued_record_wake_without_discarding_the_record() {
        let (mut endpoint, producer, control) = native_endpoint();
        assert!(endpoint.driver.handler_mut().subscribe(0));
        let bytes = vec![0, u8::MAX];
        let pointer = bytes.as_ptr();
        producer.submit(bytes).unwrap();
        assert!(matches!(
            control.sender.try_send(Command::Handler(())),
            Err(std::sync::mpsc::TrySendError::Full(Command::Handler(())))
        ));
        control.finish().unwrap();
        let state = lock(&control.shared);
        assert!(matches!(state.1, Ending::Finishing));
        match &state.0 {
            Slot::Record {
                start: 0,
                bytes,
                written: 0,
            } => {
                assert_eq!(bytes, &[0, u8::MAX]);
                assert_eq!(bytes.as_ptr(), pointer);
            }
            other => panic!("finish must retain the pending record: {other:?}"),
        }
        assert_eq!(
            progress(&state.0),
            Progress {
                accepted: 2,
                consumed: 0
            }
        );
    }

    /// A record submitted while a wake is queued already keeps its place: that wake's turn
    /// flushes it, as the handler flushes whatever its streams hold, so a full handoff is no
    /// refusal of the record (the agent-control endpoint's streams share one, D238).
    #[test]
    fn a_record_submitted_beside_a_queued_wake_is_kept_for_its_turn() {
        let (mut endpoint, producer, control) = native_endpoint();
        assert!(endpoint.driver.handler_mut().subscribe(0));
        control.sender.try_send(Command::Handler(())).unwrap();
        producer.submit(vec![1, 2, 3]).unwrap();
        assert!(matches!(
            lock(&control.shared).0,
            Slot::Record {
                start: 0,
                written: 0,
                ..
            }
        ));
        assert_eq!(
            control.progress(),
            Progress {
                accepted: 3,
                consumed: 0
            }
        );
    }

    #[test]
    fn finish_coalesces_a_queued_wake_for_an_empty_stream() {
        let (_endpoint, _producer, control) = native_endpoint();
        control.sender.try_send(Command::Handler(())).unwrap();
        control.finish().unwrap();
        control.finish().unwrap();
        let state = lock(&control.shared);
        assert!(matches!(state.1, Ending::Finishing));
        assert!(matches!(state.0, Slot::Finished(0)));
    }

    #[test]
    fn finish_after_the_native_driver_drops_keeps_the_disconnect_error() {
        let (endpoint, _producer, control) = native_endpoint();
        drop(endpoint);
        assert!(matches!(
            control.sender.try_send(Command::Handler(())),
            Err(std::sync::mpsc::TrySendError::Disconnected(Command::Handler(())))
        ));
        assert!(matches!(control.finish(), Err(Error::Transport(_))));
    }

    fn service(slot: Slot) -> Service {
        Service {
            scope: Scope {
                run: RunId([7; 32]),
                generation: 9,
                member: 0,
                channel: Channel::Stdout,
            },
            owner: [3; 32],
            shared: Arc::new(Shared {
                slot: Mutex::new((slot, Ending::Open)),
                changed: Condvar::new(),
                record_bytes: AtomicUsize::new(16),
                waker: Mutex::new(None),
            }),
            state: Subscription::Listening,
            control: [0; CONTROL_BYTES],
            control_len: 0,
        }
    }

    #[test]
    fn terminal_observations_do_not_infer_completion_from_equal_cursors() {
        let at = Progress {
            accepted: 8,
            consumed: 8,
        };
        for (slot, expected) in [
            (waiting(8), State::Pending(at)),
            (ready(8), State::Ready(at)),
            (Slot::Capturing(8), State::Capturing(at)),
            (Slot::Finished(8), State::Finished(at)),
            (
                Slot::Aborted {
                    accepted: 8,
                    consumed: 8,
                },
                State::Aborted(at),
            ),
        ] {
            let service = service(slot);
            assert_eq!(service.shared.state(), expected);
        }
    }

    #[test]
    fn terminal_ack_notification_exposes_finished_after_releasing_shared() {
        let service = service(Slot::AwaitingAck {
            start: 3,
            bytes: vec![1, 2],
        });
        service.shared.finish().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let weak = Arc::downgrade(&service.shared);
        let sink = Arc::clone(&observed);
        *service.shared.waker.lock().unwrap() = Some(Arc::new(move || {
            // try_lock distinguishes the promised post-unlock callback without hanging.
            let shared = weak.upgrade().unwrap();
            assert!(shared.slot.try_lock().is_ok());
            sink.lock().unwrap().push(shared.state());
        }));
        assert!(!service.consumed(4));
        assert!(observed.lock().unwrap().is_empty());
        assert!(service.consumed(5));
        assert_eq!(
            *observed.lock().unwrap(),
            [State::Finished(Progress {
                accepted: 5,
                consumed: 5
            })]
        );
        assert!(!service.consumed(5));
        assert_eq!(observed.lock().unwrap().len(), 1);
        service.shared.finish().unwrap();
        assert_eq!(
            service.shared.state(),
            State::Finished(Progress {
                accepted: 5,
                consumed: 5
            })
        );
    }

    #[test]
    fn terminal_finish_never_revives_an_aborted_slot_or_ending() {
        for (slot, ending) in [
            (
                Slot::Aborted {
                    accepted: 8,
                    consumed: 5,
                },
                Ending::Open,
            ),
            (
                Slot::Aborted {
                    accepted: 8,
                    consumed: 8,
                },
                Ending::Aborted,
            ),
            (ready(8), Ending::Aborted),
        ] {
            let service = service(slot);
            lock(&service.shared).1 = ending;
            let before = service.shared.state();
            assert!(matches!(service.shared.finish(), Err(Error::Closed)));
            assert_eq!(service.shared.state(), before);
        }
    }

    #[test]
    fn controls_preserve_scope_and_cursor_across_every_split() {
        let scope = Scope {
            run: RunId([42; 32]),
            generation: u64::MAX,
            member: u32::MAX,
            channel: Channel::Stderr,
        };
        let bytes = subscribe(scope, u64::MAX);
        for cut in 0..bytes.len() {
            assert_eq!(input(bytes.get(..cut).unwrap()).unwrap(), None);
        }
        assert_eq!(
            input(&bytes).unwrap(),
            Some((Input::Subscribe(scope, u64::MAX), CONTROL_BYTES))
        );
        let ack = consumed(123);
        assert_eq!(input(&ack).unwrap(), Some((Input::Consumed(123), ack.len())));
    }

    #[test]
    fn control_lengths_and_tags_are_refused_before_payload_allocation() {
        for tag in [Tag::Exec, Tag::Stdin, Tag::Stdout, Tag::Join] {
            let mut h = vec![tag as u8];
            h.extend_from_slice(&u32::MAX.to_be_bytes());
            assert!(input(&h).is_err());
        }
        let mut short = vec![Tag::Stdin as u8];
        short.extend_from_slice(&0u32.to_be_bytes());
        assert!(input(&short).is_err());
    }

    #[test]
    fn refusal_categories_preserve_scope_and_stale_generation() {
        let service = service(waiting(0));
        let scope = service.scope;
        assert_eq!(service.check_scope(scope), Ok(()));
        assert_eq!(
            service.check_scope(Scope {
                generation: scope.generation + 1,
                ..scope
            }),
            Err(Reason::Stale)
        );
        assert_eq!(
            service.check_scope(Scope {
                member: scope.member + 1,
                ..scope
            }),
            Err(Reason::Scope)
        );
        assert_eq!(
            service.check_scope(Scope {
                channel: Channel::Stderr,
                ..scope
            }),
            Err(Reason::Scope)
        );
        assert!(matches!(
            input(&[Tag::Stdin as u8, 0, 0, 0, 0]),
            Err(Error::Protocol(ProtocolError::Length))
        ));
    }

    #[test]
    fn only_a_contiguous_application_ack_returns_credit() {
        let service = service(Slot::AwaitingAck {
            start: 4,
            bytes: b"a\0\xffz".to_vec(),
        });
        assert!(!service.consumed(4));
        assert!(!service.consumed(7));
        assert!(!service.consumed(9));
        assert!(matches!(lock(&service.shared).0, Slot::AwaitingAck { .. }));
        assert!(service.consumed(8));
        assert!(matches!(lock(&service.shared).0, Slot::Ready { at: 8, .. }));
        assert!(service.consumed(8));
        assert!(!service.consumed(9));
    }

    #[test]
    fn a_record_cannot_be_acked_before_it_was_sent() {
        let service = service(Slot::Record {
            start: 4,
            bytes: vec![1, 2],
            written: 0,
        });
        assert!(!service.consumed(6));
        assert!(matches!(lock(&service.shared).0, Slot::Record { written: 0, .. }));
    }

    #[test]
    fn observer_loss_retains_only_the_unacknowledged_record_for_the_same_cursor() {
        let bytes = vec![0, 255, 13, 10];
        let pointer = bytes.as_ptr();
        let mut service = service(Slot::AwaitingAck { start: 12, bytes });
        service.disconnect();
        match &lock(&service.shared).0 {
            Slot::Record {
                start: 12,
                bytes,
                written: 0,
            } => assert_eq!(bytes.as_ptr(), pointer),
            other => panic!("{other:?}"),
        }
        assert!(!service.subscribe(0));
        assert!(!service.subscribe(16));
        assert!(service.subscribe(12));
    }

    #[test]
    fn finishing_releases_the_last_record_only_after_its_application_ack() {
        let service = service(Slot::AwaitingAck {
            start: 3,
            bytes: vec![1, 2],
        });
        lock(&service.shared).1 = Ending::Finishing;
        assert!(!service.consumed(4));
        assert!(service.consumed(5));
        assert!(matches!(lock(&service.shared).0, Slot::Finished(5)));
    }

    #[test]
    fn replay_and_forward_cursors_do_not_subscribe_to_a_new_stream() {
        let service = service(waiting(8));
        assert!(!service.subscribe(0));
        assert!(!service.subscribe(9));
        assert!(service.subscribe(8));
        assert!(!service.subscribe(8));
    }

    #[test]
    fn rejection_debug_never_prints_captured_content() {
        let secret = b"sensitive-output".to_vec();
        for rejected in [
            Rejected::Full(secret.clone()),
            Rejected::Closed(secret.clone()),
            Rejected::Invalid(secret),
        ] {
            assert!(!format!("{rejected:?}").contains("sensitive-output"));
        }
    }

    /// What an owner's stream brought it, or that its connection was lost.
    #[derive(Debug)]
    enum Heard {
        Bytes(Vec<u8>),
        Lost(String),
    }

    enum Ask {
        Connect(SocketAddr),
        /// Ends the owner's driver without a word to its endpoint, as a killed owner's ends.
        Vanish,
    }

    /// An owner as the daemon's are (crates/shards tests/common/managed_capture.rs), over the
    /// real transport on a driver of its own: it subscribes at cursor 0 once its handshake is
    /// done and tells what it hears.
    struct Owner {
        config: Option<ClientConfig>,
        pending: Vec<u8>,
        heard: mpsc::Sender<Heard>,
        vanished: bool,
    }

    impl Owner {
        fn flush(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
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
            self.pending.extend_from_slice(&subscribe(scope(), 0));
            self.flush(io, conn, stream);
            Ok(())
        }
        fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
            while let Read::Bytes(bytes) | Read::End(bytes) = io.read(conn, stream) {
                if bytes.is_empty() {
                    return;
                }
                let _ = self.heard.send(Heard::Bytes(bytes));
            }
        }
        fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
            self.flush(io, conn, stream);
        }
        fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, why: &str, _: Option<u64>) {
            let _ = self.heard.send(Heard::Lost(why.to_string()));
        }
        fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
            match ask {
                Ask::Connect(to) => {
                    if let Some(Err(e)) = self.config.take().map(|config| io.connect(to, config)) {
                        let _ = self.heard.send(Heard::Lost(e.to_string()));
                    }
                }
                Ask::Vanish => self.vanished = true,
            }
        }
        fn done(&self) -> bool {
            self.vanished
        }
    }

    /// A channel's endpoint and an owner subscribed to it over loopback, each driven on a
    /// thread of its own as the daemon's endpoints and their owners are, neither with an idle
    /// timeout or a keep-alive of its own. Both drivers end as it drops.
    struct Pair {
        producer: Producer,
        control: Control,
        /// Where its endpoint is, its key, and its owner's identity: a successor's dial.
        address: SocketAddr,
        server: crate::identity::Fingerprint,
        owner_identity: Identity,
        owner: Sender<Ask>,
        owner_driver: Option<std::thread::JoinHandle<Result<(), crate::Error>>>,
        endpoint_driver: Option<std::thread::JoinHandle<Result<(), Error>>>,
        heard: mpsc::Receiver<Heard>,
        changed: mpsc::Receiver<()>,
    }

    impl Pair {
        fn subscribed() -> Pair {
            let server = Identity::generate().unwrap();
            let owner_identity = Identity::generate().unwrap();
            let (bound, producer, control) =
                Endpoint::bind(&server, owner_identity.fingerprint(), scope(), admitted()).unwrap();
            let (changes, changed) = mpsc::channel();
            producer.on_credit(move || {
                let _ = changes.send(());
            });
            let (hears, heard) = mpsc::channel();
            let config = client_config(&owner_identity, server.fingerprint(), admitted()).unwrap();
            let (mut driver, asks) = Driver::bind_bounded(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                None,
                Owner {
                    config: Some(config),
                    pending: Vec::new(),
                    heard: hears,
                    vanished: false,
                },
                DriverLimits {
                    connections: 1,
                    command_slots: 1,
                    queued_datagram_bytes: DATAGRAM_BYTES,
                },
            )
            .unwrap();
            let address = bound.address;
            let server = server.fingerprint();
            let endpoint_driver = std::thread::Builder::new()
                .name("managed-endpoint".into())
                .spawn(move || bound.run())
                .unwrap();
            let owner_driver = std::thread::Builder::new()
                .name("managed-owner".into())
                .spawn(move || driver.run())
                .unwrap();
            asks.send(Command::Handler(Ask::Connect(address))).unwrap();
            let pair = Pair {
                producer,
                control,
                address,
                server,
                owner_identity,
                owner: asks,
                owner_driver: Some(owner_driver),
                endpoint_driver: Some(endpoint_driver),
                heard,
                changed,
            };
            let subscribed = pair.until(PATIENCE, |state| matches!(state, State::Ready(_)));
            assert!(
                subscribed.is_ok(),
                "never subscribed: {subscribed:?}; its owner heard {:?}",
                pair.heard()
            );
            pair
        }

        /// What its owner heard and the test has not taken.
        fn heard(&self) -> Vec<String> {
            self.heard
                .try_iter()
                .map(|heard| match heard {
                    Heard::Bytes(bytes) => format!("{} bytes", bytes.len()),
                    Heard::Lost(why) => format!("its connection lost: {why}"),
                })
                .collect()
        }

        /// How long the endpoint's slot took to be as `want` has it, or how it was at `within`.
        /// Each change of the slot wakes `on_credit`'s waker.
        fn until(&self, within: Duration, want: impl Fn(State) -> bool) -> Result<Duration, State> {
            let start = Instant::now();
            loop {
                let state = self.control.state();
                if want(state) {
                    return Ok(start.elapsed());
                }
                let left = within.checked_sub(start.elapsed()).ok_or(state)?;
                if self.changed.recv_timeout(left).is_err() {
                    let state = self.control.state();
                    return if want(state) {
                        Ok(start.elapsed())
                    } else {
                        Err(state)
                    };
                }
            }
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            self.owner.request_stop();
            self.control.abort();
            if let Some(driver) = self.owner_driver.take() {
                let _ = driver.join();
            }
            if let Some(driver) = self.endpoint_driver.take() {
                let _ = driver.join();
            }
        }
    }

    /// A subscription quiet for many of its windows (finding H1, docs/research/measurements/
    /// harness-bringup-v1: a channel a harness says nothing on is no idle connection), then
    /// given one record: neither side has an idle timeout or a keep-alive to hold it (D180's
    /// liveness at use), it lives through the silence, and carries the record whole.
    #[test]
    fn a_quiet_subscription_lives_with_no_idle_timeout_or_keep_alive() {
        let pair = Pair::subscribed();
        std::thread::sleep(PATIENCE);
        let state = pair.control.state();
        assert!(
            matches!(state, State::Ready(_)),
            "a subscription was let go in {PATIENCE:?} of silence: {state:?}; its owner heard {:?}",
            pair.heard()
        );
        let record = b"after-quiet\n";
        let mut bytes = pair.producer.take_buffer().unwrap().unwrap();
        bytes.extend_from_slice(record);
        pair.producer.submit(bytes).unwrap();
        let mut frame = vec![Tag::Stdout as u8];
        frame.extend_from_slice(&u32::try_from(OFFSET_BYTES + record.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&0u64.to_be_bytes());
        frame.extend_from_slice(record);
        let mut got = Vec::new();
        while got.len() < frame.len() {
            match pair.heard.recv_timeout(PATIENCE) {
                Ok(Heard::Bytes(bytes)) => got.extend_from_slice(&bytes),
                other => panic!("the record after the silence: {other:?}, after {got:?}"),
            }
        }
        assert_eq!(got, frame);
    }

    /// An owner that vanishes without a word keeps its place through its endpoint's next send,
    /// which stalls unanswered a whole window past its probe timeout and is told, not taken for
    /// the owner's death (D180, ROOT's ruling A): a successor that dials for the one place is
    /// refused, the holder asked for an answer (a PING), and once that goes unanswered past its
    /// window the successor's next attempt takes the place, the record that waited for the
    /// vanished owner its first (`net::Driver::reclaim`). Each attempt a driver of its own, as a
    /// restarted owner's, dialing again as managed/input.rs's successor does.
    #[test]
    fn a_vanished_owners_place_goes_to_its_successor_as_it_dials() {
        let mut pair = Pair::subscribed();
        pair.owner.send(Command::Handler(Ask::Vanish)).unwrap();
        pair.owner_driver.take().unwrap().join().unwrap().unwrap();
        let mut bytes = pair.producer.take_buffer().unwrap().unwrap();
        bytes.extend_from_slice(b"to nobody\n");
        pair.producer.submit(bytes).unwrap();
        let start = Instant::now();
        let mut refused = 0;
        let heard = loop {
            let (hears, heard) = mpsc::channel();
            let config = client_config(&pair.owner_identity, pair.server, admitted()).unwrap();
            let (mut driver, asks) = Driver::bind_bounded(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                None,
                Owner {
                    config: Some(config),
                    pending: Vec::new(),
                    heard: hears,
                    vanished: false,
                },
                DriverLimits {
                    connections: 1,
                    command_slots: 1,
                    queued_datagram_bytes: DATAGRAM_BYTES,
                },
            )
            .unwrap();
            let running = std::thread::spawn(move || driver.run());
            asks.send(Command::Handler(Ask::Connect(pair.address))).unwrap();
            let got = heard.recv_timeout(PATIENCE);
            asks.request_stop();
            running.join().unwrap().unwrap();
            match got {
                // Dialing again at managed/input.rs's successor's pace.
                Ok(Heard::Lost(_)) if start.elapsed() < PATIENCE => {
                    refused += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                other => break other,
            }
        };
        println!(
            "its successor subscribed after {refused} refusals, {:?}",
            start.elapsed()
        );
        assert!(refused > 0, "the vanished owner's place was free at once");
        match heard {
            Ok(Heard::Bytes(bytes)) => assert!(bytes.windows(10).any(|w| w == b"to nobody\n"), "{bytes:?}"),
            other => panic!("its successor never subscribed: {other:?}"),
        }
    }
}
