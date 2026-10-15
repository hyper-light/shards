//! `shards search image` (D172) against registries on loopback: a registry's catalog
//! searched for a term; each result said to run on this machine or not, as a pull chooses
//! its image, and what its pull takes; Docker's table piped, shards' fields by template;
//! refusals in dockerd's words; a registry's credentials found as a pull finds them. No
//! microVM, and no daemon: a search is the CLI's own.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::path::Path;

use base64::Engine as _;
use common::{TempDir, sha256_digest};

/// This machine's architecture as OCI names it: what its microVMs boot.
fn ours() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// `shards ARGS` with `config` its docker config and a `PATH` where no credential helper
/// is found: its status, stdout and stderr.
fn shards(dir: &Path, config: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let path = dir.join("path");
    std::fs::create_dir_all(&path).unwrap();
    let out = common::command()
        .args(args)
        .env("SHARDS_HOME", dir)
        .env("DOCKER_CONFIG", config)
        .env("PATH", &path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Puts into `repos` an image of `team/app` for this machine and s390x, at `latest`; one
/// of `team/lone` for s390x alone, a lone manifest; `team/old`, tagged `v1` alone; and
/// `other/thing`.
fn stock(repos: &std::sync::Mutex<common::Repos>) {
    let mut repos = repos.lock().unwrap();
    let oci_manifest = "application/vnd.oci.image.manifest.v1+json";
    let oci_index = "application/vnd.oci.image.index.v1+json";
    let manifest = |config: &str, size: usize, layers: &[usize]| {
        let layers: Vec<serde_json::Value> = layers
            .iter()
            .enumerate()
            .map(|(i, n)| {
                serde_json::json!({
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": format!("sha256:{i:064x}"),
                    "size": n,
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": oci_manifest,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config, "size": size},
            "layers": layers,
        }))
        .unwrap()
    };
    // team/app: an index, this machine's manifest and s390x's.
    let here = manifest("sha256:00", 2, &[1000, 234]);
    let there = manifest("sha256:01", 2, &[5]);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": oci_index,
        "manifests": [
            {"mediaType": oci_manifest, "digest": sha256_digest(&there), "size": there.len(), "platform": {"os": "linux", "architecture": "s390x"}},
            {"mediaType": oci_manifest, "digest": sha256_digest(&here), "size": here.len(), "platform": {"os": "linux", "architecture": ours()}},
        ],
    }))
    .unwrap();
    let app = repos.manifests.entry("team/app".into()).or_default();
    app.insert("latest".into(), (oci_index.into(), index));
    app.insert(sha256_digest(&here), (oci_manifest.into(), here));
    app.insert(sha256_digest(&there), (oci_manifest.into(), there));
    // team/lone: a manifest alone, its config s390x's.
    let config =
        br#"{"architecture":"s390x","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
    let lone = manifest(&sha256_digest(&config), config.len(), &[7]);
    repos
        .blobs
        .entry("team/lone".into())
        .or_default()
        .insert(sha256_digest(&config), config);
    repos
        .manifests
        .entry("team/lone".into())
        .or_default()
        .insert("latest".into(), (oci_manifest.into(), lone));
    // team/old: no latest.
    let old = manifest("sha256:02", 2, &[9]);
    repos
        .manifests
        .entry("team/old".into())
        .or_default()
        .insert("v1".into(), (oci_manifest.into(), old.clone()));
    repos
        .manifests
        .entry("other/thing".into())
        .or_default()
        .insert("latest".into(), (oci_manifest.into(), old));
}

