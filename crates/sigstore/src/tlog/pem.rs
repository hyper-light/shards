//! PEM as Go 1.26's encoding/pem decodes it, public keys as x509.ParsePKIXPublicKey reads
//! them (encoding/asn1), and the keys Rekor's pki/x509 makes of an entry's PEM
//! (NewPublicKey: a key, a certificate, or a chain of up to ten certificates).

use super::gocodec::STD_ENCODING;
use crate::asn1::{self, Fields, Kind, Params, Value};
use crate::x509::{self, Certificate, PublicKey};

/// A PEM block: its type, whether it had headers, its octets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: Vec<u8>,
    pub bytes: Vec<u8>,
}

const PEM_START: &[u8] = b"\n-----BEGIN ";
const PEM_END: &[u8] = b"\n-----END ";
const PEM_END_OF_LINE: &[u8] = b"-----";

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// getLine: a line, trailing spaces and tabs (and a CR before the LF) trimmed; the rest;
/// the octets consumed.
fn get_line(data: &[u8]) -> (&[u8], &[u8], usize) {
    let (mut i, j) = match data.iter().position(|c| *c == b'\n') {
        None => (data.len(), data.len()),
        Some(i) => (i, i + 1),
    };
    if j > i && i > 0 && data.get(i - 1) == Some(&b'\r') {
        i -= 1;
    }
    let mut line = data.get(..i).unwrap_or_default();
    while let Some((&last, head)) = line.split_last() {
        if last == b' ' || last == b'\t' {
            line = head;
        } else {
            break;
        }
    }
    (line, data.get(j..).unwrap_or_default(), j)
}

/// pem.Encode of a block of `kind` without headers: its octets' standard base64 in lines
/// of 64 characters between the BEGIN and END lines.
pub fn encode(kind: &str, bytes: &[u8]) -> Vec<u8> {
    let text = super::gocodec::std_encode(bytes);
    let mut out = format!("-----BEGIN {kind}-----\n").into_bytes();
    for line in text.as_bytes().chunks(64) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(format!("-----END {kind}-----\n").as_bytes());
    out
}

/// pem.Decode.
pub fn decode(data: &[u8]) -> Option<(Block, &[u8])> {
    let mut rest = data;
    let mut end_trailer_index: isize = 0;
    loop {
        let eti = usize::try_from(end_trailer_index)
            .ok()
            .filter(|e| *e <= rest.len())?;
        rest = rest.get(eti..)?;
        let end_index = find(rest, PEM_END)?;
        let mut end_index = isize::try_from(end_index).ok()?;
        end_trailer_index = end_index + isize::try_from(PEM_END.len()).ok()?;
        let begin_index = rfind(rest.get(..usize::try_from(end_index).ok()?)?, PEM_START.get(1..)?);
        let Some(begin_index) = begin_index else {
            continue;
        };
        if begin_index > 0 && rest.get(begin_index - 1) != Some(&b'\n') {
            continue;
        }
        let skip = begin_index + PEM_START.len() - 1;
        rest = rest.get(skip..)?;
        let skip = isize::try_from(skip).ok()?;
        end_index -= skip;
        end_trailer_index -= skip;
        let (type_line, after, consumed) = get_line(rest);
        rest = after;
        let consumed = isize::try_from(consumed).ok()?;
        end_index -= consumed;
        end_trailer_index -= consumed;
        let Some(kind) = type_line.strip_suffix(PEM_END_OF_LINE) else {
            continue;
        };
        let mut headers = false;
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next, consumed) = get_line(rest);
            if !line.contains(&b':') {
                break;
            }
            headers = true;
            rest = next;
            let consumed = isize::try_from(consumed).ok()?;
            end_index -= consumed;
            end_trailer_index -= consumed;
        }
        if headers && end_index < 0 {
            continue;
        }
        let Some(end_trailer) = usize::try_from(end_trailer_index)
            .ok()
            .and_then(|e| rest.get(e..))
        else {
            continue;
        };
        let trailer_len = kind.len() + PEM_END_OF_LINE.len();
        if end_trailer.len() < trailer_len {
            continue;
        }
        let (trailer, rest_of_end_line) = end_trailer.split_at(trailer_len);
        if !trailer.starts_with(kind) || !trailer.ends_with(PEM_END_OF_LINE) {
            continue;
        }
        if !get_line(rest_of_end_line).0.is_empty() {
            continue;
        }
        let mut bytes = Vec::new();
        if end_index > 0 {
            let ei = usize::try_from(end_index).ok()?;
            let b64: Vec<u8> = rest
                .get(..ei)?
                .iter()
                .copied()
                .filter(|c| *c != b' ' && *c != b'\t')
                .collect();
            match STD_ENCODING.decode(&b64) {
                Ok(b) => bytes = b,
                Err(_) => continue,
            }
        }
        let ei = usize::try_from(end_index).ok()?;
        let tail = rest.get(ei + PEM_END.len() - 1..)?;
        let (_, after, _) = get_line(tail);
        return Some((
            Block {
                kind: kind.to_vec(),
                bytes,
            },
            after,
        ));
    }
}

