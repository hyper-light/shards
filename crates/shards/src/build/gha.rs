//! The GitHub Actions cache backend (D90): `--cache-to` and `--cache-from` `type=gha`, as
//! BuildKit's cache/remotecache/gha (v0.28.1) keeps a cache in GitHub's, through
//! go-actions-cache (its pin, 54bc28c): each layer an entry `buildkit-blob-1-DIGEST`, the
//! records an entry `index-SCOPE-1-HASH#N` that each export numbers anew, read back from
//! every scope the runtime token may read.
//!
//! Both of the service's protocols, as go-actions-cache speaks them and held to it by
//! `requests_are_go_actions_caches` (`scripts/gha/generate`): v2, GitHub's own (twirp, its
//! entries uploaded to Blob Storage by signed URL), and v1, the legacy service GitHub
//! Enterprise Server keeps (`_apis/artifactcache`).

use std::io::Read as _;
use std::time::{Duration, Instant};

use base64::Engine as _;
use shards_cmdline::buildflags::CacheEntry;
use shards_registry::http::{Client, Response};
use shards_registry::url::Url;
use zeroize::Zeroizing;

/// What each request reads of an answer (go-actions-cache's `LimitReader`).
const MOST_OF_AN_ANSWER: u64 = 32 << 10;
/// What one request uploads of an entry: the legacy service's chunk
/// (`UploadChunkSize`), and the Blob Storage block BuildKit's azblob cache uses, where
/// go-actions-cache's 1 MiB default caps an entry at 50,000 MiB (Blob Storage's 50,000
/// blocks) and sends 32 times the requests.
const CHUNK: u64 = 32 << 20;
/// How many chunks of an entry go at once to the legacy service (`UploadConcurrency`).
const AT_ONCE: usize = 4;
/// The Blob Storage service version the SDK asks for.
const BLOB_VERSION: &str = "2024-11-04";
const SERVICE: &str = "/twirp/github.actions.results.api.v1.CacheService/";
/// How long SaveMutable waits on an index number another export holds before passing it.
const FORCE: Duration = Duration::from_secs(15);

/// A scope of the runtime token: a ref, and whether it may be read (1) and written (2).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Scope {
    #[serde(rename = "Scope")]
    pub scope: String,
    #[serde(rename = "Permission")]
    pub permission: u64,
}

/// An entry found: its key, and where its body is.
pub struct Entry {
    pub key: String,
    url: String,
    /// A Blob Storage URL (v2), else the legacy service's archive.
    azure: bool,
}

/// Why a save failed: the key is taken (another export has it), or otherwise.
pub enum Unsaved {
    Exists,
    Failed(String),
}

impl From<String> for Unsaved {
    fn from(e: String) -> Unsaved {
        Unsaved::Failed(e)
    }
}

/// The service, as the cache's attributes name it.
pub struct Cache {
    http: Client,
    url: String,
    token: Zeroizing<String>,
    v2: bool,
    /// `scope`: the cache's own name among the repository's.
    pub name: String,
    /// The runtime token's scopes.
    pub scopes: Vec<Scope>,
    /// `timeout`: how long a request may wait out the service's rate limit.
    timeout: Duration,
    /// [`CHUNK`], less in tests.
    chunk: u64,
}

/// go-actions-cache's `version`: one for every key, the hash of its own name.
fn version() -> String {
    super::sha256(b"|go-actionscache-1.0").hex().to_string()
}

pub fn blob_key(digest: &str) -> String {
    format!("buildkit-blob-1-{digest}")
}

