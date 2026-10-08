//! `ADD <git URL>`: BuildKit's git source (moby/buildkit v0.28.1 source/git/source.go,
//! measured against Docker Engine 29.3.1's BuildKit), fetched by shards' own client
//! (`shards_git`), so that no `git` need be beside `shards`. A ref resolves as
//! `ls-remote` lists it and BuildKit matches it; the commit is fetched one deep and its
//! tree checked out as `git checkout` writes it under BuildKit's umask: files 0644 or
//! 0755, symlinks, directories 0755, all root's; submodules too, one deep, recursively.
//!
//! Unlike BuildKit, by design (docs/design/architecture.md D48):
//! - every entry's mtime is the commit's committer time, where BuildKit's is the clock's
//!   at checkout: the same commit makes the same layer on every builder, as frontend
//!   1.27.1's `mtime=commit` asks of a build context;
//! - a submodule leaves no `.git` file naming a path inside the builder;
//! - what a ref checks out depends on that ref alone: BuildKit's shared repository lets a
//!   submodule of a ref fetched before into the checkout of one that has none;
//! - a checksum that matches neither an annotated tag nor its commit says both.
//! - over SSH (D69), a host is trusted only by the keys the user's known_hosts holds,
//!   where BuildKit trusts whatever keys it scans while planning.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

use shards_git::checkout::{self, Item};
use shards_git::object::{self, Kind as ObjectKind};
use shards_git::pack::Pack;
use shards_git::protocol::{ADVERTISEMENT_TYPE, REQUEST_TYPE, RESULT_TYPE, VERSION_2};
use shards_git::remote::{Limits, Remote, Resolved, Transport};
use shards_git::repo::{self, KeptRef, Tracked};
use shards_git::{Oid, config};
use shards_image::erofs::{Dir, Kind, Meta, Node};
use shards_registry::http::{Cancel, Client, Redirects, Request};
use shards_registry::url::Url;

use super::exec::{Exec, Ref};

/// What the plan says of a git source (llb.Git's identifier and attributes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The remote's URL, as the user wrote it (`git.fullurl`).
    pub url: String,
    /// The ref asked for; empty for the default branch.
    pub reference: String,
    /// The directory of the repository to take, without its leading `/`.
    pub subdir: String,
    pub keep_git_dir: bool,
    pub checksum: Option<String>,
    pub submodules: bool,
    /// The secrets that authorize its fetches: a token's (`git.authtokensecret`) and a
    /// whole header's (`git.authheadersecret`).
    pub auth_token: Option<String>,
    pub auth_header: Option<String>,
    /// The SSH agent its fetches over SSH authenticate with (`git.mountsshsock`).
    pub ssh_agent: Option<String>,
}

/// The source `identifier` (`git://HOST/PATH[#REF[:SUBDIR]]`) and `attrs` name.
pub fn source(identifier: &[u8], attrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<Source, String> {
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| "a git source not in UTF-8".to_string());
    let rest = identifier.strip_prefix(b"git://").ok_or("not a git source")?;
    let fragment = rest
        .iter()
        .position(|&b| b == b'#')
        .map(|i| rest.get(i + 1..).unwrap_or_default());
    let (reference, subdir) = match fragment {
        None => (String::new(), String::new()),
        Some(f) => match f.iter().position(|&b| b == b':') {
            Some(i) => (
                text(f.get(..i).unwrap_or_default())?,
                text(f.get(i + 1..).unwrap_or_default())?,
            ),
            None => (text(f)?, String::new()),
        },
    };
    let attr = |k: &[u8]| attrs.get(k).map(|v| text(v)).transpose();
    let url = match attr(b"git.fullurl")? {
        Some(u) => u,
        None => format!(
            "https://{}",
            text(rest.split(|&b| b == b'#').next().unwrap_or_default())?
        ),
    };
    Ok(Source {
        url,
        reference,
        subdir: clean_subdir(&subdir),
        keep_git_dir: attr(b"git.keepgitdir")?.as_deref() == Some("true"),
        checksum: attr(b"git.checksum")?,
        submodules: attr(b"git.skipsubmodules")?.as_deref() != Some("true"),
        auth_token: attr(b"git.authtokensecret")?,
        auth_header: attr(b"git.authheadersecret")?,
        ssh_agent: attr(b"git.mountsshsock")?,
    })
}

