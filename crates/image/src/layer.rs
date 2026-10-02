//! Flattens OCI layers into one tree, as containerd stacks them for its default
//! (overlayfs) snapshotter and moby for overlay2: each layer applies at its own paths over
//! the layers below it (containerd v2.4.1 core/diff/apply/apply_linux.go, pkg/archive).
//!
//! - A layer's whiteouts apply before its other entries, whatever the archive's order, and
//!   only to lower layers (image-spec layer.md, "Whiteouts" and "Opaque Whiteout").
//! - An entry over an existing path: two directories merge, the entry's attributes
//!   replacing the directory's; anything else is replaced (layer.md, "Changeset over
//!   existing files").
//! - Missing parent directories are created 0755 and owned by root, as containerd's
//!   mkparent creates them, and at time 0. One replaces a lower layer's non-directory in
//!   its way, as a directory in an upper layer hides it. So lower layers' symlinks are not
//!   followed, while a layer's own are, as extracting into its own directory follows them.
//! - Entries for the root itself are ignored, as containerd ignores them.
//! - A hard link may name a file from a lower layer, and its header's attributes apply to
//!   the file, as containerd applies them after link(2).
//! - Attributes are those containerd sets: symlinks are 0777; trusted.* xattrs are
//!   dropped, as containerd refuses them, and user.* ones on anything but files and
//!   directories, as Linux refuses them; times before 1970 or past Go's range become 0.

use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};

use crate::erofs::{DataRef, Dir, Kind, Meta, Node, NodeId, Source, Tree};
use crate::tar::{self, Entry, Type};
use crate::{Error, bad};

const WHITEOUT: &[u8] = b".wh.";
const OPAQUE: &[u8] = b".wh..wh..opq";
/// continuity's walkLink limit, which containerd resolves layer paths within.
const MAX_LINKS: u32 = 255;
/// Linux's PATH_MAX, which counts a symlink target's terminating NUL.
const PATH_MAX: usize = 4096;
/// Go's latest time, 2^63 - 1 ns after 1970: containerd's boundTime zeroes later ones.
pub const MAX_TIME: (i64, u32) = (9_223_372_036, 854_775_807);

/// An image's empty root: a directory, 0755 and owned by root.
pub fn root() -> Tree {
    Tree::new(Meta {
        mode: 0o755,
        ..Meta::default()
    })
}

/// Applies a layer's uncompressed archive to `tree`. Files keep `source` in their
/// [`DataRef`], to be read back from the archive by it. `each` sees every entry as it is
/// read, before it is kept, and may refuse it.
///
/// The archive is read twice, its whiteouts applied in the first pass and everything
/// else in the second, so that no list of its entries is held: holding one took a build
/// that ADDs a million entries 138 MB higher (platform-measurements.md M78).
pub fn apply(
    tree: &mut Tree,
    source: u32,
    mut archive: impl Read + Seek,
    each: &mut dyn FnMut(&Entry) -> Result<(), Error>,
) -> Result<(), Error> {
    let start = archive.stream_position()?;
    let mut layer = Layer {
        first: tree.len(),
        tree,
        source,
        linked: HashSet::new(),
    };

    // Whiteouts, all found in the lower layers before any is applied.
    let mut hidden: Vec<(NodeId, Option<Vec<u8>>)> = Vec::new();
    {
        let mut reader = tar::Reader::new(&mut archive);
        while let Some(entry) = reader.next_entry()? {
            each(&entry)?;
            let Some((parent, name)) = split(&entry.path)? else {
                continue;
            };
            if name == OPAQUE {
                hidden.push((layer.dir(parent)?, None));
            } else if let Some(target) = name.strip_prefix(WHITEOUT) {
                if matches!(target, b"" | b"." | b"..") {
                    return bad(format!("invalid whiteout {:?}", show(&entry.path)));
                }
                hidden.push((layer.dir(parent)?, Some(target.to_vec())));
            }
        }
    }
    for (dir, name) in hidden {
        match name {
            Some(name) => {
                layer.tree.remove(dir, &name);
            }
            None => layer.tree.clear(dir),
        }
    }

    archive.seek(SeekFrom::Start(start))?;
    let mut reader = tar::Reader::new(&mut archive);
    while let Some(entry) = reader.next_entry()? {
        if let Some((parent, name)) = split(&entry.path)?
            && !name.starts_with(WHITEOUT)
        {
            layer.add(parent, name, &entry)?;
        }
    }
    Ok(())
}

