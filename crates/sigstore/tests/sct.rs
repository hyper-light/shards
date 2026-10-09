//! What `sct::verify_scts` makes of testdata/sct.json's certificates, against what
//! sigstore-go's VerifySignedCertificateTimestamp made of them
//! (scripts/sigstore/generate-sct): the same error, byte for byte, or none.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use base64::Engine as _;
use shards_sigstore::sct::verify_scts;
use shards_sigstore::time::Time;
use shards_sigstore::trusted_root::TransparencyLog;
use shards_sigstore::x509::{Certificate, Eku, Hash, Options, Pool, parse_pkix_public_key};

/// Cases where shards answers otherwise, deliberately: (name, shards' answer, why).
const DEVIATIONS: &[(&str, &str, &str)] = &[(
    "hash 1",
    "only able to verify 0 SCT entries; unable to meet threshold of 1",
    "an SCT signed over an MD5 digest is refused (D105)",
)];

fn b64(s: &serde_json::Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_str().unwrap())
        .unwrap()
}

fn nanos(v: &serde_json::Value) -> Option<Time> {
    v.as_i64()
        .map(|n| Time::utc(n.div_euclid(1_000_000_000), n.rem_euclid(1_000_000_000) as u32))
}

fn run(c: &serde_json::Value) -> String {
    let leaf = Certificate::parse(&b64(&c["leaf"])).unwrap();
    let (mut roots, mut intermediates) = (Pool::default(), Pool::default());
    for r in c["roots"].as_array().unwrap() {
        roots.add(Certificate::parse(&b64(r)).unwrap());
    }
    for i in c["intermediates"].as_array().unwrap() {
        intermediates.add(Certificate::parse(&b64(i)).unwrap());
    }
    let mut ctlogs = BTreeMap::new();
    for l in c["ctlogs"].as_array().unwrap() {
        let key = parse_pkix_public_key(&b64(&l["key"])).unwrap();
        ctlogs.insert(
            l["keyId"].as_str().unwrap().to_string(),
            TransparencyLog {
                base_url: String::new(),
                id: Vec::new(),
                start: nanos(&l["startUnixNano"]),
                end: nanos(&l["endUnixNano"]),
                hash: Hash::Sha256,
                key,
                signature_hash: Hash::Sha256,
            },
        );
    }
    let opts = Options {
        roots: &roots,
        intermediates: &intermediates,
        now: Time::utc(c["now"].as_i64().unwrap(), 0),
        key_usages: vec![Eku::CodeSigning],
    };
    let chains = leaf.verify(&opts).unwrap();
    let threshold = usize::try_from(c["threshold"].as_u64().unwrap()).unwrap();
    verify_scts(&chains, threshold, &ctlogs).err().unwrap_or_default()
}

#[test]
fn scts_verify_as_sigstore_go_verifies_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/sct.json")).unwrap();
    let mut failed = Vec::new();
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let got = run(c);
        let want = match DEVIATIONS.iter().find(|d| d.0 == name) {
            Some((_, ours, _)) => (*ours).to_string(),
            None => c["error"].as_str().unwrap().to_string(),
        };
        if got != want {
            failed.push(format!("--- {name}\n  got  {got:?}\n  want {want:?}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
