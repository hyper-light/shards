//! Holds shards' walk of a build context to buildx's: the fixture tree of
//! testdata/contexts.json, made here as scripts/build/generate made it, must send what
//! fsutil sent under each case's filters (testdata/contexts-answers.json). A symlink's
//! permission bits are the host's own (0755 on macOS, 0777 on Linux) and BuildKit's
//! receiver ignores them, so they are not compared.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use serde_json::Value;
use shards_build::context::{self, Filters};
use shards_build::copy::fm;

fn strings(v: &Value, key: &str) -> Vec<Vec<u8>> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| s.as_str().unwrap().as_bytes().to_vec())
                .collect()
        })
        .unwrap_or_default()
}

fn mode_of(m: u64) -> u64 {
    if m as u32 & fm::SYMLINK != 0 {
        m & !0o777
    } else {
        m
    }
}

#[test]
fn contexts_are_sent_as_buildx_sends_them() {
    let spec: Value = serde_json::from_str(include_str!("../testdata/contexts.json")).unwrap();
    let answers: Value = serde_json::from_str(include_str!("../testdata/contexts-answers.json")).unwrap();
    let tmp = std::env::temp_dir().join(format!("shards-context-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    common::make(&tmp, &spec["tree"]);
    let root = std::fs::canonicalize(&tmp).unwrap();
    let mut failed = Vec::new();
    for (case, want) in spec["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(answers.as_array().unwrap())
    {
        let name = case["name"].as_str().unwrap();
        let filters = Filters {
            include: strings(case, "include"),
            exclude: strings(case, "exclude"),
            follow: strings(case, "follow"),
        };
        let got = match context::walk(&root, &filters) {
            Err(e) => serde_json::json!({ "name": name, "error": e.0 }),
            Ok(sent) => {
                let sent: Vec<Value> = sent
                    .iter()
                    .map(|(p, st)| {
                        serde_json::json!({
                            "path": String::from_utf8_lossy(p),
                            "mode": mode_of(u64::from(st.mode)),
                            "size": st.size,
                            "link": String::from_utf8_lossy(&st.link),
                            "mtime": st.mtime.0 * 1_000_000_000 + i64::from(st.mtime.1),
                        })
                    })
                    .collect();
                let sent = if sent.is_empty() {
                    Value::Null
                } else {
                    Value::Array(sent)
                };
                serde_json::json!({ "name": name, "sent": sent })
            }
        };
        let mut want = want.clone();
        if let Some(sent) = want.get_mut("sent").and_then(Value::as_array_mut) {
            for s in sent {
                let m = mode_of(s["mode"].as_u64().unwrap());
                s["mode"] = m.into();
            }
        }
        if got != want {
            failed.push(format!("{name}:\n shards   {got}\n buildx   {want}"));
        }
    }
    std::fs::remove_dir_all(&tmp).unwrap();
    assert!(
        failed.is_empty(),
        "{} cases:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
