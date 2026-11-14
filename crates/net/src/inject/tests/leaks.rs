//! A credential's value never reaching the guest past the injector (D242), by the ways
//! docs/bugs/2026-11-13/creds.md's creds-01 to creds-04 found: a token host's response repeating
//! its tokens in its head or trailers (creds-01), a chunk's extension (creds-02), an echo after
//! its lease ended or its value rotated (creds-03), and a coded body (creds-04); each over
//! HTTP/1.1 and HTTP/2.

use super::*;
use std::time::Duration;

impl Bench {
    /// The HTTP/1.1 test host's answer, written as it is (`H1Answers::Quiet`).
    fn answer(&mut self, bytes: &[u8]) {
        match &mut self.wire {
            Wire::H1(host) => host.tls.writer().write_all(bytes).unwrap(),
            Wire::H2(_) => panic!("the bench's host speaks HTTP/2"),
        }
    }
}

/// The granted hosts' HTTP/1.1 host, which answers nothing of itself.
fn quiet(root: &CertifiedIssuer<'static, KeyPair>, names: &[&str]) -> H1Host {
    H1Host::new(leaf_config(root, names), H1Answers::Quiet, false)
}

fn value() -> &'static str {
    std::str::from_utf8(VALUE).unwrap()
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut coder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    coder.write_all(bytes).unwrap();
    coder.finish().unwrap()
}

/// What `coded` decodes to, as the guest's own decoder would read it.
fn gunzip(coded: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = flate2::read::GzDecoder::new(coded).read_to_end(&mut out);
    out
}

/// A guest of the granted host, its request with the credential's placeholder in `x-api-key`
/// (and `more` fields) sent and arrived, its lease answered, the host's answer not yet written.
fn h1_sent(credentials: &mut Credentials, root: &CertifiedIssuer<'static, KeyPair>, more: &str) -> Bench {
    let mut bench = Bench::new(
        guest(credentials.ca.der().to_vec().into(), GRANTED, None),
        quiet(root, &[GRANTED]),
    );
    let request = format!("GET /v1/models HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {PLACEHOLDER}\r\n{more}\r\n");
    bench.guest.writer().write_all(request.as_bytes()).unwrap();
    bench.until(credentials, &mut lease_all, |b| !b.h1().had.is_empty());
    assert!(holds(&bench.h1().had[0].request, VALUE), "the value was not filled in");
    bench
}

/// Do: a granted host's request, filled in; the host answers chunked, the value in a chunk's
/// extension, as a token and as a quoted string (RFC 9112 §7.1.1). Expect: the value nowhere in
/// what the guest read; its chunks' sizes and data as they came.
#[test]
fn a_chunks_extension_never_carries_a_value_to_the_guest() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let mut bench = h1_sent(&mut credentials, &root, "");
    let v = value();
    let response = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;echo={v}\r\nhello\r\n6 ; q=\"{v}\"\r\n world\r\n0\r\n\r\n"
    );
    bench.answer(response.as_bytes());
    bench.until(&mut credentials, &mut unanswered, |b| b.got.ends_with(b"0\r\n\r\n"));
    assert!(
        !holds(&bench.got, VALUE),
        "a chunk's extension carried the value to the guest: {}",
        lossy(&bench.got)
    );
    assert!(
        bench.got.ends_with(b"\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
        "{}",
        lossy(&bench.got)
    );
}

