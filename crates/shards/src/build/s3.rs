//! The S3 cache backend (D88): `--cache-to` and `--cache-from` `type=s3`, as BuildKit's
//! cache/remotecache/s3 (v0.28.1) reads its attributes and lays out its bucket: each
//! layer at `prefix` `blobs_prefix` DIGEST, the cache's records at `prefix`
//! `manifests_prefix` NAME for each of `name`'s names, the first read back, and a layer
//! already there touched (copied onto itself) when older than `touch_refresh`.
//!
//! Requests are signed with AWS Signature Version 4, as aws-sdk-go-v2's signer signs them
//! for BuildKit's S3 client (`DisableURIPathEscaping`, as S3's client sets it), held to
//! that signer's own output by `sigv4_is_aws_sdk_go_v2s`. Unlike BuildKit's, a layer's
//! body is signed whole: its digest is its SHA-256, so S3 checks every byte it keeps,
//! for nothing more read here.

use std::io::Read as _;

use aws_lc_rs::hmac;
use sha2::{Digest as _, Sha256};
use shards_cmdline::buildflags::CacheEntry;
use shards_registry::http::{Client, Response};
use shards_registry::url::Url;
use zeroize::Zeroizing;

/// The headers the signer signs beside `host` and every `x-amz-*` (v4 `RequiredSigned
/// Headers`), lowercase; `content-length` where the body is not empty.
const SIGNED: &[&str] = &[
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-md5",
    "content-type",
    "expires",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "range",
];

/// A body not hashed (S3's `UNSIGNED-PAYLOAD`), and the hash of an empty one.
pub const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

/// Who signs: an access key, its secret, and a session's token if any.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret: Zeroizing<String>,
    pub token: Option<Zeroizing<String>>,
}

/// Neither the secret nor the token is shown.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({})", self.access_key)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// HMAC-SHA256 (RFC 2104), AWS-LC's.
fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg)
        .as_ref()
        .to_vec()
}

/// RFC 3986's encoding of a query's key or value, as the signer writes them: unreserved
/// characters as they are, every other byte `%XX`.
fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query string's pairs, decoded.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    let decode = |s: &str| -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            let hexval = |c: u8| char::from(c).to_digit(16);
            match (
                c,
                b.get(i + 1).copied().and_then(hexval),
                b.get(i + 2).copied().and_then(hexval),
            ) {
                (b'%', Some(h), Some(l)) => {
                    out.push(u8::try_from(h * 16 + l).unwrap_or(0));
                    i += 3;
                }
                (b'+', _, _) => {
                    out.push(b' ');
                    i += 1;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// A request to sign: its method, its URL's authority (host, and port unless the
/// scheme's own), its escaped path and its query, its headers, the length of its body
/// and its payload's hash (or [`UNSIGNED`]).
pub struct Request<'a> {
    pub method: &'a str,
    pub authority: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub length: u64,
    pub payload_hash: &'a str,
}

/// The headers SigV4 adds to `r` for `creds` in `region`, for S3, at `when` (UTC, as
/// `YYYYMMDDTHHMMSSZ`): `x-amz-date`, `x-amz-content-sha256`, `x-amz-security-token` where
/// there is a token, and `authorization`.
pub fn sign(r: &Request<'_>, creds: &Credentials, region: &str, when: &str) -> Vec<(String, String)> {
    let date = when.get(..8).unwrap_or(when);
    let mut added = vec![
        ("x-amz-date".to_string(), when.to_string()),
        ("x-amz-content-sha256".to_string(), r.payload_hash.to_string()),
    ];
    if let Some(t) = &creds.token {
        added.push(("x-amz-security-token".to_string(), t.to_string()));
    }
    // The headers signed: host, content-length if any, those of SIGNED and x-amz-*.
    let mut signed: Vec<(String, String)> = vec![("host".to_string(), r.authority.to_string())];
    if r.length > 0 {
        signed.push(("content-length".to_string(), r.length.to_string()));
    }
    for (k, v) in r
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_string()))
        .chain(added.clone())
    {
        if (SIGNED.contains(&k.as_str()) || k.starts_with("x-amz-")) && !signed.iter().any(|(n, _)| *n == k) {
            signed.push((k, v));
        }
    }
    signed.sort();
    let names: Vec<&str> = signed.iter().map(|(k, _)| k.as_str()).collect();
    let canonical_headers: String = signed
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    let mut pairs: Vec<(String, String)> = query_pairs(r.query)
        .into_iter()
        .map(|(k, v)| (uri_encode(&k), uri_encode(&v)))
        .collect();
    pairs.sort();
    let canonical_query: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let path = if r.path.is_empty() { "/" } else { r.path };
    let canonical = format!(
        "{}\n{path}\n{}\n{canonical_headers}\n{}\n{}",
        r.method,
        canonical_query.join("&"),
        names.join(";"),
        r.payload_hash
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{when}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let key = [region, "s3", "aws4_request"].iter().fold(
        mac(
            Zeroizing::new(format!("AWS4{}", creds.secret.as_str())).as_bytes(),
            date.as_bytes(),
        ),
        |k, part| mac(&k, part.as_bytes()),
    );
    added.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={}, Signature={}",
            creds.access_key,
            names.join(";"),
            hex(&mac(&key, to_sign.as_bytes()))
        ),
    ));
    added
}

