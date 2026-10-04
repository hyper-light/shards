//! Docker's default bridge, its subnet elected as dockerd elects it (moby docker-v29.9.0,
//! daemon/daemon_unix.go initBridgeDriver): the first subnet of dockerd's default pools
//! (libnetwork/ipamutils, localScopeDefaultNetworks) that overlaps no network the host
//! seems to use (libnetwork/netutils, InferReservedNetworks): its resolvers, and its
//! on-link IPv4 routes. A guest on a subnet the host is on could not reach that network:
//! the guest's own address, or its gateway's, would shadow the host's (a host on Docker's
//! own bridge, as a CI job in a container is, has 172.17.0.2, the guest's address there).
//!
//! The subnet is on the guest's kernel command line, and so in each template a guest on
//! the bridge saves, which is named by that line (run.rs, `template`): a template is only
//! ever restored on the subnet it was saved on.

use std::io;
use std::net::Ipv4Addr;

/// An IPv4 prefix: a network address and its length.
pub type Prefix = (Ipv4Addr, u8);

/// dockerd's default local pools, each split into subnets of the given length, in the
/// order they are tried (ipamutils, localScopeDefaultNetworks).
const POOLS: [(Ipv4Addr, u8, u8); 7] = [
    (Ipv4Addr::new(172, 17, 0, 0), 16, 16),
    (Ipv4Addr::new(172, 18, 0, 0), 16, 16),
    (Ipv4Addr::new(172, 19, 0, 0), 16, 16),
    (Ipv4Addr::new(172, 20, 0, 0), 14, 16),
    (Ipv4Addr::new(172, 24, 0, 0), 14, 16),
    (Ipv4Addr::new(172, 28, 0, 0), 14, 16),
    (Ipv4Addr::new(192, 168, 0, 0), 16, 20),
];

/// What dockerd says when every pool overlaps (ipamapi, ErrNoMoreSubnets), as it fails to
/// make its default bridge (initBridgeDriver).
pub const NO_SUBNET: &str =
    "error creating default \"bridge\" network: all predefined address pools have been fully subnetted";

/// The default bridge: its subnet, whose first address is the gateway's and second the
/// guest's, as dockerd's IPAM gives the bridge and then its first container theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bridge {
    subnet: Ipv4Addr,
    bits: u8,
}

impl Bridge {
    /// The first subnet of the default pools that overlaps none of `reserved`.
    pub fn elect(reserved: &[Prefix]) -> Option<Bridge> {
        POOLS.iter().find_map(|&(base, base_bits, bits)| {
            let count = 1u32 << (bits - base_bits);
            (0..count)
                .map(|i| Ipv4Addr::from(u32::from(base) + (i << (32 - u32::from(bits)))))
                .find(|&subnet| !reserved.iter().any(|&r| overlaps((subnet, bits), r)))
                .map(|subnet| Bridge { subnet, bits })
        })
    }

    /// The subnet as `ADDR/BITS`.
    pub fn subnet(&self) -> Prefix {
        (self.subnet, self.bits)
    }

    pub fn gateway(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.subnet) + 1)
    }

    pub fn guest(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.subnet) + 2)
    }

    /// Whether `ip` is on the bridge.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        overlaps((ip, 32), self.subnet())
    }

    /// What a guest on the bridge has on its kernel command line:
    /// `shards_net=ADDR/PREFIX,GATEWAY`.
    pub fn cmdline(&self) -> String {
        format!("shards_net={}/{},{}", self.guest(), self.bits, self.gateway())
    }
}

impl std::fmt::Display for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.subnet, self.bits)
    }
}

impl std::str::FromStr for Bridge {
    type Err = String;

    /// `ADDR/BITS`, its address the subnet's own, for a subnet with room for a gateway and
    /// a guest.
    fn from_str(s: &str) -> Result<Bridge, String> {
        let bad = || format!("{s:?} is not a subnet, ADDR/BITS");
        let (addr, bits) = s.split_once('/').ok_or_else(bad)?;
        let subnet: Ipv4Addr = addr.parse().map_err(|_| bad())?;
        let bits: u8 = bits.parse().map_err(|_| bad())?;
        if bits > 30 || u32::from(subnet) & !mask(bits) != 0 {
            return Err(bad());
        }
        Ok(Bridge { subnet, bits })
    }
}

/// The mask of a prefix `bits` long.
fn mask(bits: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(bits.min(32))).unwrap_or(0)
}

