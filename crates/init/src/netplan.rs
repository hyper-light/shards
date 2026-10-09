//! The links between an image's agents and harnesses, as its normalized Agentfile's
//! `CONNECT`s grant them (docs/design/architecture.md D59; AGENTFILE_ARCH.md §4.6, §4.7,
//! §9.7): which networks each domain joins, its address on each, which domains it may
//! open connections to, and the names it resolves.
//!
//! Networks are default deny: every agent is airgapped unless configured otherwise. A
//! network's members are the domains a `CONNECT … ON` it names, and membership grants no
//! flow. A flow exists where a `CONNECT` names it: from each domain before `TO` to each
//! after it (both ways with `WITH`), on the `--port`s the receiver accepts, which the build
//! holds within what its network lets in (`agentfile::connections`). Each domain has one
//! link (§9.7) and policy goes by link. A domain resolves its own name and those of the
//! peers it is granted a flow to, and no other.
//!
//! Past the microVM, a domain may open flows to the ports its networks let cross both
//! boundaries (§4.1, §4.6, §12 answer 6): of each network it joins that is not internal,
//! those its own grants (`--egress`, `--expose`) and the microVM's for it (`EXPOSE ...
//! FOR` it, not ingress-only) both open, as the build's `agentfile::boundary` reads them.

use std::net::{Ipv4Addr, Ipv6Addr};

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

/// The block a network with IPv6 (`NETWORK --ipv6`) takes its subnet from where it names
/// none: /64s of a /48 of unique local addresses (RFC 4193), the first that overlaps
/// neither the microVM's own network's IPv6 subnet nor a declared one (D99). Its global ID
/// spells "Shar"; these networks never leave the microVM, so no other site's ULAs meet
/// them, and the one subnet they could meet, eth0's, is kept clear of.
const POOL6: (Ipv6Addr, u8) = (Ipv6Addr::new(0xfdf0, 0x5368, 0x6172, 0, 0, 0, 0, 0), 48);
const POOL6_PREFIX: u8 = 64;

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

/// A domain's IPv6 address on one network with IPv6 (D99).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address6 {
    pub network: String,
    pub addr: Ipv6Addr,
    pub subnet: Ipv6Addr,
    pub prefix: u8,
    pub gateway: Ipv6Addr,
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
    /// Its IPv6 addresses, on the networks it joins that have IPv6.
    pub addresses6: Vec<Address6>,
    pub hosts: Vec<u8>,
    pub egress: Vec<Egress>,
    /// The ports let in past the microVM to it: of networks it is the one member of,
    /// those both boundaries open inward (the build refuses several members).
    pub ingress: Vec<Egress>,
    /// Whether it may ask the microVM's resolver for names past the microVM: one of its
    /// networks, not internal, says so (`NETWORK --dns`).
    pub dns: bool,
    /// The remote MCP servers it is granted (§4.4, §9.6), each a host and a TCP port: it
    /// reaches each at that port, at the addresses the host resolves to, and may ask the
    /// resolver for each host's name alone (`agentdns`).
    pub mcp: Vec<(String, u16)>,
    pub connects: bool,
    pub accepts: bool,
    /// The Unix sockets it is granted (`CONNECT --port=unix:<name>`), each a network, a
    /// name and whether it may make one there (it receives) or only connect to one.
    pub unix: Vec<(String, String, bool)>,
}

/// A remote MCP server's host, lowered, and port, from its URL: the port written, or else
/// its scheme's own (443 for `https`, 80 for `http`; §4.4), as the build's
/// `agentfile::mcp_endpoint` reads it. None for a source that is no http(s) URL.
fn mcp_endpoint(url: &str) -> Option<(String, u16)> {
    let (rest, default) = match url.strip_prefix("https://") {
        Some(r) => (r, 443),
        None => (url.strip_prefix("http://")?, 80),
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (hostport, default),
    };
    (!host.is_empty() && port > 0).then(|| (host.to_ascii_lowercase(), port))
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
    /// Each flow granted: from one domain to another, on the ports the receiver accepts.
    pub pairs: Vec<(usize, usize, Vec<Egress>)>,
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

fn cidr6(s: &str) -> Option<(Ipv6Addr, u8)> {
    let (a, p) = s.split_once('/')?;
    let a: Ipv6Addr = a.parse().ok()?;
    let p: u8 = p.parse().ok().filter(|p| *p <= 126)?;
    Some((Ipv6Addr::from(u128::from(a) & mask6(p)), p))
}

fn mask6(prefix: u8) -> u128 {
    u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0)
}