/// A subdir as BuildKit cleans it (path.Clean of `/` + it): `dir`, `dir/`, `./dir` and
/// `/dir` are one; the repository's root is empty.
fn clean_subdir(s: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in s.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Smart HTTP (gitprotocol-http.adoc): the advertisement, then each command `POST`ed to
/// where the advertisement was found, as git follows a redirect of its first request
/// alone (http.followRedirects=initial). Credentials of the URL go with the first
/// request, and with later ones only to that origin.
struct Http {
    client: Client,
    /// The build's Authorization for this repository, from its secrets, sent with every
    /// request in place of the URL's own credentials.
    secret_auth: Option<String>,
    /// The repository's URL, without its userinfo and trailing `/`.
    base: RefCell<String>,
    authorization: Option<String>,
    /// The URL as messages show it.
    shown: String,
}

impl Http {
    fn new(url: &str, cancel: Cancel, auth: Option<&Auth>) -> Result<Http, String> {
        let (target, authorization) = super::http::userinfo(url)?;
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let client = Client::new(Box::new(move |_| Ok(config.clone())), &agent())
            .cancelled_by(cancel)
            .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
                std::env::var(k).ok()
            }));
        let base = target.trim_end_matches('/').to_string();
        let secret_auth = auth
            .filter(|a| in_scope(&a.scope, &base))
            .map(|a| a.value.clone());
        Ok(Http {
            client,
            secret_auth,
            shown: super::http::shown(url).trim_end_matches('/').to_string(),
            base: RefCell::new(base),
            authorization,
        })
    }

    fn failed(&self, status: u16) -> String {
        match status {
            404 => format!("repository '{}/' not found", self.shown),
            401 | 403 => format!("Authentication failed for '{}/'", self.shown),
            s => format!(
                "unable to access '{}/': The requested URL returned error: {s}",
                self.shown
            ),
        }
    }
}

impl Transport for Http {
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String> {
        let at = format!("{}/info/refs?service=git-upload-pack", self.base.borrow());
        let url = Url::parse(&at).map_err(|e| e.to_string())?;
        let first = Cell::new(true);
        let authorize = |_: &Url| {
            Ok(match &self.secret_auth {
                Some(a) => Some(a.clone()),
                None if first.replace(false) => self.authorization.clone(),
                None => None,
            })
        };
        let request = Request {
            method: "GET",
            url: &url,
            headers: &[VERSION_2, ("Accept", "*/*")],
            body: &[],
            file: None,
        };
        let response = self
            .client
            .follow(&request, &authorize, Redirects::Anywhere)
            .map_err(|e| format!("unable to access '{}/': {e}", self.shown))?;
        if response.status != 200 {
            return Err(self.failed(response.status));
        }
        if response.header("content-type") != Some(ADVERTISEMENT_TYPE) {
            return Err(format!(
                "repository '{}/' is not served by git's smart HTTP protocol",
                self.shown
            ));
        }
        let landed = response.url().as_str();
        if let Some((base, _)) = landed.split_once("/info/refs") {
            *self.base.borrow_mut() = base.to_string();
        }
        Ok(Box::new(response))
    }

    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String> {
        let url =
            Url::parse(&format!("{}/git-upload-pack", self.base.borrow())).map_err(|e| e.to_string())?;
        let authorize = |_: &Url| Ok(self.secret_auth.clone().or_else(|| self.authorization.clone()));
        let request = Request {
            method: "POST",
            url: &url,
            headers: &[VERSION_2, ("Content-Type", REQUEST_TYPE), ("Accept", RESULT_TYPE)],
            body,
            file: None,
        };
        let response = self
            .client
            .follow(&request, &authorize, Redirects::SameOrigin)
            .map_err(|e| format!("unable to access '{}/': {e}", self.shown))?;
        if response.status != 200 {
            return Err(self.failed(response.status));
        }
        Ok(Box::new(response))
    }
}

fn agent() -> String {
    format!("shards/{}", env!("CARGO_PKG_VERSION"))
}

/// Where a failure belongs: resolving the ref (BuildKit's "failed to load cache key"), or
/// making the snapshot.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    CacheKey(String),
    Snapshot(String),
}

/// How long a git daemon's connection may take to open, and may go without a byte while
/// it is read: past it, the server is gone, not slow.
const DAEMON_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// A remote's transport, by its URL's scheme.
enum Wire {
    Http(Box<Http>),
    Daemon(shards_git::daemon::Daemon),
    Ssh(Box<super::ssh::Ssh>),
}

