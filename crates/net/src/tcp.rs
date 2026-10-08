//! A guest's TCP connection, translated to a host socket (networking.md R1; as passt
//! translates, written here because passt does not run on Darwin): the guest's segments
//! end here, and their bytes go to the host's socket; the host's bytes go to the guest as
//! segments, kept until the guest acknowledges them, since macOS cannot peek a socket at
//! an offset (Linux's SO_PEEK_OFF, which passt reads unacknowledged data back with).
//!
//! Guest sequence numbers never reach the wire: the host's kernel makes its own
//! (networking.md R1). Sequence numbers compare by their wrapping difference (RFC 9293
//! §3.4).

use std::collections::VecDeque;
use std::io::{self, IoSlice, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::wire::{self, ACK, FIN, PSH, RST, SYN};

/// What a connection holds of the guest's bytes the host has not taken yet, and so the
/// most this side ever advertises: 4 MiB, with a window scale of 7.
const TO_HOST: usize = 4 << 20;
const OUR_WSCALE: u8 = 7;
/// The most host bytes a connection holds unacknowledged by the guest.
const TO_GUEST: usize = 4 << 20;
/// The largest segment this side takes, and sends: the device's MTU less IPv4 and TCP's
/// headers (vmm virtio-net, MTU 65520).
pub const MSS: u16 = 65520 - 40;
/// Retransmission: first after 200 ms, Linux's TCP_RTO_MIN (include/net/tcp.h), then
/// doubling (RFC 6298 §5.5). An RTO of measured round trips (RFC 6298 §2) would sit at that
/// floor: the guest is a ring away, its round trips microseconds. The connection is reset
/// after 8, 102.2 s unanswered in all, past the 100 s RFC 1122 §4.2.3.5 sets for R2
/// (Linux's tcp_retries2 of 15 waits out minutes of a network's outages; a guest a ring
/// away has none, and one that answers nothing that long is gone).
const RTO: Duration = Duration::from_millis(200);
const RETRIES: u32 = 8;

/// `a < b` in sequence space.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    /// The host's connect is under way; the guest's SYN waits for its answer.
    Connecting,
    /// A host's connection to a published port: this side's SYN sent to the guest,
    /// awaiting its SYN-ACK.
    SynSent,
    /// SYN-ACK sent, or the connection open.
    Open,
}

/// A connection's two ends: the guest's (its address and port) and the remote's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub guest_port: u16,
    pub remote: (Ipv4Addr, u16),
}

#[derive(Debug)]
pub struct Conn {
    pub key: Key,
    pub sock: TcpStream,
    state: State,
    /// The next guest byte expected; the guest's window, scaled, and its scale.
    rcv_nxt: u32,
    guest_wnd: u32,
    guest_wscale: u8,
    guest_mss: u16,
    /// Whether the guest offered window scaling (and so gets this side's).
    scaled: bool,
    /// This side's sequence: the oldest byte the guest has not acknowledged, the next to
    /// send, and the next past all ever sent. `snd_nxt` is short of `snd_max` once a window
    /// that shrank had the guest drop what was past its edge, to be sent again.
    snd_una: u32,
    snd_nxt: u32,
    snd_max: u32,
    /// Bytes from `snd_una` sent to the guest and not yet acknowledged, then bytes not yet
    /// sent: what the guest may still need again.
    to_guest: VecDeque<u8>,
    /// Guest bytes for the host not yet written.
    to_host: VecDeque<u8>,
    /// The guest sent FIN; the host's write side is shut once `to_host` drains.
    guest_fin: bool,
    host_shut: bool,
    /// The host's socket ended; this side's FIN is sent once `to_guest` is, numbered past
    /// its last byte, and has been if `fin_sent`.
    host_eof: bool,
    fin_sent: bool,
    /// Whether the guest has acknowledged this side's SYN, which takes a sequence number
    /// and no byte.
    syn_acked: bool,
    /// Retransmission: when the oldest unacknowledged byte was last sent, and how often;
    /// or, while a window closed on bytes waiting, when the guest was last probed, and how
    /// many probes it has not answered.
    sent_at: Option<Instant>,
    retries: u32,
    probes: u32,
    pub closed: bool,
    /// The guest reset the connection: nothing more goes to it, nor is read from the host;
    /// what it sent before, which this side acknowledged, still goes to the host, whose
    /// write side is then shut, as Linux hands a reader its queued bytes before a reset's
    /// error (tcp_recvmsg) and Docker's proxy copies them before it shuts its client's
    /// (cmd/docker-proxy tcp_proxy.go).
    guest_reset: bool,
    /// The ring to the guest had no room for a segment of bytes: the connection sends
    /// again once it has ([`Conn::unblock`]).
    blocked: bool,
    /// Loss repair, as NewReno's (RFC 6582) without congestion control, which a ring has
    /// no need of. The oldest segment unacknowledged is to be sent again; `snd_max` when
    /// segments that may reach the guest twice were last sent (RFC 6582's `recover`); and,
    /// while a loss is repaired, `snd_max` when it was seen, which the repair ends at.
    resend: bool,
    recover: u32,
    repair: Option<u32>,
    /// A connection to the host's resolver for the guest's DNS over TCP: its questions and
    /// answers read, as the guest's by UDP are.
    pub dns: Option<crate::dns::Stream>,
}

/// A segment's bytes, as they lie in a queue that may wrap: one part, then the other.
pub type Payload<'a> = [&'a [u8]; 2];
/// A segment without bytes.
pub const EMPTY: Payload<'static> = [&[], &[]];

/// Bytes `at..at + n` of `parts`, one after another, where they lie; fewer if they end
/// first.
fn span(parts: Payload<'_>, at: usize, n: usize) -> Payload<'_> {
    let [a, b] = parts;
    let end = at.saturating_add(n);
    [
        clip(a, at, end),
        clip(b, at.saturating_sub(a.len()), end.saturating_sub(a.len())),
    ]
}

/// `q`'s bytes `at..at + n`, where they lie in its two halves.
fn bytes(q: &VecDeque<u8>, at: usize, n: usize) -> Payload<'_> {
    span(q.as_slices().into(), at, n)
}

/// `s`'s bytes `from..to`, cut to its length.
fn clip(s: &[u8], from: usize, to: usize) -> &[u8] {
    let to = to.min(s.len());
    s.get(from.min(to)..to).unwrap_or_default()
}

/// Where segments for the guest are written: the caller's frame builder.
pub trait ToGuest {
    /// Sends a segment; false if the ring to the guest is full (nothing was sent).
    #[allow(clippy::too_many_arguments)]
    fn segment(
        &mut self,
        key: &Key,
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        syn: Option<(u16, Option<u8>)>,
        payload: Payload<'_>,
    ) -> bool;
}

