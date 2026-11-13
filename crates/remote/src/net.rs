//! A QUIC endpoint on a UDP socket of its own (D180), driven by one thread: datagrams in and
//! out, the connections' timers, and their streams, which a [`Handler`] reads and writes. Other
//! threads reach the driver with a [`Command`] on its channel and a datagram to its socket
//! from its waker, which wakes it from the socket's read: what std gives every target,
//! Windows' consoles and pipes among the descriptors no poll(2) of a socket reaches.

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
pub(crate) const DATAGRAM: usize = 65_527;

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
    commands: Commands<C>,
    stopping: Arc<AtomicBool>,
    waker: UdpSocket,
    to: SocketAddr,
}

enum Commands<C> {
    Unbounded(mpsc::Sender<Command<C>>),
    Bounded(mpsc::SyncSender<Command<C>>),
}

/// Resources already admitted by a bounded endpoint's owner. These limits do not
/// reserve host/kernel memory or grant it: the caller retains that reservation.
#[derive(Debug, Clone, Copy)]
pub struct DriverLimits {
    pub connections: usize,
    pub command_slots: usize,
    pub queued_datagram_bytes: usize,
    pub handshake_patience: Duration,
}

impl<C> std::fmt::Debug for Sender<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl<C> Sender<C> {
    pub(crate) fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        socket_buffers(&self.waker)
    }

    /// Sends `command` and wakes the driver; an error once the driver has ended.
    pub fn send(&self, command: Command<C>) -> Result<(), Error> {
        self.try_send(command).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::new("the driver's admitted command slots are full"),
            mpsc::TrySendError::Disconnected(_) => Error::new("the connection's driver has ended"),
        })
    }

    /// A full handoff returns ownership without blocking its caller.
    pub fn try_send(&self, command: Command<C>) -> Result<(), mpsc::TrySendError<Command<C>>> {
        match &self.commands {
            Commands::Unbounded(to) => to
                .send(command)
                .map_err(|e| mpsc::TrySendError::Disconnected(e.0))?,
            Commands::Bounded(to) => to.try_send(command)?,
        }
        let _ = self.waker.send_to(&[0], self.to);
        Ok(())
    }

    /// Idempotent owner revocation does not wait behind queued data.
    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.waker.send_to(&[0], self.to);
    }
}