/// A registry's catalog, searched for a term: Docker's table of what it finds, piped;
/// whether each runs here, on what, and what its pull takes, by template; the limit and
/// filters dockerd takes, and its refusals.
#[test]
fn a_registrys_images_are_found_and_said_to_run_here_or_not() {
    let dir = TempDir::new("search");
    let config = dir.join("docker");
    std::fs::create_dir_all(&config).unwrap();
    let (port, repos) = common::writable_registry();
    stock(&repos);
    let host = format!("127.0.0.1:{port}");
    let term = format!("{host}/team");
    // Docker's table, piped, the grammar's way and Docker's.
    for args in [
        vec!["search", "image", term.as_str()],
        vec!["search", term.as_str()],
    ] {
        let (status, out, err) = shards(&dir, &config, &args);
        assert_eq!(status, Some(0), "{err}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["NAME", "DESCRIPTION", "STARS", "OFFICIAL"],
            "{out}"
        );
        let names: Vec<&str> = lines[1..]
            .iter()
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                format!("{host}/team/app"),
                format!("{host}/team/lone"),
                format!("{host}/team/old")
            ],
            "{out}"
        );
    }
    // What runs here: as a pull chooses, a lone manifest by its config, `latest` alone.
    let (status, out, err) = shards(
        &dir,
        &config,
        &[
            "search",
            "--format",
            "{{.Name}}|{{.RunsHere}}|{{.Platform}}|{{.Platforms}}|{{.Size}}",
            &term,
        ],
    );
    assert_eq!(status, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "{host}/team/app|true|linux/{arch}|linux/{arch}|1.23kB\n{host}/team/lone|false||linux/s390x|\n{host}/team/old|false|||\n",
            arch = ours()
        )
    );
    // As many as the limit; filters as dockerd's: none official, none starred.
    let (_, out, _) = shards(
        &dir,
        &config,
        &["search", "--limit", "1", "--format", "{{.Name}}", &term],
    );
    assert_eq!(out, format!("{host}/team/app\n"));
    for filter in ["is-official=true", "stars=1"] {
        let (status, out, err) = shards(
            &dir,
            &config,
            &["search", "-f", filter, "--format", "{{.Name}}", &term],
        );
        assert_eq!((status, out.as_str()), (Some(0), ""), "{filter}: {err}");
    }
    let (status, out, err) = shards(&dir, &config, &["search", "-f", "is-automated=true", &term]);
    assert_eq!(status, Some(0), "{err}");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(err.contains("the \"is-automated\" filter is deprecated"), "{err}");
    // Refused in dockerd's words.
    for (args, said) in [
        (
            vec!["search", "--limit", "101", term.as_str()],
            "shards: limit 101 is outside the range of [1, 100]".to_string(),
        ),
        (
            vec!["search", "-f", "stars=x", term.as_str()],
            "shards: invalid filter 'stars=x': strconv.Atoi: parsing \"x\": invalid syntax".to_string(),
        ),
        (
            vec!["search", "-f", "nope=1", term.as_str()],
            "shards: invalid filter 'nope'".to_string(),
        ),
        (
            vec!["search", "http://r.example/x"],
            "shards: invalid repository name: repository name (http://r.example/x) should not have a scheme"
                .to_string(),
        ),
    ] {
        let (status, _, err) = shards(&dir, &config, &args);
        assert_eq!((status, err.trim_end()), (Some(1), said.as_str()), "{args:?}");
    }
    // A registry that keeps no catalog says so.
    let (manifest, blobs) = common::test_image();
    let (bare, _) = common::registry(manifest, blobs);
    let (status, _, err) = shards(&dir, &config, &["search", &format!("127.0.0.1:{bare}/test")]);
    assert_eq!(status, Some(1), "{err}");
    assert!(
        err.starts_with(&format!(
            "shards: 127.0.0.1:{bare} serves no catalog to search: 404"
        )),
        "{err}"
    );
}

/// A registry that asks for credentials is searched with those a login kept for it, as a
/// pull finds them; without them, it says who refused.
#[test]
fn a_registry_is_searched_with_the_credentials_kept_for_it() {
    let dir = TempDir::new("search-auth");
    let config = dir.join("docker");
    std::fs::create_dir_all(&config).unwrap();
    let basic = base64::engine::general_purpose::STANDARD.encode("finder:secret");
    let (port, repos) = common::writable_registry_requiring(Some(format!("Basic {basic}")));
    stock(&repos);
    let host = format!("127.0.0.1:{port}");
    let (status, _, err) = shards(&dir, &config, &["search", &format!("{host}/team")]);
    assert_eq!(
        (status, err.trim_end()),
        (
            Some(1),
            format!("shards: {host}: authorization failed: no basic auth credentials").as_str()
        )
    );
    std::fs::write(
        config.join("config.json"),
        serde_json::json!({"auths": {host.clone(): {"auth": basic}}}).to_string(),
    )
    .unwrap();
    let (status, out, err) = shards(
        &dir,
        &config,
        &[
            "search",
            "--format",
            "{{.Name}} {{.RunsHere}}",
            &format!("{host}/team/app"),
        ],
    );
    assert_eq!(
        (status, out.as_str()),
        (Some(0), format!("{host}/team/app true\n").as_str()),
        "{err}"
    );
}
