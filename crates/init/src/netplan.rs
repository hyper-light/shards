//! The links between an image's agents and harnesses, as its normalized Agentfile's
//! `CONNECT`s grant them (docs/design/architecture.md D59; AGENTFILE_ARCH.md §4.6, §4.7,
//! §9.7): which networks each domain joins, its address on each, which domains it may
//! open connections to, and the names it resolves.
//!
//! On a network, its members, every domain a `CONNECT … ON` it names, may each reach the
//! others (§4.6), except that `CONNECT x TO y` lets `y` only answer `x` (§4.7), unless
//! another directive grants `y` to `x` outright. Each domain has one link (§9.7) and policy
//! goes by link, so a pair any shared network allows is allowed.
//!
//! Past the microVM, a domain may open flows to the ports its networks let cross both
//! boundaries (§4.1, §4.6, §12 answer 6): of each network it joins that is not internal,
//! those its own grants (`--egress`, `--expose`) and the microVM's for it (`EXPOSE ...
//! FOR` it, not ingress-only) both open, as the build's `agentfile::boundary` reads them.

use std::net::Ipv4Addr;

use crate::json::Value;

/// `/etc/hosts`'s first lines, as Docker writes them for every container (moby
/// daemon/libnetwork/etchosts/etchosts.go, `Build`): the guest's IPv6 is enabled, so the
/// variant without it (`BuildNoIPv6`) does not apply. A run's file and each domain's start
/// with them.
pub const HOSTS: &[u8] = b"127.0.0.1\tlocalhost\n\
::1\tlocalhost ip6-localhost ip6-loopback\n\
fe00::\tip6-localnet\n\
ff00::\tip6-mcastprefix\n\
ff02::1\tip6-allnodes\n\
ff02::2\tip6-allrouters\n";

/// The block subnets are taken from where a network names none: /24s of 10.244.0.0/16,
/// the first that overlap neither the microVM's own network nor a declared one.
const POOL: (Ipv4Addr, u8) = (Ipv4Addr::new(10, 244, 0, 0), 16);
const POOL_PREFIX: u8 = 24;

/// A domain, as the Agentfile names it: whether it is a harness, and its name.
pub type Name = (bool, String);

/// A domain's address on one network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub network: String,
    pub addr: Ipv4Addr,
    pub subnet: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
}

/// A port range a domain may open flows to past its microVM: the protocol's IP number
/// (6 TCP, 17 UDP), and the range's ends.
pub type Egress = (u8, u16, u16);

/// A domain's link: its addresses, the names it resolves (`/etc/hosts`), the ports it may
/// reach past the microVM, and whether it opens connections and is connected to, which its
/// Landlock rules follow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Link {
    pub addresses: Vec<Address>,
    pub hosts: Vec<u8>,
    pub egress: Vec<Egress>,
    /// The ports let in past the microVM to it: of networks it is the one member of,
    /// those both boundaries open inward (the build refuses several members).
    pub ingress: Vec<Egress>,
    pub connects: bool,
    pub accepts: bool,
}

/// A port as Docker writes one (`443`, `53/udp`, `8000-8010/tcp`), TCP where none is said.
fn port_range(s: &str) -> Option<Egress> {
    let (range, proto) = match s.rsplit_once('/') {
        Some((r, "tcp")) => (r, 6),
        Some((r, "udp")) => (r, 17),
        Some(_) => return None,
        None => (s, 6),
    };
    let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p > 0);
    let (lo, hi) = match range.split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => (port(range)?, port(range)?),
    };
    (lo <= hi).then_some((proto, lo, hi))
}

/// The plan: each domain's link, by its index in `names`, and the pairs `(from, to)` that
/// may open connections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub links: Vec<Option<Link>>,
    pub pairs: Vec<(usize, usize)>,
    /// Whether any domain reaches past the microVM: the switch's link to it.
    pub uplink: bool,
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::strings).unwrap_or_default()
}

fn cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (a, p) = s.split_once('/')?;
    let a: Ipv4Addr = a.parse().ok()?;
    let p: u8 = p.parse().ok().filter(|p| *p <= 30)?;
    Some((Ipv4Addr::from(u32::from(a) & mask(p)), p))
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn overlaps(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
    let p = a.1.min(b.1);
    u32::from(a.0) & mask(p) == u32::from(b.0) & mask(p)
}

