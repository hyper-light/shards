//! The tree an image's layers stack to, followed beside a build's snapshots, so that the
//! export writes the image's root filesystem from the last snapshot rather than read
//! every layer back and stack them again as `Store::rootfs` does.
//!
//! A snapshot is what the next step would see; what layer::apply makes of the layers is
//! what the image holds. They differ where the layers lose what the snapshot has:
//! - an entry a layer writes keeps a node's type, mode, owner and content, but only the
//!   whole seconds of its mtime (ChangeWriter truncates it; layer::meta makes a time
//!   before 1970 or past Go's range 0), and of its xattrs `security.capability` alone,
//!   if not empty (diff.rs); a symlink is 0777 (layer.rs);
//! - a node no layer writes keeps what its last entry, or the base image, gave it: a
//!   directory whose mtime or xattrs a step changes without the differ writing it, a file
//!   whose xattrs change, a node a step makes again as it was;
//! - the root is layer::root()'s, as no layer writes it;
//! - sockets are left out.
//!
//! Each commit records which nodes its layer writes, and what a node it leaves keeps in
//! the layers where the snapshot changed it ([`Stack::commit`]); [`Stack::finish`] then
//! puts the last snapshot in the layers' form in place. What it cannot follow exactly it
//! gives up on, and the export stacks the layers as `Store::rootfs` does: a hard link
//! only some of whose names a layer writes, content a layer leaves as it was where the
//! snapshot changed it, a name layer::apply reads as a whiteout, a symlink target it
//! refuses, a socket over a lower file, a merge onto a snapshot not in its layers' form,
//! and a snapshot no layers made.
//!
//! Node ids are a tree's own: a commit's upper tree must be its lower tree as the step
//! changed it, which [`Fs`]'s file operations never renumber.

use std::collections::HashMap;
use std::rc::Rc;

use shards_image::erofs::{Kind, Meta, Node, NodeId, Source, Tree};
use shards_image::layer;

use crate::Error;
use crate::diff::{self, Bits, Record};
use crate::vfs::{CAPABILITY, Fs};

/// What a stack of layers holds against `Store::rootfs`'s limits: its layers' entries,
/// the bytes of their names, link targets and xattrs (at most), and the bytes the layers'
/// archives take uncompressed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub entries: u64,
    pub metadata: u64,
    pub bytes: u64,
}

impl Tally {
    pub fn plus(self, other: Tally) -> Tally {
        Tally {
            entries: self.entries.saturating_add(other.entries),
            metadata: self.metadata.saturating_add(other.metadata),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }
}

/// How a snapshot differs from the tree its layers stack to.
#[derive(Debug, Clone, Default)]
pub struct Form {
    /// The nodes some layer wrote: in the layers' form once [`to_layer`] is applied.
    written: Bits,
    /// What nodes have in the layers where it is not what the snapshot has, in its
    /// layers' form or not: the attributes of the last entry that wrote them, or the
    /// base image's.
    kept: HashMap<NodeId, Meta>,
    /// Whether the snapshot may have sockets, which no layer has.
    sockets: bool,
    tally: Tally,
    /// The version of the snapshot's tree this describes ([`Tree::version`]): node ids
    /// are that tree's alone.
    version: u64,
}

impl Form {
    /// Whether the snapshot is the layers' tree, its root aside.
    fn is_plain(&self) -> bool {
        self.written.is_empty() && self.kept.is_empty() && !self.sockets
    }

    /// The attributes node `id`, which the snapshot has as `node`, has in the layers.
    fn layered(&self, id: NodeId, node: &Node) -> Meta {
        if id == Tree::ROOT {
            return root_meta();
        }
        if let Some(m) = self.kept.get(&id) {
            return m.clone();
        }
        self.unkept(id, node)
    }

