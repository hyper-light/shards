//! One stream between a guest vsock port and a host Unix socket (virtio 1.3 §5.10.6.4-7).
//!
//! The protocol follows Firecracker's connection state machine (vsock/csm/connection.rs),
//! with one difference: EOF from the host is a half-close. It reaches the guest as
//! SHUTDOWN(SEND), and the guest may keep sending, as with `docker exec -i` after stdin
//! ends. A host socket that is gone both ways shows up as a failed write.

use std::io::{self, Write as _};
use std::net::Shutdown;
use std::num::Wrapping;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::packet::{HOST_CID, Header, TYPE_STREAM, op, shutdown};
use super::{Span, readv, writev};
use crate::debug;
use crate::memory::GuestMemory;

/// Receive buffer advertised per connection (`buf_alloc`), which bounds what we hold for
/// a host socket that is not reading. Firecracker's CONN_TX_BUF_SIZE.
pub const BUF_ALLOC: u32 = 64 * 1024;
/// A credit update goes out once the guest's view of our free space falls below this.
/// Firecracker's CONN_CREDIT_UPDATE_THRESHOLD.
const CREDIT_UPDATE_THRESHOLD: u32 = 4 * 1024;
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
    credit_update: bool,
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
    /// Guest→host bytes the host socket has not taken yet. At most `BUF_ALLOC`.
    tx_buf: TxBuf,
    expiry: Option<Instant>,
    /// Whether the multiplexer has this connection in its RX queue.
    pub queued: bool,
    /// Whether `local_port` was allocated for a host-initiated connection.
    pub allocated_port: bool,
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
            guest_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            tx_buf: TxBuf::default(),
            expiry: None,
            queued: false,
            allocated_port: false,
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

    pub fn fd(&self) -> i32 {
        self.stream.as_raw_fd()
    }

    pub fn has_pending_rx(&self) -> bool {
        let p = self.pending;
        p.rst || p.response || p.request || p.shutdown != 0 || p.rw || p.credit_update
    }

    pub fn expiry(&self) -> Option<Instant> {
        self.expiry
    }

    /// Ends the connection: an RST goes to the guest and buffered data is dropped.
    pub fn kill(&mut self) {
        self.killed = true;
        self.tx_buf = TxBuf::default();
        self.pending = Pending {
            rst: true,
            ..Pending::default()
        };
        self.expiry = None;
    }

    /// Which host-socket events the connection waits for: (readable, writable).
    pub fn interest(&self) -> (bool, bool) {
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

    /// Whether the guest's view of our free buffer space has fallen below the threshold.
    fn guest_needs_credit(&self) -> bool {
        let seen_free = Wrapping(BUF_ALLOC) - (self.fwd_cnt - self.last_fwd_cnt_to_guest);
        seen_free.0 < CREDIT_UPDATE_THRESHOLD
    }

    fn header(&mut self, op: u16) -> Header {
        // Every packet carries our credit, so the guest's view is now current.
        self.last_fwd_cnt_to_guest = self.fwd_cnt;
        Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: self.local_port,
            dst_port: self.peer_port,
            len: 0,
            kind: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: BUF_ALLOC,
            fwd_cnt: self.fwd_cnt.0,
        }
    }

    /// The next packet for the guest, if one is due. A data packet's payload is read from
    /// the host socket straight into `space`, the guest buffer after the header.
    pub fn next_rx(&mut self, space: &[Span]) -> Option<Header> {
        if self.pending.rst {
            self.pending = Pending::default();
            return Some(self.header(op::RST));
        }
        if self.pending.response {
            self.pending.response = false;
            self.phase = Phase::Established;
            return Some(self.header(op::RESPONSE));
        }
        if self.pending.request {
            self.pending.request = false;
            self.expiry = Some(Instant::now() + REQUEST_TIMEOUT);
            return Some(self.header(op::REQUEST));
        }
        if self.pending.shutdown != 0 {
            let flags = std::mem::take(&mut self.pending.shutdown);
            let mut h = self.header(op::SHUTDOWN);
            h.flags = flags;
            return Some(h);
        }
        if self.pending.rw
            && let Some(h) = self.read_host(space)
        {
            return Some(h);
        }
        if self.pending.credit_update {
            self.pending.credit_update = false;
            // A packet sent since it was queued may already have carried the credit.
            if self.guest_needs_credit() {
                return Some(self.header(op::CREDIT_UPDATE));
            }
        }
        None
    }

    fn read_host(&mut self, space: &[Span]) -> Option<Header> {
        if self.guest_rcv_closed || self.host_eof {
            self.pending.rw = false;
            return None;
        }
        let credit = self.guest_credit();
        if credit == 0 {
            // Reading resumes when the guest's credit update arrives.
            self.pending.rw = false;
            return Some(self.header(op::CREDIT_REQUEST));
        }
        match readv(self.fd(), space, credit as usize) {
            Ok(0) => {
                // A half-close: the host may still read what the guest sends.
                self.pending.rw = false;
                self.host_eof = true;
                let mut h = self.header(op::SHUTDOWN);
                h.flags = shutdown::SEND;
                Some(h)
            }
            Ok(n) => {
                // More may be waiting: keep reading while the guest has buffers.
                self.rx_cnt += n as u32;
                let mut h = self.header(op::RW);
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
                Some(self.header(op::RST))
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
                if h.len == 0 {
                    return;
                }
                if let Err(e) = self.send_host(payload, mem) {
                    debug!("vsock: guest data for port {}: {e}", self.local_port);
                    self.kill();
                } else if self.guest_needs_credit() {
                    self.pending.credit_update = true;
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
                self.pending.credit_update = true;
            }
            _ => debug!(
                "vsock: dropping op {} for port {} in {:?}",
                h.op, self.local_port, self.phase
            ),
        }
    }

    /// Writes guest data to the host socket; what it does not take now is buffered.
    fn send_host(&mut self, payload: &[Span], mem: &GuestMemory) -> io::Result<()> {
        let total: usize = payload.iter().map(|s| s.len).sum();
        if self.tx_buf.len() + total > BUF_ALLOC as usize {
            return Err(io::Error::other("guest exceeded its credit"));
        }
        let mut written = 0;
        if self.tx_buf.is_empty() {
            written = match writev(self.fd(), payload, total) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                Err(e) => return Err(e),
            };
            self.fwd_cnt += written as u32;
        }
        let mut skip = written;
        for s in payload {
            if skip >= s.len {
                skip -= s.len;
                continue;
            }
            self.tx_buf.append(mem, s.gpa + skip as u64, s.len - skip)?;
            skip = 0;
        }
        Ok(())
    }

    /// Moves buffered data to the host, then finishes whatever the guest's shutdown asked
    /// for once nothing is left.
    fn flush(&mut self) -> io::Result<()> {
        let n = self.tx_buf.flush_to(&mut self.stream)?;
        self.fwd_cnt += n as u32;
        if !self.tx_buf.is_empty() {
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

/// Guest→host bytes waiting for the host socket.
#[derive(Debug, Default)]
struct TxBuf {
    data: Vec<u8>,
    start: usize,
}

impl TxBuf {
    fn len(&self) -> usize {
        self.data.len() - self.start
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `len` bytes of guest memory at `gpa`.
    fn append(&mut self, mem: &GuestMemory, gpa: u64, len: usize) -> io::Result<()> {
        if self.start > 0 {
            self.data.drain(..self.start);
            self.start = 0;
        }
        let old = self.data.len();
        self.data.resize(old + len, 0);
        let tail = self.data.get_mut(old..).unwrap_or_default();
        mem.read(gpa, tail)
            .map_err(|e| io::Error::other(format!("guest buffer: {e}")))
    }

    /// Writes as much as the socket takes without blocking; frees the buffer once empty.
    fn flush_to(&mut self, stream: &mut UnixStream) -> io::Result<usize> {
        let mut total = 0;
        while let Some(rest) = self.data.get(self.start..).filter(|r| !r.is_empty()) {
            match stream.write(rest) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.start += n;
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if self.is_empty() {
            *self = TxBuf::default();
        }
        Ok(total)
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
        assert_eq!(c.next_rx(&space).unwrap().op, op::RESPONSE);
        assert!(!c.has_pending_rx());
        (c, host)
    }

    #[test]
    fn guest_data_reaches_the_host_and_credit_flows_back() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 256 * 1024);
        let data: Vec<u8> = (0..60_000u32).map(|i| i as u8).collect();
        g.mem.write(BASE, &data).unwrap();
        let h = Guest::guest_header(op::RW, data.len() as u32, 256 * 1024, 0);
        c.on_guest_packet(&h, &[g.span(BASE, data.len())], &g.mem);
        assert_eq!(pump(&mut c, &mut host, data.len()), data);
        // 60,000 forwarded: the guest sees 5,536 free, above the threshold.
        assert!(!c.has_pending_rx());
        let more = Guest::guest_header(op::RW, 2000, 256 * 1024, 0);
        c.on_guest_packet(&more, &[g.span(BASE, 2000)], &g.mem);
        assert_eq!(pump(&mut c, &mut host, 2000).len(), 2000);
        // 3,536 free as the guest sees it: a credit update is owed.
        let upd = c.next_rx(&[g.span(BASE, 4096)]).unwrap();
        assert_eq!(
            (upd.op, upd.fwd_cnt, upd.buf_alloc),
            (op::CREDIT_UPDATE, 62_000, BUF_ALLOC)
        );
    }

    #[test]
    fn host_data_is_read_into_guest_memory_within_the_guest_credit() {
        let g = Guest::new();
        let (mut c, mut host) = established(&g, 3000);
        host.write_all(&[7u8; 5000]).unwrap();
        c.on_readable();
        let space = [g.span(BASE + 44, 4096 - 44)];
        let h = c.next_rx(&space).unwrap();
        assert_eq!((h.op, h.len), (op::RW, 3000));
        let mut got = vec![0u8; 3000];
        g.mem.read(BASE + 44, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 7));
        // Out of credit: the connection asks rather than reads.
        let h = c.next_rx(&space).unwrap();
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
        let h = c.next_rx(&[g.span(BASE, 4096)]).unwrap();
        assert_eq!((h.op, h.flags), (op::SHUTDOWN, shutdown::SEND));
        // The guest still sends, and the host still receives.
        g.mem.write(BASE, b"reply").unwrap();
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
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)]).unwrap().op, op::RST);
    }

    #[test]
    fn a_guest_that_ignores_credit_is_reset() {
        let g = Guest::new();
        let (mut c, host) = established(&g, 256 * 1024);
        // Fill the host socket so nothing more is taken, then exceed BUF_ALLOC.
        let big = vec![1u8; 1 << 20];
        g.mem.write(BASE, &big[..BUF_ALLOC as usize]).unwrap();
        let mut sent = 0usize;
        for _ in 0..64 {
            let h = Guest::guest_header(op::RW, BUF_ALLOC, 256 * 1024, 0);
            c.on_guest_packet(&h, &[g.span(BASE, BUF_ALLOC as usize)], &g.mem);
            sent += BUF_ALLOC as usize;
            if c.has_pending_rx() && c.killed {
                break;
            }
        }
        assert!(
            c.killed,
            "{sent} bytes past a full socket never tripped the credit check"
        );
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)]).unwrap().op, op::RST);
        drop(host);
    }

    #[test]
    fn host_initiated_connections_answer_ok_or_time_out() {
        let g = Guest::new();
        let (ours, mut host) = UnixStream::pair().unwrap();
        let mut c = Conn::host_initiated(ours, GUEST_CID, 1 << 30, 1234);
        let req = c.next_rx(&[g.span(BASE, 4096)]).unwrap();
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
        c.next_rx(&[g.span(BASE, 4096)]).unwrap();
        assert!(!c.expire(Instant::now()));
        assert!(c.expire(Instant::now() + REQUEST_TIMEOUT));
        assert_eq!(c.next_rx(&[g.span(BASE, 4096)]).unwrap().op, op::RST);
    }
}
