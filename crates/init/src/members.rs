//! The containers a microVM's own engine makes, as members of the microVM's switch
//! (D181), each as each of the image's agents is (D59, D61): a network namespace of its
//! own, linked to the switch through a gate of its own, known by its link and never by an
//! address it could claim, granted what it declares and nothing else.
//!
//! Init makes a member's namespace, its gate and its link before any process of it runs
//! ([`join`]); the joiner's child enters it. A member is on one or more of its engine's
//! networks, an address and names on each, as a domain is on its networks (D59 part
//! three). Joining grants nothing: a member accepts, from each member of a network it
//! shares, on the ports it declares, and on no other; its engine's default network, and
//! one made with no peers, grant not even that. It resolves its own names and those of
//! each peer it may reach, answered by the agents' resolver by its link. It reaches past
//! the microVM as far as the run's command does, and no farther, and nowhere from an
//! internal network alone; the command reaches it on the ports it publishes, through a
//! proxy of init's in the command's namespace ([`publish`]).

use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::links::{Reach, in_netns, open_ns};
use crate::netplan::Egress;

/// Whether the run's own processes are kept from what its agents' grants reach past the
/// microVM (`confine-eth0`, D59): its engine's members reach no farther than they.
pub static CONFINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The name the engine's clients give its default network.
pub const DEFAULT: &str = "bridge";

/// What a member publishes, from the command's network namespace: its protocol's number,
/// the address there (none for every one) and port (0 for one the kernel picks), and its
/// own port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub proto: u8,
    pub host: Option<IpAddr>,
    pub host_port: u16,
    pub port: u16,
}

/// What a joiner's setup asks of its membership: each network of its engine's it joins,
/// the address its client asked for there if it asked for one, and its names there
/// (`member=NETWORK\0ADDR[\0NAME...]`, one entry a network, ADDR empty for one init
/// picks), and the ports it declares (`member-accepts=PROTO/PORT[-PORT],...`), which its
/// peers may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub networks: Vec<(String, Option<Ipv4Addr>, Vec<String>)>,
    pub accepts: Vec<Egress>,
}

/// One port or range a member declares, `6/80` or `17/5000-5010`.
fn declared(text: &str) -> Option<Egress> {
    let (proto, ports) = text.split_once('/')?;
    let proto: u8 = proto.parse().ok().filter(|p| matches!(p, 6 | 17))?;
    let (lo, hi) = match ports.split_once('-') {
        Some((lo, hi)) => (lo.parse().ok()?, hi.parse().ok()?),
        None => {
            let p = ports.parse().ok()?;
            (p, p)
        }
    };
    (lo > 0 && lo <= hi).then_some((proto, lo, hi))
}

/// The membership `setup` asks for, where it asks for one.
pub fn asked(setup: &[Vec<u8>]) -> Result<Option<Asked>, String> {
    let mut networks = Vec::new();
    for entry in setup.iter().filter_map(|e| e.strip_prefix(b"member=")) {
        let text = std::str::from_utf8(entry).map_err(|_| "a membership that is not UTF-8".to_string())?;
        let bad = || format!("a malformed membership: {text:?}");
        let mut parts = text.split('\0');
        let network = parts.next().unwrap_or_default().to_string();
        let addr = match parts.next().unwrap_or_default() {
            "" => None,
            a => Some(a.parse::<Ipv4Addr>().map_err(|_| bad())?),
        };
        let names: Vec<String> = parts.map(str::to_string).collect();
        if network.is_empty()
            || names.iter().any(String::is_empty)
            || networks.iter().any(|(n, _, _)| *n == network)
        {
            return Err(bad());
        }
        networks.push((network, addr, names));
    }
    if networks.is_empty() {
        return Ok(None);
    }
    let mut accepts = Vec::new();
    if let Some(entry) = setup.iter().find_map(|e| e.strip_prefix(b"member-accepts=")) {
        let text = std::str::from_utf8(entry).map_err(|_| "declared ports that are not UTF-8".to_string())?;
        for one in text.split(',').filter(|t| !t.is_empty()) {
            accepts.push(declared(one).ok_or_else(|| format!("a malformed declared port: {one:?}"))?);
        }
    }
    Ok(Some(Asked { networks, accepts }))
}

/// What `setup` publishes (`own-publish=PROTO\0HOSTIP\0HOSTPORT\0PORT`).
pub fn bindings(setup: &[Vec<u8>]) -> Result<Vec<Binding>, String> {
    setup
        .iter()
        .filter_map(|e| e.strip_prefix(b"own-publish="))
        .map(|e| {
            let text = std::str::from_utf8(e).map_err(|_| "a port entry that is not UTF-8".to_string())?;
            let bad = || format!("a malformed port entry: {text:?}");
            let mut parts = text.split('\0');
            let (Some(proto), Some(host), Some(host_port), Some(port), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                return Err(bad());
            };
            Ok(Binding {
                proto: match proto {
                    "tcp" => 6,
                    "udp" => 17,
                    _ => return Err(bad()),
                },
                host: if host.is_empty() {
                    None
                } else {
                    Some(host.parse().map_err(|_| bad())?)
                },
                host_port: host_port.parse().map_err(|_| bad())?,
                port: port.parse().map_err(|_| bad())?,
            })
        })
        .collect()
}

/// A member's place on one of its networks: the network, and its address, prefix and
/// gateway there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct On {
    pub network: String,
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
}

/// A member, joined: init's descriptor of the namespace made for it, which its joiner's
/// child enters and init holds while it is a member; its place on each of its networks,
/// the first its default route's; its link's hardware address, the kernel's; and whether
/// it asks the agents' resolver for names past the microVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joined {
    pub ns: RawFd,
    pub on: Vec<On>,
    pub mac: [u8; 6],
    pub dns: bool,
}

/// A network of the engine's: its name, its subnet and gateway, the range its members'
/// addresses come from where its client gave one, whether it reaches past the microVM
/// (none from an internal one), and whether its members are one another's peers (not on
/// the engine's default network, nor on one made without).
#[derive(Debug, Clone)]
struct Net {
    name: String,
    subnet: (Ipv4Addr, u8),
    gateway: Ipv4Addr,
    range: Option<(Ipv4Addr, u8)>,
    internal: bool,
    peers: bool,
}

