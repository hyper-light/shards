//! RSA keys at crypto/rsa's bound on the public exponent (2^31-1) and past it, against
//! what Go made of them (scripts/sigstore/rsa_exponent.go): x509 parses both from PKIX,
//! PKCS #1 refuses the one past the bound, and every verification refuses a signature by
//! it, however good, in crypto/rsa's words.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use serde_json::Value;
use shards_sigstore::keys::{self, Load, VerifyWith};
use shards_sigstore::x509::{self, Hash, SigAlg};

fn unhex(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap(), 16).unwrap())
        .collect()
}

fn go(v: &Value) -> Result<(), String> {
    match v.as_str().unwrap() {
        "" => Ok(()),
        e => Err(e.to_string()),
    }
}

#[test]
fn keys_past_the_exponent_bound_verify_nothing() {
    let data: Value = serde_json::from_str(include_str!("../testdata/rsa_exponent.json")).unwrap();
    let message = data["message"].as_str().unwrap().as_bytes();
    let keys = data["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    for k in keys {
        let e = &k["e"];
        let key = x509::parse_pkix_public_key(&unhex(&k["pkix"])).map_err(|e| e.0);
        assert_eq!(
            key.as_ref().map(|_| ()).map_err(Clone::clone),
            go(&k["parsePkix"]),
            "{e}"
        );
        let key = key.unwrap();
        let pkcs1 = x509::parse_pkcs1_public_key(&unhex(&k["pkcs1"]))
            .map(|_| ())
            .map_err(|e| e.0);
        assert_eq!(pkcs1, go(&k["parsePkcs1"]), "{e}");
        let sig = unhex(&k["signature"]);
        let checked = x509::check_signature(SigAlg::Sha256Rsa, message, &sig, &key, false).map_err(|e| e.0);
        assert_eq!(checked, go(&k["verify"]), "{e}");
        let load = Load {
            hash: Some(Some(Hash::Sha256)),
            ..Load::default()
        };
        let verified = keys::load(&key, load)
            .unwrap()
            .verify(&sig, message, &VerifyWith::default());
        assert_eq!(verified, go(&k["verify"]), "{e}");
    }
}
