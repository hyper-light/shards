//! What this crate's timestamping authority makes of testdata/tsa.json's RFC 3161
//! responses, against what sigstore-go's made of them (scripts/sigstore/generate-tsa):
//! the same time, or the same error word for word.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use shards_sigstore::time::Time;
use shards_sigstore::trusted_root::TimestampingAuthority;
use shards_sigstore::x509::Certificate;

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn cert(v: &serde_json::Value) -> Option<Certificate> {
    v.as_str().map(|s| Certificate::parse(&b64(s)).unwrap())
}

fn time(v: &serde_json::Value) -> Option<Time> {
    v.as_object()
        .map(|t| Time::utc(t["secs"].as_i64().unwrap(), t["nanos"].as_u64().unwrap() as u32))
}

#[test]
fn timestamps_are_verified_as_sigstore_go_verifies_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/tsa.json")).unwrap();
    let mut failed = Vec::new();
    for c in cases.as_array().unwrap() {
        let a = &c["authority"];
        let tsa = TimestampingAuthority {
            uri: a["uri"].as_str().unwrap().to_string(),
            root: cert(&a["root"]),
            intermediates: a["intermediates"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(cert)
                .collect(),
            leaf: cert(&a["leaf"]),
            start: time(&a["start"]),
            end: time(&a["end"]),
        };
        let got = shards_sigstore::tsa::verify(
            &tsa,
            &b64(c["token"].as_str().unwrap()),
            &b64(c["signature"].as_str().unwrap()),
        );
        let got = match got {
            Ok(ts) => {
                assert_eq!(ts.uri, tsa.uri);
                (String::new(), Some((ts.time.secs, ts.time.nanos)))
            }
            Err(e) => (e, None),
        };
        let want = (
            c["error"].as_str().unwrap().to_string(),
            time(&c["time"]).map(|t| (t.secs, t.nanos)),
        );
        if got != want {
            failed.push(format!(
                "--- {}\n  got  {got:?}\n  Go   {want:?}",
                c["name"].as_str().unwrap()
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
