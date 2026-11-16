//! Networks shared by runs, the guest's side (D285 slice 5): the run's address on each,
//! eth0's second, and each of its members' grants there, held at the member's own end as D278
//! holds its reach past the microVM. A flow exists where its sender's grant out names its
//! receiver and port and its receiver's grant in names its sender and that port: each run's
//! guest holds its own members to their own grants (their gates, the switch's tables, init's
//! translation), each run's network process the run's. A member is answered its own name there
//! and the names its grants name, an alias at every address holding it, a name at its one.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard, PoisonError};

use shards_abi::run::shared::{self as abi, Told, Way};

use super::{Linked, covers, linked_now, lost, reset, resolver};
use crate::links::{Flows, Shared, Switch};
use crate::netplan::Egress;

/// Why the run's place on a shared network, or its members' grants there, were refused, by a
/// cause its host tells apart (`builtin::SHARED_*`).
#[derive(Debug)]
pub enum Refused {
    /// No member of this index runs linked in this run.
    Unknown(u32),
    /// A grant past what its member's image and its ceiling let it reach.
    Past(String),
    /// The run is on the network at another address, prefix or gateway.
    Readdressed(String),
    /// The network's subnet meets another of the microVM's networks or links.
    Overlaps(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(member) => write!(f, "member {member} runs linked to nothing in this run"),
            Refused::Past(why) | Refused::Readdressed(why) | Refused::Overlaps(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Refused {}

/// The block the microVM's own links take their addresses from, IPv4's link-local (RFC 3927
/// §2.1, 169.254/16): the switch's and the gates' ends (`links::TRANSIT_SWITCH`,
/// `transit_gate`) and the uplink's (`run::beside::UPLINK_NET`). A shared network meeting it
/// would take their routes out of eth0.
const LINKS: (Ipv4Addr, u8) = (Ipv4Addr::new(169, 254, 0, 0), 16);

/// Each shared network the run is on, as the last [`change`] that was done told it.
static NETWORKS: Mutex<Vec<abi::Shared>> = Mutex::new(Vec::new());

fn networks_now() -> MutexGuard<'static, Vec<abi::Shared>> {
    NETWORKS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `builtin::SHARED`: the run on a shared network as `args` says, whole, or off it (D285).
/// Refused, typed ([`Refused`]), where the run could not hold it so; else done in D278's
/// order: every member's gate first, so that no new flow opens on what one lost; each member's
/// open TCP flows to what it lost reset, their RSTs crossing the rules still there; then the
/// switch's tables and init's; then eth0's second address, given once those keep the run's own
/// processes off it, or taken back once nothing names it; then each member's resolver. Told
/// again, it is done again whole, so that a host that lost an answer asks again safely.
pub fn change(args: &[Vec<u8>]) -> io::Result<()> {
    let told =
        abi::parse_args(args).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let switch = crate::domains::SWITCH
        .get()
        .ok_or_else(|| io::Error::other("the microVM links no member"))?;
    // Networks before members, as nothing else takes both.
    let mut networks = networks_now();
    let mut all = linked_now();
    let id = match &told {
        Told::On(s) => s.id,
        Told::Off(id) => *id,
    };
    let was = networks.iter().find(|n| n.id == id).cloned();
    let now = match told {
        Told::On(s) => {
            held(&s, was.as_ref(), switch, &all).map_err(io::Error::other)?;
            Some(s)
        }
        // Off already: done.
        Told::Off(_) if was.is_none() => return Ok(()),
        Told::Off(_) => None,
    };
    let mut next: Vec<abi::Shared> = networks.iter().filter(|n| n.id != id).cloned().collect();
    next.extend(now.clone());
    switch.share(carried(&next, &all))?;
    for l in all.iter() {
        let (before, after) = (flows_on(was.as_ref(), l), flows_on(now.as_ref(), l));
        if before.is_none() && after.is_none() {
            continue;
        }
        let (before, after) = (before.unwrap_or_default(), after.unwrap_or_default());
        let out = lost_from(&before.opens, &after.opens);
        let into = lost_from(&before.accepts, &after.accepts);
        if !out.is_empty() || !into.is_empty() {
            reset(&l.ns, |family, id| lost_to(family, id, &out, &into))?;
        }
        if ports_in(was.as_ref(), l.member) != ports_in(now.as_ref(), l.member) {
            local_ports(l, &next)?;
        }
    }
    switch.retable()?;
    match (&was, &now) {
        (None, Some(s)) => crate::net::shared_address(s.address, s.prefix, true)?,
        (Some(s), None) => crate::net::shared_address(s.address, s.prefix, false)?,
        _ => {}
    }
    let on_it = |s: &Option<abi::Shared>, member: u32| {
        s.as_ref()
            .is_some_and(|s| s.members.iter().any(|m| m.index == member))
    };
    for l in all.iter_mut() {
        if !on_it(&was, l.member) && !on_it(&now, l.member) {
            continue;
        }
        l.names = tables(l.member, &next);
        resolver(switch, l, &l.granted.reach())?;
    }
    *networks = next;
    Ok(())
}

/// Refuses `s` where the run could not hold it so: a member that runs linked to nothing, a
/// grant past what its member's image and its ceiling let it reach, a place other than the
/// one the run is on the network at, or, joining, a subnet that meets another of the
/// microVM's networks or links.
fn held(
    s: &abi::Shared,
    was: Option<&abi::Shared>,
    switch: &Switch,
    linked: &[Linked],
) -> Result<(), Refused> {
    for m in &s.members {
        let l = linked
            .iter()
            .find(|l| l.member == m.index)
            .ok_or(Refused::Unknown(m.index))?;
        if let Some(g) = m.grants.iter().find(|g| !within_reach(l, g)) {
            return Err(Refused::Past(format!(
                "{}: {} is past what its image and its ceiling let it reach",
                l.label,
                shown(g)
            )));
        }
    }
    match was {
        Some(w) if (w.address, w.prefix, w.gateway) != (s.address, s.prefix, s.gateway) => {
            Err(Refused::Readdressed(format!(
                "the run is on network {} at {}/{} through {}, which holds while it is on it",
                s.name, w.address, w.prefix, w.gateway
            )))
        }
        Some(_) => Ok(()),
        None => {
            let mut taken = switch.subnets();
            taken.push(LINKS);
            if let Some((addr, prefix, _)) = crate::net::current() {
                taken.push(abi::subnet(addr, prefix));
            }
            let subnet = abi::subnet(s.address, s.prefix);
            match meets(subnet, &taken) {
                Some((net, prefix)) => Err(Refused::Overlaps(format!(
                    "network {}'s subnet {}/{} meets the microVM's {net}/{prefix}",
                    s.name, subnet.0, subnet.1
                ))),
                None => Ok(()),
            }
        }
    }
}

/// The first of `taken` that shares an address with `subnet`.
fn meets(subnet: (Ipv4Addr, u8), taken: &[(Ipv4Addr, u8)]) -> Option<(Ipv4Addr, u8)> {
    taken
        .iter()
        .copied()
        .find(|t| crate::netplan::overlaps(*t, subnet))
}

/// Whether grant `g` is within what member `l`'s image and its ceiling let it reach, as D278
/// holds its reach past the microVM: out, ports their port grants cover, or a destination
/// either names at the peer's name; in, ports its image lets in to it past the microVM
/// (D122), or its ceiling's port grants cover.
fn within_reach(l: &Linked, g: &abi::Grant) -> bool {
    let (image, ceiling) = (&l.granted.image, &l.granted.ceiling);
    let mut ranges: Vec<Egress> = ceiling.ports.clone();
    match g.way {
        Way::Out => {
            ranges.extend(image.ports.iter().copied());
            ranges.extend(
                image
                    .named
                    .iter()
                    .chain(&ceiling.named)
                    .filter(|(host, _)| *host == g.peer)
                    .map(|&(_, port)| (6, port, port)),
            );
        }
        Way::In => ranges.extend(l.ingress.iter().copied()),
    }
    covers(&ranges, g.ports)
}

/// A grant as its refusal names it.
fn shown(g: &abi::Grant) -> String {
    let (proto, lo, hi) = g.ports;
    let proto = if proto == 17 { "UDP" } else { "TCP" };
    match g.way {
        Way::Out => format!("out to {} on {proto} ports {lo}-{hi}", g.peer),
        Way::In => format!("in from {} on {proto} ports {lo}-{hi}", g.peer),
    }
}

/// The addresses `peer` is at on `s`, by its name or an alias it holds, but the run's own: its
/// members reach one another on its own networks, never through eth0.
fn peers(s: &abi::Shared, peer: &str) -> Vec<Ipv4Addr> {
    let mut at: Vec<Ipv4Addr> = s
        .names
        .iter()
        .chain(&s.aliases)
        .filter(|(name, a)| name == peer && *a != s.address)
        .map(|&(_, a)| a)
        .collect();
    at.sort_unstable();
    at.dedup();
    at
}

/// Member `l`'s flows on `s`: each of its grants' peer at its addresses there, with the
/// grant's ports. None where it holds no grant there.
fn flows_on(s: Option<&abi::Shared>, l: &Linked) -> Option<Flows> {
    let s = s?;
    let m = s.members.iter().find(|m| m.index == l.member)?;
    let mut f = Flows {
        domain: l.domain,
        own: l.own.clone(),
        ..Flows::default()
    };
    for g in &m.grants {
        let at = peers(s, &g.peer);
        if at.is_empty() {
            continue;
        }
        match g.way {
            Way::Out => f.opens.push((at, vec![g.ports])),
            Way::In => f.accepts.push((at, vec![g.ports])),
        }
    }
    Some(f)
}

/// `networks` as the switch carries them: on each, the run's address and its subnet, and
/// every linked member's flows there.
fn carried(networks: &[abi::Shared], linked: &[Linked]) -> Vec<Shared> {
    networks
        .iter()
        .map(|s| Shared {
            addr: s.address,
            subnet: abi::subnet(s.address, s.prefix),
            flows: linked.iter().filter_map(|l| flows_on(Some(s), l)).collect(),
        })
        .collect()
}

/// Member `member`'s view of `networks`' names, D46's rule (a name only where a flow is): on
/// each it is on, its own name at the run's address, then each name its grants name, then each
/// alias, each at every address holding it but the run's own; so that an address's PTR, its
/// first name, is a member's name where it has one.
fn tables(member: u32, networks: &[abi::Shared]) -> Vec<shards_dns::Names> {
    networks
        .iter()
        .filter_map(|s| {
            let m = s.members.iter().find(|m| m.index == member)?;
            let mut names = shards_dns::Names::new(&s.name);
            names.add(&m.name, IpAddr::V4(s.address));
            for (name, at) in s.names.iter().chain(&s.aliases) {
                if *at != s.address && m.grants.iter().any(|g| g.peer == *name) {
                    names.add(name, IpAddr::V4(*at));
                }
            }
            Some(names)
        })
        .collect()
}

/// Each peer address `before` holds a TCP port for that `after` does not, with those ports:
/// what a member's open flows to or from that peer are reset for.
fn lost_from(
    before: &[(Vec<Ipv4Addr>, Vec<Egress>)],
    after: &[(Vec<Ipv4Addr>, Vec<Egress>)],
) -> Vec<(Ipv4Addr, Vec<(u16, u16)>)> {
    let of = |list: &[(Vec<Ipv4Addr>, Vec<Egress>)], peer: Ipv4Addr| -> Vec<Egress> {
        list.iter()
            .filter(|(at, _)| at.contains(&peer))
            .flat_map(|(_, ranges)| ranges.iter().copied())
            .collect()
    };
    let mut peers: Vec<Ipv4Addr> = before.iter().flat_map(|(at, _)| at.iter().copied()).collect();
    peers.sort_unstable();
    peers.dedup();
    peers
        .into_iter()
        .filter_map(|peer| {
            let ports = lost(&of(before, peer), &of(after, peer));
            (!ports.is_empty()).then_some((peer, ports))
        })
        .collect()
}

/// Whether the socket `id` names is a flow a grant no longer admits: to a peer's address at a
/// port `out` lost for it (its far port), or from one to a port of its own `into` lost (its
/// near port). The ID: its near port and its far one, each big-endian, then its near address
/// and its far one, 16 bytes each, IPv4's in their first 4, or mapped in an IPv6 socket's.
fn lost_to(
    family: u8,
    id: &[u8; 48],
    out: &[(Ipv4Addr, Vec<(u16, u16)>)],
    into: &[(Ipv4Addr, Vec<(u16, u16)>)],
) -> bool {
    let (Some(&[n0, n1]), Some(&[f0, f1]), Some(far)) = (id.get(0..2), id.get(2..4), id.get(20..36)) else {
        return false;
    };
    let peer = if family == libc::AF_INET as u8 {
        far.get(..4)
            .and_then(|a| <[u8; 4]>::try_from(a).ok())
            .map(Ipv4Addr::from)
    } else {
        <[u8; 16]>::try_from(far)
            .ok()
            .and_then(|a| std::net::Ipv6Addr::from(a).to_ipv4_mapped())
    };
    let Some(peer) = peer else { return false };
    let hit = |lost: &[(Ipv4Addr, Vec<(u16, u16)>)], port: u16| {
        lost.iter()
            .any(|(at, ranges)| *at == peer && ranges.iter().any(|&(lo, hi)| lo <= port && port <= hi))
    };
    hit(out, u16::from_be_bytes([f0, f1])) || hit(into, u16::from_be_bytes([n0, n1]))
}

/// The ports member `member`'s grants in on `s` open to it.
fn ports_in(s: Option<&abi::Shared>, member: u32) -> Vec<Egress> {
    s.and_then(|s| s.members.iter().find(|m| m.index == member))
        .map(|m| {
            m.grants
                .iter()
                .filter(|g| g.way == Way::In)
                .map(|g| g.ports)
                .collect()
        })
        .unwrap_or_default()
}

/// Member `l`'s own connections' ports kept clear of every port others open flows to it on,
/// as it was linked (D61) and as its grants in on `networks` open (D285), set in its network
/// namespace: an answer to its own connection at a port a peer is let in on would meet that
/// peer's rule alone at its gate.
fn local_ports(l: &Linked, networks: &[abi::Shared]) -> io::Result<()> {
    let mut accepted = l.accepted.clone();
    for s in networks {
        accepted.extend(ports_in(Some(s), l.member));
    }
    let (lo, hi) = crate::links::local_ports(&accepted).unwrap_or(crate::links::LOCAL_PORTS);
    crate::links::in_netns(Some(l.ns.as_raw_fd()), move || {
        crate::setup::write_sysctl("net.ipv4.ip_local_port_range", &format!("{lo} {hi}"))
            .map_err(io::Error::other)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use shards_abi::run::egress::Grants;

    fn at(d: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 89, 0, d)
    }

    fn grant(way: Way, peer: &str, ports: Egress) -> abi::Grant {
        abi::Grant {
            way,
            peer: peer.into(),
            ports,
        }
    }

    /// The run at .2 on `team`, its member 1 `codex` granted out to `db` and in from `web`,
    /// an alias two replicas hold, one of them `web_1`; `other` none of its grants names.
    fn network() -> abi::Shared {
        abi::Shared {
            id: [b'a'; abi::ID_LEN],
            name: "team".into(),
            address: at(2),
            prefix: 24,
            gateway: at(1),
            names: vec![
                ("codex".into(), at(2)),
                ("db".into(), at(3)),
                ("web_1".into(), at(4)),
                ("web_2".into(), at(5)),
                ("other".into(), at(6)),
            ],
            aliases: vec![("web".into(), at(4)), ("web".into(), at(5))],
            members: vec![abi::Member {
                index: 1,
                name: "codex".into(),
                grants: vec![
                    grant(Way::Out, "db", (6, 5432, 5432)),
                    grant(Way::In, "web", (6, 8080, 8080)),
                    grant(Way::Out, "gone", (6, 80, 80)),
                ],
            }],
        }
    }

    fn linked(member: u32, image: Grants, ceiling: Grants, ingress: &[Egress]) -> Linked {
        Linked {
            member,
            domain: 3,
            label: "harness codex".into(),
            granted: crate::netplan::Granted {
                image,
                ceiling,
                given: None,
            },
            own: vec![Ipv4Addr::new(172, 30, 0, 2)],
            own6: Vec::new(),
            ns: std::fs::File::open("/dev/null").unwrap().into(),
            subnets: Vec::new(),
            subnets6: Vec::new(),
            ingress: ingress.to_vec(),
            accepted: ingress.to_vec(),
            names: Vec::new(),
        }
    }

    fn ports(ports: &[Egress], named: &[(&str, u16)]) -> Grants {
        Grants {
            ports: ports.to_vec(),
            named: named.iter().map(|(h, p)| (h.to_string(), *p)).collect(),
            dns: false,
        }
    }

    /// A grant out is within a member's image's or its ceiling's port grants, or a destination
    /// either names at the peer's name; a grant in, within the ports its image lets in to it or
    /// its ceiling's; anything past both is refused, naming it.
    #[test]
    fn a_members_grants_on_a_shared_network_are_held_within_its_image_and_its_ceiling() {
        let l = linked(
            1,
            ports(&[(6, 443, 443)], &[("db", 5432)]),
            ports(&[(17, 9000, 9010)], &[]),
            &[(6, 8080, 8080)],
        );
        assert!(
            within_reach(&l, &grant(Way::Out, "db", (6, 5432, 5432))),
            "a destination it names"
        );
        assert!(
            within_reach(&l, &grant(Way::Out, "any", (6, 443, 443))),
            "a port to any address"
        );
        assert!(
            within_reach(&l, &grant(Way::Out, "any", (17, 9001, 9002))),
            "its ceiling's"
        );
        assert!(
            !within_reach(&l, &grant(Way::Out, "web", (6, 5432, 5432))),
            "db's port, not web's"
        );
        assert!(
            within_reach(&l, &grant(Way::In, "web", (6, 8080, 8080))),
            "a port let in to it"
        );
        assert!(
            within_reach(&l, &grant(Way::In, "web", (17, 9000, 9000))),
            "its ceiling's"
        );
        assert!(
            !within_reach(&l, &grant(Way::In, "web", (6, 443, 443))),
            "out is not in"
        );
        assert_eq!(
            shown(&grant(Way::In, "web", (17, 9000, 9010))),
            "in from web on UDP ports 9000-9010"
        );
    }

    /// A member's flows are its grants' peers at their addresses there, an alias at each run's
    /// that holds it, a peer not on the network at none; its view of the names is its own and
    /// its grants' peers', a name answered before an alias for an address's PTR, and no name
    /// its grants do not name.
    #[test]
    fn a_members_flows_and_names_are_its_grants_peers() {
        let s = network();
        let l = linked(1, Grants::default(), Grants::default(), &[]);
        let f = flows_on(Some(&s), &l).unwrap();
        assert_eq!(f.opens, vec![(vec![at(3)], vec![(6, 5432, 5432)])]);
        assert_eq!(f.accepts, vec![(vec![at(4), at(5)], vec![(6, 8080, 8080)])]);
        assert_eq!((f.domain, f.own.clone()), (3, l.own.clone()));
        assert!(flows_on(Some(&s), &linked(2, Grants::default(), Grants::default(), &[])).is_none());
        assert!(flows_on(None, &l).is_none());
        let shared = carried(std::slice::from_ref(&s), std::slice::from_ref(&l));
        assert_eq!(shared[0].addr, at(2));
        assert_eq!(shared[0].subnet, (Ipv4Addr::new(10, 89, 0, 0), 24));
        assert_eq!(shared[0].flows, vec![f]);
        let names = tables(1, std::slice::from_ref(&s));
        assert_eq!(names.len(), 1);
        let answered = |name: &str, qtype: u16| {
            let q = query(name, qtype);
            names[0].holds(&q).then(|| names[0].answer(&q).unwrap())
        };
        // Its own name, at the run's address; db's; the alias at both replicas.
        assert_eq!(a_records(&answered("codex", 1).unwrap()), vec![at(2)]);
        assert_eq!(a_records(&answered("db.team", 1).unwrap()), vec![at(3)]);
        let mut web = a_records(&answered("web", 1).unwrap());
        web.sort_unstable();
        assert_eq!(web, vec![at(4), at(5)]);
        assert!(answered("other", 1).is_none(), "no grant names it");
        assert!(
            answered("web_1", 1).is_none(),
            "named by no grant, though its address is"
        );
        assert!(tables(2, std::slice::from_ref(&s)).is_empty(), "not on it");
    }

    /// What a member lost is each peer's TCP ports it held before and not after, by address;
    /// a flow is reset where it goes to such a peer's lost port, or comes from one to a port of
    /// its own it lost, of either family, and no other.
    #[test]
    fn what_a_member_lost_is_found_by_its_peer_and_port() {
        let before = vec![
            (vec![at(3)], vec![(6, 5432, 5432), (17, 53, 53)]),
            (vec![at(4), at(5)], vec![(6, 8000, 8100)]),
        ];
        let after = vec![(vec![at(4)], vec![(6, 8000, 8049)])];
        assert_eq!(
            lost_from(&before, &after),
            vec![
                (at(3), vec![(5432, 5432)]),
                (at(4), vec![(8050, 8100)]),
                (at(5), vec![(8000, 8100)]),
            ]
        );
        assert!(lost_from(&after, &before).is_empty(), "widened, nothing lost");
        let out = vec![(at(3), vec![(5432, 5432)])];
        let into = vec![(at(4), vec![(8080, 8080)])];
        let mut id = [0u8; 48];
        id[0..2].copy_from_slice(&40000u16.to_be_bytes());
        id[2..4].copy_from_slice(&5432u16.to_be_bytes());
        id[20..24].copy_from_slice(&at(3).octets());
        let inet = libc::AF_INET as u8;
        assert!(lost_to(inet, &id, &out, &into), "its flow out to db's lost port");
        id[2..4].copy_from_slice(&5433u16.to_be_bytes());
        assert!(!lost_to(inet, &id, &out, &into), "another port");
        let mut came = [0u8; 48];
        came[0..2].copy_from_slice(&8080u16.to_be_bytes());
        came[2..4].copy_from_slice(&51000u16.to_be_bytes());
        came[20..24].copy_from_slice(&at(4).octets());
        assert!(
            lost_to(inet, &came, &out, &into),
            "a flow web_1 opened to its port"
        );
        came[20..24].copy_from_slice(&at(5).octets());
        assert!(!lost_to(inet, &came, &out, &into), "another peer's, kept");
        let mut mapped = [0u8; 48];
        mapped[2..4].copy_from_slice(&5432u16.to_be_bytes());
        mapped[20..36].copy_from_slice(&at(3).to_ipv6_mapped().octets());
        assert!(
            lost_to(libc::AF_INET6 as u8, &mapped, &out, &into),
            "an IPv6 socket's"
        );
    }

    /// A shared network joins only where its subnet meets none of the microVM's: its links'
    /// link-local block, eth0's subnet, its own networks' and the other shared ones'.
    #[test]
    fn a_shared_network_meets_none_of_the_microvms() {
        let taken = [
            LINKS,
            (Ipv4Addr::new(172, 17, 0, 0), 16),
            (Ipv4Addr::new(10, 89, 0, 0), 24),
        ];
        assert_eq!(meets((Ipv4Addr::new(10, 90, 0, 0), 24), &taken), None);
        assert_eq!(meets((Ipv4Addr::new(10, 0, 0, 0), 8), &taken), Some(taken[2]));
        assert_eq!(meets((Ipv4Addr::new(169, 254, 77, 0), 24), &taken), Some(LINKS));
        assert_eq!(meets((Ipv4Addr::new(172, 17, 5, 0), 24), &taken), Some(taken[1]));
        assert_eq!(ports_in(Some(&network()), 1), vec![(6, 8080, 8080)]);
        assert!(ports_in(Some(&network()), 2).is_empty());
    }

    /// A query for `name` of `qtype`, ID 7, recursion desired.
    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0, 7, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    /// The addresses an answer's A records hold.
    fn a_records(answer: &[u8]) -> Vec<Ipv4Addr> {
        shards_dns::a_records(answer)
            .unwrap()
            .addresses
            .into_iter()
            .filter_map(|a| match a {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            })
            .collect()
    }
}
