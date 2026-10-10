//! An Agentfile's isolation checks (AGENTFILE_ARCH.md §9.2, D55) as shards' frontend has
//! BuildKit run them (D113): in an exec step whose root is the frontend's own image, so
//! that `shards` itself reads the trees whole, as `shards build` reads its own snapshots.
//! BuildKit's gateway cannot: its ReadDir walks one directory a call and its stat carries
//! no inode (cache/util/fsutil.go, fsutil's Stat), so no hard link is seen.
//!
//! `shards frontend check SPEC`, SPEC a JSON object:
//! - `{"domains": [...], "report": NAME}`: the image at `/target`, checked as `shards
//!   build` checks one before its export (build::domains);
//! - `{"domains": [...], "own": DIR, "report": NAME}`: a guarded step, its tree before at
//!   `/before` and after at `/after`, writing nothing in a domain but `own`'s (null where
//!   the step is no domain's directive), as `shards build`'s guard holds a step's layers.
//!
//! Each domain is `{"name", "harness", "dir"}`. The finding, or nothing, is written to
//! `/out/NAME`: a check that cannot read its trees says so there, so that the build fails
//! closed.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;
use std::process::ExitCode;

use sha2::Digest as _;
use shards_dockerfile::plan::DomainDir;
use shards_image::erofs::{DataRef, Dir, Kind, Meta, Node, NodeId, Tree};

/// The check's spec, as the frontend writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    pub domains: Vec<DomainDir>,
    /// A guarded step's own domain's directory, where the step is a directive's; the step
    /// is checked (`Some`), else the image is.
    pub guard: Option<Option<Vec<u8>>>,
    /// The file under `/out` the finding is written to.
    pub report: String,
}

impl Spec {
    pub(crate) fn json(&self) -> String {
        let s = |b: &[u8]| serde_json::Value::String(String::from_utf8_lossy(b).into_owned());
        let domains: Vec<serde_json::Value> = self
            .domains
            .iter()
            .map(|d| {
                serde_json::json!({
                    "name": s(&d.name),
                    "harness": d.harness,
                    "dir": s(&d.dir),
                })
            })
            .collect();
        let mut o = serde_json::json!({ "domains": domains, "report": self.report });
        if let (Some(guard), Some(map)) = (&self.guard, o.as_object_mut()) {
            map.insert(
                "own".into(),
                guard.as_ref().map_or(serde_json::Value::Null, |d| s(d)),
            );
        }
        o.to_string()
    }

    fn read(text: &str) -> Result<Spec, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("check spec: {e}"))?;
        let bytes = |v: &serde_json::Value| v.as_str().unwrap_or_default().as_bytes().to_vec();
        let domains = v
            .get("domains")
            .and_then(|d| d.as_array())
            .ok_or("check spec: no domains")?
            .iter()
            .map(|d| DomainDir {
                name: d.get("name").map(bytes).unwrap_or_default(),
                harness: d.get("harness").and_then(|h| h.as_bool()).unwrap_or(false),
                dir: d.get("dir").map(bytes).unwrap_or_default(),
            })
            .collect();
        let guard = v.get("own").map(|o| (!o.is_null()).then(|| bytes(o)));
        let report = v
            .get("report")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty() && !r.contains('/') && *r != "." && *r != "..")
            .ok_or("check spec: no report")?
            .to_string();
        Ok(Spec {
            domains,
            guard,
            report,
        })
    }
}

/// `shards frontend check SPEC`, inside BuildKit: the finding written to its report, a
/// spec that does not read failing the step.
pub(crate) fn run(spec: &str) -> ExitCode {
    let spec = match Spec::read(spec) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards frontend check: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = check(&spec, Path::new("/")).err().unwrap_or_default();
    match std::fs::write(Path::new("/out").join(&spec.report), text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "shards frontend check: /out/{}: {e}",
                spec.report
            );
            ExitCode::FAILURE
        }
    }
}

