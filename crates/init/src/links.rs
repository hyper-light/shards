//! The links between domains (docs/design/architecture.md D59; AGENTFILE_ARCH.md §9.7): a
//! switch, a network namespace of no process's that init holds, and for each domain with
//! a grant one veth link, `eth0` in the domain's namespace and `d<n>` in the switch's. The
//! switch routes between links and its nftables decide by link, never by address: what
//! the Agentfile's `CONNECT`s allow ([`crate::netplan`]), and the answers to it, and
//! nothing else, the switch itself included. No link reaches the microVM's own network.

use std::io;
use std::net::Ipv4Addr;
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
/// The n-th domain's link in the switch: `d<n>`, index this plus n.
const SWITCH_IFINDEX: i32 = 100;
/// The switch's link to init's namespace, past which the microVM's eth0 is: `up0` in the
/// switch, `agents0` in init's, each index this; on a link-local /30 of their own (RFC
/// 3927), which no network of the guest's or the host's takes.
const UPLINK_IFINDEX: i32 = 99;
const UPLINK_INIT: Ipv4Addr = Ipv4Addr::new(169, 254, 77, 1);
const UPLINK_SWITCH: Ipv4Addr = Ipv4Addr::new(169, 254, 77, 2);
const UPLINK_PREFIX: u8 = 30;
/// The mark init's forward chain gives what an agent sends past the microVM, by which its
/// postrouting chain gives it eth0's address.
const MARK_AGENTS: u32 = 0x5a59_0001;

/// The switch's link past the microVM: the agents' subnets, which init's namespace routes
/// back down it, and eth0's address and subnet, which their flows leave by.
pub struct Uplink {
    pub subnets: Vec<(Ipv4Addr, u8)>,
    pub eth0: (Ipv4Addr, Ipv4Addr, u8),
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
    pub const NFT_NAT_SNAT: u32 = 0;
}

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
    /// A route socket in init's own namespace, where veths are made.
    here: OwnedFd,
    /// Whether it has a link past the microVM, which every domain's default route takes.
    uplink: bool,
}

impl Switch {
    /// Makes the switch: forwarding on, and nf_tables that drop all but the answers to
    /// what is allowed, `pairs`, each `(from, to)` by the domains' indices, and each
    /// domain's `egress` past the microVM, through `uplink`, and its names' queries to
    /// `resolver`, the microVM's gateway.
    pub fn new(
        pairs: &[(usize, usize)],
        egress: &[(usize, Vec<crate::netplan::Egress>)],
        uplink: Option<&Uplink>,
        resolver: Option<Ipv4Addr>,
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
            Ok((ns, route, nftables))
        })?;
        policy(&nftables, pairs, egress, resolver)?;
        let switch = Switch {
            ns,
            route,
            here: netlink_socket(libc::NETLINK_ROUTE)?,
            uplink: uplink.is_some(),
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
        outside(&netlink_socket(libc::NETLINK_NETFILTER)?, eth0, u.eth0)
    }

    /// Links the `n`-th domain, its first process `pid`, as `link` says.
    pub fn attach(&self, n: usize, pid: libc::pid_t, link: &Link) -> io::Result<()> {
        let index = SWITCH_IFINDEX
            .checked_add(i32::try_from(n).map_err(|_| io::Error::other("too many domains"))?)
            .ok_or_else(|| io::Error::other("too many domains"))?;
        let domain_ns = open_ns(&format!("/proc/{pid}/ns/net"))?;
        // The veth: eth0 in the domain's namespace, d<n> in the switch's.
        let mut m = ifinfomsg(DOMAIN_IFINDEX, 0, 0);
        attr(&mut m, IFLA_IFNAME, b"eth0\0");
        attr(
            &mut m,
            IFLA_NET_NS_FD,
            &(domain_ns.as_raw_fd() as u32).to_ne_bytes(),
        );
        let peer_name = format!("d{n}\0");
        let switch_fd = self.ns.as_raw_fd() as u32;
        nested(&mut m, IFLA_LINKINFO, |li| {
            attr(li, IFLA_INFO_KIND, b"veth");
            nested(li, IFLA_INFO_DATA, |data| {
                nested(data, VETH_INFO_PEER, |peer| {
                    peer.extend_from_slice(&ifinfomsg(index, 0, 0));
                    attr(peer, IFLA_IFNAME, peer_name.as_bytes());
                    attr(peer, IFLA_NET_NS_FD, &switch_fd.to_ne_bytes());
                });
            });
        });
        let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
        exchange(&self.here, &[(RTM_NEWLINK, create, m)])?;
        // The domain's side: its addresses, each a host's, and its subnets through their
        // gateways, on the link.
        let up = libc::IFF_UP as u32;
        let mut ours = vec![(
            RTM_NEWLINK,
            NLM_F_REQUEST | NLM_F_ACK,
            ifinfomsg(DOMAIN_IFINDEX, up, up),
        )];
        let mut theirs = vec![(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, ifinfomsg(index, up, up))];
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
                theirs.push((RTM_NEWADDR, create, addr_msg(index, a.gateway)));
            }
            theirs.push((RTM_NEWROUTE, create, route_msg(index, a.addr, 32, None, None)));
        }
        // Past the microVM, through its first network's gateway, where there is a way:
        // after its addresses, as a route's preferred source must be one.
        if self.uplink
            && let Some(a) = link.addresses.first()
        {
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
        }
        // Up before its routes: a route through a link that is down is refused.
        let (up_ours, rest_ours) = ours.split_at(1);
        let (up_theirs, rest_theirs) = theirs.split_at(1);
        let domain_route = in_netns(Some(domain_ns.as_raw_fd()), || {
            netlink_socket(libc::NETLINK_ROUTE)
        })?;
        exchange(&domain_route, up_ours)?;
        exchange(&self.route, up_theirs)?;
        exchange(&self.route, rest_theirs)?;
        exchange(&domain_route, rest_ours)
    }
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