/// Plans the links of `names`, the domains that run, from `spec`, the normalized
/// Agentfile; `own` is the microVM's own network, which no subnet may overlap.
pub fn plan(spec: &Value, names: &[Name], own: Option<(Ipv4Addr, u8)>) -> Result<Plan, String> {
    let index = |kind: Option<&str>, name: &str| -> Option<usize> {
        let find = |harness: bool| names.iter().position(|(h, n)| *h == harness && n == name);
        match kind {
            Some("agent") => find(false),
            Some("harness") => find(true),
            _ => find(false).or_else(|| find(true)),
        }
    };
    // Per network: its members, the grants outright, the TO restrictions.
    let mut members: Vec<(String, Vec<usize>)> = Vec::new();
    let mut granted: Vec<(String, usize, usize)> = Vec::new();
    let mut restricted: Vec<(String, usize, usize)> = Vec::new();
    for c in spec.get("connections").map(Value::array).unwrap_or_default() {
        let kind = c.get("kind").and_then(Value::str);
        let from: Vec<usize> = strings(c.get("from"))
            .iter()
            .filter_map(|n| index(kind, n))
            .collect();
        let to: Vec<usize> = strings(c.get("to"))
            .iter()
            .filter_map(|n| index(kind, n))
            .collect();
        let both = matches!(c.get("bothWays"), Some(Value::Bool(true)));
        for net in strings(c.get("on")) {
            let at = match members.iter().position(|(n, _)| *n == net) {
                Some(at) => at,
                None => {
                    members.push((net.clone(), Vec::new()));
                    members.len() - 1
                }
            };
            if let Some((_, m)) = members.get_mut(at) {
                for &d in from.iter().chain(&to) {
                    if !m.contains(&d) {
                        m.push(d);
                    }
                }
            }
            for &x in &from {
                for &y in &to {
                    granted.push((net.clone(), x, y));
                    if both {
                        granted.push((net.clone(), y, x));
                    } else {
                        restricted.push((net.clone(), y, x));
                    }
                }
            }
            if both {
                for &x in &from {
                    for &y in &from {
                        granted.push((net.clone(), x, y));
                    }
                }
            }
        }
    }
    for (_, m) in &mut members {
        m.sort_unstable();
    }
    let mut pairs = Vec::new();
    for (net, m) in &members {
        for &x in m {
            for &y in m {
                let outright = granted.iter().any(|(n, a, b)| n == net && *a == x && *b == y);
                let barred = restricted.iter().any(|(n, a, b)| n == net && *a == x && *b == y);
                if x != y && (outright || !barred) && !pairs.contains(&(x, y)) {
                    pairs.push((x, y));
                }
            }
        }
    }
    pairs.sort_unstable();
    // Subnets: as declared, else from the pool.
    let declared: Vec<(String, Vec<String>, Vec<String>)> = spec
        .get("networks")
        .map(Value::array)
        .unwrap_or_default()
        .iter()
        .filter_map(|n| {
            Some((
                n.get("name").and_then(Value::str)?.to_string(),
                strings(n.get("subnets")),
                strings(n.get("gateways")),
            ))
        })
        .collect();
    let mut taken: Vec<(Ipv4Addr, u8)> = own.into_iter().collect();
    for (_, subnets, _) in &declared {
        taken.extend(subnets.iter().filter_map(|s| cidr(s)));
    }
    let mut next = 0u32;
    let mut links: Vec<Option<Link>> = vec![None; names.len()];
    for (net, m) in &members {
        let said = declared.iter().find(|(n, _, _)| n == net);
        let ipv4 = said.and_then(|(_, s, _)| s.iter().find_map(|s| cidr(s)));
        let (subnet, prefix) = match ipv4 {
            Some(s) => s,
            None => loop {
                let base = u32::from(POOL.0) + (next << (32 - u32::from(POOL_PREFIX)));
                next += 1;
                if next > 1 << (POOL_PREFIX - POOL.1) {
                    return Err(format!("network {net}: no /24 of {}/{} is free", POOL.0, POOL.1));
                }
                let candidate = (Ipv4Addr::from(base), POOL_PREFIX);
                if !taken.iter().any(|t| overlaps(*t, candidate)) {
                    taken.push(candidate);
                    break candidate;
                }
            },
        };
        let base = u32::from(subnet);
        let gateway = said
            .and_then(|(_, _, g)| g.first())
            .and_then(|g| g.parse::<Ipv4Addr>().ok())
            .unwrap_or(Ipv4Addr::from(base + 1));
        // Addresses from .2 up, past the gateway, short of the broadcast address.
        let room = (1u64 << (32 - u32::from(prefix))).saturating_sub(3);
        if m.len() as u64 > room {
            return Err(format!("network {net}: {} members, room for {room}", m.len()));
        }
        let mut host = base + 2;
        for &d in m {
            if Ipv4Addr::from(host) == gateway {
                host += 1;
            }
            let link = links
                .get_mut(d)
                .ok_or("a member beyond the domains")?
                .get_or_insert_with(Link::default);
            link.addresses.push(Address {
                network: net.clone(),
                addr: Ipv4Addr::from(host),
                subnet,
                prefix,
                gateway,
            });
            host += 1;
        }
    }
    // Names: each domain's own, and every member's of each network it joins, the first
    // network's address for a name on several.
    for d in 0..links.len() {
        let Some(own) = links.get(d).and_then(Option::as_ref) else {
            continue;
        };
        let mut hosts = String::new();
        let mut named: Vec<usize> = Vec::new();
        for a in &own.addresses {
            let Some((_, m)) = members.iter().find(|(n, _)| *n == a.network) else {
                continue;
            };
            for &peer in m {
                if named.contains(&peer) {
                    continue;
                }
                let Some(addr) = links
                    .get(peer)
                    .and_then(Option::as_ref)
                    .and_then(|l| l.addresses.iter().find(|x| x.network == a.network))
                else {
                    continue;
                };
                if let Some((_, name)) = names.get(peer) {
                    hosts.push_str(&format!("{}\t{name}\n", addr.addr));
                }
                named.push(peer);
            }
        }
        let joined: Vec<String> = own.addresses.iter().map(|a| a.network.clone()).collect();
        let egress = boundary_of(spec, &joined, true)?;
        let alone: Vec<String> = joined
            .iter()
            .filter(|n| members.iter().any(|(m, ds)| m == *n && ds.as_slice() == [d]))
            .cloned()
            .collect();
        let ingress = boundary_of(spec, &alone, false)?;
        let connects = pairs.iter().any(|(x, _)| *x == d) || !egress.is_empty();
        let accepts = pairs.iter().any(|(_, y)| *y == d) || !ingress.is_empty();
        if let Some(Some(link)) = links.get_mut(d) {
            link.hosts = [HOSTS, hosts.as_bytes()].concat();
            link.egress = egress;
            link.ingress = ingress;
            link.connects = connects;
            link.accepts = accepts;
        }
    }
    let uplink = links
        .iter()
        .flatten()
        .any(|l| !l.egress.is_empty() || !l.ingress.is_empty());
    Ok(Plan { links, pairs, uplink })
}

