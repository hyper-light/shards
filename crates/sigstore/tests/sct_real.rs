//! The certificate of moby/buildkit v0.28.1's arm64 attestation bundle, chained to
//! Fulcio's root in Sigstore's trusted root, carries an SCT its CT log signed.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use base64::Engine as _;
use shards_sigstore::sct::verify_scts;
use shards_sigstore::time::Time;
use shards_sigstore::trusted_root::TransparencyLog;
use shards_sigstore::x509::{Certificate, Eku, Hash, Options, Pool, parse_pkix_public_key};

fn b64(s: &serde_json::Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s.as_str().unwrap())
        .unwrap()
}

fn time(v: &serde_json::Value) -> Option<Time> {
    v.as_str().map(|s| {
        let t = shards_dockerfile::go::parse_rfc3339(s.as_bytes()).unwrap();
        let (s, n) = t.unix();
        Time::utc(s, n)
    })
}

#[test]
fn the_buildkit_attestation_s_certificate_has_a_verified_sct() {
    let root: serde_json::Value =
        serde_json::from_str(include_str!("../../tuf/roots/sigstore/targets/trusted_root.json")).unwrap();
    let bundle: serde_json::Value = serde_json::from_str(include_str!(
        "../testdata/real/buildkit-v0.28.1-arm64.bundle.json"
    ))
    .unwrap();
    let leaf = Certificate::parse(&b64(&bundle["verificationMaterial"]["certificate"]["rawBytes"])).unwrap();
    let (mut roots, mut intermediates) = (Pool::default(), Pool::default());
    for ca in root["certificateAuthorities"].as_array().unwrap() {
        let certs = ca["certChain"]["certificates"].as_array().unwrap();
        for (i, c) in certs.iter().enumerate() {
            let cert = Certificate::parse(&b64(&c["rawBytes"])).unwrap();
            if i + 1 == certs.len() {
                roots.add(cert);
            } else {
                intermediates.add(cert);
            }
        }
    }
    let mut ctlogs = BTreeMap::new();
    for log in root["ctlogs"].as_array().unwrap() {
        let id = b64(&log["logId"]["keyId"]);
        let key = parse_pkix_public_key(&b64(&log["publicKey"]["rawBytes"])).unwrap();
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        ctlogs.insert(
            hex,
            TransparencyLog {
                base_url: log["baseUrl"].as_str().unwrap().to_string(),
                id,
                start: time(&log["publicKey"]["validFor"]["start"]),
                end: time(&log["publicKey"]["validFor"]["end"]),
                hash: Hash::Sha256,
                key,
                signature_hash: Hash::Sha256,
            },
        );
    }
    let opts = Options {
        roots: &roots,
        intermediates: &intermediates,
        now: Time::utc(1_774_446_092, 0),
        key_usages: vec![Eku::CodeSigning],
    };
    let chains = leaf.verify(&opts).unwrap();
    assert_eq!(verify_scts(&chains, 1, &ctlogs), Ok(()));
    assert_eq!(
        verify_scts(&chains, 2, &ctlogs),
        Err("only able to verify 1 SCT entries; unable to meet threshold of 2".into())
    );
    assert_eq!(verify_scts(&[], 1, &ctlogs), Err("no chains provided".into()));
    assert_eq!(
        verify_scts(&chains, 1, &BTreeMap::new()),
        Err("only able to verify 0 SCT entries; unable to meet threshold of 1".into())
    );
}
