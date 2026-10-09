//! What this crate's verifier makes of testdata/oracle.json's bundles, against what
//! sigstore-go v1.2.2 made of them (scripts/sigstore/generate): each bundle verified
//! against its trusted root (and the DHI-like key material where a case has one) with
//! the options and policy BuildKit's policy helpers use, and the same error, or the same
//! certificate summary, public key ID, verified timestamps and statement.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]

use base64::Engine as _;
use shards_sigstore::keys;
use shards_sigstore::summary::Summary;
use shards_sigstore::time::{Time, utc};
use shards_sigstore::trusted_root;
use shards_sigstore::verify::{self, Config, Identity, KeyMaterial, Material, Outcome, Policy};

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

/// A PEM "PUBLIC KEY" block's key.
fn pem_key(pem: &str) -> shards_sigstore::x509::PublicKey {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    shards_sigstore::x509::parse_pkix_public_key(&b64(&body)).unwrap()
}

/// The summary as sigstore-go's Summary marshals it.
fn summary(s: &Summary) -> serde_json::Value {
    let e = &s.extensions;
    serde_json::json!({
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

/// The outcome in oracle.json's form.
fn result(o: &Outcome) -> serde_json::Value {
    let timestamps: Vec<serde_json::Value> = o
        .timestamps
        .iter()
        .map(
            |t| serde_json::json!({"type": t.kind, "uri": t.uri, "secs": t.time.secs, "nanos": t.time.nanos}),
        )
        .collect();
    let statement = o.statement.as_ref().map(|s| {
        let subjects: Vec<serde_json::Value> = s
            .subjects
            .iter()
            .map(|(name, digests)| {
                let mut d: Vec<(String, String)> = digests.clone();
                d.sort();
                serde_json::json!({"name": name, "digests": d.iter().map(|(a, h)| vec![a.clone(), h.clone()]).collect::<Vec<_>>()})
            })
            .collect();
        serde_json::json!({"predicateType": s.predicate_type, "subjects": subjects})
    });
    serde_json::json!({
        "certificate": o.certificate.as_ref().map(summary),
        "publicKeyId": o.public_key_id,
        "timestamps": timestamps,
        "statement": statement,
    })
}

fn run(roots: &serde_json::Value, c: &serde_json::Value) -> (String, serde_json::Value) {
    let json = roots[c["root"].as_str().unwrap()].as_str().unwrap();
    let root = match trusted_root::parse(json.as_bytes()) {
        Ok(r) => r,
        Err(e) => return (format!("trusted root: {e}"), serde_json::Value::Null),
    };
    let key = c["key"].as_object().map(|k| KeyMaterial {
        verifier: keys::load(&pem_key(k["pem"].as_str().unwrap()), keys::Load::default()).unwrap(),
        valid_from: k["validFrom"].as_i64().unwrap(),
    });
    let material = Material {
        root: &root,
        key: key.as_ref(),
        fulcio: c["fulcio"].as_bool().unwrap(),
    };
    let cf = &c["config"];
    let n = |k: &str| usize::try_from(cf[k].as_u64().unwrap()).unwrap();
    let config = Config {
        tlog: n("tlog"),
        observer: n("observer"),
        sct: n("sct"),
        signed: n("signed"),
        integrated: n("integrated"),
        no_observer: cf["noObserver"].as_bool().unwrap(),
    };
    let p = &c["policy"];
    let policy = Policy {
        digest: p["digest"].as_object().map(|d| {
            (
                d["alg"].as_str().unwrap().to_string(),
                unhex(d["hex"].as_str().unwrap()),
            )
        }),
        identity: match p["identity"].as_str().unwrap() {
            "unsafe" => Identity::Unsafe,
            _ => Identity::Any,
        },
    };
    let bundle = b64(c["bundle"].as_str().unwrap());
    let now = Time::utc(c["now"].as_i64().unwrap(), 0);
    match verify::verify(&bundle, &material, &config, &policy, utc, now) {
        Ok(o) => (String::new(), result(&o)),
        Err(e) => (e.to_string(), serde_json::Value::Null),
    }
}

#[test]
fn bundles_are_verified_as_sigstore_go_verifies_them() {
    let file: serde_json::Value = serde_json::from_str(include_str!("../testdata/oracle.json")).unwrap();
    let mut failed = Vec::new();
    let all = file["cases"].as_array().unwrap();
    for c in all {
        let name = c["name"].as_str().unwrap();
        let got = run(&file["roots"], c);
        // Go's protobuf prints "proto:" and a space or a no-break space, chosen per
        // binary; shards prints a space (D105).
        let error = c["error"].as_str().unwrap().replace("proto:\u{a0}", "proto: ");
        let want = (error, c["result"].clone());
        if got != want {
            failed.push(format!("--- {name}\n  got  {got:?}\n  Go   {want:?}"));
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
