//! The stream connections the drivers send a run's log over (D163 to D165): TCP, TLS or a
//! unix socket. A message is delivered once its receiver has it and has not ended the
//! connection on it: its receiver's TCP has acknowledged its last byte (PM M233), or, on a
//! unix socket, it is written, which puts it in its receiver's buffer; and the connection
//! is still open a while after, the kernel's RTO (RFC 6298), within which a receiver that
//! ends connections on a message it will not take is seen to. Until then it is held, and
//! written again, after those held before it, on the next connection should this one end
//! first: a receiver that restarts, or a connection reset, costs a message sent twice,
//! never one lost. A connection is ended only once nothing is held: so a receiver's end
//! after this one's is its answer to it, and one before it, the run's last message among
//! those held, is followed as any other is (`Stream::suspect`). Then TLS's close_notify is
//! sent, its write side shut, what the receiver still sends (TLS 1.3's session tickets, its
//! own close_notify) read and let go, and its socket closed once all is acknowledged. A
//! socket closed with bytes unread resets its connection, and what was written on it and
//! not yet sent goes with it (PM M233): srslog, go-gelf and fluent-logger-golang close
//! theirs so, and lose that tail.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shards_vmm::platform::UnixStream;

use super::Wait;

/// The least retransmission timeout (RFC 6298 §2.4), waited where a kernel gives none while
/// bytes are unacknowledged, and the while a message a unix socket's receiver has is held
/// for its receiver's end, a unix socket having no round trip of its own to wait.
const LEAST_RTO: Duration = Duration::from_secs(1);

/// Where a stream goes.
pub(super) enum To {
    Tcp(String),
    /// Its `HOST:PORT`, and how TLS is spoken there.
    Tls(String, Arc<rustls::ClientConfig>),
    Unix(PathBuf),
}

pub(super) struct Stream {
    to: To,
    /// Each connection's write and read timeouts, where given.
    timeouts: [Option<Duration>; 2],
    conn: Option<Conn>,
    /// The messages written on this connection or one before it that their receiver may
    /// not have, oldest first.
    held: VecDeque<Held>,
    /// How many connections in a row ended, by their receiver or a write that failed, with
    /// messages held and none of them delivered meanwhile ([`Stream::suspect`]).
    ended: u32,
    /// How long after its receiver has a message the connection is to stay open for it to
    /// be delivered: its kernel's RTO as it was last settled, or [`LEAST_RTO`].
    window: Duration,
    /// Whether its receiver had sent what is not yet read as it was last settled.
    sent: bool,
}

/// A message held: its bytes, its connection's bytes once it was written, and when its
/// receiver was first seen to have it.
struct Held {
    message: Vec<u8>,
    at: u64,
    had: Option<Instant>,
}

impl Held {
    fn new(message: Vec<u8>, at: u64) -> Held {
        Held {
            message,
            at,
            had: None,
        }
    }
}

impl Stream {
    pub(super) fn new(to: To, timeouts: [Option<Duration>; 2]) -> Stream {
        Stream {
            to,
            timeouts,
            conn: None,
            held: VecDeque::new(),
            ended: 0,
            window: LEAST_RTO,
            sent: false,
        }
    }

    /// A stream to the unix socket at `path`, on `socket`, connected to it already: the
    /// host's own syslog's, which Windows keeps none of.
    #[cfg(unix)]
    pub(super) fn unix(path: PathBuf, socket: UnixStream) -> Stream {
        let mut stream = Stream::new(To::Unix(path), [None; 2]);
        stream.conn = Some(Conn::Unix(socket, 0));
        stream
    }

    /// Writes `message` whole, on a connection made where there is none, the messages held
    /// from one before it written on it first. Once written, a message is held until it is
    /// delivered; one that is not written is its caller's to give again.
    pub(super) fn send(&mut self, message: &[u8]) -> io::Result<()> {
        self.connect()?;
        let Some(conn) = &mut self.conn else {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        };
        if let Err(e) = conn.write_all(message).and_then(|()| conn.flush()) {
            self.conn = None;
            if !self.held.is_empty() {
                self.ended = self.ended.saturating_add(1);
            }
            return Err(e);
        }
        let at = conn.written();
        self.held.push_back(Held::new(message.to_vec(), at));
        self.settle();
        Ok(())
    }

