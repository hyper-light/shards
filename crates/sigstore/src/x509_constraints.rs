//! Name constraints and certificate policies as Go 1.26's crypto/x509 reads and applies
//! them: the extension as parseNameConstraintsExtension reads it, newOIDFromDER's check
//! of a policy's OID, a chain's names checked against every constraint above them
//! (constraints.go, checkChainConstraints), and its policies against RFC 5280 as RFC 9618
//! updates it (verify.go, policiesValid, with no policies asked of the chain).

use std::collections::{BTreeMap, BTreeSet};

use crate::der::{self, Der};
use crate::x509::{Certificate, Extension, OID_SAN, X509Error, domain_name_valid};

fn err(s: &str) -> X509Error {
    X509Error(s.to_string())
}

fn quote(b: &[u8]) -> String {
    shards_dockerfile::go::quote(b)
}

/// A name constraints extension's subtrees, as Go keeps them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameConstraints {
    pub permitted_dns: Vec<Vec<u8>>,
    pub excluded_dns: Vec<Vec<u8>>,
    /// (IP, mask), each 4 or 16 octets.
    pub permitted_ips: Vec<(Vec<u8>, Vec<u8>)>,
    pub excluded_ips: Vec<(Vec<u8>, Vec<u8>)>,
    pub permitted_emails: Vec<Vec<u8>>,
    pub excluded_emails: Vec<Vec<u8>>,
    pub permitted_uris: Vec<Vec<u8>>,
    pub excluded_uris: Vec<Vec<u8>>,
}

/// isIA5String's error, or none.
fn ia5(s: &[u8]) -> Result<(), X509Error> {
    if s.is_ascii() {
        return Ok(());
    }
    Err(X509Error(format!(
        "x509: invalid constraint value: x509: {} cannot be encoded as an IA5String",
        quote(s)
    )))
}

/// isValidIPMask.
fn valid_mask(mask: &[u8]) -> bool {
    let mut seen_zero = false;
    for &b in mask {
        if seen_zero {
            if b != 0 {
                return false;
            }
            continue;
        }
        match b {
            0x00 | 0x80 | 0xc0 | 0xe0 | 0xf0 | 0xf8 | 0xfc | 0xfe => seen_zero = true,
            0xff => {}
            _ => return false,
        }
    }
    true
}

/// domainToReverseLabels' answer.
fn reverse_labels_ok(domain: &[u8]) -> bool {
    if domain.is_empty() {
        return true;
    }
    if domain.last() == Some(&b'.') {
        return false;
    }
    domain
        .split(|c| *c == b'.')
        .all(|l| !l.is_empty() && l.iter().all(|c| (33..=126).contains(c)))
}

/// An rfc2821Mailbox: its local part and domain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mailbox {
    pub local: Vec<u8>,
    pub domain: Vec<u8>,
}

impl Mailbox {
    /// rfc2821Mailbox.String: local@domain.
    fn text(&self) -> Vec<u8> {
        [self.local.as_slice(), b"@", &self.domain].concat()
    }
}

/// parseRFC2821Mailbox.
pub fn parse_mailbox(input: &[u8]) -> Option<Mailbox> {
    let &first = input.first()?;
    let mut rest = input;
    let mut local = Vec::new();
    if first == b'"' {
        rest = rest.get(1..).unwrap_or_default();
        loop {
            let (&c, tail) = rest.split_first()?;
            rest = tail;
            match c {
                b'"' => break,
                b'\\' => {
                    let (&n, tail) = rest.split_first()?;
                    if n == 11 || n == 12 || (1..=9).contains(&n) || (14..=127).contains(&n) {
                        local.push(n);
                        rest = tail;
                    } else {
                        return None;
                    }
                }
                11 | 12 | 32 | 33 | 127 => local.push(c),
                c if (1..=8).contains(&c)
                    || (14..=31).contains(&c)
                    || (35..=91).contains(&c)
                    || (93..=126).contains(&c) =>
                {
                    local.push(c);
                }
                _ => return None,
            }
        }
    } else {
        while let Some(&c) = rest.first() {
            let atext = |c: u8| c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&c);
            if c == b'\\' {
                rest = rest.get(1..).unwrap_or_default();
                let &n = rest.first()?;
                local.push(n);
                rest = rest.get(1..).unwrap_or_default();
            } else if atext(c) {
                local.push(c);
                rest = rest.get(1..).unwrap_or_default();
            } else {
                break;
            }
        }
        if local.is_empty()
            || local.first() == Some(&b'.')
            || local.last() == Some(&b'.')
            || local.windows(2).any(|w| w == b"..")
        {
            return None;
        }
    }
    match rest.split_first() {
        Some((b'@', domain)) if reverse_labels_ok(domain) => Some(Mailbox {
            local,
            domain: domain.to_vec(),
        }),
        _ => None,
    }
}

