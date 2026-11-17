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

pub(crate) const RTM_NEWLINK: u16 = 16;
pub(crate) const RTM_NEWADDR: u16 = 20;
/// rtnetlink's address removal (linux/rtnetlink.h), a running member's network let go
/// (D181).
const RTM_DELADDR: u16 = 21;
pub(crate) const RTM_NEWROUTE: u16 = 24;
/// rtnetlink's route removal (linux/rtnetlink.h), a network's subnet let go (D181).
const RTM_DELROUTE: u16 = 25;
pub(crate) const NLM_F_REQUEST: u16 = 1;
pub(crate) const NLM_F_ACK: u16 = 4;
pub(crate) const NLM_F_EXCL: u16 = 0x200;
pub(crate) const NLM_F_CREATE: u16 = 0x400;
/// A route made in place of the one there (linux/netlink.h).
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_APPEND: u16 = 0x800;
const NLMSG_ERROR: u16 = 2;
const NLA_F_NESTED: u16 = 0x8000;
pub(crate) const IFLA_IFNAME: u16 = 3;
pub(crate) const IFLA_LINKINFO: u16 = 18;
pub(crate) const IFLA_NET_NS_FD: u16 = 28;
pub(crate) const IFLA_INFO_KIND: u16 = 1;
pub(crate) const IFLA_INFO_DATA: u16 = 2;
pub(crate) const VETH_INFO_PEER: u16 = 1;
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
    /// eth0's gateway: the host itself at each port a member is granted the host at
    /// (D236), which no member its engine makes reaches ([`not_the_host`]).
    pub gateway: Ipv4Addr,
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
#[derive(Clone)]
pub struct Uplink6 {
    pub eth0: Ipv6Addr,
    pub subnets: Vec<(Ipv6Addr, u8)>,
    pub egress: Vec<(Vec<Ipv6Addr>, Vec<crate::netplan::Egress>)>,
}

/// nfnetlink and nf_tables (include/uapi/linux/netfilter/nfnetlink.h, nf_tables.h).
pub(crate) mod nft {
    pub const NFNL_SUBSYS_NFTABLES: u16 = 10;
    pub const NFNL_MSG_BATCH_BEGIN: u16 = 0x10;
    pub const NFNL_MSG_BATCH_END: u16 = 0x11;
    pub const NFT_MSG_NEWTABLE: u16 = 0;
    pub const NFT_MSG_NEWCHAIN: u16 = 3;
    pub const NFT_MSG_NEWRULE: u16 = 6;
    pub const NFT_MSG_DELRULE: u16 = 8;
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
    pub const NFTA_CT_SREG: u16 = 4;
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
    /// enum nft_ct_keys (linux/netfilter/nf_tables.h): the conntrack zone.
    pub const NFT_CT_ZONE: u32 = 17;
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
pub(crate) fn attr(buf: &mut Vec<u8>, kind: u16, value: &[u8]) {
    let len = u16::try_from(4 + value.len()).unwrap_or(u16::MAX);
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(&kind.to_ne_bytes());
    buf.extend_from_slice(value);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// A nested attribute, its contents what `f` appends.
pub(crate) fn nested(buf: &mut Vec<u8>, kind: u16, f: impl FnOnce(&mut Vec<u8>)) {
    let mut inner = Vec::new();
    f(&mut inner);
    attr(buf, kind | NLA_F_NESTED, &inner);
}

/// A u32 attribute in network order, as nf_tables reads its numbers.
pub(crate) fn be32(buf: &mut Vec<u8>, kind: u16, v: u32) {
    attr(buf, kind, &v.to_be_bytes());
}

pub(crate) fn ifinfomsg(index: i32, flags: u32, change: u32) -> Vec<u8> {
    let mut m = vec![0u8; 4];
    m.extend_from_slice(&index.to_ne_bytes());
    m.extend_from_slice(&flags.to_ne_bytes());
    m.extend_from_slice(&change.to_ne_bytes());
    m
}

pub(crate) fn netlink_socket(protocol: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, protocol) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just opened, owned from here on.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Init's own network namespace's netfilter socket, kept for init's life once opened:
/// closing one after a commit waits out an RCU grace period (netlink_release's
/// NETLINK_URELEASE, on which nft_rcv_nl_event flushes the commits' destroy work, which
/// waits in synchronize_rcu: nf_tables_api.c at Linux 6.18.48; PM M513), and the tables a
/// run's start programs there (its confinement, the host's loopback, its agents' way past
/// the microVM) are on its path to its command.
/// Kept with the process and the network namespace it was opened in: a socket's namespace
/// is its creator's (net/socket.c), and a process init forks inherits the descriptor, whose
/// acknowledgments two processes reading one socket would take from each other. A caller of
/// another process or namespace opens its own, as each caller did before.
static INIT_NFT: std::sync::Mutex<Option<(Opener, OwnedFd)>> = std::sync::Mutex::new(None);

/// Who opened a kept socket: its process, and its network namespace's inode.
type Opener = (u32, u64);

/// The calling thread's process and network namespace.
fn opener() -> io::Result<Opener> {
    use std::os::unix::fs::MetadataExt as _;
    Ok((
        std::process::id(),
        std::fs::metadata("/proc/thread-self/ns/net")?.ino(),
    ))
}

/// Runs `f` on init's own namespace's netfilter socket ([`INIT_NFT`]), opened on first use
/// by its caller, and kept for callers of the same process and namespace; a caller of
/// another opens its own, kept in its place. One whose exchange failed is let go, and the
/// next opens another, as the chooser's.
pub(crate) fn on_init_nft<T>(f: impl FnOnce(&OwnedFd) -> io::Result<T>) -> io::Result<T> {
    // A caller that cannot say which namespace it is in (a process whose mount namespace has
    // no /proc, as a member's may be as it is set up) is no opener's: its own socket, let go.
    let Ok(here) = opener() else {
        return f(&netlink_socket(libc::NETLINK_NETFILTER)?);
    };
    let mut kept = INIT_NFT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut sock = kept.take().filter(|(by, _)| *by == here).map(|(_, sock)| sock);
    let done = kept_socket(&mut sock, || netlink_socket(libc::NETLINK_NETFILTER), f);
    *kept = sock.map(|sock| (here, sock));
    done
}

/// `f` on the socket `kept` holds, or on one `open` makes, kept again where `f` succeeds.
fn kept_socket<T>(
    kept: &mut Option<OwnedFd>,
    open: impl FnOnce() -> io::Result<OwnedFd>,
    f: impl FnOnce(&OwnedFd) -> io::Result<T>,
) -> io::Result<T> {
    let sock = match kept.take() {
        Some(sock) => sock,
        None => open()?,
    };
    let done = f(&sock);
    if done.is_ok() {
        *kept = Some(sock);
    }
    done
}

/// Sends netlink messages, each `(type, flags, payload)`, as one write, and reads their
/// acknowledgments: the kernel answers a request in the send itself, so every answer is
/// there when it returns. The first error the kernel gave, if any.
pub(crate) fn exchange(sock: &OwnedFd, msgs: &[(u16, u16, Vec<u8>)]) -> io::Result<()> {
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
    let mut buf = vec![0u8; 65536];
    let acks = acknowledged(sock, asked, &mut buf);
    if acks.is_err() {
        // What the kernel said past the first error, read, so that a socket kept for
        // another exchange holds none of this one's answers.
        // SAFETY: a buffer of the length given, without waiting.
        while unsafe {
            libc::recv(
                sock.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        } > 0
        {}
    }
    acks
}

/// The `asked` acknowledgments of an exchange, read: the first error among them, if any.
fn acknowledged(sock: &OwnedFd, asked: usize, buf: &mut [u8]) -> io::Result<()> {
    let mut acked = 0usize;
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
pub(crate) fn in_netns<T: Send>(
    ns: Option<RawFd>,
    f: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
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

pub(crate) fn open_ns(path: &str) -> io::Result<OwnedFd> {
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
    /// Its nf_tables socket, by which what it carries is programmed again (D181).
    nft: OwnedFd,
    /// Whether any domain has IPv6 (D99): the switch's own IPv6 tables, and forwarding.
    ipv6: bool,
    /// Whether the uplink carries IPv6: domains with egress take a default IPv6 route.
    uplink6: bool,
    /// A route socket in init's own namespace, where veths are made.
    here: OwnedFd,
    /// Each gate's namespace, by its link's index, which nothing else holds.
    gates: std::sync::Mutex<Vec<(usize, OwnedFd)>>,
    /// Each domain's gate as it was linked, made again with the subnets the switch carries
    /// as its engine's networks come and go (D181).
    domain_gates: std::sync::Mutex<Vec<(usize, Gate)>>,
    /// What it carries, kept so that it is programmed again, whole and at once, as the
    /// members its microVM's engine makes come and go (D181).
    carried: std::sync::Mutex<Carried>,
}

/// What a switch carries: the domains' grants, as their Agentfile made them, and the
/// members its microVM's engine made (D181); its link past the microVM, once one is
/// needed; how many domains it was made for, past whose indices members are linked; and
/// every subnet a network of it holds.
struct Carried {
    pairs: Vec<(usize, usize, Vec<crate::netplan::Egress>)>,
    egress: Vec<(usize, Vec<crate::netplan::Egress>)>,
    ingress: Vec<(usize, Vec<crate::netplan::Egress>)>,
    /// The domains granted names past the microVM, which the agents' resolver asks the
    /// microVM's resolver for.
    dns: Vec<usize>,
    /// The domains whose resolv.conf names the agents' resolver (`egress::resolves`), whose
    /// questions it takes whatever they are granted, so that each is answered at once (D278).
    asking: Vec<usize>,
    uplink: Option<Uplink>,
    /// The uplink's IPv6 part, where it carries IPv6 (D99): programmed again with the rest
    /// as a member's grants change (D278).
    uplink6: Option<Uplink6>,
    domains: usize,
    subnets: Vec<(Ipv4Addr, u8)>,
    members: Vec<Member>,
    /// Each pair of members its engine's networks grant a flow (D181): from, to, and the
    /// ports the receiver declares.
    member_pairs: Vec<(usize, usize, Vec<crate::netplan::Egress>)>,
    /// The networks shared by runs the run is on (D285).
    shared: Vec<Shared>,
}

/// A network shared by runs (D285), as the switch, the gates and init's namespace carry it:
/// the run's address on it, eth0's second, and its subnet; and each domain's flows there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub addr: Ipv4Addr,
    pub subnet: (Ipv4Addr, u8),
    pub flows: Vec<Flows>,
}

/// A domain's flows on a network shared by runs (D285), as its grants there make them: its
/// addresses, from which init gives its flows there the run's address, its first taking what
/// comes in; each peer it may open flows to, at the peer's addresses there, on the peer's
/// ports; and each that may open flows to it, on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flows {
    pub domain: usize,
    pub own: Vec<Ipv4Addr>,
    pub opens: Vec<(Vec<Ipv4Addr>, Vec<crate::netplan::Egress>)>,
    pub accepts: Vec<(Vec<Ipv4Addr>, Vec<crate::netplan::Egress>)>,
}

/// Domain `n`'s gate `kept` as the switch carries it now: the subnets of every network it
/// carries or meets, `subnets`, kept off its egress past the microVM (D181, D285); and its
/// flows on the networks shared by runs, each grant's peer at its addresses there, with its
/// questions for their names (D285).
fn carried_gate(n: usize, kept: Gate, subnets: &[(Ipv4Addr, u8)], shared: &[Shared]) -> Gate {
    let mut g = kept;
    for s in subnets {
        if !g.subnets.contains(s) {
            g.subnets.push(*s);
        }
    }
    for f in shared.iter().flat_map(|s| &s.flows).filter(|f| f.domain == n) {
        g.opens.extend(f.opens.iter().cloned());
        g.accepts.extend(f.accepts.iter().cloned());
        g.dns = true;
    }
    g
}

/// The switch's grants with each domain's flows on the networks shared by runs (D285): up its
/// link to its peers' ports, down it to its own, and its questions for their names, each
/// domain's asked once.
fn sharing(
    shared: &[Shared],
    egress: &mut Vec<(usize, Vec<crate::netplan::Egress>)>,
    ingress: &mut Vec<(usize, Vec<crate::netplan::Egress>)>,
    dns: &mut Vec<usize>,
) {
    for f in shared.iter().flat_map(|s| &s.flows) {
        let out: Vec<crate::netplan::Egress> = f.opens.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        let into: Vec<crate::netplan::Egress> =
            f.accepts.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        if !out.is_empty() {
            egress.push((f.domain, out));
        }
        if !into.is_empty() {
            ingress.push((f.domain, into));
        }
        if !dns.contains(&f.domain) {
            dns.push(f.domain);
        }
    }
}

/// The domains whose questions the switch takes to the agents' resolver, each once: `dns`,
/// granted names past the microVM, and `asking`, whose resolv.conf names it (D278).
fn questions(dns: &[usize], asking: &[usize]) -> Vec<usize> {
    let mut all = dns.to_vec();
    all.extend(asking.iter().filter(|&n| !dns.contains(n)));
    all
}

/// How far past the microVM a member its engine made reaches (D181): nowhere (on an
/// internal network alone), where the run's command reaches, or within one subnet alone,
/// eth0's, where the command is kept to it (`confine_eth0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Nowhere,
    AsTheCommand,
    Within((Ipv4Addr, u8)),
}

/// A member its microVM's engine made, as the switch carries it (D181): its link's index,
/// its addresses, how far past the microVM it reaches, the ports it publishes to the run's
/// command, and whether it asks the agents' resolver.
#[derive(Debug, Clone)]
pub struct Member {
    pub index: usize,
    pub addrs: Vec<Ipv4Addr>,
    pub reach: Reach,
    pub published: Vec<crate::netplan::Egress>,
    pub dns: bool,
}

/// Every port of TCP's and UDP's: what a member reaching where the command does may open.
pub const EVERY_PORT: [crate::netplan::Egress; 2] = [(6, 1, 65535), (17, 1, 65535)];

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
    /// The subnets of the microVM's own networks, its agents' and its engine's members'
    /// (D61, D181): no flow it opens on its egress ports goes to one, each flow to a peer
    /// being a pair's, so that its gate holds a peer's ports alone as the switch does.
    pub subnets: Vec<(Ipv4Addr, u8)>,
    pub subnets6: Vec<(Ipv6Addr, u8)>,
}

impl Gate {
    /// Every port others open flows to it on, of each protocol.
    pub(crate) fn accepted(&self) -> Vec<crate::netplan::Egress> {
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
    /// receiver accepts, each of `dns` asking the microVM's resolver, each of `asking` the
    /// agents' resolver, and each domain's `egress` past the microVM, through `uplink`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
        egress: &[(usize, Vec<crate::netplan::Egress>)],
        ingress: &[(usize, Vec<crate::netplan::Egress>)],
        dns: &[usize],
        asking: &[usize],
        uplink: Option<Uplink>,
        ipv6: bool,
        domains: usize,
        subnets: &[(Ipv4Addr, u8)],
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
        policy(&nftables, pairs, egress, ingress, &questions(dns, asking))?;
        // IPv6 between domains (D99): its tables first, then forwarding, so that a kernel
        // without nf_tables' ip6 family fails the run rather than forward unfiltered.
        let uplink6 = uplink.as_ref().is_some_and(|u| u.ipv6.is_some());
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
            nft: nftables,
            ipv6,
            uplink6,
            here: netlink_socket(libc::NETLINK_ROUTE)?,
            gates: std::sync::Mutex::new(Vec::new()),
            domain_gates: std::sync::Mutex::new(Vec::new()),
            carried: std::sync::Mutex::new(Carried {
                pairs: pairs.to_vec(),
                egress: egress.to_vec(),
                ingress: ingress.to_vec(),
                dns: dns.to_vec(),
                asking: asking.to_vec(),
                uplink: None,
                uplink6: None,
                domains,
                subnets: subnets.to_vec(),
                members: Vec::new(),
                member_pairs: Vec::new(),
                shared: Vec::new(),
            }),
        };
        if let Some(u) = uplink {
            switch.up(u)?;
        }
        Ok(switch)
    }

