//! `SKILL`'s check (D54): the skills a fetched tree holds, each checked as the Agent
//! Skills reference validator checks it (shards_build::skill), and the file actions that
//! lay each in a directory of its name.
//!
//! A tree is one skill when a `SKILL.md` (or `skill.md`) is at its root, or when it is a
//! single Markdown file, the skill's `SKILL.md`; otherwise each entry at its root must be
//! a skill's directory. Anything else is refused, naming each skill and what is wrong
//! with it.
//!
//! The tree is read through [`Fetched`]: `shards build`'s own snapshot, or what BuildKit's
//! gateway answers of a solved one (the frontend, D113), the check the same for both.

use shards_build::data::Sources;
use shards_build::skill;
use shards_dockerfile::llb::{OpAction, OpActionKind};
use shards_image::erofs::{Kind, NodeId, Source as _, Tree};

use super::exec::Ref;

/// What an entry of the tree's root is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Other,
}

/// What a path of the tree is: not there, a regular file's bytes, or something else (no
/// symlink is followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Missing,
    File(Vec<u8>),
    Other,
}

/// What a skills step reads of the tree it checks.
pub trait Fetched {
    /// The root's entries, by name, in their names' order.
    fn entries(&mut self) -> Result<Vec<(Vec<u8>, EntryKind)>, String>;
    /// What `path`, relative to the root, is.
    fn lookup(&mut self, path: &[u8]) -> Result<Found, String>;
}

/// `shards build`'s snapshot of the tree.
struct Snapshot<'a> {
    tree: &'a Tree,
    sources: &'a mut Sources,
}

impl Snapshot<'_> {
    fn node(&self, path: &[u8]) -> Option<NodeId> {
        let mut at = Tree::ROOT;
        for c in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            at = self.tree.child(at, c)?;
        }
        Some(at)
    }
}

impl Fetched for Snapshot<'_> {
    fn entries(&mut self) -> Result<Vec<(Vec<u8>, EntryKind)>, String> {
        Ok(self
            .tree
            .entries(Tree::ROOT)
            .into_iter()
            .map(|(n, id)| {
                let kind = match self.tree.node(id).map(|n| &n.kind) {
                    Some(Kind::Dir(_)) => EntryKind::Dir,
                    Some(Kind::File { .. }) => EntryKind::File,
                    _ => EntryKind::Other,
                };
                (n.to_vec(), kind)
            })
            .collect())
    }

    fn lookup(&mut self, path: &[u8]) -> Result<Found, String> {
        let Some(id) = self.node(path) else {
            return Ok(Found::Missing);
        };
        let Some(node) = self.tree.node(id) else {
            return Ok(Found::Missing);
        };
        let Kind::File { size, data } = &node.kind else {
            return Ok(Found::Other);
        };
        let len = usize::try_from(*size).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; len];
        self.sources
            .read_at(*data, 0, &mut buf)
            .map_err(|e| e.to_string())?;
        Ok(Found::File(buf))
    }
}

/// The `SKILL.md` (else `skill.md`) of the directory `dir` (empty for the root), if it has
/// one that is a file: the first of the two names there decides.
fn skill_md(tree: &mut dyn Fetched, dir: &[u8]) -> Result<Option<Vec<u8>>, String> {
    for name in [b"SKILL.md".as_slice(), b"skill.md"] {
        let path = if dir.is_empty() {
            name.to_vec()
        } else {
            [dir, b"/", name].concat()
        };
        match tree.lookup(&path)? {
            Found::Missing => continue,
            Found::File(b) => return Ok(Some(b)),
            Found::Other => return Ok(None),
        }
    }
    Ok(None)
}

/// A copy of `src` in the fetched tree (input 0) to `dest`, onto what came before.
fn copy(src: Vec<u8>, dest: Vec<u8>, contents: bool) -> OpActionKind {
    OpActionKind::Copy {
        src,
        dest,
        owner: None,
        mode: -1,
        mode_str: Vec::new(),
        follow_symlink: false,
        dir_copy_contents: contents,
        attempt_unpack: false,
        create_dest_path: true,
        allow_wildcard: false,
        allow_empty_wildcard: false,
        timestamp: -1,
        include_patterns: Vec::new(),
        exclude_patterns: Vec::new(),
        required_paths: Vec::new(),
    }
}