/// The nf_tables message `kind`, of family ip, with `attrs`.
fn nft_msg(kind: u16, flags: u16, attrs: Vec<u8>) -> (u16, u16, Vec<u8>) {
    // struct nfgenmsg: family, version, resource id.
    let mut m = vec![nft::NFPROTO_IPV4, 0, 0, 0];
    m.extend_from_slice(&attrs);
    (
        (nft::NFNL_SUBSYS_NFTABLES << 8) | kind,
        NLM_F_REQUEST | NLM_F_ACK | flags,
        m,
    )
}

/// One nf_tables transaction for table `shards`, of family ip: its chains, each with
/// its rules (nf_tables_api.c commits a batch whole or not at all).
struct Batch {
    msgs: Vec<(u16, u16, Vec<u8>)>,
}

impl Batch {
    fn new() -> Batch {
        let mut b = Batch {
            msgs: vec![Batch::bound(nft::NFNL_MSG_BATCH_BEGIN)],
        };
        let mut t = Vec::new();
        attr(&mut t, nft::NFTA_TABLE_NAME, TABLE);
        b.msgs.push(nft_msg(nft::NFT_MSG_NEWTABLE, NLM_F_CREATE, t));
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
        self.msgs.push(nft_msg(nft::NFT_MSG_NEWCHAIN, NLM_F_CREATE, c));
    }

    fn rule(&mut self, chain: &[u8], exprs: Vec<u8>) {
        let mut r = Vec::new();
        attr(&mut r, nft::NFTA_RULE_TABLE, TABLE);
        attr(&mut r, nft::NFTA_RULE_CHAIN, chain);
        attr(&mut r, nft::NFTA_RULE_EXPRESSIONS | NLA_F_NESTED, &exprs);
        self.msgs
            .push(nft_msg(nft::NFT_MSG_NEWRULE, NLM_F_CREATE | NLM_F_APPEND, r));
    }

    fn commit(mut self, sock: &OwnedFd) -> io::Result<()> {
        self.msgs.push(Batch::bound(nft::NFNL_MSG_BATCH_END));
        exchange(sock, &self.msgs)
    }
}

/// `ct state established,related accept`.
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

/// Register 1, `value`.
fn load(list: &mut Vec<u8>, value: &[u8]) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_1);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            attr(v, nft::NFTA_DATA_VALUE, value)
        });
    });
}

fn link_index(n: usize) -> io::Result<u32> {
    let n = u32::try_from(n).map_err(|_| io::Error::other("too many domains"))?;
    (SWITCH_IFINDEX as u32)
        .checked_add(n)
        .ok_or_else(|| io::Error::other("too many domains"))
}

