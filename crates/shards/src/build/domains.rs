//! The build's isolation checks (AGENTFILE_ARCH.md §9.2, architecture.md D55): before an
//! image with agents or harnesses is exported, every path is placed in its domain (an
//! agent's or harness's directory and its `.d` grants beside it, or the system), and the
//! build fails, saying each path and why, where:
//!
//! - a symlink in a domain resolves, inside the image, outside it, by an absolute or a
//!   relative target, through other symlinks or not;
//! - a hard link's names lie in two domains, which no path rule can see;
//! - a domain holds a device node, FIFO or socket, or a file with set-user-ID or
//!   set-group-ID bits or file capabilities (`security.capability`), all of which
//!   `COPY --from` carries over and `--chmod` sets.

use std::collections::BTreeMap;

use shards_build::vfs::Fs;
use shards_dockerfile::plan::DomainDir;
use shards_image::erofs::{Kind, NodeId, Tree};

/// How many symlinks a resolution follows before it gives up, as Linux's (MAXSYMLINKS).
const MAX_LINKS: usize = 40;
/// How many findings an error lists before it counts the rest.
const SHOWN: usize = 20;

fn components(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect()
}

/// Which domain a path lies in: its index in `roots`, each root's components.
fn domain_of(path: &[Vec<u8>], roots: &[(Vec<Vec<u8>>, usize)]) -> Option<usize> {
    roots
        .iter()
        .filter(|(r, _)| path.len() >= r.len() && path.get(..r.len()) == Some(r.as_slice()))
        .max_by_key(|(r, _)| r.len())
        .map(|&(_, d)| d)
}

/// Where `path` (components, from the root) leads in `tree`, every symlink on the way
/// followed as the kernel follows them, `..` never above the root; past a name the tree
/// lacks, the rest as written.
fn resolve(tree: &Tree, path: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut pending: Vec<Vec<u8>> = path.into_iter().rev().collect();
    let mut at: Vec<(Vec<u8>, NodeId)> = Vec::new();
    let mut links = 0usize;
    while let Some(c) = pending.pop() {
        if c == b".." {
            at.pop();
            continue;
        }
        let dir = at.last().map_or(Tree::ROOT, |&(_, id)| id);
        let Some(id) = tree.child(dir, &c) else {
            // Not there: what is left is where it would be.
            let mut out: Vec<Vec<u8>> = at.into_iter().map(|(n, _)| n).collect();
            out.push(c);
            while let Some(rest) = pending.pop() {
                if rest == b".." {
                    out.pop();
                } else {
                    out.push(rest);
                }
            }
            return out;
        };
        match tree.node(id).map(|n| &n.kind) {
            Some(Kind::Symlink(target)) if links < MAX_LINKS => {
                links += 1;
                if target.starts_with(b"/") {
                    at.clear();
                }
                for t in components(target).into_iter().rev() {
                    pending.push(t);
                }
            }
            _ => at.push((c, id)),
        }
    }
    at.into_iter().map(|(n, _)| n).collect()
}

fn shown(path: &[Vec<u8>]) -> String {
    let mut s = String::new();
    for c in path {
        s.push('/');
        s.push_str(&String::from_utf8_lossy(c));
    }
    if s.is_empty() { "/".into() } else { s }
}

