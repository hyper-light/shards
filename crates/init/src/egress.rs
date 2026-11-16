//! A member's own grants past the microVM (D278): what its image grants it, what its host
//! gives it of its own as the run starts (`member-egress=`) and as it goes
//! (`builtin::EGRESS`), and the most its host may give it past its image
//! (`member-ceiling=`). Each configuration of them is held as D236's `held` holds an image's
//! (the network process holds the whole microVM to the union of its members' names on a
//! port), and a change is programmed whole, in this order: the member's gate, so that no new
//! flow opens on what it lost; its open TCP flows on those ports reset (sock_diag's
//! SOCK_DESTROY: the guest kernel has no ctnetlink, and its RST still crosses the old rules,
//! so the network process lets its host side go too); the switch's tables and init's own;
//! then its resolver.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Mutex;

use shards_abi::run::egress::{self as abi, Grants};

use crate::netplan::{Egress, Link};

/// A member's grants past the microVM: what its image grants it, the most its host may give
/// it past that, and what its host gives it as they stand, whole, where it has given any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Granted {
    pub image: Grants,
    pub ceiling: Grants,
    pub given: Option<Grants>,
}

impl Granted {
    /// What it reaches: what its host gave it, whole, in place of its image's grants, which
    /// it reaches until its host gives it any; so its host takes back what its image grants
    /// as it gives what its ceiling lets it.
    pub fn reach(&self) -> Grants {
        self.given.clone().unwrap_or_else(|| self.image.clone())
    }
}

/// Whether `ranges` cover every port of `range`, of its protocol.
pub fn covers(ranges: &[Egress], (proto, lo, hi): Egress) -> bool {
    let mut of: Vec<(u16, u16)> = ranges
        .iter()
        .filter(|r| r.0 == proto)
        .map(|r| (r.1, r.2))
        .collect();
    of.sort_unstable();
    // The first port of `range` none of them covers yet.
    let mut next = u32::from(lo);
    for (a, b) in of {
        if u32::from(a) > next {
            break;
        }
        next = next.max(u32::from(b) + 1);
    }
    next > u32::from(hi)
}