/// A chunk's size line, a byte at a time, is one piece of framing, its size as it says; a size
/// line holding a value filled in is refused, as no mask of it keeps its framing.
#[test]
fn a_chunks_size_line_is_read_whole_and_never_carries_a_value() {
    let mut framing = Framing::Chunked(Chunk::Size(Vec::new()));
    let mut sizes = Vec::new();
    for &b in b"1a;ext=\"x y\"\r\n".iter() {
        let (taken, ended) = walk(&mut framing, &[b], 64, &mut |p| {
            if let Piece::Size { size, line } = p {
                sizes.push((size, line.to_vec()));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!((taken, ended), (1, false));
    }
    assert_eq!(sizes, [(0x1a, b"1a;ext=\"x y\"\r\n".to_vec())]);
    let mut mask = Mask::default();
    mask.fill(b"deadbeef", b"kiln-placeholder-0");
    let mut out = Vec::new();
    let mut to = |b: &[u8]| {
        out.extend_from_slice(b);
        Ok(())
    };
    assert!(mask.size(0xdeadbeef, &mut to).is_err(), "a size line carried a value");
    assert!(mask.size(0x1a, &mut to).is_ok());
    assert_eq!(out, b"1a\r\n");
}

/// Do: a granted host's request, filled in; its lease lapses before the host answers, and the
/// host echoes the value in a field and in its body. Expect: both echoes masked with the
/// placeholder's bytes, the value nowhere in what the guest read.
#[test]
fn an_echo_after_its_lease_lapsed_is_masked_still() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let mut bench = h1_sent(&mut credentials, &root, "");
    assert_eq!(credentials.leases.lapse(Instant::now() + Duration::from_secs(120)), 1);
    assert!(credentials.leases.held(CLAUDE, Instant::now()).is_none());
    let v = value();
    let response = format!("HTTP/1.1 200 OK\r\nX-Echo: {v}\r\nContent-Length: {}\r\n\r\n{v}", v.len());
    bench.answer(response.as_bytes());
    bench.until(&mut credentials, &mut unanswered, |b| whole(&b.got));
    assert!(
        !holds(&bench.got, VALUE),
        "the echo of a lapsed lease's value reached the guest: {}",
        lossy(&bench.got)
    );
    assert_eq!(
        bench.got.windows(VALUE.len()).filter(|w| *w == echo_mask()).count(),
        2,
        "{}",
        lossy(&bench.got)
    );
}

const ROTATED: &[u8] = b"sk-ant-api03-rotated-not-a-real-value-9876";

/// Answers each lease asked with `ROTATED`, as a broker gives a value rotated outside.
fn lease_rotated(credentials: &mut Credentials) -> usize {
    let asks = credentials.leases.asks();
    for ask in &asks {
        let (id, _) = lease::decode_ask(ask).unwrap();
        let life = NonZeroU64::new(60_000_000_000).unwrap();
        let answer = lease::encode_answer(id, Ok((life, ROTATED))).unwrap();
        assert!(credentials.leases.answered(&answer, Instant::now()));
    }
    asks.len()
}

/// Do: over HTTP/2, stream 1 filled in with the value; while its response waits the value's held
/// lease is let go of as a sign-in's refresh lets it go (`Leases::forget`), and stream 3 leases
/// the rotated value; then the host echoes each stream's Authorization in a field and in DATA.
/// Expect: neither value anywhere in what the guest read; each stream's echo masked.
#[test]
fn an_h2_echo_of_a_value_rotated_while_its_response_waits_is_masked() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let mut host = H2Host::new(&root, &[GRANTED]);
    host.auto = false;
    let (mut bench, mut peer) = h2_bench(&credentials, GRANTED, host);
    let mut seen = Vec::new();
    let bearer = format!("Bearer {PLACEHOLDER}");
    peer.send_headers(1, &h2_request("GET", GRANTED, "/v1/models", Some(&bearer)), true);
    h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut lease_all, |_, b| {
        b.host().streams.values().any(|s| s.ended)
    });
    credentials.leases.forget(CLAUDE);
    peer.send_headers(3, &h2_request("GET", GRANTED, "/v1/rotated", Some(&bearer)), true);
    h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut lease_rotated, |_, b| {
        b.host().streams.values().filter(|s| s.ended).count() == 2
    });
    let ids: Vec<u32> = bench.host().streams.keys().copied().collect();
    for id in ids {
        bench.host_mut().answer(id);
    }
    h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut unanswered, |p, _| {
        p.response(1).ended && p.response(3).ended
    });
    let rotated = std::str::from_utf8(ROTATED).unwrap();
    assert_eq!(
        h2_leaked(&peer, &seen, &[value(), rotated]),
        None,
        "a value reached the guest"
    );
    let masked = [b"Bearer ", &echo_mask()[..]].concat();
    assert_eq!(value_of(&peer.response(1).fields, "x-echo"), Some(&masked[..]));
}