/// net.ParseIP's verdict: an address netip.ParseAddr reads, without a zone.
pub fn is_ip(s: &[u8]) -> bool {
    shards_dockerfile::url::parse_addr(s).is_ok() && !s.contains(&b'%')
}

/// The values of one list of subtrees: DNS, IPs, emails, URI domains.
#[derive(Default)]
struct Subtrees {
    dns: Vec<Vec<u8>>,
    ips: Vec<(Vec<u8>, Vec<u8>)>,
    emails: Vec<Vec<u8>>,
    uris: Vec<Vec<u8>>,
}

/// One subtree list's values, checked; `unhandled` set where one is of a kind Go does
/// not handle.
fn subtrees(mut list: Der<'_>, unhandled: &mut bool) -> Result<Subtrees, X509Error> {
    let mut out = Subtrees::default();
    while !list.is_empty() {
        let mut seq = list
            .read(der::SEQUENCE)
            .ok_or_else(|| err("x509: invalid NameConstraints extension"))?;
        let (tag, _, value) = seq
            .any_element()
            .ok_or_else(|| err("x509: invalid NameConstraints extension"))?;
        match tag {
            0x82 => {
                ia5(value)?;
                if !domain_name_valid(value, true) {
                    return Err(X509Error(format!(
                        "x509: failed to parse dnsName constraint {}",
                        quote(value)
                    )));
                }
                out.dns.push(value.to_vec());
            }
            0x87 => {
                let half = value.len() / 2;
                if value.len() != 8 && value.len() != 32 {
                    return Err(X509Error(format!(
                        "x509: IP constraint contained value of length {}",
                        value.len()
                    )));
                }
                let (ip, mask) = value.split_at(half);
                if !valid_mask(mask) {
                    let hex: String = mask.iter().map(|b| format!("{b:02x}")).collect();
                    return Err(X509Error(format!(
                        "x509: IP constraint contained invalid mask {hex}"
                    )));
                }
                out.ips.push((ip.to_vec(), mask.to_vec()));
            }
            0x81 => {
                ia5(value)?;
                let ok = if value.contains(&b'@') {
                    parse_mailbox(value).is_some()
                } else {
                    domain_name_valid(value, true)
                };
                if !ok {
                    return Err(X509Error(format!(
                        "x509: failed to parse rfc822Name constraint {}",
                        quote(value)
                    )));
                }
                out.emails.push(value.to_vec());
            }
            0x86 => {
                ia5(value)?;
                if is_ip(value) {
                    return Err(X509Error(format!(
                        "x509: failed to parse URI constraint {}: cannot be IP address",
                        quote(value)
                    )));
                }
                if !domain_name_valid(value, true) {
                    return Err(X509Error(format!(
                        "x509: failed to parse URI constraint {}",
                        quote(value)
                    )));
                }
                out.uris.push(value.to_vec());
            }
            _ => *unhandled = true,
        }
    }
    Ok(out)
}

/// parseNameConstraintsExtension: the constraints, and whether the extension holds a kind
/// of name Go leaves unhandled; or its error.
pub fn parse_name_constraints(e: &Extension) -> Result<(NameConstraints, bool), X509Error> {
    let invalid = || err("x509: invalid NameConstraints extension");
    let mut outer = Der(&e.value);
    let mut top = outer.read(der::SEQUENCE).ok_or_else(invalid)?;
    if !outer.is_empty() {
        return Err(invalid());
    }
    let permitted = top.optional(der::explicit(0)).ok_or_else(invalid)?;
    let excluded = top.optional(der::explicit(1)).ok_or_else(invalid)?;
    if !top.is_empty() {
        return Err(invalid());
    }
    let empty = |d: &Option<Der<'_>>| d.as_ref().is_none_or(Der::is_empty);
    if empty(&permitted) && empty(&excluded) {
        return Err(err("x509: empty name constraints extension"));
    }
    let mut unhandled = false;
    let p = match permitted {
        Some(p) => subtrees(p, &mut unhandled)?,
        None => Subtrees::default(),
    };
    let x = match excluded {
        Some(x) => subtrees(x, &mut unhandled)?,
        None => Subtrees::default(),
    };
    Ok((
        NameConstraints {
            permitted_dns: p.dns,
            permitted_ips: p.ips,
            permitted_emails: p.emails,
            permitted_uris: p.uris,
            excluded_dns: x.dns,
            excluded_ips: x.ips,
            excluded_emails: x.emails,
            excluded_uris: x.uris,
        },
        unhandled,
    ))
}