fn overlaps6(a: (Ipv6Addr, u8), b: (Ipv6Addr, u8)) -> bool {
    let p = a.1.min(b.1);
    u128::from(a.0) & mask6(p) == u128::from(b.0) & mask6(p)
}

fn overlaps(a: (Ipv4Addr, u8), b: (Ipv4Addr, u8)) -> bool {
    let p = a.1.min(b.1);
    u32::from(a.0) & mask(p) == u32::from(b.0) & mask(p)
}

/// Plans the links of `names`, the domains that run, from `spec`, the normalized
/// Agentfile; `own` and `own6` are the microVM's own network's subnets, which no subnet
/// may overlap.
pub fn plan(
    spec: &Value,
    names: &[Name],
    own: Option<(Ipv4Addr, u8)>,
    own6: Option<(Ipv6Addr, u8)>,
) -> Result<Plan, String> {
    let index = |kind: Option<&str>, name: &str| -> Option<usize> {
        let find = |harness: bool| names.iter().position(|(h, n)| *h == harness && n == name);
        match kind {
            Some("agent") => find(false),
            Some("harness") => find(true),
            _ => find(false).or_else(|| find(true)),
        }
    };
    // Per network: its members, every domain a CONNECT on it names; and the flows granted,
    // each from one domain to another on the ports its CONNECT names, and nothing else:
    // networks are default deny, and membership grants no flow.
    let mut members: Vec<(String, Vec<usize>)> = Vec::new();
    let mut sockets: Vec<(usize, String, String, bool)> = Vec::new();
    let mut pairs: Vec<(usize, usize, Vec<Egress>)> = Vec::new();
    let mut grant = |x: usize, y: usize, ports: &[Egress]| {
        if x == y {
            return;
        }
        let at = match pairs.iter().position(|(a, b, _)| *a == x && *b == y) {
            Some(at) => at,
            None => {
                pairs.push((x, y, Vec::new()));
                pairs.len() - 1
            }
        };
        if let Some((_, _, have)) = pairs.get_mut(at) {
            for p in ports {
                if !have.contains(p) {
                    have.push(*p);
                }
            }
        }
    };
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
        let all = strings(c.get("ports"));
        let unix: Vec<&str> = all.iter().filter_map(|p| p.strip_prefix("unix:")).collect();
        let ports: Vec<Egress> = all
            .iter()
            .filter(|p| !p.starts_with("unix:"))
            .map(|p| port_range(p).ok_or_else(|| format!("CONNECT --port={p}: no port or range of ports")))
            .collect::<Result<_, _>>()?;
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
        }
        if !ports.is_empty() {
            for &x in &from {
                for &y in &to {
                    grant(x, y, &ports);
                    if both {
                        grant(y, x, &ports);
                    }
                }
            }
        }
        // Its Unix sockets: those after TO receive, making them; those before connect,
        // and with WITH receive too.
        for net in strings(c.get("on")) {
            for name in &unix {
                for (&d, receives) in from.iter().map(|d| (d, both)).chain(to.iter().map(|d| (d, true))) {
                    sockets.push((d, net.clone(), (*name).to_string(), receives));
                }
            }
        }
    }
    // The remote MCP servers each domain is granted: those its `FOR` names, or with none
    // every agent (no harness). One on no network gets a network of its own, of it alone,
    // which pairs it with no one.
    let mut mcp: Vec<Vec<(String, u16)>> = vec![Vec::new(); names.len()];
    for m in spec.get("mcp").map(Value::array).unwrap_or_default() {
        let Some(endpoint) = m.get("source").and_then(Value::str).and_then(mcp_endpoint) else {
            continue;
        };
        let scope = m.get("for");
        let kind = scope.and_then(|f| f.get("kind")).and_then(Value::str);
        let named = strings(scope.and_then(|f| f.get("names")));
        let to: Vec<usize> = if named.is_empty() {
            (0..names.len())
                .filter(|&i| names.get(i).is_some_and(|(h, _)| !h))
                .collect()
        } else {
            named.iter().filter_map(|n| index(kind, n)).collect()
        };
        for d in to {
            if let Some(grants) = mcp.get_mut(d)
                && !grants.contains(&endpoint)
            {
                grants.push(endpoint.clone());
            }
        }
    }
    for (d, grants) in mcp.iter().enumerate() {
        if !grants.is_empty() && !members.iter().any(|(_, m)| m.contains(&d)) {
            let name = names.get(d).map(|(_, n)| n.as_str()).unwrap_or_default();
            members.push((format!("mcp:{name}"), vec![d]));
        }
    }
    for (_, m) in &mut members {
        m.sort_unstable();
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
    // The networks with IPv6: those that say `--ipv6` (Compose's enable_ipv6), as dockerd
    // gives a network IPv6 only when asked, its IPv6 configs ignored otherwise.
    let with_ipv6: Vec<String> = spec
        .get("networks")
        .map(Value::array)
        .unwrap_or_default()
        .iter()
        .filter(|n| matches!(n.get("ipv6"), Some(Value::Bool(true))))
        .filter_map(|n| n.get("name").and_then(Value::str).map(str::to_string))
        .collect();
    let mut taken6: Vec<(Ipv6Addr, u8)> = own6.into_iter().collect();
    for (_, subnets, _) in &declared {
        taken6.extend(subnets.iter().filter_map(|s| cidr6(s)));
    }
    let mut next6 = 0u128;
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
        if !with_ipv6.contains(net) {
            continue;
        }
        // Its IPv6 subnet: as declared, else a /64 of the pool; its gateway as declared,
        // else the subnet's first host; members from ::2 up, past the gateway.
        let ipv6 = said.and_then(|(_, s, _)| s.iter().find_map(|s| cidr6(s)));
        let (subnet6, prefix6) = match ipv6 {
            Some(s) => s,
            None => loop {
                let span = 1u128 << (POOL6_PREFIX - POOL6.1);
                if next6 >= span {
                    return Err(format!(
                        "network {net}: no /64 of {}/{} is free",
                        POOL6.0, POOL6.1
                    ));
                }
                let base = u128::from(POOL6.0) | (next6 << (128 - u32::from(POOL6_PREFIX)));
                next6 += 1;
                let candidate = (Ipv6Addr::from(base), POOL6_PREFIX);
                if !taken6.iter().any(|t| overlaps6(*t, candidate)) {
                    taken6.push(candidate);
                    break candidate;
                }
            },
        };
        let base6 = u128::from(subnet6);
        let gateway6 = said
            .and_then(|(_, _, g)| g.iter().find_map(|g| g.parse::<Ipv6Addr>().ok()))
            .unwrap_or(Ipv6Addr::from(base6 + 1));
        let room6 = (1u128 << (128 - u32::from(prefix6))).saturating_sub(2);
        if m.len() as u128 > room6 {
            return Err(format!(
                "network {net}: {} members, room for {room6} of IPv6",
                m.len()
            ));
        }
        let mut host6 = base6 + 2;
        for &d in m {
            if Ipv6Addr::from(host6) == gateway6 {
                host6 += 1;
            }
            let link = links
                .get_mut(d)
                .ok_or("a member beyond the domains")?
                .get_or_insert_with(Link::default);
            link.addresses6.push(Address6 {
                network: net.clone(),
                addr: Ipv6Addr::from(host6),
                subnet: subnet6,
                prefix: prefix6,
                gateway: gateway6,
            });
            host6 += 1;
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
                // Itself, and the peers it is granted a flow to: no name of one it may not
                // reach.
                if named.contains(&peer)
                    || (peer != d && !pairs.iter().any(|(x, y, _)| *x == d && *y == peer))
                {
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
                    // Its IPv6 address on the network too, after its IPv4 one.
                    if let Some(a6) = links
                        .get(peer)
                        .and_then(Option::as_ref)
                        .and_then(|l| l.addresses6.iter().find(|x| x.network == a.network))
                    {
                        hosts.push_str(&format!("{}\t{name}\n", a6.addr));
                    }
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
        let dns = spec
            .get("networks")
            .map(Value::array)
            .unwrap_or_default()
            .iter()
            .any(|n| {
                n.get("name")
                    .and_then(Value::str)
                    .is_some_and(|name| joined.iter().any(|j| j == name))
                    && matches!(n.get("dns"), Some(Value::Bool(true)))
                    && !matches!(n.get("internal"), Some(Value::Bool(true)))
            });
        let granted = mcp.get(d).cloned().unwrap_or_default();
        let mut egress = egress;
        for (_, port) in &granted {
            if !egress.contains(&(6, *port, *port)) {
                egress.push((6, *port, *port));
            }
        }
        // Asking names connects too, by TCP for answers too long for UDP (RFC 7766): to its
        // resolver alone, which its gate lets it reach and nothing else.
        let connects = pairs.iter().any(|(x, _, _)| *x == d) || !egress.is_empty() || dns;
        let accepts = pairs.iter().any(|(_, y, _)| *y == d) || !ingress.is_empty();
        if let Some(Some(link)) = links.get_mut(d) {
            link.hosts = [HOSTS, hosts.as_bytes()].concat();
            link.egress = egress;
            link.ingress = ingress;
            link.dns = dns;
            link.mcp = granted;
            link.connects = connects;
            link.accepts = accepts;
            for (_, net, name, receives) in sockets.iter().filter(|s| s.0 == d) {
                match link.unix.iter_mut().find(|(n, m, _)| n == net && m == name) {
                    Some(have) => have.2 |= receives,
                    None => link.unix.push((net.clone(), name.clone(), *receives)),
                }
            }
        }
    }
    let uplink = links
        .iter()
        .flatten()
        .any(|l| !l.egress.is_empty() || !l.ingress.is_empty() || l.dns || !l.mcp.is_empty());
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

/// Who may send whom messages through the in-VM server (§12 answer 18, D60), by index in
/// `names`: each `CONNECT`'s agents to those after `TO`, and both ways with `WITH`; each
/// `ATTACH`'s harnesses to its agents. A domain the other may send to answers it.
pub fn channels(spec: &Value, names: &[Name]) -> Vec<(usize, usize)> {
    let index = |kind: Option<&str>, name: &str| -> Option<usize> {
        let find = |harness: bool| names.iter().position(|(h, n)| *h == harness && n == name);
        match kind {
            Some("agent") => find(false),
            Some("harness") => find(true),
            _ => find(false).or_else(|| find(true)),
        }
    };
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut edge = |x: usize, y: usize| {
        if x != y && !out.contains(&(x, y)) {
            out.push((x, y));
        }
    };
    for c in spec.get("connections").map(Value::array).unwrap_or_default() {
        let kind = c.get("kind").and_then(Value::str);
        let both = matches!(c.get("bothWays"), Some(Value::Bool(true)));
        let to: Vec<usize> = strings(c.get("to"))
            .iter()
            .filter_map(|n| index(kind, n))
            .collect();
        for x in strings(c.get("from")).iter().filter_map(|n| index(kind, n)) {
            for &y in &to {
                edge(x, y);
                if both {
                    edge(y, x);
                }
            }
        }
    }
    for a in spec.get("attachments").map(Value::array).unwrap_or_default() {
        let agents: Vec<usize> = strings(a.get("agents"))
            .iter()
            .filter_map(|n| index(Some("agent"), n))
            .collect();
        for h in strings(a.get("harnesses"))
            .iter()
            .filter_map(|n| index(Some("harness"), n))
        {
            for &x in &agents {
                edge(h, x);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<Name> {
        n.iter().map(|n| (false, (*n).to_string())).collect()
    }

    #[test]
    fn flows_are_what_connect_names_on_its_ports_and_no_more() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"back","subnets":[],"gateways":[]}],
                "connections":[{"kind":null,"from":["a"],"bothWays":false,"to":["b"],"on":["back"],"ports":["8080","53/udp"]},
                               {"kind":null,"from":["c"],"bothWays":true,"to":["c"],"on":["side"],"ports":[]},
                               {"kind":null,"from":["d"],"bothWays":true,"to":["b"],"on":["back"],"ports":["9000-9010"]},
                               {"kind":null,"from":["e"],"bothWays":true,"to":["e"],"on":["back"],"ports":[]}]}"#,
        )
        .unwrap();
        let p = plan(
            &spec,
            &names(&["a", "b", "c", "d", "e"]),
            Some((Ipv4Addr::new(10, 244, 0, 0), 24)),
            None,
        )
        .unwrap();
        // a -> b on its ports; d <-> b both ways on its; nothing for e, a member that no
        // CONNECT pairs, nor between a and d, members of one network.
        assert_eq!(
            p.pairs,
            vec![
                (0, 1, vec![(6, 8080, 8080), (17, 53, 53)]),
                (1, 3, vec![(6, 9000, 9010)]),
                (3, 1, vec![(6, 9000, 9010)]),
            ]
        );
        // The pool's first /24 is the microVM's own: back takes the next, side the one after.
        let a = p.links[0].as_ref().unwrap();
        assert_eq!(a.addresses[0].addr, Ipv4Addr::new(10, 244, 1, 2));
        assert_eq!(a.addresses[0].gateway, Ipv4Addr::new(10, 244, 1, 1));
        assert_eq!(
            p.links[2].as_ref().unwrap().addresses[0].addr,
            Ipv4Addr::new(10, 244, 2, 2)
        );
        // Its own name and b's, the one it may reach: not d's, nor e's.
        assert_eq!(
            String::from_utf8(a.hosts.strip_prefix(HOSTS).unwrap().to_vec()).unwrap(),
            "10.244.1.2\ta\n10.244.1.3\tb\n"
        );
        assert!(a.connects && !a.accepts);
        let b = p.links[1].as_ref().unwrap();
        assert!(b.connects && b.accepts, "b reaches d, and a and d reach it");
        // c alone, and e unpaired: linked, reaching no one, resolving only themselves.
        for alone in [2, 4] {
            let l = p.links[alone].as_ref().unwrap();
            assert!(!l.connects && !l.accepts);
            assert_eq!(
                l.hosts
                    .strip_prefix(HOSTS)
                    .unwrap()
                    .iter()
                    .filter(|&&c| c == b'\n')
                    .count(),
                1
            );
        }
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
        let p = plan(&spec, &names(&["a", "b"]), None, None).unwrap();
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

    /// A remote MCP server reaches the agents its FOR names, or every agent, at its port,
    /// one on no network given a network of its own; a harness is no agent.
    #[test]
    fn remote_mcp_servers_reach_whom_they_are_for() {
        let spec = crate::json::parse(
            br#"{"networks":[],"connections":[],
                "mcp":[{"name":"web","source":"https://MCP.example/sse","for":{"kind":null,"names":["a"]}},
                       {"name":"all","source":"http://tools.example:8080","for":{"kind":null,"names":[]}},
                       {"name":"local","source":"./files","for":{"kind":null,"names":[]}}]}"#,
        )
        .unwrap();
        let mut n = names(&["a", "b"]);
        n.push((true, "h".to_string()));
        let p = plan(&spec, &n, None, None).unwrap();
        let a = p.links[0].as_ref().unwrap();
        assert_eq!(
            a.mcp,
            vec![
                ("mcp.example".to_string(), 443),
                ("tools.example".to_string(), 8080)
            ]
        );
        assert_eq!(a.egress, vec![(6, 443, 443), (6, 8080, 8080)]);
        assert_eq!(a.addresses[0].network, "mcp:a");
        let b = p.links[1].as_ref().unwrap();
        assert_eq!(b.mcp, vec![("tools.example".to_string(), 8080)]);
        assert!(p.links[2].is_none(), "a harness is given no agent's server");
        assert!(p.pairs.is_empty() && p.uplink);
    }

    #[test]
    fn a_declared_subnet_and_gateway_are_kept() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"n","subnets":["172.30.0.0/29"],"gateways":["172.30.0.2"]}],
                "connections":[{"kind":"agent","from":["x"],"bothWays":true,"to":["y"],"on":["n"],"ports":["80"]}]}"#,
        )
        .unwrap();
        let p = plan(&spec, &names(&["x", "y"]), None, None).unwrap();
        let x = &p.links[0].as_ref().unwrap().addresses[0];
        let y = &p.links[1].as_ref().unwrap().addresses[0];
        assert_eq!(
            (x.addr, x.gateway, x.prefix),
            (Ipv4Addr::new(172, 30, 0, 3), Ipv4Addr::new(172, 30, 0, 2), 29)
        );
        assert_eq!(y.addr, Ipv4Addr::new(172, 30, 0, 4));
        assert_eq!(
            p.pairs,
            vec![(0, 1, vec![(6, 80, 80)]), (1, 0, vec![(6, 80, 80)])]
        );
        // A /30 has room for one member.
        let small = crate::json::parse(
            br#"{"networks":[{"name":"n","subnets":["172.30.0.0/30"],"gateways":[]}],
                "connections":[{"kind":null,"from":["x"],"bothWays":true,"to":["y"],"on":["n"]}]}"#,
        )
        .unwrap();
        assert!(
            plan(&small, &names(&["x", "y"]), None, None)
                .unwrap_err()
                .contains("room for 1")
        );
    }

    /// A network with IPv6 (D99): its members' IPv6 addresses from ::2 up, of the subnet
    /// declared or else the pool's first /64 clear of the microVM's own, its gateway the
    /// subnet's first host or the one declared; each names its granted peers at both
    /// addresses; a network without `--ipv6` takes none, its IPv6 subnet ignored.
    #[test]
    fn networks_with_ipv6_give_their_members_ipv6_addresses() {
        let spec = crate::json::parse(
            br#"{"networks":[{"name":"six","ipv6":true,"subnets":["fd31:3::/64"],"gateways":["fd31:3::fe"]},
                             {"name":"pooled","ipv6":true,"subnets":[],"gateways":[]},
                             {"name":"four","ipv6":false,"subnets":["fd99::/64"],"gateways":[]}],
                "connections":[{"kind":null,"from":["a"],"bothWays":false,"to":["b"],"on":["six"],"ports":["7000"]},
                               {"kind":null,"from":["c"],"bothWays":true,"to":["c"],"on":["pooled"],"ports":[]},
                               {"kind":null,"from":["d"],"bothWays":true,"to":["d"],"on":["four"],"ports":[]}]}"#,
        )
        .unwrap();
        let own6 = Some((Ipv6Addr::new(0xfdf0, 0x5368, 0x6172, 0, 0, 0, 0, 0), 64));
        let p = plan(&spec, &names(&["a", "b", "c", "d"]), None, own6).unwrap();
        let six = |i: usize| p.links[i].as_ref().unwrap().addresses6.clone();
        assert_eq!(six(0)[0].addr, "fd31:3::2".parse::<Ipv6Addr>().unwrap());
        assert_eq!(six(1)[0].addr, "fd31:3::3".parse::<Ipv6Addr>().unwrap());
        assert_eq!(six(0)[0].gateway, "fd31:3::fe".parse::<Ipv6Addr>().unwrap());
        // The pool's first /64 is the microVM's own: pooled takes the next.
        assert_eq!(
            six(2)[0].subnet,
            "fdf0:5368:6172:1::".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(
            six(2)[0].gateway,
            "fdf0:5368:6172:1::1".parse::<Ipv6Addr>().unwrap()
        );
        assert!(six(3).is_empty());
        let hosts = String::from_utf8(p.links[0].as_ref().unwrap().hosts.clone()).unwrap();
        assert!(
            hosts.ends_with("10.244.0.2\ta\nfd31:3::2\ta\n10.244.0.3\tb\nfd31:3::3\tb\n"),
            "{hosts}"
        );
    }
}
