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

use std::collections::BTreeMap;

#[cfg(unix)]
mod common;

use serde_json::Value;
use shards_build::copy::Chown;
use shards_build::data::Sources;
use shards_build::diff;
use shards_build::ops::{self, CopyAction};
use shards_build::vfs::Fs;
use shards_dockerfile::llb::{OpChown, OpUser};
use shards_image::erofs::{Kind, Meta, Node, NodeId, Tree};
use shards_image::tar;

/// The time the oracle moves what the kernel stamped to.
const SENTINEL: (i64, u32) = (2_000_000_000, 123_456_789);
const FIXTURE_TIME: (i64, u32) = (1_600_000_000, 500);

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("bad base64 {c}"),
        }
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let digits: Vec<u8> = chunk.iter().copied().filter(|&c| c != b'=').collect();
        let mut n = 0u32;
        for (i, &c) in digits.iter().enumerate() {
            n |= val(c) << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..digits.len().saturating_sub(1)]);
    }
    out
}

fn xattr_value(v: &str) -> Vec<u8> {
    match v.strip_prefix("base64:") {
        Some(rest) => unbase64(rest),
        None => v.as_bytes().to_vec(),
    }
}

fn find(tree: &Tree, path: &str) -> NodeId {
    let mut id = Tree::ROOT;
    for name in path.split('/').filter(|n| !n.is_empty()) {
        id = tree
            .child(id, name.as_bytes())
            .unwrap_or_else(|| panic!("{path}: no {name}"));
    }
    id
}

/// A tree as the oracle materializes `entries`: owned, chmodded, stamped.
fn tree(entries: &Value, mem: &mut Sources) -> Tree {
    let mut tree = Tree::new(Meta {
        mode: 0o755,
        mtime: FIXTURE_TIME.0,
        mtime_nsec: FIXTURE_TIME.1,
        ..Meta::default()
    });
    for e in entries.as_array().unwrap() {
        let path = e["path"].as_str().unwrap();
        let (dir, name) = path.rsplit_once('/').unwrap();
        let parent = find(&tree, dir);
        let ty = e["type"].as_str().unwrap();
        if ty == "hardlink" {
            let target = find(&tree, e["target"].as_str().unwrap());
            tree.link(parent, name.as_bytes(), target).unwrap();
            continue;
        }
        let mtime = e
            .get("mtime")
            .map(|m| (m[0].as_i64().unwrap(), m[1].as_u64().unwrap() as u32))
            .unwrap_or(FIXTURE_TIME);
        let default_mode = match ty {
            "dir" => 0o755,
            "symlink" => 0o777,
            _ => 0o644,
        };
        let mode = e.get("mode").and_then(Value::as_u64).unwrap_or(default_mode) as u16;
        let dev = || {
            (
                e["major"].as_u64().unwrap() as u32,
                e["minor"].as_u64().unwrap() as u32,
            )
        };
        let kind = match ty {
            "dir" => Kind::Dir(BTreeMap::new()),
            "file" => {
                let data = e.get("data").and_then(Value::as_str).unwrap_or("").as_bytes();
                Kind::File {
                    size: data.len() as u64,
                    data: mem.bytes(data.to_vec()).unwrap(),
                }
            }
            "symlink" => Kind::Symlink(e["target"].as_str().unwrap().as_bytes().to_vec()),
            "char" => {
                let (major, minor) = dev();
                Kind::CharDevice { major, minor }
            }
            "block" => {
                let (major, minor) = dev();
                Kind::BlockDevice { major, minor }
            }
            "fifo" => Kind::Fifo,
            other => panic!("unknown type {other}"),
        };
        let xattrs = e
            .get("xattrs")
            .and_then(Value::as_object)
            .map(|x| {
                x.iter()
                    .map(|(k, v)| (k.as_bytes().to_vec(), xattr_value(v.as_str().unwrap())))
                    .collect()
            })
            .unwrap_or_default();
        let meta = Meta {
            mode: if ty == "symlink" { 0o777 } else { mode },
            uid: e.get("uid").and_then(Value::as_u64).unwrap_or(0) as u32,
            gid: e.get("gid").and_then(Value::as_u64).unwrap_or(0) as u32,
            mtime: mtime.0,
            mtime_nsec: mtime.1,
            xattrs,
        };
        tree.insert(parent, name.as_bytes(), Node { kind, meta }).unwrap();
    }
    tree
}

