//! Streams between guest vsock ports and host Unix sockets, with Firecracker's mapping
//! (firecracker docs/vsock.md):
//! - A host client connects to the device's socket, writes `CONNECT <port>\n`, and reads
//!   `OK <host port>\n` once the guest accepts. Then the socket is the stream.
//! - A guest connection to host port P reaches the socket `<path>_P`.
//!
//! Host ports the VM's own process serves ([`VsockHost::ports`]) are reached without a
//! socket file: a guest connection to one is one end of a socket pair, and the other end
//! goes to the port's sender. Each takes one connection; later ones are refused. Without a path, the device takes no host clients and other
//! host ports refuse.
//!
//! A snapshot keeps the streams the guest may still hold ([`Saved`]). The restored copy
//! resets each with an RST, ahead of every other packet on the RX queue, which the
//! Linux driver handles in order. A TRANSPORT_RESET event would come on the event queue
//! instead, in a work item the driver runs apart from RX (virtio_transport.c: event_work
//! and rx_work). So it could arrive after a new connection's RESPONSE, and reset that
//! connection too (docs/design/architecture.md D12).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use super::Span;
use super::conn::Conn;
use super::packet::{HOST_CID, Header, TYPE_STREAM, op};
use super::poll::{Interest, Ready};
use crate::debug;
use crate::memory::GuestMemory;
use crate::snapshot::codec::{self, Reader, Writer};
use crate::vm::VsockHost;

/// Open connections and pending handshakes together (Firecracker's MAX_CONNECTIONS).
const MAX_CONNECTIONS: usize = 1023;
/// RSTs owed for packets that matched no connection, beyond which more are dropped.
const MAX_STRAY_RSTS: usize = 256;
/// The shortest handshake line, `CONNECT 0\n`. Reading this much first, then a byte at a
/// time, never consumes data a client sends after its line.
const MIN_HANDSHAKE: usize = 10;
const MAX_HANDSHAKE: usize = 32;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// Host ports for host-initiated connections come from [2^30, 2^31), as in Firecracker.
const LOCAL_PORT_BASE: u32 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    local_port: u32,
    peer_port: u32,
}

/// What a poll entry refers to.
#[derive(Debug, Clone, Copy)]
pub enum Token {
    Listener,
    Handshake(usize),
    Conn(Key),
}

struct Handshake {
    stream: UnixStream,
    line: Vec<u8>,
    deadline: Instant,
}

/// What a snapshot keeps of a muxer: the streams the guest may still hold, and where
/// host port allocation had got to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Saved {
    /// `(host port, guest port)` of each connection, and of each RST still owed.
    ports: Vec<(u32, u32)>,
    last_local_port: u32,
}

impl Saved {
    pub fn write(&self, w: &mut Writer) {
        w.seq(&self.ports, |w, &(local, peer)| {
            w.u32(local);
            w.u32(peer);
        });
        w.u32(self.last_local_port);
    }

    pub fn read(r: &mut Reader<'_>) -> codec::Result<Saved> {
        let ports = r.seq(MAX_CONNECTIONS + MAX_STRAY_RSTS, 8, |r| Ok((r.u32()?, r.u32()?)))?;
        Ok(Saved {
            ports,
            last_local_port: r.u32()?,
        })
    }
}

pub struct Muxer {
    guest_cid: u64,
    /// The device's socket, where host clients dial, if it has one.
    listener: Option<(PathBuf, UnixListener)>,
    /// Host ports served in this process.
    served: HashMap<u32, Sender<UnixStream>>,
    handshakes: Vec<Handshake>,
    conns: HashMap<Key, Conn>,
    /// Connections with packets for the guest, served in turn.
    rxq: VecDeque<Key>,
    /// RSTs for packets that matched no connection: (host port, guest port).
    stray_rsts: VecDeque<(u32, u32)>,
    local_ports: HashSet<u32>,
    last_local_port: u32,
}

impl std::fmt::Debug for Muxer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Muxer")
            .field("path", &self.listener.as_ref().map(|(path, _)| path))
            .field("conns", &self.conns.len())
            .finish_non_exhaustive()
    }
}