/// newOIDFromDER's verdict.
pub fn new_oid_from_der(der: &[u8]) -> bool {
    if der.last().is_none_or(|b| b & 0x80 != 0) {
        return false;
    }
    let mut start = 0;
    for (i, &v) in der.iter().enumerate() {
        if i == start && v == 0x80 {
            return false;
        }
        if v & 0x80 == 0 {
            start = i + 1;
        }
    }
    true
}

// --- IP addresses, as package net holds and prints them.

/// IP.To4.
fn to4(ip: &[u8]) -> Option<&[u8]> {
    match ip.len() {
        4 => Some(ip),
        16 if ip.get(..10).is_some_and(|z| z.iter().all(|b| *b == 0))
            && ip.get(10..12) == Some(&[0xff, 0xff]) =>
        {
            ip.get(12..)
        }
        _ => None,
    }
}

/// networkNumberAndMask.
fn network_and_mask<'a>(ip: &'a [u8], mask: &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
    let nn = match to4(ip) {
        Some(v4) => v4,
        None if ip.len() == 16 => ip,
        None => return None,
    };
    let m = match mask.len() {
        4 if nn.len() != 4 => return None,
        4 => mask,
        16 if nn.len() == 4 => mask.get(12..)?,
        16 => mask,
        _ => return None,
    };
    Some((nn, m))
}

/// IPNet.Contains.
fn contains(ip: &[u8], mask: &[u8], target: &[u8]) -> bool {
    let Some((nn, m)) = network_and_mask(ip, mask) else {
        return false;
    };
    let t = to4(target).unwrap_or(target);
    if t.len() != nn.len() {
        return false;
    }
    nn.iter().zip(m).zip(t).all(|((n, m), t)| n & m == t & m)
}

/// IP.String.
pub fn ip_string(ip: &[u8]) -> String {
    if ip.is_empty() {
        return "<nil>".into();
    }
    if let Some(v4) = to4(ip) {
        return v4.iter().map(u8::to_string).collect::<Vec<_>>().join(".");
    }
    if ip.len() != 16 {
        let hex: String = ip.iter().map(|b| format!("{b:02x}")).collect();
        return format!("?{hex}");
    }
    let groups: Vec<u16> = ip
        .chunks(2)
        .map(|c| u16::from_be_bytes([c.first().copied().unwrap_or(0), c.get(1).copied().unwrap_or(0)]))
        .collect();
    let (mut zs, mut ze) = (8usize, 8usize);
    let mut i = 0;
    while i < 8 {
        let mut j = i;
        while groups.get(j) == Some(&0) {
            j += 1;
        }
        if j - i >= 2 && j - i > ze.saturating_sub(zs) {
            zs = i;
            ze = j;
        }
        i += 1;
    }
    let mut out = String::new();
    let mut i = 0;
    while i < 8 {
        if i == zs {
            out.push_str("::");
            i = ze;
            continue;
        }
        if i > 0 && !out.ends_with(':') {
            out.push(':');
        }
        out.push_str(&format!("{:x}", groups.get(i).copied().unwrap_or(0)));
        i += 1;
    }
    out
}

/// IPNet.String.
fn ipnet_string(ip: &[u8], mask: &[u8]) -> String {
    let Some((nn, m)) = network_and_mask(ip, mask) else {
        return "<nil>".into();
    };
    // simpleMaskLength.
    let mut ones = 0usize;
    let mut canonical = true;
    let mut done = false;
    for &b in m {
        if done {
            if b != 0 {
                canonical = false;
            }
            continue;
        }
        if b == 0xff {
            ones += 8;
            continue;
        }
        let lead = b.leading_ones() as usize;
        if b << lead != 0 {
            canonical = false;
        }
        ones += lead;
        done = true;
    }
    if canonical {
        format!("{}/{ones}", ip_string(nn))
    } else {
        let hex: String = m.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}/{hex}", ip_string(nn))
    }
}

// --- Constraint sets (nameConstraintsSet).

/// sortAndPrune: sorted, and each covered by the one kept before it dropped.
fn sort_and_prune<T: Clone>(
    set: &mut Vec<T>,
    cmp: impl Fn(&T, &T) -> std::cmp::Ordering,
    subset: impl Fn(&T, &T) -> bool,
) {
    if set.len() < 2 {
        return;
    }
    set.sort_by(&cmp);
    let mut kept: Vec<T> = Vec::with_capacity(set.len());
    for x in set.drain(..) {
        match kept.last() {
            Some(last) if subset(last, &x) => {}
            _ => kept.push(x),
        }
    }
    *set = kept;
}