impl Transport for Wire {
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String> {
        match self {
            Wire::Http(h) => h.advertise(),
            Wire::Daemon(d) => d.advertise(),
            Wire::Ssh(s) => s.advertise(),
        }
    }

    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String> {
        match self {
            Wire::Http(h) => h.command(body),
            Wire::Daemon(d) => d.command(body),
            Wire::Ssh(s) => s.command(body),
        }
    }
}

/// An Authorization the build's secrets give, and the URLs it is sent to.
#[derive(Debug, Clone)]
pub struct Auth {
    scope: String,
    value: String,
}

/// The Authorization `src`'s secrets give, as BuildKit's git source takes it (v0.28.1
/// source/git/source.go authSecretNames, getAuthToken): the first of the header secret
/// for the remote's host, the token secret for it, the header secret, the token secret;
/// a token as `basic` credentials of `x-access-token`, a header as it is; sent to the
/// remote, or to all of github.com for a github.com remote (tokenScope).
pub fn auth(
    src: &Source,
    secrets: &std::collections::BTreeMap<String, shards_cmdline::buildflags::SecretBytes>,
) -> Option<Auth> {
    let authority = src.url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let mut names: Vec<(String, bool)> = Vec::new();
    if let Some(h) = &src.auth_header {
        names.push((format!("{h}.{host}"), false));
    }
    if let Some(t) = &src.auth_token {
        names.push((format!("{t}.{host}"), true));
    }
    if let Some(h) = &src.auth_header {
        names.push((h.clone(), false));
    }
    if let Some(t) = &src.auth_token {
        names.push((t.clone(), true));
    }
    let (secret, token) = names
        .into_iter()
        .find_map(|(n, t)| secrets.get(&n).map(|s| (s.bytes().to_vec(), t)))?;
    let value = if token {
        let basic = shards_registry::auth::basic(b"x-access-token", &secret);
        format!("basic {}", basic.strip_prefix("Basic ").unwrap_or(&basic))
    } else {
        String::from_utf8_lossy(&secret).into_owned()
    };
    let remote = remote_url(&src.url);
    let scope = ["https://github.com/", "https://www.github.com/"]
        .into_iter()
        .find(|p| remote.starts_with(p))
        .map_or(remote.clone(), str::to_string);
    Some(Auth { scope, value })
}

/// Whether `url` is within `scope`, as git matches a URL to `http.<url>.*` (urlmatch.c):
/// the scope's scheme, host and port, and its path a prefix of the URL's at a `/`.
fn in_scope(scope: &str, url: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    url == scope || url.strip_prefix(scope).is_some_and(|rest| rest.starts_with('/'))
}

/// The build's SSH agents, and the one a source's fetches over SSH take.
#[derive(Clone, Copy)]
pub struct SshAgents<'a> {
    pub agents: &'a super::Agents,
    pub id: Option<&'a str>,
}

/// The transport `url` names: smart HTTP(S), git's own, or SSH with the agent `ssh`
/// names, as BuildKit's git source mounts it (`no SSH key "ID" forwarded from the
/// client` where the build has none of that ID).
fn wire(url: &str, cancel: &Cancel, auth: Option<&Auth>, ssh: SshAgents<'_>) -> Result<Wire, String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(Wire::Http(Box::new(Http::new(url, cancel.clone(), auth)?)));
    }
    if url.starts_with("git://") {
        return Ok(Wire::Daemon(shards_git::daemon::Daemon::of_url(
            url,
            DAEMON_PATIENCE,
        )?));
    }
    if super::ssh::is_ssh(url) {
        let id = ssh
            .id
            .ok_or_else(|| format!("{url}: a Git repository over SSH, in a source given no SSH agent"))?;
        let agent = ssh.agents.get(id).ok_or_else(|| {
            format!(
                "no SSH key {} forwarded from the client",
                shards_cmdline::go::quote(id)
            )
        })?;
        return Ok(Wire::Ssh(Box::new(super::ssh::Ssh::open(
            url,
            agent,
            DAEMON_PATIENCE,
        )?)));
    }
    Err(format!(
        "{url}: shards build fetches Git repositories over HTTP(S), SSH and git:// only"
    ))
}

