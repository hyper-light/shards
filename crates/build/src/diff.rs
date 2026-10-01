//! A step's layer, as BuildKit writes it from an overlayfs snapshot (dockerfile/1.27.1's
//! util/overlay/overlay_linux.go WriteUpperdir and Changes, continuity v0.5.0
//! fs/diff.go and fs/path.go for opaque directories, and containerd v2.3.6
//! pkg/archive/tar.go ChangeWriter): the paths the step touched, in walk order, less the
//! directories it left as they were; deletions as whiteouts; every parent of what is
//! written written before it; hard links kept; `security.capability` the one extended
//! attribute; times truncated to the second.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;

use shards_image::erofs::{DataRef, Kind, Node, NodeId, Source, Tree};
use shards_image::tar::writer::{self, Format, Header, Writer};

use crate::Error;
use crate::copy::{base, dir};
use crate::vfs::{self, CAPABILITY, Errno, Fs};

const WHITEOUT_PREFIX: &[u8] = b".wh.";
/// containerd's paxSchilyXattr.
const PAX_XATTR: &[u8] = b"SCHILY.xattr.";
/// copyBuffered's buffer, which data is read through.
const CHUNK: usize = 32 * 1024;

/// What changed at a path (continuity's ChangeKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Add,
    Modify,
    Delete,
    Unmodified,
}

/// Writes the layer of the step that made `upper` from `lower` to `out`.
pub fn write_layer(lower: &Fs, upper: &Fs, data: &mut dyn Source, out: &mut dyn Write) -> Result<(), Error> {
    let mut cw = ChangeWriter {
        tw: Writer::new(out),
        upper,
        links: upper.links(),
        inode_src: HashMap::new(),
        inode_refs: HashMap::new(),
        added_dirs: BTreeSet::new(),
        data,
    };
    let mut children: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for p in &upper.upper.touched {
        let (d, name) = parent_and_name(p);
        children.entry(d).or_default().insert(name);
    }
    let walk = Walk {
        lower,
        upper,
        children: &children,
    };
    walk.dir(b"/", &mut cw)?;
    cw.tw.finish().map_err(|e| Error(e.to_string()))?;
    Ok(())
}

fn parent_and_name(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let at = p.iter().rposition(|&c| c == b'/').unwrap_or(0);
    let parent = if at == 0 {
        b"/".to_vec()
    } else {
        p.get(..at).unwrap_or_default().to_vec()
    };
    (parent, p.get(at + 1..).unwrap_or_default().to_vec())
}

/// The node at a path, symlinks not followed anywhere: a walk of real directories.
fn at(fs: &Fs, path: &[u8]) -> Option<NodeId> {
    let mut id = Tree::ROOT;
    for name in path.split(|&c| c == b'/').filter(|n| !n.is_empty()) {
        id = fs.tree.child(id, name)?;
    }
    Some(id)
}

/// overlay.Changes over the touched paths.
struct Walk<'a> {
    lower: &'a Fs,
    upper: &'a Fs,
    children: &'a BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
}

impl Walk<'_> {
    fn dir(&self, path: &[u8], cw: &mut ChangeWriter<'_>) -> Result<(), Error> {
        let Some(names) = self.children.get(path) else {
            return Ok(());
        };
        for name in names {
            let p = vfs::join(path, name);
            let in_base = match self.lower.lstat(&p) {
                Ok(id) => Some(id),
                Err(e) if matches!(e.errno, Errno::NoEnt | Errno::NotDir) => None,
                Err(e) => {
                    return Err(Error(format!(
                        "failed to stat base file during overlay diff: {e}"
                    )));
                }
            };
            let Some(u) = at(self.upper, &p) else {
                // A whiteout, where the base has something to hide.
                if in_base.is_some() {
                    cw.handle(Change::Delete, &p, None)?;
                }
                continue;
            };
            let kind = match in_base {
                Some(l) => {
                    if !same(self.lower, l, self.upper, u, cw.data)? {
                        cw.handle(Change::Modify, &p, Some(u))?;
                    }
                    Change::Modify
                }
                None => {
                    cw.handle(Change::Add, &p, Some(u))?;
                    Change::Add
                }
            };
            if !self.upper.is_dir(u) {
                continue;
            }
            if kind == Change::Modify && self.upper.upper.recreated.contains(&p) {
                // An opaque directory: everything below it against the base's.
                if let Some(l) = in_base {
                    double_walk(self.lower, l, self.upper, u, &p, cw)?;
                }
                continue;
            }
            self.dir(&p, cw)?;
        }
        Ok(())
    }
}