    /// [`Form::layered`] were nothing kept for `id`.
    fn unkept(&self, id: NodeId, node: &Node) -> Meta {
        let mut meta = node.meta.clone();
        if self.written.get(id) {
            to_layer(&node.kind, &mut meta);
        }
        meta
    }
}

/// A snapshot's relation to the tree its layers stack to.
#[derive(Debug, Clone)]
pub enum Stack {
    /// The tree is the snapshot put in its layers' form, as this says.
    Known(Rc<Form>),
    /// Not followed, for this reason: the export stacks the layers.
    Unknown(Rc<str>),
}

/// The root's attributes in every stack of layers: layer::root()'s.
fn root_meta() -> Meta {
    layer::root()
        .node(Tree::ROOT)
        .map(|n| n.meta.clone())
        .unwrap_or_default()
}

/// Puts the attributes of a node of `kind` in the form an entry the differ writes of it
/// takes when applied: mode bits alone, 0777 for a symlink; whole seconds of mtime, 0
/// when before 1970 or past Go's range; `security.capability` alone of its xattrs, if not
/// empty.
pub fn to_layer(kind: &Kind, m: &mut Meta) {
    m.mode = match kind {
        Kind::Symlink(_) => 0o777,
        _ => m.mode & 0o7777,
    };
    if m.mtime < 0 || m.mtime > layer::MAX_TIME.0 {
        m.mtime = 0;
    }
    m.mtime_nsec = 0;
    m.xattrs.retain(|k, v| k == CAPABILITY && !v.is_empty());
}

/// Whether two nodes hold the same apart from their attributes: their type, a file's
/// size and data, a symlink's target, a device's numbers. A directory's entries are the
/// tree's, not the node's.
fn same_payload(a: &Kind, b: &Kind) -> bool {
    match (a, b) {
        (Kind::Dir(_), Kind::Dir(_)) => true,
        _ => a == b,
    }
}

/// Whether `a` and `b`, of different payloads, read the same: files of equal size and
/// bytes.
fn same_bytes(a: &Kind, b: &Kind, data: &mut dyn Source) -> Result<bool, Error> {
    match (a, b) {
        (Kind::File { size: s, data: x }, Kind::File { size: t, data: y }) if s == t => {
            diff::same_content(*x, *y, *s, data)
        }
        _ => Ok(false),
    }
}

impl Stack {
    /// A snapshot layer::apply made of its layers from layer::root(), which held `tally`;
    /// or scratch, whose root only differs.
    pub fn layers(tally: Tally, tree: &Tree) -> Stack {
        Stack::Known(Rc::new(Form {
            tally,
            version: tree.version(),
            ..Form::default()
        }))
    }

    pub fn unknown(why: &str) -> Stack {
        Stack::Unknown(why.into())
    }

    /// Whether this follows `tree`: the very tree, not another or a copy of it.
    pub fn follows(&self, tree: &Tree) -> bool {
        matches!(self, Stack::Known(f) if f.version == tree.version())
    }

    /// What its layers hold, if followed.
    pub fn tally(&self) -> Option<Tally> {
        match self {
            Stack::Known(f) => Some(f.tally),
            Stack::Unknown(_) => None,
        }
    }

    /// A merge's snapshot: layers holding `applied` applied by layer::apply onto this
    /// one. Followed only onto a snapshot that is its layers' tree, which the applied
    /// layers' entries then leave so.
    pub fn merge(&self, applied: Tally, tree: &Tree) -> Stack {
        match self {
            Stack::Known(f) if f.is_plain() => Stack::layers(f.tally.plus(applied), tree),
            Stack::Known(_) => Stack::unknown("a merge onto a snapshot not in its layers' form"),
            Stack::Unknown(_) => self.clone(),
        }
    }

