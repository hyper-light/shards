//! A microVM's network process (docs/design/architecture.md D31): the guest's frames, from
//! its virtio-net device through a shared ring (shards-netring), become host sockets, one
//! per flow, opened only if the VM's policy allows them; the host's answers become frames.
//! The guest's link reaches nothing else: no host loopback, no other VM, no host network
//! namespace. One such process serves one VM, so that its stack, which parses what the
//! guest and the Internet send, shares no memory with another VM's; the confinement that
//! would keep a compromised one from the rest of the host (seccomp, Landlock, App
//! Sandbox) is still to be built (architecture.md D31).

#![cfg(unix)]

pub mod bridge;
#[cfg(target_os = "linux")]
pub mod confine;
pub mod dns;
pub mod pktinfo;
mod poll;
mod siphash;
pub mod tcp;
pub mod wire;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use shards_netring::{Consumer, Producer, RING, Region};
use tcp::{Conn, Key, Payload, ToGuest};
use wire::Frames;

/// The ports a published connection comes from, at the gateway: IANA's dynamic range
/// (RFC 6335 §6), 49152 to 65535.
const EPHEMERAL: std::ops::RangeInclusive<u16> = 49152..=u16::MAX;

/// Packets or connections one socket gives up per pass of the loop, before the others
/// have their turn: Linux's own budget for a device per poll (net.core.dev_weight, 64;
/// Documentation/admin-guide/sysctl/net.rst), for the same fairness. A flood on one
/// published port leaves the rest of the VM's flows served.
const BUDGET: usize = 64;

/// How long a UDP flow lives, as Linux conntrack keeps one (nf_conntrack_proto_udp.c,
/// udp_packet; v6.12): 30 s past its last datagram (nf_conntrack_udp_timeout), or 120 s
/// (nf_conntrack_udp_timeout_stream) once it has had a reply and is still going 2 s after
/// its first datagram, a stream. A query and its answer hold a port 30 s, not 120.
const UDP_UNREPLIED: Duration = Duration::from_secs(30);
const UDP_STREAM: Duration = Duration::from_secs(120);
const UDP_STREAM_AFTER: Duration = Duration::from_secs(2);

/// When a UDP flow ends, unless another datagram comes.
#[derive(Debug, Clone, Copy)]
struct Lifetime {
    first: Instant,
    replied: bool,
    until: Instant,
}

impl Lifetime {
    fn new(now: Instant) -> Lifetime {
        Lifetime {
            first: now,
            replied: false,
            until: now + UDP_UNREPLIED,
        }
    }

    /// A datagram at `now`; a `reply` if it went the other way from the first.
    fn datagram(&mut self, now: Instant, reply: bool) {
        self.replied |= reply;
        let stream = self.replied && now > self.first + UDP_STREAM_AFTER;
        self.until = now + if stream { UDP_STREAM } else { UDP_UNREPLIED };
    }
}
/// Frames waiting for room in the ring, at most: past this the oldest datagram-like frame
/// is dropped, as a full NIC queue drops; TCP never adds to it unasked.
const BACKLOG: usize = 1024;

/// A transport protocol a grant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

/// The ports a VM may open flows to (`Policy::Ports`): its image's Agentfile's egress
/// grants, each a range of one protocol's ports, as Docker writes a port (`443`,
/// `53/udp`, `8000-8010/tcp`), TCP where none is said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ports(pub Vec<(Proto, u16, u16)>);

/// `NET_POLICY`'s payload: each port range as [`Ports::encode`] writes one; 0x81 where any
/// name may be resolved past the microVM; then each name granted as 0x80, its port in
/// network order, its length's byte and the name.
pub fn encode_policy(ports: &Ports, named: &[(String, u16)], dns_all: bool) -> Vec<u8> {
    let mut out = ports.encode();
    if dns_all {
        out.push(0x81);
    }
    for (host, port) in named {
        let Ok(len) = u8::try_from(host.len()) else {
            continue;
        };
        out.push(0x80);
        out.extend_from_slice(&port.to_be_bytes());
        out.push(len);
        out.extend_from_slice(host.as_bytes());
    }
    out
}

/// A policy as `NET_POLICY` carries it: the ports, the names granted with their ports, and
/// whether any name may be resolved.
pub type Granted = (Ports, Vec<(String, u16)>, bool);

/// What [`encode_policy`] wrote; None for a malformed payload.
pub fn decode_policy(mut bytes: &[u8]) -> Option<Granted> {
    let mut ranges = Vec::new();
    let mut named = Vec::new();
    let mut dns_all = false;
    while let Some(&tag) = bytes.first() {
        if tag == 0x81 {
            dns_all = true;
            bytes = bytes.get(1..)?;
        } else if tag == 0x80 {
            let (&[_, a, b, len], rest) = bytes.split_first_chunk::<4>()?;
            let (name, rest) = rest.split_at_checked(usize::from(len))?;
            named.push((String::from_utf8(name.to_vec()).ok()?, u16::from_be_bytes([a, b])));
            bytes = rest;
        } else {
            let (record, rest) = bytes.split_at_checked(5)?;
            ranges.extend(Ports::decode(record)?.0);
            bytes = rest;
        }
    }
    Some((Ports(ranges), named, dns_all))
}

impl Ports {
    /// Ports as the build's label lists them, comma-separated.
    pub fn parse(text: &str) -> Result<Ports, String> {
        let mut out = Vec::new();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (range, proto) = match item.rsplit_once('/') {
                Some((r, "tcp")) => (r, Proto::Tcp),
                Some((r, "udp")) => (r, Proto::Udp),
                Some((_, p)) => return Err(format!("{item:?}: no protocol {p:?}, only tcp or udp")),
                None => (item, Proto::Tcp),
            };
            let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p > 0);
            let (lo, hi) = match range.split_once('-') {
                Some((a, b)) => (port(a), port(b)),
                None => (port(range), port(range)),
            };
            match (lo, hi) {
                (Some(lo), Some(hi)) if lo <= hi => out.push((proto, lo, hi)),
                _ => return Err(format!("{item:?}: no port or range of ports")),
            }
        }
        Ok(Ports(out))
    }

    /// As `NET_POLICY` carries them: each a protocol's IP number and the range's ends,
    /// in network order.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() * 5);
        for (proto, lo, hi) in &self.0 {
            out.push(match proto {
                Proto::Tcp => 6,
                Proto::Udp => 17,
            });
            out.extend_from_slice(&lo.to_be_bytes());
            out.extend_from_slice(&hi.to_be_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Ports> {
        let (chunks, rest) = bytes.as_chunks::<5>();
        if !rest.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(chunks.len());
        for &[p, a, b, c, d] in chunks {
            let proto = match p {
                6 => Proto::Tcp,
                17 => Proto::Udp,
                _ => return None,
            };
            let (lo, hi) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
            if lo == 0 || lo > hi {
                return None;
            }
            out.push((proto, lo, hi));
        }
        Some(Ports(out))
    }

    /// Whether `port` of `proto` is among them.
    pub fn has(&self, proto: Proto, port: u16) -> bool {
        self.0
            .iter()
            .any(|&(p, lo, hi)| p == proto && (lo..=hi).contains(&port))
    }
}

/// What a VM may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// The Internet and the host's networks, as BuildKit's default network reaches them:
    /// a build's steps. Not the host itself (the gateway, its loopback); not link-local
    /// addresses, a cloud's instance metadata (169.254.169.254) among them, which a
    /// request made from the host's own stack would reach past the hop limit that keeps
    /// it from Docker's bridged containers (AWS IMDSv2's); and not multicast or broadcast,
    /// which Docker's bridge does not route out.
    AllowAll,
    /// Nothing: a run's, by default (spec §3), and a VM without grants.
    DenyAll,
    /// What `AllowAll` reaches, on these ports alone: an Agentfile's egress grants, the
    /// union of its agents' (D59), each agent held to its own by its microVM's switch.
    Ports(Ports),
    /// A build's proxy alone (D110, BuildKit's `exec.proxy`): TCP to the gateway's `port`,
    /// carried to the proxy's Unix socket on the host; nothing else, neither the host's
    /// networks nor the Internet, nor a resolver.
    Proxy { port: u16, socket: std::path::PathBuf },
}

/// The guest's link: its address and MAC, the gateway's, and the VM's policy.
#[derive(Debug, Clone)]
pub struct Config {
    pub guest_mac: [u8; 6],
    pub guest_ip: Ipv4Addr,
    pub gateway_mac: [u8; 6],
    pub gateway_ip: Ipv4Addr,
    /// The guest's IPv6 address and its gateway's, on a network with IPv6 (D99): none
    /// else, and the guest's IPv6 frames are dropped.
    pub guest_ip6: Option<Ipv6Addr>,
    pub gateway_ip6: Option<Ipv6Addr>,
    pub policy: Policy,
    /// The resolvers a guest's names past the microVM are asked of, the host's own
    /// (`/etc/resolv.conf`, or `SHARDS_DNS`): asked only under [`Policy::Ports`], for a
    /// name the network's members do not hold, as Docker's embedded DNS asks the host's
    /// for a name its network does not hold.
    pub resolvers: Vec<(Ipv4Addr, u16)>,
    /// Destinations granted by name, each a host and a TCP port (an Agentfile's remote MCP
    /// servers, D59): reached at the address a host is, or at those the host's resolver
    /// answered for it through this process.
    pub named: Vec<(String, u16)>,
    /// What the named have resolved to, each address with its grant's port.
    pub learned: HashSet<(IpAddr, u16)>,
    /// Whether any name may be asked past the microVM (`NETWORK --dns`); else only the
    /// named's, and every other question is REFUSED.
    pub dns_all: bool,
}

/// A MAC address as shards' processes pass it to one another (review 2.32): its six
/// bytes in hex, two digits each, between colons, as Linux and Docker write one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mac(pub [u8; 6]);

impl std::fmt::Display for Mac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl std::str::FromStr for Mac {
    type Err = String;

