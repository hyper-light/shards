//! What this crate's VerifyImage makes of testdata/image.json's images, against what
//! BuildKit's policy helpers made of them (scripts/sigstore/generate-image): each index
//! resolved to its signature chain over the referrers Go was answered and the blobs it
//! read, the signature verified against the case's trusted root (and the DHI key it was
//! given), and the same error, or the same kind, signature type, signer, timestamps,
//! Docker reference and DHI flag. Also every referrers request, in Go's order.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use base64::Engine as _;
use serde_json::{Value, json};
use shards_sigstore::image::{self, Descriptor, Provider};
use shards_sigstore::platforms::Platform;
use shards_sigstore::summary::Summary;
use shards_sigstore::time::utc;
use shards_sigstore::trusted_root;
use shards_sigstore::verify::KeyMaterial;
use shards_sigstore::{Error, keys};

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// A descriptor as Go's encoding/json wrote it.
fn descriptor(v: &Value) -> Descriptor {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let annotations: BTreeMap<String, String> = v["annotations"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default();
    Descriptor {
        media_type: s("mediaType"),
        digest: s("digest"),
        size: v["size"].as_i64().unwrap_or(0),
        urls: strings(&v["urls"]),
        annotations,
        data: v["data"].as_str().map(b64).unwrap_or_default(),
        platform: v["platform"].as_object().map(|p| Platform {
            architecture: p["architecture"].as_str().unwrap().to_string(),
            os: p["os"].as_str().unwrap().to_string(),
            os_version: p
                .get("os.version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            os_features: p.get("os.features").map(strings).unwrap_or_default(),
            variant: p
                .get("variant")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        artifact_type: s("artifactType"),
    }
}

/// The registry Go saw: its blobs, and the referrers it was answered, each request once.
struct Replay {
    blobs: HashMap<String, Vec<u8>>,
    calls: Vec<Value>,
    asked: RefCell<Vec<String>>,
}

impl Provider for Replay {
    fn referrers(
        &self,
        digest: &str,
        artifact_types: &[&str],
        filters: &[(&str, &str)],
    ) -> Result<Vec<Descriptor>, String> {
        let mut f: Vec<(&str, &str)> = filters.to_vec();
        f.sort_by_key(|(k, _)| *k);
        let key = json!({"digest": digest, "artifactTypes": artifact_types, "filters": f});
        let n = self.asked.borrow().len();
        self.asked.borrow_mut().push(key.to_string());
        let Some(call) = self.calls.get(n) else {
            return Err(format!("unrecorded referrers request {key}"));
        };
        let want = json!({"digest": call["digest"], "artifactTypes": call["artifactTypes"], "filters": call["filters"]});
        if want != key {
            return Err(format!("referrers request {key}, Go asked {want}"));
        }
        if let Some(e) = call["error"].as_str().filter(|e| !e.is_empty()) {
            return Err(e.to_string());
        }
        Ok(call["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(descriptor)
            .collect())
    }

    fn read(&self, desc: &Descriptor) -> Result<Vec<u8>, String> {
        self.blobs
            .get(&desc.digest)
            .cloned()
            .ok_or_else(|| "not found".to_string())
    }
}

/// A PEM "PUBLIC KEY" block's key.
fn pem_key(pem: &str) -> shards_sigstore::x509::PublicKey {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    shards_sigstore::x509::parse_pkix_public_key(&b64(&body)).unwrap()
}

/// The summary as sigstore-go's Summary marshals it.
fn summary(s: &Summary) -> Value {
    let e = &s.extensions;
    json!({
        "certificateIssuer": s.certificate_issuer,
        "subjectAlternativeName": s.subject_alternative_name,
        "issuer": e.issuer,
        "githubWorkflowTrigger": e.github_workflow_trigger,
        "githubWorkflowSHA": e.github_workflow_sha,
        "githubWorkflowName": e.github_workflow_name,
        "githubWorkflowRepository": e.github_workflow_repository,
        "githubWorkflowRef": e.github_workflow_ref,
        "buildSignerURI": e.build_signer_uri,
        "buildSignerDigest": e.build_signer_digest,
        "runnerEnvironment": e.runner_environment,
        "sourceRepositoryURI": e.source_repository_uri,
        "sourceRepositoryDigest": e.source_repository_digest,
        "sourceRepositoryRef": e.source_repository_ref,
        "sourceRepositoryIdentifier": e.source_repository_identifier,
        "sourceRepositoryOwnerURI": e.source_repository_owner_uri,
        "sourceRepositoryOwnerIdentifier": e.source_repository_owner_identifier,
        "buildConfigURI": e.build_config_uri,
        "buildConfigDigest": e.build_config_digest,
        "buildTrigger": e.build_trigger,
        "runInvocationURI": e.run_invocation_uri,
        "sourceRepositoryVisibilityAtSigning": e.source_repository_visibility_at_signing,
    })
}

/// The case's outcome: error, result, and the referrers requests made.
fn run(roots: &Value, c: &Value) -> (String, Value, Vec<String>) {
    let root = trusted_root::parse(roots[c["root"].as_str().unwrap()].as_str().unwrap().as_bytes()).unwrap();
    let replay = Replay {
        blobs: c["blobs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), b64(v.as_str().unwrap())))
            .collect(),
        calls: c["referrers"].as_array().unwrap().clone(),
        asked: RefCell::new(Vec::new()),
    };
    let p = &c["platform"];
    let platform = Platform {
        architecture: p["architecture"].as_str().unwrap().to_string(),
        os: p["os"].as_str().unwrap().to_string(),
        os_version: p["osVersion"].as_str().unwrap().to_string(),
        os_features: strings(&p["osFeatures"]),
        variant: p["variant"].as_str().unwrap().to_string(),
    };
    let key = c["dhiKey"].clone();
    let dhi = move || -> Result<KeyMaterial, Error> {
        let k = key
            .as_object()
            .ok_or_else(|| Error("no DHI key in this case".into()))?;
        Ok(KeyMaterial {
            verifier: keys::load(&pem_key(k["pem"].as_str().unwrap()), keys::Load::default()).unwrap(),
            valid_from: k["validFrom"].as_i64().unwrap(),
        })
    };
    let out = image::verify_image_with(
        &replay,
        &descriptor(&c["index"]),
        &platform,
        &|| Ok(&root),
        utc,
        &dhi,
    );
    let asked = replay.asked.borrow().clone();
    match out {
        Ok(si) => {
            let timestamps: Vec<Value> = si
                .timestamps
                .iter()
                .map(|t| json!({"type": t.kind, "uri": t.uri, "secs": t.time.secs, "nanos": t.time.nanos}))
                .collect();
            let r = json!({
                "kind": si.kind.input_name(),
                "signatureType": si.signature_type.input_name(),
                "signer": si.signer.as_ref().map(summary),
                "timestamps": timestamps,
                "dockerReference": si.docker_reference,
                "isDHI": si.is_dhi,
            });
            (String::new(), r, asked)
        }
        Err(e) => (e.0, Value::Null, asked),
    }
}

#[test]
fn images_are_verified_as_buildkit_s_policy_helpers_verify_them() {
    let file: Value = serde_json::from_str(include_str!("../testdata/image.json")).unwrap();
    let mut failed = Vec::new();
    let all = file["cases"].as_array().unwrap();
    for c in all {
        let name = c["name"].as_str().unwrap();
        let (error, result, asked) = run(&file["roots"], c);
        // Go's protobuf prints "proto:" and a space or a no-break space, chosen per
        // binary; shards prints a space (D105).
        let want_error = c["error"].as_str().unwrap().replace("proto:\u{a0}", "proto: ");
        let want_asked: Vec<String> = c["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                json!({"digest": r["digest"], "artifactTypes": r["artifactTypes"], "filters": r["filters"]})
                    .to_string()
            })
            .collect();
        if (&error, &result) != (&want_error, &c["result"]) || asked != want_asked {
            failed.push(format!(
                "--- {name}\n  got  {error:?} {result}\n  Go   {want_error:?} {}\n  asked {asked:?}\n  Go    {want_asked:?}",
                c["result"]
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
