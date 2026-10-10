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

/// The PKCS #8 of the oracle's key `name`.
fn oracle_pkcs8(name: &str) -> Vec<u8> {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../../sigstore/testdata/cosign/oracle.json")).unwrap();
    let k = oracle["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == name)
        .unwrap();
    let hex = k["pkcs8"].as_str().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap(), 16).unwrap())
        .collect()
}

/// Build policies see an OSI artifact as an object of a registry, as they see an image,
/// with its artifact type and its signatures (D116): `AGENT … FROM` is checked by its name
/// before it is taken, then pinned with the build's definition; `verify_image_signature`
/// holds it to a signature by the key the policy names. Unsigned, it is refused, the
/// policy's message said; signed, allowed, from the store where it was signed and from
/// the registry by a build that pulls it with its signature; with another key named,
/// refused again.
#[test]
fn policies_hold_an_agent_to_its_signature() {
    // Its build runs in a VM, and the test image it builds from carries a test guest
    // built for one.
    if common::cannot_run_vms() {
        return;
    }
    let (image, _) = common::served();
    let (pem, password, pkcs8) = cosign_key();
    let keys = TempDir::new("referrers-policy-key");
    let key = keys.join("cosign.key");
    std::fs::write(&key, &pem).unwrap();
    let ours = shards_sigstore::sign::Signer::from_pkcs8(&pkcs8).unwrap();
    let theirs = shards_sigstore::sign::Signer::from_pkcs8(&oracle_pkcs8("ecdsa p384 standard")).unwrap();
    let (port, _repos) = common::writable_registry();
    let home = TempDir::new("referrers-policy-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("COSIGN_PASSWORD", std::ffi::OsStr::new(&password)),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    let dir = TempDir::new("referrers-policy-agent");
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
    let name = format!("127.0.0.1:{port}/team/agent:1");
    let made = shards(&["build", "agent", dir.to_str().unwrap(), "-t", &name]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);

    let ctx = TempDir::new("referrers-policy-ctx");
    std::fs::write(
        ctx.join("Agentfile"),
        format!("FROM {image}\nAGENT main FROM {name}\n"),
    )
    .unwrap();
    std::fs::write(
        ctx.join("Agentfile.rego"),
        "package docker\n\ndefault allow := false\n\nallow if input.local\n\nallow if {\n  not input.image.artifactType\n  startswith(input.image.repo, \"127.0.0.1:\")\n}\n\nallow if {\n  input.image.artifactType == \"application/vnd.osi.agent.v1\"\n  verify_image_signature(input.image, \"cosign.pub\")\n}\n\ndeny_msg contains msg if {\n  not allow\n  input.image.artifactType\n  msg := sprintf(\"agent %s is not signed by our key\", [input.image.ref])\n}\n\ndecision := {\"allow\": allow, \"deny_msg\": deny_msg}\n",
    )
    .unwrap();
    let build = |home: &TempDir, public: &shards_sigstore::sign::Signer| {
        std::fs::write(
            ctx.join("cosign.pub"),
            shards_sigstore::sign::public_key_pem(public.public_key_der()),
        )
        .unwrap();
        common::run_shards_env_in(
            &ctx,
            &[],
            &["build", "--progress=plain", "--no-cache", "."],
            &[("SHARDS_HOME", home.as_os_str())],
            TIMEOUT,
        )
    };

    // Unsigned: refused by name, before it is taken.
    let refused = build(&home, &ours);
    assert_eq!(refused.status, Some(1), "{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains(&format!("Policy: agent {name} is not signed by our key")),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains(&format!(
            "could not resolve OSI artifact due to policy: source \"osi-artifact://{name}\" not allowed by policy: action DENY"
        )),
        "{}",
        refused.stderr
    );

    // The same policy asked of the artifact alone, as one about to be taken: `policy eval`
    // of the source, its kind read from its manifest.
    let eval = |args: &[&str]| {
        let mut argv = vec!["buildx", "policy", "eval", "--filename", "Agentfile"];
        argv.extend_from_slice(args);
        common::run_shards_env_in(&ctx, &[], &argv, &[("SHARDS_HOME", home.as_os_str())], TIMEOUT)
    };
    std::fs::write(
        ctx.join("cosign.pub"),
        shards_sigstore::sign::public_key_pem(ours.public_key_der()),
    )
    .unwrap();
    let source = format!("osi-artifact://{name}");
    let denied = eval(&[&source]);
    assert_eq!(
        (denied.status, denied.stderr.as_str()),
        (
            Some(1),
            format!("ERROR: policy denied: agent {name} is not signed by our key\n").as_str()
        )
    );
    let printed = eval(&[
        "--print",
        "--fields",
        "image.artifactType,image.checksum",
        &source,
    ]);
    assert_eq!(printed.status, Some(0), "{}", printed.stderr);
    assert!(
        printed
            .stdout
            .contains("\"artifactType\": \"application/vnd.osi.agent.v1\""),
        "{}",
        printed.stdout
    );

    // Signed here: allowed, checked by name and then pinned.
    let signed = shards(&["sign", "agent", &name, "--key", key.to_str().unwrap()]);
    assert_eq!(signed.status, Some(0), "{}", signed.stderr);
    let evaluated = eval(&[&source]);
    assert_eq!(
        (evaluated.status, evaluated.stderr.as_str()),
        (Some(0), ""),
        "{}",
        evaluated.stdout
    );
    let allowed = build(&home, &ours);
    assert_eq!(allowed.status, Some(0), "{}", allowed.stderr);
    assert!(
        allowed.stderr.contains(&format!(
            "policy decision for source osi-artifact://{name}: ALLOW"
        )),
        "{}",
        allowed.stderr
    );
    assert!(
        allowed.stderr.contains(&format!(
            "policy decision for source osi-artifact://{name}@sha256:"
        )),
        "{}",
        allowed.stderr
    );

    // Its provenance names the agent it took, by the digest it took: the record a signed
    // image vouches for, where a run takes the image (D116).
    let layout = TempDir::new("referrers-policy-layout");
    let out = common::run_shards_env_in(
        &ctx,
        &[],
        &[
            "build",
            "--progress=plain",
            "--provenance=mode=min",
            "-o",
            &format!("type=oci,dest={},tar=false", layout.display()),
            ".",
        ],
        &[("SHARDS_HOME", home.as_os_str())],
        TIMEOUT,
    );
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let blob = |d: &str| -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(layout.join("blobs/sha256").join(d.trim_start_matches("sha256:"))).unwrap(),
        )
        .unwrap()
    };
    let top: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let index = blob(top["manifests"][0]["digest"].as_str().unwrap());
    let attestation = index["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["annotations"]["vnd.docker.reference.type"] == "attestation-manifest")
        .unwrap();
    let attestation = blob(attestation["digest"].as_str().unwrap());
    let statement = blob(attestation["layers"][0]["digest"].as_str().unwrap());
    let predicate = &statement["predicate"];
    let materials = predicate["materials"]
        .as_array()
        .or(predicate["buildDefinition"]["resolvedDependencies"].as_array())
        .unwrap();
    let digest = made.stdout.trim().to_string();
    let hex = digest.trim_start_matches("sha256:");
    let agent = materials
        .iter()
        .find(|m| m["uri"].as_str().is_some_and(|u| u.starts_with("pkg:oci/agent@")))
        .unwrap_or_else(|| panic!("{materials:#?}"));
    assert_eq!(
        agent["uri"],
        format!("pkg:oci/agent@sha256%3A{hex}?repository_url=127.0.0.1:{port}%2Fteam%2Fagent&tag=1").as_str()
    );
    assert_eq!(agent["digest"]["sha256"], hex);

    // Another key named: refused.
    let other = build(&home, &theirs);
    assert_eq!(other.status, Some(1), "{}", other.stderr);

    // Pushed, and taken by a build elsewhere with its signature.
    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    let elsewhere = TempDir::new("referrers-policy-elsewhere");
    let pulled = build(&elsewhere, &ours);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);

    // Only an image's pin converts a source: a policy that pins an OSI artifact is
    // refused in BuildKit's words.
    std::fs::write(
        ctx.join("Agentfile.rego"),
        format!(
            "package docker\n\ndefault allow := false\n\nallow if input.local\n\nallow if {{\n  not input.image.artifactType\n  startswith(input.image.repo, \"127.0.0.1:\")\n}}\n\nallow if {{\n  input.image.artifactType\n  pin_image(input.image, \"sha256:{}\")\n}}\n\ndecision := {{\"allow\": allow}}\n",
            "1".repeat(64)
        ),
    )
    .unwrap();
    let pinned = build(&home, &ours);
    assert_eq!(pinned.status, Some(1), "{}", pinned.stderr);
    assert!(
        pinned
            .stderr
            .contains(&format!("cannot pin non-image source: \"osi-artifact://{name}\"")),
        "{}",
        pinned.stderr
    );
}

