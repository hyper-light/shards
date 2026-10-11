//! The links between domains (docs/design/architecture.md D59, D61; AGENTFILE_ARCH.md
//! §9.7): a switch, a network namespace of no process's that init holds, and for each
//! domain with a grant a gate, a namespace of its own between the domain and the switch:
//! `eth0` in the domain's namespace and `in0` in the gate's, `out0` in the gate's and
//! `d<n>` in the switch's.
//!
//! Each domain's connection tracking is its gate's alone (D61): a gate tracks the flows
//! its domain opens, and takes those others open to it, and its answers to them, past
//! tracking (`notrack`), by what the Agentfile grants: the sender's gate holds every
//! address it sends from to its own. The switch tracks nothing: it routes, and its
//! nftables, stateless, decide by link what may cross it, each grant's ports one way and
//! answers back. So no domain's flows, however many it opens, take room in another's
//! table: a full table drops what its own domain opens. No link reaches the microVM's own
//! network.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::netplan::Link;

const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_APPEND: u16 = 0x800;
const NLMSG_ERROR: u16 = 2;
const NLA_F_NESTED: u16 = 0x8000;
const IFLA_IFNAME: u16 = 3;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;
const RT_TABLE_MAIN: u8 = 254;
const RTPROT_BOOT: u8 = 3;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;
const RTNH_F_ONLINK: u32 = 4;

/// A domain's link, in its own namespace: `eth0`, after `lo`.
const DOMAIN_IFINDEX: i32 = 2;
/// A gate's links: `in0` to its domain, `out0` to the switch.
const GATE_IN: i32 = 2;
const GATE_OUT: i32 = 3;
/// The switch's end of every gate's link, link-local (RFC 3927): the same on each, where
/// the agents' resolver answers.
pub const TRANSIT_SWITCH: Ipv4Addr = Ipv4Addr::new(169, 254, 78, 1);

/// The n-th gate's end of its link to the switch: one address each, from link-local
/// 169.254.128.0/17, so that the switch routes each down its own link and its strict
/// reverse-path filtering, which checks ARP's senders too (net/ipv4/arp.c, `arp_process`),
/// holds for all. None past the 32,512th.
fn transit_gate(n: usize) -> io::Result<Ipv4Addr> {
    let n = u32::try_from(n)
        .ok()
        .filter(|n| *n < 127 * 256)
        .ok_or_else(|| io::Error::other("too many domains"))?;
    let at = n + 1;
    Ok(Ipv4Addr::new(169, 254, 128 + (at >> 8) as u8, (at & 0xff) as u8))
}
/// The n-th domain's link in the switch: `d<n>`, index this plus n.
const SWITCH_IFINDEX: i32 = 100;
/// The switch's link to init's namespace, past which the microVM's eth0 is: `up0` in the
/// switch, `agents0` in init's, each index this; on a link-local /30 of their own (RFC
/// 3927), which no network of the guest's or the host's takes.
const UPLINK_IFINDEX: i32 = 99;
const UPLINK_INIT: Ipv4Addr = {
    let [a, b, c, d] = shards_abi::run::beside::UPLINK_NET.0;
    Ipv4Addr::new(a, b, c, d + 1)
};
const UPLINK_SWITCH: Ipv4Addr = {
    let [a, b, c, d] = shards_abi::run::beside::UPLINK_NET.0;
    Ipv4Addr::new(a, b, c, d + 2)
};
const UPLINK_PREFIX: u8 = shards_abi::run::beside::UPLINK_NET.1;
/// The mark init's forward chain gives what an agent sends past the microVM, by which its
/// postrouting chain gives it eth0's address.
const MARK_AGENTS: u32 = 0x5a59_0001;

/// The switch's link past the microVM: the agents' subnets, which init's namespace routes
/// back down it, and eth0's address and subnet, which their flows leave by.
pub struct Uplink {
    pub subnets: Vec<(Ipv4Addr, u8)>,
    pub eth0: (Ipv4Addr, Ipv4Addr, u8),
    /// The ports let in, each to the address of the domain they are for: the microVM's,
    /// translated to the domain's where an `EXPOSE` maps them (D122).
    pub ingress: Vec<(Ipv4Addr, Vec<crate::netplan::Ingress>)>,
    /// The ports each domain opens flows to past the microVM, from its addresses.
    pub egress: Vec<(Vec<Ipv4Addr>, Vec<crate::netplan::Egress>)>,
    /// Whether the agents' resolver asks the microVM's.
    pub resolver: bool,
    /// Past the microVM by IPv6 (D99), where eth0 has IPv6: eth0's IPv6 address, the
    /// agents' IPv6 subnets, and the ports each domain opens flows to from its IPv6
    /// addresses. None where eth0 has no IPv6 or no domain with egress has IPv6.
    pub ipv6: Option<Uplink6>,
}

/// The uplink's IPv6 part (D99).
pub struct Uplink6 {
    pub eth0: Ipv6Addr,
    pub subnets: Vec<(Ipv6Addr, u8)>,
    pub egress: Vec<(Vec<Ipv6Addr>, Vec<crate::netplan::Egress>)>,
}

/// nfnetlink and nf_tables (include/uapi/linux/netfilter/nfnetlink.h, nf_tables.h).
mod nft {
    pub const NFNL_SUBSYS_NFTABLES: u16 = 10;
    pub const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
    pub const NFNL_MSG_BATCH_END: u16 = 0x11;
    pub const NFT_MSG_NEWTABLE: u16 = 0;
    pub const NFT_MSG_NEWCHAIN: u16 = 3;
    pub const NFT_MSG_NEWRULE: u16 = 6;
    pub const NFPROTO_IPV4: u8 = 2;
    pub const NFPROTO_IPV6: u8 = 10;
    pub const NFTA_TABLE_NAME: u16 = 1;
    pub const NFTA_CHAIN_TABLE: u16 = 1;
    pub const NFTA_CHAIN_NAME: u16 = 3;
    pub const NFTA_CHAIN_HOOK: u16 = 4;
    pub const NFTA_CHAIN_POLICY: u16 = 5;
    pub const NFTA_CHAIN_TYPE: u16 = 7;
    pub const NFTA_HOOK_HOOKNUM: u16 = 1;
    pub const NFTA_HOOK_PRIORITY: u16 = 2;
    pub const NFTA_RULE_TABLE: u16 = 1;
    pub const NFTA_RULE_CHAIN: u16 = 2;
    pub const NFTA_RULE_EXPRESSIONS: u16 = 4;
    pub const NFTA_LIST_ELEM: u16 = 1;
    pub const NFTA_EXPR_NAME: u16 = 1;
    pub const NFTA_EXPR_DATA: u16 = 2;
    pub const NFTA_DATA_VALUE: u16 = 1;
    pub const NFTA_DATA_VERDICT: u16 = 2;
    pub const NFTA_VERDICT_CODE: u16 = 1;
    pub const NFTA_META_DREG: u16 = 1;
    pub const NFTA_META_KEY: u16 = 2;
    pub const NFTA_CMP_SREG: u16 = 1;
    pub const NFTA_CMP_OP: u16 = 2;
    pub const NFTA_CMP_DATA: u16 = 3;
    pub const NFTA_CT_DREG: u16 = 1;
    pub const NFTA_CT_KEY: u16 = 2;
    pub const NFTA_BITWISE_SREG: u16 = 1;
    pub const NFTA_BITWISE_DREG: u16 = 2;
    pub const NFTA_BITWISE_LEN: u16 = 3;
    pub const NFTA_BITWISE_MASK: u16 = 4;
    pub const NFTA_BITWISE_XOR: u16 = 5;
    pub const NFTA_IMMEDIATE_DREG: u16 = 1;
    pub const NFTA_IMMEDIATE_DATA: u16 = 2;
    pub const NFT_REG_VERDICT: u32 = 0;
    pub const NFT_REG_1: u32 = 1;
    pub const NFT_REG_2: u32 = 2;
    pub const NFT_META_IIF: u32 = 4;
    pub const NFT_META_OIF: u32 = 5;
    pub const NFT_CT_STATE: u32 = 0;
    pub const NFT_CMP_EQ: u32 = 0;
    pub const NFT_CMP_NEQ: u32 = 1;
    /// NF_CT_STATE_BIT(IP_CT_ESTABLISHED) | NF_CT_STATE_BIT(IP_CT_RELATED).
    pub const ESTABLISHED_RELATED: u32 = (1 << 1) | (1 << 2);
    pub const NF_DROP: u32 = 0;
    pub const NF_ACCEPT: u32 = 1;
    pub const NF_INET_LOCAL_IN: u32 = 1;
    pub const NF_INET_FORWARD: u32 = 2;
    pub const NF_INET_LOCAL_OUT: u32 = 3;
    pub const NF_INET_POST_ROUTING: u32 = 4;
    /// NF_IP_PRI_NAT_SRC.
    pub const PRIORITY_SNAT: u32 = 100;
    pub const NFTA_META_SREG: u16 = 3;
    pub const NFT_META_MARK: u32 = 3;
    pub const NFT_META_L4PROTO: u32 = 16;
    pub const NFTA_PAYLOAD_DREG: u16 = 1;
    pub const NFTA_PAYLOAD_BASE: u16 = 2;
    pub const NFTA_PAYLOAD_OFFSET: u16 = 3;
    pub const NFTA_PAYLOAD_LEN: u16 = 4;
    pub const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
    pub const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
    pub const NFT_CMP_LTE: u32 = 3;
    pub const NFT_CMP_GTE: u32 = 5;
    pub const NFTA_NAT_TYPE: u16 = 1;
    pub const NFTA_NAT_FAMILY: u16 = 2;
    pub const NFTA_NAT_REG_ADDR_MIN: u16 = 3;
    pub const NFTA_NAT_REG_PROTO_MIN: u16 = 5;
    pub const NFT_NAT_SNAT: u32 = 0;
    pub const NFT_NAT_DNAT: u32 = 1;
    pub const NF_INET_PRE_ROUTING: u32 = 0;
    /// NF_IP_PRI_NAT_DST, -100, as the u32 nf_tables reads a priority as.
    pub const PRIORITY_DNAT: u32 = (-100i32) as u32;
    /// NF_IP_PRI_RAW, -300: before connection tracking, which `notrack` keeps a packet from.
    pub const PRIORITY_RAW: u32 = (-300i32) as u32;
    /// The fib expression (nf_tables.h): the oif a route lookup by the packet's source and
    /// arriving link gives, zero where none.
    pub const NFTA_FIB_DREG: u16 = 1;
    pub const NFTA_FIB_RESULT: u16 = 2;
    pub const NFTA_FIB_FLAGS: u16 = 3;
    pub const NFT_FIB_RESULT_OIF: u32 = 1;
    pub const NFTA_FIB_F_SADDR: u32 = 1;
    pub const NFTA_FIB_F_IIF: u32 = 8;
}