    /// What it carries, held: a poisoned lock's state is still whole, each change to it
    /// made in one step.
    fn carried(&self) -> std::sync::MutexGuard<'_, Carried> {
        self.carried
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether it has a link past the microVM, which every linked domain's and member's
    /// default route takes.
    fn has_uplink(&self) -> bool {
        self.carried().uplink.is_some()
    }

    /// How many domains it was made for: a member its microVM's engine makes is linked
    /// past their indices (D181).
    pub fn domains(&self) -> usize {
        self.carried().domains
    }

    /// Every subnet a network of it holds or meets, the domains', the engine's (D181) and
    /// those shared by runs (D285): which a new one keeps clear of, and no egress past the
    /// microVM goes to.
    pub fn subnets(&self) -> Vec<(Ipv4Addr, u8)> {
        let c = self.carried();
        let mut all = c.subnets.clone();
        all.extend(c.shared.iter().map(|s| s.subnet));
        all
    }

    /// Its link past the microVM where it has none yet (D181): what a member its
    /// microVM's engine makes reaches past it, and is reached by from the run's command,
    /// goes up it.
    pub fn uplink_once(&self) -> io::Result<()> {
        if self.has_uplink() {
            return Ok(());
        }
        let (addr, prefix, gateway) = crate::net::current()
            .ok_or_else(|| io::Error::other("the microVM has no address for its engine's containers"))?;
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        // Routed down the uplink: the microVM's own networks alone, never one shared by runs,
        // which eth0 reaches (D285).
        let subnets = self.carried().subnets.clone();
        self.up(Uplink {
            subnets,
            eth0: (addr, Ipv4Addr::from(u32::from(addr) & mask), prefix),
            gateway,
            ingress: Vec::new(),
            egress: Vec::new(),
            resolver: false,
            ipv6: None,
        })
    }

    /// Routes each of `subnets` down from init's namespace where none is yet (D181): the
    /// subnets of the networks its engine's members are on, which init reaches them by.
    pub fn routes(&self, subnets: &[(Ipv4Addr, u8)]) -> io::Result<()> {
        let mut c = self.carried();
        for &subnet in subnets {
            if c.subnets.contains(&subnet) {
                continue;
            }
            let create = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
            exchange(
                &self.here,
                &[(
                    RTM_NEWROUTE,
                    create,
                    route_msg(UPLINK_IFINDEX, subnet.0, subnet.1, Some(UPLINK_SWITCH), None),
                )],
            )?;
            c.subnets.push(subnet);
            if let Some(u) = c.uplink.as_mut() {
                u.subnets.push(subnet);
            }
        }
        drop(c);
        self.regate_domains()
    }

    /// Lets `subnet` go (D181), a network its engine removed: its route down from init's
    /// namespace, where there was one, and its place among the subnets carried.
    pub fn forget(&self, subnet: (Ipv4Addr, u8)) -> io::Result<()> {
        let mut c = self.carried();
        if !c.subnets.contains(&subnet) {
            return Ok(());
        }
        exchange(
            &self.here,
            &[(
                RTM_DELROUTE,
                NLM_F_REQUEST | NLM_F_ACK,
                route_msg(UPLINK_IFINDEX, subnet.0, subnet.1, Some(UPLINK_SWITCH), None),
            )],
        )?;
        c.subnets.retain(|s| *s != subnet);
        if let Some(u) = c.uplink.as_mut() {
            u.subnets.retain(|s| *s != subnet);
        }
        drop(c);
        self.regate_domains()
    }

    /// Carries `members` and their `pairs` (D181), each pair a flow its engine's networks
    /// grant from one to another on the ports the receiver declares, in place of those it
    /// carried: the switch's rules and init's programmed again at once.
    pub fn carry(
        &self,
        members: Vec<Member>,
        pairs: Vec<(usize, usize, Vec<crate::netplan::Egress>)>,
    ) -> io::Result<()> {
        let mut c = self.carried();
        c.members = members;
        c.member_pairs = pairs;
        self.reprogram(&c)
    }