/// search: the exact element, or the one before where `s` would go if it matches.
fn search<'a, T, V: ?Sized>(
    set: &'a [T],
    s: &V,
    cmp: impl Fn(&T, &V) -> std::cmp::Ordering,
    matches: impl Fn(&T, &V) -> bool,
) -> Option<&'a T> {
    if set.is_empty() {
        return None;
    }
    let i = set.partition_point(|x| cmp(x, s) == std::cmp::Ordering::Less);
    if let Some(x) = set.get(i)
        && cmp(x, s) == std::cmp::Ordering::Equal
    {
        return Some(x);
    }
    let c = set.get(i.saturating_sub(1))?;
    matches(c, s).then_some(c)
}

/// dnsHasSuffix: whether `b` is `a` or under it, ASCII case aside.
fn dns_has_suffix(a: &[u8], b: &[u8]) -> bool {
    if a.len() > b.len() {
        return false;
    }
    let off = b.len() - a.len();
    for (i, &ar) in a.iter().enumerate().rev() {
        let br = b.get(i + off).copied().unwrap_or(0);
        if ar == br {
            continue;
        }
        let (lo, hi) = if br < ar { (br, ar) } else { (ar, br) };
        if lo.is_ascii_uppercase() && hi == lo + (b'a' - b'A') {
            continue;
        }
        return false;
    }
    if a.first() != Some(&b'.') && b.len() > a.len() && b.get(off - 1) != Some(&b'.') {
        return false;
    }
    true
}

fn dns_key(c: u8) -> u8 {
    if c == b'.' { 0 } else { c.to_ascii_lowercase() }
}

/// dnsCompare: from the right, ASCII case folded, `.` lowest.
fn dns_compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (mut ia, mut ib) = (a.len(), b.len());
    while ia > 0 && ib > 0 {
        let (x, y) = (
            dns_key(a.get(ia - 1).copied().unwrap_or(0)),
            dns_key(b.get(ib - 1).copied().unwrap_or(0)),
        );
        if x != y {
            return x.cmp(&y);
        }
        ia -= 1;
        ib -= 1;
    }
    ia.cmp(&ib)
}

/// trimFirstLabel.
fn trim_first_label(name: &[u8]) -> &[u8] {
    match name.iter().position(|c| *c == b'.') {
        Some(i) => name.get(i..).unwrap_or_default(),
        None => &[],
    }
}

/// dnsConstraints.
struct Dns {
    all: bool,
    permitted: bool,
    set: Vec<Vec<u8>>,
    parents: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Dns {
    fn new(l: &[Vec<u8>], permitted: bool) -> Option<Dns> {
        if l.is_empty() {
            return None;
        }
        if l.iter().any(Vec::is_empty) {
            return Some(Dns {
                all: true,
                permitted: false,
                set: Vec::new(),
                parents: BTreeMap::new(),
            });
        }
        let mut set = l.to_vec();
        sort_and_prune(&mut set, |a, b| dns_compare(a, b), |a, b| dns_has_suffix(a, b));
        let mut parents = BTreeMap::new();
        if !permitted {
            for name in &set {
                let t = trim_first_label(name);
                if !t.is_empty() {
                    parents.insert(t.to_vec(), name.clone());
                }
            }
        }
        Some(Dns {
            all: false,
            permitted,
            set,
            parents,
        })
    }

    fn query(&self, s: &[u8]) -> Option<Vec<u8>> {
        if self.all {
            return Some(Vec::new());
        }
        if let Some(c) = search(
            &self.set,
            s,
            |c, s| dns_compare(c, s),
            |c, s| dns_has_suffix(c, s),
        ) {
            return Some(c.clone());
        }
        if !self.permitted && s.first() == Some(&b'*') {
            return self.parents.get(trim_first_label(s)).cloned();
        }
        None
    }
}

/// emailConstraints.
struct Email {
    dns: Option<Dns>,
    full: BTreeSet<Mailbox>,
}

impl Email {
    fn new(l: &[Vec<u8>], permitted: bool) -> Option<Email> {
        if l.is_empty() {
            return None;
        }
        let mut full = BTreeSet::new();
        let mut domains = Vec::new();
        for c in l {
            if !c.contains(&b'@') {
                domains.push(c.clone());
                continue;
            }
            let Some(mut m) = parse_mailbox(c) else {
                continue;
            };
            m.domain.make_ascii_lowercase();
            full.insert(m);
        }
        Some(Email {
            dns: if domains.is_empty() {
                None
            } else {
                Dns::new(&domains, permitted)
            },
            full,
        })
    }

