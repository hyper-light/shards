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
use std::time::{Duration, Instant};

use shards_git::checkout::{self, Item};
use shards_git::object::{self, Kind as ObjectKind};
use shards_git::pack::Pack;
use shards_git::protocol::{ADVERTISEMENT_TYPE, REQUEST_TYPE, RESULT_TYPE, VERSION_2};
use shards_git::remote::{Limits, Remote, Resolved, Transport};
use shards_git::repo::{self, KeptRef, Tracked};
use shards_git::{Oid, config};
use shards_image::erofs::{Dir, Kind, Meta, Node};
use shards_registry::http::{Cancel, Client, Redirects, Request, Response};
use shards_registry::registry::{Trouble, Verdict, retry_after};
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
    /// Whether git's advice is asked for (`git.advice`, `BUILDKIT_GIT_ADVICE`).
    pub advice: bool,
    pub checksum: Option<String>,
    pub submodules: bool,
    /// The secrets that authorize its fetches: a token's (`git.authtokensecret`) and a
    /// whole header's (`git.authheadersecret`).
    pub auth_token: Option<String>,
    pub auth_header: Option<String>,
    /// The SSH agent its fetches over SSH authenticate with (`git.mountsshsock`).
    pub ssh_agent: Option<String>,
    /// An Agentfile directive's grant (D54): the origins beyond its own it may reach.
    pub grant: Option<super::grant::Redirects>,
}

impl Source {
    /// Its grant, with the URL it is the grant of.
    fn granted(&self) -> Option<(&str, &super::grant::Redirects)> {
        self.grant.as_ref().map(|g| (self.url.as_str(), g))
    }
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
        advice: attr(b"git.advice")?.as_deref() == Some("true"),
        checksum: attr(b"git.checksum")?,
        submodules: attr(b"git.skipsubmodules")?.as_deref() != Some("true"),
        auth_token: attr(b"git.authtokensecret")?,
        auth_header: attr(b"git.authheadersecret")?,
        ssh_agent: attr(b"git.mountsshsock")?,
        grant: super::grant::of(attrs)?,
    })
}

/// A subdir as BuildKit v0.28.1's git source cleans it (path.Clean of `/` + it): `dir`,
/// `dir/`, `./dir` and `/dir` are one; the repository's root is empty.
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

/// A remote's askings over HTTP(S), kept across its askings again (D273): its client, whose
/// paths learn what each shows (D222), and what last dropped an asking, where one may pass.
struct Asking {
    client: Client,
    dropped: RefCell<Option<(Url, Dropped)>>,
}

/// What ended an asking that may pass (D273).
#[derive(Debug)]
enum Dropped {
    /// Its request failed before any answer: a name its resolver did not resolve, a
    /// connection refused, reset or cut, a wait its own time ended.
    Failed(shards_registry::Error),
    /// Its server answered that it passes (408, 502, 503, 504), naming a wait or not.
    Passing(Option<Duration>),
    /// Its answer's body was cut short: its far end had answered.
    Cut,
}

impl Asking {
    /// A client for a build's fetches, as BuildKit's git reaches a remote: the platform's
    /// roots, the proxies the environment names; its paths' records kept in `kept`, an
    /// image store's directory, where there is one, which its waits are judged by.
    fn new(cancel: &Cancel, kept: Option<&std::path::Path>) -> Result<Asking, String> {
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let client = Client::new(Box::new(move |_| Ok(config.clone())), &agent())
            .cancelled_by(cancel.clone())
            .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
                std::env::var(k).ok()
            }));
        if let Some(root) = kept {
            client.keep_in(root);
        }
        Ok(Asking {
            client,
            dropped: RefCell::new(None),
        })
    }

    fn drop_of(&self, url: &Url, dropped: Dropped) {
        *self.dropped.borrow_mut() = Some((url.clone(), dropped));
    }
}

