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
use std::io::{self, Read, Write};
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
/// Retransmission: first after 200 ms, Linux's TCP_RTO_MIN, then doubling; the connection
/// is reset after 8 (Linux tcp_retries2 is 15, at minutes; a guest on the other end of a
/// pipe that stops acknowledging is gone).
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
    /// This side's sequence: the oldest byte the guest has not acknowledged, and the next.
    snd_una: u32,
    snd_nxt: u32,
    /// Bytes from `snd_una` sent to the guest and not yet acknowledged, then bytes not yet
    /// sent: what the guest may still need again.
    to_guest: VecDeque<u8>,
    /// Guest bytes for the host not yet written.
    to_host: VecDeque<u8>,
    /// The guest sent FIN; the host's write side is shut once `to_host` drains.
    guest_fin: bool,
    host_shut: bool,
    /// The host's socket ended; this side's FIN is sent once `to_guest` is.
    host_eof: bool,
    fin_sent: bool,
    /// Whether the guest has acknowledged this side's SYN, which takes a sequence number
    /// and no byte.
    syn_acked: bool,
    /// Retransmission: when the oldest unacknowledged byte was last sent, and how often.
    sent_at: Option<Instant>,
    retries: u32,
    pub closed: bool,
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
        payload: &[u8],
    ) -> bool;
}