    /// The stack once the layer `rec` describes, of `bytes` bytes, made `upper` from
    /// `lower`, this stack's snapshot. `data` reads content the layer may leave.
    pub fn commit(
        &self,
        lower: &Fs,
        upper: &Fs,
        rec: Record,
        bytes: u64,
        data: &mut dyn Source,
    ) -> Result<Stack, Error> {
        let Stack::Known(prev) = self else {
            return Ok(self.clone());
        };
        // What this follows is the lower tree's, and the upper tree is a clone of it, as
        // a step's is: otherwise its node ids are not the ones this knows.
        if prev.version != lower.tree().version() || upper.tree().parent() != lower.tree().version() {
            return Ok(Stack::unknown("a snapshot other than the one followed"));
        }
        if let Some(why) = rec.unsure {
            return Ok(Stack::unknown(&why));
        }
        let links = |id: NodeId| rec.links.get(id).copied().unwrap_or(0);
        // A node some of whose names the layer leaves is two in the layers: one made of
        // its entry, the other as it was.
        if rec.names.iter().any(|(&id, &n)| links(id) != n) {
            return Ok(Stack::unknown("a hard link only some of whose names a layer has"));
        }
        let unknown = |why: &str| Ok(Stack::unknown(why));
        let mut next = (**prev).clone();
        // Paths the layer leaves with the lower's node where the snapshot has another.
        let mut lower_links: Option<Vec<u32>> = None;
        let mut paired = Bits::default();
        for &(l, u) in &rec.same {
            if rec.written.get(u) {
                continue;
            }
            paired.set(u);
            let (Some(ln), Some(un)) = (lower.node(l), upper.node(u)) else {
                return unknown("a node missing from its tree");
            };
            if !matches!(un.kind, Kind::Dir(_)) {
                let ll = lower_links.get_or_insert_with(|| lower.links());
                if ll.get(l).copied().unwrap_or(0) > 1 || links(u) > 1 {
                    return unknown("a hard link a layer leaves as the lower's");
                }
                if !same_payload(&ln.kind, &un.kind) && !same_bytes(&ln.kind, &un.kind, data)? {
                    return unknown("content a layer leaves as the lower's");
                }
            }
            let want = prev.layered(l, ln);
            if want == prev.unkept(u, un) {
                next.kept.remove(&u);
            } else {
                next.kept.insert(u, want);
            }
        }
        // Ids below `old` are the lower tree's nodes; the rest the step made, and each
        // the upper tree names must be in the layer, or stand for a lower node there,
        // but for sockets.
        let old = lower.tree().len().min(upper.tree().len());
        for id in old..upper.tree().len() {
            if links(id) > 0
                && !rec.written.get(id)
                && !paired.get(id)
                && !matches!(upper.node(id).map(|n| &n.kind), Some(Kind::Socket))
            {
                return unknown("a new node the layer leaves out");
            }
        }
        // Nodes the step changed in place and the layer leaves: the differ judged them
        // the same, or never saw them. Those already kept are looked at again: what a
        // step changes unseen in one, the layers do not have either.
        for id in 1..old {
            if links(id) == 0 || rec.written.get(id) {
                continue;
            }
            let (Some(a), Some(b)) = (lower.node(id), upper.node(id)) else {
                continue;
            };
            let payload = same_payload(&a.kind, &b.kind);
            if a.meta == b.meta && payload {
                continue;
            }
            if !payload && !same_bytes(&a.kind, &b.kind, data)? {
                return unknown("content a layer leaves as the lower's");
            }
            let want = prev.layered(id, a);
            if want != prev.unkept(id, b) {
                next.kept.insert(id, want);
            }
        }
        for id in rec.written.iter() {
            next.kept.remove(&id);
        }
        next.written.union(&rec.written);
        next.sockets |= rec.sockets;
        next.version = upper.tree().version();
        next.tally = next.tally.plus(Tally {
            entries: rec.entries,
            metadata: rec.metadata,
            bytes,
        });
        Ok(Stack::Known(Rc::new(next)))
    }

