//! A snapshot as a builder guest's layer (shards_abi::changes): what one step changed, or
//! the whole tree, written whole, nanoseconds and every xattr kept, so that the guest's
//! tree is the snapshot exactly; a layer's tar keeps less (crate::diff), and BuildKit's
//! next step sees the snapshot, not its layer.

use std::collections::HashMap;
use std::io;

use shards_abi::changes::{END, Entry, flag, kind};
use shards_image::erofs::{Kind, Node, NodeId, Source, Tree};

use crate::Error;
use crate::vfs::Fs;

/// How much is read of a file at once.
const CHUNK: usize = 1 << 20;

/// What to write: the step's changes alone, or everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// What the step that made the snapshot changed: every path it stamped, removals as
    /// whiteouts, a directory it made again opaque.
    Step,
    Whole,
}

/// Writes `fs` as a layer through `out`, file bytes read from `data`.
pub fn write(
    fs: &Fs,
    scope: Scope,
    data: &mut dyn Source,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), Error> {
    let tree = fs.tree();
    let io_err = |e: io::Error| Error(format!("sending a layer: {e}"));
    // The first path each node with several names was written at.
    let mut written: HashMap<NodeId, Vec<u8>> = HashMap::new();
    let mut header = Vec::with_capacity(256);
    let mut buf = vec![0u8; CHUNK];
    let mut changed: Vec<(&[u8], Option<NodeId>)> = Vec::new();
    let mut all: Vec<(&[u8], NodeId)> = Vec::new();
    // Directories to walk, and their paths in the layer.
    let mut todo: Vec<(NodeId, Vec<u8>)> = vec![(Tree::ROOT, Vec::new())];
    while let Some((dir, prefix)) = todo.pop() {
        let entries: Vec<(Vec<u8>, Option<NodeId>)> = match scope {
            Scope::Step => {
                tree.changed_into(dir, &mut changed);
                changed.iter().map(|(n, c)| (n.to_vec(), *c)).collect()
            }
            Scope::Whole => {
                tree.entries_into(dir, &mut all);
                all.iter().map(|(n, c)| (n.to_vec(), Some(*c))).collect()
            }
        };
        for (name, child) in &entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            let Some(child) = child else {
                // A removal: a whiteout, unless the name was made again, which hides what
                // was below it anyway.
                if !entries.iter().any(|(n, c)| n == name && c.is_some()) {
                    let e = Entry {
                        kind: kind::WHITEOUT,
                        path,
                        ..Entry::default()
                    };
                    header.clear();
                    e.encode_into(&mut header);
                    out(&header).map_err(io_err)?;
                }
                continue;
            };
            let Some(node) = fs.node(*child) else {
                return Err(Error(format!("a layer's node {child} is missing")));
            };
            let mut e = entry(node, path.clone());
            if let Kind::Dir(_) = node.kind {
                let mut canon = vec![b'/'];
                canon.extend_from_slice(&path);
                if fs.upper.recreated.contains(&canon) {
                    e.flags = flag::OPAQUE;
                }
                todo.push((*child, path));
            } else if let Some(first) = written.get(child) {
                e = Entry {
                    kind: kind::LINK,
                    path,
                    target: first.clone(),
                    ..Entry::default()
                };
            } else {
                written.insert(*child, path);
            }
            header.clear();
            e.encode_into(&mut header);
            out(&header).map_err(io_err)?;
            if let (kind::FILE, Kind::File { size, data: at }) = (e.kind, &node.kind) {
                let mut sent = 0u64;
                while sent < *size {
                    let n = usize::try_from(size - sent).map_or(CHUNK, |left| left.min(CHUNK));
                    let chunk = buf.get_mut(..n).unwrap_or_default();
                    data.read_at(*at, sent, chunk)
                        .map_err(|e| Error(format!("reading a file for a layer: {e}")))?;
                    out(chunk).map_err(io_err)?;
                    sent += n as u64;
                }
            }
        }
    }
    out(&[END]).map_err(io_err)
}