/// IPv6's address flag that skips duplicate address detection (linux/if_addr.h): each
/// link here has two ends, both of ours.
const IFA_F_NODAD: u8 = 0x02;
/// The switch's end of every gate's link, and the gate's, for IPv6: link-local (RFC 4291
/// §2.5.6), the same on each link, as a link-local address is the link's alone (D99).
const TRANSIT6_SWITCH: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
const TRANSIT6_GATE: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);

const TABLE: &[u8] = b"shards\0";

/// Appends a netlink attribute: length, type, value, padded to four bytes.
fn attr(buf: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let len = u16::try_from(4 + value.len()).unwrap_or(u16::MAX);
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(&kind.to_ne_bytes());
    buf.extend_from_slice(value);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// A nested attribute, its contents what `f` appends.
fn nested(buf: &mut Vec<u8>, kind: u16, f: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    f(&mut inner);
    attr(buf, kind | NLA_F_NESTED, &inner);
}

/// A u32 attribute in network order, as nf_tables reads its numbers.
fn be32(buf: &mut Vec<u8>, kind: u16, v: u32) {
    attr(buf, kind, &v.to_be_bytes());
}

fn ifinfomsg(index: i32, flags: u32, change: u32) -> Vec<u8> {
    let mut m = vec![0u8; 4];
    m.extend_from_slice(&index.to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&change.to_ne_bytes());
    m
}

fn netlink_socket(protocol: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just opened, owned from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Sends netlink messages, each `(type, flags, payload)`, as one write, and reads their
/// acknowledgments: the kernel answers a request in the send itself, so every answer is
/// there when it returns. The first error the kernel gave, if any.
fn exchange(sock: &OwnedFd, msgs: &[(u16, u16, Vec<u8>)]) -> io::Result<()> {
    let mut out = Vec::new();
    let mut asked = 0usize;
    for (seq, (kind, flags, payload)) in msgs.iter().enumerate() {
        let len =
            u32::try_from(16 + payload.len()).map_err(|_| io::Error::other("a netlink message too long"))?;
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(&flags.to_ne_bytes());
        out.extend_from_slice(&u32::try_from(seq + 1).unwrap_or(u32::MAX).to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(payload);
        if flags & NLM_F_ACK != 0 {
            asked += 1;
        }
    }
    // SAFETY: a buffer of the length given, to the kernel.
    if unsafe { libc::send(sock.as_raw_fd(), out.as_ptr().cast(), out.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut acked = 0usize;
    let mut buf = vec![0u8; 65536];
    while acked < asked {
        // SAFETY: a buffer of the length given; without waiting, as every answer is queued.
        let n = unsafe {
            libc::recv(
                sock.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        let n = match usize::try_from(n) {
            Ok(n) => n,
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Err(io::Error::other(format!(
                        "netlink acknowledged {acked} of {asked} requests"
                    )));
                }
                return Err(e);
            }
        };
        let mut at = 0usize;
        while let Some(head) = buf.get(at..at + 16).filter(|_| at < n) {
            let len = head
                .get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map_or(0, u32::from_ne_bytes) as usize;
            let kind = head
                .get(4..6)
                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                .map_or(0, u16::from_ne_bytes);
            if len < 16 {
                break;
            }
            if kind == NLMSG_ERROR {
                let code = buf
                    .get(at + 16..at + 20)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map_or(-libc::EIO, i32::from_ne_bytes);
                if code != 0 {
                    return Err(io::Error::from_raw_os_error(-code));
                }
                acked += 1;
            }
            at += (len + 3) & !3;
        }
    }
    Ok(())
}

/// Runs `f` on a thread of its own in the network namespace `ns`, or in a new one where
/// none is given; what it opens there, sockets keep (net/socket.c: a socket's namespace is
/// its creator's). Init's other threads stay where they are.
fn in_netns<T: Send>(ns: Option<RawFd>, f: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name("netns".into())
            .spawn_scoped(s, move || {
                // SAFETY: setns(2) or unshare(2) of this thread's network namespace alone.
                let rc = unsafe {
                    match ns {
                        Some(fd) => libc::setns(fd, libc::CLONE_NEWNET),
                        None => libc::unshare(libc::CLONE_NEWNET),
                    }
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                f()
            })?;
        handle
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("a network namespace's thread panicked")))
    })
}

fn open_ns(path: &str) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?
        .into())
}

/// The switch: its namespace, and a route and an nf_tables socket in it.
pub struct Switch {
    ns: OwnedFd,
    route: OwnedFd,
    /// Whether any domain has IPv6 (D99): the switch's own IPv6 tables, and forwarding.
    ipv6: bool,
    /// Whether the uplink carries IPv6: domains with egress take a default IPv6 route.
    uplink6: bool,
    /// A route socket in init's own namespace, where veths are made.
    here: OwnedFd,
    /// Whether it has a link past the microVM, which every domain's default route takes.
    uplink: bool,
    /// The gates' namespaces, which nothing else holds.
    gates: std::sync::Mutex<Vec<OwnedFd>>,
}

/// What a domain's gate lets through (D61): its domain's addresses; the flows it opens, to
/// each peer's addresses on the ports the peer accepts; those opened to it, from each
/// peer's addresses on its own; its ports past the microVM, out and in; and whether it
/// asks the agents' resolver.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    pub own: Vec<Ipv4Addr>,
    pub opens: Vec<(Vec<Ipv4Addr>, Vec<crate::netplan::Egress>)>,
    pub accepts: Vec<(Vec<Ipv4Addr>, Vec<crate::netplan::Egress>)>,
    pub egress: Vec<crate::netplan::Egress>,
    pub ingress: Vec<crate::netplan::Egress>,
    pub dns: bool,
    /// Its IPv6 addresses, and its peers' (D99): flows within the microVM alone.
    pub own6: Vec<Ipv6Addr>,
    pub opens6: Vec<(Vec<Ipv6Addr>, Vec<crate::netplan::Egress>)>,
    pub accepts6: Vec<(Vec<Ipv6Addr>, Vec<crate::netplan::Egress>)>,
    /// Whether its egress ports reach past the microVM by IPv6 too.
    pub egress6: bool,
}

impl Gate {
    /// Every port others open flows to it on, of each protocol.
    fn accepted(&self) -> Vec<crate::netplan::Egress> {
        let mut out: Vec<crate::netplan::Egress> = self.ingress.clone();
        for (_, ranges) in &self.accepts {
            out.extend(ranges.iter().copied());
        }
        out
    }
}