    fn from_str(s: &str) -> Result<Mac, String> {
        let not = || format!("{s:?} is not a MAC");
        let mut octets = s.split(':');
        let mut mac = [0u8; 6];
        for byte in &mut mac {
            let hex = octets
                .next()
                .filter(|h| h.len() == 2 && h.bytes().all(|c| c.is_ascii_hexdigit()))
                .ok_or_else(not)?;
            *byte = u8::from_str_radix(hex, 16).map_err(|_| not())?;
        }
        match octets.next() {
            Some(_) => Err(not()),
            None => Ok(Mac(mac)),
        }
    }
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
pub fn entropy(buf: &mut [u8]) -> io::Result<()> {
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
    /// Docker's default bridge, `bridge`, as the guest whose MAC is `guest_mac` sees it:
    /// the gateway's MAC made of its address, as Docker made its own (02:42 and the
    /// address's four bytes).
    pub fn on_bridge(policy: Policy, guest_mac: [u8; 6], bridge: &bridge::Bridge) -> Config {
        let [a, b, c, d] = bridge.gateway().octets();
        Config {
            guest_mac,
            guest_ip: bridge.guest(),
            gateway_mac: [0x02, 0x42, a, b, c, d],
            gateway_ip: bridge.gateway(),
            guest_ip6: None,
            gateway_ip6: None,
            policy,
            resolvers: Vec::new(),
            named: Vec::new(),
            learned: HashSet::new(),
            dns_all: false,
        }
    }

    /// The gateway of `ip`'s version: the address the guest reaches this process at.
    fn gateway_of(&self, ip: IpAddr) -> IpAddr {
        match (ip, self.gateway_ip6) {
            (IpAddr::V6(_), Some(g)) => IpAddr::V6(g),
            _ => IpAddr::V4(self.gateway_ip),
        }
    }

    /// Where a connection to `to`'s `port` goes under a build's proxy (D110): to the
    /// proxy's socket where it is the gateway's proxy port; none for anything else.
    fn proxy_flow(&self, to: IpAddr, port: u16) -> Option<std::path::PathBuf> {
        match &self.policy {
            Policy::Proxy { port: p, socket } if to == IpAddr::V4(self.gateway_ip) && port == *p => {
                Some(socket.clone())
            }
            _ => None,
        }
    }

    fn allows(&self, to: IpAddr, proto: Proto, port: u16) -> bool {
        // An IPv4 address carried in IPv6 (`::ffff:a.b.c.d`) is that address, and is
        // judged as it is: a host socket to it reaches it.
        let to = to.to_canonical();
        let named = proto == Proto::Tcp
            && (self.learned.contains(&(to, port))
                || self.named.iter().any(|(h, p)| {
                    *p == port && h.parse::<IpAddr>().ok().map(|a| a.to_canonical()) == Some(to)
                }));
        match &self.policy {
            // The proxy's one flow is the stack's own (`proxy_flow`), past this.
            Policy::DenyAll | Policy::Proxy { .. } => false,
            Policy::Ports(p) if !p.has(proto, port) && !named => false,
            // The gateway would be the host itself: never by default (rootless-security.md
            // R4.16).
            Policy::AllowAll | Policy::Ports(_) => match to {
                IpAddr::V4(to) => {
                    to != self.gateway_ip
                        && !to.is_loopback()
                        && !to.is_unspecified()
                        && !to.is_link_local()
                        && !to.is_multicast()
                        && !to.is_broadcast()
                }
                // IPv6's: not the gateway, the loopback, the unspecified address, link-local
                // unicast (fe80::/10, RFC 4291 §2.5.6) or multicast (ff00::/8), as IPv4's;
                // nor IPv4-compatible addresses (::/96, deprecated by RFC 4291 §2.5.5.1),
                // which some stacks still route to IPv4.
                IpAddr::V6(to) => {
                    Some(to) != self.gateway_ip6
                        && !to.is_loopback()
                        && !to.is_unspecified()
                        && !to.is_unicast_link_local()
                        && !to.is_multicast()
                        && to.segments().get(..6) != Some(&[0; 6])
                }
            },
        }
    }
}

/// A published port's host socket: listening for TCP, or bound for UDP's datagrams.
enum Listener {
    Tcp(std::net::TcpListener),
    Udp(UdpSocket),
}

impl Listener {
    fn as_raw_fd(&self) -> i32 {
        match self {
            Listener::Tcp(l) => l.as_raw_fd(),
            Listener::Udp(u) => u.as_raw_fd(),
        }
    }
}

/// A host peer's datagrams to a published UDP port: the guest has them from a gateway
/// port of the peer's own, and its answers to that port go back to the peer, from the
/// host address it asked.
struct Inbound {
    /// Its port, in `published`.
    published: usize,
    peer: std::net::SocketAddr,
    asked: Option<std::net::IpAddr>,
    guest_port: u16,
    life: Lifetime,
}

/// A UDP flow's host socket and how long it lives.
struct UdpFlow {
    sock: UdpSocket,
    life: Lifetime,
}

/// The guest's ends of a UDP flow: its port, and the remote address and port.
type UdpKey = (u16, IpAddr, u16);

/// What a poller's token or a timer names (review 2.14): its kind in the top byte, then
/// the generation of the slot it names, then the slot, so that what an earlier holder of
/// a slot left is never taken for its next's.
mod token {
    pub const DOORBELL: u64 = 0;
    pub const CONTROL: u64 = 1;
    pub const LISTENER: u64 = 2;
    pub const TCP: u64 = 3;
    pub const UDP: u64 = 4;
    pub const INBOUND: u64 = 5;
    pub const PEER: u64 = 6;

    pub fn of(kind: u64, generation: u32, index: u32) -> u64 {
        kind << 56 | (u64::from(generation) & 0xff_ffff) << 32 | u64::from(index)
    }

    pub fn kind(token: u64) -> u64 {
        token >> 56
    }

    pub fn generation(token: u64) -> u32 {
        ((token >> 32) & 0xff_ffff) as u32
    }

    pub fn index(token: u64) -> u32 {
        token as u32
    }
}

/// The guest's TCP connections, each in a slot of its own, found by its ends.
#[derive(Default)]
struct Tcp {
    slots: Vec<TcpSlot>,
    free: Vec<u32>,
    by_key: HashMap<Key, u32>,
}

/// A connection's slot: the connection, out of it while it is being served; what the
/// poller waits for on its socket; when its timer is armed; whether it waits among the
/// blocked.
#[derive(Default)]
struct TcpSlot {
    generation: u32,
    conn: Option<Conn>,
    key: Option<Key>,
    interest: poll::Interest,
    armed: Option<Instant>,
    blocked: bool,
}

impl Tcp {
    /// A slot for the connection of `key`, empty until it is settled into it.
    fn reserve(&mut self, key: Key) -> u32 {
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(TcpSlot::default());
                u32::try_from(self.slots.len() - 1).unwrap_or(u32::MAX)
            }
        };
        if let Some(slot) = self.slots.get_mut(i as usize) {
            *slot = TcpSlot {
                generation: slot.generation.wrapping_add(1),
                key: Some(key),
                ..TcpSlot::default()
            };
        }
        self.by_key.insert(key, i);
        i
    }

    fn slot(&mut self, i: u32) -> Option<&mut TcpSlot> {
        self.slots.get_mut(i as usize)
    }

    /// The slot `token` names, if its connection is still that one.
    fn slot_of(&self, token: u64) -> Option<u32> {
        let i = token::index(token);
        self.slots
            .get(i as usize)
            .filter(|s| s.key.is_some() && s.generation & 0xff_ffff == token::generation(token))
            .map(|_| i)
    }

    fn token(&self, i: u32) -> u64 {
        let generation = self.slots.get(i as usize).map_or(0, |s| s.generation);
        token::of(token::TCP, generation, i)
    }

    /// Takes slot `i`'s connection out, to serve it.
    fn take(&mut self, i: u32) -> Option<Conn> {
        self.slot(i).and_then(|s| s.conn.take())
    }

    /// Empties slot `i`, its connection gone.
    fn release(&mut self, i: u32) {
        if let Some(slot) = self.slots.get_mut(i as usize) {
            if let Some(key) = slot.key.take() {
                self.by_key.remove(&key);
            }
            *slot = TcpSlot {
                generation: slot.generation,
                ..TcpSlot::default()
            };
            self.free.push(i);
        }
    }
}

/// The stack's state: its frame ring, connections and flows.
struct Stack<'r> {
    cfg: Config,
    frames: Frames,
    to_guest: Producer<'r>,
    /// Frames for the guest the ring had no room for yet.
    backlog: VecDeque<Vec<u8>>,
    tcp: Tcp,
    udp: HashMap<UdpKey, UdpFlow>,
    /// UDP flows by their numbers, and the next number.
    udp_ids: HashMap<u32, UdpKey>,
    next_udp: u32,
    /// What waits for the host's sockets, the timers due, earliest first, and the
    /// connections a full ring kept from the guest, in turn (review 2.14).
    poller: poll::Poller,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    blocked: VecDeque<u64>,
    buf: Vec<u8>,
    /// Where segments to the guest are made.
    scratch: Vec<u8>,
    /// The key of this process's initial sequence numbers, and when it began: their
    /// clock's origin ([`Stack::isn`]).
    isn_key: [u8; 16],
    began: Instant,
    /// Published ports' host sockets, each with the guest port it reaches.
    published: Vec<(Listener, u16)>,
    /// Published UDP ports' flows, by their gateway ports, and those ports by their
    /// published port and peer.
    inbound: HashMap<u16, Inbound>,
    inbound_ports: HashMap<(usize, std::net::SocketAddr), u16>,
    /// The gateway port the next published connection tries first.
    next_port: u16,
    /// The other VMs on the guest's network (D46), by the slot their token names.
    peers: Vec<Option<Peer>>,
    /// The names its members answer to, which the gateway's resolver says: none off a
    /// network of its own.
    names: Option<dns::Names>,
    /// Whether the process is held to an Agentfile's TCP ports already (D31, `NET_POLICY`).
    #[cfg(target_os = "linux")]
    ports_confined: bool,
}

/// A peer on the guest's network (D46): another VM's network process, on a stream socket
/// the daemon paired the two with, and its guest's address. A frame goes each way whole,
/// its length's four bytes first. One the socket takes only in part is finished before
/// the next goes; frames meanwhile are dropped, as a switch whose port is full drops them,
/// and the guests' TCP sends them again.
struct Peer {
    ip: Ipv4Addr,
    /// Its IPv6 address, on a network with IPv6.
    ip6: Option<Ipv6Addr>,
    sock: std::os::unix::net::UnixStream,
    /// What of the frame being sent the socket has yet to take.
    unsent: Vec<u8>,
    /// What has come of the frames being received.
    received: Vec<u8>,
}

/// Sends frames to the guest through the ring, into the backlog while it is full.
struct Out<'a, 'r> {
    frames: &'a Frames,
    to_guest: &'a mut Producer<'r>,
    backlog: &'a mut VecDeque<Vec<u8>>,
    guest_ip: Ipv4Addr,
    guest_ip6: Option<Ipv6Addr>,
    /// Where segments are made, kept from one to the next.
    scratch: &'a mut Vec<u8>,
}

