//! The syslog driver's sending (D163): its messages (logging/syslog.rs) over UDP, TCP,
//! TLS or a unix socket, as srslog sends them: on a connection made as the first message
//! goes and kept, made again once where a write on it fails (srslog writer.go,
//! writeAndRetry); the host's own syslog, where no address is given, at the first of its
//! sockets that takes a connection (srslog_unix.go, unixSyslog).

use std::io::{self, Write as _};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::Arc;

use super::{Driver, Message};
use crate::logging::syslog::{Config, From, Local, To};

/// The host's own syslog's sockets, tried in turn (srslog unixSyslog's logPaths).
const LOCAL: [&str; 3] = ["/dev/log", "/var/run/syslog", "/var/run/log"];

/// The largest message every syslog receiver over UDP takes (RFC 5426 §3.2: 480 octets
/// over IPv4): one no larger that a receiver refuses is refused for another reason.
const EVERY_RECEIVER: usize = 480;

pub(super) struct Syslog {
    config: Config,
    from: From,
    /// For `tcp+tls`: the platform's roots, the CA given, and the client's certificate.
    tls: Option<Arc<rustls::ClientConfig>>,
    conn: Option<Conn>,
}

enum Conn {
    Udp(UdpSocket),
    Tcp(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Stream(UnixStream),
    Datagram(UnixDatagram),
}

impl Conn {
    /// Writes `m` whole: a datagram, or on a stream.
    fn write(&mut self, m: &[u8]) -> io::Result<()> {
        match self {
            Conn::Udp(s) => super::sockets::send_datagram(s, m),
            Conn::Datagram(s) => super::sockets::send_datagram(s, m),
            Conn::Tcp(s) => s.write_all(m),
            Conn::Stream(s) => s.write_all(m),
            Conn::Tls(s) => {
                s.write_all(m)?;
                s.flush()
            }
        }
    }
}

impl Syslog {
    pub(super) fn new(config: Config, from: From) -> Result<Syslog, String> {
        let tls = match &config.to {
            To::Tls(_) => {
                let (roots, client) = config.tls_material()?;
                Some(shards_registry::tls::client_config(roots, client).map_err(|e| e.to_string())?)
            }
            _ => None,
        };
        Ok(Syslog {
            config,
            from,
            tls,
            conn: None,
        })
    }

    fn connect(&self) -> io::Result<Conn> {
        Ok(match &self.config.to {
            To::Udp(address) => Conn::Udp(super::sockets::udp(address)?),
            To::Tcp(address) => Conn::Tcp(TcpStream::connect(address)?),
            To::Tls(address) => {
                let config = self
                    .tls
                    .clone()
                    .ok_or_else(|| io::Error::other("no TLS configuration"))?;
                let host = address
                    .rsplit_once(':')
                    .map_or(address.as_str(), |(host, _)| host)
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                let name = rustls::pki_types::ServerName::try_from(host.to_string())
                    .map_err(|e| io::Error::other(format!("{host}: {e}")))?;
                let conn = rustls::ClientConnection::new(config, name).map_err(io::Error::other)?;
                Conn::Tls(Box::new(rustls::StreamOwned::new(
                    conn,
                    TcpStream::connect(address)?,
                )))
            }
            To::Unix(path) => Conn::Stream(UnixStream::connect(path)?),
            To::Unixgram(path) => {
                let s = UnixDatagram::unbound()?;
                s.connect(path)?;
                Conn::Datagram(s)
            }
            To::Local => {
                for path in LOCAL {
                    if let Ok(s) = UnixDatagram::unbound().and_then(|s| s.connect(path).map(|()| s)) {
                        return Ok(Conn::Datagram(s));
                    }
                }
                for path in LOCAL {
                    if let Ok(s) = UnixStream::connect(path) {
                        return Ok(Conn::Stream(s));
                    }
                }
                return Err(io::Error::other(format!(
                    "no syslog here: none of {} takes a connection",
                    LOCAL.join(", ")
                )));
            }
        })
    }
}

impl Syslog {
    /// Sends `line` as a message; where its receiver refuses a datagram so long, in halves,
    /// each a message of its own, down to what every receiver takes: none of it lost, and
    /// none left waiting for a receiver that never takes it. moby's driver sends a line's
    /// 16 KiB parts whole, and drops one refused.
    fn send_line(&mut self, stream: u8, at: u64, zone: &Local, line: &[u8]) -> io::Result<()> {
        let bytes = self.config.message(&self.from, stream, at, zone, line);
        match self.write(&bytes) {
            Err(e) if super::sockets::too_large(&e) && bytes.len() > EVERY_RECEIVER && line.len() > 1 => {
                let (first, rest) = line.split_at(line.len() / 2);
                self.send_line(stream, at, zone, first)?;
                self.send_line(stream, at, zone, rest)
            }
            written => written,
        }
    }

    /// Writes `bytes` on its connection, made again once if a write on it fails.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(conn) = &mut self.conn
            && conn.write(bytes).is_ok()
        {
            return Ok(());
        }
        self.conn = None;
        let mut conn = self.connect()?;
        conn.write(bytes)?;
        self.conn = Some(conn);
        Ok(())
    }
}

impl Driver for Syslog {
    fn send(&mut self, m: &Message<'_>, _wait: &dyn Fn(std::time::Duration) -> bool) -> io::Result<()> {
        // moby's syslogger sends no empty line.
        if m.line.is_empty() {
            return Ok(());
        }
        let zone = Local::of(m.at).ok_or_else(|| io::Error::other("a time localtime(3) cannot give"))?;
        self.send_line(m.stream, m.at, &zone, m.line)
    }
}