/// Whether two prefixes share an address: the shorter's network holds the other's.
fn overlaps(a: Prefix, b: Prefix) -> bool {
    let m = mask(a.1.min(b.1));
    u32::from(a.0) & m == u32::from(b.0) & m
}

/// The IPv4 nameservers of `resolv_conf`, each as a prefix of its own address
/// (netutils, tryGetNameserversAsPrefix).
pub fn nameservers(resolv_conf: &[u8]) -> Vec<Prefix> {
    String::from_utf8_lossy(resolv_conf)
        .lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["nameserver", addr, ..] => addr.parse::<Ipv4Addr>().ok(),
            _ => None,
        })
        .map(|a| (a, 32))
        .collect()
}

/// What the host seems to use (InferReservedNetworks): the nameservers of `resolv_conf`,
/// the host's own (dockerd reads the file its resolvers are taken from), and its on-link
/// IPv4 routes.
pub fn reserved(resolv_conf: &[u8]) -> io::Result<Vec<Prefix>> {
    let mut reserved = nameservers(resolv_conf);
    reserved.extend(on_link_routes()?);
    Ok(reserved)
}

/// The bridge this host's dockerd would make: elected from what the host seems to use.
/// Routes that cannot be read are said to `note`, and the subnet elected without them, as
/// dockerd does: what fails to be read is not reserved (InferReservedNetworks).
pub fn elected_here(note: &mut dyn FnMut(String)) -> Option<Bridge> {
    let resolv = host_resolv();
    let reserved = reserved(&resolv).unwrap_or_else(|e| {
        note(format!(
            "reading the host's routes: {e}; the default bridge's subnet is elected without them"
        ));
        nameservers(&resolv)
    });
    Bridge::elect(&reserved)
}

/// The host's resolvers, as BuildKit (executor/oci resolvconfPath) and dockerd
/// (libnetwork resolvconf.Path) read them: /etc/resolv.conf, unless its one nameserver is
/// systemd-resolved's stub, 127.0.0.53, which within a guest is the guest's own address;
/// then the servers systemd-resolved forwards to, which it lists in
/// /run/systemd/resolve/resolv.conf.
pub fn host_resolv() -> Vec<u8> {
    pick_resolv(std::fs::read("/etc/resolv.conf").unwrap_or_default(), || {
        std::fs::read("/run/systemd/resolve/resolv.conf").unwrap_or_default()
    })
}

fn pick_resolv(main: Vec<u8>, systemd: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&main);
    let servers: Vec<std::net::IpAddr> = text
        .lines()
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["nameserver", addr, ..] => addr.parse().ok(),
                _ => None,
            },
        )
        .collect();
    if servers == [std::net::IpAddr::from([127, 0, 0, 53])] {
        return systemd();
    }
    main
}

/// The main table's IPv4 routes of link scope, but the default and those cloned
/// (queryOnLinkRoutes, through vishvananda/netlink's RouteList, which skips other tables
/// and cloned routes): the subnets this host is on.
#[cfg(target_os = "linux")]
pub fn on_link_routes() -> io::Result<Vec<Prefix>> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, owned from here on.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // A dump the table changed during says so (NLM_F_DUMP_INTR): it is taken again.
    for seq in 1..=8 {
        if let Some(routes) = dump_routes(&sock, seq)? {
            return Ok(routes);
        }
    }
    Err(io::Error::other("the routing table kept changing as it was read"))
}