impl Cache {
    /// The cache of `e`'s attributes, as BuildKit's `getConfig` reads them, and its
    /// token's scopes, as go-actions-cache's `New` reads them.
    pub fn of(e: &CacheEntry, env: &dyn Fn(&str) -> Option<String>) -> Result<Cache, String> {
        let attr = |k: &str| e.attrs.get(k).map(String::as_str);
        let token = attr("token").ok_or("token not set for github actions cache")?;
        let mut version = match attr("version") {
            Some(v) => shards_cmdline::go::parse_int(v).map_err(|err| {
                format!(
                    "failed to parse api version {}, expected positive integer: {err}",
                    shards_cmdline::go::quote(v)
                )
            })?,
            None => 0,
        };
        let mut url = None;
        if version != 1
            && let Some(v) = attr("url_v2")
        {
            url = Some(v);
            version = 2;
        }
        if url.is_none() {
            url = attr("url");
        }
        let url = url
            .filter(|u| !u.is_empty())
            .ok_or("url not set for github actions cache")?;
        // As BuildKit guesses for clients that name no version.
        if version == 0 {
            version = if url.contains("results-receiver.actions.githubusercontent.com") {
                2
            } else {
                1
            };
        }
        let timeout = match attr("timeout") {
            Some(t) => shards_cmdline::gotime::duration(t)
                .map_err(|err| format!("failed to parse timeout for github actions cache: {err}"))?,
            None => 10 * 60 * 1_000_000_000,
        };
        let scopes = scopes_of(token)?;
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let agent = format!("shards/{}", env!("CARGO_PKG_VERSION"));
        let http = Client::new(Box::new(move |_| Ok(config.clone())), &agent)
            .with_proxies(shards_registry::proxy::Proxies::from_env(env));
        Ok(Cache {
            http,
            url: url.trim_end_matches('/').to_string(),
            token: Zeroizing::new(token.to_string()),
            v2: version > 1,
            name: attr("scope").unwrap_or("buildkit").to_string(),
            scopes,
            timeout: Duration::from_nanos(u64::try_from(timeout).unwrap_or(0)),
            chunk: CHUNK,
        })
    }

    /// The records' key for `scope`, as BuildKit's `indexKey` makes it.
    pub fn index_key(&self, scope: &str) -> String {
        let hash = super::sha256(scope.as_bytes());
        format!(
            "index-{}-1-{}",
            self.name,
            hash.hex().get(..8).unwrap_or_default()
        )
    }

    /// The scope an export writes: the last the token may write, as BuildKit takes it.
    pub fn write_scope(&self) -> &str {
        self.scopes
            .iter()
            .rfind(|s| s.permission & 2 != 0)
            .map_or("", |s| s.scope.as_str())
    }

