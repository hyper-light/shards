//! The resolver a guest on a network of its own asks (D46), as Docker's embedded DNS answers
//! a container on a user-defined network, to a microVM on one (moby libnetwork resolver.go; measured on Docker
//! Engine 29.3.1): each member's names (its microVM's name, its aliases, its host name)
//! as A records with a TTL of 600, whatever their case; its address back to its name and
//! the network's (`name.network.`); no AAAA for a name it holds, its IPv4 alone; and,
//! upstream being denied (D31's default deny, as on Docker's `--internal` networks),
//! SERVFAIL for a name it does not hold. Queries are RFC 1035's.

use std::collections::HashMap;
use std::net::Ipv4Addr;

/// The TTL Docker's embedded DNS gives its answers (libnetwork resolver.go, respTTL).
const TTL: u32 = 600;

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

const NOERROR: u8 = 0;
const FORMERR: u8 = 1;
const SERVFAIL: u8 = 2;
const NOTIMP: u8 = 4;

/// The names a network's members answer to, and the network's name.
#[derive(Debug, Default, Clone)]
pub struct Names {
    network: String,
    /// Each name, lowered, to the addresses it has.
    by_name: HashMap<String, Vec<Ipv4Addr>>,
    /// Each address to its member's first name, for PTR.
    by_addr: HashMap<Ipv4Addr, String>,
}

impl Names {
    /// The table a `NET_NAMES` message carries: the network's name, then each name with
    /// its address, each name its length's byte first. None for a malformed one.
    pub fn decode(payload: &[u8]) -> Option<Names> {
        let mut rest = payload;
        let word = |rest: &mut &[u8]| -> Option<String> {
            let (&len, tail) = rest.split_first()?;
            let (name, tail) = tail.split_at_checked(usize::from(len))?;
            *rest = tail;
            String::from_utf8(name.to_vec()).ok()
        };
        let network = word(&mut rest)?;
        let mut names = Names {
            network,
            ..Names::default()
        };
        while !rest.is_empty() {
            let name = word(&mut rest)?;
            let (&[a, b, c, d], tail) = rest.split_first_chunk::<4>()?;
            rest = tail;
            names.add(&name, Ipv4Addr::new(a, b, c, d));
        }
        Some(names)
    }

    /// `name` at `ip`; the first name an address is given is its PTR's.
    pub fn add(&mut self, name: &str, ip: Ipv4Addr) {
        let ips = self.by_name.entry(name.to_ascii_lowercase()).or_default();
        if !ips.contains(&ip) {
            ips.push(ip);
        }
        self.by_addr.entry(ip).or_insert_with(|| name.to_string());
    }

    /// The table as a `NET_NAMES` message carries it.
    pub fn encode(network: &str, entries: &[(String, Ipv4Addr)]) -> Vec<u8> {
        let mut out = Vec::new();
        let word = |out: &mut Vec<u8>, w: &str| {
            let w = w.get(..w.len().min(255)).unwrap_or_default();
            out.push(u8::try_from(w.len()).unwrap_or(u8::MAX));
            out.extend_from_slice(w.as_bytes());
        };
        word(&mut out, network);
        for (name, ip) in entries {
            word(&mut out, name);
            out.extend_from_slice(&ip.octets());
        }
        out
    }

