//! A QUIC endpoint on a UDP socket of its own (D180), driven by one thread: datagrams in and
//! out, the connections' timers, and their streams, which a [`Handler`] reads and writes. Other
//! threads reach the driver with a [`Command`] on its channel and a datagram from its waker to
//! a wake socket of the driver's own, beside the one it serves on, which it waits on with that
//! one (poll(2); WSAPoll): what every target's sockets give, Windows' consoles and pipes among
//! the descriptors no poll of a socket reaches.

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use hyper_quic::{
    ClientConfig, ClientConfigHandle, Connection, ConnectionError, ConnectionHandle, DatagramEvent, Dir,
    Endpoint, EndpointConfig, EndpointEvent, Event, ServerConfig, StreamEvent, StreamId, VarInt,
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
    /// The connections it holds at once. One open, its handshake's or established, holds its
    /// place until it closes, or, established and stalled, until a newcomer needs the place
    /// (the driver's `reclaim`); one closed (closing or draining, RFC 9000 §10.2) until a
    /// newcomer needs the place (the driver's `free_closed`).
    pub connections: usize,
    pub command_slots: usize,
    pub queued_datagram_bytes: usize,
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

    /// Where its waker is bound: the one socket each of its clones holds again, told apart
    /// from every other socket by its own address.
    pub fn waker_addr(&self) -> Result<SocketAddr, Error> {
        self.waker
            .local_addr()
            .map_err(|e| Error::new(format!("the driver's waker: {e}")))
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
    /// `lost`'s words and application code do not carry. Nothing, by default; a stalled
    /// connection let go for a newcomer ([`Driver::reclaim`]) is QUIC's `TimedOut`.
    fn cause(&mut self, _conn: ConnKey, _error: &ConnectionError) {}
    /// A connection whose handshake has not ended sent another probe (RFC 9002 §6.2): `probes`
    /// datagrams in all, `waited` since it began. Its handshake waits on, with no window of its
    /// own where it has none or its window ran out ([`RanOut::Handshake`]), for as long as its
    /// handler does: one that gives up closes it.
    fn unanswered(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _probes: u64, _waited: Duration) {}
    /// A wait of `conn`'s ran out ([`RanOut`]): told once a wait at each stage, and the
    /// connection goes on.
    fn ran_out(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _what: RanOut) {}
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
    /// The window the handshake of a connection it opens is judged by ([`Liveness::due`]):
    /// none, waited for as long as its probing goes on.
    handshake: Option<Duration>,
    /// The slowest answer its peer's path has shown, which a connection it opens starts from.
    answer: Duration,
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

/// When a connection was last heard from, and how its waits stand (the person's ruling,
/// 2026-11-13: a cold start, and every wait after a run-out, follows the peer's liveness and
/// progress, and after that a wait's bound is the slowest answer seen). Its handshake, with a
/// peer reached before, is judged as a send is ([`Liveness::due`]): by a window of the slowest
/// handshake seen from that peer, no sooner than its probe timeout, past which it runs out and
/// goes on with none ([`Liveness::handshake_ran_out`]); with a peer never reached, or whose last
/// wait ran out, it has no window, its probing told as it goes ([`Liveness::unanswered`]). Once
/// connected, where its driver checks it at use ([`Liveness::look`]): each send of this
/// side's waits for the peer to acknowledge any of it, within a window of the slowest answer
/// seen on its path and the peer's own `max_ack_delay` (its transport parameter, as RFC 9002
/// §6.2.1's probe timeout adds it). Cold, before any send's answer has been seen on its path,
/// there is no window: a wait follows the peer's liveness and progress for as long as it takes,
/// as a handshake's packets, which a peer acknowledges at once (RFC 9000 §13.2.1), are no measure
/// of how long it takes to answer a request, its work and its host's scheduling with it (a join's
/// record synced to disk took a daemon 123-152 ms, past a window its handshake seeded). Past the
/// window a wait goes on while the peer makes progress (it is heard, or acknowledges), told once
/// as a run-out; a whole window with no progress is a stall, told once, and the wait follows the
/// peer's liveness with no window from then on, QUIC probing it (ROOT's ruling A, 2026-11-13):
/// its answer, whenever it comes, ends the stall and is the slowest seen, so that the path's
/// window covers such a stall from then on. Nothing here ends a connection. The longest silence
/// it lived through is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    began: Instant,
    /// The window its handshake is judged by, the slowest seen from its peer: none, or none once
    /// it ran out ([`Liveness::handshake_ran_out`]), waited for as long as its probing goes on.
    handshake: Option<Duration>,
    heard: Option<Instant>,
    /// When its handshake ended.
    connected: Option<Instant>,
    longest: Duration,
    /// The slowest answer seen on its path: what was known of it, then this connection's.
    answer: Duration,
    /// The peer's own `max_ack_delay`, once its transport parameters came.
    ack_delay: Duration,
    /// Its transport's probe timeout as last looked at (RFC 9002 §6.2.1), before which no wait
    /// of it is judged.
    pto: Duration,
    /// A send of this side's waiting for its answer.
    wait: Option<Wait>,
    /// Its transport's ack-eliciting frames sent and bytes acknowledged, as last looked at.
    sent: u64,
    acked: u64,
    /// A wait of its ran out: its handshake past its deadline, or a send past its window, or one
    /// stalled.
    ran_out: bool,
    /// Its peer refused it, definitely: an ICMP port-unreachable told its connected socket.
    refused: bool,
    /// The datagrams of its first flight, as its first look found them: no probe.
    flight: Option<u64>,
    /// The datagrams sent as its first look, or its last probe told, found them.
    probes: u64,
    /// When a newcomer that found its driver's places taken asked it for an answer (a PING), the
    /// first such ask it has not answered since ([`Liveness::ask`]), judged by its probe timeout
    /// ([`Liveness::unanswering`]).
    asked: Option<Instant>,
    /// A stalled send's answer came this long after it was sent, not yet told
    /// ([`Look::Answered`]).
    answered: Option<Duration>,
}

/// A send waiting for its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wait {
    since: Instant,
    /// The bytes acknowledged as it began: any more is its answer.
    acked: u64,
    /// The silence before it, since the peer was last heard.
    quiet: Duration,
    /// How far it has gone unanswered, each stage told as it is reached.
    stage: Stage,
}

/// How far a send's wait has gone unanswered (R04): each stage told once ([`Look`]), and the
/// wait ends, whatever its stage, as its answer comes ([`Liveness::note`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Within its window, or past it with nothing told yet.
    Waiting,
    /// Past its window and probe timeout while its peer made progress: told.
    RanOut,
    /// A whole window with no progress past its probe timeout: told, and followed with no
    /// window until its answer comes.
    Stalled,
}

/// A wait of a connection's that ran out, told once ([`Handler::ran_out`]) as the connection goes
/// on (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RanOut {
    /// Its handshake, with a peer reached before: nothing heard from it in the slowest handshake
    /// seen from it (`window`), past its probe timeout ([`Liveness::due`]), `waited` since it
    /// began. It goes on as a peer never reached's does, with no deadline of its own, each probe
    /// told ([`Handler::unanswered`]).
    Handshake { waited: Duration, window: Duration },
    /// A send, its connection established: unanswered past its window and probe timeout while its
    /// peer still makes progress ([`Look::RanOut`]).
    Send { waited: Duration, window: Duration },
    /// A send, its connection established: a whole window (`window`) with no progress past its
    /// probe timeout, its peer `unheard` that long ([`Look::Stalled`]). The peer may be busy,
    /// frozen or gone: the wait follows its liveness with no window, QUIC probing it, until its
    /// answer comes ([`RanOut::Answered`]), which is the slowest seen from then on, or its holder
    /// gives up.
    Stalled { unheard: Duration, window: Duration },
    /// A stalled send's answer came at last, `after` it was sent: the stall is over, and its
    /// time is the slowest answer seen from then on ([`Look::Answered`]).
    Answered { after: Duration },
}

/// What looking at an established connection found (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Nothing waits, or what waits is within its window, or stalled and told.
    Fine,
    /// A send has waited past its window, its peer still making progress: told once, and the
    /// connection goes on.
    RanOut { waited: Duration, window: Duration },
    /// A whole window passed with no progress, its peer `unheard` that long: told once, and the
    /// wait goes on with no window ([`RanOut::Stalled`]).
    Stalled { unheard: Duration, window: Duration },
    /// A stalled send was answered, `after` it was sent: told once ([`RanOut::Answered`]).
    Answered { after: Duration },
}

impl Liveness {
    /// A connection begun at `now`, its handshake judged by the window `handshake` (none: waited
    /// for as long as its probing goes on), the slowest answer its path has shown `answer` (zero
    /// where none has).
    pub fn new(now: Instant, handshake: Option<Duration>, answer: Duration) -> Liveness {
        Liveness {
            began: now,
            handshake,
            heard: None,
            connected: None,
            longest: Duration::ZERO,
            answer,
            ack_delay: Duration::ZERO,
            pto: Duration::ZERO,
            wait: None,
            sent: 0,
            acked: 0,
            ran_out: false,
            refused: false,
            flight: None,
            probes: 0,
            asked: None,
            answered: None,
        }
    }

    /// Its peer was heard at `now`: the silence since it last was, measured.
    pub fn heard(&mut self, now: Instant) {
        if let Some(last) = self.heard {
            self.longest = self.longest.max(now.saturating_duration_since(last));
        }
        self.heard = Some(now);
    }

    /// Its handshake ended at `now`, its transport having sent `sent` ack-eliciting frames and
    /// had `acked` bytes acknowledged by then: what the handshake sent is no send waiting (its
    /// frames are answered as it ends, so a wait for them would end with no answer, its quiet
    /// peer then taken for dead), and its time is no answer of a send's ([`Liveness`]). One that
    /// ends past its window and its probe timeout both ran out ([`Liveness::outran`]).
    pub fn connected(&mut self, now: Instant, sent: u64, acked: u64) {
        if self.outran(now) {
            self.ran_out = true;
        }
        self.connected = Some(now);
        self.sent = sent;
        self.acked = acked;
    }

    /// Its peer's own `max_ack_delay` (RFC 9000 §18.2), which each window adds.
    pub fn peer_ack_delay(&mut self, delay: Duration) {
        self.ack_delay = delay;
    }

    /// Its transport's probe timeout now (RFC 9002 §6.2.1: the smoothed RTT, four times its
    /// variation and the peer's `max_ack_delay`; hyper-quic's patch Q7): QUIC declares no loss
    /// sooner, so no wait is judged sooner either. A peer acknowledges application data up to
    /// its `max_ack_delay` late, and later as its timer fires late, where its handshake's
    /// packets it acknowledges at once (RFC 9000 §13.2.1): a window its handshake seeded alone
    /// would take that peer's first delayed acknowledgement for none. Nor is a handshake judged
    /// sooner ([`Liveness::due`]).
    pub fn probe_timeout(&mut self, pto: Duration) {
        self.pto = pto;
    }

    /// When its handshake runs out, unless it ends first ([`Liveness::handshake_ran_out`]),
    /// judged as a send is ([`Liveness::look`]): a whole window of the slowest handshake seen
    /// from its peer with nothing heard from that peer (its Initial and Handshake packets
    /// count), past the later of when it was last heard and its probe timeout (QUIC declares no
    /// loss sooner, RFC 9002 §6.2.1), however long it has gone on while it was heard. None once
    /// it has ended, or where its handshake has no window. A bound of the slowest handshake
    /// alone, from a path's quiet samples (one, after a join), refused a daemon reached before
    /// as the suite's own load slowed its answer: ROOT's runs at 084891178, tests/context.rs 8
    /// of 21 on HVF and 1 on KVM, "within the slowest handshake seen from it" of 0.93-4.2 ms.
    pub fn due(&self) -> Option<Instant> {
        if self.connected.is_some() {
            return None;
        }
        judged_from(self.heard, self.began, self.pto).checked_add(self.handshake?)
    }

    /// Whether its handshake, not ended by `now`, has gone past its window and its probe timeout
    /// both: a run-out, which its record keeps (`tls::Known::RanOut`), as its wait goes on while
    /// its peer is heard ([`Liveness::due`]).
    pub fn outran(&self, now: Instant) -> bool {
        self.connected.is_none()
            && self
                .handshake
                .is_some_and(|window| now.saturating_duration_since(self.began) > window.max(self.pto))
    }

