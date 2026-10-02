//! The tree a build's layers stack to, written from its last snapshot (`stack.rs`), is
//! the one `Store::rootfs` stacks from the layers, byte for byte: each case of
//! testdata/ops.json, and the steps below, runs over a base image made of the case's
//! lower tree, each step's layer is written and committed, and the EROFS image of the
//! layers stacked by layer::apply must be the image of the last snapshot put in its
//! layers' form, unless the stack gave up on following it, as it must where noted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing,
    clippy::unreachable,
    clippy::print_stderr
)]

mod cases;

use std::fs::File;
use std::io::{Cursor, Write};

use cases::{CONTEXT_DIRS, SENTINEL, run_actions, source, tree};
use serde_json::{Value, json};
use shards_build::data::Sources;
use shards_build::diff;
use shards_build::stack::{Stack, Tally};
use shards_build::vfs::Fs;
use shards_image::erofs::{self, Kind, Meta, NodeId, Source, Tree};
use shards_image::layer;

/// One block of a tar header: its name and the fields layer::apply reads, every value
/// that a header cannot hold whole in a PAX record before it.
fn member(out: &mut Vec<u8>, path: &[u8], flag: u8, link: &[u8], n: &erofs::Node, size: u64) {
    let m = &n.meta;
    let mut records = Vec::new();
    let mut record = |k: &[u8], v: &[u8]| {
        // "<len> <k>=<v>\n", the length counting itself.
        let body = k.len() + v.len() + 3;
        let mut len = body + 1;
        while len != body + len.to_string().len() {
            len = body + len.to_string().len();
        }
        records.extend_from_slice(format!("{len} ").as_bytes());
        records.extend_from_slice(k);
        records.push(b'=');
        records.extend_from_slice(v);
        records.push(b'\n');
    };
    record(b"path", path);
    if !link.is_empty() {
        record(b"linkpath", link);
    }
    record(b"uid", m.uid.to_string().as_bytes());
    record(b"gid", m.gid.to_string().as_bytes());
    let mtime = if m.mtime < 0 && m.mtime_nsec > 0 {
        format!("-{}.{:09}", -(m.mtime + 1), 1_000_000_000 - m.mtime_nsec)
    } else {
        format!("{}.{:09}", m.mtime, m.mtime_nsec)
    };
    record(b"mtime", mtime.as_bytes());
    for (k, v) in m.xattrs.iter() {
        record(&[b"SCHILY.xattr.", k.as_slice()].concat(), v);
    }
    block(out, b"PaxHeader", b'x', b"", 0o644, records.len() as u64, (0, 0));
    out.extend_from_slice(&records);
    pad(out);
    let dev = match n.kind {
        Kind::CharDevice { major, minor } | Kind::BlockDevice { major, minor } => (major, minor),
        _ => (0, 0),
    };
    block(out, b"placeholder", flag, b"", u32::from(m.mode), size, dev);
}

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{v:0w$o}", w = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
}

fn block(out: &mut Vec<u8>, name: &[u8], flag: u8, link: &[u8], mode: u32, size: u64, dev: (u32, u32)) {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = flag;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..265].copy_from_slice(b"ustar\x0000");
    octal(&mut h[329..337], u64::from(dev.0));
    octal(&mut h[337..345], u64::from(dev.1));
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    out.extend_from_slice(&h);
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(512), 0);
}