/// A path's parent directory and final name.
type Parts<'a> = (&'a [u8], &'a [u8]);

/// Splits a path into its parent and final name, or `None` for the root. No directory on
/// the way may be a whiteout: such names cannot exist (layer.md).
fn split(path: &[u8]) -> Result<Option<Parts<'_>>, Error> {
    if path.is_empty() {
        return Ok(None);
    }
    let (parent, name) = match path.iter().rposition(|&c| c == b'/') {
        Some(slash) => (
            path.get(..slash).unwrap_or_default(),
            path.get(slash + 1..).unwrap_or_default(),
        ),
        None => (&[][..], path),
    };
    if parent
        .split(|&c| c == b'/')
        .any(|part| part.starts_with(WHITEOUT))
    {
        return bad(format!("{:?} is inside a whiteout", show(path)));
    }
    Ok(Some((parent, name)))
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

struct Layer<'t> {
    tree: &'t mut Tree,
    source: u32,
    /// The first node this layer makes: every node from it on is the layer's own.
    first: NodeId,
    /// The hard links this layer has made to nodes of lower layers, by directory and
    /// name. With the layer's own nodes, they are the entries it has made: a set of every
    /// entry would hold a million names for a layer of a million files (PM M78).
    linked: HashSet<(NodeId, Vec<u8>)>,
}