/// A node's entry, but for a hard link's later names.
fn entry(node: &Node, path: Vec<u8>) -> Entry {
    let m = &node.meta;
    let mut e = Entry {
        mode: u32::from(m.mode),
        uid: m.uid,
        gid: m.gid,
        mtime: m.mtime,
        mtime_nsec: m.mtime_nsec,
        path,
        xattrs: m.xattrs.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect(),
        ..Entry::default()
    };
    match &node.kind {
        Kind::Dir(_) => e.kind = kind::DIR,
        Kind::File { size, .. } => {
            e.kind = kind::FILE;
            e.size = *size;
        }
        Kind::Symlink(t) => {
            e.kind = kind::SYMLINK;
            e.target = t.to_vec();
        }
        Kind::CharDevice { major, minor } => {
            e.kind = kind::CHAR;
            (e.major, e.minor) = (*major, *minor);
        }
        Kind::BlockDevice { major, minor } => {
            e.kind = kind::BLOCK;
            (e.major, e.minor) = (*major, *minor);
        }
        Kind::Fifo => e.kind = kind::FIFO,
        Kind::Socket => e.kind = kind::SOCKET,
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Sources;
    use crate::upper::Applier;
    use shards_image::erofs::Meta;
    use std::fs::File;

    fn empty() -> Fs {
        Fs::new(
            Tree::new(Meta {
                mode: 0o755,
                ..Meta::default()
            }),
            (1_600_000_000, 7),
        )
    }

    /// The tree as a list of what each path holds, everything a layer can carry.
    fn listing(fs: &Fs, sources: &mut Sources) -> Vec<String> {
        let mut out = Vec::new();
        let mut todo = vec![(Tree::ROOT, String::new())];
        while let Some((dir, prefix)) = todo.pop() {
            for (name, id) in fs.tree().entries(dir) {
                let path = format!("{prefix}/{}", String::from_utf8_lossy(name));
                let n = fs.node(id).unwrap();
                let content = match &n.kind {
                    Kind::File { size, data } => {
                        let mut b = vec![0u8; *size as usize];
                        if *size > 0 {
                            sources.read_at(*data, 0, &mut b).unwrap();
                        }
                        String::from_utf8_lossy(&b).into_owned()
                    }
                    Kind::Symlink(t) => String::from_utf8_lossy(t).into_owned(),
                    Kind::Dir(_) => {
                        todo.push((id, path.clone()));
                        String::new()
                    }
                    other => format!("{other:?}"),
                };
                out.push(format!("{path} {:?} {content}", n.meta));
            }
        }
        out.sort();
        out
    }

    /// A host step's changes, sent as a layer and put back into the snapshot the step
    /// started from, make the step's snapshot again exactly: what a builder guest holds
    /// of it is what the host holds.
    #[test]
    fn a_steps_changes_rebuild_its_snapshot() {
        let mut sources = Sources::default();
        let hello = sources.bytes(b"hello".to_vec()).unwrap();
        let mut lower = empty();
        lower.mkdir(b"/d", 0o755).unwrap();
        lower.create(b"/d/old", 0o644).unwrap();
        lower.create(b"/keep", 0o644).unwrap();
        lower.mkdir(b"/re", 0o755).unwrap();
        lower.create(b"/re/x", 0o644).unwrap();
        lower.begin();
        let mut upper = lower.clone();
        upper.begin();
        upper.now = (1_700_000_000, 123);
        let f = upper.create(b"/d/new", 0o640).unwrap();
        upper.set_data(f, 5, hello);
        upper.link(b"/d/new", b"/d/again").unwrap();
        upper.unlink(b"/d/old").unwrap();
        upper.setxattr(b"/keep", b"user.k", b"v", false).unwrap();
        upper.remove_all(b"/re").unwrap();
        upper.mkdir(b"/re", 0o700).unwrap();
        upper.symlink(b"d/new", b"/s").unwrap();

        let mut stream = Vec::new();
        write(&upper, Scope::Step, &mut sources, &mut |b| {
            stream.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        let path = std::env::temp_dir().join(format!("shards-sync-{}", std::process::id()));
        let mut staging = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let source = sources.archive(staging.try_clone().unwrap()).unwrap();
        let mut again = lower.clone();
        again.begin();
        let mut applier = Applier::new(&mut again, &mut staging, source, 0);
        applier.feed(&stream).unwrap();
        applier.finish().unwrap();
        assert_eq!(listing(&again, &mut sources), listing(&upper, &mut sources));
        assert_eq!(again.lstat(b"/d/new").unwrap(), again.lstat(b"/d/again").unwrap());
        assert!(
            again.read_dir(b"/re").unwrap().is_empty(),
            "made again, what was there is gone"
        );

        // The whole tree, onto nothing, makes it too.
        let mut whole = Vec::new();
        write(&upper, Scope::Whole, &mut sources, &mut |b| {
            whole.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        let at = std::fs::metadata(&path).unwrap().len();
        let mut fresh = empty();
        fresh.begin();
        let mut applier = Applier::new(&mut fresh, &mut staging, source, at);
        applier.feed(&whole).unwrap();
        applier.finish().unwrap();
        assert_eq!(listing(&fresh, &mut sources), listing(&upper, &mut sources));
        let _ = std::fs::remove_file(&path);
    }
}
