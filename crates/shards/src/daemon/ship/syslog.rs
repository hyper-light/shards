//! The syslog driver's sending (D163): its messages (logging/syslog.rs) over UDP, TCP,
//! TLS or a unix socket, as srslog sends them: on a connection made as the first message
//! goes and kept, made again once where a write on it fails (srslog writer.go,
//! writeAndRetry); the host's own syslog, where no address is given, at the first of its
//! sockets that takes a connection (srslog_unix.go, unixSyslog). Over a stream, a message
//! is held until its receiver has it, and written again on the next connection should one
//! end first (stream.rs), where srslog's are lost.

use std::io;
use std::net::UdpSocket;
#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixStream};
#[cfg(unix)]
use std::path::PathBuf;

use super::stream::{self, Stream};
use super::{Driver, Message, Wait};
use crate::logging::syslog::{Config, From, Local, To};

/// The host's own syslog's sockets, tried in turn (srslog unixSyslog's logPaths).
#[cfg(unix)]
const LOCAL: [&str; 3] = ["/dev/log", "/var/run/syslog", "/var/run/log"];

/// The largest message every syslog receiver over UDP takes (RFC 5426 §3.2: 480 octets
/// over IPv4): one no larger that a receiver refuses is refused for another reason.
const EVERY_RECEIVER: usize = 480;

pub(super) struct Syslog {
    config: Config,
    from: From,
    /// Over TCP, TLS or a unix stream socket: its stream.
    stream: Option<Stream>,
    /// Over UDP or a unix datagram socket: its socket, made as a message goes, and again
    /// after one fails.
    datagrams: Option<Datagrams>,
}

enum Datagrams {
    Udp(UdpSocket),
    #[cfg(unix)]
    Unix(UnixDatagram),
}

impl Syslog {
    pub(super) fn new(config: Config, from: From) -> Result<Syslog, String> {
        let stream = match &config.to {
            To::Tcp(address) => Some(stream::To::Tcp(address.clone())),
            To::Tls(address) => {
                // The platform's roots, the CA given, and the client's certificate.
                let (roots, client) = config.tls_material()?;
                let tls = shards_registry::tls::client_config(roots, client).map_err(|e| e.to_string())?;
                Some(stream::To::Tls(address.clone(), tls))
            }
            To::Unix(path) => Some(stream::To::Unix(path.clone())),
            To::Udp(_) | To::Unixgram(_) | To::Local => None,
        };
        Ok(Syslog {
            config,
            from,
            stream: stream.map(|to| Stream::new(to, [None; 2])),
            datagrams: None,
        })
    }

    /// Makes its datagram socket; or, where the host's own syslog takes only a stream, its
    /// stream.
    fn open(&mut self) -> io::Result<()> {
        let datagrams = match &self.config.to {
            To::Udp(address) => Datagrams::Udp(super::sockets::udp(address)?),
            #[cfg(unix)]
            To::Unixgram(path) => {
                let s = UnixDatagram::unbound()?;
                s.connect(path)?;
                Datagrams::Unix(s)
            }
            // Windows' AF_UNIX is SOCK_STREAM's alone, and it keeps no syslog of its own.
            #[cfg(windows)]
            To::Unixgram(path) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{}: Windows has no unix datagram sockets", path.display()),
                ));
            }
            #[cfg(windows)]
            To::Local => {
                return Err(io::Error::other(
                    "no syslog here: Windows keeps none of its own; syslog-address names one",
                ));
            }
            #[cfg(unix)]
            To::Local => {
                for path in LOCAL {
                    if let Ok(s) = UnixDatagram::unbound().and_then(|s| s.connect(path).map(|()| s)) {
                        self.datagrams = Some(Datagrams::Unix(s));
                        return Ok(());
                    }
                }
                for path in LOCAL {
                    if let Ok(s) = UnixStream::connect(path) {
                        self.stream = Some(Stream::unix(PathBuf::from(path), s));
                        return Ok(());
                    }
                }
                return Err(io::Error::other(format!(
                    "no syslog here: none of {} takes a connection",
                    LOCAL.join(", ")
                )));
            }
            To::Tcp(_) | To::Tls(_) | To::Unix(_) => {
                return Err(io::Error::other("a stream's address, for a datagram"));
            }
        };
        self.datagrams = Some(datagrams);
        Ok(())
    }

    /// Writes `bytes` as a message: on its stream, or as a datagram.
    fn write_once(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.stream.is_none() && self.datagrams.is_none() {
            self.open()?;
        }
        if let Some(stream) = &mut self.stream {
            return stream.send(bytes);
        }
        let sent = match &self.datagrams {
            Some(Datagrams::Udp(s)) => super::sockets::send_datagram(s, bytes),
            #[cfg(unix)]
            Some(Datagrams::Unix(s)) => super::sockets::send_datagram(s, bytes),
            None => Err(io::Error::from(io::ErrorKind::NotConnected)),
        };
        if sent.is_err() {
            self.datagrams = None;
        }
        sent
    }

    /// Writes `bytes`, once more on a new connection where that fails, as srslog's
    /// writeAndRetry does; a datagram its receiver takes none so long of is not tried
    /// again whole.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.write_once(bytes) {
            Err(e) if !super::sockets::too_large(&e) => self.write_once(bytes),
            written => written,
        }
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
}

impl Driver for Syslog {
    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        // moby's syslogger sends no empty line.
        if m.line.is_empty() {
            return Ok(());
        }
        let zone = Local::of(m.at).ok_or_else(|| io::Error::other("a time localtime(3) cannot give"))?;
        self.send_line(m.stream, m.at, &zone, m.line)
    }

    fn undelivered(&mut self) -> usize {
        self.stream.as_mut().map_or(0, Stream::undelivered)
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.stream.as_mut().map_or(Ok(()), |s| s.close(wait))
    }
}