/// One dump of the routes, numbered `seq`, on `sock`; none if the table changed during it.
#[cfg(target_os = "linux")]
fn dump_routes(sock: &std::os::fd::OwnedFd, seq: u32) -> io::Result<Option<Vec<Prefix>>> {
    use std::os::fd::AsRawFd;
    // struct nlmsghdr, then struct rtmsg asking for IPv4's.
    let mut req = Vec::with_capacity(28);
    req.extend_from_slice(&28u32.to_ne_bytes());
    req.extend_from_slice(&libc::RTM_GETROUTE.to_ne_bytes());
    req.extend_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    req.extend_from_slice(&seq.to_ne_bytes());
    req.extend_from_slice(&0u32.to_ne_bytes());
    req.extend_from_slice(&[libc::AF_INET as u8, 0, 0, 0, 0, 0, 0, 0]);
    req.extend_from_slice(&0u32.to_ne_bytes());
    // SAFETY: send(2) of a buffer of ours, of its length, to the kernel.
    let sent = unsafe { libc::send(sock.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) };
    if usize::try_from(sent).ok() != Some(req.len()) {
        return Err(if sent < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::other("a route dump's request fell short")
        });
    }
    let mut routes = Vec::new();
    let mut interrupted = false;
    // A dump's datagrams are at most as large as the reader's buffer was (af_netlink.c,
    // netlink_dump: max_recvmsg_len), and the first at most NLMSG_GOODSIZE.
    let mut buf = vec![0u8; 64 << 10];
    loop {
        // SAFETY: recv(2) into a buffer of ours, of its length; MSG_TRUNC has it say a
        // datagram's whole length, so that one cut short is seen.
        let n = unsafe {
            libc::recv(
                sock.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_TRUNC,
            )
        };
        let n = match usize::try_from(n) {
            Ok(n) if n <= buf.len() => n,
            Ok(_) => return Err(io::Error::other("a route dump's datagram was cut short")),
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
        };
        if n == 0 {
            return Err(io::Error::other("a route dump ended without NLMSG_DONE"));
        }
        if routes_of(
            buf.get(..n).unwrap_or_default(),
            seq,
            &mut routes,
            &mut interrupted,
        )? {
            return Ok((!interrupted).then_some(routes));
        }
    }
}

/// The on-link routes of one datagram of a route dump answering request `seq`, added to
/// `routes`, and whether the kernel said the table changed during it; whether the dump is
/// done.
#[cfg(any(target_os = "linux", test))]
fn routes_of(
    mut data: &[u8],
    seq: u32,
    routes: &mut Vec<Prefix>,
    interrupted: &mut bool,
) -> io::Result<bool> {
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const NLM_F_DUMP_INTR: u16 = 0x10;
    const RTM_NEWROUTE: u16 = 24;
    const RTM_F_CLONED: u32 = 0x200;
    const RT_SCOPE_LINK: u8 = 253;
    const RT_TABLE_MAIN: u8 = 254;
    const RTA_DST: u16 = 1;
    let u16_at = |b: &[u8], at: usize| {
        b.get(at..at + 2)
            .and_then(|v| v.try_into().ok())
            .map(u16::from_ne_bytes)
    };
    let u32_at = |b: &[u8], at: usize| {
        b.get(at..at + 4)
            .and_then(|v| v.try_into().ok())
            .map(u32::from_ne_bytes)
    };
    let bad = || io::Error::other("a route dump's message is malformed");
    while !data.is_empty() {
        let len = u32_at(data, 0).ok_or_else(bad)? as usize;
        let kind = u16_at(data, 4).ok_or_else(bad)?;
        let flags = u16_at(data, 6).ok_or_else(bad)?;
        let got = u32_at(data, 8).ok_or_else(bad)?;
        let msg = data.get(..len).filter(|_| len >= 16).ok_or_else(bad)?;
        data = data.get(len.next_multiple_of(4)..).unwrap_or_default();
        if got != seq {
            continue;
        }
        *interrupted |= flags & NLM_F_DUMP_INTR != 0;
        match kind {
            NLMSG_DONE => return Ok(true),
            // struct nlmsgerr: a negative errno, or 0 for an acknowledgement.
            NLMSG_ERROR => {
                let errno = u32_at(msg, 16).ok_or_else(bad)? as i32;
                if errno == 0 {
                    return Ok(true);
                }
                return Err(io::Error::from_raw_os_error(errno.saturating_neg()));
            }
            RTM_NEWROUTE => {}
            _ => continue,
        }
        // struct rtmsg: family, dst_len, src_len, tos, table, protocol, scope, type, flags.
        let rt: &[u8; 12] = msg.get(16..28).and_then(|r| r.try_into().ok()).ok_or_else(bad)?;
        let [family, dst_len, _, _, table, _, scope, _, f0, f1, f2, f3] = *rt;
        let flags = u32::from_ne_bytes([f0, f1, f2, f3]);
        if i32::from(family) != libc::AF_INET
            || table != RT_TABLE_MAIN
            || scope != RT_SCOPE_LINK
            || flags & RTM_F_CLONED != 0
        {
            continue;
        }
        let mut attrs = msg.get(28..).unwrap_or_default();
        while attrs.len() >= 4 {
            let alen = usize::from(u16_at(attrs, 0).ok_or_else(bad)?);
            let akind = u16_at(attrs, 2).ok_or_else(bad)?;
            let value = attrs.get(4..alen).filter(|_| alen >= 4).ok_or_else(bad)?;
            if akind == RTA_DST
                && let Ok(octets) = <[u8; 4]>::try_from(value)
            {
                let dst = Ipv4Addr::from(octets);
                if !dst.is_unspecified() {
                    routes.push((dst, dst_len.min(32)));
                }
            }
            attrs = attrs.get(alen.next_multiple_of(4)..).unwrap_or_default();
        }
    }
    Ok(false)
}

