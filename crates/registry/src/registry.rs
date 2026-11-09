//! One repository on one registry (docs/research/registry-pull.md R1, R8):
//! - requests with containerd v2.4.1's retries, authorization and redirects
//!   (`core/remotes/docker/resolver.go`, `doWithRetries`);
//! - resolving a reference to a descriptor as its `Resolve` does;
//! - fetching content by digest into the store, verified, and resuming a blob's download
//!   where it stopped (`fetcher.go`, `httpreadseeker.go`).

use std::fmt;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use shards_image::oci::{Descriptor, MAX_MANIFEST, media};
use shards_image::reference::{Algorithm, DOCKER_HUB, Digest, Reference};
use shards_image::store::{Download, Held, Limits, Store};

use crate::auth::{Authorizer, Credentials, loopback};
use crate::credentials::Found;
use crate::http::{Client, Redirects, Request, Response};
use crate::links::{Direction, Link, Record};
use crate::url::{Scheme, Url};
use crate::{Error, ErrorKind, Said, printable};

/// The longest of the waits before asking a throttling registry again: up to 50 ms,
/// then 100, 200, 400 and 800, each a random share of it. ECR Public throttles
/// anonymous requests at random, at 1 a second already, and less the slower they come
/// (PM M112): containerd asks again at once, five times, and a pull's burst still fails.
pub(crate) const THROTTLED: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
/// The longest window a throttle is waited out within: an allowance counted over longer
/// (Docker Hub's, over six hours) is a quota spent, not a moment's crowd.
const WINDOW_MOST: Duration = Duration::from_secs(60);
/// What resolving accepts, in containerd's order (resolver.go:170-178).
const RESOLVE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, */*";
/// How much of a refusal's body containerd reads for what the registry says
/// (`remotes/errors.NewUnexpectedStatusErr`).
const MAX_ERROR_BODY: u64 = 64000;
/// The most a page of a repository's tags may hold: a bound against a registry that
/// never ends one, where distribution's client reads all it is sent. Docker Hub sent
/// library/python's 3,974 tags in one page of 81,819 bytes (2026-10-04).
const MAX_TAGS_PAGE: u64 = 4 << 20;
pub(crate) const CHUNK: usize = 64 << 10;
/// What a blob's bytes are sent as.
const OCTET: (&str, &str) = ("Content-Type", "application/octet-stream");
const OCTETS: &[(&str, &str)] = &[OCTET];

/// An upload begun: there already (mounted or not), or open at its `Location`, taking
/// chunks of at least so many bytes.
enum Begun {
    There(bool),
    Open(Url, u64),
}

/// The bytes the registry says it has of an upload of `size` bytes, by its `Range`
/// (`0-<end>`, inclusive), where it says.
fn had(response: &Response<'_>, size: u64) -> Result<Option<u64>, Error> {
    let Some(range) = response.header("range") else {
        return Ok(None);
    };
    range
        .trim()
        .strip_prefix("0-")
        .and_then(|end| end.parse::<u64>().ok())
        .and_then(|end| end.checked_add(1))
        .filter(|&had| had <= size)
        .map(Some)
        .ok_or_else(|| Error::new(format!("an upload of {size} bytes said to have {range:?}")))
}

/// Whether the request that ended an upload, its last PUT, was answered as containerd's
/// pusher takes it: 202 is not among them, as the request that gets it hears
/// (pusher.go), though `Commit` would take it; and a `Docker-Content-Digest`, if sent,
/// must be the blob's.
fn committed(desc: &Descriptor, response: Response<'_>) -> Result<(), Error> {
    if !matches!(response.status, 200 | 201 | 204) {
        return Err(unexpected("PUT", response));
    }
    if let Some(header) = response.header("docker-content-digest") {
        let got = Digest::parse(header.trim())
            .map_err(|e| Error::new(format!("invalid content digest in response: {e}")))?;
        if got.to_string() != desc.digest {
            return Err(Error::new(format!("got digest {got}, expected {}", desc.digest)));
        }
    }
    Ok(())
}

/// A repository's registry, reached as containerd's default hosts reach it
/// (`core/remotes/docker/registry.go`): Docker Hub at `registry-1.docker.io`, loopback
/// hosts over plain HTTP, those Docker's daemon configuration names insecure over plain
/// HTTP too (D194), and every other host over https.
pub struct Registry {
    http: Client,
    auth: Authorizer,
    /// `scheme://host/v2/<repository>/`
    base: Url,
    /// For a registry that may be asked in plain HTTP, a loopback host on a port that
    /// names no scheme or a host named insecure: how (`None` for every other host).
    plain: Option<Plain>,
    /// containerd's `RepositoryScope`: pull access to the repository.
    scopes: Vec<String>,
    /// Docker Hub's mirrors a pull from it asks first, in order (D194): each a registry of
    /// its own, its host's certificates and credentials its own.
    mirrors: Vec<Mirror>,
    /// The fewest bytes of a blob it refused to take in one request, with a 413 (D226):
    /// a push sends no request of a blob so long again, `u64::MAX` before any refusal.
    refused: AtomicU64,
    /// The most bytes of a chunk it took.
    took: AtomicU64,
}

/// A mirror of Docker Hub's a pull asks first, and whether it failed: once it has, the
/// pull asks it nothing more and goes to Docker Hub itself, so that a failing mirror costs
/// a pull one error at most (any error of a mirror's).
#[derive(Debug)]
struct Mirror {
    registry: Registry,
    failed: AtomicBool,
}

/// How a registry that may be asked in plain HTTP is: asked in HTTPS first, then in plain
/// HTTP once it answers HTTPS so, or never finishes the handshake; or, named insecure
/// without a port, once its HTTPS port refuses the connection, as dockerd tries its
/// `https://` endpoint and then its `http://` one (`lookupV2Endpoints`).
#[derive(Debug)]
struct Plain {
    /// Whether it is asked in plain HTTP now.
    fallen: AtomicBool,
    /// The port it is asked at in plain HTTP: its own, or HTTP's for one named without.
    port: u16,
    /// Whether a refused connection falls back too.
    on_refusal: bool,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("base", &self.base.to_string())
            .field("mirrors", &self.mirrors.len())
            .finish_non_exhaustive()
    }
}

/// The host a reference's registry is reached at: Docker Hub's for `docker.io`.
pub fn host(reference: &Reference) -> &str {
    if reference.domain == DOCKER_HUB {
        "registry-1.docker.io"
    } else {
        &reference.domain
    }
}

impl Registry {
    /// A registry to push `reference`'s repository to, as containerd's pusher asks (D20):
    /// pull and push access to it, and pull access to `mounts`, repositories of the same
    /// registry its blobs may be mounted from.
    pub fn for_push(
        http: Client,
        reference: &Reference,
        credentials: impl Into<Found>,
        mounts: &[String],
    ) -> Result<Registry, Error> {
        let mut registry = Registry::new(http, reference, credentials)?;
        registry.scopes = vec![format!("repository:{}:pull,push", reference.path)];
        registry
            .scopes
            .extend(mounts.iter().map(|m| format!("repository:{m}:pull")));
        Ok(registry)
    }

    /// `scheme://host/v2/<repository>/`, what its requests are made relative to.
    pub(crate) fn base(&self) -> &Url {
        &self.base
    }

    /// Runs `work`, saying through `say`, once, that it waits for the registry's credential
    /// helper where it does (D184): a helper may wait on its user, or never answer, and a
    /// pull that waits on one says which, and where it was named.
    pub fn saying_waits<T>(&self, say: &(dyn Fn(&str) + Sync), work: impl FnOnce() -> T) -> T {
        if !self.auth.asks() {
            return work();
        }
        self.auth.watch();
        std::thread::scope(|scope| {
            let watcher = std::thread::Builder::new()
                .name("credential wait".into())
                .spawn_scoped(scope, || {
                    if let Some(notice) = self.auth.waits() {
                        say(&notice);
                    }
                });
            let out = work();
            self.auth.done();
            if let Ok(watcher) = watcher {
                let _ = watcher.join();
            }
            out
        })
    }

    /// A registry to delete manifests of `reference`'s repository from, and to update the
    /// referrers it lists: pull, push and delete access to it, as distribution grants a
    /// DELETE only to a `delete` scope (registry/handlers/app.go, appendAccessRecords).
    pub fn for_delete(
        http: Client,
        reference: &Reference,
        credentials: impl Into<Found>,
    ) -> Result<Registry, Error> {
        let mut registry = Registry::new(http, reference, credentials)?;
        registry.scopes = vec![format!("repository:{}:pull,push,delete", reference.path)];
        Ok(registry)
    }

    /// Whether the repository has what `desc` describes, as containerd's pusher asks
    /// before it pushes anything (pusher.go v2.4.1): a HEAD of the blob, or of the
    /// manifest or index by `tag` if given, else by its digest, accepting its type or any;
    /// by a tag, it has it only where the tag names the same digest. A check refused as
    /// unauthorized by a challenge that says why (containerd's ErrInvalidAuthorization)
    /// is no answer: the push goes on.
    pub fn exists(&self, desc: &Descriptor, manifest: bool, tag: Option<&str>) -> Result<bool, Error> {
        let path = match (manifest, tag) {
            (true, Some(tag)) => format!("manifests/{tag}"),
            (true, None) => format!("manifests/{}", desc.digest),
            (false, _) => format!("blobs/{}", desc.digest),
        };
        let url = self.base.join(&path)?;
        let accept = format!("{}, */*", desc.media_type);
        let headers = [("Accept", accept.as_str())];
        let (response, method) = match self.request("HEAD", &url, &headers) {
            Ok(answered) => answered,
            Err(e) if e.kind() == ErrorKind::Unauthorized => return Ok(false),
            Err(e) => return Err(e),
        };
        match response.status {
            200 if manifest && tag.is_some() => Ok(response
                .header("docker-content-digest")
                .is_some_and(|d| d.trim() == desc.digest)),
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(self.refused_head(method, response, &url, &headers)),
        }
    }

    /// The record of `direction` over the path to this registry (crate::links): by its host
    /// and port, and this host's address on its route there.
    pub(crate) fn record(&self, store: &Store, direction: Direction) -> Record {
        let host = self.base.host().trim_start_matches('[').trim_end_matches(']');
        Record::of(
            store.root(),
            direction,
            host,
            self.base.port(),
            self.http.route(&self.base),
        )
    }

    /// Uploads the blob `desc` describes from `file`, as containerd v2.4.1's pusher does
    /// (core/remotes/docker/pusher.go, Push and Commit): mounted from `from`, a repository
    /// of the same registry, when given and the registry does, and uploaded as if it were
    /// not given when the mount is refused as unauthorized; else an upload begun with a
    /// POST, which a 201 answers as there already, and done with one PUT of it all, whose
    /// `Docker-Content-Digest`, if it sends one, must be the blob's. Whether it was
    /// mounted.
    ///
    /// Over a `link` (D226) its bytes are counted as they go, and it goes in chunks where
    /// the link's sends have failed, each of the size that costs least a byte
    /// ([`Link::chunk`]), or where the registry, or a proxy before it, refused a request so
    /// long with a 413: then each as long as bisecting what it took and what it refused
    /// says it takes. A send that fails is resumed from what the registry says it has
    /// (distribution-spec v1.1.1, "Pushing a blob in chunks": a GET of the upload's
    /// `Location` says its `Range`), never from the blob's start, unless the upload is gone.
    pub fn upload(
        &self,
        desc: &Descriptor,
        file: &std::fs::File,
        from: Option<&str>,
        link: Option<&Link>,
    ) -> Result<bool, Error> {
        let size = desc.size().map_err(|e| Error::new(e.to_string()))?;
        let sent = link.map(Link::sent);
        let (mut at, least) = match self.begin(desc, from, link)? {
            Begun::There(mounted) => return Ok(mounted),
            Begun::Open(at, least) => (at, least),
        };
        // What the registry has taken, and what it had when a send last failed; and the
        // trouble since it last took more, ridden out for its path's budget (D222).
        let (mut done, mut mark, mut trouble) = (0u64, 0u64, Trouble::default());
        loop {
            let asked = Instant::now();
            let chunk = self.chunk(link, least);
            let whole =
                done == 0 && size < self.refused.load(Ordering::Relaxed) && chunk.is_none_or(|c| c >= size);
            let len = match chunk {
                _ if whole => size,
                Some(c) => c.max(least).min(size - done),
                None => size - done,
            };
            let answered = if whole {
                let put = at.with_query_pair("digest", &desc.digest)?;
                self.send("PUT", &put, OCTETS, &[], Some((file, 0, size)), sent)
            } else if len > 0 {
                let range = format!("{done}-{}", done + len - 1);
                let headers = [OCTET, ("Content-Range", range.as_str())];
                self.send("PATCH", &at, &headers, &[], Some((file, done, len)), sent)
            } else {
                let put = at.with_query_pair("digest", &desc.digest)?;
                let (response, _) = self.send("PUT", &put, OCTETS, &[], None, None)?;
                return committed(desc, response).map(|()| false);
            };
            let response = match answered {
                Ok((response, _)) => response,
                Err(e) if e.kind() == ErrorKind::Transient => {
                    if let Some(link) = link {
                        link.failed();
                    }
                    if done > mark {
                        trouble.over(&self.http, &at);
                    }
                    mark = done;
                    match trouble.wait(asked, self.http.budget(&at), None) {
                        Some(wait) => self.http.pause(wait)?,
                        None => {
                            return Err(e.context(format!(
                                "{}: the upload failed for {:.0?} without the registry taking more",
                                desc.digest,
                                trouble.lasted()
                            )));
                        }
                    }
                    match self.status(&at, size)? {
                        Some((url, had)) => (at, done) = (url, had),
                        None => match self.begin(desc, from, link)? {
                            Begun::There(mounted) => return Ok(mounted),
                            Begun::Open(url, _) => (at, done) = (url, 0),
                        },
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            match response.status {
                413 if whole => {
                    // Refused before the registry had it (nginx's `client_max_body_size`,
                    // 1 MiB unless set): the upload begun is still open, and takes chunks.
                    self.refused.fetch_min(size, Ordering::Relaxed);
                }
                _ if whole => return committed(desc, response).map(|()| false),
                202 => {
                    self.took.fetch_max(len, Ordering::Relaxed);
                    let had = had(&response, size)?.unwrap_or(done + len);
                    if had <= done {
                        return Err(Error::new(format!(
                            "{}: the registry has {had} bytes of an upload sent to byte {}",
                            desc.digest,
                            done + len
                        )));
                    }
                    let next = response.header("location").ok_or_else(|| {
                        Error::new(format!("{}: a chunk taken with no Location", desc.digest))
                    })?;
                    (at, done) = (response.url().join(next)?, had);
                }
                413 if len > least.max(1) => {
                    self.refused.fetch_min(len, Ordering::Relaxed);
                }
                // Not where the registry is: asked where that is, for a moment at most.
                416 if trouble.wait(asked, self.http.budget(&at), None).is_some() => {
                    drop(response);
                    match self.status(&at, size)? {
                        Some((url, had)) => (at, done) = (url, had),
                        None => return Err(Error::new(format!("{}: the upload is gone", desc.digest))),
                    }
                }
                _ => return Err(unexpected("PATCH", response)),
            }
        }
    }

    /// An upload of `desc` begun: mounted from `from` where given, else a POST, its answer
    /// timed into `link`.
    fn begin(&self, desc: &Descriptor, from: Option<&str>, link: Option<&Link>) -> Result<Begun, Error> {
        let mut started = None;
        if let Some(repo) = from {
            let url = self
                .base
                .join(&format!("blobs/uploads/?mount={}&from={repo}", desc.digest))?;
            match self.send("POST", &url, &[], &[], None, None) {
                // Not allowed to read `from`: uploaded instead.
                Ok((response, _)) if response.status == 401 => {}
                Ok((response, _)) => started = Some((response, true)),
                Err(e) if e.kind() == ErrorKind::Unauthorized => {}
                Err(e) => return Err(e.context(format!("pushing with mount from {repo}"))),
            }
        }
        let asked = std::time::Instant::now();
        let (response, mounting) = match started {
            Some(started) => started,
            None => match self.send("POST", &self.base.join("blobs/uploads/")?, &[], &[], None, None) {
                Ok((response, _)) => {
                    if let Some(link) = link {
                        link.answered(asked.elapsed());
                    }
                    (response, false)
                }
                Err(e) if e.kind() == ErrorKind::Unauthorized => {
                    return Err(e.context(
                        "push access denied, repository does not exist or may require authorization",
                    ));
                }
                Err(e) => return Err(e),
            },
        };
        let location = match response.status {
            201 => return Ok(Begun::There(mounting)),
            200 | 202 | 204 => response
                .header("location")
                .map(str::to_string)
                .ok_or_else(|| Error::new(format!("{}: an upload with no Location", desc.digest)))?,
            _ => return Err(unexpected("POST", response)),
        };
        // The least a chunk but the last may hold, where the registry says (distribution-spec
        // v1.1.1 spec.md, "Pushing a blob in chunks": `OCI-Chunk-Min-Length`).
        let least = response
            .header("oci-chunk-min-length")
            .and_then(|n| n.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Begun::Open(response.url().join(&location)?, least))
    }

    /// Where an upload at `at` of a blob of `size` bytes is: its `Location` and the bytes the
    /// registry has (a GET of it, answered 204 with its `Range`), or `None` where the
    /// registry has it no more (404).
    fn status(&self, at: &Url, size: u64) -> Result<Option<(Url, u64)>, Error> {
        let (response, _) = self.send("GET", at, &[], &[], None, None)?;
        match response.status {
            204 => {
                // `0-0` is all an upload of nothing or of one byte says (distribution's
                // blobupload.go): taken as nothing, one byte sent again at most.
                let had = match response.header("range").map(str::trim) {
                    Some("0-0") | None => 0,
                    Some(_) => had(&response, size)?.unwrap_or(0),
                };
                let url = match response.header("location") {
                    Some(next) => response.url().join(next)?,
                    None => at.clone(),
                };
                Ok(Some((url, had)))
            }
            404 => Ok(None),
            _ => Err(unexpected("GET", response)),
        }
    }

    /// The next chunk's most: the least of what costs `link` least a byte and what the
    /// registry takes, bisected between the most it took and the least it refused; `None`
    /// where neither bounds it.
    fn chunk(&self, link: Option<&Link>, least: u64) -> Option<u64> {
        let refused = self.refused.load(Ordering::Relaxed);
        let takes = (refused != u64::MAX).then(|| {
            let took = self.took.load(Ordering::Relaxed).min(refused);
            took + (refused - took) / 2
        });
        let best = link.and_then(Link::chunk);
        match (takes, best) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|c| c.max(least).max(1))
    }

    /// Puts a manifest or index as `reference` names it (a tag or its digest).
    pub fn put_manifest(&self, reference: &str, media_type: &str, bytes: &[u8]) -> Result<(), Error> {
        self.put_manifest_subject(reference, media_type, bytes)
            .map(|_| ())
    }

    /// [`put_manifest`](Self::put_manifest), and the `OCI-Subject` the registry answers it
    /// with, if it does: a registry with the referrers API says so of a manifest with a
    /// `subject` it took (distribution-spec v1.1.1 spec.md:499).
    pub fn put_manifest_subject(
        &self,
        reference: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, Error> {
        let url = self.base.join(&format!("manifests/{reference}"))?;
        let (response, _) = self.send("PUT", &url, &[("Content-Type", media_type)], bytes, None, None)?;
        match response.status {
            200 | 201 | 204 => Ok(response.header("oci-subject").map(|s| s.trim().to_string())),
            _ => Err(unexpected("PUT", response)),
        }
    }

    /// The manifest or index `tag` names: its type and bytes, at most a manifest's size;
    /// none where the repository has no such tag (a 404).
    pub fn manifest_at(&self, tag: &str) -> Result<Option<(String, Vec<u8>)>, Error> {
        let accept = format!("{}, {}", media::OCI_INDEX, media::OCI_MANIFEST);
        self.manifest_accepting(tag, &accept)
    }

    /// [`manifest_at`](Self::manifest_at), any kind a pull resolves (containerd's list):
    /// an image stored as a Docker manifest list is given as one, never the registry's
    /// choice of a single platform's manifest.
    pub(crate) fn document_at(&self, reference: &str) -> Result<Option<(String, Vec<u8>)>, Error> {
        self.manifest_accepting(reference, RESOLVE_ACCEPT)
    }

    fn manifest_accepting(&self, tag: &str, accept: &str) -> Result<Option<(String, Vec<u8>)>, Error> {
        let url = self.base.join(&format!("manifests/{tag}"))?;
        let (mut response, _) =
            self.request("GET", &url, &[("Accept", accept), ("Accept-Encoding", ENCODINGS)])?;
        match response.status {
            200..=299 => {
                let kind = response.header("content-type").unwrap_or_default().to_string();
                let encoding = response
                    .header("content-encoding")
                    .unwrap_or_default()
                    .to_string();
                let mut body = decoded(&mut response, &encoding)?;
                let mut bytes = Vec::new();
                body.by_ref()
                    .take(MAX_MANIFEST.saturating_add(1))
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_MANIFEST {
                    return Err(Error::new(format!(
                        "{tag}: a manifest larger than {MAX_MANIFEST} bytes"
                    )));
                }
                Ok(Some((kind, bytes)))
            }
            404 => Ok(None),
            _ => Err(not_fetched(response, &url)),
        }
    }

    /// Deletes the manifest `digest` (spec.md:688-693): 202 is done; a registry that does
    /// not delete answers 400 or 405, and says so.
    pub fn delete_manifest(&self, digest: &str) -> Result<(), Error> {
        let url = self.base.join(&format!("manifests/{digest}"))?;
        let (response, _) = self.request("DELETE", &url, &[])?;
        match response.status {
            200 | 202 | 204 => Ok(()),
            404 => Err(Error::of(
                ErrorKind::NotFound,
                format!("{digest}: not found in the repository"),
            )),
            _ => Err(unexpected("DELETE", response)),
        }
    }

    pub fn new(
        http: Client,
        reference: &Reference,
        credentials: impl Into<Found>,
    ) -> Result<Registry, Error> {
        let base = Url::parse(&format!("https://{}/v2/{}/", host(reference), reference.path))?;
        Registry::at(
            http,
            base,
            Authorizer::new_for(credentials),
            vec![format!("repository:{}:pull", reference.path)],
        )
    }

    /// A registry asked at `base`, answered by `auth` (its URL to come), for `scopes`:
    /// containerd v2.4.1's defaults for a loopback host (core/remotes/docker/config/
    /// hosts.go), HTTPS alone on 443 or no port, plain HTTP on 80, and on any other port
    /// HTTPS first, then plain HTTP once the server answers it so (NewHTTPFallback).
    /// A host Docker's daemon configuration names insecure, by its `host[:port]` or a
    /// network it is in (D194), is asked in HTTPS first, then in plain HTTP, as dockerd
    /// asks one, at its port, or HTTP's where it names none. containerd skips verifying a
    /// loopback host's certificate, and dockerd an insecure one's; shards verifies both,
    /// against certs.d and the system's roots. A loopback registry's credentials go to it
    /// in plain HTTP too, as containerd's do; an insecure one's never leave the machine
    /// unencrypted (auth.rs).
    fn at(
        http: Client,
        mut base: Url,
        auth: impl FnOnce(&Url) -> Authorizer,
        scopes: Vec<String>,
    ) -> Result<Registry, Error> {
        let mut plain = None;
        let ported = base.authority() != base.host();
        let fallback = |port, on_refusal| Plain {
            fallen: AtomicBool::new(false),
            port,
            on_refusal,
        };
        let local = loopback(&base);
        if local && ported {
            match base.port() {
                80 => base = base.plain()?,
                port => plain = Some(fallback(port, false)),
            }
        } else if !local && http.registries().insecure(&base.authority()) {
            plain = Some(match ported {
                true => fallback(base.port(), false),
                false => fallback(Scheme::Http.default_port(), true),
            });
        }
        let auth = auth(&base);
        let auth = match &plain {
            Some(p) if local => auth.plain_too(p.port),
            Some(p) => auth.plain_without(p.port),
            None => auth,
        };
        Ok(Registry {
            auth,
            // Its waits are what its paths show (D222).
            http: http.learning(),
            base,
            plain,
            scopes,
            mirrors: Vec::new(),
            refused: AtomicU64::new(u64::MAX),
            took: AtomicU64::new(0),
        })
    }

    /// A mirror of Docker Hub's at `mirror` (`scheme://host[:port][/path]/`, as
    /// [`crate::registries`] keeps one), for `reference`'s repository, its credentials
    /// those of the mirror's own host: dockerd gives a mirror Docker Hub's
    /// (`daemon/internal/distribution/registry.go`, `newRepository`); shards, where its
    /// user logged in to the mirror, the mirror's (D194).
    pub fn mirror(
        http: Client,
        mirror: &str,
        reference: &Reference,
        credentials: impl Into<Found>,
    ) -> Result<Registry, Error> {
        let base = Url::parse(&format!("{mirror}v2/{}/", reference.path))?;
        Registry::at(
            http,
            base,
            Authorizer::new_for(credentials),
            vec![format!("repository:{}:pull", reference.path)],
        )
    }

    /// This registry, asking `mirror` (a [`Registry::mirror`]) first, after the mirrors it
    /// asks already: a pull from Docker Hub with its mirrors (D194).
    pub fn with_mirror(mut self, mirror: Registry) -> Registry {
        self.mirrors.push(Mirror {
            registry: mirror,
            failed: AtomicBool::new(false),
        });
        self
    }

    /// Logs in to the registry at `host` (Docker Hub's at `registry-1.docker.io`), as
    /// dockerd does (moby daemon/pkg/registry/auth.go, loginV2): its `/v2/` asked with
    /// `credentials`, a bearer challenge answered asking for a refresh token as well. The
    /// refresh token, where its token server gave one: the identity token `docker login`
    /// keeps in place of a password. A refusal of the credentials is unauthorized.
    pub fn login(http: Client, host: &str, credentials: Credentials) -> Result<Option<String>, Error> {
        let base = Url::parse(&format!("https://{host}/v2/"))?;
        let registry = Registry::at(
            http,
            base,
            |base| Authorizer::new(base, credentials).offline(),
            Vec::new(),
        )?;
        let (response, _) = registry
            .request("GET", &registry.base, &[])
            .map_err(|e| match e.kind() {
                ErrorKind::Refused => Error::of(ErrorKind::Unauthorized, e.to_string()),
                _ => e,
            })?;
        match response.status {
            200..=299 => Ok(registry.auth.refresh_token()),
            401 | 403 => Err(Error::of(
                ErrorKind::Unauthorized,
                format!(
                    "{}: unauthorized: the registry refused the credentials",
                    response.url()
                ),
            )),
            _ => Err(unexpected("GET", response)),
        }
    }

    /// Sends a request as containerd's `doWithRetries` does, up to 5 attempts (D20; these
    /// numbers are containerd's, open in docs/audit/2026-10-16_docker-framing.md):
    /// - a timeout or a cut connection is tried again after 50 ms;
    /// - a 401 is answered (auth.rs), then the request is sent again;
    /// - a HEAD of a manifest refused with 405 becomes a GET;
    /// - 408 is tried again, and 500, 502, 503 or 504 once, unless it repeats: a gateway's
    ///   failure, 502, as its timeout and an unavailable server are, which containerd
    ///   leaves out (Docker Hub's token server answered a pull's request with one, which
    ///   the next passed: dev 3c2e3cd3's CI).
    ///
    /// A 429 is tried again, as containerd tries it (D131), but after a wait ([`throttle`]), and
    /// never for a quota spent: Docker Hub counts pulls over hours (§3.2). Where the
    /// registry names a window of a minute or less that its allowance is counted over, it
    /// is waited out within that window (D131). Returns the response and the method that
    /// got it.
    pub(crate) fn request<'a>(
        &self,
        method: &'a str,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<(Response<'_>, &'a str), Error> {
        self.send(method, url, headers, &[], None, None)
    }

    /// [`request`](Self::request), with a body: `body`, or so much of a file. A request of
    /// this registry's own repository goes to its mirrors first (D194), each in turn until
    /// one answers it with success; one that does not, or cannot be reached, is asked
    /// nothing more, and the registry itself is asked last. A file's bytes are counted into
    /// `sent` as they go, where given.
    fn send<'a>(
        &self,
        method: &'a str,
        url: &Url,
        headers: &[(&str, &str)],
        body: &[u8],
        file: Option<(&std::fs::File, u64, u64)>,
        sent: Option<&AtomicU64>,
    ) -> Result<(Response<'_>, &'a str), Error> {
        if let Some(rest) = url.as_str().strip_prefix(self.base.as_str()) {
            for mirror in &self.mirrors {
                if mirror.failed.load(Ordering::Acquire) {
                    continue;
                }
                let mirrored = Url::parse(&format!("{}{rest}", mirror.registry.base))?;
                match mirror
                    .registry
                    .send_here(method, &mirrored, headers, body, file, sent)
                {
                    Ok(answer) if (200..300).contains(&answer.0.status) => return Ok(answer),
                    // A manifest or blob it could not give: the pull's are asked of the
                    // next. A list it has no API for (referrers, tags) is this one's alone.
                    _ if rest.starts_with("manifests/") || rest.starts_with("blobs/") => {
                        mirror.failed.store(true, Ordering::Release);
                    }
                    _ => {}
                }
            }
        }
        self.send_here(method, url, headers, body, file, sent)
    }

    /// [`send`](Self::send), of this registry itself.
    fn send_here<'a>(
        &self,
        method: &'a str,
        url: &Url,
        headers: &[(&str, &str)],
        body: &[u8],
        file: Option<(&std::fs::File, u64, u64)>,
        sent: Option<&AtomicU64>,
    ) -> Result<(Response<'_>, &'a str), Error> {
        let mut method = method;
        let mut last: Option<u16> = None;
        let mut trouble = Trouble::default();
        let mut throttled = 0;
        let mut waited = Duration::ZERO;
        loop {
            let asked = Instant::now();
            // The registry's own host, once it answered HTTPS in plain HTTP, is asked so.
            let ours = |plain: &Plain| {
                url.scheme() == Scheme::Https
                    && url.host() == self.base.host()
                    && url.port() == self.base.port()
                    && plain.fallen.load(Ordering::Acquire)
            };
            let fallen = match &self.plain {
                Some(plain) if ours(plain) => Some(url.plain_at(plain.port)?),
                _ => None,
            };
            let answered = self.http.follow_counting(
                &Request {
                    method,
                    url: fallen.as_ref().unwrap_or(url),
                    headers,
                    body,
                    file,
                },
                &|hop| self.auth.authorization(&self.http, hop, &self.scopes),
                Redirects::Anywhere,
                sent,
            );
            let mut response = match answered {
                Ok(response) => response,
                Err(e)
                    if fallen.is_none()
                        && url.scheme() == Scheme::Https
                        && url.host() == self.base.host()
                        && url.port() == self.base.port()
                        && let Some(plain) = &self.plain
                        && (e.is_not_tls() || (e.is_refused() && plain.on_refusal)) =>
                {
                    // Asked again at once, as the fallback's round trip does.
                    plain.fallen.store(true, Ordering::Release);
                    continue;
                }
                // A timeout, a reset or a cut answer: ridden out while it lasts a moment. A
                // file's body is resumed by its upload instead, from what the registry has.
                // An overdue answer is asked again at once: its wait was drawn to end
                // where the pause would have (crate::patience `Cutoff`).
                Err(e) if e.kind() == ErrorKind::Transient && file.is_none() => {
                    match trouble.wait(asked, self.http.budget(url), None) {
                        Some(wait) => {
                            if !e.is_overdue() {
                                self.http.pause(wait)?;
                            }
                            continue;
                        }
                        None => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            };
            let repeated = last == Some(response.status);
            last = Some(response.status);
            let wait = match response.status {
                401 => self
                    .auth
                    .challenged(response.url(), &response, repeated)?
                    .then_some(Duration::ZERO),
                405 if method == "HEAD" && url.target().contains("/manifests/") => {
                    method = "GET";
                    Some(Duration::ZERO)
                }
                429 => {
                    // What the registry says of it, read ahead: a refusal passed on still
                    // says it.
                    let said = response.peek(2 << 10).map(<[u8]>::to_vec).unwrap_or_default();
                    let wait = throttle(&response, &said, throttled, waited);
                    if let Some(wait) = wait {
                        throttled += 1;
                        waited += wait;
                        // A HEAD's refusal says nothing: a manifest's is asked again as a
                        // GET, whose refusal says how long the throttle may last, as
                        // containerd asks a refused HEAD's body with a GET.
                        if method == "HEAD" && url.target().contains("/manifests/") {
                            method = "GET";
                        }
                    }
                    wait
                }
                // What the registry or its gateway says is passing (RFC 9110 §15.5.9,
                // §15.6.3 to §15.6.5): ridden out while it lasts a moment, waited as long
                // as it names.
                408 | 502 | 503 | 504 => trouble.wait(asked, self.http.budget(url), retry_after(&response)),
                // An unexpected condition, which RFC 9110 §15.6.1 does not say passes:
                // asked again once, as it may have been the moment's, but not past a
                // repeat, which is the request's own.
                500 if !repeated => trouble.wait(asked, self.http.budget(url), retry_after(&response)),
                _ => None,
            };
            let Some(wait) = wait else {
                trouble.over(&self.http, url);
                return Ok((response, method));
            };
            // Read what little a refusal carries, so its connection can be used again.
            let _ = io::copy(&mut (&mut response).take(2 << 10), &mut io::sink());
            drop(response);
            if !wait.is_zero() {
                self.http.pause(wait)?;
            }
        }
    }

    /// A refusal of a `method` request to `url`, as [`unexpected`] words it. A HEAD's 403
    /// has no body, so the URL is asked for again with a GET; if that is refused with a
    /// 403 as well, its body says what the HEAD's refusal does, as containerd v2.4.1 has
    /// it (`withGETErrorBody`).
    fn refused_head(&self, method: &str, response: Response, url: &Url, headers: &[(&str, &str)]) -> Error {
        if method != "HEAD" || response.status != 403 {
            return unexpected(method, response);
        }
        let refusal = Refusal::of(&response);
        drop(response);
        let mut body = Vec::new();
        if let Ok((mut get, _)) = self.request("GET", url, headers)
            && get.status == 403
        {
            let _ = (&mut get).take(MAX_ERROR_BODY).read_to_end(&mut body);
        }
        refusal.error(method, &body)
    }

    /// The repository's tags, as distribution's client lists them for `pull -a`
    /// (registry/client/repository.go, `tags.All`): `tags/list`, then each page its
    /// `Link` names, resolved against the page before, until one names none. A page that
    /// names one already read is refused: it would never end. A name that is no tag
    /// (`reference.WithTag` refuses it) is passed over, as dockerd passes it over for
    /// `pull -a` (Docker 29.3.1, measured): it would steer the manifest's URL off the
    /// repository (`../`, `//host`), and name an image as no reference can.
    pub fn tags(&self) -> Result<Vec<String>, Error> {
        #[derive(serde::Deserialize)]
        struct Page {
            #[serde(default)]
            tags: Option<Vec<String>>,
        }
        let mut url = self.base.join("tags/list")?;
        let mut read = std::collections::HashSet::new();
        let mut tags = Vec::new();
        loop {
            if !read.insert(url.as_str().to_string()) {
                return Err(Error::new(format!(
                    "{url}: the tags' pages name one another in a loop"
                )));
            }
            let (mut response, _) = self.request("GET", &url, &[("Accept", "application/json")])?;
            if !(200..300).contains(&response.status) {
                return Err(unexpected("GET", response));
            }
            let next = response.header("link").map(|link| {
                link.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            });
            let mut body = Vec::new();
            (&mut response).take(MAX_TAGS_PAGE + 1).read_to_end(&mut body)?;
            if body.len() as u64 > MAX_TAGS_PAGE {
                return Err(Error::new(format!(
                    "{url}: a page of tags past {MAX_TAGS_PAGE} bytes"
                )));
            }
            let page: Page = serde_json::from_slice(&body).map_err(|e| Error::new(format!("{url}: {e}")))?;
            tags.extend(page.tags.unwrap_or_default().into_iter().filter(|t| is_tag(t)));
            match next.filter(|n| !n.is_empty()) {
                Some(next) => url = url.join(&next)?,
                None => return Ok(tags),
            }
        }
    }

    /// Resolves `reference` to a descriptor, as containerd's `Resolve` does (D20):
    /// - a HEAD of `manifests/<tag or digest>` with its `Accept` list, then of
    ///   `blobs/<digest>` for a digest, and only after a 404;
    /// - the digest comes from the reference, or else from `Docker-Content-Digest` with a
    ///   `Content-Length`. Without them the manifest is fetched and hashed, and kept, so it
    ///   is not fetched again (R8).
    pub fn resolve(&self, store: &Store, reference: &Reference) -> Result<Descriptor, Error> {
        let name = reference.familiar();
        let paths = match &reference.digest {
            Some(d) => vec![format!("manifests/{d}"), format!("blobs/{d}")],
            None => vec![format!(
                "manifests/{}",
                reference.tag.as_deref().unwrap_or("latest")
            )],
        };
        let accept = [("Accept", RESOLVE_ACCEPT), ("Accept-Encoding", "identity")];
        for path in &paths {
            let url = self.base.join(path)?;
            let (response, method) = self.request("HEAD", &url, &accept)?;
            match response.status {
                200..=299 => {}
                404 => continue,
                _ => return Err(self.refused_head(method, response, &url, &accept)),
            }
            let size = response
                .header("content-length")
                .and_then(|v| v.trim().parse::<u64>().ok());
            let mut digest = reference.digest.clone();
            if digest.is_none()
                && size.is_some()
                && let Some(header) = response.header("docker-content-digest")
            {
                digest = Some(
                    Digest::parse(header.trim())
                        .map_err(|e| Error::new(format!("{name}: Docker-Content-Digest: {e}")))?,
                );
            }
            let (digest, size, media_type) = match (digest, size) {
                (Some(digest), Some(size)) if method == "HEAD" => (digest, size, manifest_type(&response)),
                (digest, _) => {
                    let mut get = if method == "GET" {
                        response
                    } else {
                        self.request("GET", &url, &accept)?.0
                    };
                    if !(200..400).contains(&get.status) {
                        return Err(unexpected("GET", get));
                    }
                    let media_type = manifest_type(&get);
                    if media_type == media::DOCKER_SCHEMA1_SIGNED || media_type == media::DOCKER_SCHEMA1 {
                        return Err(Error::new(format!(
                            "{name}: schema 1 manifests are not supported: push the image again, as schema 2 or OCI"
                        )));
                    }
                    let body = read_capped(&mut get, MAX_MANIFEST, &name)?;
                    let digest = digest
                        .unwrap_or_else(|| Digest::from_hash(Algorithm::Sha256, &Sha256::digest(&body)));
                    // The reference's digest is checked here too: `ingest` verifies it.
                    store
                        .ingest(&digest, body.len() as u64, &mut &body[..])
                        .map_err(|e| Error::new(format!("{name}: {e}")))?;
                    (digest, body.len() as u64, media_type)
                }
            };
            if size > MAX_MANIFEST {
                return Err(Error::new(format!("{name}: rejecting a {size}-byte manifest")));
            }
            return Ok(Descriptor {
                media_type,
                digest: digest.to_string(),
                size: i64::try_from(size).map_err(|_| Error::new(format!("{name}: a size past i64")))?,
                platform: None,
                annotations: Default::default(),
            });
        }
        // containerd's words: the reference as it was normalized.
        Err(Error::of(ErrorKind::NotFound, format!("{reference}: not found")))
    }

    /// An index or manifest by digest: from the store if it is there, else fetched and
    /// kept, in place of a stored copy that has changed. Either way it is checked against
    /// the descriptor's digest and size.
    pub fn fetch_document(&self, store: &Store, desc: &Descriptor) -> Result<Vec<u8>, Error> {
        let digest = desc.digest().map_err(|e| Error::new(e.to_string()))?;
        let size = desc.size().map_err(|e| Error::new(e.to_string()))?;
        if size > MAX_MANIFEST {
            return Err(Error::new(format!("{digest}: rejecting a {size}-byte manifest")));
        }
        let changed = match store.held(desc, MAX_MANIFEST)? {
            Held::Whole(bytes) => return Ok(bytes),
            Held::Invalid(why) => return Err(Error::new(why)),
            Held::Missing => false,
            Held::Changed(_) => true,
        };
        let url = self.base.join(&format!("manifests/{digest}"))?;
        let accept = accept(&desc.media_type);
        let (mut response, _) = self.request(
            "GET",
            &url,
            &[("Accept", &accept), ("Accept-Encoding", ENCODINGS)],
        )?;
        if !(200..300).contains(&response.status) {
            return Err(not_fetched(response, &url));
        }
        let encoding = response
            .header("content-encoding")
            .unwrap_or_default()
            .to_string();
        let mut body = decoded(&mut response, &encoding)?;
        let ingested = if changed {
            store.ingest_again(&digest, size, &mut body)
        } else {
            store.ingest(&digest, size, &mut body)
        };
        ingested.map_err(|e| Error::new(e.to_string()))?;
        store
            .content(desc, MAX_MANIFEST)?
            .ok_or_else(|| Error::new(format!("{digest}: gone from the store")))
    }

    /// The referrers of `digest` as containerd's FetchReferrers asks for them (D116): the
    /// Referrers API (`referrers/<digest>`, with the artifact types and filters as its
    /// query), then the Referrers Tag Schema (`manifests/sha256-<hex>`) where the API is
    /// not found or refuses; an empty index where neither is. An API that could not answer
    /// then (no answer past the request's retries, 408, 429 or a 5xx) is no API that has
    /// none: where the tag is not found either, that failure is returned, never an empty
    /// list, which a policy would read as no signature (D116). The index's bytes, at most
    /// a manifest's size.
    pub fn referrers(
        &self,
        digest: &str,
        artifact_types: &[&str],
        filters: &[(&str, &str)],
    ) -> Result<Vec<u8>, Error> {
        let accept = [
            ("Accept", "application/vnd.oci.image.index.v1+json, */*"),
            ("Accept-Encoding", ENCODINGS),
        ];
        let mut url = self.base.join(&format!("referrers/{digest}"))?;
        for t in artifact_types {
            url = url.with_query_pair("artifactType", t)?;
        }
        for (k, v) in filters {
            url = url.with_query_pair(k, v)?;
        }
        let tag = digest.replacen(':', "-", 1);
        // The API's failure is kept, the tag tried anyway; the tag's 404 means none, but
        // where the API could not answer, which leaves what it lists unknown.
        let mut first: Option<Error> = None;
        let mut unanswered = false;
        for (fallback, url) in [(false, url), (true, self.base.join(&format!("manifests/{tag}"))?)] {
            let (mut response, _) = match self.request("GET", &url, &accept) {
                Ok(r) => r,
                Err(e) => {
                    if fallback {
                        return Err(first.unwrap_or(e));
                    }
                    first = Some(e);
                    unanswered = true;
                    continue;
                }
            };
            match response.status {
                200..=299 => {
                    let encoding = response
                        .header("content-encoding")
                        .unwrap_or_default()
                        .to_string();
                    let mut body = decoded(&mut response, &encoding)?;
                    let mut bytes = Vec::new();
                    body.by_ref()
                        .take(MAX_MANIFEST.saturating_add(1))
                        .read_to_end(&mut bytes)?;
                    if bytes.len() as u64 > MAX_MANIFEST {
                        return Err(Error::of(
                            ErrorKind::NotFound,
                            format!(
                                "referrers index size {} exceeds maximum allowed {MAX_MANIFEST}: not found",
                                bytes.len()
                            ),
                        ));
                    }
                    return Ok(bytes);
                }
                404 if fallback => break,
                404 => continue,
                status => {
                    let e = not_fetched(response, &url);
                    if fallback {
                        return Err(first.unwrap_or(e));
                    }
                    unanswered = matches!(status, 408 | 429 | 500 | 502 | 503 | 504);
                    first = Some(e);
                }
            }
        }
        if unanswered && let Some(e) = first {
            return Err(e);
        }
        Ok(
            br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#
                .to_vec(),
        )
    }

    /// Where the blob `digest` is at this registry.
    pub(crate) fn blob_url(&self, digest: &Digest) -> Result<Url, Error> {
        self.base.join(&format!("blobs/{digest}"))
    }

    /// The blob `desc` names, whole, checked against its digest and size, refused past
    /// `most` bytes: a small document's (an image's config), never stored.
    pub(crate) fn small_blob(&self, desc: &Descriptor, most: u64) -> Result<Vec<u8>, Error> {
        let digest = desc.digest().map_err(|e| Error::new(e.to_string()))?;
        let size = desc.size().map_err(|e| Error::new(e.to_string()))?;
        if size > most {
            return Err(Error::new(format!("{digest}: rejecting a {size}-byte document")));
        }
        let url = self.blob_url(&digest)?;
        let (mut response, _) = self.request("GET", &url, &[("Accept", &accept(&desc.media_type))])?;
        if !(200..300).contains(&response.status) {
            return Err(not_fetched(response, &url));
        }
        let mut bytes = Vec::new();
        (&mut response)
            .take(size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        let got = format!(
            "sha256:{}",
            Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        if bytes.len() as u64 != size || got != digest.to_string() {
            return Err(Error::new(format!(
                "{digest}: the registry sent {} bytes of digest {got}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// A registry asked of its host itself, at `base` (`scheme://host/v2/`), for `scopes`:
    /// its catalog, or a search its host serves beside it.
    pub(crate) fn of_host(
        http: Client,
        base: Url,
        credentials: Credentials,
        scopes: Vec<String>,
    ) -> Result<Registry, Error> {
        Registry::at(http, base, Authorizer::new_for(credentials), scopes)
    }

    /// What a request to `url` is authorized by, as this registry's requests are.
    pub(crate) fn authorization(&self, url: &Url) -> Result<Option<String>, Error> {
        self.auth.authorization(&self.http, url, &self.scopes)
    }

    pub(crate) fn client(&self) -> &Client {
        &self.http
    }

    /// A GET of the bytes `lo..hi` of the blob at `url`, as it is stored: no encoding for
    /// the transfer, whose range would not be one of the blob's. A 206 that says it holds
    /// other bytes is refused; one that does not say is taken at its word, as a resumed
    /// [`fetch_blob`](Self::fetch_blob) takes it. `url` is the registry's, or where it
    /// redirected an earlier range of the blob.
    pub(crate) fn get_range(
        &self,
        url: &Url,
        media_type: &str,
        lo: u64,
        hi: u64,
    ) -> Result<Response<'_>, Error> {
        let range = format!("bytes={lo}-{}", hi.saturating_sub(1));
        let accept = accept(media_type);
        let headers = [
            ("Accept", accept.as_str()),
            ("Accept-Encoding", "identity"),
            ("Range", range.as_str()),
        ];
        let (response, _) = self.request("GET", url, &headers)?;
        if response.status == 206
            && let Some(range) = response.header("content-range")
            && !range.starts_with(&format!("bytes {lo}-"))
        {
            return Err(Error::new(format!("{url}: an unexpected range {range:?}")));
        }
        Ok(response)
    }

    /// A blob by digest into the store, verified. An interrupted download resumes with
    /// `Range: bytes=<offset>-`, as containerd's resumes do (D20). A server that ignores the
    /// range sends the whole blob, and the download starts over. `progress` is told the
    /// bytes as they arrive.
    pub fn fetch_blob(
        &self,
        store: &Store,
        desc: &Descriptor,
        limits: &Limits,
        progress: &dyn Fn(u64),
    ) -> Result<(), Error> {
        self.fetch_blob_as(store, desc, limits, progress, false)
    }

    /// [`fetch_blob`](Self::fetch_blob), in place of a stored copy that has changed.
    pub fn fetch_blob_again(
        &self,
        store: &Store,
        desc: &Descriptor,
        limits: &Limits,
        progress: &dyn Fn(u64),
    ) -> Result<(), Error> {
        self.fetch_blob_as(store, desc, limits, progress, true)
    }

    fn fetch_blob_as(
        &self,
        store: &Store,
        desc: &Descriptor,
        limits: &Limits,
        progress: &dyn Fn(u64),
        again: bool,
    ) -> Result<(), Error> {
        self.keep_in(store);
        let digest = desc.digest().map_err(|e| Error::new(e.to_string()))?;
        let size = desc.size().map_err(|e| Error::new(e.to_string()))?;
        let started = if again {
            store.download_again(&digest, size, limits)
        } else {
            store.download(&digest, size, limits)
        };
        let Some(mut download) = started.map_err(|e| Error::new(e.to_string()))? else {
            return Ok(());
        };
        let url = self.base.join(&format!("blobs/{digest}"))?;
        let accept = accept(&desc.media_type);
        let mut trouble = Trouble::default();
        while download.offset() < size {
            let asked = Instant::now();
            let offset = download.offset();
            let range = format!("bytes={offset}-");
            // A resumed download asks for the blob as it is: a range of an encoding of it
            // would not be one of the bytes stored so far.
            let mut headers = vec![("Accept", accept.as_str())];
            if offset > 0 {
                headers.push(("Accept-Encoding", "identity"));
                headers.push(("Range", &range));
            } else {
                headers.push(("Accept-Encoding", ENCODINGS));
            }
            let (mut response, _) = self.request("GET", &url, &headers)?;
            let whole = response.status == 200;
            match response.status {
                206 => {
                    if let Some(range) = response.header("content-range")
                        && !range.starts_with(&format!("bytes {offset}-"))
                    {
                        return Err(Error::new(format!("{digest}: an unexpected range {range:?}")));
                    }
                }
                200 if offset > 0 => {
                    download.restart().map_err(|e| Error::new(e.to_string()))?;
                }
                200 => {}
                _ => return Err(not_fetched(response, &url)),
            }
            let before = download.offset();
            let encoding = response
                .header("content-encoding")
                .unwrap_or_default()
                .to_string();
            let copied = decoded(&mut response, &encoding)
                .and_then(|mut body| copy(&mut body, &mut download, progress));
            let ended = match copied {
                Ok(()) => true,
                Err(e) if e.kind() == ErrorKind::Transient => false,
                Err(e) => return Err(e.context(&digest)),
            };
            drop(response);
            // An answer that ended as its framing said is the registry's own, not a
            // passing trouble: the whole blob, short of its size, is shorter than its
            // descriptor; a range that brought nothing will bring nothing.
            if ended && download.offset() < size && (whole || download.offset() == before) {
                return Err(short(&digest, download.offset(), desc.size));
            }
            if download.offset() > before {
                trouble.over(&self.http, &url);
                continue;
            }
            // It brought nothing: resumed while that lasts its path's budget.
            match trouble.wait(asked, self.http.budget(&url), None) {
                Some(wait) => self.http.pause(wait)?,
                None => return Err(stalled(&digest, trouble.lasted())),
            }
        }
        download.commit().map(drop).map_err(|e| Error::new(e.to_string()))
    }

    /// Waits `wait` before asking this registry again, or until its client is cancelled.
    pub(crate) fn pause(&self, wait: Duration) -> Result<(), Error> {
        self.http.pause(wait)
    }

    /// Keeps what the paths this registry's requests go over show in `store` (D222), and
    /// starts them from what they showed before.
    pub(crate) fn keep_in(&self, store: &Store) {
        self.http.keep_in(store.root());
    }

    /// How long a trouble of `url`'s server is ridden out.
    pub(crate) fn budget(&self, url: &Url) -> Duration {
        self.http.budget(url)
    }

    /// A trouble of `url`'s server ended, the asking of `url` answered.
    pub(crate) fn over(&self, trouble: &mut Trouble, url: &Url) {
        trouble.over(&self.http, url);
    }
}

/// A download that brought nothing for its path's budget (D222).
pub(crate) fn stalled(digest: &Digest, lasted: Duration) -> Error {
    Error::of(
        ErrorKind::Transient,
        format!(
            "{digest}: the download brought nothing for {:.1} s",
            lasted.as_secs_f64()
        ),
    )
}

/// A blob whose registry's answer, ended as its framing said, ends at `at` bytes, short of
/// its descriptor's `size`: the registry's own answer, which asking again does not change
/// (D222).
pub(crate) fn short(digest: &Digest, at: u64, size: i64) -> Error {
    Error::new(format!(
        "{digest}: the registry's blob ends at {at} bytes, where its descriptor says {size}"
    ))
}

/// Whether `name` is a tag: distribution's `anchoredTagRegexp` (reference/regexp.go),
/// `[\w][\w.-]{0,127}`, Go's `\w` being ASCII letters, digits and `_`.
fn is_tag(name: &str) -> bool {
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    let bytes = name.as_bytes();
    bytes.len() <= 128
        && bytes.first().is_some_and(word)
        && bytes.iter().all(|c| word(c) || matches!(c, b'.' | b'-'))
}

/// `<media type>, */*`, as containerd asks for content by descriptor (D20).
fn accept(media_type: &str) -> String {
    if media_type.is_empty() {
        "*/*".to_string()
    } else {
        format!("{media_type}, */*")
    }
}

/// containerd's `getManifestMediaType`: `Content-Type` without its parameters, and
/// `text/plain`, which an old Red Hat registry served, taken for schema 1.
fn manifest_type(response: &Response) -> String {
    let content_type = response.header("content-type").unwrap_or_default();
    let content_type = content_type.split(';').next().unwrap_or_default();
    if content_type == "text/plain" {
        media::DOCKER_SCHEMA1_SIGNED.to_string()
    } else {
        content_type.to_string()
    }
}

/// A body of at most `max` bytes.
fn read_capped(response: &mut Response, max: u64, what: &dyn fmt::Display) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    response.take(max.saturating_add(1)).read_to_end(&mut body)?;
    if body.len() as u64 > max {
        return Err(Error::new(format!(
            "{what}: rejecting a manifest over {max} bytes"
        )));
    }
    Ok(body)
}

/// What content a fetch takes encoded for its transfer, as containerd v2.4.1's fetcher
/// asks for it (core/remotes/docker/fetcher.go, open).
pub(crate) const ENCODINGS: &str = "zstd;q=1.0, gzip;q=0.8, deflate;q=0.5";

/// A response body as its `Content-Encoding` says to decode it, (D20), as containerd's fetcher
/// decodes one: each coding undone, last first; zstd, gzip (every member, as Go's reader
/// reads them), deflate (raw, RFC 1951, as Go's flate reads it), or none.
///
/// Each coding is undone once: one listed again is refused, where containerd nests
/// another decoder for it. A coding applied twice compresses nothing, and every decoder
/// nested is its buffers and a level of recursion in each read: a header listing gzip
/// 50,000 times (300 KB, well within a head's 10 MiB) overflowed a fetch thread's stack,
/// which ends the daemon. So at most three decoders, whatever the header lists.
pub(crate) fn decoded<'a>(body: &'a mut dyn Read, encoding: &str) -> Result<Box<dyn Read + 'a>, Error> {
    const CODINGS: [&str; 3] = ["zstd", "gzip", "deflate"];
    let mut undone = [false; CODINGS.len()];
    let mut body: Box<dyn Read + 'a> = Box::new(body);
    for coding in encoding.rsplit([' ', '\t', ',']).filter(|c| !c.is_empty()) {
        if coding.eq_ignore_ascii_case("identity") {
            continue;
        }
        let Some(k) = CODINGS.iter().position(|c| coding.eq_ignore_ascii_case(c)) else {
            return Err(Error::new(format!(
                "unsupported Content-Encoding algorithm: {}",
                coding.to_ascii_lowercase()
            )));
        };
        let seen = undone
            .get_mut(k)
            .ok_or_else(|| Error::new("a Content-Encoding out of range"))?;
        if std::mem::replace(seen, true) {
            return Err(Error::new(format!(
                "Content-Encoding lists {} twice: shards undoes each coding once",
                CODINGS.get(k).copied().unwrap_or_default()
            )));
        }
        body = match k {
            0 => Box::new(shards_image::store::Zstd::new(io::BufReader::new(body))),
            1 => Box::new(flate2::read::MultiGzDecoder::new(body)),
            _ => Box::new(flate2::read::DeflateDecoder::new(body)),
        };
    }
    Ok(body)
}

/// Copies a response body into a download, telling `progress` what arrived.
fn copy(response: &mut dyn Read, download: &mut Download, progress: &dyn Fn(u64)) -> Result<(), Error> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match response.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        download
            .write(buf.get(..n).unwrap_or_default())
            .map_err(|e| Error::new(e.to_string()))?;
        progress(n as u64);
    }
}

/// What a refusal's error names of its response.
struct Refusal {
    url: String,
    status: u16,
    text: String,
    /// A 429's rate limits (§3.2), where the registry gave any.
    limits: Option<String>,
}

impl Refusal {
    fn of(response: &Response) -> Refusal {
        Refusal {
            url: response.url().to_string(),
            status: response.status,
            text: printable(&response.status_text()),
            limits: (response.status == 429).then(|| rate_limits(response)).flatten(),
        }
    }

    /// The refusal of a `method` request, in containerd v2.4.1's words
    /// (`docker.unexpectedResponseErr`), then a 429's rate limits, with what dockerd
    /// says of it from `body` ([`dockerd`]). The URL is the one the request ended at,
    /// with its query hidden: containerd prints it, and with it a CDN's signature or an
    /// upload's state.
    fn error(self, method: &str, body: &[u8]) -> Error {
        let mut message = format!(
            "unexpected status from {method} request to {}: {}",
            self.url, self.text
        );
        let mut said = refusal_said(self.status, body);
        if let Some(limits) = self.limits {
            let limits = format!(" ({limits})");
            message.push_str(&limits);
            if let Said::Instead(said) = &mut said {
                said.push_str(&limits);
            }
        }
        Error::new(message).said(said)
    }
}

/// A response to a `method` request that is not a success, as [`Refusal::error`] words
/// it, from as much of its body as containerd reads (D20).
fn unexpected(method: &str, mut response: Response) -> Error {
    let refusal = Refusal::of(&response);
    let mut body = Vec::new();
    let _ = (&mut response).take(MAX_ERROR_BODY).read_to_end(&mut body);
    refusal.error(method, &body)
}

/// A fetch's refusal, as containerd words it (D20; `withErrorCheck`): a 404 is no content at
/// `url`, the URL asked.
pub(crate) fn not_fetched(response: Response, url: &Url) -> Error {
    if response.status == 404 {
        return Error::of(
            ErrorKind::NotFound,
            format!("content at {url} not found: not found"),
        );
    }
    unexpected("GET", response)
}

/// One of the errors in distribution-spec's error body (spec.md:786-825):
/// `{"errors":[{"code","message","detail"}]}`.
#[derive(Default)]
struct ErrorEntry {
    code: Option<String>,
    message: String,
    detail: Option<serde_json::Value>,
}

/// The errors of a refusal's body, as Go decodes it into containerd's `Errors`
/// (errcode.go): `null` has none, and so has an object without them or with `null` for
/// them; an error of `null` has nothing in it. None where the body is not distribution's
/// object: not JSON, or of other types.
fn errors_in(body: &serde_json::Value) -> Option<Vec<ErrorEntry>> {
    use serde_json::Value;
    let text = |v: Option<&Value>| match v {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None,
    };
    let errors = match body {
        Value::Null => return Some(Vec::new()),
        Value::Object(body) => body.get("errors"),
        _ => return None,
    };
    match errors {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(errors)) => errors
            .iter()
            .map(|e| match e {
                Value::Null => Some(ErrorEntry::default()),
                Value::Object(e) => Some(ErrorEntry {
                    code: text(e.get("code"))?,
                    message: text(e.get("message"))?.unwrap_or_default(),
                    detail: e.get("detail").filter(|d| !d.is_null()).cloned(),
                }),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

/// The error codes containerd knows, and their messages (errdesc.go). Any other code is
/// UNKNOWN.
const CODES: [(&str, &str); 5] = [
    ("UNSUPPORTED", "The operation is unsupported."),
    UNAUTHORIZED,
    DENIED,
    ("UNAVAILABLE", "service unavailable"),
    ("TOOMANYREQUESTS", "too many requests"),
];
const UNAUTHORIZED: (&str, &str) = ("UNAUTHORIZED", "authentication required");
const DENIED: (&str, &str) = ("DENIED", "requested access to the resource is denied");
const UNKNOWN: (&str, &str) = ("UNKNOWN", "unknown error");

/// What dockerd says of a refusal with this status and body (`translateRegistryError`,
/// daemon/containerd/registry_errors.go):
/// - Errors in the body: `error from registry: `, then one line each. A line is the
///   error's message, or its code's where it has none, then ` - ` and its detail where
///   that is text. An error with neither, or with its code's message alone, is its code
///   (`unauthorized`), which dockerd says twice (it appends it in both branches); we
///   say it once.
/// - None, but a 401's or 403's token-server `details`: its code's message, then those.
/// - None otherwise: `error from registry` before the whole error; `unknown` before it
///   where the body is not distribution's object, as a HEAD's never is.
///
/// Codes are distribution's strings, as the spec has them; Go would read a number too.
/// What the registry says is printed with its control characters escaped.
fn refusal_said(status: u16, body: &[u8]) -> Said {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Said::Before("unknown");
    };
    let Some(errors) = errors_in(&body) else {
        return Said::Before("unknown");
    };
    if errors.is_empty() {
        // A token server's `{"details": …}`, read as Go reads it into a struct.
        let details = match body.get("details") {
            Some(serde_json::Value::String(details)) if !details.is_empty() => Some(details),
            _ => None,
        };
        return match (status, details) {
            (401 | 403, Some(details)) => {
                let code = if status == 401 { UNAUTHORIZED } else { DENIED };
                Said::Instead(format!("{} - {}", code.1, printable(details)))
            }
            _ => Said::Before("error from registry"),
        };
    }
    let lines = errors
        .into_iter()
        .map(|entry| {
            let (code, code_message) = entry
                .code
                .as_deref()
                .and_then(|c| CODES.iter().find(|(known, _)| *known == c))
                .copied()
                .unwrap_or(UNKNOWN);
            let message = entry.message;
            if entry.detail.is_none() && (message.is_empty() || message == code_message) {
                return code.to_lowercase();
            }
            let mut line = if message.is_empty() {
                code_message.to_string()
            } else {
                printable(&message)
            };
            if let Some(serde_json::Value::String(detail)) = entry.detail {
                line.push_str(" - ");
                line.push_str(&printable(&detail));
            }
            line
        })
        .collect::<Vec<_>>();
    Said::Instead(format!("error from registry: {}", lines.join("\n")))
}

/// Docker Hub's rate-limit fields, as its documentation names them: `ratelimit-limit`
/// and `ratelimit-remaining` (`<count>;w=<seconds>`), `docker-ratelimit-source`, and
/// `Retry-After`. None where it gave none of them.
/// How long to wait before asking again after the `n`th 429 in a row, or `None` to give
/// up: a `Retry-After` longer than the waits left is never waited out, nor a quota spent
/// (Docker Hub's `ratelimit-remaining` at 0, its window hours long) that names no wait
/// that fits; a throttle is.
fn throttle(response: &Response, said: &[u8], n: usize, waited: Duration) -> Option<Duration> {
    // A window of a minute or less named for the allowance: a crowd's throttle, over by the
    // window's end, so waited out within it, the waits growing with it past the schedule. ghcr.io
    // throttled CI's pulls past the schedule's 1.55 s, naming `allowed: 44000/minute`
    // (PM M191).
    let window = allowance_window(response, said).filter(|w| *w <= WINDOW_MOST);
    let most = match (THROTTLED.get(n), window) {
        (Some(most), _) => *most,
        // Past the schedule, a wait is a share of how long the throttle has lasted (D222's
        // rule for a trouble: the time lost past its end at most what it had lasted), and
        // never past what is left of its window, below.
        (None, Some(_)) => waited.max(THROTTLED.last().copied().unwrap_or_default()),
        (None, None) => return None,
    };
    let left = match window {
        Some(w) => w.saturating_sub(waited),
        None => THROTTLED.iter().skip(n).sum(),
    };
    // Full jitter: a share of the most, at random, so a pull's requests do not come back
    // together; at least that, however little the registry names, as `Retry-After` is how
    // long a client ought to wait (RFC 9110 §10.2.3): ghcr.io named under a millisecond in
    // a 429's body (`retry-after: 900.208µs`), and a pull failed on it.
    let share = most.mul_f64(jitter()).min(left);
    match response.header("retry-after").map(|v| v.trim().parse::<u64>()) {
        // The wait it names, where the waits left hold it, a quota spent or not: it says
        // when.
        Some(Ok(secs)) => Some(Duration::from_secs(secs))
            .filter(|d| *d <= left)
            .map(|d| d.max(share)),
        // None named, an HTTP date or nothing readable: waited as one naming no wait,
        // but a quota spent, unless the window it is spent in is the short one named.
        _ => {
            let spent = window.is_none()
                && response
                    .header("ratelimit-remaining")
                    .and_then(|v| v.split(';').next())
                    .is_some_and(|left| left.trim() == "0");
            (!spent && !left.is_zero()).then_some(share)
        }
    }
}

/// The window a throttling registry counts its allowance over, where it names one: the
/// `w` of its `RateLimit-Limit` field (`100;w=21600`, Docker Hub's, in seconds), or its
/// refusal's words, `allowed: N/second` (or `minute`, `hour`, `day`), as ghcr.io's say it
/// (`retry-after: 301.718µs, allowed: 44000/minute`).
fn allowance_window(response: &Response, said: &[u8]) -> Option<Duration> {
    if let Some(w) = response
        .header("ratelimit-limit")
        .and_then(|v| v.split(';').find_map(|p| p.trim().strip_prefix("w=")))
        .and_then(|w| w.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(w));
    }
    let text = String::from_utf8_lossy(said);
    let rest = text.split("allowed: ").nth(1)?;
    let (count, unit) = rest.split_once('/')?;
    count.trim().parse::<u64>().ok()?;
    let unit: String = unit.chars().take_while(char::is_ascii_alphabetic).collect();
    let secs = match unit.as_str() {
        "second" => 1,
        "minute" => 60,
        "hour" => 3600,
        "day" => 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// A registry's trouble, ridden out while it lasts a moment (D222, PM M486): each failure
/// that may pass is asked again after a wait, a random share of how long the trouble has
/// lasted so far (full jitter: a share of the failure's own length, so that the time lost
/// past its end is at most what it had already lasted, and a crowd's asks do not come back
/// together), at least what the registry names in `Retry-After` (RFC 9110 §10.2.3); until
/// it has lasted its path's budget in all (crate::patience: twice the longest trouble the
/// path passed, at least TCP's R2), or a wait it names would end past it. No count: a
/// trouble of many quick failures (a gateway's cut connections) is ridden out as one of a
/// few slow ones (a timeout's), for as long.
#[derive(Debug, Default)]
pub(crate) struct Trouble {
    /// When its first failed attempt began.
    began: Option<Instant>,
}

impl Trouble {
    /// The trouble is over, the asking of `url` answered: what fails next begins another.
    /// One that had begun passed, and its path learns how long it lasted (D222).
    pub(crate) fn over(&mut self, http: &Client, url: &Url) {
        if let Some(began) = self.began.take() {
            http.passed(url, began.elapsed());
        }
    }

    /// How long it has lasted.
    pub(crate) fn lasted(&self) -> Duration {
        self.began.map_or(Duration::ZERO, |b| b.elapsed())
    }

    /// After an attempt begun at `attempt` failed: how long to wait before asking again,
    /// or `None` where the trouble has lasted `budget`, or the registry names a wait
    /// (`after`) past it.
    pub(crate) fn wait(
        &mut self,
        attempt: Instant,
        budget: Duration,
        after: Option<Duration>,
    ) -> Option<Duration> {
        let began = *self.began.get_or_insert(attempt);
        let lasted = began.elapsed();
        let left = budget.checked_sub(lasted).filter(|l| !l.is_zero())?;
        let share = lasted.mul_f64(jitter());
        match after {
            Some(after) if after > left => None,
            Some(after) => Some(after.max(share).min(left)),
            None => Some(share.min(left)),
        }
    }
}

/// The wait a refusal names in `Retry-After`, in seconds (RFC 9110 §10.2.3); an HTTP date
/// is not read, and names none.
fn retry_after(response: &Response) -> Option<Duration> {
    response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// A number in [0, 1), from the process's random hash keys and the time.
pub(crate) fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

fn rate_limits(response: &Response) -> Option<String> {
    let window = |v: &str| match v.split_once(";w=") {
        Some((count, seconds)) => format!("{} per {} s", count.trim(), seconds.trim()),
        None => v.trim().to_string(),
    };
    let mut facts = Vec::new();
    if let Some(limit) = response.header("ratelimit-limit") {
        facts.push(format!("limit {}", window(limit)));
    }
    if let Some(left) = response.header("ratelimit-remaining") {
        facts.push(format!(
            "{} left",
            left.split(';').next().unwrap_or_default().trim()
        ));
    }
    if let Some(source) = response.header("docker-ratelimit-source") {
        facts.push(format!("counted for {}", source.trim()));
    }
    if let Some(after) = response.header("retry-after") {
        facts.push(format!("retry after {}", after.trim()));
    }
    (!facts.is_empty()).then(|| printable(&facts.join("; ")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::testing::{After, Seen, route};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn http(status: &str, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (n, v) in fields {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str("Content-Length: 0\r\n\r\n");
        out.into_bytes()
    }

    /// Uploads a blob to a registry that answers each request with `answer`: whether it
    /// was mounted, and the requests' first lines.
    fn uploaded(
        from: Option<&str>,
        answer: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static,
    ) -> (Result<bool, Error>, Vec<String>) {
        let blob = b"blob bytes";
        let digest = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(blob));
        // A file of each call's own: the tests run at once in one process.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path_dir = shards_testdir::TempDir::new(&format!("upload-{n}")).unwrap();
        let path = path_dir.join("it");
        std::fs::write(&path, blob).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let server = route(None, move |seen| Some((answer(seen), After::Keep)));
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let desc = Descriptor {
            media_type: "application/octet-stream".into(),
            digest: digest.to_string(),
            size: i64::try_from(blob.len()).unwrap(),
            platform: None,
            annotations: Default::default(),
        };
        let done = registry.upload(&desc, &file, from, None);
        let _ = std::fs::remove_file(&path);
        let lines = server
            .requests()
            .iter()
            .map(|r| r.lines().next().unwrap_or_default().to_string())
            .collect();
        (done, lines)
    }

    /// containerd's existence checks: by a tag, only the same digest counts; a 404 is
    /// absence; a refusal whose challenge says why is no answer; anything else fails.
    /// A loopback registry on a port that names no scheme is asked in HTTPS first, then
    /// in plain HTTP once it answers the handshake in plain HTTP or never finishes it, as
    /// containerd's fallback finds (`isTLSError`); then in plain HTTP alone.
    #[test]
    fn a_loopback_registry_is_asked_in_https_then_plain_http() {
        use std::io::Write as _;
        for silent in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let hellos = Arc::new(AtomicUsize::new(0));
            let seen = hellos.clone();
            std::thread::spawn(move || {
                for tcp in listener.incoming() {
                    let Ok(mut tcp) = tcp else { return };
                    let seen = seen.clone();
                    std::thread::spawn(move || {
                        let mut first = [0u8; 1];
                        if tcp.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                            seen.fetch_add(1, Ordering::SeqCst);
                            if silent {
                                // Reads the hello and never answers it.
                                let mut sink = [0u8; 4096];
                                while matches!(tcp.read(&mut sink), Ok(n) if n > 0) {}
                            } else {
                                // Go's server reads what it is sent before it answers, and
                                // so is not reset: Winsock resets a socket closed with bytes
                                // unread, and its peer may then lose the answer.
                                let mut header = [0u8; 5];
                                if tcp.read_exact(&mut header).is_ok() {
                                    let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
                                    let mut record = vec![0u8; len];
                                    let _ = tcp.read_exact(&mut record);
                                }
                                let _ = tcp.write_all(crate::testing::GO_BAD_REQUEST);
                                let _ = tcp.shutdown(std::net::Shutdown::Write);
                                let mut sink = [0u8; 4096];
                                while matches!(tcp.read(&mut sink), Ok(n) if n > 0) {}
                            }
                            return;
                        }
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") && matches!(tcp.read(&mut byte), Ok(1)) {
                            head.push(byte[0]);
                        }
                        let _ = tcp.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    });
                }
            });
            let reference = Reference::parse(&format!("127.0.0.1:{port}/test/image:v1")).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            let url = registry.base.join("manifests/v1").unwrap();
            for _ in 0..2 {
                let (response, _) = registry.request("GET", &url, &[]).unwrap();
                assert_eq!(response.status, 404, "silent {silent}");
                assert!(response.url().as_str().starts_with("http://"), "silent {silent}");
            }
            assert_eq!(
                hellos.load(Ordering::SeqCst),
                1,
                "asked in HTTPS once, silent {silent}"
            );
        }
    }

    /// This host's address on its route out, where it has one: what a registry on the
    /// machine is reached at from off it.
    fn routable_v4() -> Option<std::net::Ipv4Addr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        // TEST-NET-1 (RFC 5737): a route is chosen, nothing is sent.
        socket.connect("192.0.2.1:9").ok()?;
        match socket.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
            _ => None,
        }
    }

    /// A registry off loopback that answers HTTPS in plain HTTP is asked in HTTPS alone,
    /// unless Docker's daemon configuration names it insecure (D194): then in HTTPS once,
    /// then in plain HTTP, as dockerd asks one; and its credentials never go to it there,
    /// where it asks for them in plain HTTP, but are refused as such.
    #[test]
    fn a_registry_named_insecure_is_asked_in_plain_http_without_credentials() {
        use std::io::Write as _;
        let Some(ip) = routable_v4() else {
            eprintln!("SKIP: this host has no IPv4 route to reach itself off loopback at");
            return;
        };
        let listener = std::net::TcpListener::bind((ip, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hellos = Arc::new(AtomicUsize::new(0));
        let authorized = Arc::new(AtomicUsize::new(0));
        let (seen, sent) = (hellos.clone(), authorized.clone());
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(mut tcp) = tcp else { return };
                let (seen, sent) = (seen.clone(), sent.clone());
                std::thread::spawn(move || {
                    let mut first = [0u8; 1];
                    if tcp.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let mut header = [0u8; 5];
                        if tcp.read_exact(&mut header).is_ok() {
                            let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
                            let mut record = vec![0u8; len];
                            let _ = tcp.read_exact(&mut record);
                        }
                        let _ = tcp.write_all(crate::testing::GO_BAD_REQUEST);
                        let _ = tcp.shutdown(std::net::Shutdown::Write);
                        let mut sink = [0u8; 4096];
                        while matches!(tcp.read(&mut sink), Ok(n) if n > 0) {}
                        return;
                    }
                    loop {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") && matches!(tcp.read(&mut byte), Ok(1)) {
                            head.push(byte[0]);
                        }
                        if head.is_empty() {
                            return;
                        }
                        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                        if head.contains("\r\nauthorization:") {
                            sent.fetch_add(1, Ordering::SeqCst);
                        }
                        let answer: &[u8] = if head.starts_with("get /v2/private/") {
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"r\"\r\nContent-Length: 0\r\n\r\n"
                        } else {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                        };
                        if tcp.write_all(answer).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let client = || {
            Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
        };
        let reference = |path: &str| Reference::parse(&format!("{ip}:{port}/{path}:v1")).unwrap();
        // Not named: HTTPS alone, its plain answer a failure.
        let secure = Registry::new(client(), &reference("test/image"), Credentials::Anonymous).unwrap();
        let url = secure.base.join("manifests/v1").unwrap();
        let refused = secure.request("GET", &url, &[]).err().unwrap().to_string();
        assert!(
            refused.contains("server gave HTTP response to HTTPS client"),
            "{refused}"
        );
        assert_eq!(hellos.load(Ordering::SeqCst), 1);
        // Named: HTTPS once, then plain HTTP.
        let named =
            crate::registries::Registries::parse(&format!(r#"{{"insecure-registries": ["{ip}:{port}"]}}"#))
                .unwrap();
        let insecure = Registry::new(
            client().with_registries(named.clone()),
            &reference("test/image"),
            Credentials::Anonymous,
        )
        .unwrap();
        let url = insecure.base.join("manifests/v1").unwrap();
        for _ in 0..2 {
            let (response, _) = insecure.request("GET", &url, &[]).unwrap();
            assert_eq!(response.status, 404);
            assert!(
                response.url().as_str().starts_with("http://"),
                "{}",
                response.url()
            );
        }
        assert_eq!(hellos.load(Ordering::SeqCst), 2, "asked in HTTPS once more");
        // Its credentials stay here: asked for them in plain HTTP, it is refused so.
        let private = Registry::new(
            client().with_registries(named),
            &reference("private/image"),
            Credentials::Password {
                username: "u".into(),
                password: "p".into(),
            },
        )
        .unwrap();
        let url = private.base.join("manifests/v1").unwrap();
        let said = private.request("GET", &url, &[]).err().unwrap();
        assert_eq!(said.kind(), ErrorKind::Unauthorized, "{said}");
        assert!(
            said.to_string().contains("asks for credentials in plain HTTP"),
            "{said}"
        );
        assert_eq!(authorized.load(Ordering::SeqCst), 0, "no credentials sent");
    }

    /// A registry named insecure without a port is asked in plain HTTP at HTTP's port once
    /// its HTTPS port refuses the connection; one named with its port, at that port, once
    /// it answers HTTPS in plain HTTP alone; one not named, never.
    #[test]
    fn where_an_insecure_registry_is_asked_in_plain_http() {
        let named = crate::registries::Registries::parse(
            r#"{"insecure-registries": ["10.255.0.1", "10.255.0.2:5000"]}"#,
        )
        .unwrap();
        let plain = |host: &str| {
            let client = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
            .with_registries(named.clone());
            let reference = Reference::parse(&format!("{host}/test/image:v1")).unwrap();
            let registry = Registry::new(client, &reference, Credentials::Anonymous).unwrap();
            registry.plain.map(|p| (p.port, p.on_refusal))
        };
        assert_eq!(plain("10.255.0.1"), Some((80, true)));
        assert_eq!(plain("10.255.0.2:5000"), Some((5000, false)));
        assert_eq!(plain("10.255.0.2"), None);
        assert_eq!(plain("10.255.0.3:5000"), None);
        assert_eq!(plain("localhost:5000"), Some((5000, false)));
        assert_eq!(plain("localhost"), None);
    }

    /// Docker Hub's mirrors (D194): a request of the repository is asked of each mirror in
    /// turn, then of the registry; a mirror that cannot give a manifest or a blob is asked
    /// nothing more, one that has no list asked for (referrers) is asked the next request.
    #[test]
    fn a_pull_asks_its_mirrors_first_and_a_failed_one_nothing_more() {
        let answering = |refused: &'static str| {
            route(None, move |seen| {
                let status = if seen.target.contains(refused) {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                Some((http(status, &[]), After::Keep))
            })
        };
        let hub = answering("/nothing/");
        let first = answering("/manifests/");
        let second = answering("/referrers/");
        let client = || {
            Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
        };
        let reference = Reference::parse(&format!("127.0.0.1:{}/library/x:v1", hub.port)).unwrap();
        let mirror = |port: u16| {
            Registry::mirror(
                client(),
                &format!("http://127.0.0.1:{port}/"),
                &reference,
                Credentials::Anonymous,
            )
            .unwrap()
        };
        let registry = Registry::new(client(), &reference, Credentials::Anonymous)
            .unwrap()
            .with_mirror(mirror(first.port))
            .with_mirror(mirror(second.port));
        let get = |path: &str| {
            let url = registry.base.join(path).unwrap();
            let (response, _) = registry.request("GET", &url, &[]).unwrap();
            (response.status, response.url().port())
        };
        let digest = |c: char| format!("blobs/sha256:{}", c.to_string().repeat(64));
        assert_eq!(get(&digest('a')), (200, first.port), "the first mirror's");
        assert_eq!(get("manifests/v1"), (200, second.port), "the first had none");
        assert_eq!(get(&digest('b')), (200, second.port), "the first asked no more");
        assert_eq!(
            get(&format!("referrers/sha256:{}", "c".repeat(64))),
            (200, hub.port)
        );
        assert_eq!(
            get(&digest('d')),
            (200, second.port),
            "a list the second lacks is its alone"
        );
        let asked = |s: &crate::testing::Server| {
            s.requests()
                .iter()
                .map(|r| r.lines().next().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(asked(&first).len(), 2, "{:?}", asked(&first));
        assert_eq!(asked(&hub).len(), 1, "{:?}", asked(&hub));
        assert!(
            asked(&hub)[0].starts_with("GET /v2/library/x/referrers/"),
            "{:?}",
            asked(&hub)
        );
    }

    /// The tag is asked for where the API is not found or refuses, a 404 of both an empty
    /// list; but an API that could not answer, past its trouble's moment, is a failure, whatever
    /// the tag's 404 says: a policy would read an empty list as no signature (D116).
    #[test]
    fn referrers_that_could_not_be_read_are_no_empty_list() {
        let digest = format!("sha256:{}", "c".repeat(64));
        let tagged = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:dd","size":1}]}"#;
        let asked = |api: &'static str, tag: Option<&'static [u8]>| {
            let server = route(None, move |seen| {
                let reply = if seen.target.contains("/referrers/") {
                    // Docker Hub's 429 for a quota spent, which no wait outlasts.
                    http(
                        api,
                        &[
                            ("RateLimit-Limit", "100;w=21600"),
                            ("RateLimit-Remaining", "0;w=21600"),
                        ],
                    )
                } else {
                    match tag {
                        Some(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.oci.image.index.v1+json\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => http("404 Not Found", &[]),
                    }
                };
                Some((reply, After::Keep))
            });
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            // A gateway's failure is ridden out for a moment (D222): a short one here.
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
            .with_moment(Duration::from_millis(300));
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            registry
                .referrers(&digest, &[], &[])
                .map(|b| String::from_utf8(b).unwrap())
        };
        let empty =
            r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        // No API, no tag: none. An API that refuses, as one a registry does not serve
        // may: the tag, and none where it is not found.
        for api in [
            "404 Not Found",
            "400 Bad Request",
            "405 Method Not Allowed",
            "403 Forbidden",
        ] {
            assert_eq!(asked(api, None).unwrap(), empty, "{api}");
            assert_eq!(asked(api, Some(tagged)).unwrap().as_bytes(), tagged, "{api}");
        }
        // An API that could not answer, past its trouble's moment: the tag where there is one,
        // else its failure.
        for api in [
            "429 Too Many Requests",
            "500 Internal Server Error",
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
        ] {
            assert!(asked(api, None).is_err(), "{api}: read as none");
            assert_eq!(asked(api, Some(tagged)).unwrap().as_bytes(), tagged, "{api}");
        }
    }

    #[test]
    fn existence_is_asked_by_head() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU16, Ordering};
        let ours = format!("sha256:{}", "a".repeat(64));
        let other = format!("sha256:{}", "b".repeat(64));
        let desc = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: ours.clone(),
            size: 10,
            platform: None,
            annotations: Default::default(),
        };
        // A registry answering every check with `answer`, made of its own port, and token
        // requests with a token.
        let check = |answer: Box<dyn Fn(u16) -> Vec<u8> + Send + Sync>, manifest: bool, tag: Option<&str>| {
            let port = Arc::new(AtomicU16::new(0));
            let held = port.clone();
            let server = route(None, move |seen| {
                let reply = if seen.target.starts_with("/token") {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"token\":\"t\"}".to_vec()
                } else {
                    answer(held.load(Ordering::SeqCst))
                };
                Some((reply, After::Keep))
            });
            port.store(server.port, Ordering::SeqCst);
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            let checked = registry.exists(&desc, manifest, tag);
            (checked, server.requests())
        };
        let digested = |d: String| -> Box<dyn Fn(u16) -> Vec<u8> + Send + Sync> {
            Box::new(move |_| http("200 OK", &[("Docker-Content-Digest", &d)]))
        };
        let (found, requests) = check(digested(other.clone()), true, Some("v1"));
        assert!(!found.unwrap(), "a tag naming another digest");
        assert!(
            requests[0].starts_with("HEAD /v2/test/image/manifests/v1 "),
            "{requests:?}"
        );
        assert!(
            requests[0].contains("Accept: application/vnd.oci.image.manifest.v1+json, */*"),
            "{requests:?}"
        );
        let (found, _) = check(digested(ours.clone()), true, Some("v1"));
        assert!(found.unwrap(), "a tag naming ours");
        let (found, requests) = check(Box::new(|_| http("404 Not Found", &[])), false, None);
        assert!(!found.unwrap());
        assert!(
            requests[0].starts_with(&format!("HEAD /v2/test/image/blobs/{ours} ")),
            "{requests:?}"
        );
        // Refused again after a token, the challenge saying why: no answer, the push goes on.
        let refused = |port: u16| {
            let challenge =
                format!(r#"Bearer realm="http://127.0.0.1:{port}/token",error="insufficient_scope""#);
            http("401 Unauthorized", &[("WWW-Authenticate", &challenge)])
        };
        let (found, requests) = check(Box::new(refused), false, None);
        assert!(!found.unwrap(), "{requests:?}");
        let (failed, _) = check(Box::new(|_| http("500 Internal Server Error", &[])), false, None);
        assert!(failed.is_err());
        // A gateway's failure is asked again, as 503 and 504 are: once, and it passed.
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let theirs = ours.clone();
        let once: Box<dyn Fn(u16) -> Vec<u8> + Send + Sync> = Box::new(move |_| {
            if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                http("502 Bad Gateway", &[])
            } else {
                http("200 OK", &[("Docker-Content-Digest", &theirs)])
            }
        });
        let (found, requests) = check(once, true, Some("v1"));
        assert!(found.unwrap(), "{requests:?}");
    }

    /// A scripted registry's answer to the `n`th request.
    type Answer = Box<dyn Fn(&Seen, usize) -> (Vec<u8>, After) + Send + Sync>;

    /// A tag list's names that are no tags are passed over, as dockerd passes them over for
    /// `pull -a` (Docker 29.3.1, measured): one would steer the manifest's URL off the
    /// repository (`../`, `//host`), or name an image as no reference can, its control
    /// characters bound for the terminal.
    #[test]
    fn a_tag_list_keeps_only_tags() {
        let listed = serde_json::json!({
            "name": "test/image",
            "tags": [
                "good", "../evil", "//elsewhere/x", "x/../y", "bad tag", "x\u{1b}[2J", "",
                "a".repeat(129), "_ok.v1-2", ".dot", "-dash", "a".repeat(128), "ünï",
            ],
        })
        .to_string();
        let server = route(None, move |_| {
            let page = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{listed}",
                listed.len()
            );
            Some((page.into_bytes(), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        assert_eq!(
            registry.tags().unwrap(),
            vec!["good".to_string(), "_ok.v1-2".into(), "a".repeat(128)]
        );
    }

    /// A `Content-Encoding` a server lists is undone once a coding: a coding listed again
    /// would nest another decoder, with its own buffers, read through one more level of
    /// recursion. A header listing gzip 20,000 times, sent to a fetch's thread, was 20,000
    /// nested decoders (640 MiB of buffers) read through 20,000 frames: the thread's stack
    /// overflowed, which ends the daemon.
    #[test]
    fn a_content_coding_is_undone_once() {
        let many = vec!["gzip"; 20_000].join(", ");
        let decoding = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let mut body: &[u8] = b"not gzip at all";
                let mut out = Vec::new();
                decoded(&mut body, &many)
                    .and_then(|mut r| r.read_to_end(&mut out).map_err(Error::from))
                    .map_err(|e| e.to_string())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(
            decoding.unwrap_err(),
            "Content-Encoding lists gzip twice: shards undoes each coding once"
        );
        let mut body: &[u8] = b"";
        for (listed, said) in [
            ("gzip, zstd, GZIP", "Content-Encoding lists gzip twice"),
            ("deflate,deflate", "Content-Encoding lists deflate twice"),
            ("zstd, br, zstd", "unsupported Content-Encoding algorithm: br"),
        ] {
            let e = decoded(&mut body, listed).err().unwrap().to_string();
            assert!(e.starts_with(said), "{listed}: {e}");
        }
        // Identity changes nothing, however often it is listed.
        let identities = vec!["identity"; 100_000].join(",");
        let mut body: &[u8] = b"as it is";
        let mut out = Vec::new();
        decoded(&mut body, &identities)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"as it is");
    }

    /// Blobs are asked for compressed for their transfer and decoded (D20), as containerd's
    /// fetcher decodes them; a resumed download asks for the blob as it is.
    #[test]
    fn transfer_encodings_are_decoded() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let blob: Arc<Vec<u8>> = Arc::new((0..200_000u32).map(|i| (i % 251) as u8).collect());
        let digest = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(&blob[..]));
        let gzip = |b: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b).unwrap();
            e.finish().unwrap()
        };
        let deflate = {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&blob).unwrap();
            e.finish().unwrap()
        };
        let zstd = ruzstd::encoding::compress_to_vec(&blob[..], ruzstd::encoding::CompressionLevel::Fastest);
        let fetch = |answer: Answer| {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (held, n) = (seen.clone(), Arc::new(AtomicUsize::new(0)));
            let server = route(None, move |req| {
                held.lock().unwrap().push(req.clone());
                Some(answer(req, n.fetch_add(1, Ordering::SeqCst)))
            });
            let dir = shards_testdir::TempDir::new(&format!("encodings-{}", server.port)).unwrap();
            let store = Store::open(&dir).unwrap();
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            let desc = Descriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
                digest: digest.to_string(),
                size: 200_000,
                platform: None,
                annotations: Default::default(),
            };
            let fetched = registry
                .fetch_blob(&store, &desc, &Limits::none(), &|_| {})
                .map(|()| std::fs::read(store.blob_path(&digest)).unwrap());
            let _ = std::fs::remove_dir_all(&dir);
            let requests = seen.lock().unwrap().clone();
            (fetched, requests)
        };
        let encoded = |body: Vec<u8>, coding: &'static str| -> Answer {
            Box::new(move |_, _| {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: {coding}\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(&body);
                (out, After::Keep)
            })
        };
        for (body, coding) in [
            (gzip(&blob), "gzip"),
            (deflate.clone(), "deflate"),
            (zstd.clone(), "zstd"),
            (gzip(&blob), "identity, gzip"),
            // Deflated, then gzipped: undone gzip first.
            (gzip(&deflate), "deflate, gzip"),
        ] {
            let (fetched, requests) = fetch(encoded(body, coding));
            assert_eq!(fetched.unwrap().as_slice(), &blob[..], "{coding}");
            assert_eq!(
                requests[0].header("accept-encoding"),
                Some("zstd;q=1.0, gzip;q=0.8, deflate;q=0.5"),
                "{coding}"
            );
        }
        let (refused, _) = fetch(encoded(blob.to_vec(), "br"));
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("unsupported Content-Encoding algorithm: br")
        );
        // A coding twice, which containerd nests again: refused, however the body is.
        let (refused, _) = fetch(encoded(gzip(&gzip(&blob)), "gzip, gzip"));
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("Content-Encoding lists gzip twice"),
        );
        // Cut short gzipped, then resumed as it is from where it stopped.
        let whole = gzip(&blob);
        let rest = blob.clone();
        let (fetched, requests) = fetch(Box::new(move |req, n| {
            if n == 0 {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                    whole.len()
                )
                .into_bytes();
                out.extend_from_slice(&whole[..whole.len() / 2]);
                // Cut short: the connection closes inside the body.
                return (out, After::Close);
            }
            let from: usize = req
                .header("range")
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|r| r.strip_suffix('-'))
                .and_then(|r| r.parse().ok())
                .unwrap();
            let mut out = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\nContent-Length: {}\r\n\r\n",
                rest.len() - 1,
                rest.len(),
                rest.len() - from
            )
            .into_bytes();
            out.extend_from_slice(&rest[from..]);
            (out, After::Keep)
        }));
        assert_eq!(fetched.unwrap().as_slice(), &blob[..]);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[1].header("accept-encoding"), Some("identity"));
        assert!(
            requests[1].header("range").is_some_and(|r| r != "bytes=0-"),
            "{requests:?}"
        );
    }

    /// A mount the registry refuses as unauthorized is uploaded instead, as containerd's
    /// pusher falls back (pusher.go v2.4.1); one it grants is done.
    #[test]
    fn a_refused_mount_is_uploaded_instead() {
        let (done, lines) = uploaded(Some("other/repo"), |seen| {
            match (seen.method.as_str(), seen.target.as_str()) {
                ("POST", t) if t.contains("mount=") => http("401 Unauthorized", &[]),
                ("POST", _) => http("202 Accepted", &[("Location", "/v2/test/image/blobs/uploads/u1")]),
                _ => http("201 Created", &[]),
            }
        });
        assert!(!done.unwrap());
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].starts_with("POST /v2/test/image/blobs/uploads/?mount="),
            "{lines:?}"
        );
        assert_eq!(lines[1], "POST /v2/test/image/blobs/uploads/ HTTP/1.1");
        assert!(
            lines[2].starts_with("PUT /v2/test/image/blobs/uploads/u1?digest=sha256%3A"),
            "{lines:?}"
        );
        let (done, lines) = uploaded(Some("other/repo"), |_| http("201 Created", &[]));
        assert!(done.unwrap());
        assert_eq!(lines.len(), 1);
    }

    /// What dockerd says of a refusal, from its status and body, as translateRegistryError
    /// says it of what containerd decoded (errcode.go).
    #[test]
    fn refusals_are_said_with_the_registrys_words() {
        let whole = |status, body: &str| match refusal_said(status, body.as_bytes()) {
            Said::Instead(said) => said,
            Said::Before(word) => format!("{word}: <the error>"),
        };
        for (status, body, said) in [
            // No body, as a HEAD's, or none of distribution's.
            (401, "", "unknown: <the error>"),
            (500, "<html>", "unknown: <the error>"),
            (500, "[]", "unknown: <the error>"),
            (500, r#"{"errors":{}}"#, "unknown: <the error>"),
            (500, r#"{"errors":[{"code":1003}]}"#, "unknown: <the error>"),
            (500, r#"{"errors":[]} trailing"#, "unknown: <the error>"),
            (500, r#"{"errors":[["DENIED","no"]]}"#, "unknown: <the error>"),
            (
                500,
                r#"{"errors":[{"code":"DENIED","message":5}]}"#,
                "unknown: <the error>",
            ),
            // Distribution's, with no errors.
            (500, "null", "error from registry: <the error>"),
            (500, "{}", "error from registry: <the error>"),
            (500, r#"{"errors":null}"#, "error from registry: <the error>"),
            (403, r#"{"errors":[]}"#, "error from registry: <the error>"),
            // A token server's details, for a 401 or 403 only.
            (
                401,
                r#"{"details":"no access"}"#,
                "authentication required - no access",
            ),
            (
                403,
                r#"{"details":"no access"}"#,
                "requested access to the resource is denied - no access",
            ),
            (
                500,
                r#"{"details":"no access"}"#,
                "error from registry: <the error>",
            ),
            (401, r#"{"details":""}"#, "error from registry: <the error>"),
            // Errors: messages, codes' messages, text details.
            (
                401,
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required","detail":[{"Type":"repository"}]}]}"#,
                "error from registry: authentication required",
            ),
            (
                429,
                r#"{"errors":[{"code":"TOOMANYREQUESTS","message":"You have reached your pull rate limit."}]}"#,
                "error from registry: You have reached your pull rate limit.",
            ),
            (
                404,
                r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown","detail":"sha256:x"}]}"#,
                "error from registry: manifest unknown - sha256:x",
            ),
            (
                403,
                r#"{"errors":[{"code":"DENIED","detail":"quota"}]}"#,
                "error from registry: requested access to the resource is denied - quota",
            ),
            (
                403,
                r#"{"errors":[{"code":"DENIED","message":"","detail":null}]}"#,
                "error from registry: denied",
            ),
            // An error that is only its code, once.
            (
                403,
                r#"{"errors":[{"code":"DENIED","message":"requested access to the resource is denied"}]}"#,
                "error from registry: denied",
            ),
            (
                429,
                r#"{"errors":[{"code":"TOOMANYREQUESTS"}]}"#,
                "error from registry: toomanyrequests",
            ),
            (
                404,
                r#"{"errors":[{"code":"NAME_UNKNOWN"}]}"#,
                "error from registry: unknown",
            ),
            (500, r#"{"errors":[null]}"#, "error from registry: unknown"),
            (
                503,
                r#"{"errors":[{"code":"UNAVAILABLE","message":"a"},{"code":"UNSUPPORTED","message":"b","detail":7}]}"#,
                "error from registry: a\nb",
            ),
            // What a registry says reaches a terminal with its controls escaped.
            (
                500,
                r#"{"errors":[{"message":"\u001b[2Jgone","detail":"\u009b31m"}]}"#,
                "error from registry: \\u{1b}[2Jgone - \\u{9b}31m",
            ),
        ] {
            assert_eq!(whole(status, body), said, "{status} {body}");
        }
    }

    /// Refusals where containerd has words of its own: a fetch's 404 is no content at the
    /// URL asked; a manifest is put with 200, 201 or 204 only; a 429's rate limits follow
    /// what the registry says of it.
    #[test]
    fn refusals_are_worded_in_full() {
        let serving = |answer: &'static [u8]| {
            let server = route(None, move |_| Some((answer.to_vec(), After::Keep)));
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            (server, registry)
        };
        let dir = shards_testdir::TempDir::new("refusal-words").unwrap();
        let store = Store::open(&dir).unwrap();
        let digest = format!("sha256:{}", "c".repeat(64));
        let desc = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: digest.clone(),
            size: 3,
            platform: None,
            annotations: Default::default(),
        };
        let (server, registry) = serving(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let e = registry
            .fetch_blob(&store, &desc, &Limits::none(), &|_| {})
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(
            e.to_string(),
            format!(
                // containerd names the request by its host's scheme, HTTPS before the
                // fallback rewrote it (resolver.go, request.String).
                "content at https://127.0.0.1:{}/v2/test/image/blobs/{digest} not found: not found",
                server.port
            )
        );
        let (server, registry) = serving(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n");
        let e = registry
            .put_manifest("v1", "application/vnd.oci.image.manifest.v1+json", b"{}")
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "unexpected status from PUT request to http://127.0.0.1:{}/v2/test/image/manifests/v1: 202 Accepted",
                server.port
            )
        );
        let limited = Refusal {
            url: "http://r/v2/x/blobs/sha256:a".into(),
            status: 429,
            text: "429 Too Many Requests".into(),
            limits: Some("limit 100 per 21600 s; 0 left".into()),
        }
        .error(
            "GET",
            br#"{"errors":[{"code":"TOOMANYREQUESTS","message":"You have reached your pull rate limit."}]}"#,
        );
        assert_eq!(
            limited.to_string(),
            "unexpected status from GET request to http://r/v2/x/blobs/sha256:a: 429 Too Many Requests \
             (limit 100 per 21600 s; 0 left)"
        );
        assert_eq!(
            limited.into_reported().to_string(),
            "error from registry: You have reached your pull rate limit. (limit 100 per 21600 s; 0 left)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Uploads `blobs` in turn, through one registry, to a server that refuses a request
    /// with a body of more than `most` bytes with a 413, as nginx's
    /// `client_max_body_size` does, and says `least` is the least a chunk may hold, where
    /// given. Each upload's result, and every request's method and Content-Range or
    /// Content-Length, in order.
    fn uploaded_through(
        blobs: &[&[u8]],
        most: usize,
        least: Option<u64>,
    ) -> (Vec<Result<bool, Error>>, Vec<String>) {
        let server = route(None, move |seen| {
            let length: usize = seen
                .header("content-length")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let answer = if length > most {
                http("413 Request Entity Too Large", &[])
            } else {
                match seen.method.as_str() {
                    "HEAD" => http("404 Not Found", &[]),
                    "POST" => {
                        let least = least.map(|n| n.to_string()).unwrap_or_default();
                        let mut fields = vec![("Location", "/v2/test/image/blobs/uploads/u?s=0")];
                        if !least.is_empty() {
                            fields.push(("OCI-Chunk-Min-Length", &least));
                        }
                        http("202 Accepted", &fields)
                    }
                    "PATCH" => {
                        let end = seen
                            .header("content-range")
                            .and_then(|r| r.split_once('-'))
                            .map(|(_, end)| end.to_string())
                            .unwrap_or_default();
                        let next = format!("/v2/test/image/blobs/uploads/u?s={end}");
                        http(
                            "202 Accepted",
                            &[("Location", &next), ("Range", &format!("0-{end}"))],
                        )
                    }
                    _ => http("201 Created", &[]),
                }
            };
            Some((answer, After::Keep))
        });
        let dir = shards_testdir::TempDir::new("upload-through").unwrap();
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let results = blobs
            .iter()
            .enumerate()
            .map(|(i, blob)| {
                let path = dir.join(format!("{i}"));
                std::fs::write(&path, blob).unwrap();
                let desc = Descriptor {
                    media_type: "application/octet-stream".into(),
                    digest: Digest::from_hash(Algorithm::Sha256, &Sha256::digest(blob)).to_string(),
                    size: i64::try_from(blob.len()).unwrap(),
                    platform: None,
                    annotations: Default::default(),
                };
                registry.upload(&desc, &std::fs::File::open(&path).unwrap(), None, None)
            })
            .collect();
        let lines = server
            .requests()
            .iter()
            .map(|r| {
                let seen = crate::testing::parse(r);
                let size = seen
                    .header("content-range")
                    .or(seen.header("content-length"))
                    .unwrap_or_default()
                    .to_string();
                format!("{} {size}", seen.method)
            })
            .collect();
        (results, lines)
    }

    /// A blob the registry refuses whole with a 413 goes up in chunks in order, each the
    /// midpoint of the most it took and the least it refused, so that its chunks bisect
    /// to the most it takes; then one PUT that ends it. A second blob through the same
    /// registry goes straight to chunks of what it took.
    #[test]
    fn a_blob_refused_whole_goes_up_in_chunks_the_registry_takes() {
        let (results, lines) = uploaded_through(&[b"0123456789", b"abcdef"], 3, None);
        assert!(results.iter().all(|r| matches!(r, Ok(false))), "{results:?}");
        assert_eq!(
            lines,
            [
                "POST 0",
                "PUT 10",
                "PATCH 0-4",
                "PATCH 0-1",
                "PATCH 2-4",
                "PATCH 5-8",
                "PATCH 5-7",
                "PATCH 8-9",
                "PUT 0",
                "POST 0",
                "PATCH 0-2",
                "PATCH 3-5",
                "PUT 0",
            ]
        );
    }

    /// A send that fails is resumed from what the registry says it has, never from the
    /// blob's start: its PUT cut off, a GET of the upload says 4 bytes are in, and the rest
    /// goes from there. Over a link, the failure is counted, and the rest goes in chunks.
    #[test]
    fn a_failed_send_resumes_from_what_the_registry_has() {
        for linked in [false, true] {
            let cut = Arc::new(AtomicUsize::new(0));
            let server = route(None, move |seen| {
                let answer = match seen.method.as_str() {
                    "POST" => http("202 Accepted", &[("Location", "/v2/test/image/blobs/uploads/u")]),
                    "PUT" if seen.header("content-length") != Some("0") => {
                        cut.fetch_add(1, Ordering::SeqCst);
                        return None;
                    }
                    "GET" => http("204 No Content", &[("Range", "0-3")]),
                    "PATCH" => {
                        let end = seen
                            .header("content-range")
                            .and_then(|r| r.split_once('-'))
                            .map(|(_, end)| end.to_string())
                            .unwrap_or_default();
                        http(
                            "202 Accepted",
                            &[
                                ("Location", "/v2/test/image/blobs/uploads/u"),
                                ("Range", &format!("0-{end}")),
                            ],
                        )
                    }
                    _ => http("201 Created", &[]),
                };
                Some((answer, After::Keep))
            });
            let dir = shards_testdir::TempDir::new("upload-resume").unwrap();
            let path = dir.join("blob");
            let blob = b"0123456789";
            std::fs::write(&path, blob).unwrap();
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            let desc = Descriptor {
                media_type: "application/octet-stream".into(),
                digest: Digest::from_hash(Algorithm::Sha256, &Sha256::digest(blob)).to_string(),
                size: 10,
                platform: None,
                annotations: Default::default(),
            };
            let link = Link::new(None);
            let done = registry.upload(
                &desc,
                &std::fs::File::open(&path).unwrap(),
                None,
                linked.then_some(&link),
            );
            assert!(matches!(done, Ok(false)), "{done:?}");
            let lines: Vec<String> = server
                .requests()
                .iter()
                .map(|r| {
                    let seen = crate::testing::parse(r);
                    let size = seen
                        .header("content-range")
                        .or(seen.header("content-length"))
                        .unwrap_or_default()
                        .to_string();
                    format!("{} {size}", seen.method)
                })
                .collect();
            assert_eq!(lines[..3], ["POST 0", "PUT 10", "GET "], "{lines:?}");
            assert_eq!(lines.last().map(String::as_str), Some("PUT 0"), "{lines:?}");
            let patches: Vec<&String> = lines.iter().filter(|l| l.starts_with("PATCH")).collect();
            assert!(patches[0].starts_with("PATCH 4-"), "{lines:?}");
            if linked {
                assert_eq!(link.failures(), 1);
                assert!(patches.iter().all(|p| p.as_str() != "PATCH 0-9"), "{lines:?}");
            } else {
                assert_eq!(patches, ["PATCH 4-9"], "unbounded, the rest in one");
            }
        }
    }

    /// A chunk is never smaller than the least the registry says one may be (`OCI-Chunk-
    /// Min-Length`), but for the last; a refusal of one so small fails the upload.
    #[test]
    fn chunks_hold_at_least_what_the_registry_asks() {
        let (results, lines) = uploaded_through(&[b"0123456789"], 4, Some(4));
        assert!(matches!(results[..], [Ok(false)]), "{results:?}");
        assert_eq!(
            lines,
            [
                "POST 0",
                "PUT 10",
                "PATCH 0-4",
                "PATCH 0-3",
                "PATCH 4-7",
                "PATCH 8-9",
                "PUT 0"
            ]
        );
        let (results, lines) = uploaded_through(&[b"0123456789"], 4, Some(6));
        let said = results[0].as_ref().unwrap_err().to_string();
        assert!(said.starts_with("unexpected status from PATCH request"), "{said}");
        assert_eq!(lines, ["POST 0", "PUT 10", "PATCH 0-5"]);
    }

    /// The statuses containerd takes: an upload begun with 200, 202 or 204, or found there
    /// already with 201; done with 200, 201 or 204, its digest checked when sent.
    #[test]
    fn uploads_take_every_status_registries_answer_with() {
        for (begun, done) in [
            ("200 OK", "200 OK"),
            ("204 No Content", "204 No Content"),
            ("202 Accepted", "201 Created"),
        ] {
            let (result, lines) = uploaded(None, move |seen| match seen.method.as_str() {
                "POST" => http(begun, &[("Location", "/v2/test/image/blobs/uploads/u2")]),
                _ => http(done, &[]),
            });
            assert!(!result.unwrap(), "{begun} {done}");
            assert_eq!(lines.len(), 2, "{lines:?}");
        }
        let (result, lines) = uploaded(None, |_| http("201 Created", &[]));
        assert!(!result.unwrap());
        assert_eq!(lines.len(), 1, "there already: no PUT");
        let other = format!("sha256:{}", "0".repeat(64));
        let (result, _) = uploaded(None, move |seen| match seen.method.as_str() {
            "POST" => http("202 Accepted", &[("Location", "/v2/test/image/blobs/uploads/u3")]),
            _ => http("201 Created", &[("Docker-Content-Digest", &other)]),
        });
        let said = result.unwrap_err().to_string();
        assert!(
            said.starts_with(&format!("got digest sha256:{}, expected sha256:", "0".repeat(64))),
            "{said}"
        );
        for done in ["202 Accepted", "400 Bad Request"] {
            let (result, _) = uploaded(None, move |seen| match seen.method.as_str() {
                "POST" => http("202 Accepted", &[("Location", "/v2/test/image/blobs/uploads/u4")]),
                _ => http(done, &[]),
            });
            let said = result.unwrap_err().to_string();
            assert!(
                said.starts_with("unexpected status from PUT request to http://127.0.0.1:"),
                "{said}"
            );
            assert!(
                said.ends_with(&format!("/v2/test/image/blobs/uploads/u4?…: {done}")),
                "{said}"
            );
        }
    }

    /// A login as dockerd's (D21): `/v2/` asked, its bearer challenge answered with the password
    /// grant asking offline access (then a GET with Basic, the token server having no
    /// OAuth2), and the refresh token given back as the identity token; a token server's
    /// refusal unauthorized, in its own words; Basic taken and refused; a registry that
    /// asks for nothing logged in to as it is.
    #[test]
    fn logins_answer_the_registrys_challenge_and_keep_its_refresh_token() {
        use base64::Engine as _;
        let client = || {
            Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
        };
        let password = |p: &str| Credentials::Password {
            username: "u".into(),
            password: p.into(),
        };
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:p")
        );
        let answer = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        };
        let good = basic.clone();
        let tokens = route(None, move |seen: &Seen| {
            let said = match (seen.method.as_str(), seen.header("authorization")) {
                ("POST", _) => answer("404 Not Found", ""),
                ("GET", Some(given)) if given == good => {
                    answer("200 OK", r#"{"token":"t","refresh_token":"r"}"#)
                }
                _ => answer(
                    "401 Unauthorized",
                    r#"{"details":"incorrect username or password"}"#,
                ),
            };
            Some((said, After::Keep))
        });
        let realm = format!(
            r#"Bearer realm="http://127.0.0.1:{}/token",service="reg""#,
            tokens.port
        );
        let bearer = route(None, move |seen: &Seen| {
            Some((
                match seen.header("authorization") {
                    Some("Bearer t") => http("200 OK", &[]),
                    _ => http("401 Unauthorized", &[("WWW-Authenticate", &realm)]),
                },
                After::Keep,
            ))
        });
        let host = format!("127.0.0.1:{}", bearer.port);
        assert_eq!(
            Registry::login(client(), &host, password("p")).unwrap(),
            Some("r".into())
        );
        let asked = tokens.requests();
        assert!(
            asked
                .iter()
                .any(|r| r.starts_with("POST") && r.contains("access_type=offline")),
            "{asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|r| r.starts_with("GET") && r.contains("offline_token=true")),
            "{asked:?}"
        );
        let refused = Registry::login(client(), &host, password("wrong")).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Unauthorized, "{refused}");
        assert!(
            refused.to_string().contains("incorrect username or password"),
            "{refused}"
        );
        // Basic, taken and refused.
        let good = basic;
        let plain = route(None, move |seen: &Seen| {
            Some((
                match seen.header("authorization") {
                    Some(given) if given == good => http("200 OK", &[]),
                    _ => http(
                        "401 Unauthorized",
                        &[("WWW-Authenticate", r#"Basic realm="reg""#)],
                    ),
                },
                After::Keep,
            ))
        });
        let host = format!("127.0.0.1:{}", plain.port);
        assert_eq!(Registry::login(client(), &host, password("p")).unwrap(), None);
        let refused = Registry::login(client(), &host, password("wrong")).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Unauthorized, "{refused}");
        // One that asks for nothing.
        let open = route(None, |_: &Seen| Some((http("200 OK", &[]), After::Keep)));
        let host = format!("127.0.0.1:{}", open.port);
        assert_eq!(
            Registry::login(client(), &host, Credentials::Anonymous).unwrap(),
            None
        );
    }

    /// A trouble's waits are random shares of how long it has lasted, at least what the
    /// registry names, until it has lasted its moment or names a wait past it (D222).
    #[test]
    fn a_troubles_waits_are_shares_of_its_length_within_a_moment() {
        let moment = Duration::from_secs(10);
        let ago = |d: Duration| Instant::now().checked_sub(d).unwrap();
        let mut trouble = Trouble::default();
        let began = ago(Duration::from_secs(2));
        let mut waits = Vec::new();
        for _ in 0..200 {
            let wait = trouble.wait(began, moment, None).unwrap();
            assert!(wait <= trouble.lasted(), "{wait:?} past {:?}", trouble.lasted());
            waits.push(wait);
        }
        // Shares, not one figure.
        waits.sort();
        assert!(waits[0] < waits[199], "{waits:?}");
        // Named: at least that.
        let named = trouble.wait(Instant::now(), moment, Some(Duration::from_secs(3)));
        assert!(named.unwrap() >= Duration::from_secs(3));
        // Named past the moment: given up on.
        assert_eq!(
            trouble.wait(Instant::now(), moment, Some(Duration::from_secs(9))),
            None
        );
        // Lasted the moment: given up on.
        assert_eq!(Trouble::default().wait(ago(moment), moment, None), None);
        // Over: what fails next begins another, and the path learns how long it lasted.
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let url = Url::parse("https://r.example/v2/").unwrap();
        trouble.over(&http, &url);
        assert!(trouble.lasted().is_zero());
        let shown = http.shown("r.example", 443);
        assert_eq!(shown.troubles, 1);
        assert!(shown.trouble_us >= 2_000_000, "{shown:?}");
        // One not begun passed nothing.
        Trouble::default().over(&http, &url);
        assert_eq!(http.shown("r.example", 443).troubles, 1);
    }

    /// A registry asked of a server that answers `failing` (a status line, or `None` to
    /// close the connection unanswered) until `lasting` has passed since its first request,
    /// then 200; riding its trouble out for `moment`. Its answer's status (or error), the
    /// requests it made, how long it took, and how long of that it chose to wait: its round
    /// trips are the rest, as long as the host running it makes them.
    fn ridden(
        failing: Option<&'static str>,
        fields: &'static [(&'static str, &'static str)],
        lasting: Duration,
        moment: Duration,
    ) -> (Result<u16, Error>, usize, Duration, Duration) {
        let first = Arc::new(std::sync::Mutex::new(None::<Instant>));
        let server = route(None, move |_| {
            let began = *first.lock().unwrap().get_or_insert_with(Instant::now);
            if began.elapsed() >= lasting {
                return Some((http("200 OK", &[]), After::Keep));
            }
            failing.map(|status| (http(status, fields), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
        .with_moment(moment);
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let url = registry.base().join("manifests/v1").unwrap();
        let t0 = Instant::now();
        let status = registry.request("GET", &url, &[]).map(|(r, _)| r.status);
        (
            status,
            server.requests().len(),
            t0.elapsed(),
            registry.http.paused(),
        )
    }

    /// A registry's passing trouble, a gateway's 502, 503 or 504 or connections cut
    /// unanswered, is ridden out for as long as it lasts within a moment, however many
    /// times it fails meanwhile, and costs at most its own length again; one that outlasts
    /// the moment is the answer; a wait the registry names is waited; a repeated 500 is the
    /// request's own, answered at once (D222). Before, two 502s in a row failed a request,
    /// as Docker Hub's token server's did a CI build (job 117733752147).
    #[test]
    fn a_registrys_passing_trouble_is_ridden_out_within_a_moment() {
        let lasting = Duration::from_millis(400);
        let moment = Duration::from_secs(20);
        for failing in [
            Some("502 Bad Gateway"),
            Some("503 Service Unavailable"),
            Some("504 Gateway Timeout"),
            Some("408 Request Timeout"),
            None,
        ] {
            let (status, asked, took, paused) = ridden(failing, &[], lasting, moment);
            assert_eq!(status.unwrap(), 200, "{failing:?}");
            assert!(asked >= 3, "{failing:?}: asked {asked} times");
            assert!(took >= lasting, "{failing:?}: {took:?}");
            // Its waits at most its length again; the rest of the time, the asking's own
            // round trips.
            assert!(paused < lasting * 2, "{failing:?}: {paused:?} of {took:?}");
        }
        // Past the moment: the registry's answer, said.
        let short = Duration::from_millis(500);
        let (status, _, took, paused) = ridden(
            Some("503 Service Unavailable"),
            &[],
            Duration::from_secs(3600),
            short,
        );
        assert_eq!(status.unwrap(), 503);
        assert!(took >= short && paused < short * 2, "{paused:?} of {took:?}");
        let (status, _, took, paused) = ridden(None, &[], Duration::from_secs(3600), short);
        assert_eq!(status.unwrap_err().kind(), ErrorKind::Transient);
        assert!(took >= short && paused < short * 2, "{paused:?} of {took:?}");
        // A wait it names is waited.
        let (status, _, took, _) = ridden(
            Some("503 Service Unavailable"),
            &[("Retry-After", "1")],
            Duration::from_millis(100),
            moment,
        );
        assert_eq!(status.unwrap(), 200);
        assert!(took >= Duration::from_secs(1), "{took:?}");
        // One past the moment is not: the answer at once.
        let (status, asked, _, paused) = ridden(
            Some("503 Service Unavailable"),
            &[("Retry-After", "3600")],
            Duration::from_secs(3600),
            moment,
        );
        assert_eq!((status.unwrap(), asked, paused), (503, 1, Duration::ZERO));
        // A 500 is asked again once; repeated, it is the answer.
        let (status, asked, _, _) = ridden(
            Some("500 Internal Server Error"),
            &[],
            Duration::from_secs(3600),
            moment,
        );
        assert_eq!((status.unwrap(), asked), (500, 2));
    }

    /// A wait before asking a registry again ends at its client's cancel, which it says.
    #[test]
    fn a_cancel_ends_a_troubles_wait() {
        let server = route(None, |_| {
            Some((http("503 Service Unavailable", &[]), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let cancel = crate::http::Cancel::new();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
        .cancelled_by(cancel.clone());
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let url = registry.base().join("manifests/v1").unwrap();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            cancel.cancel();
        });
        let t0 = Instant::now();
        let e = registry
            .request("GET", &url, &[])
            .map(|(r, _)| r.status)
            .unwrap_err();
        let took = t0.elapsed();
        canceller.join().unwrap();
        assert_eq!(e.kind(), ErrorKind::Cancelled, "{e}");
        assert!(
            took >= Duration::from_secs(3) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    /// A download that brings nothing, its connections cut before a byte, is resumed while
    /// that lasts a moment, and fetched once the server sends again; one that brings nothing
    /// for a moment fails, saying how long (D222). Before, three such answers in a row
    /// failed it, however quickly they came.
    #[test]
    fn a_download_that_brings_nothing_is_resumed_within_a_moment() {
        let blob: Arc<Vec<u8>> = Arc::new((0..100_000u32).map(|i| (i % 253) as u8).collect());
        let digest = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(&blob[..]));
        let fetch = |lasting: Duration, moment: Duration| {
            let first = Arc::new(std::sync::Mutex::new(None::<Instant>));
            let body = blob.clone();
            let server = route(None, move |_| {
                let began = *first.lock().unwrap().get_or_insert_with(Instant::now);
                let mut out =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
                if began.elapsed() >= lasting {
                    out.extend_from_slice(&body);
                    return Some((out, After::Keep));
                }
                // Its head, and none of its body.
                Some((out, After::Close))
            });
            let dir = shards_testdir::TempDir::new(&format!("nothing-{}", server.port)).unwrap();
            let store = Store::open(&dir).unwrap();
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|_| crate::tls::client_config(Vec::new(), None)),
                "shards-test",
            )
            .with_moment(moment);
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            let desc = Descriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                digest: digest.to_string(),
                size: 100_000,
                platform: None,
                annotations: Default::default(),
            };
            let t0 = Instant::now();
            let fetched = registry
                .fetch_blob(&store, &desc, &Limits::none(), &|_| {})
                .map(|()| std::fs::read(store.blob_path(&digest)).unwrap());
            (
                fetched,
                server.requests().len(),
                t0.elapsed(),
                registry.http.paused(),
            )
        };
        // Its waits at most the trouble's length again; the rest, its round trips.
        let lasting = Duration::from_millis(400);
        let (fetched, asked, _, paused) = fetch(lasting, Duration::from_secs(20));
        assert_eq!(fetched.unwrap(), *blob);
        assert!(asked >= 4, "asked {asked} times");
        assert!(paused < lasting * 2, "{paused:?}");
        let short = Duration::from_millis(500);
        let (fetched, _, took, paused) = fetch(Duration::from_secs(3600), short);
        let e = fetched.unwrap_err();
        assert!(
            e.to_string().contains("the download brought nothing for 0."),
            "{e}"
        );
        assert!(took >= short && paused < short * 2, "{paused:?} of {took:?}");
    }

    /// A path whose answers come 1.5 s late: its first, past the 1.25 s drawn from the
    /// second a path starts with (RFC 6298 §2.1), is cut and asked again at once with the
    /// wait doubled (§5.5), and answered; its answer, its cut and what the cut cost
    /// learned, the next is waited for, asked once (D222).
    #[test]
    fn a_paths_answers_are_waited_for_as_long_as_it_shows() {
        // Every answer 1.5 s late.
        let server = route(None, move |_| {
            std::thread::sleep(Duration::from_millis(1500));
            Some((http("200 OK", &[]), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
        .drawing(0.25);
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let url = registry.base().join("manifests/v1").unwrap();
        let t0 = Instant::now();
        let (response, _) = registry.request("GET", &url, &[]).unwrap();
        let took = t0.elapsed();
        assert_eq!(response.status, 200);
        drop(response);
        assert!(took >= Duration::from_millis(1250 + 1500), "{took:?}");
        assert_eq!(server.requests().len(), 2, "cut at 1.25 s, asked again");
        let shown = registry.http.shown("127.0.0.1", server.port);
        let answers = &shown.answers;
        assert_eq!(
            (answers.answered.total(), answers.cut.total(), answers.agains),
            (1, 1, 1),
            "{shown:?}"
        );
        // Its connections, the cut one's and the one after, each made in time.
        assert!(shown.connect.n >= 2, "{shown:?}");
        // What it showed is waited for: the same answer again, asked once.
        let (response, _) = registry.request("GET", &url, &[]).unwrap();
        drop(response);
        assert_eq!(server.requests().len(), 3);
    }

    /// A download silent past its path's patience is cut and resumed where it was, and
    /// its silences learned: one that pauses 1.5 s, past the second a path starts with, is
    /// fetched whole over two connections, and its path waits longer after (D222).
    #[test]
    fn a_silent_download_is_cut_and_resumed_and_its_silence_learned() {
        use std::io::{BufRead, BufReader, Write};
        let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 249) as u8).collect();
        let digest = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(&blob[..]));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = blob.clone();
        let served = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            while ranges.len() < 2 {
                let n = ranges.len();
                let (mut tcp, _) = listener.accept().unwrap();
                // Asked in TLS first, it answers as a plain registry does (testing.rs).
                let mut first = [0u8; 1];
                if tcp.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = tcp.write_all(crate::testing::GO_BAD_REQUEST);
                    continue;
                }
                let mut reader = BufReader::new(tcp.try_clone().unwrap());
                let mut from = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(r) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        from = r.trim().trim_end_matches('-').parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                ranges.push(from);
                let rest = &body[from..];
                let status = if from == 0 {
                    "200 OK"
                } else {
                    "206 Partial Content"
                };
                let _ = write!(tcp, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n", rest.len());
                if n == 0 {
                    // Half, then silence past a second, then the rest, which no one reads.
                    let _ = tcp.write_all(&rest[..100_000]);
                    std::thread::sleep(Duration::from_millis(1500));
                    let _ = tcp.write_all(&rest[100_000..]);
                } else {
                    let _ = tcp.write_all(rest);
                }
            }
            ranges
        });
        let dir = shards_testdir::TempDir::new(&format!("silent-{port}")).unwrap();
        let store = Store::open(&dir).unwrap();
        let reference = Reference::parse(&format!("127.0.0.1:{port}/test/image:v1")).unwrap();
        // Its silence cut at the 1.25 s drawn from a second.
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
        .drawing(0.25);
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let desc = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            digest: digest.to_string(),
            size: 200_000,
            platform: None,
            annotations: Default::default(),
        };
        registry
            .fetch_blob(&store, &desc, &Limits::none(), &|_| {})
            .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&digest)).unwrap(), blob);
        let ranges = served.join().unwrap();
        assert_eq!(ranges[0], 0);
        assert!(ranges[1] > 0, "resumed where it was cut: {ranges:?}");
        let shown = registry.http.shown("127.0.0.1", port);
        let silences = &shown.silences;
        assert!(silences.answered.total() > 0, "{shown:?}");
        assert_eq!((silences.cut.total(), silences.agains), (1, 1), "{shown:?}");
    }

    /// A whole blob whose answer ends as its framing says, short of its descriptor's size,
    /// fails at once, asked once: the registry's own answer, which no asking again changes.
    #[test]
    fn a_blob_answered_whole_short_of_its_size_fails_at_once() {
        let blob: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let digest = Digest::from_hash(Algorithm::Sha256, &Sha256::digest(&blob[..]));
        let server = route(None, move |_| {
            let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", blob.len()).into_bytes();
            out.extend_from_slice(&blob);
            Some((out, After::Keep))
        });
        let dir = shards_testdir::TempDir::new(&format!("short-{}", server.port)).unwrap();
        let store = Store::open(&dir).unwrap();
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let desc = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            digest: digest.to_string(),
            size: 100_000,
            platform: None,
            annotations: Default::default(),
        };
        let t0 = Instant::now();
        let e = registry
            .fetch_blob(&store, &desc, &Limits::none(), &|_| {})
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("the registry's blob ends at 50000 bytes, where its descriptor says 100000"),
            "{e}"
        );
        assert_eq!(server.requests().len(), 1);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    }
}
