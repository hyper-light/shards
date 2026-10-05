//! `commit::build_from_config` against moby's `BuildFromConfig` (scripts/commit-changes/
//! generate): each case of testdata/commit-changes.json, its changes applied to its base
//! config, gives the config or error dockerd gave. Where testdata/deviations.json names a
//! case (kind `commit`), its `ours` is what shards gives instead, by design, with its
//! reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;

use serde_json::{Value, json};
use shards_dockerfile::commit::build_from_config;

fn load(name: &str) -> Value {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata")).join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn changes_apply_as_dockerd_applies_them() {
    let devs: Vec<Value> = load("deviations.json")
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["kind"] == "commit")
        .inspect(|d| {
            assert!(
                d["reason"].as_str().is_some_and(|r| !r.is_empty()),
                "{d}: no reason"
            )
        })
        .cloned()
        .collect();
    let cases = load("commit-changes.json");
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 150, "{} cases", cases.len());
    let mut used = 0;
    let mut failures = Vec::new();
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let changes: Vec<String> = serde_json::from_value(c["changes"].clone()).unwrap();
        let ours = match build_from_config(&c["base"], &changes, c["os"].as_str().unwrap()) {
            Ok(config) => json!({ "config": config }),
            Err(e) => json!({ "error": e }),
        };
        let mut want = match c.get("error") {
            Some(e) => json!({ "error": e }),
            None => json!({ "config": c["config"] }),
        };
        if let Some(d) = devs.iter().find(|d| d["name"] == name) {
            used += 1;
            want = d["ours"].clone();
        }
        if ours != want {
            failures.push(format!("{name}:\n  ours: {ours}\n  want: {want}"));
        }
    }
    assert_eq!(used, devs.len(), "a deviation names no case");
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
