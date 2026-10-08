//! The Azure Blob Storage cache backend (D89): `--cache-to` and `--cache-from`
//! `type=azblob`, as BuildKit's cache/remotecache/azblob (v0.28.1) reads its attributes
//! and lays out its container: each layer at `prefix`/`blobs_prefix`/DIGEST, the cache's
//! records at `prefix`/`manifests_prefix`/NAME for each of `name`'s names, every one
//! read back. The container is made if it is not there.
//!
//! Requests are those azure-sdk-for-go's azblob v1.5.0 (BuildKit's) sends, signed with
//! the account's shared key as its `SharedKeyCredential` signs them, held to that client's
//! own requests by `requests_are_azblobs` (`scripts/azblob/generate`).

use std::io::Read as _;

use aws_lc_rs::hmac;
use base64::Engine as _;
use shards_cmdline::buildflags::CacheEntry;
use shards_registry::http::{Client, Response};
use shards_registry::url::Url;
use zeroize::Zeroizing;

/// The service version the SDK asks for.
const VERSION: &str = "2024-11-04";
/// What one request uploads of a layer, as BuildKit's `UploadStream` cuts it
/// (`IOChunkSize`): a smaller layer goes in one.
const BLOCK: u64 = 32 << 20;
/// The most of an error's body read for its code and message.
const MOST_OF_AN_ERROR: u64 = 64 << 10;

/// A container's cache: where it is, the key that signs for it, and BuildKit's
/// attributes.
pub struct Container {
    http: Client,
    /// The account's URL, without a trailing `/`.
    account_url: String,
    account: String,
    container: String,
    prefix: String,
    manifests_prefix: String,
    blobs_prefix: String,
    /// `name`'s names, `;` apart: each written and read.
    pub names: Vec<String>,
    key: Zeroizing<Vec<u8>>,
    /// The most uploaded at once: [`BLOCK`], less in tests.
    block: u64,
}

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

/// An attribute, else an environment variable: BuildKit reads either, the attribute
/// first even when empty.
fn setting(e: &CacheEntry, attr: &str, var: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    e.attrs.get(attr).cloned().or_else(|| env(var))
}

