//! Networks shared by runs (D285): a network process switches several beside its guest's own
//! (D46). On each the guest has an address of its own, eth0's second, and its run a link to
//! each other run's network process, as D46 links two members. Where D46's members reach one
//! another whole, a frame on a shared network crosses only as a flow both of its ends grant,
//! each end's network process holding its own members' grants, by member, peer, ports and way:
//! the sender's a grant out to its peer's port, the receiver's a grant in from that peer to its
//! port. A reply crosses by its flow's state, never by its port.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use crate::{Ports, Proto, dns, wire};

/// A network's ID as the home's leases name it (D285 slice 1): `2 × MICROVM_ID_BYTES`
/// lowercase hex digits, so that a run host names a network to its network process as the
/// home names it.
pub const ID_LEN: usize = 2 * shards_abi::MICROVM_ID_BYTES;

/// A shared network's ID ([`ID_LEN`] lowercase hex digits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkId(pub [u8; ID_LEN]);

impl NetworkId {
    /// The ID `bytes` are, where they are one.
    pub fn of(bytes: &[u8]) -> Option<NetworkId> {
        let id = <[u8; ID_LEN]>::try_from(bytes).ok()?;
        id.iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .then_some(NetworkId(id))
    }
}

/// A run's place on a shared network, and who is on it, whole (`NET_JOIN`): taken in place of
/// the last its network process had of that network as members come and go, so that a run
/// host that lost an answer asks again safely (D278's reason for whole reaches). Its grants
/// come one at a time ([`Change`]) and outlast it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub network: NetworkId,
    /// The guest's address on it, its prefix, and the network's gateway, whose resolver
    /// answers its names. Fixed while the run is on it.
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    /// The network's name, under which a member's may be asked too (`name.network`, D46).
    pub name: String,
    /// Every name on it, the run's own members' among them, at its run's address, by Docker's
    /// rules (ROOT, 2026-11-16): a member's name, one member's, at one address; an alias,
    /// shared, at each address of a run one of whose members holds it (Compose's scaled
    /// replicas); each by Docker's grammar, `_` and `.` among it, compared lowercased (RFC
    /// 4343, `shards_abi::run::shared::name_ok`). Names before aliases, so that an address's
    /// first name, its PTR's, is a member's.
    pub members: Vec<(String, Ipv4Addr)>,
}

impl Join {
    /// As `NET_JOIN` carries it: the network's ID, the address, prefix and gateway, the
    /// network's name behind its length's byte, the members' count (four bytes: a subnet's
    /// addresses fit 32 bits) and each member's name behind its length's byte and its
    /// address. None where a name is longer than its byte says.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.network.0);
        out.extend_from_slice(&self.address.octets());
        out.push(self.prefix);
        out.extend_from_slice(&self.gateway.octets());
        put_name(&mut out, &self.name)?;
        out.extend_from_slice(&u32::try_from(self.members.len()).ok()?.to_be_bytes());
        for (name, address) in &self.members {
            put_name(&mut out, name)?;
            out.extend_from_slice(&address.octets());
        }
        Some(out)
    }

    /// What [`Join::encode`] wrote, each name lowercased. None for a malformed one: a prefix
    /// past 32; the guest's address its gateway's, or off the gateway's subnet; a name of no
    /// name's grammar, or at one address twice; an address off the subnet or the gateway's; or
    /// bytes past the members.
    pub fn decode(bytes: &[u8]) -> Option<Join> {
        let (id, rest) = bytes.split_at_checked(ID_LEN)?;
        let network = NetworkId::of(id)?;
        let (&[a, b, c, d, prefix, g0, g1, g2, g3], mut rest) = rest.split_first_chunk::<9>()?;
        let (address, gateway) = (Ipv4Addr::new(a, b, c, d), Ipv4Addr::new(g0, g1, g2, g3));
        if prefix > 32 || address == gateway || !within(address, gateway, prefix) {
            return None;
        }
        let name = take_name(&mut rest)?;
        let (count, mut rest) = rest.split_first_chunk::<4>()?;
        let mut members: Vec<(String, Ipv4Addr)> = Vec::new();
        let mut seen: std::collections::HashSet<(String, Ipv4Addr)> = std::collections::HashSet::new();
        // Each member takes six bytes at least, so a count past them ends at the first missing.
        for _ in 0..u32::from_be_bytes(*count) {
            let member = take_name(&mut rest)?.to_ascii_lowercase();
            let (octets, tail) = rest.split_first_chunk::<4>()?;
            rest = tail;
            let at = Ipv4Addr::from(*octets);
            if !shards_abi::run::shared::name_ok(member.as_bytes())
                || at == gateway
                || !within(at, gateway, prefix)
                || !seen.insert((member.clone(), at))
            {
                return None;
            }
            members.push((member, at));
        }
        rest.is_empty().then_some(Join {
            network,
            address,
            prefix,
            gateway,
            name,
            members,
        })
    }
}

/// `name` behind its length's byte.
fn put_name(out: &mut Vec<u8>, name: &str) -> Option<()> {
    out.push(u8::try_from(name.len()).ok()?);
    out.extend_from_slice(name.as_bytes());
    Some(())
}

/// The name [`put_name`] wrote at the start of `rest`, which is left past it.
fn take_name(rest: &mut &[u8]) -> Option<String> {
    let (&len, tail) = rest.split_first()?;
    let (name, tail) = tail.split_at_checked(usize::from(len))?;
    *rest = tail;
    String::from_utf8(name.to_vec()).ok()
}

/// Which way a [`Grant`] lets a flow open: from its peer to its member (`In`), or from its
/// member to its peer (`Out`). A flow's replies come back by its state, whichever way it
/// opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    In,
    Out,
}

/// A member's grant on a shared network (D285), as kiln's `NetworkGrant` names one (kiln
/// bb9ffcea1, crates/kiln-seams/src/sandbox.rs): its member and its peer, a member of another
/// run, each by its name there; a range of one protocol's ports, its peer's where it opens
/// `Out`, its member's where it opens `In`; and which way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub member: String,
    pub peer: String,
    pub proto: Proto,
    pub first: u16,
    pub last: u16,
    pub way: Way,
}

/// A change of a run's grants on a shared network (`NET_PEER_GRANT`): one grant added, or taken
/// back, as each comes (kiln's `Change::Connect` and `Disconnect`), never the run's whole set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Add(Grant),
    Remove(Grant),
}

/// `NET_PEER_GRANT`'s payload: the network's ID; 1 to add, 2 to take back; the way, 1 in, 2 out;
/// the protocol's IP number (6 TCP, 17 UDP); the first port and the last, two bytes each; then
/// the member's name and the peer's, each behind its length's byte. None where a name is
/// longer than its byte says.
pub fn encode_grant(network: &NetworkId, change: &Change) -> Option<Vec<u8>> {
    let (op, g) = match change {
        Change::Add(g) => (1, g),
        Change::Remove(g) => (2, g),
    };
    let mut out = network.0.to_vec();
    out.push(op);
    out.push(match g.way {
        Way::In => 1,
        Way::Out => 2,
    });
    out.push(match g.proto {
        Proto::Tcp => wire::PROTO_TCP,
        Proto::Udp => wire::PROTO_UDP,
    });
    out.extend_from_slice(&g.first.to_be_bytes());
    out.extend_from_slice(&g.last.to_be_bytes());
    put_name(&mut out, &g.member)?;
    put_name(&mut out, &g.peer)?;
    Some(out)
}