/// A rune of `s` from `at`, as utf8.DecodeRune reads it: the rune (None where invalid)
/// and its width.
fn rune_at(s: &[u8], at: usize) -> (Option<char>, usize) {
    let rest = s.get(at..).unwrap_or_default();
    for w in 1..=4 {
        if let Some(head) = rest.get(..w)
            && let Ok(t) = std::str::from_utf8(head)
        {
            return (t.chars().next(), w);
        }
    }
    (None, 1)
}

/// unicode.IsSpace.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{85}' | '\u{a0}'
    ) || (c as u32 > 0xff && c.is_whitespace())
}

/// bytes.TrimSpace.
pub fn trim_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        let (r, w) = rune_at(s, start);
        if r.is_some_and(is_space) {
            start += w;
        } else {
            break;
        }
    }
    let mut end = s.len();
    while end > start {
        // The last rune: back up over continuation octets.
        let mut b = end - 1;
        while b > start && s.get(b).is_some_and(|c| c & 0xc0 == 0x80) && end - b < 4 {
            b -= 1;
        }
        let (r, w) = rune_at(s, b);
        if b + w == end && r.is_some_and(is_space) {
            end = b;
        } else {
            break;
        }
    }
    s.get(start..end).unwrap_or_default()
}

/// An OID's arcs in DER.
fn oid_der(arcs: &[u32]) -> Vec<u8> {
    let base128 = |v: u64, out: &mut Vec<u8>| {
        let mut groups = vec![(v & 0x7f) as u8];
        let mut v = v >> 7;
        while v > 0 {
            groups.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        out.extend(groups.iter().rev());
    };
    let mut out = Vec::new();
    if let (Some(&a), Some(&b)) = (arcs.first(), arcs.get(1)) {
        base128(u64::from(a) * 40 + u64::from(b), &mut out);
    }
    for &a in arcs.iter().skip(2) {
        base128(u64::from(a), &mut out);
    }
    out
}

/// x509.ParsePKIXPublicKey: Unknown for an X25519 key, which no verifier takes.
pub fn parse_pkix(der: &[u8]) -> Result<PublicKey, String> {
    let parsed = asn1::unmarshal(der, Kind::Struct, &Params::default().named("publicKeyInfo")).and_then(
        |(v, rest)| {
            let Value::Struct { inner, .. } = v else {
                return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
            };
            let mut f = Fields::new(inner);
            let Some(Value::Struct { inner: ai, .. }) =
                f.next(Kind::Struct, &Params::default().named("AlgorithmIdentifier"))?
            else {
                return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
            };
            let Some(Value::BitString { bytes, bit_length }) =
                f.next(Kind::BitString, &Params::default().named("BitString"))?
            else {
                return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
            };
            let mut a = Fields::new(ai);
            let Some(Value::Oid(arcs)) = a.next(Kind::Oid, &Params::default().named("ObjectIdentifier"))?
            else {
                return Err(asn1::Asn1Error("asn1: syntax error: sequence truncated".into()));
            };
            let params = match a.next(Kind::Raw, &Params::default().optional())? {
                Some(Value::Raw { full, .. }) => full.to_vec(),
                _ => Vec::new(),
            };
            Ok((arcs, params, asn1::right_align(bytes, bit_length), rest.len()))
        },
    );
    let (arcs, params, data, rest) = match parsed {
        Ok(p) => p,
        Err(e) => {
            let pkcs1 = asn1::unmarshal(der, Kind::Struct, &Params::default()).and_then(|(v, _)| {
                let Value::Struct { inner, .. } = v else {
                    return Err(asn1::Asn1Error(String::new()));
                };
                let mut f = Fields::new(inner);
                f.next(Kind::BigInt, &Params::default())?;
                f.next(Kind::Int64, &Params::default())?;
                Ok(())
            });
            if pkcs1.is_ok() {
                return Err(
                    "x509: failed to parse public key (use ParsePKCS1PublicKey instead for this key format)"
                        .into(),
                );
            }
            return Err(e.0);
        }
    };
    if rest != 0 {
        return Err("x509: trailing data after ASN.1 of public-key".into());
    }
    let oid = oid_der(&arcs);
    if oid == [0x2b, 0x65, 0x6e] {
        if !params.is_empty() {
            return Err("x509: X25519 key encoded with illegal parameters".into());
        }
        if data.len() != 32 {
            return Err("crypto/ecdh: invalid public key".into());
        }
        return Ok(PublicKey::Unknown);
    }
    if !matches!(
        oid.as_slice(),
        x509::OID_RSA | x509::OID_DSA | x509::OID_EC | x509::OID_ED25519
    ) {
        return Err("x509: unknown public key algorithm".into());
    }
    x509::parse_public_key(
        &x509::Ai {
            oid: &oid,
            params: &params,
        },
        &data,
    )
    .map_err(|e| e.0)
}

/// What rekor's pki/x509 NewPublicKey makes of PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RekorKey {
    Key(PublicKey),
    Certificate(Box<Certificate>),
    Chain(Vec<Certificate>),
}