/// When `src`'s commit was made, as BuildKit takes a Git source's SOURCE_DATE_EPOCH
/// (dockerfile/1.27.1 epoch.go, sourceDateEpochFromMetadata): the ref resolved and
/// checked as a fetch resolves it, the commit fetched, its committer's time.
pub fn commit_time(
    src: &Source,
    limits: Limits,
    cancel: &Cancel,
    auth: Option<&Auth>,
    agents: &super::Agents,
) -> Result<(i64, u32), String> {
    let ssh = SshAgents {
        agents,
        id: src.ssh_agent.as_deref(),
    };
    let wrap = if src.reference.is_empty() {
        format!("error fetching default branch for repository {}", src.url)
    } else {
        format!("failed to fetch remote {}", src.url)
    };
    let remote =
        Remote::open(wire(&src.url, cancel, auth, ssh)?, &agent()).map_err(|e| format!("{wrap}: {e}"))?;
    let resolved = remote
        .resolve(&src.reference)
        .map_err(|e| format!("{wrap}: {e}"))?
        .ok_or_else(|| format!("repository does not contain ref {}, output: \"\"", src.reference))?;
    check(src.checksum.as_deref(), &resolved)?;
    let (pack, _) = fetch_commits(&remote, &[resolved.commit], limits).map_err(|e| format!("{wrap}: {e}"))?;
    Ok((commit_of(&pack, &resolved.commit)?.committed, 0))
}

/// The snapshot of `src`: resolved, fetched within `limits`, checked out; each line git's
/// commands would print through `say`.
pub fn snapshot(
    exec: &mut Exec,
    src: &Source,
    limits: Limits,
    cancel: &Cancel,
    auth: Option<&Auth>,
    agents: &super::Agents,
    say: &dyn Fn(&str),
) -> Result<Ref, Failure> {
    let ssh = SshAgents {
        agents,
        id: src.ssh_agent.as_deref(),
    };
    let key = Failure::CacheKey;
    let snap = Failure::Snapshot;
    if let Some(c) = &src.checksum
        && !(!c.is_empty() && c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(key(format!(
            "invalid checksum {c} for Git URL, expected hex commit hash"
        )));
    }
    let wrap = if src.reference.is_empty() {
        format!("error fetching default branch for repository {}", src.url)
    } else {
        format!("failed to fetch remote {}", src.url)
    };
    let remote = Remote::open(wire(&src.url, cancel, auth, ssh).map_err(key)?, &agent())
        .map_err(|e| key(format!("{wrap}: {e}")))?;
    let resolved = remote
        .resolve(&src.reference)
        .map_err(|e| key(format!("{wrap}: {e}")))?
        .ok_or_else(|| {
            key(format!(
                "repository does not contain ref {}, output: \"\"",
                src.reference
            ))
        })?;
    check(src.checksum.as_deref(), &resolved).map_err(key)?;
    say(&resolved.commit.hex());
    // A kept repository keeps the annotated tag it was asked by, which the commit's pack
    // alone would not hold.
    let keep = src.keep_git_dir && src.subdir.is_empty();
    let mut wants = vec![resolved.commit];
    if keep && let Some(tag) = resolved.tag {
        wants.push(tag);
    }
    let (pack, shallow) = fetch_commits(&remote, &wants, limits).map_err(|e| snap(format!("{wrap}: {e}")))?;
    let commit = commit_of(&pack, &resolved.commit).map_err(snap)?;
    let stage = exec.stage().map_err(snap)?;
    let mut out = Checkout::new(
        stage.join(format!("git-{}", resolved.commit.hex())),
        auth.cloned(),
    )
    .map_err(snap)?;
    let time = (commit.committed, 0);
    let root_tree = subtree(&pack, &commit.tree, &src.subdir).map_err(snap)?;
    let tracked = out.walk(&pack, &root_tree, b"", time).map_err(snap)?;
    let mut modules = Vec::new();
    if src.submodules {
        let base = remote_url(&src.url);
        out.submodules(
            &base,
            &pack,
            &commit.tree,
            &src.subdir,
            limits,
            cancel,
            ssh,
            say,
            keep,
            &mut modules,
            0,
        )
        .map_err(snap)?;
    }
    if keep {
        let kept = kept_ref(&resolved);
        let configured: Vec<(Vec<u8>, Vec<u8>)> = modules
            .iter()
            .filter(|m| m.depth == 0)
            .map(|m| (m.name.clone(), m.url.clone().into_bytes()))
            .collect();
        let index = repo::index(&tracked).map_err(snap)?;
        let shown = super::http::shown(&src.url);
        let files = repo::git_dir(
            &pack,
            &resolved.commit,
            shallow,
            &shown,
            kept.as_ref(),
            index,
            &configured,
            None,
        )
        .map_err(snap)?;
        out.git_files(b".git", files, time).map_err(snap)?;
        for m in modules {
            out.git_files(&m.git_dir, m.files, time).map_err(snap)?;
        }
    }
    out.finish(exec, time).map_err(snap)
}