impl Muxer {
    /// Serves `host`'s ports, and listens for host clients at its path, which must not
    /// exist yet.
    pub fn new(host: VsockHost, guest_cid: u64) -> io::Result<Muxer> {
        let listener = match host.path {
            Some(path) => {
                // Bound here, or by the spawner where this process may not (App Sandbox).
                let listener = crate::platform::listen_unix(&path)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
                listener.set_nonblocking(true)?;
                Some((path, listener))
            }
            None => None,
        };
        Ok(Muxer {
            guest_cid,
            listener,
            served: host.ports.into_iter().collect(),
            handshakes: Vec::new(),
            conns: HashMap::new(),
            rxq: VecDeque::new(),
            stray_rsts: VecDeque::new(),
            local_ports: HashSet::new(),
            last_local_port: LOCAL_PORT_BASE - 1,
        })
    }

    /// Drops every connection and handshake; host clients see their sockets close.
    pub fn reset(&mut self) {
        self.handshakes.clear();
        self.conns.clear();
        self.rxq.clear();
        self.stray_rsts.clear();
        self.local_ports.clear();
    }

    /// The streams the guest may hold, for a snapshot.
    pub fn saved(&self) -> Saved {
        let conns = self.conns.keys().map(|k| (k.local_port, k.peer_port));
        Saved {
            ports: self.stray_rsts.iter().copied().chain(conns).collect(),
            last_local_port: self.last_local_port,
        }
    }

    /// Continues from a snapshot of another host's muxer. Every stream the guest may still
    /// hold is reset before any other packet reaches it. Their host ports are never
    /// reused: the guest may keep a stale socket, and Linux drops a REQUEST that matches
    /// one that is closing (virtio_transport_recv_disconnecting).
    pub fn restore(&mut self, saved: Saved) {
        for &(local_port, _) in &saved.ports {
            self.local_ports.insert(local_port);
        }
        self.stray_rsts.extend(saved.ports);
        self.last_local_port = saved.last_local_port;
    }

    pub fn has_pending_rx(&self) -> bool {
        !self.stray_rsts.is_empty() || !self.rxq.is_empty()
    }

    fn open(&self) -> usize {
        self.conns.len() + self.handshakes.len()
    }

    fn stray_rst(&mut self, local_port: u32, peer_port: u32) {
        if self.stray_rsts.len() < MAX_STRAY_RSTS {
            self.stray_rsts.push_back((local_port, peer_port));
        }
    }

    fn enqueue(&mut self, key: Key) {
        if let Some(c) = self.conns.get_mut(&key)
            && c.has_pending_rx()
            && !c.queued
        {
            c.queued = true;
            self.rxq.push_back(key);
        }
    }

    fn remove(&mut self, key: Key) {
        if let Some(c) = self.conns.remove(&key)
            && c.allocated_port
        {
            self.local_ports.remove(&key.local_port);
        }
    }

    /// Handles a packet the guest sent.
    pub fn on_guest_packet(&mut self, h: &Header, payload: &[Span], mem: &GuestMemory) {
        if h.dst_cid != HOST_CID || h.src_cid != self.guest_cid {
            debug!(
                "vsock: dropping a packet from cid {} to cid {}",
                h.src_cid, h.dst_cid
            );
            return;
        }
        if h.kind != TYPE_STREAM {
            self.stray_rst(h.dst_port, h.src_port);
            return;
        }
        let key = Key {
            local_port: h.dst_port,
            peer_port: h.src_port,
        };
        let Some(conn) = self.conns.get_mut(&key) else {
            match h.op {
                op::REQUEST => self.guest_connect(key, h),
                op::RST => {}
                _ => self.stray_rst(key.local_port, key.peer_port),
            }
            return;
        };
        if h.op == op::RST {
            self.remove(key);
            return;
        }
        conn.on_guest_packet(h, payload, mem);
        self.enqueue(key);
    }

