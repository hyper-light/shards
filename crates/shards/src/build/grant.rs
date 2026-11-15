//! An Agentfile directive's fetch held to its grant (D54, default deny): an `AGENT`,
//! `HARNESS`, `SKILL` or `MCP` source reaches the one origin it names (scheme, host and
//! port) and each origin its `--redirect` grants, in the source's own scheme at the port
//! the grant names or the scheme's own, and nothing else. Every connection is checked
//! before it is made, each redirect's hop too. A source without the planner's mark (an
//! `ADD`'s) is fetched as BuildKit fetches it.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use shards_registry::url::{Scheme, Url};

/// Why a fetch was refused its next connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// A redirect to an origin the source neither names nor grants.
    RedirectNotGranted { from: String, to: String },
    /// A remote beyond the source's own, a Git submodule's, at an origin it does not grant.
    NotGranted { source: String, to: String },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::RedirectNotGranted { from, to } => write!(
                f,
                "redirected from {from} to {to}, which the source does not grant: name it with --redirect=HOST[:PORT]"
            ),
            Refused::NotGranted { source, to } => write!(
                f,
                "{to} is not granted by the source {source}: name its origin with --redirect=HOST[:PORT]"
            ),
        }
    }
}

/// The origins a directive's source grants beyond its own: each `--redirect`'s host,
/// lowercased, and its port if it names one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redirects(Vec<(String, Option<u16>)>);

/// The grant `attrs` hold (`shards.grant`), where the source is an Agentfile directive's:
/// none for an `ADD`'s.
pub fn of(attrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<Option<Redirects>, String> {
    let Some(v) = attrs.get(shards_agentfile::agentfile::GRANT_ATTR) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for g in v.split(|&b| b == b',').filter(|g| !g.is_empty()) {
        let (host, port) = shards_agentfile::agentfile::redirect_origin(g)
            .ok_or_else(|| format!("a grant of no origin: {:?}", String::from_utf8_lossy(g)))?;
        out.push((String::from_utf8_lossy(&host).into_owned(), port));
    }
    Ok(Some(Redirects(out)))
}

/// One fetch's hops, held to its grant: the first URL's origin, then each the redirects
/// grant.
pub struct Hops {
    first: Url,
    redirects: Redirects,
    /// The last hop allowed: what the next one is a redirect from.
    last: RefCell<Option<Url>>,
    refused: RefCell<Option<Refused>>,
}

impl Hops {
    pub fn new(first: Url, redirects: Redirects) -> Hops {
        Hops {
            first,
            redirects,
            last: RefCell::new(None),
            refused: RefCell::new(None),
        }
    }

    /// Whether the connection to `to` is granted; the refusal kept where it is not, for
    /// [`refused`](Self::refused).
    pub fn check(&self, to: &Url) -> Result<(), Refused> {
        let granted = to.origin() == self.first.origin()
            || (to.scheme() == self.first.scheme()
                && self.redirects.0.iter().any(|(host, port)| {
                    *host == to.host() && port.unwrap_or_else(|| default_port(to.scheme())) == to.port()
                }));
        if granted {
            *self.last.borrow_mut() = Some(to.clone());
            return Ok(());
        }
        let from = self.last.borrow().as_ref().unwrap_or(&self.first).origin();
        let refused = Refused::RedirectNotGranted {
            from,
            to: to.origin(),
        };
        *self.refused.borrow_mut() = Some(refused.clone());
        Err(refused)
    }

    /// The refusal that ended the fetch, if one did.
    pub fn refused(&self) -> Option<Refused> {
        self.refused.borrow().clone()
    }
}

/// Whether a Git source `source`, granted `redirects`, may open remote `to` (D54): an http(s)
/// remote at the source's origin or one it grants; any other at the source's own scheme,
/// host and port.
pub fn remote(source: &str, redirects: &Redirects, to: &str) -> Result<(), Refused> {
    let refused = || Refused::NotGranted {
        source: super::http::shown(source),
        to: super::http::shown(to),
    };
    let parse = |u: &str| {
        super::http::userinfo(u)
            .ok()
            .and_then(|(t, _)| Url::parse(&t).ok())
    };
    match (parse(source), parse(to)) {
        (Some(first), Some(to)) => Hops::new(first, redirects.clone())
            .check(&to)
            .map_err(|_| refused()),
        _ if remote_origin(source).is_some() && remote_origin(source) == remote_origin(to) => Ok(()),
        _ => Err(refused()),
    }
}

/// A Git remote's origin for a comparison: its scheme, and its host and port lowercased
/// without its user; an scp-like `user@host:path` is SSH's.
fn remote_origin(url: &str) -> Option<String> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => {
            let (authority, _) = url.split_once(':')?;
            return Some(format!(
                "ssh://{}",
                authority.rsplit('@').next()?.to_ascii_lowercase()
            ));
        }
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    Some(format!("{scheme}://{}", host.to_ascii_lowercase()))
}