/// A base image's layer holding `tree` whole, every attribute as it is: what a registry
/// image's layer may hold, which a build's own layers never do.
fn base_layer(tree: &Tree, data: &mut dyn Source) -> Vec<u8> {
    let mut out = Vec::new();
    let mut first: Vec<Option<Vec<u8>>> = vec![None; tree.len()];
    let mut todo: Vec<(Vec<u8>, NodeId)> = vec![(Vec::new(), Tree::ROOT)];
    while let Some((dir, id)) = todo.pop() {
        for (name, child) in tree.entries(id).into_iter().rev() {
            let path = if dir.is_empty() {
                name.to_vec()
            } else {
                [dir.as_slice(), b"/", name].concat()
            };
            let n = tree.node(child).unwrap();
            if let Some(Some(to)) = first.get(child)
                && !matches!(n.kind, Kind::Dir(_))
            {
                member(&mut out, &path, b'1', to, n, 0);
                continue;
            }
            first[child] = Some(path.clone());
            match &n.kind {
                Kind::Dir(_) => {
                    member(&mut out, &[path.as_slice(), b"/"].concat(), b'5', b"", n, 0);
                    todo.push((path, child));
                }
                Kind::File { size, data: at } => {
                    member(&mut out, &path, b'0', b"", n, *size);
                    let mut bytes = vec![0; *size as usize];
                    data.read_at(*at, 0, &mut bytes).unwrap();
                    out.extend_from_slice(&bytes);
                    pad(&mut out);
                }
                Kind::Symlink(t) => member(&mut out, &path, b'2', t, n, 0),
                Kind::CharDevice { .. } => member(&mut out, &path, b'3', b"", n, 0),
                Kind::BlockDevice { .. } => member(&mut out, &path, b'4', b"", n, 0),
                Kind::Fifo => member(&mut out, &path, b'6', b"", n, 0),
                Kind::Socket => {}
            }
        }
    }
    out.extend_from_slice(&[0; 1024]);
    out
}

/// `layer` as a source of `mem`'s, in a file of its own.
fn keep(layer: &[u8], mem: &mut Sources) -> u32 {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "shards-stack-layer-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    File::create(&path).unwrap().write_all(layer).unwrap();
    let id = mem.archive(File::open(&path).unwrap()).unwrap();
    // Open, it stays readable once its name is gone.
    std::fs::remove_file(&path).unwrap();
    id
}

/// layer::apply of `layer` onto `tree`, its files kept as source `id`, and what it holds.
fn apply(tree: &mut Tree, id: u32, layer: &[u8]) -> Tally {
    let mut tally = Tally {
        bytes: layer.len() as u64,
        ..Tally::default()
    };
    layer::apply(tree, id, Cursor::new(layer), &mut |e| {
        tally.entries += 1;
        tally.metadata += (e.path.len() + e.link.len()) as u64;
        tally.metadata += e.xattrs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() as u64;
        Ok(())
    })
    .unwrap();
    tree.compact();
    tally
}

/// The image `Store::rootfs` writes of `layers`.
fn stacked(layers: &[Vec<u8>]) -> Vec<u8> {
    let mut tree = layer::root();
    for (i, l) in layers.iter().enumerate() {
        apply(&mut tree, i as u32, l);
    }
    let mut out = Vec::new();
    let mut archives = layer::Archives(layers.iter().map(|l| Cursor::new(l.clone())).collect());
    erofs::write(&tree, &mut archives, &mut out).unwrap();
    out
}

