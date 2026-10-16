//! A QUIC endpoint on a UDP socket of its own (D180), driven by one thread: datagrams in and
//! out, the connections' timers, and their streams, which a [`Handler`] reads and writes. Other
//! threads reach the driver with a [`Command`] on its channel and a datagram to its socket
//! from its waker, which wakes it from the socket's read: what std gives every target,
//! Windows' consoles and pipes among the descriptors no poll(2) of a socket reaches.

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use hyper_quic::{
    ClientConfig, ClientConfigHandle, Connection, ConnectionError, ConnectionHandle, DatagramEvent, Dir,
    Endpoint, EndpointConfig, Event, ServerConfig, StreamEvent, StreamId, VarInt,
};

use crate::Error;

/// The largest datagram taken off the socket: QUIC's are at most 65,527 bytes of UDP payload
/// (RFC 9000 §18.2, max_udp_payload_size), and an IPv6 jumbogram is no UDP datagram std reads.
const DATAGRAM: usize = 65_527;

/// A connection, by the number the driver gave it, which it gives no other: what a thread
/// sends for a connection that has ended reaches none that came after it.
pub type ConnKey = u64;

/// What another thread asks of the driver, which runs it on its own thread.
#[derive(Debug)]
pub enum Command<C> {
    /// The handler's own.
    Handler(C),
    /// End the driver once its connections have sent what they hold.
    Stop,
}

/// Where another thread sends the driver [`Command`]s, waking it.
pub struct Sender<C> {
    commands: mpsc::Sender<Command<C>>,
    waker: UdpSocket,
    to: SocketAddr,
}

impl<C> std::fmt::Debug for Sender<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl<C> Sender<C> {
    /// Sends `command` and wakes the driver; an error once the driver has ended.
    pub fn send(&self, command: Command<C>) -> Result<(), Error> {
        self.commands
            .send(command)
            .map_err(|_| Error::new("the connection's driver has ended"))?;
        // A lost wake is made up by the driver's next read, which ends by its next timer
        // at the latest.
        let _ = self.waker.send_to(&[0], self.to);
        Ok(())
    }
}

impl<C> Sender<C> {
    /// Another sender to the same driver, for another thread.
    pub fn try_clone(&self) -> Result<Sender<C>, Error> {
        Ok(Sender {
            commands: self.commands.clone(),
            waker: self
                .waker
                .try_clone()
                .map_err(|e| Error::new(format!("the driver's waker: {e}")))?,
            to: self.to,
        })
    }
}

/// How this side closes a connection: an application's error code (RFC 9000 §20.2), and the
/// words its peer is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Close {
    pub code: u32,
    pub why: &'static str,
}

/// What the driver tells its handler of, and the means to act on it.
pub trait Handler {
    type Command;
    /// A connection's handshake is done; `peer` is the key it presented. It is kept, or closed
    /// as the handler says.
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> Result<(), Close>;
    /// The peer opened `stream`. QUIC tells of a stream it opened before anything else of it.
    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId);
    /// `stream` has bytes to read, or ended.
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId);
    /// `stream` may take bytes again.
    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId);
    /// The peer reads no more of `stream` (STOP_SENDING), whose side here is reset already,
    /// as RFC 9000 §3.5 asks.
    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId);
    /// A connection ended: closed, timed out, or refused at its handshake; `code`, the error
    /// code its peer closed it with, where its peer did (RFC 9000 §20.2). One this side closed
    /// is not told of: its handler knows.
    fn lost(&mut self, io: &mut Io<'_>, conn: ConnKey, why: &str, code: Option<u64>);
    /// Another thread's command.
    fn command(&mut self, io: &mut Io<'_>, command: Self::Command);
    /// Whether the driver may end, once it has sent what it holds: a client's, once its
    /// command has ended and it has closed its connection, whose peer is then told so (a
    /// closing endpoint need only have sent its CONNECTION_CLOSE, RFC 9000 §10.2).
    fn done(&self) -> bool;
    /// When the handler is next to act of its own accord: never, unless it keeps time.
    fn due(&self) -> Option<Instant> {
        None
    }
    /// That time has come ([`Handler::due`]).
    fn timeout(&mut self, _io: &mut Io<'_>, _now: Instant) {}
}

/// The driver's connections, as its handler acts on them.
pub struct Io<'a> {
    endpoint: &'a mut Endpoint,
    conns: &'a mut HashMap<ConnKey, Conn>,
    next: &'a mut ConnKey,
    client: Option<ClientConfigHandle>,
    now: Instant,
}

impl std::fmt::Debug for Io<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Io")
            .field("connections", &self.conns.len())
            .finish_non_exhaustive()
    }
}

