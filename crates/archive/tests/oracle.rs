//! shards-archive held to moby/go-archive v0.3.3 and Go's archive/tar: the archives they
//! made of testdata/cases.json's trees, the trees they left of its archives, what
//! `docker cp`'s copy made, and every header Go reads (scripts/archive/generate records
//! them in testdata/answers.json, pack/, unpack/ and rebase/).
//!
//! Trees with owners, devices or security attributes need root: the generator runs this
//! test as root in Linux, where SHARDS_ARCHIVE_ROOT=1 makes skipping them a failure.
//! Elsewhere the cases that need no root run; on Windows none do, as go-archive's own
//! archives differ there by design (modes, owners, links).

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::print_stdout
)]

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use shards_archive::copy::{
    copy_resource, preserve_trailing_dot_or_separator, rebase_archive_entries, split_path_dir_entry,
    tar_resource_rebase,
};
use shards_archive::tar::{Header, Reader, Time};
use shards_archive::{PackOptions, UnpackOptions, WhiteoutFormat, apply_layer, pack, unpack};

fn testdata() -> PathBuf {
    match std::env::var_os("SHARDS_ARCHIVE_TESTDATA") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata"),
    }
}

fn load(name: &str) -> Value {
    serde_json::from_slice(&fs::read(testdata().join(name)).unwrap()).unwrap()
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Whether a case runs here; with SHARDS_ARCHIVE_ROOT=1, every case must.
fn runs(case: &Value) -> bool {
    let must = std::env::var_os("SHARDS_ARCHIVE_ROOT").is_some();
    if must {
        assert!(is_root(), "SHARDS_ARCHIVE_ROOT is set, but this is not root");
    }
    if case["linux"].as_bool() == Some(true) && !cfg!(target_os = "linux") {
        return false;
    }
    if case["root"].as_bool() == Some(true) && !is_root() {
        println!("SKIP: {} needs root", case["name"]);
        return false;
    }
    true
}

/// A byte string as the answers hold one.
fn b(s: &[u8]) -> Value {
    match std::str::from_utf8(s) {
        Ok(s) => json!(s),
        Err(_) => json!({"hex": hex(s)}),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn pattern(size: usize, seed: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 31 + seed * 17) % 251) as u8).collect()
}