    /// Makes a connection where there is none, and writes on it what is held.
    fn connect(&mut self) -> io::Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let mut conn = Conn::open(&self.to, self.timeouts)?;
        self.sent = false;
        for held in &mut self.held {
            conn.write_all(&held.message)?;
            conn.flush()?;
            held.at = conn.written();
            held.had = None;
        }
        self.conn = Some(conn);
        Ok(())
    }

    /// Lets go of the messages delivered: those its receiver has had for the connection's
    /// window, the connection not ended. A connection that cannot say what its receiver has
    /// lets go of none; one its receiver has ended is let go of, what it held going on the
    /// next, and says so.
    ///
    /// A receiver that ends a connection as it reads a message it will not take (Graylog's
    /// GELF input past its `max_message_size`, Fluent Bit's forward input past its buffer)
    /// has had that message, its TCP acknowledging its bytes as it read them. So a message
    /// it has is let go of only once the connection is found not ended by its receiver the
    /// window after it was first seen to have it; what is held goes on the next connection,
    /// and connections that end so in a row point at the message held that the receiver may
    /// not take ([`Stream::suspect`]).
    fn settle(&mut self) -> bool {
        let Some(conn) = &mut self.conn else {
            return false;
        };
        match conn.peer() {
            Peer::Ended => {
                self.conn = None;
                self.sent = false;
                if self.held.is_empty() {
                    return false;
                }
                self.ended = self.ended.saturating_add(1);
                return true;
            }
            peer => self.sent = matches!(peer, Peer::Sent),
        }
        let Ok((had, rto)) = conn.had() else {
            return false;
        };
        self.window = if rto.is_zero() { LEAST_RTO } else { rto };
        let now = Instant::now();
        for held in self.held.iter_mut().take_while(|h| h.at <= had) {
            held.had.get_or_insert(now);
        }
        let before = self.held.len();
        while self
            .held
            .front()
            .and_then(|h| h.had)
            .is_some_and(|t| now.saturating_duration_since(t) >= self.window)
        {
            self.held.pop_front();
        }
        if self.held.len() < before {
            self.ended = 0;
        }
        false
    }

    /// How many messages are held, the oldest the first.
    pub(super) fn held(&self) -> usize {
        self.held.len()
    }

    /// How many connections in a row ended with messages held.
    #[cfg(test)]
    pub(super) fn ended(&self) -> u32 {
        self.ended
    }

    /// The message held that its receiver may refuse as too long: where two connections in
    /// a row have ended with messages held and none delivered between, the longest held,
    /// where it is longer than `took`, the longest the destination is known to have taken
    /// (learned.rs). None where nothing points at one: a receiver that restarts, or a
    /// network that drops, ends connections whatever their messages' lengths.
    pub(super) fn suspect(&self, took: usize) -> Option<usize> {
        if self.ended < 2 {
            return None;
        }
        let (i, longest) = self
            .held
            .iter()
            .enumerate()
            .max_by_key(|(_, h)| h.message.len())?;
        (longest.message.len() > took).then_some(i)
    }

    /// Puts `with` where the held message at `index` was, as its driver cut it again, the
    /// count of ended connections begun again: all held goes on the next connection.
    pub(super) fn replace(&mut self, index: usize, with: Vec<Vec<u8>>) {
        if index < self.held.len() {
            let rest = self.held.split_off(index);
            self.held.extend(with.into_iter().map(|m| Held::new(m, 0)));
            self.held.extend(rest.into_iter().skip(1));
        }
        self.ended = 0;
        self.conn = None;
    }

    /// Puts each of `changed`'s messages in place of the held message at its index, as its
    /// driver wrote it again: all held goes on the next connection.
    pub(super) fn rewrite(&mut self, changed: Vec<(usize, Vec<u8>)>) {
        for (i, message) in changed {
            if let Some(held) = self.held.get_mut(i) {
                held.message = message;
            }
        }
        self.conn = None;
    }

    /// How many of the messages written their receiver may not have yet.
    pub(super) fn undelivered(&mut self) -> usize {
        self.settle();
        self.held.len()
    }

    /// Lets go of what is held: its receiver has said it has it (fluentd's acks), or its
    /// caller gives it again.
    pub(super) fn let_go(&mut self) {
        self.held.clear();
    }

    /// Drops its connection, whose state is not known: what is held goes on the next.
    pub(super) fn drop_connection(&mut self) {
        self.conn = None;
    }

    /// Ends its connection once all held is delivered and its receiver has all written on
    /// it. What is held is first written on a connection where there is none, and waited
    /// on until it is delivered: so a receiver's end that comes then is no answer to this
    /// one's, and says so (`ErrorKind::ConnectionAborted`), what it held kept for the next
    /// try, as its driver cuts it again where connections end on it ([`Stream::suspect`]).
    /// Where the end fails, what was not delivered stays held, for the next try. `wait`
    /// waits for the receiver, and says when the daemon stops.
    pub(super) fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        loop {
            if self.conn.is_none() && self.held.is_empty() {
                return Ok(());
            }
            if self.settle() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!(
                        "its receiver ended the connection with {} messages it may not have, sent again",
                        self.held.len()
                    ),
                ));
            }
            if self.held.is_empty() {
                break;
            }
            if self.conn.is_none() {
                self.connect()?;
                continue;
            }
            self.await_delivery(wait)?;
        }
        let Some(conn) = self.conn.take() else {
            return Ok(());
        };
        conn.finish(wait)?;
        self.held.clear();
        Ok(())
    }

    /// Waits for the oldest message held to be delivered: for its receiver to have it, which
    /// its TCP acknowledges within the kernel's own wait for it (or the kernel sends again),
    /// then the connection's window; either cut short as its receiver ends the connection or
    /// sends. Where what its receiver sent is not yet read, which would wake the wait at
    /// once, the while is waited whole.
    fn await_delivery(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        let left = match self.held.front().and_then(|h| h.had) {
            Some(had) => self.window.saturating_sub(had.elapsed()),
            None => self.window,
        };
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        if left.is_zero() {
            return Ok(());
        }
        if self.sent {
            return if wait.pause(left) {
                Err(super::stops())
            } else {
                Ok(())
            };
        }
        conn.readable(wait, left)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.conn {
            Some(conn) => conn.read(buf),
            None => Err(io::Error::from(io::ErrorKind::NotConnected)),
        }
    }
}

