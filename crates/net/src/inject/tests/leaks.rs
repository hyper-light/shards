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

/// A chunked body's data, its chunks joined (RFC 9112 §7.1), and whether its last chunk came.
fn dechunk(mut body: &[u8]) -> (Vec<u8>, bool) {
    let mut data = Vec::new();
    loop {
        let Some(eol) = body.windows(2).position(|w| w == b"\r\n") else {
            return (data, false);
        };
        let size = usize::from_str_radix(std::str::from_utf8(&body[..eol]).unwrap(), 16).unwrap();
        if size == 0 {
            return (data, &body[eol..] == b"\r\n\r\n");
        }
        data.extend_from_slice(&body[eol + 2..eol + 2 + size]);
        assert_eq!(&body[eol + 2 + size..eol + 4 + size], b"\r\n");
        body = &body[eol + 4 + size..];
    }
}

/// What the guest read of a response: its head and its body.
fn split(got: &[u8]) -> (String, &[u8]) {
    let at = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (lossy(&got[..at]), &got[at..])
}

/// `bytes` zstd-coded, as ruzstd codes them.
fn zstd(bytes: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
}

/// The echo the coded tests' host gives: a JSON body that repeats the value, long enough to be
/// coded in more than one piece.
fn echo_body() -> Vec<u8> {
    let filler = "x".repeat(70_000);
    format!("{{\"echo\":\"{}\",\"filler\":\"{filler}\",\"again\":\"{}\"}}", value(), value()).into_bytes()
}

/// `echo_body` as the guest is to have it: each value masked.
fn masked_echo_body() -> Vec<u8> {
    let mask = lossy(&echo_mask());
    let filler = "x".repeat(70_000);
    format!("{{\"echo\":\"{mask}\",\"filler\":\"{filler}\",\"again\":\"{mask}\"}}").into_bytes()
}

/// Do: a granted host's request, filled in, asking gzip; the host answers coded anyway (RFC 9110
/// §12.5.3 lets it), as a content coding (gzip, zstd) and as a transfer coding (`gzip, chunked`),
/// framed by its length and chunked, its body an echo of the value that holds no run of the
/// value's bytes. Expect: the request reaching the host asking `identity` alone; the guest given
/// the response decoded, as `identity` and chunked, its Content-Encoding gone and each echo of
/// the value masked; the value nowhere in what the guest read, coded or not.
#[test]
fn a_coded_echo_reaches_the_guest_decoded_and_masked() {
    let root = root("shards test root");
    let body = echo_body();
    let cases: [(&str, Vec<u8>, bool); 4] = [
        ("Content-Encoding: gzip", gzip(&body), false),
        ("Content-Encoding: zstd", zstd(&body), false),
        ("Content-Encoding: x-gzip", gzip(&body), true),
        ("Transfer-Encoding: gzip, chunked", gzip(&body), true),
    ];
    for (coding, coded, chunked) in cases {
        let mut credentials = credentials(&root);
        let mut bench = h1_sent(&mut credentials, &root, "Accept-Encoding: gzip, br\r\n");
        let sent = &bench.h1().had[0].request;
        assert!(
            holds(sent, b"\r\nAccept-Encoding: identity\r\n") && !holds(sent, b"gzip"),
            "{}",
            lossy(sent)
        );
        // gzip's Huffman codes leave no run of the value's bytes for a mask of coded bytes to find
        // (zstd's raw literals may hold it as it is).
        assert!(
            coding.contains("zstd") || !holds(&coded, VALUE),
            "the fixture's coding holds the value as it is"
        );
        let mut response = format!("HTTP/1.1 200 OK\r\n{coding}\r\n").into_bytes();
        match (chunked, coding.starts_with("Transfer")) {
            (true, true) => {}
            (true, false) => response.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
            (false, _) => response.extend_from_slice(format!("Content-Length: {}\r\n", coded.len()).as_bytes()),
        }
        response.extend_from_slice(b"\r\n");
        if chunked {
            for piece in coded.chunks(1000) {
                response.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                response.extend_from_slice(piece);
                response.extend_from_slice(b"\r\n");
            }
            response.extend_from_slice(b"0\r\n\r\n");
        } else {
            response.extend_from_slice(&coded);
        }
        bench.answer(&response);
        bench.until(&mut credentials, &mut unanswered, |b| b.closed || b.got.ends_with(b"\r\n0\r\n\r\n"));
        assert!(
            !holds(&bench.got, VALUE) && !holds(&bench.got, &coded),
            "{coding}: the value or its coding reached the guest"
        );
        let (head, rest) = split(&bench.got);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{coding}: {head}");
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("\r\ntransfer-encoding: chunked\r\n")
                && !lower.contains("content-encoding")
                && !lower.contains("content-length")
                && !lower.contains("gzip"),
            "{coding}: {head}"
        );
        let (data, whole) = dechunk(rest);
        assert!(whole, "{coding}: its last chunk never came");
        assert!(data == masked_echo_body(), "{coding}: the decoded body is not the masked echo");
        assert!(!bench.closed, "{coding}: the connection closed");
    }
}