impl Out<'_, '_> {
    /// Sends a frame of `parts`, one after another: into the ring, or queued while it is
    /// full or others wait.
    fn send(&mut self, parts: &[&[u8]]) -> bool {
        if self.backlog.is_empty() {
            match self.to_guest.try_push(parts) {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
        }
        if self.backlog.len() >= BACKLOG {
            self.backlog.pop_front();
        }
        self.backlog.push_back(parts.concat());
        true
    }

    /// Sends the frame `build` writes, made in the scratch.
    fn built(&mut self, build: impl FnOnce(&Frames, &mut Vec<u8>)) -> bool {
        let mut f = std::mem::take(self.scratch);
        build(self.frames, &mut f);
        let sent = self.send(&[&f]);
        *self.scratch = f;
        sent
    }

    /// The guest's address of `ip`'s version.
    fn guest_of(&self, ip: IpAddr) -> IpAddr {
        match (ip, self.guest_ip6) {
            (IpAddr::V6(_), Some(g)) => IpAddr::V6(g),
            _ => IpAddr::V4(self.guest_ip),
        }
    }

    /// Sends the guest `payload` from `src`, to its port `port`: the datagram's headers
    /// made in the scratch, its bytes copied from where they lie.
    fn datagram(&mut self, src: (IpAddr, u16), port: u16, payload: &[u8]) -> bool {
        let mut head = std::mem::take(self.scratch);
        let guest = self.guest_of(src.0);
        self.frames
            .udp_headers(&mut head, src, (guest, port), payload.len());
        let sent = self.send(&[&head, payload]);
        *self.scratch = head;
        sent
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
        payload: Payload<'_>,
    ) -> bool {
        let [a, b] = payload;
        let len = a.len() + b.len();
        // A segment with bytes waits for room rather than fill the backlog, so that what
        // TCP sends is what it holds; a bare control segment may queue.
        if len > 0 && !self.backlog.is_empty() {
            return false;
        }
        let mut head = std::mem::take(self.scratch);
        let guest = self.guest_of(key.remote.0);
        self.frames.tcp_headers(
            &mut head,
            key.remote,
            (guest, key.guest_port),
            seq,
            ack,
            flags,
            window,
            syn,
            len,
        );
        let sent = if len == 0 {
            self.send(&[&head])
        } else {
            matches!(self.to_guest.try_push(&[&head, a, b]), Ok(Some(_)))
        };
        *self.scratch = head;
        sent
    }
}

/// Whose a control socket is, and so what it may say (review 2.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The daemon's: published ports come on it ([`shards_ipc::kind::PUBLISH`]).
    Daemon,
    /// The VM's: published ports go on it as its run ends
    /// ([`shards_ipc::kind::UNPUBLISH`]). A VM gives nothing to publish.
    Release,
}

impl Control {
    /// Whether a message of `kind` is this socket's to send.
    fn says(self, kind: u8) -> bool {
        match self {
            Control::Daemon => matches!(
                kind,
                shards_ipc::kind::PUBLISH
                    | shards_ipc::kind::NET_ADDRESS
                    | shards_ipc::kind::NET_PEER
                    | shards_ipc::kind::NET_NAMES
                    | shards_ipc::kind::NET_POLICY
            ),
            Control::Release => kind == shards_ipc::kind::UNPUBLISH,
        }
    }
}

/// Serves the guest on `region`'s rings until the VM goes, ringing `wake_peer` and sleeping
/// on `wake_me`. Published ports come and go on `controls`, each of whom [`Control`] says;
/// their messages are taken as each comes whole, so that one cut short holds up nothing.
pub fn serve(
    region: OwnedFd,
    wake_me: OwnedFd,
    wake_peer: OwnedFd,
    cfg: Config,
    controls: Vec<(Control, std::os::unix::net::UnixStream)>,
) -> io::Result<()> {
    // Each control socket keeps its place, a token's index, until it hangs up.
    let mut controls: Vec<Option<(Control, std::os::unix::net::UnixStream, shards_ipc::Incoming)>> = controls
        .into_iter()
        .map(|(role, sock)| Some((role, sock, shards_ipc::Incoming::default())))
        .collect();
    let region = Region::map(region)?;
    // The device's frames come on 0, this side's go on 1.
    let mut from_guest: Consumer<'_> = region.consumer(0, wake_peer.try_clone()?, wake_me);
    let to_guest = region.producer(1, wake_peer);
    let mut stack = Stack {
        frames: Frames {
            gateway_mac: cfg.gateway_mac,
            guest_mac: cfg.guest_mac,
        },
        cfg,
        to_guest,
        backlog: VecDeque::new(),
        tcp: Tcp::default(),
        udp: HashMap::new(),
        udp_ids: HashMap::new(),
        next_udp: 0,
        poller: poll::Poller::new()?,
        timers: BinaryHeap::new(),
        blocked: VecDeque::new(),
        buf: vec![0u8; shards_netring::MAX_FRAME],
        scratch: Vec::new(),
        isn_key: {
            let mut key = [0u8; 16];
            entropy(&mut key)?;
            key
        },
        began: Instant::now(),
        published: Vec::new(),
        inbound: HashMap::new(),
        inbound_ports: HashMap::new(),
        next_port: *EPHEMERAL.start(),
        peers: Vec::new(),
        names: None,
        #[cfg(target_os = "linux")]
        ports_confined: false,
    };
    let doorbell = from_guest.waits_on();
    stack.poller.set(
        doorbell,
        token::of(token::DOORBELL, 0, 0),
        poll::Interest::NONE,
        poll::Interest::READ,
    )?;
    for (i, (_, sock, _)) in controls.iter().flatten().enumerate() {
        let named = token::of(token::CONTROL, 0, u32::try_from(i).unwrap_or(u32::MAX));
        stack.poller.set(
            sock.as_raw_fd(),
            named,
            poll::Interest::NONE,
            poll::Interest::READ,
        )?;
    }
    let mut frame = vec![0u8; shards_netring::MAX_FRAME];
    let mut events: Vec<poll::Event> = Vec::new();
    loop {
        // The guest's frames, a batch at a time.
        take_frames(&mut from_guest, &mut stack, &mut frame, 256, usize::MAX)?;
        stack.flush_backlog();
        // What a full ring kept from the guest goes once it has drained: nothing else
        // would send it while the connection waits on no timer and reads no socket.
        if stack.backlog.is_empty() {
            stack.unblock();
        }
        stack.timers(Instant::now());
        // Frames for the guest left in the backlog wait for room without a spin: the ring
        // that had none asked to be rung once the guest's side makes some.
        let busy = from_guest.ready().map_err(|e| io::Error::other(e.to_string()))?;
        // Asked to be rung for the guest's next frame: one that came before the ask rang
        // nothing, so the wait does not wait for it.
        let timeout = if busy || from_guest.arm().map_err(|e| io::Error::other(e.to_string()))? {
            Some(Duration::ZERO)
        } else {
            stack
                .next_timer()
                .map(|t| t.saturating_duration_since(Instant::now()))
        };
        stack.poller.wait(&mut events, timeout)?;
        for e in &events {
            let index = token::index(e.token);
            match token::kind(e.token) {
                token::DOORBELL => {
                    // The guest's device closed the ring's doorbell: the VM is gone, and
                    // what it sent last goes out before this does.
                    if e.ended {
                        let _ = take_frames(&mut from_guest, &mut stack, &mut frame, usize::MAX, RING);
                        return Ok(());
                    }
                    drain(doorbell);
                }
                token::CONTROL => {
                    let Some(entry) = controls.get_mut(index as usize) else {
                        continue;
                    };
                    // The frames the guest sent before this message was, taken before it:
                    // a run's last answer to a published port's peer goes out before the
                    // port closes (UNPUBLISH), its VM having put it in the ring first
                    // (warm.rs `release_ports`).
                    take_frames(&mut from_guest, &mut stack, &mut frame, usize::MAX, RING)?;
                    let keep = entry
                        .as_mut()
                        .is_some_and(|(role, sock, incoming)| control(&mut stack, *role, sock, incoming));
                    // Hung up, or said what was not its: closed, and forgotten. What is
                    // published stays, as the VM runs on.
                    if !keep {
                        *entry = None;
                    }
                }
                token::LISTENER => match stack.published.get(index as usize) {
                    Some((Listener::Tcp(_), _)) => stack.accept(index as usize),
                    Some((Listener::Udp(_), _)) => stack.receive(index as usize),
                    None => {}
                },
                token::TCP => stack.on_socket(e),
                token::PEER => stack.on_peer(index as usize, e),
                token::UDP => stack.on_udp(index),
                _ => {}
            }
        }
        stack.timers(Instant::now());
    }
}

/// Takes what control socket `sock` holds, `role` saying what it may say: published ports
/// come and go as each message comes whole, and each is answered once taken, so that its
/// sender's copies may close. False once it has hung up, or said what is not its to say.
fn control(
    stack: &mut Stack<'_>,
    role: Control,
    sock: &std::os::unix::net::UnixStream,
    incoming: &mut shards_ipc::Incoming,
) -> bool {
    loop {
        let m = match incoming.take(sock) {
            Ok(shards_ipc::Took::Message(m)) => m,
            Ok(shards_ipc::Took::Partial) => return true,
            Ok(shards_ipc::Took::Ended) | Err(_) => return false,
        };
        if !role.says(m.kind) {
            return false;
        }
        let answered = if m.kind == shards_ipc::kind::PUBLISH {
            stack.publish(&m.payload, m.fds);
            shards_ipc::send(sock, shards_ipc::kind::PUBLISH, &[], &[])
        } else if m.kind == shards_ipc::kind::NET_ADDRESS {
            if !stack.readdress(&m.payload) {
                return false;
            }
            shards_ipc::send(sock, shards_ipc::kind::NET_ADDRESS, &[], &[])
        } else if m.kind == shards_ipc::kind::NET_NAMES {
            let Some(names) = dns::Names::decode(&m.payload) else {
                return false;
            };
            stack.names = Some(names);
            shards_ipc::send(sock, shards_ipc::kind::NET_NAMES, &[], &[])
        } else if m.kind == shards_ipc::kind::NET_POLICY {
            let Some((ports, named, dns_all)) = decode_policy(&m.payload) else {
                return false;
            };
            // The process held to the grants' TCP ports (D31), once: a layer narrows what
            // the last left and never widens it, so a second policy is refused, and the
            // daemon, which sends one, lets the VM go. A resolver's port where names may
            // be resolved past the microVM.
            #[cfg(target_os = "linux")]
            {
                if stack.ports_confined {
                    return false;
                }
                let mut tcp: Vec<u16> = ports
                    .0
                    .iter()
                    .filter(|(proto, _, _)| *proto == Proto::Tcp)
                    .flat_map(|&(_, first, last)| first..=last)
                    .chain(named.iter().map(|&(_, port)| port))
                    .collect();
                if dns_all || !named.is_empty() {
                    tcp.push(53);
                }
                if confine::to_ports(&tcp).is_err() {
                    return false;
                }
                stack.ports_confined = true;
            }
            stack.cfg.policy = Policy::Ports(ports);
            stack.cfg.named = named;
            stack.cfg.dns_all = dns_all;
            shards_ipc::send(sock, shards_ipc::kind::NET_POLICY, &[], &[])
        } else if m.kind == shards_ipc::kind::NET_PEER {
            stack.add_peer(&m.payload, m.fds);
            shards_ipc::send(sock, shards_ipc::kind::NET_PEER, &[], &[])
        } else {
            // Flows left answer nobody: their sockets are gone, and the poller forgets
            // them.
            stack.published.clear();
            shards_ipc::send(sock, shards_ipc::kind::UNPUBLISH, &[], &[])
        };
        if answered.is_err() {
            return false;
        }
    }
}