    fn query(&self, s: &Mailbox) -> Option<Vec<u8>> {
        if self.full.contains(s) {
            return Some(s.text());
        }
        self.dns.as_ref()?.query(&s.domain)
    }
}

/// ipConstraints.
struct Ips {
    v4: Vec<(Vec<u8>, Vec<u8>)>,
    v6: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Ips {
    fn new(l: &[(Vec<u8>, Vec<u8>)]) -> Option<Ips> {
        if l.is_empty() {
            return None;
        }
        let (mut v4, mut v6): (Vec<_>, Vec<_>) = l.iter().cloned().partition(|(ip, _)| ip.len() == 4);
        let cmp = |a: &(Vec<u8>, Vec<u8>), b: &(Vec<u8>, Vec<u8>)| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1));
        let subset = |a: &(Vec<u8>, Vec<u8>), b: &(Vec<u8>, Vec<u8>)| {
            if !contains(&a.0, &a.1, &b.0) {
                return false;
            }
            let broadcast: Vec<u8> =
                b.0.iter()
                    .enumerate()
                    .map(|(i, x)| x | !b.1.get(i).copied().unwrap_or(0))
                    .collect();
            contains(&a.0, &a.1, &broadcast)
        };
        sort_and_prune(&mut v4, cmp, subset);
        sort_and_prune(&mut v6, cmp, subset);
        Some(Ips { v4, v6 })
    }

    fn query(&self, ip: &[u8]) -> Option<String> {
        let set = if ip.len() == 4 { &self.v4 } else { &self.v6 };
        search(
            set,
            ip,
            |c, t: &[u8]| c.0.as_slice().cmp(t),
            |c, t| contains(&c.0, &c.1, t),
        )
        .map(|(ip, mask)| ipnet_string(ip, mask))
    }
}

/// A URI SAN parsed for matching (parsedURI): the URI's text and its host.
struct ParsedUri {
    text: String,
    domain: Vec<u8>,
}

/// net.SplitHostPort's host.
fn split_host_port(hp: &[u8]) -> Result<&[u8], String> {
    let addr_err = |why: &str| format!("address {}: {why}", String::from_utf8_lossy(hp));
    let i = hp
        .iter()
        .rposition(|c| *c == b':')
        .ok_or_else(|| addr_err("missing port in address"))?;
    let (host, j, k) = if hp.first() == Some(&b'[') {
        let end = hp
            .iter()
            .position(|c| *c == b']')
            .ok_or_else(|| addr_err("missing ']' in address"))?;
        if end + 1 == hp.len() {
            return Err(addr_err("missing port in address"));
        }
        if end + 1 != i {
            if hp.get(end + 1) == Some(&b':') {
                return Err(addr_err("too many colons in address"));
            }
            return Err(addr_err("missing port in address"));
        }
        (hp.get(1..end).unwrap_or_default(), 1, end + 1)
    } else {
        let host = hp.get(..i).unwrap_or_default();
        if host.contains(&b':') {
            return Err(addr_err("too many colons in address"));
        }
        (host, 0, 0)
    };
    if hp.get(j..).unwrap_or_default().contains(&b'[') {
        return Err(addr_err("unexpected '[' in address"));
    }
    if hp.get(k..).unwrap_or_default().contains(&b']') {
        return Err(addr_err("unexpected ']' in address"));
    }
    Ok(host)
}

/// parseURIs.
fn parse_uris(c: &Certificate) -> Result<Vec<ParsedUri>, String> {
    let mut out = Vec::new();
    for (text, raw_host) in c.uris.iter().zip(&c.uri_hosts) {
        let mut host = raw_host.to_ascii_lowercase();
        if host.is_empty() {
            return Err(format!(
                "URI with empty host ({}) cannot be matched against constraints",
                quote(text.as_bytes())
            ));
        }
        if host.contains(&b':') && host.last() != Some(&b']') {
            host = split_host_port(raw_host)
                .map_err(|e| format!("cannot parse URI host {}: {e}", quote(raw_host)))?
                .to_vec();
        }
        let bracketed = host.first() == Some(&b'[') && host.last() == Some(&b']');
        if shards_dockerfile::url::parse_addr(&host).is_ok() || bracketed {
            return Err(format!(
                "URI with IP ({}) cannot be matched against constraints",
                quote(text.as_bytes())
            ));
        }
        out.push(ParsedUri {
            text: text.clone(),
            domain: host,
        });
    }
    Ok(out)
}

