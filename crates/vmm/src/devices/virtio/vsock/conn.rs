//! One stream between a guest vsock port and a host Unix socket (virtio 1.3 §5.10.6.4-7).
//!
//! The protocol follows Firecracker's connection state machine (vsock/csm/connection.rs),
//! with one difference: EOF from the host is a half-close. It reaches the guest as
//! SHUTDOWN(SEND), and the guest may keep sending, as with `docker exec -i` after stdin
//! ends. A host socket that is gone both ways shows up as a failed write.

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::net::Shutdown;
use std::num::Wrapping;
use std::time::{Duration, Instant};

use super::packet::{HOST_CID, Header, TYPE_STREAM, op, shutdown};
use super::{Span, readv, writev};
use crate::debug;
use crate::memory::GuestMemory;
use crate::platform::{AsRawSocket, RawSocket, UnixStream};

mod ringed;
use ringed::Ringing;

/// The least window a connection is given (`buf_alloc`): what every connection had before
/// the device lent larger ones (Firecracker's CONN_TX_BUF_SIZE), one packet of the largest
/// size a driver sends (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE). A connection always has it, so it
/// always makes progress, and what the device may hold for host sockets that do not read is
/// never more than before: this per connection, and [`WINDOW_LENT`] besides.
pub const WINDOW_LEAST: u32 = 64 * 1024;
/// The most window a connection is given: the knee of guest-to-host throughput measured over
/// windows ([`shards_abi::VSOCK_WINDOW`]).
pub const WINDOW_MOST: u32 = shards_abi::VSOCK_WINDOW;
/// What the device lends past [`WINDOW_LEAST`], shared by all its connections: one
/// stream's whole window at the measured knee. Several streams share one device worker's
/// bandwidth, and so its window, between them.
pub const WINDOW_LENT: u32 = WINDOW_MOST - WINDOW_LEAST;
/// How long the guest has to answer a request, and to finish a shutdown.
/// Firecracker's CONN_REQUEST_TIMEOUT_MS and CONN_SHUTDOWN_TIMEOUT_MS.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Host-initiated: the guest has yet to answer our REQUEST.
    LocalInit,
    /// Guest-initiated: we owe the guest a RESPONSE.
    PeerInit,
    Established,
}

/// Packets the connection owes the guest; `next_rx` sends them in field order.
#[derive(Debug, Default, Clone, Copy)]
struct Pending {
    rst: bool,
    response: bool,
    request: bool,
    shutdown: u32,
    rw: bool,
    /// A credit update our forwarding owes the guest, unless a later packet carried it.
    credit_update: bool,
    /// The answer to the guest's CREDIT_REQUEST, owed whatever it last heard.
    credit_answer: bool,
}

pub struct Conn {
    stream: UnixStream,
    guest_cid: u64,
    local_port: u32,
    peer_port: u32,
    phase: Phase,
    /// The host sent EOF; the guest has been (or is about to be) told.
    host_eof: bool,
    /// What the guest's SHUTDOWN flags said.
    guest_rcv_closed: bool,
    guest_send_closed: bool,
    /// Our side toward the host is shut for writing, after the guest's SEND shutdown.
    host_write_shut: bool,
    killed: bool,
    pending: Pending,
    // Credit counters, free-running (virtio 1.3 §5.10.6.3).
    fwd_cnt: Wrapping<u32>,
    last_fwd_cnt_to_guest: Wrapping<u32>,
    guest_buf_alloc: u32,
    guest_fwd_cnt: Wrapping<u32>,
    rx_cnt: Wrapping<u32>,
    /// The window our latest packet gave the guest (`buf_alloc`).
    window: u32,
    /// The most the guest may have sent, as `fwd_cnt` counts: where our latest packet let it
    /// go (its `fwd_cnt` and `buf_alloc`), the furthest any did, as none moves it back
    /// ([`Conn::header`]).
    granted_until: Wrapping<u32>,
    /// Guest→host bytes the host socket has not taken yet: within what was granted.
    tx_buf: TxBuf,
    expiry: Option<Instant>,
    /// Whether the multiplexer has this connection in its RX queue.
    pub queued: bool,
    /// Whether `local_port` was allocated for a host-initiated connection.
    pub allocated_port: bool,
    /// Its engine ring, where it may have one (D230; ringed.rs).
    ringing: Ringing,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("local_port", &self.local_port)
            .field("peer_port", &self.peer_port)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

/// A connection ends with its write side shut down, then its socket closed, as Unix's
/// close ends it: on Windows a socket whose last reference its process's exit closes loses
/// what its peer had not read, and one shut down and closed first keeps it (PM M199).
#[cfg(windows)]
impl Drop for Conn {
    fn drop(&mut self) {
        if !self.host_write_shut {
            let _ = self.stream.shutdown(Shutdown::Write);
        }
    }
}

impl Conn {
    fn new(stream: UnixStream, guest_cid: u64, local_port: u32, peer_port: u32, phase: Phase) -> Conn {
        Conn {
            stream,
            guest_cid,
            local_port,
            peer_port,
            phase,
            host_eof: false,
            guest_rcv_closed: false,
            guest_send_closed: false,
            host_write_shut: false,
            killed: false,
            pending: Pending::default(),
            fwd_cnt: Wrapping(0),
            last_fwd_cnt_to_guest: Wrapping(0),
            guest_buf_alloc: 0,
            window: WINDOW_LEAST,
            granted_until: Wrapping(0),
            guest_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            tx_buf: TxBuf::default(),
            expiry: None,
            queued: false,
            allocated_port: false,
            ringing: Ringing::Plain,
        }
    }

    /// A host client asked (`CONNECT <peer_port>`) to reach a guest port.
    pub fn host_initiated(stream: UnixStream, guest_cid: u64, local_port: u32, peer_port: u32) -> Conn {
        let mut c = Conn::new(stream, guest_cid, local_port, peer_port, Phase::LocalInit);
        c.pending.request = true;
        c.allocated_port = true;
        c
    }