/// Do: as above, the host coding its echo as br, which no decoder of this side's reads, and as
/// gzip twice over. Expect: refused 502 before any of it reaches the guest.
#[test]
fn a_coded_echo_this_side_cannot_decode_is_refused() {
    let root = root("shards test root");
    for coding in ["br", "gzip, gzip"] {
        let mut credentials = credentials(&root);
        let mut bench = h1_sent(&mut credentials, &root, "");
        let coded = b"\x1b\x03\x00\xf8coded";
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: {coding}\r\nContent-Length: {}\r\n\r\n",
            coded.len()
        )
        .into_bytes();
        response.extend_from_slice(coded);
        bench.answer(&response);
        bench.until(&mut credentials, &mut unanswered, |b| b.closed);
        assert!(bench.got.starts_with(b"HTTP/1.1 502 "), "{coding}: {}", lossy(&bench.got));
        assert!(!holds(&bench.got, coded), "{coding}");
    }
}

/// The HTTP/2 test host's echo of the request's Authorization, gzip-coded as one DATA frame's
/// worth after another.
fn h2_coded_echo(fields: &[hpack::Field], _: &[u8]) -> (Vec<hpack::Field>, Vec<u8>) {
    let auth = fields
        .iter()
        .find(|f| f.name == b"authorization")
        .map(|f| f.value.to_vec())
        .unwrap_or_default();
    let body = [&auth[..], b"\n", &echo_body()[..]].concat();
    let coded = gzip(&body);
    (
        vec![
            field(":status", b"200"),
            field("content-encoding", b"gzip"),
            field("content-length", coded.len().to_string().as_bytes()),
        ],
        coded,
    )
}