/// The actions of a step that stack.rs's cases use beside the oracle's.
fn run_step(lower: &Fs, src: &Fs, upper: &mut Fs, actions: &Value, mem: &mut Sources) -> Result<(), String> {
    let s = |a: &Value, k: &str| a[k].as_str().unwrap().as_bytes().to_vec();
    for a in actions.as_array().unwrap() {
        let r = match a["kind"].as_str().unwrap() {
            "link" => upper.link(&s(a, "old"), &s(a, "new")),
            "unlink" => upper.unlink(&s(a, "path")),
            "rename" => upper.rename(&s(a, "old"), &s(a, "new")),
            "chmod" => upper.chmod(&s(a, "path"), a["mode"].as_u64().unwrap() as u32),
            "utimes" => upper.utimes(
                &s(a, "path"),
                (a["sec"].as_i64().unwrap(), a["nsec"].as_u64().unwrap() as u32),
            ),
            "setxattr" => upper.setxattr(&s(a, "path"), &s(a, "key"), &s(a, "value"), false),
            "socket" => upper.mknod(&s(a, "path"), Kind::Socket, 0o755).map(|_| ()),
            // Rewrites a file's bytes in place, as open(O_TRUNC) and a write do: its node
            // stays the same.
            "rewrite" => upper.create(&s(a, "path"), 0o644).map(|id| {
                let bytes = s(a, "data");
                let len = bytes.len() as u64;
                let data = mem.bytes(bytes).unwrap();
                upper.set_data(id, len, data);
            }),
            _ => {
                run_actions(lower, src, upper, &Value::Array(vec![a.clone()]), mem)?;
                Ok(())
            }
        };
        r.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Path {
    /// The case fails, as BuildKit's does: no image.
    Failed,
    /// The image written from the snapshot, and the same as the layers'.
    Snapshot,
    /// The stack gave up, for this reason: the export stacks the layers.
    Layers(String),
}

/// The paths a step changed that it did not record, found by walking both trees whole,
/// apart from the record: what changed must be among what the step stamped (Tree::mark),
/// as Git's fsmonitor must report a superset of the changes. A node differs if it is
/// another node, or the same one with other contents or attributes; a directory whose
/// entries changed differs as well. A directory made again stands for all below it. A hard-linked file's other names are left out: the
/// in-memory file system changes the one node all its names share, where overlayfs
/// without its index copies up only the path changed, which is still to be held against
/// BuildKit.
fn unrecorded(lower: &Fs, upper: &Fs) -> Vec<String> {
    let links = upper.links();
    let mut missed = Vec::new();
    let mut todo = vec![(Tree::ROOT, Tree::ROOT, String::new())];
    while let Some((l, u, path)) = todo.pop() {
        let mut changed = Vec::new();
        upper.tree().changed_into(u, &mut changed);
        let stamped: std::collections::BTreeSet<&[u8]> = changed.iter().map(|(n, _)| *n).collect();
        let below: std::collections::BTreeMap<Vec<u8>, usize> = lower
            .tree()
            .entries(l)
            .into_iter()
            .map(|(n, id)| (n.to_vec(), id))
            .collect();
        let above: std::collections::BTreeMap<Vec<u8>, usize> = upper
            .tree()
            .entries(u)
            .into_iter()
            .map(|(n, id)| (n.to_vec(), id))
            .collect();
        let names: std::collections::BTreeSet<&Vec<u8>> = below.keys().chain(above.keys()).collect();
        for name in names {
            let p = format!("{path}/{}", String::from_utf8_lossy(name));
            let (b, a) = (below.get(name).copied(), above.get(name).copied());
            let differs = match (b, a) {
                (Some(b), Some(a)) => b != a || lower.node(b) != upper.node(a),
                _ => true,
            };
            let shared = a.is_some_and(|a| !upper.is_dir(a) && links.get(a).copied().unwrap_or(0) > 1);
            if differs && !shared && !stamped.contains(name.as_slice()) {
                missed.push(p.clone());
            }
            // A directory made again is recorded where it was made: the differ then takes
            // all below it against the lower's (an opaque directory), so only one that is
            // the same node is walked into.
            if let (Some(b), Some(a)) = (b, a)
                && b == a
                && upper.is_dir(a)
            {
                todo.push((b, a, p));
            }
        }
    }
    missed
}

/// Runs `case`'s steps over a base image of its lower tree, and checks the image.
fn check(case: &Value) -> Path {
    let mut mem = Sources::default();
    let fixture = tree(&case["lower"], &mut mem);
    let base = base_layer(&fixture, &mut mem);
    let mut s0 = layer::root();
    let id = keep(&base, &mut mem);
    let tally = apply(&mut s0, id, &base);
    let mut layers = vec![base];
    let mut lower = Fs::new(s0, SENTINEL);
    let mut stack = Stack::layers(tally, lower.tree());
    let src = source(case, &mut mem);
    let steps = match case.get("steps") {
        Some(steps) => steps.clone(),
        None => json!([case["actions"]]),
    };
    for step in steps.as_array().unwrap() {
        if let Some(actions) = step.get("merge") {
            // COPY --link: the step on scratch, its layer then applied onto the snapshot.
            let scratch = || {
                Fs::new(
                    Tree::new(Meta {
                        mode: 0o755,
                        mtime: SENTINEL.0,
                        mtime_nsec: SENTINEL.1,
                        ..Meta::default()
                    }),
                    SENTINEL,
                )
            };
            let mut upper = scratch();
            if run_step(&scratch(), &src, &mut upper, actions, &mut mem).is_err() {
                return Path::Failed;
            }
            let mut out = Vec::new();
            diff::write_layer(&scratch(), &upper, &mut mem, &mut out).unwrap();
            let id = keep(&out, &mut mem);
            let mut merged = lower.clone();
            let applied = apply(merged.unrecorded_tree(), id, &out);
            merged.begin();
            stack = stack.merge(applied, merged.tree());
            layers.push(out);
            lower = merged;
            continue;
        }
        let mut upper = lower.clone();
        upper.begin();
        if run_step(&lower, &src, &mut upper, step, &mut mem).is_err() {
            return Path::Failed;
        }
        let missed = unrecorded(&lower, &upper);
        assert!(missed.is_empty(), "changed but not recorded: {missed:?}");
        let mut out = Vec::new();
        let rec = diff::write_layer(&lower, &upper, &mut mem, &mut out).unwrap();
        stack = stack
            .commit(&lower, &upper, rec, out.len() as u64, &mut mem)
            .unwrap();
        layers.push(out);
        upper.begin();
        lower = upper;
    }
    let want = stacked(&layers);
    let path = match stack.finish(&mut lower) {
        Err(why) => Path::Layers(why.to_string()),
        Ok(()) => {
            let mut got = Vec::new();
            erofs::write(lower.tree(), &mut mem, &mut got).unwrap();
            if got != want {
                return Path::Layers(format!("DIFFERENT IMAGE ({} vs {} bytes)", got.len(), want.len()));
            }
            Path::Snapshot
        }
    };
    CONTEXT_DIRS.with(|d| {
        for dir in d.borrow_mut().drain(..) {
            std::fs::remove_dir_all(dir).unwrap();
        }
    });
    path
}

#[test]
fn every_oracle_case_stacks_to_its_snapshot() {
    let cases: Value = serde_json::from_str(include_str!("../testdata/ops.json")).unwrap();
    let (mut snapshot, mut layers, mut failed, mut wrong) = (0, Vec::new(), 0, Vec::new());
    for case in cases.as_array().unwrap() {
        if cfg!(not(unix)) && case.get("context").is_some() {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        match check(case) {
            Path::Snapshot => snapshot += 1,
            Path::Failed => failed += 1,
            Path::Layers(why) if why.starts_with("DIFFERENT") => wrong.push(format!("{name}: {why}")),
            Path::Layers(why) => layers.push(format!("{name}: {why}")),
        }
    }
    eprintln!(
        "ops.json: {snapshot} from the snapshot, {} from the layers, {failed} failing as BuildKit's do",
        layers.len()
    );
    for l in &layers {
        eprintln!("  from the layers: {l}");
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The base image of the cases below.
fn base() -> Value {
    json!([
        {"path": "/etc", "type": "dir", "mtime": [1600000000, 987654321], "xattrs": {"user.dir": "d"}},
        {"path": "/etc/hosts", "type": "file", "data": "hosts", "mtime": [1600000001, 5],
         "xattrs": {"user.a": "1", "security.capability": "base64:AQAAAgAgAAAAAAAAAAAAAAAAAAA="}},
        {"path": "/etc/keep", "type": "file", "data": "keep", "xattrs": {"user.k": "v"}},
        {"path": "/var", "type": "dir"},
        {"path": "/var/log", "type": "dir", "mtime": [1600000002, 42]},
        {"path": "/var/log/old", "type": "file", "data": "old"},
        {"path": "/data", "type": "dir"},
        {"path": "/data/a", "type": "file", "data": "a"},
        {"path": "/data/sub", "type": "dir"},
        {"path": "/data/sub/x", "type": "file", "data": "x"},
        {"path": "/hl1", "type": "file", "data": "shared"},
        {"path": "/hl2", "type": "hardlink", "target": "/hl1"},
        {"path": "/lnk", "type": "symlink", "target": "etc"},
        {"path": "/null", "type": "char", "major": 1, "minor": 3},
    ])
}

fn mkfile(path: &str, data: &str) -> Value {
    json!({"kind": "mkfile", "path": path, "data": data, "mode": 0o644, "timestamp": -1})
}

fn mkdir(path: &str) -> Value {
    json!({"kind": "mkdir", "path": path, "mode": 0o755, "parents": true, "timestamp": -1})
}

fn remove(path: &str) -> Value {
    json!({"kind": "remove", "path": path, "mode": 0, "timestamp": -1})
}

fn utimes(path: &str, sec: i64, nsec: u32) -> Value {
    json!({"kind": "utimes", "path": path, "sec": sec, "nsec": nsec})
}

fn setxattr(path: &str, key: &str, value: &str) -> Value {
    json!({"kind": "setxattr", "path": path, "key": key, "value": value})
}

fn link(old: &str, new: &str) -> Value {
    json!({"kind": "link", "old": old, "new": new})
}

/// Steps over [`base`], and the path each must take: `None` for the snapshot, or the
/// reason the stack gives up.
fn steps() -> Vec<(&'static str, Value, Option<&'static str>)> {
    vec![
        (
            "two commits",
            json!([[mkfile("/new1", "1")], [mkdir("/etc/d2")]]),
            None,
        ),
        (
            "a whiteout of a lower file, a step after another",
            json!([[mkfile("/etc/n", "n")], [remove("/etc/keep")]]),
            None,
        ),
        (
            "an opaque directory remade with a file as it was",
            json!([[
                remove("/data"),
                mkdir("/data"),
                mkfile("/data/a", "a"),
                utimes("/data/a", 1_600_000_000, 500)
            ]]),
            None,
        ),
        (
            "an opaque directory remade with a file of other bytes, size and time as they were",
            json!([[
                remove("/data"),
                mkdir("/data"),
                mkfile("/data/a", "b"),
                utimes("/data/a", 1_600_000_000, 500)
            ]]),
            Some("content a layer leaves as the lower's"),
        ),
        (
            "a file made again as it was",
            json!([[
                remove("/etc/keep"),
                mkfile("/etc/keep", "keep"),
                utimes("/etc/keep", 1_600_000_000, 500)
            ]]),
            None,
        ),
        (
            "hard links made in one layer",
            json!([[mkfile("/h", "h"), link("/h", "/h2")], [mkfile("/z", "z")]]),
            None,
        ),
        (
            "a hard link to a file of a lower layer",
            json!([[mkfile("/f", "f"), link("/f", "/g")], [link("/f", "/k")]]),
            Some("a hard link only some of whose names a layer has"),
        ),
        (
            "one name of a base hard link changed",
            json!([[{"kind": "chmod", "path": "/hl1", "mode": 0o600}]]),
            Some("a hard link only some of whose names a layer has"),
        ),
        (
            "one name of a base hard link removed",
            json!([[remove("/hl2")]]),
            None,
        ),
        (
            "a directory whose mtime alone changes, which the differ leaves",
            json!([
                [mkfile("/var/log/tmp", "t"), remove("/var/log/tmp")],
                [mkfile("/x", "x")]
            ]),
            None,
        ),
        (
            "the root's mtime changed",
            json!([[mkfile("/top", "t"), remove("/top")], [mkdir("/newdir")]]),
            None,
        ),
        (
            "nanosecond mtimes on new files and base directories",
            json!([[
                mkfile("/ns", "n"),
                utimes("/ns", 1_700_000_000, 999),
                utimes("/etc", 1_700_000_001, 123)
            ]]),
            None,
        ),
        (
            "user xattrs on new and base files, and a base directory written as a parent",
            json!([[
                mkfile("/u", "u"),
                setxattr("/u", "user.new", "1"),
                setxattr("/etc/keep", "user.more", "2"),
                mkfile("/etc/added", "a")
            ]]),
            None,
        ),
        (
            "a base file's xattr changed, then the file written",
            json!([[setxattr("/etc/hosts", "user.b", "2")],
                   [{"kind": "chmod", "path": "/etc/hosts", "mode": 0o600}]]),
            None,
        ),
        (
            "times layers cannot hold",
            json!([[
                mkfile("/old", "o"),
                utimes("/old", -5, 7),
                mkfile("/far", "f"),
                utimes("/far", 9_300_000_000, 1)
            ]]),
            None,
        ),
        (
            "a capability on a new file",
            json!([[
                mkfile("/cap", "c"),
                setxattr("/cap", "security.capability", "\u{1}\u{0}\u{0}\u{2}")
            ]]),
            None,
        ),
        (
            "ownership past 21 bits",
            json!([[mkfile("/big", "b"),
                    {"kind": "lchown", "path": "/big", "uid": 3_000_000, "gid": 70_000,
                     "mode": 0, "timestamp": -1}]]),
            None,
        ),
        (
            "a new socket, which layers leave out",
            json!([[{"kind": "socket", "path": "/sock"}]]),
            None,
        ),
        (
            "a socket over a lower file",
            json!([[remove("/etc/keep"), {"kind": "socket", "path": "/etc/keep"}]]),
            Some("a socket over a lower file"),
        ),
        (
            "a name that reads as a whiteout",
            json!([[mkfile("/.wh.etc", "w")]]),
            Some("\".wh.etc\" reads as a whiteout"),
        ),
        (
            "a merge onto the base, then a step",
            json!([{"merge": [mkfile("/m", "m")]}, [mkfile("/after", "a")]]),
            None,
        ),
        (
            "a merge after a step",
            json!([[mkfile("/before", "b")], {"merge": [mkfile("/m", "m")]}]),
            Some("a merge onto a snapshot not in its layers' form"),
        ),
        (
            "a rename and a removed directory",
            json!([[{"kind": "rename", "old": "/data/a", "new": "/etc/a"}, remove("/var")]]),
            None,
        ),
    ]
}

#[test]
fn steps_stack_to_their_snapshot_or_give_up_saying_why() {
    let mut wrong = Vec::new();
    let (mut snapshot, mut layers) = (0, 0);
    for (name, steps, want) in steps() {
        let case = json!({"name": name, "lower": base(), "src": [], "steps": steps});
        let got = check(&case);
        match (&got, want) {
            (Path::Snapshot, None) => snapshot += 1,
            (Path::Layers(why), Some(w)) if why == w => layers += 1,
            _ => wrong.push(format!("{name}: {got:?}, not {want:?}")),
        }
    }
    eprintln!("steps: {snapshot} from the snapshot, {layers} from the layers");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A file the stack already keeps as the layers have it (made again as it was), whose
/// bytes a later step then rewrites in place in a way the differ cannot see (the same size, and the same mtime to
/// the nanosecond, so sameDirent does not compare content): the layers keep the old
/// bytes. The stack must not take the snapshot's new ones; it gives up on them. No file
/// operation rewrites a file in place today (fsutil's copy removes its target first), but
/// a RUN step's programs do.
#[test]
fn a_kept_file_whose_bytes_change_unseen_is_not_taken_from_the_snapshot() {
    let rewrite = |path: &str, data: &str| json!({"kind": "rewrite", "path": path, "data": data});
    let case = json!({
        "name": "kept, then rewritten unseen",
        "lower": base(),
        "src": [],
        "steps": [
            [mkfile("/w", "hello"), utimes("/w", 1_650_000_000, 111)],
            // Made again as it was: the differ judges it the same, so the layers keep
            // the first node, and the stack keeps what they hold for the new one.
            [remove("/w"), mkfile("/w", "hello"), utimes("/w", 1_650_000_000, 111)],
            [rewrite("/w", "world"), utimes("/w", 1_650_000_000, 111)],
        ],
    });
    assert_eq!(
        check(&case),
        Path::Layers("content a layer leaves as the lower's".into())
    );
}
