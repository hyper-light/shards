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

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("shards-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The bug a build that reads in place would have: an edit after the context is read,
/// keeping the file's size, must not reach the layer.
#[test]
fn an_edit_after_the_context_is_read_does_not_reach_the_build() {
    use shards_build::data::Sources;
    use shards_image::erofs::Source as _;
    let ctx = tmp("snapshot-ctx");
    let stage = tmp("snapshot-stage");
    std::fs::write(ctx.join("f"), b"before").unwrap();
    let mut sources = Sources::default();
    let fs = context::load(&ctx, &Filters::default(), &mut sources, (0, 0), &stage).unwrap();
    std::fs::write(ctx.join("f"), b"after!").unwrap();
    let id = fs.lstat(b"/f").unwrap();
    let shards_image::erofs::Kind::File { size, data } = fs.node(id).unwrap().kind else {
        panic!("not a file")
    };
    let mut got = vec![0u8; size as usize];
    sources.read_at(data, 0, &mut got).unwrap();
    assert_eq!(got, b"before");
    std::fs::remove_file(ctx.join("f")).unwrap();
    let mut again = vec![0u8; size as usize];
    sources.read_at(data, 0, &mut again).unwrap();
    assert_eq!(again, b"before", "and a removal neither");
    drop(sources);
    std::fs::remove_dir_all(&ctx).unwrap();
    std::fs::remove_dir_all(&stage).unwrap();
}

/// A file rewritten without pause while it is taken is a state the file was in, or
/// refused as changing, and a refusal adds nothing to the stage. Each version is one
/// write, so every state the file passes through is a whole version, and a snapshot
/// mixing two could only be a torn read. Both ways of taking it: a large file cloned, a
/// small one packed.
#[test]
fn a_file_written_while_it_is_taken_is_one_version_or_refused() {
    race(4 << 20);
    race(64 << 10);
}

fn race(size: usize) {
    use shards_build::host::{Stage, Taken};
    use std::io::{Read, Seek, SeekFrom, Write};
    let dir = tmp(&format!("snapshot-race-{size}"));
    let stage_dir = dir.join("stage");
    std::fs::create_dir(&stage_dir).unwrap();
    let src = dir.join("f");
    std::fs::write(&src, vec![b'a'; size]).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    // Stops the writer however the checks end, a failed assertion included: the scope
    // joins it before the panic goes on.
    struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let mut taken = Vec::new();
    let mut stage = Stage::new(&stage_dir).unwrap();
    std::thread::scope(|s| {
        let _stop = Stop(&stop);
        s.spawn(|| {
            let mut f = std::fs::File::options().write(true).open(&src).unwrap();
            let mut n = 0u8;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n = n.wrapping_add(1);
                f.seek(SeekFrom::Start(0)).unwrap();
                let written = f.write(&vec![b'a' + n % 26; size]).unwrap();
                assert_eq!(written, size, "one write, one version");
            }
        });
        for _ in 0..200 {
            match stage.take(&src) {
                Ok((snap, t)) => {
                    assert_eq!(snap.size, size as u64);
                    taken.push(t);
                }
                Err(e) => assert!(e.to_string().contains("file changed as the build read it"), "{e}"),
            }
        }
    });
    let pack_path = stage.pack_path();
    stage.finish().unwrap();
    let mut pack = std::fs::File::open(&pack_path).unwrap();
    let packed = taken.iter().filter(|t| matches!(t, Taken::Pack(_))).count();
    assert_eq!(
        pack.metadata().unwrap().len(),
        (packed * size) as u64,
        "a refusal adds nothing"
    );
    for (i, t) in taken.iter().enumerate() {
        let b = match t {
            Taken::File(p) => std::fs::read(p).unwrap(),
            Taken::Pack(at) => {
                let mut b = vec![0u8; size];
                pack.seek(SeekFrom::Start(*at)).unwrap();
                pack.read_exact(&mut b).unwrap();
                b
            }
        };
        assert!(b.iter().all(|&c| c == b[0]), "snapshot {i} mixes versions");
    }
    let files = std::fs::read_dir(&stage_dir).unwrap().count();
    let cloned = taken.iter().filter(|t| matches!(t, Taken::File(_))).count();
    assert_eq!(
        files,
        cloned + 1,
        "the pack, and a file per clone kept, none refused"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A file a symlink replaced since the walk is not followed out of the context.
#[test]
fn a_symlink_put_in_a_files_place_is_not_followed() {
    let dir = tmp("snapshot-link");
    std::fs::write(dir.join("outside"), b"secret").unwrap();
    std::os::unix::fs::symlink(dir.join("outside"), dir.join("f")).unwrap();
    std::fs::create_dir(dir.join("stage")).unwrap();
    let mut stage = shards_build::host::Stage::new(&dir.join("stage")).unwrap();
    let e = stage.take(&dir.join("f")).unwrap_err();
    stage.finish().unwrap();
    assert_eq!(std::fs::metadata(dir.join("stage/pack")).unwrap().len(), 0, "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
}
