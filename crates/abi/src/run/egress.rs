//! A member's own grants past its microVM (D278): what a run's host lets each member reach
//! past the microVM as it starts ([`EGRESS_KEY`]), the most it may ever let it past what its
//! image grants it ([`CEILING_KEY`]), and each change of them as the run goes, told to init
//! by [`builtin::EGRESS`](super::builtin::EGRESS).
//!
//! A member's grants are items, each one of: a port range of TCP's or UDP's to any address,
//! `PROTO/LO-HI` (PROTO the IP protocol's number, 6 or 17); a destination by name at a TCP
//! port, `HOST:PORT` (D236: a network's `--egress=HOST:PORT` and a remote MCP server's
//! alike); and names asked past the microVM, [`DNS`].

use alloc::string::String;
use alloc::vec::Vec;

use super::members::{decimal, push_decimal};

/// `member-egress=<index>\0<item>\0…`: what the member reaches past the microVM as it starts,
/// whole, in place of what its image grants it, within its image's grants and its ceiling.
/// One for a member at most; none, its image's grants.
pub const EGRESS_KEY: &[u8] = b"member-egress=";
/// `member-ceiling=<index>\0<item>\0…`: the most the member may be let past what its image
/// grants it, as the run starts and as it goes. One for a member at most; none, nothing past
/// its image's.
pub const CEILING_KEY: &[u8] = b"member-ceiling=";
/// The item that grants names asked past the microVM (`NETWORK --dns`).
pub const DNS: &[u8] = b"dns";
/// A host's name as text at its most: 253, its 255 octets on the wire less its first length
/// and the root's (RFC 1035 §2.3.4), as D236 bounds a destination's.
pub const HOST_MOST: usize = 253;

/// A member's grants past the microVM: port ranges to any address, each its IP protocol's
/// number and its ends; destinations by name, each a host, lowered, and its TCP port; and
/// whether it asks names past the microVM.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grants {
    pub ports: Vec<(u8, u16, u16)>,
    pub named: Vec<(String, u16)>,
    pub dns: bool,
}

/// Why a member's grants are refused as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Its member index is not a decimal u32.
    Index,
    /// An item is none of a port range, a destination or [`DNS`].
    Item,
    /// A second entry of one key for one member.
    Twice,
    /// No member index at all.
    Shape,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Index => "a member index that is not a decimal u32",
            Error::Item => {
                "a grant that is no PROTO/LO-HI of TCP (6) or UDP (17), HOST:PORT of a lowercase host's name or IPv4 address, or dns"
            }
            Error::Twice => "a second entry of one key for one member",
            Error::Shape => "no member index",
        })
    }
}

/// A port as an item spells it: decimal, 1 to 65535, one spelling each.
fn port(text: &[u8]) -> Result<u16, Error> {
    decimal(text)
        .ok()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|p| *p > 0)
        .ok_or(Error::Item)
}

/// A host's name as a destination names one (D236): lowercase letters, digits, `-` and `.`,
/// within [`HOST_MOST`].
pub fn host_ok(host: &[u8]) -> bool {
    !host.is_empty()
        && host.len() <= HOST_MOST
        && host
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
}

/// `item` added to `into`, each once.
pub fn parse_item(item: &[u8], into: &mut Grants) -> Result<(), Error> {
    if item == DNS {
        into.dns = true;
        return Ok(());
    }
    if let Some(slash) = item.iter().position(|&b| b == b'/') {
        let (proto, range) = (
            item.get(..slash).ok_or(Error::Item)?,
            item.get(slash + 1..).ok_or(Error::Item)?,
        );
        let proto = match proto {
            b"6" => 6,
            b"17" => 17,
            _ => return Err(Error::Item),
        };
        let dash = range.iter().position(|&b| b == b'-').ok_or(Error::Item)?;
        let lo = port(range.get(..dash).ok_or(Error::Item)?)?;
        let hi = port(range.get(dash + 1..).ok_or(Error::Item)?)?;
        if lo > hi {
            return Err(Error::Item);
        }
        if !into.ports.contains(&(proto, lo, hi)) {
            into.ports.push((proto, lo, hi));
        }
        return Ok(());
    }
    let colon = item.iter().rposition(|&b| b == b':').ok_or(Error::Item)?;
    let host = item.get(..colon).ok_or(Error::Item)?;
    let at = port(item.get(colon + 1..).ok_or(Error::Item)?)?;
    if !host_ok(host) {
        return Err(Error::Item);
    }
    let host = String::from(core::str::from_utf8(host).map_err(|_| Error::Item)?);
    if !into.named.iter().any(|(h, p)| *h == host && *p == at) {
        into.named.push((host, at));
    }
    Ok(())
}

