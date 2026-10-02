//! One repository on one registry (docs/research/registry-pull.md R1, R8):
//! - requests with containerd v2.4.1's retries, authorization and redirects
//!   (`core/remotes/docker/resolver.go`, `doWithRetries`);
//! - resolving a reference to a descriptor as its `Resolve` does;
//! - fetching content by digest into the store, verified, and resuming a blob's download
//!   where it stopped (`fetcher.go`, `httpreadseeker.go`).

use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use shards_image::oci::{Descriptor, MAX_MANIFEST, media};
use shards_image::reference::{Algorithm, DOCKER_HUB, Digest, Reference};
use shards_image::store::{Download, Held, Limits, Store};

use crate::auth::{Authorizer, Credentials, loopback};
use crate::http::{Client, Redirects, Request, Response};
use crate::url::Url;
use crate::{Error, ErrorKind};

/// containerd's `maxAttempts`.
const ATTEMPTS: usize = 5;
/// containerd's pause before trying a transient transport error again.
const PAUSE: Duration = Duration::from_millis(50);
/// What resolving accepts, in containerd's order (resolver.go:170-178).
const RESOLVE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, */*";
/// How much of an error response containerd reads for its message.
const MAX_ERROR_BODY: u64 = 64 << 10;
/// A download that stops this many times in a row without progress fails, as
/// containerd's `httpReadSeeker` gives up.
const MAX_STALLS: usize = 3;
const CHUNK: usize = 64 << 10;

/// A repository's registry, reached as containerd's default hosts reach it
/// (`core/remotes/docker/registry.go`): Docker Hub at `registry-1.docker.io`, loopback
/// hosts over plain HTTP, and every other host over https.
pub struct Registry {
    http: Client,
    auth: Authorizer,
    /// `scheme://host/v2/<repository>/`
    base: Url,
    /// containerd's `RepositoryScope`: pull access to the repository.
    scopes: Vec<String>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("base", &self.base.to_string())
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
    pub fn new(http: Client, reference: &Reference, credentials: Credentials) -> Result<Registry, Error> {
        let host = host(reference);
        let mut base = Url::parse(&format!("https://{host}/v2/{}/", reference.path))?;
        if loopback(&base) {
            base = Url::parse(&format!("http://{host}/v2/{}/", reference.path))?;
        }
        Ok(Registry {
            auth: Authorizer::new(&base, credentials),
            http,
            base,
            scopes: vec![format!("repository:{}:pull", reference.path)],
        })
    }

    /// Sends a request as containerd's `doWithRetries` does, up to 5 attempts:
    /// - a timeout or a cut connection is tried again after 50 ms;
    /// - a 401 is answered (auth.rs), then the request is sent again;
    /// - a HEAD of a manifest refused with 405 becomes a GET;
    /// - 408 is tried again, and 500, 503 or 504 once, unless it repeats.
    ///
    /// A 429 is not tried again, where containerd would at once: Docker Hub counts pulls
    /// over hours (§3.2). Returns the response and the method that got it.
    fn request<'a>(
        &self,
        method: &'a str,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<(Response<'_>, &'a str), Error> {
        let mut method = method;
        let mut last: Option<u16> = None;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let sent = self.http.follow(
                &Request {
                    method,
                    url,
                    headers,
                    body: &[],
                },
                &|hop| self.auth.authorization(&self.http, hop, &self.scopes),
                Redirects::Anywhere,
            );
            let mut response = match sent {
                Ok(response) => response,
                Err(e) if e.kind() == ErrorKind::Transient && attempt < ATTEMPTS => {
                    std::thread::sleep(PAUSE);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let repeated = last == Some(response.status);
            last = Some(response.status);
            let again = match response.status {
                401 => self.auth.challenged(response.url(), &response, repeated)?,
                405 if method == "HEAD" && url.target().contains("/manifests/") => {
                    method = "GET";
                    true
                }
                408 => true,
                500 | 503 | 504 => !repeated,
                _ => false,
            };
            if !again || attempt >= ATTEMPTS {
                return Ok((response, method));
            }
            // Read what little a refusal carries, so its connection can be used again.
            let _ = io::copy(&mut (&mut response).take(2 << 10), &mut io::sink());
        }
    }

    /// Resolves `reference` to a descriptor, as containerd's `Resolve` does:
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
                // A HEAD's 403 has no body: GET it for the registry's reason.
                403 if method == "HEAD" => {
                    let (get, _) = self.request("GET", &url, &accept)?;
                    return Err(refused(get, &name));
                }
                _ => return Err(refused(response, &name)),
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
                        return Err(refused(get, &name));
                    }
                    let media_type = manifest_type(&get);
                    if media_type == media::DOCKER_SCHEMA1_SIGNED || media_type == media::DOCKER_SCHEMA1 {
                        return Err(Error::new(format!(
                            "{name}: schema 1 manifests are not supported, as Docker no longer supports them"
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
        Err(Error::of(ErrorKind::NotFound, format!("{name}: not found")))
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
            Held::Missing => false,
            Held::Changed(_) => true,
        };
        let url = self.base.join(&format!("manifests/{digest}"))?;
        let accept = accept(&desc.media_type);
        let (mut response, _) = self.request(
            "GET",
            &url,
            &[("Accept", &accept), ("Accept-Encoding", "identity")],
        )?;
        if !(200..300).contains(&response.status) {
            return Err(refused(response, &digest));
        }
        let ingested = if changed {
            store.ingest_again(&digest, size, &mut response)
        } else {
            store.ingest(&digest, size, &mut response)
        };
        ingested.map_err(|e| Error::new(e.to_string()))?;
        store
            .content(desc, MAX_MANIFEST)?
            .ok_or_else(|| Error::new(format!("{digest}: gone from the store")))
    }

    /// A blob by digest into the store, verified. An interrupted download resumes with
    /// `Range: bytes=<offset>-`, as containerd's resumes do. A server that ignores the
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
        let mut stalls = 0;
        while download.offset() < size {
            let offset = download.offset();
            let range = format!("bytes={offset}-");
            let mut headers = vec![("Accept", accept.as_str()), ("Accept-Encoding", "identity")];
            if offset > 0 {
                headers.push(("Range", &range));
            }
            let (mut response, _) = self.request("GET", &url, &headers)?;
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
                _ => return Err(refused(response, &digest)),
            }
            let before = download.offset();
            match copy(&mut response, &mut download, progress) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::Transient => {}
                Err(e) => return Err(e.context(&digest)),
            }
            if download.offset() > before {
                stalls = 0;
            } else {
                stalls += 1;
                if stalls >= MAX_STALLS {
                    return Err(Error::of(
                        ErrorKind::Transient,
                        format!("{digest}: the download stopped {MAX_STALLS} times without progress"),
                    ));
                }
            }
        }
        download.commit().map(drop).map_err(|e| Error::new(e.to_string()))
    }
}

/// `<media type>, */*`, as containerd asks for content by descriptor.
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

/// Copies a response body into a download, telling `progress` what arrived.
fn copy(response: &mut Response, download: &mut Download, progress: &dyn Fn(u64)) -> Result<(), Error> {
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

/// distribution-spec's error body: `{"errors":[{"code","message"}]}` (spec.md:786-825).
#[derive(Deserialize)]
struct Errors {
    errors: Vec<ErrorEntry>,
}

#[derive(Deserialize)]
struct ErrorEntry {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// An error for a response that is not a success, with what the registry said: its
/// error codes, or for a 429 its rate limits (§3.2).
fn refused(mut response: Response, what: &dyn fmt::Display) -> Error {
    let status = response.status;
    if status == 404 {
        return Error::of(ErrorKind::NotFound, format!("{what}: not found"));
    }
    if status == 429 {
        return Error::new(format!("{what}: {}", rate_limited(&response)));
    }
    // What Docker says when its token did not open the repository.
    if status == 401 {
        return Error::new(format!(
            "pull access denied for {what}, repository does not exist or may require 'docker login'"
        ));
    }
    let mut body = Vec::new();
    let _ = (&mut response).take(MAX_ERROR_BODY).read_to_end(&mut body);
    let said = serde_json::from_slice::<Errors>(&body)
        .ok()
        .map(|e| {
            e.errors
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty());
    match said {
        Some(said) => Error::new(format!("{what}: status {status}: {said}")),
        None => Error::new(format!("{what}: status {status}")),
    }
}

/// Docker Hub's rate-limit fields, as its documentation names them: `ratelimit-limit`
/// and `ratelimit-remaining` (`<count>;w=<seconds>`), `docker-ratelimit-source`, and
/// `Retry-After`.
fn rate_limited(response: &Response) -> String {
    let mut out = String::from("too many requests: the registry's rate limit is spent");
    let window = |v: &str| match v.split_once(";w=") {
        Some((count, seconds)) => format!("{} per {} s", count.trim(), seconds.trim()),
        None => v.trim().to_string(),
    };
    if let Some(limit) = response.header("ratelimit-limit") {
        out.push_str(&format!("; limit {}", window(limit)));
    }
    if let Some(left) = response.header("ratelimit-remaining") {
        out.push_str(&format!(
            "; {} left",
            left.split(';').next().unwrap_or_default().trim()
        ));
    }
    if let Some(source) = response.header("docker-ratelimit-source") {
        out.push_str(&format!("; counted for {}", source.trim()));
    }
    if let Some(after) = response.header("retry-after") {
        out.push_str(&format!("; retry after {}", after.trim()));
    }
    out
}
