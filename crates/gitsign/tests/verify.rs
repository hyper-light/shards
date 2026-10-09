//! What this crate verifies of testdata/verify.json, against what buildx v0.37.1's
//! builtins answered (scripts/gitsign/generate): each Git object's signature
//! (verify_git_signature), each signature over its digest (verify_http_pgp_signature),
//! and each key ring read. The cases where shards answers otherwise on purpose are
//! named in `DEVIATIONS`, with their reason (D103).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use shards_gitsign::pgpsign::{self, Formats};

/// Cases shards answers otherwise, and what it answers.
const DEVIATIONS: &[(&str, &str)] = &[
    // BuildKit's digest check verifies with a revoked key; shards refuses one.
    (
        "revoked key digest",
        "failed to verify signature with checksum digest",
    ),
];

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Time.String in UTC, as the oracle ran with TZ=UTC.
fn utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} +0000 UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn ring(keys: &str) -> serde_json::Value {
    match pgpsign::read_all_armored_key_rings(keys.as_bytes()) {
        Err(e) => serde_json::json!({ "error": e }),
        Ok(entities) => {
            let list: Vec<serde_json::Value> = entities
                .iter()
                .map(|e| {
                    let mut ids: Vec<String> = e
                        .identities
                        .keys()
                        .map(|k| String::from_utf8_lossy(k).into_owned())
                        .collect();
                    ids.sort();
                    serde_json::json!({
                        "primary": format!("{:016x}", e.primary.key_id),
                        "version": e.primary.version,
                        "algo": e.primary.algo,
                        "fingerprint": hex(&e.primary.fingerprint),
                        "identities": ids,
                        "subkeys": e.subkeys.iter().map(|s| format!("{:016x}", s.key.key_id)).collect::<Vec<_>>(),
                        "revocations": e.revocations.len(),
                    })
                })
                .collect();
            if list.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({ "entities": list })
            }
        }
    }
}

#[test]
fn signatures_verify_as_buildx_verifies_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/verify.json")).unwrap();
    let quote = |b: &[u8]| shards_dockerfile::go::quote(b);
    let formats = Formats {
        quote: &quote,
        time: &utc,
    };
    let mut failed = Vec::new();
    let mut deviated = 0;
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let signature = c["signature"].as_str().unwrap();
        let keys = c["keys"].as_str().unwrap();
        let now = c["now"].as_i64().unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(c["data"].as_str().unwrap())
            .unwrap();
        let got = match c["kind"].as_str().unwrap() {
            "git" => {
                pgpsign::verify_git_signature(signature.as_bytes(), &data, keys.as_bytes(), now, &formats)
            }
            _ => (|| {
                let (sig, _) = shards_gitsign::parse_armored_detached_signature(signature.as_bytes())?;
                let ring = pgpsign::read_all_armored_key_rings(keys.as_bytes())?;
                let (algorithm, hex) = c["digest"].as_str().unwrap().split_once(':').unwrap();
                pgpsign::verify_signature_with_digest(&sig, &ring, algorithm, hex)
            })(),
        };
        let got = got.err().unwrap_or_default();
        let mut want = c["error"].as_str().unwrap().to_string();
        if let Some((_, ours)) = DEVIATIONS.iter().find(|(n, _)| *n == name) {
            deviated += 1;
            assert_ne!(want, *ours, "{name}: no longer a deviation");
            want = (*ours).to_string();
        }
        if got != want {
            failed.push(format!("--- {name}\n  got  {got:?}\n  Go   {want:?}"));
        }
        if !c["ring"].is_null() {
            let got = ring(keys);
            if got != c["ring"] {
                failed.push(format!("--- {name} (ring)\n  got  {got}\n  Go   {}", c["ring"]));
            }
        }
    }
    assert_eq!(deviated, DEVIATIONS.len());
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