/// hash/fnv's New64a.
fn fnv(b: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &c in b {
        h ^= u64::from(c);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn check(r: libc::c_int, what: &str, p: &Path) {
    assert!(
        r == 0,
        "{what} {}: {}",
        p.display(),
        std::io::Error::last_os_error()
    );
}

#[cfg(target_os = "linux")]
fn mknod(p: &Path, kind: libc::mode_t, major: u32, minor: u32) {
    // SAFETY: the path is NUL-terminated.
    check(
        unsafe { libc::mknod(cstr(p).as_ptr(), kind | 0o600, libc::makedev(major, minor)) },
        "mknod",
        p,
    );
}

#[cfg(not(target_os = "linux"))]
fn mknod(p: &Path, _: libc::mode_t, _: u32, _: u32) {
    panic!("devices are made only as root on Linux: {}", p.display());
}

#[cfg(target_os = "linux")]
fn set_xattr(p: &Path, k: &str, v: &[u8]) {
    let k = CString::new(k).unwrap();
    // SAFETY: strings are NUL-terminated; v is valid for its length.
    check(
        unsafe { libc::lsetxattr(cstr(p).as_ptr(), k.as_ptr(), v.as_ptr().cast(), v.len(), 0) },
        "lsetxattr",
        p,
    );
}

#[cfg(target_os = "macos")]
fn set_xattr(p: &Path, k: &str, v: &[u8]) {
    let k = CString::new(k).unwrap();
    // SAFETY: strings are NUL-terminated; v is valid for its length.
    let r = unsafe {
        libc::setxattr(
            cstr(p).as_ptr(),
            k.as_ptr(),
            v.as_ptr().cast(),
            v.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    check(r, "setxattr", p);
}

/// The attributes an archive carries: user., security. and trusted. names.
#[cfg(target_os = "linux")]
fn xattrs(p: &Path) -> BTreeMap<String, String> {
    let c = cstr(p);
    let mut out = BTreeMap::new();
    let mut names = vec![0u8; 4096];
    // SAFETY: names is valid for its length.
    let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n <= 0 {
        return out;
    }
    for name in names[..n as usize].split(|&c| c == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8(name.to_vec()).unwrap();
        if !["user.", "security.", "trusted."]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let k = CString::new(name.clone()).unwrap();
        let mut v = vec![0u8; 4096];
        // SAFETY: v is valid for its length.
        let n = unsafe { libc::lgetxattr(c.as_ptr(), k.as_ptr(), v.as_mut_ptr().cast(), v.len()) };
        assert!(n >= 0);
        out.insert(name, hex(&v[..n as usize]));
    }
    out
}

#[cfg(target_os = "macos")]
fn xattrs(p: &Path) -> BTreeMap<String, String> {
    let c = cstr(p);
    let mut out = BTreeMap::new();
    let mut names = vec![0u8; 4096];
    // SAFETY: names is valid for its length.
    let n = unsafe {
        libc::listxattr(
            c.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        )
    };
    if n <= 0 {
        return out;
    }
    for name in names[..n as usize].split(|&c| c == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8(name.to_vec()).unwrap();
        if !["user.", "security.", "trusted."]
            .iter()
            .any(|p| name.starts_with(p))
        {
            continue;
        }
        let k = CString::new(name.clone()).unwrap();
        let mut v = vec![0u8; 4096];
        // SAFETY: v is valid for its length.
        let n = unsafe {
            libc::getxattr(
                c.as_ptr(),
                k.as_ptr(),
                v.as_mut_ptr().cast(),
                v.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        assert!(n >= 0);
        out.insert(name, hex(&v[..n as usize]));
    }
    out
}

#[cfg(target_os = "linux")]
fn major_minor(dev: u64) -> (u32, u32) {
    (libc::major(dev as libc::dev_t), libc::minor(dev as libc::dev_t))
}

#[cfg(not(target_os = "linux"))]
fn major_minor(dev: u64) -> (u32, u32) {
    (((dev >> 24) & 0xff) as u32, (dev & 0xff_ffff) as u32)
}

fn utimes(p: &Path, sec: i64, nsec: i64) {
    let ts = libc::timespec {
        tv_sec: sec as _,
        tv_nsec: nsec as _,
    };
    let times = [ts, ts];
    // SAFETY: times holds two timespecs; the path is NUL-terminated.
    let r = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            cstr(p).as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    check(r, "utimensat", p);
}

/// oracle/main.go's build: entries, then owners, modes and attributes, then times,
/// deepest first. On macOS a symlink's mode is set to Linux's 0777.
fn build(root: &Path, entries: &[Value]) {
    fs::create_dir_all(root).unwrap();
    for e in entries {
        let p = root.join(e["path"].as_str().unwrap());
        match e["type"].as_str().unwrap() {
            "dir" => {
                if e["path"] != "" {
                    fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
                }
            }
            "file" => {
                let data = match e["data"].as_str() {
                    Some(d) => d.as_bytes().to_vec(),
                    None => pattern(
                        e["size"].as_u64().unwrap() as usize,
                        e["seed"].as_u64().unwrap() as usize,
                    ),
                };
                fs::write(&p, data).unwrap();
            }
            "symlink" => {
                std::os::unix::fs::symlink(e["target"].as_str().unwrap(), &p).unwrap();
                #[cfg(target_os = "macos")]
                // SAFETY: the path is NUL-terminated.
                check(
                    unsafe {
                        libc::fchmodat(
                            libc::AT_FDCWD,
                            cstr(&p).as_ptr(),
                            0o777,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    },
                    "lchmod",
                    &p,
                );
            }
            "hardlink" => fs::hard_link(root.join(e["target"].as_str().unwrap()), &p).unwrap(),
            "fifo" => {
                // SAFETY: the path is NUL-terminated.
                check(unsafe { libc::mkfifo(cstr(&p).as_ptr(), 0o600) }, "mkfifo", &p);
            }
            "char" => mknod(
                &p,
                libc::S_IFCHR,
                e["major"].as_u64().unwrap() as u32,
                e["minor"].as_u64().unwrap() as u32,
            ),
            "block" => mknod(
                &p,
                libc::S_IFBLK,
                e["major"].as_u64().unwrap() as u32,
                e["minor"].as_u64().unwrap() as u32,
            ),
            t => panic!("type {t}"),
        }
    }
    for e in entries {
        if e["type"] == "hardlink" {
            continue;
        }
        let p = root.join(e["path"].as_str().unwrap());
        if e.get("uid").is_some() || e.get("gid").is_some() {
            let uid = e["uid"].as_u64().map(|u| u as u32);
            let gid = e["gid"].as_u64().map(|g| g as u32);
            std::os::unix::fs::lchown(&p, uid, gid).unwrap();
        }
        if e["type"] != "symlink"
            && let Some(mode) = e["mode"].as_u64()
        {
            fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(mode as u32)).unwrap();
        }
        if let Some(x) = e["xattrs"].as_object() {
            for (k, v) in x {
                set_xattr(&p, k, &hex_decode(v.as_str().unwrap()));
            }
        }
    }
    for e in entries.iter().rev() {
        if e["type"] == "hardlink" {
            continue;
        }
        let (sec, nsec) = match e["mtime"].as_array() {
            Some(t) => (t[0].as_i64().unwrap(), t[1].as_i64().unwrap()),
            None => (1234567890, 0),
        };
        utimes(&root.join(e["path"].as_str().unwrap()), sec, nsec);
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// oracle/main.go's manifest.
fn manifest(root: &Path, owners: bool) -> Value {
    let now = now();
    let mut out = Vec::new();
    let mut first: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
    let mut closed = BTreeMap::new();
    open_up(root, &mut closed);
    let mut ordered = Vec::new();
    walk(root, &mut ordered);
    for p in ordered {
        let rel = p.strip_prefix(root).unwrap().as_os_str().as_bytes().to_vec();
        let md = fs::symlink_metadata(&p).unwrap();
        let t = md.file_type();
        let kind = if t.is_dir() {
            "dir"
        } else if t.is_file() {
            "file"
        } else if t.is_symlink() {
            "symlink"
        } else if t.is_fifo() {
            "fifo"
        } else if t.is_char_device() {
            "char"
        } else if t.is_block_device() {
            "block"
        } else {
            "socket"
        };
        let mut m = serde_json::Map::new();
        m.insert("path".into(), b(&rel));
        m.insert("type".into(), json!(kind));
        if kind != "symlink" {
            let mode = closed.get(&p).copied().unwrap_or(md.mode());
            m.insert("mode".into(), json!(mode & 0o7777));
        }
        if owners {
            m.insert("uid".into(), json!(md.uid()));
            m.insert("gid".into(), json!(md.gid()));
        }
        match kind {
            "file" => {
                let data = read_any(&p, md.mode());
                m.insert("size".into(), json!(data.len()));
                m.insert("data".into(), json!(fnv(&data)));
            }
            "symlink" => {
                m.insert(
                    "target".into(),
                    b(fs::read_link(&p).unwrap().as_os_str().as_bytes()),
                );
            }
            "char" | "block" => {
                let (major, minor) = major_minor(md.rdev());
                m.insert("major".into(), json!(major));
                m.insert("minor".into(), json!(minor));
            }
            _ => {}
        }
        if (now - md.mtime()).abs() < 86400 {
            m.insert("mtime".into(), json!("now"));
        } else {
            m.insert("mtime".into(), json!([md.mtime(), md.mtime_nsec()]));
        }
        if kind != "dir" && md.nlink() > 1 {
            match first.get(&(md.dev(), md.ino())) {
                Some(f) => {
                    m.insert("link".into(), b(f));
                }
                None => {
                    first.insert((md.dev(), md.ino()), rel.clone());
                }
            }
        }
        let x = xattrs(&p);
        if !x.is_empty() {
            m.insert("xattrs".into(), json!(x));
        }
        out.push(Value::Object(m));
    }
    if out.is_empty() {
        return Value::Null;
    }
    Value::Array(out)
}

/// Directories only root could walk, opened to their owner, with the modes they had: an
/// archive may hold a directory of mode 0, which the root-run Go oracle walks as it is.
fn open_up(dir: &Path, closed: &mut BTreeMap<PathBuf, u32>) {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        let md = fs::symlink_metadata(&p).unwrap();
        if !md.is_dir() {
            continue;
        }
        if md.mode() & 0o700 != 0o700 {
            closed.insert(p.clone(), md.mode());
            fs::set_permissions(&p, fs::Permissions::from_mode((md.mode() & 0o7777) | 0o700)).unwrap();
        }
        open_up(&p, closed);
    }
}

/// A file's data, made readable for the read if only root could read it.
fn read_any(p: &Path, mode: u32) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    match fs::read(p) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            fs::set_permissions(p, fs::Permissions::from_mode((mode & 0o7777) | 0o400)).unwrap();
            let d = fs::read(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(mode & 0o7777)).unwrap();
            d
        }
        Err(e) => panic!("{}: {e}", p.display()),
    }
}

/// filepath.WalkDir's order: a directory, then its entries by name, each in full.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    for name in names {
        let p = dir.join(name);
        out.push(p.clone());
        if fs::symlink_metadata(&p).unwrap().is_dir() {
            walk(&p, out);
        }
    }
}

/// A temporary directory, its path free of symlinks (macOS's /var is one).
fn tmp(name: &str) -> PathBuf {
    let base = fs::canonicalize(std::env::temp_dir()).unwrap();
    // SAFETY: getpid has no preconditions.
    let dir = base.join(format!("shards-archive-{}-{name}", unsafe { libc::getpid() }));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn umask() {
    // SAFETY: umask has no preconditions.
    unsafe { libc::umask(0o022) };
}

fn trees() -> serde_json::Map<String, Value> {
    load("cases.json")["trees"].as_object().unwrap().clone()
}

/// The error a case expects, and the one it got, as text.
fn error_text(e: Option<shards_archive::Error>, path: &Path, as_: &str) -> Value {
    match e {
        None => Value::Null,
        Some(e) => json!(e.to_string().replace(path.to_str().unwrap(), as_)),
    }
}

fn report(failures: Vec<String>) {
    for f in &failures {
        println!("FAIL: {f}");
    }
    assert!(
        failures.is_empty(),
        "{} cases differ from go-archive",
        failures.len()
    );
}

/// The first byte where two archives differ, and the header it falls in.
fn diff(want: &[u8], got: &[u8]) -> String {
    let at = want
        .iter()
        .zip(got)
        .position(|(a, b)| a != b)
        .unwrap_or(want.len().min(got.len()));
    let block = at / 512 * 512;
    let show = |b: &[u8]| {
        String::from_utf8_lossy(&b[block.min(b.len())..(block + 512).min(b.len())]).replace('\0', ".")
    };
    format!(
        "{} bytes vs {}, first difference at {at}:\n  go:   {}\n  rust: {}",
        want.len(),
        got.len(),
        show(want),
        show(got)
    )
}

fn pack_opts(o: &Value) -> PackOptions {
    let strings = |v: &Value| -> Vec<Vec<u8>> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|s| s.as_str().unwrap().as_bytes().to_vec())
                    .collect()
            })
            .unwrap_or_default()
    };
    PackOptions {
        include_files: strings(&o["include"]),
        exclude_patterns: strings(&o["exclude"]),
        chown: o["chown"]
            .as_array()
            .map(|c| (c[0].as_i64().unwrap(), c[1].as_i64().unwrap())),
        include_source_dir: o["include_source_dir"].as_bool().unwrap_or(false),
        whiteout: whiteout(o),
        rebase_names: o["rebase"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.as_bytes().to_vec(), v.as_str().unwrap().as_bytes().to_vec()))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn whiteout(o: &Value) -> WhiteoutFormat {
    match o["whiteout"].as_str() {
        Some("overlay") => WhiteoutFormat::Overlay,
        _ => WhiteoutFormat::Aufs,
    }
}