impl Layer<'_> {
    /// Whether this layer made the entry `name` of `dir`, which is `child`.
    fn fresh(&self, dir: NodeId, name: &[u8], child: NodeId) -> bool {
        child >= self.first || self.linked.contains(&(dir, name.to_vec()))
    }

    /// The directory at `path` as this layer sees it: lower directories merge, the layer's
    /// own symlinks are followed, and missing directories are made, each replacing a lower
    /// layer's non-directory in its way.
    fn dir(&mut self, path: &[u8]) -> Result<NodeId, Error> {
        let mut at = Tree::ROOT;
        let mut up: Vec<NodeId> = Vec::new();
        let mut todo: VecDeque<Vec<u8>> = path.split(|&c| c == b'/').map(<[u8]>::to_vec).collect();
        let mut links = 0;
        while let Some(name) = todo.pop_front() {
            match name.as_slice() {
                b"" | b"." => continue,
                b".." => {
                    at = up.pop().unwrap_or(Tree::ROOT);
                    continue;
                }
                _ => {}
            }
            let child = self
                .tree
                .child(at, &name)
                .and_then(|id| Some((id, &self.tree.node(id)?.kind)));
            let fresh = child.is_some_and(|(id, _)| self.fresh(at, &name, id));
            let next = match child {
                Some((id, Kind::Dir(_))) => id,
                Some((_, Kind::Symlink(target))) if fresh => {
                    links += 1;
                    if links > MAX_LINKS {
                        return bad(format!("too many symlinks in {:?}", show(path)));
                    }
                    if target.first() == Some(&b'/') {
                        at = Tree::ROOT;
                        up.clear();
                    }
                    for part in target.split(|&c| c == b'/').rev() {
                        todo.push_front(part.to_vec());
                    }
                    continue;
                }
                Some(_) if fresh => return bad(format!("{:?}: not a directory", show(path))),
                _ => self.tree.insert(
                    at,
                    &name,
                    Node {
                        kind: Kind::Dir(Dir::default()),
                        meta: Meta {
                            mode: 0o755,
                            ..Meta::default()
                        },
                    },
                )?,
            };
            up.push(at);
            at = next;
        }
        Ok(at)
    }

    /// The file a hard link names: looked up as `dir` walks, without making anything.
    fn target(&self, path: &[u8]) -> Result<NodeId, Error> {
        let missing = || Error(format!("hard link to missing {:?}", show(path)));
        let mut at = Tree::ROOT;
        let mut up: Vec<NodeId> = Vec::new();
        let mut todo: VecDeque<&[u8]> = path.split(|&c| c == b'/').collect();
        let name = todo.pop_back().filter(|n| !n.is_empty()).ok_or_else(missing)?;
        let mut links = 0;
        while let Some(part) = todo.pop_front() {
            match part {
                b"" | b"." => continue,
                b".." => {
                    at = up.pop().unwrap_or(Tree::ROOT);
                    continue;
                }
                _ => {}
            }
            let id = self.tree.child(at, part).ok_or_else(missing)?;
            match &self.tree.node(id).ok_or_else(missing)?.kind {
                Kind::Dir(_) => {
                    up.push(at);
                    at = id;
                }
                Kind::Symlink(target) if self.fresh(at, part, id) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return bad(format!("too many symlinks in {:?}", show(path)));
                    }
                    if target.first() == Some(&b'/') {
                        at = Tree::ROOT;
                        up.clear();
                    }
                    for p in target.split(|&c| c == b'/').rev() {
                        todo.push_front(p);
                    }
                }
                _ => return Err(missing()),
            }
        }
        self.tree.child(at, name).ok_or_else(missing)
    }

    /// Applies one entry at `parent`/`name`.
    fn add(&mut self, parent: &[u8], name: &[u8], entry: &Entry) -> Result<(), Error> {
        let dir = self.dir(parent)?;
        let existing = self.tree.child(dir, name);
        if entry.kind == Type::Dir
            && let Some(id) = existing
            && let Some(node) = self.tree.node_mut(id)
            && matches!(node.kind, Kind::Dir(_))
        {
            node.meta = meta(entry, &node.kind);
            return Ok(());
        }
        if existing.is_some() {
            self.tree.remove(dir, name);
        }
        let kind = match entry.kind {
            Type::HardLink => {
                let target = self.target(&entry.link)?;
                self.tree.link(dir, name, target)?;
                if target < self.first {
                    self.linked.insert((dir, name.to_vec()));
                }
                if let Some(node) = self.tree.node_mut(target) {
                    node.meta = meta(entry, &node.kind);
                }
                return Ok(());
            }
            Type::File => Kind::File {
                size: entry.size,
                data: DataRef {
                    source: self.source,
                    offset: entry.offset,
                },
            },
            Type::Symlink => {
                if entry.link.is_empty() || entry.link.len() >= PATH_MAX {
                    return bad(format!("{:?}: invalid symlink target", show(&entry.path)));
                }
                Kind::Symlink(entry.link.clone())
            }
            Type::CharDevice => Kind::CharDevice {
                major: entry.devmajor,
                minor: entry.devminor,
            },
            Type::BlockDevice => Kind::BlockDevice {
                major: entry.devmajor,
                minor: entry.devminor,
            },
            Type::Dir => Kind::Dir(Dir::default()),
            Type::Fifo => Kind::Fifo,
        };
        let meta = meta(entry, &kind);
        self.tree.insert(dir, name, Node { kind, meta })?;
        Ok(())
    }
}