    /// The guest connects to host port `key.local_port`: one this process serves, or the
    /// socket `<path>_<port>`.
    fn guest_connect(&mut self, key: Key, request: &Header) {
        if self.open() >= MAX_CONNECTIONS {
            debug!(
                "vsock: connection limit reached; refusing guest port {}",
                key.peer_port
            );
            self.stray_rst(key.local_port, key.peer_port);
            return;
        }
        // A served port takes one connection: its owner dials it before anything else in
        // the guest runs, and a later dial, by a workload, is refused (AGENTFILE_ARCH.md
        // §9.7).
        let connected = match self.served.remove(&key.local_port) {
            Some(port) => serve(&port),
            None => match &self.listener {
                Some((path, _)) => {
                    let mut target = OsString::from(path.as_os_str());
                    target.push(format!("_{}", key.local_port));
                    let target = Path::new(&target);
                    // Dialled here, or by the spawner where this process may not.
                    match crate::platform::dial_unix(target) {
                        Some(dialled) => dialled.and_then(|s| s.set_nonblocking(true).map(|()| s)),
                        None => connect_nonblocking(target),
                    }
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", target.display())))
                }
                None => Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "no such host port",
                )),
            },
        };
        match connected {
            Ok(stream) => {
                self.conns
                    .insert(key, Conn::guest_initiated(stream, self.guest_cid, request));
                self.enqueue(key);
            }
            Err(e) => {
                debug!("vsock: guest connect to host port {}: {e}", key.local_port);
                self.stray_rst(key.local_port, key.peer_port);
            }
        }
    }

    /// The next packet for the guest, if any. `space` is the guest buffer after the header.
    pub fn next_rx(&mut self, space: &[Span]) -> Option<Header> {
        if let Some((local_port, peer_port)) = self.stray_rsts.pop_front() {
            return Some(Header {
                src_cid: HOST_CID,
                dst_cid: self.guest_cid,
                src_port: local_port,
                dst_port: peer_port,
                len: 0,
                kind: TYPE_STREAM,
                op: op::RST,
                flags: 0,
                buf_alloc: 0,
                fwd_cnt: 0,
            });
        }
        for _ in 0..self.rxq.len() {
            let key = self.rxq.pop_front()?;
            let Some(conn) = self.conns.get_mut(&key) else {
                continue;
            };
            let h = conn.next_rx(space);
            if conn.has_pending_rx() {
                self.rxq.push_back(key);
            } else {
                conn.queued = false;
            }
            if let Some(h) = h {
                if h.op == op::RST {
                    self.remove(key);
                }
                return Some(h);
            }
        }
        None
    }

    /// Adds what the sockets wait for.
    pub fn interests(&self, out: &mut Vec<Interest<Option<Token>>>) {
        let mut add = |fd: i32, read: bool, write: bool, token: Token| {
            out.push(Interest {
                fd,
                read,
                write,
                token: Some(token),
            });
        };
        if let Some((_, listener)) = &self.listener
            && self.open() < MAX_CONNECTIONS
        {
            add(listener.as_raw_fd(), true, false, Token::Listener);
        }
        for (i, h) in self.handshakes.iter().enumerate() {
            add(h.stream.as_raw_fd(), true, false, Token::Handshake(i));
        }
        for (key, c) in &self.conns {
            let (read, write) = c.interest();
            if read || write {
                add(c.fd(), read, write, Token::Conn(*key));
            }
        }
    }

    /// Acts on what became ready.
    pub fn on_events(&mut self, events: &[Ready<Option<Token>>]) {
        let mut ready_handshakes = Vec::new();
        for e in events {
            match e.token {
                None => {}
                Some(Token::Listener) => self.accept(),
                Some(Token::Handshake(i)) => ready_handshakes.push(i),
                Some(Token::Conn(key)) => {
                    let Some(c) = self.conns.get_mut(&key) else {
                        continue;
                    };
                    if e.write {
                        c.on_writable();
                    }
                    if e.read {
                        c.on_readable();
                    }
                    self.enqueue(key);
                }
            }
        }
        // Highest index first, so removals do not shift the ones still to handle.
        ready_handshakes.sort_unstable_by(|a, b| b.cmp(a));
        for i in ready_handshakes {
            self.handshake(i);
        }
    }

    fn accept(&mut self) {
        let Some((_, listener)) = &self.listener else {
            return;
        };
        while self.handshakes.len() + self.conns.len() < MAX_CONNECTIONS {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        debug!("vsock: host connection: {e}");
                        continue;
                    }
                    self.handshakes.push(Handshake {
                        stream,
                        line: Vec::with_capacity(MAX_HANDSHAKE),
                        deadline: Instant::now() + HANDSHAKE_TIMEOUT,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        debug!("vsock: accept: {e}");
                    }
                    return;
                }
            }
        }
    }

    fn handshake(&mut self, i: usize) {
        let Some(h) = self.handshakes.get_mut(i) else {
            return;
        };
        match read_handshake(h) {
            Ok(None) => {}
            Ok(Some(peer_port)) => {
                let h = self.handshakes.swap_remove(i);
                let local_port = self.allocate_local_port();
                let key = Key {
                    local_port,
                    peer_port,
                };
                self.conns.insert(
                    key,
                    Conn::host_initiated(h.stream, self.guest_cid, local_port, peer_port),
                );
                self.enqueue(key);
            }
            Err(e) => {
                debug!("vsock: host handshake: {e}");
                self.handshakes.swap_remove(i);
            }
        }
    }

    fn allocate_local_port(&mut self) -> u32 {
        // At most MAX_CONNECTIONS are taken, so a free port is always near.
        loop {
            self.last_local_port =
                LOCAL_PORT_BASE | (self.last_local_port.wrapping_add(1) & (LOCAL_PORT_BASE - 1));
            if self.local_ports.insert(self.last_local_port) {
                return self.last_local_port;
            }
        }
    }

    /// The earliest deadline of any handshake or connection.
    pub fn next_deadline(&self) -> Option<Instant> {
        let handshakes = self.handshakes.iter().map(|h| h.deadline);
        let conns = self.conns.values().filter_map(Conn::expiry);
        handshakes.chain(conns).min()
    }

    /// Drops late handshakes and resets late connections.
    pub fn expire(&mut self, now: Instant) {
        self.handshakes.retain(|h| h.deadline > now);
        let late: Vec<Key> = self
            .conns
            .iter_mut()
            .filter_map(|(k, c)| c.expire(now).then_some(*k))
            .collect();
        for key in late {
            self.enqueue(key);
        }
    }
}