/// A member, as init keeps it: its joiner's id, its link's index, its address and names on
/// each of its networks, the ports it declares, and its namespace.
#[derive(Debug)]
struct Kept {
    id: u32,
    index: usize,
    on: Vec<(String, Ipv4Addr, Vec<String>)>,
    accepts: Vec<Egress>,
    member: crate::links::Member,
    _ns: OwnedFd,
}

/// The engine's networks and members.
struct Engine {
    networks: Vec<Net>,
    members: Vec<Kept>,
}

static ENGINE: Mutex<Engine> = Mutex::new(Engine {
    networks: Vec::new(),
    members: Vec::new(),
});

fn engine() -> std::sync::MutexGuard<'static, Engine> {
    ENGINE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The first /24 of the switch's block (netplan's, the agents' own) that overlaps none of
/// `taken`.
fn free_subnet(taken: &[(Ipv4Addr, u8)]) -> Option<(Ipv4Addr, u8)> {
    let (base, bits) = crate::netplan::POOL;
    let count = 1u32.checked_shl(u32::from(crate::netplan::POOL_PREFIX - bits))?;
    let step = 1u32.checked_shl(u32::from(32 - crate::netplan::POOL_PREFIX))?;
    (0..count)
        .filter_map(|i| u32::from(base).checked_add(i.checked_mul(step)?))
        .map(|a| (Ipv4Addr::from(a), crate::netplan::POOL_PREFIX))
        .find(|s| !taken.iter().any(|t| crate::netplan::overlaps(*s, *t)))
}

/// The lowest address of `within` (a network's range, or its subnet) that is neither the
/// subnet's own nor its last, nor its gateway, nor one `held` holds; none past it.
fn free_address(
    subnet: (Ipv4Addr, u8),
    within: (Ipv4Addr, u8),
    gateway: Ipv4Addr,
    held: &[Ipv4Addr],
) -> Option<Ipv4Addr> {
    let mask = |p: u8| u32::MAX.checked_shl(32 - u32::from(p)).unwrap_or(0);
    let first = u32::from(subnet.0) & mask(subnet.1);
    let last = first | !mask(subnet.1);
    let from = u32::from(within.0) & mask(within.1);
    let to = from | !mask(within.1);
    (from.max(first.saturating_add(1))..=to.min(last.saturating_sub(1)))
        .map(Ipv4Addr::from)
        .find(|a| *a != gateway && !held.contains(a))
}

/// The switch, the domains' where they made one, else one made now with none of theirs.
fn switch() -> io::Result<&'static crate::links::Switch> {
    if let Some(s) = crate::domains::SWITCH.get() {
        return Ok(s);
    }
    let made = crate::links::Switch::new(&[], &[], &[], &[], None, false, 0, &[])?;
    Ok(crate::domains::SWITCH.get_or_init(|| made))
}

/// eth0's subnet: the microVM's own network, which no network of its engine's overlaps.
fn eth0_subnet() -> Option<(Ipv4Addr, u8)> {
    let (eth0, prefix, _) = crate::net::current()?;
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    Some((Ipv4Addr::from(u32::from(eth0) & mask), prefix))
}

/// The engine's default network, made as it is first joined.
fn default_network(e: &mut Engine, sw: &crate::links::Switch) -> Result<Net, String> {
    if let Some(n) = e.networks.iter().find(|n| n.name == DEFAULT) {
        return Ok(n.clone());
    }
    let mut taken = sw.subnets();
    taken.extend(eth0_subnet());
    taken.extend(e.networks.iter().map(|n| n.subnet));
    let subnet = free_subnet(&taken).ok_or_else(|| {
        format!(
            "network {DEFAULT}: no /24 of {}/{} is clear of the microVM's own networks",
            crate::netplan::POOL.0,
            crate::netplan::POOL.1
        )
    })?;
    let n = Net {
        name: DEFAULT.to_string(),
        subnet,
        gateway: Ipv4Addr::from(u32::from(subnet.0).saturating_add(1)),
        range: None,
        internal: false,
        peers: false,
    };
    e.networks.push(n.clone());
    Ok(n)
}

/// The peers a member may open flows to: each one's addresses, and the ports it declares.
type Opens = Vec<(Vec<Ipv4Addr>, Vec<Egress>)>;

/// The names a member resolves, a table a network: each peer's name there at its address.
type Tables = Vec<(String, Vec<(String, Ipv4Addr)>)>;

/// What each member's gate lets through, its pairs on the switch, and the names it asks
/// for, from every member's networks and declared ports: who shares a network with peers,
/// to whom, on the ports the receiver declares.
struct Grants {
    /// Each member's peers it may open flows to (their addresses and the ports each
    /// declares), and those that may open flows to it (their addresses).
    opens: Vec<(usize, Opens)>,
    accepts: Vec<(usize, Vec<Ipv4Addr>)>,
    pairs: Vec<(usize, usize, Vec<Egress>)>,
    /// The names each member resolves of the peers it may reach: each shared network's,
    /// each peer's names there at its address there.
    names: Vec<(usize, Tables)>,
}

fn grants(e: &Engine) -> Grants {
    let peered = |network: &str| e.networks.iter().any(|n| n.name == network && n.peers);
    let mut g = Grants {
        opens: Vec::new(),
        accepts: Vec::new(),
        pairs: Vec::new(),
        names: Vec::new(),
    };
    for me in &e.members {
        let mut opens: Opens = Vec::new();
        let mut accepts: Vec<Ipv4Addr> = Vec::new();
        let mut names: Tables = Vec::new();
        for peer in e.members.iter().filter(|p| p.id != me.id) {
            // Its addresses on the networks the two share that make peers of their members.
            let shared: Vec<(&String, Ipv4Addr, &Vec<String>)> = peer
                .on
                .iter()
                .filter(|(n, _, _)| peered(n) && me.on.iter().any(|(m, _, _)| m == n))
                .map(|(n, a, names)| (n, *a, names))
                .collect();
            if shared.is_empty() {
                continue;
            }
            let at: Vec<Ipv4Addr> = shared.iter().map(|(_, a, _)| *a).collect();
            if !peer.accepts.is_empty() {
                opens.push((at.clone(), peer.accepts.clone()));
                g.pairs.push((me.index, peer.index, peer.accepts.clone()));
                for (network, addr, peer_names) in &shared {
                    let entries = match names.iter_mut().find(|(n, _)| n == *network) {
                        Some((_, entries)) => entries,
                        None => {
                            names.push(((*network).clone(), Vec::new()));
                            let Some((_, entries)) = names.last_mut() else {
                                continue;
                            };
                            entries
                        }
                    };
                    for name in peer_names.iter() {
                        entries.push((name.clone(), *addr));
                    }
                }
            }
            if !me.accepts.is_empty() {
                accepts.extend(at);
            }
        }
        g.opens.push((me.index, opens));
        g.accepts.push((me.index, accepts));
        g.names.push((me.index, names));
    }
    g
}