struct Conn {
    handle: ConnectionHandle,
    quic: Connection,
    connected: bool,
}

/// What reading a stream gave.
#[derive(Debug)]
pub enum Read {
    /// These bytes, more maybe to come.
    Bytes(Vec<u8>),
    /// Nothing now.
    Blocked,
    /// The peer finished the stream, these bytes its last (none, maybe): all of it is read.
    /// The end comes with the bytes it follows, as QUIC lets a stream go once it has given
    /// its end: a read after it finds no stream.
    End(Vec<u8>),
    /// The peer reset the stream, or the connection is gone.
    Reset,
}

impl Io<'_> {
    fn conn(&mut self, key: ConnKey) -> Result<&mut Conn, Error> {
        self.conns
            .get_mut(&key)
            .ok_or_else(|| Error::new("no such connection"))
    }

    /// Connects to `to` with the client configuration the driver was made with.
    pub fn connect(&mut self, to: SocketAddr, config: ClientConfig) -> Result<ConnKey, Error> {
        let handle = match self.client {
            Some(handle) => handle,
            None => {
                let handle = self
                    .endpoint
                    .insert_client_config(config)
                    .map_err(|_| Error::new("no room for a client configuration"))?;
                self.client = Some(handle);
                handle
            }
        };
        let (handle, quic) = self
            .endpoint
            .connect(self.now, handle, to, crate::tls::SERVER_NAME, None)
            .map_err(|e| Error::new(format!("connecting to {to}: {e}")))?;
        Ok(place(
            self.conns,
            self.next,
            Conn {
                handle,
                quic,
                connected: false,
            },
        ))
    }

    /// Opens a bidirectional stream on `conn`.
    pub fn open(&mut self, conn: ConnKey) -> Result<StreamId, Error> {
        self.conn(conn)?
            .quic
            .streams()
            .open(Dir::Bi)
            .ok_or_else(|| Error::new("the daemon takes no more commands at once"))
    }

    /// Writes as much of `bytes` to `stream` as it takes now; how much it took.
    pub fn write(&mut self, conn: ConnKey, stream: StreamId, bytes: &[u8]) -> Result<usize, Error> {
        match self.conn(conn)?.quic.send_stream(stream).write(bytes) {
            Ok(n) => Ok(n),
            Err(hyper_quic::WriteError::Blocked) => Ok(0),
            Err(e) => Err(Error::new(format!("the connection's stream: {e}"))),
        }
    }

    /// Ends this side of `stream`, once what was written to it is sent.
    pub fn finish(&mut self, conn: ConnKey, stream: StreamId) -> Result<(), Error> {
        self.conn(conn)?
            .quic
            .send_stream(stream)
            .finish()
            .map_err(|e| Error::new(format!("the connection's stream: {e}")))
    }

    /// What `stream` has to read now.
    pub fn read(&mut self, conn: ConnKey, stream: StreamId) -> Read {
        let Ok(conn) = self.conn(conn) else {
            return Read::Reset;
        };
        let mut recv = conn.quic.recv_stream(stream);
        let mut chunks = match recv.read(true) {
            Ok(chunks) => chunks,
            Err(_) => return Read::Reset,
        };
        let mut got = Vec::new();
        let mut ended = false;
        loop {
            match chunks.next(usize::MAX) {
                Ok(Some(chunk)) => got.extend_from_slice(&chunk.bytes),
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(hyper_quic::ReadError::Blocked) => break,
                Err(hyper_quic::ReadError::Reset(_)) => {
                    let _ = chunks.finalize();
                    return Read::Reset;
                }
            }
        }
        let _ = chunks.finalize();
        match (got.is_empty(), ended) {
            (_, true) => Read::End(got),
            (false, false) => Read::Bytes(got),
            (true, false) => Read::Blocked,
        }
    }

    /// Reads no more of `stream`: its peer is asked to stop sending it (STOP_SENDING), and
    /// what still comes of it is let go. Nothing, for a stream the peer finished or reset.
    pub fn stop(&mut self, conn: ConnKey, stream: StreamId) {
        if let Ok(conn) = self.conn(conn) {
            let _ = conn.quic.recv_stream(stream).stop(VarInt::from_u32(0));
        }
    }

    /// Abandons this side of `stream` (RESET_STREAM): what was written to it and not yet
    /// sent is let go. Nothing, for a side reset already.
    pub fn reset(&mut self, conn: ConnKey, stream: StreamId) {
        if let Ok(conn) = self.conn(conn) {
            let _ = conn.quic.send_stream(stream).reset(VarInt::from_u32(0));
        }
    }

    /// Closes `conn`, saying `why` to its peer.
    pub fn close(&mut self, conn: ConnKey, code: u32, why: &str) {
        let now = self.now;
        if let Ok(conn) = self.conn(conn) {
            conn.quic.close(
                now,
                VarInt::from_u32(code),
                bytes::Bytes::copy_from_slice(why.as_bytes()),
            );
        }
    }

    /// The TLS exporter's 32 bytes for `label` on `conn` (RFC 8446 §7.5).
    pub fn export(&mut self, conn: ConnKey, label: &[u8]) -> Result<[u8; 32], Error> {
        let mut out = [0u8; 32];
        self.conn(conn)?
            .quic
            .crypto_session()
            .export_keying_material(&mut out, label, b"")
            .map_err(|_| Error::new("the connection's keys"))?;
        Ok(out)
    }

    /// When the driver's turn began.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Whether `conn` is open and its handshake done.
    pub fn is_connected(&mut self, conn: ConnKey) -> bool {
        self.conn(conn).is_ok_and(|c| c.connected && !c.quic.is_closed())
    }
}

