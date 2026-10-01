//! Git sources as BuildKit reads them: `gitutil.ParseURL` (http, https, ssh and git URLs,
//! and scp-style `user@host:path`) and the Dockerfile's `dfgitutil.ParseGitRef`, which
//! says whether `ADD`'s source is a git repository, and which ref, subdirectory and
//! options it names.

use crate::go;
use crate::url::{self, Userinfo, Values};

/// `gitutil.GitURL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUrl {
    pub scheme: Vec<u8>,
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    pub user: Option<Userinfo>,
    pub query: Option<Values>,
    /// The fragment's ref and subdirectory.
    pub opts: Option<(Vec<u8>, Vec<u8>)>,
    /// The URL without its query and fragment.
    pub remote: Vec<u8>,
}

/// Why a source is no git URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// No protocol, and no scp-style form.
    UnknownProtocol,
    /// A protocol git does not speak, or a URL that does not parse.
    Other(Vec<u8>),
}

/// `parseOpts`: `ref[:subdir]`, the subdirectory cleaned and relative.
fn parse_opts(fragment: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if fragment.is_empty() {
        return None;
    }
    let (r, sub) = match fragment.iter().position(|&b| b == b':') {
        Some(i) => (go::head(fragment, i), go::tail(fragment, i + 1)),
        None => (fragment, &b""[..]),
    };
    let sub = go::join(&[b"/", sub]);
    let sub = sub.strip_prefix(b"/").unwrap_or(&sub).to_vec();
    Some((r.to_vec(), sub))
}

/// `^[a-zA-Z0-9]+://`: the protocol, lowercased.
fn protocol(remote: &[u8]) -> Option<Vec<u8>> {
    let n = remote.iter().take_while(|c| c.is_ascii_alphanumeric()).count();
    (n > 0 && go::tail(remote, n).starts_with(b"://")).then(|| go::head(remote, n).to_ascii_lowercase())
}

/// `gitutil.ParseURL`.
pub fn parse_url(remote: &[u8]) -> Result<GitUrl, UrlError> {
    if let Some(proto) = protocol(remote) {
        if !matches!(proto.as_slice(), b"http" | b"https" | b"ssh" | b"git") {
            return Err(UrlError::Other(
                [proto.as_slice(), b": invalid protocol"].concat(),
            ));
        }
        let u = url::parse(remote).map_err(UrlError::Other)?;
        return Ok(from_url(&u));
    }
    match parse_scp(remote) {
        Some(g) => Ok(g),
        None => Err(UrlError::UnknownProtocol),
    }
}

/// `gitutil.FromURL`.
pub fn from_url(u: &url::Url) -> GitUrl {
    let mut without = u.clone();
    without.fragment.clear();
    without.raw_fragment.clear();
    without.raw_query.clear();
    let q = u.query();
    GitUrl {
        scheme: u.scheme.clone(),
        host: u.host.clone(),
        path: u.path.clone(),
        user: u.user.clone(),
        query: (!q.is_empty()).then_some(q),
        opts: parse_opts(&u.fragment),
        remote: without.string(),
    }
}

/// `sshutil.ParseSCPStyleURL`, then `fromSCPStyleURL`:
/// `^([a-zA-Z0-9-_]+)@([a-zA-Z0-9-.]+):(.*?)(?:\?(.*?))?(?:#(.*))?$`.
fn parse_scp(raw: &[u8]) -> Option<GitUrl> {
    let at = raw.iter().position(|&b| b == b'@')?;
    let user = go::head(raw, at);
    if user.is_empty()
        || !user
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return None;
    }
    let rest = go::tail(raw, at + 1);
    let colon = rest.iter().position(|&b| b == b':')?;
    let host = go::head(rest, colon);
    if host.is_empty()
        || !host
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.'))
    {
        return None;
    }
    let tail = go::tail(rest, colon + 1);
    // `.` matches no newline.
    if tail.contains(&b'\n') {
        return None;
    }
    // The path ends at the first `?` or `#`; the query at the first `#` after it.
    let end = tail
        .iter()
        .position(|&b| b == b'?' || b == b'#')
        .unwrap_or(tail.len());
    let path = go::head(tail, end);
    let (query, fragment) = match tail.get(end) {
        Some(b'?') => {
            let after = go::tail(tail, end + 1);
            match after.iter().position(|&b| b == b'#') {
                Some(h) => (go::head(after, h), go::tail(after, h + 1)),
                None => (after, &b""[..]),
            }
        }
        Some(b'#') => (&b""[..], go::tail(tail, end + 1)),
        _ => (&b""[..], &b""[..]),
    };
    let vals = if query.is_empty() {
        Values::new()
    } else {
        let (v, err) = url::parse_query(query);
        if err.is_some() {
            return None;
        }
        v
    };
    let userinfo = Userinfo {
        username: user.to_vec(),
        password: None,
    };
    // SCPStyleURL.String without query and fragment.
    let remote = [url::userinfo_string(&userinfo).as_slice(), b"@", host, b":", path].concat();
    Some(GitUrl {
        scheme: b"ssh".to_vec(),
        host: host.to_vec(),
        path: path.to_vec(),
        user: Some(userinfo),
        query: (!vals.is_empty()).then_some(vals),
        opts: parse_opts(fragment),
        remote,
    })
}

/// `dfgitutil.GitRef`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitRef {
    pub remote: Vec<u8>,
    pub short_name: Vec<u8>,
    pub reference: Vec<u8>,
    pub checksum: Vec<u8>,
    pub subdir: Vec<u8>,
    /// `github.com/...` without a protocol: read as a git source, deprecated.
    pub indistinguishable_from_local: bool,
    pub unencrypted_tcp: bool,
    pub keep_git_dir: Option<bool>,
    pub submodules: Option<bool>,
    pub mtime: Vec<u8>,
    pub fetch_by_commit: bool,
}