    /// Lets member `index`'s gate go, once nothing carried names it (D181): its namespace
    /// goes with the last descriptor of it, its links with it.
    pub fn drop_gate(&self, index: usize) {
        match self.gates.lock() {
            Ok(mut g) => g.retain(|(i, _)| *i != index),
            Err(poisoned) => poisoned.into_inner().retain(|(i, _)| *i != index),
        }
    }

    /// The switch's rules and init's, programmed again from what it carries, each in one
    /// transaction: what the domains were granted, and each member's (D181).
    fn reprogram(&self, c: &Carried) -> io::Result<()> {
        let mut pairs = c.pairs.clone();
        pairs.extend(c.member_pairs.iter().cloned());
        let mut egress = c.egress.clone();
        let mut ingress = c.ingress.clone();
        let mut dns = questions(&c.dns, &c.asking);
        for m in &c.members {
            if m.reach != Reach::Nowhere {
                egress.push((m.index, EVERY_PORT.to_vec()));
            }
            if !m.published.is_empty() {
                ingress.push((m.index, m.published.clone()));
            }
            if m.dns {
                dns.push(m.index);
            }
        }
        sharing(&c.shared, &mut egress, &mut ingress, &mut dns);
        policy(&self.nft, &pairs, &egress, &ingress, &dns)?;
        // IPv6 between domains, and past the microVM where the uplink carries it (D99): the
        // domains' alone, the engine's members having no IPv6 addresses.
        if self.ipv6 {
            let egress6: &[(usize, Vec<crate::netplan::Egress>)] = if self.uplink6 { &c.egress } else { &[] };
            policy6(&self.nft, &c.pairs, egress6)?;
        }
        let Some(u) = &c.uplink else {
            return Ok(());
        };
        // SAFETY: if_nametoindex(3) of a NUL-terminated literal.
        let eth0 = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
        if eth0 == 0 {
            return Err(io::Error::other("the microVM has no eth0 for its agents' egress"));
        }
        on_init_nft(|nft| outside(nft, eth0, u, &c.members, &c.shared))?;
        match &c.uplink6 {
            Some(six) => on_init_nft(|nft| outside6(nft, eth0, six)),
            None => Ok(()),
        }
    }

    /// Domain `n`'s grants past the microVM made `egress` and `dns`, from its addresses `own`
    /// and `own6` (D278): what the switch forwards up and down its link, init's own forward
    /// and marks for it, by IPv6 too where the uplink carries it, and whether the agents'
    /// resolver asks the microVM's; all of it programmed again whole, each table in one
    /// transaction. Its gate is [`Self::regate_domain`]'s.
    pub fn regrant(
        &self,
        n: usize,
        own: &[Ipv4Addr],
        own6: &[Ipv6Addr],
        egress: &[crate::netplan::Egress],
        dns: bool,
    ) -> io::Result<()> {
        let mut c = self.carried();
        if c.uplink.is_none() && (!egress.is_empty() || dns) {
            return Err(io::Error::other("the microVM has no way past it for its agents"));
        }
        c.egress.retain(|(i, _)| *i != n);
        if !egress.is_empty() {
            c.egress.push((n, egress.to_vec()));
        }
        c.dns.retain(|i| *i != n);
        if dns {
            c.dns.push(n);
        }
        let resolver = !c.dns.is_empty();
        if let Some(u) = c.uplink.as_mut() {
            u.egress.retain(|(from, _)| from.as_slice() != own);
            if !egress.is_empty() && !own.is_empty() {
                u.egress.push((own.to_vec(), egress.to_vec()));
            }
            u.resolver = resolver;
        }
        if let Some(six) = c.uplink6.as_mut() {
            six.egress.retain(|(from, _)| from.as_slice() != own6);
            if !egress.is_empty() && !own6.is_empty() {
                six.egress.push((own6.to_vec(), egress.to_vec()));
            }
        }
        self.reprogram(&c)
    }

    /// Domain `n`'s gate made again with its grants past the microVM `egress` and `dns`
    /// (D278), kept so, the subnets the switch carries now among those its egress keeps off,
    /// as [`Self::regate_domains`] makes them all.
    pub fn regate_domain(&self, n: usize, egress: &[crate::netplan::Egress], dns: bool) -> io::Result<()> {
        let gate = {
            let mut kept = self
                .domain_gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (_, g) = kept
                .iter_mut()
                .find(|(i, _)| *i == n)
                .ok_or_else(|| io::Error::other("no such domain's gate"))?;
            g.egress = egress.to_vec();
            g.dns = dns;
            // As the uplink carries IPv6, which it does where eth0 has it (domains::gate_of).
            g.egress6 = crate::net::current6().is_some() && !g.own6.is_empty() && !egress.is_empty();
            g.clone()
        };
        self.regate(n, &self.as_carried(n, gate))
    }

    /// Domain `n`'s gate `kept` as the switch carries it now ([`carried_gate`]).
    fn as_carried(&self, n: usize, kept: Gate) -> Gate {
        let subnets = self.subnets();
        carried_gate(n, kept, &subnets, &self.carried().shared)
    }

    /// The networks shared by runs it carries made `shared`, whole (D285): each domain's gate
    /// first, made again with its flows there and their subnets kept off its egress, so that no
    /// flow opens on what a member lost; the switch's tables and init's are [`Self::retable`]'s,
    /// once its caller has reset what was lost, as D278 orders a member's change.
    pub fn share(&self, shared: Vec<Shared>) -> io::Result<()> {
        if !shared.is_empty() && !self.has_uplink() {
            return Err(io::Error::other("the microVM has no way past it for its agents"));
        }
        self.carried().shared = shared;
        self.regate_domains()
    }

    /// The switch's tables and init's own programmed again from what it carries (D285).
    pub fn retable(&self) -> io::Result<()> {
        let c = self.carried();
        self.reprogram(&c)
    }

    /// The link past the microVM: `agents0` in init's namespace, `up0` in the switch's,
    /// the switch's default route up it and init's routes to the agents' subnets down it;
    /// init's namespace forwarding what comes up to eth0, under eth0's address, and
    /// nothing else; and the run's own processes kept to what they reached before.
    fn up(&self, u: Uplink) -> io::Result<()> {
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
        {
            let mut c = self.carried();
            c.uplink = Some(Uplink {
                subnets: u.subnets.clone(),
                eth0: u.eth0,
                gateway: u.gateway,
                ingress: u.ingress.clone(),
                egress: u.egress.clone(),
                resolver: u.resolver,
                ipv6: None,
            });
            self.reprogram(&c)?;
        }
        // IPv6 past the microVM (D99): the uplink's link-local ends, the switch's default
        // route up it and init's routes to the agents' IPv6 subnets down it; init's IPv6
        // tables, then its IPv6 forwarding.
        let Some(six) = &u.ipv6 else { return Ok(()) };
        self.carried().uplink6 = Some(six.clone());
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
        on_init_nft(|nft| outside6(nft, eth0, six))?;
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

    /// Gives member `n`, which runs in namespace `member_ns`, its address `a` on one more
    /// network, or takes it back (`add` false), as its engine connects or disconnects it
    /// (D181): the switch's route to it through the member's gate, the gate's gateway on
    /// in0 and its route down to it, and the member's address and its subnet's route
    /// through that gateway; in that order, and the other way round to take it back.
    pub fn member_address(
        &self,
        n: usize,
        member_ns: RawFd,
        a: &crate::netplan::Address,
        add: bool,
    ) -> io::Result<()> {
        let index =
            link_index(n).and_then(|i| i32::try_from(i).map_err(|_| io::Error::other("too many domains")))?;
        let transit = transit_gate(n)?;
        let (addr_kind, route_kind, flags) = if add {
            (
                RTM_NEWADDR,
                RTM_NEWROUTE,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
            )
        } else {
            (RTM_DELADDR, RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK)
        };
        let theirs = [(
            route_kind,
            flags,
            route_msg(index, a.addr, 32, Some(transit), None),
        )];
        let gates = [
            (addr_kind, flags, addr_msg(GATE_IN, a.gateway)),
            (route_kind, flags, route_msg(GATE_IN, a.addr, 32, None, None)),
        ];
        let ours = [
            (addr_kind, flags, addr_msg(DOMAIN_IFINDEX, a.addr)),
            (
                route_kind,
                flags,
                route_msg(DOMAIN_IFINDEX, a.subnet, a.prefix, Some(a.gateway), Some(a.addr)),
            ),
        ];
        let gate_ns = {
            let gates = self
                .gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (_, ns) = gates
                .iter()
                .find(|(i, _)| *i == n)
                .ok_or_else(|| io::Error::other("no such member's gate"))?;
            ns.try_clone()?
        };
        let gate_route = in_netns(Some(gate_ns.as_raw_fd()), || netlink_socket(libc::NETLINK_ROUTE))?;
        let member_route = in_netns(Some(member_ns), || netlink_socket(libc::NETLINK_ROUTE))?;
        if add {
            exchange(&self.route, &theirs)?;
            exchange(&gate_route, &gates)?;
            exchange(&member_route, &ours)
        } else {
            // A route through its address goes with it: the kernel flushes them.
            exchange(&member_route, &ours[..1])?;
            exchange(&gate_route, &gates)?;
            exchange(&self.route, &theirs)
        }
    }

    /// Member `n`'s way out of its namespace, made again through its first network's
    /// gateway `a` (D181): past the microVM where the switch has a way, or to the agents'
    /// resolver where it asks names; as a network it ran on first is let go.
    pub fn member_way_out(&self, member_ns: RawFd, a: &crate::netplan::Address, dns: bool) -> io::Result<()> {
        let (dst, prefix) = if self.has_uplink() {
            (Ipv4Addr::UNSPECIFIED, 0)
        } else if dns {
            (TRANSIT_SWITCH, 32)
        } else {
            return Ok(());
        };
        let member_route = in_netns(Some(member_ns), || netlink_socket(libc::NETLINK_ROUTE))?;
        exchange(
            &member_route,
            &[(
                RTM_NEWROUTE,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE,
                route_msg(DOMAIN_IFINDEX, dst, prefix, Some(a.gateway), Some(a.addr)),
            )],
        )
    }

    /// Programs member `n`'s gate again with what `gate` lets through, in one transaction,
    /// as its peers join and leave (D181).
    pub fn regate(&self, n: usize, gate: &Gate) -> io::Result<()> {
        let gates = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, ns) = gates
            .iter()
            .find(|(i, _)| *i == n)
            .ok_or_else(|| io::Error::other("no such member's gate"))?;
        let nft = in_netns(Some(ns.as_raw_fd()), || netlink_socket(libc::NETLINK_NETFILTER))?;
        gate_policy(&nft, gate)?;
        if gate.own6.is_empty() {
            return Ok(());
        }
        gate_policy6(&nft, gate)
    }