/// The gate of member `m`, by `g`'s grants.
fn gate_of(m: &Kept, g: &Grants) -> crate::links::Gate {
    let opens = g
        .opens
        .iter()
        .find(|(i, _)| *i == m.index)
        .map(|(_, o)| o.clone())
        .unwrap_or_default();
    let accepts = g
        .accepts
        .iter()
        .find(|(i, _)| *i == m.index)
        .map(|(_, from)| from.clone())
        .unwrap_or_default();
    crate::links::Gate {
        own: m.on.iter().map(|(_, a, _)| *a).collect(),
        opens,
        // Its declared ports whether or not it has peers yet: as it is linked, its own
        // connections' ports are chosen clear of them (`local_ports`, D61), which the
        // gate leaves untracked once a peer joins.
        accepts: if m.accepts.is_empty() {
            Vec::new()
        } else {
            vec![(accepts, m.accepts.clone())]
        },
        egress: match m.member.reach {
            Reach::Nowhere => Vec::new(),
            _ => crate::links::EVERY_PORT.to_vec(),
        },
        ingress: m.member.published.clone(),
        dns: asks(m, g),
        own6: Vec::new(),
        opens6: Vec::new(),
        accepts6: Vec::new(),
        egress6: false,
    }
}

/// Whether member `m` asks the agents' resolver: for names past the microVM, where it
/// reaches past it, or for its peers' names, where it has peers it may reach.
fn asks(m: &Kept, g: &Grants) -> bool {
    m.member.reach != Reach::Nowhere || g.names.iter().any(|(i, n)| *i == m.index && !n.is_empty())
}