impl Switch {
    /// Makes the switch: forwarding on, and nf_tables that drop all but the answers to
    /// what is allowed, `pairs`, each `(from, to)` by the domains' indices on the ports the
    /// receiver accepts, each of `dns` asking the microVM's resolver, and each
    /// domain's `egress` past the microVM, through `uplink`.
    pub fn new(
        pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
        egress: &[(usize, Vec<crate::netplan::Egress>)],
        ingress: &[(usize, Vec<crate::netplan::Egress>)],
        dns: &[usize],
        uplink: Option<&Uplink>,
        ipv6: bool,
    ) -> io::Result<Switch> {
        let (ns, route, nftables) = in_netns(None, || {
            let ns = open_ns("/proc/thread-self/ns/net")?;
            let route = netlink_socket(libc::NETLINK_ROUTE)?;
            let nftables = netlink_socket(libc::NETLINK_NETFILTER)?;
            crate::run::loopback_up()?;
            // The thread's namespace's: /proc/sys/net is looked up in the caller's
            // (net/sysctl_net.c), through the handle init kept of /proc/sys, which is
            // read-only once the guest is set up.
            crate::setup::write_sysctl("net.ipv4.ip_forward", "1").map_err(io::Error::other)?;
            // Strict reverse-path filtering (RFC 3704 §2.2), for every link made from now
            // on: each domain's addresses are routed by its own link alone, so a packet on
            // d<x> from an address not x's is dropped by the kernel, whatever an agent sends.
            for scope in ["all", "default"] {
                crate::setup::write_sysctl(&format!("net.ipv4.conf.{scope}.rp_filter"), "1")
                    .map_err(io::Error::other)?;
            }
            Ok((ns, route, nftables))
        })?;
        policy(&nftables, pairs, egress, ingress, dns)?;
        // IPv6 between domains (D99): its tables first, then forwarding, so that a kernel
        // without nf_tables' ip6 family fails the run rather than forward unfiltered.
        let uplink6 = uplink.is_some_and(|u| u.ipv6.is_some());
        if ipv6 {
            let egress6: &[(usize, Vec<crate::netplan::Egress>)] = if uplink6 { egress } else { &[] };
            policy6(&nftables, pairs, egress6)?;
            in_netns(Some(ns.as_raw_fd()), || {
                crate::setup::write_sysctl("net.ipv6.conf.all.forwarding", "1").map_err(io::Error::other)
            })?;
        }
        let switch = Switch {
            ns,
            route,
            ipv6,
            uplink6,
            here: netlink_socket(libc::NETLINK_ROUTE)?,
            uplink: uplink.is_some(),
            gates: std::sync::Mutex::new(Vec::new()),
        };
        if let Some(u) = uplink {
            switch.up(u)?;
        }
        Ok(switch)
    }

    /// The link past the microVM: `agents0` in init's namespace, `up0` in the switch's,
    /// the switch's default route up it and init's routes to the agents' subnets down it;
    /// init's namespace forwarding what comes up to eth0, under eth0's address, and
    /// nothing else; and the run's own processes kept to what they reached before.
    fn up(&self, u: &Uplink) -> io::Result<()> {
        let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
        let mut m = ifinfomsg(UPLINK_IFINDEX, 0, 0);
        attr(&mut m, IFLA_IFNAME, b"agents0\0");
        let switch_fd = self.ns.as_raw_fd() as u32;
        nested(&mut m, IFLA_LINKINFO, |li| {
            attr(li, IFLA_INFO_KIND, b"veth");
            nested(li, IFLA_INFO_DATA, |data| {
                nested(data, VETH_INFO_PEER, |peer| {
                    peer.extend_from_slice(&ifinfomsg(UPLINK_IFINDEX, 0, 0));
                    attr(peer, IFLA_IFNAME, b"up0\0");
                    attr(peer, IFLA_NET_NS_FD, &switch_fd.to_ne_bytes());
                });
            });
        });
        exchange(&self.here, &[(RTM_NEWLINK, create, m)])?;
        let up = libc::IFF_UP as u32;
        let link_up = |i| (RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, ifinfomsg(i, up, up));
        exchange(
            &self.route,
            &[
                link_up(UPLINK_IFINDEX),
                (
                    RTM_NEWADDR,
                    create,
                    addr_msg_prefix(UPLINK_IFINDEX, UPLINK_SWITCH, UPLINK_PREFIX),
                ),
            ],
        )?;
        exchange(
            &self.route,
            &[(
                RTM_NEWROUTE,
                create,
                route_msg(UPLINK_IFINDEX, Ipv4Addr::UNSPECIFIED, 0, Some(UPLINK_INIT), None),
            )],
        )?;
        let mut here = vec![
            link_up(UPLINK_IFINDEX),
            (
                RTM_NEWADDR,
                create,
                addr_msg_prefix(UPLINK_IFINDEX, UPLINK_INIT, UPLINK_PREFIX),
            ),
        ];
        for &(subnet, prefix) in &u.subnets {
            here.push((
                RTM_NEWROUTE,
                create,
                route_msg(UPLINK_IFINDEX, subnet, prefix, Some(UPLINK_SWITCH), None),
            ));
        }
        let (first, rest) = here.split_at(1);
        exchange(&self.here, first)?;
        exchange(&self.here, rest)?;
        crate::setup::write_sysctl("net.ipv4.ip_forward", "1").map_err(io::Error::other)?;
        // SAFETY: if_nametoindex(3) of a NUL-terminated literal.
        let eth0 = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
        if eth0 == 0 {
            return Err(io::Error::other("the microVM has no eth0 for its agents' egress"));
        }
        outside(&netlink_socket(libc::NETLINK_NETFILTER)?, eth0, u)?;
        // IPv6 past the microVM (D99): the uplink's link-local ends, the switch's default
        // route up it and init's routes to the agents' IPv6 subnets down it; init's IPv6
        // tables, then its IPv6 forwarding.
        let Some(six) = &u.ipv6 else { return Ok(()) };
        exchange(
            &self.route,
            &[
                (RTM_NEWADDR, create, addr6_msg(UPLINK_IFINDEX, TRANSIT6_GATE, 64)),
                (
                    RTM_NEWROUTE,
                    create,
                    route6_msg(
                        UPLINK_IFINDEX,
                        Ipv6Addr::UNSPECIFIED,
                        0,
                        Some(TRANSIT6_SWITCH),
                        None,
                    ),
                ),
            ],
        )?;
        let mut here = vec![(
            RTM_NEWADDR,
            create,
            addr6_msg(UPLINK_IFINDEX, TRANSIT6_SWITCH, 64),
        )];
        for &(subnet, prefix) in &six.subnets {
            here.push((
                RTM_NEWROUTE,
                create,
                route6_msg(UPLINK_IFINDEX, subnet, prefix, Some(TRANSIT6_GATE), None),
            ));
        }
        exchange(&self.here, &here)?;
        outside6(&netlink_socket(libc::NETLINK_NETFILTER)?, eth0, six)?;
        crate::setup::write_sysctl("net.ipv6.conf.all.forwarding", "1").map_err(io::Error::other)
    }