/// The pack of `wants`, one commit deep; from a server that will not send a commit that
/// is no ref's tip by its name alone ("not our ref": `uploadpack.allowReachableSHA1InWant`
/// off, as some hosts keep it), every ref's history whole, as BuildKit then fetches it
/// (source/git `git fetch --tags origin`). Whether what it holds is shallow.
fn fetch_commits<T: Transport>(
    remote: &Remote<T>,
    wants: &[Oid],
    limits: Limits,
) -> Result<(Pack, bool), String> {
    match remote.fetch(wants, limits) {
        Ok(pack) => Ok((pack, true)),
        Err(refused) if refused.starts_with("remote error:") => {
            let pack = remote.fetch_whole(limits)?;
            if wants.iter().all(|w| pack.has(w)) {
                Ok((pack, false))
            } else {
                Err(refused)
            }
        }
        Err(e) => Err(e),
    }
}

/// The ref a kept repository keeps: the branch or tag asked for, by its full name; none
/// for a commit asked for by its name.
fn kept_ref(r: &Resolved) -> Option<KeptRef> {
    let name = r.name.clone()?;
    Some(KeptRef {
        oid: r.tag.unwrap_or(r.commit),
        name,
    })
}

/// The checksum's check (source.go resolveMetadata): a hex prefix of the commit's name,
/// or of the annotated tag's.
fn check(checksum: Option<&str>, r: &Resolved) -> Result<(), String> {
    let Some(want) = checksum else { return Ok(()) };
    let want_lower = want.to_ascii_lowercase();
    let matches = |o: &Oid| o.hex().starts_with(&want_lower);
    if matches(&r.commit) || r.tag.as_ref().is_some_and(matches) {
        return Ok(());
    }
    Err(match &r.tag {
        Some(tag) => format!(
            "expected checksum to match {want}, got {} or {}",
            tag.hex(),
            r.commit.hex()
        ),
        None => format!("expected checksum to match {want}, got {}", r.commit.hex()),
    })
}

fn commit_of(pack: &Pack, oid: &Oid) -> Result<object::Commit, String> {
    match pack.get(oid)? {
        (ObjectKind::Commit, data) => object::commit(&data),
        _ => Err(format!("{} is not a commit", oid.hex())),
    }
}

/// The tree at `subdir` of `tree`, every part of it a real directory, as BuildKit's
/// validateDirsOnly requires.
fn subtree(pack: &Pack, tree: &Oid, subdir: &str) -> Result<Oid, String> {
    let mut at = *tree;
    let mut walked = String::new();
    for part in subdir.split('/').filter(|p| !p.is_empty()) {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(part);
        let (_, data) = pack.get(&at)?;
        let entry = object::tree(&data)?.into_iter().find(|e| e.name == part.as_bytes()).ok_or_else(|| {
            format!("invalid subdir /{subdir}: failed to lstat \"{walked}\": statat {walked}: no such file or directory")
        })?;
        if entry.mode != object::Mode::Tree {
            return Err(format!(
                "invalid subdir /{subdir}: git subpath \"/{subdir}\" contains non-directory \"{part}\""
            ));
        }
        at = entry.oid;
    }
    Ok(at)
}

/// A remote's URL without its userinfo, for submodules' relative URLs.
fn remote_url(url: &str) -> String {
    super::http::userinfo(url)
        .map(|(u, _)| u)
        .unwrap_or_else(|_| url.to_string())
}

/// A submodule's URL relative to its superproject's, as git resolves `./` and `../`
/// (submodule--helper resolve-relative-url): each `../` takes the remote's last path
/// part off.
fn relative_url(base: &str, url: &str) -> String {
    if !(url.starts_with("./") || url.starts_with("../")) {
        return url.to_string();
    }
    let mut base = base.trim_end_matches('/').to_string();
    let mut rest = url;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            rest = r;
            if let Some(i) = base.rfind('/') {
                base.truncate(i);
            }
        } else {
            break;
        }
    }
    format!("{base}/{rest}")
}

/// A submodule checked out within a kept repository: its name and URL for the
/// superproject's config, how deep it is nested, and its own repository's files, which go
/// at `git_dir`.
struct Module {
    name: Vec<u8>,
    url: String,
    depth: usize,
    git_dir: Vec<u8>,
    files: Vec<(String, Vec<u8>, u32)>,
}