/// The finding of `spec` over the trees under `root`, if any.
pub(crate) fn check(spec: &Spec, root: &Path) -> Result<(), String> {
    match &spec.guard {
        None => {
            let tree = read_tree(&root.join("target"), &[])
                .map_err(|e| format!("the domain check could not read the image: {e}"))?;
            let fs = shards_build::vfs::Fs::new(tree.tree, (0, 0));
            crate::build::domains::check(&fs, &spec.domains)
        }
        Some(own) => {
            let roots = domain_roots(&spec.domains);
            let before = read_tree(&root.join("before"), &roots)
                .map_err(|e| format!("the domain guard could not read the step's input: {e}"))?;
            let after = read_tree(&root.join("after"), &roots)
                .map_err(|e| format!("the domain guard could not read the step's output: {e}"))?;
            guard(&before, &after, own.as_deref(), &spec.domains)
        }
    }
}

fn components(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect()
}

/// Each domain's directory and its `.d` grants beside it, as components, with its index.
fn domain_roots(domains: &[DomainDir]) -> Vec<(Vec<Vec<u8>>, usize)> {
    let mut roots = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let dir = components(&d.dir);
        let mut grants = dir.clone();
        if let Some(last) = grants.last_mut() {
            last.extend_from_slice(b".d");
        }
        roots.push((dir, i));
        roots.push((grants, i));
    }
    roots
}

fn domain_of(path: &[Vec<u8>], roots: &[(Vec<Vec<u8>>, usize)]) -> Option<usize> {
    roots
        .iter()
        .filter(|(r, _)| path.len() >= r.len() && path.get(..r.len()) == Some(r.as_slice()))
        .max_by_key(|(r, _)| r.len())
        .map(|&(_, d)| d)
}

/// What a guard compares of a path in a domain: its node's kind and metadata and, for a
/// file, its contents' digest.
type Seen = (Node, Option<[u8; 32]>);

/// A tree read from the host, and what a guard compares of each path in a domain.
struct Read {
    tree: Tree,
    seen: BTreeMap<Vec<Vec<u8>>, Seen>,
}

/// The tree under `dir` as the snapshot `shards build` checks: every entry, its kind,
/// mode, owner, times and extended attributes, hard links one node; and, for paths in a
/// domain of `roots`, what a guard compares.
fn read_tree(dir: &Path, roots: &[(Vec<Vec<u8>>, usize)]) -> io::Result<Read> {
    let root_meta = std::fs::symlink_metadata(dir)?;
    let mut out = Read {
        tree: Tree::new(meta_of(dir, &root_meta)),
        seen: BTreeMap::new(),
    };
    let mut links: BTreeMap<(u64, u64), NodeId> = BTreeMap::new();
    let mut digests: BTreeMap<NodeId, [u8; 32]> = BTreeMap::new();
    let mut stack: Vec<(std::path::PathBuf, NodeId, Vec<Vec<u8>>)> =
        vec![(dir.to_path_buf(), Tree::ROOT, Vec::new())];
    while let Some((at, id, path)) = stack.pop() {
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&at)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            let p = at.join(&name);
            let md = std::fs::symlink_metadata(&p)?;
            let mut components = path.clone();
            components.push(name_bytes(&name));
            let in_domain = domain_of(&components, roots).is_some();
            let key = inode(&md);
            if let Some(&target) = key.and_then(|k| links.get(&k)) {
                out.tree
                    .link(id, &name_bytes(&name), target)
                    .map_err(|e| io::Error::other(format!("{}: {e:?}", p.display())))?;
                if in_domain && let Some(n) = out.tree.node(target) {
                    out.seen
                        .insert(components, (n.clone(), digests.get(&target).copied()));
                }
                continue;
            }
            let meta = meta_of(&p, &md);
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                Kind::Dir(Dir::default())
            } else if ft.is_symlink() {
                Kind::Symlink(name_bytes(std::fs::read_link(&p)?.as_os_str()).into_boxed_slice())
            } else if ft.is_file() {
                Kind::File {
                    size: md.len(),
                    data: DataRef { source: 0, offset: 0 },
                }
            } else {
                special(&md)
            };
            let digest = if in_domain && ft.is_file() {
                let mut h = sha2::Sha256::new();
                let mut f = std::fs::File::open(&p)?;
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    let n = io::Read::read(&mut f, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(buf.get(..n).unwrap_or_default());
                }
                Some(h.finalize().into())
            } else {
                None
            };
            let node = Node { kind, meta };
            let child = out
                .tree
                .insert(id, &name_bytes(&name), node.clone())
                .map_err(|e| io::Error::other(format!("{}: {e:?}", p.display())))?;
            if let Some(d) = digest {
                digests.insert(child, d);
            }
            if in_domain {
                out.seen.insert(components.clone(), (node, digest));
            }
            if let Some(k) = key
                && !ft.is_dir()
            {
                links.insert(k, child);
            }
            if ft.is_dir() {
                stack.push((p, child, components));
            }
        }
    }
    Ok(out)
}