/// A connection: TCP's counting the bytes written on it, and a unix socket's with the
/// bytes written on it.
enum Conn {
    Tcp(Counted),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Counted>>),
    Unix(UnixStream, u64),
}

/// What a connection's receiver was found to have done, without waiting and taking nothing
/// its reader would read ([`Conn::peer`]).
enum Peer {
    /// Nothing yet.
    Open,
    /// It sent what is not yet read.
    Sent,
    /// It ended the connection: its end (a FIN, or TLS's close_notify), a reset, or a TLS
    /// error that ends it.
    Ended,
}

/// A TCP socket, and how many bytes have been written on it.
struct Counted {
    socket: TcpStream,
    written: u64,
    /// What it has sent again, as Windows counts it (sockets.rs `unacknowledged`).
    #[cfg(windows)]
    retransmitted: super::sockets::Retransmitted,
}

impl Counted {
    fn new(socket: TcpStream) -> Counted {
        Counted {
            socket,
            written: 0,
            #[cfg(windows)]
            retransmitted: super::sockets::Retransmitted::default(),
        }
    }

    /// How many of the bytes written on it its receiver has not acknowledged, and its
    /// kernel's RTO (sockets.rs `unacknowledged`).
    fn unacknowledged(&mut self) -> io::Result<(u64, Duration)> {
        #[cfg(unix)]
        return super::sockets::unacknowledged(&self.socket);
        #[cfg(windows)]
        return super::sockets::unacknowledged(&self.socket, self.written, &mut self.retransmitted);
    }

    /// What its receiver was found to have done, peeked at without waiting.
    fn peer(&self) -> Peer {
        if self.socket.set_nonblocking(true).is_err() {
            return Peer::Open;
        }
        let mut byte = [0u8; 1];
        let peer = match self.socket.peek(&mut byte) {
            Ok(0) => Peer::Ended,
            Ok(_) => Peer::Sent,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                Peer::Open
            }
            Err(_) => Peer::Ended,
        };
        let _ = self.socket.set_nonblocking(false);
        peer
    }
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.read(buf)
    }
}

impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.socket.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(c) => c.read(buf),
            Conn::Tls(s) => s.read(buf),
            Conn::Unix(s, _) => shards_ipc::read(&*s, buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(c) => c.write(buf),
            Conn::Tls(s) => s.write(buf),
            Conn::Unix(s, written) => {
                let n = s.write(buf)?;
                *written = written.saturating_add(n as u64);
                Ok(n)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tcp(c) => c.flush(),
            Conn::Tls(s) => s.flush(),
            Conn::Unix(s, _) => s.flush(),
        }
    }
}

impl Conn {
    fn open(to: &To, [write, read]: [Option<Duration>; 2]) -> io::Result<Conn> {
        let tcp = |address: &str| -> io::Result<Counted> {
            let socket = TcpStream::connect(address)?;
            socket.set_write_timeout(write)?;
            socket.set_read_timeout(read)?;
            Ok(Counted::new(socket))
        };
        Ok(match to {
            To::Tcp(address) => Conn::Tcp(tcp(address)?),
            To::Tls(address, config) => {
                let host = address
                    .rsplit_once(':')
                    .map_or(address.as_str(), |(host, _)| host)
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                let name = rustls::pki_types::ServerName::try_from(host.to_string())
                    .map_err(|e| io::Error::other(format!("{host}: {e}")))?;
                let conn = rustls::ClientConnection::new(config.clone(), name).map_err(io::Error::other)?;
                Conn::Tls(Box::new(rustls::StreamOwned::new(conn, tcp(address)?)))
            }
            // Windows' AF_UNIX streams, which std does not open: afunix's, Winsock started.
            To::Unix(path) => {
                #[cfg(windows)]
                shards_vmm::platform::winsock()?;
                let socket = UnixStream::connect(path)?;
                socket.set_write_timeout(write)?;
                socket.set_read_timeout(read)?;
                Conn::Unix(socket, 0)
            }
        })
    }