/// Member `m`'s asker at the agents' resolver, by `g`'s grants: its link, whether it may
/// ask past the microVM, its addresses, and the names it resolves there.
fn asker_of(m: &Kept, g: &Grants) -> io::Result<crate::agentdns::Asker> {
    let local = g
        .names
        .iter()
        .find(|(i, _)| *i == m.index)
        .map(|(_, nets)| {
            nets.iter()
                .map(|(network, entries)| {
                    let mut names = shards_dns::Names::new(network);
                    for (name, addr) in entries {
                        names.add(name, IpAddr::V4(*addr));
                    }
                    names
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(crate::agentdns::Asker {
        link: crate::links::link_index(m.index)?,
        any: m.member.reach != Reach::Nowhere,
        names: Vec::new(),
        addrs: m.on.iter().map(|(_, a, _)| *a).collect(),
        local,
    })
}

/// Every member's grants programmed again, but `except`'s, whose gate was made with its
/// own: the switch's pairs, each other member's gate and its asker.
fn regrant(e: &Engine, sw: &crate::links::Switch, except: Option<usize>) -> io::Result<()> {
    let g = grants(e);
    let carried = e
        .members
        .iter()
        .map(|m| crate::links::Member {
            dns: asks(m, &g),
            ..m.member.clone()
        })
        .collect();
    sw.carry(carried, g.pairs.clone())?;
    for m in &e.members {
        if Some(m.index) != except {
            sw.regate(m.index, &gate_of(m, &g))?;
        }
        let asker = asker_of(m, &g)?;
        if asker.any || !asker.local.is_empty() {
            ask_through(sw, &asker)?;
        } else if crate::agentdns::running() {
            crate::agentdns::release(asker.link)?;
        }
    }
    Ok(())
}

/// Has the agents' resolver keep `asker`, starting it where it does not run.
fn ask_through(sw: &crate::links::Switch, asker: &crate::agentdns::Asker) -> io::Result<()> {
    if crate::agentdns::running() {
        return crate::agentdns::admit(asker);
    }
    let (_, _, upstream) =
        crate::net::current().ok_or_else(|| io::Error::other("the microVM has no resolver"))?;
    let sockets = sw.resolver_sockets(SocketAddr::from((upstream, 53)))?;
    crate::agentdns::start(sockets, vec![asker.clone()]).map_err(io::Error::other)
}

/// Makes joiner `id` a member of the networks of its engine's `asked` names, publishing
/// `published` to the run's command: its namespace, its address on each, its gate and
/// link, and its grants and every peer's, each before any process of it runs.
pub fn join(id: u32, asked: &Asked, published: &[Egress]) -> Result<Joined, String> {
    let failed = |what: &str, e: io::Error| format!("{what}: {e}");
    let sw = switch().map_err(|e| failed("the microVM's switch", e))?;
    // Past the microVM by the switch's link up, and reached by the command down it.
    sw.uplink_once()
        .map_err(|e| failed("the switch's link past the microVM", e))?;
    let mut e = engine();
    let mut nets = Vec::new();
    for (network, _, _) in &asked.networks {
        let net = match e.networks.iter().find(|n| n.name == *network) {
            Some(n) => n.clone(),
            None if network == DEFAULT => default_network(&mut e, sw)?,
            None => {
                return Err(format!(
                    "network {network}: no network of its engine's is named so"
                ));
            }
        };
        nets.push(net);
    }
    let mut on: Vec<(String, Ipv4Addr, Vec<String>)> = Vec::new();
    for (net, (_, given, names)) in nets.iter().zip(&asked.networks) {
        let held: Vec<Ipv4Addr> = e
            .members
            .iter()
            .flat_map(|m| m.on.iter())
            .filter(|(n, _, _)| *n == net.name)
            .map(|(_, a, _)| *a)
            .collect();
        let addr = match given {
            // The one its client asked for: of its subnet, past the subnet's own and last,
            // not its gateway, and held by no other member.
            Some(a) => {
                let within = (*a, 32);
                if free_address(net.subnet, within, net.gateway, &held) != Some(*a) {
                    return Err(format!(
                        "address {a} on network {}: not one of {}/{} that is free",
                        net.name, net.subnet.0, net.subnet.1
                    ));
                }
                *a
            }
            None => free_address(net.subnet, net.range.unwrap_or(net.subnet), net.gateway, &held)
                .ok_or_else(|| {
                    format!(
                        "network {}: every address of {}/{} is held",
                        net.name, net.subnet.0, net.subnet.1
                    )
                })?,
        };
        on.push((net.name.clone(), addr, names.clone()));
    }
    let index = (sw.domains()..)
        .find(|i| !e.members.iter().any(|m| m.index == *i))
        .ok_or_else(|| "no link left on the microVM's switch".to_string())?;
    // A namespace of its own, its loopback up, which nothing but this descriptor holds
    // until the joiner's child enters it.
    let ns = in_netns(None, || {
        let ns = open_ns("/proc/thread-self/ns/net")?;
        crate::run::loopback_up()?;
        Ok(ns)
    })
    .map_err(|e| failed("its network namespace", e))?;
    // Past the microVM where one of its networks is not internal, as far as the command.
    let reach = if nets.iter().all(|n| n.internal) {
        Reach::Nowhere
    } else if CONFINED.load(std::sync::atomic::Ordering::Relaxed) {
        eth0_subnet().map_or(Reach::Nowhere, Reach::Within)
    } else {
        Reach::AsTheCommand
    };
    let dns = reach != Reach::Nowhere;
    let member = crate::links::Member {
        index,
        addrs: on.iter().map(|(_, a, _)| *a).collect(),
        reach,
        published: published.to_vec(),
        dns,
    };
    let kept = Kept {
        id,
        index,
        on: on.clone(),
        accepts: asked.accepts.clone(),
        member: member.clone(),
        _ns: ns,
    };
    let link = crate::netplan::Link {
        addresses: nets
            .iter()
            .zip(&on)
            .map(|(net, (_, addr, _))| crate::netplan::Address {
                network: net.name.clone(),
                addr: *addr,
                subnet: net.subnet.0,
                prefix: net.subnet.1,
                gateway: net.gateway,
            })
            .collect(),
        addresses6: Vec::new(),
        hosts: Vec::new(),
        egress: match reach {
            Reach::Nowhere => Vec::new(),
            _ => crate::links::EVERY_PORT.to_vec(),
        },
        ingress: Vec::new(),
        dns,
        mcp: Vec::new(),
        connects: false,
        accepts: false,
        unix: Vec::new(),
    };
    e.members.push(kept);
    let joined = (|| {
        let Some(me) = e.members.last() else {
            return Err(io::Error::other("its membership, just kept, is gone"));
        };
        let g = grants(&e);
        sw.attach_ns(index, &me._ns, &link, &gate_of(me, &g))?;
        let subnets: Vec<(Ipv4Addr, u8)> = nets.iter().map(|n| n.subnet).collect();
        sw.routes(&subnets)?;
        regrant(&e, sw, Some(index))?;
        hardware_address(&me._ns)
    })();
    match joined {
        Ok(mac) => {
            let raw = e.members.last().map_or(-1, |m| m._ns.as_raw_fd());
            Ok(Joined {
                ns: raw,
                on: nets
                    .iter()
                    .zip(&on)
                    .map(|(net, (_, addr, _))| On {
                        network: net.name.clone(),
                        addr: *addr,
                        prefix: net.subnet.1,
                        gateway: net.gateway,
                    })
                    .collect(),
                mac,
                dns,
            })
        }
        Err(why) => {
            e.members.retain(|m| m.id != id);
            let _ = regrant(&e, sw, None);
            sw.drop_gate(index);
            if let Ok(link) = crate::links::link_index(index) {
                let _ = crate::agentdns::release(link);
            }
            Err(failed("its grants on the microVM's switch", why))
        }
    }
}

/// Lets joiner `id` go, if it is a member: its grants and every peer's of it, then its
/// gate and namespace.
pub fn leave(id: u32) {
    let mut e = engine();
    let Some(at) = e.members.iter().position(|m| m.id == id) else {
        return;
    };
    let gone = e.members.remove(at);
    if let Some(sw) = crate::domains::SWITCH.get() {
        if let Ok(link) = crate::links::link_index(gone.index) {
            let _ = crate::agentdns::release(link);
        }
        let _ = regrant(&e, sw, None);
        sw.drop_gate(gone.index);
    }
}

/// What process `pid`'s network namespace listens on (D181): each TCP port in LISTEN and
/// each UDP port bound unconnected (Linux's `/proc/net/tcp`, `tcp6`, `udp` and `udp6`:
/// state 0A and 07), at an address past its loopback, which nothing but itself reaches; as
/// `tcp PORT` and `udp PORT` lines, each once, in order.
pub fn listening(pid: libc::pid_t) -> Result<String, String> {
    let mut found: Vec<(&str, u16)> = Vec::new();
    for (proto, file, state) in [
        ("tcp", "tcp", "0A"),
        ("tcp", "tcp6", "0A"),
        ("udp", "udp", "07"),
        ("udp", "udp6", "07"),
    ] {
        let path = format!("/proc/{pid}/net/{file}");
        let table = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // A kernel without IPv6 has no tcp6 or udp6.
            Err(e) if e.kind() == io::ErrorKind::NotFound && file.ends_with('6') => continue,
            Err(e) => return Err(format!("{path}: {e}")),
        };
        for port in listens_in(&table, state) {
            if !found.contains(&(proto, port)) {
                found.push((proto, port));
            }
        }
    }
    found.sort_unstable();
    Ok(found
        .iter()
        .map(|(proto, port)| format!("{proto} {port}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The ports of socket table `table`'s sockets in `state`, at an address past the
/// loopback: its header line first, then a socket a line, `sl local_address rem_address
/// st ...`, each address `HEX:PORT`.
fn listens_in(table: &str, state: &str) -> Vec<u16> {
    let mut ports = Vec::new();
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(local), Some(st)) = (fields.nth(1), fields.nth(1)) else {
            continue;
        };
        let Some((addr, port)) = local.split_once(':') else {
            continue;
        };
        if st == state
            && let Ok(port) = u16::from_str_radix(port, 16)
            && !loopback(addr)
        {
            ports.push(port);
        }
    }
    ports
}

/// Whether a socket table's address (`/proc/net/tcp`'s hex, each 32-bit word in the
/// host's order) is a loopback one: 127.0.0.0/8, ::1, or 127.0.0.0/8 mapped into IPv6.
fn loopback(addr: &str) -> bool {
    let words: Vec<u32> = addr
        .as_bytes()
        .chunks(8)
        .filter_map(|w| {
            std::str::from_utf8(w)
                .ok()
                .and_then(|w| u32::from_str_radix(w, 16).ok())
        })
        .collect();
    // Each word as the kernel wrote it, the host's order: its bytes in memory order.
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
    match bytes.as_slice() {
        [127, ..] if bytes.len() == 4 => true,
        [rest @ .., 0, 0, 0, 1] if bytes.len() == 16 => rest.iter().all(|b| *b == 0),
        [z @ .., 0xff, 0xff, 127, _, _, _] if bytes.len() == 16 => z.iter().all(|b| *b == 0),
        _ => false,
    }
}

/// What the engine's `args` ask of its networks, done in init (D181): `make NAME SUBNET
/// GATEWAY RANGE FLAGS`, each of the three addresses empty for one init picks, FLAGS a
/// comma list of `internal` and `no-peers`, answered with the subnet and gateway made
/// (`10.244.3.0/24 10.244.3.1`); `default`, the engine's default network, made where it is
/// not yet, answered so; or `remove NAME`, of a network no member is on. Why not,
/// otherwise.
pub fn network(args: &[Vec<u8>]) -> Result<String, String> {
    let text: Vec<String> = args
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let arg = |i: usize| text.get(i).map(String::as_str).unwrap_or_default();
    let cidr = |s: &str| -> Result<Option<(Ipv4Addr, u8)>, String> {
        if s.is_empty() {
            return Ok(None);
        }
        let (a, p) = s.split_once('/').ok_or_else(|| format!("{s}: no IPv4 subnet"))?;
        let addr: Ipv4Addr = a.parse().map_err(|_| format!("{s}: no IPv4 subnet"))?;
        let prefix: u8 = p
            .parse()
            .ok()
            .filter(|p| (8..=30).contains(p))
            .ok_or_else(|| format!("{s}: a network of a microVM's engine is a /8 to a /30"))?;
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        Ok(Some((Ipv4Addr::from(u32::from(addr) & mask), prefix)))
    };
    let mut e = engine();
    match arg(0) {
        "default" => {
            let sw = switch().map_err(|e| format!("the microVM's switch: {e}"))?;
            let n = default_network(&mut e, sw)?;
            Ok(format!("{}/{} {}", n.subnet.0, n.subnet.1, n.gateway))
        }
        "make" => {
            let name = arg(1);
            if name.is_empty() || name == DEFAULT {
                return Err(format!("{name:?}: no name for a network of its own"));
            }
            if e.networks.iter().any(|n| n.name == name) {
                return Err(format!("network {name} is made already"));
            }
            let sw = switch().map_err(|e| format!("the microVM's switch: {e}"))?;
            let mut taken: Vec<(Ipv4Addr, u8, String)> = sw
                .subnets()
                .into_iter()
                .map(|(a, p)| (a, p, "a network of the microVM's".to_string()))
                .collect();
            if let Some((a, p)) = eth0_subnet() {
                taken.push((a, p, "the microVM's own network".to_string()));
            }
            for n in &e.networks {
                taken.push((n.subnet.0, n.subnet.1, format!("network {}", n.name)));
            }
            let subnet = match cidr(arg(2))? {
                Some(s) => {
                    if let Some((_, _, whose)) = taken
                        .iter()
                        .find(|(a, p, _)| crate::netplan::overlaps(s, (*a, *p)))
                    {
                        return Err(format!("subnet {}/{}: {whose} holds part of it", s.0, s.1));
                    }
                    s
                }
                None => {
                    let held: Vec<(Ipv4Addr, u8)> = taken.iter().map(|(a, p, _)| (*a, *p)).collect();
                    free_subnet(&held).ok_or_else(|| {
                        format!(
                            "no /24 of {}/{} is clear of the microVM's own networks",
                            crate::netplan::POOL.0,
                            crate::netplan::POOL.1
                        )
                    })?
                }
            };
            let inside = |a: Ipv4Addr| crate::netplan::overlaps((a, 32), subnet);
            let gateway = match arg(3) {
                "" => Ipv4Addr::from(u32::from(subnet.0).saturating_add(1)),
                g => {
                    let g: Ipv4Addr = g.parse().map_err(|_| format!("gateway {g}: no IPv4 address"))?;
                    if !inside(g) {
                        return Err(format!(
                            "gateway {g}: outside its subnet {}/{}",
                            subnet.0, subnet.1
                        ));
                    }
                    g
                }
            };
            let range = match cidr(arg(4))? {
                Some(r) if !crate::netplan::overlaps(r, subnet) || r.1 < subnet.1 => {
                    return Err(format!(
                        "range {}/{}: outside its subnet {}/{}",
                        r.0, r.1, subnet.0, subnet.1
                    ));
                }
                r => r,
            };
            let flags: Vec<&str> = arg(5).split(',').filter(|f| !f.is_empty()).collect();
            if let Some(f) = flags.iter().find(|f| !matches!(**f, "internal" | "no-peers")) {
                return Err(format!("{f}: no setting of a network of a microVM's engine"));
            }
            e.networks.push(Net {
                name: name.to_string(),
                subnet,
                gateway,
                range,
                internal: flags.contains(&"internal"),
                peers: !flags.contains(&"no-peers"),
            });
            Ok(format!("{}/{} {gateway}", subnet.0, subnet.1))
        }
        "remove" => {
            let name = arg(1);
            let on: Vec<u32> = e
                .members
                .iter()
                .filter(|m| m.on.iter().any(|(n, _, _)| n == name))
                .map(|m| m.id)
                .collect();
            if !on.is_empty() {
                return Err(format!(
                    "network {name}: {} of its members still run on it",
                    on.len()
                ));
            }
            let Some(at) = e.networks.iter().position(|n| n.name == name) else {
                return Err(format!("network {name}: no network of its engine's is named so"));
            };
            let gone = e.networks.remove(at);
            if let Some(sw) = crate::domains::SWITCH.get() {
                sw.forget(gone.subnet)
                    .map_err(|e| format!("network {name}: {e}"))?;
            }
            Ok(String::new())
        }
        other => Err(format!("{other:?}: nothing the engine's networks are asked")),
    }
}

/// `eth0`'s hardware address in namespace `ns`, as its kernel gave it (SIOCGIFHWADDR).
fn hardware_address(ns: &OwnedFd) -> io::Result<[u8; 6]> {
    in_netns(Some(ns.as_raw_fd()), || {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an all-zero ifreq is a valid value.
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        for (to, from) in req.ifr_name.iter_mut().zip(b"eth0".iter()) {
            *to = *from as libc::c_char;
        }
        // SAFETY: ioctl(2) of an ifreq of ours on our own socket.
        if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFHWADDR as libc::Ioctl, &raw mut req) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel filled the union's hardware address.
        let data = unsafe { req.ifr_ifru.ifru_hwaddr.sa_data };
        let mut mac = [0u8; 6];
        // `c_char` is `u8` on some architectures and `i8` on others: its byte either way.
        for (to, from) in mac.iter_mut().zip(data.iter()) {
            *to = u8::from_ne_bytes(from.to_ne_bytes());
        }
        Ok(mac)
    })
}

/// In a joiner's child, single-threaded: a network namespace of its own with its loopback
/// alone (`--network none`).
pub fn alone() -> Result<(), String> {
    // SAFETY: unshare(2) of this single-threaded child's network namespace.
    if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
        return Err(format!(
            "a network namespace of its own: {}",
            io::Error::last_os_error()
        ));
    }
    crate::run::loopback_up().map_err(|e| format!("its loopback: {e}"))
}

/// How long the proxy keeps a UDP client's mapping idle: as long as the microVM's own
/// connection tracking keeps a UDP flow (`nf_conntrack_udp_timeout` until an answer comes,
/// `nf_conntrack_udp_timeout_stream` after), read from the running kernel; where it has no
/// connection tracking, its defaults (net/netfilter/nf_conntrack_proto_udp.c: 30 s, 120 s).
fn udp_idle() -> (Duration, Duration) {
    let read = |key: &str, default: u64| {
        crate::setup::read_sysctl(key)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(default), Duration::from_secs)
    };
    (
        read("net.netfilter.nf_conntrack_udp_timeout", 30),
        read("net.netfilter.nf_conntrack_udp_timeout_stream", 120),
    )
}

/// Publishes `bindings` from the namespace of `workload`'s process to `target`, a
/// member's address, in a process of init's own: its PID and the port each binding was
/// given there, in their order; or why one could not be bound.
pub fn publish(
    workload: libc::pid_t,
    target: Ipv4Addr,
    bindings: &[Binding],
) -> Result<(libc::pid_t, Vec<u16>), String> {
    let theirs =
        open_ns(&format!("/proc/{workload}/ns/net")).map_err(|e| format!("the command's network: {e}"))?;
    let idle = udp_idle();
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2(2) fills two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("a pipe: {}", io::Error::last_os_error()));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) };
    // SAFETY: fork(2) from init, which has no other thread: the child runs only this
    // module's code, on descriptors of its own.
    match unsafe { libc::fork() } {
        -1 => Err(format!(
            "forking the ports' proxy: {}",
            io::Error::last_os_error()
        )),
        0 => {
            crate::dnsrelay::keep_only(&[theirs.as_raw_fd(), write.as_raw_fd()]);
            drop(read);
            // SAFETY: setns(2) of this single-threaded child into the command's namespace.
            let entered = unsafe { libc::setns(theirs.as_raw_fd(), libc::CLONE_NEWNET) } == 0;
            drop(theirs);
            let mut told = std::fs::File::from(write);
            if !entered {
                let _ = told.write_all(format!("E{}", io::Error::last_os_error()).as_bytes());
                // SAFETY: _exit(2) of the child, which owns nothing to flush.
                unsafe { libc::_exit(1) }
            }
            serve(bindings, target, idle, told);
            // SAFETY: as above.
            unsafe { libc::_exit(0) }
        }
        pid => {
            drop(write);
            let mut said = Vec::new();
            let _ = std::fs::File::from(read).read_to_end(&mut said);
            match said.split_first().map(|(k, rest)| (k, rest.as_chunks::<2>())) {
                Some((b'P', (ports, []))) if ports.len() == bindings.len() => {
                    Ok((pid, ports.iter().map(|p| u16::from_be_bytes(*p)).collect()))
                }
                _ => {
                    // SAFETY: kill(2) and waitpid(2) of init's own child.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        libc::waitpid(pid, std::ptr::null_mut(), 0);
                    }
                    Err(String::from_utf8_lossy(said.get(1..).unwrap_or_default()).into_owned())
                }
            }
        }
    }
}