    /// This side was absent for `gone`: its driver's thread neither ran nor waited as it had
    /// planned for that long (its process frozen or starved by its host, or descheduled), in a
    /// turn's midst or past its timer ([`Turn::absent`]). A wait of it is judged on the time this
    /// side was there to hear,
    /// so each of its instants moves on by that much, as though the absence had not been: its
    /// peer is not held to it, and what its record learns (an answer, a silence, a handshake's
    /// time) does not count it. kiln's runners froze whole processes so: a 2 ms sleep on
    /// adacachy2 took 124 ms at its p99, and one 121 ms freeze took six of `tests/loopback.rs`'s
    /// daemons' live clients for dead at once.
    pub fn absent(&mut self, gone: Duration) {
        let on = |at: Instant| at.checked_add(gone).unwrap_or(at);
        self.began = on(self.began);
        self.heard = self.heard.map(on);
        self.connected = self.connected.map(on);
        self.asked = self.asked.map(on);
        if let Some(wait) = self.wait.as_mut() {
            wait.since = on(wait.since);
        }
    }

    /// Its handshake ran out ([`Liveness::due`]): marked, and its window let go, so that it goes
    /// on as a peer never reached's does, with no deadline of its own (the person's ruling,
    /// 2026-11-13: every wait after a run-out follows the party's liveness and progress); the
    /// window it ran out of, none where it had none.
    pub fn handshake_ran_out(&mut self) -> Option<Duration> {
        self.ran_out = true;
        self.handshake.take()
    }

    /// Its peer refused it, definitely.
    pub fn refused(&mut self) {
        self.refused = true;
    }

    /// While its handshake has not ended and nothing of its peer's has been heard, `datagrams`
    /// sent in all: where more went than before, the datagrams its probes sent (RFC 9002 §6.2),
    /// past its first flight, which its first look finds and which is no probe, and how long it
    /// has waited. A peer heard is answering, however slowly: its handshake's window, where it
    /// has one, judges it ([`Liveness::due`]).
    pub fn unanswered(&mut self, now: Instant, datagrams: u64) -> Option<(u64, Duration)> {
        if self.connected.is_some() || self.heard.is_some() {
            return None;
        }
        let Some(flight) = self.flight else {
            self.flight = Some(datagrams);
            self.probes = datagrams;
            return None;
        };
        if datagrams <= self.probes {
            return None;
        }
        self.probes = datagrams;
        Some((datagrams - flight, now.saturating_duration_since(self.began)))
    }

    /// Its window: the slowest answer seen and the peer's `max_ack_delay`; none, cold, before
    /// any send's answer has been seen on its path.
    pub fn window(&self) -> Option<Duration> {
        (!self.answer.is_zero()).then(|| self.answer.saturating_add(self.ack_delay))
    }

    /// Looks at it at `now`, its transport's ack-eliciting frames sent and bytes acknowledged in
    /// all being `sent` and `acked`: an answer raises the slowest; a send with none waiting
    /// begins a wait; a wait past its window with progress is a run-out, told once; a whole
    /// window with no progress, a stall, told once, after which the wait has no window until its
    /// answer comes. Neither is judged before its probe timeout ([`Liveness::probe_timeout`]): a
    /// run-out is past the window and that timeout both, and a stall a whole window with no
    /// progress past the later of its last progress and that timeout.
    pub fn look(&mut self, now: Instant, sent: u64, acked: u64) -> Look {
        let Some(connected) = self.connected else {
            return Look::Fine;
        };
        self.note(now, sent, acked);
        if let Some(after) = self.answered.take() {
            return Look::Answered { after };
        }
        // Cold, a wait follows its peer's liveness and progress for as long as it takes.
        let Some(window) = self.window() else {
            return Look::Fine;
        };
        let stalled = self.stalled(now);
        let last = self.heard.map_or(connected, |heard| heard.max(connected));
        let Some(wait) = self.wait.as_mut() else {
            return Look::Fine;
        };
        match wait.stage {
            // Followed with no window, its answer to come.
            Stage::Stalled => Look::Fine,
            _ if stalled => {
                wait.stage = Stage::Stalled;
                self.ran_out = true;
                Look::Stalled {
                    unheard: now.saturating_duration_since(last),
                    window,
                }
            }
            Stage::Waiting if now.saturating_duration_since(wait.since) > window.max(self.pto) => {
                wait.stage = Stage::RanOut;
                self.ran_out = true;
                Look::RanOut {
                    waited: now.saturating_duration_since(wait.since),
                    window,
                }
            }
            Stage::Waiting | Stage::RanOut => Look::Fine,
        }
    }

    /// Whether a send of its has gone a whole window with no progress, past the later of its
    /// last progress and its probe timeout, by `now`: what a stall is ([`Liveness::look`]), told
    /// or not, judged afresh, so that a peer heard since its stall was told is not one. None
    /// cold, with no window.
    pub fn stalled(&self, now: Instant) -> bool {
        let (Some(wait), Some(window)) = (self.wait, self.window()) else {
            return false;
        };
        now.saturating_duration_since(judged_from(self.heard, wait.since, self.pto)) > window
    }

    /// Takes in what its transport has sent and had acknowledged by `now`, `sent` ack-eliciting
    /// frames and `acked` bytes in all: an answer raises the slowest and ends the wait it
    /// answers, a stalled one's too, whose answer takes in its stall; a send with none waiting
    /// begins one, as of `now`. Taken in as a send goes, before anything more is read (as its
    /// driver sends between reads, [`Driver::take_in`]): taken in after its answer had come, a
    /// send would begin a wait that answer had already ended, and its quiet peer, with nothing
    /// more to answer, would be taken for stalled.
    pub fn note(&mut self, now: Instant, sent: u64, acked: u64) {
        let Some(connected) = self.connected else {
            return;
        };
        if let Some(wait) = self.wait
            && acked > wait.acked
        {
            let took = now.saturating_duration_since(wait.since);
            self.answer = self.answer.max(took);
            if wait.stage == Stage::Stalled {
                self.answered = Some(took);
            }
            self.wait = None;
        }
        if sent > self.sent && self.wait.is_none() {
            let last = self.heard.map_or(connected, |heard| heard.max(connected));
            self.wait = Some(Wait {
                since: now,
                acked,
                quiet: now.saturating_duration_since(last),
                stage: Stage::Waiting,
            });
        }
        self.sent = sent;
        self.acked = acked;
    }

    /// When [`Liveness::look`] must look again for what waits to change: its window's end (its
    /// probe timeout's, where that is later), and a whole window past its last progress (or past
    /// its probe timeout); none once it stalled, its answer what changes it.
    pub fn next(&self) -> Option<Instant> {
        let wait = self.wait?;
        let window = self.window()?;
        let stall = judged_from(self.heard, wait.since, self.pto).checked_add(window)?;
        match wait.stage {
            Stage::Stalled => None,
            Stage::RanOut => Some(stall),
            Stage::Waiting => wait
                .since
                .checked_add(window.max(self.pto))
                .map(|out| out.min(stall)),
        }
    }

    /// A newcomer found its driver's places taken at `now` and asks it for an answer (its driver
    /// sends a PING): the ask it has to answer, unless an earlier one is still unanswered, which
    /// stands, so that newcomers dialing again put off no judgement of it.
    pub fn ask(&mut self, now: Instant) {
        if self.answered_ask() {
            self.asked = Some(now);
        }
    }

    /// Whether it has answered the last ask, or none was made: its peer heard since.
    fn answered_ask(&self) -> bool {
        self.asked
            .is_none_or(|asked| self.heard.is_some_and(|heard| heard > asked))
    }

    /// Whether a newcomer may take its place at `now` ([`Driver::reclaim`]): unheard since an
    /// earlier newcomer asked it for an answer, past its own probe timeout (RFC 9002 §6.2.1: its
    /// smoothed RTT, four times its variation and its peer's `max_ack_delay`, within which a
    /// live peer acknowledges the PING), warm or cold, stalled or not (ROOT's ruling on #72: a
    /// cold holder, whose path showed no answer, has no window and never stalls, and was never
    /// given up). One heard since the ask, however late, keeps it.
    pub fn unanswering(&self, now: Instant) -> bool {
        self.asked
            .is_some_and(|asked| !self.answered_ask() && now.saturating_duration_since(asked) > self.pto)
    }

    /// The silence after which a send of its stalled and is still unanswered, its path maybe
    /// lost (a middlebox's state gone, or its peer): none where no send waits stalled, a stall
    /// its answer ended being its peer's slowness, not the path's.
    pub fn lost(&self) -> Option<Duration> {
        self.wait
            .filter(|wait| wait.stage == Stage::Stalled)
            .map(|wait| wait.quiet)
    }

    /// When a keep-alive goes, where its path must be kept alive within `keep` of silence (the
    /// shortest silence after which one was found dead, or the peer's own idle timeout): early
    /// by the window, so that it lands; none while a send waits, or before the handshake ends.
    pub fn keep_alive(&self, keep: Duration) -> Option<Instant> {
        let connected = self.connected?;
        if self.wait.is_some() {
            return None;
        }
        let last = self.heard.map_or(connected, |heard| heard.max(connected));
        last.checked_add(keep.saturating_sub(self.window().unwrap_or(Duration::ZERO)))
    }

    /// The longest its peer went unheard, between two packets of its own.
    pub fn longest_silence(&self) -> Duration {
        self.longest
    }

    /// When its peer was last heard: none where it never was.
    pub fn last_heard(&self) -> Option<Instant> {
        self.heard
    }

    /// How long its handshake took, once it ended.
    pub fn handshake_took(&self) -> Option<Duration> {
        self.connected.map(|at| at.saturating_duration_since(self.began))
    }
}