    /// Links the `n`-th domain, its first process `pid`, as `link` says, through a gate
    /// of its own that lets through what `gate` says: its network namespace, which its
    /// grants' changes reach (D278).
    pub fn attach(&self, n: usize, pid: libc::pid_t, link: &Link, gate: &Gate) -> io::Result<OwnedFd> {
        let ns = open_ns(&format!("/proc/{pid}/ns/net"))?;
        self.attach_ns(n, &ns, link, gate)?;
        self.domain_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((n, gate.clone()));
        Ok(ns)
    }

    /// Each domain's gate made again as the switch carries it now ([`carried_gate`]), as its
    /// engine makes or removes a network (D181) and the run joins or leaves one shared by runs
    /// or its members' grants there change (D285).
    fn regate_domains(&self) -> io::Result<()> {
        let kept = self
            .domain_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (n, gate) in kept {
            self.regate(n, &self.as_carried(n, gate))?;
        }
        Ok(())
    }

    /// Links the network namespace `domain_ns` as the `n`-th link, as [`Self::attach`]
    /// links a domain's: a member its microVM's engine makes, before any process of it
    /// enters the namespace (D181).
    pub fn attach_ns(&self, n: usize, domain_ns: &OwnedFd, link: &Link, gate: &Gate) -> io::Result<()> {
        let index =
            link_index(n).and_then(|i| i32::try_from(i).map_err(|_| io::Error::other("too many domains")))?;
        let transit = transit_gate(n)?;
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
            // What the domain opens is tracked here (gate_policy): its UDP flows kept as
            // the host's network process keeps them, not this new namespace's 30 s.
            crate::setup::keep_udp_flows().map_err(io::Error::other)?;
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
            if self.has_uplink() {
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
            Ok(mut g) => g.push((n, gate_ns)),
            Err(poisoned) => poisoned.into_inner().push((n, gate_ns)),
        }
        Ok(())
    }
}

/// The ports a connection takes where its namespace says no others: the kernel's default
/// `ip_local_port_range` (net/ipv4/af_inet.c, inet_init_net).
pub(crate) const LOCAL_PORTS: (u16, u16) = (32768, 60999);

/// The ports a domain's own connections may take, none of `accepted`: the kernel's default
/// range ([`LOCAL_PORTS`]) where none falls in it, else the widest run past 1023 that none
/// does. None where the default stands.
pub(crate) fn local_ports(accepted: &[crate::netplan::Egress]) -> Option<(u16, u16)> {
    let clear = |lo: u16, hi: u16| accepted.iter().all(|&(_, a, b)| b < lo || a > hi);
    if clear(LOCAL_PORTS.0, LOCAL_PORTS.1) {
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

pub(crate) fn addr_msg_prefix(index: i32, addr: Ipv4Addr, prefix: u8) -> Vec<u8> {
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
pub(crate) fn route_msg(
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
pub(crate) fn expr(list: &mut Vec<u8>, name: &[u8], data: impl FnOnce(&mut Vec<u8>)) {
    nested(list, nft::NFTA_LIST_ELEM, |e| {
        attr(e, nft::NFTA_EXPR_NAME, name);
        nested(e, nft::NFTA_EXPR_DATA, data);
    });
}

pub(crate) fn accept(list: &mut Vec<u8>) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_VERDICT);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            nested(v, nft::NFTA_DATA_VERDICT, |c| {
                be32(c, nft::NFTA_VERDICT_CODE, nft::NF_ACCEPT)
            });
        });
    });
}

pub(crate) fn compare(list: &mut Vec<u8>, op: u32, value: &[u8]) {
    expr(list, b"cmp\0", |d| {
        be32(d, nft::NFTA_CMP_SREG, nft::NFT_REG_1);
        be32(d, nft::NFTA_CMP_OP, op);
        nested(d, nft::NFTA_CMP_DATA, |v| attr(v, nft::NFTA_DATA_VALUE, value));
    });
}

pub(crate) fn meta(list: &mut Vec<u8>, key: u32) {
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
pub(crate) struct Batch {
    msgs: Vec<(u16, u16, Vec<u8>)>,
    family: u8,
}

impl Batch {
    pub(crate) fn new() -> Batch {
        Batch::of(nft::NFPROTO_IPV4)
    }

    pub(crate) fn of(family: u8) -> Batch {
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
    pub(crate) fn chain(&mut self, name: &[u8], kind: &[u8], hook: u32, priority: u32, policy: u32) {
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

    pub(crate) fn rule(&mut self, chain: &[u8], exprs: Vec<u8>) {
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

    /// Every rule of `chain` gone (a DELRULE naming no rule, nft_delrule_by_chain).
    pub(crate) fn flush(&mut self, chain: &[u8]) {
        let mut r = Vec::new();
        attr(&mut r, nft::NFTA_RULE_TABLE, TABLE);
        attr(&mut r, nft::NFTA_RULE_CHAIN, chain);
        self.msgs.push(nft_msg(self.family, nft::NFT_MSG_DELRULE, 0, r));
    }

    pub(crate) fn commit(mut self, sock: &OwnedFd) -> io::Result<()> {
        self.msgs.push(Batch::bound(nft::NFNL_MSG_BATCH_END));
        exchange(sock, &self.msgs)
    }
}

/// The chain of the ports several domains are given (D122), each to the domain the
/// chooser finds listening on it, at eth0's own address, before `pre` (crate::chooser).
const CHOSEN: &[u8] = b"chosen\0";

/// The chain that puts a chosen UDP port's flows in its choice's conntrack zone, before
/// connection tracking (D122).
const CHOSEN_ZONE: &[u8] = b"chosen-zone\0";

/// One of the chooser's choices: what comes in on eth0 to ports `from` goes to `to`, at its
/// ports `at`; and, for UDP's, the conntrack zone of that holder, which its flows, both
/// ways, are tracked in.
pub(crate) type Chosen = (
    crate::netplan::Egress,
    Ipv4Addr,
    crate::netplan::Egress,
    Option<u16>,
);

/// The chooser's choices, programmed again whole in one transaction: what comes in on
/// eth0 to each port `from` goes to `to`, at its port `at`, as `pre` gives a port that is
/// one domain's; a port of no one listening, nowhere here (the run's own command's, as a
/// port no domain claims is). A UDP port's flows are tracked in its holder's zone, coming
/// in and the holder's answers going out: each holder has a zone of its own for the run
/// (crate::chooser's `Choices`), in which a flow admitted before a change is new, and so
/// translated to the new holder, its entry in the old holder's zone left to expire, or
/// taken again, translated to that holder, should it hold the port again; the old
/// holder's answers past the change are new flows no grant opens. Linux keeps a NAT binding for a flow's life (nf_nat_core.c; a
/// UDP flow lives 30 s past its last datagram, 120 s once it streams,
/// nf_conntrack_proto_udp.c), and a guest kernel without ctnetlink (shards' has none) has
/// no other way to let one go.
///
/// The choices are eth0's own address's alone, `own`: what comes in to it, and the holders'
/// answers to anywhere but `shared`, the subnets of the networks shared by runs the run is
/// on (D285). Their flows come in to eth0's second address on each, which `shared-in` gives
/// its members before `chosen` sees them, and are tracked in zone 0 both ways: a member's
/// answer to one, put in its zone as a holder, would meet no flow there and go untranslated.
///
/// On `nft`, a netfilter socket its caller keeps for its life: closing one that has
/// committed waits out a grace period (nft_rcv_nl_event flushes the transactions' destroy
/// work, which waits in synchronize_rcu, nf_tables_api.c; PM M513).
pub(crate) fn program_chosen(
    nft: &OwnedFd,
    own: Ipv4Addr,
    shared: &[(Ipv4Addr, u8)],
    chosen: &[Chosen],
) -> io::Result<()> {
    chosen_batch(interface_index("eth0")?, own, shared, chosen).commit(nft)
}

/// [`program_chosen`]'s transaction, made, eth0 at index `eth0`.
fn chosen_batch(eth0: u32, own: Ipv4Addr, shared: &[(Ipv4Addr, u8)], chosen: &[Chosen]) -> Batch {
    let mut b = Batch::new();
    b.chain(
        CHOSEN_ZONE,
        b"filter\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_RAW,
        nft::NF_ACCEPT,
    );
    b.flush(CHOSEN_ZONE);
    let zone = |e: &mut Vec<u8>, z: u16| {
        load(e, &z.to_ne_bytes());
        expr(e, b"ct\0", |d| {
            be32(d, nft::NFTA_CT_KEY, nft::NFT_CT_ZONE);
            be32(d, nft::NFTA_CT_SREG, nft::NFT_REG_1);
        });
    };
    // Each address matched after its ports, so that a datagram of no chosen port pays for
    // no more than it did: this chain sees every packet.
    for &(from, to, at, z) in chosen {
        let Some(z) = z else { continue };
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        ports(&mut e, from);
        address(&mut e, DESTINATION, own);
        zone(&mut e, z);
        b.rule(CHOSEN_ZONE, e);
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
        address(&mut e, SOURCE, to);
        source_ports(&mut e, at);
        for &subnet in shared {
            not_within(&mut e, DESTINATION, subnet);
        }
        zone(&mut e, z);
        b.rule(CHOSEN_ZONE, e);
    }
    b.chain(
        CHOSEN,
        b"nat\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_DNAT.wrapping_sub(1),
        nft::NF_ACCEPT,
    );
    b.flush(CHOSEN);
    for &(from, to, at, _) in chosen {
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        ports(&mut e, from);
        address(&mut e, DESTINATION, own);
        load(&mut e, &to.octets());
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
        b.rule(CHOSEN, e);
    }
    b
}

/// On the host's network (D124): what the run's processes send IPv6's loopback (`::1`)
/// goes to the host's, its TCP and UDP. IPv6 keeps `::1` off every link but the loopback
/// (RFC 4291 §2.5.3), so it goes by address translation: to `gateway6`, which the network
/// process takes for the host's own `::1`, from eth0's address, `guest6`, its answers
/// translated back by connection tracking. But for the ports `held` (each a protocol and a
/// port) by the run's own servers there, which stay the guest's. Made again whole, in one
/// transaction, as those change: chains `host-out`, at the output hook as NAT's
/// destination translation is, and `host-post`, at postrouting, of table `shards` of
/// family ip6. On `nft`, a netfilter socket its caller keeps, as [`program_chosen`]'s.
pub fn host_loopback6(
    nft: &OwnedFd,
    guest6: Ipv6Addr,
    gateway6: Ipv6Addr,
    held: &[(u8, u16)],
) -> io::Result<()> {
    let eth0 = interface_index("eth0")?;
    let mut b = Batch::of(nft::NFPROTO_IPV6);
    b.chain(
        b"host-out\0",
        b"nat\0",
        nft::NF_INET_LOCAL_OUT,
        nft::PRIORITY_DNAT,
        nft::NF_ACCEPT,
    );
    b.flush(b"host-out\0");
    for &(proto, port) in held {
        let mut e = Vec::new();
        address6(&mut e, DESTINATION6, Ipv6Addr::LOCALHOST);
        ports(&mut e, (proto, port, port));
        accept(&mut e);
        b.rule(b"host-out\0", e);
    }
    for proto in [libc::IPPROTO_TCP as u8, libc::IPPROTO_UDP as u8] {
        let mut e = Vec::new();
        address6(&mut e, DESTINATION6, Ipv6Addr::LOCALHOST);
        meta(&mut e, nft::NFT_META_L4PROTO);
        compare(&mut e, nft::NFT_CMP_EQ, &[proto]);
        load(&mut e, &gateway6.octets());
        expr(&mut e, b"nat\0", |d| {
            be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_DNAT);
            be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV6));
            be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
        });
        b.rule(b"host-out\0", e);
    }
    b.chain(
        b"host-post\0",
        b"nat\0",
        nft::NF_INET_POST_ROUTING,
        nft::PRIORITY_SNAT,
        nft::NF_ACCEPT,
    );
    b.flush(b"host-post\0");
    let mut e = Vec::new();
    on(&mut e, nft::NFT_META_OIF, eth0);
    address6(&mut e, SOURCE6, Ipv6Addr::LOCALHOST);
    load(&mut e, &guest6.octets());
    expr(&mut e, b"nat\0", |d| {
        be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_SNAT);
        be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV6));
        be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
    });
    b.rule(b"host-post\0", e);
    b.commit(nft)
}