impl Container {
    /// The container of `e`'s attributes, as BuildKit's `getConfig` reads them.
    pub fn of(e: &CacheEntry, env: &dyn Fn(&str) -> Option<String>) -> Result<Container, String> {
        let account_url = setting(e, "account_url", "BUILDKIT_AZURE_STORAGE_ACCOUNT_URL", env).ok_or(
            "either ${BUILDKIT_AZURE_STORAGE_ACCOUNT_URL} or account_url attribute is required for azblob cache",
        )?;
        let url = Url::parse(&account_url)
            .map_err(|err| format!("azure storage account url provided is not a valid url: {err}"))?;
        let account = setting(e, "account_name", "BUILDKIT_AZURE_STORAGE_ACCOUNT_NAME", env)
            .unwrap_or_else(|| url.host().split('.').next().unwrap_or_default().to_string());
        if account.is_empty() {
            return Err("unable to retrieve account name from account url or ${BUILDKIT_AZURE_STORAGE_ACCOUNT_NAME} or account_name attribute for azblob cache".into());
        }
        let secret = e.attrs.get("secret_access_key").filter(|k| !k.is_empty()).ok_or(
            "the azblob cache needs the account's key (secret_access_key): shards does not sign \
             with Azure AD identities",
        )?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(secret.as_bytes())
            .map_err(|err| format!("failed to create shared key: decode account key: {err}"))?;
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        let http = Client::new(
            Box::new(move |_| Ok(config.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .with_proxies(shards_registry::proxy::Proxies::from_env(env));
        Ok(Container {
            http,
            account_url: account_url.trim_end_matches('/').to_string(),
            account,
            container: setting(e, "container", "BUILDKIT_AZURE_STORAGE_CONTAINER", env)
                .unwrap_or_else(|| "buildkit-cache".into()),
            prefix: setting(e, "prefix", "BUILDKIT_AZURE_STORAGE_PREFIX", env).unwrap_or_default(),
            manifests_prefix: e
                .attrs
                .get("manifests_prefix")
                .cloned()
                .unwrap_or_else(|| "manifests".into()),
            blobs_prefix: e
                .attrs
                .get("blobs_prefix")
                .cloned()
                .unwrap_or_else(|| "blobs".into()),
            names: match e.attrs.get("name") {
                Some(n) => n.split(';').map(str::to_string).collect(),
                None => vec!["buildkit".into()],
            },
            key: Zeroizing::new(key),
            block: BLOCK,
        })
    }

    pub fn manifest_key(&self, name: &str) -> String {
        join(&[&self.prefix, &self.manifests_prefix, name])
    }

    pub fn blob_key(&self, digest: &str) -> String {
        join(&[&self.prefix, &self.blobs_prefix, digest])
    }

    /// Sends `method` to `target` (under the account: the container, a blob), signed, with
    /// `headers`; `op` names it in errors.
    fn send(
        &self,
        op: &str,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: Body<'_>,
    ) -> Result<Response<'_>, String> {
        let url = Url::parse(&format!("{}/{target}", self.account_url)).map_err(|e| e.to_string())?;
        let date = http_date(std::time::SystemTime::now());
        let mut all: Vec<(&str, &str)> = vec![("accept", "application/xml")];
        all.extend_from_slice(headers);
        all.push(("x-ms-date", &date));
        all.push(("x-ms-version", VERSION));
        let target = url.target();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let authorization = sign(method, path, query, &all, body.len(), &self.account, &self.key);
        all.push(("authorization", &authorization));
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
            .map_err(|e| format!("{op}: {e}"))
    }

    fn blob_target(&self, key: &str) -> String {
        format!("{}/{}", self.container, path_escape(key))
    }

    /// The container, made if it is not there, as BuildKit's `createContainerClient`
    /// makes it; one made meanwhile by another is the one.
    pub fn ensure(&self) -> Result<(), String> {
        let target = format!("{}?restype=container", self.container);
        let r = self.send("GetProperties", "GET", &target, &[], Body::Bytes(&[]))?;
        match (r.status, r.header("x-ms-error-code")) {
            (200, _) => return Ok(()),
            (404, Some("ContainerNotFound")) => {}
            _ => {
                return Err(format!(
                    "failed to get properties of cache container {}: {}",
                    self.container,
                    failed("GetProperties", r)
                ));
            }
        }
        let r = self.send("Create", "PUT", &target, &[], Body::Bytes(&[]))?;
        match (r.status, r.header("x-ms-error-code")) {
            (201, _) | (409, Some("ContainerAlreadyExists")) => Ok(()),
            _ => Err(format!(
                "failed to create cache container {}: {}",
                self.container,
                failed("Create", r)
            )),
        }
    }

    /// Whether there is a blob at `key`.
    pub fn exists(&self, key: &str) -> Result<bool, String> {
        let r = self.send(
            "GetProperties",
            "HEAD",
            &self.blob_target(key),
            &[],
            Body::Bytes(&[]),
        )?;
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(format!(
                "failed to check blob {key} existence: {}",
                failed("GetProperties", r)
            )),
        }
    }

    /// The blob at `key`, or `None`.
    pub fn get(&self, key: &str) -> Result<Option<Response<'_>>, String> {
        let r = self.send(
            "DownloadStream",
            "GET",
            &self.blob_target(key),
            &[],
            Body::Bytes(&[]),
        )?;
        match r.status {
            200 => Ok(Some(r)),
            404 => Ok(None),
            _ => Err(failed("DownloadStream", r)),
        }
    }