/// The skills `fetched` holds, checked; and the actions that lay them out, each in a
/// directory of its name, onto nothing. `came_as` is the directory a source that is one
/// skill came as, which its name must match; none for one without (a URL, a heredoc).
pub fn layout(fetched: &Ref, came_as: &str, sources: &mut Sources) -> Result<Vec<OpAction>, String> {
    let mut snapshot = Snapshot {
        tree: fetched.fs.tree(),
        sources,
    };
    layout_of(&mut snapshot, came_as)
}

/// [`layout`] of a tree read through `tree`.
pub fn layout_of(tree: &mut dyn Fetched, came_as: &str) -> Result<Vec<OpAction>, String> {
    let entries = tree.entries()?;
    let mut errors = Vec::new();
    let mut laid: Vec<OpActionKind> = Vec::new();
    // Its name as the frontmatter gives it, checked against `dir` when there is one.
    let check =
        |shown: &str, dir: Option<&str>, md: Option<Vec<u8>>, errors: &mut Vec<String>| -> Option<String> {
            let parsed = md
                .as_deref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .map(|t| t.replace("\r\n", "\n").replace('\r', "\n"))
                .and_then(|t| skill::parse_frontmatter(&t).ok());
            let name = parsed.as_ref().and_then(|(m, _)| skill::name_of(m));
            let dir = dir
                .map(str::to_string)
                .or_else(|| name.clone())
                .unwrap_or_default();
            let found = skill::validate(&dir, md.as_deref());
            if found.is_empty() {
                name
            } else {
                for e in found {
                    errors.push(format!("skill {shown}: {e}"));
                }
                None
            }
        };
    let root_md = skill_md(tree, b"")?;
    if root_md.is_some() {
        // The tree is one skill.
        let dir = (!came_as.is_empty()).then_some(came_as);
        if let Some(name) = check(
            if came_as.is_empty() { "." } else { came_as },
            dir,
            root_md,
            &mut errors,
        ) {
            laid.push(copy(b"/".to_vec(), format!("/{name}").into_bytes(), true));
        }
    } else if let [(file, EntryKind::File)] = entries.as_slice()
        && file.to_ascii_lowercase().ends_with(b".md")
    {
        // A single Markdown file: the skill's SKILL.md, its directory its name.
        let shown = String::from_utf8_lossy(file).into_owned();
        let md = match tree.lookup(file)? {
            Found::File(b) => Some(b),
            _ => None,
        };
        if let Some(name) = check(&shown, None, md, &mut errors) {
            laid.push(copy(
                [b"/".as_slice(), file].concat(),
                format!("/{name}/SKILL.md").into_bytes(),
                false,
            ));
        }
    } else {
        for (entry, kind) in &entries {
            let shown = String::from_utf8_lossy(entry).into_owned();
            if *kind != EntryKind::Dir {
                errors.push(format!(
                    "{shown} is no skill: a skill is a directory whose SKILL.md names it"
                ));
                continue;
            }
            let md = skill_md(tree, entry)?;
            if check(&shown, Some(&shown), md, &mut errors).is_some() {
                laid.push(copy(
                    [b"/".as_slice(), entry].concat(),
                    [b"/".as_slice(), entry].concat(),
                    true,
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if laid.is_empty() {
        return Err("no skill in it: a skill is a directory whose SKILL.md names it".into());
    }
    // Chained onto nothing: each action's base the one before it, the last the output.
    let n = laid.len();
    Ok(laid
        .into_iter()
        .enumerate()
        .map(|(i, action)| OpAction {
            input: if i == 0 { -1 } else { i as i64 },
            secondary_input: 0,
            output: if i + 1 == n { 0 } else { -1 },
            action,
        })
        .collect())
}