impl Conn {
    /// A connection for the guest's SYN: its host socket connecting, without blocking.
    pub fn open(key: Key, seg: &wire::Tcp<'_>, isn: u32) -> io::Result<Conn> {
        let sock = connect(key.remote)?;
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
            to_guest: VecDeque::new(),
            to_host: VecDeque::new(),
            guest_fin: false,
            host_shut: false,
            host_eof: false,
            fin_sent: false,
            syn_acked: false,
            sent_at: None,
            retries: 0,
            closed: false,
        })
    }

    /// The window this side advertises, scaled if the guest scales.
    fn window(&self) -> u16 {
        let free = TO_HOST.saturating_sub(self.to_host.len());
        let shift = if self.scaled { OUR_WSCALE } else { 0 };
        u16::try_from(free >> shift).unwrap_or(u16::MAX)
    }

    fn ack(&self, out: &mut dyn ToGuest) {
        out.segment(
            &self.key,
            self.snd_nxt,
            self.rcv_nxt,
            ACK,
            self.window(),
            None,
            &[],
        );
    }

    /// A reset for the guest, ending the connection.
    pub fn reset(&mut self, out: &mut dyn ToGuest) {
        out.segment(&self.key, self.snd_nxt, self.rcv_nxt, RST | ACK, 0, None, &[]);
        self.closed = true;
    }

    /// Whether this connection waits for its socket to become writable: to finish
    /// connecting, or to take what the guest sent.
    pub fn wants_write(&self) -> bool {
        self.state == State::Connecting || !self.to_host.is_empty()
    }

    /// Whether it can take host bytes for the guest now.
    pub fn wants_read(&self) -> bool {
        self.state == State::Open
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
                &[],
            );
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.sent_at = Some(Instant::now());
            return;
        }
        self.flush_to_host(out);
    }

    fn flush_to_host(&mut self, out: &mut dyn ToGuest) {
        let before_window = self.window();
        while !self.to_host.is_empty() {
            let (a, _) = self.to_host.as_slices();
            match self.sock.write(a) {
                Ok(0) => break,
                Ok(n) => {
                    self.to_host.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
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
                Ok(n) => self.to_guest.extend(buf.get(..n).unwrap_or_default()),
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

    /// Sends what the guest has not had yet of `to_guest`, within its window, then FIN once
    /// all is sent and the host is done.
    fn send_new(&mut self, out: &mut dyn ToGuest) {
        if !self.syn_acked {
            return;
        }
        let mss = usize::from(self.guest_mss.min(MSS)).max(1);
        loop {
            let sent =
                (self.snd_nxt.wrapping_sub(self.snd_una) as usize).saturating_sub(usize::from(self.fin_sent));
            let unsent = self.to_guest.len().saturating_sub(sent);
            let in_flight = sent;
            let window = (self.guest_wnd as usize).saturating_sub(in_flight);
            let n = unsent.min(window).min(mss);
            if n == 0 {
                break;
            }
            let chunk: Vec<u8> = self.to_guest.range(sent..sent + n).copied().collect();
            let flags = ACK | if n == unsent { PSH } else { 0 };
            if !out.segment(
                &self.key,
                self.snd_nxt,
                self.rcv_nxt,
                flags,
                self.window(),
                None,
                &chunk,
            ) {
                break;
            }
            self.snd_nxt = self.snd_nxt.wrapping_add(n as u32);
            self.sent_at.get_or_insert_with(Instant::now);
        }
        let all_sent = self.snd_nxt.wrapping_sub(self.snd_una) as usize == self.to_guest.len();
        if self.host_eof
            && !self.fin_sent
            && all_sent
            && out.segment(
                &self.key,
                self.snd_nxt,
                self.rcv_nxt,
                FIN | ACK,
                self.window(),
                None,
                &[],
            )
        {
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.fin_sent = true;
            self.sent_at.get_or_insert_with(Instant::now);
        }
    }

    /// A segment from the guest for this connection.
    pub fn on_segment(&mut self, seg: &wire::Tcp<'_>, out: &mut dyn ToGuest) {
        if seg.flags & RST != 0 {
            self.closed = true;
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
                    &[],
                );
            }
            return;
        }
        if self.state != State::Open {
            return;
        }
        if seg.flags & ACK != 0 {
            let acked = seg.ack.wrapping_sub(self.snd_una);
            let outstanding = self.snd_nxt.wrapping_sub(self.snd_una);
            if acked > 0 && acked <= outstanding {
                // The SYN and FIN take a sequence number each, but no byte of to_guest.
                let mut bytes = acked as usize;
                if !self.syn_acked {
                    self.syn_acked = true;
                    bytes -= 1;
                }
                if self.fin_sent && seg.ack == self.snd_nxt {
                    bytes = bytes.saturating_sub(1);
                }
                self.to_guest.drain(..bytes.min(self.to_guest.len()));
                self.snd_una = seg.ack;
                self.retries = 0;
                self.sent_at = (self.snd_una != self.snd_nxt).then(Instant::now);
            }
            self.guest_wnd = u32::from(seg.window) << self.guest_wscale;
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
            let take = data.len().min(room);
            self.to_host.extend(data.get(..take).unwrap_or_default());
            self.rcv_nxt = self.rcv_nxt.wrapping_add(take as u32);
            answer = true;
        } else if !seg.payload.is_empty() {
            // Old or out of order: say where this side is.
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
        if self.fin_sent && self.snd_una == self.snd_nxt && self.guest_fin && self.host_shut {
            self.closed = true;
        }
    }

    /// When the next retransmission is due, if anything is unacknowledged.
    pub fn deadline(&self) -> Option<Instant> {
        self.sent_at.map(|t| t + RTO * 2u32.saturating_pow(self.retries))
    }

    /// Resends from `snd_una` if its time has come; resets the connection after
    /// [`RETRIES`].
    pub fn on_timer(&mut self, now: Instant, out: &mut dyn ToGuest) {
        let Some(due) = self.deadline() else { return };
        if now < due {
            return;
        }
        if self.retries >= RETRIES {
            self.reset(out);
            return;
        }
        self.retries += 1;
        // Everything from snd_una is sent again.
        let fin = self.fin_sent;
        self.snd_nxt = self.snd_una;
        self.fin_sent = false;
        self.sent_at = Some(now);
        if !self.syn_acked {
            let wscale = self.scaled.then_some(OUR_WSCALE);
            out.segment(
                &self.key,
                self.snd_una,
                self.rcv_nxt,
                SYN | ACK,
                self.window(),
                Some((MSS, wscale)),
                &[],
            );
            self.snd_nxt = self.snd_una.wrapping_add(1);
            return;
        }
        self.host_eof |= fin;
        self.send_new(out);
    }

    pub fn fd(&self) -> i32 {
        self.sock.as_raw_fd()
    }
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