/// An interface's index, by its name.
pub(crate) fn interface_index(name: &str) -> io::Result<u32> {
    let name = std::ffi::CString::new(name).map_err(|_| io::Error::other("a name with a NUL"))?;
    // SAFETY: if_nametoindex(3) with a NUL-terminated name.
    match unsafe { libc::if_nametoindex(name.as_ptr()) } {
        0 => Err(io::Error::last_os_error()),
        index => Ok(index),
    }
}

/// `ct state established,related accept`.
/// The verdict drop.
pub(crate) fn drop_verdict(list: &mut Vec<u8>) {
    expr(list, b"immediate\0", |d| {
        be32(d, nft::NFTA_IMMEDIATE_DREG, nft::NFT_REG_VERDICT);
        nested(d, nft::NFTA_IMMEDIATE_DATA, |v| {
            nested(v, nft::NFTA_DATA_VERDICT, |c| {
                be32(c, nft::NFTA_VERDICT_CODE, nft::NF_DROP)
            });
        });
    });
}

pub(crate) fn answers() -> Vec<u8> {
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
pub(crate) fn masked(list: &mut Vec<u8>, mask: &[u8]) {
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
pub(crate) fn payload(list: &mut Vec<u8>, base: u32, offset: u32, len: u32) {
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
pub(crate) fn load(list: &mut Vec<u8>, value: &[u8]) {
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
pub(crate) fn address(list: &mut Vec<u8>, offset: u32, addr: Ipv4Addr) {
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 4);
    compare(list, nft::NFT_CMP_EQ, &addr.octets());
}
pub(crate) const SOURCE: u32 = 12;
pub(crate) const DESTINATION: u32 = 16;

/// TCP to `gateway`, eth0's, dropped but at its resolver's port, 53 (D236): the gateway is
/// the host itself at each port a member is granted the host at, which the run's own
/// processes and the members its engine makes are granted at none. Under a run's grants
/// the gateway serves them nothing else by TCP (its network process's resolver alone).
fn not_the_host(list: &mut Vec<u8>, gateway: Ipv4Addr) {
    address(list, DESTINATION, gateway);
    meta(list, nft::NFT_META_L4PROTO);
    compare(list, nft::NFT_CMP_EQ, &[6]);
    payload(list, nft::NFT_PAYLOAD_TRANSPORT_HEADER, 2, 2);
    compare(list, nft::NFT_CMP_NEQ, &53u16.to_be_bytes());
    drop_verdict(list);
}

/// The packet's address at `offset` (its source's or destination's) within `subnet`.
fn within(list: &mut Vec<u8>, offset: u32, (net, prefix): (Ipv4Addr, u8)) {
    let mask = 32u32
        .checked_sub(u32::from(prefix))
        .and_then(|free| u32::MAX.checked_shl(free))
        .unwrap_or(0);
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 4);
    masked(list, &mask.to_be_bytes());
    compare(list, nft::NFT_CMP_EQ, &(u32::from(net) & mask).to_be_bytes());
}

/// The packet's address at `offset` (its source's or destination's) outside `subnet`.
fn not_within(list: &mut Vec<u8>, offset: u32, (net, prefix): (Ipv4Addr, u8)) {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 4);
    masked(list, &mask.to_be_bytes());
    compare(list, nft::NFT_CMP_NEQ, &(u32::from(net) & mask).to_be_bytes());
}

/// The packet's IPv6 address at `offset` outside `subnet`.
fn not_within6(list: &mut Vec<u8>, offset: u32, (net, prefix): (Ipv6Addr, u8)) {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    payload(list, nft::NFT_PAYLOAD_NETWORK_HEADER, offset, 16);
    masked(list, &mask.to_be_bytes());
    compare(list, nft::NFT_CMP_NEQ, &(u128::from(net) & mask).to_be_bytes());
}

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
pub(crate) fn on(list: &mut Vec<u8>, key: u32, index: u32) {
    meta(list, key);
    compare(list, nft::NFT_CMP_EQ, &index.to_ne_bytes());
}

/// The switch's nf_tables, in one transaction, and none of them tracking anything
/// (D61): chain `input`, which drops all but the agents' questions to its resolver, from
/// each domain of `dns`, and its resolver's answers from the microVM's; chain
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
    policy_batch(pairs, egress, ingress, dns)?.commit(sock)
}

/// [`policy`]'s transaction, made.
fn policy_batch(
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    egress: &[(usize, Vec<crate::netplan::Egress>)],
    ingress: &[(usize, Vec<crate::netplan::Egress>)],
    dns: &[usize],
) -> io::Result<Batch> {
    let up = UPLINK_IFINDEX as u32;
    let mut b = Batch::new();
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    // Programmed again whole as members come and go (D181): emptied first, in the same
    // transaction, so that no packet meets a part of either.
    b.flush(b"input\0");
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
    b.flush(b"forward\0");
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
    Ok(b)
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
    gate_batch(g).commit(sock)
}