/// An agent's SBOM (D116): made by `shards build agent --sbom` with the scanner it names,
/// run over the agent's content as a build's result is scanned (D81), its config left out;
/// kept with it as a referrer of type `application/spdx+json` whose one layer is the
/// scanner's SPDX document, listed by `inspect`, and pushed with it.
#[test]
fn an_agents_sbom_is_a_referrer_scanned_as_buildkit_scans() {
    if common::cannot_run_vms() {
        return;
    }
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("referrers-sbom-home");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", common::kernel().as_os_str()),
        ("SHARDS_INIT", common::guest_init().as_os_str()),
    ];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env, TIMEOUT);
    // The scanner: the test guest's, as sboms_are_scanned_as_buildkit_scans_them makes it.
    let scanner_ctx = TempDir::new("referrers-sbom-scanner");
    std::fs::write(
        scanner_ctx.join("Dockerfile"),
        "FROM scratch\nCOPY testguest /bin/testguest\nENTRYPOINT [\"/bin/testguest\", \"sbomscan\"]\n",
    )
    .unwrap();
    std::fs::copy(common::test_guest(), scanner_ctx.join("testguest")).unwrap();
    let scanner = format!("127.0.0.1:{port}/test/scanner:1");
    let made = shards(&["build", "-t", &scanner, scanner_ctx.to_str().unwrap()]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let pushed = shards(&["push", &scanner]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);

    let dir = TempDir::new("referrers-sbom-agent");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho agent\n").unwrap();
    std::fs::write(dir.join("lib/data.txt"), "data\n").unwrap();
    std::fs::write(dir.join("agent.json"), r#"{"name":"main","version":"1.0.0"}"#).unwrap();
    let name = format!("127.0.0.1:{port}/team/scanned:1");
    let made = shards(&[
        "build",
        "agent",
        dir.to_str().unwrap(),
        "-t",
        &name,
        &format!("--sbom=generator={scanner}"),
    ]);
    assert_eq!(made.status, Some(0), "{}", made.stderr);
    let digest = made.stdout.trim().to_string();
    assert!(made.stderr.contains(&format!("of {digest}")), "{}", made.stderr);
    let inspected = shards(&["inspect", "agent", &name]);
    let doc: serde_json::Value = serde_json::from_str(&inspected.stdout).unwrap();
    let referrer = doc[0]["Referrers"][0].clone();
    assert_eq!(
        referrer["artifactType"], "application/spdx+json",
        "{}",
        inspected.stdout
    );
    let blob = |d: &str| {
        std::fs::read(
            home.join("images/blobs/sha256")
                .join(d.trim_start_matches("sha256:")),
        )
        .unwrap()
    };
    let manifest: serde_json::Value =
        serde_json::from_slice(&blob(referrer["digest"].as_str().unwrap())).unwrap();
    assert_eq!(manifest["subject"]["digest"], digest.as_str());
    let sbom: serde_json::Value =
        serde_json::from_slice(&blob(manifest["layers"][0]["digest"].as_str().unwrap())).unwrap();
    // The agent's files, its config left out.
    assert_eq!(
        sbom["files"],
        serde_json::json!(["lib/data.txt", "run.sh"]),
        "{sbom}"
    );

    let pushed = shards(&["push", "agent", &name]);
    assert_eq!(pushed.status, Some(0), "{}", pushed.stderr);
    assert!(pushed.stdout.contains("pushed SBOM sha256:"), "{}", pushed.stdout);
    let repos = repos.lock().unwrap();
    let (_, list) = &repos.manifests["team/scanned"][&digest.replacen(':', "-", 1)];
    let list: serde_json::Value = serde_json::from_slice(list).unwrap();
    assert_eq!(list["manifests"][0]["artifactType"], "application/spdx+json");
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