/// parseMailboxes.
fn parse_mailboxes(c: &Certificate) -> Result<Vec<Mailbox>, String> {
    c.emails
        .iter()
        .map(|e| {
            let mut m = parse_mailbox(e.as_bytes())
                .ok_or_else(|| format!("cannot parse rfc822Name {}", quote(e.as_bytes())))?;
            m.domain.make_ascii_lowercase();
            Ok(m)
        })
        .collect()
}

/// One certificate's constraints (chainConstraints' node).
struct Constraints {
    index: usize,
    ip: (Option<Ips>, Option<Ips>),
    dns: (Option<Dns>, Option<Dns>),
    uri: (Option<Dns>, Option<Dns>),
    email: (Option<Email>, Option<Email>),
}

/// checkConstraints' errors.
fn not_permitted(kind: &str, p: &[u8]) -> String {
    format!("{kind} {} is not permitted by any constraint", quote(p))
}

fn excluded(kind: &str, p: &[u8], by: &[u8]) -> String {
    format!("{kind} {} is excluded by constraint {}", quote(p), quote(by))
}

impl Constraints {
    fn of(index: usize, nc: &NameConstraints) -> Constraints {
        Constraints {
            index,
            ip: (Ips::new(&nc.permitted_ips), Ips::new(&nc.excluded_ips)),
            dns: (
                Dns::new(&nc.permitted_dns, true),
                Dns::new(&nc.excluded_dns, false),
            ),
            uri: (
                Dns::new(&nc.permitted_uris, true),
                Dns::new(&nc.excluded_uris, false),
            ),
            email: (
                Email::new(&nc.permitted_emails, true),
                Email::new(&nc.excluded_emails, false),
            ),
        }
    }

    /// chainConstraints.check.
    fn check(&self, c: &Certificate, uris: &[ParsedUri], emails: &[Mailbox]) -> Result<(), String> {
        for ip in &c.ips {
            let p = ip_string(ip);
            if let Some(perm) = &self.ip.0
                && perm.query(ip).is_none()
            {
                return Err(not_permitted("IP address", p.as_bytes()));
            }
            if let Some(by) = self.ip.1.as_ref().and_then(|x| x.query(ip)) {
                return Err(excluded("IP address", p.as_bytes(), by.as_bytes()));
            }
        }
        for d in &c.dns_names {
            let d = d.as_bytes();
            if !domain_name_valid(d, false) {
                return Err(format!("x509: cannot parse dnsName {}", quote(d)));
            }
            if let Some(perm) = &self.dns.0
                && perm.query(d).is_none()
            {
                return Err(not_permitted("DNS name", d));
            }
            if let Some(by) = self.dns.1.as_ref().and_then(|x| x.query(d)) {
                return Err(excluded("DNS name", d, &by));
            }
        }
        for u in uris {
            if !domain_name_valid(&u.domain, false) {
                return Err(format!(
                    "x509: internal error: URI SAN {} failed to parse",
                    quote(u.text.as_bytes())
                ));
            }
            if let Some(perm) = &self.uri.0
                && perm.query(&u.domain).is_none()
            {
                return Err(not_permitted("URI", u.text.as_bytes()));
            }
            if let Some(by) = self.uri.1.as_ref().and_then(|x| x.query(&u.domain)) {
                return Err(excluded("URI", u.text.as_bytes(), &by));
            }
        }
        for e in emails {
            if !domain_name_valid(&e.domain, false) {
                return Err(format!("x509: cannot parse rfc822Name {}", quote(&e.text())));
            }
            if let Some(perm) = &self.email.0
                && perm.query(e).is_none()
            {
                return Err(not_permitted("email address", &e.text()));
            }
            if let Some(by) = self.email.1.as_ref().and_then(|x| x.query(e)) {
                return Err(excluded("email address", &e.text(), &by));
            }
        }
        Ok(())
    }
}

/// checkChainConstraints: each certificate's names against the constraints of those
/// above it.
pub fn check_chain_constraints(chain: &[&Certificate]) -> Result<(), String> {
    let list: Vec<Constraints> = chain
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.name_constraints.as_ref().map(|nc| Constraints::of(i, nc)))
        .collect();
    if list.is_empty() {
        return Ok(());
    }
    let mut current = 0;
    for (i, c) in chain.iter().enumerate() {
        if c.extension(OID_SAN).is_none() {
            continue;
        }
        if list.get(current).is_some_and(|cc| i >= cc.index) {
            while list.get(current).is_some_and(|cc| cc.index <= i) {
                if current + 1 >= list.len() {
                    return Ok(());
                }
                current += 1;
            }
        }
        let uris = parse_uris(c)?;
        let emails = parse_mailboxes(c)?;
        for n in list.get(current..).unwrap_or_default() {
            n.check(c, &uris, &emails)?;
        }
    }
    Ok(())
}

