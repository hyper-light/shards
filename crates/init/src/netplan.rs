//! The links between an image's agents and harnesses, as its normalized Agentfile's
//! `CONNECT`s grant them (docs/design/architecture.md D59; AGENTFILE_ARCH.md §4.6, §4.7,
//! §9.7): which networks each domain joins, its address on each, which domains it may
//! open connections to, and the names it resolves.
//!
//! On a network, its members, every domain a `CONNECT … ON` it names, may each reach the
//! others (§4.6), except that `CONNECT x TO y` lets `y` only answer `x` (§4.7), unless
//! another directive grants `y` to `x` outright. Each domain has one link (§9.7) and policy
//! goes by link, so a pair any shared network allows is allowed.

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

/// A domain's link: its addresses, the names it resolves (`/etc/hosts`), and whether it
/// opens connections and is connected to, which its Landlock rules follow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Link {
    pub addresses: Vec<Address>,
    pub hosts: Vec<u8>,
    pub connects: bool,
    pub accepts: bool,
}

/// The plan: each domain's link, by its index in `names`, and the pairs `(from, to)` that
/// may open connections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub links: Vec<Option<Link>>,
    pub pairs: Vec<(usize, usize)>,
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
        let connects = pairs.iter().any(|(x, _)| *x == d);
        let accepts = pairs.iter().any(|(_, y)| *y == d);
        if let Some(Some(link)) = links.get_mut(d) {
            link.hosts = [HOSTS, hosts.as_bytes()].concat();
            link.connects = connects;
            link.accepts = accepts;
        }
    }
    Ok(Plan { links, pairs })
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