    /// Sends a request to the service, waiting out its rate limit (429, `Retry-After`, else
    /// a backoff from 1 s to 90 s) for at most `timeout`, as go-actions-cache's
    /// `doWithRetries` does; a failure carries the service's own message.
    fn call(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Body<'_>,
    ) -> Result<Response<'_>, Unsaved> {
        let url = Url::parse(url).map_err(|e| e.to_string())?;
        let authorization = Zeroizing::new(format!("Bearer {}", self.token.as_str()));
        let mut all: Vec<(&str, &str)> = vec![("authorization", &authorization)];
        if !self.v2 {
            all.push(("accept", "application/json;api-version=6.0-preview.1"));
        }
        all.extend_from_slice(headers);
        let until = Instant::now() + self.timeout;
        let mut backoff = Duration::from_secs(1);
        let (bytes, file) = body.parts();
        loop {
            let r = self
                .http
                .send(&shards_registry::http::Request {
                    method,
                    url: &url,
                    headers: &all,
                    body: bytes,
                    file,
                })
                .map_err(|e| e.to_string())?;
            if (200..300).contains(&r.status) {
                return Ok(r);
            }
            if r.status == 429 {
                let wait = r
                    .header("retry-after")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .map_or(backoff, |s| Duration::from_secs(u64::try_from(s).unwrap_or(0)));
                drop(r);
                if Instant::now() + wait > until {
                    return Err(Unsaved::Failed(format!(
                        "the cache service asks to wait {wait:?}, past the timeout"
                    )));
                }
                std::thread::sleep(wait);
                backoff = (backoff * 2).min(Duration::from_secs(90));
                continue;
            }
            return Err(refused(r));
        }
    }

    /// A JSON request to the service, and its JSON answer.
    fn json(&self, method: &str, url: &str, body: &serde_json::Value) -> Result<serde_json::Value, Unsaved> {
        let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let r = self.call(
            method,
            url,
            &[("content-type", "application/json")],
            Body::Bytes(&body),
        )?;
        let answer = read(r)?;
        if answer.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        Ok(serde_json::from_slice(&answer).map_err(|e| e.to_string())?)
    }

    fn twirp(&self, method: &str, body: &serde_json::Value) -> Result<serde_json::Value, Unsaved> {
        self.json("POST", &format!("{}{SERVICE}{method}", self.url), body)
    }

    fn v1(&self, path: &str) -> String {
        format!("{}/_apis/artifactcache/{path}", self.url)
    }

    /// The newest entry whose key begins with the first of `keys` that any begins with.
    pub fn load(&self, keys: &[&str]) -> Result<Option<Entry>, String> {
        let first = keys.first().copied().unwrap_or_default();
        if self.v2 {
            let v = self
                .twirp(
                    "GetCacheEntryDownloadURL",
                    &serde_json::json!({ "key": first, "restore_keys": keys, "version": version() }),
                )
                .map_err(failure)?;
            if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                return Ok(None);
            }
            return Ok(Some(Entry {
                key: v
                    .get("matched_key")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                url: v
                    .get("signed_download_url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                azure: true,
            }));
        }
        let url = format!(
            "{}?keys={}&version={}",
            self.v1("cache"),
            query_escape(&keys.join(",")),
            version()
        );
        let r = self.call("GET", &url, &[], Body::Bytes(&[])).map_err(failure)?;
        let answer = read(r).map_err(failure)?;
        if answer.is_empty() {
            return Ok(None);
        }
        let v: serde_json::Value = serde_json::from_slice(&answer).map_err(|e| e.to_string())?;
        let key = v
            .get("cacheKey")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if key.is_empty() {
            return Ok(None);
        }
        Ok(Some(Entry {
            key: key.to_string(),
            url: v
                .get("archiveLocation")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            azure: false,
        }))
    }

    /// An entry's body.
    pub fn download(&self, e: &Entry) -> Result<Response<'_>, String> {
        let url = Url::parse(&e.url).map_err(|e| e.to_string())?;
        let headers: &[(&str, &str)] = if e.azure {
            &[("accept", "application/xml"), ("x-ms-version", BLOB_VERSION)]
        } else {
            &[]
        };
        let r = self
            .http
            .send(&shards_registry::http::Request {
                method: "GET",
                url: &url,
                headers,
                body: &[],
                file: None,
            })
            .map_err(|e| e.to_string())?;
        if !(200..300).contains(&r.status) {
            return Err(format!(
                "invalid status response {} for the entry {}",
                r.status_text(),
                e.key
            ));
        }
        Ok(r)
    }

    /// Saves `len` bytes of `file` (or `bytes`) as `key`: reserved, uploaded, committed.
    pub fn save(&self, key: &str, body: Body<'_>) -> Result<(), Unsaved> {
        if self.v2 {
            let v = self.twirp(
                "CreateCacheEntry",
                &serde_json::json!({ "key": key, "version": version() }),
            )?;
            if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(Unsaved::Failed("failed to reserve cache".into()));
            }
            let url = v
                .get("signed_upload_url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            self.upload_blob(url, body)?;
            let v = self.twirp(
                "FinalizeCacheEntryUpload",
                &serde_json::json!({ "key": key, "size_bytes": body.len(), "version": version() }),
            )?;
            if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(Unsaved::Failed("failed to commit cache".into()));
            }
            return Ok(());
        }
        let v = self.json(
            "POST",
            &self.v1("caches"),
            &serde_json::json!({ "key": key, "version": version() }),
        )?;
        let id = v
            .get("cacheId")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n != 0)
            .ok_or_else(|| format!("invalid response {v}"))?;
        let url = self.v1(&format!("caches/{id}"));
        self.upload_chunks(&url, body)?;
        self.json("POST", &url, &serde_json::json!({ "size": body.len() }))?;
        Ok(())
    }

    /// The body to Blob Storage at a signed URL: in one request below [`CHUNK`], else in
    /// blocks of it, committed together.
    fn upload_blob(&self, signed: &str, body: Body<'_>) -> Result<(), String> {
        let put = |url: &str, headers: &[(&str, &str)], body: Body<'_>| -> Result<(), String> {
            let url = Url::parse(url).map_err(|e| e.to_string())?;
            let mut all = vec![("accept", "application/xml")];
            all.extend_from_slice(headers);
            all.push(("x-ms-version", BLOB_VERSION));
            let (bytes, file) = body.parts();
            let r = self
                .http
                .send(&shards_registry::http::Request {
                    method: "PUT",
                    url: &url,
                    headers: &all,
                    body: bytes,
                    file,
                })
                .map_err(|e| e.to_string())?;
            if (200..300).contains(&r.status) {
                Ok(())
            } else {
                Err(format!(
                    "uploading to the cache's storage: {}: {}",
                    r.status_text(),
                    r.header("x-ms-error-code").unwrap_or_default()
                ))
            }
        };
        let len = body.len();
        if len < self.chunk {
            return put(
                signed,
                &[
                    ("content-type", "application/octet-stream"),
                    ("x-ms-blob-type", "BlockBlob"),
                ],
                body,
            );
        }
        let sep = if signed.contains('?') { '&' } else { '?' };
        let mut list = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<BlockList>");
        let (mut from, mut n) = (0u64, 0u64);
        while from < len {
            let size = (len - from).min(self.chunk);
            let id = base64::engine::general_purpose::STANDARD.encode(format!("shards-{n:010}"));
            put(
                &format!("{signed}{sep}blockid={}&comp=block", query_escape(&id)),
                &[("content-type", "application/octet-stream")],
                body.slice(from, size),
            )?;
            list.push_str(&format!("<Latest>{id}</Latest>"));
            from += size;
            n += 1;
        }
        list.push_str("</BlockList>");
        put(
            &format!("{signed}{sep}comp=blocklist"),
            &[("content-type", "application/xml")],
            Body::Bytes(list.as_bytes()),
        )
    }

    /// The body to the legacy service, [`CHUNK`]s at a time, [`AT_ONCE`] at once.
    fn upload_chunks(&self, url: &str, body: Body<'_>) -> Result<(), Unsaved> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let len = body.len();
        let next = AtomicU64::new(0);
        let failed = AtomicBool::new(false);
        let work = || -> Result<(), Unsaved> {
            while !failed.load(Ordering::Relaxed) {
                let from = next.fetch_add(CHUNK, Ordering::Relaxed);
                if from >= len {
                    break;
                }
                let size = (len - from).min(CHUNK);
                let range = format!("bytes {from}-{}/*", from + size - 1);
                let sent = self.call(
                    "PATCH",
                    url,
                    &[
                        ("content-type", "application/octet-stream"),
                        ("content-range", &range),
                    ],
                    body.slice(from, size),
                );
                if let Err(e) = sent.and_then(|r| read(r).map(drop)) {
                    failed.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            }
            Ok(())
        };
        let chunks = usize::try_from(len.div_ceil(CHUNK)).unwrap_or(AT_ONCE);
        std::thread::scope(|s| {
            let spawned: Vec<_> = (0..AT_ONCE.min(chunks))
                .map(|_| {
                    std::thread::Builder::new()
                        .name("gha-upload".into())
                        .spawn_scoped(s, work)
                })
                .collect();
            let mut result = Ok(());
            for h in spawned {
                let r = match h {
                    Ok(h) => h
                        .join()
                        .unwrap_or_else(|_| Err(Unsaved::Failed("an upload ended in a panic".into()))),
                    Err(e) => {
                        failed.store(true, Ordering::Relaxed);
                        Err(Unsaved::Failed(format!("starting an upload: {e}")))
                    }
                };
                if result.is_ok() {
                    result = r;
                }
            }
            result
        })
    }

    /// Saves `bytes` as the next number of `key`, as go-actions-cache's `SaveMutable` does:
    /// after the newest there, waiting up to [`FORCE`] on a number another export holds,
    /// then passing it.
    pub fn save_mutable(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        let prefix = format!("{key}#");
        let mut blocked = Duration::ZERO;
        'fresh: loop {
            let newest = self.load(&[&prefix])?;
            let mut idx: u64 = 0;
            if let Some(e) = &newest {
                // Another export may have saved meanwhile.
                if self.load(&[&prefix])?.is_none_or(|again| again.key != e.key) {
                    continue;
                }
                let n = e.key.strip_prefix(&prefix).unwrap_or_default();
                if n.is_empty() {
                    return Err(format!("corrupt empty index for {key}"));
                }
                idx = n
                    .parse()
                    .map_err(|err| format!("failed to parse {key} index: {err}"))?;
            }
            loop {
                idx += 1;
                match self.save(&format!("{key}#{idx}"), Body::Bytes(bytes)) {
                    Ok(()) => return Ok(()),
                    Err(Unsaved::Exists) if blocked <= FORCE => {
                        blocked += Duration::from_secs(2);
                        std::thread::sleep(Duration::from_secs(2));
                        continue 'fresh;
                    }
                    // Held a long time: its export may have died; the next number.
                    Err(Unsaved::Exists) => {}
                    Err(Unsaved::Failed(e)) => return Err(e),
                }
            }
        }
    }
}

