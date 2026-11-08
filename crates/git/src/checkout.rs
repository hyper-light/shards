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
/// it is to `visit`; trees nested deeper than `max_depth` are refused, as git refuses
/// them past core.maxTreeDepth (4096 by default, git v2.51.0).
pub fn walk(pack: &Pack, tree: &Oid, visit: &mut Visit<'_>) -> Result<(), String> {
    walk_in(pack, tree, &mut Vec::new(), 0, visit)
}

const MAX_DEPTH: usize = 4096;

fn walk_in(
    pack: &Pack,
    tree: &Oid,
    path: &mut Vec<u8>,
    depth: usize,
    visit: &mut Visit<'_>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("a tree nested too deep".into());
    }
    let (kind, data) = pack.get(tree)?;
    if kind != Kind::Tree {
        return Err(format!("{} is not a tree", tree.hex()));
    }
    for entry in object::tree(&data)? {
        // git refuses to check out `.git` in any case and any directory (verify_path).
        if entry.name.eq_ignore_ascii_case(b".git") {
            return Err("a tree holding .git".into());
        }
        let len = path.len();
        if len > 0 {
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
                visit(path, Item::Dir)?;
                walk_in(pack, &entry.oid, path, depth + 1, visit)?;
            }
            Mode::File | Mode::Executable => visit(
                path,
                Item::File {
                    executable: entry.mode == Mode::Executable,
                    data: blob(&entry.oid)?,
                    oid: entry.oid,
                },
            )?,
            Mode::Symlink => visit(
                path,
                Item::Symlink {
                    target: blob(&entry.oid)?,
                    oid: entry.oid,
                },
            )?,
            Mode::Gitlink => visit(path, Item::Submodule { commit: entry.oid })?,
        }
        path.truncate(len);
    }
    Ok(())
}
