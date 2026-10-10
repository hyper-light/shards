//! OpenSSH certificates as keys and as the keys of signatures, held to buildx v0.37.1's
//! x/crypto/ssh v0.55.0 by testdata/certs.json (scripts/gitsign/generate-certs): each
//! certificate and key ParsePublicKey and ParseAuthorizedKey read, its type, fingerprint and
//! wire form as x/crypto writes it again, or the error; and verify_git_signature of each
//! signature made with a certificate, as buildx's gitsign.VerifySignature answers.

#![allow(clippy::unwrap_used, clippy::panic)]

use base64::Engine as _;
use shards_gitsign::pgpsign::{self, Formats};
use shards_gitsign::ssh;

fn utc(secs: i64) -> String {
    format!("{secs}")
}

#[test]
fn certificates_read_and_verify_as_buildx_reads_and_verifies_them() {
    let answers: serde_json::Value = serde_json::from_str(include_str!("../testdata/certs.json")).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let quote = |b: &[u8]| shards_dockerfile::go::quote(b);
    let mut failed = Vec::new();
    let mut certificates = 0;
    for c in answers["parse"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let input = c["input"].as_str().unwrap();
        let got = match c["kind"].as_str().unwrap() {
            "wire" => ssh::parse_public_key(&b64.decode(input).unwrap()),
            _ => ssh::parse_authorized_key(input.as_bytes(), &quote),
        };
        let got = match got {
            Ok(k) => {
                if matches!(k, ssh::PublicKey::Certificate(_)) {
                    certificates += 1;
                }
                serde_json::json!({
                    "type": k.kind(),
                    "fingerprint": k.fingerprint(),
                    "marshal": b64.encode(k.marshal()),
                })
            }
            Err(e) => serde_json::json!({ "error": e }),
        };
        let mut want = serde_json::Map::new();
        for k in ["type", "fingerprint", "marshal", "error"] {
            if let Some(v) = c.get(k) {
                want.insert(k.into(), v.clone());
            }
        }
        if got != serde_json::Value::Object(want.clone()) {
            failed.push(format!(
                "--- {name}\n  got  {got}\n  Go   {}",
                serde_json::Value::Object(want)
            ));
        }
    }
    let data = b64.decode(answers["data"].as_str().unwrap()).unwrap();
    let formats = Formats {
        quote: &quote,
        time: &utc,
    };
    for c in answers["verify"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let got = pgpsign::verify_git_signature(
            c["signature"].as_str().unwrap().as_bytes(),
            &data,
            c["keys"].as_str().unwrap().as_bytes(),
            0,
            &formats,
        )
        .err()
        .unwrap_or_default();
        let want = c["error"].as_str().unwrap();
        if got != want {
            failed.push(format!("--- {name}\n  got  {got:?}\n  Go   {want:?}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
    // Every kind of certificate x/crypto reads, among the answers.
    assert!(certificates >= 20, "{certificates}");
}
