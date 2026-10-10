//! The build's proxy's CA in a step's trust bundle (D110), as BuildKit v0.33.0's executor
//! puts it there before the step runs and takes it out after (executor/proxyca_linux.go):
//! appended to the first of [`BUNDLES`] that is a regular file, between two marker lines,
//! unless the bundle holds it already; taken out as the marked block that holds it, else
//! as every block that is it. Blocks are found as Go's encoding/pem finds them, and their
//! base64 read as Go's `StdEncoding` reads it, so the same bundles hold the same
//! certificates here as there. The files themselves are read and written in build.rs.

/// The trust bundles of Linux's TLS stacks, in the order BuildKit looks for them
/// (`linuxSystemCertFiles`).
pub const BUNDLES: [&[u8]; 6] = [
    b"/etc/ssl/certs/ca-certificates.crt",
    b"/etc/pki/tls/certs/ca-bundle.crt",
    b"/etc/ssl/ca-bundle.pem",
    b"/etc/pki/tls/cacert.pem",
    b"/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    b"/etc/ssl/cert.pem",
];

/// What the CA is put between (`proxyCABegin`, `proxyCAEnd`).
pub const BEGIN: &[u8] = b"\n# buildkit proxy CA begin\n";
pub const END: &[u8] = b"# buildkit proxy CA end\n";

/// The largest bundle read (`maxCertBundleBytes`).
pub const MOST: u64 = 10 << 20;

/// firstCertificate's refusal of a CA with no certificate.
pub const NO_CERTIFICATE: &str = "proxy CA PEM does not contain a certificate";

const PEM_START: &[u8] = b"\n-----BEGIN ";
const PEM_END: &[u8] = b"\n-----END ";
const PEM_END_OF_LINE: &[u8] = b"-----";

/// A PEM block: its type, and its bytes decoded.
#[derive(Debug, PartialEq, Eq)]
pub struct Block<'a> {
    pub kind: &'a [u8],
    pub bytes: Vec<u8>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// pem's getLine: the first line of `data`, ended by `\n` or `\r\n`, less trailing spaces
/// and tabs, and what follows the line's end.
fn line(data: &[u8]) -> (&[u8], &[u8]) {
    let (end, next) = match data.iter().position(|&b| b == b'\n') {
        None => (data.len(), data.len()),
        Some(i) if i > 0 && data.get(i - 1) == Some(&b'\r') => (i - 1, i + 1),
        Some(i) => (i, i + 1),
    };
    let mut line = data.get(..end).unwrap_or_default();
    while let Some((&last, rest)) = line.split_last()
        && (last == b' ' || last == b'\t')
    {
        line = rest;
    }
    (line, data.get(next..).unwrap_or_default())
}

/// encoding/pem's `Decode` (Go 1.26): the first block in `data` whose lines and base64
/// are whole, and what follows its end line; `None` where there is none.
pub fn decode(data: &[u8]) -> Option<(Block<'_>, &[u8])> {
    let start = PEM_START.get(1..).unwrap_or_default();
    let mut rest = data;
    loop {
        rest = if rest.starts_with(start) {
            rest.get(start.len()..)?
        } else {
            rest.get(find(rest, PEM_START)? + PEM_START.len()..)?
        };
        let (type_line, after) = line(rest);
        rest = after;
        let Some(kind) = type_line.strip_suffix(PEM_END_OF_LINE) else {
            continue;
        };
        // Its headers, `key: value` lines, until one without a colon: only whether there
        // are any counts here. A block cut short in them ends the search.
        let mut headers = false;
        loop {
            if rest.is_empty() {
                return None;
            }
            let (l, next) = line(rest);
            if !l.contains(&b':') {
                break;
            }
            headers = true;
            rest = next;
        }
        let end = PEM_END.get(1..).unwrap_or_default();
        let (end_index, trailer_index) = if !headers && rest.starts_with(end) {
            (0, end.len())
        } else {
            match find(rest, PEM_END) {
                Some(i) => (i, i + PEM_END.len()),
                None => continue,
            }
        };
        let trailer = rest.get(trailer_index..).unwrap_or_default();
        let trailer_len = kind.len() + PEM_END_OF_LINE.len();
        let (Some(end_line), Some(rest_of_end_line)) =
            (trailer.get(..trailer_len), trailer.get(trailer_len..))
        else {
            continue;
        };
        if !end_line.starts_with(kind) || !end_line.ends_with(PEM_END_OF_LINE) {
            continue;
        }
        // Nothing but spaces and tabs may follow on the end line.
        if !line(rest_of_end_line).0.is_empty() {
            continue;
        }
        let text: Vec<u8> = rest
            .get(..end_index)
            .unwrap_or_default()
            .iter()
            .copied()
            .filter(|&b| b != b' ' && b != b'\t')
            .collect();
        let Some(bytes) = base64(&text) else { continue };
        let (_, after) = line(rest.get(end_index + PEM_END.len() - 1..).unwrap_or_default());
        return Some((Block { kind, bytes }, after));
    }
}