/// The attributes containerd gives a node made from `entry`.
fn meta(entry: &Entry, kind: &Kind) -> Meta {
    let file_or_dir = matches!(kind, Kind::File { .. } | Kind::Dir(_));
    let (mtime, mtime_nsec) = if entry.mtime < 0 || (entry.mtime, entry.mtime_nsec) > MAX_TIME {
        (0, 0)
    } else {
        (entry.mtime, entry.mtime_nsec)
    };
    Meta {
        mode: match kind {
            Kind::Symlink(_) => 0o777,
            _ => (entry.mode & 0o7777) as u16,
        },
        uid: entry.uid,
        gid: entry.gid,
        mtime,
        mtime_nsec,
        xattrs: entry
            .xattrs
            .iter()
            .filter(|(name, _)| {
                name.starts_with(b"security.")
                    || *name == b"system.posix_acl_access"
                    || *name == b"system.posix_acl_default"
                    || (file_or_dir && name.starts_with(b"user."))
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    }
}

/// Reads file data back from the layers' archives, indexed by the `source` each was
/// applied with.
#[derive(Debug)]
pub struct Archives<R>(pub Vec<R>);

impl<R: Read + Seek> Source for Archives<R> {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let archive = usize::try_from(data.source)
            .ok()
            .and_then(|i| self.0.get_mut(i))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such layer"))?;
        let pos = data
            .offset
            .checked_add(at)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflows"))?;
        archive.seek(SeekFrom::Start(pos))?;
        archive.read_exact(buf)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::tar::tests::{Member, Writer};

    fn file<'a>(name: &'a [u8], data: &'a [u8]) -> Member<'a> {
        Member {
            name,
            data,
            ..Member::default()
        }
    }

    fn dir(name: &[u8], mode: u32) -> Member<'_> {
        Member {
            name,
            flag: b'5',
            mode,
            ..Member::default()
        }
    }

    fn symlink<'a>(name: &'a [u8], target: &'a [u8]) -> Member<'a> {
        Member {
            name,
            flag: b'2',
            link: target,
            ..Member::default()
        }
    }

    fn hardlink<'a>(name: &'a [u8], target: &'a [u8]) -> Member<'a> {
        Member {
            name,
            flag: b'1',
            link: target,
            ..Member::default()
        }
    }

    fn whiteout(name: &[u8]) -> Member<'_> {
        file(name, b"")
    }

    type Flat = (Tree, Archives<Cursor<Vec<u8>>>);

    /// Applies each layer in turn, keeping the archives to read file data back from.
    fn flatten(layers: &[Vec<Member<'_>>]) -> Result<Flat, Error> {
        let mut tree = root();
        let mut archives = Vec::new();
        for (i, members) in layers.iter().enumerate() {
            let mut w = Writer::default();
            for m in members {
                w.member(*m);
            }
            let archive = w.finish();
            apply(&mut tree, i as u32, Cursor::new(archive.as_slice()), &mut |_| {
                Ok(())
            })?;
            archives.push(Cursor::new(archive));
        }
        Ok((tree, Archives(archives)))
    }

    fn find(tree: &Tree, path: &str) -> Option<NodeId> {
        path.split('/')
            .filter(|p| !p.is_empty())
            .try_fold(Tree::ROOT, |at, name| tree.child(at, name.as_bytes()))
    }

    /// Each path under the root with a short description, in order.
    fn listing(tree: &Tree, archives: &mut Archives<Cursor<Vec<u8>>>) -> Vec<String> {
        fn walk(
            tree: &Tree,
            at: NodeId,
            prefix: &str,
            archives: &mut Archives<Cursor<Vec<u8>>>,
            out: &mut Vec<String>,
        ) {
            for (name, id) in tree.entries(at) {
                let path = format!("{prefix}/{}", String::from_utf8_lossy(name));
                let node = tree.node(id).unwrap();
                let m = &node.meta;
                let what = match &node.kind {
                    Kind::Dir(_) => "dir".to_string(),
                    Kind::File { size, data } => {
                        let mut bytes = vec![0; *size as usize];
                        archives.read_at(*data, 0, &mut bytes).unwrap();
                        format!("file {:?}", String::from_utf8(bytes).unwrap())
                    }
                    Kind::Symlink(t) => format!("-> {}", String::from_utf8_lossy(t)),
                    Kind::CharDevice { major, minor } => format!("char {major}:{minor}"),
                    Kind::BlockDevice { major, minor } => format!("block {major}:{minor}"),
                    Kind::Fifo => "fifo".to_string(),
                    Kind::Socket => "socket".to_string(),
                };
                out.push(format!("{path} {what} {:o} {}:{}", m.mode, m.uid, m.gid));
                walk(tree, id, &path, archives, out);
            }
        }
        let mut out = Vec::new();
        walk(tree, Tree::ROOT, "", archives, &mut out);
        out
    }

    fn lines(tree: &Tree, archives: &mut Archives<Cursor<Vec<u8>>>) -> String {
        listing(tree, archives).join("\n")
    }

    #[test]
    fn layers_apply_over_the_layers_below() {
        let (tree, mut archives) = flatten(&[
            vec![
                dir(b"etc/", 0o755),
                file(b"etc/hosts", b"old hosts"),
                file(b"etc/passwd", b"root"),
                dir(b"var/", 0o755),
                dir(b"var/lib/", 0o700),
                file(b"var/lib/state", b"state"),
                dir(b"opt/", 0o755),
                file(b"opt/tool", b"tool"),
                dir(b"srv/", 0o755),
                file(b"srv/keep", b"keep"),
                file(b"srv/gone", b"gone"),
                dir(b"srv/sub/", 0o755),
                file(b"srv/sub/deep", b"deep"),
                file(b"flip", b"was a file"),
                Member {
                    name: b"dev/null",
                    flag: b'3',
                    dev: (1, 3),
                    ..Member::default()
                },
            ],
            vec![
                // Changed and added files.
                file(b"etc/hosts", b"new hosts"),
                file(b"etc/resolv.conf", b"nameserver"),
                // A directory's attributes change; its children stay.
                dir(b"var/lib/", 0o750),
                // A file over a directory, and a directory over a file.
                file(b"opt", b"now a file"),
                dir(b"flip/", 0o711),
                file(b"flip/inner", b"inner"),
                // Whiteouts of a file and of a directory.
                whiteout(b"etc/.wh.passwd"),
                whiteout(b"srv/.wh.sub"),
                whiteout(b"srv/.wh.gone"),
            ],
        ])
        .unwrap();
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/dev dir 755 0:0",
                "/dev/null char 1:3 644 0:0",
                "/etc dir 755 0:0",
                "/etc/hosts file \"new hosts\" 644 0:0",
                "/etc/resolv.conf file \"nameserver\" 644 0:0",
                "/flip dir 711 0:0",
                "/flip/inner file \"inner\" 644 0:0",
                "/opt file \"now a file\" 644 0:0",
                "/srv dir 755 0:0",
                "/srv/keep file \"keep\" 644 0:0",
                "/var dir 755 0:0",
                "/var/lib dir 750 0:0",
                "/var/lib/state file \"state\" 644 0:0",
            ]
            .join("\n")
        );
    }

    #[test]
    fn whiteouts_hide_only_lower_layers() {
        let (tree, mut archives) = flatten(&[
            vec![
                file(b"a", b"lower a"),
                file(b"b", b"lower b"),
                dir(b"d/", 0o755),
                file(b"d/lower", b"lower"),
                dir(b"d/sub/", 0o755),
                file(b"d/sub/x", b"x"),
                dir(b"e/", 0o755),
                file(b"e/lower", b"lower"),
            ],
            vec![
                // The layer's own entries survive its whiteouts, before or after them.
                file(b"a", b"upper a"),
                whiteout(b".wh.a"),
                whiteout(b".wh.b"),
                file(b"b", b"upper b"),
                // Opaque directories keep the layer's entries, whichever comes first.
                file(b"d/upper", b"upper"),
                whiteout(b"d/.wh..wh..opq"),
                whiteout(b"e/.wh..wh..opq"),
                file(b"e/upper", b"upper"),
            ],
        ])
        .unwrap();
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/a file \"upper a\" 644 0:0",
                "/b file \"upper b\" 644 0:0",
                "/d dir 755 0:0",
                "/d/upper file \"upper\" 644 0:0",
                "/e dir 755 0:0",
                "/e/upper file \"upper\" 644 0:0",
            ]
            .join("\n")
        );
    }

    #[test]
    fn lower_symlinks_are_not_followed() {
        let (tree, mut archives) = flatten(&[
            vec![
                dir(b"usr/", 0o755),
                dir(b"usr/lib/", 0o755),
                file(b"usr/lib/libc", b"libc"),
                symlink(b"lib", b"usr/lib"),
                dir(b"c/", 0o755),
                file(b"c/kept", b"kept"),
                symlink(b"a", b"c"),
            ],
            vec![
                // Through a lower symlink, as overlayfs stacks it: a new directory hides
                // the symlink, and what it pointed at is untouched.
                file(b"lib/foo", b"foo"),
                // An opaque directory replacing a lower symlink hides nothing behind it.
                dir(b"a/", 0o755),
                whiteout(b"a/.wh..wh..opq"),
                file(b"a/f", b"f"),
            ],
        ])
        .unwrap();
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/a dir 755 0:0",
                "/a/f file \"f\" 644 0:0",
                "/c dir 755 0:0",
                "/c/kept file \"kept\" 644 0:0",
                "/lib dir 755 0:0",
                "/lib/foo file \"foo\" 644 0:0",
                "/usr dir 755 0:0",
                "/usr/lib dir 755 0:0",
                "/usr/lib/libc file \"libc\" 644 0:0",
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_layers_own_symlinks_are_followed() {
        let (tree, mut archives) = flatten(&[vec![
            dir(b"real/", 0o755),
            symlink(b"rel", b"real"),
            symlink(b"abs", b"/real/../real"),
            dir(b"real/sub/", 0o755),
            symlink(b"real/sub/up", b"../.."),
            file(b"rel/a", b"a"),
            file(b"abs/b", b"b"),
            file(b"real/sub/up/real/c", b"c"),
            hardlink(b"d", b"rel/a"),
        ]])
        .unwrap();
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/abs -> /real/../real 777 0:0",
                "/d file \"a\" 644 0:0",
                "/real dir 755 0:0",
                "/real/a file \"a\" 644 0:0",
                "/real/b file \"b\" 644 0:0",
                "/real/c file \"c\" 644 0:0",
                "/real/sub dir 755 0:0",
                "/real/sub/up -> ../.. 777 0:0",
                "/rel -> real 777 0:0",
            ]
            .join("\n")
        );
        let looped = flatten(&[vec![symlink(b"l", b"l"), file(b"l/x", b"x")]]);
        assert!(looped.is_err(), "a symlink loop");
        let through_file = flatten(&[vec![file(b"f", b"f"), file(b"f/x", b"x")]]);
        assert!(through_file.is_err(), "a path through the layer's own file");
    }

    #[test]
    fn hard_links_share_a_file() {
        let (tree, mut archives) = flatten(&[
            vec![
                file(b"one", b"shared"),
                hardlink(b"two", b"one"),
                file(b"base", b"base"),
            ],
            vec![
                // Replacing one name leaves the other with the old file.
                file(b"one", b"replaced"),
                // A link to a lower layer's file, whose header sets the file's attributes.
                Member {
                    name: b"three",
                    flag: b'1',
                    link: b"base",
                    mode: 0o600,
                    uid: 5,
                    ..Member::default()
                },
            ],
        ])
        .unwrap();
        assert_eq!(find(&tree, "base"), find(&tree, "three"));
        assert_ne!(find(&tree, "one"), find(&tree, "two"));
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/base file \"base\" 600 5:0",
                "/one file \"replaced\" 644 0:0",
                "/three file \"base\" 600 5:0",
                "/two file \"shared\" 644 0:0",
            ]
            .join("\n")
        );
        for (layer, why) in [
            (vec![hardlink(b"x", b"missing")], "a missing target"),
            (vec![dir(b"d/", 0o755), hardlink(b"x", b"d")], "a directory"),
            (
                vec![file(b"x", b"x"), hardlink(b"x", b"x")],
                "itself, which the link replaces",
            ),
        ] {
            assert!(flatten(&[layer]).is_err(), "{why}");
        }
    }

    #[test]
    fn parents_are_made_as_containerd_makes_them() {
        let (tree, mut archives) = flatten(&[
            vec![
                dir(b"kept/", 0o700),
                file(b"file", b"file"),
                // The root's own entry is ignored.
                dir(b"./", 0o700),
            ],
            vec![
                file(b"kept/x", b"x"),
                file(b"file/y", b"y"),
                file(b"new/deep/z", b"z"),
                whiteout(b"ghost/.wh.w"),
            ],
        ])
        .unwrap();
        assert_eq!(tree.node(Tree::ROOT).unwrap().meta.mode, 0o755);
        assert_eq!(
            lines(&tree, &mut archives),
            [
                "/file dir 755 0:0",
                "/file/y file \"y\" 644 0:0",
                "/ghost dir 755 0:0",
                "/kept dir 700 0:0",
                "/kept/x file \"x\" 644 0:0",
                "/new dir 755 0:0",
                "/new/deep dir 755 0:0",
                "/new/deep/z file \"z\" 644 0:0",
            ]
            .join("\n")
        );
    }

    #[test]
    fn attributes_are_the_ones_containerd_sets() {
        let mut w = Writer::default();
        w.pax(&[
            ("mtime", b"1700000000.5"),
            ("SCHILY.xattr.security.capability", b"cap"),
            ("SCHILY.xattr.user.note", b"note"),
            ("SCHILY.xattr.trusted.overlay.opaque", b"y"),
            ("SCHILY.xattr.system.posix_acl_access", b"acl"),
            ("SCHILY.xattr.lustre.x", b"x"),
        ])
        .member(file(b"f", b"f"))
        .pax(&[
            ("SCHILY.xattr.user.note", b"note"),
            ("SCHILY.xattr.security.selinux", b"label"),
        ])
        .member(Member {
            mode: 0o644,
            ..symlink(b"s", b"f")
        })
        .pax(&[("mtime", b"-1")])
        .member(dir(b"old/", 0o755))
        .pax(&[("mtime", b"9223372037")])
        .member(dir(b"late/", 0o755))
        .member(Member {
            mode: 0o7755,
            ..file(b"suid", b"s")
        });
        let mut tree = root();
        apply(&mut tree, 0, Cursor::new(w.finish()), &mut |_| Ok(())).unwrap();
        let meta = |path: &str| tree.node(find(&tree, path).unwrap()).unwrap().meta.clone();
        let names = |m: &Meta| {
            m.xattrs
                .keys()
                .map(|k| String::from_utf8(k.clone()).unwrap())
                .collect::<Vec<_>>()
        };
        let f = meta("f");
        assert_eq!((f.mtime, f.mtime_nsec), (1_700_000_000, 500_000_000));
        assert_eq!(
            names(&f),
            ["security.capability", "system.posix_acl_access", "user.note"]
        );
        let s = meta("s");
        assert_eq!((s.mode, names(&s)), (0o777, vec!["security.selinux".to_string()]));
        assert_eq!((meta("old").mtime, meta("late").mtime), (0, 0));
        assert_eq!(meta("suid").mode, 0o7755);
    }

    #[test]
    fn invalid_layers_are_refused() {
        for (layer, why) in [
            (vec![whiteout(b".wh.")], "a whiteout without a name"),
            (vec![whiteout(b"d/.wh..")], "a whiteout of .."),
            (vec![file(b".wh.x/y", b"y")], "an entry inside a whiteout"),
            (vec![symlink(b"s", b"")], "an empty symlink"),
        ] {
            assert!(flatten(&[layer]).is_err(), "{why}");
        }
        let long = Writer::default()
            .pax(&[("path", &[b'n'; 256])])
            .member(file(b"n", b""))
            .finish();
        assert!(
            apply(&mut root(), 0, Cursor::new(long.as_slice()), &mut |_| Ok(())).is_err(),
            "a name longer than 255 bytes"
        );
    }
}