    /// The guest's REQUEST for host port `request.dst_port` reached a listening socket.
    pub fn guest_initiated(stream: UnixStream, guest_cid: u64, request: &Header) -> Conn {
        let mut c = Conn::new(
            stream,
            guest_cid,
            request.dst_port,
            request.src_port,
            Phase::PeerInit,
        );
        c.guest_buf_alloc = request.buf_alloc;
        c.guest_fwd_cnt = Wrapping(request.fwd_cnt);
        c.pending.response = true;
        c
    }

    pub fn fd(&self) -> RawSocket {
        self.stream.raw_socket()
    }

    pub fn has_pending_rx(&self) -> bool {
        let p = self.pending;
        p.rst
            || p.response
            || p.request
            || p.shutdown != 0
            || p.rw
            || p.credit_update
            || p.credit_answer
            || self.ring_owes()
    }

    pub fn expiry(&self) -> Option<Instant> {
        self.expiry
    }

    /// An ended connection, kept for its host socket until the socket's peer ends its side,
    /// if it has not: on Windows a socket closed while its peer may still read loses the
    /// peer what it had not read yet, where the closer held some of the peer's data unread
    /// and where the peer writes after the close (PM M358). None where the peer has ended
    /// its side, all it sent read, and the socket may close at once.
    ///
    /// What the guest sent that the host had not taken goes as its connection ends, as on
    /// Unix: its socket takes no more, and the muxer counts nothing a lingering one holds.
    #[cfg(windows)]
    pub fn linger(mut self) -> Option<Conn> {
        self.tx_buf = TxBuf::default();
        // Its ring goes with the connection; only the host socket lingers.
        self.ringing = Ringing::Plain;
        if !self.host_write_shut {
            self.host_write_shut = true;
            let _ = self.stream.shutdown(Shutdown::Write);
        }
        (!self.host_eof && !self.drained()).then_some(self)
    }

    /// Reads what a lingering connection's host socket holds, and drops it: whether the
    /// socket's peer has ended its side (end-of-stream, or an error: it is gone).
    #[cfg(windows)]
    pub fn drained(&mut self) -> bool {
        let mut sink = [0u8; 4096];
        loop {
            match io::Read::read(&mut &self.stream, &mut sink) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return e.kind() != io::ErrorKind::WouldBlock,
            }
        }
    }

    /// Ends the connection: an RST goes to the guest and buffered data is dropped.
    pub fn kill(&mut self) {
        self.killed = true;
        self.ringing = Ringing::Plain;
        self.tx_buf = TxBuf::default();
        self.pending = Pending {
            rst: true,
            ..Pending::default()
        };
        self.expiry = None;
    }

    /// Which host-socket events the connection waits for: (readable, writable).
    pub fn interest(&self) -> (bool, bool) {
        if let Some(ringed) = self.ring_interest() {
            return ringed;
        }
        let read = !self.killed
            && self.phase == Phase::Established
            && !self.host_eof
            && !self.guest_rcv_closed
            && !self.pending.rw
            && self.guest_credit() > 0;
        (read, !self.killed && !self.tx_buf.is_empty())
    }

    /// The host socket has data, EOF or an error to report.
    pub fn on_readable(&mut self) {
        if self.ring_on_readable() {
            return;
        }
        if !self.killed && self.phase == Phase::Established && !self.host_eof {
            self.pending.rw = true;
        }
    }

    /// The host socket can take more of the buffered guest data.
    pub fn on_writable(&mut self) {
        if let Err(e) = self.flush() {
            debug!("vsock: host socket for port {}: {e}", self.local_port);
            self.kill();
        }
    }

    /// Kills a connection whose deadline passed. Returns whether it did.
    pub fn expire(&mut self, now: Instant) -> bool {
        match self.expiry {
            Some(t) if t <= now => {
                debug!("vsock: port {} timed out in {:?}", self.local_port, self.phase);
                self.kill();
                true
            }
            _ => false,
        }
    }

    /// Bytes the guest can still receive (virtio 1.3 §5.10.6.3).
    fn guest_credit(&self) -> u32 {
        (Wrapping(self.guest_buf_alloc) - (self.rx_cnt - self.guest_fwd_cnt)).0
    }

    /// The most the guest may send us unacknowledged: our buffer, or its own where that is
    /// less. Linux caps a socket's TX credit at its own buf_alloc
    /// (net/vmw_vsock/virtio_transport_common.c `virtio_transport_tx_buf_size`, 8ee784fdf006,
    /// in the pinned 6.18.48), so a guest may stop well inside the window we give it.
    fn guest_window(&self) -> u32 {
        self.window.min(self.guest_buf_alloc)
    }

    /// What the guest may still send past what we have forwarded: our exposure, at most a
    /// window, its buffered bytes included.
    fn exposure(&self) -> u32 {
        (self.granted_until - self.fwd_cnt).0
    }

    /// The share of the device's [`WINDOW_LENT`] this connection holds.
    pub fn lent(&self) -> u32 {
        self.exposure().saturating_sub(WINDOW_LEAST)
    }

    /// What holds of a connection between any two of its calls (tests): its window within
    /// [`WINDOW_LEAST`] and [`WINDOW_MOST`]; what the guest may have sent past what was
    /// forwarded no more than [`WINDOW_MOST`], what it buffers within that, and the room it
    /// keeps within twice that or [`WINDOW_LEAST`]; and an update owed once half the guest's
    /// window is forwarded.
    #[cfg(test)]
    pub fn check(&self) -> Result<(), String> {
        let exposure = self.exposure();
        let (held, room) = (self.tx_buf.len(), self.tx_buf.data.capacity());
        let holds = (WINDOW_LEAST..=WINDOW_MOST).contains(&self.window)
            && exposure <= WINDOW_MOST
            && held <= exposure as usize
            && room <= (WINDOW_LEAST as usize).max(2 * exposure as usize)
            && (!self.guest_needs_credit() || self.has_pending_rx() || self.killed || self.guest_send_closed);
        if holds {
            return Ok(());
        }
        Err(format!(
            "port {} to guest port {}: window {}, exposure {exposure}, held {held}, room {room}, \
             credit owed {} pending {:?}, killed {}",
            self.local_port,
            self.peer_port,
            self.window,
            self.guest_needs_credit(),
            self.pending,
            self.killed
        ))
    }

