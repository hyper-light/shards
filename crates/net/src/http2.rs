//! HTTP/2 (RFC 9113) for the credential injector (D242, kiln plan 01a1d2b88be57304b972761e2a5c6781
//! step 7): the request field blocks it rewrites, over shards-http2's frames, settings, flow
//! control and HPACK, which this module re-exports for inject.rs and which the gateway's client
//! shares (R12). An injected flow is two connections, the guest's and the host's, and the injector
//! is an end of each: field compression is each connection's own (§4.3.1), and so is flow
//! control, hop by hop (§6.9). So a request's block is decoded with the guest's hop's decoder,
//! its credential filled in where `credential::decide` says, and encoded anew with the host's
//! hop's encoder, its credential fields never indexed (RFC 7541 §7.1.3, f42); nothing of one
//! hop's encoded bytes reaches the other.
//!
//! Like inject.rs it holds no socket and reads no clock. Its caller is inject.rs' `H2`, an
//! injected flow whose two hops chose `h2` (RFC 7301, RFC 9113 §3.2).

use std::ops::Range;

use zeroize::Zeroizing;

pub use shards_http2::*;

use crate::dns::credential::{self, CredentialGrant, Fill};
use crate::hpack::{Field, Indexing};
use crate::inject::{authority_host, token_in};

/// What the injector does with a request's field block (D242), as `credential::decide` says.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// It goes on to the host, each fill's credential filled in where its placeholder stands.
    Go(Go),
    /// Its host is not the SNI: answered 421 on its stream alone (RFC 9110 §7.4, f44).
    Misdirected { host: String },
    /// Malformed (RFC 9113 §8.1.1): a stream error PROTOCOL_ERROR, never sent on.
    Malformed(&'static str),
}

/// A request to go on: its fields as they came, its host, and its fills.
#[derive(Debug, PartialEq, Eq)]
pub struct Go {
    pub fields: Vec<Field>,
    pub host: String,
    pub fills: Vec<Filled>,
}

/// A credential to be filled in: the field, where its placeholder lies in its value, and the
/// credential `decide` named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    pub field: usize,
    pub token: Range<usize>,
    pub reference: String,
}

/// The request pseudo-header fields (§8.3.1), and RFC 8441's `:protocol` (§4).
const PSEUDO: [&[u8]; 5] = [b":method", b":scheme", b":authority", b":path", b":protocol"];
/// The fields HTTP/2 has no use of (§8.2.2).
const CONNECTION_SPECIFIC: [&[u8]; 5] = [
    b"connection",
    b"proxy-connection",
    b"keep-alive",
    b"transfer-encoding",
    b"upgrade",
];

/// A request's decoded `fields`, held to RFC 9113 §8.2 and §8.3.1 as an origin holds them (a
/// gateway is one, RFC 9110 §3.7), and what `decide` says of each credential field's token for
/// `grants` on a flow whose TLS named `sni`.
pub fn request(fields: Vec<Field>, grants: &[CredentialGrant], sni: &str) -> Request {
    if let Err(why) = well_formed(&fields) {
        return Request::Malformed(why);
    }
    let one = |name: &[u8]| {
        let mut all = fields.iter().filter(move |f| f.name == name);
        (all.next(), all.next())
    };
    let host = match (one(b":authority"), one(b"host")) {
        ((Some(_), Some(_)), _) => return Request::Malformed("two :authority fields"),
        (_, (Some(_), Some(_))) => return Request::Malformed("two Host fields"),
        ((Some(a), None), (Some(h), None)) if !a.value.eq_ignore_ascii_case(&h.value) => {
            return Request::Malformed("a Host that is not the :authority");
        }
        ((Some(f), None), _) | (_, (Some(f), None)) => f,
        _ => return Request::Malformed("a request that names no host"),
    };
    let Some(host) = std::str::from_utf8(&host.value).ok().and_then(authority_host) else {
        return Request::Malformed("a request's host is no host");
    };
    if credential::decide(grants, sni, &host, None) == Fill::Misdirected {
        return Request::Misdirected { host };
    }
    let mut fills = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let Some(token) = token_in(&field.name, &field.value) else {
            continue;
        };
        let Some(placeholder) = field
            .value
            .get(token.clone())
            .and_then(|t| std::str::from_utf8(t).ok())
        else {
            continue;
        };
        if let Fill::With(reference) = credential::decide(grants, sni, &host, Some(placeholder)) {
            fills.push(Filled {
                field: i,
                token,
                reference,
            });
        }
    }
    Request::Go(Go { fields, host, fills })
}

