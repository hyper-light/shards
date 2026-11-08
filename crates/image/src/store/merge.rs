//! A cache record's layers merged into one, as overlayfs stacks them (D134;
//! Documentation/filesystems/overlayfs.rst): each layer an EROFS image of a step's upper
//! directory, a whiteout a character device numbered 0:0 and an opaque directory one whose
//! `trusted.overlay.opaque` is `y`. A run of layers merged is the layer overlayfs would
//! show of them over whatever lies below them, which it keeps hidden where they hid it; at
//! the bottom nothing lies below, and what hid it goes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};

use crate::Error;
use crate::erofs::{self, DataRef, Kind, Meta, Node, NodeId, Source, Tree};

/// The xattr that makes a directory opaque, and its value.
const OPAQUE: &[u8] = b"trusted.overlay.opaque";
const OPAQUE_YES: &[u8] = b"y";

/// Layers' images, read as one source: each file's data in the image its index names.
struct Images<'f>(&'f [(File, u64)]);

impl Source for Images<'_> {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let file = usize::try_from(data.source)
            .ok()
            .and_then(|i| self.0.get(i))
            .map(|(file, _)| file)
            .ok_or_else(|| io::Error::other("a file of no layer"))?;
        let pos = data
            .offset
            .checked_add(at)
            .ok_or_else(|| io::Error::other("a read past a layer"))?;
        crate::layer::read_exact_at(file, buf, pos)
    }
}

/// What a merge came to: the layer's bytes written, and the bytes of regular files below it
/// it hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Merged {
    pub size: u64,
    pub hidden: u64,
}

/// Merges `layers`, a run of a record's layers, the lowest first, each with the bytes of
/// regular files below it that it hides, into one image written to `out`, its volume's
/// UUID `uuid`. `bottom` says nothing lies below the run: then nothing in the merged layer
/// hides anything, so its whiteouts go and its directories are opaque no more. What the
/// merged layer hides is what its lowest hid and what each layer above hid that the run
/// did not hold.
pub(super) fn merge(
    layers: &[(File, u64)],
    bottom: bool,
    uuid: [u8; 16],
    out: &mut dyn Write,
) -> Result<Merged, Error> {
    let mut merged: Option<Tree> = None;
    let mut hidden = 0u64;
    for (i, (file, hides)) in layers.iter().enumerate() {
        let source = u32::try_from(i).map_err(|_| Error("too many layers".into()))?;
        let layer = erofs::read_tree(&mut &*file, source)?;
        match &mut merged {
            None => {
                let mut layer = layer;
                if bottom {
                    uncover(&mut layer, Tree::ROOT)?;
                } else {
                    hidden = *hides;
                }
                merged = Some(layer);
            }
            Some(below) => {
                let mut removed = Vec::new();
                stack(below, &layer, bottom, &mut removed)?;
                if let (Some(root), Some(top)) = (below.node_mut(Tree::ROOT), layer.node(Tree::ROOT)) {
                    root.meta = top.meta.clone();
                }
                if !bottom {
                    hidden = hidden.saturating_add(hides.saturating_sub(dropped(below, &removed)));
                }
            }
        }
    }
    let tree = merged.ok_or_else(|| Error("no layers to merge".into()))?;
    let mut counted = Counted { out, n: 0 };
    erofs::write_uuid(&tree, &mut Images(layers), &mut counted, uuid)?;
    counted.flush()?;
    Ok(Merged {
        size: counted.n,
        hidden,
    })
}

/// A writer that counts what it wrote.
struct Counted<'o> {
    out: &'o mut dyn Write,
    n: u64,
}

impl Write for Counted<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.n = self.n.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

fn is_whiteout(node: &Node) -> bool {
    matches!(node.kind, Kind::CharDevice { major: 0, minor: 0 })
}

fn is_opaque(meta: &Meta) -> bool {
    meta.xattrs.get(OPAQUE).is_some_and(|v| v == OPAQUE_YES)
}

fn set_opaque(meta: &mut Meta, opaque: bool) {
    if opaque {
        meta.xattrs.insert(OPAQUE.to_vec(), OPAQUE_YES.to_vec());
    } else {
        meta.xattrs.remove(OPAQUE);
    }
}