/// A checkout being made: its entries, and the staged file their bytes are written to.
struct Checkout {
    path: PathBuf,
    file: File,
    written: u64,
    entries: Vec<(Vec<u8>, Node, Option<u64>)>,
    dirs: std::collections::HashSet<Vec<u8>>,
    /// The build's Authorization, for submodules within its scope.
    auth: Option<Auth>,
}

impl Checkout {
    fn new(path: PathBuf, auth: Option<Auth>) -> Result<Checkout, String> {
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Checkout {
            path,
            file,
            written: 0,
            entries: Vec::new(),
            dirs: std::collections::HashSet::new(),
            auth,
        })
    }

    fn meta(mode: u16, time: (i64, u32)) -> Meta {
        Meta {
            mode,
            mtime: time.0,
            mtime_nsec: time.1,
            ..Meta::default()
        }
    }

    fn dir(&mut self, path: &[u8], time: (i64, u32)) {
        if self.dirs.insert(path.to_vec()) {
            self.entries.push((
                path.to_vec(),
                Node {
                    kind: Kind::Dir(Dir::default()),
                    meta: Self::meta(0o755, time),
                },
                None,
            ));
        }
    }

    /// `bytes` as the file at `path`, of `mode`.
    fn file(&mut self, path: Vec<u8>, bytes: &[u8], mode: u16, time: (i64, u32)) -> Result<(), String> {
        let at = self.written;
        self.file
            .write_all(bytes)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let size = bytes.len() as u64;
        self.written += size;
        self.entries.push((
            path,
            Node {
                kind: Kind::File {
                    size,
                    data: shards_image::erofs::DataRef {
                        source: 0,
                        offset: at,
                    },
                },
                meta: Self::meta(mode, time),
            },
            Some(at),
        ));
        Ok(())
    }

    /// `tree`'s entries under `prefix`; what the index of its repository records of them.
    fn walk(
        &mut self,
        pack: &Pack,
        tree: &Oid,
        prefix: &[u8],
        time: (i64, u32),
    ) -> Result<Vec<Tracked>, String> {
        let stamp = u32::try_from(time.0).unwrap_or(0);
        let mut tracked = Vec::new();
        checkout::walk(pack, tree, &mut |path, item| {
            let full = if prefix.is_empty() {
                path.to_vec()
            } else {
                [prefix, b"/", path].concat()
            };
            let size = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
            match item {
                Item::Dir => self.dir(&full, time),
                Item::Submodule { commit } => {
                    self.dir(&full, time);
                    tracked.push(Tracked {
                        path: path.to_vec(),
                        mode: 0o160000,
                        oid: commit,
                        size: 0,
                        time: stamp,
                    });
                }
                Item::Symlink { target, oid } => {
                    tracked.push(Tracked {
                        path: path.to_vec(),
                        mode: 0o120000,
                        oid,
                        size: size(target.len()),
                        time: stamp,
                    });
                    self.entries.push((
                        full,
                        Node {
                            kind: Kind::Symlink(target.into_boxed_slice()),
                            meta: Self::meta(0o777, time),
                        },
                        None,
                    ));
                }
                Item::File {
                    executable,
                    data,
                    oid,
                } => {
                    let (mode, git_mode) = if executable {
                        (0o755, 0o100755)
                    } else {
                        (0o644, 0o100644)
                    };
                    tracked.push(Tracked {
                        path: path.to_vec(),
                        mode: git_mode,
                        oid,
                        size: size(data.len()),
                        time: stamp,
                    });
                    self.file(full, &data, mode, time)?;
                }
            }
            Ok(())
        })?;
        Ok(tracked)
    }

    /// The files of a repository's `.git` at `at`, its directories made as they are met.
    fn git_files(
        &mut self,
        at: &[u8],
        files: Vec<(String, Vec<u8>, u32)>,
        time: (i64, u32),
    ) -> Result<(), String> {
        let mut parts = Vec::new();
        for part in at.split(|&b| b == b'/') {
            parts.push(part);
            self.dir(&parts.join(&b'/'), time);
        }
        for d in [
            "objects",
            "objects/info",
            "objects/pack",
            "refs",
            "refs/heads",
            "refs/tags",
        ] {
            self.dir(&[at, b"/", d.as_bytes()].concat(), time);
        }
        for (path, bytes, mode) in files {
            let full = [at, b"/", path.as_bytes()].concat();
            if let Some(i) = full.iter().rposition(|&b| b == b'/') {
                let parent = full.get(..i).unwrap_or_default().to_vec();
                let mut p = Vec::new();
                for part in parent.split(|&b| b == b'/') {
                    if !p.is_empty() {
                        p.push(b'/');
                    }
                    p.extend_from_slice(part);
                    self.dir(&p.clone(), time);
                }
            }
            self.file(full, &bytes, u16::try_from(mode).unwrap_or(0o644), time)?;
        }
        Ok(())
    }

    /// The submodules of `tree` (the commit's root) that fall within `subdir`, each
    /// fetched one deep from its URL and checked out at its path, recursively; with `keep`,
    /// each with a repository of its own under `.git/modules`, as `git submodule` keeps
    /// them, recorded in `modules`.
    #[allow(clippy::too_many_arguments)]
    fn submodules(
        &mut self,
        url: &str,
        pack: &Pack,
        tree: &Oid,
        subdir: &str,
        limits: Limits,
        cancel: &Cancel,
        ssh: SshAgents<'_>,
        say: &dyn Fn(&str),
        keep: bool,
        modules: &mut Vec<Module>,
        depth: usize,
    ) -> Result<(), String> {
        // Submodules within submodules this deep are a cycle.
        if depth > 32 {
            return Err("submodules nested too deep".into());
        }
        let mut links = Vec::new();
        checkout::walk(pack, tree, &mut |path, item| {
            if let Item::Submodule { commit } = item {
                links.push((path.to_vec(), commit));
            }
            Ok(())
        })?;
        if links.is_empty() {
            return Ok(());
        }
        let declared = match blob_at(pack, tree, b".gitmodules")? {
            Some(text) => config::submodules(&text)?,
            None => Vec::new(),
        };
        let parent_prefix = modules.last().filter(|_| depth > 0).map(|m| m.git_dir.clone());
        for (path, commit) in links {
            let shown = String::from_utf8_lossy(&path).into_owned();
            let Some(local) = within(&path, subdir) else {
                continue;
            };
            let module = declared
                .iter()
                .find(|m| m.path.as_deref() == Some(path.as_slice()));
            let Some(sub_url) = module.and_then(|m| m.url.as_deref()) else {
                return Err(format!(
                    "No url found for submodule path '{shown}' in .gitmodules"
                ));
            };
            let sub_url = relative_url(url, &String::from_utf8_lossy(sub_url));
            let name = module.map(|m| m.name.clone()).unwrap_or_else(|| path.clone());
            say(&format!(
                "Submodule '{}' ({sub_url}) registered for path '{shown}'",
                String::from_utf8_lossy(&name)
            ));
            let remote = Remote::open(wire(&sub_url, cancel, self.auth.as_ref(), ssh)?, &agent())?;
            let (sub_pack, sub_shallow) = fetch_commits(&remote, &[commit], limits)?;
            let sub_commit = commit_of(&sub_pack, &commit)?;
            let time = (sub_commit.committed, 0);
            let tracked = self.walk(&sub_pack, &sub_commit.tree, &local, time)?;
            say(&format!(
                "Submodule path '{shown}': checked out '{}'",
                commit.hex()
            ));
            if keep {
                // Its repository under its superproject's `.git/modules`, and a `.git` file in
                // its work tree that names it, each path relative to the other.
                let git_dir = match &parent_prefix {
                    Some(p) => [p.as_slice(), b"/modules/", &name].concat(),
                    None => [b".git/modules/".as_slice(), &name].concat(),
                };
                let up = |p: &[u8]| "../".repeat(p.split(|&b| b == b'/').count());
                let gitfile = format!("gitdir: {}{}\n", up(&local), String::from_utf8_lossy(&git_dir));
                self.file(
                    [local.as_slice(), b"/.git"].concat(),
                    gitfile.as_bytes(),
                    0o644,
                    time,
                )?;
                let worktree = format!("{}{}", up(&git_dir), String::from_utf8_lossy(&local));
                let index = repo::index(&tracked)?;
                let files = repo::git_dir(
                    &sub_pack,
                    &commit,
                    sub_shallow,
                    &sub_url,
                    None,
                    index,
                    &[],
                    Some(&worktree),
                )?;
                modules.push(Module {
                    name: name.clone(),
                    url: sub_url.clone(),
                    depth,
                    git_dir,
                    files,
                });
            }
            self.submodules(
                &sub_url,
                &sub_pack,
                &sub_commit.tree,
                "",
                limits,
                cancel,
                ssh,
                say,
                keep,
                modules,
                depth + 1,
            )
            .map_err(|e| e.replace("path '", &format!("path '{shown}/")))?;
        }
        Ok(())
    }

    /// The snapshot: the root a directory of the commit's time.
    fn finish(mut self, exec: &mut Exec, time: (i64, u32)) -> Result<Ref, String> {
        self.file.flush().map_err(|e| e.to_string())?;
        drop(self.file);
        let data = exec
            .sources
            .host(self.path, self.written)
            .map_err(|e| e.to_string())?;
        let entries = self
            .entries
            .into_iter()
            .map(|(path, mut node, at)| {
                if let (Some(at), Kind::File { data: d, .. }) = (at, &mut node.kind) {
                    *d = shards_image::erofs::DataRef {
                        source: data.source,
                        offset: at,
                    };
                }
                (path, node)
            })
            .collect();
        exec.tree_of(Self::meta(0o755, time), entries, self.written)
    }
}