/// Runs `op`, askings of a remote, asking again what the network drops (D273): a name its
/// resolver did not resolve, a connection refused, reset or cut, a wait its own time ended,
/// a server's answer that it passes, an answer's body cut short. Each is a trouble of its
/// path's, ridden out as a registry's is (D222, `Trouble`): asked again after a random share
/// of how long the trouble has lasted, at least what its server names, each asking said
/// through `say` in the build's words; with no count and no deadline, until it is answered,
/// the build is cancelled, or a re-ask while the trouble follows its far end's liveness
/// says the far end is gone (refused, unresolved or cut before any answer), which fails the
/// asking naming it. Anything else fails at once, as before.
fn ridden<R>(asking: &Asking, say: &dyn Fn(&str), mut op: impl FnMut() -> Result<R, Failure>) -> Result<R, Failure> {
    let mut trouble = Trouble::default();
    let mut troubled: Option<Url> = None;
    loop {
        let attempt = Instant::now();
        let failed = match op() {
            Ok(done) => {
                if let Some(url) = &troubled {
                    trouble.over(&asking.client, url);
                }
                return Ok(done);
            }
            Err(failed) => failed,
        };
        let Some((url, dropped)) = asking.dropped.borrow_mut().take() else {
            return Err(failed);
        };
        let verdict = match &dropped {
            Dropped::Failed(e) => trouble.failed(&asking.client, &url, attempt, e),
            Dropped::Passing(after) => Verdict::Again(trouble.passing(&asking.client, &url, attempt, *after)),
            Dropped::Cut => Verdict::Again(trouble.passing(&asking.client, &url, attempt, None)),
        };
        troubled = Some(url.clone());
        match verdict {
            Verdict::Again(wait) => {
                say(&format!(
                    "{}: asking {} again in {:.1} s",
                    failed.text(),
                    url.authority(),
                    wait.as_secs_f64()
                ));
                asking
                    .client
                    .pause(wait)
                    .map_err(|e| failed.and(&format!(": {e}")))?;
            }
            Verdict::Gone => {
                let why = match &dropped {
                    Dropped::Failed(e) if e.is_refused() => "it refused the connection",
                    Dropped::Failed(e) if e.is_unresolved() => "its name did not resolve",
                    _ => "it ended the asking before any answer",
                };
                return Err(failed.and(&format!(
                    " ({} is gone: {why} again, after {:.1} s of trouble)",
                    url.authority(),
                    trouble.lasted().as_secs_f64()
                )));
            }
            Verdict::Fails => return Err(failed),
        }
    }
}

/// An answer's body, its cut recorded where the network dropped it (D273).
struct Heard<'a> {
    body: Response<'a>,
    asking: &'a Asking,
    url: Url,
}

impl Read for Heard<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.body.read(buf);
        if let Err(e) = &read
            && matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            )
        {
            self.asking.drop_of(&self.url, Dropped::Cut);
        }
        read
    }
}

/// Smart HTTP (gitprotocol-http.adoc): the advertisement, then each command `POST`ed to
/// where the advertisement was found, as git follows a redirect of its first request
/// alone (http.followRedirects=initial). Credentials of the URL go with the first
/// request, and with later ones only to that origin.
struct Http<'a> {
    asking: &'a Asking,
    /// The build's Authorization for this repository, from its secrets, sent with every
    /// request in place of the URL's own credentials.
    secret_auth: Option<String>,
    /// The repository's URL, without its userinfo and trailing `/`.
    base: RefCell<String>,
    authorization: Option<String>,
    /// The URL as messages show it.
    shown: String,
    /// Each hop held to the grant of an Agentfile directive's source (D54).
    hops: Option<super::grant::Hops>,
}