    /// The agents' resolver's sockets, of the switch's namespace (`agentdns`): port 53 of
    /// every switch address, which each agent asks at its gateway, by UDP and by TCP; one
    /// connected to `upstream`, the microVM's resolver; and the namespace, which its TCP
    /// connections upstream are made in.
    pub fn resolver_sockets(&self, upstream: std::net::SocketAddr) -> io::Result<crate::agentdns::Sockets> {
        in_netns(Some(self.ns.as_raw_fd()), || {
            let listen = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 53))?;
            let up = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            up.connect(upstream)?;
            let tcp = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 53))?;
            Ok(crate::agentdns::Sockets {
                listen,
                upstream: up,
                tcp,
                ns: self.ns.try_clone()?,
            })
        })
    }

    /// Links the `n`-th domain, its first process `pid`, as `link` says, through a gate
    /// of its own that lets through what `gate` says.
    pub fn attach(&self, n: usize, pid: libc::pid_t, link: &Link, gate: &Gate) -> io::Result<()> {
        let index =
            link_index(n).and_then(|i| i32::try_from(i).map_err(|_| io::Error::other("too many domains")))?;
        let transit = transit_gate(n)?;
        let domain_ns = open_ns(&format!("/proc/{pid}/ns/net"))?;
        // The gate's namespace: forwarding on, strict reverse-path filtering, its own
        // tables, and loopback.
        let (gate_ns, gate_route, gate_nft) = in_netns(None, || {
            let ns = open_ns("/proc/thread-self/ns/net")?;
            let route = netlink_socket(libc::NETLINK_ROUTE)?;
            let nftables = netlink_socket(libc::NETLINK_NETFILTER)?;
            crate::run::loopback_up()?;
            crate::setup::write_sysctl("net.ipv4.ip_forward", "1").map_err(io::Error::other)?;
            for scope in ["all", "default"] {
                crate::setup::write_sysctl(&format!("net.ipv4.conf.{scope}.rp_filter"), "1")
                    .map_err(io::Error::other)?;
            }
            Ok((ns, route, nftables))
        })?;
        let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
        // A veth whose ends are `a` (index `ai`) in namespace `an` and `b` in `bn`.
        let veth = |ai: i32, a: &[u8], an: RawFd, bi: i32, b: &[u8], bn: RawFd| {
            let mut m = ifinfomsg(ai, 0, 0);
            attr(&mut m, IFLA_IFNAME, a);
            attr(&mut m, IFLA_NET_NS_FD, &(an as u32).to_ne_bytes());
            nested(&mut m, IFLA_LINKINFO, |li| {
                attr(li, IFLA_INFO_KIND, b"veth");
                nested(li, IFLA_INFO_DATA, |data| {
                    nested(data, VETH_INFO_PEER, |peer| {
                        peer.extend_from_slice(&ifinfomsg(bi, 0, 0));
                        attr(peer, IFLA_IFNAME, b);
                        attr(peer, IFLA_NET_NS_FD, &(bn as u32).to_ne_bytes());
                    });
                });
            });
            (RTM_NEWLINK, create, m)
        };
        let switch_name = format!("d{n}\0");
        exchange(
            &self.here,
            &[
                veth(
                    DOMAIN_IFINDEX,
                    b"eth0\0",
                    domain_ns.as_raw_fd(),
                    GATE_IN,
                    b"in0\0",
                    gate_ns.as_raw_fd(),
                ),
                veth(
                    GATE_OUT,
                    b"out0\0",
                    gate_ns.as_raw_fd(),
                    index,
                    switch_name.as_bytes(),
                    self.ns.as_raw_fd(),
                ),
            ],
        )?;
        let up = libc::IFF_UP as u32;
        let link_up = |i| (RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, ifinfomsg(i, up, up));
        // The domain's side: its addresses, each a host's, and its subnets through their
        // gateways, which are its gate's; past the microVM through its first one, where
        // there is a way; the agents' resolver through it, where a grant gives it names.
        let mut ours = vec![link_up(DOMAIN_IFINDEX)];
        let mut gateways: Vec<Ipv4Addr> = Vec::new();
        for a in &link.addresses {
            ours.push((RTM_NEWADDR, create, addr_msg(DOMAIN_IFINDEX, a.addr)));
            ours.push((
                RTM_NEWROUTE,
                create,
                route_msg(DOMAIN_IFINDEX, a.subnet, a.prefix, Some(a.gateway), Some(a.addr)),
            ));
            if !gateways.contains(&a.gateway) {
                gateways.push(a.gateway);
            }
        }
        if let Some(a) = link.addresses.first() {
            if self.uplink {
                ours.push((
                    RTM_NEWROUTE,
                    create,
                    route_msg(
                        DOMAIN_IFINDEX,
                        Ipv4Addr::UNSPECIFIED,
                        0,
                        Some(a.gateway),
                        Some(a.addr),
                    ),
                ));
            } else if gate.dns {
                ours.push((
                    RTM_NEWROUTE,
                    create,
                    route_msg(DOMAIN_IFINDEX, TRANSIT_SWITCH, 32, Some(a.gateway), Some(a.addr)),
                ));
            }
        }
        // The gate's: the gateways on in0, its end of the transit on out0; its domain's
        // addresses down in0, everything else up out0 to the switch.
        let mut gates = vec![link_up(GATE_IN), link_up(GATE_OUT)];
        for g in &gateways {
            gates.push((RTM_NEWADDR, create, addr_msg(GATE_IN, *g)));
        }
        gates.push((RTM_NEWADDR, create, addr_msg(GATE_OUT, transit)));
        for a in &link.addresses {
            gates.push((RTM_NEWROUTE, create, route_msg(GATE_IN, a.addr, 32, None, None)));
        }
        gates.push((
            RTM_NEWROUTE,
            create,
            route_msg(GATE_OUT, Ipv4Addr::UNSPECIFIED, 0, Some(TRANSIT_SWITCH), None),
        ));
        // The switch's: its end of the transit on d<n>, and the domain's addresses through
        // the gate.
        let mut theirs = vec![
            link_up(index),
            (RTM_NEWADDR, create, addr_msg(index, TRANSIT_SWITCH)),
            (RTM_NEWROUTE, create, route_msg(index, transit, 32, None, None)),
        ];
        for a in &link.addresses {
            theirs.push((
                RTM_NEWROUTE,
                create,
                route_msg(index, a.addr, 32, Some(transit), None),
            ));
        }
        // IPv6 (D99): the domain's addresses and subnets through their gateways, the gate's
        // gateways on in0 and its link-local end on out0, its default route up to the
        // switch's; the switch's link-local end on d<n> and the domain's addresses through
        // the gate. No duplicate address detection: each link has two ends, both ours.
        if !link.addresses6.is_empty() && !self.ipv6 {
            return Err(io::Error::other("a domain with IPv6 on a switch made without it"));
        }
        let mut gateways6: Vec<Ipv6Addr> = Vec::new();
        for a in &link.addresses6 {
            ours.push((RTM_NEWADDR, create, addr6_msg(DOMAIN_IFINDEX, a.addr, 128)));
            ours.push((
                RTM_NEWROUTE,
                create,
                route6_msg(DOMAIN_IFINDEX, a.subnet, a.prefix, Some(a.gateway), Some(a.addr)),
            ));
            if !gateways6.contains(&a.gateway) {
                gateways6.push(a.gateway);
            }
            gates.push((RTM_NEWROUTE, create, route6_msg(GATE_IN, a.addr, 128, None, None)));
            theirs.push((
                RTM_NEWROUTE,
                create,
                route6_msg(index, a.addr, 128, Some(TRANSIT6_GATE), None),
            ));
        }
        // Past the microVM by IPv6, where the uplink carries it and the domain has egress.
        if let Some(a) = link.addresses6.first()
            && self.uplink6
            && !gate.egress.is_empty()
        {
            ours.push((
                RTM_NEWROUTE,
                create,
                route6_msg(
                    DOMAIN_IFINDEX,
                    Ipv6Addr::UNSPECIFIED,
                    0,
                    Some(a.gateway),
                    Some(a.addr),
                ),
            ));
        }
        if !link.addresses6.is_empty() {
            for g in &gateways6 {
                gates.push((RTM_NEWADDR, create, addr6_msg(GATE_IN, *g, 128)));
            }
            gates.push((RTM_NEWADDR, create, addr6_msg(GATE_OUT, TRANSIT6_GATE, 64)));
            gates.push((
                RTM_NEWROUTE,
                create,
                route6_msg(GATE_OUT, Ipv6Addr::UNSPECIFIED, 0, Some(TRANSIT6_SWITCH), None),
            ));
            theirs.push((RTM_NEWADDR, create, addr6_msg(index, TRANSIT6_SWITCH, 64)));
        }
        // Up before its routes: a route through a link that is down is refused.
        let domain_route = in_netns(Some(domain_ns.as_raw_fd()), || {
            // Its own connections' ports none of those it accepts, so that what answers
            // them is tracked by its gate (D61).
            if let Some((lo, hi)) = local_ports(&gate.accepted()) {
                crate::setup::write_sysctl("net.ipv4.ip_local_port_range", &format!("{lo} {hi}"))
                    .map_err(io::Error::other)?;
            }
            netlink_socket(libc::NETLINK_ROUTE)
        })?;
        let (up_ours, rest_ours) = ours.split_at(1);
        let (up_gates, rest_gates) = gates.split_at(2);
        let (up_theirs, rest_theirs) = theirs.split_at(1);
        exchange(&domain_route, up_ours)?;
        exchange(&gate_route, up_gates)?;
        exchange(&self.route, up_theirs)?;
        exchange(&gate_route, rest_gates)?;
        exchange(&self.route, rest_theirs)?;
        exchange(&domain_route, rest_ours)?;
        gate_policy(&gate_nft, gate)?;
        // IPv6's tables, then its forwarding, as the switch's (D99).
        if !link.addresses6.is_empty() {
            gate_policy6(&gate_nft, gate)?;
            in_netns(Some(gate_ns.as_raw_fd()), || {
                crate::setup::write_sysctl("net.ipv6.conf.all.forwarding", "1").map_err(io::Error::other)
            })?;
        }
        match self.gates.lock() {
            Ok(mut g) => g.push(gate_ns),
            Err(poisoned) => poisoned.into_inner().push(gate_ns),
        }
        Ok(())
    }
}

