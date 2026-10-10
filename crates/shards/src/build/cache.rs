//! The build cache (docs/design/architecture.md D50, D114): a step whose definition and
//! inputs a build has met before is not run again; its result is the layers it made then,
//! as BuildKit's solver reuses a vertex its cache key finds (`#N CACHED`).
//!
//! A step has two keys, as a vertex has in BuildKit's solver: the SHA-256 of shards'
//! version, its definition and each input's own key, in its inputs' order (not where they
//! sit in the plan, which another build lays out otherwise); and the same with what it reads
//! of an input in place of that input's key, where BuildKit takes a content checksum of
//! what an operation reads (solver/llbsolver/ops/file.go, exec.go `getMountDeps`): a copy's
//! sources, a read-only mount's. The second is asked only where the first finds nothing,
//! and it is the key the step's result is known by, so a change to what a step does not
//! read runs neither it nor what follows. An input that no chain of layers makes (the build
//! context, a download, a Git checkout) is keyed by its content. Content is digested as
//! BuildKit's content checksums take it (cache/contenthash): a file's tar header fields but
//! its time, and its bytes; a directory's header and each entry's name and digest. A record
//! keeps each of the step's outputs as its layers, held in the store from collection while
//! it is there, and the key its result is known by.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use sha2::{Digest as _, Sha256};
use shards_build::data::Sources;
use shards_build::vfs::Fs;
use shards_dockerfile::export::Layer;
use shards_dockerfile::glob::{MatchInfo, PatternMatcher};
use shards_dockerfile::llb::{Op, OpActionKind, OpChown, OpKind, OpMountKind, OpUser, Sharing};
use shards_image::erofs::{Kind, Node, NodeId, Source as _, Tree};

/// The version of what a key covers: a change to how steps are made changes it.
const FORMAT: &[u8] = b"shards build cache 2";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn framed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

