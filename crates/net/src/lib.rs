//! A microVM's network process (docs/design/architecture.md D31): the guest's frames, from
//! its virtio-net device through a shared ring (shards-netring), become host sockets, one
//! per flow, opened only if the VM's policy allows them; the host's answers become frames.
//! The guest's link reaches nothing else: no host loopback, no other VM, no host network
//! namespace. One such process serves one VM, so that its stack, which parses what the
//! guest and the Internet send, reaches no other VM if it is compromised.

#![cfg(unix)]

pub mod tcp;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use shards_netring::{Consumer, Producer, Region};
use tcp::{Conn, Key, ToGuest};
use wire::Frames;

/// How long a UDP flow lives without a datagram either way: Linux conntrack's timeout for
/// a UDP flow that has seen replies (nf_conntrack_udp_timeout_stream, 120 s).
const UDP_IDLE: Duration = Duration::from_secs(120);
/// Frames waiting for room in the ring, at most: past this the oldest datagram-like frame
/// is dropped, as a full NIC queue drops; TCP never adds to it unasked.
const BACKLOG: usize = 1024;

/// What a VM may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Everything but the host's own addresses: a build's steps, and `docker run`'s
    /// default bridge.
    AllowAll,
    /// Nothing: what a VM without grants has.
    DenyAll,
}

/// The guest's link: its address and MAC, the gateway's, and the VM's policy.
#[derive(Debug, Clone)]
pub struct Config {
    pub guest_mac: [u8; 6],
    pub guest_ip: Ipv4Addr,
    pub gateway_mac: [u8; 6],
    pub gateway_ip: Ipv4Addr,
    pub policy: Policy,
}

impl Config {
    /// Docker's default bridge as a container on it sees it (moby
    /// daemon/libnetwork/drivers/bridge): the first address after the gateway's in
    /// 172.17.0.0/16, its MAC made of its address (02:42 then the four octets).
    pub fn docker_default(policy: Policy) -> Config {
        let guest_ip = Ipv4Addr::new(172, 17, 0, 2);
        let [a, b, c, d] = guest_ip.octets();
        Config {
            guest_mac: [0x02, 0x42, a, b, c, d],
            guest_ip,
            gateway_mac: [0x02, 0x42, 172, 17, 0, 1],
            gateway_ip: Ipv4Addr::new(172, 17, 0, 1),
            policy,
        }
    }

    /// What the guest's kernel command line names: `shards_net=ADDR/PREFIX,GATEWAY`.
    pub fn cmdline(&self) -> String {
        format!("shards_net={}/16,{}", self.guest_ip, self.gateway_ip)
    }

    fn allows(&self, to: Ipv4Addr) -> bool {
        match self.policy {
            Policy::DenyAll => false,
            // The gateway would be the host itself: never by default (rootless-security.md
            // R4.16).
            Policy::AllowAll => to != self.gateway_ip && !to.is_loopback() && !to.is_unspecified(),
        }
    }
}

/// A UDP flow's host socket and when it was last used.
struct UdpFlow {
    sock: UdpSocket,
    last: Instant,
}

/// The stack's state: its frame ring, connections and flows.
struct Stack<'r> {
    cfg: Config,
    frames: Frames,
    to_guest: Producer<'r>,
    /// Frames for the guest the ring had no room for yet.
    backlog: VecDeque<Vec<u8>>,
    tcp: HashMap<Key, Conn>,
    udp: HashMap<(u16, Ipv4Addr, u16), UdpFlow>,
    buf: Vec<u8>,
    isn: u32,
}

/// Sends frames to the guest through the ring, into the backlog while it is full.
struct Out<'a, 'r> {
    frames: &'a Frames,
    to_guest: &'a mut Producer<'r>,
    backlog: &'a mut VecDeque<Vec<u8>>,
    guest_ip: Ipv4Addr,
    scratch: Vec<u8>,
    /// TCP segments wait for room rather than fill the backlog.
    tcp_blocked: bool,
}