    /// Whether to tell the guest what we have forwarded since it last heard: once that is
    /// half the window it may use, a receiver's window update at Fr = 1/2 of its buffer
    /// (RFC 1122 §4.2.3.3). It is then credited before it runs out, has the other half to
    /// send while the update travels, and hears at most twice a window.
    fn guest_needs_credit(&self) -> bool {
        let unseen = (self.fwd_cnt - self.last_fwd_cnt_to_guest).0;
        unseen > 0 && unseen >= self.guest_window().div_ceil(2)
    }

    /// A packet for the guest. It carries our credit, so the guest's view is now current,
    /// and our window. That grows, as far as the guest's own buffer lets it use (Linux sends
    /// no more unacknowledged, 8ee784fdf006), once the guest has used half of what it has
    /// ([`Conn::guest_needs_credit`]), and otherwise stays: a connection whose guest sends
    /// little holds none of the device's lent window. It is never more than
    /// [`WINDOW_LEAST`] and the lent window `spare` says no other connection holds; nor less
    /// than [`WINDOW_LEAST`], or than what earlier packets let the guest send: its right edge
    /// never moves left (RFC 1122 §4.2.2.16), as a Linux guest before 3ef3d52a1a98 (6.19)
    /// takes a shrunk window's credit unsigned, past what it was ever given.
    fn header(&mut self, op: u16, spare: u32) -> Header {
        let asked = if self.guest_needs_credit() {
            WINDOW_MOST
        } else {
            self.window
        };
        let room = WINDOW_LEAST.saturating_add(spare.min(WINDOW_LENT));
        self.window = asked
            .min(self.guest_buf_alloc)
            .min(room)
            .max(WINDOW_LEAST)
            .max(self.exposure());
        self.last_fwd_cnt_to_guest = self.fwd_cnt;
        self.granted_until = self.fwd_cnt + Wrapping(self.window);
        Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: self.local_port,
            dst_port: self.peer_port,
            len: 0,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: self.window,
            fwd_cnt: self.fwd_cnt.0,
        }
    }

    /// The next packet for the guest, if one is due. A data packet's payload is read from
    /// the host socket straight into `space`, the guest buffer after the header.
    /// `spare`: the device's lent window no other connection holds ([`Conn::header`]).
    pub fn next_rx(&mut self, space: &[Span], spare: u32) -> Option<Header> {
        if self.pending.rst {
            self.pending = Pending::default();
            return Some(self.header(op::RST, spare));
        }
        if self.pending.response {
            self.pending.response = false;
            self.phase = Phase::Established;
            return Some(self.header(op::RESPONSE, spare));
        }
        if self.pending.request {
            self.pending.request = false;
            self.expiry = Some(Instant::now() + REQUEST_TIMEOUT);
            return Some(self.header(op::REQUEST, spare));
        }
        if self.pending.shutdown != 0 {
            let flags = std::mem::take(&mut self.pending.shutdown);
            let mut h = self.header(op::SHUTDOWN, spare);
            h.flags = flags;
            return Some(h);
        }
        if let Some(h) = self.ring_next_rx(space) {
            return Some(h);
        }
        if self.pending.rw
            && let Some(h) = self.read_host(space, spare)
        {
            return Some(h);
        }
        if self.pending.credit_update || self.pending.credit_answer {
            let asked = std::mem::take(&mut self.pending.credit_answer);
            self.pending.credit_update = false;
            // A packet sent since an update was queued may already have carried the credit;
            // the guest's own request is answered whatever it last heard: "the reply to this
            // query is a VIRTIO_VSOCK_OP_CREDIT_UPDATE packet" (virtio 1.3 §5.10.6.3).
            if asked || self.guest_needs_credit() {
                return Some(self.header(op::CREDIT_UPDATE, spare));
            }
        }
        None
    }

    fn read_host(&mut self, space: &[Span], spare: u32) -> Option<Header> {
        if self.guest_rcv_closed || self.host_eof {
            self.pending.rw = false;
            return None;
        }
        let credit = self.guest_credit();
        if credit == 0 {
            // Reading resumes when the guest's credit update arrives.
            self.pending.rw = false;
            return Some(self.header(op::CREDIT_REQUEST, spare));
        }
        match readv(&self.stream, space, credit as usize) {
            Ok(0) => {
                // A half-close: the host may still read what the guest sends.
                self.pending.rw = false;
                self.host_eof = true;
                let mut h = self.header(op::SHUTDOWN, spare);
                h.flags = shutdown::SEND;
                Some(h)
            }
            Ok(n) => {
                // More may be waiting: keep reading while the guest has buffers.
                self.rx_cnt += n as u32;
                let mut h = self.header(op::RW, spare);
                h.len = n as u32;
                Some(h)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.pending.rw = false;
                None
            }
            Err(e) => {
                debug!("vsock: reading host socket for port {}: {e}", self.local_port);
                self.kill();
                self.pending = Pending::default();
                Some(self.header(op::RST, spare))
            }
        }
    }

    /// Handles a packet from the guest for this connection (not RST, which the
    /// multiplexer handles by removing the connection).
    pub fn on_guest_packet(&mut self, h: &Header, payload: &[Span], mem: &GuestMemory) {
        self.guest_buf_alloc = h.buf_alloc;
        self.guest_fwd_cnt = Wrapping(h.fwd_cnt);
        if self.killed {
            return;
        }
        match (h.op, self.phase) {
            (op::RW, Phase::Established) if !self.guest_send_closed => {
                if h.len == 0 || self.ring_doorbells(h, payload, mem) {
                    return;
                }
                if let Err(e) = self.send_host(payload, mem) {
                    debug!("vsock: guest data for port {}: {e}", self.local_port);
                    self.kill();
                }
            }
            (op::RESPONSE, Phase::LocalInit) => {
                self.phase = Phase::Established;
                self.expiry = None;
                // A fresh socket's buffer takes this line whole.
                let line = format!("OK {}\n", self.local_port);
                if self.stream.write(line.as_bytes()).ok() != Some(line.len()) {
                    self.kill();
                }
            }
            (op::SHUTDOWN, Phase::Established) => {
                self.guest_rcv_closed |= h.flags & shutdown::RCV != 0;
                self.guest_send_closed |= h.flags & shutdown::SEND != 0;
                if self.guest_rcv_closed {
                    self.pending.rw = false;
                }
                if let Err(e) = self.flush() {
                    debug!("vsock: host socket for port {}: {e}", self.local_port);
                    self.kill();
                } else if self.guest_rcv_closed && self.guest_send_closed && !self.tx_buf.is_empty() {
                    self.expiry = Some(Instant::now() + SHUTDOWN_TIMEOUT);
                }
            }
            (op::CREDIT_UPDATE, _) => {}
            (op::CREDIT_REQUEST, Phase::Established | Phase::PeerInit) => {
                self.pending.credit_answer = true;
            }
            _ => debug!(
                "vsock: dropping op {} for port {} in {:?}",
                h.op, self.local_port, self.phase
            ),
        }
        // Every packet says the guest's buffer, which bounds what it sends (8ee784fdf006):
        // with half of its window forwarded, by its data or by its buffer shrinking, the
        // guest is owed an update, which nothing later may owe it.
        if !self.killed && !self.guest_send_closed && self.guest_needs_credit() {
            self.pending.credit_update = true;
        }
    }

    /// Writes guest data to the host socket; what it does not take now is buffered.
    fn send_host(&mut self, payload: &[Span], mem: &GuestMemory) -> io::Result<()> {
        let total: usize = payload.iter().map(|s| s.len).sum();
        if self.tx_buf.len() + total > self.exposure() as usize {
            return Err(io::Error::other("guest exceeded its credit"));
        }
        let mut written = 0;
        if self.tx_buf.is_empty() {
            written = match writev(&self.stream, payload, total) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                Err(e) => return Err(e),
            };
            self.fwd_cnt += written as u32;
        }
        let limit = self.exposure() as usize;
        let mut skip = written;
        for s in payload {
            if skip >= s.len {
                skip -= s.len;
                continue;
            }
            let gpa = s
                .gpa
                .checked_add(skip as u64)
                .ok_or_else(|| io::Error::other("guest buffer past the address space"))?;
            self.tx_buf.append(mem, gpa, s.len - skip, limit)?;
            skip = 0;
        }
        Ok(())
    }

    /// Moves buffered data to the host, then finishes whatever the guest's shutdown asked
    /// for once nothing is left.
    fn flush(&mut self) -> io::Result<()> {
        let n = self.tx_buf.flush_to(&mut self.stream)?;
        self.fwd_cnt += n as u32;
        self.tx_buf.fit(self.exposure() as usize);
        if !self.tx_buf.is_empty() || !self.ring_drained()? {
            return Ok(());
        }
        if self.guest_send_closed && !self.host_write_shut {
            self.host_write_shut = true;
            // The host reads EOF. It may already be gone, which the next read shows.
            let _ = self.stream.shutdown(Shutdown::Write);
        }
        if self.guest_rcv_closed && self.guest_send_closed {
            // Both directions are over; RST ends the connection (virtio 1.3 §5.10.6.6).
            self.pending = Pending {
                rst: true,
                ..Pending::default()
            };
            self.expiry = None;
        } else if n > 0 && self.guest_needs_credit() {
            self.pending.credit_update = true;
        }
        Ok(())
    }
}