fn failure(u: Unsaved) -> String {
    match u {
        Unsaved::Exists => "the cache entry already exists".into(),
        Unsaved::Failed(e) => e,
    }
}

/// A request's body.
#[derive(Clone, Copy)]
pub enum Body<'a> {
    Bytes(&'a [u8]),
    File(&'a std::fs::File, u64, u64),
}

impl<'a> Body<'a> {
    fn len(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::File(_, _, n) => *n,
        }
    }

    /// `len` bytes from `from`.
    fn slice(self, from: u64, len: u64) -> Body<'a> {
        match self {
            Body::Bytes(b) => {
                let at = usize::try_from(from).unwrap_or(usize::MAX);
                let end = at.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
                Body::Bytes(b.get(at..end).unwrap_or_default())
            }
            Body::File(f, at, _) => Body::File(f, at + from, len),
        }
    }

    #[allow(clippy::type_complexity)]
    fn parts(self) -> (&'a [u8], Option<(&'a std::fs::File, u64, u64)>) {
        match self {
            Body::Bytes(b) => (b, None),
            Body::File(f, at, n) => (&[], Some((f, at, n))),
        }
    }
}

/// An answer's body, as much as go-actions-cache reads.
fn read(r: Response<'_>) -> Result<Vec<u8>, Unsaved> {
    let mut out = Vec::new();
    r.take(MOST_OF_AN_ANSWER)
        .read_to_end(&mut out)
        .map_err(|e| Unsaved::Failed(e.to_string()))?;
    Ok(out)
}

