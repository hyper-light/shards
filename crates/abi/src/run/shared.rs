//! A run's place on a network shared by runs and its members' grants there (D285 slice 5),
//! told to init by [`builtin::SHARED`](super::builtin::SHARED) whole as either changes, as
//! D278's [`EGRESS`](super::builtin::EGRESS) is told a member's reach: the network's ID and
//! name; the run's address there, eth0's second, its prefix and the network's gateway; every
//! member on it, the run's own among them, by its name there (one member's) or an alias
//! (shared, as Compose's scaled replicas share one), at its run's address; and each of the
//! run's own members on it, by its index and its name there, with its grants, each a way, a
//! peer's name and a range of one protocol's ports: the peer's where the member opens flows
//! (`out`), its own where the peer does (`in`). A run whose last member leaves the network is
//! told so by the network's ID alone ([`args_off`]).

use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use super::egress::{HOST_MOST, port_range, push_port_range};
use super::members::decimal;

/// A network's ID as the home names it (D285 slice 1): `2 × MICROVM_ID_BYTES` lowercase hex
/// digits.
pub const ID_LEN: usize = 2 * crate::MICROVM_ID_BYTES;

/// Which way a [`Grant`] lets a flow open: from its peer to its member (`In`), or from its
/// member to its peer (`Out`). A flow's replies come back by its state, whichever way it
/// opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    In,
    Out,
}

/// A member's grant on a shared network, as kiln's `NetworkGrant` names one (kiln bb9ffcea1,
/// crates/kiln-seams/src/sandbox.rs): its way, its peer's name there, and a range of one
/// protocol's ports (its IP number, 6 or 17, and its ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub way: Way,
    pub peer: String,
    pub ports: (u8, u16, u16),
}

/// One of the run's own members on a shared network: its index in the run, its name there,
/// and its grants, one at least.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub index: u32,
    pub name: String,
    pub grants: Vec<Grant>,
}

/// The run on a shared network, as it is to stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub id: [u8; ID_LEN],
    /// The network's name, under which a member's may be asked too (`name.network`, D46).
    pub name: String,
    /// The run's address there, its prefix and the network's gateway: fixed while the run is
    /// on it.
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    /// Every member's name there, each one member's, at its run's address.
    pub names: Vec<(String, Ipv4Addr)>,
    /// Every alias there, at each address of a run one of whose members holds it.
    pub aliases: Vec<(String, Ipv4Addr)>,
    /// The run's own members there, one at least.
    pub members: Vec<Member>,
}

/// What [`builtin::SHARED`](super::builtin::SHARED) tells init of one network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Told {
    /// The run on it, as it is to stand.
    On(Shared),
    /// The run off it, by its ID: its last member left it.
    Off([u8; ID_LEN]),
}

/// Why [`parse_args`] refuses what it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No ID, or one not [`ID_LEN`] lowercase hex digits.
    Id,
    /// The run's place is no `ADDR/PREFIX,GATEWAY` of IPv4 whose address is on its gateway's
    /// subnet and not the gateway, or the network's name is none of Docker's.
    Place,
    /// An item of no kind, or malformed: a name of no name's grammar, an address off the
    /// network's subnet or its gateway's, an index not a decimal u32, ports not
    /// `PROTO/FIRST-LAST` of TCP or UDP.
    Item,
    /// A name at two addresses, a member's index or name twice, or one grant twice.
    Twice,
    /// None of the run's members on it, a member with no grant, a grant of an index no member
    /// item names, a member whose name is not the network's at the run's address, or a grant
    /// whose peer is its member.
    Member,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Id => "no network ID of 64 lowercase hex digits",
            Error::Place => {
                "no place on the network of ADDR/PREFIX,GATEWAY, its address on its gateway's subnet, or no network name"
            }
            Error::Item => {
                "an item that is no name, alias, member, in or out of names, addresses on the network and ports of TCP (6) or UDP (17)"
            }
            Error::Twice => "a name at two addresses, a member twice, or a grant twice",
            Error::Member => {
                "no member of the run's on it, a member with no grant or of no name of the network's at the run's address, a grant of no member, or one whose peer is its member"
            }
        })
    }
}