/// The routing table's IPv4 routes without a gateway, but the default and those cloned
/// (ARP's): the subnets this host is on, as XNU's route dump gives them (bsd/net/rtsock.c,
/// sysctl_dumpentry). dockerd has no macOS host; this is its rule with XNU's flags.
#[cfg(target_os = "macos")]
pub fn on_link_routes() -> io::Result<Vec<Prefix>> {
    let mut mib = [
        libc::CTL_NET,
        libc::PF_ROUTE,
        0,
        libc::AF_INET,
        libc::NET_RT_DUMP,
        0,
    ];
    // The table may grow between asking its size and reading it: asked again then.
    for _ in 0..8 {
        let mut len: libc::size_t = 0;
        // SAFETY: sysctl(3) asking a size, into a size_t of ours.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; len + len / 4];
        let mut got = buf.len();
        // SAFETY: sysctl(3) into a buffer of ours, of the length said.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                buf.as_mut_ptr().cast(),
                &mut got,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOMEM) {
                continue;
            }
            return Err(e);
        }
        return dumped_routes(buf.get(..got).unwrap_or_default());
    }
    Err(io::Error::other("the routing table kept growing as it was read"))
}

/// The on-link routes of an XNU route dump.
#[cfg(any(target_os = "macos", test))]
fn dumped_routes(mut data: &[u8]) -> io::Result<Vec<Prefix>> {
    // struct rt_msghdr (net/route.h): rtm_msglen, rtm_version, rtm_type, rtm_index, then
    // rtm_flags and rtm_addrs, as ints, at 8 and 12; the addresses follow its 92 bytes.
    const HEADER: usize = 92;
    const RTM_VERSION: u8 = 5;
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    const RTF_HOST: u32 = 0x4;
    const RTF_WASCLONED: u32 = 0x20000;
    const RTA_DST: u32 = 0x1;
    const RTA_NETMASK: u32 = 0x4;
    const RTAX_MAX: u32 = 8;
    const AF_INET: u8 = 2;
    let bad = || io::Error::other("a route dump's message is malformed");
    let mut routes = Vec::new();
    while !data.is_empty() {
        let len = usize::from(u16::from_ne_bytes([
            *data.first().ok_or_else(bad)?,
            *data.get(1).ok_or_else(bad)?,
        ]));
        let msg = data.get(..len).filter(|_| len >= HEADER).ok_or_else(bad)?;
        data = data.get(len..).unwrap_or_default();
        if msg.get(2) != Some(&RTM_VERSION) {
            return Err(io::Error::other("a route dump of a version this does not read"));
        }
        let int_at = |at: usize| {
            msg.get(at..at + 4)
                .and_then(|v| v.try_into().ok())
                .map(u32::from_ne_bytes)
        };
        let flags = int_at(8).ok_or_else(bad)?;
        let addrs = int_at(12).ok_or_else(bad)?;
        // Each address in turn, as long as it says, rounded up to 4 bytes (ROUNDUP32).
        let (mut dst, mut netmask) = (None, None);
        let mut at = HEADER;
        for i in 0..RTAX_MAX {
            if addrs & (1 << i) == 0 {
                continue;
            }
            let sa_len = usize::from(*msg.get(at).ok_or_else(bad)?);
            let sa = msg.get(at..at + sa_len).ok_or_else(bad)?;
            // sockaddr_in: len, family, port, then the address at 4; a netmask may be cut
            // short past its last byte that is not 0, its family unset.
            let octets = |sa: &[u8]| {
                let mut a = [0u8; 4];
                for (k, b) in a.iter_mut().enumerate() {
                    *b = sa.get(4 + k).copied().unwrap_or(0);
                }
                Ipv4Addr::from(a)
            };
            match 1 << i {
                RTA_DST if sa.get(1) == Some(&AF_INET) => dst = Some(octets(sa)),
                RTA_NETMASK => netmask = Some(octets(sa)),
                _ => {}
            }
            at += if sa_len == 0 {
                4
            } else {
                sa_len.next_multiple_of(4)
            };
        }
        let Some(dst) = dst else { continue };
        if flags & RTF_UP == 0 || flags & (RTF_GATEWAY | RTF_WASCLONED) != 0 || dst.is_unspecified() {
            continue;
        }
        let bits = match netmask {
            _ if flags & RTF_HOST != 0 => 32,
            Some(m) => {
                let m = u32::from(m);
                // A mask that is not contiguous names no prefix.
                if m.leading_ones() + m.trailing_zeros() != 32 {
                    continue;
                }
                m.leading_ones() as u8
            }
            None => 32,
        };
        routes.push((dst, bits));
    }
    Ok(routes)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn on_link_routes() -> io::Result<Vec<Prefix>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reading this host's routes is not supported on this platform",
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn the_first_free_subnet_of_the_pools_in_order_is_elected() {
        let at = |reserved: &[(&str, u8)]| {
            let r: Vec<Prefix> = reserved.iter().map(|&(a, b)| (ip(a), b)).collect();
            Bridge::elect(&r).map(|b| b.to_string())
        };
        assert_eq!(at(&[]).as_deref(), Some("172.17.0.0/16"));
        // A CI job's container on Docker's own bridge, as dockerd in it elects.
        assert_eq!(at(&[("172.17.0.0", 16)]).as_deref(), Some("172.18.0.0/16"));
        // A nameserver on it, or a small on-link subnet in it, takes all of it.
        assert_eq!(at(&[("172.17.0.53", 32)]).as_deref(), Some("172.18.0.0/16"));
        assert_eq!(
            at(&[("172.17.200.0", 24), ("172.18.0.0", 16)]).as_deref(),
            Some("172.19.0.0/16")
        );
        // The /14 pools give their /16s in turn.
        assert_eq!(
            at(&[("172.16.0.0", 12)]).as_deref(),
            Some("192.168.0.0/20"),
            "172.16/12 covers every 172 pool"
        );
        assert_eq!(
            at(&[("172.17.0.0", 16), ("172.18.0.0", 15), ("172.20.0.0", 16)]).as_deref(),
            Some("172.21.0.0/16")
        );
        // 192.168/16 in /20s.
        assert_eq!(
            at(&[("172.16.0.0", 12), ("192.168.0.0", 24), ("192.168.17.4", 32)]).as_deref(),
            Some("192.168.32.0/20")
        );
        assert_eq!(at(&[("0.0.0.0", 0)]), None, "{NO_SUBNET}");
        assert_eq!(at(&[("128.0.0.0", 1)]), None);
    }

    #[test]
    fn a_bridge_names_its_gateway_and_guest() {
        let b = Bridge::elect(&[(ip("172.17.0.0"), 16)]).unwrap();
        assert_eq!((b.gateway(), b.guest()), (ip("172.18.0.1"), ip("172.18.0.2")));
        assert_eq!(b.cmdline(), "shards_net=172.18.0.2/16,172.18.0.1");
        assert!(b.contains(ip("172.18.255.255")) && !b.contains(ip("172.19.0.0")));
        assert_eq!("172.18.0.0/16".parse::<Bridge>(), Ok(b));
        for bad in ["172.18.0.1/16", "172.18.0.0", "172.18.0.0/31", "x/16"] {
            assert!(bad.parse::<Bridge>().is_err(), "{bad}");
        }
    }

    /// systemd-resolved's stub, alone, sends the reader to the servers it forwards to;
    /// any other list, the stub among others included, is read as it is.
    #[test]
    fn systemd_resolveds_stub_is_read_past() {
        let systemd = || b"nameserver 10.0.0.2\n".to_vec();
        for (main, want) in [
            (
                &b"nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n"[..],
                &b"nameserver 10.0.0.2\n"[..],
            ),
            (b"# systemd\nnameserver   127.0.0.53\n", b"nameserver 10.0.0.2\n"),
            (
                b"nameserver 127.0.0.53\nnameserver 1.1.1.1\n",
                b"nameserver 127.0.0.53\nnameserver 1.1.1.1\n",
            ),
            (b"nameserver 192.168.1.1\n", b"nameserver 192.168.1.1\n"),
            (b"", b""),
        ] {
            assert_eq!(
                pick_resolv(main.to_vec(), systemd),
                want,
                "{}",
                String::from_utf8_lossy(main)
            );
        }
    }

    #[test]
    fn nameservers_are_the_ipv4_ones() {
        let conf = b"# comment\nsearch example.com\nnameserver 10.0.0.2\nnameserver 2001:db8::1\nnameserver 192.168.1.254 # x\nnameserver bad\n";
        assert_eq!(
            nameservers(conf),
            vec![(ip("10.0.0.2"), 32), (ip("192.168.1.254"), 32)]
        );
    }

    /// One RTM_NEWROUTE of a dump: `rtmsg` and its RTA_DST.
    fn route(seq: u32, dst: [u8; 4], dst_len: u8, table: u8, scope: u8, flags: u32) -> Vec<u8> {
        routed(seq, 2, dst, dst_len, table, scope, flags)
    }

    /// [`route`], with the message's own flags.
    fn routed(
        seq: u32,
        nl_flags: u16,
        dst: [u8; 4],
        dst_len: u8,
        table: u8,
        scope: u8,
        flags: u32,
    ) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&36u32.to_ne_bytes());
        m.extend_from_slice(&24u16.to_ne_bytes());
        m.extend_from_slice(&nl_flags.to_ne_bytes());
        m.extend_from_slice(&seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&[libc::AF_INET as u8, dst_len, 0, 0, table, 2, scope, 1]);
        m.extend_from_slice(&flags.to_ne_bytes());
        m.extend_from_slice(&8u16.to_ne_bytes());
        m.extend_from_slice(&1u16.to_ne_bytes());
        m.extend_from_slice(&dst);
        m
    }

    #[test]
    fn a_linux_route_dump_gives_the_main_tables_on_link_routes() {
        let mut d = Vec::new();
        d.extend(route(1, [172, 17, 0, 0], 16, 254, 253, 0));
        // Another table's, a global-scope route via a gateway, a cloned one, another
        // request's: none.
        d.extend(route(1, [10, 0, 0, 0], 8, 255, 253, 0));
        d.extend(route(1, [10, 1, 0, 0], 16, 254, 0, 0));
        d.extend(route(1, [10, 2, 0, 0], 16, 254, 253, 0x200));
        d.extend(route(7, [10, 3, 0, 0], 16, 254, 253, 0));
        let (mut routes, mut interrupted) = (Vec::new(), false);
        assert!(!routes_of(&d, 1, &mut routes, &mut interrupted).unwrap());
        assert_eq!(routes, vec![(ip("172.17.0.0"), 16)]);
        let ended = |kind: u16, errno: i32| {
            let mut m = Vec::new();
            m.extend_from_slice(&20u32.to_ne_bytes());
            m.extend_from_slice(&kind.to_ne_bytes());
            m.extend_from_slice(&2u16.to_ne_bytes());
            m.extend_from_slice(&1u32.to_ne_bytes());
            m.extend_from_slice(&0u32.to_ne_bytes());
            m.extend_from_slice(&errno.to_ne_bytes());
            m
        };
        assert!(routes_of(&ended(3, 0), 1, &mut routes, &mut interrupted).unwrap());
        assert!(!interrupted);
        // An error is the kernel's errno; an acknowledgement ends the dump.
        let e = routes_of(&ended(2, -libc::EPERM), 1, &mut routes, &mut interrupted).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        assert!(routes_of(&ended(2, 0), 1, &mut routes, &mut interrupted).unwrap());
        // A dump the table changed during says so, for it to be taken again.
        routes_of(
            &routed(1, 2 | 0x10, [10, 4, 0, 0], 16, 254, 253, 0),
            1,
            &mut routes,
            &mut interrupted,
        )
        .unwrap();
        assert!(interrupted);
        // A message longer than what holds it is refused, not read past.
        let mut cut = route(1, [172, 17, 0, 0], 16, 254, 253, 0);
        cut.truncate(30);
        assert!(routes_of(&cut, 1, &mut Vec::new(), &mut false).is_err());
    }

    /// The layout `dumped_routes` reads is XNU's own (net/route.h, as libc binds it).
    #[cfg(target_os = "macos")]
    #[test]
    fn an_xnu_route_message_is_laid_out_as_read() {
        assert_eq!(std::mem::size_of::<libc::rt_msghdr>(), 92);
        assert_eq!(std::mem::offset_of!(libc::rt_msghdr, rtm_flags), 8);
        assert_eq!(std::mem::offset_of!(libc::rt_msghdr, rtm_addrs), 12);
        assert_eq!(libc::RTM_VERSION, 5);
        assert_eq!(
            (
                libc::RTF_UP,
                libc::RTF_GATEWAY,
                libc::RTF_HOST,
                libc::RTF_WASCLONED
            ),
            (0x1, 0x2, 0x4, 0x20000)
        );
        assert_eq!((libc::RTA_DST, libc::RTA_NETMASK, libc::RTAX_MAX), (0x1, 0x4, 8));
    }

    /// One message of an XNU route dump: its header, then the addresses `addrs` names.
    fn rt(flags: u32, addrs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut m = vec![0u8; 92];
        m[2] = 5;
        m[3] = 4;
        m[8..12].copy_from_slice(&flags.to_ne_bytes());
        let bits: u32 = addrs.iter().map(|(b, _)| b).sum();
        m[12..16].copy_from_slice(&bits.to_ne_bytes());
        for (_, sa) in addrs {
            m.extend_from_slice(sa);
            let pad = if sa.first() == Some(&0) {
                4 - sa.len()
            } else {
                sa.len().next_multiple_of(4) - sa.len()
            };
            m.extend(std::iter::repeat_n(0, pad));
        }
        let len = u16::try_from(m.len()).unwrap();
        m[0..2].copy_from_slice(&len.to_ne_bytes());
        m
    }

    fn sin(a: [u8; 4]) -> Vec<u8> {
        let mut s = vec![16, 2, 0, 0];
        s.extend_from_slice(&a);
        s.extend_from_slice(&[0; 8]);
        s
    }

    #[test]
    fn an_xnu_route_dump_gives_the_routes_without_a_gateway() {
        let link = vec![20u8, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut d = Vec::new();
        // 192.168.1/24 link#15 UCS: its netmask cut short at its last byte that is not 0.
        d.extend(rt(
            0x1 | 0x100 | 0x800,
            &[
                (1, sin([192, 168, 1, 0])),
                (2, link.clone()),
                (4, vec![7, 0, 0, 0, 255, 255, 255]),
            ],
        ));
        // default via 192.168.1.254 (UG), and an ARP entry (UHLW): neither.
        d.extend(rt(
            0x1 | 0x2,
            &[
                (1, sin([0, 0, 0, 0])),
                (2, sin([192, 168, 1, 254])),
                (4, vec![0, 0, 0, 0]),
            ],
        ));
        d.extend(rt(
            0x1 | 0x4 | 0x400 | 0x20000,
            &[(1, sin([192, 168, 1, 66])), (2, link.clone())],
        ));
        // A host route without a gateway (UH): a /32.
        d.extend(rt(0x1 | 0x4, &[(1, sin([10, 9, 8, 7])), (2, link)]));
        assert_eq!(
            dumped_routes(&d).unwrap(),
            vec![(ip("192.168.1.0"), 24), (ip("10.9.8.7"), 32)]
        );
        let mut cut = rt(0x1, &[(1, sin([10, 0, 0, 0]))]);
        cut.truncate(95);
        cut[0..2].copy_from_slice(&95u16.to_ne_bytes());
        assert!(dumped_routes(&cut).is_err(), "an address past its message");
    }

    /// What an election on this host costs, n = `ELECTIONS`: its resolvers and routes
    /// read, and a subnet elected (PM M101). p50, p90, p99 and max, in microseconds.
    #[test]
    #[ignore = "a measurement: ELECTIONS=… cargo test --release -p shards-net -- --ignored --nocapture"]
    fn an_election_costs() {
        let n: usize = std::env::var("ELECTIONS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(1000);
        let mut took: Vec<u128> = (0..n)
            .map(|_| {
                let start = std::time::Instant::now();
                std::hint::black_box(elected_here(&mut |note| panic!("{note}")));
                start.elapsed().as_nanos()
            })
            .collect();
        took.sort_unstable();
        let at = |q: f64| took[((took.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
        println!(
            "an election, n {n}: p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} µs",
            at(0.5),
            at(0.9),
            at(0.99),
            at(1.0)
        );
    }

    /// This host's own routes are read whole, as the election reads them.
    #[test]
    fn this_hosts_on_link_routes_are_read() {
        let routes = on_link_routes().unwrap();
        assert!(routes.iter().all(|&(_, bits)| bits <= 32), "{routes:?}");
        let elected = Bridge::elect(&routes);
        println!("on-link routes {routes:?}: elected {elected:?}");
    }
}