/// [`gate_policy`]'s transaction, made.
fn gate_batch(g: &Gate) -> Batch {
    let (inside, outside) = (GATE_IN as u32, GATE_OUT as u32);
    let mut b = Batch::new();
    b.chain(
        b"raw\0",
        b"filter\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_RAW,
        nft::NF_ACCEPT,
    );
    // Programmed again whole as a member's peers change (D181).
    b.flush(b"raw\0");
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
    b.flush(b"forward\0");
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
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_IIF, inside);
            address(&mut e, SOURCE, own);
            for &subnet in &g.subnets {
                not_within(&mut e, DESTINATION, subnet);
            }
            ports(&mut e, range);
            accept(&mut e);
            b.rule(b"forward\0", e);
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
    b
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
    b.flush(b"pre\0");
    b.rule(b"pre\0", reverse_path());
    b.chain(b"input\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_DROP);
    b.flush(b"input\0");
    b.rule(b"input\0", neighbor_discovery());
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.flush(b"forward\0");
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
    b.flush(b"raw\0");
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
    b.flush(b"input\0");
    b.rule(b"input\0", neighbor_discovery());
    b.chain(b"output\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_DROP);
    b.flush(b"output\0");
    b.rule(b"output\0", neighbor_discovery());
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.flush(b"forward\0");
    b.rule(b"forward\0", answers());
    for &own in &g.own6 {
        // Past the microVM, on its egress ports, where the uplink carries IPv6.
        if g.egress6 {
            for &range in &g.egress {
                let mut e = Vec::new();
                on(&mut e, nft::NFT_META_IIF, inside);
                address6(&mut e, SOURCE6, own);
                for &subnet in &g.subnets6 {
                    not_within6(&mut e, DESTINATION6, subnet);
                }
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
    b.flush(b"in\0");
    b.rule(b"in\0", answers());
    b.rule(b"in\0", neighbor_discovery());
    let mut e = Vec::new();
    on(&mut e, nft::NFT_META_IIF, up);
    drop_verdict(&mut e);
    b.rule(b"in\0", e);
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.flush(b"forward\0");
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
    b.flush(b"post\0");
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
///   down the link takes (D99);
/// - on each network shared by runs (D285), eth0's second address: `shared-in` gives what a
///   grant in lets in to the run's address there its member's first address, before the
///   chooser's chain and `pre` see it, and drops anything else new to that address; `forward`
///   lets each domain's flows out to the peers its grants out name, on their ports, marked,
///   and nothing else to the network's subnet; `post` gives them the run's address there;
///   `in` and `own` keep the run's own processes off it.
///
/// The run's own processes were kept to eth0's subnet before its command started
/// ([`confine_eth0`]).
fn outside(sock: &OwnedFd, eth0: u32, u: &Uplink, members: &[Member], shared: &[Shared]) -> io::Result<()> {
    for family in [nft::NFPROTO_IPV4, nft::NFPROTO_IPV6] {
        own_batch(family, eth0, members, shared).commit(sock)?;
    }
    outside_batch(eth0, u, members, shared).commit(sock)
}

/// [`outside`]'s chain `own` of `family`, made.
fn own_batch(family: u8, eth0: u32, members: &[Member], shared: &[Shared]) -> Batch {
    let mut b = Batch::of(family);
    b.chain(b"own\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_ACCEPT);
    // Programmed again whole as members come and go (D181).
    b.flush(b"own\0");
    if family == nft::NFPROTO_IPV6 {
        b.rule(b"own\0", neighbor_discovery());
    }
    if family == nft::NFPROTO_IPV4 {
        // What the run's command reaches of the members its engine made: the ports each
        // publishes to it, at its own address, and nothing else of theirs (D181).
        for m in members {
            for &at in &m.addrs {
                for &range in &m.published {
                    let mut e = Vec::new();
                    on(&mut e, nft::NFT_META_OIF, UPLINK_IFINDEX as u32);
                    address(&mut e, DESTINATION, at);
                    ports(&mut e, range);
                    accept(&mut e);
                    b.rule(b"own\0", e);
                }
            }
        }
        // Nothing of the run's own on a network shared by runs (D285): none of its flows
        // from the run's address there, nor to its subnet, which no grant names.
        for s in shared {
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_OIF, eth0);
            address(&mut e, SOURCE, s.addr);
            drop_verdict(&mut e);
            b.rule(b"own\0", e);
            let mut e = Vec::new();
            on(&mut e, nft::NFT_META_OIF, eth0);
            within(&mut e, DESTINATION, s.subnet);
            drop_verdict(&mut e);
            b.rule(b"own\0", e);
        }
    }
    let mut e = Vec::new();
    on(&mut e, nft::NFT_META_OIF, UPLINK_IFINDEX as u32);
    drop_verdict(&mut e);
    b.rule(b"own\0", e);
    b
}

/// The chain that gives what a grant in on a network shared by runs lets in its member's
/// address (D285): at NAT's priority less two, so that `pre`, which takes a port whatever
/// address it came to (D122), sees none of it, nor the chooser's chain ([`CHOSEN`]), which
/// takes eth0's own address alone ([`program_chosen`]).
const SHARED_IN: &[u8] = b"shared-in\0";

/// [`outside`]'s chains of family ip but `own`, made.
fn outside_batch(eth0: u32, u: &Uplink, members: &[Member], shared: &[Shared]) -> Batch {
    let ((addr, _, _), ingress) = (u.eth0, &u.ingress);
    let mut b = Batch::new();
    b.chain(b"in\0", b"filter\0", nft::NF_INET_LOCAL_IN, 0, nft::NF_ACCEPT);
    b.flush(b"in\0");
    b.rule(b"in\0", answers());
    let mut e = Vec::new();
    meta(&mut e, nft::NFT_META_IIF);
    compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
    drop_verdict(&mut e);
    b.rule(b"in\0", e);
    // Nothing new reaches the run's own processes at its address on a network shared by runs
    // (D285): what a grant lets in there `shared-in` gave its member.
    for s in shared {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, eth0);
        address(&mut e, DESTINATION, s.addr);
        drop_verdict(&mut e);
        b.rule(b"in\0", e);
    }
    b.chain(
        SHARED_IN,
        b"nat\0",
        nft::NF_INET_PRE_ROUTING,
        nft::PRIORITY_DNAT.wrapping_sub(2),
        nft::NF_ACCEPT,
    );
    b.flush(SHARED_IN);
    for s in shared {
        for f in &s.flows {
            let Some(&to) = f.own.first() else { continue };
            for (peers, ranges) in &f.accepts {
                for &from in peers {
                    for &range in ranges {
                        let mut e = Vec::new();
                        on(&mut e, nft::NFT_META_IIF, eth0);
                        address(&mut e, SOURCE, from);
                        address(&mut e, DESTINATION, s.addr);
                        ports(&mut e, range);
                        load(&mut e, &to.octets());
                        expr(&mut e, b"nat\0", |d| {
                            be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_DNAT);
                            be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV4));
                            be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
                        });
                        b.rule(SHARED_IN, e);
                    }
                }
            }
        }
        // A new flow no grant in admits, dropped (a NAT chain sees a flow's first packet alone,
        // so the answers of what goes out from this address pass).
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, eth0);
        address(&mut e, DESTINATION, s.addr);
        drop_verdict(&mut e);
        b.rule(SHARED_IN, e);
    }
    b.chain(b"forward\0", b"filter\0", nft::NF_INET_FORWARD, 0, nft::NF_DROP);
    b.flush(b"forward\0");
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
        b.flush(b"pre\0");
        // A port several domains are given goes to the one listening (D122): chosen, as
        // they listen and stop, by the chooser (crate::chooser) in a chain of its own,
        // before this one; every other to its one domain here.
        let contested = crate::chooser::contested(ingress);
        if !contested.is_empty() {
            b.chain(
                CHOSEN,
                b"nat\0",
                nft::NF_INET_PRE_ROUTING,
                nft::PRIORITY_DNAT.wrapping_sub(1),
                nft::NF_ACCEPT,
            );
        }
        for (to, ranges) in ingress {
            // Each range's ports no contest is for, at the domain's own ports there.
            let alone = ranges.iter().flat_map(|&(from, at)| {
                crate::chooser::uncontested(from, &contested)
                    .into_iter()
                    .filter_map(move |(lo, hi)| {
                        let shift = |port: u16| port.checked_sub(from.1).and_then(|d| at.1.checked_add(d));
                        Some(((from.0, lo, hi), (at.0, shift(lo)?, shift(hi)?)))
                    })
            });
            for (from, at) in alone {
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
    // it is granted; each member's its engine made, as far as the run's command reaches
    // (D181); the agents' resolver's, from the switch's end of the uplink.
    let up = |b: &mut Batch, from: Ipv4Addr, range: crate::netplan::Egress, to: Option<(Ipv4Addr, u8)>| {
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_IIF);
        compare(&mut e, nft::NFT_CMP_EQ, &(UPLINK_IFINDEX as u32).to_ne_bytes());
        meta(&mut e, nft::NFT_META_OIF);
        compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
        address(&mut e, SOURCE, from);
        if let Some(subnet) = to {
            within(&mut e, DESTINATION, subnet);
        }
        ports(&mut e, range);
        load(&mut e, &MARK_AGENTS.to_ne_bytes());
        expr(&mut e, b"meta\0", |d| {
            be32(d, nft::NFTA_META_KEY, nft::NFT_META_MARK);
            be32(d, nft::NFTA_META_SREG, nft::NFT_REG_1);
        });
        accept(&mut e);
        b.rule(b"forward\0", e);
    };
    // Each domain's flows on the networks shared by runs (D285), before any grant past the
    // microVM: in, from each peer a grant in names to the address `shared-in` gave them, on
    // its ports; out, from its addresses to each peer a grant out names, on the peer's ports,
    // marked; and nothing else of a domain's, nor of a member its engine made, to their subnets.
    for s in shared {
        for f in &s.flows {
            if let Some(&to) = f.own.first() {
                for (peers, ranges) in &f.accepts {
                    for &from in peers {
                        for &range in ranges {
                            let mut e = Vec::new();
                            on(&mut e, nft::NFT_META_IIF, eth0);
                            on(&mut e, nft::NFT_META_OIF, UPLINK_IFINDEX as u32);
                            address(&mut e, SOURCE, from);
                            address(&mut e, DESTINATION, to);
                            ports(&mut e, range);
                            accept(&mut e);
                            b.rule(b"forward\0", e);
                        }
                    }
                }
            }
            for (peers, ranges) in &f.opens {
                for &from in &f.own {
                    for &to in peers {
                        for &range in ranges {
                            up(&mut b, from, range, Some((to, 32)));
                        }
                    }
                }
            }
        }
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, UPLINK_IFINDEX as u32);
        within(&mut e, DESTINATION, s.subnet);
        drop_verdict(&mut e);
        b.rule(b"forward\0", e);
    }
    for (from, ranges) in &u.egress {
        for &a in from {
            for &range in ranges {
                up(&mut b, a, range, None);
            }
        }
    }
    // Past the domains' own grants, the host itself is no member's its engine made: within
    // eth0's subnet, they would reach its gateway (D236).
    if !members.is_empty() {
        let mut e = Vec::new();
        on(&mut e, nft::NFT_META_IIF, UPLINK_IFINDEX as u32);
        on(&mut e, nft::NFT_META_OIF, eth0);
        not_the_host(&mut e, u.gateway);
        b.rule(b"forward\0", e);
    }
    for m in members {
        let within = match m.reach {
            Reach::Nowhere => continue,
            Reach::AsTheCommand => None,
            Reach::Within(subnet) => Some(subnet),
        };
        for &a in &m.addrs {
            for range in EVERY_PORT {
                up(&mut b, a, range, within);
            }
        }
    }
    if u.resolver || members.iter().any(|m| m.dns) {
        up(&mut b, UPLINK_SWITCH, (17, 53, 53), None);
        up(&mut b, UPLINK_SWITCH, (6, 53, 53), None);
    }
    b.chain(
        b"post\0",
        b"nat\0",
        nft::NF_INET_POST_ROUTING,
        nft::PRIORITY_SNAT,
        nft::NF_ACCEPT,
    );
    b.flush(b"post\0");
    let snat = |b: &mut Batch, to: Option<(Ipv4Addr, u8)>, from: Ipv4Addr| {
        let mut e = Vec::new();
        meta(&mut e, nft::NFT_META_MARK);
        compare(&mut e, nft::NFT_CMP_EQ, &MARK_AGENTS.to_ne_bytes());
        if let Some(subnet) = to {
            within(&mut e, DESTINATION, subnet);
        }
        load(&mut e, &from.octets());
        expr(&mut e, b"nat\0", |d| {
            be32(d, nft::NFTA_NAT_TYPE, nft::NFT_NAT_SNAT);
            be32(d, nft::NFTA_NAT_FAMILY, u32::from(nft::NFPROTO_IPV4));
            be32(d, nft::NFTA_NAT_REG_ADDR_MIN, nft::NFT_REG_1);
        });
        b.rule(b"post\0", e);
    };
    // A flow to a network shared by runs leaves from the run's address there (D285), the
    // network process taking it from that address to that network alone.
    for s in shared {
        snat(&mut b, Some(s.subnet), s.addr);
    }
    snat(&mut b, None, addr);
    b
}