/// RFC 9113 §8.2.1's least validation, §8.2.2's connection-specific fields, and §8.3.1's
/// pseudo-header fields: known ones, each once, before every other field.
fn well_formed(fields: &[Field]) -> Result<(), &'static str> {
    let mut regular = false;
    let mut seen: Vec<&[u8]> = Vec::new();
    for f in fields {
        let pseudo = f.name.first() == Some(&b':');
        let name = if pseudo {
            f.name.get(1..).unwrap_or_default()
        } else {
            &f.name[..]
        };
        if name.is_empty()
            || name
                .iter()
                .any(|&b| b <= 0x20 || b.is_ascii_uppercase() || b >= 0x7f || b == b':')
        {
            return Err("a field name HTTP/2 forbids");
        }
        if f.value.iter().any(|&b| matches!(b, 0 | b'\n' | b'\r'))
            || f.value.first().is_some_and(|&b| b == b' ' || b == b'\t')
            || f.value.last().is_some_and(|&b| b == b' ' || b == b'\t')
        {
            return Err("a field value HTTP/2 forbids");
        }
        if pseudo {
            if regular || !PSEUDO.contains(&&f.name[..]) || seen.contains(&&f.name[..]) {
                return Err("a pseudo-header field out of place, unknown or twice");
            }
            seen.push(&f.name);
        } else {
            regular = true;
            if CONNECTION_SPECIFIC.contains(&&f.name[..]) || (f.name == b"te" && &f.value[..] != b"trailers")
            {
                return Err("a connection-specific field");
            }
        }
    }
    let connect = fields
        .iter()
        .any(|f| f.name == b":method" && &f.value[..] == b"CONNECT");
    let has = |n: &[u8]| seen.contains(&n);
    let complete = has(b":method")
        && if connect && !has(b":protocol") {
            !has(b":scheme") && !has(b":path") && has(b":authority")
        } else {
            has(b":scheme") && has(b":path")
        };
    complete
        .then_some(())
        .ok_or("a request without its pseudo-header fields")
}

impl Go {
    /// The references of the credentials to lease, once each.
    pub fn references(&self) -> Vec<&str> {
        let mut all: Vec<&str> = Vec::new();
        for f in &self.fills {
            if !all.contains(&f.reference.as_str()) {
                all.push(&f.reference);
            }
        }
        all
    }