fn owner(v: &Value) -> Option<OpChown> {
    let o = v.get("owner")?;
    let user = |u: &Value| -> Option<OpUser> {
        if u.is_null() {
            return None;
        }
        Some(match u.get("id") {
            Some(id) => OpUser::Id(id.as_u64().unwrap() as u32),
            None => OpUser::Name {
                name: u["name"].as_str().unwrap().as_bytes().to_vec(),
                input: 0,
            },
        })
    };
    Some(OpChown {
        user: o.get("user").and_then(user),
        group: o.get("group").and_then(user),
    })
}

fn flag(a: &Value, key: &str) -> bool {
    a.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn strings(a: &Value, key: &str) -> Vec<Vec<u8>> {
    a.get(key)
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .map(|s| s.as_str().unwrap().as_bytes().to_vec())
                .collect()
        })
        .unwrap_or_default()
}

/// Runs a case's actions; what fails is BuildKit's message.
fn run(case: &Value, mem: &mut Sources) -> Result<Vec<u8>, String> {
    let lower = Fs::new(tree(&case["lower"], mem), SENTINEL);
    let src = if case.get("context").and_then(Value::as_bool) == Some(true) {
        context_source(case, mem)
    } else {
        Fs::new(tree(&case["src"], mem), SENTINEL)
    };
    let mut upper = lower.clone();
    upper.begin();
    for a in case["actions"].as_array().unwrap() {
        let chown = owner(a);
        let ch = match &chown {
            Some(c) => ops::read_user(Some(c), Some(&lower), Some(&lower), mem).map_err(|e| e.0)?,
            None => Chown::Keep,
        };
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").as_bytes().to_vec();
        let mode = a["mode"].as_i64().unwrap() as i32;
        let ts = a["timestamp"].as_i64().unwrap();
        let r = match a["kind"].as_str().unwrap() {
            "mkdir" => ops::mkdir(&mut upper, &s("path"), mode, flag(a, "parents"), ch, ts),
            "mkfile" => {
                let data = s("data");
                let at = mem.bytes(data.clone()).unwrap();
                ops::mkfile(&mut upper, &s("path"), mode, (data.len() as u64, at), ch, ts)
            }
            "copy" => {
                let action = CopyAction {
                    src: s("src"),
                    dest: s("dest"),
                    mode,
                    mode_str: s("mode_str"),
                    follow_symlink: flag(a, "follow_symlink"),
                    dir_copy_contents: flag(a, "dir_copy_contents"),
                    attempt_unpack: false,
                    create_dest_path: flag(a, "create_dest_path"),
                    allow_wildcard: flag(a, "allow_wildcard"),
                    allow_empty_wildcard: flag(a, "allow_empty_wildcard"),
                    timestamp: ts,
                    include_patterns: strings(a, "include"),
                    exclude_patterns: strings(a, "exclude"),
                };
                ops::copy(&src, &mut upper, &action, ch)
            }
            // The oracle's deletion, os.RemoveAll, for the differ's whiteouts.
            "remove" => upper
                .remove_all(&s("path"))
                .map_err(|e| shards_build::Error(e.to_string())),
            // And its os.Lchown, for what chown clears; -1 leaves an ID.
            "lchown" => {
                let id = |k: &str| a[k].as_i64().unwrap() as u32;
                upper
                    .lchown(&s("path"), id("uid"), id("gid"))
                    .map_err(|e| shards_build::Error(e.to_string()))
            }
            other => panic!("unknown action {other}"),
        };
        r.map_err(|e| e.0)?;
    }
    let mut out = Vec::new();
    diff::write_layer(&lower, &upper, mem, &mut out).map_err(|e| e.0)?;
    Ok(out)
}

/// A case's source as a build context: its tree made in a directory and read as
/// BuildKit receives a context.
#[cfg(unix)]
fn context_source(case: &Value, mem: &mut Sources) -> Fs {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "shards-oracle-ctx-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    common::make(&dir, &case["src"]);
    let fs = shards_build::context::load(&dir, &Default::default(), mem, SENTINEL).unwrap();
    CONTEXT_DIRS.with(|d| d.borrow_mut().push(dir));
    fs
}

#[cfg(not(unix))]
fn context_source(_: &Value, _: &mut Sources) -> Fs {
    unreachable!("context cases run on Unix hosts")
}

thread_local! {
    /// Context directories made, removed once their cases' layers are written.
    static CONTEXT_DIRS: std::cell::RefCell<Vec<std::path::PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
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
        CONTEXT_DIRS.with(|d| {
            for dir in d.borrow_mut().drain(..) {
                std::fs::remove_dir_all(dir).unwrap();
            }
        });
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