fn default_port(s: Scheme) -> u16 {
    match s {
        Scheme::Http => 80,
        Scheme::Https => 443,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hops(first: &str, grant: &str) -> Hops {
        let attrs = BTreeMap::from([(b"shards.grant".to_vec(), grant.as_bytes().to_vec())]);
        Hops::new(Url::parse(first).unwrap(), of(&attrs).unwrap().unwrap())
    }

    #[test]
    fn a_fetch_reaches_its_own_origin_and_what_it_grants_alone() {
        let h = hops(
            "https://github.com/o/r/releases/download/v1/a.tgz",
            "objects.githubusercontent.com",
        );
        assert!(
            h.check(&Url::parse("https://github.com/o/r/releases/download/v1/a.tgz").unwrap())
                .is_ok()
        );
        assert!(
            h.check(&Url::parse("https://objects.githubusercontent.com/x?sig=1").unwrap())
                .is_ok()
        );
        // Another port, a downgrade, another host: none granted.
        for to in [
            "https://objects.githubusercontent.com:8443/x",
            "http://objects.githubusercontent.com/x",
            "https://evil.example/x",
            "https://sub.objects.githubusercontent.com/x",
        ] {
            let refused = h.check(&Url::parse(to).unwrap()).unwrap_err();
            assert_eq!(
                refused,
                Refused::RedirectNotGranted {
                    from: "https://objects.githubusercontent.com:443".into(),
                    to: Url::parse(to).unwrap().origin(),
                }
            );
            assert_eq!(h.refused(), Some(refused));
        }
        let shown = h
            .check(&Url::parse("https://evil.example/x").unwrap())
            .unwrap_err()
            .to_string();
        assert_eq!(
            shown,
            "redirected from https://objects.githubusercontent.com:443 to https://evil.example:443, which the source does not grant: name it with --redirect=HOST[:PORT]"
        );
    }

    #[test]
    fn a_grant_names_its_port_or_the_schemes_own() {
        let h = hops("http://127.0.0.1:8080/a.tgz", "127.0.0.1:9090,mirror.example");
        assert!(h.check(&Url::parse("http://127.0.0.1:9090/b").unwrap()).is_ok());
        assert!(h.check(&Url::parse("http://mirror.example/b").unwrap()).is_ok());
        assert!(
            h.check(&Url::parse("http://mirror.example:8080/b").unwrap())
                .is_err()
        );
        assert!(h.check(&Url::parse("http://127.0.0.1:9091/b").unwrap()).is_err());
    }

    #[test]
    fn a_git_source_opens_only_the_remotes_it_grants() {
        let none = Redirects::default();
        let one = Redirects(vec![("mirror.example".into(), None)]);
        assert!(remote("https://git.example/a.git", &none, "https://git.example/b.git").is_ok());
        assert!(
            remote(
                "https://u:p@git.example/a.git",
                &none,
                "https://git.example/b.git"
            )
            .is_ok()
        );
        assert!(remote("https://git.example/a.git", &one, "https://mirror.example/c.git").is_ok());
        assert_eq!(
            remote(
                "https://u:p@git.example/a.git",
                &none,
                "https://other.example/c.git"
            ),
            Err(Refused::NotGranted {
                source: "https://u:***@git.example/a.git".into(),
                to: "https://other.example/c.git".into(),
            })
        );
        assert!(remote("git@git.example:o/a.git", &none, "ssh://git@git.example/o/b.git").is_ok());
        assert!(remote("git@git.example:o/a.git", &none, "git@other.example:o/b.git").is_err());
        assert!(remote("git@git.example:o/a.git", &none, "https://git.example/o/b.git").is_err());
        assert!(remote("git://git.example/a", &none, "git://git.example/b").is_ok());
    }

    #[test]
    fn an_adds_source_carries_no_grant() {
        assert_eq!(of(&BTreeMap::new()).unwrap(), None);
        let empty = BTreeMap::from([(b"shards.grant".to_vec(), Vec::new())]);
        assert_eq!(of(&empty).unwrap(), Some(Redirects::default()));
    }
}