/// encoding/base64's `StdEncoding.Decode` (Go 1.26): the standard alphabet, padded, `\r`
/// and `\n` skipped wherever they are; `None` for anything it refuses (CorruptInputError).
pub fn base64(src: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let newline = |c: u8| c == b'\n' || c == b'\r';
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut i = 0;
    loop {
        // One quantum: up to four letters, newlines skipped.
        let mut got = [0u32; 4];
        let mut n = 0;
        let mut padded = false;
        while n < 4 {
            let Some(&c) = src.get(i) else {
                // The end: whole quanta only, an unpadded part refused.
                if n == 0 {
                    return Some(out);
                }
                return None;
            };
            i += 1;
            if let Some(v) = value(c) {
                if let Some(slot) = got.get_mut(n) {
                    *slot = v;
                }
                n += 1;
                continue;
            }
            if newline(c) {
                continue;
            }
            if c != b'=' {
                return None;
            }
            // Padding: after two letters `==`, after three `=`, then only newlines.
            match n {
                2 => {
                    while src.get(i).is_some_and(|&c| newline(c)) {
                        i += 1;
                    }
                    if src.get(i) != Some(&b'=') {
                        return None;
                    }
                    i += 1;
                }
                3 => {}
                _ => return None,
            }
            while src.get(i).is_some_and(|&c| newline(c)) {
                i += 1;
            }
            if i < src.len() {
                return None;
            }
            padded = true;
            break;
        }
        let [a, b, c, d] = got;
        let v = a << 18 | b << 12 | c << 6 | d;
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        out.extend_from_slice(bytes.get(..n - 1).unwrap_or_default());
        if padded {
            return Some(out);
        }
    }
}

/// `firstCertificate`'s certificate: the bytes of the first CERTIFICATE block.
pub fn first_certificate(pem: &[u8]) -> Option<Vec<u8>> {
    let mut rest = pem;
    loop {
        let (block, after) = decode(rest)?;
        if block.kind == b"CERTIFICATE" {
            return Some(block.bytes);
        }
        rest = after;
    }
}

/// `containsCertificate`: whether a CERTIFICATE block of `data` is `der`.
pub fn contains(data: &[u8], der: &[u8]) -> bool {
    let mut rest = data;
    while let Some((block, after)) = decode(rest) {
        if block.kind == b"CERTIFICATE" && block.bytes == der {
            return true;
        }
        rest = after;
    }
    false
}

/// The bundle `original` with the CA `ca` appended between the markers, as InjectProxyCA
/// writes it.
pub fn appended(original: &[u8], ca: &[u8]) -> Vec<u8> {
    let mut next = original.to_vec();
    if next.last().is_some_and(|&b| b != b'\n') {
        next.push(b'\n');
    }
    next.extend_from_slice(BEGIN);
    next.extend_from_slice(ca);
    if next.last() != Some(&b'\n') {
        next.push(b'\n');
    }
    next.extend_from_slice(END);
    next
}