impl Conn {
    /// A connection for the guest's SYN: its host socket connecting, without blocking.
    pub fn open(key: Key, seg: &wire::Tcp<'_>, isn: u32) -> io::Result<Conn> {
        Conn::open_to(key, key.remote, seg, isn)
    }

    /// A connection for the guest's SYN to `key`'s remote end, its host socket connecting
    /// to `to` instead.
    pub fn open_to(key: Key, to: (Ipv4Addr, u16), seg: &wire::Tcp<'_>, isn: u32) -> io::Result<Conn> {
        let sock = connect(to)?;
        Ok(Conn {
            key,
            sock,
            state: State::Connecting,
            rcv_nxt: seg.seq.wrapping_add(1),
            guest_wnd: u32::from(seg.window),
            guest_wscale: seg.wscale.unwrap_or(0).min(14),
            guest_mss: seg.mss.unwrap_or(536),
            scaled: seg.wscale.is_some(),
            snd_una: isn,
            snd_nxt: isn,
            snd_max: isn,
            to_guest: VecDeque::new(),
            to_host: VecDeque::new(),
            guest_fin: false,
            host_shut: false,
            host_eof: false,
            fin_sent: false,
            syn_acked: false,
            sent_at: None,
            retries: 0,
            probes: 0,
            closed: false,
            guest_reset: false,
            blocked: false,
            resend: false,
            recover: isn,
            repair: None,
            dns: None,
        })
    }

    /// A connection a host client made to a published port, `sock` accepted: this side
    /// opens it to the guest, as the client's own SYN would (RFC 9293 §3.5), offering
    /// window scaling, which holds if the guest's SYN-ACK takes it.
    pub fn accept(key: Key, sock: TcpStream, isn: u32, out: &mut dyn ToGuest) -> Conn {
        let c = Conn {
            key,
            sock,
            state: State::SynSent,
            rcv_nxt: 0,
            guest_wnd: 0,
            guest_wscale: 0,
            guest_mss: 536,
            scaled: false,
            snd_una: isn,
            snd_nxt: isn.wrapping_add(1),
            snd_max: isn.wrapping_add(1),
            to_guest: VecDeque::new(),
            to_host: VecDeque::new(),
            guest_fin: false,
            host_shut: false,
            host_eof: false,
            fin_sent: false,
            syn_acked: false,
            sent_at: Some(Instant::now()),
            retries: 0,
            probes: 0,
            closed: false,
            guest_reset: false,
            blocked: false,
            resend: false,
            recover: isn,
            repair: None,
            dns: None,
        };
        c.send_syn(out);
        c
    }