/// The ports a domain's own connections may take, none of `accepted`: the kernel's default
/// range (32768 to 60999, net/ipv4/af_inet.c) where none falls in it, else the widest run
/// past 1023 that none does. None where the default stands.
fn local_ports(accepted: &[crate::netplan::Egress]) -> Option<(u16, u16)> {
    let clear = |lo: u16, hi: u16| accepted.iter().all(|&(_, a, b)| b < lo || a > hi);
    if clear(32768, 60999) {
        return None;
    }
    let mut taken: Vec<(u16, u16)> = accepted.iter().map(|&(_, a, b)| (a, b)).collect();
    taken.sort_unstable();
    let mut best: Option<(u16, u16)> = None;
    let mut from: u32 = 1024;
    for (a, b) in taken.iter().copied().chain([(u16::MAX, u16::MAX)]) {
        let to = u32::from(a).saturating_sub(1).min(65535);
        if to >= from && best.is_none_or(|(l, h)| to - from > u32::from(h - l)) {
            best = Some((u16::try_from(from).ok()?, u16::try_from(to).ok()?));
        }
        from = from.max(u32::from(b) + 1);
    }
    best
}

fn addr_msg(index: i32, addr: Ipv4Addr) -> Vec<u8> {
    addr_msg_prefix(index, addr, 32)
}

fn addr_msg_prefix(index: i32, addr: Ipv4Addr, prefix: u8) -> Vec<u8> {
    // struct ifaddrmsg: family, prefix length, flags, scope, index.
    let mut m = vec![libc::AF_INET as u8, prefix, 0, RT_SCOPE_UNIVERSE];
    m.extend_from_slice(&(index as u32).to_ne_bytes());
    attr(&mut m, IFA_LOCAL, &addr.octets());
    attr(&mut m, IFA_ADDRESS, &addr.octets());
    m
}

/// An IPv6 address `addr/prefix` on link `index`, with no duplicate address detection.
fn addr6_msg(index: i32, addr: Ipv6Addr, prefix: u8) -> Vec<u8> {
    // struct ifaddrmsg: family, prefix length, flags, scope, index.
    let mut m = vec![libc::AF_INET6 as u8, prefix, IFA_F_NODAD, RT_SCOPE_UNIVERSE];
    m.extend_from_slice(&(index as u32).to_ne_bytes());
    attr(&mut m, IFA_LOCAL, &addr.octets());
    attr(&mut m, IFA_ADDRESS, &addr.octets());
    m
}

/// An IPv6 route to `dst/prefix` on link `index`, as [`route_msg`] makes IPv4's.
fn route6_msg(
    index: i32,
    dst: Ipv6Addr,
    prefix: u8,
    gateway: Option<Ipv6Addr>,
    source: Option<Ipv6Addr>,
) -> Vec<u8> {
    let (scope, flags) = match gateway {
        Some(_) => (RT_SCOPE_UNIVERSE, RTNH_F_ONLINK),
        None => (RT_SCOPE_LINK, 0),
    };
    let mut m = vec![
        libc::AF_INET6 as u8,
        prefix,
        0,
        0,
        RT_TABLE_MAIN,
        RTPROT_BOOT,
        scope,
        RTN_UNICAST,
    ];
    m.extend_from_slice(&flags.to_ne_bytes());
    attr(&mut m, RTA_DST, &dst.octets());
    if let Some(g) = gateway {
        attr(&mut m, RTA_GATEWAY, &g.octets());
    }
    if let Some(s) = source {
        attr(&mut m, RTA_PREFSRC, &s.octets());
    }
    attr(&mut m, RTA_OIF, &(index as u32).to_ne_bytes());
    m
}

/// A route to `dst/prefix` on link `index`: through `gateway`, on the link though no
/// subnet of it holds the gateway, or else the link's own.
fn route_msg(
    index: i32,
    dst: Ipv4Addr,
    prefix: u8,
    gateway: Option<Ipv4Addr>,
    source: Option<Ipv4Addr>,
) -> Vec<u8> {
    let (scope, flags) = match gateway {
        Some(_) => (RT_SCOPE_UNIVERSE, RTNH_F_ONLINK),
        None => (RT_SCOPE_LINK, 0),
    };
    // struct rtmsg.
    let mut m = vec![
        libc::AF_INET as u8,
        prefix,
        0,
        0,
        RT_TABLE_MAIN,
        RTPROT_BOOT,
        scope,
        RTN_UNICAST,
    ];
    m.extend_from_slice(&flags.to_ne_bytes());
    attr(&mut m, RTA_DST, &dst.octets());
    if let Some(g) = gateway {
        attr(&mut m, RTA_GATEWAY, &g.octets());
    }
    if let Some(s) = source {
        attr(&mut m, RTA_PREFSRC, &s.octets());
    }
    attr(&mut m, RTA_OIF, &(index as u32).to_ne_bytes());
    m
}

/// One expression of a rule.
fn expr(list: &mut Vec<u8>, name: &[u8], data: impl FnOnce(&mut Vec<u8>)) {
    nested(list, nft::NFTA_LIST_ELEM, |e| {
        attr(e, nft::NFTA_EXPR_NAME, name);
        nested(e, nft::NFTA_EXPR_DATA, data);
    });
}

fn accept(list: &mut Vec<u8>) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_VERDICT);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            nested(v, nft::NFTA_DATA_VERDICT, |c| {
                be32(c, nft::NFTA_VERDICT_CODE, nft::NF_ACCEPT)
            });
        });
    });
}

fn compare(list: &mut Vec<u8>, op: u32, value: &[u8]) {
    expr(list, b"cmp\0", |d| {
        be32(d, nft::NFTA_CMP_SREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_CMP_OP, op);
        nested(d, nft::NFTA_CMP_DATA, |v| attr(v, nft::NFTA_DATA_VALUE, value));
    });
}

fn meta(list: &mut Vec<u8>, key: u32) {
    expr(list, b"meta\0", |d| {
        be32(d, nft::NFTA_META_DREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_META_KEY, key);
    });
}

/// The nf_tables message `kind`, of `family` (ip or ip6), with `attrs`.
fn nft_msg(family: u8, kind: u16, flags: u16, attrs: Vec<u8>) -> (u16, u16, Vec<u8>) {
    // struct nfgenmsg: family, version, resource id.
    let mut m = vec![family, 0, 0, 0];
    m.extend_from_slice(&attrs);
    (
        (nft::NFNL_SUBSYS_NFTABLES << 8) | kind,
        NLM_F_REQUEST | NLM_F_ACK | flags,
        m,
    )
}

/// One nf_tables transaction for table `shards`, of family ip (or ip6, [`Batch::of`]):
/// its chains, each with its rules (nf_tables_api.c commits a batch whole or not at all).
struct Batch {
    msgs: Vec<(u16, u16, Vec<u8>)>,
    family: u8,
}

impl Batch {
    fn new() -> Batch {
        Batch::of(nft::NFPROTO_IPV4)
    }

    fn of(family: u8) -> Batch {
        let mut b = Batch {
            msgs: vec![Batch::bound(nft::NFNL_MSG_BATCH_BEGIN)],
            family,
        };
        let mut t = Vec::new();
        attr(&mut t, nft::NFTA_TABLE_NAME, TABLE);
        b.msgs
            .push(nft_msg(family, nft::NFT_MSG_NEWTABLE, NLM_F_CREATE, t));
        b
    }

    /// A batch's bounds: res_id the subsystem, in network order (nfnetlink.c).
    fn bound(kind: u16) -> (u16, u16, Vec<u8>) {
        let mut m = vec![libc::AF_UNSPEC as u8, 0];
        m.extend_from_slice(&nft::NFNL_SUBSYS_NFTABLES.to_be_bytes());
        (kind, NLM_F_REQUEST, m)
    }

    /// A base chain on `hook`, of `kind` (`filter\0`, `nat\0`), its policy `policy`.
    fn chain(&mut self, name: &[u8], kind: &[u8], hook: u32, priority: u32, policy: u32) {
        let mut c = Vec::new();
        attr(&mut c, nft::NFTA_CHAIN_TABLE, TABLE);
        attr(&mut c, nft::NFTA_CHAIN_NAME, name);
        nested(&mut c, nft::NFTA_CHAIN_HOOK, |h| {
            be32(h, nft::NFTA_HOOK_HOOKNUM, hook);
            be32(h, nft::NFTA_HOOK_PRIORITY, priority);
        });
        be32(&mut c, nft::NFTA_CHAIN_POLICY, policy);
        attr(&mut c, nft::NFTA_CHAIN_TYPE, kind);
        self.msgs
            .push(nft_msg(self.family, nft::NFT_MSG_NEWCHAIN, NLM_F_CREATE, c));
    }

    fn rule(&mut self, chain: &[u8], exprs: Vec<u8>) {
        let mut r = Vec::new();
        attr(&mut r, nft::NFTA_RULE_TABLE, TABLE);
        attr(&mut r, nft::NFTA_RULE_CHAIN, chain);
        attr(&mut r, nft::NFTA_RULE_EXPRESSIONS | NLA_F_NESTED, &exprs);
        self.msgs.push(nft_msg(
            self.family,
            nft::NFT_MSG_NEWRULE,
            NLM_F_CREATE | NLM_F_APPEND,
            r,
        ));
    }

