//! What this crate's tlog makes of testdata/tlog.json's entries, against what sigstore-go
//! v1.2.2 made of them (scripts/sigstore/generate-tlog): parsing, validation, each
//! accessor, the signed entry timestamp, the Rekor v1 proof and checkpoint, the Rekor v2
//! entry hash and log entry; byte for byte.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::Value;
use shards_sigstore::bundle::{InclusionProof, TlogEntry};
use shards_sigstore::keys::{self, Details, Load};
use shards_sigstore::time::{Time, utc};
use shards_sigstore::tlog::{self, EntryKey, V2Verifier};
use shards_sigstore::trusted_root::TransparencyLog;
use shards_sigstore::x509;

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn enc(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An implicit-presence bytes field: empty is unset.
fn implicit(v: &Value) -> Option<Vec<u8>> {
    let b = b64(v.as_str()?);
    (!b.is_empty()).then_some(b)
}

fn entry(j: &Value) -> TlogEntry {
    TlogEntry {
        log_index: j["logIndex"].as_i64().unwrap(),
        log_id: (!j["logId"].is_null()).then(|| implicit(&j["logId"])),
        kind_version: j["kindVersion"].as_array().map(|kv| {
            (
                kv[0].as_str().unwrap().to_string(),
                kv[1].as_str().unwrap().to_string(),
            )
        }),
        integrated_time: j["integratedTime"].as_i64().unwrap(),
        inclusion_promise: (!j["promise"].is_null()).then(|| implicit(&j["promise"])),
        inclusion_proof: (!j["proof"].is_null()).then(|| {
            let p = &j["proof"];
            InclusionProof {
                log_index: p["logIndex"].as_i64().unwrap(),
                root_hash: b64(p["rootHash"].as_str().unwrap()),
                tree_size: p["treeSize"].as_i64().unwrap(),
                hashes: p["hashes"]
                    .as_array()
                    .map(|h| h.iter().map(|x| b64(x.as_str().unwrap())).collect())
                    .unwrap_or_default(),
                checkpoint: p["checkpoint"].as_str().map(str::to_string),
            }
        }),
        canonicalized_body: if j["body"].is_null() {
            None
        } else {
            implicit(&j["body"])
        },
    }
}

fn details(name: &str) -> Details {
    use Details::*;
    [
        RsaPkcs1v15_2048Sha256,
        RsaPkcs1v15_3072Sha256,
        RsaPkcs1v15_4096Sha256,
        RsaPss2048Sha256,
        RsaPss3072Sha256,
        RsaPss4096Sha256,
        EcdsaP256Sha256,
        EcdsaP384Sha384,
        EcdsaP384Sha256,
        EcdsaP521Sha512,
        EcdsaP521Sha256,
        Ed25519,
        Ed25519Ph,
    ]
    .into_iter()
    .find(|d| d.proto().0 == name)
    .unwrap()
}

/// What the crate makes of a case, in the oracle's shape.
fn run(c: &Value) -> Value {
    let tle = entry(&c["tle"]);
    let e = match tlog::parse_entry(&tle) {
        Ok(e) => e,
        Err(err) => return serde_json::json!({ "parse": err }),
    };
    let mut w = serde_json::json!({
        "parse": "",
        "validate": tlog::validate_entry(&e).err().unwrap_or_default(),
        "isV2": e.is_rekor_v2(),
        "signature": enc(&e.signature()),
        "publicKey": match e.public_key() {
            Some(EntryKey::Certificate(c)) => format!("cert:{}", enc(&c.raw)),
            Some(EntryKey::Key(k)) => keys::marshal_pkix(&k).map_or("key:?".into(), |d| format!("key:{}", enc(&d))),
            None => String::new(),
        },
        "hashedRekordDigest": e.hashed_rekord_digest().map(|(d, a)| format!("{}:{a}", hex(&d))).unwrap_or_default(),
        "dssePayloadHash": e.dsse_payload_hash().map(|d| hex(&d)).unwrap_or_default(),
        "promise": e.has_inclusion_promise(),
        "proof": e.has_inclusion_proof(),
        "v1sth": tlog::has_rekor_v1_sth(&e),
        "set": null,
        "inclusion": null,
        "entryHash": null,
        "v2": null,
    });
    let log = &c["log"];
    if log.is_null() {
        return w;
    }
    let key = x509::parse_pkix_public_key(&b64(log["key"].as_str().unwrap())).unwrap();
    let id = b64(log["id"].as_str().unwrap());
    let t = TransparencyLog {
        base_url: log["baseUrl"].as_str().unwrap().into(),
        id: id.clone(),
        start: log["start"].as_i64().map(|s| Time::utc(s, 0)),
        end: log["end"].as_i64().map(|s| Time::utc(s, 0)),
        hash: x509::Hash::Sha256,
        signature_hash: tlog::checkpoint_hash(&key),
        key: key.clone(),
    };
    let verifier = keys::load(
        &key,
        Load {
            hash: Some(Some(t.signature_hash)),
            ..Load::default()
        },
    )
    .unwrap();
    let mut logs = BTreeMap::new();
    logs.insert(hex(&id), t);
    w["set"] = tlog::verify_set(&e, &logs, utc).err().unwrap_or_default().into();
    if !e.is_rekor_v2() && e.has_inclusion_proof() {
        w["inclusion"] = tlog::verify_inclusion_v1(&e, &verifier)
            .err()
            .unwrap_or_default()
            .into();
    }
    let v2 = &c["v2"];
    if !v2.is_null() {
        let raw = b64(v2["verifierRaw"].as_str().unwrap());
        let ver = if v2["verifierKind"] == "publicKey" {
            V2Verifier::PublicKey(raw)
        } else {
            V2Verifier::Certificate(raw)
        };
        match tlog::v2_entry_hash(
            &b64(v2["digest"].as_str().unwrap()),
            &b64(v2["signature"].as_str().unwrap()),
            &ver,
            details(v2["details"].as_str().unwrap()),
        ) {
            Err(err) => w["entryHash"] = format!("error: {err}").into(),
            Ok(h) => {
                w["entryHash"] = enc(&h).into();
                w["v2"] = tlog::verify_v2(&e, v2["origin"].as_str().unwrap(), &verifier, &h)
                    .err()
                    .unwrap_or_default()
                    .into();
            }
        }
    }
    w
}

#[test]
fn entries_are_read_and_checked_as_sigstore_go_s() {
    let cases: Value = serde_json::from_str(include_str!("../testdata/tlog.json")).unwrap();
    let mut failed = Vec::new();
    let all = cases.as_array().unwrap();
    for c in all {
        let got = run(c);
        let want = &c["want"];
        let mismatch: Vec<String> = want
            .as_object()
            .unwrap()
            .iter()
            // A parse failure is all there is to compare.
            .filter(|(k, _)| want["parse"] == "" || k.as_str() == "parse")
            .filter(|(k, v)| got.get(k.as_str()).unwrap_or(&Value::Null) != *v)
            .map(|(k, v)| {
                format!(
                    "  {k}:\n    got  {}\n    Go   {v}",
                    got.get(k.as_str()).unwrap_or(&Value::Null)
                )
            })
            .collect();
        if !mismatch.is_empty() {
            failed.push(format!(
                "--- {}\n{}",
                c["name"].as_str().unwrap(),
                mismatch.join("\n")
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} differ:\n{}",
        failed.len(),
        all.len(),
        failed.join("\n")
    );
}