    /// Writes `body` at `key`, in place of what was there: a manifest, as BuildKit's
    /// `uploadManifest` writes it (last writer wins).
    pub fn put(&self, key: &str, body: Body<'_>) -> Result<(), String> {
        let r = self.send(
            "Upload",
            "PUT",
            &self.blob_target(key),
            &[
                ("content-type", "application/octet-stream"),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            body,
        )?;
        ok("Upload", r)
    }

    /// Writes `body` at `key` unless a blob is there, as BuildKit's
    /// `uploadBlobIfNotExists` does: in one request below [`BLOCK`], else in blocks of it,
    /// committed together, either only where there is none.
    pub fn put_new(&self, key: &str, file: &std::fs::File, len: u64) -> Result<(), String> {
        let target = self.blob_target(key);
        let there =
            |r: &Response<'_>| r.status == 409 && r.header("x-ms-error-code") == Some("BlobAlreadyExists");
        if len < self.block {
            let r = self.send(
                "UploadStream",
                "PUT",
                &target,
                &[
                    ("content-type", "application/octet-stream"),
                    ("if-none-match", "*"),
                    ("x-ms-blob-type", "BlockBlob"),
                ],
                Body::File(file, 0, len),
            )?;
            return if there(&r) { Ok(()) } else { ok("UploadStream", r) };
        }
        let mut list = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        list.push_str("\n<BlockList>");
        let (mut from, mut n) = (0u64, 0u64);
        while from < len {
            let size = (len - from).min(self.block);
            // Every ID of a blob the same length (Blob Storage's rule).
            let id = base64::engine::general_purpose::STANDARD.encode(format!("shards-{n:010}"));
            let r = self.send(
                "StageBlock",
                "PUT",
                &format!("{target}?blockid={}&comp=block", query_escape(&id)),
                &[("content-type", "application/octet-stream")],
                Body::File(file, from, size),
            )?;
            ok("StageBlock", r)?;
            list.push_str(&format!("<Latest>{id}</Latest>"));
            from += size;
            n += 1;
        }
        list.push_str("</BlockList>");
        let r = self.send(
            "CommitBlockList",
            "PUT",
            &format!("{target}?comp=blocklist"),
            &[("content-type", "application/xml"), ("if-none-match", "*")],
            Body::Bytes(list.as_bytes()),
        )?;
        if there(&r) {
            Ok(())
        } else {
            ok("CommitBlockList", r)
        }
    }
}

fn ok(op: &str, r: Response<'_>) -> Result<(), String> {
    if (200..300).contains(&r.status) {
        Ok(())
    } else {
        Err(failed(op, r))
    }
}

/// A failed response's error: the operation, the status, and Blob Storage's code and
/// message.
fn failed(op: &str, mut r: Response<'_>) -> String {
    let mut body = Vec::new();
    let _ = (&mut r).take(MOST_OF_AN_ERROR).read_to_end(&mut body);
    let body = String::from_utf8_lossy(&body);
    let code = r
        .header("x-ms-error-code")
        .map(str::to_string)
        .or_else(|| super::s3::xml_text(&body, "Code"))
        .unwrap_or_default();
    let message = super::s3::xml_text(&body, "Message")
        .map(|m| m.lines().next().unwrap_or_default().to_string())
        .unwrap_or_default();
    format!("{op}: {}: {code}: {message}", r.status_text())
}

/// Go's `filepath.Join` on Linux, where BuildKit runs: the parts not empty, `/` between,
/// cleaned.
fn join(parts: &[&str]) -> String {
    let kept: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if kept.is_empty() {
        return String::new();
    }
    shards_cmdline::mounts::clean(&kept.join("/"))
}

/// Go's `url.PathEscape`, as the SDK escapes a blob's name: `/` among what it escapes.
fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~$&+,;=:@".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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

/// A query's pairs, decoded as Go's `url.ParseQuery` decodes them.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    let decode = |s: &str| -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            let hex = |c: u8| char::from(c).to_digit(16);
            match (
                c,
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
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

/// The `Authorization` of a request, as azblob's `SharedKeyCredential` signs it
/// (`buildStringToSign`): its method, standard headers and `x-ms-*` headers in the SDK's
/// order, and its resource, the account and the escaped path and its query, every
/// parameter's values sorted and joined.
fn sign(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    length: u64,
    account: &str,
    key: &[u8],
) -> String {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| *v)
    };
    let length = if length == 0 {
        String::new()
    } else {
        length.to_string()
    };
    let mut ms: Vec<(String, Vec<&str>)> = Vec::new();
    for (k, v) in headers {
        let name = k.trim().to_ascii_lowercase();
        if name.starts_with("x-ms-") {
            match ms.iter_mut().find(|(n, _)| *n == name) {
                Some((_, vs)) => vs.push(v),
                None => ms.push((name, vec![v])),
            }
        }
    }
    ms.sort_by(|(a, _), (b, _)| compare_headers(a, b));
    let canonical_headers: Vec<String> = ms.iter().map(|(k, vs)| format!("{k}:{}", vs.join(","))).collect();
    let mut resource = format!("/{account}{}", if path.is_empty() { "/" } else { path });
    let mut params: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in query_pairs(query) {
        match params.iter_mut().find(|(n, _)| *n == k) {
            Some((_, vs)) => vs.push(v),
            None => params.push((k, vec![v])),
        }
    }
    params.sort();
    for (k, mut vs) in params {
        vs.sort();
        resource.push_str(&format!("\n{}:{}", k.to_ascii_lowercase(), vs.join(",")));
    }
    let to_sign = [
        method,
        get("content-encoding"),
        get("content-language"),
        &length,
        get("content-md5"),
        get("content-type"),
        "",
        get("if-modified-since"),
        get("if-match"),
        get("if-none-match"),
        get("if-unmodified-since"),
        get("range"),
        &canonical_headers.join("\n"),
        &resource,
    ]
    .join("\n");
    let mac = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), to_sign.as_bytes());
    format!(
        "SharedKey {account}:{}",
        base64::engine::general_purpose::STANDARD.encode(mac.as_ref())
    )
}