/// The proxy's side: each binding bound, the ports said on `told` (`P` and each a big-endian
/// u16) or why not (`E` and the reason), then each relayed until the process is killed.
fn serve(bindings: &[Binding], target: Ipv4Addr, idle: (Duration, Duration), mut told: std::fs::File) {
    enum Bound {
        Tcp(TcpListener, u16),
        Udp(UdpSocket, u16),
    }
    let mut bound = Vec::with_capacity(bindings.len());
    let mut ports = vec![b'P'];
    for b in bindings {
        let at = SocketAddr::new(b.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), b.host_port);
        let made = match b.proto {
            6 => TcpListener::bind(at).and_then(|l| Ok((l.local_addr()?.port(), Bound::Tcp(l, b.port)))),
            _ => UdpSocket::bind(at).and_then(|s| Ok((s.local_addr()?.port(), Bound::Udp(s, b.port)))),
        };
        match made {
            Ok((port, it)) => {
                ports.extend_from_slice(&port.to_be_bytes());
                bound.push(it);
            }
            Err(e) => {
                let proto = if b.proto == 6 { "tcp" } else { "udp" };
                let why = if e.kind() == io::ErrorKind::AddrInUse {
                    format!(
                        "{at}/{proto} is taken in the command's network: another of its programs, or another container its engine made, listens there"
                    )
                } else {
                    format!("{at}/{proto} in the command's network: {e}")
                };
                let _ = told.write_all(format!("E{why}").as_bytes());
                return;
            }
        }
    }
    if told.write_all(&ports).is_err() {
        return;
    }
    drop(told);
    std::thread::scope(|s| {
        for it in &bound {
            let _ = std::thread::Builder::new()
                .name("ports".into())
                .spawn_scoped(s, move || match it {
                    Bound::Tcp(listener, port) => {
                        relay_tcp(listener, SocketAddr::new(IpAddr::V4(target), *port))
                    }
                    Bound::Udp(sock, port) => {
                        relay_udp(sock, SocketAddr::new(IpAddr::V4(target), *port), idle)
                    }
                });
        }
    });
}

