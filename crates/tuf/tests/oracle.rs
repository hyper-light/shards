//! What this crate's updater makes of testdata/oracle.json's repositories, against what
//! go-tuf v2's made of them (scripts/tuf/generate): each fetched from memory at the same
//! time, the same error or the same target, and the same root version reached.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use base64::Engine as _;
use shards_tuf::updater::Updater;
use shards_tuf::{Fetch, FetchError};

struct Mem(BTreeMap<String, Vec<u8>>);

impl Fetch for Mem {
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, FetchError> {
        let Some(data) = self.0.get(url) else {
            return Err(FetchError::Status {
                url: url.to_string(),
                code: 404,
            });
        };
        if data.len() as u64 > max {
            return Err(FetchError::TooLong {
                url: url.to_string(),
                length: data.len() as u64,
                max,
            });
        }
        Ok(data.clone())
    }
}

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn run(c: &serde_json::Value, dir: &std::path::Path) -> (String, String, i64) {
    let files = Mem(c["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), b64(v.as_str().unwrap())))
        .collect());
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    if let Some(local) = c["local"].as_object() {
        for (k, v) in local {
            std::fs::write(dir.join(k), b64(v.as_str().unwrap())).unwrap();
        }
    }
    let now = shards_dockerfile::go::parse_rfc3339(c["now"].as_str().unwrap().as_bytes())
        .unwrap()
        .unix();
    let root = b64(c["root"].as_str().unwrap());
    let base = c["base"].as_str().unwrap();
    let mut up = match Updater::new(&root, dir, base, &files, false, now) {
        Ok(u) => u,
        Err(e) => return (e.to_string(), String::new(), 0),
    };
    let finish = |up: &Updater<'_>, e: String, sha: String| (e, sha, up.trusted.root.signed.version);
    if let Err(e) = up.refresh() {
        return finish(&up, e.to_string(), String::new());
    }
    let target = match up.target_info(c["target"].as_str().unwrap()) {
        Ok(t) => t,
        Err(e) => return finish(&up, e.to_string(), String::new()),
    };
    match up.download_target(&target) {
        Ok(data) => {
            let sha = hex(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &data).as_ref());
            finish(&up, String::new(), sha)
        }
        Err(e) => finish(&up, e.to_string(), String::new()),
    }
}

#[test]
fn repositories_are_trusted_as_go_tuf_trusts_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/oracle.json")).unwrap();
    let dir_guard = shards_testdir::TempDir::new("tuf-oracle").unwrap();
    let dir = dir_guard.join("tuf-oracle");
    let mut failed = Vec::new();
    for c in cases.as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let got = run(c, &dir);
        let want = (
            c["error"].as_str().unwrap().to_string(),
            c["targetSha256"].as_str().unwrap_or_default().to_string(),
            c["rootVersion"].as_i64().unwrap(),
        );
        if got != want {
            failed.push(format!("--- {name}\n  got  {got:?}\n  Go   {want:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