/// The SDK's order of `x-ms-*` names (`compareHeaders`): by weight tables that pass over
/// `-` first, then break ties by where it stands.
fn compare_headers(lhs: &str, rhs: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    const LV0: [u16; 128] = [
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x71c, 0x0, 0x71f, 0x721, 0x723,
        0x725, 0x0, 0x0, 0x0, 0x72d, 0x803, 0x0, 0x0, 0x733, 0x0, 0xd03, 0xd1a, 0xd1c, 0xd1e, 0xd20, 0xd22,
        0xd24, 0xd26, 0xd28, 0xd2a, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0xe02, 0xe09, 0xe0a, 0xe1a, 0xe21,
        0xe23, 0xe25, 0xe2c, 0xe32, 0xe35, 0xe36, 0xe48, 0xe51, 0xe70, 0xe7c, 0xe7e, 0xe89, 0xe8a, 0xe91,
        0xe99, 0xe9f, 0xea2, 0xea4, 0xea6, 0xea7, 0xea9, 0x0, 0x0, 0x0, 0x743, 0x744, 0x748, 0xe02, 0xe09,
        0xe0a, 0xe1a, 0xe21, 0xe23, 0xe25, 0xe2c, 0xe32, 0xe35, 0xe36, 0xe48, 0xe51, 0xe70, 0xe7c, 0xe7e,
        0xe89, 0xe8a, 0xe91, 0xe99, 0xe9f, 0xea2, 0xea4, 0xea6, 0xea7, 0xea9, 0x0, 0x74c, 0x0, 0x750, 0x0,
    ];
    const fn lv2() -> [u16; 128] {
        let mut t = [0u16; 128];
        t[39] = 0x8012;
        t[45] = 0x8212;
        t
    }
    const LV2: [u16; 128] = lv2();
    let tables: [&[u16; 128]; 2] = [&LV0, &LV2];
    let (lhs, rhs) = (lhs.as_bytes(), rhs.as_bytes());
    let (mut level, mut i, mut j) = (0usize, 0usize, 0usize);
    let weight = |t: &[u16; 128], s: &[u8], at: usize| match s.get(at) {
        Some(&c) => t.get(usize::from(c)).copied().unwrap_or(0),
        None => 1,
    };
    while let Some(t) = tables.get(level) {
        if level == tables.len() - 1 && i != j {
            return j.cmp(&i);
        }
        let (w1, w2) = (weight(t, lhs, i), weight(t, rhs, j));
        if w1 == 1 && w2 == 1 {
            i = 0;
            j = 0;
            level += 1;
        } else if w1 == w2 {
            i += 1;
            j += 1;
        } else if w1 == 0 {
            i += 1;
        } else if w2 == 0 {
            j += 1;
        } else {
            return w1.cmp(&w2);
        }
    }
    Ordering::Equal
}