/// What [`encode_grant`] wrote, its names lowercased. None for a malformed one: a name of no
/// name's grammar ([`Join::members`]'), a peer that is its member, port 0, or a first port
/// past the last, as kiln's `NetworkGrant::check` refuses one; or bytes past its peer's name.
pub fn decode_grant(bytes: &[u8]) -> Option<(NetworkId, Change)> {
    let (id, rest) = bytes.split_at_checked(ID_LEN)?;
    let network = NetworkId::of(id)?;
    let (&[op, way, proto, f0, f1, l0, l1], mut rest) = rest.split_first_chunk::<7>()?;
    let way = match way {
        1 => Way::In,
        2 => Way::Out,
        _ => return None,
    };
    let proto = match proto {
        wire::PROTO_TCP => Proto::Tcp,
        wire::PROTO_UDP => Proto::Udp,
        _ => return None,
    };
    let (first, last) = (u16::from_be_bytes([f0, f1]), u16::from_be_bytes([l0, l1]));
    let member = take_name(&mut rest)?.to_ascii_lowercase();
    let peer = take_name(&mut rest)?.to_ascii_lowercase();
    if !rest.is_empty()
        || !shards_abi::run::shared::name_ok(member.as_bytes())
        || !shards_abi::run::shared::name_ok(peer.as_bytes())
        || member == peer
        || first == 0
        || first > last
    {
        return None;
    }
    let grant = Grant {
        member,
        peer,
        proto,
        first,
        last,
        way,
    };
    let change = match op {
        1 => Change::Add(grant),
        2 => Change::Remove(grant),
        _ => return None,
    };
    Some((network, change))
}

/// `NET_LINK`'s payload: the network's ID, then the other run's address on it.
pub fn encode_link(network: &NetworkId, peer: Ipv4Addr) -> Vec<u8> {
    [network.0.as_slice(), peer.octets().as_slice()].concat()
}

/// What [`encode_link`] wrote.
pub fn decode_link(bytes: &[u8]) -> Option<(NetworkId, Ipv4Addr)> {
    let (id, peer) = bytes.split_at_checked(ID_LEN)?;
    Some((
        NetworkId::of(id)?,
        Ipv4Addr::from(<[u8; 4]>::try_from(peer).ok()?),
    ))
}

/// Why a network process took nothing of a shared network's ask (D285), as its answer's
/// byte says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No network of that ID is joined (`NET_LEAVE`, `NET_LINK`, `NET_PEER_GRANT`).
    NotJoined,
    /// The network is joined at another address, prefix or gateway: left first, then
    /// joined anew.
    Readdressed,
    /// Its subnet shares an address with another network the run is on, or holds the
    /// guest's own address or its gateway's.
    Overlaps,
    /// A link to an address at which no member of another run is.
    NotAPeer,
    /// The process could not hold a descriptor for each link its join still needs, which
    /// would come cut from their messages (`sys::reserve`).
    NoDescriptors,
    /// A grant added that the run holds already.
    Granted,
    /// A grant taken back that the run holds not.
    NotGranted,
}

impl Refusal {
    const ALL: [Refusal; 7] = [
        Refusal::NotJoined,
        Refusal::Readdressed,
        Refusal::Overlaps,
        Refusal::NotAPeer,
        Refusal::NoDescriptors,
        Refusal::Granted,
        Refusal::NotGranted,
    ];

    /// Its byte in an answer: never 0, which says an ask was taken.
    pub fn code(self) -> u8 {
        match self {
            Refusal::NotJoined => 1,
            Refusal::Readdressed => 2,
            Refusal::Overlaps => 3,
            Refusal::NotAPeer => 4,
            Refusal::NoDescriptors => 5,
            Refusal::Granted => 6,
            Refusal::NotGranted => 7,
        }
    }

    /// The refusal whose byte `code` is.
    pub fn of(code: u8) -> Option<Refusal> {
        Refusal::ALL.into_iter().find(|r| r.code() == code)
    }
}

/// The answer to `NET_JOIN`, `NET_LEAVE`, `NET_LINK` or `NET_PEER_GRANT`: 0 where it was taken,
/// else its refusal's code.
pub fn encode_answer(answer: Result<(), Refusal>) -> [u8; 1] {
    [answer.err().map_or(0, Refusal::code)]
}

/// What [`encode_answer`] wrote.
pub fn decode_answer(payload: &[u8]) -> Option<Result<(), Refusal>> {
    match payload {
        [0] => Some(Ok(())),
        [code] => Refusal::of(*code).map(Err),
        _ => None,
    }
}

/// `address`'s bits that `prefix` names, as a mask.
fn mask(prefix: u8) -> u32 {
    32u32
        .checked_sub(u32::from(prefix))
        .and_then(|free| u32::MAX.checked_shl(free))
        .unwrap_or(0)
}

/// Whether `address` lies in the subnet of `on` and `prefix`.
fn within(address: Ipv4Addr, on: Ipv4Addr, prefix: u8) -> bool {
    u32::from(address) & mask(prefix) == u32::from(on) & mask(prefix)
}

/// A shared network as its network process holds it: the run's place and the members from
/// its [`Join`], and the run's grants there, the switch's table, by member, peer, ports and
/// way.
#[derive(Debug)]
pub(crate) struct Network {
    pub(crate) id: NetworkId,
    pub(crate) address: Ipv4Addr,
    prefix: u8,
    pub(crate) gateway: Ipv4Addr,
    name: String,
    /// Each name there at its run's address, as its join gives them ([`Join::members`]): a
    /// member's at one, an alias at each that holds it.
    members: Vec<(String, Ipv4Addr)>,
    /// Each other run's address whose link ended at its far end (D285 slice 6: that run
    /// ended, or left the network): its names and aliases answered no more from then on, the
    /// closed link the evidence, until a link there comes again; a join told before that
    /// run's leases went, which names them still, answers none of them meanwhile. At most one
    /// for each address its members are at.
    ended: Vec<Ipv4Addr>,
    grants: Vec<Grant>,
    /// The names its guest is answered there (D46's rule, a name only where a flow is): its
    /// run's members' own and those its grants name. Made again whole from `members` and
    /// `grants`, its sources, as either changes; as many as the network's members.
    pub(crate) names: dns::Names,
}

impl Network {
    fn new(join: Join) -> Network {
        let mut network = Network {
            id: join.network,
            address: join.address,
            prefix: join.prefix,
            gateway: join.gateway,
            name: join.name,
            members: join.members,
            ended: Vec::new(),
            grants: Vec::new(),
            names: dns::Names::default(),
        };
        network.rename();
        network
    }

    /// The names its guest is answered, made again from its members and its grants, in its
    /// join's order, names before aliases, so that an address's first name, its PTR's, is a
    /// member's; none at an address whose link has ended.
    fn rename(&mut self) {
        let mut names = dns::Names::new(&self.name);
        for (name, at) in &self.members {
            if self.ended.contains(at) {
                continue;
            }
            if *at == self.address || self.grants.iter().any(|g| g.peer == *name) {
                names.add(name, IpAddr::V4(*at));
            }
        }
        self.names = names;
    }