/// What `ParseGitRef` makes of a source: a git ref; or no git source, and whether it was
/// one that failed (`isGit`), with the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Git(GitRef),
    NotGit,
    BadGit(Vec<u8>),
}

/// `dfgitutil.ParseGitRef`.
pub fn parse_git_ref(source: &[u8]) -> Parsed {
    let mut res = GitRef::default();
    let remote = if source.starts_with(b"./") || source.starts_with(b"../") {
        return Parsed::NotGit;
    } else if source.starts_with(b"github.com/") {
        res.indistinguishable_from_local = true;
        let Ok(mut u) = url::parse(source) else {
            return Parsed::NotGit;
        };
        u.scheme = b"https".to_vec();
        from_url(&u)
    } else {
        let Ok(remote) = parse_url(source) else {
            return Parsed::NotGit;
        };
        if matches!(remote.scheme.as_slice(), b"http" | b"git") {
            res.unencrypted_tcp = true;
        }
        if matches!(remote.scheme.as_slice(), b"http" | b"https") && !remote.path.ends_with(b".git") {
            return Parsed::NotGit;
        }
        remote
    };
    res.remote = remote.remote.clone();
    if res.indistinguishable_from_local
        && let Some(i) = url_find(&res.remote, b"://")
    {
        res.remote = go::tail(&res.remote, i + 3).to_vec();
    }
    if let Some((r, sub)) = &remote.opts {
        res.reference = r.clone();
        res.subdir = sub.clone();
    }
    let last = res.remote.rsplit(|&b| b == b'/').next().unwrap_or_default();
    res.short_name = last.strip_suffix(b".git").unwrap_or(last).to_vec();
    match load_query(&mut res, remote.query.as_ref()) {
        Ok(()) => Parsed::Git(res),
        Err(e) => Parsed::BadGit(e),
    }
}

fn url_find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `GitRef.loadQuery`, the keys in order (where Go's map order is random).
fn load_query(gf: &mut GitRef, query: Option<&Values>) -> Result<(), Vec<u8>> {
    let Some(query) = query else {
        return Ok(());
    };
    let q = |b: &[u8]| go::quote(b);
    let (mut tag, mut branch) = (Vec::new(), Vec::new());
    let flag = |k: &[u8], v: Option<&Vec<u8>>| -> Result<bool, Vec<u8>> {
        match v {
            None => Ok(true),
            Some(v) => go::parse_bool(v).ok_or_else(|| {
                format!("invalid {} value: {}", String::from_utf8_lossy(k), q(v)).into_bytes()
            }),
        }
    };
    for (k, vs) in query {
        let v: Option<&Vec<u8>> = match vs.as_slice() {
            [] => None,
            [one] if one.is_empty() => None,
            [one] => Some(one),
            _ => return Err(format!("query {} has multiple values", q(k)).into_bytes()),
        };
        if v.is_none() && !matches!(k.as_slice(), b"submodules" | b"keep-git-dir" | b"fetch-by-commit") {
            return Err(format!("query {} has no value", q(k)).into_bytes());
        }
        let val = v.cloned().unwrap_or_default();
        match k.as_slice() {
            b"ref" => {
                if !gf.reference.is_empty() && gf.reference != val {
                    return Err(format!("ref conflicts: {} vs {}", q(&gf.reference), q(&val)).into_bytes());
                }
                gf.reference = val;
            }
            b"tag" => tag = val,
            b"branch" => branch = val,
            b"subdir" => {
                if !gf.subdir.is_empty() && gf.subdir != val {
                    return Err(format!("subdir conflicts: {} vs {}", q(&gf.subdir), q(&val)).into_bytes());
                }
                gf.subdir = val;
            }
            b"checksum" | b"commit" => gf.checksum = val,
            b"keep-git-dir" => gf.keep_git_dir = Some(flag(k, v)?),
            b"submodules" => gf.submodules = Some(flag(k, v)?),
            b"mtime" => match val.as_slice() {
                b"checkout" | b"commit" => gf.mtime = val,
                _ => {
                    return Err(format!(
                        "invalid mtime value: {} (must be \"checkout\" or \"commit\")",
                        q(&val)
                    )
                    .into_bytes());
                }
            },
            b"fetch-by-commit" => gf.fetch_by_commit = flag(k, v)?,
            _ => return Err(format!("unexpected query {}", q(k)).into_bytes()),
        }
    }
    if !tag.is_empty() {
        let tag = if tag.starts_with(b"refs/tags/") {
            tag
        } else {
            [b"refs/tags/".as_slice(), &tag].concat()
        };
        if !gf.reference.is_empty() && gf.reference != tag {
            return Err(format!("ref conflicts: {} vs {}", q(&gf.reference), q(&tag)).into_bytes());
        }
        gf.reference = tag.clone();
        if !branch.is_empty() {
            return Err(b"branch conflicts with tag".to_vec());
        }
    }
    if !branch.is_empty() {
        let branch = if branch.starts_with(b"refs/heads/") {
            branch
        } else {
            [b"refs/heads/".as_slice(), &branch].concat()
        };
        if !gf.reference.is_empty() && gf.reference != branch {
            return Err(format!("ref conflicts: {} vs {}", q(&gf.reference), q(&branch)).into_bytes());
        }
        gf.reference = branch;
    }
    Ok(())
}