/// `at` as HTTP dates it (IMF-fixdate, RFC 9110 §5.6.7), as `x-ms-date` carries it.
fn http_date(at: std::time::SystemTime) -> String {
    let t = time::OffsetDateTime::from(at);
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS.get(usize::from(t.weekday().number_days_from_monday()))
            .unwrap_or(&""),
        t.day(),
        MONTHS
            .get(usize::from(u8::from(t.month())).saturating_sub(1))
            .unwrap_or(&""),
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request of testdata/azblob.json (`scripts/azblob/generate`), signed again as
    /// azblob's client signed it; its keys escaped as the client escapes them.
    #[test]
    fn requests_are_azblobs() {
        let key = base64::engine::general_purpose::STANDARD
            .decode(
                "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==",
            )
            .unwrap();
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("testdata/azblob.json")).unwrap();
        assert_eq!(cases.len(), 13);
        for c in &cases {
            let url = c["url"].as_str().unwrap();
            let target = format!(
                "/devstoreaccount1{}",
                url.split_once("/devstoreaccount1").unwrap().1
            );
            let target = target.as_str();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let headers: Vec<(String, String)> = c["headers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    (
                        h[0].as_str().unwrap().to_string(),
                        h[1].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            let given: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let want = given.iter().find(|(k, _)| *k == "authorization").unwrap().1;
            let got = sign(
                c["method"].as_str().unwrap(),
                path,
                query,
                &given,
                c["length"].as_u64().unwrap_or(0),
                "devstoreaccount1",
                &key,
            );
            assert_eq!(got, want, "{}: {url}", c["op"]);
        }
        let paths: Vec<&str> = cases.iter().map(|c| c["url"].as_str().unwrap()).collect();
        for (key, escaped) in [
            ("cache/blobs/sha256:abc", "cache%2Fblobs%2Fsha256:abc"),
            ("a dir/☃", "a%20dir%2F%E2%98%83"),
        ] {
            assert_eq!(path_escape(key), escaped);
            assert!(
                paths
                    .iter()
                    .any(|p| p.ends_with(&format!("/buildkit-cache/{escaped}")))
            );
        }
    }

    /// Keys as Go's filepath.Join makes them.
    #[test]
    fn keys_are_joined_as_go_joins_them() {
        assert_eq!(join(&["", "manifests", "buildkit"]), "manifests/buildkit");
        assert_eq!(join(&["team/", "/blobs/", "sha256:a"]), "team/blobs/sha256:a");
        assert_eq!(join(&["a", "../b", "c"]), "b/c");
        assert_eq!(join(&["", "", ""]), "");
    }

    /// Every request the backend makes, held to Azurite, Microsoft's Blob Storage, which
    /// checks shared-key signatures (`docs/research/platform-measurements.md` M125):
    /// `SHARDS_TEST_AZBLOB` names its account's URL, the emulator's own account and key.
    /// Blocks are 1 MiB, so a 3 MiB layer goes in three, as a layer past 32 MiB does.
    #[test]
    fn azblob_requests_are_blob_storages() {
        let Ok(account_url) = std::env::var("SHARDS_TEST_AZBLOB") else {
            println!("SKIP: SHARDS_TEST_AZBLOB names no Blob Storage");
            return;
        };
        let e = CacheEntry {
            kind: "azblob".into(),
            attrs: [
                ("account_url", account_url),
                ("account_name", "devstoreaccount1".into()),
                (
                    "secret_access_key",
                    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==".into(),
                ),
                ("container", format!("probe-{}", std::process::id())),
                ("prefix", "team dir".into()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        };
        let mut c = Container::of(&e, &|_| None).unwrap();
        c.block = 1 << 20;
        let read = |key: &str| {
            let mut out = Vec::new();
            c.get(key).unwrap().unwrap().read_to_end(&mut out).unwrap();
            out
        };
        // Made, then there.
        c.ensure().unwrap();
        c.ensure().unwrap();
        let m = c.manifest_key("buildkit");
        assert_eq!(m, "team dir/manifests/buildkit");
        assert!(!c.exists(&m).unwrap());
        assert!(c.get(&m).unwrap().is_none());
        c.put(&m, Body::Bytes(b"{\"records\":{}}")).unwrap();
        assert!(c.exists(&m).unwrap());
        assert_eq!(read(&m), b"{\"records\":{}}");
        c.put(&m, Body::Bytes(b"{}")).unwrap();
        assert_eq!(read(&m), b"{}", "the last writer wins");
        let path = std::env::temp_dir().join(format!("shards-azblob-probe-{}", std::process::id()));
        let big: Vec<u8> = (0..3u32 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &big).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        // One request, then blocks; each again, where it is there, nothing.
        let small = c.blob_key("sha256:small");
        c.put_new(&small, &file, 1000).unwrap();
        assert_eq!(read(&small), &big[..1000]);
        c.put_new(&small, &file, 1000).unwrap();
        let blocks = c.blob_key("sha256:blocks");
        c.put_new(&blocks, &file, big.len() as u64).unwrap();
        assert!(read(&blocks) == big, "the blocks are the file");
        c.put_new(&blocks, &file, big.len() as u64).unwrap();
        drop(file);
        let _ = std::fs::remove_file(&path);
        // A key signed wrong is refused.
        let mut wrong = Container::of(&e, &|_| None).unwrap();
        wrong.key = Zeroizing::new(b"not the key".to_vec());
        assert!(wrong.exists(&m).is_err());
    }
}
