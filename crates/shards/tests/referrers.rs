//! OSI artifacts' signatures as OCI 1.1 referrers (D116): an agent signed as cosign v3.1.3
//! signs with a key, the signature kept with it and listed by `inspect`, pushed with it to
//! a registry with no referrers API (its list under the referrers tag schema, as
//! distribution v3.1.2 takes it) and to one with the API (as zot v2.1.22 has it), pulled
//! with it, and deleted from the registry and the list.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::time::Duration;

use common::{TempDir, run_shards_env};

const TIMEOUT: Duration = Duration::from_secs(120);
const BUNDLE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";

/// A key cosign v3.1.3 encrypted, and its password (crates/sigstore's oracle).
fn cosign_key() -> (String, String, Vec<u8>) {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../../sigstore/testdata/cosign/oracle.json")).unwrap();
    let k = oracle["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "ecdsa p256 standard")
        .unwrap();
    let hex = k["pkcs8"].as_str().unwrap();
    let pkcs8: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap(), 16).unwrap())
        .collect();
    (
        k["pem"].as_str().unwrap().to_string(),
        k["password"].as_str().unwrap().to_string(),
        pkcs8,
    )
}

/// An agent of several platforms is signed as its index: the index is what its name
/// resolves to, in the store and in the registry, as cosign signs the digest a name
/// resolves to.
#[test]
fn an_index_is_what_a_signature_of_several_platforms_names() {
    let (pem, password, _) = cosign_key();
    let keys = TempDir::new("referrers-index-key");
    let key = keys.join("cosign.key");
    std::fs::write(&key, &pem).unwrap();
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("referrers-index-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("COSIGN_PASSWORD", std::ffi::OsStr::new(&password)),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("referrers-index-agent");
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
    let name = format!("127.0.0.1:{port}/team/multi:1");
    let made = shards(&[
        "build",
        "agent",
        dir.to_str().unwrap(),
        "-t",
        &name,
        "--platform",
        "linux/amd64,linux/arm64",
    ]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let index = made.stdout.trim().to_string();
    let signed = shards(&["sign", "agent", &name, "--key", key.to_str().unwrap()]);
    assert_eq!(signed.status, Some(0), "{}", signed.stderr);
    assert!(
        signed.stdout.contains(&format!("{index} signed by the key ")),
        "{}",
        signed.stdout
    );
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let repos = repos.lock().unwrap();
    let manifests = &repos.manifests["team/multi"];
    assert_eq!(common::sha256_digest(&manifests["1"].1), index);
    let (_, list) = &manifests[&index.replacen(':', "-", 1)];
    let list: serde_json::Value = serde_json::from_slice(list).unwrap();
    let signature = list["manifests"][0]["digest"].as_str().unwrap();
    let m: serde_json::Value = serde_json::from_slice(&manifests[signature].1).unwrap();
    assert_eq!(m["subject"]["digest"], index.as_str());
    assert_eq!(
        m["subject"]["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
}

#[test]
fn signatures_are_referrers_pushed_pulled_and_deleted_as_cosigns_are() {
    let (pem, password, pkcs8) = cosign_key();
    let keys = TempDir::new("referrers-key");
    let key = keys.join("cosign.key");
    std::fs::write(&key, &pem).unwrap();
    let public = shards_sigstore::sign::key_of_spki(
        shards_sigstore::sign::Signer::from_pkcs8(&pkcs8)
            .unwrap()
            .public_key_der(),
    )
    .unwrap();
    for api in [false, true] {
        let (port, repos) = common::writable_registry();
        repos.lock().unwrap().referrers_api = api;
        let home = TempDir::new("referrers-home");
        let env = [
            ("SHARDS_HOME", home.as_os_str()),
            ("COSIGN_PASSWORD", std::ffi::OsStr::new(&password)),
        ];
        let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
        let dir = TempDir::new("referrers-agent");
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho agent\n").unwrap();
        std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
        let name = format!("127.0.0.1:{port}/team/signed:1");
        let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &name]);
        assert_eq!(made.status, Some(0), "{}", made.stderr);
        let digest = made.stdout.trim().to_string();

        // Signed: kept with it, shown by inspect, its subject what the name names.
        let signed = shards(&["sign", "agent", &name, "--key", key.to_str().unwrap()]);
        assert_eq!(signed.status, Some(0), "{}", signed.stderr);
        assert!(
            signed.stdout.contains(&format!("{digest} signed by the key ")),
            "{}",
            signed.stdout
        );
        let inspected = shards(&["inspect", "agent", &name]);
        let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
        let referrers = doc[0]["Referrers"].as_array().unwrap().clone();
        assert_eq!(referrers.len(), 1, "{}", inspected.stdout);
        assert_eq!(referrers[0]["artifactType"], BUNDLE);
        assert_eq!(
            referrers[0]["annotations"]["dev.sigstore.bundle.predicateType"],
            "https://sigstore.dev/cosign/sign/v1"
        );
        let signature = referrers[0]["digest"].as_str().unwrap().to_string();
        // Signing a key without its password says why.
        let bad = run_shards_env(
            &[],
            &["sign", "agent", &name, "--key", key.to_str().unwrap()],
            &[
                ("SHARDS_HOME", home.as_os_str()),
                ("COSIGN_PASSWORD", std::ffi::OsStr::new("another")),
            ],
            TIMEOUT,
        );
        assert_ne!(bad.status, Some(0));
        assert!(
            bad.stderr.contains("decrypt: encrypted: decryption failed"),
            "{}",
            bad.stderr
        );

        // Pushed with it: listed by the registry, or under the tag schema without an API.
        let pushed = shards(&["push", "agent", &name]);
        assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
        let how = if api {
            "the registry lists it"
        } else {
            "listed under the referrers tag schema"
        };
        assert!(
            pushed
                .stdout
                .contains(&format!("pushed signature {signature} ({how})")),
            "{}",
            pushed.stdout
        );
        let tag = digest.replacen(':', "-", 1);
        let bundle = {
            let repos = repos.lock().unwrap();
            let manifests = &repos.manifests["team/signed"];
            let listed = manifests.get(&tag);
            if api {
                assert!(listed.is_none(), "a tag schema list where the API lists it");
            } else {
                let (_, list) = listed.unwrap();
                let list: serde_json::Value = serde_json::from_slice(list).unwrap();
                assert_eq!(list["manifests"].as_array().unwrap().len(), 1, "{list}");
                assert_eq!(list["manifests"][0]["digest"], signature.as_str());
                assert_eq!(list["manifests"][0]["artifactType"], BUNDLE);
                // The manifest's annotations, copied (spec.md:508).
                assert_eq!(
                    list["manifests"][0]["annotations"]["dev.sigstore.bundle.content"],
                    "dsse-envelope"
                );
            }
            let (_, m) = &manifests[&signature];
            let m: serde_json::Value = serde_json::from_slice(m).unwrap();
            assert_eq!(m["subject"]["digest"], digest.as_str());
            let layer = m["layers"][0]["digest"].as_str().unwrap();
            repos.blobs["team/signed"][layer].clone()
        };
        // What was pushed verifies as cosign verify --key verifies it.
        shards_sigstore::sign::verify_key_signed(
            &bundle,
            &digest,
            &public,
            &shards_sigstore::trusted_root::TrustedRoot::default(),
            shards_sigstore::time::utc,
        )
        .unwrap();

        // A list under the tag schema is anyone's who may push: one that names a referrer
        // of another object too has that one left.
        let stray = if api {
            None
        } else {
            let other = format!("sha256:{}", "0".repeat(64));
            let manifest = format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"{BUNDLE}","config":{{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2}},"layers":[],"subject":{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{other}","size":1}}}}"#
            );
            let d = common::sha256_digest(manifest.as_bytes());
            let mut repos = repos.lock().unwrap();
            let manifests = repos.manifests.get_mut("team/signed").unwrap();
            let kind = "application/vnd.oci.image.manifest.v1+json".to_string();
            manifests.insert(d.clone(), (kind.clone(), manifest.clone().into_bytes()));
            let (list_kind, list) = manifests[&tag].clone();
            let mut list: serde_json::Value = serde_json::from_slice(&list).unwrap();
            list["manifests"].as_array_mut().unwrap().push(serde_json::json!({
                "mediaType": kind, "size": manifest.len(), "digest": d, "artifactType": BUNDLE,
            }));
            manifests.insert(tag.clone(), (list_kind, serde_json::to_vec(&list).unwrap()));
            Some(d)
        };

        // Pulled with it, nothing of it here first.
        let removed = shards(&["rm", "agent", &name]);
        assert_eq!(removed.status, Some(0), "{}", removed.stderr);
        let pulled = shards(&["pull", "agent", &name]);
        assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
        assert!(
            pulled.stdout.contains(&format!("signature {signature}")),
            "{}",
            pulled.stdout
        );
        if let Some(stray) = &stray {
            assert!(
                pulled
                    .stderr
                    .contains(&format!("referrer {stray} left: it does not refer to {digest}")),
                "{}",
                pulled.stderr
            );
            let mut repos = repos.lock().unwrap();
            let manifests = repos.manifests.get_mut("team/signed").unwrap();
            manifests.remove(stray);
            let (list_kind, list) = manifests[&tag].clone();
            let mut list: serde_json::Value = serde_json::from_slice(&list).unwrap();
            list["manifests"]
                .as_array_mut()
                .unwrap()
                .retain(|m| m["digest"] != stray.as_str());
            manifests.insert(tag.clone(), (list_kind, serde_json::to_vec(&list).unwrap()));
        }
        let inspected = shards(&["inspect", "agent", &name]);
        let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
        assert_eq!(doc[0]["Referrers"][0]["digest"], signature.as_str());

        // Deleted from the registry, and from its list there, and let go of here.
        let deleted = shards(&["rm", "agent", &name, "--referrer", &signature]);
        assert_eq!(deleted.status, Some(0), "{}", deleted.stderr);
        assert!(deleted.stdout.contains("here too"), "{}", deleted.stdout);
        {
            let repos = repos.lock().unwrap();
            let manifests = &repos.manifests["team/signed"];
            assert!(!manifests.contains_key(&signature));
            if !api {
                let (_, list) = &manifests[&tag];
                let list: serde_json::Value = serde_json::from_slice(list).unwrap();
                assert!(list["manifests"].as_array().unwrap().is_empty(), "{list}");
            }
        }
        let inspected = shards(&["inspect", "agent", &name]);
        let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
        assert!(doc[0]["Referrers"].as_array().unwrap().is_empty());
        // Pulled again, nothing refers to it.
        let pulled = shards(&["pull", "agent", &name]);
        assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
        assert!(!pulled.stdout.contains("signature"), "{}", pulled.stdout);
    }
}