// Queue sizes are cost facts for an owner's reservation, never admission authority.
// Linux socket(7) reports its doubled bookkeeping-inclusive SO_*BUF values.
fn socket_buffers(socket: &UdpSocket) -> Result<u64, Error> {
    #[cfg(unix)]
    fn get(socket: &UdpSocket, option: i32) -> std::io::Result<u64> {
        use std::os::fd::AsRawFd as _;
        let mut bytes: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&bytes) as libc::socklen_t;
        // SAFETY: the owned socket and a correctly sized integer output.
        let rc = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&raw mut bytes).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if len as usize != std::mem::size_of_val(&bytes) || bytes <= 0 {
            return Err(std::io::Error::other("unknown UDP queue size"));
        }
        u64::try_from(bytes).map_err(std::io::Error::other)
    }
    #[cfg(windows)]
    fn get(socket: &UdpSocket, option: i32) -> std::io::Result<u64> {
        use std::os::windows::io::AsRawSocket as _;
        use windows_sys::Win32::Networking::WinSock::{SOL_SOCKET, WSAGetLastError, getsockopt};
        let mut bytes: i32 = 0;
        let mut len = std::mem::size_of_val(&bytes) as i32;
        // SAFETY: the owned socket and a correctly sized integer output.
        let rc = unsafe {
            getsockopt(
                socket.as_raw_socket() as usize,
                SOL_SOCKET,
                option,
                (&raw mut bytes).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            // SAFETY: retrieves this thread's Winsock failure.
            return Err(std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        if len as usize != std::mem::size_of_val(&bytes) || bytes <= 0 {
            return Err(std::io::Error::other("unknown UDP queue size"));
        }
        u64::try_from(bytes).map_err(std::io::Error::other)
    }
    #[cfg(unix)]
    use libc::{SO_RCVBUF, SO_SNDBUF};
    #[cfg(windows)]
    use windows_sys::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF};
    get(socket, SO_RCVBUF)
        .and_then(|rx| {
            get(socket, SO_SNDBUF).and_then(|tx| {
                rx.checked_add(tx)
                    .ok_or_else(|| std::io::Error::other("UDP queue size overflow"))
            })
        })
        .map_err(|e| Error::new(format!("the driver's kernel queues: {e}")))
}

impl<C> Sender<C> {
    /// Another sender to the same driver, for another thread.
    pub fn try_clone(&self) -> Result<Sender<C>, Error> {
        Ok(Sender {
            commands: match &self.commands {
                Commands::Unbounded(to) => Commands::Unbounded(to.clone()),
                Commands::Bounded(to) => Commands::Bounded(to.clone()),
            },
            stopping: Arc::clone(&self.stopping),
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
    /// Why a connection ended, as QUIC told it (RFC 9000 §20, RFC 9001 §4.8), just before
    /// [`Handler::lost`] tells of it: for a handler that types its peer's refusals, which
    /// `lost`'s words and application code do not carry. Nothing, by default; a connection
    /// whose handshake ran out of patience ([`DriverLimits::handshake_patience`]) has none.
    fn cause(&mut self, _conn: ConnKey, _error: &ConnectionError) {}
    /// Another thread's command.
    fn command(&mut self, io: &mut Io<'_>, command: Self::Command);
    /// The peer allows more of the streams this side opens (MAX_STREAMS, RFC 9000 §4.6): one
    /// that could not be opened before may be now. Nothing, for a handler that opens none.
    fn available(&mut self, _io: &mut Io<'_>, _conn: ConnKey) {}
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
    liveness: Liveness,
}

/// When a connection was last heard from, and whether it lives on: its handshake held to
/// [`IDLE`](crate::tls::IDLE) from its start, by the driver, as nothing it carries is
/// anyone's yet; then to [`QUIET`](crate::tls::QUIET) from the last packet its peer sent, by
/// its transport's idle timer (RFC 9000 §10.1), which each packet restarts, the
/// acknowledgments of what it sends among them: it lives while it progresses. The longest
/// silence it lived through is kept, the evidence its bound is judged by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    began: Instant,
    heard: Option<Instant>,
    connected: bool,
    longest: Duration,
}

impl Liveness {
    pub fn new(now: Instant) -> Liveness {
        Liveness {
            began: now,
            heard: None,
            connected: false,
            longest: Duration::ZERO,
        }
    }

    /// Its peer was heard at `now`: the silence since it last was, measured.
    pub fn heard(&mut self, now: Instant) {
        if let Some(last) = self.heard {
            self.longest = self.longest.max(now.saturating_duration_since(last));
        }
        self.heard = Some(now);
    }

    /// Its handshake is done.
    pub fn connected(&mut self) {
        self.connected = true;
    }

    /// When the driver ends it unless its handshake is done first: none once it is, as its
    /// transport's idle timer holds it from then on.
    pub fn due(&self) -> Option<Instant> {
        if self.connected {
            return None;
        }
        self.began.checked_add(crate::tls::IDLE)
    }

    /// The longest its peer went unheard, between two packets of its own.
    pub fn longest_silence(&self) -> Duration {
        self.longest
    }
}

/// What a connection saw of the path it crossed, for its caller's record: the longest its
/// peer went unheard ([`Liveness`]), its round trip as last estimated (RFC 9002 §5), the bytes
/// it sent in UDP datagrams and the bytes of them its peer acknowledged, and the packets it
/// took for lost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    pub longest_silence: Duration,
    pub rtt: Duration,
    pub sent_bytes: u64,
    pub acked_bytes: u64,
    pub lost_packets: u64,
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
                liveness: Liveness::new(self.now),
            },
        ))
    }

    /// What `conn` saw of its path so far.
    pub fn seen(&mut self, conn: ConnKey) -> Option<Seen> {
        let conn = self.conn(conn).ok()?;
        let stats = conn.quic.stats();
        Some(Seen {
            longest_silence: conn.liveness.longest_silence(),
            rtt: stats.path.rtt,
            sent_bytes: stats.udp_tx.bytes,
            acked_bytes: stats.path.acked_bytes,
            lost_packets: stats.path.lost_packets,
        })
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
        self.read_within(conn, stream, usize::MAX).unwrap_or(Read::Reset)
    }

    /// Takes no more than `limit` bytes; a bounded protocol can inspect its header
    /// before admitting the payload. Buffer growth is fallible.
    pub fn read_within(&mut self, conn: ConnKey, stream: StreamId, limit: usize) -> Result<Read, Error> {
        if limit == 0 {
            return Ok(Read::Blocked);
        }
        let Ok(conn) = self.conn(conn) else {
            return Ok(Read::Reset);
        };
        let mut recv = conn.quic.recv_stream(stream);
        let mut chunks = match recv.read(true) {
            Ok(chunks) => chunks,
            Err(_) => return Ok(Read::Reset),
        };
        let mut got = Vec::new();
        let mut ended = false;
        while got.len() < limit {
            match chunks.next(limit.saturating_sub(got.len())) {
                Ok(Some(chunk)) => {
                    got.try_reserve_exact(chunk.bytes.len())
                        .map_err(|_| Error::new("no admitted memory for received stream bytes"))?;
                    got.extend_from_slice(&chunk.bytes);
                }
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(hyper_quic::ReadError::Blocked) => break,
                Err(hyper_quic::ReadError::Reset(_)) => {
                    let _ = chunks.finalize();
                    return Ok(Read::Reset);
                }
            }
        }
        let _ = chunks.finalize();
        Ok(match (got.is_empty(), ended) {
            (_, true) => Read::End(got),
            (false, false) => Read::Bytes(got),
            (true, false) => Read::Blocked,
        })
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

    /// Sets what `conn`'s peer may have open and send, and what this side keeps unacknowledged,
    /// as streams come and go (RFC 9000 §4.1, §4.6): its concurrent bidirectional streams, the
    /// connection's receive window, and this side's send window.
    pub fn limit(&mut self, conn: ConnKey, streams: u64, receive: u64, send: u64) -> Result<(), Error> {
        let window = |n: u64| VarInt::from_u64(n).map_err(|_| Error::new("a limit past QUIC's"));
        let (streams, receive) = (window(streams)?, window(receive)?);
        let conn = self.conn(conn)?;
        conn.quic.set_max_concurrent_streams(Dir::Bi, streams);
        conn.quic.set_receive_window(receive);
        conn.quic.set_send_window(send);
        Ok(())
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
    stop_requested: Arc<AtomicBool>,
    limits: Option<DriverLimits>,
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
        Self::bind_with_limits(at, server, handler, None)
    }

    /// Uses only the command/connection/datagram capacity the caller admitted.
    pub fn bind_bounded(
        at: SocketAddr,
        server: Option<ServerConfig>,
        handler: H,
        limits: DriverLimits,
    ) -> Result<(Self, Sender<H::Command>), Error> {
        if limits.connections == 0 || limits.command_slots == 0 || limits.queued_datagram_bytes < DATAGRAM {
            return Err(Error::new(
                "endpoint capacity cannot hold its protocol state and one datagram",
            ));
        }
        Self::bind_with_limits(at, server, handler, Some(limits))
    }

    fn bind_with_limits(
        at: SocketAddr,
        server: Option<ServerConfig>,
        handler: H,
        limits: Option<DriverLimits>,
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
        let (commands_tx, commands) = match limits {
            Some(limits) => {
                let (tx, rx) = mpsc::sync_channel(limits.command_slots);
                (Commands::Bounded(tx), rx)
            }
            None => {
                let (tx, rx) = mpsc::channel();
                (Commands::Unbounded(tx), rx)
            }
        };
        let stop_requested = Arc::new(AtomicBool::new(false));
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
                stop_requested: Arc::clone(&stop_requested),
                limits,
                out: VecDeque::new(),
            },
            Sender {
                commands: commands_tx,
                stopping: stop_requested,
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

    pub(crate) fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        socket_buffers(&self.socket)
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
        let mut datagram = Vec::new();
        datagram
            .try_reserve_exact(DATAGRAM)
            .map_err(|_| Error::new("no memory for the endpoint datagram"))?;
        datagram.resize(DATAGRAM, 0);
        loop {
            let now = Instant::now();
            self.commands_now(now);
            if self.handler.due().is_some_and(|at| at <= now) {
                let (mut io, handler) = self.io(now);
                handler.timeout(&mut io, now);
                self.client = io.client;
            }
            self.timers(now);
            self.expire(now);
            if self.limits.is_some() {
                self.socket
                    .set_nonblocking(true)
                    .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
                let sent = self.transmit(now);
                self.socket
                    .set_nonblocking(false)
                    .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
                sent?;
                if self.stopping {
                    self.out.clear();
                    return Ok(());
                }
            } else {
                self.transmit(now)?;
            }
            if (self.stopping || self.handler.done()) && self.out.is_empty() {
                return Ok(());
            }
            let next = match (self.next_timer(), self.handler.due()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let wait = next
                .map(|at| at.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| self.limits.map_or(crate::tls::IDLE, |l| l.handshake_patience))
                .max(if self.limits.is_some() {
                    Duration::from_nanos(1)
                } else {
                    Duration::from_micros(100)
                });
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
                    self.receive(now, from, bytes)?;
                    // Whatever else came meanwhile, before a turn sends anything.
                    self.socket
                        .set_nonblocking(true)
                        .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
                    let turns = self
                        .limits
                        .map_or(usize::MAX, |l| l.queued_datagram_bytes / DATAGRAM);
                    for _ in 1..turns {
                        let Ok((n, from)) = self.socket.recv_from(&mut datagram) else {
                            break;
                        };
                        if from != self.waker {
                            let bytes = BytesMut::from(datagram.get(..n).unwrap_or_default());
                            self.receive(now, from, bytes)?;
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
                // A signal interrupts a timed receive (Linux signal(7)): another turn
                // checks commands and timers without losing any of the connections.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
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
        if self.stop_requested.swap(false, Ordering::AcqRel) {
            self.stopping = true;
            if self.limits.is_some() {
                self.out.clear();
            }
            for conn in self.conns.values_mut() {
                conn.quic.close(
                    now,
                    VarInt::from_u32(0),
                    bytes::Bytes::from_static(b"the daemon stopped"),
                );
            }
        }
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

    fn receive(&mut self, now: Instant, from: SocketAddr, bytes: BytesMut) -> Result<(), Error> {
        let mut response = Vec::new();
        match self.endpoint.handle(now, from, None, None, bytes, &mut response) {
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.stopping || self.limits.is_some_and(|l| self.conns.len() >= l.connections) {
                    if let Some(transmit) = self.endpoint.refuse(incoming, &mut response) {
                        self.queue_datagram(
                            transmit.destination,
                            response.get(..transmit.size).unwrap_or_default(),
                        )?;
                    }
                    return Ok(());
                }
                match self.endpoint.accept(incoming, now, &mut response, None, None) {
                    Ok((handle, quic)) => {
                        let mut liveness = Liveness::new(now);
                        liveness.heard(now);
                        let key = place(
                            &mut self.conns,
                            &mut self.next,
                            Conn {
                                handle,
                                quic,
                                connected: false,
                                liveness,
                            },
                        );
                        self.pump(now, key);
                    }
                    Err(e) => {
                        if let Some(transmit) = e.response {
                            self.queue_datagram(
                                transmit.destination,
                                response.get(..transmit.size).unwrap_or_default(),
                            )?;
                        }
                    }
                }
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(key) = self.key_of(handle) {
                    if let Some(conn) = self.conns.get_mut(&key) {
                        conn.liveness.heard(now);
                        conn.quic.handle_event(event, self.endpoint.configs_mut());
                    }
                    self.pump(now, key);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.queue_datagram(
                    transmit.destination,
                    response.get(..transmit.size).unwrap_or_default(),
                )?;
            }
            None => {}
        }
        Ok(())
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
                    conn.liveness.connected();
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
                handler.cause(key, &reason);
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
            Event::Stream(StreamEvent::Available { dir: Dir::Bi }) => {
                let (mut io, handler) = self.io(now);
                handler.available(&mut io, key);
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

    /// Ends each connection whose handshake is not done within [`IDLE`](crate::tls::IDLE)
    /// of its start ([`Liveness::due`]), its handler told as of a timeout: its transport's
    /// own idle bound is the longer [`QUIET`](crate::tls::QUIET), for a peer known to be
    /// there.
    fn expire(&mut self, now: Instant) {
        self.keys.clear();
        for (key, conn) in &self.conns {
            let due = self.limits.map_or_else(
                || conn.liveness.due(),
                |l| {
                    (!conn.connected)
                        .then(|| conn.liveness.began.checked_add(l.handshake_patience))
                        .flatten()
                },
            );
            if !conn.quic.is_closed() && due.is_some_and(|at| at <= now) {
                self.keys.push(*key);
            }
        }
        for at in 0..self.keys.len() {
            let Some(&key) = self.keys.get(at) else {
                break;
            };
            let (mut io, handler) = self.io(now);
            io.close(key, 0, "its handshake took too long");
            handler.lost(&mut io, key, "timed out", None);
            self.client = io.client;
        }
    }

    fn next_timer(&mut self) -> Option<Instant> {
        let limits = self.limits;
        self.conns
            .values_mut()
            .filter_map(|c| {
                let handshake = limits
                    .map_or_else(
                        || c.liveness.due(),
                        |l| {
                            (!c.connected)
                                .then(|| c.liveness.began.checked_add(l.handshake_patience))
                                .flatten()
                        },
                    )
                    .filter(|_| !c.quic.is_closed());
                match (c.quic.poll_timeout(), handshake) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            })
            .min()
    }

    fn queue_datagram(&mut self, to: SocketAddr, bytes: &[u8]) -> Result<(), Error> {
        let held = if self.limits.is_some() {
            self.out
                .iter()
                .try_fold(0usize, |n, (_, b)| n.checked_add(b.capacity()))
                .ok_or_else(|| Error::new("endpoint datagram accounting overflow"))?
        } else {
            0
        };
        if self
            .limits
            .is_some_and(|l| bytes.len() > l.queued_datagram_bytes.saturating_sub(held))
        {
            return Err(Error::new("the endpoint's admitted datagram storage is full"));
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| Error::new("no memory for an endpoint datagram"))?;
        if self
            .limits
            .is_some_and(|l| owned.capacity() > l.queued_datagram_bytes.saturating_sub(held))
        {
            return Err(Error::new("the datagram allocation exceeds its admitted storage"));
        }
        owned.extend_from_slice(bytes);
        self.out
            .try_reserve(1)
            .map_err(|_| Error::new("no memory for an endpoint datagram entry"))?;
        self.out.push_back((to, owned));
        Ok(())
    }

    fn flush_datagrams(&mut self) {
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
    }

    fn transmit(&mut self, now: Instant) -> Result<(), Error> {
        if self.limits.is_some() {
            self.flush_datagrams();
            if !self.out.is_empty() {
                return Ok(());
            }
        }
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
                self.queue_datagram(
                    transmit.destination,
                    buffer.get(..transmit.size).unwrap_or_default(),
                )?;
                if self.limits.is_some() {
                    self.flush_datagrams();
                    if !self.out.is_empty() {
                        break;
                    }
                }
                self.pump(now, key);
            }
        }
        self.flush_datagrams();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tls::{IDLE, QUIET};

    /// A timed socket read is not restarted after a signal, even with SA_RESTART
    /// (Linux signal(7)). A harmless host signal must leave the driver serving commands.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_interrupted_receive_keeps_serving_commands() {
        const CHILD: &str = "SHARDS_TEST_INTERRUPTED_DRIVER";
        if std::env::var_os(CHILD).is_none() {
            // Signal dispositions belong to the process: keep this one away from every
            // other test, including whichever handlers the test runner already installed.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "net::tests::an_interrupted_receive_keeps_serving_commands",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use std::os::fd::AsRawFd;
        use std::os::unix::thread::JoinHandleExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        static HEARD: AtomicBool = AtomicBool::new(false);
        extern "C" fn heard(_: libc::c_int) {
            HEARD.store(true, Ordering::SeqCst);
        }

        // SAFETY: a zeroed sigaction has no uninitialized fields; its mask is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = heard as *const () as usize;
        // SAFETY: action is writable, and the handler only stores a lock-free atomic bool.
        unsafe {
            assert_eq!(libc::sigemptyset(&mut action.sa_mask), 0);
            assert_eq!(libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()), 0);
        }

        struct Waiting;
        impl Handler for Waiting {
            type Command = ();
            fn connected(&mut self, _: &mut Io<'_>, _: ConnKey, _: &[u8]) -> Result<(), Close> {
                Ok(())
            }
            fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn readable(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn writable(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
            fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, _: Option<u64>) {}
            fn command(&mut self, _: &mut Io<'_>, _: ()) {}
            fn done(&self) -> bool {
                false
            }
        }

        let (mut driver, sender) = Driver::bind("127.0.0.1:0".parse().unwrap(), None, Waiting).unwrap();
        let fd = driver.socket.as_raw_fd();
        let (started, ready) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: gettid identifies this live thread.
            started.send(unsafe { libc::gettid() }).unwrap();
            driver.run()
        });
        let tid = ready.recv().unwrap();
        let thread = worker.as_pthread_t();
        // std's RawPthread is an integer on Linux; musl's libc::pthread_t is a pointer.
        #[cfg(target_env = "musl")]
        let thread = thread as libc::pthread_t;
        let syscall = format!("/proc/self/task/{tid}/syscall");
        loop {
            let call = std::fs::read_to_string(&syscall).unwrap();
            let mut words = call.split_whitespace();
            let number = words.next().and_then(|s| s.parse::<libc::c_long>().ok());
            let socket = words
                .next()
                .and_then(|s| i32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            if number == Some(libc::SYS_recvfrom) && socket == Some(fd) {
                break;
            }
            assert!(!worker.is_finished(), "the driver ended before its receive");
            std::thread::yield_now();
        }
        // SAFETY: the borrowed pthread identity belongs to worker, still owned and not
        // joined. It is live, blocked in this socket's recvfrom; its handler records
        // delivery without restarting that timed read.
        assert_eq!(unsafe { libc::pthread_kill(thread, libc::SIGUSR1) }, 0);
        while !HEARD.load(Ordering::SeqCst) {
            assert!(!worker.is_finished(), "the driver ended before the signal");
            std::thread::yield_now();
        }
        // The old driver may already have dropped its receiver after EINTR: check its
        // actual result, rather than letting a failed Stop hide that error.
        let _ = sender.send(Command::Stop);
        worker.join().unwrap().unwrap();
    }

    /// A connection's handshake is held to IDLE from its start, and nothing of the driver's
    /// after: an established one lives as long as its peer is heard within QUIET, however
    /// long it carries, and the longest silence it lived through is the one measured.
    #[test]
    fn a_connection_lives_while_it_is_heard_and_its_handshake_alone_is_held_to_idle() {
        assert!(QUIET > IDLE);
        let t0 = Instant::now();
        let mut l = Liveness::new(t0);
        assert_eq!(l.due(), t0.checked_add(IDLE));
        l.heard(t0 + Duration::from_millis(30));
        // Heard from, its handshake still not done: still due at IDLE from its start.
        assert_eq!(l.due(), t0.checked_add(IDLE));
        l.connected();
        assert_eq!(l.due(), None);
        // An hour of a transfer, its peer heard every second but for one gap of 42 s, longer
        // than IDLE and shorter than QUIET: it lives, and the gap is what it says.
        let mut at = t0 + Duration::from_millis(30);
        for s in 1..3600u64 {
            at += if s == 1800 {
                Duration::from_secs(42)
            } else {
                Duration::from_secs(1)
            };
            l.heard(at);
        }
        assert_eq!(l.longest_silence(), Duration::from_secs(42));
        assert_eq!(l.due(), None);
    }
}