/// The ports of `before`'s TCP ranges that `after`'s do not cover, as ranges.
pub fn lost(before: &[Egress], after: &[Egress]) -> Vec<(u16, u16)> {
    let mut kept: Vec<(u32, u32)> = after
        .iter()
        .filter(|r| r.0 == 6)
        .map(|r| (u32::from(r.1), u32::from(r.2)))
        .collect();
    kept.sort_unstable();
    let mut out: Vec<(u16, u16)> = Vec::new();
    for &(proto, lo, hi) in before {
        if proto != 6 {
            continue;
        }
        let (mut from, hi) = (u32::from(lo), u32::from(hi));
        for &(a, b) in &kept {
            if b < from || a > hi {
                continue;
            }
            if a > from {
                out.extend(gap(from, a - 1));
            }
            from = from.max(b + 1);
        }
        if from <= hi {
            out.extend(gap(from, hi));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Ports `lo` to `hi` as a range, where they are ports.
fn gap(lo: u32, hi: u32) -> Option<(u16, u16)> {
    Some((u16::try_from(lo).ok()?, u16::try_from(hi).ok()?))
}

/// Every port `g` opens past the microVM: its port grants, and its destinations' TCP ports.
pub fn opened(g: &Grants) -> Vec<Egress> {
    let mut out = g.ports.clone();
    for (_, port) in &g.named {
        if !out.contains(&(6, *port, *port)) {
            out.push((6, *port, *port));
        }
    }
    out
}

/// Whether `g` asks the agents' resolver: for any name, or its destinations' (as a link's
/// gate lets its questions through, `domains::gate_of`).
pub fn asks(g: &Grants) -> bool {
    g.dns || !g.named.is_empty()
}

/// The first item of `asked` past what `image` and `ceiling` grant together; none where every
/// one is within them: a port range their port grants cover, a destination either names or
/// whose TCP port either grants to any address, and names past the microVM where either grants
/// them.
pub fn past(asked: &Grants, image: &Grants, ceiling: &Grants) -> Option<String> {
    let wide: Vec<Egress> = image.ports.iter().chain(&ceiling.ports).copied().collect();
    let proto = |p: u8| if p == 17 { "UDP" } else { "TCP" };
    for &range in &asked.ports {
        if !covers(&wide, range) {
            return Some(format!("{} ports {}-{}", proto(range.0), range.1, range.2));
        }
    }
    for (host, port) in &asked.named {
        let named = image
            .named
            .iter()
            .chain(&ceiling.named)
            .any(|(h, p)| h == host && p == port);
        if !named && !covers(&wide, (6, *port, *port)) {
            return Some(format!("{host}:{port}"));
        }
    }
    if asked.dns && !image.dns && !ceiling.dns {
        return Some("names past the microVM (dns)".into());
    }
    None
}

/// The first two members whose reach the microVM cannot hold apart, as the build's
/// `agentfile::held` refuses an image (D236), by their labels and what each reaches; none
/// where it can. On a TCP port one member reaches by name, every member granted the port must
/// reach it by the same names, none by a port grant to any address beside them: the network
/// process holds the whole microVM to the union of its members' names on a port, and each
/// member's init lets it out on that port to any address. `reach` is each member's identity,
/// label and grants.
pub fn unheld(reach: &[(usize, &str, Grants)]) -> Option<String> {
    let mut ports: Vec<u16> = reach
        .iter()
        .flat_map(|(_, _, g)| g.named.iter().map(|(_, p)| *p))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    for port in ports {
        // What each member granted the port reaches there: any address, or these names.
        let at: Vec<(usize, &str, Option<Vec<&str>>)> = reach
            .iter()
            .filter_map(|(id, label, g)| {
                if g.ports
                    .iter()
                    .any(|&(p, lo, hi)| p == 6 && lo <= port && port <= hi)
                {
                    return Some((*id, *label, None));
                }
                let mut names: Vec<&str> = g
                    .named
                    .iter()
                    .filter(|(_, p)| *p == port)
                    .map(|(h, _)| h.as_str())
                    .collect();
                names.sort_unstable();
                (!names.is_empty()).then_some((*id, *label, Some(names)))
            })
            .collect();
        let Some((by_name, label, names)) = at
            .iter()
            .find_map(|(id, label, n)| n.as_ref().map(|n| (*id, *label, n)))
        else {
            continue;
        };
        let what = |reached: Option<&Vec<&str>>| match reached {
            None => "any address there, by a port grant".to_string(),
            Some(names) => names.join(", "),
        };
        if let Some((_, other, theirs)) = at
            .iter()
            .find(|(id, _, theirs)| *id != by_name && theirs.as_ref() != Some(names))
        {
            return Some(format!(
                "TCP port {port} past the microVM: {label} would reach {} and {other} {}, which the microVM's network process cannot hold apart on one port (D236)",
                what(Some(names)),
                what(theirs.as_ref()),
            ));
        }
    }
    None
}

/// What a link reaches past the microVM, as its grants: its port grants to any address, its
/// destinations and whether it asks any name.
pub fn of_link(link: &Link) -> Grants {
    Grants {
        ports: link.wide.clone(),
        named: link.named.clone(),
        dns: link.dns,
    }
}

/// `given` made `link`'s reach, whole, in place of its image's: its port grants, its
/// destinations (each one's TCP port opened) and its names; the host itself in its hosts file
/// at `gateway` where a destination names it (D236). Its TCP connections Landlock leaves to its
/// gate where it may open any.
fn reach_as(link: &mut Link, given: &Grants, gateway: Option<Ipv4Addr>) {
    link.wide = given.ports.clone();
    link.named = given.named.clone();
    link.dns = given.dns;
    link.egress = opened(given);
    link.connects |= !link.egress.is_empty() || link.dns;
    hosts_the_host(link, given, gateway);
}

/// The host itself in `link`'s hosts file at `gateway`, once, where `g` names it (D236): its
/// name is never asked of a resolver.
fn hosts_the_host(link: &mut Link, g: &Grants, gateway: Option<Ipv4Addr>) {
    let Some(gateway) = gateway else { return };
    if !g.named.iter().any(|(h, _)| h == shards_dns::HOST) {
        return;
    }
    let line = format!("{gateway}\t{}\n", shards_dns::HOST);
    if !link.hosts.windows(line.len()).any(|w| w == line.as_bytes()) {
        link.hosts.extend_from_slice(line.as_bytes());
    }
}

/// Each member's grants as its run starts (D278), into the links `links` plans, by domain:
/// its image's, as planned; the most its host may give it past them (`member-ceiling=`); and
/// what its host gives it (`member-egress=`), within those two, its link's reach whole in
/// place of its image's, the members so granted held apart.
/// `members` is each domain's member index, `labels` its label and `outward` whether its link
/// joins a network that is not internal; `gateway`, eth0's. A member its link lets past the
/// microVM may be given a ceiling: its link then has its way past the microVM from the start
/// (its resolver, its hosts file's entry for the host itself, its TCP connections), which its
/// gate holds shut until a grant opens it. Refused, saying why: a ceiling or a grant for a
/// member of no such link or of none, a grant past its ceiling, a second entry for one member,
/// and members the microVM cannot hold apart.
pub fn at_start(
    links: &mut [Option<Link>],
    members: &[u32],
    labels: &[String],
    outward: &[bool],
    setup: &[Vec<u8>],
    gateway: Option<Ipv4Addr>,
) -> Result<(), String> {
    let ceilings = abi::of(setup, abi::CEILING_KEY).map_err(|e| format!("member-ceiling: {e}"))?;
    let given = abi::of(setup, abi::EGRESS_KEY).map_err(|e| format!("member-egress: {e}"))?;
    let at = |links: &[Option<Link>], member: u32, what: &str| -> Result<usize, String> {
        let d = members
            .iter()
            .position(|&m| m == member)
            .ok_or_else(|| format!("{what} for member {member}, which the image does not start"))?;
        match (links.get(d), outward.get(d)) {
            (Some(Some(_)), Some(true)) => Ok(d),
            _ => Err(format!(
                "{what} for {}, which no network of its image lets past the microVM",
                labels.get(d).map_or("a member", String::as_str)
            )),
        }
    };
    for (member, ceiling) in &ceilings {
        let d = at(links, *member, "a ceiling past the microVM")?;
        if let Some(Some(link)) = links.get_mut(d) {
            link.granted.ceiling = ceiling.clone();
            // Its way past the microVM from the start, its gate shut until a grant opens it.
            link.connects |= !ceiling.ports.is_empty() || !ceiling.named.is_empty() || ceiling.dns;
            hosts_the_host(link, ceiling, gateway);
        }
    }
    for link in links.iter_mut().flatten() {
        link.granted.image = of_link(link);
    }
    for (member, g) in &given {
        let d = at(links, *member, "a grant past the microVM")?;
        let label = labels.get(d).map_or("a member", String::as_str);
        if let Some(Some(link)) = links.get_mut(d) {
            if let Some(item) = past(g, &link.granted.image, &link.granted.ceiling) {
                return Err(format!(
                    "{label}: {item} is past what its image and its ceiling let it reach"
                ));
            }
            link.granted.given = Some(g.clone());
            reach_as(link, g, gateway);
        }
    }
    // The image's own configuration was held apart at its build and as its host read it
    // (`agentfile::held`): one its grants make is held here.
    if given.is_empty() {
        return Ok(());
    }
    let reach: Vec<(usize, &str, Grants)> = links
        .iter()
        .enumerate()
        .filter_map(|(d, l)| {
            let l = l.as_ref()?;
            Some((d, labels.get(d).map_or("a member", String::as_str), of_link(l)))
        })
        .collect();
    match unheld(&reach) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// Whether `link` has a way past the microVM to be given as its run goes: a ceiling.
pub fn may_widen(link: &Link) -> bool {
    let c = &link.granted.ceiling;
    !c.ports.is_empty() || !c.named.is_empty() || c.dns
}

/// A member linked as its run started, kept for its grants' changes as the run goes.
struct Linked {
    member: u32,
    /// Its domain's index: its link's and its gate's.
    domain: usize,
    label: String,
    granted: Granted,
    own: Vec<Ipv4Addr>,
    own6: Vec<Ipv6Addr>,
    /// Its network namespace, where its flows are.
    ns: OwnedFd,
    /// The microVM's own subnets, which no flow past it goes to (its gate's).
    subnets: Vec<(Ipv4Addr, u8)>,
    subnets6: Vec<(Ipv6Addr, u8)>,
}

/// Every member linked in this run, for [`change`].
static LINKED: Mutex<Vec<Linked>> = Mutex::new(Vec::new());

fn linked_now() -> std::sync::MutexGuard<'static, Vec<Linked>> {
    LINKED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keeps member `member`, domain `domain`, linked as `link` and `gate` say, its network
/// namespace `ns`, for its grants' changes as the run goes.
pub fn linked(member: u32, domain: usize, label: &str, link: &Link, gate: &crate::links::Gate, ns: OwnedFd) {
    linked_now().push(Linked {
        member,
        domain,
        label: label.to_string(),
        granted: link.granted.clone(),
        own: gate.own.clone(),
        own6: gate.own6.clone(),
        ns,
        subnets: gate.subnets.clone(),
        subnets6: gate.subnets6.clone(),
    });
}

/// Why a change of a member's grants was refused, by a cause its host tells apart
/// (`builtin::EGRESS_*`).
#[derive(Debug)]
pub enum Refused {
    /// No member of this index runs linked past anything in this run.
    Unknown(u32),
    /// A grant past what its image and its ceiling let it reach.
    Past(String),
    /// The members, so granted, the microVM cannot hold apart (D236).
    Unheld(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(member) => write!(f, "member {member} runs linked to nothing in this run"),
            Refused::Past(why) | Refused::Unheld(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Refused {}

/// `builtin::EGRESS`: member `args` names given the grants they say, whole, in place of its
/// reach as it stood, its image's or what it was given last (D278). Within what its image and
/// its ceiling let it reach, every member's reach held apart as D236 holds an image's;
/// programmed gate first, its lost TCP flows reset, then the switch and init's tables, then its
/// resolver. Refused, typed ([`Refused`]).
pub fn change(args: &[Vec<u8>]) -> io::Result<()> {
    let (member, asked) =
        abi::parse_args(args).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let unknown = || io::Error::other(Refused::Unknown(member));
    let switch = crate::domains::SWITCH.get().ok_or_else(unknown)?;
    let mut all = linked_now();
    let at = all.iter().position(|l| l.member == member).ok_or_else(unknown)?;
    let l = all.get(at).ok_or_else(unknown)?;
    if let Some(item) = past(&asked, &l.granted.image, &l.granted.ceiling) {
        return Err(io::Error::other(Refused::Past(format!(
            "{}: {item} is past what its image and its ceiling let it reach",
            l.label
        ))));
    }
    let before = l.granted.reach();
    let after = asked.clone();
    let reach: Vec<(usize, &str, Grants)> = all
        .iter()
        .map(|x| {
            let g = if x.member == member {
                after.clone()
            } else {
                x.granted.reach()
            };
            (x.domain, x.label.as_str(), g)
        })
        .collect();
    if let Some(why) = unheld(&reach) {
        return Err(io::Error::other(Refused::Unheld(why)));
    }
    let egress = opened(&after);
    let dns = asks(&after);
    // Its gate first: no new flow opens on what it lost.
    switch.regate_domain(l.domain, &egress, dns)?;
    // Its open TCP flows on those ports reset, their RSTs crossing the rules still there.
    let lost = lost(&opened(&before), &egress);
    if !lost.is_empty() {
        reset(&l.ns, &lost, &l.subnets, &l.subnets6)?;
    }
    switch.regrant(l.domain, &l.own, &l.own6, &egress, dns)?;
    resolver(switch, l, &after)?;
    if let Some(l) = all.get_mut(at) {
        l.granted.given = Some(asked);
    }
    Ok(())
}

/// The agents' resolver told what member `l` may ask now, `g`'s names: any, or its
/// destinations' but the host itself's, which its hosts file holds (D236); started for it
/// where it runs for no one yet.
fn resolver(switch: &crate::links::Switch, l: &Linked, g: &Grants) -> io::Result<()> {
    let asker = crate::agentdns::Asker {
        link: crate::links::link_index(l.domain)?,
        any: g.dns,
        names: g
            .named
            .iter()
            .map(|(h, _)| h.clone())
            .filter(|h| h != shards_dns::HOST)
            .collect(),
        addrs: l.own.clone(),
        local: Vec::new(),
    };
    if crate::agentdns::running() {
        return crate::agentdns::admit(&asker);
    }
    if !asker.any && asker.names.is_empty() {
        return Ok(());
    }
    let (_, _, gateway) = crate::net::current()
        .ok_or_else(|| io::Error::other("the microVM has no address for its agents' resolver"))?;
    let sockets = switch.resolver_sockets(std::net::SocketAddr::from((gateway, 53)))?;
    crate::agentdns::start(sockets, vec![asker]).map_err(io::Error::other)
}

/// TCP states a flow past the microVM may be in to be reset: every one but listening, closed
/// and TIME_WAIT's (include/net/tcp_states.h: ESTABLISHED 1 to CLOSING 11).
const RESET_STATES: u32 =
    (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 8) | (1 << 9) | (1 << 11);
/// sock_diag's request to end one socket (linux/sock_diag.h), as `ss -K` asks it.
const SOCK_DESTROY: u16 = 21;
/// NLM_F_ACK (linux/netlink.h).
const NLM_F_ACK: u16 = 4;

/// Resets every TCP flow in network namespace `ns` to a port of `lost` at an address past the
/// microVM (none of `subnets`, `subnets6` or loopback), by sock_diag's SOCK_DESTROY
/// (CONFIG_INET_DIAG_DESTROY): its process reads ECONNABORTED, its peer an RST. A socket that
/// ended meanwhile is no failure.
fn reset(
    ns: &OwnedFd,
    lost: &[(u16, u16)],
    subnets: &[(Ipv4Addr, u8)],
    subnets6: &[(Ipv6Addr, u8)],
) -> io::Result<()> {
    crate::links::in_netns(Some(ns.as_raw_fd()), || {
        let ask = crate::hostbind::diag_socket()?;
        let mut found: Vec<(u8, [u8; 48])> = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            found.extend(dump(&ask, family)?.into_iter().filter_map(|m| {
                let (family, id) = flow(&m)?;
                past_the_microvm(family, &id, lost, subnets, subnets6).then_some((family, id))
            }));
        }
        for (family, id) in found {
            match crate::links::exchange(
                &ask,
                &[(
                    SOCK_DESTROY,
                    crate::hostbind::NLM_F_REQUEST | NLM_F_ACK,
                    destroy_request(family, &id),
                )],
            ) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })
}

/// A sock_diag request for `family`'s TCP sockets in [`RESET_STATES`]: its nlmsghdr, then
/// inet_diag_req_v2 (family, protocol, extensions, pad, states, a zeroed socket ID).
fn dump_request(family: u8) -> Vec<u8> {
    let mut req = Vec::with_capacity(72);
    req.extend_from_slice(&72u32.to_ne_bytes());
    req.extend_from_slice(&crate::hostbind::SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req.extend_from_slice(&(crate::hostbind::NLM_F_REQUEST | crate::hostbind::NLM_F_DUMP).to_ne_bytes());
    req.extend_from_slice(&[0; 8]);
    req.extend_from_slice(&[family, libc::IPPROTO_TCP as u8, 0, 0]);
    req.extend_from_slice(&RESET_STATES.to_ne_bytes());
    req.extend_from_slice(&[0; 48]);
    req
}

/// SOCK_DESTROY's inet_diag_req_v2 for the socket `id` names exactly (its ports, addresses,
/// interface and cookie, as its dump said them).
fn destroy_request(family: u8, id: &[u8; 48]) -> Vec<u8> {
    let mut req = Vec::with_capacity(56);
    req.extend_from_slice(&[family, libc::IPPROTO_TCP as u8, 0, 0]);
    req.extend_from_slice(&u32::MAX.to_ne_bytes());
    req.extend_from_slice(id);
    req
}

/// Every message of a dump of `family`'s TCP sockets on `ask`, each whole.
fn dump(ask: &OwnedFd, family: u8) -> io::Result<Vec<Vec<u8>>> {
    let req = dump_request(family);
    // SAFETY: send(2) of a buffer of ours, to the kernel.
    if unsafe { libc::send(ask.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        let n = crate::chooser::whole(ask, &mut buf)
            .ok_or_else(|| io::Error::other("sock_diag's dump ended unfinished"))?;
        let mut msgs = buf.get(..n).unwrap_or_default();
        while let Some(len) = msgs
            .get(..4)
            .and_then(|l| <[u8; 4]>::try_from(l).ok())
            .map(u32::from_ne_bytes)
            .and_then(|l| usize::try_from(l).ok())
            .filter(|&l| l >= 16)
        {
            let which = msgs
                .get(4..6)
                .and_then(|k| <[u8; 2]>::try_from(k).ok())
                .map(u16::from_ne_bytes);
            match which {
                Some(crate::hostbind::NLMSG_DONE) => return Ok(out),
                Some(crate::hostbind::NLMSG_ERROR) => {
                    // nlmsgerr's error, negated (linux/netlink.h).
                    let errno = msgs
                        .get(16..20)
                        .and_then(|e| <[u8; 4]>::try_from(e).ok())
                        .map_or(libc::EIO, |e| i32::from_ne_bytes(e).saturating_neg());
                    return Err(io::Error::from_raw_os_error(errno));
                }
                _ => out.extend(msgs.get(..len).map(<[u8]>::to_vec)),
            }
            msgs = msgs.get(len.next_multiple_of(4)..).unwrap_or_default();
        }
    }
}

/// One dumped socket's family and inet_diag_sockid: after its nlmsghdr (16), its family,
/// state, timer and retransmits (4), then the ID's 48 bytes.
fn flow(m: &[u8]) -> Option<(u8, [u8; 48])> {
    let family = *m.get(16)?;
    let id = <[u8; 48]>::try_from(m.get(20..68)?).ok()?;
    Some((family, id))
}

/// Whether the socket `id` names is a flow to a port of `lost` at an address past the
/// microVM: none of its own subnets, nor loopback. The ID: its source port and destination
/// port, each big-endian, then its source and destination addresses, 16 bytes each, IPv4's in
/// their first 4.
fn past_the_microvm(
    family: u8,
    id: &[u8; 48],
    lost: &[(u16, u16)],
    subnets: &[(Ipv4Addr, u8)],
    subnets6: &[(Ipv6Addr, u8)],
) -> bool {
    let (Some(&[a, b]), Some(dst)) = (id.get(2..4), id.get(20..36)) else {
        return false;
    };
    let port = u16::from_be_bytes([a, b]);
    if !lost.iter().any(|&(lo, hi)| lo <= port && port <= hi) {
        return false;
    }
    if family == libc::AF_INET as u8 {
        let Some(a) = dst
            .get(..4)
            .and_then(|a| <[u8; 4]>::try_from(a).ok())
            .map(Ipv4Addr::from)
        else {
            return false;
        };
        !a.is_loopback() && !subnets.iter().any(|&(net, prefix)| within(a, net, prefix))
    } else {
        let Some(a) = <[u8; 16]>::try_from(dst).ok().map(Ipv6Addr::from) else {
            return false;
        };
        if let Some(v4) = a.to_ipv4_mapped() {
            return !v4.is_loopback() && !subnets.iter().any(|&(net, prefix)| within(v4, net, prefix));
        }
        !a.is_loopback() && !subnets6.iter().any(|&(net, prefix)| within6(a, net, prefix))
    }
}

fn within(a: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(a) & mask == u32::from(net) & mask
}

fn within6(a: Ipv6Addr, net: Ipv6Addr, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    u128::from(a) & mask == u128::from(net) & mask
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn g(ports: &[Egress], named: &[(&str, u16)], dns: bool) -> Grants {
        Grants {
            ports: ports.to_vec(),
            named: named.iter().map(|(h, p)| (h.to_string(), *p)).collect(),
            dns,
        }
    }

    /// Ranges cover a range of their protocol across their joins, never across a gap or of
    /// the other protocol; what a grant loses is each TCP port of it the next does not cover.
    #[test]
    fn ranges_cover_and_lose_by_the_port() {
        let r = [(6, 80, 90), (6, 91, 100), (17, 53, 53), (6, 443, 443)];
        assert!(covers(&r, (6, 85, 100)));
        assert!(covers(&r, (6, 443, 443)));
        assert!(!covers(&r, (6, 85, 101)));
        assert!(!covers(&r, (17, 80, 80)));
        assert!(!covers(&r, (6, 442, 443)));
        assert!(covers(&[(6, 1, 65535)], (6, 65535, 65535)));
        assert_eq!(
            lost(&[(6, 80, 100), (17, 53, 53)], &[(6, 85, 90)]),
            vec![(80, 84), (91, 100)]
        );
        assert_eq!(lost(&[(6, 443, 443)], &[(6, 1, 65535)]), vec![]);
        assert_eq!(lost(&[(6, 1, 65535)], &[]), vec![(1, 65535)]);
        assert_eq!(lost(&[(6, 10, 20)], &[(6, 10, 10), (6, 20, 20)]), vec![(11, 19)]);
    }

    /// A member's own grants within its image's and its ceiling's pass; a port, a destination
    /// or names past both are refused, naming the first; a destination whose port a port grant
    /// opens to any address is within it.
    #[test]
    fn a_grant_is_held_within_its_image_and_its_ceiling() {
        let image = g(&[(6, 443, 443)], &[("api.example.com", 8443)], false);
        let ceiling = g(&[(17, 5000, 5010)], &[("tools.example", 7000)], true);
        let within = g(
            &[(6, 443, 443), (17, 5001, 5002)],
            &[("tools.example", 7000), ("other.example", 443)],
            true,
        );
        assert_eq!(past(&within, &image, &ceiling), None);
        assert_eq!(
            past(&g(&[(6, 80, 80)], &[], false), &image, &ceiling),
            Some("TCP ports 80-80".into())
        );
        assert_eq!(
            past(&g(&[], &[("evil.example", 7000)], false), &image, &ceiling),
            Some("evil.example:7000".into())
        );
        assert_eq!(
            past(&g(&[], &[], true), &image, &Grants::default()),
            Some("names past the microVM (dns)".into())
        );
        assert_eq!(
            past(&Grants::default(), &Grants::default(), &Grants::default()),
            None
        );
    }

    /// Members reaching one TCP port alike pass, by the same names or each by a port grant;
    /// one by name beside another by other names, or beside one by a port grant, cannot be held
    /// apart and is refused naming both; names on other ports, or UDP grants, are no matter.
    #[test]
    fn members_are_held_apart_as_an_images_are() {
        let names = g(&[], &[("api.example.com", 443)], false);
        let wide = g(&[(6, 1, 1000)], &[], false);
        assert_eq!(unheld(&[(0, "a", names.clone()), (1, "b", names.clone())]), None);
        assert_eq!(unheld(&[(0, "a", wide.clone()), (1, "b", wide.clone())]), None);
        assert_eq!(
            unheld(&[
                (0, "a", names.clone()),
                (1, "b", g(&[(17, 443, 443)], &[("x.example", 80)], false))
            ]),
            None
        );
        let other = g(&[], &[("chatgpt.com", 443)], false);
        let why = unheld(&[(0, "a", names.clone()), (1, "b", other)]).unwrap();
        assert!(
            why.contains("TCP port 443") && why.contains("a would reach api.example.com"),
            "{why}"
        );
        let why = unheld(&[(0, "a", names), (1, "b", wide)]).unwrap();
        assert!(why.contains("b any address there, by a port grant"), "{why}");
    }

    /// A dump asks for TCP's sockets in every state a flow may be reset in; a destroy names
    /// the dumped socket exactly; a flow is past the microVM where its port is lost and its
    /// destination none of the microVM's subnets nor loopback, of either family.
    #[test]
    fn a_lost_flow_is_found_and_named_exactly() {
        let req = dump_request(libc::AF_INET as u8);
        assert_eq!(req.len(), 72);
        assert_eq!(&req[16..18], &[libc::AF_INET as u8, 6]);
        assert_eq!(u32::from_ne_bytes(req[20..24].try_into().unwrap()), RESET_STATES);
        assert_eq!(RESET_STATES & (1 << 10), 0, "no listener");
        let mut id = [0u8; 48];
        id[2..4].copy_from_slice(&443u16.to_be_bytes());
        id[20..24].copy_from_slice(&[93, 184, 216, 34]);
        id[40..48].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let destroy = destroy_request(libc::AF_INET as u8, &id);
        assert_eq!(destroy.len(), 56);
        assert_eq!(&destroy[8..], &id);
        let subnets = [(Ipv4Addr::new(10, 88, 0, 0), 16)];
        let inet = libc::AF_INET as u8;
        assert!(past_the_microvm(inet, &id, &[(443, 443)], &subnets, &[]));
        assert!(!past_the_microvm(inet, &id, &[(80, 80)], &subnets, &[]));
        id[20..24].copy_from_slice(&[10, 88, 3, 4]);
        assert!(
            !past_the_microvm(inet, &id, &[(443, 443)], &subnets, &[]),
            "a peer's flow"
        );
        id[20..24].copy_from_slice(&[127, 0, 0, 1]);
        assert!(!past_the_microvm(inet, &id, &[(443, 443)], &subnets, &[]));
        let mut id6 = [0u8; 48];
        id6[2..4].copy_from_slice(&443u16.to_be_bytes());
        id6[20..36].copy_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        let inet6 = libc::AF_INET6 as u8;
        assert!(past_the_microvm(inet6, &id6, &[(443, 443)], &subnets, &[]));
        let own6 = [("2001:db8::".parse().unwrap(), 64)];
        assert!(!past_the_microvm(inet6, &id6, &[(443, 443)], &subnets, &own6));
        id6[20..36].copy_from_slice(&Ipv4Addr::new(10, 88, 0, 9).to_ipv6_mapped().octets());
        assert!(
            !past_the_microvm(inet6, &id6, &[(443, 443)], &subnets, &[]),
            "a peer, mapped"
        );
        let mut m = vec![0u8; 16];
        m.extend_from_slice(&[libc::AF_INET as u8, 1, 0, 0]);
        m.extend_from_slice(&id);
        m.extend_from_slice(&[0; 20]);
        assert_eq!(flow(&m), Some((libc::AF_INET as u8, id)));
    }

    /// Grants as a run starts: a ceiling and a member's own within it made part of its link
    /// (its port, its destination's port, its names, the host itself in its hosts file), its
    /// image's kept apart; a grant past its ceiling, one for a member on internal networks
    /// alone or of no link, and members it would leave unheld, refused.
    #[test]
    fn grants_as_a_run_starts_join_its_links() {
        let link = |egress: &[Egress]| Link {
            egress: egress.to_vec(),
            wide: egress.to_vec(),
            ..Link::default()
        };
        let labels = vec![
            "harness codex".to_string(),
            "agent helper".to_string(),
            "agent shut".to_string(),
        ];
        let entry = |key: &[u8], m: u32, g: &Grants| abi::entry(key, m, g);
        let ceiling = g(&[(6, 8000, 8100)], &[(shards_dns::HOST, 7070)], true);
        let given = g(&[(6, 8080, 8080)], &[(shards_dns::HOST, 7070)], false);
        let setup = vec![
            entry(abi::CEILING_KEY, 0, &ceiling),
            entry(abi::EGRESS_KEY, 0, &given),
            // helper's image grants it 443: it starts with nothing past the microVM.
            entry(abi::EGRESS_KEY, 1, &Grants::default()),
        ];
        let gateway = Some(Ipv4Addr::new(172, 17, 0, 1));
        let mut links = vec![
            Some(link(&[(6, 443, 443)])),
            Some(link(&[(6, 443, 443)])),
            Some(link(&[])),
        ];
        at_start(
            &mut links,
            &[0, 1, 2],
            &labels,
            &[true, true, false],
            &setup,
            gateway,
        )
        .unwrap();
        let codex = links[0].as_ref().unwrap();
        assert_eq!(codex.granted.image, g(&[(6, 443, 443)], &[], false));
        assert_eq!(
            (&codex.granted.ceiling, &codex.granted.given),
            (&ceiling, &Some(given.clone()))
        );
        assert_eq!(
            codex.wide,
            vec![(6, 8080, 8080)],
            "what it was given, in place of its image's"
        );
        assert_eq!(codex.egress, vec![(6, 8080, 8080), (6, 7070, 7070)]);
        assert!(codex.connects && !codex.dns);
        assert!(String::from_utf8_lossy(&codex.hosts).contains("172.17.0.1\thost.docker.internal\n"));
        let helper = links[1].as_ref().unwrap();
        assert!(
            helper.egress.is_empty() && helper.wide.is_empty(),
            "taken back as it starts"
        );
        assert_eq!(helper.granted.reach(), Grants::default());
        assert_eq!(
            links[2].as_ref().unwrap().granted.reach(),
            Grants::default(),
            "its image's, none given"
        );
        assert!(may_widen(codex) && !may_widen(helper));
        let refused = |setup: Vec<Vec<u8>>, links: &mut Vec<Option<Link>>| {
            at_start(links, &[0, 1, 2], &labels, &[true, true, false], &setup, gateway).unwrap_err()
        };
        let fresh = || vec![Some(link(&[(6, 443, 443)])), Some(link(&[])), Some(link(&[]))];
        // Its image's reach is within it: given back as it starts, a member reaches it.
        let mut back = fresh();
        let again = vec![entry(abi::EGRESS_KEY, 0, &g(&[(6, 443, 443)], &[], false))];
        at_start(
            &mut back,
            &[0, 1, 2],
            &labels,
            &[true, true, false],
            &again,
            gateway,
        )
        .unwrap();
        assert_eq!(back[0].as_ref().unwrap().egress, vec![(6, 443, 443)]);
        let past_it = refused(
            vec![entry(abi::EGRESS_KEY, 1, &g(&[(6, 22, 22)], &[], false))],
            &mut fresh(),
        );
        assert!(
            past_it.contains("agent helper: TCP ports 22-22 is past"),
            "{past_it}"
        );
        let shut = refused(vec![entry(abi::CEILING_KEY, 2, &ceiling)], &mut fresh());
        assert!(shut.contains("agent shut, which no network"), "{shut}");
        let mut unlinked = vec![Some(link(&[(6, 443, 443)])), None, Some(link(&[]))];
        let none = refused(vec![entry(abi::CEILING_KEY, 1, &ceiling)], &mut unlinked);
        assert!(none.contains("agent helper"), "{none}");
        let absent = refused(vec![entry(abi::CEILING_KEY, 9, &ceiling)], &mut fresh());
        assert!(absent.contains("member 9"), "{absent}");
        let names = g(&[], &[("api.example.com", 443)], false);
        let unheld = refused(
            vec![
                entry(abi::CEILING_KEY, 1, &names),
                entry(abi::EGRESS_KEY, 1, &names),
            ],
            &mut fresh(),
        );
        assert!(unheld.contains("TCP port 443"), "{unheld}");
    }
}
