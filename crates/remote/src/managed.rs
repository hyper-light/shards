//! Owner-authenticated, zero-retention managed output on the existing QUIC driver.

use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use hyper_quic::{ClientConfig, IdleTimeout, StreamId, TransportConfig, VarInt};

use crate::frame::{self, Tag};
use crate::identity::{Fingerprint, Identity};
use crate::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use crate::{Error, tls};

pub const ALPN: &[u8] = b"shards-managed-output/1";

const FRAME_HEAD: usize = size_of::<u8>() + size_of::<u32>();
const OFFSET_BYTES: usize = size_of::<u64>();
const SCOPE_BYTES: usize = size_of::<Fingerprint>() + size_of::<u64>();
const SUBSCRIBE_BYTES: usize = SCOPE_BYTES + OFFSET_BYTES;
const CONTROL_BYTES: usize = FRAME_HEAD + SUBSCRIBE_BYTES;
const DATA_HEAD: usize = FRAME_HEAD + OFFSET_BYTES;

/// A run and its current management generation, minted by the trusted owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub run: Fingerprint,
    pub generation: u64,
}

/// The fixed subscription message; the cursor names the first unconsumed record byte.
pub fn subscribe(scope: Scope, offset: u64) -> [u8; CONTROL_BYTES] {
    let mut out = [0u8; CONTROL_BYTES];
    let mut at = 0usize;
    for part in [
        &[Tag::Exec as u8][..],
        &(SUBSCRIBE_BYTES as u32).to_be_bytes(),
        &scope.run,
        &scope.generation.to_be_bytes(),
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
    let Some((tag, len)) = frame::peek(bytes)? else {
        return Ok(None);
    };
    let expected = match tag {
        Tag::Exec => SUBSCRIBE_BYTES,
        Tag::Stdin => OFFSET_BYTES,
        _ => return Err(Error::new("not a managed output control")),
    };
    if len != expected {
        return Err(Error::new("invalid managed output control length"));
    }
    let end = FRAME_HEAD
        .checked_add(len)
        .ok_or_else(|| Error::new("control length overflow"))?;
    let Some(payload) = bytes.get(FRAME_HEAD..end) else {
        return Ok(None);
    };
    let value = if tag == Tag::Exec {
        let (run, rest) = payload
            .split_first_chunk::<{ size_of::<Fingerprint>() }>()
            .ok_or_else(|| Error::new("subscription has no run"))?;
        let (generation, rest) = rest
            .split_first_chunk::<OFFSET_BYTES>()
            .ok_or_else(|| Error::new("subscription has no generation"))?;
        let offset =
            <[u8; OFFSET_BYTES]>::try_from(rest).map_err(|_| Error::new("subscription has no cursor"))?;
        Input::Subscribe(
            Scope {
                run: *run,
                generation: u64::from_be_bytes(*generation),
            },
            u64::from_be_bytes(offset),
        )
    } else {
        let offset =
            <[u8; OFFSET_BYTES]>::try_from(payload).map_err(|_| Error::new("consumption has no cursor"))?;
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
    Aborted,
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
    record_bytes: usize,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
    pub fn capacity(&self) -> usize {
        let state = lock(&self.shared);
        if matches!(state.0, Slot::Ready(_)) && matches!(state.1, Ending::Open) {
            self.shared.record_bytes
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
                Slot::Ready(_) if matches!(state.1, Ending::Open) => return Ok(self.shared.record_bytes),
                Slot::Finished(_) | Slot::Aborted => return Err(Error::new("managed output is closed")),
                _ if !matches!(state.1, Ending::Open) => return Err(Error::new("managed output is ending")),
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
        if bytes.is_empty() || bytes.capacity() > self.shared.record_bytes {
            return Err(Rejected::Invalid(bytes));
        }
        {
            let mut state = lock(&self.shared);
            let Slot::Ready(start) = state.0 else {
                return Err(match state.0 {
                    Slot::Finished(_) | Slot::Aborted => Rejected::Closed(bytes),
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
            let old = std::mem::replace(&mut state.0, Slot::Aborted);
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
        self.sender.send(Command::Handler(()))
    }

    /// Revocation uses the driver's separate stop flag, even with a full handoff.
    pub fn abort(&self) {
        {
            let mut state = lock(&self.shared);
            state.0 = Slot::Aborted;
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
        VarInt::from_u64(u64::try_from(receive_bytes).map_err(|_| Error::new("receive window overflow"))?)
            .map_err(|_| Error::new("receive window does not fit QUIC"))?;
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
                .map_err(|_| Error::new("output patience does not fit QUIC"))?,
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
        .ok_or_else(|| Error::new("record window overflow"))?;
    tls::client_with(identity, server, transport(admitted, receive)?, ALPN)
}

impl Endpoint {
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
            return Err(Error::new("managed output lacks admitted protocol capacity"));
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
            record_bytes: admitted.record_bytes,
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
            return Err(Error::new("the OS selected an excluded managed output port"));
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
            state.0 = Slot::Aborted;
        }
        drop(state);
        shared.notify();
        result
    }
}

impl Service {
    fn refuse(&mut self, io: &mut Io<'_>, conn: ConnKey) {
        io.close(conn, 1, "managed output subscription refused");
        self.disconnect();
    }

    fn disconnect(&mut self) {
        self.state = Subscription::Listening;
        self.control_len = 0;
        let mut state = lock(&self.shared);
        match &mut state.0 {
            Slot::Ready(at) => state.0 = Slot::Waiting(*at),
            Slot::Record { written, .. } => *written = 0,
            Slot::AwaitingAck { .. } => {
                if let Slot::AwaitingAck { start, bytes } = std::mem::replace(&mut state.0, Slot::Aborted) {
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
            if !self.controls(io, conn, stream) || ended {
                self.refuse(io, conn);
                return;
            }
        }
        self.flush(io);
    }

    fn controls(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) -> bool {
        loop {
            let bytes = self.control.get(..self.control_len).unwrap_or_default();
            let (message, end) = match input(bytes) {
                Ok(Some(value)) => value,
                Ok(None) => return true,
                Err(_) => return false,
            };
            match (self.state, message) {
                (Subscription::Connecting(c), Input::Subscribe(scope, offset))
                    if c == conn && scope == self.scope =>
                {
                    if !self.subscribe(offset) {
                        return false;
                    }
                    self.state = Subscription::Subscribed { conn, stream };
                }
                (Subscription::Subscribed { conn: c, stream: s }, Input::Consumed(offset))
                    if c == conn && s == stream =>
                {
                    if !self.consumed(offset) {
                        return false;
                    }
                }
                _ => return false,
            }
            self.control.copy_within(end..self.control_len, 0);
            self.control_len -= end;
            self.flush(io);
            if self.control_len == 0 {
                return true;
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
        if let Slot::Record { start, bytes, .. } = std::mem::replace(&mut state.0, Slot::Aborted) {
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
                code: 1,
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

    fn service(slot: Slot) -> Service {
        Service {
            scope: Scope {
                run: [7; 32],
                generation: 9,
            },
            owner: [3; 32],
            shared: Arc::new(Shared {
                slot: Mutex::new((slot, Ending::Open)),
                changed: Condvar::new(),
                record_bytes: 16,
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
            run: [42; 32],
            generation: u64::MAX,
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