/// Whether `name` is a member's name or alias as Docker's grammar spells one,
/// `[a-zA-Z0-9][a-zA-Z0-9_.-]*` (moby daemon/names, `RestrictedNamePattern`), lowercased
/// (RFC 4343), within a host's name's [`HOST_MOST`]: names of slice 4's leases may hold `_`
/// and `.`, as container names do.
pub fn name_ok(name: &[u8]) -> bool {
    let rest_ok = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-');
    match name.split_first() {
        Some((first, rest)) => {
            name.len() <= HOST_MOST
                && (first.is_ascii_lowercase() || first.is_ascii_digit())
                && rest.iter().all(rest_ok)
        }
        None => false,
    }
}

/// Whether `name` is a network's name as Docker's grammar spells one,
/// `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, within the byte its length takes in a table of names
/// (`shards_dns::Names::encode`).
fn network_ok(name: &[u8]) -> bool {
    let rest_ok = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-');
    match name.split_first() {
        Some((first, rest)) => {
            name.len() <= usize::from(u8::MAX) && first.is_ascii_alphanumeric() && rest.iter().all(rest_ok)
        }
        None => false,
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

/// The subnet `address` and `prefix` name: its network's address and its prefix.
pub fn subnet(address: Ipv4Addr, prefix: u8) -> (Ipv4Addr, u8) {
    (Ipv4Addr::from(u32::from(address) & mask(prefix)), prefix)
}

/// The ID `id` is, where it is one.
fn id_of(id: &[u8]) -> Result<[u8; ID_LEN], Error> {
    let id = <[u8; ID_LEN]>::try_from(id).map_err(|_| Error::Id)?;
    if id.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
        Ok(id)
    } else {
        Err(Error::Id)
    }
}

/// `ADDR/PREFIX,GATEWAY`, as an `address=` setup entry says eth0's.
fn place(text: &[u8]) -> Result<(Ipv4Addr, u8, Ipv4Addr), Error> {
    let text = core::str::from_utf8(text).map_err(|_| Error::Place)?;
    let (cidr, gateway) = text.split_once(',').ok_or(Error::Place)?;
    let (address, prefix) = cidr.split_once('/').ok_or(Error::Place)?;
    let address: Ipv4Addr = address.parse().map_err(|_| Error::Place)?;
    let gateway: Ipv4Addr = gateway.parse().map_err(|_| Error::Place)?;
    let prefix = u8::try_from(decimal(prefix.as_bytes()).map_err(|_| Error::Place)?)
        .ok()
        .filter(|p| *p <= 32)
        .ok_or(Error::Place)?;
    if address == gateway || !within(address, gateway, prefix) {
        return Err(Error::Place);
    }
    Ok((address, prefix, gateway))
}

fn text(word: &[u8]) -> Result<String, Error> {
    core::str::from_utf8(word)
        .map(String::from)
        .map_err(|_| Error::Item)
}

/// [`builtin::SHARED`](super::builtin::SHARED)'s arguments for a run on `s`: its ID, its place
/// (`ADDR/PREFIX,GATEWAY`), its name, then each name (`name NAME ADDR`), alias (`alias NAME
/// ADDR`), member (`member INDEX NAME`) and grant (`in INDEX PEER PROTO/FIRST-LAST`, or
/// `out …`), each its own argument.
pub fn args(s: &Shared) -> Vec<Vec<u8>> {
    let mut out = alloc::vec![
        s.id.to_vec(),
        alloc::format!("{}/{},{}", s.address, s.prefix, s.gateway).into_bytes(),
        s.name.as_bytes().to_vec(),
    ];
    for (name, at) in &s.names {
        out.push(alloc::format!("name {name} {at}").into_bytes());
    }
    for (name, at) in &s.aliases {
        out.push(alloc::format!("alias {name} {at}").into_bytes());
    }
    for m in &s.members {
        out.push(alloc::format!("member {} {}", m.index, m.name).into_bytes());
        for g in &m.grants {
            let way = match g.way {
                Way::In => "in",
                Way::Out => "out",
            };
            let mut item = alloc::format!("{way} {} {} ", m.index, g.peer).into_bytes();
            push_port_range(&mut item, g.ports);
            out.push(item);
        }
    }
    out
}

/// [`builtin::SHARED`](super::builtin::SHARED)'s arguments for a run whose last member left the
/// network `id`: its ID alone.
pub fn args_off(id: &[u8; ID_LEN]) -> Vec<Vec<u8>> {
    alloc::vec![id.to_vec()]
}

/// What [`args`] or [`args_off`] tells, checked: each name at one address, each member and
/// each of its grants once, every address on the network's subnet and none its gateway.
pub fn parse_args(args: &[Vec<u8>]) -> Result<Told, Error> {
    let (id, rest) = args.split_first().ok_or(Error::Id)?;
    let id = id_of(id)?;
    let [at, name, items @ ..] = rest else {
        return match rest {
            [] => Ok(Told::Off(id)),
            _ => Err(Error::Place),
        };
    };
    let (address, prefix, gateway) = place(at)?;
    if !network_ok(name) {
        return Err(Error::Place);
    }
    let mut s = Shared {
        id,
        name: text(name)?,
        address,
        prefix,
        gateway,
        names: Vec::new(),
        aliases: Vec::new(),
        members: Vec::new(),
    };
    let on = |word: &[u8]| -> Result<Ipv4Addr, Error> {
        let at: Ipv4Addr = core::str::from_utf8(word)
            .ok()
            .and_then(|w| w.parse().ok())
            .ok_or(Error::Item)?;
        if at == gateway || !within(at, gateway, prefix) {
            return Err(Error::Item);
        }
        Ok(at)
    };
    let named = |word: &[u8]| {
        if name_ok(word) {
            text(word)
        } else {
            Err(Error::Item)
        }
    };
    let mut grants: Vec<(u32, Grant)> = Vec::new();
    for item in items {
        let words: Vec<&[u8]> = item.split(|&b| b == b' ').collect();
        match words.as_slice() {
            [b"name", name, at] => {
                let (name, at) = (named(name)?, on(at)?);
                if s.names.iter().any(|(n, _)| *n == name) {
                    return Err(Error::Twice);
                }
                s.names.push((name, at));
            }
            [b"alias", name, at] => {
                let entry = (named(name)?, on(at)?);
                if s.aliases.contains(&entry) {
                    return Err(Error::Twice);
                }
                s.aliases.push(entry);
            }
            [b"member", index, name] => {
                let (index, name) = (decimal(index).map_err(|_| Error::Item)?, named(name)?);
                if s.members.iter().any(|m| m.index == index || m.name == name) {
                    return Err(Error::Twice);
                }
                s.members.push(Member {
                    index,
                    name,
                    grants: Vec::new(),
                });
            }
            [way @ (b"in" | b"out"), index, peer, ports] => {
                let way = if *way == b"in" { Way::In } else { Way::Out };
                let grant = Grant {
                    way,
                    peer: named(peer)?,
                    ports: port_range(ports).ok_or(Error::Item)?,
                };
                let index = decimal(index).map_err(|_| Error::Item)?;
                if grants.iter().any(|(i, g)| *i == index && *g == grant) {
                    return Err(Error::Twice);
                }
                grants.push((index, grant));
            }
            _ => return Err(Error::Item),
        }
    }
    for (index, grant) in grants {
        let m = s
            .members
            .iter_mut()
            .find(|m| m.index == index)
            .ok_or(Error::Member)?;
        if grant.peer == m.name {
            return Err(Error::Member);
        }
        m.grants.push(grant);
    }
    let at_run = |name: &str| s.names.iter().any(|(n, at)| n == name && *at == s.address);
    if s.members.is_empty() || s.members.iter().any(|m| m.grants.is_empty() || !at_run(&m.name)) {
        return Err(Error::Member);
    }
    Ok(Told::On(s))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use alloc::vec;

    fn shared() -> Shared {
        let at = |d| Ipv4Addr::new(10, 89, 0, d);
        Shared {
            id: [b'a'; ID_LEN],
            name: "Team_net".into(),
            address: at(2),
            prefix: 24,
            gateway: at(1),
            names: vec![
                ("codex".into(), at(2)),
                ("db".into(), at(3)),
                ("web_1".into(), at(4)),
            ],
            aliases: vec![("web".into(), at(4)), ("web".into(), at(5))],
            members: vec![Member {
                index: 1,
                name: "codex".into(),
                grants: vec![
                    Grant {
                        way: Way::Out,
                        peer: "db".into(),
                        ports: (6, 5432, 5432),
                    },
                    Grant {
                        way: Way::In,
                        peer: "web".into(),
                        ports: (17, 9000, 9010),
                    },
                ],
            }],
        }
    }

    /// A run's place, the network's names and aliases, its members and their grants read
    /// back as written, and its leaving by the ID alone; each refused shape refused by its
    /// kind: names exclusive, aliases shared.
    #[test]
    fn a_runs_place_and_grants_on_a_shared_network_read_back_as_written() {
        let s = shared();
        assert_eq!(parse_args(&args(&s)), Ok(Told::On(s.clone())));
        assert_eq!(parse_args(&args_off(&s.id)), Ok(Told::Off(s.id)));
        let with = |item: &[u8]| {
            let mut a = args(&s);
            a.push(item.to_vec());
            parse_args(&a)
        };
        // An alias at another run's address is shared; a name is one member's.
        assert!(matches!(with(b"alias db 10.89.0.9"), Ok(Told::On(_))));
        assert_eq!(with(b"name db 10.89.0.9"), Err(Error::Twice));
        assert_eq!(with(b"alias web 10.89.0.5"), Err(Error::Twice));
        assert_eq!(with(b"out 1 db 6/5432-5432"), Err(Error::Twice));
        assert_eq!(with(b"member 1 other"), Err(Error::Twice));
        assert_eq!(with(b"out 7 db 6/80-80"), Err(Error::Member), "no member 7");
        assert_eq!(with(b"out 1 codex 6/80-80"), Err(Error::Member), "its own name");
        for bad in [
            &b"name db 10.90.0.3"[..],
            b"name db 10.89.0.1",
            b"name Db 10.89.0.3",
            b"name -db 10.89.0.3",
            b"alias web 10.89.0",
            b"member 01 codex",
            b"out 1 db 6/0-80",
            b"out 1 db 1/80-80",
            b"sideways 1 db 6/80-80",
            b"name  db 10.89.0.3",
        ] {
            assert_eq!(with(bad), Err(Error::Item), "{:?}", bad.escape_ascii());
        }
        let mut lone = args(&s);
        lone.push(b"name helper 10.89.0.2".to_vec());
        lone.push(b"member 2 helper".to_vec());
        assert_eq!(parse_args(&lone), Err(Error::Member), "a member with no grant");
        lone.push(b"out 2 db 6/80-80".to_vec());
        assert!(
            matches!(parse_args(&lone), Ok(Told::On(_))),
            "two of the run's members"
        );
        let mut elsewhere = args(&s);
        elsewhere.push(b"member 2 web_1".to_vec());
        elsewhere.push(b"out 2 db 6/80-80".to_vec());
        assert_eq!(
            parse_args(&elsewhere),
            Err(Error::Member),
            "a member of the run's named at another run's address"
        );
        let mut a = args(&s);
        a[1] = b"10.89.0.1/24,10.89.0.1".to_vec();
        assert_eq!(parse_args(&a), Err(Error::Place));
        a[1] = b"10.89.1.2/24,10.89.0.1".to_vec();
        assert_eq!(parse_args(&a), Err(Error::Place));
        a[1] = b"10.89.0.2/33,10.89.0.1".to_vec();
        assert_eq!(parse_args(&a), Err(Error::Place));
        let mut a = args(&s);
        a[2] = b"-net".to_vec();
        assert_eq!(parse_args(&a), Err(Error::Place));
        assert_eq!(parse_args(&[b"A".repeat(ID_LEN)]), Err(Error::Id));
        assert_eq!(parse_args(&[]), Err(Error::Id));
        assert_eq!(
            parse_args(&[s.id.to_vec(), b"10.89.0.2/24,10.89.0.1".to_vec()]),
            Err(Error::Place)
        );
        let no_members = vec![s.id.to_vec(), b"10.89.0.2/24,10.89.0.1".to_vec(), b"net".to_vec()];
        assert_eq!(parse_args(&no_members), Err(Error::Member));
        assert!(name_ok(b"web_1.x-y") && !name_ok(b"") && !name_ok(&[b'a'; HOST_MOST + 1]));
        assert_eq!(
            subnet(Ipv4Addr::new(10, 89, 0, 2), 24),
            (Ipv4Addr::new(10, 89, 0, 0), 24)
        );
        assert_eq!(subnet(Ipv4Addr::new(10, 89, 0, 2), 0), (Ipv4Addr::UNSPECIFIED, 0));
    }
}