/// Do: a granted host's request, filled in, asking gzip; the host answers gzip anyway, its body
/// an echo of the value that holds no run of the value's bytes. Expect: the request reaching the
/// host asking `identity` alone (RFC 9110 §12.5.3); the response refused 502 before any of its
/// body reaches the guest, so that nothing the guest reads decodes to the value.
#[test]
fn a_coded_echo_never_reaches_the_guest() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let mut bench = h1_sent(&mut credentials, &root, "Accept-Encoding: gzip, br\r\n");
    let coded = gzip(format!("{{\"echo\":\"{}\"}}", value()).as_bytes());
    assert!(!holds(&coded, VALUE), "the fixture's coding holds the value as it is");
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        coded.len()
    )
    .into_bytes();
    response.extend_from_slice(&coded);
    bench.answer(&response);
    bench.until(&mut credentials, &mut unanswered, |b| b.closed || whole(&b.got));
    let body = bench
        .got
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(&[][..], |at| &bench.got[at + 4..]);
    assert!(
        !holds(&bench.got, &coded),
        "the coded echo reached the guest, decoding to the value: {}",
        holds(&gunzip(body), VALUE)
    );
    assert!(bench.got.starts_with(b"HTTP/1.1 502 "), "{}", lossy(&bench.got));
    let sent = &bench.h1().had[0].request;
    assert!(
        holds(sent, b"\r\nAccept-Encoding: identity\r\n") && !holds(sent, b"gzip"),
        "{}",
        lossy(sent)
    );
}

/// Do: as above, the host coding its echo as a transfer coding (`gzip, chunked`, RFC 9112 §7).
/// Expect: refused 502, the coded bytes never reaching the guest.
#[test]
fn a_transfer_coded_echo_never_reaches_the_guest() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let mut bench = h1_sent(&mut credentials, &root, "");
    let coded = gzip(value().as_bytes());
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n{:x}\r\n",
        coded.len()
    )
    .into_bytes();
    response.extend_from_slice(&coded);
    response.extend_from_slice(b"\r\n0\r\n\r\n");
    bench.answer(&response);
    bench.until(&mut credentials, &mut unanswered, |b| {
        b.closed || b.got.ends_with(b"0\r\n\r\n")
    });
    assert!(
        !holds(&bench.got, &coded),
        "the transfer-coded echo reached the guest: {}",
        lossy(&bench.got)
    );
    assert!(bench.got.starts_with(b"HTTP/1.1 502 "), "{}", lossy(&bench.got));
}

/// The HTTP/2 test host's echo of the request's Authorization, gzip-coded.
fn h2_coded_echo(fields: &[hpack::Field], _: &[u8]) -> (Vec<hpack::Field>, Vec<u8>) {
    let auth = fields
        .iter()
        .find(|f| f.name == b"authorization")
        .map(|f| f.value.to_vec())
        .unwrap_or_default();
    (
        vec![field(":status", b"200"), field("content-encoding", b"gzip")],
        gzip(&auth),
    )
}

/// Do: over HTTP/2, stream 1 filled in and asking gzip; the host answers gzip, an echo of the
/// value. Expect: the request asking `identity` alone; the stream answered 502 by the injector,
/// the coded DATA never reaching the guest.
#[test]
fn an_h2_coded_echo_never_reaches_the_guest() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let host = H2Host::serving(&root, &[GRANTED], h2_coded_echo);
    let (mut bench, mut peer) = h2_bench(&credentials, GRANTED, host);
    let mut seen = Vec::new();
    let bearer = format!("Bearer {PLACEHOLDER}");
    let mut request = h2_request("GET", GRANTED, "/v1/models", Some(&bearer));
    request.push(field("accept-encoding", b"gzip, br"));
    peer.send_headers(1, &request, true);
    h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut lease_all, |p, _| {
        p.response(1).ended || !p.ends().is_empty()
    });
    let coded = gzip(&[b"Bearer ", VALUE].concat());
    let Got { fields, data, .. } = peer.response(1);
    assert!(
        !holds(&seen, &coded) && !holds(&gunzip(&data), VALUE),
        "the coded echo reached the guest"
    );
    assert_eq!(value_of(&fields, ":status"), Some(&b"502"[..]));
    let codings: Vec<&[u8]> = bench
        .host()
        .at("/v1/models")
        .fields
        .iter()
        .filter(|f| f.name == b"accept-encoding")
        .map(|f| &f.value[..])
        .collect();
    assert_eq!(codings, [&b"identity"[..]]);
}