/// What a connection saw of the path it crossed, for its caller's record: the longest its
/// peer went unheard and when it was last heard ([`Liveness`]), its round trip as last
/// estimated (RFC 9002 §5), the bytes it sent in UDP datagrams and the bytes of them its peer
/// acknowledged, the packets it took for lost, and the idle timeout it ran on (RFC 9000 §10.1:
/// none between two shards ends, which advertise none); and what its peer's record learns from
/// it (`tls::Known::after`): how long its handshake took, where it ended, the slowest answer it
/// waited for, the silence after which a send of its stalled and was never answered
/// ([`Liveness::lost`]), whether a wait of it ran out, and whether its peer refused it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    pub longest_silence: Duration,
    /// When its peer was last heard, none where it never was: the silence since is measured
    /// from it, whatever ended the connection.
    pub heard: Option<Instant>,
    pub rtt: Duration,
    pub sent_bytes: u64,
    pub acked_bytes: u64,
    pub lost_packets: u64,
    /// The idle timeout its transport ran on, as both ends negotiated it (hyper-quic's patch
    /// Q34): a peer's own, where it advertised one, bounds a stalled wait (D180).
    pub idle: Option<Duration>,
    /// Its transport's probe timeout as it ended (RFC 9002 §6.2.1, patch Q7's), which its waits
    /// were judged no sooner than.
    pub pto: Duration,
    pub handshake: Option<Duration>,
    pub answer: Duration,
    pub lost: Option<Duration>,
    pub ran_out: bool,
    pub refused: bool,
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
        let mut liveness = Liveness::new(self.now, self.handshake, self.answer);
        liveness.probe_timeout(quic.probe_timeout());
        Ok(place(
            self.conns,
            self.next,
            Conn {
                handle,
                quic,
                connected: false,
                liveness,
            },
        ))
    }

    /// What `conn` saw of its path so far.
    pub fn seen(&mut self, conn: ConnKey) -> Option<Seen> {
        let now = self.now;
        let conn = self.conn(conn).ok()?;
        let stats = conn.quic.stats();
        Some(Seen {
            longest_silence: conn.liveness.longest_silence(),
            heard: conn.liveness.last_heard(),
            rtt: stats.path.rtt,
            sent_bytes: stats.udp_tx.bytes,
            acked_bytes: stats.path.acked_bytes,
            lost_packets: stats.path.lost_packets,
            idle: conn.quic.idle_timeout(),
            pto: conn.quic.probe_timeout(),
            handshake: conn.liveness.handshake_took(),
            answer: conn.liveness.answer,
            lost: conn.liveness.lost(),
            ran_out: conn.liveness.ran_out || conn.liveness.outran(now),
            refused: conn.liveness.refused,
        })
    }

    /// Where `conn`'s peer is, as its packets last came from (RFC 9000 §9: a peer may move).
    pub fn peer(&mut self, conn: ConnKey) -> Option<SocketAddr> {
        self.conn(conn).ok().map(|c| c.quic.remote_address())
    }

    /// What `conn`'s own TLS handshake negotiated, once it has chosen its suite and its group
    /// (the server's hello, RFC 8446 §4.1.3, §4.2.8): none before.
    pub fn negotiated(&mut self, conn: ConnKey) -> Option<crate::tls::Negotiated> {
        let conn = self.conn(conn).ok()?;
        let data = conn.quic.crypto_session().handshake_data()?;
        let data = data
            .downcast::<hyper_quic::crypto::rustls::HandshakeData>()
            .ok()?;
        Some(crate::tls::Negotiated {
            suite: u16::from(data.negotiated_cipher_suite?),
            group: u16::from(data.negotiated_key_exchange_group?),
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
    /// The window each connection's handshake is judged by ([`Liveness::due`]): none, waited
    /// for as long as its probing goes on.
    handshake: Option<Duration>,
    /// The slowest answer its peer's path has shown, which each connection starts from.
    answer: Duration,
    /// The shortest silence after which a connection on its peer's path was found dead: where
    /// there is one, a connection is kept alive within it.
    lost: Option<Duration>,
    /// The one peer its socket is connected to, where it was aimed ([`Driver::aim`]): an ICMP
    /// port-unreachable then reads as that peer's refusal.
    aimed: Option<SocketAddr>,
    /// Its aimed peer refused, as a send was told ([`Driver::flush_datagrams`]).
    refusal: bool,
    /// Where its [`Sender`]s wake it, a socket of its own beside the one it serves on: a socket
    /// aimed at its peer takes datagrams from that peer alone (connect(2): Linux udp(7), XNU
    /// `udp_input`, Winsock's connect), so a wake sent there would never come.
    wake: UdpSocket,
    /// The handshakes it holds at once from peers not yet known, its owner's resources' bound
    /// ([`Driver::hold_handshakes`]): past it, the oldest unanswered is given up for the newest.
    slots: Option<usize>,
    /// The connections it holds at once, its owner's resources' bound: a bounded driver's
    /// admitted connections, or what [`Driver::hold_connections`] says. Past it a newcomer is
    /// refused, and takes the place of a holder that does not answer ([`Driver::reclaim`]).
    places: Option<usize>,
    out: VecDeque<(SocketAddr, Vec<u8>)>,
}

/// How a driver holds its connections: its handshakes' window, the slowest answer they start
/// from, and the silence they are kept alive within. Every established connection's liveness
/// is checked at use, its transport having no idle timer of its own.
#[derive(Debug, Clone, Copy)]
struct Held {
    handshake: Option<Duration>,
    answer: Duration,
    lost: Option<Duration>,
}

impl<H: Handler> Driver<H> {
    /// A driver on a socket bound to `at`, serving connections under `server` where it is
    /// given, each connection held as what is known of its peer says ([`crate::tls::Known`]; a
    /// daemon's, whose clients are not known before their handshakes, as of a peer never
    /// reached), its liveness checked at use ([`Liveness`]); and the [`Sender`] that reaches it.
    pub fn bind(
        at: SocketAddr,
        server: Option<ServerConfig>,
        handler: H,
        known: crate::tls::Known,
    ) -> Result<(Self, Sender<H::Command>), Error> {
        let path = known.path();
        let held = Held {
            handshake: known.handshake_window(),
            answer: path.map_or(Duration::ZERO, |p| p.answer),
            lost: path.and_then(|p| p.lost),
        };
        Self::bind_with_limits(at, server, handler, None, held)
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
        // Its peers are never reached before (a managed endpoint and its owner meet once): their
        // handshakes wait as long as their probing goes on, and their sends are judged by
        // liveness at use, as a context's are.
        let held = Held {
            handshake: None,
            answer: Duration::ZERO,
            lost: None,
        };
        Self::bind_with_limits(at, server, handler, Some(limits), held)
    }

    fn bind_with_limits(
        at: SocketAddr,
        server: Option<ServerConfig>,
        handler: H,
        limits: Option<DriverLimits>,
        held: Held,
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
        let wake = UdpSocket::bind(loopback).map_err(|e| Error::new(format!("the driver's wake: {e}")))?;
        wake.set_nonblocking(true)
            .map_err(|e| Error::new(format!("the driver's wake: {e}")))?;
        let to = wake
            .local_addr()
            .map_err(|e| Error::new(format!("the driver's wake: {e}")))?;
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
                handshake: held.handshake,
                answer: held.answer,
                lost: held.lost,
                aimed: None,
                refusal: false,
                wake,
                slots: None,
                places: limits.map(|limits| limits.connections),
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

    /// Connects its socket to `peer`, the one it talks to (a client's daemon), so that the
    /// peer's host refusing it with an ICMP port-unreachable (no QUIC endpoint listens there)
    /// reads as that, definitely: Linux and macOS say ECONNREFUSED on the next read of a
    /// connected UDP socket (udp(7); `udp_err` on BSD), Windows WSAECONNRESET.
    pub fn aim(&mut self, peer: SocketAddr) -> Result<(), Error> {
        self.socket
            .connect(peer)
            .map_err(|e| Error::new(format!("aiming the driver's socket at {peer}: {e}")))?;
        self.aimed = Some(peer);
        Ok(())
    }

    /// Holds `slots` handshakes at once from peers not yet known, the bound its owner's own
    /// resources set (the person's ruling, 2026-11-13: no deadline of shards' own): a handshake
    /// that comes past it takes the place of the oldest still unanswered, which is given up.
    pub fn hold_handshakes(&mut self, slots: usize) {
        self.slots = Some(slots);
    }

    /// Holds `places` connections at once, handshakes and established alike, the bound its
    /// owner's own resources set (ROOT's ruling on #72): a newcomer past it is refused, each
    /// established holder asked for an answer (a PING), and a later attempt takes the place of
    /// one that has not answered it within its probe timeout ([`Driver::reclaim`]).
    pub fn hold_connections(&mut self, places: usize) {
        self.places = Some(places);
    }

    /// The address the socket serves on.
    pub fn local(&self) -> Result<SocketAddr, Error> {
        self.socket
            .local_addr()
            .map_err(|e| Error::new(format!("the driver's socket: {e}")))
    }

    /// What the kernel holds for its sockets' queues: the one it serves on and its wake's.
    pub(crate) fn kernel_buffer_bytes(&self) -> Result<u64, Error> {
        socket_buffers(&self.socket)?
            .checked_add(socket_buffers(&self.wake)?)
            .ok_or_else(|| Error::new("the driver's kernel queues: UDP queue size overflow"))
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
        let mut last: Option<Turn> = None;
        loop {
            let now = Instant::now();
            let cpu = thread_cpu();
            // The time since the last turn began that this side neither ran nor waited as it
            // planned is its own absence, which no wait of it is judged on ([`Liveness::absent`]),
            // before anything is judged this turn.
            if let Some(gone) = last
                .take()
                .map(|turn| turn.absent(now, cpu))
                .filter(|g| !g.is_zero())
            {
                for conn in self.conns.values_mut() {
                    conn.liveness.absent(gone);
                }
            }
            let mut turn = Turn {
                at: now,
                cpu,
                blocked: Duration::ZERO,
                late: Duration::ZERO,
            };
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
            if std::mem::take(&mut self.refusal) {
                self.refused(Instant::now());
            }
            // After what this turn sent: a send's wait begins as it goes, and what the watch
            // queues (a keep-alive) goes on the next turn, which its due time brings at once.
            self.watch(now);
            if (self.stopping || self.handler.done()) && self.out.is_empty() {
                return Ok(());
            }
            let next = match (self.next_timer(), self.handler.due()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            // Until its next timer, in the whole milliseconds poll(2) waits; with none due, until
            // a datagram or a wake comes, which every command and stop sends ([`Sender`]) to its
            // wake socket ([`Driver::wake`]).
            let looked = Instant::now();
            let wait = next.map(|at| whole_ms(at.saturating_duration_since(looked)));
            let planned = wait.filter(|w| !w.is_zero()).and_then(|w| looked.checked_add(w));
            let ready = readable(&self.socket, &self.wake, wait);
            let back = Instant::now();
            // Waited as planned until its timer, or until what came; past its timer, absent.
            turn.blocked = planned
                .map_or(back, |at| back.min(at))
                .saturating_duration_since(looked);
            turn.late = planned
                .and_then(|at| back.checked_duration_since(at))
                .unwrap_or_default();
            last = Some(turn);
            let (served, woken) = match ready {
                Ok(ready) => ready,
                // A signal interrupts the wait (Linux signal(7)): another turn checks commands
                // and timers without losing any of the connections.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::new(format!("the driver's sockets: {e}"))),
            };
            if woken {
                // Every wake that came, and the commands they tell of served before what came on
                // the socket meanwhile, as each was sent before it (one socket's order before).
                while self.wake.recv_from(&mut datagram).is_ok() {}
                self.commands_now(Instant::now());
            }
            // A socket aimed at its peer is read whatever its wait said, so that its peer's
            // refusal is read as it comes where the wait does not tell an error (WSAPoll).
            if !served && self.aimed.is_none() {
                continue;
            }
            // What is there read without waiting: the socket non-blocking while it is read. A
            // receive timeout of the least, a microsecond, Linux keeps in whole jiffies
            // (SO_RCVTIMEO, net/core/sock.c `sock_set_timeout`), so that an aimed socket with
            // nothing to read waited a whole one out each turn, a wait none of the driver's timers
            // planned ([`Turn::absent`]).
            self.socket
                .set_nonblocking(true)
                .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
            let taken = self.take_in(&mut datagram);
            self.socket
                .set_nonblocking(false)
                .map_err(|e| Error::new(format!("the driver's socket: {e}")))?;
            taken?;
        }
    }

    /// Takes in what its socket holds, its socket non-blocking: each datagram as it is read,
    /// within its admitted queue, before a turn sends anything; or its aimed peer's refusal.
    fn take_in(&mut self, datagram: &mut [u8]) -> Result<(), Error> {
        match self.socket.recv_from(datagram) {
            Ok((n, from)) => {
                if from == self.waker {
                    return Ok(());
                }
                let now = Instant::now();
                let bytes = BytesMut::from(datagram.get(..n).unwrap_or_default());
                let mut fed = self.receive(now, from, bytes)?;
                // Whatever else came meanwhile, each heard as it is read, and what a connection it
                // fed has to send sent before more is read: its acknowledgement above all, which
                // its peer awaits at once from its second ack-eliciting packet (RFC 9000
                // §13.2.2), within its max_ack_delay at the latest (§13.2.1). Drained whole before
                // any was sent, a backlog at a peer's full rate held every acknowledgement back
                // and heard each datagram as of the drain's start, its silence as long as the
                // drain (moved.rs's live move on HVF: an import's connection taken for dead
                // mid-stream).
                let turns = self
                    .limits
                    .map_or(usize::MAX, |l| l.queued_datagram_bytes / DATAGRAM);
                let mut buffer = Vec::new();
                for _ in 1..turns {
                    if let Some(key) = fed.take() {
                        self.transmit_conn(Instant::now(), key, &mut buffer)?;
                        self.flush_datagrams();
                        // What it sent, taken in before what answers it is read.
                        if let Some(conn) = self.conns.get_mut(&key)
                            && conn.connected
                        {
                            let stats = conn.quic.stats();
                            conn.liveness.note(
                                Instant::now(),
                                eliciting(&stats.frame_tx),
                                stats.path.acked_bytes,
                            );
                        }
                    }
                    let Ok((n, from)) = self.socket.recv_from(datagram) else {
                        break;
                    };
                    if from != self.waker {
                        let bytes = BytesMut::from(datagram.get(..n).unwrap_or_default());
                        fed = self.receive(Instant::now(), from, bytes)?;
                    }
                }
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(())
            }
            // A signal interrupts a receive (Linux signal(7)): another turn checks commands
            // and timers without losing any of the connections.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
            // An ICMP port-unreachable: on a socket aimed at its one peer, that peer's
            // definite refusal (ECONNREFUSED; Windows' WSAECONNRESET); on another, a
            // datagram of one peer's of many refused, which its connection's own probing
            // deals with.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                if self.aimed.is_some() {
                    self.refused(Instant::now());
                }
                Ok(())
            }
            Err(e) => Err(Error::new(format!("the driver's socket: {e}"))),
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
                handshake: self.handshake,
                answer: self.answer,
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

    /// Feeds `bytes` from `from` to its connection, or to a new one: the connection fed, if any.
    fn receive(&mut self, now: Instant, from: SocketAddr, bytes: BytesMut) -> Result<Option<ConnKey>, Error> {
        let mut response = Vec::new();
        match self.endpoint.handle(now, from, None, None, bytes, &mut response) {
            Some(DatagramEvent::NewConnection(incoming)) => {
                if let Some(places) = self.places
                    && !self.stopping
                {
                    self.free_closed(now, places)?;
                    if self.conns.len() >= places && self.reclaim(now) {
                        self.free_closed(now, places)?;
                    }
                }
                if self.stopping || self.places.is_some_and(|places| self.conns.len() >= places) {
                    // Its places still taken, each established connection holding one is asked
                    // for an answer (a PING): one whose peer does not answer it within its probe
                    // timeout gives its place to a newcomer's later attempt ([`Driver::reclaim`]);
                    // one that answers, however late, keeps it. This one is refused.
                    if !self.stopping {
                        for conn in self.conns.values_mut() {
                            if conn.connected && !conn.quic.is_closed() {
                                conn.liveness.ask(now);
                                conn.quic.ping();
                            }
                        }
                    }
                    if let Some(transmit) = self.endpoint.refuse(incoming, &mut response) {
                        self.queue_datagram(
                            transmit.destination,
                            response.get(..transmit.size).unwrap_or_default(),
                        )?;
                    }
                    return Ok(None);
                }
                self.make_room(now);
                match self.endpoint.accept(incoming, now, &mut response, None, None) {
                    Ok((handle, quic)) => {
                        let mut liveness = Liveness::new(now, self.handshake, self.answer);
                        liveness.probe_timeout(quic.probe_timeout());
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
                        return Ok(Some(key));
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
                    return Ok(Some(key));
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
        Ok(None)
    }

    fn key_of(&self, handle: ConnectionHandle) -> Option<ConnKey> {
        self.conns
            .iter()
            .find_map(|(key, c)| (c.handle == handle).then_some(*key))
    }

    /// A newcomer finds its `places` full: a place held by a closed connection is freed for it.
    /// Closed (closing or draining, RFC 9000 §10.2), a connection carries nothing again, its
    /// state kept three probe timeouts only to absorb its peer's last packets: the one nearest
    /// its drain is let go at once, as §10.2 lets an endpoint end those states early "to allow
    /// for faster resource recovery", at the cost §10.2 names, a late packet of its peer's
    /// answered by a stateless reset (§10.3), which ends the connection for that peer as its
    /// close would. What it has yet to send, its CONNECTION_CLOSE (a datagram at most), goes
    /// with this turn's datagrams, so none is freed while the socket holds back an earlier one.
    /// A connection open, its handshake's or established, keeps its place.
    fn free_closed(&mut self, now: Instant, places: usize) -> Result<(), Error> {
        let mut buffer = Vec::new();
        while self.conns.len() >= places && self.out.is_empty() {
            let Some(key) = self
                .conns
                .iter_mut()
                .filter_map(|(key, c)| c.quic.is_closed().then(|| (*key, c.quic.poll_timeout())))
                .min_by_key(|(_, drains)| *drains)
                .map(|(key, _)| key)
            else {
                return Ok(());
            };
            buffer.clear();
            if let Some(conn) = self.conns.get_mut(&key)
                && let Some(transmit) = conn
                    .quic
                    .poll_transmit(now, 1, &mut buffer, self.endpoint.configs())
            {
                self.queue_datagram(
                    transmit.destination,
                    buffer.get(..transmit.size).unwrap_or_default(),
                )?;
            }
            // Its last events, then its endpoint told it has drained.
            self.pump(now, key);
            if let Some(conn) = self.conns.remove(&key) {
                self.endpoint.handle_event(conn.handle, EndpointEvent::drained());
            }
        }
        Ok(())
    }

    /// A newcomer finds every place taken: of the established connections holding them, the one
    /// unheard longest of those that have not answered, within their probe timeout, the PING an
    /// earlier newcomer sent them ([`Liveness::unanswering`]) is let go for it, its handler told
    /// as QUIC tells a peer unheard past its bound (RFC 9000 §10.1), for a handler that types its
    /// ends; whether one was. A stall alone ends no connection (ROOT's ruling A): one is let go
    /// only as a newcomer needs its place, so that a driver's resources stay its owner's bound
    /// ([`Driver::hold_connections`]), and a holder that answers, however late, keeps its place.
    fn reclaim(&mut self, now: Instant) -> bool {
        let Some(key) = self
            .conns
            .iter()
            .filter(|(_, c)| c.connected && !c.quic.is_closed() && c.liveness.unanswering(now))
            .min_by_key(|(_, c)| c.liveness.last_heard())
            .map(|(key, _)| *key)
        else {
            return false;
        };
        let (mut io, handler) = self.io(now);
        io.close(key, 0, "unanswered past its probe timeout, its place needed");
        handler.cause(key, &ConnectionError::TimedOut);
        handler.lost(
            &mut io,
            key,
            "no answer to a newcomer's PING within its probe timeout, its place needed by the newcomer",
            None,
        );
        self.client = io.client;
        true
    }

    /// A handshake comes: where the handshakes held fill their slots, the oldest still
    /// unanswered is given up ([`given_up`]), its handler told.
    fn make_room(&mut self, now: Instant) {
        let Some(slots) = self.slots else {
            return;
        };
        let held: Vec<(ConnKey, Instant)> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.connected && !c.quic.is_closed())
            .map(|(key, c)| (*key, c.liveness.began))
            .collect();
        if let Some(key) = given_up(&held, slots) {
            let (mut io, handler) = self.io(now);
            io.close(key, 0, "a newer handshake took its place");
            handler.lost(&mut io, key, "its handshake gave its place to a newer one", None);
            self.client = io.client;
        }
    }

    /// The aimed peer refused: each connection not closed yet ends so, its handler told once.
    fn refused(&mut self, now: Instant) {
        self.keys.clear();
        self.keys.extend(self.conns.keys());
        for at in 0..self.keys.len() {
            let Some(&key) = self.keys.get(at) else {
                break;
            };
            match self.conns.get_mut(&key) {
                Some(conn) if !conn.quic.is_closed() => conn.liveness.refused(),
                _ => continue,
            }
            let (mut io, handler) = self.io(now);
            io.close(key, 0, "refused there");
            handler.lost(
                &mut io,
                key,
                "refused: no QUIC endpoint listens there (ICMP port unreachable)",
                None,
            );
            self.client = io.client;
        }
    }

    /// Each connection's liveness: a handshake's probes told; an established connection's,
    /// where the driver checks it, looked at (a run-out told, a stall told, neither ending it)
    /// and kept alive where its path or its peer's idle timer has shown it must be.
    fn watch(&mut self, now: Instant) {
        self.keys.clear();
        self.keys.extend(self.conns.keys());
        for at in 0..self.keys.len() {
            let Some(&key) = self.keys.get(at) else {
                break;
            };
            let Some(conn) = self.conns.get_mut(&key) else {
                continue;
            };
            if conn.quic.is_closed() {
                continue;
            }
            // Its handshake's too, whose window is judged no sooner ([`Liveness::due`]).
            conn.liveness.probe_timeout(conn.quic.probe_timeout());
            let stats = conn.quic.stats();
            if let Some((probes, waited)) = conn.liveness.unanswered(now, stats.udp_tx.datagrams) {
                let (mut io, handler) = self.io(now);
                handler.unanswered(&mut io, key, probes, waited);
                self.client = io.client;
                continue;
            }
            if !conn.connected {
                continue;
            }
            let look = conn
                .liveness
                .look(now, eliciting(&stats.frame_tx), stats.path.acked_bytes);
            let told = match look {
                Look::Fine => None,
                Look::RanOut { waited, window } => Some(RanOut::Send { waited, window }),
                Look::Stalled { unheard, window } => Some(RanOut::Stalled { unheard, window }),
                Look::Answered { after } => Some(RanOut::Answered { after }),
            };
            if let Some(what) = told {
                let (mut io, handler) = self.io(now);
                handler.ran_out(&mut io, key, what);
                self.client = io.client;
            }
            let Some(conn) = self.conns.get_mut(&key) else {
                continue;
            };
            if let Some(keep) = keep_within(self.lost, conn.quic.idle_timeout())
                && conn.liveness.keep_alive(keep).is_some_and(|at| at <= now)
            {
                conn.quic.ping();
            }
        }
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
                    let stats = conn.quic.stats();
                    conn.liveness
                        .connected(now, eliciting(&stats.frame_tx), stats.path.acked_bytes);
                    conn.liveness.peer_ack_delay(conn.quic.peer_max_ack_delay());
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

    /// Each connection whose handshake has not ended by its due time ([`Liveness::due`]) has run
    /// out: marked, its handler told once ([`RanOut::Handshake`]), and its handshake goes on as a
    /// peer never reached's does, with no deadline of its own, each probe told, until it ends or
    /// its handler gives up (the person's ruling, 2026-11-13: every wait after a run-out follows
    /// the party's liveness and progress). Given up there, a daemon its loaded host descheduled
    /// past its probe timeout and slowest handshake was asked "is it listening there" (ROOT's run
    /// of `tests/loopback.rs` on adas-macbook-pro-2 at ea6f4b53b, load1 about 44).
    fn expire(&mut self, now: Instant) {
        self.keys.clear();
        self.keys.extend(
            self.conns
                .iter()
                .filter(|(_, c)| !c.quic.is_closed() && c.liveness.due().is_some_and(|at| at <= now))
                .map(|(key, _)| *key),
        );
        for at in 0..self.keys.len() {
            let Some(&key) = self.keys.get(at) else {
                break;
            };
            let Some(conn) = self.conns.get_mut(&key) else {
                continue;
            };
            let waited = now.saturating_duration_since(conn.liveness.began);
            let Some(window) = conn.liveness.handshake_ran_out() else {
                continue;
            };
            let (mut io, handler) = self.io(now);
            handler.ran_out(&mut io, key, RanOut::Handshake { waited, window });
            self.client = io.client;
        }
    }

    fn next_timer(&mut self) -> Option<Instant> {
        let lost = self.lost;
        self.conns
            .values_mut()
            .filter_map(|c| {
                if c.quic.is_closed() {
                    return c.quic.poll_timeout();
                }
                let handshake = c.liveness.due();
                let (liveness, keep) = match c.connected {
                    true => (
                        c.liveness.next(),
                        keep_within(lost, c.quic.idle_timeout()).and_then(|k| c.liveness.keep_alive(k)),
                    ),
                    false => (None, None),
                };
                [c.quic.poll_timeout(), handshake, liveness, keep]
                    .into_iter()
                    .flatten()
                    .min()
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
            // A connected socket takes no address of its own peer (macOS refuses one, EISCONN).
            let sent = match self.aimed {
                Some(aimed) if aimed == *to => self.socket.send(bytes),
                _ => self.socket.send_to(bytes, *to),
            };
            match sent {
                Ok(_) => {
                    self.out.pop_front();
                }
                // The socket's buffer is full: the rest goes on the next turn.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // A socket aimed at its peer is told of that peer's refusal (an ICMP port
                // unreachable) by its next send as by its next read, whichever comes first: Linux
                // gives a connected socket's pending error, and clears it, at either (udp(7):
                // ECONNREFUSED, "caused by a previous packet sent over the socket"), as Winsock's
                // send does (WSAECONNRESET, "a previous send operation resulted in an ICMP Port
                // Unreachable message"). On Linux's loopback the refusal of a flight's first
                // datagram comes back before its second is sent, and a client whose second send
                // took it, its read finding none, waited on forever (ROOT's KVM run at 237f3ebe7,
                // tests/loopback.rs's closed port 22 minutes).
                Err(e)
                    if self.aimed.is_some()
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                        ) =>
                {
                    self.out.pop_front();
                    self.refusal = true;
                }
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
            self.transmit_conn(now, key, &mut buffer)?;
        }
        self.flush_datagrams();
        Ok(())
    }

    /// Queues what connection `key` has to send, a bounded driver sending each as its socket
    /// takes it.
    fn transmit_conn(&mut self, now: Instant, key: ConnKey, buffer: &mut Vec<u8>) -> Result<(), Error> {
        loop {
            buffer.clear();
            let Some(conn) = self.conns.get_mut(&key) else {
                break;
            };
            let Some(transmit) = conn.quic.poll_transmit(now, 1, buffer, self.endpoint.configs()) else {
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
        Ok(())
    }
}

/// Of the handshakes `held` (each connection and when it began), the one a newer gives its
/// place to where they fill `slots`: the oldest, unanswered longest; none while a slot is free.
fn given_up(held: &[(ConnKey, Instant)], slots: usize) -> Option<ConnKey> {
    if held.len() < slots {
        return None;
    }
    held.iter().min_by_key(|(_, began)| *began).map(|(key, _)| *key)
}

/// The ack-eliciting frames a connection sent (RFC 9002 §2: every frame but ACK, PADDING and
/// CONNECTION_CLOSE), as its transport counts them.
fn eliciting(f: &hyper_quic::FrameStats) -> u64 {
    [
        f.ping,
        f.reset_stream,
        f.stop_sending,
        f.crypto,
        f.datagram,
        f.new_token,
        f.max_data,
        f.max_stream_data,
        f.max_streams_bidi,
        f.max_streams_uni,
        f.data_blocked,
        f.stream_data_blocked,
        f.streams_blocked_bidi,
        f.streams_blocked_uni,
        f.new_connection_id,
        f.retire_connection_id,
        f.path_challenge,
        f.path_response,
        f.stream,
        f.ack_frequency,
        f.immediate_ack,
        u64::from(f.handshake_done),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
}

/// Where a wait begun `since` counts its silence from: its peer last `heard` after it began, or
/// its probe timeout `pto` past its start, whichever is later.
fn judged_from(heard: Option<Instant>, since: Instant, pto: Duration) -> Instant {
    let progress = heard.map_or(since, |heard| heard.max(since));
    progress.max(since.checked_add(pto).unwrap_or(since))
}

/// A driver's turn, as it was had ([`Liveness::absent`]): when it began, the CPU time its thread
/// had run by then, how long its wait blocked as planned (until its timer, or until what came),
/// and how far past its timer it woke.
#[derive(Debug, Clone, Copy)]
struct Turn {
    at: Instant,
    cpu: Option<Duration>,
    blocked: Duration,
    late: Duration,
}

impl Turn {
    /// How long, from this turn's start to the next's at `now` (its thread having run `cpu` by
    /// then), its side was absent: the time its thread neither ran nor waited as planned, where
    /// the host keeps a thread's CPU time ([`thread_cpu`]), which takes in a freeze in a turn's
    /// midst as well as past its timer; else how far past its timer its wait woke.
    fn absent(&self, now: Instant, cpu: Option<Duration>) -> Duration {
        match (self.cpu, cpu) {
            (Some(was), Some(is)) => now
                .saturating_duration_since(self.at)
                .saturating_sub(is.saturating_sub(was))
                .saturating_sub(self.blocked),
            _ => self.late,
        }
    }
}

/// The CPU time the calling thread has run, where the host keeps one finer than its scheduler's
/// tick: POSIX's thread CPU-time clock (clock_gettime(2), `CLOCK_THREAD_CPUTIME_ID`).
#[cfg(unix)]
fn thread_cpu() -> Option<Duration> {
    let mut at = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) writes the timespec it is given, which lives through the call.
    let read = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut at) };
    if read != 0 {
        return None;
    }
    Some(Duration::new(
        u64::try_from(at.tv_sec).ok()?,
        u32::try_from(at.tv_nsec).ok()?,
    ))
}

/// None on Windows: a thread's times there move at the clock interrupt (GetThreadTimes), coarser
/// than a turn, so a driver's absence is how far past its timer its wait woke alone.
#[cfg(windows)]
fn thread_cpu() -> Option<Duration> {
    None
}

/// `wait`, rounded up to the whole milliseconds poll(2) and WSAPoll wait ([`readable`]).
fn whole_ms(wait: Duration) -> Duration {
    Duration::from_millis(u64::try_from(wait.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX))
}

/// Whether `socket` and `wake` have something to read (a datagram, or `socket`'s error: its
/// peer's ICMP refusal), waiting up to `wait` for either, none: until one does. poll(2) waits
/// whole milliseconds, so a wait under one is one, and a timer is looked at no sooner than it
/// is due.
#[cfg(unix)]
fn readable(socket: &UdpSocket, wake: &UdpSocket, wait: Option<Duration>) -> std::io::Result<(bool, bool)> {
    use std::os::fd::AsRawFd as _;
    let fd = |s: &UdpSocket| libc::pollfd {
        fd: s.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut fds = [fd(socket), fd(wake)];
    let ms = wait.map_or(-1, |w| {
        libc::c_int::try_from(w.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)
    });
    // SAFETY: poll(2) over this driver's two sockets, open while it runs, the array's length
    // its count.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let [served, woken] = fds.map(|f| f.revents != 0);
    Ok((served, woken))
}

/// [`readable`] on Windows: WSAPoll's, its milliseconds as poll(2)'s.
#[cfg(windows)]
fn readable(socket: &UdpSocket, wake: &UdpSocket, wait: Option<Duration>) -> std::io::Result<(bool, bool)> {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{POLLRDNORM, WSAGetLastError, WSAPOLLFD, WSAPoll};
    let fd = |s: &UdpSocket| WSAPOLLFD {
        fd: s.as_raw_socket() as usize,
        events: POLLRDNORM,
        revents: 0,
    };
    let mut fds = [fd(socket), fd(wake)];
    let ms = wait.map_or(-1, |w| {
        i32::try_from(w.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
    });
    // SAFETY: WSAPoll over this driver's two sockets, open while it runs, the array's length its
    // count.
    let n = unsafe { WSAPoll(fds.as_mut_ptr(), 2, ms) };
    if n < 0 {
        // SAFETY: retrieves this thread's Winsock failure.
        return Err(std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    let [served, woken] = fds.map(|f| f.revents != 0);
    Ok((served, woken))
}

/// The silence a connection is kept alive within: the shortest after which one on its path was
/// found dead (`lost`), and the idle timeout its peer negotiated (RFC 9000 §10.1, hyper-quic's
/// patch Q34), whichever is shorter; none where neither is, and no keep-alive goes.
fn keep_within(lost: Option<Duration>, idle: Option<Duration>) -> Option<Duration> {
    match (lost, idle) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A wait on the driver's sockets is not restarted after a signal, whatever SA_RESTART says
    /// (poll(2), Linux signal(7)). A harmless host signal must leave the driver serving
    /// commands.
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

        let (mut driver, sender) = Driver::bind(
            "127.0.0.1:0".parse().unwrap(),
            None,
            Waiting,
            crate::tls::Known::Never,
        )
        .unwrap();
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
            // Its wait: poll(2) of its sockets, ppoll's where the target has no poll.
            #[cfg(target_arch = "x86_64")]
            let waiting = [libc::SYS_poll, libc::SYS_ppoll];
            #[cfg(not(target_arch = "x86_64"))]
            let waiting = [libc::SYS_ppoll];
            if number.is_some_and(|n| waiting.contains(&n)) {
                break;
            }
            assert!(!worker.is_finished(), "the driver ended before its receive");
            std::thread::yield_now();
        }
        // SAFETY: the borrowed pthread identity belongs to worker, still owned and not
        // joined. It is live, blocked in its sockets' poll; its handler records delivery,
        // which ends that wait.
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A handshake is judged by its window, the slowest seen from its peer
    /// (`a_handshake_runs_out_a_whole_window_unheard_past_its_probe_timeout`), or by none, as
    /// long as its probing goes on, each new probe past its first flight told once with how long
    /// it has waited, and none once its peer is heard; its end measures its first answer, and
    /// nothing of the handshake's holds it after. The longest silence a connection lived through
    /// is the one measured, its peer last heard at its last packet.
    #[test]
    fn a_handshake_is_held_to_its_window_or_probes_on_and_its_silences_are_measured() {
        let t0 = Instant::now();
        let mut held = Liveness::new(t0, Some(ms(40)), Duration::ZERO);
        assert_eq!(held.due(), t0.checked_add(ms(40)));
        assert_eq!(held.unanswered(t0, 2), None, "its first flight is no probe");
        assert_eq!(held.unanswered(t0 + ms(1), 3), Some((1, ms(1))));
        assert_eq!(held.unanswered(t0 + ms(2), 3), None, "no new probe");
        assert_eq!(held.unanswered(t0 + ms(3), 5), Some((3, ms(3))));
        held.heard(t0 + ms(4));
        assert_eq!(held.unanswered(t0 + ms(5), 6), None, "its peer answers");
        let mut l = Liveness::new(t0, None, Duration::ZERO);
        assert_eq!(
            l.due(),
            None,
            "a peer never reached is waited for as long as it probes"
        );
        assert_eq!(l.last_heard(), None, "nothing heard yet");
        l.heard(t0 + ms(30));
        assert_eq!(l.last_heard(), Some(t0 + ms(30)));
        l.connected(t0 + ms(30), 0, 0);
        assert_eq!((l.due(), l.handshake_took()), (None, Some(ms(30))));
        assert_eq!(l.unanswered(t0 + ms(31), 9), None, "its handshake ended");
        assert_eq!(l.window(), None, "its handshake is no answer of a send's");
        l.peer_ack_delay(ms(25));
        assert_eq!(l.window(), None);
        // An hour of a transfer, its peer heard every second but for one gap of 42 s: it lives,
        // and the gap is what it says.
        let mut at = t0 + ms(30);
        for s in 1..3600u64 {
            at += if s == 1800 {
                Duration::from_secs(42)
            } else {
                Duration::from_secs(1)
            };
            l.heard(at);
        }
        assert_eq!(l.longest_silence(), Duration::from_secs(42));
        assert_eq!(l.last_heard(), Some(at));
        assert_eq!(l.due(), None);
    }

    /// A handshake with a peer reached before is judged as a send is: a whole window of the
    /// slowest handshake seen from that peer with nothing heard from it, past the later of its
    /// last packet heard and its probe timeout, before which QUIC declares no loss (RFC 9002
    /// §6.2.1); one heard lives on past both, its run-out kept, and one that ends within them
    /// ran out not, however much slower than the slowest. Red with the slowest handshake its
    /// deadline from its start: ROOT's runs of tests/context.rs at 084891178 (8 of 21 on HVF, 1
    /// on KVM), a daemon reached before given up "within the slowest handshake seen from it" of
    /// 0.93-4.2 ms under the suite's own load. One unheard past them runs out, marked, and goes
    /// on with no window, as a peer never reached's does, its probes told: red with its window
    /// kept, its run-out its end ("is it listening there", ROOT's run of tests/loopback.rs on
    /// adas-macbook-pro-2 at ea6f4b53b, load1 about 44).
    #[test]
    fn a_handshake_runs_out_a_whole_window_unheard_past_its_probe_timeout() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, Some(ms(2)), Duration::ZERO);
        l.probe_timeout(ms(26));
        assert_eq!(
            l.due(),
            t0.checked_add(ms(28)),
            "no sooner than its probe timeout"
        );
        assert!(!l.outran(t0 + ms(26)));
        assert!(
            l.outran(t0 + ms(27)),
            "past its window and its probe timeout both"
        );
        l.heard(t0 + ms(27));
        assert_eq!(
            l.due(),
            t0.checked_add(ms(29)),
            "a whole window past its last packet"
        );
        l.heard(t0 + ms(28));
        l.connected(t0 + ms(29), 0, 0);
        assert_eq!(
            (l.due(), l.ran_out),
            (None, true),
            "it lived on, its run-out kept"
        );
        let mut slower = Liveness::new(t0, Some(ms(2)), Duration::ZERO);
        slower.probe_timeout(ms(26));
        slower.heard(t0 + ms(14));
        slower.connected(t0 + ms(14), 0, 0);
        assert_eq!(
            (slower.handshake_took(), slower.ran_out),
            (Some(ms(14)), false),
            "seven times the slowest, within its probe timeout"
        );
        // Unheard past its window and probe timeout: run out, marked, and on with no window.
        let mut unheard = Liveness::new(t0, Some(ms(2)), Duration::ZERO);
        unheard.probe_timeout(ms(26));
        assert_eq!(unheard.unanswered(t0, 2), None, "its first flight");
        assert_eq!(unheard.due(), t0.checked_add(ms(28)));
        assert_eq!(
            unheard.handshake_ran_out(),
            Some(ms(2)),
            "the window it ran out of"
        );
        assert_eq!(
            (unheard.due(), unheard.ran_out),
            (None, true),
            "no deadline, marked"
        );
        assert_eq!(
            unheard.unanswered(t0 + ms(60), 4),
            Some((2, ms(60))),
            "its probes told on"
        );
        unheard.heard(t0 + ms(200));
        unheard.connected(t0 + ms(200), 0, 0);
        assert_eq!(
            (unheard.handshake_took(), unheard.ran_out),
            (Some(ms(200)), true),
            "answered once heard, its run-out kept"
        );
    }

    /// A send taken in as it goes (`Liveness::note`), before what answers it is read, is
    /// answered by that answer, its quiet peer living on. Taken in only after its answer was
    /// read, it began a wait that answer had already ended, and its peer, with nothing more to
    /// answer, was taken for dead a window on: what failed `tests/loopback.rs` (2 to 3 of 26 on
    /// adacachy2, 7 on macOS) as its driver first sent between its socket's reads.
    #[test]
    fn a_send_taken_in_before_its_answer_is_read_is_answered_by_it() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, ms(2));
        l.heard(t0);
        l.connected(t0, 0, 0);
        l.peer_ack_delay(ms(5));
        l.probe_timeout(ms(10));
        assert_eq!(l.look(t0 + ms(1), 1, 0), Look::Fine, "a first send waits");
        l.note(t0 + ms(2), 2, 0);
        l.heard(t0 + ms(3));
        assert_eq!(l.look(t0 + ms(3), 2, 200), Look::Fine, "both answered at once");
        assert_eq!(l.wait, None, "nothing waits");
        assert_eq!(l.look(t0 + ms(100), 2, 200), Look::Fine, "its quiet peer lives");
    }

    /// A wait is judged on the time its side was there to hear (`Liveness::absent`): its driver
    /// absent for 200 ms, woken that much past the time it planned to look again (kiln's runners
    /// froze whole processes for up to 124 ms), a send waiting through it is not dead as its
    /// driver resumes, and its answer and silence do not count the absence; a handshake's window
    /// moves on by it too. Red with no absence taken: the send dead as its driver resumes.
    #[test]
    fn a_wait_is_judged_on_the_time_its_side_was_there_to_hear() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, ms(2));
        l.heard(t0);
        l.connected(t0, 0, 0);
        l.peer_ack_delay(ms(25));
        l.probe_timeout(ms(26));
        assert_eq!(
            l.look(t0 + ms(1), 1, 0),
            Look::Fine,
            "a send waits, its window 27 ms"
        );
        l.absent(ms(200));
        assert_eq!(
            l.look(t0 + ms(203), 1, 0),
            Look::Fine,
            "not dead as its driver resumes"
        );
        l.heard(t0 + ms(205));
        assert_eq!(l.look(t0 + ms(205), 1, 100), Look::Fine);
        assert_eq!(
            (l.answer, l.longest_silence()),
            (ms(4), ms(5)),
            "its answer and its silence less the 200 ms its side was absent"
        );
        let mut h = Liveness::new(t0, Some(ms(2)), Duration::ZERO);
        h.probe_timeout(ms(26));
        h.absent(ms(200));
        assert_eq!(h.due(), t0.checked_add(ms(228)));
    }

    /// Liveness at use (the person's ruling, 2026-11-13, and ROOT's ruling A): a send waits for
    /// the peer to acknowledge any of it, within the slowest answer seen and the peer's
    /// max_ack_delay; an answer raises the slowest; past the window, a wait whose peer is still
    /// heard is told once as a run-out and goes on; a whole window with nothing heard is a stall,
    /// told once with how long its peer went unheard, and followed with no window: its answer,
    /// however late, ends it and is the slowest seen, so that the window covers such a stall
    /// from then on. Red with a stall the connection's death (`Look::Dead` before): a client
    /// frozen 200 ms mid-import taken for dead by its daemon (tests/loopback.rs).
    #[test]
    fn a_send_waits_its_window_runs_out_with_progress_and_stalls_without() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, Duration::ZERO);
        assert_eq!(
            l.look(t0, 1, 0),
            Look::Fine,
            "nothing waits before its handshake ends"
        );
        l.heard(t0 + ms(10));
        l.connected(t0 + ms(10), 0, 0);
        l.peer_ack_delay(ms(5));
        assert_eq!(l.window(), None, "cold");
        assert_eq!(l.look(t0 + ms(10), 0, 0), Look::Fine);
        // A send at 20 ms, its answer 12 ms on: the first and slowest, 12 ms; the window 17 ms.
        assert_eq!(l.look(t0 + ms(20), 1, 0), Look::Fine);
        assert_eq!(l.next(), None, "cold: nothing to judge it by");
        l.heard(t0 + ms(32));
        assert_eq!(l.look(t0 + ms(32), 1, 100), Look::Fine);
        assert_eq!((l.window(), l.next()), (Some(ms(17)), None));
        // A send after 88 ms of silence; its peer heard at 10 ms without an answer.
        assert_eq!(l.look(t0 + ms(120), 2, 100), Look::Fine);
        l.heard(t0 + ms(130));
        assert_eq!(
            l.look(t0 + ms(140), 2, 100),
            Look::RanOut {
                waited: ms(20),
                window: ms(17)
            }
        );
        assert_eq!(l.look(t0 + ms(141), 2, 100), Look::Fine, "a run-out is told once");
        assert_eq!(l.next(), Some(t0 + ms(147)));
        assert_eq!(l.look(t0 + ms(146), 2, 100), Look::Fine);
        assert_eq!(
            l.look(t0 + ms(148), 2, 100),
            Look::Stalled {
                unheard: ms(18),
                window: ms(17)
            }
        );
        assert_eq!(
            l.look(t0 + ms(10_000), 2, 100),
            Look::Fine,
            "a stall is told once, and followed with no window"
        );
        assert_eq!(l.next(), None, "its answer is what changes it");
        assert_eq!((l.lost(), l.ran_out), (Some(ms(88)), true));
        // Its answer, 10 s after the send: the stall ends, told once, and the slowest answer
        // covers it.
        l.heard(t0 + ms(10_120));
        assert_eq!(
            l.look(t0 + ms(10_120), 2, 200),
            Look::Answered { after: ms(10_000) }
        );
        assert_eq!(l.look(t0 + ms(10_121), 2, 200), Look::Fine);
        let seen = (l.answer, l.window(), l.lost(), l.ran_out);
        assert_eq!(seen, (ms(10_000), Some(ms(10_005)), None, true));
        // As long a silence again is within its window.
        assert_eq!(l.look(t0 + ms(10_200), 3, 200), Look::Fine);
        assert_eq!(l.look(t0 + ms(20_000), 3, 200), Look::Fine);
        assert_eq!(l.next(), Some(t0 + ms(20_205)));
    }

    /// A stall is judged afresh as a newcomer needs a place (`Liveness::stalled`): a send
    /// unanswered a whole window past its probe timeout is stalled, told or not; one whose peer
    /// was heard since is not, whatever was told, until a whole window passes unheard again;
    /// cold, with no window, none is. A holder gives its place to a newcomer once it has not
    /// answered, within its probe timeout, the PING an earlier newcomer sent it
    /// (`Liveness::unanswering`), stalled or not, cold too; a later newcomer's ask puts off no
    /// judgement of an earlier one still unanswered, and one answered is asked again.
    #[test]
    fn a_stalled_holder_unheard_since_a_newcomer_asked_gives_way() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, ms(2));
        l.heard(t0);
        l.connected(t0, 0, 0);
        l.peer_ack_delay(ms(5));
        l.probe_timeout(ms(10));
        assert!(!l.stalled(t0 + ms(100)), "nothing waits");
        assert_eq!(l.look(t0 + ms(1), 1, 0), Look::Fine);
        // Its window 7 ms, judged from its probe timeout, 11 ms in.
        assert!(!l.stalled(t0 + ms(18)));
        assert!(
            l.stalled(t0 + ms(19)),
            "a whole window unanswered past its probe timeout"
        );
        assert!(
            !l.unanswering(t0 + ms(19)),
            "stalled, but no newcomer asked it yet"
        );
        assert!(matches!(l.look(t0 + ms(19), 1, 0), Look::Stalled { .. }));
        l.ask(t0 + ms(20));
        assert!(!l.unanswering(t0 + ms(21)), "asked within its probe timeout");
        assert!(
            l.unanswering(t0 + ms(31)),
            "unheard past its probe timeout since asked"
        );
        l.ask(t0 + ms(25));
        assert_eq!(l.asked, Some(t0 + ms(20)), "the ask unanswered stands");
        l.heard(t0 + ms(30));
        assert!(
            !l.stalled(t0 + ms(31)),
            "its peer heard since: answering, however late"
        );
        assert!(!l.unanswering(t0 + ms(38)), "it answered the ask");
        assert!(l.stalled(t0 + ms(38)), "and unheard a whole window again");
        l.ask(t0 + ms(40));
        assert_eq!(l.asked, Some(t0 + ms(40)), "asked again");
        assert!(!l.unanswering(t0 + ms(41)));
        assert!(l.unanswering(t0 + ms(51)));
        let mut cold = Liveness::new(t0, None, Duration::ZERO);
        cold.heard(t0);
        cold.connected(t0, 0, 0);
        cold.peer_ack_delay(ms(5));
        assert_eq!(cold.look(t0, 1, 0), Look::Fine);
        cold.ask(t0);
        assert!(!cold.stalled(t0 + ms(100_000)), "cold: no window to judge by");
        assert!(
            cold.unanswering(t0 + ms(100_000)),
            "cold, unheard since asked: judged by its liveness all the same"
        );
    }

    /// What its handshake sent, answered as it ends, is no send waiting: a wait begun for it
    /// would never see an answer, and its peer, quiet while its command runs, would be taken
    /// for dead a window on (what failed 10 of tests/loopback.rs's 19 before). A send after it
    /// waits for its own answer.
    #[test]
    fn what_its_handshake_sent_is_no_send_waiting() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, Duration::ZERO);
        l.heard(t0 + ms(10));
        l.connected(t0 + ms(10), 4, 1200);
        l.peer_ack_delay(ms(5));
        assert_eq!(l.look(t0 + ms(11), 4, 1200), Look::Fine);
        assert_eq!(l.next(), None, "nothing waits");
        assert_eq!(l.look(t0 + ms(10_000), 4, 1200), Look::Fine, "a quiet peer lives");
        assert_eq!(l.look(t0 + ms(10_001), 5, 1200), Look::Fine);
        assert_eq!(l.next(), None, "cold, it follows its peer's liveness");
    }

    /// Cold, before any send's answer has been seen on its path, a wait has no window: its
    /// peer's first answer may take far longer than its handshake did (a join's record synced to
    /// disk, its host's scheduling), and it is waited for as long as it takes, nothing told; that
    /// answer is the slowest seen, and sets the window from then on. Red with the handshake
    /// seeding the window: tests/context.rs's joins died so, 123-152 ms after a fresh client's
    /// handshake (ROOT's HVF run at dc7f45bc6).
    #[test]
    fn a_cold_wait_follows_its_peer_however_long_its_first_answer_takes() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, Duration::ZERO);
        l.heard(t0 + ms(1));
        l.connected(t0 + ms(1), 0, 0);
        l.peer_ack_delay(ms(25));
        l.probe_timeout(ms(28));
        assert_eq!(l.window(), None);
        assert_eq!(l.look(t0 + ms(1), 1, 0), Look::Fine);
        assert_eq!(l.next(), None, "no window to look again at");
        assert_eq!(l.look(t0 + ms(10_001), 1, 0), Look::Fine, "10 s unanswered, cold");
        l.heard(t0 + ms(10_001));
        assert_eq!(l.look(t0 + ms(10_001), 1, 100), Look::Fine);
        assert_eq!(l.window(), Some(ms(10_025)));
    }

    /// A wait is judged no sooner than QUIC's loss recovery judges its send (its probe timeout,
    /// RFC 9002 §6.2.1): a peer's acknowledgement delayed by its max_ack_delay and a late timer,
    /// past a window its path's record seeded (a 2 ms answer) but within the probe timeout, has
    /// not run out, and is the slowest answer from then on; a send nothing answers runs out past
    /// its window and stalls a whole window past its probe timeout. Red before: 10 of tests/loopback.rs's 19 died on a
    /// send 28.9 ms unanswered, their window 26.9 ms (a handshake of 1.9 ms and 25 ms of
    /// max_ack_delay), their probe timeout 30.7 ms.
    #[test]
    fn a_wait_is_judged_no_sooner_than_its_probe_timeout() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, ms(2));
        l.heard(t0 + ms(2));
        l.connected(t0 + ms(2), 0, 0);
        l.peer_ack_delay(ms(25));
        l.probe_timeout(ms(31));
        assert_eq!(l.window(), Some(ms(27)));
        assert_eq!(l.look(t0 + ms(2), 1, 0), Look::Fine);
        assert_eq!(l.next(), Some(t0 + ms(33)), "judged at its probe timeout");
        assert_eq!(
            l.look(t0 + ms(30), 1, 0),
            Look::Fine,
            "past its window, within its timeout"
        );
        l.heard(t0 + ms(31));
        assert_eq!(l.look(t0 + ms(31), 1, 100), Look::Fine);
        assert_eq!(l.window(), Some(ms(54)));
        assert_eq!(l.look(t0 + ms(40), 2, 100), Look::Fine);
        assert_eq!(l.next(), Some(t0 + ms(94)));
        assert_eq!(
            l.look(t0 + ms(95), 2, 100),
            Look::RanOut {
                waited: ms(55),
                window: ms(54)
            }
        );
        assert_eq!(l.next(), Some(t0 + ms(125)));
        assert_eq!(l.look(t0 + ms(125), 2, 100), Look::Fine);
        assert_eq!(
            l.look(t0 + ms(126), 2, 100),
            Look::Stalled {
                unheard: ms(95),
                window: ms(54)
            }
        );
        assert_eq!(l.lost(), Some(ms(9)), "the silence before the send that stalled");
    }

    /// A keep-alive goes before the silence its path is kept within, early by the window so
    /// that it lands; none while a send waits; and it is kept within the shortest silence found
    /// dead and the peer's own idle timeout, whichever is shorter, or none.
    #[test]
    fn a_keep_alive_goes_early_by_the_window_and_none_while_a_send_waits() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, None, ms(10));
        assert_eq!(l.keep_alive(ms(100)), None, "none before its handshake ends");
        l.heard(t0 + ms(4));
        l.connected(t0 + ms(4), 0, 0);
        l.peer_ack_delay(ms(5));
        assert_eq!(l.window(), Some(ms(15)));
        assert_eq!(l.keep_alive(ms(100)), Some(t0 + ms(89)));
        assert_eq!(
            l.keep_alive(ms(9)),
            Some(t0 + ms(4)),
            "at once, where the window is longer"
        );
        assert_eq!(l.look(t0 + ms(50), 1, 0), Look::Fine);
        assert_eq!(l.keep_alive(ms(100)), None, "a send waits");
        assert_eq!(keep_within(Some(ms(90_000)), Some(ms(10_000))), Some(ms(10_000)));
        assert_eq!(keep_within(None, Some(ms(10_000))), Some(ms(10_000)));
        assert_eq!(keep_within(Some(ms(90_000)), None), Some(ms(90_000)));
        assert_eq!(keep_within(None, None), None);
    }

    /// What a test's party heard of its connections.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Heard {
        Connected(ConnKey),
        Bytes(ConnKey),
        Stalled(ConnKey),
        Lost(ConnKey, String),
        Frozen,
    }

    /// What a test asks of a party's driver.
    enum Ask {
        Connect(SocketAddr, Box<ClientConfig>),
        /// Holds its driver's thread, as a process stopped or descheduled is held, until let go.
        Freeze(mpsc::Receiver<()>),
        /// A byte on a stream of its own, which its peer hears.
        Say(ConnKey),
        /// Ends its driver without a word to its peers, as a killed process's ends.
        Vanish,
        /// Ends its driver as its next connection's handshake ends, once what ends it is sent:
        /// nothing of its endpoint's is answered after, as of an owner gone at its handshake.
        VanishAtConnect,
    }

    /// A bounded endpoint or a client of one, on the real transport: it tells what its driver
    /// tells it.
    struct Party {
        heard: mpsc::Sender<Heard>,
        vanished: bool,
        vanish_at_connect: bool,
    }

    impl Handler for Party {
        type Command = Ask;
        fn connected(&mut self, _: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
            self.vanished |= self.vanish_at_connect;
            let _ = self.heard.send(Heard::Connected(conn));
            Ok(())
        }
        fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
            self.readable(io, conn, stream);
        }
        fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
            let mut any = false;
            while let Read::Bytes(bytes) | Read::End(bytes) = io.read(conn, stream) {
                if bytes.is_empty() {
                    break;
                }
                any = true;
            }
            if any {
                let _ = self.heard.send(Heard::Bytes(conn));
            }
        }
        fn writable(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
        fn lost(&mut self, _: &mut Io<'_>, conn: ConnKey, why: &str, _: Option<u64>) {
            let _ = self.heard.send(Heard::Lost(conn, why.to_string()));
        }
        fn ran_out(&mut self, _: &mut Io<'_>, conn: ConnKey, what: RanOut) {
            if let RanOut::Stalled { .. } = what {
                let _ = self.heard.send(Heard::Stalled(conn));
            }
        }
        fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
            match ask {
                Ask::Connect(to, config) => {
                    if let Err(e) = io.connect(to, *config) {
                        let _ = self.heard.send(Heard::Lost(ConnKey::MAX, e.to_string()));
                    }
                }
                Ask::Freeze(until) => {
                    let _ = self.heard.send(Heard::Frozen);
                    let _ = until.recv();
                }
                Ask::Say(conn) => {
                    let stream = io.open(conn).unwrap();
                    assert_eq!(io.write(conn, stream, b"x").unwrap(), 1);
                    io.finish(conn, stream).unwrap();
                }
                Ask::Vanish => self.vanished = true,
                Ask::VanishAtConnect => self.vanish_at_connect = true,
            }
        }
        fn done(&self) -> bool {
            self.vanished
        }
    }

    /// A party's driver as bound, and what reaches it.
    type Bound = Result<(Driver<Party>, Sender<Ask>), Error>;

    /// A party running: what reaches it, what it hears, where it is, and its driver's thread.
    type Running = (
        Sender<Ask>,
        mpsc::Receiver<Heard>,
        SocketAddr,
        std::thread::JoinHandle<Result<(), Error>>,
    );

    /// A party on a driver and a thread of its own.
    fn party(bound: impl FnOnce(Party) -> Bound) -> Running {
        let (heard, hears) = mpsc::channel();
        let (mut driver, asks) = bound(Party {
            heard,
            vanished: false,
            vanish_at_connect: false,
        })
        .unwrap();
        let at = driver.local().unwrap();
        let running = std::thread::spawn(move || driver.run());
        (asks, hears, at, running)
    }

    /// The next of what `hears` hears that `want` takes, the rest of it told if it never comes.
    fn next_heard(hears: &mpsc::Receiver<Heard>, want: impl Fn(&Heard) -> bool) -> Heard {
        let patience = Duration::from_secs(60);
        let mut other = Vec::new();
        loop {
            match hears.recv_timeout(patience) {
                Ok(heard) if want(&heard) => return heard,
                Ok(heard) => other.push(heard),
                Err(_) => panic!("nothing wanted within {patience:?}; heard {other:?}"),
            }
        }
    }

    /// A bounded endpoint's one place, held by an established connection (step 5's output and
    /// input endpoints and D238's agent-control endpoint share this driver): a newcomer that
    /// finds it taken is refused and the holder asked for an answer (a PING). A holder frozen
    /// past its window stalls, told once, and keeps its place: let go, it answers, and the next
    /// newcomer is refused again. A holder gone without a word stalls the same, and the next
    /// newcomer takes its place (ROOT's ruling A, its third condition: the reclaim on contention
    /// bounds the endpoint's resources). Red with a stall the holder's death: the frozen holder
    /// let go at its stall, before any newcomer needed its place.
    #[test]
    fn a_stalled_holder_gives_its_place_to_a_newcomer_and_one_that_answers_keeps_it() {
        let server = crate::identity::Identity::generate().unwrap();
        let config = |of: &crate::identity::Identity| {
            Box::new(crate::tls::client(of, server.fingerprint(), crate::tls::Known::Never).unwrap())
        };
        let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let tls = crate::tls::server(&server).unwrap();
        let limits = DriverLimits {
            connections: 1,
            command_slots: 8,
            queued_datagram_bytes: DATAGRAM * 8,
        };
        let (endpoint, at_endpoint, address, serving) =
            party(|p| Driver::bind_bounded(any, Some(tls), p, limits));
        let bind = |p: Party| Driver::bind(any, None, p, crate::tls::Known::Never);
        let (holder, at_holder, _, holding) = party(bind);
        let (newcomer, at_newcomer, _, _coming) = party(bind);
        let (holder_id, newcomer_id) = (
            crate::identity::Identity::generate().unwrap(),
            crate::identity::Identity::generate().unwrap(),
        );
        let connected = |h: &Heard| matches!(h, Heard::Connected(_));
        holder
            .send(Command::Handler(Ask::Connect(address, config(&holder_id))))
            .unwrap();
        let Heard::Connected(held) = next_heard(&at_endpoint, connected) else {
            panic!("not connected")
        };
        let Heard::Connected(mine) = next_heard(&at_holder, connected) else {
            panic!("not connected")
        };
        // Heard by its endpoint, which has an answer of the holder's by then, its window.
        holder.send(Command::Handler(Ask::Say(mine))).unwrap();
        assert_eq!(next_heard(&at_endpoint, |_| true), Heard::Bytes(held));

        // Frozen: a newcomer is refused, and the holder, asked for an answer, stalls.
        let (thaw, frozen) = mpsc::channel();
        holder.send(Command::Handler(Ask::Freeze(frozen))).unwrap();
        assert_eq!(next_heard(&at_holder, |_| true), Heard::Frozen);
        let lost = |h: &Heard| matches!(h, Heard::Lost(..));
        newcomer
            .send(Command::Handler(Ask::Connect(address, config(&newcomer_id))))
            .unwrap();
        let refused = next_heard(&at_newcomer, |h| lost(h) || connected(h));
        assert!(matches!(refused, Heard::Lost(..)), "{refused:?}");
        assert_eq!(
            next_heard(&at_endpoint, |_| true),
            Heard::Stalled(held),
            "its holder stalls, and keeps its place"
        );
        // Let go, it answers; the next newcomer is refused, its place kept.
        thaw.send(()).unwrap();
        holder.send(Command::Handler(Ask::Say(mine))).unwrap();
        assert_eq!(next_heard(&at_endpoint, |_| true), Heard::Bytes(held));
        newcomer
            .send(Command::Handler(Ask::Connect(address, config(&newcomer_id))))
            .unwrap();
        let refused = next_heard(&at_newcomer, |h| lost(h) || connected(h));
        assert!(matches!(refused, Heard::Lost(..)), "{refused:?}");
        holder.send(Command::Handler(Ask::Say(mine))).unwrap();
        assert_eq!(
            next_heard(&at_endpoint, |_| true),
            Heard::Bytes(held),
            "a holder that answered keeps its place"
        );

        // Gone without a word: the next newcomer finds it stalled, and takes its place.
        holder.send(Command::Handler(Ask::Vanish)).unwrap();
        holding.join().unwrap().unwrap();
        newcomer
            .send(Command::Handler(Ask::Connect(address, config(&newcomer_id))))
            .unwrap();
        let refused = next_heard(&at_newcomer, |h| lost(h) || connected(h));
        assert!(matches!(refused, Heard::Lost(..)), "{refused:?}");
        assert_eq!(next_heard(&at_endpoint, |_| true), Heard::Stalled(held));
        newcomer
            .send(Command::Handler(Ask::Connect(address, config(&newcomer_id))))
            .unwrap();
        let taken = next_heard(&at_newcomer, |h| lost(h) || connected(h));
        assert!(matches!(taken, Heard::Connected(_)), "{taken:?}");
        let Heard::Lost(gone, _) = next_heard(&at_endpoint, |_| true) else {
            panic!("its holder was not let go");
        };
        assert_eq!(gone, held);
        assert!(matches!(next_heard(&at_endpoint, |_| true), Heard::Connected(_)));
        endpoint.send(Command::Stop).unwrap();
        serving.join().unwrap().unwrap();
        newcomer.send(Command::Handler(Ask::Vanish)).unwrap();
    }

    /// A holder gone as its handshake ended, nothing of its endpoint's answered after (an owner
    /// gone before the handshake's last frames were acknowledged), is cold: its path showed no
    /// answer, so it has no window and never stalls. Under contention a holder is judged by its
    /// liveness (ROOT's ruling on #72): asked for an answer (a PING), it gives its place to a
    /// later attempt once unheard past its probe timeout. Red with a stall the only way to
    /// give it up: the newcomer refused at every attempt.
    #[test]
    fn a_cold_holder_unheard_past_its_probe_timeout_gives_its_place_to_a_newcomer() {
        let server = crate::identity::Identity::generate().unwrap();
        let config = |of: &crate::identity::Identity| {
            Box::new(crate::tls::client(of, server.fingerprint(), crate::tls::Known::Never).unwrap())
        };
        let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let tls = crate::tls::server(&server).unwrap();
        let limits = DriverLimits {
            connections: 1,
            command_slots: 8,
            queued_datagram_bytes: DATAGRAM * 8,
        };
        let (endpoint, at_endpoint, address, serving) =
            party(|p| Driver::bind_bounded(any, Some(tls), p, limits));
        let bind = |p: Party| Driver::bind(any, None, p, crate::tls::Known::Never);
        let (holder, _at_holder, _, holding) = party(bind);
        let (newcomer, at_newcomer, _, _coming) = party(bind);
        let (holder_id, newcomer_id) = (
            crate::identity::Identity::generate().unwrap(),
            crate::identity::Identity::generate().unwrap(),
        );
        holder.send(Command::Handler(Ask::VanishAtConnect)).unwrap();
        holder
            .send(Command::Handler(Ask::Connect(address, config(&holder_id))))
            .unwrap();
        let Heard::Connected(held) = next_heard(&at_endpoint, |h| matches!(h, Heard::Connected(_))) else {
            panic!("not connected")
        };
        holding.join().unwrap().unwrap();
        let patience = Duration::from_secs(20);
        let until = Instant::now() + patience;
        let mut attempts = 0;
        loop {
            attempts += 1;
            newcomer
                .send(Command::Handler(Ask::Connect(address, config(&newcomer_id))))
                .unwrap();
            match next_heard(&at_newcomer, |h| {
                matches!(h, Heard::Lost(..) | Heard::Connected(_))
            }) {
                Heard::Connected(_) => break,
                refused => assert!(
                    Instant::now() < until,
                    "refused {attempts} times over {patience:?}: {refused:?}"
                ),
            }
        }
        assert!(attempts >= 2, "its first attempt asks the holder");
        let Heard::Lost(gone, _) = next_heard(&at_endpoint, |h| matches!(h, Heard::Lost(..))) else {
            panic!("its holder was not let go");
        };
        assert_eq!(gone, held);
        endpoint.send(Command::Stop).unwrap();
        serving.join().unwrap().unwrap();
        newcomer.send(Command::Handler(Ask::Vanish)).unwrap();
    }

    /// A bounded endpoint's one place, held by a handshake in flight (a client whose server's
    /// flight never reached it): a handshake holding a place is judged as any holder is (ROOT's
    /// ruling on #72's handshakes). While its client answers each newcomer's ask, it keeps its
    /// place, over three of the probe timeouts its endpoint has before any RTT sample (RFC 9002
    /// §6.2.2: the initial RTT's 333 ms, 999 ms; the client acknowledges nothing, so none comes);
    /// once its client is silent, a newcomer takes its place, as that handshake goes unheard past
    /// its probe timeout since the newcomer asked. Red with only established holders asked: the
    /// newcomer refused at every attempt.
    #[test]
    fn a_handshake_unheard_past_its_probe_timeout_gives_its_place_and_one_heard_keeps_it() {
        let server = crate::identity::Identity::generate().unwrap();
        let config = |of: &crate::identity::Identity| {
            crate::tls::client(of, server.fingerprint(), crate::tls::Known::Never).unwrap()
        };
        let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let tls = crate::tls::server(&server).unwrap();
        let limits = DriverLimits {
            connections: 1,
            command_slots: 8,
            queued_datagram_bytes: DATAGRAM * 8,
        };
        let (endpoint, at_endpoint, address, serving) =
            party(|p| Driver::bind_bounded(any, Some(tls), p, limits));
        let (newcomer, at_newcomer, _, _coming) =
            party(|p| Driver::bind(any, None, p, crate::tls::Known::Never));
        let (holder_id, newcomer_id) = (
            crate::identity::Identity::generate().unwrap(),
            crate::identity::Identity::generate().unwrap(),
        );

        // The holder: a client endpoint driven by hand, which reads nothing of its server's, so
        // that its handshake never ends. It answers as this test says with its first datagram
        // again, which its endpoint routes to its handshake (by the Initial's destination
        // connection ID) and hears, as it would a resend or a network's duplicate; a PING of its
        // own, never acknowledged, its congestion window soon holds back.
        let socket = UdpSocket::bind(any).unwrap();
        let mut client = Endpoint::new(EndpointConfig::default(), None, true, None).unwrap();
        let handle = client.insert_client_config(config(&holder_id)).unwrap();
        let (_, mut quic) = client
            .connect(Instant::now(), handle, address, crate::tls::SERVER_NAME, None)
            .unwrap();
        let mut first = Vec::new();
        let sent = quic
            .poll_transmit(Instant::now(), 1, &mut first, client.configs())
            .unwrap();
        first.truncate(sent.size);
        socket.send_to(&first, address).unwrap();
        // Its endpoint took it, and answered with its flight.
        socket.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let mut datagram = vec![0; DATAGRAM];
        socket.recv_from(&mut datagram).unwrap();

        let attempt = || {
            newcomer
                .send(Command::Handler(Ask::Connect(
                    address,
                    Box::new(config(&newcomer_id)),
                )))
                .unwrap();
            next_heard(&at_newcomer, |h| {
                matches!(h, Heard::Lost(..) | Heard::Connected(_))
            })
        };
        // Heard after each ask: it keeps its place.
        let answering = Instant::now() + Duration::from_secs(3);
        let mut attempts = 0;
        while Instant::now() < answering {
            attempts += 1;
            let refused = attempt();
            assert!(
                matches!(refused, Heard::Lost(..)),
                "attempt {attempts}: a handshake heard after the ask gave its place: {refused:?}"
            );
            socket.send_to(&first, address).unwrap();
        }
        assert!(
            at_endpoint.try_recv().is_err(),
            "the endpoint let a handshake go that answered"
        );

        // Silent: the next newcomer takes its place.
        let patience = Duration::from_secs(20);
        let until = Instant::now() + patience;
        let mut attempts = 0;
        loop {
            attempts += 1;
            match attempt() {
                Heard::Connected(_) => break,
                refused => assert!(
                    Instant::now() < until,
                    "refused {attempts} times over {patience:?}: {refused:?}"
                ),
            }
        }
        assert!(attempts >= 2, "its first attempt asks the holder");
        assert!(
            matches!(next_heard(&at_endpoint, |_| true), Heard::Lost(..)),
            "its handshake was let go first"
        );
        assert!(matches!(next_heard(&at_endpoint, |_| true), Heard::Connected(_)));
        endpoint.send(Command::Stop).unwrap();
        serving.join().unwrap().unwrap();
        newcomer.send(Command::Handler(Ask::Vanish)).unwrap();
    }

    /// A handshake asked for an answer is judged by its own spaces' probe timeout, which counts
    /// no `max_ack_delay` (RFC 9002 §6.2.1; its transport's application-data one, less its peer's
    /// `max_ack_delay`): unheard past that since the ask, it gives way; heard since, it keeps its
    /// place. Established, the application-data one judges it.
    #[test]
    fn a_handshake_asked_is_judged_by_its_handshakes_probe_timeout() {
        let t0 = Instant::now();
        let mut shaking = Liveness::new(t0, None, Duration::ZERO);
        shaking.heard(t0);
        shaking.peer_ack_delay(ms(25));
        // Before any RTT sample: the initial RTT's 999 ms, and its peer's 25 ms.
        shaking.probe_timeout(ms(1024));
        shaking.ask(t0 + ms(1));
        assert!(!shaking.unanswering(t0 + ms(1000)), "within its handshake's");
        assert!(
            shaking.unanswering(t0 + ms(1001)),
            "past its handshake's, though within the application data's"
        );
        shaking.heard(t0 + ms(1002));
        assert!(!shaking.unanswering(t0 + ms(5000)), "heard since the ask");
        let mut established = shaking;
        established.connected(t0 + ms(1003), 0, 0);
        established.ask(t0 + ms(1004));
        assert!(!established.unanswering(t0 + ms(2028)));
        assert!(established.unanswering(t0 + ms(2029)));
    }

    /// Under contention a holder is judged by its liveness (ROOT's ruling on #72): asked for an
    /// answer, one unheard past its own probe timeout gives way, warm or cold, stalled or not;
    /// one heard since the ask keeps its place, within its probe timeout or later.
    #[test]
    fn a_holder_asked_gives_way_unheard_past_its_probe_timeout_warm_or_cold() {
        let t0 = Instant::now();
        // Cold: no answer seen on its path, so no window and no stall.
        let mut cold = Liveness::new(t0, None, Duration::ZERO);
        cold.heard(t0);
        cold.connected(t0, 0, 0);
        cold.peer_ack_delay(ms(5));
        cold.probe_timeout(ms(10));
        assert_eq!(cold.look(t0, 1, 0), Look::Fine);
        assert!(!cold.unanswering(t0 + ms(100_000)), "never asked");
        cold.ask(t0 + ms(1));
        assert!(!cold.stalled(t0 + ms(100_000)), "cold: no window");
        assert!(!cold.unanswering(t0 + ms(11)), "within its probe timeout");
        assert!(cold.unanswering(t0 + ms(12)), "unheard past it");
        // Warm, within its window: not stalled, but unheard past its probe timeout.
        let mut warm = Liveness::new(t0, None, ms(50));
        warm.heard(t0);
        warm.connected(t0, 0, 0);
        warm.peer_ack_delay(ms(5));
        warm.probe_timeout(ms(10));
        warm.ask(t0 + ms(1));
        assert!(!warm.stalled(t0 + ms(12)));
        assert!(warm.unanswering(t0 + ms(12)));
        // Heard within its probe timeout: kept, however long after.
        warm.heard(t0 + ms(5));
        assert!(!warm.unanswering(t0 + ms(1000)));
        // Asked again, heard past its probe timeout but before it was judged: kept.
        warm.ask(t0 + ms(20));
        assert_eq!(warm.asked, Some(t0 + ms(20)));
        warm.ask(t0 + ms(25));
        assert_eq!(warm.asked, Some(t0 + ms(20)), "the ask unanswered stands");
        warm.heard(t0 + ms(40));
        assert!(!warm.unanswering(t0 + ms(1000)));
    }

    /// A handshake past the slots its driver's owner holds takes the place of the oldest still
    /// unanswered; while a slot is free, none is given up.
    #[test]
    fn a_handshake_past_its_slots_takes_the_oldest_unanswered_place() {
        let t0 = Instant::now();
        let held = [(4, t0 + ms(30)), (2, t0 + ms(10)), (7, t0 + ms(20))];
        assert_eq!(given_up(&held, 4), None);
        assert_eq!(given_up(&held, 3), Some(2));
        assert_eq!(given_up(&held[..1], 1), Some(4));
        assert_eq!(given_up(&[], 0), None);
    }
}