/// `removeInjectedCA`: `data` without the marked block that holds `der`; where there is
/// none, without every CERTIFICATE block that is `der`, each with what pem's search passed
/// over on its way to it.
pub fn removed(data: &[u8], der: &[u8]) -> Vec<u8> {
    if let Some(begin) = find(data, BEGIN) {
        let after = begin + BEGIN.len();
        if let Some(end) = data.get(after..).and_then(|d| find(d, END)) {
            let end = after + end + END.len();
            if contains(data.get(begin..end).unwrap_or_default(), der) {
                let mut out = data.get(..begin).unwrap_or_default().to_vec();
                out.extend_from_slice(data.get(end..).unwrap_or_default());
                return out;
            }
        }
    }
    let mut out = Vec::with_capacity(data.len());
    let mut rest = data;
    while !rest.is_empty() {
        let Some(at) = find(rest, b"-----BEGIN ") else {
            out.extend_from_slice(rest);
            break;
        };
        let (before, from) = rest.split_at(at);
        out.extend_from_slice(before);
        let Some((block, after)) = decode(from) else {
            out.extend_from_slice(from);
            break;
        };
        let consumed = from.len() - after.len();
        if block.kind != b"CERTIFICATE" || block.bytes != der {
            out.extend_from_slice(from.get(..consumed).unwrap_or_default());
        }
        rest = after;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_read_as_go_reads_it() {
        for (text, want) in [
            (&b""[..], Some(&b""[..])),
            (b"aGk=", Some(b"hi")),
            (b"aGk", None),
            (b"aGlo", Some(b"hih")),
            (b"aA==", Some(b"h")),
            (b"aA=", None),
            (b"a===", None),
            (b"aGk=\r\n\n", Some(b"hi")),
            (b"aG\nk=", Some(b"hi")),
            (b"aA=\n=", Some(b"h")),
            (b"aGk=aGk=", None),
            (b"aGk*", None),
            (b"=", None),
            (b"\n\n", Some(b"")),
            (b"aGlo\naGlo", Some(b"hihhih")),
        ] {
            assert_eq!(
                base64(text).as_deref(),
                want,
                "{:?}",
                String::from_utf8_lossy(text)
            );
        }
    }

    const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";

    #[test]
    fn blocks_are_found_as_pem_finds_them() {
        let (b, rest) = decode(CERT).unwrap();
        assert_eq!(
            (b.kind, b.bytes.as_slice(), rest),
            (&b"CERTIFICATE"[..], &[0u8, 1, 2][..], &b""[..])
        );
        // After text, past a bad block, CRLF lines, headers, trailing blanks.
        let (b, rest) = decode(b"junk\n-----BEGIN X-----\n!!\n-----END X-----\n-----BEGIN Y-----\r\nAAEC\r\n-----END Y----- \t\r\nnext").unwrap();
        assert_eq!(
            (b.kind, b.bytes.as_slice(), rest),
            (&b"Y"[..], &[0u8, 1, 2][..], &b"next"[..])
        );
        let (b, _) = decode(b"-----BEGIN Z-----\nProc-Type: 4,ENCRYPTED\n\nAAEC\n-----END Z-----\n").unwrap();
        assert_eq!(b.bytes, [0, 1, 2]);
        // An empty block with no headers ends at once.
        assert_eq!(
            decode(b"-----BEGIN E-----\n-----END E-----\n").map(|(b, _)| b.bytes),
            Some(Vec::new())
        );
        // Its end must name its type, and nothing may follow on that line.
        assert!(decode(b"-----BEGIN A-----\nAAEC\n-----END B-----\n").is_none());
        assert!(decode(b"-----BEGIN A-----\nAAEC\n-----END A----- x\n").is_none());
        // A block cut short in its headers ends the search.
        assert!(decode(b"-----BEGIN A-----\nK: v\n").is_none());
        assert!(decode(b"no block").is_none());
        // A type line its dashes do not end begins no block; the search goes on past it.
        assert!(decode(b"-----BEGIN A\nAAEC\n-----END A-----\n").is_none());
        assert_eq!(
            decode(b"-----BEGIN A\nAAEC\n-----END A-----\n-----BEGIN B-----\nAAEC\n-----END B-----\n")
                .map(|(b, _)| b.kind),
            Some(&b"B"[..])
        );
    }

    #[test]
    fn the_ca_goes_in_and_comes_out_as_buildkit_puts_it() {
        let der = first_certificate(CERT).unwrap();
        let bundle = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
        let with = appended(bundle, CERT);
        assert_eq!(
            with,
            [
                &bundle[..],
                b"\n\n# buildkit proxy CA begin\n",
                CERT,
                b"# buildkit proxy CA end\n"
            ]
            .concat()
        );
        assert!(contains(&with, &der) && !contains(bundle, &der));
        assert_eq!(removed(&with, &der), [&bundle[..], b"\n"].concat());
        // Moved out of its markers: each copy of it alone goes, the rest kept.
        let moved = [&b"a\n"[..], CERT, b"b\n", CERT].concat();
        assert_eq!(removed(&moved, &der), b"a\nb\n");
        // A marked block that does not hold it is left.
        let other = [BEGIN, b"x\n", END].concat();
        assert_eq!(removed(&other, &der), other);
        assert_eq!(appended(b"", CERT), [BEGIN, CERT, END].concat());
    }

    /// Each case's bundle and CA, and what the step made of the bundle, as BuildKit
    /// v0.33.0's InjectProxyCA and its cleanup made them (scripts/proxy/generate): the CA
    /// refused, held already, or appended, then taken out.
    #[test]
    fn bundles_are_what_buildkits_inject_proxy_ca_makes_them() {
        let oracle = crate::json::parse(include_bytes!("../testdata/proxyca.json")).unwrap();
        let bytes = |v: Option<&crate::json::Value>| -> Vec<u8> {
            let v = v.unwrap();
            match v.str() {
                Some(s) => s.as_bytes().to_vec(),
                None => base64(v.get("base64").and_then(|b| b.str()).unwrap().as_bytes()).unwrap(),
            }
        };
        let cases = oracle.get("cases").unwrap().array();
        assert!(cases.len() > 20);
        for case in cases {
            let name = case.get("name").and_then(|n| n.str()).unwrap();
            let bundle = bytes(case.get("bundle"));
            let ca = bytes(case.get("ca"));
            let Some(der) = first_certificate(&ca) else {
                assert_eq!(
                    case.get("error").and_then(|e| e.str()),
                    Some(NO_CERTIFICATE),
                    "{name}"
                );
                continue;
            };
            assert!(case.get("error").is_none(), "{name}");
            let changed = !contains(&bundle, &der);
            assert_eq!(
                Some(changed),
                case.get("changed")
                    .map(|c| matches!(c, crate::json::Value::Bool(true))),
                "{name}"
            );
            let after = if changed {
                appended(&bundle, &ca)
            } else {
                bundle.clone()
            };
            assert_eq!(after, bytes(case.get("after")), "{name}");
            let step = bytes(case.get("step"));
            let cleaned = if changed { removed(&step, &der) } else { step };
            assert_eq!(cleaned, bytes(case.get("cleaned")), "{name}");
        }
    }
}