/// The most one PutObject, CopyObject or part takes (S3's limits): a larger layer goes in
/// parts of this size, as BuildKit's touch copies one (`maxCopyObjectSize`).
const MOST_AT_ONCE: u64 = 5 << 30;
/// The most of an error's body read for its code and message.
const MOST_OF_AN_ERROR: u64 = 64 << 10;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A bucket's cache: where it is, who signs for it, and BuildKit's attributes.
pub struct Bucket {
    http: Client,
    bucket: String,
    region: String,
    prefix: String,
    manifests_prefix: String,
    blobs_prefix: String,
    /// `name`'s names, `;` apart: written each, read the first.
    pub names: Vec<String>,
    /// `touch_refresh`, in nanoseconds.
    touch_refresh: i128,
    /// `endpoint_url` (or `$AWS_ENDPOINT_URL_S3`, `$AWS_ENDPOINT_URL`), if not AWS's own.
    endpoint: Option<Url>,
    path_style: bool,
    pub parallelism: usize,
    creds: Credentials,
    /// The most sent at once: [`MOST_AT_ONCE`], less in tests.
    part: u64,
}

/// Writes part `number` of upload `id`, `len` bytes from `from`: its ETag.
type Part<'a> = dyn Fn(u64, &str, u64, u64) -> Result<String, String> + 'a;

/// A request's body.
#[derive(Clone, Copy)]
pub enum Body<'a> {
    Bytes(&'a [u8]),
    /// So many bytes of a file from an offset.
    File(&'a std::fs::File, u64, u64),
}

impl Body<'_> {
    fn len(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::File(_, _, len) => *len,
        }
    }
}

/// What a HEAD found of a key: when it was last written, and its size.
pub struct Found {
    pub modified: Option<std::time::SystemTime>,
    pub size: u64,
}

/// An attribute as BuildKit's `getConfig` reads it: there, even empty, or not.
fn attr<'a>(e: &'a CacheEntry, k: &str) -> Option<&'a str> {
    e.attrs.get(k).map(String::as_str)
}

