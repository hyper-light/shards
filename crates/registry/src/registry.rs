//! One repository on one registry (docs/research/registry-pull.md R1, R8):
//! - requests with containerd v2.4.1's retries, authorization and redirects
//!   (`core/remotes/docker/resolver.go`, `doWithRetries`);
//! - resolving a reference to a descriptor as its `Resolve` does;
//! - fetching content by digest into the store, verified, and resuming a blob's download
//!   where it stopped (`fetcher.go`, `httpreadseeker.go`).

use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use shards_image::oci::{Descriptor, MAX_MANIFEST, media};
use shards_image::reference::{Algorithm, DOCKER_HUB, Digest, Reference};
use shards_image::store::{Download, Held, Limits, Store};

use crate::auth::{Authorizer, Credentials, loopback};
use crate::http::{Client, Redirects, Request, Response};
use crate::url::Url;
use crate::{Error, ErrorKind, Said, printable};

/// containerd's `maxAttempts`.
const ATTEMPTS: usize = 5;
/// containerd's pause before trying a transient transport error again.
const PAUSE: Duration = Duration::from_millis(50);
/// What resolving accepts, in containerd's order (resolver.go:170-178).
const RESOLVE_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, */*";
/// How much of a refusal's body containerd reads for what the registry says
/// (`remotes/errors.NewUnexpectedStatusErr`).
const MAX_ERROR_BODY: u64 = 64000;
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
    /// A registry to push `reference`'s repository to, as containerd's pusher asks:
    /// pull and push access to it, and pull access to `mounts`, repositories of the same
    /// registry its blobs may be mounted from.
    pub fn for_push(
        http: Client,
        reference: &Reference,
        credentials: Credentials,
        mounts: &[String],
    ) -> Result<Registry, Error> {
        let mut registry = Registry::new(http, reference, credentials)?;
        registry.scopes = vec![format!("repository:{}:pull,push", reference.path)];
        registry
            .scopes
            .extend(mounts.iter().map(|m| format!("repository:{m}:pull")));
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

    /// Uploads the blob `desc` describes from `file`, as containerd v2.4.1's pusher does
    /// (core/remotes/docker/pusher.go, Push and Commit): mounted from `from`, a repository
    /// of the same registry, when given and the registry does, and uploaded as if it were
    /// not given when the mount is refused as unauthorized; else an upload begun with a
    /// POST, which a 201 answers as there already, and done with one PUT of it all, whose
    /// `Docker-Content-Digest`, if it sends one, must be the blob's. Whether it was
    /// mounted.
    pub fn upload(&self, desc: &Descriptor, file: &std::fs::File, from: Option<&str>) -> Result<bool, Error> {
        let mut started = None;
        if let Some(repo) = from {
            let url = self
                .base
                .join(&format!("blobs/uploads/?mount={}&from={repo}", desc.digest))?;
            match self.send("POST", &url, &[], &[], None) {
                // Not allowed to read `from`: uploaded instead.
                Ok((response, _)) if response.status == 401 => {}
                Ok((response, _)) => started = Some((response, true)),
                Err(e) if e.kind() == ErrorKind::Unauthorized => {}
                Err(e) => return Err(e.context(format!("pushing with mount from {repo}"))),
            }
        }
        let (response, mounting) = match started {
            Some(started) => started,
            None => match self.send("POST", &self.base.join("blobs/uploads/")?, &[], &[], None) {
                Ok((response, _)) => (response, false),
                Err(e) if e.kind() == ErrorKind::Unauthorized => {
                    return Err(e.context(
                        "push access denied, repository does not exist or may require authorization",
                    ));
                }
                Err(e) => return Err(e),
            },
        };
        let location = match response.status {
            201 => return Ok(mounting),
            200 | 202 | 204 => response
                .header("location")
                .map(str::to_string)
                .ok_or_else(|| Error::new(format!("{}: an upload with no Location", desc.digest)))?,
            _ => return Err(unexpected("POST", response)),
        };
        let mut put = response.url().join(&location)?;
        put = put.with_query_pair("digest", &desc.digest)?;
        let size = desc.size().map_err(|e| Error::new(e.to_string()))?;
        let (response, _) = self.send(
            "PUT",
            &put,
            &[("Content-Type", "application/octet-stream")],
            &[],
            Some((file, size)),
        )?;
        // As containerd's pusher takes them: 202 is not among them, as the request that
        // gets it hears (pusher.go), though `Commit` would take it.
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
        Ok(false)
    }

    /// Puts a manifest or index as `reference` names it (a tag or its digest).
    pub fn put_manifest(&self, reference: &str, media_type: &str, bytes: &[u8]) -> Result<(), Error> {
        let url = self.base.join(&format!("manifests/{reference}"))?;
        let (response, _) = self.send("PUT", &url, &[("Content-Type", media_type)], bytes, None)?;
        match response.status {
            200 | 201 | 204 => Ok(()),
            _ => Err(unexpected("PUT", response)),
        }
    }

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
        self.send(method, url, headers, &[], None)
    }

    /// [`request`](Self::request), with a body: `body`, or so much of a file.
    fn send<'a>(
        &self,
        method: &'a str,
        url: &Url,
        headers: &[(&str, &str)],
        body: &[u8],
        file: Option<(&std::fs::File, u64)>,
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
                    body,
                    file,
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
            match copied {
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

/// What content a fetch takes encoded for its transfer, as containerd v2.4.1's fetcher
/// asks for it (core/remotes/docker/fetcher.go, open).
const ENCODINGS: &str = "zstd;q=1.0, gzip;q=0.8, deflate;q=0.5";

/// A response body as its `Content-Encoding` says to decode it, as containerd's fetcher
/// decodes one: each coding undone, last first; zstd, gzip (every member, as Go's reader
/// reads them), deflate (raw, RFC 1951, as Go's flate reads it), or none.
fn decoded<'a>(body: &'a mut dyn Read, encoding: &str) -> Result<Box<dyn Read + 'a>, Error> {
    let codings: Vec<String> = encoding
        .split([' ', '\t', ','])
        .filter(|c| !c.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut body: Box<dyn Read + 'a> = Box::new(body);
    for coding in codings.iter().rev() {
        body = match coding.as_str() {
            "zstd" => Box::new(shards_image::store::Zstd::new(io::BufReader::new(body))),
            "gzip" => Box::new(flate2::read::MultiGzDecoder::new(body)),
            "deflate" => Box::new(flate2::read::DeflateDecoder::new(body)),
            "identity" => body,
            other => {
                return Err(Error::new(format!(
                    "unsupported Content-Encoding algorithm: {other}"
                )));
            }
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
        let mut said = dockerd(self.status, body);
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
/// it, from as much of its body as containerd reads.
fn unexpected(method: &str, mut response: Response) -> Error {
    let refusal = Refusal::of(&response);
    let mut body = Vec::new();
    let _ = (&mut response).take(MAX_ERROR_BODY).read_to_end(&mut body);
    refusal.error(method, &body)
}

/// A fetch's refusal, as containerd words it (`withErrorCheck`): a 404 is no content at
/// `url`, the URL asked.
fn not_fetched(response: Response, url: &Url) -> Error {
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
fn dockerd(status: u16, body: &[u8]) -> Said {
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
        let path = std::env::temp_dir().join(format!("shards-upload-{}-{n}", std::process::id()));
        std::fs::write(&path, blob).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let server = route(None, move |seen| Some((answer(seen), After::Keep)));
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = Client::new(
            Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
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
        let done = registry.upload(&desc, &file, from);
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
    #[test]
    fn existence_is_asked_as_containerd_asks_it() {
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
                Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
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
    }

    /// A scripted registry's answer to the `n`th request.
    type Answer = Box<dyn Fn(&Seen, usize) -> (Vec<u8>, After) + Send + Sync>;

    /// Blobs are asked for compressed for their transfer and decoded as containerd's
    /// fetcher decodes them; a resumed download asks for the blob as it is.
    #[test]
    fn transfer_encodings_are_decoded_as_containerd_decodes_them() {
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
            let dir =
                std::env::temp_dir().join(format!("shards-encodings-{}-{}", std::process::id(), server.port));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = Store::open(&dir).unwrap();
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
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
            (gzip(&gzip(&blob)), "gzip, gzip"),
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
    fn refusals_are_said_in_dockerds_words() {
        let whole = |status, body: &str| match dockerd(status, body.as_bytes()) {
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
    fn refusals_are_worded_as_containerd_words_them() {
        let serving = |answer: &'static [u8]| {
            let server = route(None, move |_| Some((answer.to_vec(), After::Keep)));
            let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
            let http = Client::new(
                Box::new(|url| Err(Error::new(format!("{url}: no TLS here")))),
                "shards-test",
            );
            let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
            (server, registry)
        };
        let dir = std::env::temp_dir().join(format!("shards-refusal-words-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
                "content at http://127.0.0.1:{}/v2/test/image/blobs/{digest} not found: not found",
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
            limited.in_dockerds_words().to_string(),
            "error from registry: You have reached your pull rate limit. (limit 100 per 21600 s; 0 left)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The statuses containerd takes: an upload begun with 200, 202 or 204, or found there
    /// already with 201; done with 200, 201 or 204, its digest checked when sent.
    #[test]
    fn uploads_take_the_statuses_containerd_takes() {
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
}