    fn commit(mut self, sock: &OwnedFd) -> io::Result<()> {
        self.msgs.push(Batch::bound(nft::NFNL_MSG_BATCH_END));
        exchange(sock, &self.msgs)
    }
}

/// `ct state established,related accept`.
/// The verdict drop.
fn drop_verdict(list: &mut Vec<u8>) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_VERDICT);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            nested(v, nft::NFTA_DATA_VERDICT, |c| {
                be32(c, nft::NFTA_VERDICT_CODE, nft::NF_DROP)
            });
        });
    });
}

fn answers() -> Vec<u8> {
    let mut e = Vec::new();
    expr(&mut e, b"ct\0", |d| {
        be32(d, nft::NFTA_CT_DREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_CT_KEY, nft::NFT_CT_STATE);
    });
    masked(&mut e, &nft::ESTABLISHED_RELATED.to_ne_bytes());
    compare(&mut e, nft::NFT_CMP_NEQ, &0u32.to_ne_bytes());
    accept(&mut e);
    e
}

/// Register 1, and-ed with `mask`.
fn masked(list: &mut Vec<u8>, mask: &[u8]) {
    let len = u32::try_from(mask.len()).unwrap_or(0);
    expr(list, b"bitwise\0", |d| {
        be32(d, nft::NFTA_BITWISE_SREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_BITWISE_DREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_BITWISE_LEN, len);
        nested(d, nft::NFTA_BITWISE_MASK, |v| attr(v, nft::NFTA_DATA_VALUE, mask));
        nested(d, nft::NFTA_BITWISE_XOR, |v| {
            attr(v, nft::NFTA_DATA_VALUE, &vec![0u8; mask.len()])
        });
    });
}

/// Register 1, `len` bytes of the packet at `offset` of header `base`.
fn payload(list: &mut Vec<u8>, base: u32, offset: u32, len: u32) {
    expr(list, b"payload\0", |d| {
        be32(d, nft::NFTA_PAYLOAD_DREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_PAYLOAD_BASE, base);
        be32(d, nft::NFTA_PAYLOAD_OFFSET, offset);
        be32(d, nft::NFTA_PAYLOAD_LEN, len);
    });
}

/// A protocol's ports: `meta l4proto`, then the destination port, at offset 2 of TCP's
/// header and UDP's alike, within the range, compared as the network-order bytes it is.
fn ports(list: &mut Vec<u8>, (proto, lo, hi): crate::netplan::Egress) {
    meta(list, nft::NFT_META_L4PROTO);
    compare(list, nft::NFT_CMP_EQ, &[proto]);
    payload(list, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
    compare(list, nft::NFT_CMP_GTE, &lo.to_be_bytes());
    compare(list, nft::NFT_CMP_LTE, &hi.to_be_bytes());
}

/// Register 1, `value`.
fn load(list: &mut Vec<u8>, value: &[u8]) {
    load_into(list, nft::NFT_REG_1, value);
}

/// Register `reg`, `value`.
fn load_into(list: &mut Vec<u8>, reg: u32, value: &[u8]) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, reg);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            attr(v, nft::NFTA_DATA_VALUE, value)
        });
    });
}

/// The switch's end of the n-th domain's link, `d<n>`, by interface index.
pub(crate) fn link_index(n: usize) -> io::Result<u32> {
    let n = u32::try_from(n).map_err(|_| io::Error::other("too many domains"))?;
    (SWITCH_IFINDEX as u32)
        .checked_add(n)
        .ok_or_else(|| io::Error::other("too many domains"))
}

/// A protocol's source ports, as [`ports`] the destination's: at offset 0 of TCP's
/// header and UDP's alike.
fn source_ports(list: &mut Vec<u8>, (proto, lo, hi): crate::netplan::Egress) {
    meta(list, nft::NFT_META_L4PROTO);
    compare(list, nft::NFT_CMP_EQ, &[proto]);
    payload(list, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 0, 2);
    compare(list, nft::NFT_CMP_GTE, &lo.to_be_bytes());
    compare(list, nft::NFT_CMP_LTE, &hi.to_be_bytes());
}

/// The packet's source (offset 12 of IPv4's header) or destination (16) address, `addr`.
fn address(list: &mut Vec<u8>, offset: u32, addr: Ipv4Addr) {
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 4);
    compare(list, nft::NFT_CMP_EQ, &addr.octets());
}
const SOURCE: u32 = 12;
const DESTINATION: u32 = 16;

/// The packet's IPv6 source (offset 8 of its header) or destination (24) address.
fn address6(list: &mut Vec<u8>, offset: u32, addr: Ipv6Addr) {
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 16);
    compare(list, nft::NFT_CMP_EQ, &addr.octets());
}
const SOURCE6: u32 = 8;
const DESTINATION6: u32 = 24;

/// Neighbor discovery (RFC 4861: router and neighbor solicitations and advertisements,
/// redirects, ICMPv6 types 133 to 137), of hop limit 255 alone (§7.1.1), which a link's
/// two ends need to reach each other: accepted.
fn neighbor_discovery() -> Vec<u8> {
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_L4PROTO);
    compare(&mut e, nft::NFT_CMP_EQ, &[58]);
    payload(&mut e, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 0, 1);
    compare(&mut e, nft::NFT_CMP_GTE, &[133]);
    compare(&mut e, nft::NFT_CMP_LTE, &[137]);
    payload(&mut e, nft::NFT_PAYLOAD_NETWORK_HEADER, 7, 1);
    compare(&mut e, nft::NFT_CMP_EQ, &[255]);
    accept(&mut e);
    e
}

/// Strict reverse-path filtering for IPv6 (RFC 3704 §2.2), which has no rp_filter
/// sysctl: a packet whose source no route reaches through the link it came on is dropped
/// (nft's `fib saddr . iif oif missing drop`).
fn reverse_path() -> Vec<u8> {
    let mut e = Vec::new();
    expr(&mut e, b"fib\0", |d| {
        be32(d, nft::NFTA_FIB_DREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_FIB_RESULT, nft::NFT_FIB_RESULT_OIF);
        be32(
            d,
            nft::NFTA_FIB_FLAGS,
            nft::NFTA_FIB_F_SADDR | nft::NFTA_FIB_F_IIF,
        );
    });
    compare(&mut e, nft::NFT_CMP_EQ, &0u32.to_ne_bytes());
    drop_verdict(&mut e);
    e
}

/// The arriving (`NFT_META_IIF`) or leaving (`NFT_META_OIF`) link, `index`.
fn on(list: &mut Vec<u8>, key: u32, index: u32) {
    meta(list, key);
    compare(list, nft::NFT_CMP_EQ, &index.to_ne_bytes());
}

/// The switch's nf_tables, in one transaction, and none of them tracking anything
/// (D61): chain `input`, which drops all but the agents' questions to its resolver, from
/// each domain granted names, and its resolver's answers from the microVM's; chain
/// `forward`, which drops all but, for each grant, its flows one way on its ports and
/// their answers the other: each pair's, from the first's link to the second's; each
/// domain's past the microVM, up to the microVM's link; and what comes in to each, down.
/// Who opened a flow, and whether an answer answers one, each domain's gate knows.
fn policy(
    sock: &OwnedFd,
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    egress: &[(usize, Vec<crate::netplan::Egress>)],
    ingress: &[(usize, Vec<crate::netplan::Egress>)],
    dns: &[usize],
) -> io::Result<()> {
    let up = UPLINK_IFINDEX as u32;
    let mut b = Batch::new();
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    // What the microVM's resolver answers the agents' resolver, and the agents'
    // questions, by UDP and by TCP, for answers too long for UDP (RFC 7766).
    for proto in [17, 6] {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, up);
        source_ports(&mut e, (proto, 53, 53));
        accept(&mut e);
        b.rule(b"input\0", e);
        for n in dns {
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, link_index(*n)?);
            ports(&mut e, (proto, 53, 53));
            accept(&mut e);
            b.rule(b"input\0", e);
        }
    }
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    // A flow from `from` to `to` on `range`, and its answers back.
    let both = |b: &mut Batch, from: u32, to: u32, range: crate::netplan::Egress| {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, from);
        on(&mut e, nft::NFT_META_OIF, to);
        ports(&mut e, range);
        accept(&mut e);
        b.rule(b"forward\0", e);
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, to);
        on(&mut e, nft::NFT_META_OIF, from);
        source_ports(&mut e, range);
        accept(&mut e);
        b.rule(b"forward\0", e);
    };
    for (from, to, ranges) in pairs {
        for &range in ranges {
            both(&mut b, link_index(*from)?, link_index(*to)?, range);
        }
    }
    for (n, ranges) in egress {
        for &range in ranges {
            both(&mut b, link_index(*n)?, up, range);
        }
    }
    for (n, ranges) in ingress {
        for &range in ranges {
            both(&mut b, up, link_index(*n)?, range);
        }
    }
    b.commit(sock)
}