/// The regular files at `id` and under it, into `out`.
fn files_under(tree: &Tree, id: NodeId, out: &mut Vec<NodeId>) {
    let mut todo = vec![id];
    while let Some(id) = todo.pop() {
        match tree.node(id).map(|n| &n.kind) {
            Some(Kind::File { .. }) => out.push(id),
            Some(Kind::Dir(_)) => todo.extend(tree.entries(id).into_iter().map(|(_, c)| c)),
            _ => {}
        }
    }
}

/// The bytes of the files in `removed` that `tree` no longer names, each a whole number of
/// blocks, as an image holds them: a file with another name still holds its bytes.
fn dropped(tree: &Tree, removed: &[NodeId]) -> u64 {
    if removed.is_empty() {
        return 0;
    }
    let mut named = Vec::new();
    files_under(tree, Tree::ROOT, &mut named);
    let named: std::collections::HashSet<NodeId> = named.into_iter().collect();
    let mut counted = std::collections::HashSet::new();
    removed
        .iter()
        .filter(|id| !named.contains(id) && counted.insert(**id))
        .filter_map(|&id| match tree.node(id).map(|n| &n.kind) {
            Some(Kind::File { size, .. }) => Some(size.next_multiple_of(erofs::BLOCK)),
            _ => None,
        })
        .fold(0, u64::saturating_add)
}

