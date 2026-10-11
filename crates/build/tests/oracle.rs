//! Holds shards' file operations and layers to BuildKit's, byte for byte: each case of
//! testdata/ops.json run here must fail as BuildKit's file backend failed, or write the
//! layer its overlay differ wrote, as testdata/ops-answers.json records them
//! (scripts/build/generate).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing,
    clippy::unreachable
)]

mod cases;

use cases::{CONTEXT_DIRS, SENTINEL, run_actions, source, tree, unbase64};
use serde_json::Value;
use shards_build::data::Sources;
use shards_build::diff;
use shards_build::vfs::Fs;
use shards_image::tar;

/// Runs a case's actions; what fails is BuildKit's message.
fn run(case: &Value, mem: &mut Sources) -> Result<Vec<u8>, String> {
    let lower = Fs::new(tree(&case["lower"], mem), SENTINEL);
    let src = source(case, mem);
    let mut upper = lower.clone();
    upper.begin();
    run_actions(&lower, &src, &mut upper, &case["actions"], mem)?;
    let mut out = Vec::new();
    diff::write_layer(&lower, &upper, mem, &mut out).map_err(|e| e.0)?;
    Ok(out)
}

/// A layer's members, for a readable failure.
fn listing(layer: &[u8]) -> String {
    let mut r = tar::Reader::new(layer);
    let mut out = String::new();
    while let Ok(Some(e)) = r.next_entry() {
        out.push_str(&format!(
            "{} {:?} {:o} {}:{} {}.{} {} {:?} {:?}\n",
            String::from_utf8_lossy(&e.path),
            e.kind,
            e.mode,
            e.uid,
            e.gid,
            e.mtime,
            e.mtime_nsec,
            e.size,
            String::from_utf8_lossy(&e.link),
            e.xattrs
                .keys()
                .map(|k| String::from_utf8_lossy(k).into_owned())
                .collect::<Vec<_>>()
        ));
    }
    out
}

#[test]
fn file_operations_and_layers_match_buildkit() {
    let cases: Value = serde_json::from_str(include_str!("../testdata/ops.json")).unwrap();
    let answers: Value = serde_json::from_str(include_str!("../testdata/ops-answers.json")).unwrap();
    let mut failed = Vec::new();
    for (case, answer) in cases.as_array().unwrap().iter().zip(answers.as_array().unwrap()) {
        let name = case["name"].as_str().unwrap();
        assert_eq!(answer["name"].as_str(), Some(name));
        if cfg!(not(unix)) && case.get("context").is_some() {
            continue;
        }
        let mut mem = Sources::default();
        let got = run(case, &mut mem);
        CONTEXT_DIRS.with(|d| d.borrow_mut().clear());
        let want_err = answer["error"].as_str().unwrap_or("");
        match got {
            Err(e) if e == want_err => {}
            Err(e) => failed.push(format!("{name}: error {e:?}, BuildKit's {want_err:?}")),
            Ok(_) if !want_err.is_empty() => {
                failed.push(format!("{name}: no error, BuildKit's {want_err:?}"))
            }
            Ok(layer) => {
                let want = unbase64(answer["layer"].as_str().unwrap());
                if layer != want {
                    failed.push(format!(
                        "{name}: layer differs\n--- shards\n{}--- BuildKit\n{}",
                        listing(&layer),
                        listing(&want)
                    ));
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases:\n{}",
        failed.len(),
        cases.as_array().unwrap().len(),
        failed.join("\n")
    );
}
