//! The host's path roots a run's domains are given (D235): top-level directories beneath
//! which what is plugged into a domain as it runs lies, at its host path. The host names
//! each as a setup entry, and shards-init provisions every domain with each as it starts.

use alloc::vec::Vec;

/// `plug-root=/<name>`: one of the host's path roots.
pub const ROOT_KEY: &[u8] = b"plug-root=";

/// The top-level directories shards-init mounts over in a domain's view, which no root may
/// be: what is plugged beneath one would lie under init's own mounts there, or widen the
/// rights of those init makes writable (its Unix sockets' directories, under `/run`).
pub const REFUSED: [&[u8]; 6] = [b"proc", b"sys", b"dev", b"tmp", b"etc", b"run"];

/// The name of the root `path` names (`/Users` names `Users`), where it may be one: a
/// single component past `/`, not `.` or `..`, without a NUL, and none of [`REFUSED`].
pub fn root_name(path: &[u8]) -> Option<&[u8]> {
    path.strip_prefix(b"/").filter(|n| {
        !n.is_empty()
            && !n.contains(&b'/')
            && !n.contains(&0)
            && !matches!(*n, b"." | b"..")
            && !REFUSED.contains(n)
    })
}

/// Why a path is no place a domain is given something at (D235).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outside {
    /// It is not absolute.
    Relative,
    /// It has an empty, `.` or `..` component.
    Unplain,
    /// It lies strictly beneath none of the run's roots.
    Rootless,
}

/// The components of `dst` where it lies strictly beneath one of `roots`, each a root's
/// name (`Users`): absolute, each component plain, its first a root, and at least one more.
/// A mount at a root itself would hide the directory Landlock's rule for it is tied to,
/// which a path walk then skips (security/landlock/fs.c, "Ignores hidden mount points"), so
/// what is plugged in lies beneath one.
pub fn beneath<'a>(dst: &'a [u8], roots: &[&[u8]]) -> Result<Vec<&'a [u8]>, Outside> {
    let parts: Vec<&[u8]> = dst
        .strip_prefix(b"/")
        .ok_or(Outside::Relative)?
        .split(|&b| b == b'/')
        .collect();
    if parts.iter().any(|p| matches!(*p, b"" | b"." | b"..")) {
        return Err(Outside::Unplain);
    }
    match parts.first() {
        Some(root) if parts.len() > 1 && roots.contains(root) => Ok(parts),
        _ => Err(Outside::Rootless),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root is one top-level directory, never one of init's own.
    #[test]
    fn a_root_is_one_top_level_directory_not_inits() {
        assert_eq!(root_name(b"/Users"), Some(&b"Users"[..]));
        assert_eq!(root_name(b"/mnt"), Some(&b"mnt"[..]));
        for wrong in [
            &b"Users"[..],
            b"/",
            b"/Users/ada",
            b"/Users/",
            b"/.",
            b"/..",
            b"/a\0b",
            b"/proc",
            b"/sys",
            b"/dev",
            b"/tmp",
            b"/etc",
            b"/run",
        ] {
            assert_eq!(root_name(wrong), None, "{wrong:?}");
        }
    }

    /// What a domain is given lies strictly beneath a root, by a plain absolute path.
    #[test]
    fn a_plug_lies_strictly_beneath_a_root() {
        let roots: [&[u8]; 2] = [b"Users", b"Volumes"];
        assert_eq!(
            beneath(b"/Users/ada/proj", &roots),
            Ok(alloc::vec![&b"Users"[..], b"ada", b"proj"])
        );
        assert_eq!(
            beneath(b"/Volumes/x", &roots),
            Ok(alloc::vec![&b"Volumes"[..], b"x"])
        );
        assert_eq!(beneath(b"/home/ada", &roots), Err(Outside::Rootless));
        assert_eq!(beneath(b"/Users", &roots), Err(Outside::Rootless));
        assert_eq!(beneath(b"/", &roots), Err(Outside::Unplain));
        assert_eq!(beneath(b"Users/ada", &roots), Err(Outside::Relative));
        for unplain in [&b"/Users/../etc"[..], b"/Users/./x", b"/Users//x", b"/Users/x/"] {
            assert_eq!(beneath(unplain, &roots), Err(Outside::Unplain), "{unplain:?}");
        }
        assert_eq!(beneath(b"/Users/ada", &[]), Err(Outside::Rootless));
    }
}