    /// Whether `name` is a member's or an alias at `peer`.
    fn names_at(&self, name: &str, peer: Ipv4Addr) -> bool {
        self.members.iter().any(|(n, at)| n == name && *at == peer)
    }

    /// Each other run's address on it, once: a link's to come where it has none.
    pub(crate) fn runs(&self) -> std::collections::HashSet<Ipv4Addr> {
        self.members
            .iter()
            .map(|&(_, at)| at)
            .filter(|&at| at != self.address)
            .collect()
    }

    /// Whether a member of another run is at `peer`: a link there is one of its.
    pub(crate) fn names_peer(&self, peer: Ipv4Addr) -> bool {
        peer != self.address && self.members.iter().any(|&(_, at)| at == peer)
    }

    /// Whether flow `key` on the link to `peer`, opened by `by`, is granted: by a grant that
    /// opens its way, of its protocol, its port among the grant's (the peer's where the guest
    /// opened it, the guest's where the peer did), its peer a member at `peer`.
    pub(crate) fn admits(&self, peer: Ipv4Addr, by: Side, key: &FlowKey) -> bool {
        let (way, port) = match by {
            Side::Ours => (Way::Out, key.theirs),
            Side::Theirs => (Way::In, key.ours),
        };
        self.grants.iter().any(|g| {
            g.way == way
                && g.proto == key.proto
                && (g.first..=g.last).contains(&port)
                && self.names_at(&g.peer, peer)
        })
    }

    /// The room of the link to `peer`'s flows: every key the grants of its members admit,
    /// each of their ports with every value of a flow's other end, which no table holds
    /// twice (D285's cold start, its grants' own 5-tuples, the link fixing both addresses).
    pub(crate) fn room(&self, peer: Ipv4Addr) -> usize {
        let ports = |way: Way| {
            Ports(
                self.grants
                    .iter()
                    .filter(|g| g.way == way && self.names_at(&g.peer, peer))
                    .map(|g| (g.proto, g.first, g.last))
                    .collect(),
            )
        };
        let keys = count(&ports(Way::Out)).saturating_add(count(&ports(Way::In)));
        usize::try_from(keys.saturating_mul(PORT_VALUES)).unwrap_or(usize::MAX)
    }

    /// Takes `change`: a grant added that it holds not, or taken back that it holds.
    fn change(&mut self, change: Change) -> Result<(), Refusal> {
        match change {
            Change::Add(g) if self.grants.contains(&g) => return Err(Refusal::Granted),
            Change::Add(g) => self.grants.push(g),
            Change::Remove(g) => {
                let held = self
                    .grants
                    .iter()
                    .position(|h| *h == g)
                    .ok_or(Refusal::NotGranted)?;
                self.grants.swap_remove(held);
            }
        }
        self.rename();
        Ok(())
    }
}

/// The values a port takes, 2^16 (RFC 9293 §3.1, RFC 768): a flow's free end, where its
/// grant fixes the other's.
const PORT_VALUES: u64 = 1 << 16;

/// How many (protocol, port) pairs `ports` holds, each once however its ranges overlap.
fn count(ports: &Ports) -> u64 {
    let mut total = 0u64;
    for proto in [Proto::Tcp, Proto::Udp] {
        let mut ranges: Vec<(u16, u16)> = ports
            .0
            .iter()
            .filter(|(p, _, _)| *p == proto)
            .map(|&(_, lo, hi)| (lo, hi))
            .collect();
        ranges.sort_unstable();
        // The last port counted so far.
        let mut counted: Option<u16> = None;
        for (lo, hi) in ranges {
            let from = match counted {
                Some(last) if lo <= last => last.checked_add(1),
                _ => Some(lo),
            };
            if let Some(from) = from.filter(|&from| from <= hi) {
                total = total.saturating_add(u64::from(hi - from) + 1);
            }
            counted = Some(counted.map_or(hi, |last| last.max(hi)));
        }
    }
    total
}

/// The shared networks a run is on.
#[derive(Debug, Default)]
pub(crate) struct Networks(Vec<Network>);

impl Networks {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Network> {
        self.0.iter()
    }

    pub(crate) fn get(&self, id: &NetworkId) -> Option<&Network> {
        self.0.iter().find(|n| n.id == *id)
    }

    /// The network on which `address` is the guest's.
    pub(crate) fn at(&self, address: Ipv4Addr) -> Option<&Network> {
        self.0.iter().find(|n| n.address == address)
    }

    /// Whether `address` is the guest's on a shared network.
    pub(crate) fn holds(&self, address: Ipv4Addr) -> bool {
        self.at(address).is_some()
    }

    /// Whether `join` may be taken beside the guest's `own` addresses (its address on its
    /// own network, that network's gateway, its link-local ones) and the networks it is on:
    /// the same place on a network joined already, or a subnet none of them shares.
    pub(crate) fn check(&self, join: &Join, own: &[Ipv4Addr]) -> Result<(), Refusal> {
        if let Some(n) = self.get(&join.network) {
            let same = (n.address, n.prefix, n.gateway) == (join.address, join.prefix, join.gateway);
            return if same { Ok(()) } else { Err(Refusal::Readdressed) };
        }
        let shares = self.0.iter().any(|n| {
            let shorter = n.prefix.min(join.prefix);
            within(n.gateway, join.gateway, shorter)
        }) || own.iter().any(|&a| within(a, join.gateway, join.prefix));
        if shares { Err(Refusal::Overlaps) } else { Ok(()) }
    }

    /// Takes `join`, checked: its members and name in place of the network's last, its
    /// grants kept, and each ended link's address kept so while a member is there; or the
    /// network beside the others, granting nothing yet.
    pub(crate) fn put(&mut self, join: Join) {
        match self.0.iter_mut().find(|n| n.id == join.network) {
            Some(n) => {
                n.name = join.name;
                n.members = join.members;
                let members = &n.members;
                n.ended.retain(|e| members.iter().any(|(_, at)| at == e));
                n.rename();
            }
            None => self.0.push(Network::new(join)),
        }
    }

    /// The link to the run at `peer` on network `id` ended at its far end (D285 slice 6): that
    /// run's names and aliases there answered no more, at once, until a link there comes
    /// again ([`Networks::linked`]).
    pub(crate) fn unlinked(&mut self, id: &NetworkId, peer: Ipv4Addr) {
        if let Some(n) = self.0.iter_mut().find(|n| n.id == *id)
            && n.names_peer(peer)
            && !n.ended.contains(&peer)
        {
            n.ended.push(peer);
            n.rename();
        }
    }

    /// A link to the run at `peer` on network `id` taken: its names and aliases there
    /// answered again, as the network's join gives them.
    pub(crate) fn linked(&mut self, id: &NetworkId, peer: Ipv4Addr) {
        if let Some(n) = self.0.iter_mut().find(|n| n.id == *id)
            && let Some(at) = n.ended.iter().position(|e| *e == peer)
        {
            n.ended.swap_remove(at);
            n.rename();
        }
    }

    /// Takes `change` of network `id`'s grants.
    pub(crate) fn change(&mut self, id: &NetworkId, change: Change) -> Result<(), Refusal> {
        self.0
            .iter_mut()
            .find(|n| n.id == *id)
            .ok_or(Refusal::NotJoined)?
            .change(change)
    }