/// A domain's gate's nf_tables (D61), in one transaction:
///
/// - `raw`, before tracking: the flows others open to the domain, and its answers to them,
///   kept from its gate's table (`notrack`), so that a peer opening many takes none of its
///   room;
/// - `forward`, which drops all but the answers of what the domain opened (tracked here
///   alone); what it opens, from its own addresses: to each peer on the ports the peer
///   accepts, past the microVM on its egress ports, and to the agents' resolver where
///   granted; what peers open to it on its ports, from their own addresses, and from past
///   the microVM on its ingress ports; and its answers to those.
fn gate_policy(sock: &OwnedFd, g: &Gate) -> io::Result<()> {
    let (inside, outside) = (GATE_IN as u32, GATE_OUT as u32);
    let mut b = Batch::new();
    b.chain(
        b"raw\0",
        b"filter\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_RAW,
        nft::NF_ACCEPT,
    );
    for range in g.accepted() {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, outside);
        ports(&mut e, range);
        expr(&mut e, b"notrack\0", |_| {});
        b.rule(b"raw\0", e);
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, inside);
        source_ports(&mut e, range);
        expr(&mut e, b"notrack\0", |_| {});
        b.rule(b"raw\0", e);
    }
    // The gate itself serves nothing and says nothing: what is addressed to its gateways
    // is dropped, not refused, which would answer.
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    b.chain(b"output\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_DROP);
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    for &own in &g.own {
        // What it opens: from its own address, in from its domain.
        let opens = |b: &mut Batch, to: Option<Ipv4Addr>, range: crate::netplan::Egress| {
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, inside);
            address(&mut e, SOURCE, own);
            if let Some(to) = to {
                address(&mut e, DESTINATION, to);
            }
            ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
        };
        for (peer, ranges) in &g.opens {
            for &to in peer {
                for &range in ranges {
                    opens(&mut b, Some(to), range);
                }
            }
        }
        for &range in &g.egress {
            opens(&mut b, None, range);
        }
        if g.dns {
            opens(&mut b, Some(TRANSIT_SWITCH), (17, 53, 53));
            opens(&mut b, Some(TRANSIT_SWITCH), (6, 53, 53));
        }
        // What is opened to it, from a peer's address or past the microVM, and its answers
        // back, untracked.
        let opened = |b: &mut Batch, from: Option<Ipv4Addr>, range: crate::netplan::Egress| {
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, outside);
            if let Some(from) = from {
                address(&mut e, SOURCE, from);
            }
            address(&mut e, DESTINATION, own);
            ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, inside);
            address(&mut e, SOURCE, own);
            if let Some(from) = from {
                address(&mut e, DESTINATION, from);
            }
            source_ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
        };
        for (peer, ranges) in &g.accepts {
            for &from in peer {
                for &range in ranges {
                    opened(&mut b, Some(from), range);
                }
            }
        }
        for &range in &g.ingress {
            opened(&mut b, None, range);
        }
    }
    b.commit(sock)
}

/// The switch's IPv6 tables (D99), stateless as its IPv4 ones: its own address answers
/// neighbor discovery alone, reverse paths are strict, and it forwards each pair's flows,
/// and each domain's past the microVM where the uplink carries IPv6 (`egress`), one way
/// on their ports and their answers back, and nothing else.
fn policy6(
    sock: &OwnedFd,
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    egress: &[(usize, Vec<crate::netplan::Egress>)],
) -> io::Result<()> {
    let mut b = Batch::of(nft::NFPROTO_IPV6);
    b.chain(b"pre\0", b"filter\0", nft::NF_INET_PRE_ROUTING, 0, nft::NF_ACCEPT);
    b.rule(b"pre\0", reverse_path());
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    b.rule(b"input\0", neighbor_discovery());
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    let up = UPLINK_IFINDEX as u32;
    let mut flows: Vec<(u32, u32, Vec<crate::netplan::Egress>)> = Vec::new();
    for (from, to, ranges) in pairs {
        flows.push((link_index(*from)?, link_index(*to)?, ranges.clone()));
    }
    // Each domain's past the microVM, up to the uplink (D99).
    for (n, ranges) in egress {
        flows.push((link_index(*n)?, up, ranges.clone()));
    }
    for (from, to, ranges) in &flows {
        let (from, to) = (*from, *to);
        for &range in ranges {
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, from);
            on(&mut e, nft::NFT_META_OIF, to);
            ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, to);
            on(&mut e, nft::NFT_META_OIF, from);
            source_ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
        }
    }
    b.commit(sock)
}

/// A domain's gate's IPv6 tables (D99), as its IPv4 ones ([`gate_policy`]) within the
/// microVM: what others open to it kept from its table, its own address alone answering
/// neighbor discovery, reverse paths strict, and forwarded: answers to what it opened, what
/// it opens from its own addresses to each peer's on the peer's ports, and what peers open
/// to it from theirs on its ports, with its answers.
fn gate_policy6(sock: &OwnedFd, g: &Gate) -> io::Result<()> {
    let (inside, outside) = (GATE_IN as u32, GATE_OUT as u32);
    let mut b = Batch::of(nft::NFPROTO_IPV6);
    b.chain(
        b"raw\0",
        b"filter\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_RAW,
        nft::NF_ACCEPT,
    );
    b.rule(b"raw\0", reverse_path());
    for range in g.accepted() {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, outside);
        ports(&mut e, range);
        expr(&mut e, b"notrack\0", |_| {});
        b.rule(b"raw\0", e);
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, inside);
        source_ports(&mut e, range);
        expr(&mut e, b"notrack\0", |_| {});
        b.rule(b"raw\0", e);
    }
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    b.rule(b"input\0", neighbor_discovery());
    b.chain(b"output\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_DROP);
    b.rule(b"output\0", neighbor_discovery());
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    for &own in &g.own6 {
        // Past the microVM, on its egress ports, where the uplink carries IPv6.
        if g.egress6 {
            for &range in &g.egress {
                let mut e = Vec::new();
                on(&mut e, nft::NFT_META_IIF, inside);
                address6(&mut e, SOURCE6, own);
                ports(&mut e, range);
                accept(&mut e);
                b.rule(b"forward\0", e);
            }
        }
        for (peer, ranges) in &g.opens6 {
            for &to in peer {
                for &range in ranges {
                    let mut e = Vec::new();
                    on(&mut e, nft::NFT_META_IIF, inside);
                    address6(&mut e, SOURCE6, own);
                    address6(&mut e, DESTINATION6, to);
                    ports(&mut e, range);
                    accept(&mut e);
                    b.rule(b"forward\0", e);
                }
            }
        }
        for (peer, ranges) in &g.accepts6 {
            for &from in peer {
                for &range in ranges {
                    let mut e = Vec::new();
                    on(&mut e, nft::NFT_META_IIF, outside);
                    address6(&mut e, SOURCE6, from);
                    address6(&mut e, DESTINATION6, own);
                    ports(&mut e, range);
                    accept(&mut e);
                    b.rule(b"forward\0", e);
                    let mut e = Vec::new();
                    on(&mut e, nft::NFT_META_IIF, inside);
                    address6(&mut e, SOURCE6, own);
                    address6(&mut e, DESTINATION6, from);
                    source_ports(&mut e, range);
                    accept(&mut e);
                    b.rule(b"forward\0", e);
                }
            }
        }
    }
    b.commit(sock)
}

/// Init's own namespace's IPv6 tables, once agents reach past the microVM by IPv6 (D99), as
/// [`outside`] makes IPv4's: `in` drops what comes up from the switch but answers and
/// neighbor discovery; `forward` drops all but answers and each domain's egress, from its
/// IPv6 addresses on its ports, up to eth0, which it marks; `post` gives what is marked
/// eth0's IPv6 address, the one the network process takes frames from.
fn outside6(sock: &OwnedFd, eth0: u32, u: &Uplink6) -> io::Result<()> {
    let up = UPLINK_IFINDEX as u32;
    let mut b = Batch::of(nft::NFPROTO_IPV6);
    b.chain(b"in\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_ACCEPT);
    b.rule(b"in\0", answers());
    b.rule(b"in\0", neighbor_discovery());
    let mut e = Vec::new();
    on(&mut e, nft::NFT_META_IIF, up);
    drop_verdict(&mut e);
    b.rule(b"in\0", e);
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    for (from, ranges) in &u.egress {
        for &a in from {
            for &range in ranges {
                let mut e = Vec::new();
                on(&mut e, nft::NFT_META_IIF, up);
                on(&mut e, nft::NFT_META_OIF, eth0);
                address6(&mut e, SOURCE6, a);
                ports(&mut e, range);
                load(&mut e, &MARK_AGENTS.to_ne_bytes());
                expr(&mut e, b"meta\0", |d| {
                    be32(d, nft::NFTA_META_KEY, nft::NFT_META_MARK);
                    be32(d, nft::NFTA_META_SREG, nft::NFT_REG_1);
                });
                accept(&mut e);
                b.rule(b"forward\0", e);
            }
        }
    }
    b.chain(
        b"post\0",
        b"nat\0",
        nft::NF_INET_POST_ROUTING,
        nft::PRIORITY_SNAT,
        nft::NF_ACCEPT,
    );
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_MARK);
    compare(&mut e, nft::NFT_CMP_EQ, &MARK_AGENTS.to_ne_bytes());
    load(&mut e, &u.eth0.octets());
    expr(&mut e, b"nat\0", |d| {
        be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_SNAT);
        be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV6));
        be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
    });
    b.rule(b"post\0", e);
    b.commit(sock)
}

