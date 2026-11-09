//! A commit's tree, walked as `git checkout` writes it: each directory before what it
//! holds, entries in the tree's order.

use crate::Oid;
use crate::object::{self, Kind, Mode};
use crate::pack::Pack;

/// One path of a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Dir,
    File {
        executable: bool,
        data: Vec<u8>,
        oid: Oid,
    },
    Symlink {
        target: Vec<u8>,
        oid: Oid,
    },
    /// A submodule's commit: an empty directory, unless the submodule is fetched too.
    Submodule {
        commit: Oid,
    },
}

/// What a walk gives each path to.
pub type Visit<'a> = dyn FnMut(&[u8], Item) -> Result<(), String> + 'a;

/// Walks the tree `tree` of `pack`, giving each path (relative, `/`-separated) and what
/// it is to `visit`; a tree nested deeper than [`MAX_DEPTH`] is refused, as git refuses
/// it. The trees open are a stack of their own, not recursion, so a repository's nesting
/// is bounded by git's limit alone and never by the calling thread's stack.
pub fn walk(pack: &Pack, tree: &Oid, visit: &mut Visit<'_>) -> Result<(), String> {
    // Each tree open: its entries not yet given, and how long `path` is at it.
    let mut open = vec![(entries(pack, tree)?.into_iter(), 0usize)];
    let mut path = Vec::new();
    while let Some((rest, at)) = open.last_mut() {
        let at = *at;
        let Some(entry) = rest.next() else {
            open.pop();
            continue;
        };
        // git refuses to check out `.git` in any case and any directory (verify_path).
        if entry.name.eq_ignore_ascii_case(b".git") {
            return Err("a tree holding .git".into());
        }
        path.truncate(at);
        if at > 0 {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        let blob = |oid: &Oid| -> Result<Vec<u8>, String> {
            match pack.get(oid)? {
                (Kind::Blob, data) => Ok(data),
                _ => Err(format!("{} is not a blob", oid.hex())),
            }
        };
        match entry.mode {
            Mode::Tree => {
                visit(&path, Item::Dir)?;
                // The root is at depth 0, so this tree is as deep as the trees open.
                if open.len() > MAX_DEPTH {
                    return Err("a tree nested too deep".into());
                }
                open.push((entries(pack, &entry.oid)?.into_iter(), path.len()));
            }
            Mode::File | Mode::Executable => visit(
                &path,
                Item::File {
                    executable: entry.mode == Mode::Executable,
                    data: blob(&entry.oid)?,
                    oid: entry.oid,
                },
            )?,
            Mode::Symlink => visit(
                &path,
                Item::Symlink {
                    target: blob(&entry.oid)?,
                    oid: entry.oid,
                },
            )?,
            Mode::Gitlink => visit(&path, Item::Submodule { commit: entry.oid })?,
        }
    }
    Ok(())
}

/// The deepest a tree may be, the root at 0: core.maxTreeDepth's default where BuildKit
/// runs git, Linux (git v2.51.0 environment.c:105-106, `max_allowed_tree_depth`; 512 and
/// 1280 only in Windows builds), checked as unpack-trees checks it before it reads a tree
/// (tree-walk.c:445, `traverse_trees_cur_depth > max_allowed_tree_depth`).
pub const MAX_DEPTH: usize = 2048;

/// The entries of the tree `tree`.
fn entries(pack: &Pack, tree: &Oid) -> Result<Vec<object::Entry>, String> {
    let (kind, data) = pack.get(tree)?;
    if kind != Kind::Tree {
        return Err(format!("{} is not a tree", tree.hex()));
    }
    object::tree(&data)
}