// --- Certificate policies (policiesValid).

/// anyPolicy, 2.5.29.32.0.
const ANY_POLICY: &[u8] = &[0x55, 0x1d, 0x20, 0x00];

struct Node {
    valid: Vec<u8>,
    expected: Vec<Vec<u8>>,
    parents: BTreeSet<usize>,
    children: BTreeSet<usize>,
}

/// policyGraph: nodes in an arena, each stratum's by policy.
struct Graph {
    nodes: Vec<Node>,
    strata: Vec<BTreeMap<Vec<u8>, usize>>,
    parent_index: BTreeMap<Vec<u8>, Vec<usize>>,
    depth: usize,
}

impl Graph {
    fn new() -> Graph {
        let root = Node {
            valid: ANY_POLICY.to_vec(),
            expected: vec![ANY_POLICY.to_vec()],
            parents: BTreeSet::new(),
            children: BTreeSet::new(),
        };
        let mut s = BTreeMap::new();
        s.insert(ANY_POLICY.to_vec(), 0);
        Graph {
            nodes: vec![root],
            strata: vec![s],
            parent_index: BTreeMap::new(),
            depth: 0,
        }
    }

    /// newPolicyGraphNode.
    fn node(&mut self, valid: Vec<u8>, parents: &[usize]) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node {
            expected: vec![valid.clone()],
            valid,
            parents: parents.iter().copied().collect(),
            children: BTreeSet::new(),
        });
        for &p in parents {
            if let Some(n) = self.nodes.get_mut(p) {
                n.children.insert(id);
            }
        }
        id
    }

    fn insert(&mut self, id: usize) {
        let key = self.nodes.get(id).map(|n| n.valid.clone()).unwrap_or_default();
        if let Some(s) = self.strata.get_mut(self.depth) {
            s.insert(key, id);
        }
    }

    fn parents_with_expected(&self, policy: &[u8]) -> Vec<usize> {
        if self.depth == 0 {
            return Vec::new();
        }
        self.parent_index.get(policy).cloned().unwrap_or_default()
    }

    fn parent_with_any(&self) -> Option<usize> {
        if self.depth == 0 {
            return None;
        }
        self.strata.get(self.depth - 1)?.get(ANY_POLICY).copied()
    }

    fn leaf(&self, policy: &[u8]) -> Option<usize> {
        self.strata.get(self.depth)?.get(policy).copied()
    }

    fn delete_leaf(&mut self, policy: &[u8]) {
        let Some(id) = self.leaf(policy) else {
            return;
        };
        let (parents, children) = match self.nodes.get(id) {
            Some(n) => (n.parents.clone(), n.children.clone()),
            None => return,
        };
        for p in parents {
            if let Some(n) = self.nodes.get_mut(p) {
                n.children.remove(&id);
            }
        }
        for c in children {
            if let Some(n) = self.nodes.get_mut(c) {
                n.parents.remove(&id);
            }
        }
        if let Some(s) = self.strata.get_mut(self.depth) {
            s.remove(policy);
        }
    }

    fn prune(&mut self) {
        for i in (1..self.depth).rev() {
            let ids: Vec<(Vec<u8>, usize)> = self
                .strata
                .get(i)
                .map(|s| s.iter().map(|(k, v)| (k.clone(), *v)).collect())
                .unwrap_or_default();
            for (key, id) in ids {
                let Some(n) = self.nodes.get(id) else { continue };
                if !n.children.is_empty() {
                    continue;
                }
                let parents = n.parents.clone();
                for p in parents {
                    if let Some(pn) = self.nodes.get_mut(p) {
                        pn.children.remove(&id);
                    }
                }
                if let Some(s) = self.strata.get_mut(i) {
                    s.remove(&key);
                }
            }
        }
    }

    fn incr_depth(&mut self) {
        let mut index: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
        if let Some(s) = self.strata.get(self.depth) {
            for &id in s.values() {
                if let Some(n) = self.nodes.get(id) {
                    for e in &n.expected {
                        index.entry(e.clone()).or_default().push(id);
                    }
                }
            }
        }
        self.parent_index = index;
        self.depth += 1;
        self.strata.push(BTreeMap::new());
    }

    /// validPolicyNodes' policies.
    fn valid_policies(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for s in self.strata.iter().rev() {
            for &id in s.values() {
                let Some(n) = self.nodes.get(id) else { continue };
                if n.valid == ANY_POLICY || n.parents.len() != 1 {
                    continue;
                }
                if n.parents
                    .iter()
                    .all(|p| self.nodes.get(*p).is_some_and(|pn| pn.valid == ANY_POLICY))
                {
                    out.push(n.valid.clone());
                }
            }
        }
        out
    }
}