/// Where scripts/archive/generate's Go side left the overlayfs upper directories it made
/// in the container; elsewhere there are none.
fn overlay_upper(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("SHARDS_ARCHIVE_OVERLAY");
    if dir.is_none() {
        assert!(
            std::env::var_os("SHARDS_ARCHIVE_ROOT").is_none(),
            "SHARDS_ARCHIVE_ROOT is set, but SHARDS_ARCHIVE_OVERLAY is not"
        );
    }
    Some(PathBuf::from(dir?).join(format!("overlay-{name}")).join("upper"))
}

#[test]
fn pack_as_go_archive() {
    umask();
    let cases = load("cases.json");
    let answers = load("answers.json");
    let trees = trees();
    let work = tmp("pack");
    let mut built = BTreeMap::new();
    let mut failures = Vec::new();
    for case in cases["pack"].as_array().unwrap() {
        if !runs(case) {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        if let Some(o) = case["overlay"].as_str() {
            let Some(upper) = overlay_upper(o) else {
                println!("SKIP: {name} needs the overlayfs upper directory the generator makes");
                continue;
            };
            built.insert(format!("overlay:{o}"), upper);
        }
        let tree = match case["overlay"].as_str() {
            Some(o) => format!("overlay:{o}"),
            None => case["tree"].as_str().unwrap().to_string(),
        };
        let tree = tree.as_str();
        let root = built
            .entry(tree.to_string())
            .or_insert_with(|| {
                let root = work.join(format!("tree-{tree}"));
                build(&root, trees[tree].as_array().unwrap());
                root
            })
            .clone();
        let src = match case["src"].as_str() {
            Some(s) if !s.is_empty() => PathBuf::from(format!("{}/{s}", root.display())),
            _ => root.clone(),
        };
        let result = if case["kind"] == "resource" {
            let rebase = case["rebase_name"].as_str().unwrap_or("");
            tar_resource_rebase(&src, rebase.as_bytes(), Vec::new())
        } else {
            pack(&src, &pack_opts(&case["opts"]), Vec::new())
        };
        let want_err = &answers["pack"][name]["error"];
        match result {
            Ok(got) => {
                if !want_err.is_null() {
                    failures.push(format!(
                        "pack {name}: Go failed with {want_err}, Rust made an archive"
                    ));
                    continue;
                }
                let want = fs::read(testdata().join("pack").join(format!("{name}.tar"))).unwrap();
                if want != got {
                    failures.push(format!("pack {name}: {}", diff(&want, &got)));
                }
            }
            Err(e) => {
                let got = error_text(Some(e), &root, "<root>");
                if &got != want_err {
                    failures.push(format!("pack {name}: error {got}, Go's {want_err}"));
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&work);
    report(failures);
}

/// A case's input archive, as oracle/main.go's makeTar found or made it.
fn input(case: &Value, name: &str) -> Vec<u8> {
    input_in(case, name, "unpack")
}

/// A case's input archive; one made by Go's tar.Writer is in `dir`.
fn input_in(case: &Value, name: &str, dir: &str) -> Vec<u8> {
    let i = &case["input"];
    if let Some(f) = i["go_tar"].as_str() {
        return fs::read(testdata().join("go-tar").join(f)).unwrap();
    }
    if let Some(p) = i["pack"].as_str() {
        return fs::read(testdata().join("pack").join(format!("{p}.tar"))).unwrap();
    }
    if let Some(u) = i["unpack"].as_str() {
        let cases = load("cases.json");
        let uc = cases["unpack"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == u)
            .unwrap()
            .clone();
        return input(&uc, u);
    }
    fs::read(testdata().join(dir).join(format!("{name}.tar"))).unwrap()
}

fn unpack_opts(o: &Value) -> UnpackOptions {
    UnpackOptions {
        no_lchown: o["no_lchown"].as_bool().unwrap_or(false),
        chown: o["chown"]
            .as_array()
            .map(|c| (c[0].as_i64().unwrap(), c[1].as_i64().unwrap())),
        no_overwrite_dir_non_dir: o["no_overwrite_dir_non_dir"].as_bool().unwrap_or(false),
        best_effort_xattrs: o["best_effort_xattrs"].as_bool().unwrap_or(false),
        exclude_patterns: o["exclude"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| s.as_str().unwrap().as_bytes().to_vec())
                    .collect()
            })
            .unwrap_or_default(),
        whiteout: whiteout(o),
    }
}

/// A case's destination, `top/dest`: its `pre` tree, or an empty directory.
fn destination(case: &Value, top: &Path, trees: &serde_json::Map<String, Value>) -> PathBuf {
    let dest = top.join("dest");
    match case["pre"].as_str() {
        Some(pre) => build(&dest, trees[pre].as_array().unwrap()),
        None => {
            fs::create_dir_all(&dest).unwrap();
            fs::set_permissions(&dest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
    }
    dest
}

#[test]
fn unpack_as_go_archive() {
    umask();
    let cases = load("cases.json");
    let answers = load("answers.json");
    let trees = trees();
    let work = tmp("unpack");
    let mut failures = Vec::new();
    for case in cases["unpack"].as_array().unwrap() {
        if !runs(case) {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let archive = input(case, name);
        let top = work.join(format!("unpack-{name}"));
        let dest = destination(case, &top, &trees);
        let opts = unpack_opts(&case["opts"]);
        let err = unpack(archive.as_slice(), &dest, &opts).err();
        let got = json!({
            "error": error_text(err, &dest, "<dest>"),
            "manifest": manifest(&top, case["owners"].as_bool().unwrap_or(false)),
        });
        let want = &answers["unpack"][name];
        if got["error"] != want["error"] {
            failures.push(format!(
                "unpack {name}: error {}, Go's {}",
                got["error"], want["error"]
            ));
        }
        if got["manifest"] != want["manifest"] {
            failures.push(format!(
                "unpack {name}: tree differs\n  go:   {}\n  rust: {}",
                want["manifest"], got["manifest"]
            ));
        }
    }
    let _ = fs::remove_dir_all(&work);
    report(failures);
}

#[test]
fn layer_as_go_archive() {
    umask();
    let cases = load("cases.json");
    let answers = load("answers.json");
    let trees = trees();
    let work = tmp("layer");
    let mut failures = Vec::new();
    for case in cases["layer"].as_array().unwrap() {
        if !runs(case) {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let archive = input_in(case, name, "layer");
        let top = work.join(format!("layer-{name}"));
        let dest = destination(case, &top, &trees);
        let (size, err) = match apply_layer(archive.as_slice(), &dest, &unpack_opts(&case["opts"])) {
            Ok(size) => (size, None),
            Err(e) => (0, Some(e)),
        };
        let got = json!({
            "error": error_text(err, &dest, "<dest>"),
            "size": size,
            "manifest": manifest(&top, case["owners"].as_bool().unwrap_or(false)),
        });
        let want = &answers["layer"][name];
        if &got != want {
            failures.push(format!("layer {name}:\n  go:   {want}\n  rust: {got}"));
        }
        // A saved overlayfs upper directory applied over its lower tree is what the
        // kernel showed through the mount.
        if let Some(o) = case["merged"].as_str() {
            let applied: Vec<Value> = manifest(&dest, false)
                .as_array()
                .unwrap()
                .iter()
                .map(|m| json!({"path": m["path"], "type": m["type"], "data": m["data"], "target": m["target"]}))
                .collect();
            let merged = &answers["overlays"][o]["merged"];
            if &Value::Array(applied.clone()) != merged {
                failures.push(format!(
                    "layer {name}: not overlayfs' merged view\n  kernel: {merged}\n  rust:   {}",
                    Value::Array(applied)
                ));
            }
        }
    }
    let _ = fs::remove_dir_all(&work);
    report(failures);
}

#[test]
fn copy_as_go_archive() {
    umask();
    let cases = load("cases.json");
    let answers = load("answers.json");
    let trees = trees();
    let work = tmp("copy");
    let mut failures = Vec::new();
    for case in cases["copy"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let root = work.join(format!("copy-{name}"));
        build(&root, trees["copy"].as_array().unwrap());
        let src = PathBuf::from(format!("{}/{}", root.display(), case["src"].as_str().unwrap()));
        let dst = PathBuf::from(format!("{}/{}", root.display(), case["dst"].as_str().unwrap()));
        let err = copy_resource(&src, &dst, case["follow"].as_bool().unwrap_or(false)).err();
        let got = json!({"error": error_text(err, &root, "<root>"), "manifest": manifest(&root, false)});
        let want = &answers["copy"][name];
        if &got != want {
            failures.push(format!("copy {name}:\n  go:   {want}\n  rust: {got}"));
        }
    }
    let _ = fs::remove_dir_all(&work);
    report(failures);
}

#[test]
fn rebase_as_go_archive() {
    let cases = load("cases.json");
    let answers = load("answers.json");
    let mut failures = Vec::new();
    for case in cases["rebase"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let archive = input(case, name);
        let old = case["old"].as_str().unwrap().as_bytes();
        let new = case["new"].as_str().unwrap().as_bytes();
        let want = fs::read(testdata().join("rebase").join(format!("{name}.tar"))).unwrap();
        match rebase_archive_entries(archive.as_slice(), Vec::new(), old, new) {
            Ok(got) if got == want => {}
            Ok(got) => failures.push(format!("rebase {name}: {}", diff(&want, &got))),
            Err(e) => failures.push(format!(
                "rebase {name}: {e}, Go's {}",
                answers["rebase"][name]["error"]
            )),
        }
    }
    report(failures);
}

fn time(t: Time) -> Value {
    if t.is_zero() {
        return Value::Null;
    }
    json!([t.sec, t.nsec])
}

/// oracle/main.go's dump: what the reader gives of an archive.
fn dump(archive: &[u8]) -> Value {
    let mut r = Reader::new(archive);
    let mut entries = Vec::new();
    loop {
        let h: Header = match r.next_header() {
            Ok(Some(h)) => h,
            Ok(None) => return json!({"entries": entries_or_null(entries)}),
            Err(e) => return json!({"entries": entries_or_null(entries), "error": e.to_string()}),
        };
        let mut data = Vec::new();
        let rerr = r.read_to_end(&mut data).err();
        let pax: Vec<Value> = h.pax.iter().map(|(k, v)| json!([b(k), b(v)])).collect();
        let mut e = json!({
            "typeflag": h.typeflag, "name": b(&h.name), "linkname": b(&h.linkname), "size": h.size,
            "mode": h.mode, "uid": h.uid, "gid": h.gid, "uname": b(&h.uname), "gname": b(&h.gname),
            "mtime": time(h.mtime), "atime": time(h.atime), "ctime": time(h.ctime),
            "devmajor": h.devmajor, "devminor": h.devminor,
            "pax": if pax.is_empty() { Value::Null } else { Value::Array(pax) },
            "format": h.format.0, "data": fnv(&data), "data_len": data.len(),
        });
        if let Some(err) = &rerr {
            let inner = err
                .get_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| err.to_string());
            e["error"] = json!(inner);
        }
        entries.push(e);
        if rerr.is_some() {
            return json!({"entries": entries_or_null(entries)});
        }
    }
}

fn entries_or_null(e: Vec<Value>) -> Value {
    if e.is_empty() {
        Value::Null
    } else {
        Value::Array(e)
    }
}

#[test]
fn read_as_go() {
    let answers = load("answers.json");
    let mut failures = Vec::new();
    for (name, want) in answers["read"].as_object().unwrap() {
        let archive = fs::read(testdata().join(name)).unwrap();
        let got = dump(&archive);
        if &got != want {
            failures.push(format!("read {name}:\n  go:   {want}\n  rust: {got}"));
        }
    }
    report(failures);
}

#[test]
fn paths_as_go() {
    let answers = load("answers.json");
    for case in answers["paths"].as_array().unwrap() {
        let p = Path::new(case["path"].as_str().unwrap());
        let (dir, base) = split_path_dir_entry(p);
        assert_eq!(
            json!([dir.to_str().unwrap(), base.to_str().unwrap()]),
            case["split"],
            "{p:?}"
        );
        let cleaned = PathBuf::from(clean(case["path"].as_str().unwrap()));
        assert_eq!(
            json!(preserve_trailing_dot_or_separator(&cleaned, p).to_str().unwrap()),
            case["preserve"],
            "{p:?}"
        );
    }
}

/// filepath.Clean, for the test's own inputs.
fn clean(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            _ => out.push(part),
        }
    }
    let body = out.join("/");
    match (rooted, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".into(),
        (false, false) => body,
    }
}