/// `path`, a submodule's path in the repository, relative to `subdir`: `None` where it is
/// outside it.
fn within(path: &[u8], subdir: &str) -> Option<Vec<u8>> {
    if subdir.is_empty() {
        return Some(path.to_vec());
    }
    path.strip_prefix(subdir.as_bytes())?
        .strip_prefix(b"/")
        .map(<[u8]>::to_vec)
}

/// The blob at `name` in `tree`, if it holds one.
fn blob_at(pack: &Pack, tree: &Oid, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let (_, data) = pack.get(tree)?;
    match object::tree(&data)?.into_iter().find(|e| e.name == name) {
        Some(e) if matches!(e.mode, object::Mode::File | object::Mode::Executable) => {
            Ok(Some(pack.get(&e.oid)?.1))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_read_as_the_plan_writes_them() {
        let attrs: BTreeMap<Vec<u8>, Vec<u8>> = [
            (b"git.fullurl".to_vec(), b"http://h/repo.git".to_vec()),
            (b"git.checksum".to_vec(), b"540d46b".to_vec()),
        ]
        .into_iter()
        .collect();
        let s = source(b"git://h/repo.git#main:./dir/", &attrs).unwrap();
        assert_eq!((s.reference.as_str(), s.subdir.as_str()), ("main", "dir"));
        assert_eq!(s.checksum.as_deref(), Some("540d46b"));
        assert!(s.submodules && !s.keep_git_dir);
        let bare = source(b"git://h/repo.git", &BTreeMap::new()).unwrap();
        assert_eq!(
            (bare.url.as_str(), bare.reference.as_str()),
            ("https://h/repo.git", "")
        );
        assert_eq!(clean_subdir("/a/../b/./c/"), "b/c");
    }

    #[test]
    fn submodule_urls_resolve_as_git_resolves_them() {
        assert_eq!(
            relative_url("http://h/a/repo.git", "../sub.git"),
            "http://h/a/sub.git"
        );
        assert_eq!(
            relative_url("http://h/a/repo.git/", "./x"),
            "http://h/a/repo.git/x"
        );
        assert_eq!(
            relative_url("http://h/a/repo.git", "https://o/s.git"),
            "https://o/s.git"
        );
    }

    /// BuildKit's measured answers (Docker Engine 29.3.1), but for the annotated tag's,
    /// whose tag name BuildKit loses ("got  or ...").
    #[test]
    fn checksums_are_checked_as_buildkit_checks_them() {
        let c = Oid::parse(b"540d46b365c5df8d9809be74b5470dd61048f373").unwrap();
        let t = Oid::parse(b"ed61d74300000000000000000000000000000000").unwrap();
        let plain = Resolved {
            commit: c,
            tag: None,
            name: None,
        };
        let tagged = Resolved {
            commit: c,
            tag: Some(t),
            name: None,
        };
        assert_eq!(check(Some("540d46b"), &plain), Ok(()));
        assert_eq!(check(Some("540D46B"), &plain), Ok(()));
        assert_eq!(check(Some("ed61d743"), &tagged), Ok(()));
        assert_eq!(
            check(Some("84b80291c1a9dfba8c1c12153db386e9525b496f"), &plain).unwrap_err(),
            "expected checksum to match 84b80291c1a9dfba8c1c12153db386e9525b496f, got 540d46b365c5df8d9809be74b5470dd61048f373"
        );
        assert_eq!(
            check(Some("84b8029"), &tagged).unwrap_err(),
            "expected checksum to match 84b8029, got ed61d74300000000000000000000000000000000 or 540d46b365c5df8d9809be74b5470dd61048f373"
        );
    }
}
