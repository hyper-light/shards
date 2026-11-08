//! Keyless signatures made against a local Sigstore (Fulcio v1.9.0, Rekor v1.5.4,
//! rekor-tiles v2.3.0, timestamp-authority v2.1.4: docs/research/measurements/keyless,
//! PM M184), shards' and cosign v3.1.3's, of each signing config, and shards' of
//! identities signed in for at Dex (a browser's flow and a device code's); each verified
//! offline
//! against that Sigstore's trusted root as `cosign verify` verifies a keyless signature
//! (a log entry, the certificate's SCT, an observer timestamp, the identity and its
//! issuer, the artifact among the statement's subjects); a changed one refused.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;

use shards_sigstore::time::{Time, utc};
use shards_sigstore::verify::{Config, Identity, Material, Policy, verify};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/keyless")
}

fn cases() -> Vec<serde_json::Value> {
    serde_json::from_slice::<Vec<serde_json::Value>>(&std::fs::read(dir().join("cases.json")).unwrap())
        .unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

/// cosign verify's checks of a keyless signature: one log entry, one SCT, one observer
/// timestamp (the authority's or the log's), the subject digest.
fn check(bundle: &[u8], digest: &str) -> Result<shards_sigstore::verify::Outcome, shards_sigstore::Error> {
    let root = shards_sigstore::trusted_root::parse(&std::fs::read(dir().join("trusted_root.json")).unwrap())
        .unwrap();
    let material = Material {
        root: &root,
        key: None,
        fulcio: true,
    };
    let config = Config {
        tlog: 1,
        sct: 1,
        observer: 1,
        ..Config::default()
    };
    let (alg, hex) = digest.split_once(':').unwrap();
    let policy = Policy {
        digest: Some((alg.to_string(), unhex(hex))),
        identity: Identity::Any,
    };
    verify(bundle, &material, &config, &policy, utc, Time::utc(0, 0))
}

#[test]
fn shards_and_cosign_keyless_signatures_verify_as_cosign_verifies_them() {
    let cases = cases();
    // Each signing config, signed by each; and shards' signatures of identities signed in
    // for at Dex, in a browser and by a device code.
    assert_eq!(cases.len(), 8, "{cases:#?}");
    for c in &cases {
        let bundle = std::fs::read(dir().join(c["bundle"].as_str().unwrap())).unwrap();
        let digest = c["digest"].as_str().unwrap();
        let outcome = check(&bundle, digest).unwrap_or_else(|e| panic!("{}: {e:?}", c["bundle"]));
        let cert = outcome.certificate.unwrap();
        assert_eq!(cert.subject_alternative_name, c["identity"].as_str().unwrap());
        assert_eq!(cert.extensions.issuer, c["issuer"].as_str().unwrap());
        // A timestamp authority's where the config named one; the log's otherwise.
        let kinds: Vec<&str> = outcome.timestamps.iter().map(|t| t.kind).collect();
        if c["tsa"].as_bool().unwrap() {
            assert!(kinds.contains(&"TimestampAuthority"), "{kinds:?}");
        } else {
            assert!(kinds.contains(&"Tlog"), "{kinds:?}");
        }
        let b: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
        let kind = &b["verificationMaterial"]["tlogEntries"][0]["kindVersion"];
        let v2 = c["logs"][0].as_u64().unwrap() == 2;
        assert_eq!(kind["kind"], if v2 { "hashedrekord" } else { "dsse" });
        assert_eq!(kind["version"], if v2 { "0.0.2" } else { "0.0.1" });
    }
}

/// shards' bundle holds what cosign's holds, signing config by signing config.
#[test]
fn shards_bundles_hold_what_cosign_s_hold() {
    let cases = cases();
    let shape = |file: &str| {
        let b: serde_json::Value = serde_json::from_slice(&std::fs::read(dir().join(file)).unwrap()).unwrap();
        let vm = &b["verificationMaterial"];
        let mut material: Vec<String> = vm.as_object().unwrap().keys().cloned().collect();
        material.sort();
        let entries: Vec<Vec<String>> = vm["tlogEntries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let mut k: Vec<String> = e.as_object().unwrap().keys().cloned().collect();
                // protojson leaves a zero out: a log's first entry (shards signed first in
                // each fresh log) is at index 0.
                if !k.iter().any(|f| f == "logIndex") {
                    k.push("logIndex".into());
                }
                k.sort();
                k
            })
            .collect();
        let mut top: Vec<String> = b.as_object().unwrap().keys().cloned().collect();
        top.sort();
        (b["mediaType"].clone(), material, entries, top)
    };
    for ours in cases.iter().filter(|c| c["signer"] == "shards") {
        let theirs = cases
            .iter()
            .find(|c| c["signer"] == "cosign" && c["logs"] == ours["logs"] && c["tsa"] == ours["tsa"])
            .unwrap();
        assert_eq!(
            shape(ours["bundle"].as_str().unwrap()),
            shape(theirs["bundle"].as_str().unwrap()),
            "{}",
            ours["bundle"]
        );
    }
}

/// A signature over other bytes, or of another artifact, is refused.
#[test]
fn a_changed_keyless_signature_is_refused() {
    for c in cases() {
        let bundle = std::fs::read(dir().join(c["bundle"].as_str().unwrap())).unwrap();
        let digest = c["digest"].as_str().unwrap();
        let mut b: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
        let sig = b["dsseEnvelope"]["signatures"][0]["sig"]
            .as_str()
            .unwrap()
            .to_string();
        let flipped = match sig.strip_prefix('M') {
            Some(rest) => format!("N{rest}"),
            None => format!("M{}", sig.get(1..).unwrap()),
        };
        b["dsseEnvelope"]["signatures"][0]["sig"] = serde_json::Value::String(flipped);
        assert!(check(&serde_json::to_vec(&b).unwrap(), digest).is_err());
        let other = format!("sha256:{}", "0".repeat(64));
        assert!(check(&bundle, &other).is_err());
    }
}