    /// Forgets network `id`, its grants with it; whether the run was on it.
    pub(crate) fn remove(&mut self, id: &NetworkId) -> bool {
        let before = self.0.len();
        self.0.retain(|n| n.id != *id);
        self.0.len() != before
    }
}

/// A link's state on its shared network (`NET_LINK`): the network, and the flows that cross it.
#[derive(Debug)]
pub(crate) struct Link {
    pub(crate) network: NetworkId,
    pub(crate) flows: Flows,
}

/// Which of a link's two guests sent a packet, or opened a flow: this process's, or the
/// other run's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Ours,
    Theirs,
}

/// A packet's ends as a link's flows know them: its protocol, the guest's port and the
/// other run's guest's; the link fixes both addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FlowKey {
    pub(crate) proto: Proto,
    pub(crate) ours: u16,
    pub(crate) theirs: u16,
}

/// What of a TCP segment its flow's state follows.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Segment {
    flags: u8,
    seq: u32,
    ack: u32,
    /// Its payload's length.
    len: u32,
}

/// A packet on a link, as its flow sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Packet {
    pub(crate) key: FlowKey,
    /// Its TCP segment's; none of a UDP datagram.
    tcp: Option<Segment>,
}

impl Packet {
    /// `ip`'s TCP segment or UDP datagram, sent by `from`; none of any other protocol, nor
    /// of one cut short: no grant admits what has no port.
    pub(crate) fn of(ip: &wire::Ip<'_>, from: Side) -> Option<Packet> {
        let (proto, src, dst, tcp) = match ip.proto {
            wire::PROTO_TCP => {
                let t = wire::tcp(ip.payload)?;
                let segment = Segment {
                    flags: t.flags,
                    seq: t.seq,
                    ack: t.ack,
                    len: u32::try_from(t.payload.len()).ok()?,
                };
                (Proto::Tcp, t.src_port, t.dst_port, Some(segment))
            }
            wire::PROTO_UDP => {
                let u = wire::udp(ip.payload)?;
                (Proto::Udp, u.src_port, u.dst_port, None)
            }
            _ => return None,
        };
        let (ours, theirs) = match from {
            Side::Ours => (src, dst),
            Side::Theirs => (dst, src),
        };
        Some(Packet {
            key: FlowKey { proto, ours, theirs },
            tcp,
        })
    }

    /// The packet frame `f` carries on a shared network's link from `from`, the other run's
    /// address there, to `to`, the guest's: IPv4 alone and whole, as a fragment parses as
    /// none (`wire::ipv4`) and no port admits one; none from or to another address.
    pub(crate) fn linked(f: &[u8], from: Ipv4Addr, to: Ipv4Addr) -> Option<Packet> {
        let eth = f.get(wire::VNET..).and_then(wire::eth)?;
        if eth.kind != wire::ETHERTYPE_IPV4 {
            return None;
        }
        let ip = wire::ipv4(eth.payload)?;
        if ip.src != IpAddr::V4(from) || ip.dst != IpAddr::V4(to) {
            return None;
        }
        Packet::of(&ip, Side::Theirs)
    }

    /// Whether it may be a flow's first: a UDP datagram, or a SYN alone.
    fn opens(&self) -> bool {
        self.tcp
            .is_none_or(|s| s.flags & (wire::SYN | wire::ACK | wire::RST | wire::FIN) == wire::SYN)
    }
}

/// How long a TCP flow lives idle before it may go to make room, by its state: RFC 5382
/// (BCP 142) REQ-5's floors, "the value of the 'established connection idle-timeout' MUST NOT
/// be less than 2 hours 4 minutes. The value of the 'transitory connection idle-timeout' MUST
/// NOT be less than 4 minutes"; a UDP flow's is RFC 4787 REQ-5's,
/// `shards_abi::bounds::UDP_MAPPING_IDLE`.
const ESTABLISHED: Duration = Duration::from_secs((2 * 60 + 4) * 60);
const TRANSITORY: Duration = Duration::from_secs(4 * 60);

/// Why a packet did not cross a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// Of no flow, and not one's first: a TCP segment but a SYN alone.
    NoFlow,
    /// A flow's first that its sender's grant (at its sender's end) or its receiver's (at
    /// its receiver's) does not take.
    Ungranted,
    /// A flow's first with its link's table full of flows none of which is idle past its
    /// floor, or no memory to hold it.
    Full,
}

/// A flow's state (D285): a UDP flow is open from its first datagram; a TCP flow opens with
/// its opener's SYN, is open once a SYN came each way, closes from a FIN, and ends with an
/// RST or once a FIN each way is acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Udp,
    Opening,
    Open,
    /// Each side's FIN, once sent.
    Closing {
        ours: Option<Fin>,
        theirs: Option<Fin>,
    },
}

/// A FIN: the sequence number just past it, which its acknowledgement names, and whether
/// the other side has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fin {
    past: u32,
    acked: bool,
}

/// What a packet did to its flow.
enum Carried {
    Kept,
    /// An RST, or the last FIN's acknowledgement: the packet crosses, and the flow ends.
    Ended,
    /// A SYN alone on a flow closing: its ports taken again, a new flow, granted anew.
    Anew,
}

#[derive(Debug)]
struct Flow {
    /// Whose grant opened it.
    by: Side,
    state: State,
    /// When it last carried a packet.
    seen: Instant,
}

impl Flow {
    /// When it may go to make room: its floor past its last packet; none where the clock has
    /// no instant that far on.
    fn until(&self) -> Option<Instant> {
        let floor = match self.state {
            State::Udp => shards_abi::bounds::UDP_MAPPING_IDLE,
            State::Open => ESTABLISHED,
            State::Opening | State::Closing { .. } => TRANSITORY,
        };
        self.seen.checked_add(floor)
    }

    /// Follows a packet of its own from `from`.
    fn carry(&mut self, from: Side, tcp: Option<Segment>) -> Carried {
        let Some(s) = tcp else {
            return Carried::Kept;
        };
        if s.flags & wire::RST != 0 {
            return Carried::Ended;
        }
        let syn = s.flags & wire::SYN != 0;
        match self.state {
            State::Opening if syn && from != self.by => self.state = State::Open,
            State::Closing { .. } if syn && s.flags & wire::ACK == 0 => return Carried::Anew,
            _ => {}
        }
        if let State::Closing { ours, theirs } = &mut self.state
            && s.flags & wire::ACK != 0
        {
            let other = match from {
                Side::Ours => theirs,
                Side::Theirs => ours,
            };
            if let Some(fin) = other
                && at_or_past(s.ack, fin.past)
            {
                fin.acked = true;
            }
        }
        if s.flags & wire::FIN != 0 {
            // A SYN's sequence number, its bytes', then the FIN's own.
            let past = s
                .seq
                .wrapping_add(u32::from(syn))
                .wrapping_add(s.len)
                .wrapping_add(1);
            if !matches!(self.state, State::Closing { .. }) {
                self.state = State::Closing {
                    ours: None,
                    theirs: None,
                };
            }
            if let State::Closing { ours, theirs } = &mut self.state {
                let own = match from {
                    Side::Ours => ours,
                    Side::Theirs => theirs,
                };
                // A FIN sent again keeps its first's end.
                if own.is_none() {
                    *own = Some(Fin { past, acked: false });
                }
            }
        }
        match self.state {
            State::Closing {
                ours: Some(Fin { acked: true, .. }),
                theirs: Some(Fin { acked: true, .. }),
            } => Carried::Ended,
            _ => Carried::Kept,
        }
    }
}