/// Each of `g`'s items, as [`parse_item`] reads them: its port ranges, its destinations,
/// then [`DNS`] where it asks names.
pub fn items(g: &Grants) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(g.ports.len() + g.named.len() + 1);
    for &(proto, lo, hi) in &g.ports {
        let mut item = Vec::new();
        push_decimal(&mut item, u32::from(proto));
        item.push(b'/');
        push_decimal(&mut item, u32::from(lo));
        item.push(b'-');
        push_decimal(&mut item, u32::from(hi));
        out.push(item);
    }
    for (host, at) in &g.named {
        let mut item = host.as_bytes().to_vec();
        item.push(b':');
        push_decimal(&mut item, u32::from(*at));
        out.push(item);
    }
    if g.dns {
        out.push(DNS.to_vec());
    }
    out
}

/// [`builtin::EGRESS`](super::builtin::EGRESS)'s arguments: the member's index in decimal,
/// then each of its grants as they are to stand, whole.
pub fn args(member: u32, g: &Grants) -> Vec<Vec<u8>> {
    let mut index = Vec::new();
    push_decimal(&mut index, member);
    let mut out = alloc::vec![index];
    out.extend(items(g));
    out
}

/// The member and the grants [`args`] gives, checked.
pub fn parse_args(args: &[Vec<u8>]) -> Result<(u32, Grants), Error> {
    let (index, items) = args.split_first().ok_or(Error::Shape)?;
    let mut g = Grants::default();
    for item in items {
        parse_item(item, &mut g)?;
    }
    Ok((decimal(index).map_err(|_| Error::Index)?, g))
}

/// The setup entry of `key` ([`EGRESS_KEY`] or [`CEILING_KEY`]) giving member `member` `g`.
pub fn entry(key: &[u8], member: u32, g: &Grants) -> Vec<u8> {
    let mut out = key.to_vec();
    push_decimal(&mut out, member);
    for item in items(g) {
        out.push(0);
        out.extend_from_slice(&item);
    }
    out
}

/// Each member's grants that `setup`'s entries of `key` give, by its index, each member once.
pub fn of(setup: &[Vec<u8>], key: &[u8]) -> Result<Vec<(u32, Grants)>, Error> {
    let mut out: Vec<(u32, Grants)> = Vec::new();
    for value in setup.iter().filter_map(|e| e.strip_prefix(key)) {
        let mut fields = value.split(|&b| b == 0);
        let index = decimal(fields.next().ok_or(Error::Shape)?).map_err(|_| Error::Index)?;
        let mut g = Grants::default();
        for item in fields {
            parse_item(item, &mut g)?;
        }
        if out.iter().any(|(m, _)| *m == index) {
            return Err(Error::Twice);
        }
        out.push((index, g));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use alloc::vec;

    fn some() -> Grants {
        Grants {
            ports: vec![(6, 443, 443), (17, 5000, 5010)],
            named: vec![("api.example.com".into(), 443), ("10.0.0.9".into(), 8443)],
            dns: true,
        }
    }

    /// A member's grants read back as written, as a built-in's arguments and as a setup entry;
    /// none at all, nothing; each refused shape refused by its kind.
    #[test]
    fn a_members_grants_read_back_as_written() {
        let g = some();
        assert_eq!(parse_args(&args(7, &g)), Ok((7, g.clone())));
        assert_eq!(
            parse_args(&args(0, &Grants::default())),
            Ok((0, Grants::default()))
        );
        let setup = vec![
            entry(EGRESS_KEY, 3, &g),
            b"readonly".to_vec(),
            entry(CEILING_KEY, 3, &Grants::default()),
            entry(EGRESS_KEY, 4, &Grants::default()),
        ];
        assert_eq!(
            of(&setup, EGRESS_KEY),
            Ok(vec![(3, g.clone()), (4, Grants::default())])
        );
        assert_eq!(of(&setup, CEILING_KEY), Ok(vec![(3, Grants::default())]));
        assert_eq!(
            of(
                &[entry(CEILING_KEY, 1, &g), entry(CEILING_KEY, 1, &g)],
                CEILING_KEY
            ),
            Err(Error::Twice)
        );
        // Each item once, however often written.
        let twice = vec![
            b"1".to_vec(),
            b"6/80-80".to_vec(),
            b"6/80-80".to_vec(),
            DNS.to_vec(),
            DNS.to_vec(),
        ];
        assert_eq!(
            parse_args(&twice),
            Ok((
                1,
                Grants {
                    ports: vec![(6, 80, 80)],
                    named: vec![],
                    dns: true
                }
            ))
        );
        assert_eq!(parse_args(&[]), Err(Error::Shape));
        assert_eq!(parse_args(&[b"01".to_vec()]), Err(Error::Index));
        for bad in [
            &b"1/80-80"[..],
            b"6/80",
            b"6/0-80",
            b"6/90-80",
            b"6/80-65536",
            b"6/080-80",
            b"Api.example.com:443",
            b"api.example.com:0",
            b"api.example.com",
            b":443",
            b"a_b:443",
            b"[::1]:443",
            b"DNS",
        ] {
            assert_eq!(
                parse_args(&[b"1".to_vec(), bad.to_vec()]),
                Err(Error::Item),
                "{:?}",
                bad.escape_ascii()
            );
        }
        let long = vec![b'a'; HOST_MOST + 1];
        assert!(!host_ok(&long) && host_ok(&long[..HOST_MOST]));
    }
}