/// The ports that cross the microVM's boundary for a domain on `joined`, outward (egress)
/// or inward (ingress), each once: of each joined network that is not internal, those
/// both its own grants (`NETWORK --expose`, and `--egress` or `--ingress`) and the
/// microVM's for it (`EXPOSE ... FOR` it, both ways or `AS` that direction) open, where
/// their ranges meet. Two boundaries, and a flow crossing both needs both
/// (AGENTFILE_ARCH.md §12 answer 6); the build's `agentfile::boundary` reads them alike.
fn boundary_of(spec: &Value, joined: &[String], outward: bool) -> Result<Vec<Egress>, String> {
    let away = if outward { "ingress" } else { "egress" };
    let ranges = |ports: Vec<String>| -> Result<Vec<Egress>, String> {
        ports
            .iter()
            .map(|p| port_range(p).ok_or_else(|| format!("the port {p:?} is no port or range of ports")))
            .collect()
    };
    let mut out: Vec<Egress> = Vec::new();
    for n in spec.get("networks").map(Value::array).unwrap_or_default() {
        let Some(name) = n.get("name").and_then(Value::str) else {
            continue;
        };
        if matches!(n.get("internal"), Some(Value::Bool(true))) || !joined.iter().any(|j| j == name) {
            continue;
        }
        let ours = ranges(
            n.get("ports")
                .map(Value::array)
                .unwrap_or_default()
                .iter()
                .filter(|p| p.get("direction").and_then(Value::str) != Some(away))
                .filter_map(|p| p.get("port").and_then(Value::str).map(str::to_string))
                .collect(),
        )?;
        let mut vms = Vec::new();
        for e in spec.get("exposures").map(Value::array).unwrap_or_default() {
            if e.get("direction").and_then(Value::str) != Some(away)
                && strings(e.get("networks")).iter().any(|x| x == name)
            {
                vms.extend(ranges(strings(e.get("ports")))?);
            }
        }
        for &(p, lo, hi) in &ours {
            for &(q, a, b) in &vms {
                let (lo, hi) = (lo.max(a), hi.min(b));
                if p == q && lo <= hi && !out.contains(&(p, lo, hi)) {
                    out.push((p, lo, hi));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<Name> {
        n.iter().map(|n| (false, (*n).to_string())).collect()
    }

    #[test]
    fn members_reach_each_other_but_to_lets_its_targets_only_answer() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"back","subnets":[],"gateways":[]}],
                "connections":[{"kind":null,"from":["a"],"bothWays":false,"to":["b"],"on":["back"]},
                               {"kind":null,"from":["c"],"bothWays":true,"to":["c"],"on":["side"]},
                               {"kind":null,"from":["d"],"bothWays":true,"to":["d"],"on":["back"]}]}"#,
        )
        .unwrap();
        let p = plan(
            &spec,
            &names(&["a", "b", "c", "d"]),
            Some((Ipv4Addr::new(10, 244, 0, 0), 24)),
        )
        .unwrap();
        // a -> b granted; b -> a barred; d, on back, reaches and is reached by both.
        assert_eq!(p.pairs, vec![(0, 1), (0, 3), (1, 3), (3, 0), (3, 1)]);
        // The pool's first /24 is the microVM's own: back takes the next, side the one after.
        let a = p.links[0].as_ref().unwrap();
        assert_eq!(a.addresses[0].addr, Ipv4Addr::new(10, 244, 1, 2));
        assert_eq!(a.addresses[0].gateway, Ipv4Addr::new(10, 244, 1, 1));
        assert_eq!(
            p.links[2].as_ref().unwrap().addresses[0].addr,
            Ipv4Addr::new(10, 244, 2, 2)
        );
        assert_eq!(
            String::from_utf8(a.hosts.strip_prefix(HOSTS).unwrap().to_vec()).unwrap(),
            "10.244.1.2\ta\n10.244.1.3\tb\n10.244.1.4\td\n"
        );
        assert!(a.connects && a.accepts);
        let b = p.links[1].as_ref().unwrap();
        assert!(b.connects && b.accepts, "b reaches d, and a reaches it");
        // c is alone on side: linked, reaching no one.
        let c = p.links[2].as_ref().unwrap();
        assert!(!c.connects && !c.accepts);
    }

    #[test]
    fn a_domain_reaches_past_the_microvm_what_its_networks_grant() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"out","internal":false,"subnets":[],"gateways":[],
                             "ports":[{"port":"443","direction":"egress"},{"port":"8080","direction":"ingress"},
                                      {"port":"53/udp","direction":"both"}]},
                            {"name":"shut","internal":true,"subnets":[],"gateways":[],
                             "ports":[{"port":"22","direction":"egress"}]}],
                "exposures":[{"ports":["443","8080"],"direction":"both","networks":["out"]},
                             {"ports":["53/udp"],"direction":"egress","networks":["out"]},
                             {"ports":["9000-9010"],"direction":"egress","networks":["out"]},
                             {"ports":["22"],"direction":"both","networks":["shut"]}],
                "connections":[{"kind":null,"from":["a"],"bothWays":true,"to":["a"],"on":["out"]},
                               {"kind":null,"from":["b"],"bothWays":true,"to":["b"],"on":["shut"]}]}"#,
        )
        .unwrap();
        let p = plan(&spec, &names(&["a", "b"]), None).unwrap();
        let a = p.links[0].as_ref().unwrap();
        // 443 and 53/udp both boundaries open outward; not 8080 (the network's
        // ingress-only), nor 9000-9010 (the microVM's alone).
        assert_eq!(a.egress, vec![(6, 443, 443), (17, 53, 53)]);
        assert!(a.connects, "egress lets it connect");
        // 8080 both boundaries open inward, to out's one member.
        assert_eq!(a.ingress, vec![(6, 8080, 8080)]);
        assert!(a.accepts, "ingress lets it be connected to");
        let b = p.links[1].as_ref().unwrap();
        assert!(
            b.egress.is_empty(),
            "an internal network reaches nothing past the microVM"
        );
        assert!(!b.connects);
        assert!(p.uplink);
    }

    #[test]
    fn a_declared_subnet_and_gateway_are_kept() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"n","subnets":["172.30.0.0/29"],"gateways":["172.30.0.2"]}],
                "connections":[{"kind":"agent","from":["x"],"bothWays":true,"to":["y"],"on":["n"]}]}"#,
        )
        .unwrap();
        let p = plan(&spec, &names(&["x", "y"]), None).unwrap();
        let x = &p.links[0].as_ref().unwrap().addresses[0];
        let y = &p.links[1].as_ref().unwrap().addresses[0];
        assert_eq!(
            (x.addr, x.gateway, x.prefix),
            (Ipv4Addr::new(172, 30, 0, 3), Ipv4Addr::new(172, 30, 0, 2), 29)
        );
        assert_eq!(y.addr, Ipv4Addr::new(172, 30, 0, 4));
        assert_eq!(p.pairs, vec![(0, 1), (1, 0)]);
        // A /30 has room for one member.
        let small = crate::json::parse(
            br#"{"networks":[{"name":"n","subnets":["172.30.0.0/30"],"gateways":[]}],
                "connections":[{"kind":null,"from":["x"],"bothWays":true,"to":["y"],"on":["n"]}]}"#,
        )
        .unwrap();
        assert!(
            plan(&small, &names(&["x", "y"]), None)
                .unwrap_err()
                .contains("room for 1")
        );
    }
}
