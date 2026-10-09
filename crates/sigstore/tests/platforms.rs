//! containerd/platforms (as buildx v0.37.1 vendors it) against testdata/platforms.json
//! (scripts/sigstore/generate-image): over a grid of platforms, each one's Normalize and
//! FormatAll, which of the grid Only(it) matches, and Only(it).Less of every pair, on a
//! host of the GOOS the generator ran on (an empty OS is the host's).

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use serde_json::{Value, json};
use shards_sigstore::platforms::{self, Only, Platform};

fn platform(v: &Value) -> Platform {
    Platform {
        architecture: v["architecture"].as_str().unwrap().to_string(),
        os: v["os"].as_str().unwrap().to_string(),
        os_version: v["osVersion"].as_str().unwrap().to_string(),
        os_features: v["osFeatures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect(),
        variant: v["variant"].as_str().unwrap().to_string(),
    }
}

fn json_of(p: &Platform) -> Value {
    json!({"os": p.os, "architecture": p.architecture, "variant": p.variant, "osVersion": p.os_version, "osFeatures": p.os_features})
}

#[test]
fn platforms_match_and_order_as_containerd_s() {
    let file: Value = serde_json::from_str(include_str!("../testdata/platforms.json")).unwrap();
    let goos = match file["goos"].as_str().unwrap() {
        "darwin" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => panic!("generated on {other}"),
    };
    let cases = file["cases"].as_array().unwrap();
    let grid: Vec<Platform> = cases.iter().map(|c| platform(&c["platform"])).collect();
    let mut failed = Vec::new();
    for (c, p) in cases.iter().zip(&grid) {
        let only = Only::on(p, goos);
        let normal = json_of(&platforms::normalize_on(p, goos));
        if normal != c["normalized"] {
            failed.push(format!("{p:?} Normalize: {normal} != {}", c["normalized"]));
        }
        let fa = platforms::format_all(p);
        if fa != c["formatAll"].as_str().unwrap() {
            failed.push(format!("{p:?} FormatAll: {fa:?} != {}", c["formatAll"]));
        }
        let m: String = grid
            .iter()
            .map(|x| if only.matches(x) { '1' } else { '0' })
            .collect();
        if m != c["matches"].as_str().unwrap() {
            for (i, (g, w)) in m.chars().zip(c["matches"].as_str().unwrap().chars()).enumerate() {
                if g != w {
                    failed.push(format!("Only({p:?}).Match({:?}): {g} != {w}", grid[i]));
                }
            }
        }
        let want = c["less"].as_str().unwrap().as_bytes();
        let mut k = 0;
        for a in &grid {
            for b in &grid {
                let g = only.less(a, b);
                if g != (want[k] == b'1') {
                    failed.push(format!("Only({p:?}).Less({a:?}, {b:?}): {g}"));
                }
                k += 1;
            }
        }
    }
    let shown: Vec<&String> = failed.iter().take(60).collect();
    assert!(failed.is_empty(), "{} differ:\n{shown:#?}", failed.len());
}

#[test]
fn an_empty_os_is_this_host_s() {
    let p = Platform {
        architecture: "arm64".into(),
        ..Platform::default()
    };
    let want = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    assert_eq!(platforms::normalize(&p).os, want);
}