impl Bucket {
    /// The bucket of `e`'s attributes, as BuildKit's `getConfig` reads them, and the
    /// credentials it is signed for.
    pub fn of(e: &CacheEntry, env: &dyn Fn(&str) -> Option<String>) -> Result<Bucket, String> {
        let bucket = match attr(e, "bucket") {
            Some(b) => b.to_string(),
            None => env("AWS_BUCKET").ok_or("bucket ($AWS_BUCKET) not set for s3 cache")?,
        };
        let region = match attr(e, "region") {
            Some(r) => r.to_string(),
            None => env("AWS_REGION").ok_or("region ($AWS_REGION) not set for s3 cache")?,
        };
        let names = match attr(e, "name") {
            Some(n) => n.split(';').map(str::to_string).collect(),
            None => vec!["buildkit".to_string()],
        };
        // An unreadable duration or flag is the default, as BuildKit's are.
        let touch_refresh = attr(e, "touch_refresh")
            .and_then(shards_cmdline::gotime::parse_duration)
            .map_or(24 * 3600 * 1_000_000_000, i128::from);
        let path_style = attr(e, "use_path_style")
            .and_then(|b| shards_cmdline::go::parse_bool(b).ok())
            .unwrap_or(false);
        let parallelism = match attr(e, "upload_parallelism") {
            Some(n) => shards_cmdline::go::parse_int10(n)
                .ok()
                .filter(|&n| n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("upload_parallelism must be a positive integer")?,
            None => 4,
        };
        // The SDK's own endpoint settings, unless the cache names one; its path style
        // only with the cache's (BuildKit's `newS3Client`).
        let (endpoint, path_style) = match attr(e, "endpoint_url").filter(|u| !u.is_empty()) {
            Some(u) => (Some(u.to_string()), path_style),
            None => (
                env("AWS_ENDPOINT_URL_S3")
                    .or_else(|| env("AWS_ENDPOINT_URL"))
                    .filter(|u| !u.is_empty()),
                false,
            ),
        };
        let endpoint = endpoint
            .map(|u| Url::parse(&u).map_err(|err| format!("endpoint_url: {err}")))
            .transpose()?;
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let http = Client::new(
            Box::new(move |_| Ok(config.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .with_proxies(shards_registry::proxy::Proxies::from_env(env));
        Ok(Bucket {
            http,
            bucket,
            region,
            prefix: attr(e, "prefix").unwrap_or_default().to_string(),
            manifests_prefix: attr(e, "manifests_prefix").unwrap_or("manifests/").to_string(),
            blobs_prefix: attr(e, "blobs_prefix").unwrap_or("blobs/").to_string(),
            names,
            touch_refresh,
            endpoint,
            path_style,
            parallelism,
            creds: credentials(e, env)?,
            part: MOST_AT_ONCE,
        })
    }

    pub fn manifest_key(&self, name: &str) -> String {
        format!("{}{}{name}", self.prefix, self.manifests_prefix)
    }

    pub fn blob_key(&self, digest: &str) -> String {
        format!("{}{}{digest}", self.prefix, self.blobs_prefix)
    }

    /// Whether a layer last written at `modified` is to be touched.
    pub fn stale(&self, modified: Option<std::time::SystemTime>) -> bool {
        let age = modified
            .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
            .map_or(0, |d| i128::try_from(d.as_nanos()).unwrap_or(i128::MAX));
        age > self.touch_refresh
    }

    /// `key`'s URL, with `query`: virtual-hosted (`BUCKET.s3.REGION.amazonaws.com`, or the
    /// bucket before the endpoint's host) where the bucket can be a host's label, else
    /// with the bucket first in the path, as the SDK's S3 endpoint rules have it. The key
    /// is escaped as the SDK escapes it (`EscapePath`, `/` kept).
    fn url(&self, key: &str, query: &str) -> Result<Url, String> {
        let hostable = (3..=63).contains(&self.bucket.len())
            && self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !self.bucket.starts_with('-')
            && !self.bucket.ends_with('-');
        let key = escape_path(key);
        let query = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        let text = match &self.endpoint {
            None => {
                let suffix = if self.region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                if hostable {
                    format!("https://{}.s3.{}.{suffix}/{key}{query}", self.bucket, self.region)
                } else {
                    format!("https://s3.{}.{suffix}/{}/{key}{query}", self.region, self.bucket)
                }
            }
            Some(ep) => {
                let scheme = match ep.scheme() {
                    shards_registry::url::Scheme::Http => "http",
                    shards_registry::url::Scheme::Https => "https",
                };
                let base = ep
                    .target()
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('/');
                if hostable && !self.path_style {
                    format!("{scheme}://{}.{}{base}/{key}{query}", self.bucket, ep.authority())
                } else {
                    format!("{scheme}://{}{base}/{}/{key}{query}", ep.authority(), self.bucket)
                }
            }
        };
        Url::parse(&text).map_err(|e| e.to_string())
    }

    /// Sends S3's operation `op` (its method by its name) to `key`, signed; `op` names it in
    /// errors, as the SDK names it.
    fn send(
        &self,
        op: &str,
        key: &str,
        query: &str,
        headers: &[(&str, &str)],
        body: Body<'_>,
        payload_hash: &str,
    ) -> Result<Response<'_>, String> {
        let method = match op {
            "GetObject" => "GET",
            "HeadObject" => "HEAD",
            "CreateMultipartUpload" | "CompleteMultipartUpload" => "POST",
            "AbortMultipartUpload" => "DELETE",
            _ => "PUT",
        };
        let url = self.url(key, query)?;
        let authority = url.authority();
        let target = url.target();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let when = amz_date(std::time::SystemTime::now());
        let added = sign(
            &Request {
                method,
                authority: &authority,
                path,
                query,
                headers,
                length: body.len(),
                payload_hash,
            },
            &self.creds,
            &self.region,
            &when,
        );
        let mut all: Vec<(&str, &str)> = headers.to_vec();
        all.extend(added.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let (bytes, file) = match body {
            Body::Bytes(b) => (b, None),
            Body::File(f, from, len) => (&[][..], Some((f, from, len))),
        };
        self.http
            .send(&shards_registry::http::Request {
                method,
                url: &url,
                headers: &all,
                body: bytes,
                file,
            })
            .map_err(|e| format!("operation error S3: {op}, {e}"))
    }

    /// `key`'s object, or `None` where there is none.
    pub fn get(&self, key: &str) -> Result<Option<Response<'_>>, String> {
        let r = self.send("GetObject", key, "", &[], Body::Bytes(&[]), EMPTY_SHA256)?;
        match r.status {
            200 => Ok(Some(r)),
            404 => Ok(None),
            _ => Err(failed("GetObject", r)),
        }
    }

    /// What there is at `key`, or `None`.
    pub fn head(&self, key: &str) -> Result<Option<Found>, String> {
        let r = self.send("HeadObject", key, "", &[], Body::Bytes(&[]), EMPTY_SHA256)?;
        match r.status {
            200 => Ok(Some(Found {
                modified: r.header("last-modified").and_then(|d| {
                    time::OffsetDateTime::parse(d.trim(), &time::format_description::well_known::Rfc2822)
                        .ok()
                        .map(std::time::SystemTime::from)
                }),
                size: r
                    .header("content-length")
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0),
            })),
            404 => Ok(None),
            _ => Err(failed("HeadObject", r)),
        }
    }

    /// Writes `body`, whose SHA-256 is `sha256` (hex), at `key`: in one PutObject, or in
    /// parts past [`MOST_AT_ONCE`], each that size but the last.
    pub fn put(&self, key: &str, body: Body<'_>, sha256: &str) -> Result<(), String> {
        if let Body::File(file, at, len) = body
            && len > self.part
        {
            return self.in_parts(key, len, &|n, id, from, size| {
                let query = format!("partNumber={n}&uploadId={}", uri_encode(id));
                let r = self.send(
                    "UploadPart",
                    key,
                    &query,
                    &[],
                    Body::File(file, at + from, size),
                    UNSIGNED,
                )?;
                let etag = r.header("etag").map(str::to_string);
                ok("UploadPart", r)?;
                etag.ok_or_else(|| "operation error S3: UploadPart, no ETag".to_string())
            });
        }
        let r = self.send("PutObject", key, "", &[], body, sha256)?;
        ok("PutObject", r).map(drop)
    }

    /// Copies `key` onto itself, its metadata replaced, so that a bucket's lifecycle rules
    /// count its age from now, as BuildKit's `touch` does: past [`MOST_AT_ONCE`], in parts.
    pub fn touch(&self, key: &str, size: u64) -> Result<(), String> {
        let source = escape_path(&format!("{}/{key}", self.bucket));
        if size < self.part {
            let now = amz_date(std::time::SystemTime::now());
            let r = self.send(
                "CopyObject",
                key,
                "",
                &[
                    ("x-amz-copy-source", &source),
                    ("x-amz-metadata-directive", "REPLACE"),
                    ("x-amz-meta-updated-at", &now),
                ],
                Body::Bytes(&[]),
                EMPTY_SHA256,
            )?;
            return ok("CopyObject", r).map(drop);
        }
        self.in_parts(key, size, &|n, id, from, len| {
            let query = format!("partNumber={n}&uploadId={}", uri_encode(id));
            let range = format!("bytes={from}-{}", from + len - 1);
            let r = self.send(
                "UploadPartCopy",
                key,
                &query,
                &[
                    ("x-amz-copy-source", &source),
                    ("x-amz-copy-source-range", &range),
                ],
                Body::Bytes(&[]),
                EMPTY_SHA256,
            )?;
            let body = ok("UploadPartCopy", r)?;
            xml_text(&body, "ETag").ok_or_else(|| "operation error S3: UploadPartCopy, no ETag".to_string())
        })
    }

    /// A multipart upload of `size` bytes at `key`, `part(number, upload, from, len)`
    /// writing each and giving its ETag; aborted if any fails.
    fn in_parts(&self, key: &str, size: u64, part: &Part<'_>) -> Result<(), String> {
        let r = self.send(
            "CreateMultipartUpload",
            key,
            "uploads=",
            &[],
            Body::Bytes(&[]),
            EMPTY_SHA256,
        )?;
        let body = ok("CreateMultipartUpload", r)?;
        let id =
            xml_text(&body, "UploadId").ok_or("operation error S3: CreateMultipartUpload, no UploadId")?;
        let upload = format!("uploadId={}", uri_encode(&id));
        let done = (|| -> Result<(), String> {
            // As the SDK writes it: in S3's namespace, which a server may require.
            let mut parts =
                String::from(r#"<CompleteMultipartUpload xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
            let (mut from, mut n) = (0u64, 1u64);
            while from < size {
                let len = (size - from).min(self.part);
                let etag = part(n, &id, from, len)?;
                parts.push_str(&format!(
                    "<Part><ETag>{}</ETag><PartNumber>{n}</PartNumber></Part>",
                    xml_escape(&etag)
                ));
                from += len;
                n += 1;
            }
            parts.push_str("</CompleteMultipartUpload>");
            let hash = super::sha256(parts.as_bytes());
            let r = self.send(
                "CompleteMultipartUpload",
                key,
                &upload,
                &[("content-type", "application/xml")],
                Body::Bytes(parts.as_bytes()),
                hash.hex(),
            )?;
            ok("CompleteMultipartUpload", r).map(drop)
        })();
        if done.is_err() {
            // What it left is S3's to drop; a failure here adds nothing to the first.
            let _ = self
                .send(
                    "AbortMultipartUpload",
                    key,
                    &upload,
                    &[],
                    Body::Bytes(&[]),
                    EMPTY_SHA256,
                )
                .map(|r| ok("AbortMultipartUpload", r));
        }
        done
    }
}

/// A response's body if it succeeded, else the error it carries. S3 can answer a copy or
/// a completed upload with 200 and an error in the body: that is an error too.
fn ok(op: &str, mut r: Response<'_>) -> Result<String, String> {
    if !(200..300).contains(&r.status) {
        return Err(failed(op, r));
    }
    let mut body = Vec::new();
    (&mut r)
        .take(MOST_OF_AN_ERROR)
        .read_to_end(&mut body)
        .map_err(|e| format!("operation error S3: {op}, {e}"))?;
    let body = String::from_utf8_lossy(&body).into_owned();
    if let Some(code) = xml_text(&body, "Code").filter(|_| body.contains("<Error>")) {
        let message = xml_text(&body, "Message").unwrap_or_default();
        return Err(format!(
            "operation error S3: {op}, https response error StatusCode: {}, api error {code}: {message}",
            r.status
        ));
    }
    Ok(body)
}

/// The error of a response that failed, as the SDK words it: the status, S3's request
/// IDs, and its code and message.
fn failed(op: &str, mut r: Response<'_>) -> String {
    let mut body = Vec::new();
    let _ = (&mut r).take(MOST_OF_AN_ERROR).read_to_end(&mut body);
    let body = String::from_utf8_lossy(&body);
    let mut text = format!(
        "operation error S3: {op}, https response error StatusCode: {}, RequestID: {}, HostID: {}",
        r.status,
        r.header("x-amz-request-id").unwrap_or_default(),
        r.header("x-amz-id-2").unwrap_or_default()
    );
    match xml_text(&body, "Code") {
        Some(code) => text.push_str(&format!(
            ", api error {code}: {}",
            xml_text(&body, "Message").unwrap_or_default()
        )),
        None => text.push_str(&format!(", api error {}", r.status_text())),
    }
    text
}

/// The text of the first `<tag>` in `xml`, its entities read.
pub(super) fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let rest = xml.get(start..)?;
    let end = rest.find(&format!("</{tag}>"))?;
    Some(
        rest.get(..end)?
            .replace("&quot;", "\"")
            .replace("&#34;", "\"")
            .replace("&apos;", "'")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A key as the SDK puts it in a path (`httpbinding.EscapePath`, `/` kept).
fn escape_path(s: &str) -> String {
    s.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

/// `when` as SigV4 dates requests: `YYYYMMDDTHHMMSSZ`, UTC.
fn amz_date(when: std::time::SystemTime) -> String {
    let t = time::OffsetDateTime::from(when);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// The credentials a cache is signed with, as buildx and the SDK's default chain find
/// them: the cache's `access_key_id` and `secret_access_key` (with `session_token`);
/// else `$AWS_ACCESS_KEY_ID` and `$AWS_SECRET_ACCESS_KEY` (`$AWS_SESSION_TOKEN`); else
/// `$AWS_PROFILE`'s (`default`) keys in the shared credentials file, then the config
/// file (`$AWS_SHARED_CREDENTIALS_FILE`, `$AWS_CONFIG_FILE`, in `~/.aws`).
fn credentials(e: &CacheEntry, env: &dyn Fn(&str) -> Option<String>) -> Result<Credentials, String> {
    let given = |k: &str| attr(e, k).filter(|v| !v.is_empty()).map(str::to_string);
    let var = |k: &str| env(k).filter(|v| !v.is_empty());
    let pair = |id: Option<String>, secret: Option<String>, token: Option<String>| {
        Some(Credentials {
            access_key: id?,
            secret: Zeroizing::new(secret?),
            token: token.map(Zeroizing::new),
        })
    };
    if let Some(c) = pair(
        given("access_key_id"),
        given("secret_access_key"),
        given("session_token"),
    ) {
        return Ok(c);
    }
    if let Some(c) = pair(
        var("AWS_ACCESS_KEY_ID").or_else(|| var("AWS_ACCESS_KEY")),
        var("AWS_SECRET_ACCESS_KEY").or_else(|| var("AWS_SECRET_KEY")),
        var("AWS_SESSION_TOKEN"),
    ) {
        return Ok(c);
    }
    let profile = var("AWS_PROFILE").unwrap_or_else(|| "default".into());
    let home = var("HOME").or_else(|| var("USERPROFILE")).unwrap_or_default();
    let in_home = |name: &str| std::path::Path::new(&home).join(".aws").join(name);
    let mut keys = std::collections::BTreeMap::new();
    let config = var("AWS_CONFIG_FILE").map_or_else(|| in_home("config"), std::path::PathBuf::from);
    let shared =
        var("AWS_SHARED_CREDENTIALS_FILE").map_or_else(|| in_home("credentials"), std::path::PathBuf::from);
    // The config file names a profile `profile NAME` (`default` either way); the
    // credentials file's keys win.
    for (path, section) in [
        (&config, format!("profile {profile}")),
        (&shared, profile.clone()),
    ] {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                for (k, v) in ini_section(&text, &section, path == &config) {
                    keys.insert(k, Zeroizing::new(v));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
    }
    let key = |k: &str| keys.get(k).map(|v| v.to_string());
    if let Some(c) = pair(
        key("aws_access_key_id"),
        key("aws_secret_access_key"),
        key("aws_session_token"),
    ) {
        return Ok(c);
    }
    if let Some(how) = [
        "role_arn",
        "credential_process",
        "sso_session",
        "sso_start_url",
        "web_identity_token_file",
    ]
    .into_iter()
    .find(|k| keys.contains_key(*k))
    {
        return Err(format!(
            "the AWS profile {profile} gets its credentials by {how}, which shards does not do: \
             give the s3 cache access_key_id and secret_access_key"
        ));
    }
    Err(
        "no AWS credentials for the s3 cache: give it access_key_id and secret_access_key, \
         set $AWS_ACCESS_KEY_ID and $AWS_SECRET_ACCESS_KEY, or keep them in a profile of \
         ~/.aws/credentials"
            .into(),
    )
}

/// The keys of `section` in an INI file (the SDK's `ini` package: `#` and `;` comments,
/// `[name]` sections, `key = value`, keys lowercased). In a config file, `[profile
/// default]` and `[default]` are one.
fn ini_section(text: &str, section: &str, config: bool) -> Vec<(String, String)> {
    let named = |name: &str| -> String {
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if config && name == "profile default" {
            "default".into()
        } else {
            name
        }
    };
    let want = named(section);
    let mut inside = false;
    let mut keys = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = named(name) == want;
        } else if let Some((k, v)) = line.split_once('=').filter(|_| inside) {
            keys.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case of testdata/sigv4.json (`scripts/s3/generate`): the headers the signer
    /// added, those it signs and the signature, as aws-sdk-go-v2's v4 signer set them.
    #[test]
    fn sigv4_is_aws_sdk_go_v2s() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("testdata/sigv4.json")).unwrap();
        assert_eq!(cases.len(), 7);
        for c in cases {
            let url = c["url"].as_str().unwrap();
            let rest = url.split_once("://").unwrap().1;
            let (authority, target) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let given: Vec<(String, String)> = c["headers"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|h| {
                            (
                                h["name"].as_str().unwrap().to_string(),
                                h["value"].as_str().unwrap().to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let headers: Vec<(&str, &str)> = given.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let token = c["token"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
            let creds = Credentials {
                access_key: "AKIDEXAMPLE".into(),
                secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
                token: token.map(Zeroizing::new),
            };
            let added = sign(
                &Request {
                    method: c["method"].as_str().unwrap(),
                    authority,
                    path,
                    query,
                    headers: &headers,
                    length: c["length"].as_u64().unwrap(),
                    payload_hash: c["payload_hash"].as_str().unwrap(),
                },
                &creds,
                c["region"].as_str().unwrap(),
                "20261008T123456Z",
            );
            for want in c["signed"].as_array().unwrap() {
                let name = want["name"].as_str().unwrap();
                if given.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
                    continue;
                }
                let got = added.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
                assert_eq!(got, want["value"].as_str(), "{} {url}: {name}", c["method"]);
            }
        }
    }

    /// Every request the backend makes, held to a real S3 (`docs/research/platform-
    /// measurements.md` M-S3): `SHARDS_TEST_S3` names its endpoint, path-style, its
    /// bucket `cache`, signed for `SHARDS_TEST_S3_KEY` and `SHARDS_TEST_S3_SECRET`, in
    /// `us-east-1`. Parts are 5 MiB, S3's least, so a 12 MiB object is written and
    /// touched in three, as a layer past 5 GiB is.
    #[test]
    fn s3_requests_are_a_real_s3s() {
        let Ok(endpoint) = std::env::var("SHARDS_TEST_S3") else {
            println!("SKIP: SHARDS_TEST_S3 names no S3");
            return;
        };
        let attrs = [
            ("bucket", "cache".to_string()),
            ("region", "us-east-1".into()),
            ("endpoint_url", endpoint),
            ("use_path_style", "true".into()),
            ("access_key_id", std::env::var("SHARDS_TEST_S3_KEY").unwrap()),
            (
                "secret_access_key",
                std::env::var("SHARDS_TEST_S3_SECRET").unwrap(),
            ),
            ("prefix", "probe dir/".into()),
            ("touch_refresh", "0s".into()),
        ];
        let e = CacheEntry {
            kind: "s3".into(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        };
        let mut b = Bucket::of(&e, &|_| None).unwrap();
        b.part = 5 << 20;
        let read = |key: &str| {
            let mut out = Vec::new();
            b.get(key).unwrap().unwrap().read_to_end(&mut out).unwrap();
            out
        };
        // One PutObject, read back; a key with `:` and a space in it.
        let small = b"the cache's records".to_vec();
        let key = b.blob_key("sha256:small");
        b.put(&key, Body::Bytes(&small), super::super::sha256(&small).hex())
            .unwrap();
        assert_eq!(read(&key), small);
        let found = b.head(&key).unwrap().unwrap();
        assert_eq!(found.size, small.len() as u64);
        assert!(found.modified.is_some());
        assert!(b.head(&b.blob_key("sha256:absent")).unwrap().is_none());
        assert!(b.get(&b.blob_key("sha256:absent")).unwrap().is_none());
        // A body signed wrong is refused.
        assert!(
            b.put(&key, Body::Bytes(b"other"), super::super::sha256(&small).hex())
                .is_err()
        );
        // Touched: copied onto itself, the same.
        b.touch(&key, found.size).unwrap();
        assert_eq!(read(&key), small);
        // A file, from an offset, in parts.
        let big: Vec<u8> = (0..12u32 << 20).map(|i| (i % 251) as u8).collect();
        let path_dir = shards_testdir::TempDir::new("s3-probe").unwrap();
        let path = path_dir.join("s3-probe");
        std::fs::write(&path, [b"skip".as_slice(), &big].concat()).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let key = b.blob_key("sha256:big");
        b.put(&key, Body::File(&file, 4, big.len() as u64), UNSIGNED)
            .unwrap();
        assert!(read(&key) == big, "the parts are the file");
        b.put(&b.blob_key("sha256:whole"), Body::File(&file, 0, 4), UNSIGNED)
            .unwrap();
        assert_eq!(read(&b.blob_key("sha256:whole")), b"skip");
        drop(file);
        let _ = std::fs::remove_file(&path);
        // Touched in parts.
        b.touch(&key, big.len() as u64).unwrap();
        assert!(read(&key) == big, "the copied parts are the object");
        // The manifests prefix, under the prefix.
        let m = b.manifest_key("buildkit");
        assert_eq!(m, "probe dir/manifests/buildkit");
        b.put(&m, Body::Bytes(b"{}"), super::super::sha256(b"{}").hex())
            .unwrap();
        assert_eq!(read(&m), b"{}");
    }
}