/// A refusal, as go-actions-cache's `checkResponse` reads it: the legacy service's
/// `message` and `typeKey`, or v2's twirp `code` and `msg`; `AlreadyExists` among them a
/// key taken.
fn refused(r: Response<'_>) -> Unsaved {
    let status = r.status_text();
    let body = match read(r) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let body = body.strip_prefix("\u{feff}".as_bytes()).unwrap_or(&body);
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let text = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (message, kind) = if !text("message").is_empty() && !text("typeKey").is_empty() {
        (text("message"), text("typeKey"))
    } else if !text("code").is_empty() {
        let message = [text("msg"), text("message")]
            .into_iter()
            .find(|m| !m.is_empty())
            .unwrap_or_else(|| text("code"));
        (message, text("code"))
    } else if !text("message").is_empty() {
        (text("message"), String::new())
    } else {
        return Unsaved::Failed(format!(
            "unknown error {status}: {}",
            String::from_utf8_lossy(body)
        ));
    };
    if kind.contains("AlreadyExists")
        || kind == "already_exists"
        || message.to_ascii_lowercase().contains("already exists")
    {
        return Unsaved::Exists;
    }
    Unsaved::Failed(message)
}

/// The runtime token's scopes (`ac`), checked to be in force (`nbf`, `exp`), as
/// go-actions-cache's `New` reads it; its signature is the service's to check.
fn scopes_of(token: &str) -> Result<Vec<Scope>, String> {
    let claims = token.split('.').nth(1).ok_or("the cache token is no JWT")?;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .map_err(|e| format!("the cache token's claims: {e}"))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&claims).map_err(|e| format!("the cache token's claims: {e}"))?;
    let ac = claims
        .get("ac")
        .ok_or("invalid token without access controls")?
        .as_str()
        .ok_or("invalid token with access controls not a string")?;
    let time = |k: &str| -> Result<i64, String> {
        claims
            .get(k)
            .ok_or_else(|| format!("invalid token without {k}"))?
            .as_f64()
            .map(|t| t as i64)
            .ok_or_else(|| format!("invalid token with {k} not a number"))
    };
    let (exp, nbf) = (time("exp")?, time("nbf")?);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    if now > exp {
        return Err(format!("cache token expired at {}", rfc3339(exp)));
    }
    if now < nbf {
        return Err(format!("invalid token with future issue time {}", rfc3339(nbf)));
    }
    serde_json::from_str(ac).map_err(|e| format!("failed to parse token access controls: {e}"))
}

/// Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            _ if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) => out.push(char::from(b)),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `secs` since the epoch, as RFC 3339 writes it in UTC.
fn rfc3339(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        Err(_) => secs.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Write as _};

    /// An entry past a chunk goes to its signed URL in blocks, the URL's own query kept,
    /// and is committed as one, its blocks in order; a smaller one in one request.
    #[test]
    fn entries_go_to_blob_storage_in_blocks() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Mutex::new(Vec::<(String, Vec<u8>)>::new());
        let token = format!(
            "e30.{}.x",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"ac":"[]","exp":4102444800,"nbf":1577836800}"#)
        );
        let e = CacheEntry {
            kind: "gha".into(),
            attrs: [("token", token.as_str()), ("url_v2", "http://127.0.0.1:1/")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let mut cache = Cache::of(&e, &|_| None).unwrap();
        cache.chunk = 4;
        let body = b"0123456789".to_vec();
        std::thread::scope(|s| {
            s.spawn(|| {
                while seen.lock().unwrap().len() < 5 {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut out = stream;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 {
                            break;
                        }
                        let mut length = 0;
                        loop {
                            let mut h = String::new();
                            reader.read_line(&mut h).unwrap();
                            if h.trim().is_empty() {
                                break;
                            }
                            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap();
                            }
                        }
                        let mut b = vec![0u8; length];
                        reader.read_exact(&mut b).unwrap();
                        seen.lock().unwrap().push((line.trim().to_string(), b));
                        out.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n")
                            .unwrap();
                        if seen.lock().unwrap().len() == 5 {
                            break;
                        }
                    }
                }
            });
            let url = format!("http://127.0.0.1:{port}/blob/k?sig=w");
            cache.upload_blob(&url, Body::Bytes(&body)).unwrap();
            cache.upload_blob(&url, Body::Bytes(b"abc")).unwrap();
        });
        let seen = seen.into_inner().unwrap();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        let id = |n: u64| {
            query_escape(&base64::engine::general_purpose::STANDARD.encode(format!("shards-{n:010}")))
        };
        assert_eq!(
            lines,
            [
                format!("PUT /blob/k?sig=w&blockid={}&comp=block HTTP/1.1", id(0)),
                format!("PUT /blob/k?sig=w&blockid={}&comp=block HTTP/1.1", id(1)),
                format!("PUT /blob/k?sig=w&blockid={}&comp=block HTTP/1.1", id(2)),
                "PUT /blob/k?sig=w&comp=blocklist HTTP/1.1".to_string(),
                "PUT /blob/k?sig=w HTTP/1.1".to_string(),
            ]
        );
        let blocks: Vec<&[u8]> = seen.iter().take(3).map(|(_, b)| b.as_slice()).collect();
        assert_eq!(blocks, [b"0123".as_slice(), b"4567", b"89"]);
        let list = String::from_utf8(seen[3].1.clone()).unwrap();
        let ids: Vec<&str> = list
            .split("<Latest>")
            .skip(1)
            .map(|p| p.split("</Latest>").next().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(seen[4].1, b"abc");
    }
}