    /// The block for the host's hop: each fill's value (`value` gives a held lease's) in its
    /// placeholder's place, every credential field never indexed (RFC 7541 §7.1.3, f42), every
    /// other field as it came, encoded by the host's hop's own encoder; wiped as it drops. None
    /// where a value is not held, the encoder untouched, so that the caller can ask again.
    pub fn encode<'v>(
        &self,
        encoder: &mut hpack::Encoder,
        value: impl Fn(&str) -> Option<&'v [u8]>,
    ) -> Option<Zeroizing<Vec<u8>>> {
        let mut fields = self.fields.clone();
        for fill in &self.fills {
            let value = value(&fill.reference)?;
            let field = fields.get_mut(fill.field)?;
            let mut filled =
                Zeroizing::new(Vec::with_capacity(field.value.len().saturating_add(value.len())));
            filled.extend_from_slice(field.value.get(..fill.token.start)?);
            filled.extend_from_slice(value);
            filled.extend_from_slice(field.value.get(fill.token.end..)?);
            field.value = filled;
        }
        for field in &mut fields {
            // The fields `token_in` reads a credential from, filled or not.
            if matches!(&field.name[..], b"authorization" | b"x-api-key") {
                field.indexing = Indexing::Never;
            }
        }
        let mut block = Zeroizing::new(Vec::new());
        encoder.encode(&fields, &mut block);
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &str = "api.anthropic.com";
    const OPENAI: &str = "api.openai.com";
    const VALUE: &[u8] = b"sk-ant-api03-the-real-value";
    const HOLDS: Holds = Holds {
        stream: Settings::INITIAL.initial_window_size,
        connection: Settings::INITIAL.initial_window_size,
    };

    fn placeholder(c: char) -> String {
        format!("kiln-placeholder-{}", c.to_string().repeat(64))
    }

    fn grants() -> Vec<CredentialGrant> {
        vec![
            CredentialGrant {
                reference: "anthropic/key".into(),
                toward: vec![ANTHROPIC.into()],
                placeholder: placeholder('a'),
            },
            CredentialGrant {
                reference: "openai/key".into(),
                toward: vec![OPENAI.into()],
                placeholder: placeholder('b'),
            },
        ]
    }

    /// A request as a guest sends it, its placeholder indexed, as RFC 7541 §7.1.3 lets a client.
    fn request_fields(authority: &str, authorization: &str, extra: &[(&str, &str)]) -> Vec<Field> {
        let mut fields = vec![
            Field::new(b":method", b"POST", Indexing::Incremental),
            Field::new(b":scheme", b"https", Indexing::Incremental),
            Field::new(b":authority", authority.as_bytes(), Indexing::Incremental),
            Field::new(b":path", b"/v1/messages", Indexing::Incremental),
            Field::new(b"authorization", authorization.as_bytes(), Indexing::Incremental),
        ];
        fields.extend(
            extra
                .iter()
                .map(|(n, v)| Field::new(n.as_bytes(), v.as_bytes(), Indexing::Incremental)),
        );
        fields
    }

    fn bearer(token: &[u8]) -> Vec<u8> {
        [b"Bearer ", token].concat()
    }

    /// The guest's side: its own encoder's block for `fields`, and the frames carrying it.
    fn guest_sends(encoder: &mut hpack::Encoder, stream: u32, fields: &[Field]) -> (Vec<u8>, Vec<u8>) {
        let mut block = Vec::new();
        encoder.encode(fields, &mut block);
        let mut wire = Vec::new();
        headers(stream, &block, false, Settings::INITIAL.max_frame_size, &mut wire);
        (block, wire)
    }

    /// A field block read from its frames and decoded by `decoder`.
    fn read(decoder: &mut hpack::Decoder, wire: &[u8]) -> Vec<Field> {
        let most = Settings::INITIAL.max_frame_size;
        let (head, payload) = frame(wire, most).unwrap().unwrap();
        let (mut block, mut whole) = Block::headers(head, payload).unwrap();
        let mut at = HEAD + payload.len();
        while !whole {
            let (head, payload) = frame(&wire[at..], most).unwrap().unwrap();
            whole = block.continued(head, payload, usize::MAX).unwrap();
            at += HEAD + payload.len();
        }
        assert_eq!(at, wire.len(), "one block, nothing after it");
        match block.decode(decoder, usize::MAX).unwrap() {
            hpack::Decoded::Fields(fields) => fields,
            other => panic!("{other:?}"),
        }
    }

    fn pairs(fields: &[Field]) -> Vec<(Vec<u8>, Vec<u8>)> {
        fields
            .iter()
            .map(|f| (f.name.clone(), f.value.to_vec()))
            .collect()
    }

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn go(request: Request) -> Go {
        match request {
            Request::Go(go) => go,
            other => panic!("{other:?}"),
        }
    }

    fn head(kind: u8, stream: u32, length: u32) -> Head {
        Head {
            length,
            kind,
            flags: 0,
            stream,
        }
    }

    fn settings(params: &[(u16, u32)]) -> Vec<u8> {
        params
            .iter()
            .flat_map(|(i, v)| [&i.to_be_bytes()[..], &v.to_be_bytes()[..]].concat())
            .collect()
    }

    /// Each frame's type, flags, stream and length.
    fn frames(mut wire: &[u8]) -> Vec<(u8, u8, u32, u32)> {
        let mut all = Vec::new();
        while let Some((h, payload)) = frame(wire, u32::MAX).unwrap() {
            all.push((h.kind, h.flags, h.stream, h.length));
            wire = &wire[HEAD + payload.len()..];
        }
        all
    }

    #[test]
    fn a_placeholder_in_a_header_block_comes_out_filled_and_never_indexed() {
        let grants = grants();
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        let mut host = hpack::Decoder::new(4096);
        // A field past a frame's 16,384 octets, so that each side's block runs into CONTINUATIONs.
        let large = "v".repeat(20_000);
        let sent = request_fields(
            ANTHROPIC,
            &format!("Bearer {}", placeholder('a')),
            &[("content-type", "application/json"), ("x-large", &large)],
        );
        // Twice on one connection: the second block finds the value in no table of either hop.
        for stream in [1, 3] {
            let (_, wire) = guest_sends(&mut guest, stream, &sent);
            let go = go(request(read(&mut guest_hop.decoder, &wire), &grants, ANTHROPIC));
            assert_eq!(go.references(), ["anthropic/key"]);
            let block = go
                .encode(&mut host_hop.encoder, |r| (r == "anthropic/key").then_some(VALUE))
                .unwrap();
            let mut out = Vec::new();
            headers(stream, &block, false, host_hop.peer.max_frame_size, &mut out);
            assert!(frames(&out).len() > 1, "HEADERS and CONTINUATION");
            let arrived = read(&mut host, &out);
            let mut expected = pairs(&sent);
            expected[4].1 = bearer(VALUE);
            assert_eq!(pairs(&arrived), expected, "stream {stream}");
            assert_eq!(arrived[4].indexing, Indexing::Never, "stream {stream}");
            assert!(!holds(&out, placeholder('a').as_bytes()));
        }
    }

    #[test]
    fn an_ungranted_hosts_block_passes_as_it_came() {
        // A flow to api.openai.com, whose TLS the injector ends as a grant names it, carrying the
        // placeholder of a credential granted toward api.anthropic.com alone.
        let sent = request_fields(
            OPENAI,
            &format!("Bearer {}", placeholder('a')),
            &[("x-trace", "1")],
        );
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        let (_, wire) = guest_sends(&mut guest, 1, &sent);
        let go = go(request(read(&mut guest_hop.decoder, &wire), &grants(), OPENAI));
        assert!(go.fills.is_empty());
        let block = go
            .encode(&mut host_hop.encoder, |r| -> Option<&'static [u8]> {
                panic!("no lease of {r} asked")
            })
            .unwrap();
        let mut out = Vec::new();
        headers(1, &block, false, host_hop.peer.max_frame_size, &mut out);
        assert_eq!(pairs(&read(&mut hpack::Decoder::new(4096), &out)), pairs(&sent));
    }

    #[test]
    fn a_block_for_a_host_not_the_sni_is_misdirected() {
        let sent = request_fields(ANTHROPIC, &format!("Bearer {}", placeholder('a')), &[]);
        assert_eq!(
            request(sent.clone(), &grants(), OPENAI),
            Request::Misdirected {
                host: ANTHROPIC.into()
            }
        );
        let mut host = sent.clone();
        host.push(Field::new(b"host", b"evil.example", Indexing::Incremental));
        assert!(matches!(
            request(host, &grants(), ANTHROPIC),
            Request::Malformed(_)
        ));
        let mut upper = sent;
        upper.push(Field::new(b"X-Up", b"1", Indexing::Incremental));
        assert!(matches!(
            request(upper, &grants(), ANTHROPIC),
            Request::Malformed(_)
        ));
    }

    #[test]
    fn each_hops_dynamic_table_stays_its_own_across_blocks() {
        let grants = grants();
        let mut guest = hpack::Encoder::new(4096, 4096);
        let mut guest_hop = Hop::new(HOLDS, 4096);
        let mut host_hop = Hop::new(HOLDS, 4096);
        // The host keeps a table of 128 octets where the guest's encoder keeps 4,096.
        let mut host = hpack::Decoder::new(4096);
        let mut ack = Vec::new();
        let params = settings(&[(0x1, 128)]);
        host_hop
            .settings(head(SETTINGS, 0, params.len() as u32), &params, &mut ack)
            .unwrap();
        host.set_limit(128);
        let session = "s".repeat(40);
        for n in 0..3u32 {
            let sent = request_fields(
                ANTHROPIC,
                &format!("Bearer {}", placeholder('a')),
                &[("x-session", &session), ("x-request", &n.to_string())],
            );
            let (guest_block, wire) = guest_sends(&mut guest, 1 + 2 * n, &sent);
            let got = read(&mut guest_hop.decoder, &wire);
            assert_eq!(
                pairs(&got),
                pairs(&sent),
                "the guest's hop decodes block {n} with its own table"
            );
            let block = go(request(got, &grants, ANTHROPIC))
                .encode(&mut host_hop.encoder, |_| Some(VALUE))
                .unwrap();
            let arrived = match host.decode(&block, usize::MAX).unwrap() {
                hpack::Decoded::Fields(f) => f,
                other => panic!("{other:?}"),
            };
            let mut expected = pairs(&sent);
            expected[4].1 = bearer(VALUE);
            assert_eq!(
                pairs(&arrived),
                expected,
                "the host's hop decodes block {n} with its own table"
            );
            if n > 0 {
                assert_ne!(
                    block[..],
                    guest_block[..],
                    "block {n} re-encoded, never passed as it came"
                );
            }
        }
    }
}
