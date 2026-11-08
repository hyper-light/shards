//! RSA keys of every size of public exponent, held to what buildx v0.37.1 verifies them
//! with by testdata/rsa.json (scripts/gitsign/generate-rsa): Go's crypto/rsa, which
//! refuses an exponent above 2^31-1, even or below 2, and a modulus even or under 1024
//! bits, each in its words, before it looks at a signature (each key here has
//! signatures its own private exponent made, so a key wrongly taken would verify them);
//! x/crypto/ssh's ParsePublicKey, which refuses exponents of more than 24 bits; and
//! go-crypto's packet reader, which skips keys whose exponent is written in more than
//! three octets.

#![allow(clippy::unwrap_used, clippy::panic)]

use base64::Engine as _;
use shards_gitsign::signature::Hash;
use shards_gitsign::{Packet, Reader, arith, pem, ssh};

#[test]
fn rsa_keys_are_taken_and_refused_as_go_takes_and_refuses_them() {
    let o: serde_json::Value = serde_json::from_str(include_str!("../testdata/rsa.json")).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    let field = |c: &serde_json::Value, k: &str| b64.decode(c[k].as_str().unwrap()).unwrap();
    let mut failures = Vec::new();
    let mut too_large = 0;
    for c in o["verify"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let (n, e, digest) = (field(c, "n"), field(c, "e"), field(c, "digest"));
        let (pkcs1, pss) = (field(c, "pkcs1"), field(c, "pss"));
        let key_error = arith::rsa_key_error(&n, &e);
        if key_error.as_deref() != c["keyError"].as_str() {
            failures.push(format!("{name}: key error {key_error:?}, want {}", c["keyError"]));
        }
        too_large += usize::from(c["keyError"] == "crypto/rsa: public exponent too large");
        let got = [
            arith::rsa_pkcs1_verify(&n, &e, Hash::Sha256, &digest, &pkcs1),
            arith::rsa_pss_verify(&n, &e, Hash::Sha256, &digest, &pss, None),
            arith::rsa_pss_verify(&n, &e, Hash::Sha256, &digest, &pss, Some(32)),
        ];
        let want = ["pkcs1Answer", "pssAutoAnswer", "pssHashAnswer"].map(|k| c[k] == "ok");
        if got != want {
            failures.push(format!("{name}: verified {got:?}, want {want:?}"));
        }
    }
    assert!(too_large >= 3, "the oracle holds exponents above 2^31-1");
    for c in o["ssh"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let parsed = ssh::parse_public_key(&field(c, "wire"));
        let parse = match &parsed {
            Ok(k) => format!("ok {}", k.kind()),
            Err(e) => e.clone(),
        };
        if parse != c["parse"].as_str().unwrap() {
            failures.push(format!("ssh {name}: {parse}, want {}", c["parse"]));
        }
        if c["signature"].is_string() {
            let block = pem::decode(&field(c, "signature")).unwrap();
            let sig = ssh::parse_signature(&block.bytes).unwrap();
            let verify = match ssh::verify(&field(c, "message"), &sig, parsed.as_ref().unwrap()) {
                Ok(()) => "ok".to_string(),
                Err(e) => e,
            };
            if verify != c["verify"].as_str().unwrap() {
                failures.push(format!("ssh {name}: verified {verify}, want {}", c["verify"]));
            }
        }
    }
    for c in o["pgp"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let next = match Reader::new(&field(c, "packet")).next_packet() {
            Ok(Some(Packet::PublicKey(_))) => "ok".to_string(),
            Ok(None) => "EOF".to_string(),
            Ok(Some(p)) => format!("{p:?}"),
            Err(e) => e.to_string(),
        };
        if next != c["next"].as_str().unwrap() {
            failures.push(format!("pgp {name}: {next}, want {}", c["next"]));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
