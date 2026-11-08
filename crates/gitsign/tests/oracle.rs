//! What this crate makes of testdata/corpus.json, against what go-crypto v1.4.1 made of
//! it as buildx v0.37.1 vendors it (testdata/answers.json, by scripts/gitsign/generate).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use shards_gitsign::signature::Signature;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn signatures_are_read_as_go_crypto_reads_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/corpus.json")).unwrap();
    let answers: serde_json::Value = serde_json::from_str(include_str!("../testdata/answers.json")).unwrap();
    let mut failed = Vec::new();
    for (c, want) in cases.as_array().unwrap().iter().zip(answers.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        let armored = c["armored"].as_str().unwrap();
        let mut got = serde_json::json!({ "name": name });
        match shards_gitsign::parse_armored_detached_signature(armored.as_bytes()) {
            Err(e) => got["error"] = e.into(),
            Ok((sig, _)) => fields(&mut got, &sig),
        }
        got["summary"] = summary(armored).into();
        if &got != want {
            failed.push(format!("--- {name}\n  got  {got}\n  Go   {want}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

fn fields(got: &mut serde_json::Value, sig: &Signature) {
    got["version"] = sig.version.into();
    got["sigType"] = sig.sig_type.into();
    got["algo"] = sig.pubkey_algo.into();
    got["hash"] = sig.hash.map_or(0, |h| h.id()).into();
    got["created"] = sig.creation_time.unwrap_or(0).into();
    if let Some(k) = sig.issuer_key_id {
        got["keyID"] = format!("{k:016x}").into();
    }
    if let Some(f) = &sig.issuer_fingerprint {
        got["fingerprint"] = hex(f).into();
    }
    got["hashSuffix"] = hex(&sig.hash_suffix).into();
    got["hashTag"] = hex(&sig.hash_tag).into();
    // omitempty, as the oracle writes them.
    for k in ["version", "sigType", "algo", "hash", "created"] {
        if got[k] == 0 {
            got.as_object_mut().unwrap().remove(k);
        }
    }
    for k in ["fingerprint", "hashSuffix", "hashTag"] {
        if got[k] == "" {
            got.as_object_mut().unwrap().remove(k);
        }
    }
}

/// buildx's git signature summary, as the oracle writes it.
fn summary(armored: &str) -> String {
    match shards_gitsign::summary(armored.as_bytes()) {
        shards_gitsign::Summary::None => "none".into(),
        shards_gitsign::Summary::Pgp {
            version,
            key_id: Some(k),
        } => format!("pgp v{version} {k:016x}"),
        shards_gitsign::Summary::Pgp {
            version,
            key_id: None,
        } => format!("pgp v{version}"),
        shards_gitsign::Summary::Ssh { version, fingerprint } => format!("ssh v{version} {fingerprint}"),
    }
}
