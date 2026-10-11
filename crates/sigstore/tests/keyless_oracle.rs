//! shards' keyless signing held to what cosign v3.1.3's libraries send and make
//! (scripts/keyless/generate: sigstore-go v1.2.2's own Fulcio, timestamp authority, Rekor
//! v1 and Rekor v2 clients against test servers): each request byte for byte, given the
//! same key, proof and signature; each log's answer made the bundle's entry as Go makes
//! it; the bundle as protojson writes it; the services selected; the subjects of tokens.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use serde_json::Value;
use shards_sigstore::signing_config::{self, Selector, Service, ServiceConfiguration};
use shards_sigstore::{fulcio, keyless, rekor, tsa};

fn oracle() -> Value {
    serde_json::from_slice(include_bytes!("../testdata/keyless-oracle.json")).unwrap()
}

fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// JSON with the white space outside its strings removed: protojson adds some at random
/// in some builds (detrand), which is no part of what it means.
fn compact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut quoted, mut escaped) = (false, false);
    for c in s.chars() {
        if quoted {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
        } else if c == '"' {
            quoted = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

fn bundle_field<'v>(b: &'v Value, path: &[&str]) -> &'v Value {
    path.iter().fold(b, |v, k| &v[*k])
}

#[test]
fn fulcio_is_asked_as_sigstore_go_asks() {
    let o = oracle();
    let ours = fulcio::request(
        "ECDSA",
        o["keyPem"].as_str().unwrap(),
        &unb64(o["proof"].as_str().unwrap()),
    );
    assert_eq!(
        String::from_utf8(ours).unwrap(),
        o["fulcio"]["request"].as_str().unwrap()
    );
    // The leaf of the chain it answered.
    let answer = o["fulcio"]["answer"].as_str().unwrap();
    assert_eq!(
        fulcio::certificate(answer.as_bytes()).unwrap(),
        unb64(o["certDer"].as_str().unwrap())
    );
}

#[test]
fn the_timestamp_authority_is_asked_as_sigstore_go_asks() {
    let o = oracle();
    let ours = tsa::request(&unb64(o["signature"].as_str().unwrap()));
    assert_eq!(ours, unb64(o["tsa"]["request"].as_str().unwrap()));
}

