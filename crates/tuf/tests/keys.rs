//! RSA keys at crypto/rsa's bound on the public exponent (2^31-1) and past it, from
//! crates/sigstore/testdata/rsa_exponent.json (scripts/sigstore/rsa_exponent.go, which
//! records what Go makes of them), as go-tuf takes them (measured: go-tuf as buildx
//! v0.37.1 vendors it, Go 1.26.1): a PKIX key past the bound is read, and counts for
//! nothing toward a threshold, which other keys may still meet; a PKCS #1 one is refused,
//! and with it the delegation.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use base64::Engine as _;
use serde_json::Value;
use shards_tuf::keys::{self, Hash};
use shards_tuf::metadata::{Body, Metadata};
use shards_tuf::trusted::verify_delegate;

fn unhex(v: &Value) -> Vec<u8> {
    let s = v.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap(), 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn pem(kind: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|l| std::str::from_utf8(l).unwrap())
        .collect();
    format!(
        "-----BEGIN {kind}-----\n{}\n-----END {kind}-----\n",
        lines.join("\n")
    )
}

/// A root whose role lists an RSA key (`public`, a PEM) and then an Ed25519 key,
/// threshold one, signed by the Ed25519 key.
fn root(public: &str) -> Metadata {
    let ed = Ed25519KeyPair::generate().unwrap();
    let doc = |sigs: &str| {
        format!(
            r#"{{"signed":{{"_type":"root","spec_version":"1.0.31","version":1,"expires":"2031-01-01T00:00:00Z","consistent_snapshot":true,"keys":{{"rsa":{{"keytype":"rsa","scheme":"rsassa-pss-sha256","keyval":{{"public":{}}}}},"ed":{{"keytype":"ed25519","scheme":"ed25519","keyval":{{"public":"{}"}}}}}},"roles":{{"root":{{"keyids":["rsa","ed"],"threshold":1}}}}}},"signatures":{sigs}}}"#,
            serde_json::to_string(public).unwrap(),
            hex(ed.public_key().as_ref())
        )
    };
    let unsigned = Metadata::from_bytes("root", doc("[]").as_bytes()).unwrap();
    let sig = ed.sign(&unsigned.signed.canonical().unwrap());
    let signed = doc(&format!(r#"[{{"keyid":"ed","sig":"{}"}}]"#, hex(sig.as_ref())));
    Metadata::from_bytes("root", signed.as_bytes()).unwrap()
}

/// What VerifyDelegate refuses first (go-tuf v2.4.2 metadata.go, its loop over the role's
/// keys; measured as buildx v0.37.1 vendors it): each key is looked up and read before
/// the canonical form it is to verify is made, so a missing or unreadable key is refused
/// before a form cjson cannot write.
#[test]
fn keys_are_read_before_the_canonical_form_is_made() {
    for (keys, want) in [
        (r#"{}"#, "value error: key with ID k not found in root keyids"),
        (
            r#"{"k":{"keytype":"rsa","scheme":"rsassa-pss-sha256","keyval":{"public":"x"}}}"#,
            "PEM decoding failed",
        ),
        (
            r#"{"k":{"keytype":"ed25519","scheme":"ed25519","keyval":{"public":"00"}}}"#,
            "Can't canonicalize floating point number '1.5'",
        ),
    ] {
        let doc = format!(
            r#"{{"signed":{{"_type":"root","version":1,"x-n":1.5,"keys":{keys},"roles":{{"root":{{"keyids":["k"],"threshold":1}}}}}},"signatures":[]}}"#
        );
        let m = Metadata::from_bytes("root", doc.as_bytes()).unwrap();
        assert_eq!(
            verify_delegate(&m, "root", &m).map_err(|e| e.to_string()),
            Err(want.to_string())
        );
    }
}

#[test]
fn rsa_keys_past_the_exponent_bound_count_for_nothing() {
    let data: Value =
        serde_json::from_str(include_str!("../../sigstore/testdata/rsa_exponent.json")).unwrap();
    let message = data["message"].as_str().unwrap().as_bytes();
    for k in data["keys"].as_array().unwrap() {
        let e = &k["e"];
        let pkix = pem("PUBLIC KEY", &unhex(&k["pkix"]));
        let pkcs1 = pem("RSA PUBLIC KEY", &unhex(&k["pkcs1"]));
        let trusted = root(&pkix);
        let Body::Root {
            keys: Some(listed), ..
        } = &trusted.signed.body
        else {
            panic!("no keys");
        };
        let rsa = listed
            .iter()
            .find(|(id, _)| id == "rsa")
            .unwrap()
            .1
            .clone()
            .unwrap();
        let public = keys::to_public_key(&rsa).unwrap();
        let verified = public.verify(Hash::Sha256, message, &unhex(&k["signaturePss"]));
        assert_eq!(verified, k["verifyPss"] == "", "{e}");
        assert_eq!(verify_delegate(&trusted, "root", &trusted), Ok(()), "{e}");

        let r = root(&pkcs1);
        let refused = verify_delegate(&r, "root", &r).map_err(|e| e.to_string());
        match k["parsePkcs1"].as_str().unwrap() {
            // The Ed25519 key meets the threshold whatever the RSA key's signature.
            "" => assert_eq!(refused, Ok(()), "{e}"),
            go => assert_eq!(refused, Err(go.to_string()), "{e}"),
        }
    }
}