/// Whether sequence number `n` is `of` or after it (RFC 9293 §3.4's arithmetic modulo 2^32).
fn at_or_past(n: u32, of: u32) -> bool {
    n.wrapping_sub(of) < 1 << 31
}

/// The earlier of `a` and `b`, either none.
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A link's flows (D285), held by the network process at each of its ends: each by its
/// ports, opened by the grant of the side whose packet was its first, and gone as its
/// state ends it, as its link goes, or, idle past its floor, to make room for a new one.
#[derive(Debug)]
pub(crate) struct Flows {
    flows: HashMap<FlowKey, Flow>,
    room: usize,
    /// No flow may go to make room before this, a bound each packet keeps: none where none
    /// ever may.
    due: Option<Instant>,
}

impl Flows {
    pub(crate) fn new(room: usize) -> Flows {
        Flows {
            flows: HashMap::new(),
            room,
            due: None,
        }
    }

    /// Whether packet `p`, sent by `from`, crosses at `now`: one of an open flow, which it
    /// carries on; or a flow's first, where `granted` takes its key and there is room for
    /// it, made by letting go of every flow idle past its floor where the table is full.
    pub(crate) fn admit(
        &mut self,
        from: Side,
        p: &Packet,
        granted: impl FnOnce(&FlowKey) -> bool,
        now: Instant,
    ) -> Result<(), Dropped> {
        if let Some(flow) = self.flows.get_mut(&p.key) {
            match flow.carry(from, p.tcp) {
                Carried::Kept => {
                    flow.seen = now;
                    self.due = earliest(self.due, flow.until());
                    return Ok(());
                }
                Carried::Ended => {
                    self.flows.remove(&p.key);
                    return Ok(());
                }
                Carried::Anew => {
                    self.flows.remove(&p.key);
                }
            }
        }
        if !p.opens() {
            return Err(Dropped::NoFlow);
        }
        if !granted(&p.key) {
            return Err(Dropped::Ungranted);
        }
        if self.flows.len() >= self.room && !self.make_room(now) {
            return Err(Dropped::Full);
        }
        // Held only where memory holds it: a full allocator drops the packet, never the
        // process (R01).
        if self.flows.try_reserve(1).is_err() {
            return Err(Dropped::Full);
        }
        let flow = Flow {
            by: from,
            state: if p.tcp.is_some() {
                State::Opening
            } else {
                State::Udp
            },
            seen: now,
        };
        self.due = earliest(self.due, flow.until());
        self.flows.insert(p.key, flow);
        Ok(())
    }

    /// Lets go of every flow idle past its floor at `now`, once one may be: RFC 5382's NAT
    /// "abandon[s] sessions reactively in response to new connection initiations", so no
    /// flow goes on a clock alone. Whether there is room now.
    fn make_room(&mut self, now: Instant) -> bool {
        if self.due.is_some_and(|due| due <= now) {
            self.flows
                .retain(|_, f| f.until().is_none_or(|until| until > now));
            self.due = self.flows.values().filter_map(Flow::until).min();
        }
        self.flows.len() < self.room
    }

    /// Ends every flow `grants` no longer admits, by its opener, and holds the rest to `room`,
    /// which they fit: each is a key the grants admit.
    pub(crate) fn regrant(&mut self, room: usize, grants: impl Fn(Side, &FlowKey) -> bool) {
        self.flows.retain(|key, flow| grants(flow.by, key));
        self.room = room;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.flows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> NetworkId {
        let mut id = [b'0'; ID_LEN];
        id[ID_LEN - 1] = b'0' + n;
        NetworkId(id)
    }

    fn join() -> Join {
        Join {
            network: id(1),
            address: Ipv4Addr::new(10, 89, 0, 2),
            prefix: 24,
            gateway: Ipv4Addr::new(10, 89, 0, 1),
            name: "team".into(),
            members: vec![
                ("harness".into(), Ipv4Addr::new(10, 89, 0, 2)),
                ("db".into(), Ipv4Addr::new(10, 89, 0, 3)),
                ("cache".into(), Ipv4Addr::new(10, 89, 0, 3)),
                ("web".into(), Ipv4Addr::new(10, 89, 0, 4)),
            ],
        }
    }

    fn grant(member: &str, peer: &str, proto: Proto, ports: (u16, u16), way: Way) -> Grant {
        Grant {
            member: member.into(),
            peer: peer.into(),
            proto,
            first: ports.0,
            last: ports.1,
            way,
        }
    }

    /// D285 slice 3. A join, a grant's change, a link and an answer read back as written,
    /// names lowercased; a join with a prefix past 32, its address its gateway's or off its
    /// subnet, a member off the subnet, of no name's grammar or at one address twice, an ID
    /// that is not lowercase hex, and a payload cut short or with bytes past it are refused; so
    /// is a grant of port 0, of ports first past last, to its own member, or of an unknown way,
    /// and an answer of no refusal. By Docker's rules (slice 4): a name with `_` and `.`, and
    /// an alias at two addresses, are taken, in a join and a grant.
    #[test]
    fn joins_grants_links_and_answers_read_back_as_written() {
        let j = join();
        let bytes = j.encode().unwrap();
        assert_eq!(Join::decode(&bytes), Some(j.clone()));
        assert_eq!(
            Join::decode(&bytes[..bytes.len() - 1]),
            None,
            "a member cut short"
        );
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Join::decode(&longer), None, "bytes past it");
        let malformed = |change: &dyn Fn(&mut Join)| {
            let mut j = join();
            change(&mut j);
            Join::decode(&j.encode().unwrap())
        };
        assert_eq!(malformed(&|j| j.prefix = 33), None);
        assert_eq!(malformed(&|j| j.address = j.gateway), None);
        assert_eq!(malformed(&|j| j.address = Ipv4Addr::new(10, 90, 0, 2)), None);
        assert_eq!(malformed(&|j| j.members[1].1 = Ipv4Addr::new(10, 90, 0, 3)), None);
        assert_eq!(malformed(&|j| j.members[1].1 = j.gateway), None);
        assert_eq!(malformed(&|j| j.members[1].0 = "_no".into()), None);
        assert_eq!(malformed(&|j| j.members[1].0 = "no such".into()), None);
        assert_eq!(
            malformed(&|j| j.members[2].0 = "DB".into()),
            None,
            "db twice at one address"
        );
        let shared = malformed(&|j| j.members[3].0 = "DB".into()).expect("an alias at two addresses");
        assert_eq!(shared.members[3], ("db".to_string(), Ipv4Addr::new(10, 89, 0, 4)));
        assert!(malformed(&|j| j.members[3].0 = "Web_1.proj".into()).is_some());
        let mut upper = bytes.clone();
        upper[0] = b'A';
        assert_eq!(Join::decode(&upper), None, "an ID's hex is lowercase");
        let added = Change::Add(grant("harness", "db", Proto::Tcp, (5432, 5432), Way::Out));
        let wire = encode_grant(&id(1), &added).unwrap();
        assert_eq!(decode_grant(&wire), Some((id(1), added)));
        let shouted = Change::Remove(grant("Harness", "DB", Proto::Udp, (53, 60), Way::In));
        let lowered = Change::Remove(grant("harness", "db", Proto::Udp, (53, 60), Way::In));
        assert_eq!(
            decode_grant(&encode_grant(&id(1), &shouted).unwrap()),
            Some((id(1), lowered))
        );
        let docker = Change::Add(grant("web_1", "db.proj", Proto::Tcp, (80, 80), Way::Out));
        assert_eq!(
            decode_grant(&encode_grant(&id(1), &docker).unwrap()),
            Some((id(1), docker))
        );
        for bad in [
            grant("harness", "db", Proto::Tcp, (0, 80), Way::Out),
            grant("harness", "db", Proto::Tcp, (81, 80), Way::Out),
            grant("harness", "Harness", Proto::Tcp, (80, 80), Way::Out),
            grant("harness", "-db", Proto::Tcp, (80, 80), Way::Out),
        ] {
            assert_eq!(
                decode_grant(&encode_grant(&id(1), &Change::Add(bad)).unwrap()),
                None
            );
        }
        let mut way = wire.clone();
        way[ID_LEN + 1] = 3;
        assert_eq!(decode_grant(&way), None, "no such way");
        assert_eq!(decode_grant(&wire[..wire.len() - 1]), None);
        let link = encode_link(&id(1), Ipv4Addr::new(10, 89, 0, 3));
        assert_eq!(decode_link(&link), Some((id(1), Ipv4Addr::new(10, 89, 0, 3))));
        assert_eq!(decode_link(&link[..link.len() - 1]), None);
        for answer in [Ok(()), Err(Refusal::NotJoined), Err(Refusal::NotGranted)] {
            assert_eq!(decode_answer(&encode_answer(answer)), Some(answer));
        }
        assert!(Refusal::ALL.iter().all(|r| r.code() != 0));
        assert_eq!(decode_answer(&[8]), None);
        assert_eq!(decode_answer(&[]), None);
    }