/// Guest→host bytes waiting for the host socket, in a ring: what the socket takes leaves
/// the rest where it is, however large the window.
#[derive(Debug, Default)]
struct TxBuf {
    data: VecDeque<u8>,
}

impl TxBuf {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Appends `len` bytes of guest memory at `gpa`, `limit` being the most the buffer may
    /// have to hold: what the guest may have sent past what the host took (send_host).
    fn append(&mut self, mem: &GuestMemory, gpa: u64, len: usize, limit: usize) -> io::Result<()> {
        let old = self.data.len();
        let needed = old
            .checked_add(len)
            .ok_or_else(|| io::Error::other("guest data past the address space"))?;
        // Doubling as it grows, but never past `limit`, which bounds what the guest may have
        // buffered: a buffer left to itself would double past it, 65,535 bytes to 131,070
        // for one more (audit D09).
        if needed > self.data.capacity() {
            let target = needed
                .max(self.data.capacity().saturating_mul(2))
                .min(limit.max(needed));
            self.data
                .try_reserve_exact(target - old)
                .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
        }
        self.data.resize(needed, 0);
        // The new bytes are the ring's last `len`: the end of its first part and all of its
        // second, or the end of its second.
        let (front, back) = self.data.as_mut_slices();
        let (first, second) = match old.checked_sub(front.len()) {
            Some(into_back) => (
                back.get_mut(into_back..).unwrap_or_default(),
                <&mut [u8]>::default(),
            ),
            None => (front.get_mut(old..).unwrap_or_default(), back),
        };
        let access = mem.access().map_err(io::Error::other)?;
        let read = |gpa: u64, buf: &mut [u8]| {
            access
                .read(gpa, buf)
                .map_err(|e| io::Error::other(format!("guest buffer: {e}")))
        };
        read(gpa, first)?;
        let next = gpa
            .checked_add(first.len() as u64)
            .ok_or_else(|| io::Error::other("guest data past the address space"))?;
        read(next, second)
    }