impl<'a> Http<'a> {
    fn new(
        url: &str,
        asking: &'a Asking,
        auth: Option<&Auth>,
        grant: Option<(&str, &super::grant::Redirects)>,
    ) -> Result<Http<'a>, String> {
        let (target, authorization) = super::http::userinfo(url)?;
        let base = target.trim_end_matches('/').to_string();
        let secret_auth = auth
            .filter(|a| in_scope(&a.scope, &base))
            .map(|a| a.value.clone());
        let hops = match grant {
            None => None,
            Some((source, redirects)) => {
                let (first, _) = super::http::userinfo(source)?;
                Some(super::grant::Hops::new(
                    Url::parse(&first).map_err(|e| e.to_string())?,
                    redirects.clone(),
                ))
            }
        };
        Ok(Http {
            asking,
            secret_auth,
            shown: super::http::shown(url).trim_end_matches('/').to_string(),
            base: RefCell::new(base),
            authorization,
            hops,
        })
    }

    /// Whether the hop to `to` is granted.
    fn check(&self, to: &Url) -> Result<(), shards_registry::Error> {
        match &self.hops {
            Some(h) => h
                .check(to)
                .map_err(|r| std::io::Error::other(r.to_string()).into()),
            None => Ok(()),
        }
    }

    /// A request's failure: the grant's refusal where it refused a hop.
    fn unreached(&self, e: &shards_registry::Error) -> String {
        match self.hops.as_ref().and_then(super::grant::Hops::refused) {
            Some(r) => format!("unable to access '{}/': {r}", self.shown),
            None => format!("unable to access '{}/': {e}", self.shown),
        }
    }

    /// `answered`, an asking of `url`: an answer that it passes, or a failure, recorded
    /// where it may pass (D273); a hop its grant refused never is.
    fn asked(
        &self,
        url: &Url,
        answered: Result<Response<'a>, shards_registry::Error>,
    ) -> Result<Response<'a>, String> {
        match answered {
            Ok(response) => {
                if matches!(response.status, 408 | 502 | 503 | 504) {
                    self.asking
                        .drop_of(url, Dropped::Passing(retry_after(&response)));
                }
                Ok(response)
            }
            Err(e) => {
                let said = self.unreached(&e);
                if self.hops.as_ref().and_then(super::grant::Hops::refused).is_none() {
                    self.asking.drop_of(url, Dropped::Failed(e));
                }
                Err(said)
            }
        }
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