/// Init's own namespace's nf_tables, once agents reach past the microVM:
///
/// - `forward` drops all but answers, and what comes up from the switch (`agents0`) to
///   eth0 as a domain's egress grants it, from its addresses, or as the agents' resolver
///   asks, which it marks: the one stateful point on the way, as the gates and the switch
///   let a domain's answers out by their ports alone (D61), so that one sending from a
///   port it is reached on as though answering opens nothing;
/// - `in` drops what comes up from the switch but answers: the run's own processes serve
///   no domain;
/// - `post` gives what is marked eth0's address, so that the network process, which takes
///   frames from the guest's address alone, takes it;
/// - `own` drops whatever this namespace's own processes send down `agents0`, of both IP
///   versions (D115): the run's, which share this namespace, serve no domain and are
///   granted none, so a packet of theirs there, a forged answer to an agent's flow
///   (`IP_FREEBIND` takes any source address and needs no capability, ip(7)) or one that
///   conntrack turns back into one, crosses no grant. Its answers are not let past:
///   nothing here opens a flow to a domain. IPv6's neighbor discovery is, which forwarding
///   down the link takes (D99).
///
/// The run's own processes were kept to eth0's subnet before its command started
/// ([`confine_eth0`]).
fn outside(sock: &OwnedFd, eth0: u32, u: &Uplink) -> io::Result<()> {
    let ((addr, _, _), ingress) = (u.eth0, &u.ingress);
    for family in [nft::NFPROTO_IPV4, nft::NFPROTO_IPV6] {
        let mut b = Batch::of(family);
        b.chain(b"own\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_ACCEPT);
        if family == nft::NFPROTO_IPV6 {
            b.rule(b"own\0", neighbor_discovery());
        }
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_OIF, UPLINK_IFINDEX as u32);
        drop_verdict(&mut e);
        b.rule(b"own\0", e);
        b.commit(sock)?;
    }
    let mut b = Batch::new();
    b.chain(b"in\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_ACCEPT);
    b.rule(b"in\0", answers());
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_IIF);
    compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
    drop_verdict(&mut e);
    b.rule(b"in\0", e);
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    // What is let in: from eth0 to the agents' link, once `pre` has given it the address
    // of the domain it is for, and its port there.
    for (_, ranges) in ingress {
        for &(_, at) in ranges {
            let mut e = Vec::new();
            meta(&mut e, nft::NFT_META_IIF);
            compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
            meta(&mut e, nft::NFT_META_OIF);
            compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
            ports(&mut e, at);
            accept(&mut e);
            b.rule(b"forward\0", e);
        }
    }
    if !ingress.is_empty() {
        b.chain(
            b"pre\0",
            b"nat\0",
            nft::NF_INET_PRE_ROUTING,
            nft::PRIORITY_DNAT,
            nft::NF_ACCEPT,
        );
        for (to, ranges) in ingress {
            for &(from, at) in ranges {
                let mut e = Vec::new();
                meta(&mut e, nft::NFT_META_IIF);
                compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
                ports(&mut e, from);
                load(&mut e, &to.octets());
                // Mapped to the domain's own port (D122): that port, in a register of its
                // own, as nft's `dnat to ADDR:PORT` writes it.
                let mapped = from != at;
                if mapped {
                    load_into(&mut e, nft::NFT_REG_2, &at.1.to_be_bytes());
                }
                expr(&mut e, b"nat\0", |d| {
                    be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_DNAT);
                    be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV4));
                    be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
                    if mapped {
                        be32(d, nft::NFTA_NAT_REG_PROTO_MIN, nft::NFT_REG_2);
                    }
                });
                b.rule(b"pre\0", e);
            }
        }
    }
    // New flows up from the switch: each domain's, from its own addresses on the ports
    // it is granted; the agents' resolver's, from the switch's end of the uplink.
    let up = |b: &mut Batch, from: Ipv4Addr, range: crate::netplan::Egress| {
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
        meta(&mut e, nft::NFT_META_OIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        address(&mut e, SOURCE, from);
        ports(&mut e, range);
        load(&mut e, &MARK_AGENTS.to_ne_bytes());
        expr(&mut e, b"meta\0", |d| {
            be32(d, nft::NFTA_META_KEY, nft::NFT_META_MARK);
            be32(d, nft::NFTA_META_SREG, nft::NFT_REG_1);
        });
        accept(&mut e);
        b.rule(b"forward\0", e);
    };
    for (from, ranges) in &u.egress {
        for &a in from {
            for &range in ranges {
                up(&mut b, a, range);
            }
        }
    }
    if u.resolver {
        up(&mut b, UPLINK_SWITCH, (17, 53, 53));
        up(&mut b, UPLINK_SWITCH, (6, 53, 53));
    }
    b.chain(
        b"post\0",
        b"nat\0",
        nft::NF_INET_POST_ROUTING,
        nft::PRIORITY_SNAT,
        nft::NF_ACCEPT,
    );
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_MARK);
    compare(&mut e, nft::NFT_CMP_EQ, &MARK_AGENTS.to_ne_bytes());
    load(&mut e, &addr.octets());
    expr(&mut e, b"nat\0", |d| {
        be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_SNAT);
        be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV4));
        be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
    });
    b.rule(b"post\0", e);
    b.commit(sock)
}

/// Keeps the run's own processes, of both IP versions, to what they reached before a run
/// whose image grants its agents anything past the microVM (D59, D99): the host's network
/// process holds the agents' grants for the whole microVM, from before the run, so no new
/// flow leaves eth0 but to eth0's own subnet (its network's members, D46). Chain `out` of
/// family ip, and of ip6 where eth0 has IPv6, each dropping the rest. Made before the
/// run's command starts, and fails the run where the kernel has no ip6 family for it.
pub fn confine_eth0() -> io::Result<()> {
    // SAFETY: if_nametoindex(3) of a NUL-terminated literal.
    let eth0 = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
    if eth0 == 0 {
        return Err(io::Error::other("no eth0 to confine"));
    }
    let sock = netlink_socket(libc::NETLINK_NETFILTER)?;
    // Answers, then the subnet's own, then nothing else out eth0.
    let out = |family: u8, at: u32, subnet: &[u8], mask: &[u8]| -> io::Result<()> {
        let mut b = Batch::of(family);
        b.chain(b"out\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_ACCEPT);
        b.rule(b"out\0", answers());
        // IPv6's neighbor discovery, to multicast addresses outside any subnet: what
        // reaching the gateway at all takes, as ARP does for IPv4 below IP.
        if family == nft::NFPROTO_IPV6 {
            b.rule(b"out\0", neighbor_discovery());
        }
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_OIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        payload(
            &mut e,
            nft::NFT_PAYLOAD_NETWORK_HEADER,
            at,
            u32::try_from(subnet.len()).unwrap_or(0),
        );
        masked(&mut e, mask);
        compare(&mut e, nft::NFT_CMP_EQ, subnet);
        accept(&mut e);
        b.rule(b"out\0", e);
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_OIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        drop_verdict(&mut e);
        b.rule(b"out\0", e);
        b.commit(&sock)
    };
    let (addr, prefix, _) = crate::net::current().ok_or_else(|| io::Error::other("eth0 has no address"))?;
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    // The destination address: at offset 16 of IPv4's header, 24 of IPv6's.
    out(
        nft::NFPROTO_IPV4,
        DESTINATION,
        &Ipv4Addr::from(u32::from(addr) & mask).octets(),
        &mask.to_be_bytes(),
    )?;
    if let Some((addr6, prefix6, _)) = crate::net::current6() {
        let mask6 = u128::MAX.checked_shl(128 - u32::from(prefix6)).unwrap_or(0);
        out(
            nft::NFPROTO_IPV6,
            24,
            &std::net::Ipv6Addr::from(u128::from(addr6) & mask6).octets(),
            &mask6.to_be_bytes(),
        )?;
    }
    Ok(())
}