/// policiesValid with no policies asked (VerifyOptions.CertificatePolicies empty).
pub fn policies_valid(chain: &[&Certificate]) -> bool {
    if chain.len() == 1 {
        return true;
    }
    let n = chain.len() as i64 - 1;
    let mut pg = Some(Graph::new());
    let (mut inhibit_any, mut explicit, mut mapping) = (n + 1, n + 1, n + 1);
    for i in (0..chain.len() - 1).rev() {
        let Some(cert) = chain.get(i) else { continue };
        let self_signed = cert.raw_issuer == cert.raw_subject;
        if cert.policies.is_empty() {
            pg = None;
        }
        if explicit == 0 && pg.is_none() {
            return false;
        }
        if let Some(g) = pg.as_mut() {
            g.incr_depth();
            let mut has_any = false;
            for policy in &cert.policies {
                if policy.as_slice() == ANY_POLICY {
                    has_any = true;
                    continue;
                }
                let mut parents = g.parents_with_expected(policy);
                if parents.is_empty()
                    && let Some(a) = g.parent_with_any()
                {
                    parents = vec![a];
                }
                if !parents.is_empty() {
                    let id = g.node(policy.clone(), &parents);
                    g.insert(id);
                }
            }
            let i64i = i as i64;
            if has_any && (inhibit_any > 0 || (n - i64i < n && self_signed)) {
                let mut missing: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
                let leaves = g.strata.get(g.depth).cloned().unwrap_or_default();
                let parents: Vec<usize> = g
                    .strata
                    .get(g.depth - 1)
                    .map(|s| s.values().copied().collect())
                    .unwrap_or_default();
                for p in parents {
                    let expected = g.nodes.get(p).map(|x| x.expected.clone()).unwrap_or_default();
                    for e in expected {
                        if !leaves.contains_key(&e) {
                            missing.entry(e).or_default().push(p);
                        }
                    }
                }
                for (oid, parents) in missing {
                    let id = g.node(oid, &parents);
                    g.insert(id);
                }
            }
            g.prune();
            if i != 0 && !cert.policy_mappings.is_empty() {
                let mut mappings: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
                for (issuer, subject) in &cert.policy_mappings {
                    if mapping > 0 {
                        if issuer.as_slice() == ANY_POLICY || subject.as_slice() == ANY_POLICY {
                            return false;
                        }
                        match mappings.iter_mut().find(|(k, _)| k == issuer) {
                            Some((_, v)) => v.push(subject.clone()),
                            None => mappings.push((issuer.clone(), vec![subject.clone()])),
                        }
                    } else {
                        g.delete_leaf(issuer);
                        g.prune();
                    }
                }
                for (issuer, subjects) in mappings {
                    if let Some(m) = g.leaf(&issuer) {
                        if let Some(node) = g.nodes.get_mut(m) {
                            node.expected = subjects;
                        }
                    } else if let Some(any) = g.leaf(ANY_POLICY) {
                        let id = g.node(issuer, &[any]);
                        if let Some(node) = g.nodes.get_mut(id) {
                            node.expected = subjects;
                        }
                        g.insert(id);
                    }
                }
            }
        }
        if i != 0 {
            if !self_signed {
                explicit = (explicit - 1).max(0);
                mapping = (mapping - 1).max(0);
                inhibit_any = (inhibit_any - 1).max(0);
            }
            if let Some(v) = cert.require_explicit_policy.filter(|v| *v >= 0 && *v < explicit) {
                explicit = v;
            }
            if let Some(v) = cert.inhibit_policy_mapping.filter(|v| *v >= 0 && *v < mapping) {
                mapping = v;
            }
            if let Some(v) = cert.inhibit_any_policy.filter(|v| *v >= 0 && *v < inhibit_any) {
                inhibit_any = v;
            }
        }
    }
    if explicit > 0 {
        explicit -= 1;
    }
    if chain
        .first()
        .is_some_and(|c| c.require_explicit_policy == Some(0))
    {
        explicit = 0;
    }
    let mut valid: Vec<Vec<u8>> = Vec::new();
    if let Some(g) = &pg {
        valid = g.valid_policies();
        if g.leaf(ANY_POLICY).is_some() {
            valid.push(ANY_POLICY.to_vec());
        }
    }
    !(explicit == 0 && valid.is_empty())
}