    /// A query of `name`'s records of `qtype` (A 1, PTR 12), as a guest's resolver asks one.
    fn question(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(u8::try_from(label.len()).unwrap());
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    /// D285 slice 6. Do: join db (with cache) at .3 and web at .4, `svc` an alias of both,
    /// harness granted out to each; end db's link at its far end; join again naming db still,
    /// as a join told before db's leases went does; link db again; end it again and join
    /// without db, then with it. Expect: each name its grants name answered at first; db's
    /// link ended, every name and alias at .3 answered no more at once, the rest kept (`svc` at
    /// .4 alone, web's address back to `web`, its member's name, never the alias); the join
    /// naming db answering it no more; a link there again answering it again; the join without
    /// db forgetting its mark, so that a run at .3 later is answered as it joins; an end on
    /// another network, or of an address no member is at, changing nothing.
    #[test]
    fn a_runs_names_go_with_its_link() {
        let (db, web) = (Ipv4Addr::new(10, 89, 0, 3), Ipv4Addr::new(10, 89, 0, 4));
        let mut j = join();
        j.members.push(("svc".into(), db));
        j.members.push(("svc".into(), web));
        let mut networks = Networks::default();
        networks.put(j.clone());
        for peer in ["db", "cache", "web", "svc"] {
            let to = grant("harness", peer, Proto::Tcp, (80, 80), Way::Out);
            assert_eq!(networks.change(&id(1), Change::Add(to)), Ok(()));
        }
        let answer = |networks: &Networks, name: &str, qtype: u16| -> Option<Vec<u8>> {
            let n = networks.get(&id(1)).unwrap();
            let q = question(name, qtype);
            n.names.holds(&q).then(|| n.names.answer(&q).unwrap())
        };
        let holds = |a: &[u8], at: Ipv4Addr| a.windows(4).any(|w| w == at.octets().as_slice());
        let svc = answer(&networks, "svc", 1).unwrap();
        assert!(holds(&svc, db) && holds(&svc, web));
        assert!(answer(&networks, "db", 1).is_some() && answer(&networks, "cache", 1).is_some());
        networks.unlinked(&id(2), db);
        networks.unlinked(&id(1), Ipv4Addr::new(10, 89, 0, 9));
        assert!(
            answer(&networks, "db", 1).is_some(),
            "another network's, or no member's"
        );
        networks.unlinked(&id(1), db);
        assert_eq!(answer(&networks, "db", 1), None, "db's name withdrawn");
        assert_eq!(answer(&networks, "cache", 1), None, "every name at its address");
        let svc = answer(&networks, "svc", 1).unwrap();
        assert!(holds(&svc, web) && !holds(&svc, db), "the alias at web's alone");
        assert_eq!(answer(&networks, "3.0.89.10.in-addr.arpa", 12), None);
        let back = answer(&networks, "4.0.89.10.in-addr.arpa", 12).unwrap();
        assert!(
            back.windows(4).any(|w| w == b"\x03web".as_slice()),
            "web's address back to its member's name"
        );
        networks.put(j.clone());
        assert_eq!(answer(&networks, "db", 1), None, "a join that names it still");
        networks.linked(&id(1), db);
        assert!(answer(&networks, "db", 1).is_some(), "a link there again");
        networks.unlinked(&id(1), db);
        let mut without = j.clone();
        without.members.retain(|(_, at)| *at != db);
        networks.put(without);
        networks.put(j);
        assert!(
            answer(&networks, "db", 1).is_some(),
            "a run at its address later, as it joins"
        );
    }

    /// A network is joined at one place: its join again at the same place is taken, at
    /// another refused `Readdressed`; a subnet that shares an address with another network's,
    /// or holds the guest's own address or gateway, is refused `Overlaps`; one that shares
    /// none is taken beside them.
    #[test]
    fn a_network_is_joined_at_one_place_and_no_two_share_an_address() {
        let mut networks = Networks::default();
        let own = [Ipv4Addr::new(172, 17, 0, 2), Ipv4Addr::new(172, 17, 0, 1)];
        let j = join();
        assert_eq!(networks.check(&j, &own), Ok(()));
        networks.put(j.clone());
        assert_eq!(networks.check(&j, &own), Ok(()), "the same place again");
        let moved = Join {
            address: Ipv4Addr::new(10, 89, 0, 9),
            ..join()
        };
        assert_eq!(networks.check(&moved, &own), Err(Refusal::Readdressed));
        let wider = Join {
            network: id(2),
            address: Ipv4Addr::new(10, 88, 0, 2),
            prefix: 15,
            gateway: Ipv4Addr::new(10, 88, 0, 1),
            ..join()
        };
        assert_eq!(networks.check(&wider, &own), Err(Refusal::Overlaps));
        let on_own = Join {
            network: id(2),
            address: Ipv4Addr::new(172, 17, 9, 2),
            prefix: 16,
            gateway: Ipv4Addr::new(172, 17, 9, 1),
            ..join()
        };
        assert_eq!(networks.check(&on_own, &own), Err(Refusal::Overlaps));
        let beside = Join {
            network: id(2),
            address: Ipv4Addr::new(10, 90, 0, 2),
            gateway: Ipv4Addr::new(10, 90, 0, 1),
            members: Vec::new(),
            ..join()
        };
        assert_eq!(networks.check(&beside, &own), Ok(()));
        networks.put(beside.clone());
        assert_eq!(networks.at(beside.address).map(|n| n.id), Some(id(2)));
        assert!(networks.remove(&id(1)));
        assert!(!networks.remove(&id(1)));
        assert!(!networks.holds(j.address));
    }

    /// The switch's table (D285, kiln's grants as they come): one grant added at a time,
    /// refused `Granted` where held, taken back one at a time, refused `NotGranted` where not
    /// held, and `NotJoined` off a network; two members' same grant held as two. A flow is
    /// granted by a grant of its way, protocol and port whose peer is a member at the link's
    /// address: out to the peer's port, in to the member's, from that peer alone. The names
    /// answered are the run's members' and those its grants name; a join again keeps the
    /// grants, its members' addresses resolving them anew.
    #[test]
    fn grants_are_held_one_at_a_time_by_member_peer_ports_and_way() {
        let mut networks = Networks::default();
        networks.put(join());
        let (db, web) = (Ipv4Addr::new(10, 89, 0, 3), Ipv4Addr::new(10, 89, 0, 4));
        let to_db = grant("harness", "db", Proto::Tcp, (5432, 5432), Way::Out);
        let from_web = grant("harness", "web", Proto::Tcp, (8080, 8081), Way::In);
        let twice = grant("agent", "db", Proto::Tcp, (5432, 5432), Way::Out);
        assert_eq!(networks.change(&id(1), Change::Add(to_db.clone())), Ok(()));
        assert_eq!(
            networks.change(&id(1), Change::Add(to_db.clone())),
            Err(Refusal::Granted)
        );
        assert_eq!(networks.change(&id(1), Change::Add(from_web.clone())), Ok(()));
        assert_eq!(networks.change(&id(1), Change::Add(twice.clone())), Ok(()));
        assert_eq!(
            networks.change(&id(9), Change::Add(twice.clone())),
            Err(Refusal::NotJoined)
        );
        let key = |proto, ours, theirs| FlowKey { proto, ours, theirs };
        let n = networks.get(&id(1)).unwrap();
        assert!(n.admits(db, Side::Ours, &key(Proto::Tcp, 40000, 5432)));
        assert!(
            !n.admits(db, Side::Ours, &key(Proto::Udp, 40000, 5432)),
            "its protocol alone"
        );
        assert!(
            !n.admits(web, Side::Ours, &key(Proto::Tcp, 40000, 5432)),
            "its peer alone"
        );
        assert!(
            !n.admits(db, Side::Theirs, &key(Proto::Tcp, 5432, 40000)),
            "its way alone"
        );
        assert!(n.admits(web, Side::Theirs, &key(Proto::Tcp, 8081, 50000)));
        assert!(
            !n.admits(db, Side::Theirs, &key(Proto::Tcp, 8081, 50000)),
            "from web alone"
        );
        assert_eq!(n.room(db), 1 << 16);
        assert_eq!(n.room(web), 2 << 16);
        let answered = |n: &Network, name: &str| {
            let mut q = vec![0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
            for label in name.split('.') {
                q.push(u8::try_from(label.len()).unwrap());
                q.extend_from_slice(label.as_bytes());
            }
            q.extend_from_slice(&[0, 0, 1, 0, 1]);
            n.names.holds(&q)
        };
        assert!(answered(n, "harness") && answered(n, "db") && answered(n, "web.team"));
        assert!(!answered(n, "cache"), "a member its grants name not");
        // db's run moves: its grants follow the name.
        let moved = Join {
            members: vec![
                ("harness".into(), Ipv4Addr::new(10, 89, 0, 2)),
                ("db".into(), web),
            ],
            ..join()
        };
        networks.put(moved);
        let n = networks.get(&id(1)).unwrap();
        assert!(n.admits(web, Side::Ours, &key(Proto::Tcp, 40000, 5432)));
        assert!(!n.admits(db, Side::Ours, &key(Proto::Tcp, 40000, 5432)));
        assert!(!n.names_peer(db), "no member is at its old address");
        assert_eq!(networks.change(&id(1), Change::Remove(to_db.clone())), Ok(()));
        assert_eq!(
            networks.change(&id(1), Change::Remove(to_db)),
            Err(Refusal::NotGranted)
        );
        let n = networks.get(&id(1)).unwrap();
        assert!(
            n.admits(web, Side::Ours, &key(Proto::Tcp, 40000, 5432)),
            "agent's own, kept"
        );
        assert_eq!(networks.change(&id(1), Change::Remove(twice)), Ok(()));
        let n = networks.get(&id(1)).unwrap();
        assert!(!n.admits(web, Side::Ours, &key(Proto::Tcp, 40000, 5432)));
        assert!(!answered(n, "db"), "a name no grant names any more");
    }

    /// A link's room is every key its grants admit: each granted (protocol, port), counted
    /// once however its ranges overlap, with each of a port's 2^16 values at the other end.
    #[test]
    fn a_links_room_is_every_key_its_grants_admit() {
        let ports = Ports(vec![
            (Proto::Tcp, 80, 90),
            (Proto::Tcp, 85, 95),
            (Proto::Tcp, 65535, 65535),
            (Proto::Udp, 80, 80),
        ]);
        assert_eq!(count(&ports), 16 + 1 + 1);
        assert_eq!(
            count(&Ports(vec![(Proto::Tcp, 1, 65535), (Proto::Tcp, 2, 65535)])),
            65535
        );
        let mut networks = Networks::default();
        networks.put(join());
        for g in [
            grant("harness", "db", Proto::Tcp, (5432, 5432), Way::Out),
            grant("agent", "db", Proto::Tcp, (5432, 5433), Way::Out),
            grant("harness", "cache", Proto::Udp, (11211, 11211), Way::In),
        ] {
            networks.change(&id(1), Change::Add(g)).unwrap();
        }
        let n = networks.get(&id(1)).unwrap();
        assert_eq!(
            n.room(Ipv4Addr::new(10, 89, 0, 3)),
            3 << 16,
            "db's and cache's run"
        );
        assert_eq!(n.room(Ipv4Addr::new(10, 89, 0, 4)), 0, "web's, granted nothing");
    }

    /// A TCP segment on a link: its ports as the link's flows key them, the guest's first.
    fn tcp(ours: u16, theirs: u16, flags: u8, seq: u32, ack: u32, len: u32) -> Packet {
        Packet {
            key: FlowKey {
                proto: Proto::Tcp,
                ours,
                theirs,
            },
            tcp: Some(Segment { flags, seq, ack, len }),
        }
    }

    /// A UDP datagram on a link.
    fn udp(ours: u16, theirs: u16) -> Packet {
        Packet {
            key: FlowKey {
                proto: Proto::Udp,
                ours,
                theirs,
            },
            tcp: None,
        }
    }

    fn yes(_: &FlowKey) -> bool {
        true
    }

    fn no(_: &FlowKey) -> bool {
        false
    }

    /// D285's flow, at one end. Do: open a TCP flow by its opener's SYN, its grant taking it;
    /// answer, carry bytes, then close it with a FIN each way and their acknowledgements; and
    /// open another, ended by an RST. Expect: no segment but a SYN alone opens one; the
    /// first's SYN refused where its grant takes it not; every segment of an open flow crosses
    /// whatever its port; the flow gone once both FINs are acknowledged, not before, and at
    /// the RST.
    #[test]
    fn a_tcp_flow_opens_on_a_granted_syn_and_ends_with_its_fins_acknowledged() {
        use wire::{ACK, FIN, RST, SYN};
        let now = Instant::now();
        let mut flows = Flows::new(1 << 16);
        let (o, t) = (Side::Ours, Side::Theirs);
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, ACK, 1, 1, 0), yes, now),
            Err(Dropped::NoFlow),
            "a segment of no flow"
        );
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, SYN, 100, 0, 0), no, now),
            Err(Dropped::Ungranted)
        );
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, SYN, 100, 0, 0), yes, now),
            Ok(())
        );
        // Its answer and the rest cross as the flow's, whatever a grant says.
        for (from, p) in [
            (t, tcp(40000, 5432, SYN | ACK, 900, 101, 0)),
            (o, tcp(40000, 5432, ACK, 101, 901, 0)),
            (o, tcp(40000, 5432, ACK, 101, 901, 10)),
            (t, tcp(40000, 5432, ACK, 901, 111, 20)),
        ] {
            assert_eq!(flows.admit(from, &p, no, now), Ok(()));
        }
        assert_eq!(flows.flows.values().next().map(|f| f.state), Some(State::Open));
        // Ours closes: its FIN at 111, acknowledged by 112; theirs at 921, by 922.
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, FIN | ACK, 111, 921, 0), no, now),
            Ok(())
        );
        assert_eq!(
            flows.admit(t, &tcp(40000, 5432, ACK, 921, 112, 0), no, now),
            Ok(())
        );
        assert_eq!(
            flows.admit(t, &tcp(40000, 5432, FIN | ACK, 921, 112, 0), no, now),
            Ok(())
        );
        assert_eq!(flows.len(), 1, "a FIN each way, the last not yet acknowledged");
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, ACK, 112, 922, 0), no, now),
            Ok(())
        );
        assert_eq!(flows.len(), 0, "both FINs acknowledged");
        assert_eq!(
            flows.admit(o, &tcp(40000, 5432, ACK, 112, 922, 0), no, now),
            Err(Dropped::NoFlow),
            "nothing of it crosses after"
        );
        assert_eq!(flows.admit(t, &tcp(5432, 41000, SYN, 7, 0, 0), yes, now), Ok(()));
        assert_eq!(
            flows.admit(o, &tcp(5432, 41000, RST | ACK, 0, 8, 0), no, now),
            Ok(())
        );
        assert_eq!(flows.len(), 0, "an RST ends it");
    }

    /// A SYN alone on a flow closing takes its ports again as a new flow, granted anew; a
    /// UDP flow is open from its first datagram, either way after.
    #[test]
    fn a_closing_flows_ports_taken_again_are_granted_anew() {
        use wire::{ACK, FIN, SYN};
        let now = Instant::now();
        let mut flows = Flows::new(1 << 16);
        let (o, t) = (Side::Ours, Side::Theirs);
        flows.admit(o, &tcp(40000, 80, SYN, 1, 0, 0), yes, now).unwrap();
        flows
            .admit(t, &tcp(40000, 80, SYN | ACK, 9, 2, 0), no, now)
            .unwrap();
        flows
            .admit(o, &tcp(40000, 80, FIN | ACK, 2, 10, 0), no, now)
            .unwrap();
        assert_eq!(
            flows.admit(o, &tcp(40000, 80, SYN, 50, 0, 0), no, now),
            Err(Dropped::Ungranted),
            "its new SYN, its grant gone"
        );
        assert_eq!(flows.len(), 0);
        assert_eq!(flows.admit(t, &udp(53, 9000), no, now), Err(Dropped::Ungranted));
        flows.admit(t, &udp(53, 9000), yes, now).unwrap();
        assert_eq!(flows.admit(o, &udp(53, 9000), no, now), Ok(()), "its answer");
    }

    /// A full table makes room only of flows idle past their floors (RFC 4787 REQ-5's two
    /// minutes for UDP, RFC 5382 REQ-5's four for TCP opening or closing and 2 h 4 min
    /// established), every one of them then, and none before: a new flow is dropped `Full`
    /// until one is.
    #[test]
    fn a_full_table_makes_room_of_flows_idle_past_their_floors_alone() {
        use wire::{ACK, SYN};
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut flows = Flows::new(3);
        let (o, t) = (Side::Ours, Side::Theirs);
        let held = |flows: &Flows, p: Packet| flows.flows.contains_key(&p.key);
        flows.admit(o, &udp(1, 53), yes, start).unwrap();
        flows.admit(o, &tcp(2, 80, SYN, 1, 0, 0), yes, start).unwrap();
        flows.admit(o, &tcp(3, 80, SYN, 1, 0, 0), yes, start).unwrap();
        flows
            .admit(t, &tcp(3, 80, SYN | ACK, 9, 2, 0), no, start)
            .unwrap();
        assert_eq!(
            flows.admit(o, &udp(4, 53), yes, at(119)),
            Err(Dropped::Full),
            "none idle two minutes yet"
        );
        assert_eq!(flows.admit(o, &udp(4, 53), yes, at(122)), Ok(()));
        assert!(!held(&flows, udp(1, 53)), "the UDP flow made room");
        assert_eq!(
            flows.admit(o, &udp(5, 53), yes, at(239)),
            Err(Dropped::Full),
            "the opening flow's four minutes not yet past"
        );
        assert_eq!(flows.admit(o, &udp(5, 53), yes, at(241)), Ok(()));
        assert!(
            !held(&flows, tcp(2, 80, SYN, 0, 0, 0)),
            "the opening flow made room"
        );
        assert!(
            held(&flows, tcp(3, 80, SYN, 0, 0, 0)) && held(&flows, udp(4, 53)),
            "the open flow and a UDP flow within its floor kept"
        );
        // At 7000 s both UDP flows are long idle; the open one, carried then, is kept.
        flows.admit(o, &tcp(3, 80, ACK, 2, 10, 0), no, at(7000)).unwrap();
        flows.admit(o, &udp(6, 53), yes, at(7000)).unwrap();
        flows.admit(o, &udp(7, 53), yes, at(7000)).unwrap();
        assert_eq!(flows.len(), 3);
        assert!(held(&flows, tcp(3, 80, SYN, 0, 0, 0)));
        assert_eq!(
            flows.admit(o, &udp(8, 53), yes, at(7000)),
            Err(Dropped::Full),
            "every flow within its floor"
        );
    }

    /// Grants that change end the flows they no longer admit, each by its opener's grant,
    /// and keep the rest.
    #[test]
    fn grants_changed_end_the_flows_they_no_longer_admit() {
        use wire::SYN;
        let now = Instant::now();
        let mut flows = Flows::new(1 << 16);
        flows
            .admit(Side::Ours, &tcp(40000, 5432, SYN, 1, 0, 0), yes, now)
            .unwrap();
        flows
            .admit(Side::Theirs, &tcp(8080, 50000, SYN, 1, 0, 0), yes, now)
            .unwrap();
        // Its egress lost 5432; its ingress keeps 8080.
        flows.regrant(1 << 16, |by, key| match by {
            Side::Ours => key.theirs != 5432,
            Side::Theirs => key.ours == 8080,
        });
        assert_eq!(flows.len(), 1);
        assert!(flows.flows.contains_key(&tcp(8080, 50000, SYN, 0, 0, 0).key));
    }
}
