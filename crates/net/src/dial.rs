//! What an AppContainer keeps a network process from, on Windows (D142): the host's own
//! addresses and its loopback (PM M204), and a build's proxy's socket (PM M201). Its
//! spawner's dialer connects to them for it and shares each connection once connected
//! (WSADuplicateSocketW, PM M203), as Windows documents a shared socket's use: one process
//! makes the connection, another exchanges data on it. Each ask is numbered and answered as
//! its connect ends, so that no connection waits on another's and this process serves the
//! guest meanwhile: the guest's SYN waits for its answer as it would for a far end's SYN-ACK.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::net::{IpAddr, SocketAddr, TcpStream};

use shards_ipc::kind;

use crate::sys::{AsRawSock as _, RawSock, UnixStream};
use crate::tcp::{HostSock, Key, Syn};

/// A connection asked for and not yet given: the guest's SYN it answers.
#[derive(Debug)]
pub struct Dialing {
    pub key: Key,
    pub syn: Syn,
    pub isn: u32,
    /// Its flow is DNS to the host's resolver.
    pub dns: bool,
    /// Its host end is the build's proxy, not a TCP connection.
    proxy: bool,
}

/// The spawner's dialer, as this process holds it.
#[derive(Debug)]
pub struct Dialer {
    link: UnixStream,
    incoming: shards_ipc::Incoming,
    /// What the link has yet to take of what was sent on it.
    unsent: Vec<u8>,
    /// The host's own addresses, as its spawner gave them.
    host: Vec<IpAddr>,
    next: u32,
    asked: HashMap<u32, Dialing>,
    keys: HashMap<Key, u32>,
}

impl Dialer {
    /// The dialer at the far end of `link`, for the host's addresses `host`.
    pub fn new(link: UnixStream, host: Vec<IpAddr>) -> io::Result<Dialer> {
        link.set_nonblocking(true)?;
        Ok(Dialer {
            link,
            incoming: shards_ipc::Incoming::default(),
            unsent: Vec::new(),
            host,
            next: 0,
            asked: HashMap::new(),
            keys: HashMap::new(),
        })
    }

    pub fn sock(&self) -> RawSock {
        self.link.raw_sock()
    }

    /// Whether a connection to `to` goes through it: the host's own addresses and its
    /// loopback.
    pub fn reaches(&self, to: IpAddr) -> bool {
        to.is_loopback() || self.host.contains(&to)
    }

    /// Whether a connection for `key` is asked for already: the guest's SYN again.
    pub fn asking(&self, key: &Key) -> bool {
        self.keys.contains_key(key)
    }

    /// Whether the link has yet to take some of what was sent on it.
    pub fn wants_write(&self) -> bool {
        !self.unsent.is_empty()
    }

    /// Asks for a connection for the guest's `syn` to `key`'s far end: to `to`, or to the
    /// build's proxy where none.
    pub fn ask(&mut self, to: Option<SocketAddr>, key: Key, syn: Syn, isn: u32, dns: bool) -> io::Result<()> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let mut payload = id.to_be_bytes().to_vec();
        if let Some(to) = to {
            match to.ip() {
                IpAddr::V4(v4) => payload.extend(v4.octets()),
                IpAddr::V6(v6) => payload.extend(v6.octets()),
            }
            payload.extend(to.port().to_be_bytes());
        }
        self.send(kind::DIAL, &payload)?;
        self.keys.insert(key, id);
        self.asked.insert(
            id,
            Dialing {
                key,
                syn,
                isn,
                dns,
                proxy: to.is_none(),
            },
        );
        Ok(())
    }

    fn send(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        shards_ipc::write_message(&mut self.unsent, kind, payload)?;
        self.flush()
    }

    /// Sends what the link takes now of what is unsent.
    pub fn flush(&mut self) -> io::Result<()> {
        while !self.unsent.is_empty() {
            match (&self.link).write(&self.unsent) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.unsent.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The answers come whole, each with what it asked for: its connection, connected, or
    /// why there is none; and whether the dialer is still there. Once it is gone, every
    /// connection still asked for is answered with that.
    pub fn answers(&mut self) -> (Vec<(Dialing, io::Result<HostSock>)>, bool) {
        let mut out = Vec::new();
        loop {
            let m = match self.incoming.take(&self.link) {
                Ok(shards_ipc::Took::Message(m)) => m,
                Ok(shards_ipc::Took::Partial) => return (out, true),
                Ok(shards_ipc::Took::Ended) | Err(_) => break,
            };
            let Some((id, rest)) = m.payload.split_first_chunk::<4>() else {
                break;
            };
            let id = u32::from_be_bytes(*id);
            let Some(d) = self.asked.remove(&id) else {
                break;
            };
            self.keys.remove(&d.key);
            let got = match m.kind {
                kind::GRANTED => {
                    let adopted = self.adopt(&d, rest);
                    // Its spawner's copy goes once this process has its own, or could not
                    // take it.
                    let _ = self.send(kind::TAKEN, &id.to_be_bytes());
                    adopted
                }
                kind::ERR => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    String::from_utf8_lossy(rest).into_owned(),
                )),
                _ => {
                    out.push((
                        d,
                        Err(io::Error::other("the dialer answered with something else")),
                    ));
                    break;
                }
            };
            out.push((d, got));
        }
        self.keys.clear();
        out.extend(self.asked.drain().map(|(_, d)| {
            (
                d,
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "the dialer went")),
            )
        }));
        (out, false)
    }

    fn adopt(&self, d: &Dialing, info: &[u8]) -> io::Result<HostSock> {
        if d.proxy {
            let sock = UnixStream::from_shared(info)?;
            sock.set_nonblocking(true)?;
            return Ok(HostSock::Unix(sock));
        }
        let sock = TcpStream::from(shards_ipc::adopt_socket(info)?);
        sock.set_nonblocking(true)?;
        Ok(HostSock::Tcp(sock))
    }
}