/// Each connection to `listener` passed on to `to`, both ways, until either side ends.
fn relay_tcp(listener: &TcpListener, to: SocketAddr) {
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let _ = std::thread::Builder::new().name("port".into()).spawn(move || {
            let Ok(server) = TcpStream::connect(to) else {
                return;
            };
            let (Ok(mut c_in), Ok(mut s_out)) = (client.try_clone(), server.try_clone()) else {
                return;
            };
            let up = std::thread::Builder::new().name("port up".into()).spawn(move || {
                let _ = io::copy(&mut c_in, &mut s_out);
                let _ = s_out.shutdown(std::net::Shutdown::Write);
            });
            let (mut s_in, mut c_out) = (server, client);
            if up.is_ok() {
                let _ = io::copy(&mut s_in, &mut c_out);
            }
            let _ = c_out.shutdown(std::net::Shutdown::Both);
            if let Ok(up) = up {
                let _ = up.join();
            }
        });
    }
}

/// A UDP client's mapping: its socket to the member, when either side last sent, and
/// whether the member has answered.
struct Mapping {
    out: UdpSocket,
    last: Instant,
    answered: bool,
}

/// Each client's datagrams to `sock` passed on to `to` from a socket of its own, and the
/// answers back to it, until the mapping has been idle as long as `idle` says: its first
/// before an answer, its second after.
fn relay_udp(sock: &UdpSocket, to: SocketAddr, idle: (Duration, Duration)) {
    let clients: Mutex<std::collections::HashMap<SocketAddr, Mapping>> =
        Mutex::new(std::collections::HashMap::new());
    let held = || clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut buf = vec![0u8; 65536];
    std::thread::scope(|s| {
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let datagram = buf.get(..n).unwrap_or_default();
            let mut known = held();
            if let Some(m) = known.get_mut(&from) {
                m.last = Instant::now();
                let _ = m.out.send(datagram);
                continue;
            }
            let Ok(out) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|o| o.connect(to).map(|()| o))
            else {
                continue;
            };
            let _ = out.send(datagram);
            let Ok(back) = out.try_clone() else { continue };
            known.insert(
                from,
                Mapping {
                    out,
                    last: Instant::now(),
                    answered: false,
                },
            );
            drop(known);
            let held = &held;
            let _ = std::thread::Builder::new()
                .name("port".into())
                .spawn_scoped(s, move || {
                    let mut answer = vec![0u8; 65536];
                    loop {
                        // Woken as the mapping could next go idle.
                        let wait = {
                            let known = held();
                            let Some(m) = known.get(&from) else { return };
                            let limit = if m.answered { idle.1 } else { idle.0 };
                            limit.saturating_sub(m.last.elapsed())
                        };
                        if wait.is_zero() {
                            break;
                        }
                        let _ = back.set_read_timeout(Some(wait));
                        match back.recv(&mut answer) {
                            Ok(n) => {
                                if let Some(m) = held().get_mut(&from) {
                                    m.last = Instant::now();
                                    m.answered = true;
                                }
                                let _ = sock.send_to(answer.get(..n).unwrap_or_default(), from);
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    held().remove(&from);
                });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket table's listeners past the loopback: the any address's and an address's
    /// of its own, not 127.0.0.1's, ::1's or 127.0.0.1's mapped, nor a connected socket.
    #[test]
    fn what_listens_past_the_loopback_is_found() {
        let v4 =
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1B58 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11 1 0 100 0 0 10 0
   1: 0100007F:1B59 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12 1 0 100 0 0 10 0
   2: 0200F40A:1B5A 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 13 1 0 100 0 0 10 0
   3: 0200F40A:1B5B 0300F40A:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 14 1 0 100 0 0 10 0
";
        assert_eq!(listens_in(v4, "0A"), [7000, 7002]);
        let v6 = "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:1F40 00000000000000000000000000000000:0000 0A
   1: 00000000000000000000000001000000:1F41 00000000000000000000000000000000:0000 0A
   2: 0000000000000000FFFF00000100007F:1F42 00000000000000000000000000000000:0000 0A
";
        assert_eq!(listens_in(v6, "0A"), [8000]);
    }

    fn entry(parts: &[&str]) -> Vec<u8> {
        parts.join("\0").into_bytes()
    }

    #[test]
    fn a_members_networks_ports_and_publishes_are_read_as_the_daemon_writes_them() {
        let setup = vec![
            entry(&["member=bridge", "", "db"]),
            entry(&["member=app", "10.244.1.9", "db", "postgres"]),
            b"member-accepts=6/5432,17/5000-5010".to_vec(),
            entry(&["own-publish=tcp", "", "0", "80"]),
            entry(&["own-publish=udp", "127.0.0.1", "53", "53"]),
        ];
        assert_eq!(
            asked(&setup).unwrap(),
            Some(Asked {
                networks: vec![
                    ("bridge".into(), None, vec!["db".into()]),
                    (
                        "app".into(),
                        Some(Ipv4Addr::new(10, 244, 1, 9)),
                        vec!["db".into(), "postgres".into()]
                    ),
                ],
                accepts: vec![(6, 5432, 5432), (17, 5000, 5010)],
            })
        );
        assert_eq!(
            bindings(&setup).unwrap(),
            [
                Binding {
                    proto: 6,
                    host: None,
                    host_port: 0,
                    port: 80
                },
                Binding {
                    proto: 17,
                    host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    host_port: 53,
                    port: 53
                },
            ]
        );
        assert_eq!(
            asked(&[entry(&["member=bridge"])]).unwrap().unwrap().networks,
            [("bridge".into(), None, vec![])]
        );
        assert!(asked(&[entry(&["member=", "", "db"])]).is_err());
        assert!(asked(&[entry(&["member=bridge", "", ""])]).is_err());
        assert!(asked(&[entry(&["member=bridge", "10.244.1", "db"])]).is_err());
        assert!(asked(&[entry(&["member=a"]), entry(&["member=a"])]).is_err());
        assert!(asked(&[entry(&["member=a"]), b"member-accepts=1/80".to_vec()]).is_err());
        assert!(asked(&[entry(&["member=a"]), b"member-accepts=6/90-80".to_vec()]).is_err());
        assert_eq!(asked(&[]).unwrap(), None);
    }

    /// A network's subnet is the first /24 of the switch's block clear of every one taken,
    /// its members' addresses the lowest free in its range past its gateway.
    #[test]
    fn subnets_and_addresses_are_the_lowest_free() {
        let first = (Ipv4Addr::new(10, 244, 0, 0), 24);
        assert_eq!(free_subnet(&[]), Some(first));
        assert_eq!(
            free_subnet(&[first, (Ipv4Addr::new(10, 244, 1, 0), 24)]),
            Some((Ipv4Addr::new(10, 244, 2, 0), 24))
        );
        // A wider one taken takes every /24 it holds.
        assert_eq!(
            free_subnet(&[(Ipv4Addr::new(10, 244, 0, 0), 23)]),
            Some((Ipv4Addr::new(10, 244, 2, 0), 24))
        );
        assert_eq!(free_subnet(&[(Ipv4Addr::new(10, 0, 0, 0), 8)]), None);
        let gw = Ipv4Addr::new(10, 244, 0, 1);
        assert_eq!(
            free_address(first, first, gw, &[]),
            Some(Ipv4Addr::new(10, 244, 0, 2))
        );
        assert_eq!(
            free_address(
                first,
                first,
                gw,
                &[Ipv4Addr::new(10, 244, 0, 2), Ipv4Addr::new(10, 244, 0, 4)]
            ),
            Some(Ipv4Addr::new(10, 244, 0, 3))
        );
        // A gateway given elsewhere frees .1; a range keeps to itself.
        assert_eq!(
            free_address(first, first, Ipv4Addr::new(10, 244, 0, 254), &[]),
            Some(Ipv4Addr::new(10, 244, 0, 1))
        );
        let range = (Ipv4Addr::new(10, 244, 0, 128), 25);
        assert_eq!(
            free_address(first, range, gw, &[]),
            Some(Ipv4Addr::new(10, 244, 0, 128))
        );
        let tiny = (Ipv4Addr::new(10, 244, 9, 0), 30);
        assert_eq!(
            free_address(
                tiny,
                tiny,
                Ipv4Addr::new(10, 244, 9, 1),
                &[Ipv4Addr::new(10, 244, 9, 2)]
            ),
            None
        );
    }

    /// Members of a network with peers grant one another the ports each declares, and
    /// names; members of the default network, or of none in common, grant nothing.
    #[test]
    fn grants_are_what_each_member_declares_to_its_peers() {
        let ns = || std::fs::File::open("/dev/null").unwrap().into();
        let kept = |id: u32, index: usize, on: &[(&str, [u8; 4], &[&str])], accepts: &[Egress]| Kept {
            id,
            index,
            on: on
                .iter()
                .map(|(n, a, names)| {
                    (
                        (*n).to_string(),
                        Ipv4Addr::from(*a),
                        names.iter().map(|s| (*s).to_string()).collect(),
                    )
                })
                .collect(),
            accepts: accepts.to_vec(),
            member: crate::links::Member {
                index,
                addrs: Vec::new(),
                reach: Reach::AsTheCommand,
                published: Vec::new(),
                dns: true,
            },
            _ns: ns(),
        };
        let net = |name: &str, peers: bool| Net {
            name: name.into(),
            subnet: (Ipv4Addr::new(10, 244, 1, 0), 24),
            gateway: Ipv4Addr::new(10, 244, 1, 1),
            range: None,
            internal: false,
            peers,
        };
        let e = Engine {
            networks: vec![net("bridge", false), net("app", true), net("other", true)],
            members: vec![
                kept(1, 10, &[("app", [10, 244, 1, 2], &["web"])], &[(6, 80, 80)]),
                kept(
                    2,
                    11,
                    &[("app", [10, 244, 1, 3], &["db", "postgres"])],
                    &[(6, 5432, 5432)],
                ),
                kept(3, 12, &[("other", [10, 244, 2, 2], &["far"])], &[(6, 9, 9)]),
                kept(4, 13, &[("bridge", [10, 244, 0, 2], &["alone"])], &[(6, 7, 7)]),
                kept(5, 14, &[("app", [10, 244, 1, 4], &["quiet"])], &[]),
            ],
        };
        let g = grants(&e);
        let mut pairs = g.pairs.clone();
        pairs.sort();
        // web and db, each to the other's port; quiet reaches both and is reached by none.
        assert_eq!(
            pairs,
            [
                (10, 11, vec![(6, 5432, 5432)]),
                (11, 10, vec![(6, 80, 80)]),
                (14, 10, vec![(6, 80, 80)]),
                (14, 11, vec![(6, 5432, 5432)]),
            ]
        );
        let names_of = |index: usize| -> Vec<(String, Ipv4Addr)> {
            let mut all: Vec<(String, Ipv4Addr)> = g
                .names
                .iter()
                .filter(|(i, _)| *i == index)
                .flat_map(|(_, nets)| nets.iter().flat_map(|(_, entries)| entries.iter().cloned()))
                .collect();
            all.sort();
            all
        };
        assert_eq!(
            names_of(10),
            [
                ("db".into(), Ipv4Addr::new(10, 244, 1, 3)),
                ("postgres".into(), Ipv4Addr::new(10, 244, 1, 3))
            ]
        );
        assert_eq!(names_of(14).len(), 3);
        // Nothing for the member alone on its network, nor for the default network's.
        assert!(names_of(12).is_empty() && names_of(13).is_empty());
        let gate = gate_of(e.members.get(1).unwrap(), &g);
        assert_eq!(
            gate.opens,
            [(vec![Ipv4Addr::new(10, 244, 1, 2)], vec![(6, 80, 80)])]
        );
        let mut from = gate.accepts.first().unwrap().0.clone();
        from.sort();
        assert_eq!(from, [Ipv4Addr::new(10, 244, 1, 2), Ipv4Addr::new(10, 244, 1, 4)]);
        assert_eq!(gate.accepts.first().unwrap().1, [(6, 5432, 5432)]);
        assert!(gate_of(e.members.get(3).unwrap(), &g).opens.is_empty());
        // Alone on its network, far's gate still holds its declared port, from no one.
        assert_eq!(
            gate_of(e.members.get(2).unwrap(), &g).accepts,
            [(Vec::new(), vec![(6, 9, 9)])]
        );
    }
}