/// cryptoutils.UnmarshalCertificatesFromPEMLimited.
fn certificates_limited(pem: &[u8], iterations: usize) -> Result<Vec<Certificate>, String> {
    let mut out = Vec::new();
    let mut remaining = trim_space(pem);
    while !remaining.is_empty() {
        if out.len() == iterations {
            return Err("too many certificates specified in PEM block".into());
        }
        let Some((block, rest)) = decode(remaining) else {
            return Err("error during PEM decoding".into());
        };
        out.push(Certificate::parse(&block.bytes).map_err(|e| e.0)?);
        remaining = rest;
    }
    Ok(out)
}

/// x509.NewPublicKey.
pub fn new_public_key(raw: &[u8]) -> Result<RekorKey, String> {
    let trimmed = trim_space(raw);
    let Some((block, rest)) = decode(trimmed) else {
        return Err("invalid public key: failure decoding PEM".into());
    };
    if !rest.is_empty() {
        return certificates_limited(trimmed, 10).map(RekorKey::Chain);
    }
    match block.kind.as_slice() {
        b"PUBLIC KEY" => parse_pkix(&block.bytes).map(RekorKey::Key),
        b"CERTIFICATE" => Certificate::parse(&block.bytes)
            .map(|c| RekorKey::Certificate(Box::new(c)))
            .map_err(|e| e.0),
        other => Err(format!(
            "invalid public key: cannot handle type {}",
            String::from_utf8_lossy(other)
        )),
    }
}

/// verifyCertChain.
pub fn verify_cert_chain(chain: &[Certificate]) -> Result<(), String> {
    let Some(first) = chain.first() else {
        return Err("no certificate chain provided".into());
    };
    if chain.len() == 1 {
        return Ok(());
    }
    let mut roots = x509::Pool::default();
    if let Some(last) = chain.last() {
        roots.add(last.clone());
    }
    let mut subs = x509::Pool::default();
    for c in chain.get(1..chain.len() - 1).unwrap_or_default() {
        subs.add(c.clone());
    }
    let opts = x509::Options {
        roots: &roots,
        intermediates: &subs,
        now: first.not_before,
        key_usages: vec![x509::Eku::Any],
    };
    first.verify(&opts).map(|_| ()).map_err(|e| e.0)
}

impl RekorKey {
    /// CryptoPubKey: the key, or the (first) certificate's.
    pub fn crypto_key(&self) -> PublicKey {
        match self {
            RekorKey::Key(k) => k.clone(),
            RekorKey::Certificate(c) => c.public_key.clone(),
            RekorKey::Chain(cs) => cs
                .first()
                .map(|c| c.public_key.clone())
                .unwrap_or(PublicKey::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_decodes_as_go_s() {
        let (b, rest) = decode(b"junk\n-----BEGIN X-----\nYWJj\n-----END X-----\ntail").unwrap();
        assert_eq!(
            (b.kind.as_slice(), b.bytes.as_slice(), rest),
            (&b"X"[..], &b"abc"[..], &b"tail"[..])
        );
        assert!(decode(b"-----BEGIN X-----\nYWJj\n-----END Y-----\n").is_none());
        let (b, _) = decode(b"-----BEGIN X-----\nA: b\n\nYWJj\n-----END X-----").unwrap();
        assert_eq!(b.bytes, b"abc");
        assert_eq!(trim_space(" \u{a0}x\u{2003} ".as_bytes()), b"x");
    }
}