    fn send_syn(&self, out: &mut dyn ToGuest) {
        out.segment(
            &self.key,
            self.snd_una,
            0,
            SYN,
            u16::try_from(TO_HOST.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
            Some((MSS, Some(OUR_WSCALE))),
            EMPTY,
        );
    }

    /// The guest's answer to this side's SYN: its SYN-ACK opens the connection, its RST
    /// refuses it, as a closed port refuses (the client's socket is then closed).
    fn on_syn_sent(&mut self, seg: &wire::Tcp<'_>, out: &mut dyn ToGuest) {
        if seg.flags & ACK != 0 && seg.ack != self.snd_nxt {
            if seg.flags & RST == 0 {
                out.segment(&self.key, seg.ack, 0, RST, 0, None, EMPTY);
            }
            return;
        }
        if seg.flags & RST != 0 {
            self.closed = true;
            return;
        }
        if seg.flags & (SYN | ACK) != SYN | ACK {
            return;
        }
        self.rcv_nxt = seg.seq.wrapping_add(1);
        self.scaled = seg.wscale.is_some();
        self.guest_wscale = if self.scaled {
            seg.wscale.unwrap_or(0).min(14)
        } else {
            0
        };
        self.guest_wnd = u32::from(seg.window);
        self.guest_mss = seg.mss.unwrap_or(536);
        self.snd_una = seg.ack;
        self.syn_acked = true;
        self.sent_at = None;
        self.retries = 0;
        self.state = State::Open;
        self.ack(out);
    }

    /// The window this side advertises, scaled if the guest scales.
    fn window(&self) -> u16 {
        let free = TO_HOST.saturating_sub(self.to_host.len());
        let shift = self.our_scale();
        u16::try_from(free >> shift).unwrap_or(u16::MAX)
    }

    /// The shift of the window this side advertises: its own scale, if the guest scales.
    fn our_scale(&self) -> u8 {
        if self.scaled { OUR_WSCALE } else { 0 }
    }

    fn ack(&self, out: &mut dyn ToGuest) {
        out.segment(
            &self.key,
            self.snd_nxt,
            self.rcv_nxt,
            ACK,
            self.window(),
            None,
            EMPTY,
        );
    }

    /// A reset for the guest, ending the connection.
    pub fn reset(&mut self, out: &mut dyn ToGuest) {
        self.lost("a reset", None);
        self.closed = true;
        if self.guest_reset {
            return;
        }
        out.segment(&self.key, self.snd_nxt, self.rcv_nxt, RST | ACK, 0, None, EMPTY);
        self.closed = true;
    }

    /// After the guest's reset: what it sent goes to the host, then the host's write side
    /// is shut and the connection ends; a host that cannot take it is reset.
    fn drain_after_reset(&mut self, out: &mut dyn ToGuest) {
        if !self.to_host.is_empty() {
            let (a, b) = self.to_host.as_slices();
            match write_now(&self.sock, [a, b]) {
                Ok(n) => {
                    self.to_host.drain(..n);
                }
                Err(_) => {
                    self.lost("the host's socket, after the guest's reset,", None);
                    self.closed = true;
                    return;
                }
            }
        }
        if self.to_host.is_empty() {
            let _ = self.sock.shutdown(Shutdown::Write);
            self.host_shut = true;
            self.closed = true;
        }
        let _ = out;
    }

    /// Says on stderr (the daemon's log) that `why` ends this connection with bytes the
    /// guest sent, and was told the host has, not yet written to the host: they are lost.
    fn lost(&self, why: &str, seg: Option<&wire::Tcp<'_>>) {
        if self.to_host.is_empty() {
            return;
        }
        let at = seg.map_or(String::new(), |s| {
            format!(
                ", its seq {} ack {} against rcv_nxt {} snd_una {}",
                s.seq, s.ack, self.rcv_nxt, self.snd_una
            )
        });
        let _ = writeln!(
            io::stderr(),
            "shards-net: {why} ended guest port {}'s connection from {:?} with {} bytes for the host unwritten{at}; guest_fin {} host_eof {} fin_sent {}",
            self.key.guest_port,
            self.key.remote,
            self.to_host.len(),
            self.guest_fin,
            self.host_eof,
            self.fin_sent
        );
    }

    /// Whether this connection waits for its socket to become writable: to finish
    /// connecting, or to take what the guest sent.
    pub fn wants_write(&self) -> bool {
        self.state == State::Connecting || self.state == State::Open && !self.to_host.is_empty()
    }

    /// Whether it can take host bytes for the guest now.
    pub fn wants_read(&self) -> bool {
        !self.guest_reset
            && self.state == State::Open
            && !self.host_eof
            && self.to_guest.len() < TO_GUEST.min(self.guest_wnd as usize + 1)
    }

    /// The host socket may be writable: a connect done, or room for the guest's bytes.
    pub fn on_writable(&mut self, out: &mut dyn ToGuest) {
        if self.state == State::Connecting {
            match self.sock.take_error() {
                Ok(None) => {}
                _ => {
                    // Refused, unreachable, or timed out: the guest hears a reset, as its
                    // connect would from the remote.
                    self.reset(out);
                    return;
                }
            }
            // Connected only once the peer is known.
            if self.sock.peer_addr().is_err() {
                return;
            }
            self.state = State::Open;
            let wscale = self.scaled.then_some(OUR_WSCALE);
            out.segment(
                &self.key,
                self.snd_nxt,
                self.rcv_nxt,
                SYN | ACK,
                u16::try_from(TO_HOST.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
                Some((MSS, wscale)),
                EMPTY,
            );
            self.sent_to(self.snd_nxt.wrapping_add(1));
            self.sent_at = Some(Instant::now());
            return;
        }
        self.flush_to_host(out);
    }

    fn flush_to_host(&mut self, out: &mut dyn ToGuest) {
        if self.guest_reset {
            self.drain_after_reset(out);
            return;
        }
        let before_window = self.window();
        if !self.to_host.is_empty() {
            let (a, b) = self.to_host.as_slices();
            match write_now(&self.sock, [a, b]) {
                Ok(n) => {
                    self.to_host.drain(..n);
                }
                Err(_) => {
                    self.reset(out);
                    return;
                }
            }
        }
        if self.to_host.is_empty() && self.guest_fin && !self.host_shut {
            let _ = self.sock.shutdown(Shutdown::Write);
            self.host_shut = true;
        }
        // A window that opened from shut, or by a half, is announced.
        let now = self.window();
        if before_window == 0 && now > 0 || u32::from(now) > 2 * u32::from(before_window).max(1) {
            self.ack(out);
        }
    }

    /// The host socket may be readable: its bytes to the guest, while the guest's window
    /// takes them.
    pub fn on_readable(&mut self, out: &mut dyn ToGuest, buf: &mut [u8]) {
        while self.wants_read() {
            let room = TO_GUEST
                .min(self.guest_wnd as usize + 1)
                .saturating_sub(self.to_guest.len());
            let want = room.min(buf.len());
            if want == 0 {
                break;
            }
            match self.sock.read(buf.get_mut(..want).unwrap_or_default()) {
                Ok(0) => {
                    self.host_eof = true;
                    break;
                }
                Ok(n) => {
                    let got = buf.get(..n).unwrap_or_default();
                    match self.dns.as_mut() {
                        Some(d) => {
                            self.to_guest.extend(d.from_host(got));
                            // A resolver that stops part way through an answer holds no
                            // more than this side holds for any host.
                            if d.held() > TO_GUEST {
                                self.reset(out);
                                return;
                            }
                        }
                        None => self.to_guest.extend(got),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.reset(out);
                    return;
                }
            }
        }
        self.send_new(out);
    }

    /// The largest segment the guest takes.
    fn mss(&self) -> usize {
        usize::from(self.guest_mss.min(MSS)).max(1)
    }

    /// Bytes of `to_guest` in flight: sent, from `snd_una` to `snd_nxt`, and not yet
    /// acknowledged. The FIN, if among them, is numbered past the last byte.
    fn in_flight(&self) -> usize {
        (self.snd_nxt.wrapping_sub(self.snd_una) as usize).min(self.to_guest.len())
    }

    /// Whether the guest has acknowledged this side's FIN: the last sequence number ever
    /// sent.
    fn fin_acked(&self) -> bool {
        self.fin_sent && self.snd_una == self.snd_max
    }

    /// Records what was sent up to `seq`.
    fn sent_to(&mut self, seq: u32) {
        self.snd_nxt = seq;
        if before(self.snd_max, seq) {
            self.snd_max = seq;
        }
    }

    /// Sends the segment repair asks for, then what the guest has not had yet of
    /// `to_guest`, within its window, then FIN once all is sent and the host is done. Each
    /// segment's bytes go from the queue to the ring, copied once.
    fn send_new(&mut self, out: &mut dyn ToGuest) {
        if !self.syn_acked || self.guest_reset {
            return;
        }
        self.blocked = false;
        if self.resend && !self.retransmit(out) {
            self.blocked = true;
            return;
        }
        let mss = self.mss();
        loop {
            let sent = self.in_flight();
            let unsent = self.to_guest.len().saturating_sub(sent);
            let window = (self.guest_wnd as usize).saturating_sub(sent);
            let payload = bytes(&self.to_guest, sent, unsent.min(window).min(mss));
            let n = payload[0].len() + payload[1].len();
            if n == 0 {
                break;
            }
            let flags = ACK | if n == unsent { PSH } else { 0 };
            if !out.segment(
                &self.key,
                self.snd_nxt,
                self.rcv_nxt,
                flags,
                self.window(),
                None,
                payload,
            ) {
                self.blocked = true;
                break;
            }
            self.sent_to(self.snd_nxt.wrapping_add(n as u32));
            self.sent_at.get_or_insert_with(Instant::now);
        }
        // The FIN follows the last byte, sent once all of them are (and again if a window
        // that shrank had the guest drop it).
        let all_sent = self.snd_nxt.wrapping_sub(self.snd_una) as usize == self.to_guest.len();
        if self.host_eof
            && all_sent
            && !self.fin_acked()
            && out.segment(
                &self.key,
                self.snd_nxt,
                self.rcv_nxt,
                FIN | ACK,
                self.window(),
                None,
                EMPTY,
            )
        {
            self.sent_to(self.snd_nxt.wrapping_add(1));
            self.fin_sent = true;
            self.sent_at.get_or_insert_with(Instant::now);
        }
        // Bytes waiting on a window closed, none in flight: the guest is probed, lest a
        // window it opens without saying so leave them waiting for ever ([`Conn::on_timer`]).
        if self.closed_on_bytes() {
            self.sent_at.get_or_insert_with(Instant::now);
        }
    }

    /// Whether bytes wait on a window the guest closed, with none in flight.
    fn closed_on_bytes(&self) -> bool {
        self.syn_acked && self.snd_nxt == self.snd_una && self.guest_wnd == 0 && !self.to_guest.is_empty()
    }

    /// Sends again the oldest of what the guest has not acknowledged, alone (RFC 6298
    /// §5.4): a segment of bytes from `snd_una`, or the FIN if it is all that is left.
    /// Nothing past it is sent again: the guest's acknowledgement of it says what else it
    /// lacks. False if the ring had no room; it is asked again once it has.
    fn retransmit(&mut self, out: &mut dyn ToGuest) -> bool {
        let in_flight = self.in_flight();
        let payload = bytes(&self.to_guest, 0, in_flight.min(self.mss()));
        let n = payload[0].len() + payload[1].len();
        let (flags, payload) = if n > 0 {
            (ACK | if n == in_flight { PSH } else { 0 }, payload)
        } else if self.snd_nxt != self.snd_una {
            // No bytes in flight, yet a sequence number is: the FIN's.
            (FIN | ACK, EMPTY)
        } else {
            self.resend = false;
            return true;
        };
        self.resend = !out.segment(
            &self.key,
            self.snd_una,
            self.rcv_nxt,
            flags,
            self.window(),
            None,
            payload,
        );
        !self.resend
    }

    /// An acknowledgement of new sequence numbers, up to `ack`.
    fn on_acked(&mut self, ack: u32) {
        // The SYN takes a sequence number and no byte of `to_guest`, as does the FIN past
        // its last byte.
        let mut bytes = ack.wrapping_sub(self.snd_una) as usize;
        if !self.syn_acked {
            self.syn_acked = true;
            bytes -= 1;
        }
        self.to_guest.drain(..bytes.min(self.to_guest.len()));
        self.snd_una = ack;
        // What a window that shrank had the guest drop came after all.
        if before(self.snd_nxt, ack) {
            self.snd_nxt = ack;
        }
        self.retries = 0;
        self.sent_at = (self.snd_una != self.snd_nxt).then(Instant::now);
        // Short of the repair's end, its acknowledgement stops at the next segment lost
        // (RFC 6582 §3.2's partial acknowledgement): ring and guest take segments in
        // order, so all sent before the one sent again had reached the guest before it,
        // and what the guest lacks of them it lost.
        if let Some(end) = self.repair {
            if before(ack, end) {
                self.resend = true;
            } else {
                self.repair = None;
            }
        }
    }

    /// A duplicate acknowledgement (RFC 5681 §2): the guest has a segment past one it
    /// lacks, and ring and guest take segments in order, so the one it lacks was lost: it
    /// goes again at once. RFC 5681 §3.2 waits for three duplicates, and RFC 5827 for one
    /// fewer than the segments in flight, lest a segment only late be sent again; nothing
    /// here comes late (the guest's one receive queue keeps the ring's order), and a guest
    /// short of memory can leave a single duplicate, saying nothing of what it drops past a
    /// hole (tcp_data_queue_ofo, LINUX_MIB_TCPOFODROP; PM M104).
    ///
    /// Not if it acknowledges no more than `recover` (RFC 6582 §3.2 step 2): a segment sent
    /// again at a timeout, or after a window shrank, may reach the guest twice, and comes
    /// after all sent before it, so the guest's duplicate for it acknowledges no more than
    /// they. A repair's segments fill what the guest lacks, and reach it once.
    fn on_duplicate(&mut self, ack: u32) {
        if self.repair.is_none() && before(self.recover, ack) {
            self.repair = Some(self.snd_max);
            self.resend = true;
            self.sent_at = Some(Instant::now());
        }
    }

    /// The guest's window as it now is. One that shrank below what is in flight has the
    /// guest drop what is past its edge, as Linux does when it drops a segment for want of
    /// memory and closes its window (tcp_select_window, ICSK_ACK_NOMEM; its
    /// LINUX_MIB_BEYOND_WINDOW counts what it then drops): sent again as the window opens,
    /// as RFC 9293 §3.8.6.2.1 has a sender robust to a window that shrinks.
    fn on_window(&mut self) {
        let edge = self.snd_una.wrapping_add(self.guest_wnd);
        if self.syn_acked && before(edge, self.snd_nxt) {
            self.snd_nxt = edge;
            self.recover = self.snd_max;
            self.repair = None;
            if self.snd_nxt == self.snd_una {
                self.sent_at = None;
            }
        }
    }

    /// A segment from the guest for this connection.
    pub fn on_segment(&mut self, seg: &wire::Tcp<'_>, out: &mut dyn ToGuest) {
        if self.state == State::SynSent {
            self.on_syn_sent(seg, out);
            return;
        }
        if self.guest_reset {
            return;
        }
        if seg.flags & RST != 0 {
            // RFC 5961 §3.2: a reset numbered exactly where this side is ends the
            // connection; one elsewhere in the window is answered with where this side is,
            // which a peer that meant it answers with an exact one; any other is dropped.
            if seg.seq != self.rcv_nxt {
                if seg.seq.wrapping_sub(self.rcv_nxt) < u32::from(self.window()) << self.our_scale() {
                    self.ack(out);
                }
                return;
            }
            self.guest_reset = true;
            self.to_guest.clear();
            self.sent_at = None;
            self.flush_to_host(out);
            return;
        }
        if seg.flags & SYN != 0 {
            // The guest's SYN again: its SYN-ACK was lost or is still to come.
            if self.state == State::Open && seg.seq.wrapping_add(1) == self.rcv_nxt {
                let wscale = self.scaled.then_some(OUR_WSCALE);
                out.segment(
                    &self.key,
                    self.snd_una,
                    self.rcv_nxt,
                    SYN | ACK,
                    self.window(),
                    Some((MSS, wscale)),
                    EMPTY,
                );
            }
            return;
        }
        if self.state != State::Open {
            return;
        }
        if seg.flags & ACK != 0 {
            let acked = seg.ack.wrapping_sub(self.snd_una);
            let window = u32::from(seg.window) << self.guest_wscale;
            // An acknowledgement of what was never sent, or from before what the guest has
            // acknowledged, says nothing of what it has, nor of its window (RFC 9293
            // §3.10.7.4).
            if acked <= self.snd_max.wrapping_sub(self.snd_una) {
                self.probes = 0;
                if acked > 0 {
                    self.on_acked(seg.ack);
                } else if self.snd_nxt != self.snd_una
                    && self.syn_acked
                    && seg.payload.is_empty()
                    && seg.flags & FIN == 0
                    && window == self.guest_wnd
                {
                    self.on_duplicate(seg.ack);
                }
                self.guest_wnd = window;
                self.on_window();
            }
        }
        // The guest's bytes: what is new of them, from rcv_nxt on.
        let mut data = seg.payload;
        let mut seq = seg.seq;
        if before(seq, self.rcv_nxt) {
            let old = self.rcv_nxt.wrapping_sub(seq) as usize;
            data = data.get(old..).unwrap_or_default();
            seq = self.rcv_nxt;
        }
        let mut answer = false;
        if seq == self.rcv_nxt && !data.is_empty() && !self.guest_fin {
            let room = TO_HOST.saturating_sub(self.to_host.len());
            let taken = data.get(..data.len().min(room)).unwrap_or_default();
            if let Some(d) = self.dns.as_mut() {
                // Questions go whole, if granted; refusals where no answer is part way.
                let granted = d.from_guest(taken);
                self.to_host.extend(granted);
                self.to_guest.extend(d.refusals());
                if d.held() > TO_GUEST {
                    self.reset(out);
                    return;
                }
            }
            // Straight from the frame to the host's socket while nothing waits before them;
            // what it has no room for now waits in `to_host`.
            let written = if self.dns.is_some() {
                0
            } else if self.to_host.is_empty() {
                match write_now(&self.sock, [taken, &[]]) {
                    Ok(n) => n,
                    Err(_) => {
                        self.reset(out);
                        return;
                    }
                }
            } else {
                0
            };
            if self.dns.is_none() {
                self.to_host.extend(taken.get(written..).unwrap_or_default());
            }
            self.rcv_nxt = self.rcv_nxt.wrapping_add(taken.len() as u32);
            answer = true;
        } else if !seg.payload.is_empty() {
            // Old or out of order: say where this side is.
            answer = true;
        }
        // A segment from before what this side has had is not acceptable, and is answered
        // with where this side is (RFC 9293 §3.10.7.4): a keepalive or window probe,
        // which carries nothing and is numbered one before, among them.
        if before(seg.seq, self.rcv_nxt) {
            answer = true;
        }
        if seg.flags & FIN != 0 && seq.wrapping_add(data.len() as u32) == self.rcv_nxt && !self.guest_fin {
            self.guest_fin = true;
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            answer = true;
        }
        self.flush_to_host(out);
        if answer && !self.closed {
            self.ack(out);
        }
        self.send_new(out);
        if self.fin_acked() && self.guest_fin && self.host_shut {
            self.closed = true;
        }
    }

    /// Whether a full ring kept its bytes from the guest.
    pub fn blocked(&self) -> bool {
        self.blocked
    }

    /// Sends what a full ring kept from the guest, a segment to send again first, now that
    /// it may have room.
    pub fn unblock(&mut self, out: &mut dyn ToGuest) {
        if self.blocked {
            self.send_new(out);
        }
    }

    /// When the next retransmission is due, if anything is unacknowledged.
    pub fn deadline(&self) -> Option<Instant> {
        if self.guest_reset {
            return None;
        }
        self.sent_at.map(|t| t + RTO * 2u32.saturating_pow(self.retries))
    }

    /// Sends the oldest segment unacknowledged again if its time has come, alone (RFC
    /// 6298 §5.4); resets the connection after [`RETRIES`]. While a window is closed on
    /// bytes waiting, probes it instead.
    pub fn on_timer(&mut self, now: Instant, out: &mut dyn ToGuest) {
        let Some(due) = self.deadline() else { return };
        if now < due {
            return;
        }
        if self.state == State::Open && self.snd_nxt == self.snd_una {
            self.probe(now, out);
            return;
        }
        if self.retries >= RETRIES {
            self.reset(out);
            return;
        }
        self.retries += 1;
        self.sent_at = Some(now);
        // A timeout ends a repair, and what was sent before it is `recover` (RFC 6582
        // §3.2): an acknowledgement after one may be of a segment the guest had all along,
        // late rather than lost, and says nothing of what it lacks.
        self.recover = self.snd_max;
        self.repair = None;
        if self.state == State::SynSent {
            self.send_syn(out);
            return;
        }
        if !self.syn_acked {
            let wscale = self.scaled.then_some(OUR_WSCALE);
            out.segment(
                &self.key,
                self.snd_una,
                self.rcv_nxt,
                SYN | ACK,
                self.window(),
                Some((MSS, wscale)),
                EMPTY,
            );
            return;
        }
        self.resend = true;
        self.send_new(out);
    }

    /// A probe of a window closed on bytes waiting (RFC 9293 §3.8.6.1): a segment numbered
    /// one before what the guest has acknowledged, which it answers with its window, as
    /// Linux's are (tcp_xmit_probe_skb). The wait doubles from one to the next, up to
    /// [`RETRIES`] doublings; the connection is reset only after [`RETRIES`] go unanswered,
    /// never while the guest answers them (RFC 1122 §4.2.2.17).
    fn probe(&mut self, now: Instant, out: &mut dyn ToGuest) {
        if !self.closed_on_bytes() {
            self.sent_at = None;
            return;
        }
        if self.probes >= RETRIES {
            self.reset(out);
            return;
        }
        self.probes += 1;
        self.retries = (self.retries + 1).min(RETRIES);
        self.sent_at = Some(now);
        out.segment(
            &self.key,
            self.snd_una.wrapping_sub(1),
            self.rcv_nxt,
            ACK,
            self.window(),
            None,
            EMPTY,
        );
    }

    pub fn fd(&self) -> i32 {
        self.sock.as_raw_fd()
    }
}

/// Writes what `sock` takes now of `parts`, one after another, in as few writes as it
/// takes them in: how much it took, or the error that ended it.
fn write_now(mut sock: impl Write, parts: Payload<'_>) -> io::Result<usize> {
    let mut done = 0;
    while done < parts[0].len() + parts[1].len() {
        let [a, b] = span(parts, done, usize::MAX);
        match sock.write_vectored(&[IoSlice::new(a), IoSlice::new(b)]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// A TCP socket connecting to `to` without blocking.
fn connect(to: (Ipv4Addr, u16)) -> io::Result<TcpStream> {
    let addr = SocketAddr::from(to);
    // SAFETY: socket(2) with constant arguments; the descriptor is owned below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made.
    let sock = unsafe { <TcpStream as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    sock.set_nonblocking(true)?;
    // No Nagle on the host's leg: the guest's own TCP chose what to send together.
    sock.set_nodelay(true)?;
    // SAFETY: fcntl(2) on our own descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    let sa = sockaddr(addr);
    // SAFETY: a sockaddr_in of its own length.
    let r = unsafe {
        libc::connect(
            fd,
            (&raw const sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(sock)
}

fn sockaddr(addr: SocketAddr) -> libc::sockaddr_in {
    // SAFETY: an all-zero sockaddr_in is valid.
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(target_os = "macos")]
    {
        sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    if let SocketAddr::V4(v4) = addr {
        sa.sin_port = v4.port().to_be();
        sa.sin_addr = libc::in_addr {
            s_addr: u32::from_ne_bytes(v4.ip().octets()),
        };
    }
    sa
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// What a connection sent the guest: each segment's sequence, acknowledgement, flags
    /// and bytes. A full ring refuses segments of bytes, as the stack's does.
    #[derive(Default)]
    struct Sent {
        segments: Vec<(u32, u32, u8, Vec<u8>)>,
        full: bool,
    }

    impl ToGuest for Sent {
        fn segment(
            &mut self,
            _: &Key,
            seq: u32,
            ack: u32,
            flags: u8,
            _: u16,
            _: Option<(u16, Option<u8>)>,
            payload: Payload<'_>,
        ) -> bool {
            if self.full && payload != EMPTY {
                return false;
            }
            self.segments.push((seq, ack, flags, payload.concat()));
            true
        }
    }

    impl Sent {
        /// What was sent since last asked.
        fn take(&mut self) -> Vec<(u32, u32, u8, Vec<u8>)> {
            std::mem::take(&mut self.segments)
        }
    }

    fn segment(seq: u32, ack: u32, flags: u8) -> wire::Tcp<'static> {
        wire::Tcp {
            src_port: 80,
            dst_port: 40_000,
            seq,
            ack,
            flags,
            window: 65_535,
            mss: None,
            wscale: None,
            payload: &[],
        }
    }

    /// A connection a host client made, opened to the guest: this side's SYN numbered
    /// 1000, the guest's 5000. The client's end, held while the connection lives.
    fn opened(sent: &mut Sent) -> (Conn, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (sock, _) = listener.accept().unwrap();
        sock.set_nonblocking(true).unwrap();
        let key = Key {
            guest_port: 80,
            remote: (Ipv4Addr::new(172, 17, 0, 1), 40_000),
        };
        let mut c = Conn::accept(key, sock, 1000, sent);
        c.on_segment(&segment(5000, 1001, SYN | ACK), sent);
        (c, client)
    }

    /// The guest's reset counts only where RFC 5961 §3.2 says: exactly where this side
    /// is; one elsewhere in the window is challenged, one past it dropped. What the guest
    /// sent before it, and this side acknowledged, reaches the host whole before the
    /// connection ends, and nothing more goes to the guest (published ports lost the tail
    /// of an echo the guest's server had sent, the guest's kernel having reset it).
    #[test]
    fn a_guest_reset_delivers_what_was_acknowledged_first() {
        let mut sent = Sent::default();
        let (mut c, mut client) = opened(&mut sent);
        // More than the client's socket takes unread: the rest waits in `to_host`.
        let big: Vec<u8> = (0..TO_HOST as u32).map(|i| (i % 251) as u8).collect();
        let mut seq = 5001u32;
        for chunk in big.chunks(60_000) {
            let seg = wire::Tcp {
                payload: chunk,
                ..segment(seq, 1001, ACK)
            };
            c.on_segment(&seg, &mut sent);
            seq = seq.wrapping_add(chunk.len() as u32);
        }
        assert!(!c.to_host.is_empty());
        sent.take();
        c.on_segment(&segment(seq.wrapping_add(10), 0, RST), &mut sent);
        assert_eq!(sent.take(), [(1001, seq, ACK, vec![])], "a challenge");
        c.on_segment(&segment(seq.wrapping_add(1 << 30), 0, RST), &mut sent);
        assert_eq!(sent.take(), []);
        assert!(!c.closed);
        c.on_segment(&segment(seq, 0, RST), &mut sent);
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            client.read_to_end(&mut got).unwrap();
            got
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !c.closed && Instant::now() < deadline {
            c.on_writable(&mut sent);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(c.closed);
        let quiet = !c.wants_read() && c.deadline().is_none();
        // Closed, it goes with its socket, as the network process lets it go (`settle`).
        drop(c);
        let got = reader.join().unwrap();
        assert!(got == big, "the host had {} bytes of {}", got.len(), big.len());
        assert!(
            quiet,
            "nothing read from the host nor timed after the guest's reset"
        );
        assert_eq!(sent.take(), [], "nothing went to the guest after its reset");
    }

    /// A keepalive or window probe, which carries nothing and is numbered one before
    /// what this side has had, is answered with where this side is (RFC 9293
    /// §3.10.7.4); an in-order acknowledgement is not.
    #[test]
    fn probes_are_answered_and_acknowledgements_are_not() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        sent.take();
        c.on_segment(&segment(5000, 1001, ACK), &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK, vec![])]);
        c.on_segment(&segment(5001, 1001, ACK), &mut sent);
        assert_eq!(sent.take(), []);
    }

    /// The guest's bytes go straight to the host's socket while nothing waits before them,
    /// and both halves of a queue that wraps go in one write, in order.
    #[test]
    fn guest_bytes_reach_the_host_in_order() {
        let mut sent = Sent::default();
        let (mut c, mut client) = opened(&mut sent);
        let straight = wire::Tcp {
            payload: b"straight",
            ..segment(5001, 1001, ACK)
        };
        c.on_segment(&straight, &mut sent);
        // Taken at once, they never touched the queue, nor made it take memory.
        assert_eq!(c.to_host.capacity(), 0);
        let mut q = VecDeque::with_capacity(7);
        for &b in b"abc".iter().rev() {
            q.push_front(b);
        }
        q.extend(b"defg");
        assert_eq!(q.as_slices(), (&b"abc"[..], &b"defg"[..]));
        c.to_host = q;
        c.on_writable(&mut sent);
        assert!(c.to_host.is_empty());
        let mut got = [0u8; 15];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"straightabcdefg");
    }

    /// A socket that takes a few bytes a write is written the rest of `parts`, in order,
    /// until it takes no more.
    #[test]
    fn short_writes_go_on_from_where_they_stopped() {
        struct Trickle(Vec<u8>);
        impl Write for Trickle {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.0.len() >= 8 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = b.len().min(3);
                self.0.extend_from_slice(&b[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sock = Trickle(Vec::new());
        // abc, then de (a write takes one slice at a time by default), then fgh.
        assert_eq!(write_now(&mut sock, [b"abcde", b"fghij"]).unwrap(), 8);
        assert_eq!(sock.0, b"abcdefgh");
    }

    /// What the host's socket has no room for waits in `to_host`, and follows in order as
    /// the socket makes room.
    #[test]
    fn bytes_the_host_has_no_room_for_wait_their_turn() {
        let mut sent = Sent::default();
        let (mut c, mut client) = opened(&mut sent);
        let chunk: Vec<u8> = (0..65_000u32).map(|i| (i % 249) as u8).collect();
        let (mut seq, mut total) = (5001u32, 0usize);
        while c.to_host.is_empty() && total < TO_HOST {
            let bytes = wire::Tcp {
                payload: &chunk,
                ..segment(seq, 1001, ACK)
            };
            c.on_segment(&bytes, &mut sent);
            seq = seq.wrapping_add(chunk.len() as u32);
            total += chunk.len();
        }
        assert!(
            !c.to_host.is_empty(),
            "the host's socket took all {total} bytes at once"
        );
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (mut got, mut buf) = (Vec::with_capacity(total), vec![0u8; 1 << 16]);
        while got.len() < total {
            let n = client.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
            c.on_writable(&mut sent);
        }
        assert!(c.to_host.is_empty());
        assert!(got.chunks(chunk.len()).all(|piece| piece == chunk));
    }

    /// 4000 host bytes for the guest, sent: the guest takes 536-byte segments (no MSS
    /// offered), so 7 of them and one of 248 bytes, from 1001.
    fn sending(sent: &mut Sent) -> (Conn, TcpStream, Vec<u8>) {
        let (mut c, client) = opened(sent);
        sent.take();
        let data: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        c.to_guest.extend(&data);
        c.send_new(sent);
        let segments = sent.take();
        assert_eq!(segments.len(), 8);
        assert_eq!(segments.last().unwrap().2, ACK | PSH);
        let bytes: Vec<u8> = segments.iter().flat_map(|s| s.3.clone()).collect();
        assert_eq!(bytes, data);
        (c, client, data)
    }

    /// Segments are the queue's bytes where it wraps, each numbered by where it starts,
    /// copied from the queue's two halves as they lie.
    #[test]
    fn segments_are_the_queues_bytes_across_its_wrap() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        let data: Vec<u8> = (0..4000u32).map(|i| (i % 253) as u8).collect();
        // Pushed in front of an empty queue, the first 1000 bytes lie at its buffer's end
        // and the rest at its start.
        let mut q = VecDeque::with_capacity(data.len());
        for &b in data[..1000].iter().rev() {
            q.push_front(b);
        }
        q.extend(&data[1000..]);
        let (front, back) = q.as_slices();
        assert_eq!((front.len(), back.len()), (1000, 3000));
        c.to_guest = q;
        sent.take();
        c.send_new(&mut sent);
        let mut at = 0;
        for (seq, _, _, bytes) in sent.take() {
            assert_eq!(seq, 1001 + at as u32);
            assert_eq!(bytes, data[at..at + bytes.len()], "the segment at {at}");
            at += bytes.len();
        }
        assert_eq!(at, data.len());
    }

    /// A timeout sends the oldest segment unacknowledged again, alone (RFC 6298 §5.4), and
    /// doubles the wait for the next; nothing past it is sent again, and the guest's
    /// acknowledgement of all ends it.
    #[test]
    fn a_timeout_sends_the_oldest_segment_alone() {
        let mut sent = Sent::default();
        let (mut c, _client, data) = sending(&mut sent);
        let due = c.deadline().unwrap();
        c.on_timer(due - Duration::from_millis(1), &mut sent);
        assert_eq!(sent.take(), []);
        c.on_timer(due, &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK, data[..536].to_vec())]);
        assert_eq!(c.deadline(), Some(due + 2 * RTO));
        c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        assert_eq!(sent.take(), []);
        assert_eq!(c.deadline(), None);
    }

    /// A duplicate acknowledgement sends the segment the guest lacks again, at once; each
    /// partial acknowledgement after it, the next one it lacks (RFC 6582 §3.2); and all
    /// acknowledged ends the repair.
    #[test]
    fn duplicates_and_partial_acknowledgements_repair_what_was_lost() {
        let mut sent = Sent::default();
        let (mut c, _client, data) = sending(&mut sent);
        // The first and the fourth lost: each of the six that came says where the guest is.
        let dup = segment(5001, 1001, ACK);
        c.on_segment(&dup, &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK, data[..536].to_vec())]);
        for _ in 0..5 {
            c.on_segment(&dup, &mut sent);
        }
        assert_eq!(sent.take(), []);
        // The first filled, the guest acknowledges up to the fourth: sent again at once.
        c.on_segment(&segment(5001, 1001 + 3 * 536, ACK), &mut sent);
        assert_eq!(
            sent.take(),
            [(1001 + 3 * 536, 5001, ACK, data[3 * 536..4 * 536].to_vec())]
        );
        c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        assert_eq!(sent.take(), []);
        assert_eq!(c.deadline(), None);
        // A later loss is repaired as the first was.
        c.to_guest.extend(&data[..1000]);
        c.send_new(&mut sent);
        assert_eq!(sent.take().len(), 2);
        c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        assert_eq!(sent.take(), [(5001, 5001, ACK, data[..536].to_vec())]);
    }

    /// An acknowledgement that changes the window, carries bytes or a FIN says something
    /// new, and is no duplicate (RFC 5681 §2): none of them repairs anything.
    #[test]
    fn what_says_something_new_is_no_duplicate() {
        let mut sent = Sent::default();
        let (mut c, _client, _) = sending(&mut sent);
        for i in 0..4u16 {
            let window = wire::Tcp {
                window: 65_534 + i % 2,
                ..segment(5001, 1001, ACK)
            };
            c.on_segment(&window, &mut sent);
        }
        for i in 0..4u32 {
            let bytes = wire::Tcp {
                payload: b"x",
                ..segment(5001 + i, 1001, ACK)
            };
            c.on_segment(&bytes, &mut sent);
        }
        let fin = segment(5005, 1001, ACK | FIN);
        c.on_segment(&fin, &mut sent);
        assert!(sent.take().iter().all(|s| s.3.is_empty()));
    }

    /// Duplicates that acknowledge no more than what was sent before a timeout are of
    /// segments the timeout sent again, which the guest had: they repair nothing (RFC 6582
    /// §3.2 step 2).
    #[test]
    fn a_timeouts_duplicates_repair_nothing() {
        let mut sent = Sent::default();
        let (mut c, _client, data) = sending(&mut sent);
        c.on_timer(c.deadline().unwrap(), &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK, data[..536].to_vec())]);
        // Late, not lost: the guest had it all, and says so again for each copy.
        c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        c.to_guest.extend(&data[..2000]);
        c.send_new(&mut sent);
        assert_eq!(sent.take().len(), 4);
        for _ in 0..3 {
            c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        }
        assert_eq!(sent.take(), []);
    }

    /// A window that shrinks below what is in flight has the guest drop what is past its
    /// edge, as Linux's does when it closes its window for want of memory: nothing is
    /// waited on meanwhile, and all past the edge goes again, in order, as it opens.
    #[test]
    fn a_window_that_shrinks_has_what_was_past_it_sent_again() {
        let mut sent = Sent::default();
        let (mut c, _client, data) = sending(&mut sent);
        // The second segment dropped, the window closed at it.
        let closed = wire::Tcp {
            window: 0,
            ..segment(5001, 1001 + 536, ACK)
        };
        c.on_segment(&closed, &mut sent);
        // Nothing in flight, nothing awaited but the window: the guest is probed for it.
        let probe = c.deadline().unwrap();
        // What it drops past its edge, it says so of: nothing for it.
        for _ in 0..4 {
            c.on_segment(&closed, &mut sent);
        }
        assert_eq!(sent.take(), []);
        assert_eq!(c.deadline(), Some(probe));
        c.on_segment(&segment(5001, 1001 + 536, ACK), &mut sent);
        let again = sent.take();
        assert_eq!(again.first().map(|s| s.0), Some(1001 + 536));
        let bytes: Vec<u8> = again.iter().flat_map(|s| s.3.clone()).collect();
        assert_eq!(bytes, data[536..]);
        // Segments past the edge still on their way when it shrank reach a guest whose
        // window has opened, before those sent again: what it says of them is no loss.
        c.on_segment(&segment(5001, 1001 + 536, ACK), &mut sent);
        assert_eq!(sent.take(), []);
        // Had the guest had them after all, its acknowledgement of them all is taken.
        c.on_segment(&segment(5001, 5001, ACK), &mut sent);
        assert_eq!(c.deadline(), None);
        assert!(c.to_guest.is_empty());
    }

    /// A window closed on bytes waiting is probed with a segment numbered one before what
    /// the guest has acknowledged, at waits that double; probes the guest answers keep the
    /// connection, however long the window stays shut, and its opening sends the bytes.
    #[test]
    fn a_window_closed_on_bytes_is_probed_until_it_opens() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        let shut = wire::Tcp {
            window: 0,
            ..segment(5001, 1001, ACK)
        };
        c.on_segment(&shut, &mut sent);
        c.to_guest.extend(b"waiting");
        c.send_new(&mut sent);
        sent.take();
        let mut due = c.deadline().unwrap();
        for i in 1..=2 * RETRIES {
            c.on_timer(due, &mut sent);
            assert_eq!(sent.take(), [(1000, 5001, ACK, vec![])]);
            let next = c.deadline().unwrap();
            assert_eq!(next - due, RTO * 2u32.pow(i.min(RETRIES)));
            due = next;
            c.on_segment(&shut, &mut sent);
            assert!(!c.closed);
        }
        c.on_segment(&segment(5001, 1001, ACK), &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK | PSH, b"waiting".to_vec())]);
    }

    /// Probes the guest never answers end the connection after [`RETRIES`].
    #[test]
    fn unanswered_probes_end_the_connection() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        let shut = wire::Tcp {
            window: 0,
            ..segment(5001, 1001, ACK)
        };
        c.on_segment(&shut, &mut sent);
        c.to_guest.extend(b"waiting");
        c.send_new(&mut sent);
        for _ in 0..RETRIES {
            c.on_timer(c.deadline().unwrap(), &mut sent);
            assert!(!c.closed);
        }
        sent.take();
        c.on_timer(c.deadline().unwrap(), &mut sent);
        assert!(c.closed);
        assert_eq!(sent.take(), [(1001, 5001, RST | ACK, vec![])]);
    }

    /// The FIN past the last byte goes again, after them, once a window that shrank had the
    /// guest drop it; the connection's end waits for its acknowledgement.
    #[test]
    fn a_fin_past_a_shrunk_window_goes_again_after_the_bytes() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        sent.take();
        c.to_guest.extend(b"last words");
        c.host_eof = true;
        c.send_new(&mut sent);
        assert_eq!(
            sent.take(),
            [
                (1001, 5001, ACK | PSH, b"last words".to_vec()),
                (1011, 5001, FIN | ACK, vec![])
            ]
        );
        c.on_segment(
            &wire::Tcp {
                window: 0,
                ..segment(5001, 1001, ACK)
            },
            &mut sent,
        );
        c.on_segment(&segment(5001, 1001, ACK), &mut sent);
        assert_eq!(
            sent.take(),
            [
                (1001, 5001, ACK | PSH, b"last words".to_vec()),
                (1011, 5001, FIN | ACK, vec![])
            ]
        );
        c.on_segment(&segment(5001, 1012, ACK | FIN), &mut sent);
        c.flush_to_host(&mut sent);
        assert!(c.closed);
    }

    /// A segment lost from a window of two has one duplicate say so (RFC 5827's early
    /// retransmit): it goes again at once, not at a timeout.
    #[test]
    fn a_loss_in_a_small_window_is_repaired_at_once() {
        let mut sent = Sent::default();
        let (mut c, _client) = opened(&mut sent);
        sent.take();
        let small = |seq, ack| wire::Tcp {
            window: 2 * 536,
            ..segment(seq, ack, ACK)
        };
        c.on_segment(&small(5001, 1001), &mut sent);
        let data: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        c.to_guest.extend(&data);
        c.send_new(&mut sent);
        assert_eq!(sent.take().len(), 2);
        c.on_segment(&small(5001, 1001), &mut sent);
        assert_eq!(sent.take(), [(1001, 5001, ACK, data[..536].to_vec())]);
    }

    /// A segment to send again that a full ring has no room for waits for room, and goes
    /// before anything new.
    #[test]
    fn a_full_ring_keeps_a_segment_to_send_again() {
        let mut sent = Sent::default();
        let (mut c, _client, data) = sending(&mut sent);
        // 100 bytes more for the guest, which the full ring refuses too.
        c.to_guest.extend(&data[..100]);
        sent.full = true;
        let dup = segment(5001, 1001, ACK);
        for _ in 0..3 {
            c.on_segment(&dup, &mut sent);
        }
        assert!(c.blocked());
        assert_eq!(sent.take(), []);
        sent.full = false;
        c.unblock(&mut sent);
        assert_eq!(
            sent.take(),
            [
                (1001, 5001, ACK, data[..536].to_vec()),
                (5001, 5001, ACK | PSH, data[..100].to_vec()),
            ]
        );
        assert!(!c.blocked());
    }
}
