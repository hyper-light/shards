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