    /// Writes as much as the socket takes without blocking; frees the buffer once empty.
    fn flush_to(&mut self, stream: &mut UnixStream) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let (front, _) = self.data.as_slices();
            if front.is_empty() {
                break;
            }
            match stream.write(front) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    let n = n.min(front.len());
                    self.data.drain(..n);
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if self.data.is_empty() {
            *self = TxBuf::default();
        }
        Ok(total)
    }

    /// Gives back room past twice `limit`, the most the buffer may have to hold now that the
    /// host took some, down to `limit`: a buffer grown for a window since lent elsewhere
    /// holds no more than its connection's share. Its room was at most what it might hold as
    /// it was made ([`TxBuf::append`]), so a fall below half of that forwarded more bytes
    /// than the move copies. Room up to [`WINDOW_LEAST`] stays, as every connection has
    /// that. Where the smaller buffer cannot be had, the larger stays.
    fn fit(&mut self, limit: usize) {
        let room = self.data.capacity();
        if room <= WINDOW_LEAST as usize || room <= limit.saturating_mul(2) {
            return;
        }
        let mut smaller = VecDeque::new();
        if smaller.try_reserve_exact(limit.max(self.data.len())).is_ok() {
            let (front, back) = self.data.as_slices();
            smaller.extend(front);
            smaller.extend(back);
            self.data = smaller;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::io::Read as _;

    use super::*;

    const GUEST_CID: u64 = 3;
    const BASE: u64 = 0x8000_0000;

    struct Guest {
        mem: GuestMemory,
    }

    impl Guest {
        fn new() -> Guest {
            Guest {
                mem: GuestMemory::anonymous(&[(BASE, 1 << 20)]).unwrap(),
            }
        }
        fn span(&self, gpa: u64, len: usize) -> Span {
            Span {
                gpa,
                ptr: self.mem.host_ptr(gpa, len).unwrap(),
                len,
            }
        }
        fn guest_header(op: u16, len: u32, buf_alloc: u32, fwd_cnt: u32) -> Header {
            Header {
                src_cid: GUEST_CID,
                dst_cid: HOST_CID,
                src_port: 1000,
                dst_port: 5000,
                len,
                kind: TYPE_STREAM,
                op,
                flags: 0,
                buf_alloc,
                fwd_cnt,
            }
        }
    }

    /// Flushes the connection into the host socket and reads it until `want` bytes came
    /// through: socket buffers are small (8 KiB for macOS Unix sockets), so data moves
    /// in turns.
    fn pump(c: &mut Conn, host: &mut UnixStream, want: usize) -> Vec<u8> {
        let mut got = Vec::with_capacity(want);
        let mut buf = vec![0u8; 64 * 1024];
        while got.len() < want {
            c.on_writable();
            match host.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("reading the host end: {e}"),
            }
        }
        got
    }

    /// A guest-initiated connection that has sent its RESPONSE.
    fn established(g: &Guest, buf_alloc: u32) -> (Conn, UnixStream) {
        let (ours, host) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        host.set_nonblocking(true).unwrap();
        let req = Guest::guest_header(op::REQUEST, 0, buf_alloc, 0);
        let mut c = Conn::guest_initiated(ours, GUEST_CID, &req);
        let space = [g.span(BASE, 4096)];
        assert_eq!(c.next_rx(&space, 0).unwrap().op, op::RESPONSE);
        assert!(!c.has_pending_rx());
        (c, host)
    }

    /// What a connection buffers for the host never has room for more than the guest may
    /// have sent: filled a byte short, then to the brim, its capacity stays within that, the
    /// least window and the most, where doubling would take it to twice that (audit D09);
    /// and so it does filled in small pieces. The bytes are the guest's, in order.
    #[test]
    fn a_buffer_never_holds_room_past_the_credit() {
        let g = Guest::new();
        let region = 1 << 20;
        g.mem
            .access()
            .unwrap()
            .write(BASE, &(0..region).map(|i| i as u8).collect::<Vec<_>>())
            .unwrap();
        // `len` bytes from the region's start, a region at a time: each byte its offset.
        let fill = |b: &mut TxBuf, mut len: usize, limit: usize| {
            while len > 0 {
                let n = len.min(region);
                b.append(&g.mem, BASE, n, limit).unwrap();
                len -= n;
            }
        };
        for full in [WINDOW_LEAST as usize, WINDOW_MOST as usize] {
            let mut b = TxBuf::default();
            fill(&mut b, full - 1, full);
            b.append(&g.mem, BASE + ((full - 1) % region) as u64, 1, full)
                .unwrap();
            assert_eq!(b.len(), full);
            assert!(
                b.data.capacity() <= full,
                "{} bytes of room for {full}",
                b.data.capacity()
            );
            assert!(b.data.iter().enumerate().all(|(i, &v)| v == i as u8));
        }
        let full = WINDOW_LEAST as usize;
        let mut small = TxBuf::default();
        for i in 0..full / 100 {
            small.append(&g.mem, BASE + (100 * i) as u64, 100, full).unwrap();
            assert!(
                small.data.capacity() <= full,
                "{} bytes of room",
                small.data.capacity()
            );
        }
    }

    /// The ring hands the host the guest's bytes in order however appends and partial
    /// writes interleave and its parts wrap, never with room past what it may have to hold;
    /// it gives room back as the host takes its bytes ([`TxBuf::fit`]), and frees it once
    /// empty.
    #[test]
    fn a_ring_keeps_the_guests_order_and_gives_back_its_room() {
        let g = Guest::new();
        // Each byte its offset modulo a prime, so that one out of place shows.
        let pattern: Vec<u8> = (0..1usize << 20).map(|i| (i % 251) as u8).collect();
        g.mem.access().unwrap().write(BASE, &pattern).unwrap();
        let (mut ours, mut host) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        host.set_nonblocking(true).unwrap();
        let limit = WINDOW_MOST as usize;
        let mut b = TxBuf::default();
        let (mut sent, mut got) = (0usize, Vec::new());
        let mut buf = vec![0u8; 7919];
        let mut read = |host: &mut UnixStream, got: &mut Vec<u8>| match host.read(&mut buf) {
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("reading the host end: {e}"),
        };
        let mut wrapped = false;
        for step in 0..2000usize {
            let len = 1 + (step * 7717) % super::super::packet::MAX_PAYLOAD as usize;
            if b.len() + len <= limit {
                b.append(&g.mem, BASE + (sent % 251) as u64, len, limit).unwrap();
                sent += len;
            }
            wrapped |= !b.data.as_slices().1.is_empty();
            assert!(b.data.capacity() <= limit, "{} bytes of room", b.data.capacity());
            if step % 3 == 0 {
                b.flush_to(&mut ours).unwrap();
            }
            if step % 5 == 0 {
                read(&mut host, &mut got);
            }
        }
        assert!(wrapped, "the ring never wrapped");
        // The host takes most of it: the room it keeps follows what is left.
        while b.len() > 100_000 {
            b.flush_to(&mut ours).unwrap();
            read(&mut host, &mut got);
        }
        let left = b.len();
        b.fit(left);
        assert!(
            b.data.capacity() <= (WINDOW_LEAST as usize).max(2 * left),
            "{} bytes of room for {left}",
            b.data.capacity()
        );
        while got.len() < sent {
            b.flush_to(&mut ours).unwrap();
            read(&mut host, &mut got);
        }
        assert_eq!(b.data.capacity(), 0, "an empty buffer keeps no room");
        assert_eq!(got.len(), sent);
        let first_wrong = got.iter().enumerate().find(|&(i, &v)| v != (i % 251) as u8);
        assert_eq!(first_wrong, None, "of {sent} bytes");
    }

    /// Sends `len` bytes, packets of the largest size a driver sends, from a guest whose
    /// buffer is `buf_alloc`.
    fn send(c: &mut Conn, g: &Guest, len: usize, buf_alloc: u32) {
        let most = super::super::packet::MAX_PAYLOAD as usize;
        let mut left = len;
        while left > 0 {
            let n = left.min(most);
            let h = Guest::guest_header(op::RW, n as u32, buf_alloc, 0);
            c.on_guest_packet(&h, &[g.span(BASE, n)], &g.mem);
            left -= n;
        }
    }

    /// A guest that sends little is given none of the device's lent window, however much of
    /// it is spare: its question is answered within the least window. Once it has used half
    /// of that, its window grows to the most, as its buffer is.
    #[test]
    fn a_window_grows_only_once_its_guest_uses_it() {
        let g = Guest::new();
        let (ours, mut host) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        host.set_nonblocking(true).unwrap();
        let req = Guest::guest_header(op::REQUEST, 0, WINDOW_MOST, 0);
        let mut c = Conn::guest_initiated(ours, GUEST_CID, &req);
        let space = [g.span(BASE, 4096)];
        let resp = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!(
            (resp.op, resp.buf_alloc, c.lent()),
            (op::RESPONSE, WINDOW_LEAST, 0),
            "a connection held lent window before its guest sent anything"
        );
        send(&mut c, &g, 100, WINDOW_MOST);
        assert_eq!(pump(&mut c, &mut host, 100).len(), 100);
        let ask = Guest::guest_header(op::CREDIT_REQUEST, 0, WINDOW_MOST, 0);
        c.on_guest_packet(&ask, &[], &g.mem);
        let answer = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!(
            (answer.op, answer.buf_alloc, c.lent()),
            (op::CREDIT_UPDATE, WINDOW_LEAST, 0)
        );
        let half = (WINDOW_LEAST / 2) as usize;
        send(&mut c, &g, half, WINDOW_MOST);
        assert_eq!(pump(&mut c, &mut host, half).len(), half);
        let update = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!(
            (update.op, update.buf_alloc, c.lent()),
            (op::CREDIT_UPDATE, WINDOW_MOST, WINDOW_LENT)
        );
        c.check().unwrap();
    }

    /// A window grows no further than the guest's own buffer, all Linux sends
    /// unacknowledged (8ee784fdf006), whatever the device has spare: a default socket's
    /// 256 KiB. Once the guest raises its buffer, as the engine relay does, it grows to the
    /// most.
    #[test]
    fn a_window_grows_no_further_than_the_guests_buffer() {
        let g = Guest::new();
        let default = 256 * 1024;
        let (mut c, mut host) = established(&g, default);
        let space = [g.span(BASE, 4096)];
        let half = (WINDOW_LEAST / 2) as usize;
        send(&mut c, &g, half, default);
        assert_eq!(pump(&mut c, &mut host, half).len(), half);
        let update = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!(
            (update.op, update.buf_alloc),
            (op::CREDIT_UPDATE, default),
            "a window past what the guest's buffer lets it send"
        );
        // SO_VM_SOCKETS_BUFFER_SIZE raised: Linux says so at once.
        let raised = Guest::guest_header(op::CREDIT_UPDATE, 0, WINDOW_MOST, 0);
        c.on_guest_packet(&raised, &[], &g.mem);
        let half = default as usize / 2;
        send(&mut c, &g, half, WINDOW_MOST);
        assert_eq!(pump(&mut c, &mut host, half).len(), half);
        let update = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!((update.op, update.buf_alloc), (op::CREDIT_UPDATE, WINDOW_MOST));
        c.check().unwrap();
    }

    /// What a window let the guest send stays its to send when the device's spare window
    /// shrinks: a later packet's window is what is left of the earlier one, its right edge
    /// where it was (RFC 1122 §4.2.2.16), and all of that is taken with the host reading
    /// none. A byte past it resets the connection.
    #[test]
    fn a_window_never_takes_back_what_it_gave() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, WINDOW_MOST);
        let space = [g.span(BASE, 4096)];
        let half = (WINDOW_LEAST / 2) as usize;
        send(&mut c, &g, half, WINDOW_MOST);
        assert_eq!(pump(&mut c, &mut host, half).len(), half);
        let grant = c.next_rx(&space, WINDOW_LENT).unwrap();
        assert_eq!(grant.buf_alloc, WINDOW_MOST);
        let edge = grant.fwd_cnt.wrapping_add(grant.buf_alloc);
        // A mebibyte of it, which the host takes. Then other connections hold all the device
        // lends, and the guest asks.
        let mib = 1 << 20;
        send(&mut c, &g, mib, WINDOW_MOST);
        assert_eq!(pump(&mut c, &mut host, mib).len(), mib);
        let ask = Guest::guest_header(op::CREDIT_REQUEST, 0, WINDOW_MOST, 0);
        c.on_guest_packet(&ask, &[], &g.mem);
        let shrunk = c.next_rx(&space, 0).unwrap();
        assert_eq!(shrunk.op, op::CREDIT_UPDATE);
        assert_eq!(
            shrunk.fwd_cnt.wrapping_add(shrunk.buf_alloc),
            edge,
            "the right edge moved: window {} at {}",
            shrunk.buf_alloc,
            shrunk.fwd_cnt
        );
        assert!(shrunk.buf_alloc < WINDOW_MOST);
        // The rest of it, the host reading none: taken. A byte more: reset.
        let rest = edge.wrapping_sub(shrunk.fwd_cnt) as usize;
        send(&mut c, &g, rest, WINDOW_MOST);
        assert!(!c.killed, "what the window let through was refused");
        c.check().unwrap();
        send(&mut c, &g, 1, WINDOW_MOST);
        assert!(c.killed, "a byte past the window was taken");
        assert_eq!(c.next_rx(&space, 0).unwrap().op, op::RST);
    }

    /// A guest whose program lowers its buffer (SO_VM_SOCKETS_BUFFER_SIZE) may have half of
    /// its smaller window forwarded already, and then sends no more until it hears (Linux
    /// caps its credit at its buffer, 8ee784fdf006): the update is owed as its buffer is
    /// said, as no later data may come to owe it.
    #[test]
    fn a_guest_whose_buffer_shrinks_is_owed_its_update() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 256 * 1024);
        let sent = 16 * 1024;
        send(&mut c, &g, sent, 256 * 1024);
        assert_eq!(pump(&mut c, &mut host, sent).len(), sent);
        assert!(
            !c.has_pending_rx(),
            "a quarter of its window forwarded owes nothing yet"
        );
        let lowered = Guest::guest_header(op::CREDIT_UPDATE, 0, sent as u32, 0);
        c.on_guest_packet(&lowered, &[], &g.mem);
        let owed = c.next_rx(&[g.span(BASE, 4096)], 0).map(|h| (h.op, h.fwd_cnt));
        assert_eq!(
            owed,
            Some((op::CREDIT_UPDATE, sent as u32)),
            "a guest whose buffer shrank waits for an update that never comes"
        );
    }

    /// A connection kept for its host socket's peer (PM M358) holds none of what the guest
    /// sent that its host had not taken: its socket takes no more.
    #[cfg(windows)]
    #[test]
    fn a_lingering_connection_holds_none_of_what_its_host_did_not_take() {
        let g = Guest::new();
        let (mut c, _host) = established(&g, 256 * 1024);
        c.tx_buf
            .append(&g.mem, BASE, 1000, WINDOW_LEAST as usize)
            .unwrap();
        let lingering = c.linger().expect("its host's end is open");
        assert_eq!((lingering.tx_buf.len(), lingering.tx_buf.data.capacity()), (0, 0));
    }

    #[test]
    fn guest_data_reaches_the_host_and_credit_flows_back() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 256 * 1024);
        // The guest may use our whole 64 KiB window: the update is owed at half of it.
        let half = (WINDOW_LEAST / 2) as usize;
        let data: Vec<u8> = (0..half - 1).map(|i| i as u8).collect();
        g.mem.access().unwrap().write(BASE, &data).unwrap();
        let h = Guest::guest_header(op::RW, data.len() as u32, 256 * 1024, 0);
        c.on_guest_packet(&h, &[g.span(BASE, data.len())], &g.mem);
        assert_eq!(pump(&mut c, &mut host, data.len()), data);
        // A byte short of half the window forwarded: nothing is owed yet.
        assert!(!c.has_pending_rx());
        let more = Guest::guest_header(op::RW, 1, 256 * 1024, 0);
        c.on_guest_packet(&more, &[g.span(BASE, 1)], &g.mem);
        assert_eq!(pump(&mut c, &mut host, 1).len(), 1);
        // Half the window forwarded: a credit update is owed.
        let upd = c.next_rx(&[g.span(BASE, 4096)], 0).unwrap();
        assert_eq!(
            (upd.op, upd.fwd_cnt, upd.buf_alloc),
            (op::CREDIT_UPDATE, half as u32, WINDOW_LEAST)
        );
    }

    /// A guest that may hold back less than our window is credited before it runs out.
    /// Linux sends no more than the lesser of our buffer and its own unacknowledged
    /// (virtio_transport_tx_buf_size, 8ee784fdf006; in the pinned 6.18.48), so a credit
    /// owed only once our whole window is forwarded never comes and it waits for ever.
    #[test]
    fn a_guest_buffer_below_the_window_is_credited_before_it_is_used_up() {
        let g = Guest::new();
        let small: u32 = 16 * 1024;
        let (mut c, mut host) = established(&g, small);
        // The guest sends its whole buffer, then waits for credit.
        g.mem
            .access()
            .unwrap()
            .write(BASE, &vec![5u8; small as usize])
            .unwrap();
        let h = Guest::guest_header(op::RW, small, small, 0);
        c.on_guest_packet(&h, &[g.span(BASE, small as usize)], &g.mem);
        assert_eq!(pump(&mut c, &mut host, small as usize).len(), small as usize);
        let owed = c.next_rx(&[g.span(BASE, 4096)], 0).map(|h| (h.op, h.fwd_cnt));
        assert_eq!(
            owed,
            Some((op::CREDIT_UPDATE, small)),
            "a guest whose buffer is used up waits for a credit update that never comes"
        );
    }

    /// A guest's CREDIT_REQUEST is answered whatever it last heard: "the reply to this
    /// query is a VIRTIO_VSOCK_OP_CREDIT_UPDATE packet" (virtio 1.3 §5.10.6.3).
    #[test]
    fn a_credit_request_is_answered() {
        let g = Guest::new();
        let (mut c, _host) = established(&g, 256 * 1024);
        let ask = Guest::guest_header(op::CREDIT_REQUEST, 0, 256 * 1024, 0);
        c.on_guest_packet(&ask, &[], &g.mem);
        let answer = c.next_rx(&[g.span(BASE, 4096)], 0).map(|h| (h.op, h.buf_alloc));
        assert_eq!(
            answer,
            Some((op::CREDIT_UPDATE, WINDOW_LEAST)),
            "a guest's credit request went unanswered"
        );
    }

    #[test]
    fn host_data_is_read_into_guest_memory_within_the_guest_credit() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 3000);
        host.write_all(&[7u8; 5000]).unwrap();
        c.on_readable();
        let space = [g.span(BASE + 44, 4096 - 44)];
        let h = c.next_rx(&space, 0).unwrap();
        assert_eq!((h.op, h.len), (op::RW, 3000));
        let mut got = vec![0u8; 3000];
        g.mem.access().unwrap().read(BASE + 44, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 7));
        // Out of credit: the connection asks rather than reads.
        let h = c.next_rx(&space, 0).unwrap();
        assert_eq!(h.op, op::CREDIT_REQUEST);
        assert_eq!(c.interest(), (false, false));
        // The guest consumed it all: reading resumes.
        let upd = Guest::guest_header(op::CREDIT_UPDATE, 0, 3000, 3000);
        c.on_guest_packet(&upd, &[], &g.mem);
        assert_eq!(c.interest(), (true, false));
    }

    #[test]
    fn host_eof_is_a_half_close() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 256 * 1024);
        host.shutdown(Shutdown::Write).unwrap();
        c.on_readable();
        let h = c.next_rx(&[g.span(BASE, 4096)], 0).unwrap();
        assert_eq!((h.op, h.flags), (op::SHUTDOWN, shutdown::SEND));
        // The guest still sends, and the host still receives.
        g.mem.access().unwrap().write(BASE, b"reply").unwrap();
        let rw = Guest::guest_header(op::RW, 5, 256 * 1024, 0);
        c.on_guest_packet(&rw, &[g.span(BASE, 5)], &g.mem);
        let mut got = [0u8; 5];
        host.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"reply");
        // The guest closes: its data is flushed, the host reads EOF, and RST ends it.
        let bye = Header {
            flags: shutdown::RCV | shutdown::SEND,
            ..Guest::guest_header(op::SHUTDOWN, 0, 256 * 1024, 0)
        };
        c.on_guest_packet(&bye, &[], &g.mem);
        assert_eq!(host.read(&mut got).unwrap(), 0);
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)], 0).unwrap().op, op::RST);
    }

    #[test]
    fn a_guest_that_ignores_credit_is_reset() {
        let g = Guest::new();
        let (mut c, host) = established(&g, 256 * 1024);
        // Fill the host socket so nothing more is taken, then exceed WINDOW_LEAST.
        let big = vec![1u8; 1 << 20];
        g.mem
            .access()
            .unwrap()
            .write(BASE, &big[..WINDOW_LEAST as usize])
            .unwrap();
        let mut sent = 0usize;
        for _ in 0..64 {
            let h = Guest::guest_header(op::RW, WINDOW_LEAST, 256 * 1024, 0);
            c.on_guest_packet(&h, &[g.span(BASE, WINDOW_LEAST as usize)], &g.mem);
            sent += WINDOW_LEAST as usize;
            if c.has_pending_rx() && c.killed {
                break;
            }
        }
        assert!(
            c.killed,
            "{sent} bytes past a full socket never tripped the credit check"
        );
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)], 0).unwrap().op, op::RST);
        drop(host);
    }

    #[test]
    fn host_initiated_connections_answer_ok_or_time_out() {
        let g = Guest::new();
        let (ours, mut host) = UnixStream::pair().unwrap();
        let mut c = Conn::host_initiated(ours, GUEST_CID, 1 << 30, 1234);
        let req = c.next_rx(&[g.span(BASE, 4096)], 0).unwrap();
        assert_eq!((req.op, req.src_port, req.dst_port), (op::REQUEST, 1 << 30, 1234));
        let resp = Header {
            src_port: 1234,
            dst_port: 1 << 30,
            ..Guest::guest_header(op::RESPONSE, 0, 4096, 0)
        };
        c.on_guest_packet(&resp, &[], &g.mem);
        let mut line = [0u8; 14];
        host.read_exact(&mut line).unwrap();
        assert_eq!(&line, b"OK 1073741824\n");

        let (ours, _host) = UnixStream::pair().unwrap();
        let mut c = Conn::host_initiated(ours, GUEST_CID, (1 << 30) + 1, 1234);
        c.next_rx(&[g.span(BASE, 4096)], 0).unwrap();
        assert!(!c.expire(Instant::now()));
        assert!(c.expire(Instant::now() + REQUEST_TIMEOUT));
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)], 0).unwrap().op, op::RST);
    }
}