/// What a step's key takes of one of its inputs.
#[derive(Debug, Clone, Copy)]
pub enum Dep<'a> {
    /// Its own key: what made it.
    Key(&'a str),
    /// The digest of what the step reads of it ([`Digests::reads`]).
    Read(&'a str),
}

/// Step `op`'s key, each input taken as `deps` says, in its inputs' order.
pub fn op_key(op: &Op, deps: &[Dep<'_>]) -> String {
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    framed(&mut h, env!("CARGO_PKG_VERSION").as_bytes());
    framed(&mut h, definition(op).as_bytes());
    framed(&mut h, format!("{:?}", op.platform).as_bytes());
    for (input, dep) in op.inputs.iter().zip(deps) {
        h.update(input.index.to_be_bytes());
        match dep {
            Dep::Key(k) => {
                h.update(b"k");
                framed(&mut h, k.as_bytes());
            }
            Dep::Read(d) => {
                h.update(b"r");
                framed(&mut h, d.as_bytes());
            }
        }
    }
    hex(&h.finalize())
}

/// A step's key where it reached the network through the build's proxy (D110): its own,
/// marked as BuildKit's solver marks its vertex (`\0buildkit.proxy-network.v0`), so a step
/// made with the network open is never taken for one made through the proxy, nor the
/// other way round. BuildKit's cache key leaves the mark out.
pub fn proxied_key(key: &str) -> String {
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    framed(&mut h, b"\0buildkit.proxy-network.v0");
    framed(&mut h, key.as_bytes());
    hex(&h.finalize())
}

/// A proxied step's record (D110): its outputs' layers, and what its requests came to, the
/// requests the policies let go first, which are asked again before the record is taken.
pub fn encode_proxied(
    key: &str,
    outputs: &[Vec<Layer>],
    capture: &super::proxy::capture::Capture,
) -> Result<String, String> {
    // The record any step has (its key and its outputs, D114), its requests beside them.
    let mut record: serde_json::Value =
        serde_json::from_str(&encode(key, outputs)?).map_err(|e| e.to_string())?;
    let pairs = |list: &[(String, String)]| -> serde_json::Value {
        list.iter().map(|(a, b)| serde_json::json!([a, b])).collect()
    };
    let materials: Vec<(String, String)> = capture
        .materials()
        .into_iter()
        .map(|m| (m.url, m.digest))
        .collect();
    let incomplete: serde_json::Value = capture
        .incomplete
        .iter()
        .map(|i| serde_json::json!([i.method, i.url, i.reason]))
        .collect();
    let fields = record
        .as_object_mut()
        .ok_or("a step's record that is no object")?;
    fields.insert(
        "proxy".into(),
        serde_json::json!({
            "allowed": pairs(&capture.allowed),
            "materials": pairs(&materials),
            "incomplete": incomplete,
        }),
    );
    serde_json::to_string(&record).map_err(|e| e.to_string())
}

/// What [`encode_proxied`] made: the outputs, and the capture, its materials whole.
pub fn decode_proxied(body: &[u8]) -> Result<(Vec<Vec<Layer>>, super::proxy::capture::Capture), String> {
    use super::proxy::capture::{Capture, Incomplete, Material, REASONS};
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let bad = || "a proxied step's record without its requests".to_string();
    let outputs = decode(v.get("outputs").ok_or_else(bad)?.to_string().as_bytes())?;
    let strings = |item: &serde_json::Value| -> Result<Vec<String>, String> {
        item.as_array()
            .ok_or_else(bad)?
            .iter()
            .map(|s| s.as_str().map(str::to_string).ok_or_else(bad))
            .collect()
    };
    let list = |key: &str| -> Result<Vec<Vec<String>>, String> {
        v.get("proxy")
            .and_then(|p| p.get(key))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(bad)?
            .iter()
            .map(strings)
            .collect()
    };
    let mut capture = Capture::default();
    for item in list("allowed")? {
        let [method, url] = <[String; 2]>::try_from(item).map_err(|_| bad())?;
        capture.allowed.push((method, url));
    }
    for item in list("materials")? {
        let [url, digest] = <[String; 2]>::try_from(item).map_err(|_| bad())?;
        capture.materials.push(Material { url, digest });
    }
    for item in list("incomplete")? {
        let [method, url, reason] = <[String; 3]>::try_from(item).map_err(|_| bad())?;
        let reason = REASONS.into_iter().find(|r| *r == reason).ok_or_else(bad)?;
        capture.incomplete.push(Incomplete { method, url, reason });
    }
    Ok((outputs, capture))
}

/// A step's definition as its keys cover it, as BuildKit's CacheMap digests one: an exec
/// without its proxy variables, its extra hosts' addresses, its mounts' selectors (which
/// what it reads covers), or the id and sharing of a cache mount other than a Dockerfile's
/// default one (exec.go, `checkShouldClearCacheOpts`); a copy with its source's last name
/// alone, the rest of it in what the copy reads (file.go).
fn definition(op: &Op) -> String {
    let mut kind = op.kind.clone();
    match &mut kind {
        OpKind::Exec { process, mounts, .. } => {
            process.proxy = None;
            for h in &mut process.extra_hosts {
                h.ip.clear();
            }
            for m in mounts.iter_mut() {
                m.selector.clear();
                let dest = m.dest.clone();
                if let OpMountKind::Cache { id, sharing } = &mut m.kind {
                    let default = *sharing == Sharing::Shared
                        && (*id == dest || id.splitn(2, |&b| b == b'/').nth(1) == Some(dest.as_slice()));
                    if !default {
                        id.clear();
                        *sharing = Sharing::Shared;
                    }
                }
            }
        }
        OpKind::File { actions } => {
            for a in actions.iter_mut() {
                let read = usize::try_from(a.secondary_input).is_ok_and(|i| i < op.inputs.len());
                if let OpActionKind::Copy { src, .. } = &mut a.action
                    && read
                {
                    *src = shards_build::copy::base(src);
                }
            }
        }
        _ => {}
    }
    format!("{kind:?}")
}

/// A source's key, from what it is (`identifier`) and what it holds (`content`).
pub fn source_key(identifier: &[u8], content: &str) -> String {
    let mut h = Sha256::new();
    framed(&mut h, FORMAT);
    framed(&mut h, identifier);
    framed(&mut h, content.as_bytes());
    hex(&h.finalize())
}

/// What a step reads of an input, as BuildKit's content checksum selects it
/// (opsutils.Selector): a path, which may hold a wildcard, its last symlink followed or
/// not, and the patterns what is under it is taken by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub path: Vec<u8>,
    pub wildcard: bool,
    pub follow: bool,
    pub include: Vec<Vec<u8>>,
    pub exclude: Vec<Vec<u8>>,
}

impl Selector {
    fn at(path: &[u8], follow: bool) -> Selector {
        Selector {
            path: path.to_vec(),
            wildcard: false,
            follow,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    fn filtered(&self) -> bool {
        self.wildcard || !self.include.is_empty() || !self.exclude.is_empty()
    }
}

/// `containsWildcards`, as file.go asks it of a copy's source.
fn has_wildcards(name: &[u8]) -> bool {
    let mut i = 0;
    while let Some(&c) = name.get(i) {
        if c == b'\\' {
            i += 1;
        } else if matches!(c, b'*' | b'?' | b'[') {
            return true;
        }
        i += 1;
    }
    false
}

/// `dedupePaths`: each path once, and none under another.
fn dedupe(paths: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = paths
        .iter()
        .filter(|p1| {
            !paths.iter().any(|p2| {
                let mut parent = p2.clone();
                if !parent.ends_with(b"/") {
                    parent.push(b'/');
                }
                p1 != &p2 && p1.starts_with(&parent)
            })
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What step `op` reads of each input, as BuildKit takes content checksums of it: a file
/// operation's copy sources, and the passwd and group files a name is looked up in, of
/// an input no action of it writes on (file.go `CacheMap`, `processOwner`,
/// `dedupeSelectors`); an exec's mounts of an input none of them may change, the root
/// mount aside (exec.go `getMountDeps`, `toSelectors`). `None` for an input it is keyed
/// by itself; an empty list for the whole of one.
pub fn reads(op: &Op) -> Vec<Option<Vec<Selector>>> {
    let n = op.inputs.len();
    let mut out: Vec<Option<Vec<Selector>>> = vec![None; n];
    let slot = |i: i64| usize::try_from(i).ok().filter(|&i| i < n);
    match &op.kind {
        OpKind::File { actions } => {
            let mut sels: Vec<Vec<Selector>> = vec![Vec::new(); n];
            let mut written = vec![false; n];
            let owner = |o: &Option<OpChown>, sels: &mut Vec<Vec<Selector>>| {
                let Some(o) = o else { return };
                for (u, file) in [(&o.user, &b"/etc/passwd"[..]), (&o.group, b"/etc/group")] {
                    if let Some(OpUser::Name { input, .. }) = u
                        && let Some(s) = slot(*input).and_then(|i| sels.get_mut(i))
                    {
                        s.push(Selector::at(file, true));
                    }
                }
            };
            for a in actions {
                if let Some(w) = slot(a.input).and_then(|i| written.get_mut(i)) {
                    *w = true;
                }
                match &a.action {
                    OpActionKind::Copy {
                        src,
                        owner: o,
                        follow_symlink,
                        allow_wildcard,
                        include_patterns,
                        exclude_patterns,
                        ..
                    } => {
                        owner(o, &mut sels);
                        if let Some(s) = slot(a.secondary_input).and_then(|i| sels.get_mut(i)) {
                            s.push(Selector {
                                path: src.clone(),
                                wildcard: *allow_wildcard && has_wildcards(src),
                                follow: *follow_symlink,
                                include: include_patterns.clone(),
                                exclude: exclude_patterns.clone(),
                            });
                        }
                    }
                    OpActionKind::Mkdir { owner: o, .. } | OpActionKind::Mkfile { owner: o, .. } => {
                        owner(o, &mut sels)
                    }
                    #[allow(unreachable_patterns)]
                    _ => {}
                }
            }
            for (i, s) in sels.into_iter().enumerate() {
                if s.is_empty() || written.get(i).copied().unwrap_or(true) {
                    continue;
                }
                // dedupeSelectors: plain paths once each, followed or not, filtered ones as
                // they are, all by path.
                let (plain, mut filtered): (Vec<Selector>, Vec<Selector>) =
                    s.into_iter().partition(|s| !s.filtered());
                let mut kept = Vec::new();
                for follow in [false, true] {
                    let paths: Vec<Vec<u8>> = plain
                        .iter()
                        .filter(|s| s.follow == follow)
                        .map(|s| s.path.clone())
                        .collect();
                    kept.extend(dedupe(&paths).into_iter().map(|p| Selector::at(&p, follow)));
                }
                kept.append(&mut filtered);
                kept.sort_by(|a, b| a.path.cmp(&b.path));
                if let Some(o) = out.get_mut(i) {
                    *o = Some(kept);
                }
            }
        }
        OpKind::Exec { mounts, .. } => {
            let mut paths: Vec<Vec<Vec<u8>>> = vec![Vec::new(); n];
            let mut content = vec![false; n];
            for m in mounts {
                if matches!(
                    m.kind,
                    OpMountKind::Tmpfs { .. } | OpMountKind::Secret { .. } | OpMountKind::Ssh { .. }
                ) {
                    continue;
                }
                let Some(i) = slot(m.input) else { continue };
                let sel = shards_dockerfile::go::join(&[b"/", &m.selector]);
                // A mount whose changes no step takes, one it cannot change, or one of all
                // of its input reads it; the root mount aside. The last mount of an input
                // says, as getMountDeps's does.
                let read = (m.output == -1 || m.readonly || sel == b"/") && m.dest != b"/";
                if let Some(p) = paths.get_mut(i) {
                    p.push(sel);
                }
                if let Some(c) = content.get_mut(i) {
                    *c = read;
                }
            }
            for (i, read) in content.into_iter().enumerate() {
                if !read {
                    continue;
                }
                let p = paths.get(i).map(|p| dedupe(p)).unwrap_or_default();
                // toSelectors: the whole input where any mount takes all of it.
                let sels = if p.iter().any(|p| p.is_empty() || p == b"/") {
                    Vec::new()
                } else {
                    p.iter().map(|p| Selector::at(p, true)).collect()
                };
                if let Some(o) = out.get_mut(i) {
                    *o = Some(sels);
                }
            }
        }
        _ => {}
    }
    out
}

/// A node's tar header fields as BuildKit's checksum keeps them (contenthash `NewFromStat`,
/// tarsum v1): permission and set-id bits (a symlink's always 0777), owner, size, type,
/// link target, device numbers, and its extended attributes but `security.*` other than
/// `security.capability`, and `system.*`; not its time.
fn header(h: &mut Sha256, node: &Node) {
    let (flag, size, link, dev): (u8, u64, &[u8], (u32, u32)) = match &node.kind {
        Kind::Dir(_) => (b'5', 0, b"", (0, 0)),
        Kind::File { size, .. } => (b'0', *size, b"", (0, 0)),
        Kind::Symlink(t) => (b'2', 0, t, (0, 0)),
        Kind::CharDevice { major, minor } => (b'3', 0, b"", (*major, *minor)),
        Kind::BlockDevice { major, minor } => (b'4', 0, b"", (*major, *minor)),
        Kind::Fifo => (b'6', 0, b"", (0, 0)),
        // A socket's type bit is cleared first: a regular file's header.
        Kind::Socket => (b'0', 0, b"", (0, 0)),
    };
    let mode = if flag == b'2' {
        0o777
    } else {
        u32::from(node.meta.mode) & 0o7777
    };
    h.update(mode.to_be_bytes());
    h.update(node.meta.uid.to_be_bytes());
    h.update(node.meta.gid.to_be_bytes());
    h.update(size.to_be_bytes());
    h.update([flag]);
    framed(h, link);
    h.update(dev.0.to_be_bytes());
    h.update(dev.1.to_be_bytes());
    for (k, v) in node.meta.xattrs.iter() {
        if k == b"security.capability" || !(k.starts_with(b"security.") || k.starts_with(b"system.")) {
            framed(h, k);
            framed(h, v);
        }
    }
}

/// A directory's header alone, as a filtered checksum takes one.
fn header_digest(node: &Node) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"h");
    header(&mut h, node);
    h.finalize().into()
}

/// What `pattern` says of `candidate`, given its directory's results: whether it takes it.
fn takes(
    pattern: Option<&mut PatternMatcher>,
    candidate: &[u8],
    parent: &MatchInfo,
) -> Result<(bool, MatchInfo), String> {
    match pattern {
        Some(p) => p
            .matches_using_parent_results(candidate, parent)
            .map_err(|e| String::from_utf8_lossy(&e).into_owned()),
        None => Ok((true, MatchInfo::default())),
    }
}

/// Each node's digest of the snapshots a build reads, as BuildKit's content checksums take
/// them: a file's header and bytes; a directory's header and each entry's name and digest,
/// in name order. Each is taken once a build.
#[derive(Debug, Default)]
pub struct Digests {
    known: Vec<(Rc<Fs>, Known)>,
}

/// The digests of a snapshot's nodes taken so far, by node.
type Known = HashMap<NodeId, [u8; 32]>;

/// A directory above the path a filtered checksum is at: its depth, path and header, the
/// patterns' results for it, and whether it is taken yet.
struct Above {
    depth: usize,
    path: Vec<u8>,
    header: [u8; 32],
    include: MatchInfo,
    exclude: MatchInfo,
    taken: bool,
}

impl Digests {
    fn slot(&mut self, fs: &Rc<Fs>) -> usize {
        match self.known.iter().position(|(f, _)| Rc::ptr_eq(f, fs)) {
            Some(i) => i,
            None => {
                self.known.push((fs.clone(), HashMap::new()));
                self.known.len() - 1
            }
        }
    }

    /// Node `id` of `fs`'s digest.
    pub fn node(&mut self, fs: &Rc<Fs>, id: NodeId, sources: &mut Sources) -> Result<[u8; 32], String> {
        let at = self.slot(fs);
        let memo = &mut self.known.get_mut(at).ok_or("a snapshot's digests are gone")?.1;
        let tree = fs.tree();
        let mut stack = vec![(id, false)];
        let mut buf = vec![0u8; 1 << 16];
        while let Some((n, ready)) = stack.pop() {
            if memo.contains_key(&n) {
                continue;
            }
            let node = tree.node(n).ok_or("a snapshot names a node it lacks")?;
            let mut h = Sha256::new();
            match &node.kind {
                // Its entries first, then it.
                Kind::Dir(_) if !ready => {
                    stack.push((n, true));
                    stack.extend(
                        tree.entries(n)
                            .into_iter()
                            .filter(|(_, c)| !memo.contains_key(c))
                            .map(|(_, c)| (c, false)),
                    );
                    continue;
                }
                Kind::Dir(_) => {
                    header(&mut h, node);
                    for (name, child) in tree.entries(n) {
                        framed(&mut h, name);
                        h.update(memo.get(&child).ok_or("an entry's digest was not taken")?);
                    }
                }
                Kind::File { size, data } => {
                    header(&mut h, node);
                    let mut at = 0u64;
                    while at < *size {
                        let n = usize::try_from((*size - at).min(buf.len() as u64)).unwrap_or(buf.len());
                        let chunk = buf.get_mut(..n).ok_or("a chunk past the buffer")?;
                        sources.read_at(*data, at, chunk).map_err(|e| e.to_string())?;
                        h.update(&*chunk);
                        at += n as u64;
                    }
                }
                _ => header(&mut h, node),
            }
            memo.insert(n, h.finalize().into());
        }
        memo.get(&id)
            .copied()
            .ok_or_else(|| "a node's digest was not taken".into())
    }

    /// The digest of all of `fs`: a source's content.
    pub fn root(&mut self, fs: &Rc<Fs>, sources: &mut Sources) -> Result<String, String> {
        self.node(fs, Tree::ROOT, sources).map(|d| hex(&d))
    }

    /// What `sel` reads of `fs`, as BuildKit's `Checksum` takes it: a path's digest; a
    /// wildcard's matches, as the copy finds them, each with its digest; under patterns, the
    /// headers of the directories and the files they take, each by its path, with the
    /// headers of the directories above them (contenthash `includedPaths`). None where a
    /// path it names is not there.
    pub fn read(
        &mut self,
        fs: &Rc<Fs>,
        sel: &Selector,
        sources: &mut Sources,
    ) -> Result<Option<[u8; 32]>, String> {
        let path = shards_dockerfile::go::join(&[b"/", &sel.path]);
        let find = |p: &[u8]| if sel.follow { fs.stat(p) } else { fs.lstat(p) };
        if !sel.filtered() {
            return match find(&path) {
                Ok(id) => self.node(fs, id, sources).map(Some),
                Err(_) => Ok(None),
            };
        }
        let bases = if sel.wildcard {
            match shards_build::copy::resolve_wildcards(fs, &path, sel.follow) {
                Ok(m) => m,
                Err(_) => return Ok(None),
            }
        } else {
            vec![path]
        };
        let matcher = |p: &[Vec<u8>]| (!p.is_empty()).then(|| PatternMatcher::new(p)).transpose();
        let (Ok(mut include), Ok(mut exclude)) = (matcher(&sel.include), matcher(&sel.exclude)) else {
            return Ok(None);
        };
        let mut h = Sha256::new();
        for base in bases {
            let Ok(id) = find(&base) else {
                return Ok(None);
            };
            framed(&mut h, &base);
            if !fs.is_dir(id) || (include.is_none() && exclude.is_none()) {
                h.update(self.node(fs, id, sources)?);
                continue;
            }
            let tree = fs.tree();
            let mut above: Vec<Above> = Vec::new();
            let mut todo: Vec<(NodeId, Vec<u8>, usize)> = vec![(id, Vec::new(), 0)];
            while let Some((n, rel, depth)) = todo.pop() {
                while above.last().is_some_and(|a| a.depth >= depth) {
                    above.pop();
                }
                let node = tree.node(n).ok_or("a snapshot names a node it lacks")?;
                let (inc_parent, exc_parent) = above
                    .last()
                    .map(|a| (a.include.clone(), a.exclude.clone()))
                    .unwrap_or_default();
                let (mut taken, inc) = takes(include.as_mut(), &rel, &inc_parent)?;
                let mut exc = MatchInfo::default();
                if taken {
                    let (excluded, info) = takes(exclude.as_mut(), &rel, &exc_parent)?;
                    exc = info;
                    taken = exclude.is_none() || !excluded;
                }
                let dir = matches!(node.kind, Kind::Dir(_));
                let digest = if dir {
                    header_digest(node)
                } else if taken {
                    self.node(fs, n, sources)?
                } else {
                    [0; 32]
                };
                if taken {
                    for a in above.iter_mut().filter(|a| !a.taken) {
                        a.taken = true;
                        framed(&mut h, &a.path);
                        h.update(a.header);
                    }
                    framed(&mut h, &rel);
                    h.update(digest);
                }
                if dir {
                    above.push(Above {
                        depth,
                        path: rel.clone(),
                        header: digest,
                        include: inc,
                        exclude: exc,
                        taken,
                    });
                    let mut entries = tree.entries(n);
                    entries.reverse();
                    for (name, child) in entries {
                        let mut r = rel.clone();
                        if !r.is_empty() {
                            r.push(b'/');
                        }
                        r.extend_from_slice(name);
                        todo.push((child, r, depth + 1));
                    }
                }
            }
        }
        Ok(Some(h.finalize().into()))
    }

    /// The digest of what a step reads of an input ([`reads`]): each selector and what it
    /// reads, all of it for none; none where a path one names is not there.
    pub fn reads(
        &mut self,
        fs: &Rc<Fs>,
        sels: &[Selector],
        sources: &mut Sources,
    ) -> Result<Option<String>, String> {
        let all = [Selector::at(b"/", false)];
        let sels = if sels.is_empty() { &all[..] } else { sels };
        let mut h = Sha256::new();
        for s in sels {
            framed(&mut h, &s.path);
            h.update([u8::from(s.wildcard), u8::from(s.follow)]);
            for p in s.include.iter().chain([&b"\0".to_vec()]).chain(&s.exclude) {
                framed(&mut h, p);
            }
            match self.read(fs, s, sources)? {
                Some(d) => h.update(d),
                None => return Ok(None),
            }
        }
        Ok(Some(hex(&h.finalize())))
    }
}

/// A record's body: the key its result is known by, and each output's layers, as the
/// build made them.
pub fn encode(key: &str, outputs: &[Vec<Layer>]) -> Result<String, String> {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let outputs: Vec<serde_json::Value> = outputs
        .iter()
        .map(|layers| {
            layers
                .iter()
                .map(|l| {
                    let annotations: BTreeMap<String, String> =
                        l.annotations.iter().map(|(k, v)| (text(k), text(v))).collect();
                    let created = l
                        .created
                        .as_ref()
                        .map(|t| t.rfc3339_nano())
                        .transpose()
                        .map_err(|e| text(&e))?;
                    Ok(serde_json::json!({
                        "mediaType": text(&l.media_type),
                        "digest": text(&l.digest),
                        "size": l.size,
                        "diffID": text(&l.diff_id),
                        "annotations": annotations,
                        "created": created,
                        "description": text(&l.description),
                    }))
                })
                .collect::<Result<Vec<_>, String>>()
                .map(serde_json::Value::Array)
        })
        .collect::<Result<_, String>>()?;
    serde_json::to_string(&serde_json::json!({"key": key, "outputs": outputs})).map_err(|e| e.to_string())
}

/// The key a record's result is known by, where its body says one.
pub fn known_as(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("key")?.as_str().map(str::to_string)
}

/// The layers of each output a record's body holds.
pub fn decode(body: &[u8]) -> Result<Vec<Vec<Layer>>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let outputs: Vec<Vec<serde_json::Value>> = match v.get("outputs") {
        Some(o) => serde_json::from_value(o.clone()),
        None => serde_json::from_value(v),
    }
    .map_err(|e| e.to_string())?;
    let field =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.as_bytes().to_vec());
    outputs
        .into_iter()
        .map(|layers| {
            layers
                .into_iter()
                .map(|v| {
                    let annotations = v
                        .get("annotations")
                        .and_then(|a| a.as_object())
                        .map(|o| {
                            o.iter()
                                .filter_map(|(k, x)| {
                                    Some((k.as_bytes().to_vec(), x.as_str()?.as_bytes().to_vec()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let created = match field(&v, "created") {
                        Some(t) => Some(
                            shards_dockerfile::go::parse_rfc3339(&t)
                                .map_err(|e| String::from_utf8_lossy(&e).into_owned())?,
                        ),
                        None => None,
                    };
                    Ok(Layer {
                        media_type: field(&v, "mediaType").ok_or("a cached layer without a media type")?,
                        digest: field(&v, "digest").ok_or("a cached layer without a digest")?,
                        size: v
                            .get("size")
                            .and_then(serde_json::Value::as_u64)
                            .ok_or("a cached layer without a size")?,
                        diff_id: field(&v, "diffID").ok_or("a cached layer without a diff ID")?,
                        annotations,
                        created,
                        description: field(&v, "description").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shards_build::vfs::Fs;
    use shards_dockerfile::llb::{Input, OpAction, OpMount, Process, ProxyEnv};
    use shards_image::erofs::Meta;

    fn source(id: &str) -> Op {
        Op {
            inputs: Vec::new(),
            kind: OpKind::Source {
                identifier: id.as_bytes().to_vec(),
                attrs: BTreeMap::new(),
            },
            platform: None,
        }
    }

    #[test]
    fn steps_are_keyed_by_what_they_are_and_read() {
        let a = source("docker-image://a");
        assert_eq!(op_key(&a, &[]), op_key(&a, &[]));
        assert_ne!(op_key(&a, &[]), op_key(&source("docker-image://b"), &[]));
        // Where an input sits in the plan does not count; what it is does.
        let merge = |at: usize| Op {
            inputs: vec![Input { op: at, index: 0 }],
            kind: OpKind::Merge,
            platform: None,
        };
        assert_eq!(
            op_key(&merge(1), &[Dep::Key("k")]),
            op_key(&merge(7), &[Dep::Key("k")])
        );
        assert_ne!(
            op_key(&merge(1), &[Dep::Key("k")]),
            op_key(&merge(1), &[Dep::Key("j")])
        );
        // A key and a digest of what is read are never the same input.
        assert_ne!(
            op_key(&merge(1), &[Dep::Key("k")]),
            op_key(&merge(1), &[Dep::Read("k")])
        );
        assert_ne!(
            source_key(b"local://context", "x"),
            source_key(b"local://context", "y")
        );
    }

    /// A step under the build's proxy is keyed apart from the same step without it (D110),
    /// and its record keeps what its requests came to: the requests the policies let go,
    /// its materials (a redirect's source by its target's digest) and what is none.
    #[test]
    fn proxied_steps_keep_their_requests() {
        use super::super::proxy::capture::{Capture, Incomplete, Material, Request};
        let key = op_key(&source("docker-image://a"), &[]);
        assert_ne!(proxied_key(&key), key);
        assert_eq!(proxied_key(&key), proxied_key(&key));
        let capture = Capture {
            requests: vec![Request {
                method: "GET".into(),
                url: "http://h/r".into(),
                redirect: "http://h/x".into(),
                status: 302,
            }],
            materials: vec![Material {
                url: "http://h/x".into(),
                digest: "sha256:aa".into(),
            }],
            incomplete: vec![Incomplete {
                method: "POST".into(),
                url: "http://h/p".into(),
                reason: "method_not_materializable",
            }],
            allowed: vec![
                ("GET".into(), "http://h/r".into()),
                ("POST".into(), "http://h/p".into()),
            ],
        };
        let outputs = vec![Vec::new()];
        let (back, held) =
            decode_proxied(encode_proxied("k", &outputs, &capture).unwrap().as_bytes()).unwrap();
        assert_eq!(back, outputs);
        assert_eq!(held.allowed, capture.allowed);
        assert_eq!(held.incomplete, capture.incomplete);
        let mut materials: Vec<String> = held
            .materials()
            .into_iter()
            .map(|m| format!("{} {}", m.url, m.digest))
            .collect();
        materials.sort();
        assert_eq!(materials, ["http://h/r sha256:aa", "http://h/x sha256:aa"]);
        // An unproxied record, or one with a reason no request has, is refused.
        assert!(decode_proxied(encode("k", &outputs).unwrap().as_bytes()).is_err());
        let odd = encode_proxied("k", &outputs, &capture)
            .unwrap()
            .replace("method_not_materializable", "other");
        assert!(decode_proxied(odd.as_bytes()).is_err());
    }

    fn exec(mounts: Vec<OpMount>, proxy: Option<ProxyEnv>, ip: &[u8]) -> Op {
        Op {
            inputs: vec![Input { op: 0, index: 0 }, Input { op: 1, index: 0 }],
            kind: OpKind::Exec {
                process: Box::new(Process {
                    args: vec![b"sh".to_vec()],
                    proxy,
                    extra_hosts: vec![shards_dockerfile::llb::HostIp {
                        host: b"h".to_vec(),
                        ip: ip.to_vec(),
                    }],
                    ..Process::default()
                }),
                mounts,
                network: Default::default(),
                security: Default::default(),
                secret_env: Vec::new(),
                devices: Vec::new(),
            },
            platform: None,
        }
    }

    fn mount(
        input: i64,
        dest: &str,
        selector: &str,
        readonly: bool,
        output: i64,
        kind: OpMountKind,
    ) -> OpMount {
        OpMount {
            input,
            selector: selector.as_bytes().to_vec(),
            dest: dest.as_bytes().to_vec(),
            output,
            readonly,
            kind,
        }
    }

    /// file.go's and exec.go's choices: which inputs a step reads, and how much of each.
    #[test]
    fn steps_read_what_buildkit_checksums() {
        let copy = |input: i64, secondary: i64, src: &str| OpAction {
            input,
            secondary_input: secondary,
            output: 0,
            action: OpActionKind::Copy {
                src: src.as_bytes().to_vec(),
                dest: b"/d".to_vec(),
                owner: None,
                mode: -1,
                mode_str: Vec::new(),
                follow_symlink: true,
                dir_copy_contents: true,
                attempt_unpack: false,
                create_dest_path: true,
                allow_wildcard: true,
                allow_empty_wildcard: true,
                timestamp: -1,
                include_patterns: Vec::new(),
                exclude_patterns: Vec::new(),
                required_paths: Vec::new(),
            },
        };
        let file = |actions: Vec<OpAction>| Op {
            inputs: vec![Input { op: 0, index: 0 }, Input { op: 1, index: 0 }],
            kind: OpKind::File { actions },
            platform: None,
        };
        // COPY a/b *.txt: the destination keyed by itself, the sources read, deduped and
        // in order, a wildcard as one.
        let r = reads(&file(vec![
            copy(0, 1, "/a/b"),
            copy(2, 1, "/*.txt"),
            copy(3, 1, "/a"),
        ]));
        assert_eq!(r.first(), Some(&None));
        let sels = r.get(1).cloned().flatten().unwrap();
        let paths: Vec<_> = sels
            .iter()
            .map(|s| (String::from_utf8_lossy(&s.path).into_owned(), s.wildcard))
            .collect();
        assert_eq!(paths, [("/*.txt".to_string(), true), ("/a".to_string(), false)]);
        // An input an action writes on is keyed by itself, though copied from too.
        let r = reads(&file(vec![copy(1, 0, "/x"), copy(0, 1, "/y")]));
        assert_eq!(r, vec![None, None]);

        let bind = OpMountKind::Bind;
        let root = mount(0, "/", "", false, 0, bind.clone());
        // A read-only mount of part of an input reads that part; the root is keyed by itself.
        let r = reads(&exec(
            vec![root.clone(), mount(1, "/src", "sub", true, -1, bind.clone())],
            None,
            b"",
        ));
        assert_eq!(r.first(), Some(&None));
        assert_eq!(
            r.get(1).cloned().flatten().unwrap(),
            vec![Selector::at(b"/sub", true)]
        );
        // A writable mount whose changes are an output is keyed by itself.
        let r = reads(&exec(
            vec![root.clone(), mount(1, "/out", "sub", false, 1, bind.clone())],
            None,
            b"",
        ));
        assert_eq!(r.get(1), Some(&None));
        // All of an input, read where a mount takes all of it.
        let r = reads(&exec(
            vec![
                root.clone(),
                mount(1, "/a", "sub", true, -1, bind.clone()),
                mount(1, "/b", "", true, -1, bind.clone()),
            ],
            None,
            b"",
        ));
        assert_eq!(r.get(1).cloned().flatten(), Some(Vec::new()));
    }

    /// What BuildKit's CacheMap leaves out of an exec's digest is no part of its key.
    #[test]
    fn keys_leave_out_what_buildkits_leave_out() {
        let deps = [Dep::Key("a"), Dep::Key("b")];
        let root = mount(0, "/", "", false, 0, OpMountKind::Bind);
        let cache = |id: &str, sharing| {
            mount(
                -1,
                "/c",
                "",
                false,
                -1,
                OpMountKind::Cache {
                    id: id.as_bytes().to_vec(),
                    sharing,
                },
            )
        };
        let key = |mounts: Vec<OpMount>, proxy, ip: &[u8]| op_key(&exec(mounts, proxy, ip), &deps);
        let plain = key(
            vec![root.clone(), cache("//c", Sharing::Shared)],
            None,
            b"1.2.3.4",
        );
        assert_eq!(
            plain,
            key(
                vec![root.clone(), cache("//c", Sharing::Shared)],
                Some(ProxyEnv::default()),
                b"5.6.7.8"
            )
        );
        // A cache's own id and sharing are not in it; a Dockerfile's default id is.
        let named = key(vec![root.clone(), cache("/one", Sharing::Locked)], None, b"");
        assert_eq!(
            named,
            key(vec![root.clone(), cache("/two", Sharing::Private)], None, b"")
        );
        assert_ne!(
            plain,
            key(vec![root.clone(), cache("//d", Sharing::Shared)], None, b"")
        );
        // A mount's selector is what it reads, not its definition.
        let sel = |s: &str| {
            key(
                vec![root.clone(), mount(1, "/m", s, true, -1, OpMountKind::Bind)],
                None,
                b"",
            )
        };
        assert_eq!(sel("x"), sel("y"));
    }

    /// A snapshot of `files`: each path, its bytes, mode and owner.
    fn snapshot(files: &[(&str, &[u8], u32, u32)], sources: &mut Sources) -> Rc<Fs> {
        let mut fs = Fs::new(
            Tree::new(Meta {
                mode: 0o755,
                ..Meta::default()
            }),
            (0, 0),
        );
        for (path, bytes, mode, uid) in files {
            let path = path.as_bytes();
            let parent = shards_build::copy::dir(path);
            if parent != b"/" && fs.stat(&parent).is_err() {
                fs.mkdir(&parent, 0o755).unwrap();
            }
            let id = fs.create(path, *mode).unwrap();
            let data = sources.bytes(bytes.to_vec()).unwrap();
            fs.set_data(id, bytes.len() as u64, data);
            fs.lchown(path, *uid, 0).unwrap();
        }
        Rc::new(fs)
    }

    /// What a selector reads changes with what it covers, as BuildKit's checksum does, and
    /// with nothing else: not a file beside it, nor a time.
    #[test]
    fn what_is_read_changes_with_what_it_covers() {
        let mut sources = Sources::default();
        let mut d = Digests::default();
        let base: &[(&str, &[u8], u32, u32)] = &[
            ("/app/a.txt", b"a", 0o644, 0),
            ("/app/b.txt", b"b", 0o644, 0),
            ("/app/c.md", b"c", 0o644, 0),
            ("/README", b"r", 0o644, 0),
        ];
        let one = snapshot(base, &mut sources);
        let mut read = |files: &[(&str, &[u8], u32, u32)], sel: &Selector| {
            let fs = snapshot(files, &mut sources);
            d.read(&fs, sel, &mut sources).unwrap()
        };
        let with = |path: &str, bytes: &'static [u8], mode: u32, uid: u32| {
            base.iter()
                .map(|f| if f.0 == path { (f.0, bytes, mode, uid) } else { *f })
                .collect::<Vec<_>>()
        };
        let app = Selector::at(b"/app", true);
        let at_first = read(base, &app);
        assert!(at_first.is_some());
        assert_eq!(read(&with("/README", b"changed", 0o644, 0), &app), at_first);
        assert_ne!(read(&with("/app/a.txt", b"changed", 0o644, 0), &app), at_first);
        assert_ne!(read(&with("/app/a.txt", b"a", 0o600, 0), &app), at_first);
        assert_ne!(read(&with("/app/a.txt", b"a", 0o644, 1000), &app), at_first);
        // A wildcard: what it matches and those alone.
        let txt = Selector {
            wildcard: true,
            ..Selector::at(b"/app/*.txt", true)
        };
        let matched = read(base, &txt);
        assert_eq!(read(&with("/app/c.md", b"changed", 0o644, 0), &txt), matched);
        assert_ne!(read(&with("/app/b.txt", b"changed", 0o644, 0), &txt), matched);
        // Patterns: what they leave out does not count.
        let no_md = Selector {
            exclude: vec![b"*.md".to_vec()],
            ..Selector::at(b"/app", true)
        };
        let kept = read(base, &no_md);
        assert_eq!(read(&with("/app/c.md", b"changed", 0o644, 0), &no_md), kept);
        assert_ne!(read(&with("/app/a.txt", b"changed", 0o644, 0), &no_md), kept);
        let only_md = Selector {
            include: vec![b"*.md".to_vec()],
            ..Selector::at(b"/app", true)
        };
        let md = read(base, &only_md);
        assert_eq!(read(&with("/app/a.txt", b"changed", 0o644, 0), &only_md), md);
        assert_ne!(read(&with("/app/c.md", b"changed", 0o644, 0), &only_md), md);
        // Nothing there: no digest, and the step finds out as it runs.
        assert_eq!(
            d.read(&one, &Selector::at(b"/nowhere", true), &mut sources)
                .unwrap(),
            None
        );
    }

    /// A file's digest takes the extended attributes BuildKit's checksum takes: not
    /// `security.*` but `security.capability`, nor `system.*`.
    #[test]
    fn a_files_digest_takes_the_attributes_buildkits_takes() {
        let mut sources = Sources::default();
        let mut d = Digests::default();
        let mut with = |attr: Option<(&[u8], &[u8])>| {
            let mut fs = Fs::new(
                Tree::new(Meta {
                    mode: 0o755,
                    ..Meta::default()
                }),
                (0, 0),
            );
            let id = fs.create(b"/f", 0o644).unwrap();
            let data = sources.bytes(b"x".to_vec()).unwrap();
            fs.set_data(id, 1, data);
            if let Some((k, v)) = attr {
                fs.setxattr(b"/f", k, v, false).unwrap();
            }
            d.read(&Rc::new(fs), &Selector::at(b"/f", true), &mut sources)
                .unwrap()
        };
        let plain = with(None);
        assert_eq!(with(Some((b"security.selinux", b"system_u:object_r:x"))), plain);
        assert_eq!(with(Some((b"system.posix_acl_access", b"\x02"))), plain);
        assert_ne!(with(Some((b"security.capability", b"\x01"))), plain);
        assert_ne!(with(Some((b"user.note", b"hi"))), plain);
    }

    #[test]
    fn records_keep_their_layers_whole() {
        let layer = Layer {
            media_type: b"application/vnd.oci.image.layer.v1.tar".to_vec(),
            digest: b"sha256:aa".to_vec(),
            size: 7,
            diff_id: b"sha256:bb".to_vec(),
            annotations: [(b"k".to_vec(), b"v".to_vec())].into_iter().collect(),
            created: Some(shards_dockerfile::go::Time::from_unix(1_600_000_000)),
            description: b"RUN x".to_vec(),
        };
        let outputs = vec![vec![layer.clone()], Vec::new()];
        let body = encode("k1", &outputs).unwrap();
        assert_eq!(decode(body.as_bytes()).unwrap(), outputs);
        assert_eq!(known_as(body.as_bytes()).as_deref(), Some("k1"));
    }
}
