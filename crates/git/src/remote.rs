//! A remote repository, as BuildKit's git source reaches it (moby/buildkit v0.28.1
//! source/git/source.go): its default branch, a ref resolved as `ls-remote` lists it,
//! and one commit fetched one deep, by protocol v2 over a [`Transport`].

use std::io::Read;

use crate::Oid;
use crate::pack::Pack;
use crate::protocol::{self, Capabilities, Ref};

/// How a remote is reached: smart HTTP's two requests, or git's own protocol's.
pub trait Transport {
    /// The capability advertisement (`GET info/refs?service=git-upload-pack`).
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String>;
    /// A command's answer (`POST git-upload-pack`).
    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String>;
}

/// What a fetch may take: the largest pack, and the largest object in it.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub pack: usize,
    pub object: usize,
}

/// A remote that speaks protocol v2.
#[derive(Debug)]
pub struct Remote<T> {
    transport: T,
    caps: Capabilities,
    agent: String,
}

/// What a ref resolves to: its commit, the annotated tag it was peeled from, and the ref
/// that matched (none for a commit's name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub commit: Oid,
    pub tag: Option<Oid>,
    pub name: Option<Vec<u8>>,
}

impl<T: Transport> Remote<T> {
    /// Asks `transport` what it can do; a server that does not speak version 2 is refused.
    pub fn open(transport: T, agent: &str) -> Result<Remote<T>, String> {
        let caps = protocol::advertisement(transport.advertise()?)?
            .ok_or("the server does not speak git protocol version 2")?;
        if caps.value(b"ls-refs").is_none() || caps.value(b"fetch").is_none() {
            return Err("the server cannot list refs and fetch".into());
        }
        Ok(Remote {
            transport,
            caps,
            agent: agent.to_string(),
        })
    }

    /// Every ref, with HEAD's target and tags peeled, as `ls-remote` lists them.
    pub fn refs(&self) -> Result<Vec<Ref>, String> {
        protocol::refs(self.transport.command(&protocol::ls_refs(&self.agent, &[])?)?)
    }

    /// `name` resolved as BuildKit resolves an ADD's ref (source.go resolveMetadata): the
    /// default branch where it is empty (HEAD's target, `refs/heads/...`); a full commit
    /// name as it is, without asking; otherwise `refs/NAME` exactly, then the branch
    /// `refs/heads/NAME`, then the tag `refs/tags/NAME`. An annotated tag gives the commit
    /// it names. `Ok(None)`: the repository has no such ref.
    pub fn resolve(&self, name: &str) -> Result<Option<Resolved>, String> {
        if is_commit_name(name) {
            let commit = Oid::parse(name.as_bytes()).ok_or("a bad commit name")?;
            return Ok(Some(Resolved {
                commit,
                tag: None,
                name: None,
            }));
        }
        let refs = self.refs()?;
        let wanted: Vec<Vec<u8>> = if name.is_empty() {
            let head = refs.iter().find(|r| r.name == b"HEAD");
            match head
                .and_then(|h| h.target.clone())
                .filter(|t| t.starts_with(b"refs/heads/"))
            {
                Some(target) => vec![target],
                None => return Err("the repository has no default branch".into()),
            }
        } else {
            let n = name.as_bytes();
            let mut w = vec![[b"refs/".as_slice(), n.strip_prefix(b"refs/").unwrap_or(n)].concat()];
            if !n.starts_with(b"refs/") {
                w.push([b"refs/heads/".as_slice(), n].concat());
                w.push([b"refs/tags/".as_slice(), n].concat());
            }
            w
        };
        for want in &wanted {
            if let Some(r) = refs.iter().find(|r| &r.name == want) {
                return Ok(Some(match r.peeled {
                    Some(commit) => Resolved {
                        commit,
                        tag: Some(r.oid),
                        name: Some(r.name.clone()),
                    },
                    None => Resolved {
                        commit: r.oid,
                        tag: None,
                        name: Some(r.name.clone()),
                    },
                }));
            }
        }
        Ok(None)
    }

    /// The pack of `commits`, one commit deep, as `fetch --depth=1` takes them.
    pub fn fetch(&self, commits: &[Oid], limits: Limits) -> Result<Pack, String> {
        let body = protocol::fetch(&self.agent, commits, 1, &self.caps)?;
        let got = protocol::fetched(self.transport.command(&body)?, limits.pack)?;
        Pack::read(got.pack, limits.object)
    }

    /// The pack of every ref's history, whole, as BuildKit's `git fetch --tags origin`
    /// takes a repository whose server will not send a commit by its name alone.
    pub fn fetch_whole(&self, limits: Limits) -> Result<Pack, String> {
        let mut wants: Vec<Oid> = Vec::new();
        for r in self.refs()? {
            if !wants.contains(&r.oid) {
                wants.push(r.oid);
            }
        }
        if wants.is_empty() {
            return Err("the repository has no refs".into());
        }
        let body = protocol::fetch(&self.agent, &wants, 0, &self.caps)?;
        let got = protocol::fetched(self.transport.command(&body)?, limits.pack)?;
        Pack::read(got.pack, limits.object)
    }
}

/// IsCommitSHA (util/gitutil/git_commit.go): exactly 40 (SHA-1) or 64 (SHA-256)
/// lowercase hexadecimal digits; anything shorter is a ref's name.
pub fn is_commit_name(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_names_are_told_from_refs_as_buildkit_tells_them() {
        assert!(is_commit_name(&"a".repeat(40)));
        assert!(is_commit_name(&"0".repeat(64)));
        assert!(!is_commit_name(&"A".repeat(40)));
        assert!(!is_commit_name("84b8029"));
        assert!(!is_commit_name("main"));
    }
}