/// continuity's doubleWalkDiff of the trees under `l` and `u`, rebased onto `root`.
fn double_walk(
    lower: &Fs,
    l: NodeId,
    upper: &Fs,
    u: NodeId,
    root: &[u8],
    cw: &mut ChangeWriter<'_>,
) -> Result<(), Error> {
    let entries = |fs: &Fs, id: NodeId| -> BTreeMap<Vec<u8>, NodeId> {
        fs.tree
            .entries(id)
            .into_iter()
            .map(|(n, c)| (n.to_vec(), c))
            .collect()
    };
    let (le, ue) = (entries(lower, l), entries(upper, u));
    let names: BTreeSet<&Vec<u8>> = le.keys().chain(ue.keys()).collect();
    for name in names {
        let p = vfs::join(root, name);
        match (le.get(name), ue.get(name)) {
            (Some(_), None) => cw.handle(Change::Delete, &p, None)?,
            (None, Some(&un)) => add_all(upper, un, &p, cw)?,
            (Some(&ln), Some(&un)) => {
                let same = same(lower, ln, upper, un, cw.data)?;
                if same {
                    if !upper.is_dir(un) && cw.links.get(un).copied().unwrap_or(0) > 1 {
                        cw.handle(Change::Unmodified, &p, Some(un))?;
                    }
                } else {
                    cw.handle(Change::Modify, &p, Some(un))?;
                }
                match (lower.is_dir(ln), upper.is_dir(un)) {
                    (true, true) => double_walk(lower, ln, upper, un, &p, cw)?,
                    (false, true) => add_children(upper, un, &p, cw)?,
                    _ => {}
                }
            }
            (None, None) => {}
        }
    }
    Ok(())
}

fn add_all(upper: &Fs, id: NodeId, p: &[u8], cw: &mut ChangeWriter<'_>) -> Result<(), Error> {
    cw.handle(Change::Add, p, Some(id))?;
    add_children(upper, id, p, cw)
}

fn add_children(upper: &Fs, id: NodeId, p: &[u8], cw: &mut ChangeWriter<'_>) -> Result<(), Error> {
    for (name, child) in upper.tree.entries(id) {
        add_all(upper, child, &vfs::join(p, name), cw)?;
    }
    Ok(())
}

/// `st_mode`, as Linux's stat reports it.
fn st_mode(n: &Node) -> u32 {
    vfs::type_bits(&n.kind) | u32::from(n.meta.mode)
}

fn rdev(n: &Node) -> (u32, u32) {
    match n.kind {
        Kind::CharDevice { major, minor } | Kind::BlockDevice { major, minor } => (major, minor),
        _ => (0, 0),
    }
}

/// `st_size`: a file's bytes, a symlink's target's, and 0 for the rest.
fn st_size(n: &Node) -> u64 {
    match &n.kind {
        Kind::File { size, .. } => *size,
        Kind::Symlink(t) => t.len() as u64,
        _ => 0,
    }
}

