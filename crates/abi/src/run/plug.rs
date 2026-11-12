//! The host's path roots a run's domains are given (D235): top-level directories beneath
//! which what is plugged into a domain as it runs lies, at its host path. The host names
//! each as a setup entry, and shards-init provisions every domain with each as it starts.

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
}