    /// The answer to the query `q`, or none for what is not a query to answer.
    pub fn answer(&self, q: &[u8]) -> Option<Vec<u8>> {
        let (&[id0, id1, f0, _f1], rest) = q.split_first_chunk::<4>()?;
        // A response, or a header cut short, is not answered.
        if f0 & 0x80 != 0 {
            return None;
        }
        let (&[qd0, qd1, ..], _) = rest.split_first_chunk::<8>()?;
        let opcode = (f0 >> 3) & 0x0f;
        let rd = f0 & 0x01;
        let reply = |rcode: u8, question: &[u8], answers: &[Vec<u8>]| {
            let mut out = vec![id0, id1, 0x80 | (opcode << 3) | rd, 0x80 | rcode];
            let qd: u16 = if question.is_empty() { 0 } else { 1 };
            out.extend_from_slice(&qd.to_be_bytes());
            out.extend_from_slice(&u16::try_from(answers.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(question);
            for a in answers {
                out.extend_from_slice(a);
            }
            out
        };
        if opcode != 0 {
            return Some(reply(NOTIMP, &[], &[]));
        }
        if u16::from_be_bytes([qd0, qd1]) != 1 {
            return Some(reply(FORMERR, &[], &[]));
        }
        let body = q.get(12..)?;
        let Some((labels, end)) = qname(body) else {
            return Some(reply(FORMERR, &[], &[]));
        };
        let Some(&[t0, t1, c0, c1]) = body.get(end..).and_then(|b| b.first_chunk::<4>()) else {
            return Some(reply(FORMERR, &[], &[]));
        };
        let question = body.get(..end + 4).unwrap_or_default();
        let (qtype, qclass) = (u16::from_be_bytes([t0, t1]), u16::from_be_bytes([c0, c1]));
        let name = labels.join(".").to_ascii_lowercase();
        // The answers name the question's name by a pointer to it (offset 12).
        let record = |kind: u16, data: &[u8]| {
            let mut r = vec![0xc0, 12];
            r.extend_from_slice(&kind.to_be_bytes());
            r.extend_from_slice(&CLASS_IN.to_be_bytes());
            r.extend_from_slice(&TTL.to_be_bytes());
            r.extend_from_slice(&u16::try_from(data.len()).unwrap_or(0).to_be_bytes());
            r.extend_from_slice(data);
            r
        };
        if qclass != CLASS_IN {
            return Some(reply(SERVFAIL, question, &[]));
        }
        if qtype == TYPE_PTR {
            let Some(ip) = ptr_addr(&name) else {
                return Some(reply(SERVFAIL, question, &[]));
            };
            return Some(match self.by_addr.get(&ip) {
                Some(member) => {
                    let mut data = Vec::new();
                    for label in [member.as_str(), self.network.as_str()] {
                        let label = label.get(..label.len().min(63)).unwrap_or_default();
                        data.push(u8::try_from(label.len()).unwrap_or(0));
                        data.extend_from_slice(label.as_bytes());
                    }
                    data.push(0);
                    reply(NOERROR, question, &[record(TYPE_PTR, &data)])
                }
                None => reply(SERVFAIL, question, &[]),
            });
        }
        let Some(ips) = self.by_name.get(&name) else {
            return Some(reply(SERVFAIL, question, &[]));
        };
        let answers: Vec<Vec<u8>> = if qtype == TYPE_A {
            ips.iter().map(|ip| record(TYPE_A, &ip.octets())).collect()
        } else {
            // AAAA and the rest: the name is there, with none of that type.
            let _ = TYPE_AAAA;
            Vec::new()
        };
        Some(reply(NOERROR, question, &answers))
    }
}

/// A question's name: its labels, and where it ends past its zero byte. Pointers are not
/// a question's to use.
fn qname(body: &[u8]) -> Option<(Vec<String>, usize)> {
    let mut labels = Vec::new();
    let mut at = 0;
    loop {
        let len = usize::from(*body.get(at)?);
        if len == 0 {
            return Some((labels, at + 1));
        }
        if len > 63 {
            return None;
        }
        let label = body.get(at + 1..at + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        at += 1 + len;
        if at > 255 {
            return None;
        }
    }
}

/// A query's one question's name, lowered: what an agent asks of the resolver.
pub fn query_name(q: &[u8]) -> Option<String> {
    if q.get(2)? & 0x80 != 0 || u16::from_be_bytes([*q.get(4)?, *q.get(5)?]) != 1 {
        return None;
    }
    let (labels, _) = qname(q.get(12..)?)?;
    Some(labels.join(".").to_ascii_lowercase())
}

/// REFUSED (RFC 1035 §4.1.1, RCODE 5) for query `q`, its question kept: a name no grant
/// lets the guest ask past the microVM (default deny), said at once, so that no resolver
/// waits out its timeouts.
pub fn refused(q: &[u8]) -> Option<Vec<u8>> {
    let (&[id0, id1, f0, _], _) = q.split_first_chunk::<4>()?;
    let (_, end) = qname(q.get(12..)?)?;
    let question = q.get(12..12 + end + 4)?;
    let mut out = vec![id0, id1, 0x80 | (f0 & 0x79), 0x80 | 5, 0, 1, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(question);
    Some(out)
}

/// A response's question name, lowered and without its last dot, and the addresses of
/// its answers' A records (RFC 1035 §4.1): what a name granted to agents resolved to
/// (D59). None for what is no response of one question.
pub fn a_records(msg: &[u8]) -> Option<(String, Vec<Ipv4Addr>)> {
    let be16 = |at: usize| Some(u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]));
    if msg.get(2)? & 0x80 == 0 || be16(4)? != 1 {
        return None;
    }
    let (labels, end) = qname(msg.get(12..)?)?;
    let name = labels.join(".").to_ascii_lowercase();
    // Past the question's type and class.
    let mut at = 12 + end + 4;
    let mut out = Vec::new();
    for _ in 0..be16(6)? {
        // An owner name: labels, ending in a zero byte or a two-byte pointer.
        loop {
            let len = *msg.get(at)?;
            if len & 0xc0 == 0xc0 {
                at += 2;
                break;
            }
            at += 1 + usize::from(len);
            if len == 0 {
                break;
            }
        }
        let (kind, length) = (be16(at)?, usize::from(be16(at + 8)?));
        at += 10;
        if kind == TYPE_A && length == 4 {
            let &[a, b, c, d] = msg.get(at..at + 4)?.first_chunk::<4>()?;
            out.push(Ipv4Addr::new(a, b, c, d));
        }
        at += length;
    }
    Some((name, out))
}

/// The address `d.c.b.a.in-addr.arpa` names.
fn ptr_addr(name: &str) -> Option<Ipv4Addr> {
    let rest = name.strip_suffix(".in-addr.arpa")?;
    let parts: Vec<u8> = rest.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [d, c, b, a] = parts.as_slice() else {
        return None;
    };
    Some(Ipv4Addr::new(*a, *b, *c, *d))
}

/// One DNS-over-TCP connection between the guest and the host's resolver (RFC 7766), as
/// a guest's UDP questions are (D59): each whole question (RFC 1035 §4.2.2, its length's
/// two bytes first) goes to the host's resolver only if a grant names it, and is REFUSED
/// otherwise; each answer goes to the guest once it is whole, its A records learned. A
/// refusal goes where no answer is part way, as RFC 7766 §7 lets answers come in any order.
#[derive(Debug)]
pub struct Stream {
    /// Whether any name may be asked; else these alone, lowered.
    any: bool,
    names: Vec<String>,
    /// The guest's bytes of a question not yet whole, and the host's of an answer.
    asked: Vec<u8>,
    answer: Vec<u8>,
    /// Refusals, framed, waiting for no answer to be part way.
    refusals: Vec<u8>,
    /// What answers of granted names resolved to, for the stack to learn.
    pub learned: Vec<(String, Vec<Ipv4Addr>)>,
}

/// The whole messages at the front of `buf`, taken from it, each with its length's bytes.
fn framed(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(&[a, b]) = buf.first_chunk::<2>() {
        let end = 2 + usize::from(u16::from_be_bytes([a, b]));
        if buf.len() < end {
            break;
        }
        out.push(buf.drain(..end).collect());
    }
    out
}

impl Stream {
    pub fn new(any: bool, names: Vec<String>) -> Stream {
        Stream {
            any,
            names,
            asked: Vec::new(),
            answer: Vec::new(),
            refusals: Vec::new(),
            learned: Vec::new(),
        }
    }

    /// The guest's `bytes`: what of them goes to the host, whole granted questions.
    pub fn from_guest(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.asked.extend_from_slice(bytes);
        let mut out = Vec::new();
        for m in framed(&mut self.asked) {
            let q = m.get(2..).unwrap_or_default();
            let granted = self.any || query_name(q).is_some_and(|n| self.names.contains(&n));
            if granted {
                out.extend_from_slice(&m);
            } else if let Some(no) = refused(q)
                && let Ok(len) = u16::try_from(no.len())
            {
                self.refusals.extend_from_slice(&len.to_be_bytes());
                self.refusals.extend_from_slice(&no);
            }
        }
        out
    }

    /// The host's `bytes`, to the guest: each answer whole, once it is, read for what it
    /// resolved, then the refusals that waited for its end.
    pub fn from_host(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.answer.extend_from_slice(bytes);
        let mut out = Vec::new();
        for m in framed(&mut self.answer) {
            if let Some(found) = a_records(m.get(2..).unwrap_or_default()) {
                self.learned.push(found);
            }
            out.extend_from_slice(&m);
            out.append(&mut self.refusals);
        }
        out
    }

    /// The bytes held for the guest: refusals, and an answer not yet whole.
    pub fn held(&self) -> usize {
        self.refusals.len() + self.answer.len()
    }

    /// The refusals that may go now: none while an answer is part way.
    pub fn refusals(&mut self) -> Vec<u8> {
        if !self.answer.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut self.refusals)
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    fn names() -> Names {
        let entries = vec![
            ("web".to_string(), Ipv4Addr::new(172, 19, 0, 2)),
            ("alias".to_string(), Ipv4Addr::new(172, 19, 0, 2)),
            ("alias".to_string(), Ipv4Addr::new(172, 19, 0, 3)),
        ];
        Names::decode(&Names::encode("net1", &entries)).unwrap()
    }

    /// As Docker's embedded DNS answered on Docker Engine 29.3.1: A records of TTL 600,
    /// whatever the case; an alias's every member; no AAAA; PTR as name.network.; and
    /// SERVFAIL for what it does not hold, upstream denied.
    /// A forwarded response's name and A records, whatever its owner names are written
    /// as (a pointer, labels) and whatever other records come between.
    #[test]
    fn a_responses_addresses_are_read() {
        let mut m = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 3, 0, 0, 0, 0];
        m.extend_from_slice(b"\x03MCP\x07example\x00\x00\x01\x00\x01");
        // A CNAME by pointer, then two As, one by labels.
        m.extend_from_slice(&[0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 0x0c]);
        m.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 9]);
        m.extend_from_slice(b"\x01x\x00");
        m.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 10]);
        assert_eq!(
            a_records(&m),
            Some((
                "mcp.example".to_string(),
                vec![Ipv4Addr::new(203, 0, 113, 9), Ipv4Addr::new(203, 0, 113, 10)]
            ))
        );
        // The query asked it, and is refused with its question kept.
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x03MCP\x07example\x00\x00\x01\x00\x01");
        assert_eq!(query_name(&q).as_deref(), Some("mcp.example"));
        let no = refused(&q).unwrap();
        assert_eq!(
            (&no[..2], no[2] & 0x80, no[3] & 0x0f),
            (&[0x12, 0x34][..], 0x80, 5)
        );
        assert_eq!(&no[12..], &q[12..]);
        // A query is no answer; a cut one is nothing.
        let mut q = m.clone();
        q[2] = 0x01;
        assert_eq!(a_records(&q), None);
        assert_eq!(a_records(&m[..m.len() - 2]), None);
    }

    #[test]
    fn names_are_answered_as_dockers_embedded_dns_answers() {
        let n = names();
        let a = n.answer(&query("WEB", TYPE_A)).unwrap();
        assert_eq!(&a[..2], &[0x12, 0x34]);
        assert_eq!(a[3] & 0x0f, NOERROR);
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 1);
        assert_eq!(&a[a.len() - 10..a.len() - 6], &600u32.to_be_bytes());
        assert_eq!(&a[a.len() - 4..], &[172, 19, 0, 2]);
        let a = n.answer(&query("alias", TYPE_A)).unwrap();
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 2);
        let a = n.answer(&query("web", TYPE_AAAA)).unwrap();
        assert_eq!((a[3] & 0x0f, u16::from_be_bytes([a[6], a[7]])), (NOERROR, 0));
        let a = n.answer(&query("example.com", TYPE_A)).unwrap();
        assert_eq!(a[3] & 0x0f, SERVFAIL);
        let a = n.answer(&query("2.0.19.172.in-addr.arpa", TYPE_PTR)).unwrap();
        assert!(a.ends_with(b"\x03web\x04net1\x00"), "{a:?}");
        assert_eq!(n.answer(&a), None, "a response is not answered");
        assert!(n.answer(&[1, 2]).is_none());
    }

    fn framed_msg(m: &[u8]) -> Vec<u8> {
        [&(m.len() as u16).to_be_bytes()[..], m].concat()
    }

    /// An answer to `q` of one A record, `ip`.
    fn answered(q: &[u8], ip: [u8; 4]) -> Vec<u8> {
        let mut r = q.to_vec();
        r[2] |= 0x80;
        r[7] = 1;
        r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        r.extend_from_slice(&ip);
        r
    }

    #[test]
    fn tcp_questions_go_on_whole_and_only_as_granted() {
        let mut s = Stream::new(false, vec!["mcp.example".into()]);
        let granted = framed_msg(&query("MCP.example", TYPE_A));
        let other = framed_msg(&query("api.example", TYPE_A));
        // A question split across segments goes on once it is whole.
        assert!(s.from_guest(&granted[..5]).is_empty());
        assert_eq!(s.from_guest(&granted[5..]), granted);
        // Another name: not to the host; REFUSED, at once, with nothing part way.
        assert!(s.from_guest(&other).is_empty());
        let no = s.refusals();
        assert_eq!(no, framed_msg(&refused(&query("api.example", TYPE_A)).unwrap()));
        assert_eq!(no[2 + 3] & 0x0f, 5);
        assert!(s.refusals().is_empty());
        // Any name, granted any.
        let mut any = Stream::new(true, Vec::new());
        assert_eq!(any.from_guest(&other), other);
    }

    #[test]
    fn tcp_refusals_wait_for_an_answer_to_end_and_answers_are_learned() {
        let mut s = Stream::new(false, vec!["mcp.example".into()]);
        let q = query("mcp.example", TYPE_A);
        s.from_guest(&framed_msg(&q));
        let answer = framed_msg(&answered(&q, [192, 0, 2, 7]));
        // Part of the answer: held, whole messages alone go to the guest.
        assert!(s.from_host(&answer[..10]).is_empty());
        assert_eq!(s.held(), 10);
        // A refusal now waits for the answer's end, then follows it.
        s.from_guest(&framed_msg(&query("api.example", TYPE_A)));
        assert!(s.refusals().is_empty());
        let out = s.from_host(&answer[10..]);
        let no = framed_msg(&refused(&query("api.example", TYPE_A)).unwrap());
        assert_eq!(out, [answer.clone(), no].concat());
        assert_eq!(s.held(), 0);
        assert_eq!(
            s.learned,
            vec![("mcp.example".to_string(), vec![Ipv4Addr::new(192, 0, 2, 7)])]
        );
    }
}