/// Checks `fs` against the domains of `domains`; nothing to check without any.
pub fn check(fs: &Fs, domains: &[DomainDir]) -> Result<(), String> {
    if domains.is_empty() {
        return Ok(());
    }
    let tree = fs.tree();
    let mut roots = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let dir = components(&d.dir);
        let mut grants = dir.clone();
        if let Some(last) = grants.last_mut() {
            last.extend_from_slice(b".d");
        }
        roots.push((dir, i));
        roots.push((grants, i));
    }
    let what = |d: Option<usize>| match d.and_then(|i| domains.get(i)) {
        Some(d) => format!(
            "{} {}",
            if d.harness { "the harness" } else { "the agent" },
            String::from_utf8_lossy(&d.name)
        ),
        None => "the system".into(),
    };
    // Each domain's user, as the microVM's init numbers them: agents in the order
    // declared, then harnesses (shards_abi::DOMAIN_FIRST_ID).
    let mut users: Vec<u32> = vec![0; domains.len()];
    let mut n = 0u32;
    for harness in [false, true] {
        for (i, _) in domains.iter().enumerate().filter(|(_, d)| d.harness == harness) {
            if let Some(u) = users.get_mut(i) {
                *u = shards_abi::DOMAIN_FIRST_ID.saturating_add(n);
            }
            n = n.saturating_add(1);
        }
    }
    let user_of = |id: u32| users.iter().position(|&u| u == id);
    let mut findings = Vec::new();
    let mut names: BTreeMap<NodeId, Vec<Vec<Vec<u8>>>> = BTreeMap::new();
    let mut stack: Vec<(Vec<Vec<u8>>, NodeId)> = vec![(Vec::new(), Tree::ROOT)];
    while let Some((path, id)) = stack.pop() {
        let Some(node) = tree.node(id) else { continue };
        let domain = domain_of(&path, &roots);
        if let Kind::Dir(_) = node.kind {
            for (name, child) in tree.entries(id).into_iter().rev() {
                let mut p = path.clone();
                p.push(name.to_vec());
                stack.push((p, child));
            }
        } else {
            names.entry(id).or_default().push(path.clone());
        }
        // A path a domain's user owns, outside that domain or in another's: what its
        // agent could change or read that no grant gives it.
        for (what_id, owner) in [("user", node.meta.uid), ("group", node.meta.gid)] {
            if let Some(k) = user_of(owner)
                && domain != Some(k)
            {
                findings.push(format!(
                    "{}: owned by {}'s {what_id} ({owner}), in {}",
                    shown(&path),
                    what(Some(k)),
                    match domain {
                        Some(_) => format!("{}'s domain", what(domain)),
                        None => "the system, outside its domain".to_string(),
                    }
                ));
            }
        }
        if domain.is_none() {
            continue;
        }
        let here = shown(&path);
        match &node.kind {
            Kind::CharDevice { .. } | Kind::BlockDevice { .. } => {
                findings.push(format!("{here}: a device node in {}'s domain", what(domain)));
            }
            Kind::Fifo => findings.push(format!("{here}: a FIFO in {}'s domain", what(domain))),
            Kind::Socket => findings.push(format!("{here}: a socket in {}'s domain", what(domain))),
            Kind::Symlink(target) => {
                let mut from = path.clone();
                from.pop();
                let lexical = if target.starts_with(b"/") {
                    components(target)
                } else {
                    let mut p = from.clone();
                    p.extend(components(target));
                    p
                };
                let to = resolve(tree, lexical);
                let lands = domain_of(&to, &roots);
                if lands != domain {
                    findings.push(format!(
                        "{here} -> {}: a symlink out of {}'s domain, to {}",
                        String::from_utf8_lossy(target),
                        what(domain),
                        what(lands)
                    ));
                }
            }
            _ => {}
        }
        if !matches!(node.kind, Kind::Dir(_) | Kind::Symlink(_)) {
            if node.meta.mode & 0o6000 != 0 {
                findings.push(format!(
                    "{here}: set-user-ID or set-group-ID bits ({:o}) in {}'s domain",
                    node.meta.mode,
                    what(domain)
                ));
            }
            if node.meta.xattrs.get(b"security.capability").is_some() {
                findings.push(format!("{here}: file capabilities in {}'s domain", what(domain)));
            }
        }
    }
    for paths in names.values().filter(|p| p.len() > 1) {
        let mut seen: Vec<Option<usize>> = paths.iter().map(|p| domain_of(p, &roots)).collect();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() > 1 {
            let list: Vec<String> = paths.iter().map(|p| shown(p)).collect();
            findings.push(format!(
                "{}: one file's hard links in {}",
                list.join(", "),
                seen.iter().map(|d| what(*d)).collect::<Vec<_>>().join(" and ")
            ));
        }
    }
    if findings.is_empty() {
        return Ok(());
    }
    let n = findings.len();
    let mut text = String::from("the image breaks its domains' isolation (AGENTFILE_ARCH.md §9.2):");
    for f in findings.iter().take(SHOWN) {
        text.push_str("\n  ");
        text.push_str(f);
    }
    if n > SHOWN {
        text.push_str(&format!("\n  and {} more", n - SHOWN));
    }
    Err(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shards_image::erofs::{DataRef, Dir, Meta, Node};

    fn dir(tree: &mut Tree, at: NodeId, name: &str) -> NodeId {
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind: Kind::Dir(Dir::default()),
                meta: Meta {
                    mode: 0o755,
                    ..Meta::default()
                },
            },
        )
        .unwrap()
    }

    fn node(tree: &mut Tree, at: NodeId, name: &str, kind: Kind, mode: u16) -> NodeId {
        tree.insert(
            at,
            name.as_bytes(),
            Node {
                kind,
                meta: Meta {
                    mode,
                    ..Meta::default()
                },
            },
        )
        .unwrap()
    }

    fn file() -> Kind {
        Kind::File {
            size: 0,
            data: DataRef { source: 0, offset: 0 },
        }
    }

    fn link(target: &str) -> Kind {
        Kind::Symlink(target.as_bytes().to_vec().into_boxed_slice())
    }

    /// An image with an agent `main` and a harness `drive`, and `add` its extra entries.
    fn checked(add: impl FnOnce(&mut Tree, NodeId, NodeId, NodeId)) -> Result<(), String> {
        let mut tree = Tree::new(Meta {
            mode: 0o755,
            ..Meta::default()
        });
        let etc = dir(&mut tree, Tree::ROOT, "etc");
        node(&mut tree, etc, "passwd", file(), 0o644);
        let agents = dir(&mut tree, Tree::ROOT, "agents");
        let main = dir(&mut tree, agents, "main");
        let grants = dir(&mut tree, agents, "main.d");
        node(&mut tree, main, "run", file(), 0o755);
        let harness = dir(&mut tree, Tree::ROOT, "harness");
        let drive = dir(&mut tree, harness, "drive");
        let _ = grants;
        add(&mut tree, main, drive, etc);
        let fs = Fs::new(tree, (0, 0));
        let domains = [
            DomainDir {
                name: b"main".to_vec(),
                harness: false,
                dir: b"/agents/main".to_vec(),
            },
            DomainDir {
                name: b"drive".to_vec(),
                harness: true,
                dir: b"/harness/drive".to_vec(),
            },
        ];
        check(&fs, &domains)
    }

    /// What a domain's user owns lies in its domain: not in the system (`--chown` to an
    /// agent's uid on /etc), nor in another domain; by uid or by gid. main is the first
    /// domain's user (200000), drive, the harness after the agents, the next.
    #[test]
    fn a_domains_user_owns_nothing_outside_it() {
        let owned = |tree: &mut Tree, at: NodeId, name: &str, uid: u32, gid: u32| {
            tree.insert(
                at,
                name.as_bytes(),
                Node {
                    kind: file(),
                    meta: Meta {
                        mode: 0o644,
                        uid,
                        gid,
                        ..Meta::default()
                    },
                },
            )
            .unwrap();
        };
        // Its own, in its domain: kept.
        assert_eq!(
            checked(|t, main, _, _| owned(t, main, "mine", 200_000, 200_000)),
            Ok(())
        );
        let e = checked(|t, _, _, etc| owned(t, etc, "shadow", 200_000, 0)).unwrap_err();
        assert!(
            e.contains(
                "/etc/shadow: owned by the agent main's user (200000), in the system, outside its domain"
            ),
            "{e}"
        );
        let e = checked(|t, main, _, _| owned(t, main, "theirs", 200_001, 0)).unwrap_err();
        assert!(
            e.contains(
                "/agents/main/theirs: owned by the harness drive's user (200001), in the agent main's domain"
            ),
            "{e}"
        );
        let e = checked(|t, _, _, etc| owned(t, etc, "group", 0, 200_000)).unwrap_err();
        assert!(
            e.contains("/etc/group: owned by the agent main's group (200000)"),
            "{e}"
        );
        // An ID past the domains' is no domain's.
        assert_eq!(checked(|t, _, _, etc| owned(t, etc, "other", 200_002, 0)), Ok(()));
    }

    #[test]
    fn domains_hold_nothing_that_reaches_past_them() {
        assert_eq!(checked(|_, _, _, _| {}), Ok(()));
        // Inside: kept.
        assert_eq!(
            checked(|t, main, _, _| {
                node(t, main, "ok", link("run"), 0o777);
            }),
            Ok(())
        );
        assert_eq!(
            checked(|t, main, _, _| {
                node(t, main, "abs", link("/agents/main/run"), 0o777);
            }),
            Ok(())
        );
        for (name, target, says) in [
            (
                "abs",
                "/etc/passwd",
                "/agents/main/abs -> /etc/passwd: a symlink out of the agent main's domain, to the system",
            ),
            (
                "rel",
                "../../etc",
                "/agents/main/rel -> ../../etc: a symlink out of the agent main's domain, to the system",
            ),
            (
                "other",
                "../../harness/drive",
                "/agents/main/other -> ../../harness/drive: a symlink out of the agent main's domain, to the harness drive",
            ),
        ] {
            let e = checked(|t, main, _, _| {
                node(t, main, name, link(target), 0o777);
            })
            .unwrap_err();
            assert!(e.contains(says), "{e}");
        }
        // Through another symlink, which itself stays in.
        let e = checked(|t, main, _, _| {
            node(t, main, "hop", link("."), 0o777);
            node(t, main, "out", link("hop/../../../etc"), 0o777);
        })
        .unwrap_err();
        assert!(e.contains("/agents/main/out -> hop/../../../etc"), "{e}");
        // A hard link across domains.
        let e = checked(|t, main, _, etc| {
            let id = t.child(etc, b"passwd").unwrap();
            t.link(main, b"passwd", id).unwrap();
        })
        .unwrap_err();
        assert!(
            e.contains("one file's hard links in the system and the agent main"),
            "{e}"
        );
        // What a domain may not hold.
        let e = checked(|t, _, drive, _| {
            node(t, drive, "dev", Kind::CharDevice { major: 1, minor: 3 }, 0o666);
            node(t, drive, "fifo", Kind::Fifo, 0o644);
            node(t, drive, "suid", file(), 0o4755);
        })
        .unwrap_err();
        assert!(
            e.contains("/harness/drive/dev: a device node in the harness drive's domain"),
            "{e}"
        );
        assert!(e.contains("/harness/drive/fifo: a FIFO"), "{e}");
        assert!(
            e.contains("/harness/drive/suid: set-user-ID or set-group-ID bits (4755)"),
            "{e}"
        );
        // The grants beside a domain are its own.
        let e = checked(|t, _, _, _| {
            let agents = t.child(Tree::ROOT, b"agents").unwrap();
            let grants = t.child(agents, b"main.d").unwrap();
            node(t, grants, "out", link("/etc"), 0o777);
        })
        .unwrap_err();
        assert!(e.contains("/agents/main.d/out -> /etc"), "{e}");
    }
}
