//! The objects a checkout reads: commits, annotated tags and trees (git's commit.c
//! parse_commit_buffer, tag.c parse_tag_buffer and tree-walk.c decode_tree_entry).

use crate::Oid;

/// An object's kind, numbered as a packfile numbers it (gitformat-pack.adoc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn name(self) -> &'static [u8] {
        match self {
            Kind::Commit => b"commit",
            Kind::Tree => b"tree",
            Kind::Blob => b"blob",
            Kind::Tag => b"tag",
        }
    }

    pub fn of_pack(n: u8) -> Option<Kind> {
        match n {
            1 => Some(Kind::Commit),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Blob),
            4 => Some(Kind::Tag),
            _ => None,
        }
    }
}

/// A tree's entry, as git writes them: its mode, name and object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: Mode,
    pub name: Vec<u8>,
    pub oid: Oid,
}

/// What an entry is, by its mode (git's canon_mode): the five git writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 100644.
    File,
    /// 100755.
    Executable,
    /// 120000: its blob is the link's target.
    Symlink,
    /// 040000.
    Tree,
    /// 160000: a submodule's commit.
    Gitlink,
}

impl Mode {
    /// canon_mode: a mode as git reads it, which keeps only the kind and, for a file,
    /// whether its owner may execute it (old trees wrote 100664 and such).
    /// tree-walk.c get_mode reads any octal digits; fsck alone warns of zero padding.
    fn parse(octal: &[u8]) -> Option<Mode> {
        if octal.is_empty() || octal.len() > 6 {
            return None;
        }
        let mut mode: u32 = 0;
        for &c in octal {
            if !(b'0'..=b'7').contains(&c) {
                return None;
            }
            mode = mode << 3 | u32::from(c - b'0');
        }
        Some(match mode & 0o170000 {
            // ce_permissions: the owner's execute bit alone decides.
            0o100000 if mode & 0o100 != 0 => Mode::Executable,
            0o100000 => Mode::File,
            0o120000 => Mode::Symlink,
            0o040000 => Mode::Tree,
            0o160000 => Mode::Gitlink,
            _ => return None,
        })
    }
}

/// A tree's entries, in its order; `Err` names what is malformed, as fsck would.
pub fn tree(data: &[u8]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or("a tree entry without a mode")?;
        let mode = Mode::parse(rest.get(..space).unwrap_or_default()).ok_or("a tree entry of a bad mode")?;
        rest = rest.get(space + 1..).unwrap_or_default();
        let nul = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("a tree entry without a name's end")?;
        let name = rest.get(..nul).unwrap_or_default().to_vec();
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err(format!("a tree entry named {:?}", String::from_utf8_lossy(&name)));
        }
        let oid: [u8; 20] = rest
            .get(nul + 1..nul + 21)
            .and_then(|b| b.try_into().ok())
            .ok_or("a tree entry cut short")?;
        rest = rest.get(nul + 21..).unwrap_or_default();
        entries.push(Entry {
            mode,
            name,
            oid: Oid(oid),
        });
    }
    Ok(entries)
}

/// What a checkout reads of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    /// The committer's time, seconds since the epoch.
    pub committed: i64,
}

/// A commit's headers.
pub fn commit(data: &[u8]) -> Result<Commit, String> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut committed = None;
    for line in headers(data) {
        if let Some(hex) = line.strip_prefix(b"tree ") {
            tree = Some(Oid::parse(hex).ok_or("a commit's bad tree")?);
        } else if let Some(hex) = line.strip_prefix(b"parent ") {
            parents.push(Oid::parse(hex).ok_or("a commit's bad parent")?);
        } else if let Some(who) = line.strip_prefix(b"committer ") {
            committed = Some(signature_time(who).ok_or("a commit's bad committer")?);
        }
    }
    Ok(Commit {
        tree: tree.ok_or("a commit without a tree")?,
        parents,
        committed: committed.ok_or("a commit without a committer")?,
    })
}

/// What an annotated tag names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub object: Oid,
    pub kind: Kind,
}

pub fn tag(data: &[u8]) -> Result<Tag, String> {
    let mut object = None;
    let mut kind = None;
    for line in headers(data) {
        if let Some(hex) = line.strip_prefix(b"object ") {
            object = Some(Oid::parse(hex).ok_or("a tag's bad object")?);
        } else if let Some(name) = line.strip_prefix(b"type ") {
            kind = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag]
                .into_iter()
                .find(|k| k.name() == name);
        }
    }
    Ok(Tag {
        object: object.ok_or("a tag without an object")?,
        kind: kind.ok_or("a tag without a type")?,
    })
}

/// An object's header lines, up to the blank line before its message; a continuation
/// line (a signature's) is skipped.
fn headers(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&b| b == b'\n')
        .take_while(|l| !l.is_empty())
        .filter(|l| !l.starts_with(b" "))
}

/// The time of `Name <email> 1700000000 +0100`: the number after the last `>`.
fn signature_time(who: &[u8]) -> Option<i64> {
    let after = who.get(who.iter().rposition(|&b| b == b'>')? + 1..)?;
    let field = after.split(|&b| b == b' ').find(|f| !f.is_empty())?;
    std::str::from_utf8(field).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_commits_and_tags_are_read_as_git_reads_them() {
        let a = Oid([1; 20]);
        let b = Oid([2; 20]);
        let mut data = Vec::new();
        for (mode, name, oid) in [
            (&b"100644"[..], &b"a.txt"[..], a),
            (b"100755", b"run", a),
            (b"100664", b"old", a),
            (b"100744", b"own", a),
            (b"120000", b"link", b),
            (b"40000", b"dir", b),
            (b"160000", b"sub", b),
        ] {
            data.extend_from_slice(mode);
            data.push(b' ');
            data.extend_from_slice(name);
            data.push(0);
            data.extend_from_slice(&oid.0);
        }
        let modes: Vec<Mode> = tree(&data).unwrap().iter().map(|e| e.mode).collect();
        assert_eq!(
            modes,
            [
                Mode::File,
                Mode::Executable,
                Mode::File,
                Mode::Executable,
                Mode::Symlink,
                Mode::Tree,
                Mode::Gitlink
            ]
        );
        assert!(tree(b"100644 ../x\0aaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(tree(b"100644 x\0short").is_err());
        assert!(tree(b"999999 x\0aaaaaaaaaaaaaaaaaaaa").is_err());

        let c = commit(
            b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
              parent ce013625030ba8dba906f756967f9e9ca394464a\n\
              author A <a@x> 1700000000 +0000\n\
              committer C <c@x> 1700000100 -0130\n\
              gpgsig -----BEGIN PGP SIGNATURE-----\n \n -----END PGP SIGNATURE-----\n\
              \n\
              message\n",
        )
        .unwrap();
        assert_eq!(c.tree.hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.committed, 1_700_000_100);
        assert!(commit(b"author A <a@x> 1 +0000\n\nm").is_err());

        let t = tag(b"object ce013625030ba8dba906f756967f9e9ca394464a\ntype commit\ntag v1\n\nm\n").unwrap();
        assert_eq!(t.kind, Kind::Commit);
    }
}