impl Out<'_, '_> {
    fn send(&mut self, frame: &[u8]) -> bool {
        if self.backlog.is_empty() {
            match self.to_guest.try_push_with(frame.len(), |dst| {
                // SAFETY: the record's `frame.len()` bytes.
                unsafe { std::ptr::copy_nonoverlapping(frame.as_ptr(), dst, frame.len()) };
            }) {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
        }
        if self.backlog.len() >= BACKLOG {
            self.backlog.pop_front();
        }
        self.backlog.push_back(frame.to_vec());
        true
    }
}

impl ToGuest for Out<'_, '_> {
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
    ) -> bool {
        if !self.backlog.is_empty() && !payload.is_empty() {
            self.tcp_blocked = true;
            return false;
        }
        let mut f = std::mem::take(&mut self.scratch);
        self.frames.tcp(
            &mut f,
            key.remote,
            (self.guest_ip, key.guest_port),
            seq,
            ack,
            flags,
            window,
            syn,
            payload,
        );
        // A segment with bytes waits for room, so that what TCP sends is what it holds;
        // a bare control segment may queue.
        let sent = if payload.is_empty() {
            self.send(&f)
        } else {
            match self.to_guest.try_push_with(f.len(), |dst| {
                // SAFETY: the record's `f.len()` bytes.
                unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), dst, f.len()) };
            }) {
                Ok(Some(_)) => true,
                _ => {
                    self.tcp_blocked = true;
                    false
                }
            }
        };
        self.scratch = f;
        sent
    }
}