/// `conn` among `conns`, under the number after the last one given; that number.
fn place(conns: &mut HashMap<ConnKey, Conn>, next: &mut ConnKey, conn: Conn) -> ConnKey {
    let key = *next;
    // 2^64 connections: more than a driver lives to see.
    *next = next.wrapping_add(1);
    conns.insert(key, conn);
    key
}

/// A QUIC endpoint on its own socket, its handler `H`.
#[allow(missing_debug_implementations)]
pub struct Driver<H: Handler> {
    socket: UdpSocket,
    waker: SocketAddr,
    endpoint: Endpoint,
    conns: HashMap<ConnKey, Conn>,
    next: ConnKey,
    /// The connections' numbers, as a turn goes through them.
    keys: Vec<ConnKey>,
    client: Option<ClientConfigHandle>,
    commands: mpsc::Receiver<Command<H::Command>>,
    handler: H,
    stopping: bool,
    out: VecDeque<(SocketAddr, Vec<u8>)>,
}

impl<H: Handler> Driver<H> {
    /// A driver on a socket bound to `at`, serving connections under `server` where it is
    /// given, and the [`Sender`] that reaches it.
    pub fn bind(
        at: SocketAddr,
        server: Option<ServerConfig>,
        handler: H,
    ) -> Result<(Self, Sender<H::Command>), Error> {
        let socket = UdpSocket::bind(at).map_err(|e| Error::new(format!("{at}: {e}")))?;
        let local = socket
            .local_addr()
            .map_err(|e| Error::new(format!("{at}: {e}")))?;
        // Wakes come from loopback whatever address the socket serves on.
        let loopback = if local.is_ipv4() { "127.0.0.1:0" } else { "[::1]:0" };
        let waker = UdpSocket::bind(loopback).map_err(|e| Error::new(format!("the driver's waker: {e}")))?;
        let waker_at = waker
            .local_addr()
            .map_err(|e| Error::new(format!("the driver's waker: {e}")))?;
        let mut to = local;
        if to.ip().is_unspecified() {
            to.set_ip(waker_at.ip());
        }
        let endpoint = Endpoint::new(EndpointConfig::default(), server, true, None)
            .map_err(|_| Error::new("no random bytes for QUIC"))?;
        let (commands_tx, commands) = mpsc::channel();
        Ok((
            Driver {
                socket,
                waker: waker_at,
                endpoint,
                conns: HashMap::new(),
                next: 0,
                keys: Vec::new(),
                client: None,
                commands,
                handler,
                stopping: false,
                out: VecDeque::new(),
            },
            Sender {
                commands: commands_tx,
                waker,
                to,
            },
        ))
    }

    /// The address the socket serves on.
    pub fn local(&self) -> Result<SocketAddr, Error> {
        self.socket
            .local_addr()
            .map_err(|e| Error::new(format!("the driver's socket: {e}")))
    }

    /// The handler, once the driver has ended.
    pub fn into_handler(self) -> H {
        self.handler
    }