/// The switch's nf_tables, in one transaction: chain `input`, which drops all, so no
/// domain reaches the switch; chain `forward`, which drops all but the answers of
/// connections (conntrack's established and related), new ones from each pair's first
/// link to its second, and from each domain's link up to the microVM's, to the ports its
/// networks grant.
fn policy(
    sock: &OwnedFd,
    pairs: &[(usize, usize)],
    egress: &[(usize, Vec<crate::netplan::Egress>)],
    resolver: Option<Ipv4Addr>,
) -> io::Result<()> {
    let mut b = Batch::new();
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    for &(from, to) in pairs {
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &link_index(from)?.to_ne_bytes());
        meta(&mut e, nft::NFT_META_OIF);
        compare(&mut e, nft::NFT_CMP_EQ, &link_index(to)?.to_ne_bytes());
        accept(&mut e);
        b.rule(b"forward\0", e);
    }
    for (n, ranges) in egress {
        for &(proto, lo, hi) in ranges {
            let mut e = Vec::new();
            meta(&mut e, nft::NFT_META_IIF);
            compare(&mut e, nft::NFT_CMP_EQ, &link_index(*n)?.to_ne_bytes());
            meta(&mut e, nft::NFT_META_OIF);
            compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
            meta(&mut e, nft::NFT_META_L4PROTO);
            compare(&mut e, nft::NFT_CMP_EQ, &[proto]);
            // The destination port, at offset 2 of TCP's header and UDP's alike, compared
            // as the network-order bytes it is.
            payload(&mut e, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
            compare(&mut e, nft::NFT_CMP_GTE, &lo.to_be_bytes());
            compare(&mut e, nft::NFT_CMP_LTE, &hi.to_be_bytes());
            accept(&mut e);
            b.rule(b"forward\0", e);
        }
        // Its names' queries, by UDP, to the gateway's resolver alone (D59): the network
        // process asks the host's for it.
        if let Some(r) = resolver {
            let mut e = Vec::new();
            meta(&mut e, nft::NFT_META_IIF);
            compare(&mut e, nft::NFT_CMP_EQ, &link_index(*n)?.to_ne_bytes());
            meta(&mut e, nft::NFT_META_OIF);
            compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
            meta(&mut e, nft::NFT_META_L4PROTO);
            compare(&mut e, nft::NFT_CMP_EQ, &[17]);
            // The destination address, at offset 16 of IPv4's header.
            payload(&mut e, nft::NFT_PAYLOAD_NETWORK_HEADER, 16, 4);
            compare(&mut e, nft::NFT_CMP_EQ, &r.octets());
            payload(&mut e, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
            compare(&mut e, nft::NFT_CMP_EQ, &53u16.to_be_bytes());
            accept(&mut e);
            b.rule(b"forward\0", e);
        }
    }
    b.commit(sock)
}

/// Init's own namespace's nf_tables, once agents reach past the microVM:
///
/// - `forward` drops all but answers, and what comes up from the switch (`agents0`) to
///   eth0, which it marks;
/// - `post` gives what is marked eth0's address, so that the network process, which takes
///   frames from the guest's address alone, takes it;
/// - `out` keeps the run's own processes to what they reached before: the default deny
///   held them at the host, which now opens the agents' ports to the whole microVM, so
///   no new flow leaves eth0 but to eth0's own subnet (its network's members, D46).
fn outside(sock: &OwnedFd, eth0: u32, (addr, subnet, prefix): (Ipv4Addr, Ipv4Addr, u8)) -> io::Result<()> {
    let mut b = Batch::new();
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.rule(b"forward\0", answers());
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_IIF);
    compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
    meta(&mut e, nft::NFT_META_OIF);
    compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
    load(&mut e, &MARK_AGENTS.to_ne_bytes());
    expr(&mut e, b"meta\0", |d| {
        be32(d, nft::NFTA_META_KEY, nft::NFT_META_MARK);
        be32(d, nft::NFTA_META_SREG, nft::NFT_REG_1);
    });
    accept(&mut e);
    b.rule(b"forward\0", e);
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
    b.chain(b"out\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_ACCEPT);
    b.rule(b"out\0", answers());
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_OIF);
    compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
    // The destination address, at offset 16 of IPv4's header, in eth0's subnet.
    payload(&mut e, nft::NFT_PAYLOAD_NETWORK_HEADER, 16, 4);
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    masked(&mut e, &mask.to_be_bytes());
    compare(&mut e, nft::NFT_CMP_EQ, &subnet.octets());
    accept(&mut e);
    b.rule(b"out\0", e);
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_OIF);
    compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
    expr(&mut e, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_VERDICT);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            nested(v, nft::NFTA_DATA_VERDICT, |c| {
                be32(c, nft::NFTA_VERDICT_CODE, nft::NF_DROP)
            });
        });
    });
    b.rule(b"out\0", e);
    b.commit(sock)
}
