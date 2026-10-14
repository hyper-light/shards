//! The stream connections the drivers send a run's log over (D163 to D165): TCP, TLS or a
//! unix socket. A message is delivered once its receiver's TCP has acknowledged its last
//! byte (PM M233); until then it is held, and written again, after those held before it,
//! on the next connection should this one end first: a receiver that restarts, or a
//! connection reset, costs a message sent twice, never one lost. A connection is ended only
//! once its receiver has all written on it: TLS's close_notify sent, its write side shut,
//! what the receiver still sends (TLS 1.3's session tickets, its own close_notify) read and
//! let go, and its socket closed once all is acknowledged. A socket closed with bytes
//! unread resets its connection, and what was written on it and not yet sent goes with it
//! (PM M233): srslog, go-gelf and fluent-logger-golang close theirs so, and lose that
//! tail. What is written on a unix socket is in its receiver's buffer as the write returns,
//! and stays there through the sender's close: it is delivered as it is written.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::Wait;

/// The least retransmission timeout (RFC 6298 §2.4), waited where a kernel gives none while
/// bytes are unacknowledged.
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
    /// not have, oldest first: each, and its connection's bytes once it was written.
    held: VecDeque<(Vec<u8>, u64)>,
}

impl Stream {
    pub(super) fn new(to: To, timeouts: [Option<Duration>; 2]) -> Stream {
        Stream {
            to,
            timeouts,
            conn: None,
            held: VecDeque::new(),
        }
    }

    /// A stream to the unix socket at `path`, on `socket`, connected to it already.
    pub(super) fn unix(path: PathBuf, socket: UnixStream) -> Stream {
        Stream {
            to: To::Unix(path),
            timeouts: [None; 2],
            conn: Some(Conn::Unix(socket)),
            held: VecDeque::new(),
        }
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
            return Err(e);
        }
        if let Some(tcp) = conn.tcp() {
            self.held.push_back((message.to_vec(), tcp.written));
            self.settle();
        }
        Ok(())
    }

    /// Makes a connection where there is none, and writes on it what is held.
    fn connect(&mut self) -> io::Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let mut conn = Conn::open(&self.to, self.timeouts)?;
        for (message, at) in &mut self.held {
            conn.write_all(message)?;
            conn.flush()?;
            *at = conn.tcp().map_or(0, |tcp| tcp.written);
        }
        self.conn = Some(conn);
        Ok(())
    }

    /// Lets go of the messages delivered: those whose last byte their receiver's TCP has
    /// acknowledged. A connection that cannot say, or has ended, lets go of none.
    fn settle(&mut self) {
        let Some(tcp) = self.conn.as_ref().and_then(Conn::tcp) else {
            return;
        };
        let Ok((unacknowledged, _)) = super::sockets::unacknowledged(&tcp.socket) else {
            return;
        };
        let acknowledged = tcp.written.saturating_sub(unacknowledged);
        while self.held.front().is_some_and(|(_, at)| *at <= acknowledged) {
            self.held.pop_front();
        }
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

    /// Ends its connection once its receiver has all written on it, what is held from one
    /// before it written first. Where that fails, what was not delivered stays held, for
    /// the next try. `wait` waits for the receiver, and says when the daemon stops.
    pub(super) fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        if self.conn.is_none() && self.held.is_empty() {
            return Ok(());
        }
        self.connect()?;
        let Some(conn) = self.conn.take() else {
            return Ok(());
        };
        conn.finish(wait)?;
        self.held.clear();
        Ok(())
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

/// A connection: TCP's counting the bytes written on it.
enum Conn {
    Tcp(Counted),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Counted>>),
    Unix(UnixStream),
}

/// A TCP socket, and how many bytes have been written on it.
struct Counted {
    socket: TcpStream,
    written: u64,
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
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(c) => c.write(buf),
            Conn::Tls(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Tcp(c) => c.flush(),
            Conn::Tls(s) => s.flush(),
            Conn::Unix(s) => s.flush(),
        }
    }
}

