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

/// The ports a published connection comes from, at the gateway: IANA's dynamic range
/// (RFC 6335 §6), 49152 to 65535.
const EPHEMERAL: std::ops::RangeInclusive<u16> = 49152..=u16::MAX;

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

/// Docker's default bridge as a container on it sees it: the first address after the
/// gateway's in 172.17.0.0/16.
const DOCKER_GUEST_IP: Ipv4Addr = Ipv4Addr::new(172, 17, 0, 2);
const DOCKER_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(172, 17, 0, 1);

/// What a guest on Docker's default bridge has on its kernel command line:
/// `shards_net=ADDR/PREFIX,GATEWAY`.
pub fn docker_cmdline() -> String {
    format!("shards_net={DOCKER_GUEST_IP}/16,{DOCKER_GATEWAY_IP}")
}

/// A fresh guest MAC, random, locally administered and unicast, as current Docker gives
/// each container (measured under Docker Desktop, 2026-10-02).
pub fn random_mac() -> io::Result<[u8; 6]> {
    let mut mac = [0u8; 6];
    entropy(&mut mac)?;
    mac[0] = (mac[0] & 0xfe) | 0x02;
    Ok(mac)
}

/// `buf` filled from the kernel's random source.
fn entropy(buf: &mut [u8]) -> io::Result<()> {
    // getrandom(2), which the libc crate binds on glibc and musl alike; it fills a buffer
    // of at most 256 bytes at once once the pool is initialized, but a signal can
    // interrupt it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    loop {
        // SAFETY: getrandom(2) into a buffer of ours, of its length.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if usize::try_from(n).is_ok_and(|n| n == buf.len()) {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if n >= 0 || e.kind() != io::ErrorKind::Interrupted {
            return Err(if n >= 0 {
                io::Error::other("getrandom(2) fell short")
            } else {
                e
            });
        }
    }
    // getentropy(2) elsewhere, which fills up to 256 bytes or fails.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: getentropy(2) into a buffer of ours, of its length.
        if unsafe { libc::getentropy(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Config {
    /// Docker's default bridge as the guest whose MAC is `guest_mac` sees it.
    pub fn docker_default(policy: Policy, guest_mac: [u8; 6]) -> Config {
        Config {
            guest_mac,
            guest_ip: DOCKER_GUEST_IP,
            gateway_mac: [0x02, 0x42, 172, 17, 0, 1],
            gateway_ip: DOCKER_GATEWAY_IP,
            policy,
        }
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
    /// Published ports' listening sockets, each with the guest port it reaches.
    published: Vec<(std::net::TcpListener, u16)>,
    /// The gateway port the next published connection tries first.
    next_port: u16,
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
/// Published ports come on a socket of `controls`, from the daemon
/// ([`shards_ipc::kind::PUBLISH`]), and go on one, from the VM as its run ends
/// ([`shards_ipc::kind::UNPUBLISH`]).
pub fn serve(
    region: OwnedFd,
    wake_me: OwnedFd,
    wake_peer: OwnedFd,
    cfg: Config,
    mut controls: Vec<std::os::unix::net::UnixStream>,
) -> io::Result<()> {
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
        isn: seed()?,
        published: Vec::new(),
        next_port: *EPHEMERAL.start(),
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
        let control_at = fds.len();
        for c in &controls {
            fds.push(libc::pollfd {
                fd: c.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let (listeners_at, listening) = (fds.len(), stack.published.len());
        for (l, _) in &stack.published {
            fds.push(libc::pollfd {
                fd: l.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
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
        // Those that hung up are gone; what is published stays, as the VM runs on.
        let mut i = 0;
        controls.retain(|c| {
            let ready = fds.get(control_at + i).is_some_and(|p| p.revents != 0);
            i += 1;
            if !ready {
                return true;
            }
            match shards_ipc::recv(c) {
                // Said back once taken: the sender's copies may close.
                Ok(Some(m)) if m.kind == shards_ipc::kind::PUBLISH => {
                    stack.publish(&m.payload, m.fds);
                    shards_ipc::send(c, shards_ipc::kind::PUBLISH, &[], &[]).is_ok()
                }
                Ok(Some(m)) if m.kind == shards_ipc::kind::UNPUBLISH => {
                    stack.published.clear();
                    shards_ipc::send(c, shards_ipc::kind::UNPUBLISH, &[], &[]).is_ok()
                }
                Ok(Some(_)) => true,
                _ => false,
            }
        });
        // Those polled: a PUBLISH just read adds more, an UNPUBLISH leaves none.
        let accepting: Vec<usize> = (0..listening.min(stack.published.len()))
            .filter(|i| fds.get(listeners_at + i).is_some_and(|p| p.revents != 0))
            .collect();
        for i in accepting {
            stack.accept(i);
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
fn seed() -> io::Result<u32> {
    let mut b = [0u8; 4];
    entropy(&mut b)?;
    Ok(u32::from_ne_bytes(b))
}

impl<'r> Stack<'r> {
    /// Takes published ports' listening sockets, each for the guest port its payload's
    /// next big-endian u16 names; what does not pair up is closed.
    fn publish(&mut self, ports: &[u8], fds: Vec<OwnedFd>) {
        for (fd, port) in fds.into_iter().zip(ports.as_chunks::<2>().0) {
            let listener = std::net::TcpListener::from(fd);
            if listener.set_nonblocking(true).is_ok() {
                self.published.push((listener, u16::from_be_bytes(*port)));
            }
        }
    }

    /// Accepts what published port `i` holds: each connection opened to the guest's port,
    /// from the gateway, as a userland proxy's connection comes from it.
    fn accept(&mut self, i: usize) {
        loop {
            let Some((listener, guest_port)) = self.published.get(i) else {
                return;
            };
            let guest_port = *guest_port;
            let Ok((sock, _)) = listener.accept() else { return };
            if sock.set_nonblocking(true).is_err() {
                continue;
            }
            let Some(port) = self.free_port(guest_port) else {
                // Every port of the range in use: refused, as a full table refuses.
                continue;
            };
            let key = Key {
                guest_port,
                remote: (self.cfg.gateway_ip, port),
            };
            self.isn = self.isn.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let isn = self.isn;
            let c = {
                let mut o = self.out();
                Conn::accept(key, sock, isn, &mut o)
            };
            self.tcp.insert(key, c);
        }
    }

    /// A gateway port no connection to `guest_port` uses, from the ephemeral range, in
    /// turn: the guest tells connections apart by both ends.
    fn free_port(&mut self, guest_port: u16) -> Option<u16> {
        for _ in EPHEMERAL {
            let port = self.next_port;
            self.next_port = port
                .checked_add(1)
                .filter(|p| EPHEMERAL.contains(p))
                .unwrap_or(*EPHEMERAL.start());
            let key = Key {
                guest_port,
                remote: (self.cfg.gateway_ip, port),
            };
            if !self.tcp.contains_key(&key) {
                return Some(port);
            }
        }
        None
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each guest's MAC is its own, and one a host's stack takes as a unicast address of
    /// a local, not a vendor's, assignment.
    #[test]
    fn macs_are_fresh_local_unicast_ones() {
        let macs: Vec<[u8; 6]> = (0..64).map(|_| random_mac().unwrap()).collect();
        for mac in &macs {
            assert_eq!(mac[0] & 0x03, 0x02, "{mac:02x?}");
        }
        let mut distinct = macs.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), macs.len());
    }
}