    /// The bytes written on it.
    fn written(&self) -> u64 {
        match self {
            Conn::Tcp(c) => c.written,
            Conn::Tls(s) => s.sock.written,
            Conn::Unix(_, written) => *written,
        }
    }

    /// How many of the bytes written on it its receiver has, and its kernel's RTO: its
    /// TCP's acknowledged; a unix socket's, all written, which are in its receiver's buffer
    /// as the write returns, and no RTO.
    fn had(&mut self) -> io::Result<(u64, Duration)> {
        match self {
            Conn::Tcp(c) => {
                let (unacknowledged, rto) = c.unacknowledged()?;
                Ok((c.written.saturating_sub(unacknowledged), rto))
            }
            Conn::Tls(s) => {
                let (unacknowledged, rto) = s.sock.unacknowledged()?;
                Ok((s.sock.written.saturating_sub(unacknowledged), rto))
            }
            Conn::Unix(_, written) => Ok((*written, Duration::ZERO)),
        }
    }

    /// What its receiver has done, found without waiting and taking nothing its reader
    /// would read. Over TLS the records come first: a session ticket is taken, what the
    /// receiver sent is its reader's, and its close_notify (RFC 8446 §6.1: it "will not
    /// send any more messages on this connection") ends the connection as its FIN would,
    /// which comes after it; a FIN without it, or a fatal alert, ends it too.
    fn peer(&mut self) -> Peer {
        match self {
            Conn::Tcp(c) => c.peer(),
            Conn::Tls(s) => {
                if s.sock.socket.set_nonblocking(true).is_err() {
                    return Peer::Open;
                }
                let peer = loop {
                    match s.conn.process_new_packets() {
                        Err(_) => break Peer::Ended,
                        Ok(io) if io.peer_has_closed() => break Peer::Ended,
                        Ok(io) if io.plaintext_bytes_to_read() > 0 => break Peer::Sent,
                        Ok(_) => {}
                    }
                    if !s.conn.wants_read() {
                        break Peer::Open;
                    }
                    match s.conn.read_tls(&mut s.sock) {
                        Ok(0) => break Peer::Ended,
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Peer::Open,
                        Err(_) => break Peer::Ended,
                    }
                };
                let _ = s.sock.socket.set_nonblocking(false);
                peer
            }
            Conn::Unix(s, _) => unix_peer(s),
        }
    }

    /// Waits until its socket is readable, or `d` has passed; an error where the daemon
    /// stops first. Its socket blocks again after, as its writes and reads have it.
    fn readable(&self, wait: &Wait<'_>, d: Duration) -> io::Result<()> {
        let socket = match self {
            Conn::Tcp(c) => &c.socket,
            Conn::Tls(s) => &s.sock.socket,
            #[cfg(unix)]
            Conn::Unix(s, _) => return wait.readable(s.as_fd(), d),
            // An AF_UNIX socket of afunix's, which no event is selected on here: the while.
            #[cfg(windows)]
            Conn::Unix(..) => {
                return if wait.pause(d) {
                    Err(super::stops())
                } else {
                    Ok(())
                };
            }
        };
        #[cfg(unix)]
        let waited = wait.readable(socket.as_fd(), d);
        #[cfg(windows)]
        let waited = wait.readable(std::os::windows::io::AsSocket::as_socket(socket), d);
        socket.set_nonblocking(false)?;
        waited
    }