    /// The handler, before the driver runs.
    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }

    /// Runs until [`Command::Stop`] comes, or the handler is done, and what it holds is
    /// sent.
    pub fn run(&mut self) -> Result<(), Error> {
        let mut datagram = vec![0u8; DATAGRAM];
        loop {
            let now = Instant::now();
            self.commands_now(now);
            if self.handler.due().is_some_and(|at| at <= now) {
                let (mut io, handler) = self.io(now);
                handler.timeout(&mut io, now);
                self.client = io.client;
            }
            self.timers(now);
            self.transmit(now)?;
            if (self.stopping || self.handler.done()) && self.out.is_empty() {
                return Ok(());
            }
            let next = match (self.next_timer(), self.handler.due()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let wait = next
                .map(|at| at.saturating_duration_since(Instant::now()))
                .unwrap_or(crate::tls::IDLE)
                .max(Duration::from_micros(100));
            self.socket
                .set_read_timeout(Some(wait))
                .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
            match self.socket.recv_from(&mut datagram) {
                Ok((n, from)) => {
                    if from == self.waker {
                        continue;
                    }
                    let now = Instant::now();
                    let bytes = BytesMut::from(datagram.get(..n).unwrap_or_default());
                    self.receive(now, from, bytes);
                    // Whatever else came meanwhile, before a turn sends anything.
                    self.socket
                        .set_nonblocking(true)
                        .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
                    while let Ok((n, from)) = self.socket.recv_from(&mut datagram) {
                        if from != self.waker {
                            let bytes = BytesMut::from(datagram.get(..n).unwrap_or_default());
                            self.receive(now, from, bytes);
                        }
                    }
                    self.socket
                        .set_nonblocking(false)
                        .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                // A datagram a peer's ICMP refused (Windows says so on the next read):
                // its connection's timers deal with it.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(e) => return Err(Error::new(format!("the driver's socket: {e}"))),
            }
        }
    }

    fn io(&mut self, now: Instant) -> (Io<'_>, &mut H) {
        (
            Io {
                endpoint: &mut self.endpoint,
                conns: &mut self.conns,
                next: &mut self.next,
                client: self.client,
                now,
            },
            &mut self.handler,
        )
    }

    fn commands_now(&mut self, now: Instant) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Stop => {
                    self.stopping = true;
                    // Each peer is told, by the close the next turn sends.
                    for conn in self.conns.values_mut() {
                        conn.quic.close(
                            now,
                            VarInt::from_u32(0),
                            bytes::Bytes::from_static(b"the daemon stopped"),
                        );
                    }
                }
                Command::Handler(c) => {
                    let (mut io, handler) = self.io(now);
                    handler.command(&mut io, c);
                    self.client = io.client;
                }
            }
        }
    }

    fn receive(&mut self, now: Instant, from: SocketAddr, bytes: BytesMut) {
        let mut response = Vec::new();
        match self.endpoint.handle(now, from, None, None, bytes, &mut response) {
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.stopping {
                    if let Some(transmit) = self.endpoint.refuse(incoming, &mut response) {
                        self.out.push_back((
                            transmit.destination,
                            response.get(..transmit.size).unwrap_or_default().to_vec(),
                        ));
                    }
                    return;
                }
                match self.endpoint.accept(incoming, now, &mut response, None, None) {
                    Ok((handle, quic)) => {
                        let key = place(
                            &mut self.conns,
                            &mut self.next,
                            Conn {
                                handle,
                                quic,
                                connected: false,
                            },
                        );
                        self.pump(now, key);
                    }
                    Err(e) => {
                        if let Some(transmit) = e.response {
                            self.out.push_back((
                                transmit.destination,
                                response.get(..transmit.size).unwrap_or_default().to_vec(),
                            ));
                        }
                    }
                }
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(key) = self.key_of(handle) {
                    if let Some(conn) = self.conns.get_mut(&key) {
                        conn.quic.handle_event(event, self.endpoint.configs_mut());
                    }
                    self.pump(now, key);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.out.push_back((
                    transmit.destination,
                    response.get(..transmit.size).unwrap_or_default().to_vec(),
                ));
            }
            None => {}
        }
    }

    fn key_of(&self, handle: ConnectionHandle) -> Option<ConnKey> {
        self.conns
            .iter()
            .find_map(|(key, c)| (c.handle == handle).then_some(*key))
    }

    /// Runs `key`'s endpoint events and tells the handler what changed.
    fn pump(&mut self, now: Instant, key: ConnKey) {
        loop {
            let Some(conn) = self.conns.get_mut(&key) else {
                return;
            };
            let mut events = Vec::new();
            while let Some(event) = conn.quic.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(conn.handle, event) {
                    conn.quic.handle_event(event, self.endpoint.configs_mut());
                }
            }
            while let Some(event) = conn.quic.poll() {
                events.push(event);
            }
            if events.is_empty() {
                break;
            }
            for event in events {
                self.event(now, key, event);
            }
        }
        // A connection drained of its last packets is let go.
        if self.conns.get(&key).is_some_and(|c| c.quic.is_drained()) {
            self.conns.remove(&key);
        }
    }

    fn event(&mut self, now: Instant, key: ConnKey, event: Event) {
        match event {
            Event::Connected => {
                let peer = self
                    .conns
                    .get(&key)
                    .and_then(|c| c.quic.crypto_session().peer_identity())
                    .and_then(|any| {
                        any.downcast::<Vec<hyper_quic::rustls::pki_types::CertificateDer<'static>>>()
                            .ok()
                    })
                    .and_then(|certs| certs.first().map(|c| c.as_ref().to_vec()))
                    .unwrap_or_default();
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.connected = true;
                }
                let (mut io, handler) = self.io(now);
                if let Err(close) = handler.connected(&mut io, key, &peer) {
                    io.close(key, close.code, close.why);
                }
                self.client = io.client;
            }
            Event::ConnectionLost { reason } => {
                let code = match &reason {
                    ConnectionError::ApplicationClosed(close) => Some(close.error_code.into_inner()),
                    _ => None,
                };
                let why = reason.to_string();
                let (mut io, handler) = self.io(now);
                handler.lost(&mut io, key, &why, code);
                self.client = io.client;
            }
            Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                // Streams the peer opened are taken as they come. QUIC gives this event
                // before any queued for them (quinn-proto's StreamsState::poll).
                while let Some(conn) = self.conns.get_mut(&key) {
                    let Some(stream) = conn.quic.streams().accept(Dir::Bi) else {
                        break;
                    };
                    let (mut io, handler) = self.io(now);
                    handler.opened(&mut io, key, stream);
                    self.client = io.client;
                }
            }
            Event::Stream(StreamEvent::Stopped { id, .. }) => {
                // RFC 9000 §3.5: a side whose peer reads no more of it is reset.
                let (mut io, handler) = self.io(now);
                io.reset(key, id);
                handler.stopped(&mut io, key, id);
                self.client = io.client;
            }
            Event::Stream(StreamEvent::Readable { id }) => {
                let (mut io, handler) = self.io(now);
                handler.readable(&mut io, key, id);
                self.client = io.client;
            }
            Event::Stream(StreamEvent::Writable { id }) => {
                let (mut io, handler) = self.io(now);
                handler.writable(&mut io, key, id);
                self.client = io.client;
            }
            _ => {}
        }
    }

    fn timers(&mut self, now: Instant) {
        self.keys.clear();
        for (key, conn) in &mut self.conns {
            if conn.quic.poll_timeout().is_some_and(|at| at <= now) {
                conn.quic.handle_timeout(now);
                self.keys.push(*key);
            }
        }
        for at in 0..self.keys.len() {
            if let Some(&key) = self.keys.get(at) {
                self.pump(now, key);
            }
        }
    }

    fn next_timer(&mut self) -> Option<Instant> {
        self.conns
            .values_mut()
            .filter_map(|c| c.quic.poll_timeout())
            .min()
    }

    fn transmit(&mut self, now: Instant) -> Result<(), Error> {
        let mut buffer = Vec::new();
        self.keys.clear();
        self.keys.extend(self.conns.keys());
        for at in 0..self.keys.len() {
            let Some(&key) = self.keys.get(at) else {
                break;
            };
            loop {
                buffer.clear();
                let Some(conn) = self.conns.get_mut(&key) else {
                    break;
                };
                let Some(transmit) = conn
                    .quic
                    .poll_transmit(now, 1, &mut buffer, self.endpoint.configs())
                else {
                    break;
                };
                self.out.push_back((
                    transmit.destination,
                    buffer.get(..transmit.size).unwrap_or_default().to_vec(),
                ));
                self.pump(now, key);
            }
        }
        while let Some((to, bytes)) = self.out.front() {
            match self.socket.send_to(bytes, *to) {
                Ok(_) => {
                    self.out.pop_front();
                }
                // The socket's buffer is full: the rest goes on the next turn.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // A datagram refused as it was sent goes as a lost one would.
                Err(_) => {
                    self.out.pop_front();
                }
            }
        }
        Ok(())
    }
}