impl Drop for Muxer {
    fn drop(&mut self) {
        if let Some((path, _)) = &self.listener {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A connection to a port this process serves: the muxer's end of a socket pair, whose
/// other end goes to the port's sender. Refused once nothing receives there.
fn serve(port: &Sender<UnixStream>) -> io::Result<UnixStream> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_nonblocking(true)?;
    port.send(theirs)
        .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "no longer served"))?;
    Ok(ours)
}

/// Reads a handshake line without consuming anything after it. `Some(port)` once the line
/// is complete and valid; `None` while more is to come.
fn read_handshake(h: &mut Handshake) -> io::Result<Option<u32>> {
    loop {
        let want = MIN_HANDSHAKE.saturating_sub(h.line.len()).max(1);
        let mut buf = [0u8; MIN_HANDSHAKE];
        let dst = buf.get_mut(..want).unwrap_or_default();
        match h.stream.read(dst) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                h.line.extend(dst.iter().take(n));
                if h.line.contains(&b'\n') {
                    return parse_connect(&h.line)
                        .map(Some)
                        .ok_or_else(|| io::Error::other("expected `CONNECT <port>`"));
                }
                if h.line.len() >= MAX_HANDSHAKE {
                    return Err(io::Error::other("handshake line too long"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// `CONNECT <port>` followed by a newline (case-insensitive, as Firecracker).
fn parse_connect(line: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(line).ok()?;
    let (command, rest) = text.split_once('\n')?;
    if !rest.is_empty() {
        return None;
    }
    let mut words = command.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("connect") {
        return None;
    }
    let port = words.next()?.parse().ok()?;
    words.next().is_none().then_some(port)
}

/// Connects without blocking: a host listener whose backlog is full refuses at once
/// instead of stalling the device.
fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero sockaddr_un is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long for sockaddr_un",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh socket that nothing else owns.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fcntl(2) on our own descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    // SAFETY: `addr` is a valid sockaddr_un and the length is its size.
    let rc = unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(stream)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn parses_connect_lines_and_nothing_else() {
        assert_eq!(parse_connect(b"CONNECT 1234\n"), Some(1234));
        assert_eq!(parse_connect(b"connect 0\n"), Some(0));
        assert_eq!(parse_connect(b"CONNECT  52 \r\n"), Some(52));
        for bad in [
            &b"CONNECT\n"[..],
            b"CONNECT x\n",
            b"CONNECT 1 2\n",
            b"LISTEN 5\n",
            b"CONNECT 4294967296\n",
            b"CONNECT 1\nrest",
        ] {
            assert_eq!(parse_connect(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn handshakes_arrive_in_pieces_and_leave_later_data_unread() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut h = Handshake {
            stream: server,
            line: Vec::new(),
            deadline: Instant::now(),
        };
        client.write_all(b"CONN").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), None);
        client.write_all(b"ECT 77").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), None);
        client.write_all(b"\npayload").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), Some(77));
        let mut rest = [0u8; 7];
        h.stream.read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"payload");
    }

    /// A restored muxer resets what the snapshot held before the guest hears anything
    /// else, even a connection it made before the first packet went out, and allocates
    /// host ports past the snapshot's without reusing a held one.
    #[test]
    fn restores_reset_held_streams_first_and_keep_their_ports() {
        let dir = std::env::temp_dir().join(format!("shards-vsock-restore-{}", std::process::id()));
        let mut m = Muxer::new(VsockHost::at(dir.clone()), 3).unwrap();
        let mut target = dir.clone().into_os_string();
        target.push("_5000");
        let host = UnixListener::bind(&target).unwrap();
        let held = [(5000, 49_152), (LOCAL_PORT_BASE + 7, 1234)];
        m.restore(Saved {
            ports: held.to_vec(),
            last_local_port: LOCAL_PORT_BASE + 6,
        });

        // The guest dials host port 5000 from a new port.
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        let request = Header {
            src_cid: 3,
            dst_cid: HOST_CID,
            src_port: 49_153,
            dst_port: 5000,
            len: 0,
            kind: TYPE_STREAM,
            op: op::REQUEST,
            flags: 0,
            buf_alloc: 1 << 16,
            fwd_cnt: 0,
        };
        m.on_guest_packet(&request, &[], &mem);
        assert!(host.accept().is_ok(), "the guest's connection reached the host");
        let mut sent = Vec::new();
        while let Some(h) = m.next_rx(&[]) {
            sent.push((h.op, h.src_port, h.dst_port));
        }
        assert_eq!(
            sent,
            [
                (op::RST, 5000, 49_152),
                (op::RST, LOCAL_PORT_BASE + 7, 1234),
                (op::RESPONSE, 5000, 49_153),
            ]
        );
        assert_eq!(m.allocate_local_port(), LOCAL_PORT_BASE + 8);
        let _ = std::fs::remove_file(&target);
    }

    /// A guest's request for host `port`, from guest port `from`.
    fn request(port: u32, from: u32) -> Header {
        Header {
            src_cid: 3,
            dst_cid: HOST_CID,
            src_port: from,
            dst_port: port,
            len: 0,
            kind: TYPE_STREAM,
            op: op::REQUEST,
            flags: 0,
            buf_alloc: 1 << 16,
            fwd_cnt: 0,
        }
    }

    /// A port this process serves gets the guest's connection as a connected socket, with
    /// no socket file; a port nothing serves, with no path to dial, a served port whose
    /// receiver is gone, and a second connection to a served port, are refused with an RST.
    #[test]
    fn served_ports_get_connections_without_socket_files() {
        let (sender, arrived) = std::sync::mpsc::channel();
        let (gone, dropped) = std::sync::mpsc::channel();
        drop(dropped);
        let host = VsockHost {
            path: None,
            ports: vec![(52, sender), (53, gone)],
        };
        let mut m = Muxer::new(host, 3).unwrap();
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        for (port, from) in [(52, 49_152), (7, 49_153), (53, 49_154), (52, 49_155)] {
            m.on_guest_packet(&request(port, from), &[], &mem);
        }
        let mut sent = Vec::new();
        while let Some(h) = m.next_rx(&[]) {
            sent.push((h.op, h.src_port, h.dst_port));
        }
        sent.sort_unstable();
        let mut expected = vec![
            (op::RESPONSE, 52, 49_152),
            (op::RST, 7, 49_153),
            (op::RST, 53, 49_154),
            (op::RST, 52, 49_155),
        ];
        expected.sort_unstable();
        assert_eq!(sent, expected);
        let mut theirs = arrived.try_recv().unwrap();
        assert!(arrived.try_recv().is_err(), "one connection, one socket");
        // The pair is connected: what the host writes is the muxer's to read.
        theirs.write_all(b"x").unwrap();
        let key = Key {
            local_port: 52,
            peer_port: 49_152,
        };
        assert!(m.conns.contains_key(&key));
        let mut interests = Vec::new();
        m.interests(&mut interests);
        assert!(
            !interests.iter().any(|i| matches!(i.token, Some(Token::Listener))),
            "no path, nothing to listen on"
        );
    }

    #[test]
    fn saved_streams_round_trip() {
        let saved = Saved {
            ports: vec![(5000, 49_152), (LOCAL_PORT_BASE, 1234)],
            last_local_port: LOCAL_PORT_BASE + 3,
        };
        let mut w = Writer::default();
        saved.write(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(Saved::read(&mut r).unwrap(), saved);
        r.finish().unwrap();
    }

    #[test]
    fn local_ports_stay_in_range_and_unique() {
        let dir = std::env::temp_dir().join(format!("shards-vsock-ports-{}", std::process::id()));
        let mut m = Muxer::new(VsockHost::at(dir.clone()), 3).unwrap();
        m.last_local_port = u32::MAX - 1;
        let a = m.allocate_local_port();
        let b = m.allocate_local_port();
        assert!((LOCAL_PORT_BASE..1 << 31).contains(&a) && (LOCAL_PORT_BASE..1 << 31).contains(&b));
        assert_ne!(a, b);
    }
}
