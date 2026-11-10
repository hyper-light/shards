//! Owner-authenticated, zero-retention managed output on the existing QUIC driver.

use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use hyper_quic::{ClientConfig, IdleTimeout, StreamId, TransportConfig, VarInt};
use shards_abi::run::managed::Channel;

use crate::frame::{self, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use crate::tls;

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
    Window,
    Patience,
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
    pub patience: std::time::Duration,
}

#[derive(Debug)]
enum Slot {
    Waiting(u64),
    Ready(u64),
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

fn progress(slot: &Slot) -> Progress {
    match slot {
        Slot::Waiting(at) | Slot::Ready(at) | Slot::Finished(at) => Progress {
            accepted: *at,
            consumed: *at,
        },
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

impl Producer {
    /// Raises retained-record allowance only after the caller extends its owned
    /// reservation. This does not enlarge QUIC's fixed in-flight send window.
    pub fn admit(&self, record_bytes: usize) -> Result<(), Error> {
        if record_bytes == 0 || record_bytes > frame::MAX_FRAME.saturating_sub(OFFSET_BYTES) {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
        self.shared.record_bytes.fetch_max(record_bytes, Ordering::AcqRel);
        Ok(())
    }

    pub fn capacity(&self) -> usize {
        let state = lock(&self.shared);
        if matches!(state.0, Slot::Ready(_)) && matches!(state.1, Ending::Open) {
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
                Slot::Ready(_) if matches!(state.1, Ending::Open) => {
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
            let Slot::Ready(start) = state.0 else {
                return Err(match state.0 {
                    Slot::Finished(_) | Slot::Aborted { .. } => Rejected::Closed(bytes),
                    _ => Rejected::Full(bytes),
                });
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
        if self.sender.send(Command::Handler(())).is_err() {
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

    /// Ends normally after the admitted record is application-acknowledged.
    pub fn finish(&self) -> Result<(), Error> {
        {
            let mut state = lock(&self.shared);
            state.1 = Ending::Finishing;
            if let Slot::Ready(at) | Slot::Waiting(at) = state.0 {
                state.0 = Slot::Finished(at);
            }
        }
        self.shared.notify();
        // Shared owns finishing; a queued unit wake will flush that state too.
        // Only this endpoint's producer/control enqueue these unit notifications.
        match self.sender.try_send(Command::Handler(())) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Error::Transport(crate::Error::new(
                "the connection's driver has ended",
            ))),
        }
    }

    /// Revocation uses the driver's separate stop flag, even with a full handoff.
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

fn transport(admitted: Admitted, receive_bytes: usize) -> Result<TransportConfig, Error> {
    let mut transport = TransportConfig::default();
    let receive =
        VarInt::from_u64(u64::try_from(receive_bytes).map_err(|_| Error::Admission(AdmissionError::Window))?)
            .map_err(|_| Error::Admission(AdmissionError::Window))?;
    transport
        .stream_receive_window(receive)
        .receive_window(receive)
        .send_window(admitted.transport_bytes)
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .crypto_buffer_size(admitted.handshake_bytes)
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0)
        .max_idle_timeout(Some(
            IdleTimeout::try_from(admitted.patience)
                .map_err(|_| Error::Admission(AdmissionError::Patience))?,
        ));
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
        if admitted.record_bytes == 0
            || admitted.record_bytes > frame::MAX_FRAME.saturating_sub(OFFSET_BYTES)
            || admitted.transport_bytes == 0
            || admitted.handshake_bytes == 0
            || admitted.patience.is_zero()
            || std::time::Instant::now().checked_add(admitted.patience).is_none()
        {
            return Err(Error::Admission(AdmissionError::Capacity));
        }
        let transport = transport(admitted, CONTROL_BYTES)?;
        let mut config = tls::owner_server(identity, owner, transport, ALPN)?;
        config
            .max_incoming(1)
            .incoming_buffer_size(0)
            .incoming_buffer_size_total(0);
        let shared = Arc::new(Shared {
            slot: Mutex::new((Slot::Waiting(0), Ending::Open)),
            changed: Condvar::new(),
            record_bytes: AtomicUsize::new(admitted.record_bytes),
            waker: Mutex::new(None),
        });
        let handler = Service {
            scope,
            owner,
            shared: Arc::clone(&shared),
            state: Subscription::Listening,
            control: [0; CONTROL_BYTES],
            control_len: 0,
        };
        let (driver, sender) = Driver::bind_bounded(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Some(config),
            handler,
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
        let producer = Producer {
            shared: Arc::clone(&shared),
            sender: sender.try_clone()?,
        };
        let control = Control { shared, sender };
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
        self.driver.handler_mut().state = Subscription::Closed;
        let shared = &self.driver.handler_mut().shared;
        let mut state = lock(shared);
        if !matches!(state.0, Slot::Finished(_)) {
            let at = progress(&state.0);
            state.0 = Slot::Aborted {
                accepted: at.accepted,
                consumed: at.consumed,
            };
        }
        drop(state);
        shared.notify();
        result.map_err(Error::Transport)
    }
}

impl Service {
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

    fn disconnect(&mut self) {
        self.state = Subscription::Listening;
        self.control_len = 0;
        let mut state = lock(&self.shared);
        match &mut state.0 {
            Slot::Ready(at) => state.0 = Slot::Waiting(*at),
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
        loop {
            let room = CONTROL_BYTES.saturating_sub(self.control_len);
            let read = io.read_within(conn, stream, room);
            let (bytes, ended) = match read {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => break,
                Ok(Read::Reset) | Err(_) => {
                    self.refuse(io, conn);
                    return;
                }
            };
            let Some(target) = self
                .control
                .get_mut(self.control_len..self.control_len.saturating_add(bytes.len()))
            else {
                self.refuse(io, conn);
                return;
            };
            target.copy_from_slice(&bytes);
            self.control_len = self.control_len.saturating_add(bytes.len());
            if let Err(reason) = self.controls(io, conn, stream) {
                self.refuse_reason(io, conn, reason);
                return;
            }
            if ended {
                self.refuse_reason(io, conn, Reason::Closed);
                return;
            }
        }
        self.flush(io);
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
        match &state.0 {
            Slot::Waiting(at) if *at == offset => state.0 = Slot::Ready(offset),
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
                state.0 = if matches!(state.1, Ending::Finishing) {
                    Slot::Finished(offset)
                } else {
                    Slot::Ready(offset)
                };
                drop(state);
                self.shared.notify();
                true
            }
            Slot::Ready(at) if *at == offset => true,
            _ => false,
        }
    }

    fn flush(&mut self, io: &mut Io<'_>) {
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
    use super::*;

    #[allow(clippy::indexing_slicing)]
    mod operator_boundaries {
        use super::*;

        #[test]
        fn ack_prefixes_and_coalesced_controls_preserve_exact_frame_ends() {
            let first = consumed(u64::MAX);
            for cut in 0..first.len() {
                assert_eq!(input(&first[..cut]).unwrap(), None);
            }
            let second = consumed(0);
            let joined = [first.as_slice(), second.as_slice()].concat();
            let (message, end) = input(&joined).unwrap().unwrap();
            assert_eq!(message, Input::Consumed(u64::MAX));
            assert_eq!(end, first.len());
            assert_eq!(
                input(&joined[end..]).unwrap(),
                Some((Input::Consumed(0), second.len()))
            );
        }

        #[test]
        fn malformed_ack_does_not_resynchronize_into_a_following_valid_control() {
            let valid = consumed(u64::MAX);
            for length in [0, OFFSET_BYTES - 1, OFFSET_BYTES + 1] {
                let mut wrong = consumed(0);
                wrong[1..FRAME_HEAD].copy_from_slice(&u32::try_from(length).unwrap().to_be_bytes());
                let joined = [wrong.as_slice(), valid.as_slice()].concat();
                assert!(matches!(
                    input(&joined),
                    Err(Error::Protocol(ProtocolError::Length))
                ));
            }
            for tag in [0, Tag::Stdout as u8] {
                let mut wrong = consumed(0);
                wrong[0] = tag;
                let joined = [wrong.as_slice(), valid.as_slice()].concat();
                assert!(matches!(
                    input(&joined),
                    Err(Error::Protocol(ProtocolError::ControlKind))
                ));
            }
            assert_eq!(
                input(&valid).unwrap(),
                Some((Input::Consumed(u64::MAX), valid.len()))
            );
        }

        #[test]
        fn invalid_subscription_channel_is_not_an_alternate_scope() {
            let scope = Scope {
                run: RunId([42; 32]),
                generation: u64::MAX,
                member: u32::MAX,
                channel: Channel::Stderr,
            };
            let valid = subscribe(scope, u64::MAX);
            let channel_at = FRAME_HEAD + size_of::<RunId>() + size_of::<u64>() + size_of::<u32>();
            for channel in [0, u8::MAX] {
                let mut wrong = valid;
                wrong[channel_at] = channel;
                assert!(matches!(
                    input(&wrong),
                    Err(Error::Protocol(ProtocolError::Channel))
                ));
            }
            assert_eq!(
                input(&valid).unwrap(),
                Some((Input::Subscribe(scope, u64::MAX), valid.len()))
            );
        }

        #[test]
        fn decoded_last_legal_ack_preserves_pending_bytes_until_the_exact_end() {
            let bytes = [0, u8::MAX];
            let start = u64::MAX - u64::try_from(bytes.len()).unwrap();
            for ending in [Ending::Open, Ending::Finishing] {
                let service = service(Slot::AwaitingAck {
                    start,
                    bytes: bytes.to_vec(),
                });
                lock(&service.shared).1 = ending;
                let pointer = match &lock(&service.shared).0 {
                    Slot::AwaitingAck { bytes, .. } => bytes.as_ptr(),
                    other => panic!("unexpected prepared slot {other:?}"),
                };
                for cursor in [0, start, u64::MAX - 1] {
                    let wire = consumed(cursor);
                    let (Input::Consumed(decoded), end) = input(&wire).unwrap().unwrap() else {
                        panic!("expected ACK wire variant");
                    };
                    assert_eq!(end, wire.len());
                    assert!(!service.consumed(decoded));
                    let state = lock(&service.shared);
                    assert_eq!(
                        progress(&state.0),
                        Progress {
                            accepted: u64::MAX,
                            consumed: start
                        }
                    );
                    match &state.0 {
                        Slot::AwaitingAck {
                            start: actual,
                            bytes: retained,
                        } => {
                            assert_eq!(*actual, start);
                            assert_eq!(retained, &bytes);
                            assert_eq!(retained.as_ptr(), pointer);
                        }
                        other => panic!("rejected ACK changed pending record {other:?}"),
                    }
                }
                let wire = consumed(u64::MAX);
                let (Input::Consumed(decoded), _) = input(&wire).unwrap().unwrap() else {
                    panic!("expected final ACK");
                };
                assert!(service.consumed(decoded));
                let state = lock(&service.shared);
                match ending {
                    Ending::Open => assert!(matches!(state.0, Slot::Ready(u64::MAX))),
                    Ending::Finishing => assert!(matches!(state.0, Slot::Finished(u64::MAX))),
                    Ending::Aborted => panic!("not a prepared case"),
                }
                assert_eq!(
                    progress(&state.0),
                    Progress {
                        accepted: u64::MAX,
                        consumed: u64::MAX
                    }
                );
            }
        }

        #[test]
        fn malformed_control_then_disconnect_requires_replay_before_ack() {
            let bytes = vec![0, u8::MAX];
            let pointer = bytes.as_ptr();
            let start = u64::MAX - u64::try_from(bytes.len()).unwrap();
            let mut service = service(Slot::AwaitingAck { start, bytes });
            let mut malformed = consumed(u64::MAX);
            malformed[0] = Tag::Stdout as u8;
            assert!(matches!(
                input(&malformed),
                Err(Error::Protocol(ProtocolError::ControlKind))
            ));
            // Service::refuse_reason performs this existing disconnect after its I/O close.
            // This pure composition deliberately does not model or claim that I/O close.
            service.disconnect();
            assert!(matches!(service.state, Subscription::Listening));
            match &lock(&service.shared).0 {
                Slot::Record {
                    start: actual,
                    bytes,
                    written: 0,
                } => {
                    assert_eq!(*actual, start);
                    assert_eq!(bytes.as_ptr(), pointer);
                }
                other => panic!("refusal cleanup must retain the unsent replay {other:?}"),
            }
            assert!(!service.subscribe(u64::MAX));
            assert!(service.subscribe(start));
            let (Input::Consumed(end), _) = input(&consumed(u64::MAX)).unwrap().unwrap() else {
                panic!("expected ACK");
            };
            assert!(!service.consumed(end), "a replay has not yet been sent");
            assert!(matches!(lock(&service.shared).0, Slot::Record { written: 0, .. }));
        }
    }

    fn native_endpoint() -> (Endpoint, Producer, Control) {
        let identity = Identity::generate().unwrap();
        Endpoint::bind(
            &identity,
            identity.fingerprint(),
            Scope {
                run: RunId([7; 32]),
                generation: 9,
                member: 0,
                channel: Channel::Stdout,
            },
            Admitted {
                record_bytes: CONTROL_BYTES,
                transport_bytes: u64::try_from(CONTROL_BYTES + DATA_HEAD).unwrap(),
                // RFC 8446: uint24 handshake length plus its four-byte header.
                handshake_bytes: (1 << 24) - 1 + 4,
                datagram_bytes: DATAGRAM_BYTES,
                // Finite setup admission only; the driver is never run by these tests.
                patience: std::time::Duration::from_secs(1),
            },
        )
        .unwrap()
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
        let service = service(Slot::Waiting(0));
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
        assert!(matches!(lock(&service.shared).0, Slot::Ready(8)));
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
        let service = service(Slot::Waiting(8));
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
}