/// Do: a granted host's WebSocket upgrade, filled in, offering permessage-deflate (RFC 7692),
/// the host taking it anyway. Expect: the offer cut on its way (RFC 6455 §9.1); the 101 refused
/// 502, nothing of the tunnel reaching the guest.
#[test]
fn a_filled_websocket_takes_no_extension() {
    let root = root("shards test root");
    let mut credentials = credentials(&root);
    let upgrade = "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Extensions: permessage-deflate\r\n";
    let mut bench = h1_sent(&mut credentials, &root, upgrade);
    let sent = &bench.h1().had[0].request;
    assert!(!holds(sent, b"permessage-deflate"), "{}", lossy(sent));
    bench.answer(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n");
    bench.until(&mut credentials, &mut unanswered, |b| b.closed || !b.got.is_empty());
    bench.settle(&mut credentials, &mut unanswered);
    assert!(bench.got.starts_with(b"HTTP/1.1 502 "), "{}", lossy(&bench.got));
    assert!(bench.closed);
}

/// The token host's answer to the code's exchange, chunked, its trailer section `trailers`.
fn chunked_token_answer(trailers: &str) -> Vec<u8> {
    let answer = token_response(ISSUED_ACCESS, ISSUED_REFRESH);
    let (a, b) = answer.split_at(answer.len() / 2);
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n{trailers}\r\n",
        a.len(),
        b.len()
    )
    .into_bytes()
}

/// Do: Claude Code's code exchanged at its HTTP/1.1 token host, which answers chunked, with a
/// trailer field and with none (RFC 9112 §7.1.2). Expect: each captured, the guest given the
/// token response with the broker's placeholders in their places and nothing else, its trailer
/// section no part of its body (RFC 9110 §6.5.1 lets a recipient that decodes the chunked coding
/// drop it); no real token in it.
#[test]
fn a_chunked_token_responses_trailers_are_no_part_of_its_body() {
    let root = root("shards test root");
    for trailers in ["", "X-Trace: 1\r\n"] {
        let mut credentials = sign_in_credentials(&root, false);
        let ca = credentials.ca.der().to_vec();
        let mut broker = Broker::default();
        let mut cli = Bench::new(guest(ca.into(), TOKEN_HOST, None), quiet(&root, &[TOKEN_HOST]));
        cli.guest
            .writer()
            .write_all(&post("/v1/oauth/token", TOKEN_HOST, EXCHANGE))
            .unwrap();
        cli.until(&mut credentials, &mut |c| broker.serve(c), |b| !b.h1().had.is_empty());
        cli.answer(&chunked_token_answer(trailers));
        cli.until(&mut credentials, &mut |c| broker.serve(c), |b| b.closed || whole(&b.got));
        let got = lossy(&cli.got);
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "trailers {trailers:?}: {got}");
        let expected = token_response(SIGNED_ACCESS, SIGNED_REFRESH);
        assert!(got.ends_with(&format!("\r\n\r\n{expected}")), "trailers {trailers:?}: {got}");
        assert_eq!(broker.access, ISSUED_ACCESS.as_bytes());
    }
}

const EXCHANGE: &str = r#"{"grant_type":"authorization_code","code":"c0de","redirect_uri":"https://platform.claude.com/oauth/code/callback","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","code_verifier":"v","state":"s"}"#;