/// A file name's bytes.
#[cfg(unix)]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().as_bytes().to_vec()
}

/// A file's device and inode, which its hard links share.
#[cfg(unix)]
fn inode(md: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    (md.nlink() > 1).then(|| (md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn inode(_: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// A device, FIFO or socket's kind.
#[cfg(unix)]
fn special(md: &std::fs::Metadata) -> Kind {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let ft = md.file_type();
    let rdev = md.rdev();
    // Linux's encoding of a device number (the major in bits 8 to 19 and 32 up, the minor
    // in 0 to 7 and 20 to 31), as glibc's gnu_dev_major and gnu_dev_minor read it.
    let major = (((rdev >> 32) & 0xffff_f000) | ((rdev >> 8) & 0xfff)) as u32;
    let minor = (((rdev >> 12) & 0xffff_ff00) | (rdev & 0xff)) as u32;
    if ft.is_char_device() {
        Kind::CharDevice { major, minor }
    } else if ft.is_block_device() {
        Kind::BlockDevice { major, minor }
    } else if ft.is_fifo() {
        Kind::Fifo
    } else {
        Kind::Socket
    }
}

#[cfg(not(unix))]
fn special(_: &std::fs::Metadata) -> Kind {
    Kind::Socket
}

/// An entry's permission bits, owner, modification time and extended attributes.
#[cfg(unix)]
fn meta_of(path: &Path, md: &std::fs::Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt as _;
    Meta {
        mode: (md.mode() & 0o7777) as u16,
        uid: md.uid(),
        gid: md.gid(),
        mtime: md.mtime(),
        mtime_nsec: u32::try_from(md.mtime_nsec()).unwrap_or(0),
        xattrs: xattrs(path),
    }
}

#[cfg(not(unix))]
fn meta_of(_: &Path, _: &std::fs::Metadata) -> Meta {
    Meta::default()
}

/// A path's own extended attributes (not its symlink target's): Linux's llistxattr and
/// lgetxattr, where the check runs; none elsewhere.
#[cfg(target_os = "linux")]
fn xattrs(path: &Path) -> shards_image::erofs::Xattrs {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = shards_image::erofs::Xattrs::default();
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return out;
    };
    // SAFETY: a NUL-terminated path and buffers of the lengths given.
    let n = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
    let Ok(len) = usize::try_from(n) else { return out };
    if len == 0 {
        return out;
    }
    let mut names = vec![0u8; len];
    // SAFETY: as above, `names` holds `len` bytes.
    let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), len) };
    let Ok(got) = usize::try_from(n) else { return out };
    for name in names
        .get(..got)
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
    {
        let Ok(cname) = std::ffi::CString::new(name) else {
            continue;
        };
        // SAFETY: as above.
        let size = unsafe { libc::lgetxattr(c.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
        let Ok(size) = usize::try_from(size) else { continue };
        let mut value = vec![0u8; size];
        // SAFETY: `value` holds `size` bytes.
        let read = unsafe { libc::lgetxattr(c.as_ptr(), cname.as_ptr(), value.as_mut_ptr().cast(), size) };
        let Ok(read) = usize::try_from(read) else { continue };
        value.truncate(read);
        out.insert(name.to_vec(), value);
    }
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn xattrs(_: &Path) -> shards_image::erofs::Xattrs {
    shards_image::erofs::Xattrs::default()
}

/// A guarded step's writes: what differs between its trees before and after, in no domain
/// but `own`'s; a removal is a write. Named as `shards build`'s guard names it: the first
/// thing written that is no directory, where there is one, in the paths' order.
fn guard(before: &Read, after: &Read, own: Option<&[u8]>, domains: &[DomainDir]) -> Result<(), String> {
    let roots = domain_roots(domains);
    let own = own.and_then(|o| domain_of(&components(o), &roots));
    let mut paths: Vec<&Vec<Vec<u8>>> = before.seen.keys().chain(after.seen.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut found: Option<(Vec<Vec<u8>>, usize)> = None;
    for p in paths {
        let Some(d) = domain_of(p, &roots) else { continue };
        if Some(d) == own {
            continue;
        }
        let (b, a) = (before.seen.get(p), after.seen.get(p));
        let same = match (b, a) {
            (Some((nb, hb)), Some((na, ha))) => same_node(nb, na) && hb == ha,
            _ => false,
        };
        if same {
            continue;
        }
        let dir = a.or(b).is_some_and(|(n, _)| matches!(n.kind, Kind::Dir(_)));
        if found.is_none() || !dir {
            found = Some((p.clone(), d));
        }
        if !dir {
            break;
        }
    }
    let Some((path, d)) = found else { return Ok(()) };
    let dom = domains.get(d).map_or_else(String::new, |x| {
        format!(
            "{} {}",
            if x.harness { "the harness" } else { "the agent" },
            String::from_utf8_lossy(&x.name)
        )
    });
    Err(format!(
        "it writes /{} in {dom}'s domain, which only that domain's own directives write (AGENTFILE_ARCH.md §9.2)",
        String::from_utf8_lossy(&path.join(&b"/"[..]))
    ))
}

/// Whether two entries are one as a layer would carry them: kind and target, mode, owner,
/// time and extended attributes (a file's contents compared apart).
fn same_node(a: &Node, b: &Node) -> bool {
    let kind = match (&a.kind, &b.kind) {
        (Kind::Dir(_), Kind::Dir(_)) | (Kind::Fifo, Kind::Fifo) | (Kind::Socket, Kind::Socket) => true,
        (Kind::File { size: x, .. }, Kind::File { size: y, .. }) => x == y,
        (Kind::Symlink(x), Kind::Symlink(y)) => x == y,
        (Kind::CharDevice { major: a1, minor: a2 }, Kind::CharDevice { major: b1, minor: b2 })
        | (Kind::BlockDevice { major: a1, minor: a2 }, Kind::BlockDevice { major: b1, minor: b2 }) => {
            a1 == b1 && a2 == b2
        }
        _ => false,
    };
    kind && a.meta == b.meta
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn domains() -> Vec<DomainDir> {
        vec![
            DomainDir {
                name: b"a".to_vec(),
                harness: false,
                dir: b"/agents/a".to_vec(),
            },
            DomainDir {
                name: b"h".to_vec(),
                harness: true,
                dir: b"/harnesses/h".to_vec(),
            },
        ]
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("shards-d113-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The image check reads a host tree as `shards build`'s snapshot is read: a symlink
    /// out of a domain, one file's hard links in two domains, a FIFO and set-user-ID bits
    /// in one, each named.
    #[test]
    fn an_images_tree_read_from_the_host_is_checked_as_shards_builds() {
        let root = scratch("image");
        let t = root.join("target");
        std::fs::create_dir_all(t.join("agents/a")).unwrap();
        std::fs::create_dir_all(t.join("harnesses/h")).unwrap();
        std::fs::write(t.join("agents/a/ok"), b"fine").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", t.join("agents/a/out")).unwrap();
        std::fs::write(t.join("harnesses/h/shared"), b"x").unwrap();
        std::fs::hard_link(t.join("harnesses/h/shared"), t.join("agents/a/shared")).unwrap();
        let fifo = std::ffi::CString::new(t.join("agents/a/pipe").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        std::fs::write(t.join("agents/a/suid"), b"#!").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(t.join("agents/a/suid"), std::fs::Permissions::from_mode(0o4755)).unwrap();
        let spec = Spec {
            domains: domains(),
            guard: None,
            report: "image".into(),
        };
        let e = check(&spec, &root).unwrap_err();
        for said in [
            "/agents/a/out -> /etc/passwd: a symlink out of the agent a's domain, to the system",
            "/agents/a/pipe: a FIFO in the agent a's domain",
            "/agents/a/suid: set-user-ID or set-group-ID bits (4755) in the agent a's domain",
            "one file's hard links in the agent a and the harness h",
        ] {
            assert!(e.contains(said), "{said} not in {e}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A guarded step writing in another's domain is refused in the guard's words; in its
    /// own, or outside every domain, it is not; a removal is a write.
    #[test]
    fn a_guarded_steps_writes_are_held_to_its_own_domain() {
        let root = scratch("guard");
        for side in ["before", "after"] {
            std::fs::create_dir_all(root.join(side).join("agents/a")).unwrap();
            std::fs::create_dir_all(root.join(side).join("harnesses/h")).unwrap();
            std::fs::write(root.join(side).join("harnesses/h/keep"), b"k").unwrap();
            std::fs::write(root.join(side).join("harnesses/h/gone"), b"g").unwrap();
        }
        // Times as the copy left them may differ between the two trees: make them one.
        let same_times = |p: &Path| {
            let t = std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1));
            let f = std::fs::File::open(p).unwrap();
            f.set_times(t).unwrap();
        };
        let settle = |root: &Path| {
            for side in ["before", "after"] {
                for p in [
                    "harnesses/h/keep",
                    "harnesses/h/gone",
                    "harnesses/h",
                    "agents/a",
                    "agents",
                    "harnesses",
                    ".",
                ] {
                    let path = root.join(side).join(p);
                    if path.exists() {
                        same_times(&path);
                    }
                }
            }
        };
        settle(&root);
        let own_a = Spec {
            domains: domains(),
            guard: Some(Some(b"/agents/a".to_vec())),
            report: "0".into(),
        };
        assert_eq!(check(&own_a, &root), Ok(()));
        // A write in its own domain, and outside every domain: allowed.
        std::fs::write(root.join("after/agents/a/new"), b"n").unwrap();
        std::fs::write(root.join("after/etc-file"), b"n").unwrap();
        settle(&root);
        assert_eq!(check(&own_a, &root), Ok(()));
        // Contents changed in another's, size and time as they were: refused.
        std::fs::write(root.join("after/harnesses/h/keep"), b"z").unwrap();
        settle(&root);
        assert_eq!(
            check(&own_a, &root),
            Err("it writes /harnesses/h/keep in the harness h's domain, which only that domain's own directives write (AGENTFILE_ARCH.md §9.2)".into())
        );
        std::fs::write(root.join("after/harnesses/h/keep"), b"k").unwrap();
        settle(&root);
        assert_eq!(check(&own_a, &root), Ok(()));
        // A removal in another's: refused.
        std::fs::remove_file(root.join("after/harnesses/h/gone")).unwrap();
        settle(&root);
        assert_eq!(
            check(&own_a, &root),
            Err("it writes /harnesses/h/gone in the harness h's domain, which only that domain's own directives write (AGENTFILE_ARCH.md §9.2)".into())
        );
        // A step that is no directive's: any domain's write is refused.
        let none = Spec {
            domains: domains(),
            guard: Some(None),
            report: "1".into(),
        };
        // A spec reads back as written.
        assert_eq!(Spec::read(&none.json()), Ok(none.clone()));
        assert_eq!(Spec::read(&own_a.json()), Ok(own_a.clone()));
        assert!(
            check(&none, &root)
                .unwrap_err()
                .starts_with("it writes /agents/a/new in the agent a's domain")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