    /// Puts `fs`, this stack's snapshot, in its layers' form in place: the tree
    /// layer::apply makes of the layers, as erofs::write writes it. The reason it is not
    /// followed, if it is not.
    pub fn finish(&self, fs: &mut Fs) -> Result<(), Rc<str>> {
        let form = match self {
            Stack::Known(f) => f,
            Stack::Unknown(why) => return Err(why.clone()),
        };
        if form.version != fs.tree().version() {
            return Err("a snapshot other than the one followed".into());
        }
        for id in form.written.iter() {
            if !form.kept.contains_key(&id)
                && let Some(Node { kind, meta }) = fs.unrecorded_tree().node_mut(id)
            {
                to_layer(kind, meta);
            }
        }
        for (&id, m) in &form.kept {
            if let Some(n) = fs.unrecorded_tree().node_mut(id) {
                n.meta = m.clone();
            }
        }
        if let Some(n) = fs.unrecorded_tree().node_mut(Tree::ROOT) {
            n.meta = root_meta();
        }
        if form.sockets {
            let mut todo = vec![Tree::ROOT];
            let mut gone: Vec<(NodeId, Vec<u8>)> = Vec::new();
            let mut entries = Vec::new();
            while let Some(dir) = todo.pop() {
                fs.tree().entries_into(dir, &mut entries);
                for &(name, id) in &entries {
                    match fs.tree().node(id).map(|n| &n.kind) {
                        Some(Kind::Socket) => gone.push((dir, name.to_vec())),
                        Some(Kind::Dir(_)) => todo.push(id),
                        _ => {}
                    }
                }
            }
            for (dir, name) in gone {
                fs.unrecorded_tree().remove(dir, &name);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use shards_image::erofs::{Dir, Meta, Node};

    use super::*;
    use crate::data::Sources;
    use crate::diff::write_layer;

    fn fs() -> Fs {
        let mut tree = layer::root();
        let dir = Node {
            kind: Kind::Dir(Dir::default()),
            meta: Meta {
                mode: 0o755,
                ..Meta::default()
            },
        };
        tree.insert(Tree::ROOT, b"d", dir).unwrap();
        Fs::new(tree, (1_600_000_000, 0))
    }

    /// A stack follows its own tree: not a copy of it, which is another tree, nor the
    /// tree once compaction has renumbered its nodes.
    #[test]
    fn a_stack_follows_its_tree_alone() {
        let mut lower = fs();
        let stack = Stack::layers(Tally::default(), lower.tree());
        assert!(stack.follows(lower.tree()));
        assert!(!stack.follows(lower.clone().tree()));
        lower.unrecorded_tree().remove(Tree::ROOT, b"d");
        lower.unrecorded_tree().compact();
        assert!(!stack.follows(lower.tree()));
    }

    /// A commit gives up, rather than read node ids as another tree's, unless its lower
    /// tree is the one followed and its upper tree a copy of that one: so a step on one
    /// fork of a snapshot is never taken for a step on another.
    #[test]
    fn a_commit_of_another_tree_gives_up() {
        let lower = fs();
        let stack = Stack::layers(Tally::default(), lower.tree());
        let commit = |stack: &Stack, lower: &Fs, upper: &Fs| {
            let mut out = Vec::new();
            let rec = write_layer(lower, upper, &mut Sources::default(), &mut out).unwrap();
            stack
                .commit(lower, upper, rec, 0, &mut Sources::default())
                .unwrap()
        };
        let mut upper = lower.clone();
        upper.begin();
        upper.mkdir(b"/n", 0o755).unwrap();
        let next = commit(&stack, &lower, &upper);
        assert!(next.follows(upper.tree()));
        // A fork of the same snapshot, committed with the first fork's stack.
        let mut sibling = lower.clone();
        sibling.begin();
        let mut child = sibling.clone();
        child.begin();
        assert!(matches!(commit(&next, &sibling, &child), Stack::Unknown(_)));
        // An upper tree that is no copy of the lower.
        assert!(matches!(commit(&stack, &lower, &fs()), Stack::Unknown(_)));
    }
}