/// Takes the guest's frames from the ring, `frames` of them at most, until it is empty or
/// `bytes` of them are taken: a ring's worth ([`RING`]) is every frame the ring held when
/// asked, however fast the guest refills it.
fn take_frames(
    from_guest: &mut Consumer<'_>,
    stack: &mut Stack<'_>,
    frame: &mut [u8],
    frames: usize,
    bytes: usize,
) -> io::Result<()> {
    let (mut n_frames, mut n_bytes) = (0usize, 0usize);
    while n_frames < frames && n_bytes < bytes {
        let got = from_guest
            .pop(|n, copy| {
                copy(0, frame.as_mut_ptr(), n);
                n
            })
            .map_err(|e| io::Error::other(e.to_string()))?;
        let Some(n) = got else { break };
        stack.on_guest_frame(frame.get(..n).unwrap_or_default());
        n_frames += 1;
        n_bytes = n_bytes.saturating_add(n.max(1));
    }
    Ok(())
}

fn drain(fd: i32) {
    let mut buf = [0u8; 64];
    // SAFETY: a local buffer, from a non-blocking descriptor.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// A gateway port no published UDP flow uses, from the ephemeral range, in turn from
/// `next`.
fn free_udp_port(next: &mut u16, inbound: &HashMap<u16, Inbound>) -> Option<u16> {
    for _ in EPHEMERAL {
        let port = *next;
        *next = port
            .checked_add(1)
            .filter(|p| EPHEMERAL.contains(p))
            .unwrap_or(*EPHEMERAL.start());
        if !inbound.contains_key(&port) {
            return Some(port);
        }
    }
    None
}

/// [`Stack::isn`] of connection `conn` of `guest`, under `secret`, `ticks` 64 ns ticks on.
fn initial_seq(secret: &[u8; 16], ticks: u64, guest: IpAddr, conn: &Key) -> u32 {
    let octets = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let mut m = octets(guest);
    m.extend(octets(conn.remote.0));
    m.extend_from_slice(&conn.guest_port.to_be_bytes());
    m.extend_from_slice(&conn.remote.1.to_be_bytes());
    let hash = siphash::siphash24(secret, &m);
    (hash as u32).wrapping_add(ticks as u32)
}

impl<'r> Stack<'r> {
    /// The initial sequence number of the connection `key` names, as RFC 6528 makes one
    /// and Linux does (net/core/secure_seq.c, `secure_tcp_seq` and `seq_scale`): SipHash-2-4
    /// of its addresses and ports, keyed by this process's secret, plus a clock of 64 ns
    /// ticks. No one who sees one connection's can predict another's (review 2.31): a
    /// workload with raw sockets could otherwise put its own segments into the connections
    /// of the other processes of its guest.
    fn isn(&self, key: &Key) -> u32 {
        let ticks = u64::try_from(self.began.elapsed().as_nanos() >> 6).unwrap_or(u64::MAX);
        let guest = match (key.remote.0, self.cfg.guest_ip6) {
            (IpAddr::V6(_), Some(g)) => IpAddr::V6(g),
            _ => IpAddr::V4(self.cfg.guest_ip),
        };
        initial_seq(&self.isn_key, ticks, guest, key)
    }

    /// Takes published ports' host sockets, each for the guest port and protocol its
    /// payload's next three bytes name (a big-endian u16, then the IP protocol number);
    /// what does not pair up, or names another protocol, is closed.
    fn publish(&mut self, ports: &[u8], fds: Vec<OwnedFd>) {
        for (fd, &[hi, lo, proto]) in fds.into_iter().zip(ports.as_chunks::<3>().0) {
            let listener = match proto {
                wire::PROTO_TCP => Listener::Tcp(std::net::TcpListener::from(fd)),
                wire::PROTO_UDP => Listener::Udp(UdpSocket::from(fd)),
                _ => continue,
            };
            let nonblocking = match &listener {
                Listener::Tcp(l) => l.set_nonblocking(true),
                Listener::Udp(u) => u.set_nonblocking(true),
            };
            let named = token::of(
                token::LISTENER,
                0,
                u32::try_from(self.published.len()).unwrap_or(u32::MAX),
            );
            if nonblocking.is_ok()
                && self
                    .poller
                    .set(
                        listener.as_raw_fd(),
                        named,
                        poll::Interest::NONE,
                        poll::Interest::READ,
                    )
                    .is_ok()
            {
                self.published.push((listener, u16::from_be_bytes([hi, lo])));
            }
        }
    }

    /// Takes what published UDP port `i` holds: each datagram to the guest's port, from
    /// its peer's gateway port.
    fn receive(&mut self, i: usize) {
        let Some((Listener::Udp(sock), guest_port)) = self.published.get(i) else {
            return;
        };
        let guest_port = *guest_port;
        for _ in 0..BUDGET {
            let Ok((n, peer, asked)) = pktinfo::recv(sock, &mut self.buf) else {
                return;
            };
            let port = match self.inbound_ports.get(&(i, peer)) {
                Some(&port) => port,
                None => {
                    // Every port in use: dropped, as a full table drops.
                    let Some(port) = free_udp_port(&mut self.next_port, &self.inbound) else {
                        continue;
                    };
                    self.inbound_ports.insert((i, peer), port);
                    port
                }
            };
            let now = Instant::now();
            match self.inbound.entry(port) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let f = e.get_mut();
                    f.asked = asked;
                    f.life.datagram(now, false);
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    let life = Lifetime::new(now);
                    v.insert(Inbound {
                        published: i,
                        peer,
                        asked,
                        guest_port,
                        life,
                    });
                    self.timers.push(Reverse((
                        life.until,
                        token::of(token::INBOUND, 0, u32::from(port)),
                    )));
                }
            }
            let mut o = Out {
                frames: &self.frames,
                to_guest: &mut self.to_guest,
                backlog: &mut self.backlog,
                guest_ip: self.cfg.guest_ip,
                guest_ip6: self.cfg.guest_ip6,
                scratch: &mut self.scratch,
            };
            o.datagram(
                (IpAddr::V4(self.cfg.gateway_ip), port),
                guest_port,
                self.buf.get(..n).unwrap_or_default(),
            );
        }
    }

    /// Accepts what published port `i` holds: each connection opened to the guest's port,
    /// from the gateway, as a userland proxy's connection comes from it.
    fn accept(&mut self, i: usize) {
        for _ in 0..BUDGET {
            let Some((Listener::Tcp(listener), guest_port)) = self.published.get(i) else {
                return;
            };
            let guest_port = *guest_port;
            let Ok((sock, _)) = listener.accept() else { return };
            // Nagle's on this leg too would hold what the guest's own TCP already chose
            // to send, as Go's net, docker-proxy's, never does.
            if sock
                .set_nonblocking(true)
                .and_then(|()| sock.set_nodelay(true))
                .is_err()
            {
                continue;
            }
            let Some(port) = self.free_port(guest_port) else {
                // Every port of the range in use: refused, as a full table refuses.
                continue;
            };
            let key = Key {
                guest_port,
                remote: (IpAddr::V4(self.cfg.gateway_ip), port),
            };
            let isn = self.isn(&key);
            let c = {
                let mut o = self.out();
                Conn::accept(key, sock, isn, &mut o)
            };
            let slot = self.tcp.reserve(key);
            self.settle(slot, c);
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
                remote: (IpAddr::V4(self.cfg.gateway_ip), port),
            };
            if !self.tcp.by_key.contains_key(&key) {
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
            guest_ip6: self.cfg.guest_ip6,
            scratch: &mut self.scratch,
        }
    }

    fn flush_backlog(&mut self) {
        while let Some(f) = self.backlog.front() {
            match self.to_guest.try_push(&[f]) {
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
                    self.out().built(|frames, f| frames.arp_reply(f, &req));
                }
            }
            wire::ETHERTYPE_IPV4 => {
                let Some(ip) = wire::ipv4(e.payload) else { return };
                self.on_guest_ip(f, &ip);
            }
            wire::ETHERTYPE_IPV6 => {
                // No IPv6 on the guest's network: its frames (link-local chatter, router
                // and multicast listener messages) reach nothing.
                let Some(guest6) = self.cfg.guest_ip6 else { return };
                let Some(ip) = wire::ipv6(e.payload) else { return };
                // Neighbor discovery for any address but its own: the gateway's MAC, as
                // ARP is answered. The guest may ask from its link-local address (Linux
                // solicits from the address the waiting packet has, else that one); the
                // frame is its own, by its MAC, either way.
                if let Some(ns) = wire::neighbor_solicit(&ip) {
                    if ns.target != guest6 {
                        self.out().built(|frames, f| frames.neighbor_advert(f, &ns));
                    }
                    return;
                }
                self.on_guest_ip(f, &ip);
            }
            _ => {}
        }
    }

    /// The guest's IP packet `ip`, of either version, in frame `f`: to a peer, or to the
    /// stack, only from the guest's own address of its version.
    fn on_guest_ip(&mut self, f: &[u8], ip: &wire::Ip<'_>) {
        let own = match ip.src {
            IpAddr::V4(src) => src == self.cfg.guest_ip,
            IpAddr::V6(src) => Some(src) == self.cfg.guest_ip6,
        };
        if !own {
            return;
        }
        // A peer's, on the guest's network: to its VM whole, past the policy, as a
        // bridge's members reach one another.
        if let Some(i) = self.peers.iter().position(|p| {
            p.as_ref().is_some_and(|p| match ip.dst {
                IpAddr::V4(dst) => p.ip == dst,
                IpAddr::V6(dst) => p.ip6 == Some(dst),
            })
        }) {
            self.forward_to_peer(i, f);
            return;
        }
        match ip.proto {
            wire::PROTO_ICMP | wire::PROTO_ICMPV6 => self.on_icmp(ip),
            wire::PROTO_UDP => self.on_guest_udp(ip),
            wire::PROTO_TCP => self.on_guest_tcp(ip),
            _ => {}
        }
    }

    /// The guest's address on its network, its prefix and gateway (`NET_ADDRESS`): what
    /// its frames come from, and the gateway they go through. False for a malformed one.
    /// Then, on a network with IPv6, its IPv6 address, prefix and gateway (16, 1 and 16
    /// bytes).
    fn readdress(&mut self, payload: &[u8]) -> bool {
        let Some((&[a, b, c, d, _prefix, g0, g1, g2, g3], rest)) = payload.split_first_chunk::<9>() else {
            return false;
        };
        let v6 = match rest.len() {
            0 => None,
            33 => {
                let (Some(guest), Some(gateway)) = (
                    rest.get(..16).and_then(|o| <[u8; 16]>::try_from(o).ok()),
                    rest.get(17..33).and_then(|o| <[u8; 16]>::try_from(o).ok()),
                ) else {
                    return false;
                };
                Some((Ipv6Addr::from(guest), Ipv6Addr::from(gateway)))
            }
            _ => return false,
        };
        self.cfg.guest_ip = Ipv4Addr::new(a, b, c, d);
        self.cfg.gateway_ip = Ipv4Addr::new(g0, g1, g2, g3);
        self.cfg.gateway_mac = [0x02, 0x42, g0, g1, g2, g3];
        self.cfg.guest_ip6 = v6.map(|(guest, _)| guest);
        self.cfg.gateway_ip6 = v6.map(|(_, gateway)| gateway);
        self.frames.gateway_mac = self.cfg.gateway_mac;
        true
    }

    /// A peer (`NET_PEER`): its guest's address, then its IPv6 one on a network with
    /// IPv6, and the socket to its network process.
    fn add_peer(&mut self, payload: &[u8], fds: Vec<OwnedFd>) {
        let (Some((&[a, b, c, d], rest)), Some(fd)) =
            (payload.split_first_chunk::<4>(), fds.into_iter().next())
        else {
            return;
        };
        let ip6 = match rest.len() {
            0 => None,
            16 => rest.first_chunk::<16>().map(|o| Ipv6Addr::from(*o)),
            _ => return,
        };
        let sock = std::os::unix::net::UnixStream::from(fd);
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        let i = self
            .peers
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.peers.len());
        let token = token::of(token::PEER, 0, u32::try_from(i).unwrap_or(u32::MAX));
        if self
            .poller
            .set(
                sock.as_raw_fd(),
                token,
                poll::Interest::NONE,
                poll::Interest::READ,
            )
            .is_err()
        {
            return;
        }
        let peer = Peer {
            ip: Ipv4Addr::new(a, b, c, d),
            ip6,
            sock,
            unsent: Vec::new(),
            received: Vec::new(),
        };
        match self.peers.get_mut(i) {
            Some(slot) => *slot = Some(peer),
            None => self.peers.push(Some(peer)),
        }
    }

    /// Sends the guest's frame `f` to peer `i`: its virtio header's segmentation cleared,
    /// as the frame fits the peer's MTU whole (the guest's TCP segments a 65520-byte MTU's
    /// way), its checksum's partial state kept, which the peer's device takes
    /// (VIRTIO_NET_F_GUEST_CSUM).
    fn forward_to_peer(&mut self, i: usize, f: &[u8]) {
        let Some(Some(peer)) = self.peers.get_mut(i) else {
            return;
        };
        if !peer.unsent.is_empty() {
            return;
        }
        let Ok(len) = u32::try_from(f.len()) else { return };
        let mut framed = Vec::with_capacity(4 + f.len());
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(f);
        // gso_type, hdr_len and gso_size: bytes 1 to 5 of the header.
        if let Some(gso) = framed.get_mut(5..10) {
            gso.fill(0);
        }
        use std::io::Write as _;
        match (&peer.sock).write(&framed) {
            Ok(n) if n == framed.len() => {}
            Ok(n) => {
                peer.unsent = framed.split_off(n);
                let token = token::of(token::PEER, 0, u32::try_from(i).unwrap_or(u32::MAX));
                let both = poll::Interest {
                    read: true,
                    write: true,
                };
                let _ = self
                    .poller
                    .set(peer.sock.as_raw_fd(), token, poll::Interest::READ, both);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.drop_peer(i),
        }
    }

    /// Peer `i`'s socket is ready: what is unsent goes on, and its frames come to the
    /// guest, each only from its own address to the guest's, from the gateway's MAC as a
    /// routed one would.
    fn on_peer(&mut self, i: usize, e: &poll::Event) {
        use std::io::{Read as _, Write as _};
        let token = token::of(token::PEER, 0, u32::try_from(i).unwrap_or(u32::MAX));
        let mut gone = e.ended;
        let mut frames = Vec::new();
        {
            let Some(Some(peer)) = self.peers.get_mut(i) else {
                return;
            };
            if e.write && !peer.unsent.is_empty() {
                match (&peer.sock).write(&peer.unsent) {
                    Ok(n) => {
                        peer.unsent.drain(..n);
                        if peer.unsent.is_empty() {
                            let both = poll::Interest {
                                read: true,
                                write: true,
                            };
                            let _ = self
                                .poller
                                .set(peer.sock.as_raw_fd(), token, both, poll::Interest::READ);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => gone = true,
                }
            }
            let mut buf = [0u8; 64 * 1024];
            for _ in 0..BUDGET {
                match (&peer.sock).read(&mut buf) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => peer.received.extend_from_slice(buf.get(..n).unwrap_or_default()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        gone = true;
                        break;
                    }
                }
            }
            while let Some(&len) = peer.received.first_chunk::<4>() {
                let len = u32::from_be_bytes(len) as usize;
                if len > shards_netring::MAX_FRAME {
                    gone = true;
                    break;
                }
                if peer.received.len() < 4 + len {
                    break;
                }
                let frame: Vec<u8> = peer.received.drain(..4 + len).skip(4).collect();
                frames.push((peer.ip, peer.ip6, frame));
            }
        }
        for (from, from6, mut frame) in frames {
            let Some(eth) = frame.get(wire::VNET..).and_then(wire::eth) else {
                continue;
            };
            let ip = match eth.kind {
                wire::ETHERTYPE_IPV4 => wire::ipv4(eth.payload),
                wire::ETHERTYPE_IPV6 => wire::ipv6(eth.payload),
                _ => None,
            };
            let Some(ip) = ip else { continue };
            let theirs_to_ours = match (ip.src, ip.dst) {
                (IpAddr::V4(src), IpAddr::V4(dst)) => src == from && dst == self.cfg.guest_ip,
                (IpAddr::V6(src), IpAddr::V6(dst)) => from6 == Some(src) && self.cfg.guest_ip6 == Some(dst),
                _ => false,
            };
            if !theirs_to_ours {
                continue;
            }
            if let Some(macs) = frame.get_mut(wire::VNET..wire::VNET + 12)
                && let (dst, src) = macs.split_at_mut(6)
            {
                dst.copy_from_slice(&self.cfg.guest_mac);
                src.copy_from_slice(&self.cfg.gateway_mac);
            }
            self.out().send(&[&frame]);
        }
        if gone {
            self.drop_peer(i);
        }
    }

    /// Forgets peer `i`, whose VM has gone.
    fn drop_peer(&mut self, i: usize) {
        if let Some(slot) = self.peers.get_mut(i)
            && let Some(peer) = slot.take()
        {
            let token = token::of(token::PEER, 0, u32::try_from(i).unwrap_or(u32::MAX));
            let was = if peer.unsent.is_empty() {
                poll::Interest::READ
            } else {
                poll::Interest {
                    read: true,
                    write: true,
                }
            };
            let _ = self
                .poller
                .set(peer.sock.as_raw_fd(), token, was, poll::Interest::NONE);
        }
    }

    /// An echo request to the gateway, ICMP's or ICMPv6's, is answered here; others are not
    /// reached yet.
    fn on_icmp(&mut self, ip: &wire::Ip<'_>) {
        let p = ip.payload;
        let request = if ip.proto == wire::PROTO_ICMPV6 {
            wire::ICMPV6_ECHO_REQUEST
        } else {
            8
        };
        if p.first() == Some(&request) && ip.dst == self.cfg.gateway_of(ip.src) {
            let data = p.get(4..).unwrap_or_default();
            self.out()
                .built(|frames, f| frames.icmp_echo_reply(f, ip.dst, ip.src, data));
        }
    }

    fn on_guest_udp(&mut self, ip: &wire::Ip<'_>) {
        let Some(u) = wire::udp(ip.payload) else { return };
        let gateway = self.cfg.gateway_of(ip.src);
        // An answer to a published port's peer, through the gateway port it was given.
        if ip.dst == gateway {
            // The network's resolver, as Docker's embedded DNS (the guest's 127.0.0.11
            // relays to it).
            if u.dst_port == 53
                && let Some(answer) = self.names.as_ref().and_then(|n| n.answer(u.payload))
            {
                self.out().datagram((gateway, 53), u.src_port, &answer);
                return;
            }
            // A name past the microVM, for agents granted egress (D59): to the host's
            // resolver, as a flow keyed to the gateway, so that its answer comes back from
            // it.
            if u.dst_port == 53
                && matches!(self.cfg.policy, Policy::Ports(_))
                && let Some(&up) = self.cfg.resolvers.first()
            {
                // Only what a grant names: any name with `--dns`, else a remote MCP
                // server's own (default deny).
                let asked = dns::query_name(u.payload);
                let granted = self.cfg.dns_all
                    || asked
                        .as_deref()
                        .is_some_and(|q| self.cfg.named.iter().any(|(h, _)| h.eq_ignore_ascii_case(q)));
                if granted {
                    self.flow((u.src_port, ip.dst, 53), (IpAddr::V4(up.0), up.1), u.payload);
                } else if let Some(no) = dns::refused(u.payload) {
                    self.out().datagram((gateway, 53), u.src_port, &no);
                }
                return;
            }
            if let Some(f) = self.inbound.get_mut(&u.dst_port)
                && gateway.is_ipv4()
                && f.guest_port == u.src_port
                && let Some((Listener::Udp(sock), _)) = self.published.get(f.published)
            {
                let _ = pktinfo::send(sock, u.payload, f.peer, f.asked);
                f.life.datagram(Instant::now(), true);
            }
            return;
        }
        // Refused, said at once (review 2.27): a connected socket's next call fails
        // (Linux: EHOSTUNREACH, net/ipv4/icmp.c icmp_err_convert), where a datagram
        // dropped would leave a resolver to wait out its timeouts.
        if !self.cfg.allows(ip.dst, Proto::Udp, u.dst_port) {
            self.out().built(|frames, f| {
                frames.icmp_unreachable(f, gateway, ip.src, ip);
            });
            return;
        }
        self.flow((u.src_port, ip.dst, u.dst_port), (ip.dst, u.dst_port), u.payload);
    }

    /// Sends `payload` on UDP flow `key`, its host socket connected to `to`, made where
    /// there is none; its answers go to the guest from `key`'s remote end.
    fn flow(&mut self, key: UdpKey, to: (IpAddr, u16), payload: &[u8]) {
        let f = match self.udp.entry(key) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let any = match to.0 {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                };
                let Ok(sock) = UdpSocket::bind((any, 0)) else {
                    return;
                };
                if sock.connect(to).is_err() || sock.set_nonblocking(true).is_err() {
                    return;
                }
                // Its answers waited for from now on, and its end timed.
                let id = self.next_udp;
                let named = token::of(token::UDP, 0, id);
                if self
                    .poller
                    .set(
                        sock.as_raw_fd(),
                        named,
                        poll::Interest::NONE,
                        poll::Interest::READ,
                    )
                    .is_err()
                {
                    return;
                }
                self.next_udp = id.wrapping_add(1);
                self.udp_ids.insert(id, key);
                let life = Lifetime::new(Instant::now());
                self.timers.push(Reverse((life.until, named)));
                v.insert(UdpFlow { sock, life })
            }
        };
        let _ = f.sock.send(payload);
        f.life.datagram(Instant::now(), false);
    }

    /// What UDP flow `id`'s host socket holds, to the guest.
    fn on_udp(&mut self, id: u32) {
        let Some(key) = self.udp_ids.get(&id).copied() else {
            return;
        };
        let Some(f) = self.udp.get_mut(&key) else { return };
        let mut o = Out {
            frames: &self.frames,
            to_guest: &mut self.to_guest,
            backlog: &mut self.backlog,
            guest_ip: self.cfg.guest_ip,
            guest_ip6: self.cfg.guest_ip6,
            scratch: &mut self.scratch,
        };
        for _ in 0..BUDGET {
            let Ok(n) = f.sock.recv(&mut self.buf) else {
                return;
            };
            f.life.datagram(Instant::now(), true);
            let got = self.buf.get(..n).unwrap_or_default();
            // A forwarded answer for a name granted to agents: what it resolved to, each
            // address reached at its grant's port from now on.
            if key.1 == IpAddr::V4(self.cfg.gateway_ip)
                && key.2 == 53
                && !self.cfg.named.is_empty()
                && let Some((name, addrs)) = dns::a_records(got)
            {
                for (host, port) in &self.cfg.named {
                    if host.eq_ignore_ascii_case(&name) {
                        self.cfg.learned.extend(addrs.iter().map(|a| (*a, *port)));
                    }
                }
            }
            o.datagram((key.1, key.2), key.0, got);
        }
    }

    fn on_guest_tcp(&mut self, ip: &wire::Ip<'_>) {
        let Some(seg) = wire::tcp(ip.payload) else { return };
        let key = Key {
            guest_port: seg.src_port,
            remote: (ip.dst, seg.dst_port),
        };
        let existing = self.tcp.by_key.get(&key).copied();
        let (slot, mut c) = if let Some(slot) = existing {
            let Some(c) = self.tcp.take(slot) else { return };
            (slot, c)
        } else {
            if seg.flags & wire::SYN == 0 || seg.flags & wire::ACK != 0 {
                // Not a connection's start, and no connection: a reset, as a host with no
                // such connection answers (RFC 9293 §3.10.7.1).
                if seg.flags & wire::RST == 0 {
                    // An ACK's reset takes its sequence number from it, and acknowledges
                    // nothing; else it starts at 0 and acknowledges the segment.
                    let mut o = self.out();
                    if seg.flags & wire::ACK != 0 {
                        o.segment(&key, seg.ack, 0, wire::RST, 0, None, tcp::EMPTY);
                    } else {
                        let ack = seg
                            .seq
                            .wrapping_add(seg.payload.len() as u32)
                            .wrapping_add(u32::from(seg.flags & (wire::SYN | wire::FIN) != 0));
                        o.segment(&key, 0, ack, wire::RST | wire::ACK, 0, None, tcp::EMPTY);
                    }
                }
                return;
            }
            // DNS over TCP past the microVM, for agents granted egress (D59), as by UDP: to
            // the host's resolver, each question as its grants say.
            let resolver = (ip.dst == IpAddr::V4(self.cfg.gateway_ip)
                && seg.dst_port == 53
                && matches!(self.cfg.policy, Policy::Ports(_)))
            .then(|| self.cfg.resolvers.first().map(|&(a, p)| (IpAddr::V4(a), p)))
            .flatten();
            let proxy = self.cfg.proxy_flow(ip.dst, seg.dst_port);
            if resolver.is_none() && proxy.is_none() && !self.cfg.allows(ip.dst, Proto::Tcp, seg.dst_port) {
                let mut o = self.out();
                o.segment(
                    &key,
                    0,
                    seg.seq.wrapping_add(1),
                    wire::RST | wire::ACK,
                    0,
                    None,
                    tcp::EMPTY,
                );
                return;
            }
            let opened = match resolver {
                Some(to) => Conn::open_to(key, to, &seg, self.isn(&key)).map(|mut c| {
                    let names = self
                        .cfg
                        .named
                        .iter()
                        .map(|(h, _)| h.to_ascii_lowercase())
                        .collect();
                    c.dns = Some(dns::Stream::new(self.cfg.dns_all, names));
                    c
                }),
                None => match proxy {
                    Some(socket) => Conn::open_unix(key, &socket, &seg, self.isn(&key)),
                    None => Conn::open(key, &seg, self.isn(&key)),
                },
            };
            match opened {
                Ok(c) => (self.tcp.reserve(key), c),
                Err(_) => {
                    let mut o = self.out();
                    o.segment(
                        &key,
                        0,
                        seg.seq.wrapping_add(1),
                        wire::RST | wire::ACK,
                        0,
                        None,
                        tcp::EMPTY,
                    );
                    return;
                }
            }
        };
        {
            let mut o = self.out();
            c.on_segment(&seg, &mut o);
        }
        self.settle(slot, c);
    }

    /// Puts connection `c` back in `slot` once it has been served: its socket waited on
    /// for what it now wants, its timer armed for its deadline unless an earlier one is,
    /// and its turn kept if a full ring held it back; or its slot emptied if it has
    /// closed, its socket with it, which the poller forgets (review 2.14).
    fn settle(&mut self, slot: u32, mut c: Conn) {
        if c.closed {
            self.tcp.release(slot);
            return;
        }
        let named = self.tcp.token(slot);
        let wants = poll::Interest {
            read: c.wants_read(),
            write: c.wants_write(),
        };
        let (deadline, blocked, fd) = (c.deadline(), c.blocked(), c.fd());
        let Some(had) = self.tcp.slot(slot).map(|s| s.interest) else {
            return;
        };
        if self.poller.set(fd, named, had, wants).is_err() {
            // A socket that cannot be waited on would never be served: the guest hears
            // a reset, as from a peer gone.
            let mut o = self.out();
            c.reset(&mut o);
            self.tcp.release(slot);
            return;
        }
        let Some(s) = self.tcp.slot(slot) else { return };
        s.interest = wants;
        if let Some(due) = deadline
            && s.armed.is_none_or(|armed| due < armed)
        {
            s.armed = Some(due);
            self.timers.push(Reverse((due, named)));
        }
        if blocked && !s.blocked {
            s.blocked = true;
            self.blocked.push_back(named);
        }
        s.conn = Some(c);
    }

    /// Connections a full ring held back, sent again in the order they were held, until
    /// the ring is full again: the rest keep their turn.
    fn unblock(&mut self) {
        while let Some(named) = self.blocked.pop_front() {
            let Some(slot) = self.tcp.slot_of(named) else {
                continue;
            };
            if let Some(s) = self.tcp.slot(slot) {
                s.blocked = false;
            }
            let Some(mut c) = self.tcp.take(slot) else {
                continue;
            };
            {
                let mut o = self.out();
                c.unblock(&mut o);
            }
            let again = !c.closed && c.blocked();
            self.settle(slot, c);
            if again {
                // Its turn first next time: it went to the back as it settled.
                if self.blocked.back() == Some(&named) {
                    self.blocked.pop_back();
                    self.blocked.push_front(named);
                }
                return;
            }
        }
    }

    /// A connection's host socket ready, as the poller says it.
    fn on_socket(&mut self, e: &poll::Event) {
        let Some(slot) = self.tcp.slot_of(e.token) else {
            return;
        };
        let Some(mut c) = self.tcp.take(slot) else { return };
        let mut buf = std::mem::take(&mut self.buf);
        {
            let mut o = Out {
                frames: &self.frames,
                to_guest: &mut self.to_guest,
                backlog: &mut self.backlog,
                guest_ip: self.cfg.guest_ip,
                guest_ip6: self.cfg.guest_ip6,
                scratch: &mut self.scratch,
            };
            if e.write {
                c.on_writable(&mut o);
            }
            if !c.closed && e.read {
                c.on_readable(&mut o, &mut buf);
            }
        }
        self.buf = buf;
        // What answers of the guest's DNS over TCP resolved granted names to.
        if let Some(d) = c.dns.as_mut() {
            for (name, addrs) in d.learned.drain(..) {
                for (host, port) in &self.cfg.named {
                    if host.eq_ignore_ascii_case(&name) {
                        self.cfg.learned.extend(addrs.iter().map(|a| (*a, *port)));
                    }
                }
            }
        }
        self.settle(slot, c);
    }

    /// Runs the timers due by `now`, earliest first: a connection's retransmission or
    /// probe, a UDP flow's end. One a later timer took the place of, or whose flow lives
    /// on, does nothing, or is armed again for when it ends (review 2.14).
    fn timers(&mut self, now: Instant) {
        while let Some(&Reverse((at, named))) = self.timers.peek() {
            if at > now {
                break;
            }
            self.timers.pop();
            match token::kind(named) {
                token::TCP => {
                    let Some(slot) = self.tcp.slot_of(named) else {
                        continue;
                    };
                    match self.tcp.slot(slot) {
                        Some(s) if s.armed == Some(at) => s.armed = None,
                        _ => continue,
                    }
                    let Some(mut c) = self.tcp.take(slot) else {
                        continue;
                    };
                    {
                        let mut o = self.out();
                        c.on_timer(now, &mut o);
                    }
                    self.settle(slot, c);
                }
                token::UDP => {
                    let id = token::index(named);
                    let Some(key) = self.udp_ids.get(&id).copied() else {
                        continue;
                    };
                    match self.udp.get(&key) {
                        Some(f) if now < f.life.until => {
                            self.timers.push(Reverse((f.life.until, named)));
                        }
                        _ => {
                            // Its socket closes with it, and the poller forgets it.
                            self.udp.remove(&key);
                            self.udp_ids.remove(&id);
                        }
                    }
                }
                token::INBOUND => {
                    let Ok(port) = u16::try_from(token::index(named)) else {
                        continue;
                    };
                    match self.inbound.get(&port) {
                        Some(f) if now < f.life.until => {
                            self.timers.push(Reverse((f.life.until, named)));
                        }
                        Some(_) => {
                            if let Some(f) = self.inbound.remove(&port) {
                                self.inbound_ports.remove(&(f.published, f.peer));
                            }
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// The earliest timer's time.
    fn next_timer(&self) -> Option<Instant> {
        self.timers.peek().map(|Reverse((at, _))| *at)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A slot's token names its connection alone: once the slot is emptied, and taken by
    /// another, what was waited on or timed for the first reaches nothing (review 2.14).
    #[test]
    fn a_slots_token_is_its_connections_alone() {
        let mut tcp = Tcp::default();
        let key = |port| Key {
            guest_port: port,
            remote: (IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
        };
        let a = tcp.reserve(key(1));
        let first = tcp.token(a);
        assert_eq!(tcp.slot_of(first), Some(a));
        assert_eq!(token::kind(first), token::TCP);
        tcp.release(a);
        assert_eq!(tcp.slot_of(first), None);
        assert!(!tcp.by_key.contains_key(&key(1)));
        let b = tcp.reserve(key(2));
        assert_eq!(b, a, "the slot is taken again");
        assert_eq!(
            tcp.slot_of(first),
            None,
            "the first's events are not the second's"
        );
        assert_eq!(tcp.slot_of(tcp.token(b)), Some(b));
        for (kind, generation, index) in [(token::UDP, 0, u32::MAX), (token::INBOUND, 0xff_ffff, 7)] {
            let named = token::of(kind, generation, index);
            assert_eq!(
                (token::kind(named), token::generation(named), token::index(named)),
                (kind, generation, index)
            );
        }
    }

    /// A connection's timer is armed once for its deadline, however often it settles,
    /// and armed again when it fires: the timers hold a connection once, not once a
    /// change (review 2.14).
    #[test]
    fn a_connections_timer_is_armed_once() {
        let region = Region::map(shards_netring::memory().unwrap()).unwrap();
        let (_, rings) = shards_netring::doorbell().unwrap();
        let bridge = bridge::Bridge::elect(&[]).unwrap();
        let cfg = Config::on_bridge(Policy::DenyAll, [2, 0, 0, 0, 0, 1], &bridge);
        let mut stack = Stack {
            frames: Frames {
                gateway_mac: cfg.gateway_mac,
                guest_mac: cfg.guest_mac,
            },
            cfg,
            to_guest: region.producer(1, rings),
            backlog: VecDeque::new(),
            tcp: Tcp::default(),
            udp: HashMap::new(),
            udp_ids: HashMap::new(),
            next_udp: 0,
            poller: poll::Poller::new().unwrap(),
            timers: BinaryHeap::new(),
            blocked: VecDeque::new(),
            buf: vec![0u8; 2048],
            scratch: Vec::new(),
            isn_key: [0; 16],
            began: Instant::now(),
            published: Vec::new(),
            inbound: HashMap::new(),
            inbound_ports: HashMap::new(),
            next_port: *EPHEMERAL.start(),
            peers: Vec::new(),
            names: None,
            #[cfg(target_os = "linux")]
            ports_confined: false,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (sock, _) = listener.accept().unwrap();
        sock.set_nonblocking(true).unwrap();
        let key = Key {
            guest_port: 80,
            remote: (IpAddr::V4(stack.cfg.gateway_ip), 49152),
        };
        // Opened to the guest: its SYN sent, a retransmission due.
        let c = Conn::accept(key, sock, 1000, &mut stack.out());
        let slot = stack.tcp.reserve(key);
        stack.settle(slot, c);
        assert_eq!(stack.timers.len(), 1);
        for _ in 0..3 {
            let c = stack.tcp.take(slot).unwrap();
            stack.settle(slot, c);
        }
        assert_eq!(stack.timers.len(), 1, "armed once");
        let due = stack.next_timer().unwrap();
        stack.timers(due);
        assert_eq!(stack.timers.len(), 1, "fired, and armed again");
        assert!(stack.next_timer().unwrap() > due);
    }

    /// A segment's headers and its bytes, from both halves of a queue that wraps, reach the
    /// ring as one frame, as does a datagram; while frames wait in the backlog, a segment
    /// of bytes waits for room, and a bare one queues behind them.
    #[test]
    fn frames_reach_the_ring_whole() {
        let region = Region::map(shards_netring::memory().unwrap()).unwrap();
        let (cw, pr) = shards_netring::doorbell().unwrap();
        let (_, cr) = shards_netring::doorbell().unwrap();
        let mut producer = region.producer(1, pr);
        let mut consumer = region.consumer(1, cr, cw);
        let mut take = || {
            consumer
                .pop(|n, copy| {
                    let mut f = vec![0u8; n];
                    copy(0, f.as_mut_ptr(), n);
                    f
                })
                .unwrap()
        };
        let frames = Frames {
            gateway_mac: [2, 0, 0, 0, 0, 1],
            guest_mac: [2, 0x42, 0xac, 0x11, 0, 2],
        };
        let (mut backlog, mut scratch) = (VecDeque::new(), Vec::new());
        let guest_ip = Ipv4Addr::new(172, 17, 0, 2);
        let mut out = Out {
            frames: &frames,
            to_guest: &mut producer,
            backlog: &mut backlog,
            guest_ip,
            guest_ip6: None,
            scratch: &mut scratch,
        };
        let key = Key {
            guest_port: 80,
            remote: (IpAddr::from([1, 2, 3, 4]), 40_000),
        };
        assert!(out.segment(&key, 7, 9, wire::ACK, 100, None, [b"hello, ", b"world"]));
        assert!(out.datagram((IpAddr::from([8, 8, 8, 8]), 53), 5353, b"an answer"));
        out.backlog.push_back(vec![0; 64]);
        assert!(!out.segment(&key, 19, 9, wire::ACK, 100, None, [b"later", b""]));
        assert!(out.segment(&key, 19, 9, wire::ACK, 100, None, tcp::EMPTY));
        assert_eq!(out.backlog.len(), 2);
        let segment = take().unwrap();
        let ip = wire::ipv4(wire::eth(segment.get(wire::VNET..).unwrap()).unwrap().payload).unwrap();
        assert_eq!((ip.src, ip.dst), (key.remote.0, IpAddr::V4(guest_ip)));
        let t = wire::tcp(ip.payload).unwrap();
        assert_eq!((t.seq, t.ack, t.payload), (7, 9, &b"hello, world"[..]));
        let datagram = take().unwrap();
        let ip = wire::ipv4(wire::eth(datagram.get(wire::VNET..).unwrap()).unwrap().payload).unwrap();
        let u = wire::udp(ip.payload).unwrap();
        assert_eq!((u.src_port, u.dst_port, u.payload), (53, 5353, &b"an answer"[..]));
        assert_eq!(take(), None);
    }

    /// A MAC goes as text and comes back the same; what is not six two-digit hex bytes
    /// between colons is no MAC.
    #[test]
    fn macs_read_back_as_written() {
        let mac = Mac([0x02, 0x42, 0xac, 0x11, 0x00, 0xff]);
        assert_eq!(mac.to_string(), "02:42:ac:11:00:ff");
        assert_eq!("02:42:ac:11:00:ff".parse(), Ok(mac));
        assert_eq!("02:42:AC:11:00:FF".parse(), Ok(mac));
        for not in [
            "",
            "02:42:ac:11:00",
            "02:42:ac:11:00:ff:01",
            "2:42:ac:11:00:ff",
            "+2:42:ac:11:00:ff",
            "02:42:ac:11:00:fg",
            "02-42-ac-11-00-ff",
            "02:42:ac:11:00:ff:",
        ] {
            assert!(not.parse::<Mac>().is_err(), "{not:?}");
        }
    }

    /// What a build's steps reach: the Internet and the host's networks, not the host
    /// itself, a cloud's instance metadata, multicast or broadcast; a run's, by default,
    /// nothing.
    #[test]
    fn policies_reach_what_they_say() {
        let bridge = bridge::Bridge::elect(&[]).unwrap();
        let allow = Config::on_bridge(Policy::AllowAll, [2, 0, 0, 0, 0, 1], &bridge);
        for to in [[8, 8, 8, 8], [192, 168, 1, 10], [10, 0, 0, 1], [172, 17, 0, 3]] {
            assert!(allow.allows(IpAddr::from(to), Proto::Tcp, 443), "{to:?}");
        }
        for to in [
            [172, 17, 0, 1],
            [127, 0, 0, 1],
            [0, 0, 0, 0],
            [169, 254, 169, 254],
            [169, 254, 0, 1],
            [224, 0, 0, 251],
            [239, 255, 255, 250],
            [255, 255, 255, 255],
        ] {
            assert!(!allow.allows(IpAddr::from(to), Proto::Tcp, 443), "{to:?}");
        }
        let deny = Config::on_bridge(Policy::DenyAll, [2, 0, 0, 0, 0, 1], &bridge);
        assert!(!deny.allows(IpAddr::from([8, 8, 8, 8]), Proto::Tcp, 443));
        // IPv6 alike (D99): the gateway, loopback, link-local and multicast kept out, and
        // an IPv4 address carried in IPv6 judged as the IPv4 address it is.
        let mut allow = allow;
        allow.gateway_ip6 = Some("fd00:17::1".parse().unwrap());
        for to in ["2001:4860:4860::8888", "fd00:17::3", "::ffff:8.8.8.8"] {
            assert!(allow.allows(to.parse().unwrap(), Proto::Tcp, 443), "{to}");
        }
        for to in [
            "fd00:17::1",
            "::1",
            "::",
            "fe80::1",
            "ff02::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:172.17.0.1",
        ] {
            assert!(!allow.allows(to.parse().unwrap(), Proto::Tcp, 443), "{to}");
        }
    }

    /// A build's proxy (D110): its one flow, to the gateway's proxy port, reaches the
    /// proxy's socket; every other, the Internet, the host's networks, another port of the
    /// gateway's and UDP included, is refused.
    #[test]
    fn a_proxy_policy_reaches_its_proxy_alone() {
        let dir = shards_testdir::TempDir::new("net-proxy").unwrap();
        let socket = dir.join("p");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let bridge = bridge::Bridge::elect(&[]).unwrap();
        let policy = Policy::Proxy {
            port: 3128,
            socket: socket.clone(),
        };
        let cfg = Config::on_bridge(policy, [2, 0, 0, 0, 0, 1], &bridge);
        let gateway = IpAddr::V4(cfg.gateway_ip);
        for (to, proto, port) in [
            (IpAddr::from([8, 8, 8, 8]), Proto::Tcp, 443),
            (IpAddr::from([172, 17, 0, 3]), Proto::Tcp, 3128),
            (gateway, Proto::Tcp, 3128),
            (gateway, Proto::Udp, 53),
            (IpAddr::from([8, 8, 8, 8]), Proto::Udp, 53),
        ] {
            assert!(!cfg.allows(to, proto, port), "{to} {proto:?} {port}");
        }
        assert_eq!(cfg.proxy_flow(gateway, 3128), Some(socket.clone()));
        assert_eq!(cfg.proxy_flow(gateway, 3129), None);
        assert_eq!(cfg.proxy_flow(IpAddr::from([8, 8, 8, 8]), 3128), None);
        // A guest's SYN to it connects to the socket; one elsewhere is reset, unconnected.
        let region = Region::map(shards_netring::memory().unwrap()).unwrap();
        let (_, rings) = shards_netring::doorbell().unwrap();
        let guest_ip = cfg.guest_ip;
        let mut stack = Stack {
            frames: Frames {
                gateway_mac: cfg.gateway_mac,
                guest_mac: cfg.guest_mac,
            },
            cfg,
            to_guest: region.producer(1, rings),
            backlog: VecDeque::new(),
            tcp: Tcp::default(),
            udp: HashMap::new(),
            udp_ids: HashMap::new(),
            next_udp: 0,
            poller: poll::Poller::new().unwrap(),
            timers: BinaryHeap::new(),
            blocked: VecDeque::new(),
            buf: vec![0u8; 2048],
            scratch: Vec::new(),
            isn_key: [0; 16],
            began: Instant::now(),
            published: Vec::new(),
            inbound: HashMap::new(),
            inbound_ports: HashMap::new(),
            next_port: *EPHEMERAL.start(),
            peers: Vec::new(),
            names: None,
            #[cfg(target_os = "linux")]
            ports_confined: false,
        };
        let syn = |stack: &mut Stack<'_>, to: IpAddr, port: u16| {
            let mut frame = Vec::new();
            stack.frames.tcp_headers(
                &mut frame,
                (IpAddr::V4(guest_ip), 40_000),
                (to, port),
                7,
                0,
                wire::SYN,
                65_535,
                Some((1460, Some(7))),
                0,
            );
            let ip = wire::ipv4(frame.get(wire::VNET + wire::ETH..).unwrap()).unwrap();
            stack.on_guest_tcp(&ip);
        };
        syn(&mut stack, IpAddr::from([8, 8, 8, 8]), 443);
        syn(&mut stack, gateway, 3129);
        assert!(stack.tcp.by_key.is_empty(), "no connection but the proxy's");
        assert!(listener.accept().is_err(), "nothing reached the proxy");
        syn(&mut stack, gateway, 3128);
        assert_eq!(stack.tcp.by_key.len(), 1);
        assert!(listener.accept().is_ok(), "the proxy took it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An Agentfile's egress grants: their ports alone, of their protocol, and never
    /// what `AllowAll` keeps out; written as Docker writes ports, and carried whole.
    #[test]
    fn ports_reach_their_ports_alone() {
        let bridge = bridge::Bridge::elect(&[]).unwrap();
        let ports = Ports::parse("443, 53/udp,8000-8010/tcp").unwrap();
        assert_eq!(Ports::decode(&ports.encode()), Some(ports.clone()));
        let cfg = Config::on_bridge(Policy::Ports(ports), [2, 0, 0, 0, 0, 1], &bridge);
        let out = IpAddr::from([8, 8, 8, 8]);
        for (proto, port) in [
            (Proto::Tcp, 443),
            (Proto::Udp, 53),
            (Proto::Tcp, 8000),
            (Proto::Tcp, 8010),
        ] {
            assert!(cfg.allows(out, proto, port), "{proto:?} {port}");
        }
        for (proto, port) in [
            (Proto::Udp, 443),
            (Proto::Tcp, 53),
            (Proto::Tcp, 7999),
            (Proto::Tcp, 8011),
        ] {
            assert!(!cfg.allows(out, proto, port), "{proto:?} {port}");
        }
        assert!(!cfg.allows(IpAddr::from([169, 254, 169, 254]), Proto::Tcp, 443));
        assert!(!cfg.allows(IpAddr::from([127, 0, 0, 1]), Proto::Tcp, 443));
        for bad in ["0", "70000", "9-8", "443/sctp", "x"] {
            assert!(Ports::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Ports::decode(&[6, 0, 0, 0, 1]), None);
        // Names granted, beside the ports, carried whole.
        let named = vec![("mcp.example".to_string(), 443), ("10.0.0.7".to_string(), 8443)];
        let both = encode_policy(&Ports::parse("53/udp").unwrap(), &named, true);
        assert_eq!(
            decode_policy(&both),
            Some((Ports::parse("53/udp").unwrap(), named.clone(), true))
        );
        assert_eq!(
            decode_policy(&encode_policy(&Ports::default(), &[], false)),
            Some((Ports::default(), vec![], false))
        );
        assert_eq!(decode_policy(&both[..both.len() - 1]), None);
        // A named grant reaches its address, learned or literal, at its port alone.
        let mut cfg = Config::on_bridge(Policy::Ports(Ports::default()), [2, 0, 0, 0, 0, 1], &bridge);
        cfg.named = named;
        let learned = IpAddr::from([203, 0, 113, 9]);
        assert!(!cfg.allows(learned, Proto::Tcp, 443), "not yet resolved");
        cfg.learned.insert((learned, 443));
        assert!(cfg.allows(learned, Proto::Tcp, 443));
        assert!(!cfg.allows(learned, Proto::Tcp, 80));
        assert!(!cfg.allows(learned, Proto::Udp, 443));
        assert!(cfg.allows(IpAddr::from([10, 0, 0, 7]), Proto::Tcp, 8443));
        assert!(!cfg.allows(IpAddr::from([10, 0, 0, 8]), Proto::Tcp, 8443));
        assert_eq!(Ports::decode(&[6, 0, 1]), None);
    }

    /// The daemon's control socket publishes and the VM's lets go, and neither says the
    /// other's: a VM gives the network process nothing to serve (review 2.19).
    #[test]
    fn each_control_socket_says_only_its_own() {
        use shards_ipc::kind::{PUBLISH, UNPUBLISH};
        assert!(Control::Daemon.says(PUBLISH) && !Control::Daemon.says(UNPUBLISH));
        assert!(Control::Release.says(UNPUBLISH) && !Control::Release.says(PUBLISH));
    }

    /// An initial sequence number is its connection's and its secret's alone, plus its
    /// clock: another connection, or another process's secret, gives another, and the
    /// clock adds to it (RFC 6528 §3, review 2.31).
    #[test]
    fn initial_sequence_numbers_are_the_connections_and_the_secrets() {
        let guest = IpAddr::from([172, 17, 0, 2]);
        let conn = |port| Key {
            guest_port: port,
            remote: (IpAddr::from([93, 184, 215, 14]), 443),
        };
        let (one, two) = ([1u8; 16], [2u8; 16]);
        let isn = initial_seq(&one, 0, guest, &conn(40_000));
        assert_eq!(isn, initial_seq(&one, 0, guest, &conn(40_000)));
        assert_ne!(isn, initial_seq(&one, 0, guest, &conn(40_001)));
        assert_ne!(isn, initial_seq(&two, 0, guest, &conn(40_000)));
        assert_eq!(initial_seq(&one, 5, guest, &conn(40_000)), isn.wrapping_add(5));
    }

    /// UDP flows live as conntrack keeps them: 30 s from a datagram, until a reply has
    /// come and a datagram more than 2 s after the first; 120 s from then.
    #[test]
    fn udp_flows_live_as_conntrack_keeps_them() {
        let t0 = Instant::now();
        let mut life = Lifetime::new(t0);
        assert_eq!(life.until, t0 + UDP_UNREPLIED);
        // A query answered at once: still 30 s.
        life.datagram(t0 + Duration::from_millis(5), true);
        assert_eq!(life.until, t0 + Duration::from_millis(5) + UDP_UNREPLIED);
        // More one way only, past 2 s: not a stream without a reply.
        let mut one_way = Lifetime::new(t0);
        one_way.datagram(t0 + Duration::from_secs(3), false);
        assert_eq!(one_way.until, t0 + Duration::from_secs(3) + UDP_UNREPLIED);
        // Replied, and going past 2 s: a stream.
        life.datagram(t0 + Duration::from_secs(3), false);
        assert_eq!(life.until, t0 + Duration::from_secs(3) + UDP_STREAM);
    }

    /// A UDP flow's gateway port is one no other flow has, in turn, and none once all are.
    #[test]
    fn udp_flows_take_free_gateway_ports_in_turn() {
        let flow = || Inbound {
            published: 0,
            peer: "127.0.0.1:1".parse().unwrap(),
            asked: None,
            guest_port: 53,
            life: Lifetime::new(Instant::now()),
        };
        let start = *EPHEMERAL.start();
        let mut inbound = HashMap::from([(start, flow())]);
        let mut next = start;
        assert_eq!(free_udp_port(&mut next, &inbound), Some(start + 1));
        assert_eq!(free_udp_port(&mut next, &inbound), Some(start + 2));
        let mut last = u16::MAX;
        assert_eq!(free_udp_port(&mut last, &inbound), Some(u16::MAX));
        assert_eq!(last, start, "past the range's end, its start");
        inbound.extend(EPHEMERAL.map(|p| (p, flow())));
        assert_eq!(free_udp_port(&mut next, &inbound), None);
    }

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

    /// A guest's answer to a published port's peer that is in the ring when the port's
    /// UNPUBLISH comes goes out before the port closes, though no doorbell rang for it: a
    /// run's last datagram, which its VM puts in the ring before it asks for its ports
    /// to close (warm.rs `release_ports`), reaches its peer (the published UDP flake,
    /// 2026-10-10).
    #[test]
    fn frames_in_the_ring_go_before_a_control_message() {
        use std::os::fd::AsFd as _;
        use std::os::unix::net::UnixStream;
        let region = shards_netring::memory().unwrap();
        let ring = Region::map(region.try_clone().unwrap()).unwrap();
        let (net_waits, device_rings) = shards_netring::doorbell().unwrap();
        let (device_waits, net_rings) = shards_netring::doorbell().unwrap();
        // A doorbell nobody waits on: the guest's answer is put in the ring unannounced.
        let (_nobody, unheard) = shards_netring::doorbell().unwrap();
        let (_also_nobody, room) = shards_netring::doorbell().unwrap();
        let bridge = bridge::Bridge::elect(&[]).unwrap();
        let cfg = Config::on_bridge(Policy::DenyAll, [2, 0, 0, 0, 0, 1], &bridge);
        let (daemon, daemons) = UnixStream::pair().unwrap();
        let (vm, vms) = UnixStream::pair().unwrap();
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        let published = listener.local_addr().unwrap();
        std::thread::scope(|scope| {
            let served = scope.spawn(move || {
                serve(
                    region,
                    net_waits,
                    net_rings,
                    cfg,
                    vec![(Control::Daemon, daemons), (Control::Release, vms)],
                )
            });
            let mut ports = 5353u16.to_be_bytes().to_vec();
            ports.push(wire::PROTO_UDP);
            shards_ipc::send(&daemon, shards_ipc::kind::PUBLISH, &ports, &[listener.as_fd()]).unwrap();
            let soon = || Instant::now() + Duration::from_secs(10);
            assert!(shards_ipc::recv_by(&daemon, soon()).unwrap().is_some());
            drop(listener);
            let client = UdpSocket::bind("127.0.0.1:0").unwrap();
            client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            client.send_to(b"ping", published).unwrap();
            // The datagram as the guest is given it, then answered from the guest's port.
            let mut to_guest = ring.consumer(1, room, device_waits);
            let deadline = soon();
            let mut f = loop {
                let got = to_guest
                    .pop(|n, copy| {
                        let mut f = vec![0u8; n];
                        copy(0, f.as_mut_ptr(), n);
                        f
                    })
                    .unwrap();
                match got {
                    Some(f) if f.ends_with(b"ping") => break f,
                    _ => {
                        assert!(Instant::now() < deadline, "no datagram reached the guest");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            };
            // virtio-net's header, then Ethernet's (MACs at 0 and 6), IPv4's (addresses
            // at 12 and 16 of its 20 bytes) and UDP's (ports at 0 and 2): each pair
            // swapped, which leaves both checksums as they were.
            let (eth, ip) = (12, 12 + 14);
            let udp = ip + 20;
            for (a, b, len) in [(eth, eth + 6, 6), (ip + 12, ip + 16, 4), (udp, udp + 2, 2)] {
                for i in 0..len {
                    f.swap(a + i, b + i);
                }
            }
            // The network process asleep again, waiting to be rung, which it is not.
            std::thread::sleep(Duration::from_millis(100));
            let mut from_guest = ring.producer(0, unheard);
            let pushed = from_guest.try_push_with(f.len(), |dst| {
                // SAFETY: `dst` has room for the frame's bytes.
                unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), dst, f.len()) }
            });
            assert!(matches!(pushed, Ok(Some(_))));
            shards_ipc::send(&vm, shards_ipc::kind::UNPUBLISH, &[], &[]).unwrap();
            assert!(shards_ipc::recv_by(&vm, soon()).unwrap().is_some());
            let mut buf = [0u8; 64];
            let answered = client.recv_from(&mut buf);
            // The VM goes: its device's doorbell closes, and the network process ends.
            drop(device_rings);
            assert!(served.join().unwrap().is_ok());
            let (n, from) = answered.expect("the guest's answer before the port closed");
            assert_eq!((&buf[..n], from), (&b"ping"[..], published));
        });
    }
}