/// sameDirent (continuity's sameFile): two files from different file systems are the
/// same when type, mode, owner, device, capabilities and, for what is not a directory,
/// size and mtime agree, and content too when both mtimes may have been truncated.
fn same(lower: &Fs, l: NodeId, upper: &Fs, u: NodeId, data: &mut dyn Source) -> Result<bool, Error> {
    let (Some(f1), Some(f2)) = (lower.node(l), upper.node(u)) else {
        return Ok(false);
    };
    if st_mode(f1) != st_mode(f2)
        || f1.meta.uid != f2.meta.uid
        || f1.meta.gid != f2.meta.gid
        || rdev(f1) != rdev(f2)
    {
        return Ok(false);
    }
    if f1.meta.xattrs.get(CAPABILITY) != f2.meta.xattrs.get(CAPABILITY) {
        return Ok(false);
    }
    if lower.is_dir(l) {
        return Ok(true);
    }
    if st_size(f1) != st_size(f2) || f1.meta.mtime != f2.meta.mtime {
        return Ok(false);
    }
    if f1.meta.mtime_nsec == 0 && f2.meta.mtime_nsec == 0 {
        return match (&f1.kind, &f2.kind) {
            (Kind::Symlink(a), Kind::Symlink(b)) => Ok(a == b),
            _ if st_size(f1) == 0 => Ok(true),
            (Kind::File { size, data: a }, Kind::File { data: b, .. }) => same_content(*a, *b, *size, data),
            _ => Ok(true),
        };
    }
    Ok(f1.meta.mtime_nsec == f2.meta.mtime_nsec)
}

fn same_content(a: DataRef, b: DataRef, size: u64, data: &mut dyn Source) -> Result<bool, Error> {
    if a == b {
        return Ok(true);
    }
    let (mut x, mut y) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut at = 0u64;
    while at < size {
        let n = usize::try_from((size - at).min(CHUNK as u64)).unwrap_or(CHUNK);
        let (xs, ys) = (
            x.get_mut(..n).unwrap_or_default(),
            y.get_mut(..n).unwrap_or_default(),
        );
        data.read_at(a, at, xs).map_err(|e| Error(e.to_string()))?;
        data.read_at(b, at, ys).map_err(|e| Error(e.to_string()))?;
        if xs != ys {
            return Ok(false);
        }
        at += n as u64;
    }
    Ok(true)
}

/// containerd's ChangeWriter.
struct ChangeWriter<'a> {
    tw: Writer<&'a mut dyn Write>,
    upper: &'a Fs,
    links: Vec<u32>,
    inode_src: HashMap<NodeId, Vec<u8>>,
    inode_refs: HashMap<NodeId, Vec<Vec<u8>>>,
    added_dirs: BTreeSet<Vec<u8>>,
    data: &'a mut dyn Source,
}