    /// Ends the connection once its receiver has all written on it, and has ended its own
    /// side, as it does once it has read all of this one's.
    fn finish(self, wait: &Wait<'_>) -> io::Result<()> {
        let mut tcp = match self {
            Conn::Unix(..) => return Ok(()),
            Conn::Tcp(c) => c,
            Conn::Tls(mut s) => {
                s.conn.send_close_notify();
                s.flush()?;
                s.into_parts().1
            }
        };
        tcp.socket.shutdown(Shutdown::Write)?;
        tcp.socket.set_nonblocking(true)?;
        let mut ended = false;
        let mut scrap = [0u8; 4096];
        loop {
            while !ended {
                match tcp.socket.read(&mut scrap) {
                    Ok(0) => ended = true,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            let (unacknowledged, rto) = tcp.unacknowledged()?;
            // Closed once its receiver has ended its side too, as Winsock's graceful shutdown
            // closes ("Graceful Shutdown, Linger Options, and Socket Closure"): its TCP's
            // acknowledgement says its kernel has the bytes, not that it has read them, and a
            // close before more of its bytes come, or with any unread, resets the connection,
            // which on Windows takes from it what it had not read (CI 40900902805: a TLS
            // receiver's session tickets came after the last read here, and it read nothing
            // of the line its TCP had acknowledged).
            if ended && unacknowledged == 0 {
                return Ok(());
            }
            // Its acknowledgement comes within the kernel's own wait for it, or the kernel
            // sends again; a reset or a timeout ends the connection. Its receiver's end is
            // waited for as what it sends is read and let go.
            let rto = if rto.is_zero() { LEAST_RTO } else { rto };
            if ended {
                if wait.pause(rto) {
                    return Err(super::stops());
                }
            } else {
                #[cfg(unix)]
                wait.readable(tcp.socket.as_fd(), rto)?;
                #[cfg(windows)]
                wait.readable(std::os::windows::io::AsSocket::as_socket(&tcp.socket), rto)?;
            }
        }
    }
}

/// What a unix socket's receiver was found to have done, peeked at without waiting: its
/// end, at which a read takes nothing, or a reset; what it sent; or nothing yet.
#[cfg(unix)]
fn unix_peer(socket: &UnixStream) -> Peer {
    use std::os::fd::AsRawFd as _;
    let mut byte = [0u8; 1];
    // SAFETY: recv(2) of at most a byte into `byte`, peeking, without waiting.
    let n = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match n {
        0 => Peer::Ended,
        n if n > 0 => Peer::Sent,
        _ => match io::Error::last_os_error().kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Peer::Open,
            _ => Peer::Ended,
        },
    }
}

/// [`unix_peer`] of afunix's socket, Winsock's recv peeking, the socket not blocking for
/// it.
#[cfg(windows)]
fn unix_peer(socket: &UnixStream) -> Peer {
    use windows_sys::Win32::Networking::WinSock::{MSG_PEEK, WSAEWOULDBLOCK, WSAGetLastError, recv};
    if socket.set_nonblocking(true).is_err() {
        return Peer::Open;
    }
    let mut byte = [0u8; 1];
    // SAFETY: recv of at most a byte into `byte`, peeking, on a socket `socket` owns.
    let n = unsafe { recv(socket.raw(), byte.as_mut_ptr(), 1, MSG_PEEK) };
    let peer = match n {
        0 => Peer::Ended,
        n if n > 0 => Peer::Sent,
        // SAFETY: the calling thread's last Winsock error, read at once.
        _ if unsafe { WSAGetLastError() } == WSAEWOULDBLOCK => Peer::Open,
        _ => Peer::Ended,
    };
    let _ = socket.set_nonblocking(false);
    peer
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Sets the socket option `option` of `socket` to `value`'s ints: SO_RCVBUF's one,
    /// SO_LINGER's two (Unix's `linger`; Windows' LINGER, two u16s).
    #[cfg(unix)]
    fn set(socket: &impl std::os::fd::AsRawFd, option: libc::c_int, value: &[libc::c_int]) {
        // SAFETY: setsockopt(2) of `value`'s ints, its length theirs.
        let set = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                value.as_ptr().cast(),
                std::mem::size_of_val(value) as libc::socklen_t,
            )
        };
        assert_eq!(set, 0);
    }

    #[cfg(windows)]
    fn set(socket: &impl std::os::windows::io::AsRawSocket, option: i32, value: &[i32]) {
        use windows_sys::Win32::Networking::WinSock::{SO_LINGER, SOCKET, SOL_SOCKET, setsockopt};
        let linger: Vec<u16> = value.iter().map(|&v| u16::try_from(v).unwrap()).collect();
        let bytes: &[u8] = if option == SO_LINGER {
            // SAFETY: the u16s' own bytes.
            unsafe { std::slice::from_raw_parts(linger.as_ptr().cast(), linger.len() * 2) }
        } else {
            // SAFETY: the i32s' own bytes.
            unsafe { std::slice::from_raw_parts(value.as_ptr().cast(), std::mem::size_of_val(value)) }
        };
        // SAFETY: setsockopt of `bytes`, its length theirs.
        let set = unsafe {
            setsockopt(
                socket.as_raw_socket() as SOCKET,
                SOL_SOCKET,
                option,
                bytes.as_ptr(),
                i32::try_from(bytes.len()).unwrap(),
            )
        };
        assert_eq!(set, 0);
    }

    #[cfg(unix)]
    use libc::{SO_LINGER, SO_RCVBUF};
    #[cfg(windows)]
    use windows_sys::Win32::Networking::WinSock::{SO_LINGER, SO_RCVBUF};

    /// A TLS server's config for `localhost`, and a client's that trusts its CA.
    pub(in crate::daemon::ship) fn tls() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut ca = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().unwrap()).unwrap();
        let mut leaf = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = leaf.signed_by(&key, &ca).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(server), Arc::new(client))
    }

    /// What a stream writes reaches its receiver whole however soon after it is closed,
    /// the receiver's TLS 1.3 session tickets unread, and the receiver sees it end cleanly,
    /// with TLS's close_notify. A socket closed with bytes unread resets its connection,
    /// and what is not yet sent goes with it: D163's CI lost a run's last line so, a line
    /// held back by Nagle's algorithm behind the one before it (PM M233).
    #[test]
    fn a_stream_closed_delivers_all_it_wrote() {
        let (server, client) = tls();
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        for _ in 0..20 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = format!("localhost:{}", listener.local_addr().unwrap().port());
            let config = server.clone();
            let reading = std::thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                let mut tls =
                    rustls::StreamOwned::new(rustls::ServerConnection::new(config).unwrap(), socket);
                let mut got = Vec::new();
                // Ok at close_notify; an error at a reset, or at an end without it.
                let ended = tls.read_to_end(&mut got).map(|_| ()).map_err(|e| e.kind());
                (got, ended)
            });
            let mut stream = Stream::new(To::Tls(address, client.clone()), [None; 2]);
            stream.send(b"first\n").unwrap();
            // The session tickets come, unread; then two lines, the second held back
            // until the first is acknowledged, and at once the close.
            std::thread::sleep(Duration::from_millis(50));
            stream.send(b"out\n").unwrap();
            stream.send(b"err\n").unwrap();
            stream.close(&Wait(&stopped)).unwrap();
            let (got, ended) = reading.join().unwrap();
            assert_eq!(
                (String::from_utf8(got).unwrap(), ended),
                ("first\nout\nerr\n".into(), Ok(()))
            );
            assert_eq!(stream.undelivered(), 0);
        }
    }

    /// A stream ends once its receiver has ended its side, not once its receiver's TCP has
    /// its bytes: a receiver that sends after this end has shut its writing, before it
    /// reads, is answered with no reset, and reads all written, then its end.
    #[test]
    fn a_stream_ends_once_its_receiver_has_ended_its_side() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (closing, told) = std::sync::mpsc::channel::<()>();
        let reading = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            told.recv().unwrap();
            // Past the acknowledgement of all it was sent, which a close once acknowledged
            // took for its end.
            std::thread::sleep(Duration::from_millis(200));
            let late = socket.write_all(b"late").map_err(|e| e.kind());
            let mut got = Vec::new();
            let ended = socket.read_to_end(&mut got).map(|_| ()).map_err(|e| e.kind());
            (late, got, ended)
        });
        let mut stream = Stream::new(To::Tcp(address), [None; 2]);
        stream.send(b"line\n").unwrap();
        closing.send(()).unwrap();
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        stream.close(&Wait(&stopped)).unwrap();
        let (late, got, ended) = reading.join().unwrap();
        assert_eq!(
            (late, String::from_utf8(got).unwrap(), ended),
            (Ok(()), "line\n".into(), Ok(()))
        );
    }

    /// A stream over TLS whose server's certificate a CA given beside the platform's roots
    /// vouches for, as syslog's `syslog-tls-ca-cert` gives one (shards_registry's
    /// `client_config`, the platform's verifier): what it writes reaches its server whole,
    /// ended with TLS's close_notify, on every host.
    #[test]
    fn a_stream_trusts_a_ca_given_beside_the_platforms() {
        let now = time::OffsetDateTime::now_utc();
        let mut ca = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.not_before = now - time::Duration::hours(1);
        // Named apart: Windows' chain engine takes a certificate whose subject is its issuer's
        // for one that signed itself, and checks its signature by its own key
        // (TRUST_E_CERT_SIGNATURE), where rcgen names both alike unless told.
        ca.distinguished_name = rcgen::DistinguishedName::new();
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "shards test CA");
        ca.not_after = now + time::Duration::days(1);
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().unwrap()).unwrap();
        let mut leaf = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        leaf.not_before = now - time::Duration::hours(1);
        leaf.distinguished_name = rcgen::DistinguishedName::new();
        leaf.distinguished_name
            .push(rcgen::DnType::CommonName, "localhost");
        leaf.not_after = now + time::Duration::days(1);
        leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = leaf.signed_by(&key, &ca).unwrap();
        let server = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("localhost:{}", listener.local_addr().unwrap().port());
        let reading = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(server).unwrap(), socket);
            let mut got = Vec::new();
            let ended = tls.read_to_end(&mut got).map(|_| ()).map_err(|e| e.to_string());
            (got, ended)
        });
        let client = shards_registry::tls::client_config(vec![ca.der().clone()], None).unwrap();
        let mut stream = Stream::new(To::Tls(address, client), [None; 2]);
        let sent = stream.send(b"line\n");
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        let closed = stream.close(&Wait(&stopped));
        let (got, ended) = reading.join().unwrap();
        assert_eq!(
            (sent.map_err(|e| e.to_string()), closed.map_err(|e| e.to_string())),
            (Ok(()), Ok(()))
        );
        assert_eq!(
            (String::from_utf8(got).unwrap(), ended),
            ("line\n".into(), Ok(()))
        );
    }

    /// What its receiver had not acknowledged when its connection was dropped, a stream
    /// writes again on its next, in order, before what comes after (PM M233). Its receiver
    /// reads none: messages go until a write makes no progress for the stream's write
    /// timeout, both buffers full, and the stream drops the connection, holding what was
    /// never delivered; the receiver then resets it, as one that closes with
    /// bytes unread does. Waiting on a receive buffer's size instead failed under load:
    /// macOS takes some 520 KiB however small a buffer it reports (5 of 200 runs lost the
    /// held messages to it, and one hung).
    #[test]
    fn what_a_dropped_connection_had_not_delivered_goes_on_the_next() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // The least receive buffer the kernel takes, so that the buffers fill soon.
        set(&listener, SO_RCVBUF, &[1]);
        let mut stream = Stream::new(
            To::Tcp(listener.local_addr().unwrap().to_string()),
            [Some(Duration::from_millis(300)), None],
        );
        let message = |i: u32| i.to_be_bytes().repeat(256);
        let mut sent = 0u32;
        while stream.send(&message(sent)).is_ok() {
            sent += 1;
            assert!(sent < 65_536, "its receiver took 64 MiB");
        }
        assert!(
            stream.conn.is_none(),
            "a write that failed dropped its connection"
        );
        // Its receiver resets the connection: a close with SO_LINGER 0.
        let (first, _) = listener.accept().unwrap();
        set(&first, SO_LINGER, &[1, 0]);
        drop(first);
        let held: Vec<Vec<u8>> = stream.held.iter().map(|h| h.message.clone()).collect();
        assert!(held.len() >= 2, "{} held", held.len());
        let reading = std::thread::spawn(move || {
            let (mut second, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            second.read_to_end(&mut got).unwrap();
            got
        });
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        stream.send(b"last").unwrap();
        stream.close(&Wait(&stopped)).unwrap();
        // The messages held are the last sent, in order; and after them, the next.
        let first_held = sent - u32::try_from(held.len()).unwrap();
        assert_eq!(held, (first_held..sent).map(message).collect::<Vec<_>>());
        let mut want: Vec<u8> = held.concat();
        want.extend_from_slice(b"last");
        assert_eq!(reading.join().unwrap(), want);
    }

    /// Do: a receiver of NUL-ended messages that ends its connection on reading one of more
    /// than 100 bytes, as Graylog's GELF input ends one past its `max_message_size`; send
    /// a short message, a long one, then short ones, each connection's end found as the
    /// stream next settles. Expect the long one pointed at once two connections have ended
    /// with it held, and not before; nothing pointed at that is no longer than what the
    /// destination took; and, the long one replaced by short pieces, every message but it
    /// received.
    #[test]
    fn a_receiver_that_ends_connections_on_a_message_points_at_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (got, frames) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let mut read = std::io::BufReader::new(conn);
                loop {
                    let mut frame = Vec::new();
                    match std::io::BufRead::read_until(&mut read, 0, &mut frame) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    frame.pop();
                    if frame.len() > 100 {
                        break;
                    }
                    if got.send(frame).is_err() {
                        return;
                    }
                }
            }
        });
        let mut stream = Stream::new(To::Tcp(address), [None; 2]);
        let long = [vec![b'L'; 300], vec![0]].concat();
        stream.send(b"first\0").unwrap();
        stream.send(&long).unwrap();
        let started = std::time::Instant::now();
        let mut i = 0;
        let at = loop {
            assert!(started.elapsed() < Duration::from_secs(20), "nothing pointed at");
            if let Some(at) = stream.suspect(10) {
                break at;
            }
            std::thread::sleep(Duration::from_millis(20));
            stream.undelivered();
            let _ = stream.send(format!("tick{i}\0").as_bytes());
            i += 1;
        };
        assert_eq!(stream.held.get(at).map(|h| h.message.len()), Some(301));
        assert_eq!(
            stream.suspect(301),
            None,
            "pointed at though no longer than taken"
        );
        stream.replace(at, vec![b"piece1\0".to_vec(), b"piece2\0".to_vec()]);
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        stream.close(&Wait(&stopped)).unwrap();
        let frames: Vec<String> = frames.try_iter().map(|f| String::from_utf8(f).unwrap()).collect();
        assert!(
            frames.contains(&"first".to_string()) && frames.contains(&"piece2".to_string()),
            "{frames:?}"
        );
        assert!(frames.iter().all(|f| f.len() <= 100));
    }

    /// The frames of `conn`, each ended by a NUL, sent on `got` until one is longer than
    /// 100 bytes, which ends the connection as `end` does; false where `got` is gone.
    fn frames<C: Read>(conn: C, got: &std::sync::mpsc::Sender<Vec<u8>>, end: impl FnOnce(C)) -> bool {
        let mut read = std::io::BufReader::new(conn);
        loop {
            let mut frame = Vec::new();
            match std::io::BufRead::read_until(&mut read, 0, &mut frame) {
                Ok(0) | Err(_) => return true,
                Ok(_) => {}
            }
            frame.pop();
            if frame.len() > 100 {
                end(read.into_inner());
                return true;
            }
            if got.send(frame).is_err() {
                return false;
            }
        }
    }

    /// Settles `stream` until it has counted `n` connections ended with messages held: its
    /// receiver's end found as it settles, no write failing meanwhile.
    fn until_ended(stream: &mut Stream, n: u32) {
        let started = std::time::Instant::now();
        while stream.ended < n {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "{} ends found, not {n}",
                stream.ended
            );
            std::thread::sleep(Duration::from_millis(5));
            stream.undelivered();
        }
        assert!(stream.conn.is_none(), "an end found let its connection go");
    }

    /// Do: over TLS, to a receiver of NUL-ended messages that ends its connection on reading
    /// one of more than 100 bytes with TLS's close_notify, then shuts its TCP's writing and
    /// holds its socket (so that nothing resets it); send a short message and a long one,
    /// and once the receiver has ended the connection, settle the stream; then send another,
    /// on a connection of its own, the held written first, and settle again. Expect each end
    /// found as the stream settles (RFC 8446 §6.1's close_notify read as its end), no write
    /// failing; and the long one pointed at after the second.
    #[test]
    fn a_tls_receivers_close_notify_ends_its_connection() {
        let (server, client) = tls();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("localhost:{}", listener.local_addr().unwrap().port());
        let (got, _frames) = std::sync::mpsc::channel::<Vec<u8>>();
        let (ends, ended) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let tls =
                    rustls::StreamOwned::new(rustls::ServerConnection::new(server.clone()).unwrap(), conn);
                let going = frames(tls, &got, |mut tls| {
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                    let _ = tls.sock.shutdown(Shutdown::Write);
                    kept.push(tls);
                    let _ = ends.send(());
                });
                if !going {
                    return;
                }
            }
        });
        let mut stream = Stream::new(To::Tls(address, client), [None; 2]);
        let long = [vec![b'L'; 300], vec![0]].concat();
        stream.send(b"first\0").unwrap();
        stream.send(&long).unwrap();
        ended.recv_timeout(Duration::from_secs(20)).unwrap();
        until_ended(&mut stream, 1);
        stream.send(b"tick\0").unwrap();
        ended.recv_timeout(Duration::from_secs(20)).unwrap();
        until_ended(&mut stream, 2);
        let at = stream.suspect(10).unwrap();
        assert_eq!(stream.held.get(at).map(|h| h.message.len()), Some(301));
    }

    /// Do: over a unix socket, to a receiver of NUL-ended messages that ends its connection
    /// on reading one of more than 100 bytes; send a short message and a long one, settle
    /// the stream once the receiver has ended the connection, send another on a new one,
    /// settle again; then put short pieces in the long one's place and close. Expect what a
    /// unix socket's receiver has held until it is delivered, as TCP's is: each end found
    /// with both held, the long one pointed at after the second; and every message but it
    /// received, its pieces among them.
    #[test]
    fn a_unix_receiver_that_ends_connections_on_a_message_points_at_it() {
        let dir = shards_testdir::TempDir::new("ux").unwrap();
        let path = dir.join("r.sock");
        let listener = shards_vmm::platform::UnixListener::bind(&path).unwrap();
        let (got, got_frames) = std::sync::mpsc::channel::<Vec<u8>>();
        let (ends, ended) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                let Ok((conn, _)) = listener.accept() else { return };
                if !frames(conn, &got, |conn| {
                    drop(conn);
                    let _ = ends.send(());
                }) {
                    return;
                }
            }
        });
        let mut stream = Stream::new(To::Unix(path), [None; 2]);
        let long = [vec![b'L'; 300], vec![0]].concat();
        stream.send(b"first\0").unwrap();
        stream.send(&long).unwrap();
        assert_eq!(stream.held(), 2, "a unix socket's messages held");
        ended.recv_timeout(Duration::from_secs(20)).unwrap();
        until_ended(&mut stream, 1);
        stream.send(b"tick\0").unwrap();
        ended.recv_timeout(Duration::from_secs(20)).unwrap();
        until_ended(&mut stream, 2);
        let at = stream.suspect(10).unwrap();
        assert_eq!(stream.held.get(at).map(|h| h.message.len()), Some(301));
        stream.replace(at, vec![b"piece1\0".to_vec(), b"piece2\0".to_vec()]);
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        stream.close(&Wait(&stopped)).unwrap();
        let mut frames: Vec<String> = Vec::new();
        for want in ["first", "piece1", "piece2", "tick"] {
            while !frames.iter().any(|f| f == want) {
                match got_frames.recv_timeout(Duration::from_secs(20)) {
                    Ok(f) => frames.push(String::from_utf8(f).unwrap()),
                    Err(e) => panic!("{want}: {e}: {frames:?}"),
                }
            }
        }
    }
}