/// Do: over HTTP/2, stream 1 filled in and asking gzip; the host answers gzip anyway, an echo of
/// the value. Expect: the request asking `identity` alone; the stream's response decoded, its
/// Content-Encoding and Content-Length gone and each echo masked; no value in anything the guest
/// read; no stream reset.
#[test]
fn an_h2_coded_echo_reaches_the_guest_decoded_and_masked() {
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
    let codings: Vec<&[u8]> = bench
        .host()
        .at("/v1/models")
        .fields
        .iter()
        .filter(|f| f.name == b"accept-encoding")
        .map(|f| &f.value[..])
        .collect();
    assert_eq!(codings, [&b"identity"[..]]);
    assert_eq!(h2_leaked(&peer, &seen, &[value()]), None, "the value reached the guest");
    let Got { fields, data, ended } = peer.response(1);
    assert!(ended);
    assert_eq!(value_of(&fields, ":status"), Some(&b"200"[..]));
    assert_eq!(value_of(&fields, "content-encoding"), None);
    assert_eq!(value_of(&fields, "content-length"), None);
    let masked = [b"Bearer ", &echo_mask()[..], b"\n", &masked_echo_body()[..]].concat();
    assert!(data == masked, "the decoded DATA is not the masked echo");
    assert_eq!(peer.ends(), [], "a stream reset or the connection ended");
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

/// The token host's answer to the code's exchange, its tokens repeated in its head: a field, a
/// cookie and the reason phrase.
fn echoing_token_answer(request: &[u8]) -> Vec<u8> {
    let answer = token_response(ISSUED_ACCESS, ISSUED_REFRESH);
    assert!(holds(request, br#""grant_type":"authorization_code""#));
    format!(
        "HTTP/1.1 200 {ISSUED_ACCESS}\r\nContent-Type: application/json\r\nX-Echo: {ISSUED_ACCESS}\r\nSet-Cookie: rt={ISSUED_REFRESH}; Secure\r\nContent-Length: {}\r\n\r\n{answer}",
        answer.len()
    )
    .into_bytes()
}


/// Do: Claude Code's code exchanged at its HTTP/1.1 token host, whose response repeats the
/// issued tokens in its head. Expect: the response refused 502 before its tokens are asked
/// outside; no real token in anything the guest read; nothing captured.
#[test]
fn a_token_responses_head_never_carries_its_tokens_to_the_guest() {
    let root = root("shards test root");
    let mut credentials = sign_in_credentials(&root, false);
    let ca = credentials.ca.der().to_vec();
    let mut broker = Broker::default();
    let mut cli = Bench::new(guest(ca.into(), TOKEN_HOST, None), quiet(&root, &[TOKEN_HOST]));
    cli.guest
        .writer()
        .write_all(&post("/v1/oauth/token", TOKEN_HOST, EXCHANGE))
        .unwrap();
    cli.until(&mut credentials, &mut |c| broker.serve(c), |b| !b.h1().had.is_empty());
    let answer = echoing_token_answer(&cli.h1().had[0].request);
    cli.answer(&answer);
    cli.until(&mut credentials, &mut |c| broker.serve(c), |b| b.closed || whole(&b.got));
    for real in [ISSUED_ACCESS, ISSUED_REFRESH] {
        assert!(
            !holds(&cli.got, real.as_bytes()),
            "a real token reached the guest: {}",
            lossy(&cli.got)
        );
    }
    assert!(cli.got.starts_with(b"HTTP/1.1 502 "), "{}", lossy(&cli.got));
    assert!(broker.access.is_empty(), "a refused response's tokens were captured");
}

/// The token host's HTTP/2 answer, its access token repeated in a field.
fn h2_echoing_token_answer(fields: &[hpack::Field], body: &[u8]) -> (Vec<hpack::Field>, Vec<u8>) {
    let (mut head, answer) = h2_token_answer(fields, body);
    head.push(field("x-echo", ISSUED_ACCESS.as_bytes()));
    (head, answer)
}

/// A guest of the HTTP/2 token host answering as `answers` says, HTTP/2 chosen.
fn h2_token_guest(
    root: &CertifiedIssuer<'static, KeyPair>,
    credentials: &mut Credentials,
    answers: H2Answer,
) -> (Bench, Peer) {
    let guest = guest_offering(credentials.ca.der().to_vec().into(), TOKEN_HOST, None, &[b"h2"]);
    let host = H2Host::serving(root, &[TOKEN_HOST], answers);
    let mut bench = Bench::on(guest, Wire::H2(Box::new(host)));
    bench.until(credentials, &mut unanswered, |b| !b.guest.is_handshaking());
    (bench, Peer::new(End::Client, GUEST_HOLDS))
}

/// Do: over HTTP/2, the code's exchange at the token host, its response repeating the access
/// token in a field; then on another connection, its refresh token in the response's trailers.
/// Expect: each stream answered 502 by the injector; no real token in anything the guest read
/// or decoded; nothing captured.
#[test]
fn an_h2_token_responses_fields_and_trailers_never_carry_its_tokens_to_the_guest() {
    let root = root("shards test root");
    for trailers in [false, true] {
        let mut credentials = sign_in_credentials(&root, false);
        let mut broker = Broker::default();
        let answers: H2Answer = if trailers {
            h2_token_answer
        } else {
            h2_echoing_token_answer
        };
        let (mut bench, mut peer) = h2_token_guest(&root, &mut credentials, answers);
        bench.host_mut().auto = !trailers;
        let mut seen = Vec::new();
        peer.send_headers(1, &h2_post("/v1/oauth/token", EXCHANGE), false);
        peer.send_data(1, EXCHANGE.as_bytes(), true);
        if trailers {
            h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut |c| broker.serve(c), |_, b| {
                b.host().streams.values().any(|s| s.ended)
            });
            let host = bench.host_mut();
            let s = host.streams.get_mut(&1).unwrap();
            s.answered = true;
            let (head, data) = h2_token_answer(&s.fields, &s.body);
            let head: Vec<hpack::Field> = head.into_iter().filter(|f| f.name != b"content-length").collect();
            host.peer.send_headers(1, &head, false);
            host.peer.send_data(1, &data, false);
            host.peer
                .send_headers(1, &[field("x-refresh", ISSUED_REFRESH.as_bytes())], true);
        }
        h2_until(&mut bench, &mut peer, &mut seen, &mut credentials, &mut |c| broker.serve(c), |p, _| {
            p.response(1).ended || !p.ends().is_empty()
        });
        assert_eq!(
            h2_leaked(&peer, &seen, &[ISSUED_ACCESS, ISSUED_REFRESH]),
            None,
            "a real token reached the guest (trailers: {trailers})"
        );
        assert_eq!(value_of(&peer.response(1).fields, ":status"), Some(&b"502"[..]));
        assert!(broker.access.is_empty(), "a refused response's tokens were captured");
    }
}

/// A token's echo is found in a head as it came, percent-encoded and as a quoted string's pairs.
#[test]
fn a_tokens_echo_in_a_head_is_found_escaped_or_not() {
    let body = token_response(ISSUED_ACCESS, ISSUED_REFRESH);
    let tokens = signin::read(body.as_bytes()).unwrap().unwrap();
    let pct: String = ISSUED_ACCESS.bytes().map(|b| format!("%{b:02X}")).collect();
    let quoted: String = ISSUED_REFRESH.chars().flat_map(|c| ['\\', c]).collect();
    for head in [
        format!("X-Echo: {ISSUED_ACCESS}"),
        format!("Location: /cb?t={pct}"),
        format!("X-Echo: \"{quoted}\""),
    ] {
        assert_eq!(
            signin::unechoed(&tokens, [head.as_bytes()]),
            Err(signin::Refusal::Echoed),
            "{head}"
        );
    }
    assert_eq!(signin::unechoed(&tokens, [&b"X-Echo: kiln%2"[..]]), Ok(()));
}