/// Serves the guest on `region`'s rings until the VM goes, ringing `wake_peer` and sleeping
/// on `wake_me`.
pub fn serve(region: OwnedFd, wake_me: OwnedFd, wake_peer: OwnedFd, cfg: Config) -> io::Result<()> {
    let region = Region::map(region)?;
    // The device's frames come on 0, this side's go on 1.
    let mut from_guest: Consumer<'_> = region.consumer(0, wake_peer.try_clone()?, wake_me.try_clone()?);
    let to_guest = region.producer(1, wake_peer, wake_me);
    let mut stack = Stack {
        frames: Frames {
            gateway_mac: cfg.gateway_mac,
            guest_mac: cfg.guest_mac,
        },
        cfg,
        to_guest,
        backlog: VecDeque::new(),
        tcp: HashMap::new(),
        udp: HashMap::new(),
        buf: vec![0u8; shards_netring::MAX_FRAME],
        isn: seed(),
    };
    let mut frame = vec![0u8; shards_netring::MAX_FRAME];
    let mut fds: Vec<libc::pollfd> = Vec::new();
    let mut keys: Vec<Option<Key>> = Vec::new();
    loop {
        // The guest's frames, a batch at a time.
        for _ in 0..256 {
            let got = from_guest
                .pop(|n, copy| {
                    copy(0, frame.as_mut_ptr(), n);
                    n
                })
                .map_err(|e| io::Error::other(e.to_string()))?;
            let Some(n) = got else { break };
            stack.on_guest_frame(frame.get(..n).unwrap_or_default());
        }
        stack.flush_backlog();
        // Host sockets, and the ring.
        fds.clear();
        keys.clear();
        fds.push(libc::pollfd {
            fd: from_guest.waits_on(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(None);
        for (k, c) in &stack.tcp {
            let mut events = 0;
            if c.wants_read() && stack.backlog.is_empty() {
                events |= libc::POLLIN;
            }
            if c.wants_write() {
                events |= libc::POLLOUT;
            }
            if events != 0 {
                fds.push(libc::pollfd {
                    fd: c.fd(),
                    events,
                    revents: 0,
                });
                keys.push(Some(*k));
            }
        }
        let udp_base = fds.len();
        let udp_keys: Vec<(u16, Ipv4Addr, u16)> = stack.udp.keys().copied().collect();
        for k in &udp_keys {
            if let Some(f) = stack.udp.get(k) {
                fds.push(libc::pollfd {
                    fd: f.sock.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        let now = Instant::now();
        let next = stack
            .tcp
            .values()
            .filter_map(Conn::deadline)
            .chain(stack.udp.values().map(|f| f.last + UDP_IDLE))
            .min();
        let busy =
            from_guest.ready().map_err(|e| io::Error::other(e.to_string()))? || !stack.backlog.is_empty();
        let timeout = if busy {
            0
        } else {
            let _ = from_guest.arm();
            next.map_or(-1, |t| {
                i32::try_from(t.saturating_duration_since(now).as_millis())
                    .unwrap_or(i32::MAX)
                    .max(1)
            })
        };
        // SAFETY: an array of pollfds of the length given.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        // The guest's device closed the ring's doorbell: the VM is gone.
        if fds
            .first()
            .is_some_and(|p| p.revents & (libc::POLLHUP | libc::POLLERR) != 0)
        {
            return Ok(());
        }
        drain(from_guest.waits_on());
        for (p, k) in fds.iter().zip(&keys).skip(1) {
            let Some(k) = k else { continue };
            if p.revents == 0 {
                continue;
            }
            stack.on_socket(k, p.revents);
        }
        for (i, k) in udp_keys.iter().enumerate() {
            if fds.get(udp_base + i).is_some_and(|p| p.revents != 0) {
                stack.on_udp(k);
            }
        }
        stack.timers(Instant::now());
    }
}

fn drain(fd: i32) {
    let mut buf = [0u8; 64];
    // SAFETY: a local buffer, from a non-blocking descriptor.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// A starting point for initial sequence numbers no guest can predict from the last.
fn seed() -> u32 {
    let mut b = [0u8; 4];
    // SAFETY: getentropy(3) into a local buffer.
    unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) };
    u32::from_ne_bytes(b)
}

impl<'r> Stack<'r> {
    fn out(&mut self) -> Out<'_, 'r> {
        Out {
            frames: &self.frames,
            to_guest: &mut self.to_guest,
            backlog: &mut self.backlog,
            guest_ip: self.cfg.guest_ip,
            scratch: Vec::new(),
            tcp_blocked: false,
        }
    }

    fn flush_backlog(&mut self) {
        while let Some(f) = self.backlog.front() {
            match self.to_guest.try_push_with(f.len(), |dst| {
                // SAFETY: the record's `f.len()` bytes.
                unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), dst, f.len()) };
            }) {
                Ok(Some(_)) => {
                    self.backlog.pop_front();
                }
                _ => break,
            }
        }
    }

    fn on_guest_frame(&mut self, f: &[u8]) {
        let Some(e) = f.get(wire::VNET..).and_then(wire::eth) else {
            return;
        };
        if e.src != self.cfg.guest_mac {
            return;
        }
        match e.kind {
            wire::ETHERTYPE_ARP => {
                if let Some(req) = wire::arp_request(e.payload)
                    && req.sender_ip == self.cfg.guest_ip
                    && req.target_ip != self.cfg.guest_ip
                {
                    let mut out = Vec::new();
                    self.frames.arp_reply(&mut out, &req);
                    self.out().send(&out);
                }
            }
            wire::ETHERTYPE_IPV4 => {
                let Some(ip) = wire::ipv4(e.payload) else { return };
                if ip.src != self.cfg.guest_ip {
                    return;
                }
                match ip.proto {
                    wire::PROTO_ICMP => self.on_icmp(&ip),
                    wire::PROTO_UDP => self.on_guest_udp(&ip),
                    wire::PROTO_TCP => self.on_guest_tcp(&ip),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// An echo request to the gateway is answered here; others are not reached yet.
    fn on_icmp(&mut self, ip: &wire::Ip<'_>) {
        let p = ip.payload;
        if p.first() == Some(&8) && ip.dst == self.cfg.gateway_ip {
            let mut out = Vec::new();
            self.frames
                .icmp_echo_reply(&mut out, ip.dst, ip.src, p.get(4..).unwrap_or_default());
            self.out().send(&out);
        }
    }

    fn on_guest_udp(&mut self, ip: &wire::Ip<'_>) {
        let Some(u) = wire::udp(ip.payload) else { return };
        if !self.cfg.allows(ip.dst) {
            return;
        }
        let key = (u.src_port, ip.dst, u.dst_port);
        let f = match self.udp.entry(key) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
                    return;
                };
                if sock.connect((ip.dst, u.dst_port)).is_err() || sock.set_nonblocking(true).is_err() {
                    return;
                }
                v.insert(UdpFlow {
                    sock,
                    last: Instant::now(),
                })
            }
        };
        let _ = f.sock.send(u.payload);
        f.last = Instant::now();
    }

    fn on_udp(&mut self, key: &(u16, Ipv4Addr, u16)) {
        let (guest_ip, frames) = (self.cfg.guest_ip, &self.frames);
        let Some(f) = self.udp.get_mut(key) else { return };
        let mut out = Vec::new();
        while let Ok(n) = f.sock.recv(&mut self.buf) {
            {
                {
                    frames.udp(
                        &mut out,
                        (key.1, key.2),
                        (guest_ip, key.0),
                        self.buf.get(..n).unwrap_or_default(),
                    );
                    f.last = Instant::now();
                    let mut o = Out {
                        frames,
                        to_guest: &mut self.to_guest,
                        backlog: &mut self.backlog,
                        guest_ip,
                        scratch: Vec::new(),
                        tcp_blocked: false,
                    };
                    o.send(&out);
                }
            }
        }
    }

    fn on_guest_tcp(&mut self, ip: &wire::Ip<'_>) {
        let Some(seg) = wire::tcp(ip.payload) else { return };
        let key = Key {
            guest_port: seg.src_port,
            remote: (ip.dst, seg.dst_port),
        };
        let mut conn = self.tcp.remove(&key);
        if conn.is_none() {
            if seg.flags & wire::SYN == 0 || seg.flags & wire::ACK != 0 {
                // Not a connection's start, and no connection: a reset, as a host with no
                // such connection answers (RFC 9293 §3.10.7.1).
                if seg.flags & wire::RST == 0 {
                    let ack = seg
                        .seq
                        .wrapping_add(seg.payload.len() as u32)
                        .wrapping_add(u32::from(seg.flags & (wire::SYN | wire::FIN) != 0));
                    let mut o = self.out();
                    o.segment(&key, seg.ack, ack, wire::RST | wire::ACK, 0, None, &[]);
                }
                return;
            }
            if !self.cfg.allows(ip.dst) {
                let mut o = self.out();
                o.segment(
                    &key,
                    0,
                    seg.seq.wrapping_add(1),
                    wire::RST | wire::ACK,
                    0,
                    None,
                    &[],
                );
                return;
            }
            self.isn = self.isn.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            match Conn::open(key, &seg, self.isn) {
                Ok(c) => conn = Some(c),
                Err(_) => {
                    let mut o = self.out();
                    o.segment(
                        &key,
                        0,
                        seg.seq.wrapping_add(1),
                        wire::RST | wire::ACK,
                        0,
                        None,
                        &[],
                    );
                    return;
                }
            }
        }
        if let Some(mut c) = conn {
            {
                let mut o = self.out();
                c.on_segment(&seg, &mut o);
            }
            if !c.closed {
                self.tcp.insert(key, c);
            }
        }
    }

    fn on_socket(&mut self, key: &Key, revents: i16) {
        let Some(mut c) = self.tcp.remove(key) else { return };
        let mut buf = std::mem::take(&mut self.buf);
        {
            let mut o = Out {
                frames: &self.frames,
                to_guest: &mut self.to_guest,
                backlog: &mut self.backlog,
                guest_ip: self.cfg.guest_ip,
                scratch: Vec::new(),
                tcp_blocked: false,
            };
            if revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                c.on_writable(&mut o);
            }
            if !c.closed && revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                c.on_readable(&mut o, &mut buf);
            }
        }
        self.buf = buf;
        if !c.closed {
            self.tcp.insert(*key, c);
        }
    }

    fn timers(&mut self, now: Instant) {
        let keys: Vec<Key> = self.tcp.keys().copied().collect();
        for k in keys {
            let Some(mut c) = self.tcp.remove(&k) else {
                continue;
            };
            {
                let mut o = self.out();
                c.on_timer(now, &mut o);
            }
            if !c.closed {
                self.tcp.insert(k, c);
            }
        }
        self.udp
            .retain(|_, f| now.saturating_duration_since(f.last) < UDP_IDLE);
    }
}
