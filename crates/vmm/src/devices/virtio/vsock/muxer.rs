//! Streams between guest vsock ports and host Unix sockets, with Firecracker's mapping
//! (firecracker docs/vsock.md):
//! - A host client connects to the device's socket, writes `CONNECT <port>\n`, and reads
//!   `OK <host port>\n` once the guest accepts. Then the socket is the stream.
//! - A guest connection to host port P reaches the socket `<path>_P`.
//! - A managed machine's host client may write `RATE <milli>\n` in place of `CONNECT` (D255,
//!   `shards_abi::rate`): its connection goes to the machine's rate ([`VsockHost::rate`]),
//!   whose owner answers on it. Only a dial on the socket says one; a machine without a rate
//!   closes such a dial unanswered.
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
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use super::Span;
use super::conn::{Conn, WINDOW_LENT};
use super::packet::{HOST_CID, Header, TYPE_STREAM, op};
use super::poll::{Interest, Ready};
use crate::debug;
use crate::memory::GuestMemory;
use crate::platform::{AsRawSocket, UnixListener, UnixStream};
use crate::snapshot::codec::{self, Reader, Writer};
use crate::vm::VsockHost;

/// RSTs owed, at most, for the largest rings a guest may negotiate ([`stray_rsts_bound`]).
const MAX_STRAY_RSTS: usize = stray_rsts_bound(super::QUEUE_SIZE, super::QUEUE_SIZE);
/// The shortest `CONNECT` line, `CONNECT 0\n`. Reading this much first, then a byte at a
/// time, never consumes data a client sends after its line. A managed machine's `RATE` line
/// may be shorter (`RATE 0\n`), but nothing follows it: its client waits for its answer
/// (D255).
const MIN_HANDSHAKE: usize = handshake_len(decimal_digits(0), LF);
/// The longest handshake line: `CONNECT 4294967295\r\n`, or a managed machine's
/// `RATE 18446744073709551615\r\n` (`shards_abi::rate::ASK_MOST`), the longer. A longer one is
/// refused before its end: a port has at most `u32::MAX`'s digits, a rate `u64::MAX`'s, and
/// their words one space between them.
const MAX_HANDSHAKE: usize = longer(
    handshake_len(decimal_digits(u32::MAX), CRLF),
    shards_abi::rate::ASK_MOST,
);
/// Host ports for host-initiated connections come from [2^30, 2^31), as in Firecracker.
const LOCAL_PORT_BASE: u32 = 1 << 30;
/// What a connection's host socket is asked to buffer each way (SO_SNDBUF, SO_RCVBUF), as
/// a sender's bytes in flight are bounded by its send buffer (XNU bsd/kern/uipc_usrreq.c):
/// the smallest measured within the noise of the best with the most window, on
/// Hypervisor.framework. 256 MiB took 14.3 ms guest to host through 256 KiB socket buffers
/// and through 4 MiB ones, relayed 87.0 and 85.9, host to guest 85.7 and 85.5 (p50 of 11
/// rounds, r433), where macOS's own 8 KiB (net.local.stream.sendspace) took 355.1 guest to
/// host with the least window (r421; docs/research/measurements/vsock-window-v1).
const SOCKET_BUFFER: u32 = 256 << 10;
/// Whether this process sizes its host sockets ([`SOCKET_BUFFER`]): not on Linux, whose
/// confined VM process's seccomp filter allows no setsockopt (shards' confine.rs), so that
/// one would end it (SIGSYS), and whose own default, net.core.wmem_default's 212,992 bytes,
/// is about that size already.
const SIZES_SOCKETS: bool = cfg!(not(target_os = "linux"));

/// The handshake's words (firecracker docs/vsock.md: `CONNECT PORT\n`, PORT in decimal),
/// whose line a client's terminal may end with `\r\n` as well.
const CONNECT: &[u8] = b"CONNECT";
const SPACE: &[u8] = b" ";
const LF: &[u8] = b"\n";
const CRLF: &[u8] = b"\r\n";

/// The decimal digits of `n`.
const fn decimal_digits(mut n: u32) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// A handshake line's length: its words, a port of `digits` digits, and `end`.
const fn handshake_len(digits: usize, end: &[u8]) -> usize {
    CONNECT.len() + SPACE.len() + digits + end.len()
}

/// The longer of `a` and `b`.
const fn longer(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// A host client's handshake line: a guest's port to connect to, or a managed machine's
/// rate (D255).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Connect(u32),
    Rate(u64),
}

/// RSTs owed for packets that matched no connection, beyond which more are dropped, for a
/// guest whose RX ring holds `rx_ring` buffers and whose TX ring `tx_ring` (the sizes it
/// negotiated, virtio 1.3 §2.7, at most the device's). An RST answers one guest packet,
/// and a round takes at most a TX ring of those (`step`); it is a packet of its own, one RX
/// buffer each (`fill_rx`), and the RX ring holds at most `rx_ring` buffers at once, which
/// Linux's driver keeps full (net/vmw_vsock/virtio_transport.c `virtio_vsock_rx_fill`,
/// `while (vq->num_free)`). So a guest that takes its RSTs as they come never has more
/// owed than one round answers or one RX ring takes, whichever is more; past that, it is
/// not taking them. Linux bounds its own replies owed the same way, to a ring's size
/// (`virtio_transport_more_replies`).
pub(super) const fn stray_rsts_bound(rx_ring: u16, tx_ring: u16) -> usize {
    if rx_ring > tx_ring {
        rx_ring as usize
    } else {
        tx_ring as usize
    }
}

/// A muxer's slots ([`Muxer::slots`]): what this process's descriptor limit, raised as far
/// as it may go as a share process raises its own, leaves past the descriptors it holds,
/// its listener's and spare's among them, one to a connection, its host socket
/// (`shards_abi::bounds::connections`, D12). Where it cannot say what it holds, the limit
/// alone; where it cannot say its limit, no number: its descriptors bound them then, as
/// they bound a dial past what it held as the muxer was made ([`Muxer::refuse_one`]).
fn slots() -> usize {
    let limit = crate::platform::raise_descriptor_limit()
        .ok()
        .or_else(crate::platform::descriptor_limit);
    match limit {
        Some(limit) => {
            let held = crate::platform::open_descriptors().unwrap_or(0);
            shards_abi::bounds::connections(limit, held, 1)
        }
        None => usize::MAX,
    }
}

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
    #[cfg(windows)]
    Lingering(usize),
}

