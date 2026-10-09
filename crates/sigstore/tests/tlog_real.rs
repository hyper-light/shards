//! A real Rekor v1 entry (moby/buildkit v0.28.1's arm64 provenance, signed by GitHub
//! Actions) read and checked against Sigstore's trusted root as carried in the binary.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use base64::Engine as _;
use shards_sigstore::bundle::{InclusionProof, TlogEntry};
use shards_sigstore::keys::{self, Load};
use shards_sigstore::time::{Time, utc};
use shards_sigstore::tlog;
use shards_sigstore::trusted_root::TransparencyLog;
use shards_sigstore::x509;

fn b64(v: &serde_json::Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(v.as_str().unwrap())
        .unwrap()
}

fn int(v: &serde_json::Value) -> i64 {
    v.as_str().unwrap().parse().unwrap()
}

fn rekor_logs() -> BTreeMap<String, TransparencyLog> {
    let root: serde_json::Value =
        serde_json::from_str(include_str!("../../tuf/roots/sigstore/targets/trusted_root.json")).unwrap();
    let mut out = BTreeMap::new();
    for t in root["tlogs"].as_array().unwrap() {
        if t["publicKey"]["keyDetails"] != "PKIX_ECDSA_P256_SHA_256" {
            continue;
        }
        let id = b64(&t["logId"]["keyId"]);
        let key = x509::parse_pkix_public_key(&b64(&t["publicKey"]["rawBytes"])).unwrap();
        let start = shards_dockerfile::go::parse_rfc3339(
            t["publicKey"]["validFor"]["start"].as_str().unwrap().as_bytes(),
        )
        .unwrap()
        .unix();
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        out.insert(
            hex,
            TransparencyLog {
                base_url: t["baseUrl"].as_str().unwrap().into(),
                id,
                start: Some(Time::utc(start.0, start.1)),
                end: None,
                hash: x509::Hash::Sha256,
                signature_hash: tlog::checkpoint_hash(&key),
                key,
            },
        );
    }
    out
}

#[test]
fn the_buildkit_attestation_s_entry_is_in_rekor() {
    let bundle: serde_json::Value = serde_json::from_str(include_str!(
        "../testdata/real/buildkit-v0.28.1-arm64.bundle.json"
    ))
    .unwrap();
    let e = &bundle["verificationMaterial"]["tlogEntries"][0];
    let p = &e["inclusionProof"];
    let tle = TlogEntry {
        log_index: int(&e["logIndex"]),
        log_id: Some(Some(b64(&e["logId"]["keyId"]))),
        kind_version: Some((
            e["kindVersion"]["kind"].as_str().unwrap().into(),
            e["kindVersion"]["version"].as_str().unwrap().into(),
        )),
        integrated_time: int(&e["integratedTime"]),
        inclusion_promise: Some(Some(b64(&e["inclusionPromise"]["signedEntryTimestamp"]))),
        inclusion_proof: Some(InclusionProof {
            log_index: int(&p["logIndex"]),
            root_hash: b64(&p["rootHash"]),
            tree_size: int(&p["treeSize"]),
            hashes: p["hashes"].as_array().unwrap().iter().map(b64).collect(),
            checkpoint: Some(p["checkpoint"]["envelope"].as_str().unwrap().into()),
        }),
        canonicalized_body: Some(b64(&e["canonicalizedBody"])),
    };
    let entry = tlog::parse_entry(&tle).unwrap();
    tlog::validate_entry(&entry).unwrap();
    assert!(!entry.is_rekor_v2());
    assert!(tlog::has_rekor_v1_sth(&entry));
    let logs = rekor_logs();
    tlog::verify_set(&entry, &logs, utc).unwrap();
    let log = logs.values().find(|l| l.id == entry.log_key_id()).unwrap();
    let verifier = keys::load(
        &log.key,
        Load {
            hash: Some(Some(log.signature_hash)),
            ..Load::default()
        },
    )
    .unwrap();
    tlog::verify_inclusion_v1(&entry, &verifier).unwrap();
    let payload = b64(&bundle["dsseEnvelope"]["payload"]);
    let want = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &payload);
    assert_eq!(entry.dsse_payload_hash().unwrap(), want.as_ref());
    let sig = b64(&bundle["dsseEnvelope"]["signatures"][0]["sig"]);
    assert_eq!(entry.signature(), sig);
    let cert = b64(&bundle["verificationMaterial"]["certificate"]["rawBytes"]);
    match entry.public_key().unwrap() {
        tlog::EntryKey::Certificate(c) => assert_eq!(c.raw, cert),
        other => panic!("{other:?}"),
    }
    // A flipped bit anywhere breaks the SET and the proof.
    let mut bad = entry.clone();
    bad.tle.integrated_time += 1;
    assert_eq!(
        tlog::verify_set(&bad, &logs, utc).unwrap_err(),
        "unable to verify SET"
    );
    let mut bad = entry.clone();
    if let Some(p) = bad.tle.inclusion_proof.as_mut() {
        p.hashes[0][0] ^= 1;
    }
    assert!(
        tlog::verify_inclusion_v1(&bad, &verifier)
            .unwrap_err()
            .starts_with("calculated root:\n")
    );
}