/// Rekor v1: the proposed entry of the envelope and certificate, byte for byte; its answer
/// the entry protojson writes.
#[test]
fn rekor_v1_is_asked_and_answered_as_sigstore_go_does() {
    let o = oracle();
    let b: Value = serde_json::from_str(o["bundleV1"].as_str().unwrap()).unwrap();
    let cert = unb64(
        bundle_field(&b, &["verificationMaterial", "certificate", "rawBytes"])
            .as_str()
            .unwrap(),
    );
    let pem = shards_sigstore::tlog::pem::encode("CERTIFICATE", &cert);
    let payload = unb64(o["payload"].as_str().unwrap());
    let envelope = rekor::envelope_json(
        &payload,
        "application/vnd.in-toto+json",
        &unb64(o["signature"].as_str().unwrap()),
    );
    let ours = rekor::proposed_dsse(&envelope, &pem);
    assert_eq!(
        String::from_utf8(ours).unwrap(),
        o["rekorV1"]["request"].as_str().unwrap()
    );
    let entry = rekor::entry_v1(
        o["rekorV1"]["answer"].as_str().unwrap().as_bytes(),
        o["rekorV1"]["etag"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(rekor::entry_json(&entry), compact(o["entryV1"].as_str().unwrap()));
    // An answer not of the ETag's entry is refused, where Go would read nil fields.
    assert!(rekor::entry_v1(o["rekorV1"]["answer"].as_str().unwrap().as_bytes(), "other").is_err());
}

/// Rekor v2: the hashedrekord request over the envelope's PAE digest, byte for byte; its
/// answer, the entry.
#[test]
fn rekor_v2_is_asked_and_answered_as_sigstore_go_does() {
    let o = oracle();
    let b: Value = serde_json::from_str(o["bundleV2"].as_str().unwrap()).unwrap();
    let cert = unb64(
        bundle_field(&b, &["verificationMaterial", "certificate", "rawBytes"])
            .as_str()
            .unwrap(),
    );
    let sig = unb64(b["dsseEnvelope"]["signatures"][0]["sig"].as_str().unwrap());
    let payload = unb64(o["payload"].as_str().unwrap());
    let pae = shards_sigstore::sign::pae("application/vnd.in-toto+json", &payload);
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &pae);
    let ours = rekor::request_v2(digest.as_ref(), &sig, Some(&cert), &[], "PKIX_ECDSA_P256_SHA_256");
    assert_eq!(
        compact(&String::from_utf8(ours).unwrap()),
        compact(o["rekorV2"]["request"].as_str().unwrap())
    );
    let entry = rekor::entry_v2(o["rekorV2"]["answer"].as_str().unwrap().as_bytes()).unwrap();
    assert_eq!(rekor::entry_json(&entry), compact(o["entryV2"].as_str().unwrap()));
}

/// The bundle sign.Bundle assembled, from the same certificate, timestamp, entry, payload
/// and signature, as protojson writes it.
#[test]
fn the_bundle_is_the_one_sign_bundle_assembles() {
    let o = oracle();
    for (key, entry) in [("bundleV1", "entryV1"), ("bundleV2", "entryV2")] {
        let go = o[key].as_str().unwrap();
        let b: Value = serde_json::from_str(go).unwrap();
        let vm = &b["verificationMaterial"];
        let cert = unb64(vm["certificate"]["rawBytes"].as_str().unwrap());
        let stamps: Vec<Vec<u8>> = vm["timestampVerificationData"]["rfc3161Timestamps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| unb64(t["signedTimestamp"].as_str().unwrap()))
            .collect();
        let e = rekor::entry_v2(o[entry].as_str().unwrap().as_bytes()).unwrap();
        let sig = unb64(b["dsseEnvelope"]["signatures"][0]["sig"].as_str().unwrap());
        let payload = unb64(o["payload"].as_str().unwrap());
        let ours = keyless::bundle_json(&cert, &[e], &stamps, &payload, &sig);
        assert_eq!(String::from_utf8(ours).unwrap(), compact(go), "{key}");
    }
}

fn service(v: &Value) -> Service {
    let at = |s: &Value| {
        s.as_str()
            .map(|s| shards_dockerfile::go::parse_rfc3339(s.as_bytes()).unwrap().unix())
    };
    Service {
        url: v["url"].as_str().unwrap().into(),
        major_api_version: u32::try_from(v["majorApiVersion"].as_u64().unwrap()).unwrap(),
        start: at(&v["start"]),
        end: at(&v["end"]),
        operator: v["operator"].as_str().unwrap().into(),
    }
}

#[test]
fn services_are_selected_as_sigstore_go_selects_them() {
    let o = oracle();
    let cases = o["selections"].as_array().unwrap();
    assert_eq!(cases.len(), 18);
    for c in cases {
        let services: Vec<Service> = c["services"].as_array().unwrap().iter().map(service).collect();
        let versions: Vec<u32> = c["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| u32::try_from(v.as_u64().unwrap()).unwrap())
            .collect();
        let now = shards_dockerfile::go::parse_rfc3339(c["now"].as_str().unwrap().as_bytes())
            .unwrap()
            .unix();
        let all = ServiceConfiguration {
            selector: Selector::All,
            count: 0,
        };
        match signing_config::select_services(
            &services,
            all,
            &versions,
            now,
            &mut signing_config::random_below,
        ) {
            Ok(got) => {
                let urls: Vec<&str> = got.iter().map(|s| s.url.as_str()).collect();
                let want: Vec<&str> = c["selected"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|u| u.as_str().unwrap())
                    .collect();
                assert_eq!(urls, want, "{c}");
            }
            Err(e) => assert!(e.starts_with(c["error"].as_str().unwrap()), "{e} for {c}"),
        }
    }
}

#[test]
fn a_tokens_subject_is_the_one_oauthflow_reads() {
    let o = oracle();
    for c in o["subjects"].as_array().unwrap() {
        let got = fulcio::subject_from_claims(c["claims"].as_str().unwrap().as_bytes());
        match c["subject"].as_str() {
            Some(want) => assert_eq!(got.unwrap(), want, "{c}"),
            None => assert_eq!(got.unwrap_err(), c["error"].as_str().unwrap(), "{c}"),
        }
    }
}