/// Keeps the run's own processes, of both IP versions, to what they reached before a run
/// whose image grants its agents anything past the microVM (D59, D99): the host's network
/// process holds the agents' grants for the whole microVM, from before the run, so no new
/// flow leaves eth0 but to eth0's own subnet (its network's members, D46), and none by TCP
/// to its gateway but its resolver's, the gateway being the host itself where a member is
/// granted the host (D236). Chain `out` of
/// family ip, and of ip6 where eth0 has IPv6, each dropping the rest. Made before the
/// run's command starts, and fails the run where the kernel has no ip6 family for it.
pub fn confine_eth0() -> io::Result<()> {
    // SAFETY: if_nametoindex(3) of a NUL-terminated literal.
    let eth0 = unsafe { libc::if_nametoindex(c"eth0".as_ptr()) };
    if eth0 == 0 {
        return Err(io::Error::other("no eth0 to confine"));
    }
    // Answers, then the subnet's own, but the host itself at its gateway, then nothing else
    // out eth0.
    let out = |sock: &OwnedFd,
               family: u8,
               at: u32,
               subnet: &[u8],
               mask: &[u8],
               gateway: Option<Ipv4Addr>|
     -> io::Result<()> {
        let mut b = Batch::of(family);
        b.chain(b"out\0", b"filter\0", nft::NF_INET_LOCAL_OUT, 0, nft::NF_ACCEPT);
        b.rule(b"out\0", answers());
        // IPv6's neighbor discovery, to multicast addresses outside any subnet: what
        // reaching the gateway at all takes, as ARP does for IPv4 below IP.
        if family == nft::NFPROTO_IPV6 {
            b.rule(b"out\0", neighbor_discovery());
        }
        // The gateway, of the subnet, is the host itself at a port a member is granted the
        // host at (D236), which the run's own processes are not.
        if let Some(gateway) = gateway {
            let mut e = Vec::new();
            meta(&mut e, nft::NFT_META_OIF);
            compare(&mut e, nft::NFT_CMP_EQ, &eth0.to_ne_bytes());
            not_the_host(&mut e, gateway);
            b.rule(b"out\0", e);
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
        b.commit(sock)
    };
    let (addr, prefix, gateway) =
        crate::net::current().ok_or_else(|| io::Error::other("eth0 has no address"))?;
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    on_init_nft(|sock| {
        // The destination address: at offset 16 of IPv4's header, 24 of IPv6's.
        out(
            sock,
            nft::NFPROTO_IPV4,
            DESTINATION,
            &Ipv4Addr::from(u32::from(addr) & mask).octets(),
            &mask.to_be_bytes(),
            Some(gateway),
        )?;
        if let Some((addr6, prefix6, _)) = crate::net::current6() {
            let mask6 = u128::MAX.checked_shl(128 - u32::from(prefix6)).unwrap_or(0);
            out(
                sock,
                nft::NFPROTO_IPV6,
                24,
                &std::net::Ipv6Addr::from(u128::from(addr6) & mask6).octets(),
                &mask6.to_be_bytes(),
                None,
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// The rules a transaction adds, each its payload as sent.
    fn rules(b: &Batch) -> Vec<Vec<u8>> {
        let rule = (nft::NFNL_SUBSYS_NFTABLES << 8) | nft::NFT_MSG_NEWRULE;
        b.msgs
            .iter()
            .filter(|(kind, _, _)| *kind == rule)
            .map(|(_, _, payload)| payload.clone())
            .collect()
    }

    /// Whether `rule` holds `bytes` somewhere in it.
    fn holds(rule: &[u8], bytes: &[u8]) -> bool {
        rule.windows(bytes.len()).any(|w| w == bytes)
    }

    /// A member granted a TCP port as its run goes (D278): the switch's transaction carries
    /// its flows up its link on that port and their answers back, two rules more, and names
    /// its link and its port; the agents' resolver taken by it, two more each way of DNS's;
    /// revoked, what it carried before, rule for rule. Its gate's transaction lets it out on
    /// the port from its own address, to none of the microVM's subnets, and to the resolver
    /// where it asks names, and none of it once revoked; each transaction empties its chains
    /// first, so that nothing of what it carried is left.
    #[test]
    fn a_grant_and_its_revoke_program_the_switch_and_the_gate_whole() {
        let image = vec![(0usize, vec![(6u8, 443u16, 443u16)])];
        let before = policy_batch(&[], &image, &[], &[]).unwrap();
        let mut granted = image.clone();
        granted.push((1, vec![(6, 8443, 8443)]));
        let after = policy_batch(&[], &granted, &[], &[1]).unwrap();
        let (was, is) = (rules(&before), rules(&after));
        assert_eq!(
            is.len(),
            was.len() + 2 + 2,
            "its flow each way, and its questions by UDP and TCP"
        );
        let link = link_index(1).unwrap().to_ne_bytes();
        let port = 8443u16.to_be_bytes();
        let new: Vec<&Vec<u8>> = is.iter().filter(|r| !was.contains(r)).collect();
        assert_eq!(
            new.iter().filter(|r| holds(r, &link) && holds(r, &port)).count(),
            2
        );
        assert!(was.iter().all(|r| is.contains(r)), "what it carried, kept");
        let revoked = policy_batch(&[], &image, &[], &[]).unwrap();
        assert_eq!(rules(&revoked), was);
        let delete = (nft::NFNL_SUBSYS_NFTABLES << 8) | nft::NFT_MSG_DELRULE;
        assert_eq!(
            after.msgs.iter().filter(|(kind, _, _)| *kind == delete).count(),
            2,
            "input and forward emptied"
        );
        let own = Ipv4Addr::new(10, 88, 0, 2);
        let gate = Gate {
            own: vec![own],
            subnets: vec![(Ipv4Addr::new(10, 88, 0, 0), 16)],
            ..Gate::default()
        };
        let shut = rules(&gate_batch(&gate));
        let open = rules(&gate_batch(&Gate {
            egress: vec![(6, 8443, 8443)],
            dns: true,
            ..gate.clone()
        }));
        let opened: Vec<&Vec<u8>> = open.iter().filter(|r| !shut.contains(r)).collect();
        assert_eq!(opened.len(), 3, "its port, and DNS by UDP and TCP");
        assert!(opened.iter().all(|r| holds(r, &own.octets())));
        assert_eq!(opened.iter().filter(|r| holds(r, &port)).count(), 1);
        assert_eq!(rules(&gate_batch(&gate)), shut, "revoked, as it was");
    }

    /// Whether `rule` is one of `chain`'s: its NFTA_RULE_CHAIN attribute, length and all.
    fn of_chain(rule: &[u8], chain: &[u8]) -> bool {
        let mut named = Vec::new();
        attr(&mut named, nft::NFTA_RULE_CHAIN, chain);
        holds(rule, &named)
    }

    /// A network shared by runs (D285), at init's end: a grant in translated in `shared-in`
    /// from its peer at the run's address there to its member's first address, and anything
    /// else new to that address dropped there and in `in`; a grant out let up from the
    /// member's address to its peer's port, marked, and nothing else to the subnet; its flows
    /// given the run's address there before eth0's own; the run's own processes kept off it.
    /// With none, `shared-in` is made empty, and the rest is as it was. The member's gate holds
    /// its grants' peers and ports alone, another's gate none of them; the switch carries its
    /// link up to the peer's port and down to its own, and its questions.
    #[test]
    fn a_shared_networks_flows_are_translated_and_held_by_their_grants() {
        let own = Ipv4Addr::new(172, 30, 0, 2);
        let run = Ipv4Addr::new(10, 89, 0, 2);
        let (db, web) = (Ipv4Addr::new(10, 89, 0, 3), Ipv4Addr::new(10, 89, 0, 4));
        let subnet = (Ipv4Addr::new(10, 89, 0, 0), 24);
        let shared = vec![Shared {
            addr: run,
            subnet,
            flows: vec![Flows {
                domain: 1,
                own: vec![own],
                opens: vec![(vec![db], vec![(6, 5432, 5432)])],
                accepts: vec![(vec![web], vec![(6, 8080, 8080)])],
            }],
        }];
        let eth0_addr = Ipv4Addr::new(172, 17, 0, 5);
        let u = Uplink {
            subnets: vec![(Ipv4Addr::new(172, 30, 0, 0), 24)],
            eth0: (eth0_addr, Ipv4Addr::new(172, 17, 0, 0), 16),
            gateway: Ipv4Addr::new(172, 17, 0, 1),
            ingress: Vec::new(),
            egress: vec![(vec![own], vec![(6, 443, 443)])],
            resolver: false,
            ipv6: None,
        };
        let eth0 = 2u32;
        let none = outside_batch(eth0, &u, &[], &[]);
        let delete = (nft::NFNL_SUBSYS_NFTABLES << 8) | nft::NFT_MSG_DELRULE;
        assert!(
            none.msgs
                .iter()
                .any(|(kind, _, m)| *kind == delete && of_chain(m, SHARED_IN)),
            "made and emptied with none"
        );
        let (was, is) = (rules(&none), rules(&outside_batch(eth0, &u, &[], &shared)));
        assert!(was.iter().all(|r| is.contains(r)), "what it carried, kept");
        let new: Vec<&Vec<u8>> = is.iter().filter(|r| !was.contains(r)).collect();
        assert_eq!(new.len(), 7, "{new:?}");
        let (port_in, port_out) = (8080u16.to_be_bytes(), 5432u16.to_be_bytes());
        let has = |chain: &[u8], parts: &[&[u8]]| {
            new.iter()
                .filter(|r| of_chain(r, chain) && parts.iter().all(|p| holds(r, p)))
                .count()
        };
        assert_eq!(
            has(
                SHARED_IN,
                &[&web.octets(), &run.octets(), &own.octets(), &port_in]
            ),
            1,
            "web's flows in, to the member"
        );
        assert_eq!(has(SHARED_IN, &[&run.octets()]), 2, "and the rest, dropped");
        assert_eq!(has(b"in\0", &[&run.octets()]), 1);
        assert_eq!(has(b"forward\0", &[&web.octets(), &own.octets(), &port_in]), 1);
        assert_eq!(
            has(
                b"forward\0",
                &[&own.octets(), &db.octets(), &port_out, &MARK_AGENTS.to_ne_bytes()]
            ),
            1
        );
        assert_eq!(has(b"forward\0", &[&subnet.0.octets()]), 1, "nothing else to it");
        assert_eq!(has(b"post\0", &[&run.octets(), &subnet.0.octets()]), 1);
        let post = |addr: Ipv4Addr| {
            is.iter()
                .position(|r| of_chain(r, b"post\0") && holds(r, &addr.octets()))
                .unwrap()
        };
        assert!(
            post(run) < post(eth0_addr),
            "the shared network's before eth0's own"
        );
        let (alone, kept_off) = (
            rules(&own_batch(nft::NFPROTO_IPV4, eth0, &[], &[])),
            rules(&own_batch(nft::NFPROTO_IPV4, eth0, &[], &shared)),
        );
        assert_eq!(kept_off.len(), alone.len() + 2);
        assert!(kept_off.iter().any(|r| holds(r, &run.octets())));
        assert!(kept_off.iter().any(|r| holds(r, &subnet.0.octets())));
        assert_eq!(
            rules(&own_batch(nft::NFPROTO_IPV6, eth0, &[], &shared)),
            rules(&own_batch(nft::NFPROTO_IPV6, eth0, &[], &[])),
            "IPv4 alone"
        );
        // Its gate: its peer's port out, and its own in from that peer alone, beside its own.
        let kept = Gate {
            own: vec![own],
            subnets: vec![(Ipv4Addr::new(172, 30, 0, 0), 24)],
            ..Gate::default()
        };
        let g = carried_gate(1, kept.clone(), &[subnet], &shared);
        assert_eq!(
            (&g.opens, &g.accepts),
            (&shared[0].flows[0].opens, &shared[0].flows[0].accepts)
        );
        assert!(g.dns && g.subnets.contains(&subnet) && g.subnets.len() == 2);
        let other = carried_gate(2, kept.clone(), &[subnet], &shared);
        assert!(other.opens.is_empty() && other.accepts.is_empty() && !other.dns);
        assert!(other.subnets.contains(&subnet), "kept off its egress");
        let gated = rules(&gate_batch(&g));
        assert!(
            gated
                .iter()
                .any(|r| holds(r, &own.octets()) && holds(r, &db.octets()) && holds(r, &port_out))
        );
        assert!(
            gated
                .iter()
                .any(|r| holds(r, &web.octets()) && holds(r, &own.octets()) && holds(r, &port_in))
        );
        let (mut egress, mut ingress, mut dns) = (vec![(1, vec![(6, 443, 443)])], Vec::new(), vec![1]);
        sharing(&shared, &mut egress, &mut ingress, &mut dns);
        assert_eq!(egress, vec![(1, vec![(6, 443, 443)]), (1, vec![(6, 5432, 5432)])]);
        assert_eq!(ingress, vec![(1, vec![(6, 8080, 8080)])]);
        assert_eq!(dns, vec![1], "asked once");
    }

    /// The ports' chooser takes eth0's own address alone (D122, D285): what comes in to it, put
    /// in a UDP holder's zone and given to the holder, and the holder's answers to anywhere but
    /// a network shared by runs, whose flows come in to eth0's second address and keep zone 0
    /// both ways; with no such network, its answers everywhere, as before.
    #[test]
    fn the_chooser_takes_eth0s_own_address_alone() {
        let (eth0, own) = (2u32, Ipv4Addr::new(172, 17, 0, 5));
        let holder = Ipv4Addr::new(172, 30, 0, 2);
        let subnet = (Ipv4Addr::new(10, 89, 0, 0), 24);
        let chosen: Vec<Chosen> = vec![
            ((17, 9000, 9000), holder, (17, 9000, 9000), Some(1)),
            ((6, 7000, 7000), holder, (6, 7000, 7000), None),
        ];
        let zoned = |b: &Batch| -> Vec<Vec<u8>> {
            rules(b)
                .into_iter()
                .filter(|r| of_chain(r, CHOSEN_ZONE))
                .collect()
        };
        let alone = zoned(&chosen_batch(eth0, own, &[], &chosen));
        let shared = zoned(&chosen_batch(eth0, own, &[subnet], &chosen));
        assert_eq!((alone.len(), shared.len()), (2, 2), "the UDP choice's, each way");
        let (coming, answers): (Vec<&Vec<u8>>, Vec<&Vec<u8>>) =
            shared.iter().partition(|r| !holds(r, &holder.octets()));
        assert_eq!((coming.len(), answers.len()), (1, 1));
        assert!(
            holds(coming[0], &own.octets()),
            "what comes in to eth0's own address"
        );
        assert!(
            holds(answers[0], &subnet.0.octets()),
            "its answers kept off the network"
        );
        assert!(alone.iter().all(|r| !holds(r, &subnet.0.octets())));
        assert!(
            alone.contains(coming[0]),
            "what comes in, whatever networks it is on"
        );
        let dnat: Vec<Vec<u8>> = rules(&chosen_batch(eth0, own, &[subnet], &chosen))
            .into_iter()
            .filter(|r| of_chain(r, CHOSEN))
            .collect();
        assert_eq!(dnat.len(), 2);
        assert!(
            dnat.iter()
                .all(|r| holds(r, &own.octets()) && holds(r, &holder.octets()))
        );
    }

    /// A domain whose resolv.conf names the agents' resolver has its questions taken whatever
    /// it is granted (D278), once each beside those granted names: by UDP and by TCP, on its
    /// link alone.
    #[test]
    fn a_domain_that_asks_the_resolver_is_taken_whatever_it_is_granted() {
        assert_eq!(questions(&[1], &[1, 2]), vec![1, 2]);
        assert_eq!(questions(&[], &[]), Vec::<usize>::new());
        let shut = rules(&policy_batch(&[], &[], &[], &[]).unwrap());
        let asking = rules(&policy_batch(&[], &[], &[], &questions(&[], &[2])).unwrap());
        let link = link_index(2).unwrap().to_ne_bytes();
        let new: Vec<&Vec<u8>> = asking.iter().filter(|r| !shut.contains(r)).collect();
        assert_eq!(new.len(), 2, "its questions by UDP and by TCP");
        assert!(new.iter().all(|r| holds(r, &link)));
    }

    /// A netlink acknowledgment: an nlmsghdr, the error (0 for none), and the request's
    /// header.
    fn ack(code: i32) -> Vec<u8> {
        let mut m = 36u32.to_ne_bytes().to_vec();
        m.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
        m.extend_from_slice(&[0; 10]);
        m.extend_from_slice(&code.to_ne_bytes());
        m.extend_from_slice(&[0; 16]);
        m
    }

    /// An exchange the kernel refuses part of leaves none of its answers on the socket, so
    /// that one kept (the chooser's, PM M513) counts only the next exchange's for it.
    #[test]
    fn a_kept_socket_holds_no_answer_of_an_exchange_before() {
        let (ours, kernel) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let ours = OwnedFd::from(ours);
        let asked = vec![(0u16, NLM_F_REQUEST | NLM_F_ACK, Vec::new()); 3];
        for code in [0, -libc::EEXIST, 0] {
            kernel.send(&ack(code)).unwrap();
        }
        let e = exchange(&ours, &asked).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
        for _ in 0..3 {
            kernel.send(&ack(0)).unwrap();
        }
        exchange(&ours, &asked).unwrap();
        let mut buf = [0u8; 64];
        // SAFETY: recv(2) into a buffer of ours, without waiting.
        let left = unsafe {
            libc::recv(
                ours.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(left, -1, "an answer was left");
    }

    /// Init's commits at a run's start share one socket, opened once and never closed
    /// between them (closing one waits out a grace period, PM M513): each exchange on it
    /// counts its own answers, and one that fails lets it go, so that the next opens
    /// another.
    #[test]
    fn a_runs_start_commits_on_one_kept_socket() {
        let kernels = std::cell::RefCell::new(Vec::new());
        let open = || {
            let (ours, kernel) = std::os::unix::net::UnixDatagram::pair()?;
            kernels.borrow_mut().push(kernel);
            Ok(OwnedFd::from(ours))
        };
        let answer = |codes: &[i32]| {
            for &code in codes {
                kernels.borrow().last().unwrap().send(&ack(code)).unwrap();
            }
        };
        let asked = vec![(0u16, NLM_F_REQUEST | NLM_F_ACK, Vec::new()); 2];
        let mut kept = None;
        // The first commit opens it, and the ones after use it.
        let first = kept_socket(&mut kept, open, |s| Ok(s.as_raw_fd())).unwrap();
        for _ in 0..3 {
            answer(&[0, 0]);
            let fd = kept_socket(&mut kept, open, |s| {
                exchange(s, &asked)?;
                Ok(s.as_raw_fd())
            })
            .unwrap();
            assert_eq!(fd, first);
        }
        assert_eq!(kernels.borrow().len(), 1);
        // A refused commit lets it go; the next opens another, on which it succeeds.
        answer(&[0, -libc::EEXIST]);
        let e = kept_socket(&mut kept, open, |s| exchange(s, &asked)).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
        assert!(kept.is_none());
        kept_socket(&mut kept, open, |_| Ok(())).unwrap();
        answer(&[0, 0]);
        kept_socket(&mut kept, open, |s| exchange(s, &asked)).unwrap();
        assert_eq!(kernels.borrow().len(), 2);
        assert!(kept.is_some());
    }
}