impl ChangeWriter<'_> {
    fn header(&mut self, hdr: &Header) -> Result<(), Error> {
        self.tw
            .header(hdr)
            .map_err(|e| Error(format!("failed to write file header: {e}")))
    }

    fn handle(&mut self, kind: Change, p: &[u8], id: Option<NodeId>) -> Result<(), Error> {
        if kind == Change::Delete {
            let name = vfs::join(&dir(p), &[WHITEOUT_PREFIX, &base(p)].concat());
            let hdr = Header {
                typeflag: writer::REG,
                name: name.get(1..).unwrap_or_default().to_vec(),
                ..Header::default()
            };
            self.include_parents(&hdr)?;
            return self
                .tw
                .header(&hdr)
                .map_err(|e| Error(format!("failed to write whiteout header: {e}")));
        }
        let Some(id) = id else { return Ok(()) };
        let Some(node) = self.upper.node(id) else {
            return Ok(());
        };
        let mut hdr = Header {
            mode: i64::from(node.meta.mode),
            uid: i64::from(node.meta.uid),
            gid: i64::from(node.meta.gid),
            mtime: node.meta.mtime,
            format: Format::Pax,
            ..Header::default()
        };
        match &node.kind {
            Kind::Socket => return Ok(()),
            Kind::File { size, .. } => {
                hdr.typeflag = writer::REG;
                hdr.size = i64::try_from(*size).map_err(|_| Error("file too large".into()))?;
            }
            Kind::Dir(_) => hdr.typeflag = writer::DIR,
            Kind::Symlink(t) => {
                hdr.typeflag = writer::SYMLINK;
                hdr.linkname = t.clone();
            }
            Kind::CharDevice { major, minor } => {
                hdr.typeflag = writer::CHAR;
                hdr.devmajor = i64::from(*major);
                hdr.devminor = i64::from(*minor);
            }
            Kind::BlockDevice { major, minor } => {
                hdr.typeflag = writer::BLOCK_DEVICE;
                hdr.devmajor = i64::from(*major);
                hdr.devminor = i64::from(*minor);
            }
            Kind::Fifo => hdr.typeflag = writer::FIFO,
        }
        let mut name = p.strip_prefix(b"/").unwrap_or(p).to_vec();
        if self.upper.is_dir(id) && !name.ends_with(b"/") {
            name.push(b'/');
        }
        hdr.name = name;

        let mut additional: Vec<Vec<u8>> = Vec::new();
        let linked = !self.upper.is_dir(id) && self.links.get(id).copied().unwrap_or(0) > 1;
        if linked {
            if let Some(src) = self.inode_src.get(&id) {
                hdr.typeflag = writer::LINK;
                hdr.linkname = src.clone();
                hdr.size = 0;
            } else {
                if kind == Change::Unmodified {
                    self.inode_refs.entry(id).or_default().push(hdr.name.clone());
                    return Ok(());
                }
                self.inode_src.insert(id, hdr.name.clone());
                additional = self.inode_refs.remove(&id).unwrap_or_default();
            }
        } else if kind == Change::Unmodified {
            return Ok(());
        }
        if let Some(cap) = node.meta.xattrs.get(CAPABILITY).filter(|c| !c.is_empty()) {
            hdr.pax.insert([PAX_XATTR, CAPABILITY].concat(), cap.clone());
        }
        self.include_parents(&hdr)?;
        self.header(&hdr)?;
        if hdr.typeflag == writer::REG
            && hdr.size > 0
            && let Kind::File { size, data } = &node.kind
        {
            self.copy_data(*data, *size)?;
        }
        if !additional.is_empty() {
            let source = hdr.name.clone();
            for extra in additional {
                hdr.name = extra;
                hdr.typeflag = writer::LINK;
                hdr.linkname = source.clone();
                hdr.size = 0;
                self.include_parents(&hdr)?;
                self.header(&hdr)?;
            }
        }
        Ok(())
    }

    fn copy_data(&mut self, data: DataRef, size: u64) -> Result<(), Error> {
        let mut buf = vec![0u8; CHUNK];
        let mut at = 0u64;
        while at < size {
            let n = usize::try_from((size - at).min(CHUNK as u64)).unwrap_or(CHUNK);
            let chunk = buf.get_mut(..n).unwrap_or_default();
            self.data
                .read_at(data, at, chunk)
                .map_err(|e| Error(format!("failed to copy: {e}")))?;
            self.tw
                .write(chunk)
                .map_err(|e| Error(format!("failed to copy: {e}")))?;
            at += n as u64;
        }
        Ok(())
    }

    /// includeParents: a parent not yet written is written first, as a modification.
    fn include_parents(&mut self, hdr: &Header) -> Result<(), Error> {
        let mut name = hdr.name.as_slice();
        while name.ends_with(b"/") {
            name = name.get(..name.len() - 1).unwrap_or_default();
        }
        let parent = dir(name);
        if !name.is_empty() && name != b"." && parent != b"." && !self.added_dirs.contains(&parent) {
            self.added_dirs.insert(parent.clone());
            let abs = vfs::join(b"/", &parent);
            let id = self.upper.stat(&abs).map_err(|e| Error(e.to_string()))?;
            self.handle(Change::Modify, &abs, Some(id))?;
        }
        if hdr.typeflag == writer::DIR {
            self.added_dirs.insert(name.to_vec());
        }
        Ok(())
    }
}