impl Transport for Http<'_> {
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String> {
        let at = format!("{}/info/refs?service=git-upload-pack", self.base.borrow());
        let url = Url::parse(&at).map_err(|e| e.to_string())?;
        let first = Cell::new(true);
        let authorize = |to: &Url| {
            self.check(to)?;
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
        let asking = self.asking;
        let response = self.asked(&url, asking.client.follow(&request, &authorize, Redirects::Anywhere))?;
        if response.status != 200 {
            return Err(self.failed(response.status));
        }
        if response.header("content-type") != Some(ADVERTISEMENT_TYPE) {
            return Err(format!(
                "repository '{}/' is not served by git's smart HTTP protocol",
                self.shown
            ));
        }
        let landed = response.url().clone();
        if let Some((base, _)) = landed.as_str().split_once("/info/refs") {
            *self.base.borrow_mut() = base.to_string();
        }
        Ok(Box::new(Heard {
            body: response,
            asking,
            url: landed,
        }))
    }

    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String> {
        let url =
            Url::parse(&format!("{}/git-upload-pack", self.base.borrow())).map_err(|e| e.to_string())?;
        let authorize = |to: &Url| {
            self.check(to)?;
            Ok(self.secret_auth.clone().or_else(|| self.authorization.clone()))
        };
        let request = Request {
            method: "POST",
            url: &url,
            headers: &[VERSION_2, ("Content-Type", REQUEST_TYPE), ("Accept", RESULT_TYPE)],
            body,
            file: None,
        };
        let asking = self.asking;
        let response = self.asked(&url, asking.client.follow(&request, &authorize, Redirects::SameOrigin))?;
        if response.status != 200 {
            return Err(self.failed(response.status));
        }
        let landed = response.url().clone();
        Ok(Box::new(Heard {
            body: response,
            asking,
            url: landed,
        }))
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

impl Failure {
    fn text(&self) -> &str {
        match self {
            Failure::CacheKey(s) | Failure::Snapshot(s) => s,
        }
    }

    /// The same failure, `more` after its words.
    fn and(self, more: &str) -> Failure {
        match self {
            Failure::CacheKey(s) => Failure::CacheKey(s + more),
            Failure::Snapshot(s) => Failure::Snapshot(s + more),
        }
    }

    fn into_text(self) -> String {
        match self {
            Failure::CacheKey(s) | Failure::Snapshot(s) => s,
        }
    }
}

/// How long a git daemon's connection may take to open, and may go without a byte while
/// it is read: past it, the server is gone, not slow.
const DAEMON_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// A remote's transport, by its URL's scheme.
enum Wire<'a> {
    Http(Box<Http<'a>>),
    Daemon(shards_git::daemon::Daemon),
    Ssh(Box<super::ssh::Ssh>),
}

impl Transport for Wire<'_> {
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
/// names, as BuildKit v0.28.1's git source mounts it (`no SSH key "ID" forwarded from the
/// client` where the build has none of that ID).
/// With `grant`, an Agentfile directive's source and its grant (D54), the remote is opened
/// only at an origin it grants.
fn wire<'a>(
    url: &str,
    asking: &'a Asking,
    auth: Option<&Auth>,
    ssh: SshAgents<'_>,
    grant: Option<(&str, &super::grant::Redirects)>,
) -> Result<Wire<'a>, String> {
    if let Some((source, redirects)) = grant {
        super::grant::remote(source, redirects, url).map_err(|r| r.to_string())?;
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(Wire::Http(Box::new(Http::new(url, asking, auth, grant)?)));
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
/// checked as a fetch resolves it, the commit fetched, its committer's time and the
/// offset of its zone.
pub fn commit_time(
    src: &Source,
    limits: Limits,
    cancel: &Cancel,
    auth: Option<&Auth>,
    agents: &super::Agents,
) -> Result<Option<(i64, i32)>, String> {
    let ssh = SshAgents {
        agents,
        id: src.ssh_agent.as_deref(),
    };
    let wrap = if src.reference.is_empty() {
        format!("error fetching default branch for repository {}", src.url)
    } else {
        format!("failed to fetch remote {}", src.url)
    };
    let asking = Asking::new(cancel, None)?;
    // Planned before any step runs: no step's log says its askings again.
    let (resolved, pack) = ridden(&asking, &|_| {}, || {
        let failed = |e: String| Failure::CacheKey(format!("{wrap}: {e}"));
        let remote = Remote::open(
            wire(&src.url, &asking, auth, ssh, src.granted()).map_err(Failure::CacheKey)?,
            &agent(),
        )
        .map_err(failed)?;
        let resolved = remote.resolve(&src.reference).map_err(failed)?.ok_or_else(|| {
            Failure::CacheKey(format!(
                "repository does not contain ref {}, output: \"\"",
                src.reference
            ))
        })?;
        check(src.checksum.as_deref(), &resolved).map_err(Failure::CacheKey)?;
        let (pack, _) = fetch_commits(&remote, &[resolved.commit], limits).map_err(failed)?;
        Ok((resolved, pack))
    })
    .map_err(Failure::into_text)?;
    match pack.get(&resolved.commit)? {
        // As the Dockerfile frontend reads the commit object BuildKit's git source gives
        // it: the committer's time, none where the line says none well formed.
        (ObjectKind::Commit, data) => Ok(super::gitobject::parse(&data)?.to_commit()?.committer.when),
        _ => Err(format!("{} is not a commit", resolved.commit.hex())),
    }
}

/// What a build policy asks of `src` (D102), as BuildKit's git source resolves its
/// metadata: the ref its name resolves to (a commit's name as it is), the object that ref
/// names (an annotated tag's own) and the commit; and where `objects`, the commit's and
/// the tag's raw objects, fetched one commit deep.
pub fn metadata(
    src: &Source,
    objects: bool,
    limits: Limits,
    cancel: &Cancel,
    auth: Option<&Auth>,
    agents: &super::Agents,
) -> Result<super::policy::GitMeta, String> {
    let ssh = SshAgents {
        agents,
        id: src.ssh_agent.as_deref(),
    };
    let wrap = if src.reference.is_empty() {
        format!("error fetching default branch for repository {}", src.url)
    } else {
        format!("failed to fetch remote {}", src.url)
    };
    let asking = Asking::new(cancel, None)?;
    // Planned before any step runs: no step's log says its askings again.
    let (resolved, pack) = ridden(&asking, &|_| {}, || {
        let failed = |e: String| Failure::CacheKey(format!("{wrap}: {e}"));
        let remote = Remote::open(
            wire(&src.url, &asking, auth, ssh, src.granted()).map_err(Failure::CacheKey)?,
            &agent(),
        )
        .map_err(failed)?;
        let resolved = remote.resolve(&src.reference).map_err(failed)?.ok_or_else(|| {
            Failure::CacheKey(format!(
                "repository does not contain ref {}, output: \"\"",
                src.reference
            ))
        })?;
        let pack = match objects {
            true => {
                let mut wants = vec![resolved.commit];
                if let Some(tag) = resolved.tag {
                    wants.push(tag);
                }
                Some(fetch_commits(&remote, &wants, limits).map_err(failed)?.0)
            }
            false => None,
        };
        Ok((resolved, pack))
    })
    .map_err(Failure::into_text)?;
    let reference = match &resolved.name {
        Some(n) => String::from_utf8_lossy(n).into_owned(),
        None => src.reference.clone(),
    };
    let mut meta = super::policy::GitMeta {
        reference,
        checksum: resolved.tag.unwrap_or(resolved.commit).hex(),
        commit_checksum: resolved.commit.hex(),
        commit_object: None,
        tag_object: None,
    };
    if let Some(pack) = pack {
        meta.commit_object = Some(pack.get(&resolved.commit)?.1);
        if let Some(tag) = resolved.tag {
            meta.tag_object = Some(pack.get(&tag)?.1);
        }
    }
    Ok(meta)
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
) -> Result<(Ref, String), Failure> {
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
    // A kept repository keeps the annotated tag it was asked by, which the commit's pack
    // alone would not hold.
    let keep = src.keep_git_dir && src.subdir.is_empty();
    let asking = Asking::new(cancel, Some(exec.store.root())).map_err(key)?;
    let (resolved, pack, shallow) = ridden(&asking, say, || {
        let remote = Remote::open(
            wire(&src.url, &asking, auth, ssh, src.granted()).map_err(key)?,
            &agent(),
        )
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
        let mut wants = vec![resolved.commit];
        if keep && let Some(tag) = resolved.tag {
            wants.push(tag);
        }
        let (pack, shallow) =
            fetch_commits(&remote, &wants, limits).map_err(|e| snap(format!("{wrap}: {e}")))?;
        Ok((resolved, pack, shallow))
    })?;
    say(&resolved.commit.hex());
    let commit = commit_of(&pack, &resolved.commit).map_err(snap)?;
    if keep {
        checked_out(&pack, &resolved.commit, src.advice, say).map_err(snap)?;
    }
    let stage = exec.stage().map_err(snap)?;
    let mut out = Checkout::new(
        stage.join(format!("git-{}", resolved.commit.hex())),
        auth.cloned(),
        src.granted().map(|(u, g)| (u.to_string(), g.clone())),
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
            &asking,
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
    out.finish(exec, time)
        .map(|r| (r, resolved.commit.hex()))
        .map_err(snap)
}

/// What git says as it checks a kept repository out at `commit` (D135): its advice on the
/// detached HEAD that leaves, where `advice` asks for it, as git words it for a checkout of
/// the commit by its name (builtin/checkout.c, advice.detachedHead), and then the commit,
/// as `describe_detached_head` names it: its abbreviation among the repository's objects
/// and its subject. BuildKit's git, which checks out what it fetched as `FETCH_HEAD`, says
/// the same of that (PM M195).
fn checked_out(pack: &Pack, commit: &Oid, advice: bool, say: &dyn Fn(&str)) -> Result<(), String> {
    if advice {
        say(&format!("Note: switching to '{}'.", commit.hex()));
        for line in DETACHED_HEAD_ADVICE {
            say(line);
        }
    }
    let data = match pack.get(commit)? {
        (ObjectKind::Commit, data) => data,
        _ => return Err(format!("{} is not a commit", commit.hex())),
    };
    let hex = commit.hex();
    let short = hex.get(..commit.abbrev(pack.names())).unwrap_or(&hex);
    say(&format!(
        "HEAD is now at {short} {}",
        String::from_utf8_lossy(&object::subject(&data))
    ));
    Ok(())
}

/// The lines git advises a detached HEAD with, after its note of what it switched to, as
/// git 2.52 prints them (PM M195: under BuildKit v0.34.0 and Docker 29.3.1 alike).
const DETACHED_HEAD_ADVICE: [&str; 16] = [
    "",
    "You are in 'detached HEAD' state. You can look around, make experimental",
    "changes and commit them, and you can discard any commits you make in this",
    "state without impacting any branches by switching back to a branch.",
    "",
    "If you want to create a new branch to retain commits you create, you may",
    "do so (now or later) by using -c with the switch command. Example:",
    "",
    "  git switch -c <new-branch-name>",
    "",
    "Or undo this operation with:",
    "",
    "  git switch -",
    "",
    "Turn off this advice by setting config variable advice.detachedHead to false",
    "",
];

/// The pack of `wants`, one commit deep; from a server that will not send a commit that
/// is no ref's tip by its name alone ("not our ref": `uploadpack.allowReachableSHA1InWant`
/// off, as some hosts keep it), every ref's history whole, as BuildKit then fetches it
/// (v0.28.1 source/git: `git fetch --tags origin`). Whether what it holds is shallow.
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

/// The tree at `subdir` of `tree`, every part of it a real directory, as BuildKit v0.28.1's
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
    /// The source's grant, which its submodules are held to (D54).
    grant: Option<(String, super::grant::Redirects)>,
}

impl Checkout {
    fn new(
        path: PathBuf,
        auth: Option<Auth>,
        grant: Option<(String, super::grant::Redirects)>,
    ) -> Result<Checkout, String> {
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Checkout {
            path,
            file,
            written: 0,
            entries: Vec::new(),
            dirs: std::collections::HashSet::new(),
            auth,
            grant,
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
        asking: &Asking,
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
            let grant = self.grant.as_ref().map(|(u, g)| (u.as_str(), g));
            let (sub_pack, sub_shallow) = ridden(asking, say, || {
                let remote = Remote::open(
                    wire(&sub_url, asking, self.auth.as_ref(), ssh, grant).map_err(Failure::Snapshot)?,
                    &agent(),
                )
                .map_err(Failure::Snapshot)?;
                fetch_commits(&remote, &[commit], limits).map_err(Failure::Snapshot)
            })
            .map_err(Failure::into_text)?;
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
                asking,
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

    /// A loopback server that answers its `n`-th connection (from 0) with `answer(n)` and
    /// then closes it, until `count` connections.
    fn serving(count: usize, answer: fn(usize) -> Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::thread::spawn(move || {
            for n in 0..count {
                let (mut tcp, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = tcp.read(&mut request).unwrap();
                let _ = tcp.write_all(&answer(n));
                let _ = tcp.shutdown(std::net::Shutdown::Write);
            }
        });
        (port, served)
    }

    /// An advertisement's body the network cut short, 10 of its 32 bytes come, is fetched
    /// again, the asking said in the step's log, and read whole (D273).
    #[test]
    fn a_fetch_cut_mid_body_is_asked_again_and_completes() {
        const WHOLE: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";
        let (port, served) = serving(2, |n| {
            let mut answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ADVERTISEMENT_TYPE}\r\nContent-Length: 32\r\n\r\n"
            )
            .into_bytes();
            answer.extend_from_slice(if n == 0 { &WHOLE[..10] } else { &WHOLE[..] });
            answer
        });
        let asking = Asking::new(&Cancel::new(), None).unwrap();
        let said = RefCell::new(Vec::new());
        let url = format!("http://127.0.0.1:{port}/repo.git");
        let got = ridden(&asking, &|line| said.borrow_mut().push(line.to_string()), || {
            let http = Http::new(&url, &asking, None, None).map_err(Failure::Snapshot)?;
            let mut body = Vec::new();
            http.advertise()
                .map_err(Failure::Snapshot)?
                .read_to_end(&mut body)
                .map_err(|e| Failure::Snapshot(format!("the server's advertisement: {e}")))?;
            Ok(body)
        })
        .unwrap();
        assert_eq!(got, WHOLE);
        let said = said.into_inner();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("the server's advertisement: ")
                && said[0].contains(&format!("asking 127.0.0.1:{port} again in ")),
            "{said:?}"
        );
        served.join().unwrap();
    }

    /// A remote that is gone, nothing listening where it is, is asked again once, said, and
    /// refused on the re-ask its far end refused while the trouble followed its liveness (a
    /// path that has passed none): in words naming it (D273).
    #[test]
    fn a_remote_that_is_gone_is_refused_naming_it() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let asking = Asking::new(&Cancel::new(), None).unwrap();
        let said = RefCell::new(Vec::new());
        let url = format!("http://127.0.0.1:{port}/repo.git");
        let asked = Cell::new(0);
        let e = ridden(&asking, &|line| said.borrow_mut().push(line.to_string()), || {
            asked.set(asked.get() + 1);
            let http = Http::new(&url, &asking, None, None).map_err(Failure::CacheKey)?;
            http.advertise().map(drop).map_err(Failure::CacheKey)
        })
        .unwrap_err();
        assert_eq!(asked.get(), 2);
        assert_eq!(said.into_inner().len(), 1);
        let Failure::CacheKey(e) = e else {
            panic!("{e:?}")
        };
        assert!(
            e.starts_with(&format!("unable to access 'http://127.0.0.1:{port}/repo.git/': "))
                && e.contains(&format!("(127.0.0.1:{port} is gone: it refused the connection again, after ")),
            "{e}"
        );
    }

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
    fn git_checksums_are_checked() {
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