/// What a layer with nothing below it holds: its whiteouts gone, its directories opaque
/// no more.
fn uncover(tree: &mut Tree, dir: NodeId) -> Result<(), Error> {
    let mut todo = vec![dir];
    while let Some(dir) = todo.pop() {
        if let Some(node) = tree.node_mut(dir) {
            set_opaque(&mut node.meta, false);
        }
        let entries: Vec<(Vec<u8>, NodeId)> = tree
            .entries(dir)
            .into_iter()
            .map(|(name, id)| (name.to_vec(), id))
            .collect();
        for (name, id) in entries {
            match tree.node(id) {
                Some(n) if is_whiteout(n) => {
                    tree.remove(dir, &name);
                }
                Some(Node {
                    kind: Kind::Dir(_), ..
                }) => todo.push(id),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Stacks `upper` over `below`, as overlayfs shows an upper layer over a lower one: what
/// `upper` names replaces what `below` names there, but a directory over a directory, which
/// merge unless the upper one is opaque. A whiteout removes what it names, and stays to
/// hide what lies below `below` unless `bottom`; a directory that replaced something is
/// opaque, as it hid what lay below. `removed` gathers the regular files of `below` whose
/// names went; a node of `upper` of several names is one node in `below` too.
fn stack(below: &mut Tree, upper: &Tree, bottom: bool, removed: &mut Vec<NodeId>) -> Result<(), Error> {
    let mut placed: HashMap<NodeId, NodeId> = HashMap::new();
    let mut todo = vec![(Tree::ROOT, Tree::ROOT)];
    while let Some((dir, upper_dir)) = todo.pop() {
        for (name, up) in upper.entries(upper_dir) {
            let node = upper
                .node(up)
                .ok_or_else(|| Error("a layer's entry names no node".into()))?;
            let existing = below.child(dir, name);
            // What it merges with: a directory, under a directory not opaque. Anything
            // else there goes.
            let merges = existing.filter(|&e| {
                matches!(node.kind, Kind::Dir(_))
                    && !is_opaque(&node.meta)
                    && below.node(e).is_some_and(|n| matches!(n.kind, Kind::Dir(_)))
            });
            if let (Some(e), None) = (existing, merges) {
                files_under(below, e, removed);
                below.remove(dir, name);
            }
            if let Some(&node_id) = placed.get(&up) {
                below.link(dir, name, node_id)?;
                continue;
            }
            match &node.kind {
                _ if is_whiteout(node) => {
                    if !bottom {
                        below.insert(dir, name, node.clone())?;
                    }
                }
                Kind::Dir(_) => {
                    let mut meta = node.meta.clone();
                    if let Some(e) = merges {
                        // The upper one's metadata, and the lower one's opacity, as it hid
                        // what lies below it.
                        let was_opaque = below.node(e).is_some_and(|n| is_opaque(&n.meta));
                        set_opaque(&mut meta, was_opaque && !bottom);
                        if let Some(n) = below.node_mut(e) {
                            n.meta = meta;
                        }
                        todo.push((e, up));
                    } else {
                        set_opaque(
                            &mut meta,
                            !bottom && (is_opaque(&node.meta) || existing.is_some()),
                        );
                        let id = below.insert(
                            dir,
                            name,
                            Node {
                                kind: Kind::Dir(erofs::Dir::default()),
                                meta,
                            },
                        )?;
                        todo.push((id, up));
                    }
                }
                _ => {
                    let id = below.insert(dir, name, node.clone())?;
                    placed.insert(up, id);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn meta(mode: u16) -> Meta {
        Meta {
            mode,
            mtime: 1_700_000_000,
            ..Meta::default()
        }
    }

    fn dir(tree: &mut Tree, at: NodeId, name: &str, opaque: bool) -> NodeId {
        let mut m = meta(0o755);
        set_opaque(&mut m, opaque);
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind: Kind::Dir(erofs::Dir::default()),
                meta: m,
            },
        )
        .unwrap()
    }

    /// A file whose bytes are `fill` repeated `len` times, from `bytes`, which collects them.
    fn file(tree: &mut Tree, at: NodeId, name: &str, bytes: &mut Vec<u8>, fill: u8, len: usize) -> NodeId {
        let offset = bytes.len() as u64;
        bytes.extend(std::iter::repeat_n(fill, len));
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind: Kind::File {
                    size: len as u64,
                    data: DataRef { source: 0, offset },
                },
                meta: meta(0o644),
            },
        )
        .unwrap()
    }

    fn whiteout(tree: &mut Tree, at: NodeId, name: &str) {
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind: Kind::CharDevice { major: 0, minor: 0 },
                meta: meta(0),
            },
        )
        .unwrap();
    }

    struct Bytes(Vec<u8>);

    impl Source for Bytes {
        fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
            let from = (data.offset + at) as usize;
            buf.copy_from_slice(&self.0[from..from + buf.len()]);
            Ok(())
        }
    }

    /// `tree` as an image in a file of `dir`'s, its files' bytes from `bytes`.
    fn layer(dir: &std::path::Path, name: &str, tree: &Tree, bytes: Vec<u8>) -> File {
        let path = dir.join(name);
        let mut out = File::create(&path).unwrap();
        erofs::write(tree, &mut Bytes(bytes), &mut out).unwrap();
        File::open(&path).unwrap()
    }

    /// What a merged image holds: each path, what it is, and a file's bytes.
    fn listing(image: &[u8]) -> Vec<(String, String)> {
        let tree = erofs::read_tree(&mut io::Cursor::new(image), 0).unwrap();
        let mut out = Vec::new();
        let mut todo = vec![(Tree::ROOT, String::new())];
        while let Some((d, path)) = todo.pop() {
            for (name, id) in tree.entries(d) {
                let p = format!("{path}/{}", String::from_utf8_lossy(name));
                let n = tree.node(id).unwrap();
                let what = match &n.kind {
                    Kind::Dir(_) => {
                        todo.push((id, p.clone()));
                        if is_opaque(&n.meta) {
                            "dir opaque".into()
                        } else {
                            "dir".into()
                        }
                    }
                    Kind::File { size, data } => {
                        let offset = data.offset as usize;
                        let bytes = &image[offset..offset + *size as usize];
                        format!("file {}", String::from_utf8_lossy(bytes))
                    }
                    Kind::CharDevice { major: 0, minor: 0 } => "whiteout".into(),
                    other => format!("{other:?}"),
                };
                out.push((p, what));
            }
        }
        out.sort();
        out
    }

    /// Three layers as overlayfs leaves them: files replaced, removed and added, a
    /// directory removed and made again (opaque), a file made a directory, a directory
    /// made a file, hard links. Merged over something they keep hidden what they hid;
    /// merged at the bottom, what hid it goes.
    #[test]
    fn layers_merge_as_overlayfs_shows_them() {
        let tmp = shards_testdir::TempDir::new("merge").unwrap();
        // The lowest: a, d/{x, y}, e/z, f, g/w.
        let (mut t0, mut b0) = (Tree::new(meta(0o755)), Vec::new());
        file(&mut t0, Tree::ROOT, "a", &mut b0, b'a', 3);
        let d = dir(&mut t0, Tree::ROOT, "d", false);
        file(&mut t0, d, "x", &mut b0, b'x', 5000);
        file(&mut t0, d, "y", &mut b0, b'y', 2);
        let e = dir(&mut t0, Tree::ROOT, "e", false);
        file(&mut t0, e, "z", &mut b0, b'z', 1);
        file(&mut t0, Tree::ROOT, "f", &mut b0, b'f', 1);
        let g = dir(&mut t0, Tree::ROOT, "g", false);
        file(&mut t0, g, "w", &mut b0, b'w', 1);
        // Its changes: a replaced, d/x removed, e removed and made again with q, f made a
        // directory, g made a file, h added with a second name.
        let (mut t1, mut b1) = (Tree::new(meta(0o700)), Vec::new());
        file(&mut t1, Tree::ROOT, "a", &mut b1, b'A', 4);
        let d1 = dir(&mut t1, Tree::ROOT, "d", false);
        whiteout(&mut t1, d1, "x");
        let e1 = dir(&mut t1, Tree::ROOT, "e", true);
        file(&mut t1, e1, "q", &mut b1, b'q', 1);
        let f1 = dir(&mut t1, Tree::ROOT, "f", true);
        file(&mut t1, f1, "in", &mut b1, b'i', 1);
        file(&mut t1, Tree::ROOT, "g", &mut b1, b'G', 1);
        let h = file(&mut t1, Tree::ROOT, "h", &mut b1, b'h', 1);
        t1.link(d1, "h2".as_bytes(), h).unwrap();
        // The next: d removed whole, a new file in e, and a directory where a was, not
        // opaque, as a step that took the layers before `t1` and made it leaves it (two
        // steps sharing a cache, `t1`'s kept first).
        let (mut t2, mut b2) = (Tree::new(meta(0o711)), Vec::new());
        whiteout(&mut t2, Tree::ROOT, "d");
        let e2 = dir(&mut t2, Tree::ROOT, "e", false);
        file(&mut t2, e2, "r", &mut b2, b'r', 1);
        let a2 = dir(&mut t2, Tree::ROOT, "a", false);
        file(&mut t2, a2, "in2", &mut b2, b'2', 1);
        let layers = [
            (layer(&tmp, "0", &t0, b0), 0),
            (layer(&tmp, "1", &t1, b1), 5000 + 4096),
            (layer(&tmp, "2", &t2, b2), 3 * 4096),
        ];

        // The upper two over something: their whiteouts and opacity stay.
        let mut out = Vec::new();
        let m = merge(&layers[1..], false, [1; 16], &mut out).unwrap();
        assert_eq!(m.size, out.len() as u64);
        let s = |p: &str, w: &str| (p.to_string(), w.to_string());
        assert_eq!(
            listing(&out),
            vec![
                // It hid what lay below `t1`'s file, which hid it.
                s("/a", "dir opaque"),
                s("/a/in2", "file 2"),
                s("/d", "whiteout"),
                s("/e", "dir opaque"),
                s("/e/q", "file q"),
                s("/e/r", "file r"),
                s("/f", "dir opaque"),
                s("/f/in", "file i"),
                s("/g", "file G"),
                s("/h", "file h"),
            ]
        );
        // What each hid, less what of the run's own went: `t1`'s /a went, a block; d/h2
        // went with d, but its file is still /h.
        assert_eq!(m.hidden, 5000 + 4096 + 3 * 4096 - 4096);

        // All three, at the bottom: nothing hides anything any more.
        let mut out = Vec::new();
        let m = merge(&layers, true, [2; 16], &mut out).unwrap();
        assert_eq!(m.hidden, 0);
        assert_eq!(
            listing(&out),
            vec![
                s("/a", "dir"),
                s("/a/in2", "file 2"),
                s("/e", "dir"),
                s("/e/q", "file q"),
                s("/e/r", "file r"),
                s("/f", "dir"),
                s("/f/in", "file i"),
                s("/g", "file G"),
                s("/h", "file h"),
            ]
        );
        let tree = erofs::read_tree(&mut io::Cursor::new(&out), 0).unwrap();
        let root = tree.node(Tree::ROOT).unwrap();
        assert_eq!(root.meta.mode, 0o711, "the root is the uppermost's");

        // The lower two at the bottom: the hard link is one file of two names.
        let mut out = Vec::new();
        merge(&layers[..2], true, [3; 16], &mut out).unwrap();
        let tree = erofs::read_tree(&mut io::Cursor::new(&out), 0).unwrap();
        let d = tree.child(Tree::ROOT, b"d").unwrap();
        assert_eq!(tree.child(d, b"h2"), tree.child(Tree::ROOT, b"h"));
        assert_eq!(tree.child(d, b"x"), None);
        assert!(tree.child(d, b"y").is_some());
        assert!(tree.child(tree.child(Tree::ROOT, b"g").unwrap(), b"w").is_none());
    }
}