struct Handshake {
    stream: UnixStream,
    line: Vec<u8>,
    /// Its latest progress: its accept, then the latest bytes of its line. A pending
    /// handshake holds its slot against a newcomer on no clock: a client that is gone is seen
    /// at once, as its socket's end; one that is alive keeps its slot while no other wants
    /// it, and gives it up, silent longest first, when one does ([`Muxer::make_room`]). Its
    /// line is a local process's to write, as soon as the process runs, so a load that
    /// delays the process delays nothing else (D12).
    heard: Instant,
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
        // Whatever a save wrote, a host's with more slots than this one's too: as many as
        // the input's bytes hold, as `seq` refuses a count they cannot.
        let ports = r.seq(usize::MAX, 8, |r| Ok((r.u32()?, r.u32()?)))?;
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
    /// Host ports served in this process, each for its first connection.
    served: HashMap<u32, Sender<UnixStream>>,
    /// Host ports served in this process for every connection.
    every: HashMap<u32, crate::vm::ServedPort>,
    /// Where a host client's `RATE` line goes with its connection, on a managed machine
    /// ([`VsockHost::rate`], D255).
    rate: Option<Sender<(UnixStream, u64)>>,
    handshakes: Vec<Handshake>,
    conns: HashMap<Key, Conn>,
    /// Ended connections whose host sockets are held until their peers end their sides
    /// ([`Conn::linger`]).
    #[cfg(windows)]
    lingering: Vec<Conn>,
    /// Connections with packets for the guest, served in turn.
    rxq: VecDeque<Key>,
    /// RSTs for packets that matched no connection: (host port, guest port).
    stray_rsts: VecDeque<(u32, u32)>,
    /// How many [`stray_rsts`](Muxer::stray_rsts) may be owed: [`stray_rsts_bound`] of the
    /// guest's rings ([`Muxer::set_rings`]), of the largest it may negotiate until then.
    max_stray_rsts: usize,
    /// Open connections and pending handshakes together, at most (Firecracker's
    /// MAX_CONNECTIONS): what this process's descriptors leave as the muxer is made
    /// ([`slots`]). Past them a host's dial is refused ([`Muxer::take`]), and so is a
    /// guest's connection ([`Muxer::guest_connect`]).
    slots: usize,
    /// A descriptor held for a dial no other is left for ([`Muxer::refuse_one`]). None while
    /// it cannot be made again, as another took the one its refusal let go: the listener
    /// waits unpolled until a round makes it ([`Muxer::on_events`]), where a dial no
    /// descriptor is left for would keep it ready for ever.
    spare: Option<Spare>,
    local_ports: HashSet<u32>,
    last_local_port: u32,
    /// The device's lent window ([`WINDOW_LENT`]) its connections hold, summed: never more
    /// than it ([`Muxer::next_rx`]).
    lent: u64,
    /// The guest's memory, once the device is activated: what a ring port's connections'
    /// rings are attached in (D230).
    memory: Option<Arc<GuestMemory>>,
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
        // Made with the listener, so that the slots read below leave it its descriptor.
        let spare = match &listener {
            Some((path, listener)) => Some(
                spare(listener)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: its spare: {e}", path.display())))?,
            ),
            None => None,
        };
        Ok(Muxer {
            guest_cid,
            listener,
            served: host.ports.into_iter().collect(),
            every: host.every.into_iter().collect(),
            rate: host.rate,
            handshakes: Vec::new(),
            conns: HashMap::new(),
            #[cfg(windows)]
            lingering: Vec::new(),
            rxq: VecDeque::new(),
            stray_rsts: VecDeque::new(),
            max_stray_rsts: MAX_STRAY_RSTS,
            slots: slots(),
            spare,
            local_ports: HashSet::new(),
            last_local_port: LOCAL_PORT_BASE - 1,
            lent: 0,
            memory: None,
        })
    }

    /// Drops every connection and handshake; host clients see their sockets close.
    pub fn reset(&mut self) {
        self.handshakes.clear();
        self.conns.clear();
        self.lent = 0;
        #[cfg(windows)]
        self.lingering.clear();
        self.rxq.clear();
        self.stray_rsts.clear();
        self.local_ports.clear();
    }

    /// The streams the guest may hold, for a snapshot: its connections, then the resets
    /// owed, newest first. As many as a muxer never restored holds at most: its slots, and
    /// the resets [`stray_rst`](Muxer::stray_rst) keeps owed, a ring's worth, as Linux keeps
    /// its own replies owed (`virtio_transport_more_replies`). A restored muxer owes a reset
    /// for every stream its snapshot held, and saved again before they go out it keeps the
    /// newest; the oldest go unsaid, and a stream of theirs is reset as its next packet
    /// finds no connection (review 1.15, D12).
    pub fn saved(&self) -> Saved {
        let conns = self.conns.keys().map(|k| (k.local_port, k.peer_port));
        Saved {
            ports: conns
                .chain(self.stray_rsts.iter().rev().copied())
                .take(self.slots.saturating_add(self.max_stray_rsts))
                .collect(),
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

    /// The sizes of the RX and TX rings the guest negotiated, which bound the RSTs owed.
    /// The guest memory a ring port's connections' rings are attached in (D230).
    pub fn set_memory(&mut self, memory: Arc<GuestMemory>) {
        self.memory = Some(memory);
    }

    pub fn set_rings(&mut self, rx: u16, tx: u16) {
        self.max_stray_rsts = stray_rsts_bound(rx, tx);
    }

    pub fn has_pending_rx(&self) -> bool {
        !self.stray_rsts.is_empty() || !self.rxq.is_empty()
    }

    /// The slots no newcomer can take: connections. A pending handshake gives its own up
    /// to one ([`Muxer::make_room`]).
    fn open(&self) -> usize {
        self.conns.len()
    }

    /// Where every slot is taken, frees one for a newcomer: the pending handshake silent
    /// longest gives its slot up (the client sees its socket close).
    fn make_room(&mut self) {
        self.make_room_within(self.slots);
    }

    /// [`make_room`](Muxer::make_room) where `slots` are all there are.
    fn make_room_within(&mut self, slots: usize) {
        if self.handshakes.len() + self.conns.len() < slots {
            return;
        }
        let silent = self
            .handshakes
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| h.heard)
            .map(|(i, _)| i);
        if let Some(i) = silent {
            debug!("vsock: every slot taken; a host connection silent longest gives its own up");
            self.handshakes.swap_remove(i);
        }
    }

    fn stray_rst(&mut self, local_port: u32, peer_port: u32) {
        if self.stray_rsts.len() < self.max_stray_rsts {
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
        let Some(c) = self.conns.remove(&key) else {
            return;
        };
        self.lent = self.lent.saturating_sub(u64::from(c.lent()));
        if c.allocated_port {
            self.local_ports.remove(&key.local_port);
        }
        #[cfg(windows)]
        if let Some(c) = c.linger() {
            self.lingering.push(c);
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
        let held = u64::from(conn.lent());
        conn.on_guest_packet(h, payload, mem);
        self.lent = self
            .lent
            .saturating_sub(held)
            .saturating_add(u64::from(conn.lent()));
        self.enqueue(key);
    }

    /// The guest connects to host port `key.local_port`: one this process serves, or the
    /// socket `<path>_<port>`.
    fn guest_connect(&mut self, key: Key, request: &Header) {
        if self.open() >= self.slots {
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
        let served = self.served.remove(&key.local_port);
        let every = self.every.get(&key.local_port);
        // A ring port's connection comes with the handle its owner offers a ring through.
        let ring = every
            .filter(|port| port.ringed())
            .and_then(|_| self.memory.clone())
            .map(super::ring::RingHandle::new);
        let connected = match (served.as_ref(), every) {
            (Some(port), _) => serve(|stream| {
                if port.send(stream).is_ok() {
                    crate::vm::Admission::Accepted
                } else {
                    crate::vm::Admission::Closed
                }
            }),
            (None, Some(port)) => serve(|stream| port.accept_with(stream, ring.clone())),
            (None, None) => match &self.listener {
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
                if SIZES_SOCKETS {
                    size_buffers(&stream);
                }
                self.make_room();
                let mut conn = Conn::guest_initiated(stream, self.guest_cid, request);
                if let Some(ring) = ring {
                    conn.ringable(ring);
                }
                self.conns.insert(key, conn);
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
            // What no other connection holds of the lent window is this one's to take.
            let others = self.lent.saturating_sub(u64::from(conn.lent()));
            let spare = u64::from(WINDOW_LENT).saturating_sub(others);
            let h = conn.next_rx(space, u32::try_from(spare).unwrap_or(0));
            self.lent = others.saturating_add(u64::from(conn.lent()));
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
        let mut add = |fd: crate::platform::RawSocket, read: bool, write: bool, token: Token| {
            out.push(Interest {
                fd,
                read,
                write,
                token: Some(token),
            });
        };
        if let Some((_, listener)) = &self.listener
            && self.spare.is_some()
        {
            add(listener.raw_socket(), true, false, Token::Listener);
        }
        for (i, h) in self.handshakes.iter().enumerate() {
            add(h.stream.raw_socket(), true, false, Token::Handshake(i));
        }
        for (key, c) in &self.conns {
            let (read, write) = c.interest();
            if read || write {
                add(c.fd(), read, write, Token::Conn(*key));
            }
        }
        #[cfg(windows)]
        for (i, c) in self.lingering.iter().enumerate() {
            add(c.fd(), true, false, Token::Lingering(i));
        }
    }

    /// Acts on what became ready.
    pub fn on_events(&mut self, events: &[Ready<Option<Token>>]) {
        // A spare its refusal let go, whose descriptor another took first, is made again as
        // soon as one is free: until then the listener is left unpolled.
        if self.spare.is_none()
            && let Some((_, listener)) = &self.listener
        {
            self.spare = spare(listener).ok();
        }
        let mut ready_handshakes = Vec::new();
        let mut listener = false;
        #[cfg(windows)]
        let mut ready_lingering = Vec::new();
        for e in events {
            match e.token {
                None => {}
                Some(Token::Listener) => listener = true,
                Some(Token::Handshake(i)) => ready_handshakes.push(i),
                #[cfg(windows)]
                Some(Token::Lingering(i)) => ready_lingering.push(i),
                Some(Token::Conn(key)) => {
                    let Some(c) = self.conns.get_mut(&key) else {
                        continue;
                    };
                    let held = u64::from(c.lent());
                    if e.write {
                        c.on_writable();
                    }
                    self.lent = self.lent.saturating_sub(held).saturating_add(u64::from(c.lent()));
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
        // After the handshakes, whose indices a slot given up would move.
        if listener {
            self.accept();
        }
        #[cfg(windows)]
        {
            ready_lingering.sort_unstable_by(|a, b| b.cmp(a));
            for i in ready_lingering {
                if self.lingering.get_mut(i).is_some_and(Conn::drained) {
                    self.lingering.swap_remove(i);
                }
            }
        }
    }

    /// Takes host clients' dials, a ring's worth a round at most, as many as the resets a
    /// round may owe the guest ([`Muxer::max_stray_rsts`]), so that dials keep none of the
    /// guest's packets from their turn (`step`, audit A09). Each is a pending handshake
    /// ([`Muxer::take`]), or refused, its connection closed, where every slot is a
    /// connection's or no descriptor is left for it ([`Muxer::refuse_one`]): none waits in
    /// the listener's queue for a connection to end, or keeps it ready for ever.
    fn accept(&mut self) {
        for _ in 0..self.max_stray_rsts {
            let Some((_, listener)) = &self.listener else {
                return;
            };
            match listener.accept() {
                Ok((stream, _)) => self.take(stream),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if out_of_descriptors(&e) && self.spare.is_some() => self.refuse_one(),
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        debug!("vsock: accept: {e}");
                    }
                    return;
                }
            }
        }
    }

    /// A host client's dial, accepted: a pending handshake, in the slot one silent longest
    /// gives up where every slot is taken ([`Muxer::make_room`]); or refused, its connection
    /// closed, where every slot is a connection's, as Firecracker's muxer refuses one past
    /// its MAX_CONNECTIONS.
    fn take(&mut self, stream: UnixStream) {
        self.make_room();
        if self.handshakes.len() + self.conns.len() >= self.slots {
            debug!("vsock: every slot a connection's; a host connection refused");
            return;
        }
        if let Err(e) = stream.set_nonblocking(true) {
            debug!("vsock: host connection: {e}");
            return;
        }
        if SIZES_SOCKETS {
            size_buffers(&stream);
        }
        self.handshakes.push(Handshake {
            stream,
            line: Vec::with_capacity(MAX_HANDSHAKE),
            heard: Instant::now(),
        });
    }

    /// Refuses the dial at the head of the listener's queue, which no descriptor is left to
    /// take: the spare let go for it, the dial accepted and its connection closed at once,
    /// which its client reads as its end before any answer, and the spare made again
    /// (libev's documentation, "the special problem of accept()ing when you can't"; libuv's
    /// `uv__emfile_trick`). Where another takes the descriptor first, a later round makes
    /// the spare again ([`Muxer::on_events`]).
    fn refuse_one(&mut self) {
        let Some((_, listener)) = &self.listener else {
            return;
        };
        self.spare = None;
        match listener.accept() {
            Ok(_) => debug!("vsock: no descriptor left for a host connection; refused"),
            Err(e) => debug!("vsock: accept: {e}"),
        }
        self.spare = spare(listener).ok();
    }

    fn handshake(&mut self, i: usize) {
        let Some(h) = self.handshakes.get_mut(i) else {
            return;
        };
        match read_handshake(h) {
            Ok(None) => {}
            // Its connection goes to its owner, who answers on it; with none, it closes
            // unanswered, as a line of no handshake's does.
            Ok(Some(Line::Rate(milli))) => {
                let h = self.handshakes.swap_remove(i);
                match &self.rate {
                    Some(to) => {
                        // Out of the muxer's waits, to be written blocking by its owner.
                        if let Err(e) = super::poll::release(h.stream.raw_socket()) {
                            debug!("vsock: a rate's dial kept in the muxer's waits: {e}");
                        }
                        if to.send((h.stream, milli)).is_err() {
                            debug!("vsock: a rate no one takes any more; its dial closed");
                        }
                    }
                    None => debug!("vsock: a rate asked of a machine that takes none; its dial closed"),
                }
            }
            Ok(Some(Line::Connect(peer_port))) => {
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
        // At most its slots are taken, so a free port is always near.
        loop {
            self.last_local_port =
                LOCAL_PORT_BASE | (self.last_local_port.wrapping_add(1) & (LOCAL_PORT_BASE - 1));
            if self.local_ports.insert(self.last_local_port) {
                return self.last_local_port;
            }
        }
    }

    /// What holds of the muxer between any two of its calls (tests): its lent window what its
    /// connections hold, together, and never more than [`WINDOW_LENT`]; and each
    /// connection's own ([`Conn::check`]).
    #[cfg(test)]
    fn check(&self) -> Result<(), String> {
        let held: u64 = self.conns.values().map(|c| u64::from(c.lent())).sum();
        if held != self.lent || self.lent > u64::from(WINDOW_LENT) {
            return Err(format!(
                "lent {} of {WINDOW_LENT}, where its connections hold {held}",
                self.lent
            ));
        }
        self.conns.values().try_for_each(Conn::check)
    }

    /// The earliest deadline of any connection. Handshakes have none ([`Handshake::heard`]).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.conns.values().filter_map(Conn::expiry).min()
    }

    /// Resets late connections.
    pub fn expire(&mut self, now: Instant) {
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
fn serve(accept: impl FnOnce(UnixStream) -> crate::vm::Admission) -> io::Result<UnixStream> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_nonblocking(true)?;
    // Ours is sized as its connection is made (Muxer::guest_connect); theirs sends the
    // host's bytes.
    if SIZES_SOCKETS {
        size_buffers(&theirs);
    }
    match accept(theirs) {
        crate::vm::Admission::Accepted => {}
        crate::vm::Admission::Closed => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "host port is closed",
            ));
        }
    }
    Ok(ours)
}

/// The buffers [`size_buffers`] asks for.
#[cfg(unix)]
const BUFFERS: [(libc::c_int, &str); 2] = [(libc::SO_SNDBUF, "SO_SNDBUF"), (libc::SO_RCVBUF, "SO_RCVBUF")];
#[cfg(windows)]
const BUFFERS: [(i32, &str); 2] = [
    (windows_sys::Win32::Networking::WinSock::SO_SNDBUF, "SO_SNDBUF"),
    (windows_sys::Win32::Networking::WinSock::SO_RCVBUF, "SO_RCVBUF"),
];

/// Asks the host's kernel to buffer [`SOCKET_BUFFER`] each way on a connection's host
/// socket. A size it refuses or cuts is said, and the socket kept: a smaller buffer only
/// costs speed.
fn size_buffers(stream: &UnixStream) {
    for (option, name) in BUFFERS {
        match set_buffer(stream.raw_socket(), option, SOCKET_BUFFER) {
            Ok(got) if got >= SOCKET_BUFFER as usize => {}
            Ok(got) => debug!("vsock: a host socket's {name} is {got} bytes of {SOCKET_BUFFER} asked"),
            Err(e) => debug!("vsock: a host socket's {name}: {e}"),
        }
    }
}

/// Sets a socket's buffer `option` to `bytes`, and says what the kernel made of it: Linux
/// doubles what it takes (socket(7)), and cuts it to net.core.wmem_max or rmem_max first.
#[cfg(unix)]
fn set_buffer(socket: crate::platform::RawSocket, option: libc::c_int, bytes: u32) -> io::Result<usize> {
    let want = libc::c_int::try_from(bytes).map_err(io::Error::other)?;
    let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: setsockopt(2) of an int option from `want`, `size` its length, on a socket
    // the caller holds.
    let set = unsafe { libc::setsockopt(socket, libc::SOL_SOCKET, option, (&raw const want).cast(), size) };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    buffer_size(socket, option)
}

/// A socket's buffer `option`, in bytes.
#[cfg(unix)]
fn buffer_size(socket: crate::platform::RawSocket, option: libc::c_int) -> io::Result<usize> {
    let mut got: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) of an int option into `got`, `len` its length, on a socket the
    // caller holds.
    let read = unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_SOCKET,
            option,
            (&raw mut got).cast(),
            &raw mut len,
        )
    };
    if read != 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(got).map_err(io::Error::other)
}

/// Sets a socket's buffer `option` to `bytes`, and says what Winsock made of it.
#[cfg(windows)]
fn set_buffer(socket: crate::platform::RawSocket, option: i32, bytes: u32) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{SOL_SOCKET, setsockopt};
    let want = i32::try_from(bytes).map_err(io::Error::other)?;
    let size = i32::try_from(std::mem::size_of::<i32>()).map_err(io::Error::other)?;
    // SAFETY: setsockopt of an int option from `want`, `size` its length, on a socket the
    // caller holds.
    if unsafe { setsockopt(socket, SOL_SOCKET, option, (&raw const want).cast(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    buffer_size(socket, option)
}

/// A socket's buffer `option`, in bytes.
#[cfg(windows)]
fn buffer_size(socket: crate::platform::RawSocket, option: i32) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{SOL_SOCKET, getsockopt};
    let mut got: i32 = 0;
    let mut len = i32::try_from(std::mem::size_of::<i32>()).map_err(io::Error::other)?;
    // SAFETY: getsockopt of an int option into `got`, `len` its length, on a socket the
    // caller holds.
    if unsafe { getsockopt(socket, SOL_SOCKET, option, (&raw mut got).cast(), &raw mut len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(got).map_err(io::Error::other)
}

/// Reads a handshake line without consuming anything after it. `Some` once the line is
/// complete and valid; `None` while more is to come.
fn read_handshake(h: &mut Handshake) -> io::Result<Option<Line>> {
    loop {
        let want = MIN_HANDSHAKE.saturating_sub(h.line.len()).max(1);
        let mut buf = [0u8; MIN_HANDSHAKE];
        let dst = buf.get_mut(..want).unwrap_or_default();
        match h.stream.read(dst) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                h.heard = Instant::now();
                h.line.extend(dst.iter().take(n));
                if h.line.contains(&b'\n') {
                    return parse_line(&h.line)
                        .map(Some)
                        .ok_or_else(|| io::Error::other("expected `CONNECT <port>` or `RATE <milli>`"));
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

/// A whole handshake line: `CONNECT <port>`, or a managed machine's `RATE <milli>`
/// (`shards_abi::rate::read_ask`, D255).
fn parse_line(line: &[u8]) -> Option<Line> {
    parse_connect(line)
        .map(Line::Connect)
        .or_else(|| shards_abi::rate::read_ask(line).map(Line::Rate))
}

/// `CONNECT <port>` followed by a newline (case-insensitive, as Firecracker).
fn parse_connect(line: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(line).ok()?;
    let (command, rest) = text.split_once('\n')?;
    if !rest.is_empty() {
        return None;
    }
    let mut words = command.split_whitespace();
    if !words.next()?.as_bytes().eq_ignore_ascii_case(CONNECT) {
        return None;
    }
    let port = words.next()?.parse().ok()?;
    words.next().is_none().then_some(port)
}

/// A descriptor held for a dial no other descriptor is left for ([`Muxer::refuse_one`]):
/// let go, it is the one accept takes. On Unix a copy of the listener's (dup(2)), which
/// asks nothing a confined VM process may be refused.
#[cfg(unix)]
type Spare = UnixListener;
/// On Windows, whose sockets are kernel handles, counted with the rest of a process's
/// (WSAEMFILE), an event's, which a VM process in its AppContainer makes as its waker's is
/// (mod.rs), where a copy of a socket asks for the process by its id (WSADuplicateSocketW).
#[cfg(windows)]
type Spare = std::os::windows::io::OwnedHandle;

#[cfg(unix)]
fn spare(listener: &UnixListener) -> io::Result<Spare> {
    listener.try_clone()
}

#[cfg(windows)]
fn spare(_listener: &UnixListener) -> io::Result<Spare> {
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::System::Threading::CreateEventW;
    // SAFETY: an unnamed event, of default security, reset by a wait, and not set.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle CreateEventW just made, which nothing else owns.
    Ok(unsafe { Spare::from_raw_handle(event) })
}

/// Whether an accept found no descriptor for its connection: the process's are all open
/// (EMFILE; WSAEMFILE on Windows), or on Unix the system's (ENFILE).
fn out_of_descriptors(e: &io::Error) -> bool {
    #[cfg(unix)]
    let out = [libc::EMFILE, libc::ENFILE];
    #[cfg(windows)]
    let out = [windows_sys::Win32::Networking::WinSock::WSAEMFILE];
    e.raw_os_error().is_some_and(|code| out.contains(&code))
}

/// Connects without blocking: a host listener whose backlog is full refuses at once
/// instead of stalling the device.
#[cfg(windows)]
fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect_nonblocking(path)
}

/// Connects without blocking: a host listener whose backlog is full refuses at once
/// instead of stalling the device.
#[cfg(unix)]
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
    use std::time::Duration;

    use super::super::conn::{WINDOW_LEAST, WINDOW_MOST};
    use super::super::packet::MAX_PAYLOAD;
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
        assert_eq!(parse_line(b"CONNECT 52\n"), Some(Line::Connect(52)));
        assert_eq!(parse_line(b"rate 250\r\n"), Some(Line::Rate(250)));
        assert_eq!(parse_line(b"RATE 4294967296\n"), Some(Line::Rate(1 << 32)));
        for bad in [&b"RATE\n"[..], b"RATE 1 2\n", b"RATED 1 2\n", b"RATE 1\nrest"] {
            assert_eq!(parse_line(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    /// A rate comes from a dial on the device's socket alone (D255): a host client's `RATE`
    /// line reaches the machine's rate with its connection, which the answer goes back on.
    /// The guest's connections, to a port this process serves and to ports none serves, and
    /// what it sends on them (a `RATE` line among it) never do; the bytes go to the port's
    /// owner. A machine that takes no rate closes such a dial unanswered.
    #[test]
    fn a_rate_comes_from_a_dial_on_the_socket_alone() {
        let dir_guard = shards_testdir::TempDir::new("vsock-rate").unwrap();
        let dir = dir_guard.join("v");
        let (served, arrived) = std::sync::mpsc::channel();
        let (rates, rated) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: Some(dir.clone()),
            ports: Vec::new(),
            every: vec![(54, served.into())],
            rate: Some(rates),
        };
        let mut m = Muxer::new(host, 3).unwrap();
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        m.on_guest_packet(&request(54, 49_152), &[], &mem);
        assert_eq!(m.next_rx(&[]).map(|h| h.op), Some(op::RESPONSE));
        let mut theirs = arrived.try_recv().unwrap();
        let line = b"RATE 250\n";
        mem.access().unwrap().write(0x8000_0000, line).unwrap();
        let span = Span {
            gpa: 0x8000_0000,
            ptr: mem.host_ptr(0x8000_0000, line.len()).unwrap(),
            len: line.len(),
        };
        let rw = Header {
            op: op::RW,
            len: line.len() as u32,
            ..request(54, 49_152)
        };
        m.on_guest_packet(&rw, &[span], &mem);
        for port in [0, 1, 1024, u32::MAX] {
            m.on_guest_packet(&request(port, 49_153), &[], &mem);
        }
        assert!(rated.try_recv().is_err(), "a guest's connection reached the rate");
        let mut got = [0u8; 9];
        theirs.read_exact(&mut got).unwrap();
        assert_eq!(&got, line, "the guest's bytes are its port's owner's");
        let mut client = UnixStream::connect(&dir).unwrap();
        m.on_events(&[Ready {
            token: Some(Token::Listener),
            read: true,
            write: false,
        }]);
        client.write_all(b"RATE 250\n").unwrap();
        m.on_events(&[Ready {
            token: Some(Token::Handshake(0)),
            read: true,
            write: false,
        }]);
        let (mut asked, milli) = rated.try_recv().unwrap();
        assert_eq!(milli, 250);
        assert!(m.handshakes.is_empty(), "the dial is the rate's owner's");
        assert_eq!(m.conns.len(), 1, "the guest's connection alone");
        asked.set_nonblocking(false).unwrap();
        asked.write_all(b"RATED 250 100000000\n").unwrap();
        drop(asked);
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "RATED 250 100000000\n");
        // A machine that takes no rate: the dial closes unanswered.
        let none_guard = shards_testdir::TempDir::new("vsock-norate").unwrap();
        let none_at = none_guard.join("v");
        let mut none = Muxer::new(VsockHost::at(none_at.clone()), 3).unwrap();
        let mut client = UnixStream::connect(&none_at).unwrap();
        none.on_events(&[Ready {
            token: Some(Token::Listener),
            read: true,
            write: false,
        }]);
        client.write_all(b"RATE 250\n").unwrap();
        none.on_events(&[Ready {
            token: Some(Token::Handshake(0)),
            read: true,
            write: false,
        }]);
        assert!(none.handshakes.is_empty());
        let end = client.read(&mut [0u8; 1]);
        assert!(
            matches!(&end, Ok(0))
                || end
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "{end:?}"
        );
    }

    /// A rate's dial, read through the muxer's own waits as its device's thread makes them,
    /// is handed out able to block: on Windows a wait's WSAEventSelect held its socket
    /// non-blocking, so its owner's set_nonblocking(false) failed WSAEINVAL, its answer was
    /// never written, and its client read the dial closed unanswered (CI run 43435318858,
    /// boot.rs's a_managed_guest_is_held_to_the_rate_its_client_asks_on_its_socket on
    /// windows-2022).
    #[test]
    fn a_rates_dial_is_handed_out_able_to_block() {
        let dir_guard = shards_testdir::TempDir::new("vsock-rate-wait").unwrap();
        let dir = dir_guard.join("v");
        let (rates, rated) = std::sync::mpsc::channel();
        let host = VsockHost {
            rate: Some(rates),
            ..VsockHost::at(dir.clone())
        };
        let mut m = Muxer::new(host, 3).unwrap();
        let mut poller = super::super::poll::Poller::new().unwrap();
        let mut client = UnixStream::connect(&dir).unwrap();
        client.write_all(b"RATE 250\n").unwrap();
        // The device's loop: what the muxer asks waited on, then what came handed to it.
        let mut handed = None;
        for _ in 0..8 {
            let mut interests = Vec::new();
            m.interests(&mut interests);
            let mut ready = Vec::new();
            poller
                .wait(&interests, Some(Duration::from_millis(250)), &mut ready)
                .unwrap();
            m.on_events(&ready);
            if let Ok(dial) = rated.try_recv() {
                handed = Some(dial);
                break;
            }
        }
        let (mut asked, milli) = handed.expect("the rate's dial never reached its owner");
        assert_eq!(milli, 250);
        asked.set_nonblocking(false).unwrap();
        asked.write_all(b"RATED 250 100000000\n").unwrap();
        drop(asked);
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "RATED 250 100000000\n");
    }

    /// The handshake's bounds are its own words': `CONNECT 0\n` the shortest line, and
    /// `CONNECT 4294967295\r\n` the longest, which is read whole, where a byte more is
    /// refused before its end.
    #[test]
    fn handshake_lines_are_bounded_by_the_protocols_words() {
        assert_eq!(MIN_HANDSHAKE, b"CONNECT 0\n".len());
        assert_eq!(MAX_HANDSHAKE, b"RATE 18446744073709551615\r\n".len());
        assert_eq!(decimal_digits(9), 1);
        assert_eq!(decimal_digits(10), 2);
        let read = |line: &[u8]| {
            let (mut client, server) = UnixStream::pair().unwrap();
            server.set_nonblocking(true).unwrap();
            let mut h = Handshake {
                stream: server,
                line: Vec::new(),
                heard: Instant::now(),
            };
            client.write_all(line).unwrap();
            read_handshake(&mut h)
        };
        assert_eq!(
            read(b"CONNECT 4294967295\r\n").unwrap(),
            Some(Line::Connect(u32::MAX))
        );
        assert_eq!(
            read(b"connect 4294967295\n").unwrap(),
            Some(Line::Connect(u32::MAX))
        );
        assert_eq!(
            read(b"RATE 18446744073709551615\r\n").unwrap(),
            Some(Line::Rate(u64::MAX))
        );
        assert_eq!(read(b"RATE 0\n").unwrap(), Some(Line::Rate(0)));
        assert!(read(b"RATE  18446744073709551615\r\n").is_err());
    }

    /// Set in the child `past_its_descriptors_a_dial_is_refused_at_once` runs.
    #[cfg(unix)]
    const LOWERED: &str = "SHARDS_VMM_MUXER_LOWERED";

    /// Past the descriptors its process may open, a host's dial is refused at once, its
    /// connection closed, where the muxer's accept fails EMFILE: not left in the listener's
    /// queue, as the listener, polled again, fails again for ever. Run in a child of this
    /// test binary, which lowers its soft descriptor limit (setrlimit(2)) to its lowest free
    /// descriptor, so the next one any accept makes fails, once its clients have dialed
    /// (D12; review: connections derived, refused typed, EMFILE included).
    #[cfg(unix)]
    #[test]
    fn past_its_descriptors_a_dial_is_refused_at_once() {
        const NAME: &str =
            "devices::virtio::vsock::muxer::tests::past_its_descriptors_a_dial_is_refused_at_once";
        const REFUSED: &str = "muxer child: every dial past its descriptors refused";
        if std::env::var_os(LOWERED).is_none() {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
                .env(LOWERED, "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&child.stdout);
            assert!(
                child.status.success() && said.contains(REFUSED),
                "{:?}\n{said}{}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            return;
        }
        let dir_guard = shards_testdir::TempDir::new("vsock-emfile").unwrap();
        let dir = dir_guard.join("v");
        let mut m = Muxer::new(VsockHost::at(dir.clone()), 3).unwrap();
        // An input: any few clients show it.
        let mut clients: Vec<UnixStream> = (0..3).map(|_| UnixStream::connect(&dir).unwrap()).collect();
        let mut was = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) into a local.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut was) }, 0);
        // SAFETY: dup(2) of stdin, the lowest descriptor free: the next one made.
        let lowest = unsafe { libc::dup(0) };
        assert!(lowest >= 0);
        // SAFETY: close(2) of the descriptor dup just made.
        unsafe { libc::close(lowest) };
        let lowered = libc::rlimit {
            rlim_cur: libc::rlim_t::try_from(lowest).unwrap(),
            rlim_max: was.rlim_max,
        };
        // SAFETY: setrlimit(2) from a local, in this child alone.
        let set = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const lowered) };
        assert_eq!(set, 0);
        m.on_events(&[Ready {
            token: Some(Token::Listener),
            read: true,
            write: false,
        }]);
        let ends: Vec<String> = clients
            .iter_mut()
            .map(|c| {
                c.set_nonblocking(true).unwrap();
                match c.read(&mut [0u8; 1]) {
                    Ok(0) => "closed".to_string(),
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => "closed".to_string(),
                    other => format!("{other:?}"),
                }
            })
            .collect();
        // SAFETY: setrlimit(2) from a local: the limit as it was, so the child ends cleanly.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const was) }, 0);
        assert!(ends.iter().all(|e| e == "closed"), "{ends:?}");
        println!("{REFUSED}");
    }

    /// Past its slots, every one a connection's, a host's dial is refused at once, its
    /// connection closed, as Firecracker's muxer refuses one past its MAX_CONNECTIONS: not
    /// left in the listener's queue until a connection ends (D12; review: connections
    /// derived, refused typed past them).
    #[test]
    fn past_its_slots_a_dial_is_refused_at_once() {
        let dir_guard = shards_testdir::TempDir::new("vsock-full").unwrap();
        let dir = dir_guard.join("v");
        let (sender, _arrived) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: Some(dir.clone()),
            ports: Vec::new(),
            every: vec![(54, sender.into())],
            rate: None,
        };
        let mut m = Muxer::new(host, 3).unwrap();
        // An input: one slot, which the guest's connection holds.
        m.slots = 1;
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        m.on_guest_packet(&request(54, 49_152), &[], &mem);
        assert_eq!(m.conns.len(), 1);
        let mut client = UnixStream::connect(&dir).unwrap();
        // What the device polls is what the muxer asks for, and the listener is ready.
        let mut interests = Vec::new();
        m.interests(&mut interests);
        let ready: Vec<Ready<Option<Token>>> = interests
            .iter()
            .filter(|i| matches!(i.token, Some(Token::Listener)))
            .map(|i| Ready {
                token: i.token,
                read: true,
                write: false,
            })
            .collect();
        m.on_events(&ready);
        client.set_nonblocking(true).unwrap();
        let end = client.read(&mut [0u8; 1]);
        assert!(
            matches!(&end, Ok(0))
                || end
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "a dial past its slots: {end:?}"
        );
        assert_eq!(m.conns.len(), 1, "the guest's connection kept");
    }

    /// A pending handshake waits on no clock: it outlives any deadline, and gives its slot
    /// up only to a newcomer, the one silent longest first.
    #[test]
    fn a_silent_handshake_keeps_its_slot_until_a_newcomer_wants_it() {
        let dir_guard = shards_testdir::TempDir::new("vsock-room").unwrap();
        let dir = dir_guard.join("v");
        let mut m = Muxer::new(VsockHost::at(dir.clone()), 3).unwrap();
        let mut clients = Vec::new();
        for _ in 0..3 {
            clients.push(UnixStream::connect(&dir).unwrap());
            m.on_events(&[Ready {
                token: Some(Token::Listener),
                read: true,
                write: false,
            }]);
        }
        assert_eq!(m.handshakes.len(), 3);
        assert_eq!(m.next_deadline(), None, "no clock on a handshake");
        m.expire(Instant::now() + Duration::from_secs(3600));
        assert_eq!(m.handshakes.len(), 3, "kept past any deadline");
        // The first two clients, accepted first, write part of their lines: the third is
        // silent longest.
        for (client, h) in clients.iter_mut().zip(&mut m.handshakes).take(2) {
            client.write_all(b"CONN").unwrap();
            assert_eq!(read_handshake(h).unwrap(), None);
        }
        m.make_room_within(4);
        assert_eq!(m.handshakes.len(), 3, "a slot free: none gives its own up");
        m.make_room_within(3);
        assert_eq!(m.handshakes.len(), 2);
        assert!(
            m.handshakes.iter().all(|h| h.line == b"CONN"),
            "the silent one went"
        );
        let mut buf = [0u8; 1];
        assert_eq!(clients[2].read(&mut buf).unwrap(), 0, "its client sees its end");
    }

    /// RSTs owed for packets that matched nothing are bounded by the guest's rings: one
    /// round's TX packets, or one RX ring, whichever is more.
    #[test]
    fn stray_resets_owed_follow_the_guests_rings() {
        assert_eq!(stray_rsts_bound(64, 128), 128);
        assert_eq!(stray_rsts_bound(256, 16), 256);
        let host = VsockHost {
            path: None,
            ports: Vec::new(),
            every: Vec::new(),
            rate: None,
        };
        let mut m = Muxer::new(host, 3).unwrap();
        assert_eq!(m.max_stray_rsts, MAX_STRAY_RSTS);
        m.set_rings(64, 128);
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        for from in 0..200 {
            m.on_guest_packet(&request(7, 50_000 + from), &[], &mem);
        }
        assert_eq!(m.stray_rsts.len(), 128);
    }

    #[test]
    fn handshakes_arrive_in_pieces_and_leave_later_data_unread() {
        let (mut client, server) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut h = Handshake {
            stream: server,
            line: Vec::new(),
            heard: Instant::now(),
        };
        client.write_all(b"CONN").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), None);
        client.write_all(b"ECT 77").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), None);
        client.write_all(b"\npayload").unwrap();
        assert_eq!(read_handshake(&mut h).unwrap(), Some(Line::Connect(77)));
        let mut rest = [0u8; 7];
        h.stream.read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"payload");
    }

    /// A restored muxer resets what the snapshot held before the guest hears anything
    /// else, even a connection it made before the first packet went out, and allocates
    /// host ports past the snapshot's without reusing a held one.
    #[test]
    fn restores_reset_held_streams_first_and_keep_their_ports() {
        let dir_guard = shards_testdir::TempDir::new("vsock-restore").unwrap();
        let dir = dir_guard.join("v");
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
            every: Vec::new(),
            rate: None,
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

    /// A port served for every connection gets each, apart, as its own socket.
    #[test]
    fn ports_served_for_every_connection_get_each() {
        let (sender, arrived) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: None,
            ports: Vec::new(),
            every: vec![(54, sender.into())],
            rate: None,
        };
        let mut m = Muxer::new(host, 3).unwrap();
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        for from in [49_152, 49_153, 49_154] {
            m.on_guest_packet(&request(54, from), &[], &mem);
        }
        let mut sent = Vec::new();
        while let Some(h) = m.next_rx(&[]) {
            sent.push((h.op, h.dst_port));
        }
        sent.sort_unstable();
        assert_eq!(
            sent,
            [
                (op::RESPONSE, 49_152),
                (op::RESPONSE, 49_153),
                (op::RESPONSE, 49_154)
            ]
        );
        assert_eq!(arrived.try_iter().count(), 3);
    }

    /// What the guest sent before it ended a connection is the host's to read whole, then
    /// end-of-stream, though the host had sent data the guest took none of and writes again
    /// after the guest's end, as an exec's stdin's end can come after a command that ended
    /// at once: on Windows a socket closed while its peer may still read loses the peer what
    /// it had not read (PM M358), so the muxer holds its end until the host ends its own.
    #[test]
    fn what_the_guest_sent_before_its_end_is_read_whole_whatever_the_host_writes() {
        use super::super::packet::shutdown;
        let (sender, arrived) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: None,
            ports: Vec::new(),
            every: vec![(54, sender.into())],
            rate: None,
        };
        let mut m = Muxer::new(host, 3).unwrap();
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        m.on_guest_packet(&request(54, 49_152), &[], &mem);
        assert_eq!(m.next_rx(&[]).map(|h| h.op), Some(op::RESPONSE));
        let mut theirs = arrived.try_recv().unwrap();
        theirs.write_all(b"stdin").unwrap();
        let said = b"the command's output, then its exit status";
        mem.access().unwrap().write(0x8000_0000, said).unwrap();
        let output = Span {
            gpa: 0x8000_0000,
            ptr: mem.host_ptr(0x8000_0000, said.len()).unwrap(),
            len: said.len(),
        };
        let rw = Header {
            op: op::RW,
            len: said.len() as u32,
            ..request(54, 49_152)
        };
        m.on_guest_packet(&rw, &[output], &mem);
        let ended = Header {
            op: op::SHUTDOWN,
            flags: shutdown::RCV | shutdown::SEND,
            ..request(54, 49_152)
        };
        m.on_guest_packet(&ended, &[], &mem);
        assert_eq!(m.next_rx(&[]).map(|h| h.op), Some(op::RST));
        assert!(m.conns.is_empty(), "the guest's connection is over");
        let _ = theirs.write_all(b" and more");
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        let end = loop {
            match theirs.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => break Some(e.kind()),
            }
        };
        assert_eq!(got, said, "what the guest sent, whole");
        // End-of-stream; on Linux, whose muxer closed its end holding what the host sent
        // unread, a reset after it (unix_release_sock).
        assert!(
            end.is_none() || cfg!(target_os = "linux") && end == Some(io::ErrorKind::ConnectionReset),
            "{end:?}"
        );
        #[cfg(windows)]
        {
            assert_eq!(m.lingering.len(), 1, "held while the host's end is open");
            drop(theirs);
            // The host's end reads as its socket's end (poll.rs): the muxer's goes then.
            let mut interests = Vec::new();
            m.interests(&mut interests);
            let ready: Vec<Ready<Option<Token>>> = interests
                .iter()
                .filter(|i| matches!(i.token, Some(Token::Lingering(_))))
                .map(|i| Ready {
                    token: i.token,
                    read: true,
                    write: false,
                })
                .collect();
            assert_eq!(ready.len(), 1);
            m.on_events(&ready);
            assert!(m.lingering.is_empty(), "let go once the host's end is gone");
        }
    }

    /// A save holds one save's worth of streams, however many saves came before it with
    /// none of their resets sent between: its connections, then the newest resets owed, as
    /// many as a muxer never restored holds at most, its slots and a ring's worth of resets
    /// ([`Muxer::stray_rst`]). A restore reads whatever a save wrote, a host's with more
    /// slots too (review 1.15).
    #[test]
    fn a_save_holds_one_saves_worth_however_many_came_before() {
        let dir_guard = shards_testdir::TempDir::new("vsock-resave").unwrap();
        let (sender, _arrived) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: Some(dir_guard.join("v")),
            ports: Vec::new(),
            every: vec![(54, sender.into())],
            rate: None,
        };
        let mut m = Muxer::new(host, 3).unwrap();
        // An input: few slots and small rings, which saves in a row outgrow.
        m.slots = 3;
        m.set_rings(4, 4);
        let one = m.slots + m.max_stray_rsts;
        // Restored from three saves' worth, none of whose resets went out; then the guest
        // connects.
        let held: Vec<(u32, u32)> = (0..3 * one as u32)
            .map(|i| (LOCAL_PORT_BASE + i, 1000 + i))
            .collect();
        m.restore(Saved {
            ports: held.clone(),
            last_local_port: LOCAL_PORT_BASE + 3 * one as u32,
        });
        let mem = GuestMemory::anonymous(&[(0x8000_0000, 1 << 16)]).unwrap();
        m.on_guest_packet(&request(54, 49_152), &[], &mem);
        let saved = m.saved();
        let mut kept = vec![(54, 49_152)];
        kept.extend(held.iter().rev().take(one - 1));
        assert_eq!(saved.ports, kept, "its connection, then the newest owed");
        // A restore reads a save whole, one of a host with more slots too.
        let bigger = Saved {
            ports: held,
            last_local_port: saved.last_local_port,
        };
        let mut w = Writer::default();
        bigger.write(&mut w);
        let bytes = w.into_bytes();
        assert_eq!(Saved::read(&mut Reader::new(&bytes)).unwrap(), bigger);
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
        let dir_guard = shards_testdir::TempDir::new("vsock-ports").unwrap();
        let dir = dir_guard.join("v");
        let mut m = Muxer::new(VsockHost::at(dir.clone()), 3).unwrap();
        m.last_local_port = u32::MAX - 1;
        let a = m.allocate_local_port();
        let b = m.allocate_local_port();
        assert!((LOCAL_PORT_BASE..1 << 31).contains(&a) && (LOCAL_PORT_BASE..1 << 31).contains(&b));
        assert_ne!(a, b);
    }

    // The device's lent window (conn.rs, WINDOW_LENT), over connections to a port this
    // process serves for every connection, their guests' credit kept as Linux keeps it.

    /// Where the guest's packets are, in its memory.
    const PACKETS: u64 = 0x8000_0000;

    /// The bytes a guest sends: each its offset in its stream, modulo a prime, so that one
    /// out of place shows. A packet's are this from its offset's place.
    fn pattern() -> &'static [u8] {
        static PATTERN: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        PATTERN.get_or_init(|| (0..MAX_PAYLOAD as usize + 251).map(|i| (i % 251) as u8).collect())
    }

    /// A muxer serving port 54 for every connection, and where their host ends arrive.
    fn served_every() -> (Muxer, std::sync::mpsc::Receiver<UnixStream>) {
        let (sender, arrived) = std::sync::mpsc::channel();
        let host = VsockHost {
            path: None,
            ports: Vec::new(),
            every: vec![(54, sender.into())],
            rate: None,
        };
        (Muxer::new(host, 3).unwrap(), arrived)
    }

    /// A guest's end of one connection to port 54, as Linux keeps it: what our latest
    /// packet let it send (virtio_transport_space_update) within its own buffer
    /// (8ee784fdf006); and the host's end, which checks that it reads the guest's bytes in
    /// order.
    struct Peer {
        key: Key,
        host: UnixStream,
        /// Its own buffer.
        buf_alloc: u32,
        tx_cnt: u32,
        heard_fwd: u32,
        heard_buf: u32,
        /// The furthest our packets let it send.
        edge: u32,
        /// Packets whose window was less than the one before.
        shrunk: usize,
        read: usize,
        reset: bool,
    }

    impl Peer {
        /// Guest port `from`, its buffer `buf_alloc`, asks to connect; [`hear`] the answer.
        fn connect(
            m: &mut Muxer,
            arrived: &std::sync::mpsc::Receiver<UnixStream>,
            mem: &GuestMemory,
            from: u32,
            buf_alloc: u32,
        ) -> Peer {
            m.on_guest_packet(
                &Header {
                    buf_alloc,
                    ..request(54, from)
                },
                &[],
                mem,
            );
            let host = arrived.try_recv().unwrap();
            host.set_nonblocking(true).unwrap();
            Peer {
                key: Key {
                    local_port: 54,
                    peer_port: from,
                },
                host,
                buf_alloc,
                tx_cnt: 0,
                heard_fwd: 0,
                heard_buf: 0,
                edge: 0,
                shrunk: 0,
                read: 0,
                reset: false,
            }
        }

        /// What Linux lets it send now: the lesser of our window and its own buffer, past
        /// what it sent that it has not heard we forwarded (virtio_transport_has_space).
        fn credit(&self) -> u32 {
            self.heard_buf
                .min(self.buf_alloc)
                .saturating_sub(self.tx_cnt.wrapping_sub(self.heard_fwd))
        }

        /// Sends `n` bytes, a packet's at most, of the [`pattern`].
        fn send(&mut self, m: &mut Muxer, mem: &GuestMemory, n: u32) {
            let at = (self.tx_cnt % 251) as usize;
            let bytes = &pattern()[at..at + n as usize];
            mem.access().unwrap().write(PACKETS, bytes).unwrap();
            let span = Span {
                gpa: PACKETS,
                ptr: mem.host_ptr(PACKETS, bytes.len()).unwrap(),
                len: bytes.len(),
            };
            let h = Header {
                op: op::RW,
                len: n,
                buf_alloc: self.buf_alloc,
                ..request(54, self.key.peer_port)
            };
            m.on_guest_packet(&h, &[span], mem);
            self.tx_cnt = self.tx_cnt.wrapping_add(n);
        }

        /// Sends all Linux lets it, a packet at a time, the host reading as it goes where
        /// `reading`.
        fn send_all(&mut self, m: &mut Muxer, mem: &GuestMemory, reading: bool) {
            while self.credit() > 0 {
                self.send(m, mem, self.credit().min(MAX_PAYLOAD));
                if reading {
                    self.drain(m);
                }
            }
        }

        /// The host reads all it can, each byte checked, and the muxer writes what it holds
        /// as the socket turns writable, until neither moves.
        fn drain(&mut self, m: &mut Muxer) {
            let mut buf = [0u8; MAX_PAYLOAD as usize];
            loop {
                let mut moved = false;
                loop {
                    match self.host.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let at = self.read % 251;
                            assert!(
                                buf[..n] == pattern()[at..at + n],
                                "port {}: bytes {} to {} not the guest's",
                                self.key.peer_port,
                                self.read,
                                self.read + n
                            );
                            self.read += n;
                            moved = true;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => panic!("reading the host end: {e}"),
                    }
                }
                m.on_events(&[Ready {
                    token: Some(Token::Conn(self.key)),
                    read: false,
                    write: true,
                }]);
                if !moved {
                    return;
                }
            }
        }
    }

    /// Gives the guest every packet the muxer has for it, each to its peer: (guest port, op,
    /// window), in order. No window is less than the least, no right edge moves left (RFC
    /// 1122 §4.2.2.16), and the muxer's bounds hold after each ([`Muxer::check`]).
    fn hear(m: &mut Muxer, peers: &mut [Peer]) -> Vec<(u32, u16, u32)> {
        let mut heard = Vec::new();
        while let Some(h) = m.next_rx(&[]) {
            m.check().unwrap();
            heard.push((h.dst_port, h.op, h.buf_alloc));
            let Some(p) = peers.iter_mut().find(|p| p.key.peer_port == h.dst_port) else {
                continue;
            };
            if h.op == op::RST {
                p.reset = true;
                continue;
            }
            assert!(
                h.buf_alloc >= WINDOW_LEAST,
                "port {}: a window of {}",
                h.dst_port,
                h.buf_alloc
            );
            let edge = h.fwd_cnt.wrapping_add(h.buf_alloc);
            assert!(
                edge.wrapping_sub(p.edge) < 1 << 31,
                "port {}: the right edge moved left, from {} to {edge}",
                h.dst_port,
                p.edge
            );
            if h.buf_alloc < p.heard_buf {
                p.shrunk += 1;
            }
            (p.heard_fwd, p.heard_buf, p.edge) = (h.fwd_cnt, h.buf_alloc, edge);
        }
        heard
    }

    /// A lone connection is offered the most window once its guest has used half of what it
    /// had, all the device lends: it may send all of it with the host reading none, and a
    /// byte past it resets it.
    #[test]
    fn a_lone_connection_is_offered_the_most_window_and_reset_past_it() {
        let (mut m, arrived) = served_every();
        let mem = GuestMemory::anonymous(&[(PACKETS, 1 << 16)]).unwrap();
        let mut p = Peer::connect(&mut m, &arrived, &mem, 49_152, WINDOW_MOST);
        let heard = hear(&mut m, std::slice::from_mut(&mut p));
        assert_eq!(heard, [(49_152, op::RESPONSE, WINDOW_LEAST)]);
        p.send(&mut m, &mem, WINDOW_LEAST / 2);
        p.drain(&mut m);
        let heard = hear(&mut m, std::slice::from_mut(&mut p));
        assert_eq!(
            heard,
            [(49_152, op::CREDIT_UPDATE, WINDOW_MOST)],
            "a lone connection's window, its guest's buffer the most"
        );
        p.send_all(&mut m, &mem, false);
        assert_eq!(p.tx_cnt, WINDOW_LEAST / 2 + WINDOW_MOST);
        assert_eq!(
            m.conns[&p.key].interest(),
            (true, true),
            "what the window let through was refused"
        );
        p.send(&mut m, &mem, 1);
        let heard = hear(&mut m, std::slice::from_mut(&mut p));
        assert_eq!(
            heard.first().map(|&(_, op, _)| op),
            Some(op::RST),
            "a byte past the window was taken"
        );
    }

    /// Two connections share what the device lends: the second is given the least window
    /// while the first holds all of it, and all of it once the first's guest has sent what
    /// it was given and the host has taken that.
    #[test]
    fn two_connections_share_the_lent_window() {
        let (mut m, arrived) = served_every();
        let mem = GuestMemory::anonymous(&[(PACKETS, 1 << 16)]).unwrap();
        let mut peers = vec![
            Peer::connect(&mut m, &arrived, &mem, 49_152, WINDOW_MOST),
            Peer::connect(&mut m, &arrived, &mem, 49_153, WINDOW_MOST),
        ];
        hear(&mut m, &mut peers);
        for (i, window) in [(0, WINDOW_MOST), (1, WINDOW_LEAST)] {
            peers[i].send(&mut m, &mem, WINDOW_LEAST / 2);
            peers[i].drain(&mut m);
            let port = peers[i].key.peer_port;
            assert_eq!(hear(&mut m, &mut peers), [(port, op::CREDIT_UPDATE, window)]);
        }
        // The second uses half its window again; the first sends all it was given, which the
        // host takes.
        peers[1].send(&mut m, &mem, WINDOW_LEAST / 2);
        peers[1].drain(&mut m);
        peers[0].send_all(&mut m, &mem, true);
        assert_eq!(
            hear(&mut m, &mut peers),
            [
                (49_153, op::CREDIT_UPDATE, WINDOW_MOST),
                (49_152, op::CREDIT_UPDATE, WINDOW_LEAST)
            ]
        );
    }

    /// Connections side by side as a busy device has them: guests sending what Linux lets
    /// them, now and then past it; their buffers raised and lowered; hosts reading now and
    /// then; connections ending and new ones made. After every step the muxer's bounds hold
    /// ([`Muxer::check`]), no window is under the least and no right edge moves left
    /// ([`hear`]), no guest that kept to its credit is reset, and each host reads its guest's
    /// bytes in order. At the end, the hosts having taken all, every guest has credit.
    #[test]
    fn a_busy_device_keeps_its_window_bounds() {
        const STEPS: usize = 4000;
        let (mut m, arrived) = served_every();
        let mem = GuestMemory::anonymous(&[(PACKETS, 1 << 16)]).unwrap();
        // xorshift64* (Vigna, 2016), seeded: the same steps every run.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) % n as u64) as usize
        };
        let buffers = [16 * 1024, 256 * 1024, WINDOW_MOST];
        let mut peers: Vec<Peer> = Vec::new();
        // Guests that sent past what they were given.
        let mut broke: Vec<u32> = Vec::new();
        let (mut from, mut most_lent, mut grown, mut shrunk) = (50_000, 0, 0, 0);
        for step in 0..STEPS {
            let roll = next(100);
            let one = if peers.is_empty() {
                None
            } else {
                Some(next(peers.len()))
            };
            match (roll, one) {
                (0..=4, _) if peers.len() < 8 => {
                    let buf_alloc = buffers[next(buffers.len())];
                    peers.push(Peer::connect(&mut m, &arrived, &mem, from, buf_alloc));
                    from += 1;
                }
                (5, Some(i)) => {
                    let gone = peers.swap_remove(i);
                    shrunk += gone.shrunk;
                    let rst = Header {
                        op: op::RST,
                        ..request(54, gone.key.peer_port)
                    };
                    m.on_guest_packet(&rst, &[], &mem);
                }
                // Past what it was given, once it was given something.
                (6, Some(i)) if peers[i].heard_buf > 0 => {
                    let p = &mut peers[i];
                    let mut past = p.edge.wrapping_sub(p.tx_cnt).saturating_add(1);
                    while past > 0 {
                        let n = past.min(MAX_PAYLOAD);
                        p.send(&mut m, &mem, n);
                        past -= n;
                    }
                    broke.push(p.key.peer_port);
                }
                (7..=45, Some(i)) => {
                    let p = &mut peers[i];
                    let n = (1 + next(MAX_PAYLOAD as usize)) as u32;
                    let n = n.min(p.credit());
                    if n > 0 {
                        p.send(&mut m, &mem, n);
                    }
                }
                (46..=70, Some(i)) => peers[i].drain(&mut m),
                (71..=95, _) => {
                    let heard = hear(&mut m, &mut peers);
                    grown += heard
                        .iter()
                        .filter(|&&(_, op, window)| op == op::CREDIT_UPDATE && window > WINDOW_LEAST)
                        .count();
                }
                (96..=97, Some(i)) => {
                    // Its program sets its buffer, and Linux says so at once.
                    let p = &mut peers[i];
                    p.buf_alloc = buffers[next(buffers.len())];
                    let said = Header {
                        op: op::CREDIT_UPDATE,
                        buf_alloc: p.buf_alloc,
                        ..request(54, p.key.peer_port)
                    };
                    m.on_guest_packet(&said, &[], &mem);
                }
                _ => {}
            }
            m.check().unwrap_or_else(|e| panic!("step {step}: {e}"));
            most_lent = most_lent.max(m.lent);
            if let Some(p) = peers
                .iter()
                .find(|p| p.reset && !broke.contains(&p.key.peer_port))
            {
                panic!(
                    "step {step}: port {} was reset, its guest within what it was given",
                    p.key.peer_port
                );
            }
            shrunk += peers.iter().filter(|p| p.reset).map(|p| p.shrunk).sum::<usize>();
            peers.retain(|p| !p.reset);
        }
        // The hosts take all, and every guest hears what it is owed.
        for _ in 0..2 {
            for p in &mut peers {
                p.drain(&mut m);
            }
            hear(&mut m, &mut peers);
        }
        m.check().unwrap();
        for p in peers.iter().filter(|p| !broke.contains(&p.key.peer_port)) {
            assert!(!p.reset, "port {} was reset", p.key.peer_port);
            assert_eq!(
                p.read, p.tx_cnt as usize,
                "port {}: its host's bytes",
                p.key.peer_port
            );
            assert!(p.credit() > 0, "port {} waits for credit", p.key.peer_port);
        }
        shrunk += peers.iter().map(|p| p.shrunk).sum::<usize>();
        println!(
            "{STEPS} steps: lent at most {most_lent} of {WINDOW_LENT}; {grown} windows grown, \
             {shrunk} shrunk; {} guests reset past their windows",
            broke.len()
        );
        assert!(
            grown > 0 && shrunk > 0 && most_lent > 0,
            "the lent window was not exercised"
        );
    }

    /// A connection's host sockets ask for SOCKET_BUFFER each way where this process sizes
    /// them, and keep the kernel's own on Linux, whose VM process may make no setsockopt
    /// (SIZES_SOCKETS). Asked, a Unix kernel gives that, or more than it gave before where
    /// it cuts what an unprivileged process asks (Linux: net.core.wmem_max), and the socket
    /// carries bytes as before; Windows may refuse, which leaves it its size.
    #[test]
    fn host_sockets_ask_for_their_buffers_where_this_process_may() {
        let sizes = |socket| BUFFERS.map(|(option, _)| buffer_size(socket, option).unwrap());
        let (mut fresh, mut peer) = UnixStream::pair().unwrap();
        let before = sizes(fresh.raw_socket());
        for ((option, name), was) in BUFFERS.into_iter().zip(before) {
            match set_buffer(fresh.raw_socket(), option, SOCKET_BUFFER) {
                Ok(got) => assert!(
                    got >= SOCKET_BUFFER as usize || got > was,
                    "{name}: {was} bytes, then {got} asked for {SOCKET_BUFFER}"
                ),
                Err(e) if cfg!(windows) => println!("{name} refused: {e}"),
                Err(e) => panic!("{name}: {e}"),
            }
        }
        println!(
            "asked for {SOCKET_BUFFER} each way: {before:?} became {:?}",
            sizes(fresh.raw_socket())
        );
        fresh.write_all(b"x").unwrap();
        let mut byte = [0u8; 1];
        peer.read_exact(&mut byte).unwrap();
        // A served port's connection, both its ends.
        let (mut m, arrived) = served_every();
        let mem = GuestMemory::anonymous(&[(PACKETS, 1 << 16)]).unwrap();
        m.on_guest_packet(&request(54, 49_152), &[], &mem);
        let theirs = arrived.try_recv().unwrap();
        let key = Key {
            local_port: 54,
            peer_port: 49_152,
        };
        for made in [sizes(theirs.raw_socket()), sizes(m.conns[&key].fd())] {
            if cfg!(target_os = "linux") {
                assert_eq!(made, before, "a Linux VM process sized a socket");
            } else {
                assert!(
                    made.iter().all(|&s| s >= SOCKET_BUFFER as usize) || cfg!(windows),
                    "{made:?}"
                );
            }
        }
    }
}
