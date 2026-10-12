//! Held to git itself: a repository git makes, packed by git with deep delta chains of
//! both kinds, read back object for object as `git cat-file` reads it.

#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::Path;
use std::process::Command;

use shards_git::Oid;
use shards_git::object::{self, Kind};
use shards_git::pack::Pack;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[test]
fn packs_git_writes_are_read_object_for_object() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let dir = shards_testdir::TempDir::new("git").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    // Many versions of one file, so that git deltifies them against one another.
    let mut text = String::new();
    for i in 0..60 {
        text.push_str(&format!("line {i}: {}\n", "x".repeat(i % 7 * 10)));
        std::fs::write(dir.join("grow.txt"), &text).unwrap();
        std::fs::create_dir_all(dir.join(format!("d{}", i % 3))).unwrap();
        std::fs::write(dir.join(format!("d{}/f", i % 3)), format!("{i}\n").repeat(i + 1)).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&dir, &["tag", "-a", "v1", "-m", "annotated"]);
    let mut objects: Vec<Oid> = String::from_utf8(git(&dir, &["rev-list", "--objects", "--all"]))
        .unwrap()
        .lines()
        .map(|l| Oid::parse(l.split(' ').next().unwrap().as_bytes()).unwrap())
        .chain([Oid::parse(
            String::from_utf8(git(&dir, &["rev-parse", "v1"]))
                .unwrap()
                .trim()
                .as_bytes(),
        )
        .unwrap()])
        .collect();
    objects.sort();
    objects.dedup();
    let want = {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let list: String = objects.iter().map(|o| o.hex() + "\n").collect();
        child.stdin.take().unwrap().write_all(list.as_bytes()).unwrap();
        child.wait_with_output().unwrap().stdout
    };
    for (delta_base_offset, name) in [(true, "ofs"), (false, "ref")] {
        let list: String = objects.iter().map(|o| o.hex() + "\n").collect();
        let mut args = vec!["pack-objects", "--stdout", "--depth=50", "--window=50"];
        if delta_base_offset {
            args.push("--delta-base-offset");
        }
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(list.as_bytes()).unwrap();
        let bytes = child.wait_with_output().unwrap().stdout;
        let pack = Pack::read(bytes, 1 << 24).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(pack.len(), objects.len(), "{name}");
        let mut batch: &[u8] = &want;
        for oid in &objects {
            let (kind, data) = pack.get(oid).unwrap();
            let hex = oid.hex();
            // `<oid> <type> <size>\n<data>\n`, in the order asked.
            let line_end = batch.iter().position(|&b| b == b'\n').unwrap();
            let line = String::from_utf8(batch[..line_end].to_vec()).unwrap();
            let fields: Vec<&str> = line.split(' ').collect();
            let size: usize = fields[2].parse().unwrap();
            assert_eq!(fields[0], hex);
            assert_eq!(
                std::str::from_utf8(kind.name()).unwrap(),
                fields[1],
                "{name} {hex}"
            );
            assert_eq!(data, &batch[line_end + 1..line_end + 1 + size], "{name} {hex}");
            batch = &batch[line_end + 2 + size..];
            match kind {
                Kind::Commit => drop(object::commit(&data).unwrap()),
                Kind::Tree => drop(object::tree(&data).unwrap()),
                Kind::Tag => drop(object::tag(&data).unwrap()),
                Kind::Blob => {}
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs `git upload-pack --stateless-rpc`, as git's HTTP backend runs it for a `POST`,
/// with protocol version 2, on `body`.
fn upload_pack(dir: &Path, advertise: bool, body: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    cmd.arg("upload-pack").arg("--stateless-rpc");
    if advertise {
        cmd.arg("--advertise-refs");
    }
    let mut child = cmd
        .arg(dir)
        .env("GIT_PROTOCOL", "version=2")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

/// A shallow fetch of a branch from git's own upload-pack, its tree walked as git checks
/// it out: every path, mode, symlink and file's bytes as `git ls-tree` and `git show` say.
#[test]
fn a_shallow_fetch_from_git_checks_out_as_git_does() {
    use shards_git::checkout::{self, Item};
    use shards_git::protocol;
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let dir_guard = shards_testdir::TempDir::new("git-fetch").unwrap();
    let dir = dir_guard.join("git-fetch");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("old"), "first\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "one"]);
    std::fs::write(dir.join("a/b/deep.txt"), "deep\n").unwrap();
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(dir.join("empty"), "").unwrap();
    std::fs::write(dir.join("héllo"), "unicode\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["update-index", "--chmod=+x", "run.sh"]);
    git(
        &dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "120000,e69de29bb2d1d6434b8b29ae775ad8c2e48c5391,dangling",
        ],
    );
    git(&dir, &["commit", "-q", "-m", "two"]);
    git(&dir, &["tag", "-a", "v2", "-m", "annotated"]);

    let caps = protocol::advertisement(&upload_pack(&dir, true, b"")[..])
        .unwrap()
        .unwrap();
    let listed = protocol::refs(
        &upload_pack(
            &dir,
            false,
            &protocol::ls_refs("t", &[b"HEAD", b"refs/tags/"]).unwrap(),
        )[..],
    )
    .unwrap();
    let head = listed.iter().find(|r| r.name == b"HEAD").unwrap();
    assert_eq!(head.target.as_deref(), Some(&b"refs/heads/main"[..]));
    let tag = listed.iter().find(|r| r.name == b"refs/tags/v2").unwrap();
    assert_eq!(tag.peeled, Some(head.oid));

    let body = protocol::fetch("t", &[head.oid], 1, &caps).unwrap();
    let got = protocol::fetched(&upload_pack(&dir, false, &body)[..], 1 << 24).unwrap();
    assert_eq!(got.shallow, [head.oid]);
    let pack = Pack::read(got.pack, 1 << 24).unwrap();
    let (kind, data) = pack.get(&head.oid).unwrap();
    assert_eq!(kind, Kind::Commit);
    let commit = object::commit(&data).unwrap();
    let mut seen = Vec::new();
    checkout::walk(&pack, &commit.tree, &mut |path, item| {
        seen.push((String::from_utf8(path.to_vec()).unwrap(), item));
        Ok(())
    })
    .unwrap();
    // git's own listing of the same tree, recursive, with trees.
    let listing = String::from_utf8(git(
        &dir,
        &["-c", "core.quotepath=false", "ls-tree", "-r", "-t", "HEAD"],
    ))
    .unwrap();
    let want: Vec<(String, String)> = listing
        .lines()
        .map(|l| {
            let (meta, path) = l.split_once('\t').unwrap();
            (path.to_string(), meta.split(' ').next().unwrap().to_string())
        })
        .collect();
    assert_eq!(seen.len(), want.len(), "{seen:?}");
    for ((path, item), (want_path, mode)) in seen.iter().zip(&want) {
        assert_eq!(path, want_path);
        let shown = git(&dir, &["show", &format!("HEAD:{path}")]);
        match item {
            Item::Dir => assert_eq!(mode, "040000"),
            Item::File { executable, data, .. } => {
                assert_eq!(mode, if *executable { "100755" } else { "100644" }, "{path}");
                assert_eq!(data, &shown, "{path}");
            }
            Item::Symlink { target, .. } => {
                assert_eq!(mode, "120000");
                assert_eq!(target, &shown);
            }
            Item::Submodule { .. } => assert_eq!(mode, "160000"),
        }
    }
    // The first commit is past the shallow fetch's depth.
    assert!(commit.parents.iter().all(|p| !pack.has(p)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// git's upload-pack as a transport: each request one stateless run of it.
struct UploadPack<'a>(&'a Path);

impl shards_git::remote::Transport for UploadPack<'_> {
    fn advertise(&self) -> Result<Box<dyn std::io::Read + '_>, String> {
        Ok(Box::new(std::io::Cursor::new(upload_pack(self.0, true, b""))))
    }
    fn command(&self, body: &[u8]) -> Result<Box<dyn std::io::Read + '_>, String> {
        Ok(Box::new(std::io::Cursor::new(upload_pack(self.0, false, body))))
    }
}

/// Refs resolve as BuildKit resolves an ADD's (source.go resolveMetadata): the default
/// branch, a branch before a tag of its name, an annotated tag peeled, a missing ref none.
#[test]
fn refs_resolve_as_buildkit_resolves_them() {
    use shards_git::remote::{Limits, Remote, Resolved};
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let dir = shards_testdir::TempDir::new("git-refs").unwrap();
    git(&dir, &["init", "-q", "-b", "trunk"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "one"]);
    let one = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("f"), "2\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "two"]);
    let two = git(&dir, &["rev-parse", "HEAD"]);
    git(&dir, &["tag", "-a", "rel", "-m", "annotated"]);
    // A tag and a branch of one name, on different commits: the branch wins.
    let text = |b: &[u8]| String::from_utf8(b.trim_ascii().to_vec()).unwrap();
    git(&dir, &["tag", "same", &text(&one)]);
    git(&dir, &["branch", "same", &text(&two)]);
    let oid = |b: &[u8]| Oid::parse(b.trim_ascii()).unwrap();
    let remote = Remote::open(UploadPack(&dir), "shards-test").unwrap();
    assert_eq!(
        remote.resolve("").unwrap(),
        Some(Resolved {
            commit: oid(&two),
            tag: None,
            name: Some(b"refs/heads/trunk".to_vec())
        })
    );
    assert_eq!(remote.resolve("same").unwrap().unwrap().commit, oid(&two));
    assert_eq!(
        remote.resolve("refs/tags/same").unwrap().unwrap().commit,
        oid(&one)
    );
    let rel = remote.resolve("rel").unwrap().unwrap();
    assert_eq!(rel.commit, oid(&two));
    assert_eq!(rel.tag, Some(oid(&git(&dir, &["rev-parse", "rel"]))));
    assert_eq!(remote.resolve("nope").unwrap(), None);
    let pack = remote
        .fetch(
            &[oid(&one)],
            Limits {
                pack: 1 << 24,
                object: 1 << 24,
            },
        )
        .unwrap();
    assert!(pack.has(&oid(&one)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.git` written for a checkout (ADD --keep-git-dir) is one git reads as its own:
/// `verify-pack` takes the pack's index, `fsck` finds nothing amiss, `status` finds the
/// work tree clean, HEAD is the commit, detached, the history shallow, the branch kept.
#[test]
#[cfg(unix)]
fn a_kept_git_dir_is_one_git_reads_as_its_own() {
    use shards_git::checkout::{self, Item};
    use shards_git::protocol;
    use shards_git::repo::{self, KeptRef, Tracked};
    use std::os::unix::fs::PermissionsExt as _;
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let base_dir = shards_testdir::TempDir::new("git-keep").unwrap();
    let base = base_dir.join("git-keep");
    let _ = std::fs::remove_dir_all(&base);
    let (dir, out) = (base.join("origin"), base.join("checkout"));
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("old"), "first\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "one"]);
    std::fs::write(dir.join("a/b/deep.txt"), "deep\n").unwrap();
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(dir.join("empty"), "").unwrap();
    std::os::unix::fs::symlink("old", dir.join("link")).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["update-index", "--chmod=+x", "run.sh"]);
    git(&dir, &["commit", "-q", "-m", "two"]);

    let caps = protocol::advertisement(&upload_pack(&dir, true, b"")[..])
        .unwrap()
        .unwrap();
    let head = Oid::parse(git(&dir, &["rev-parse", "HEAD"]).trim_ascii()).unwrap();
    let body = protocol::fetch("t", &[head], 1, &caps).unwrap();
    let got = protocol::fetched(&upload_pack(&dir, false, &body)[..], 1 << 24).unwrap();
    let pack = Pack::read(got.pack, 1 << 24).unwrap();
    let commit = object::commit(&pack.get(&head).unwrap().1).unwrap();
    let time = u32::try_from(commit.committed).unwrap();
    let mut tracked = Vec::new();
    checkout::walk(&pack, &commit.tree, &mut |path, item| {
        let at = out.join(std::str::from_utf8(path).unwrap());
        let text = path.to_vec();
        match item {
            Item::Dir => std::fs::create_dir_all(&at).unwrap(),
            Item::File { executable, data, .. } => {
                std::fs::write(&at, &data).unwrap();
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
                let oid = Oid::of(Kind::Blob, &data).unwrap();
                let git_mode = if executable { 0o100755 } else { 0o100644 };
                tracked.push(Tracked {
                    path: text,
                    mode: git_mode,
                    oid,
                    size: data.len() as u32,
                    time,
                });
            }
            Item::Symlink { target, .. } => {
                std::os::unix::fs::symlink(std::str::from_utf8(&target).unwrap(), &at).unwrap();
                let oid = Oid::of(Kind::Blob, &target).unwrap();
                tracked.push(Tracked {
                    path: text,
                    mode: 0o120000,
                    oid,
                    size: target.len() as u32,
                    time,
                });
            }
            Item::Submodule { .. } => panic!("this repository has no submodules"),
        }
        Ok(())
    })
    .unwrap();
    let kept = KeptRef {
        name: b"refs/heads/main".to_vec(),
        oid: head,
    };
    let files = repo::git_dir(
        &pack,
        &head,
        true,
        "http://h/repo.git",
        Some(&kept),
        repo::index(&tracked).unwrap(),
        &[],
        None,
    )
    .unwrap();
    for (path, bytes, mode) in files {
        let at = out.join(".git").join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, bytes).unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    for d in ["refs/tags", "objects/info"] {
        std::fs::create_dir_all(out.join(".git").join(d)).unwrap();
    }
    let idx = std::fs::read_dir(out.join(".git/objects/pack"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "idx"))
        .unwrap();
    git(&out, &["verify-pack", idx.to_str().unwrap()]);
    git(&out, &["fsck", "--no-progress"]);
    assert_eq!(git(&out, &["status", "--porcelain"]), b"", "a clean work tree");
    assert_eq!(
        git(&out, &["rev-parse", "HEAD"]).trim_ascii(),
        head.hex().as_bytes()
    );
    assert_eq!(
        git(&out, &["rev-parse", "--is-shallow-repository"]).trim_ascii(),
        b"true"
    );
    assert_eq!(
        git(&out, &["for-each-ref", "--format=%(refname)"]).trim_ascii(),
        b"refs/heads/main"
    );
    assert_eq!(
        git(&out, &["remote", "get-url", "origin"]).trim_ascii(),
        b"http://h/repo.git"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// git's own transport, against `git daemon`: refs resolve and a commit fetches as over
/// smart HTTP.
#[test]
fn a_git_daemon_serves_as_smart_http_does() {
    use shards_git::daemon::Daemon;
    use shards_git::remote::{Limits, Remote};
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let base_dir = shards_testdir::TempDir::new("git-daemon").unwrap();
    let base = base_dir.join("git-daemon");
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("repo.git");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "one"]);
    let head = Oid::parse(git(&dir, &["rev-parse", "HEAD"]).trim_ascii()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // git-daemon itself, not `git daemon`, which runs it as a child that killing `git`
    // would leave behind.
    let exec_path = String::from_utf8(git(&dir, &["--exec-path"])).unwrap();
    let mut daemon = Command::new(Path::new(exec_path.trim()).join("git-daemon"))
        .args([
            "--export-all",
            "--reuseaddr",
            "--listen=127.0.0.1",
            &format!("--port={port}"),
        ])
        .arg(format!("--base-path={}", base.display()))
        .arg(&base)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("git://127.0.0.1:{port}/repo.git");
    let patience = std::time::Duration::from_secs(10);
    // The daemon listens a moment after it starts.
    let mut remote = None;
    for _ in 0..100 {
        if let Ok(r) = Remote::open(Daemon::of_url(&url, patience).unwrap(), "t") {
            remote = Some(r);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let remote = remote.expect("git daemon never answered");
    assert_eq!(remote.resolve("").unwrap().unwrap().commit, head);
    assert_eq!(remote.resolve("main").unwrap().unwrap().commit, head);
    let pack = remote
        .fetch(
            &[head],
            Limits {
                pack: 1 << 24,
                object: 1 << 24,
            },
        )
        .unwrap();
    assert!(pack.has(&head));
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&base);
}

/// A commit's subject as git's one-line format shows it (`%s`): a message of one line, of
/// a paragraph of several, after blank lines, with whitespace at the lines' ends, with
/// none at all.
#[test]
fn a_commits_subject_is_gits() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let dir = shards_testdir::TempDir::new("git-subject").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let tree = String::from_utf8(git(&dir, &["write-tree"])).unwrap();
    let messages: [&[u8]; 7] = [
        b"two\n",
        b"first line\nsecond line\n\nthe body\n",
        b"\n\n  \nafter blanks\n",
        b"trailing  \t\n next\t \n",
        b"no newline",
        b"",
        b"tab\tinside and \xc3\xa9\n",
    ];
    for message in messages {
        let file = dir.join("message");
        std::fs::write(&file, message).unwrap();
        let commit = String::from_utf8(git(
            &dir,
            &["commit-tree", tree.trim(), "-F", file.to_str().unwrap()],
        ))
        .unwrap();
        let commit = commit.trim();
        let want = git(&dir, &["log", "-1", "--format=%s", commit]);
        let data = git(&dir, &["cat-file", "commit", commit]);
        assert_eq!(
            object::subject(&data),
            want.strip_suffix(b"\n").unwrap_or(&want),
            "{:?}",
            String::from_utf8_lossy(message)
        );
    }
}

/// Each object's abbreviation as git shows it (`ls-tree --abbrev`, `rev-parse --short`,
/// `core.abbrev` unset): 7 digits in a small repository; in one of over 16,384 objects, 8,
/// and 9 for two blobs that share their first 8.
#[test]
fn an_objects_abbreviation_is_gits() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("SKIP: no git on this host");
        return;
    }
    let dir = shards_testdir::TempDir::new("git-abbrev").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a"), "a\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "small"]);
    git(&dir, &["repack", "-adq"]);
    let pack_of = |dir: &Path| {
        let packs = dir.join(".git/objects/pack");
        let name = std::fs::read_dir(&packs)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "pack"))
            .unwrap();
        Pack::read(std::fs::read(name).unwrap(), 1 << 30).unwrap()
    };
    let pack = pack_of(&dir);
    let head = Oid::parse(
        String::from_utf8(git(&dir, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .as_bytes(),
    )
    .unwrap();
    let short = git(&dir, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(head.abbrev(pack.names()), short.len() - 1);
    assert_eq!(short.len() - 1, 7);

    // Two blob contents whose names share their first 8 digits, found by trying.
    let mut seen = std::collections::HashMap::new();
    let (mut x, mut y) = (String::new(), String::new());
    for i in 0u64.. {
        let content = format!("collide {i}\n");
        let oid = Oid::of(object::Kind::Blob, content.as_bytes()).unwrap();
        // Its first 8 digits: its first 4 bytes.
        if let Some(other) = seen.insert(oid.0[..4].to_vec(), content.clone()) {
            (x, y) = (other, content);
            break;
        }
    }
    let big = shards_testdir::TempDir::new("git-abbrev-big").unwrap();
    git(&big, &["init", "-q", "-b", "main"]);
    let mut import =
        String::from("commit refs/heads/main\ncommitter t <t@t> 1600000000 +0000\ndata 3\nbig\n");
    let mut blob = |path: &str, content: &str| {
        import.push_str(&format!(
            "M 100644 inline {path}\ndata {}\n{content}\n",
            content.len()
        ));
    };
    blob("x", &x);
    blob("y", &y);
    for i in 0..20_000 {
        blob(&format!("f/{i}"), &format!("filler {i}\n"));
    }
    {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&big)
            .args(["fast-import", "--quiet"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", big.as_os_str())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(import.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }
    git(&big, &["repack", "-adq"]);
    git(&big, &["prune-packed"]);
    let pack = pack_of(&big);
    assert!(pack.len() > 1 << 14, "{}", pack.len());
    let listed = String::from_utf8(git(&big, &["ls-tree", "-r", "--abbrev", "main"])).unwrap();
    let full = String::from_utf8(git(&big, &["ls-tree", "-r", "main"])).unwrap();
    let mut lengths = std::collections::BTreeMap::new();
    for (short, long) in listed.lines().zip(full.lines()) {
        let abbreviated = short.split_whitespace().nth(2).unwrap();
        let oid = Oid::parse(long.split_whitespace().nth(2).unwrap().as_bytes()).unwrap();
        assert_eq!(oid.abbrev(pack.names()), abbreviated.len(), "{long}");
        *lengths.entry(abbreviated.len()).or_insert(0) += 1;
    }
    assert!(lengths.get(&8).is_some_and(|&n| n > 19_000), "{lengths:?}");
    assert!(
        lengths.keys().any(|&l| l >= 9),
        "the two that share 8 digits: {lengths:?}"
    );
}