impl Conn {
    fn open(to: &To, [write, read]: [Option<Duration>; 2]) -> io::Result<Conn> {
        let tcp = |address: &str| -> io::Result<Counted> {
            let socket = TcpStream::connect(address)?;
            socket.set_write_timeout(write)?;
            socket.set_read_timeout(read)?;
            Ok(Counted { socket, written: 0 })
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
            To::Unix(path) => {
                let socket = UnixStream::connect(path)?;
                socket.set_write_timeout(write)?;
                socket.set_read_timeout(read)?;
                Conn::Unix(socket)
            }
        })
    }

    /// Its TCP socket, and what has been written on it: none for a unix socket's.
    fn tcp(&self) -> Option<&Counted> {
        match self {
            Conn::Tcp(c) => Some(c),
            Conn::Tls(s) => Some(&s.sock),
            Conn::Unix(_) => None,
        }
    }

    /// Ends the connection once its receiver has all written on it.
    fn finish(self, wait: &Wait<'_>) -> io::Result<()> {
        let mut socket = match self {
            Conn::Unix(_) => return Ok(()),
            Conn::Tcp(c) => c.socket,
            Conn::Tls(mut s) => {
                s.conn.send_close_notify();
                s.flush()?;
                s.into_parts().1.socket
            }
        };
        socket.shutdown(Shutdown::Write)?;
        socket.set_nonblocking(true)?;
        let mut ended = false;
        let mut scrap = [0u8; 4096];
        loop {
            while !ended {
                match socket.read(&mut scrap) {
                    Ok(0) => ended = true,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            let (unacknowledged, rto) = super::sockets::unacknowledged(&socket)?;
            if unacknowledged == 0 {
                return Ok(());
            }
            // Its acknowledgement comes within the kernel's own wait for it, or the kernel
            // sends again; a reset or a timeout ends the connection.
            let rto = if rto.is_zero() { LEAST_RTO } else { rto };
            if ended {
                if wait.pause(rto) {
                    return Err(super::stops());
                }
            } else {
                wait.readable(socket.as_fd(), rto)?;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A TLS server's config for `localhost`, and a client's that trusts its CA.
    fn tls() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
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
        let (_stop, stopped) = UnixStream::pair().unwrap();
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

    /// What its receiver had not acknowledged when its connection was reset, a stream
    /// writes again on its next, in order, before what comes after (PM M233): written
    /// until two messages are unacknowledged, in flight or behind the window of a
    /// receiver that reads none.
    #[test]
    fn what_a_reset_connection_had_not_delivered_goes_on_the_next() {
        use std::os::fd::AsRawFd as _;
        let set = |fd: std::os::fd::RawFd, option: libc::c_int, value: &[libc::c_int]| {
            // SAFETY: setsockopt(2) of `value`'s ints, its length theirs.
            let set = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    value.as_ptr().cast(),
                    std::mem::size_of_val(value) as libc::socklen_t,
                )
            };
            assert_eq!(set, 0);
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // The least receive buffer the kernel takes, and a receiver that reads none: macOS
        // grows it to 526,720 bytes as they come, Linux keeps it at 2,304 (PM M233).
        set(listener.as_raw_fd(), libc::SO_RCVBUF, &[1]);
        let mut stream = Stream::new(To::Tcp(listener.local_addr().unwrap().to_string()), [None; 2]);
        let message = |i: u32| i.to_be_bytes().repeat(256);
        let mut sent = 0u32;
        while stream.undelivered() < 2 {
            assert!(sent < 4096, "nothing held");
            stream.send(&message(sent)).unwrap();
            sent += 1;
        }
        // Its receiver resets the connection: a close with SO_LINGER 0.
        let (first, _) = listener.accept().unwrap();
        set(first.as_raw_fd(), libc::SO_LINGER, &[1, 0]);
        drop(first);
        // Its next write fails, or its close does: what is held then goes on the next.
        let (_stop, stopped) = UnixStream::pair().unwrap();
        let wait = Wait(&stopped);
        let mut last = stream.send(b"last").is_ok();
        while stream.conn.is_some() {
            let _ = stream.close(&wait);
        }
        let held: Vec<Vec<u8>> = stream.held.iter().map(|(m, _)| m.clone()).collect();
        assert!(held.len() >= 2, "{} held", held.len());
        let reading = std::thread::spawn(move || {
            let (mut second, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            second.read_to_end(&mut got).unwrap();
            got
        });
        if !last {
            stream.send(b"last").unwrap();
            last = true;
        }
        stream.close(&wait).unwrap();
        // The messages held are the last sent, in order; and after them, the next.
        let unsent: Vec<Vec<u8>> = held.iter().filter(|m| m.as_slice() != b"last").cloned().collect();
        let first_held = sent - u32::try_from(unsent.len()).unwrap();
        assert_eq!(unsent, (first_held..sent).map(message).collect::<Vec<_>>());
        let mut want: Vec<u8> = unsent.concat();
        want.extend_from_slice(b"last");
        assert!(last);
        assert_eq!(reading.join().unwrap(), want);
    }
}
