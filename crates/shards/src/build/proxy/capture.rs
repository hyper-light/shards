//! What a step's requests through the build's proxy come to (D110), as BuildKit v0.33.0's
//! proxy records them (util/network/proxy.go `ProxyCapture`; proxyprovider's
//! `recordRequest`, `recordResponse` and `recordIncomplete`): each request with its status
//! and where it redirected, which the step's log lists; each GET answered whole with a
//! 2xx, by its content's digest, a material of the build's provenance; and what could not
//! be one, and why. URLs are recorded as BuildKit records them: a scheme's default port
//! left out, credentials redacted.

use shards_dockerfile::url::{self, Url};

/// A request as the step's log lists it (`ProxyRequest`): its URL, and where it
/// redirected, as [`capture_url`] makes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub redirect: String,
    pub status: u16,
}

/// A response read whole (`ProxyMaterial`): its request's URL and its body's digest,
/// `sha256:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Material {
    pub url: String,
    pub digest: String,
}

/// A request that is no material, and why (`ProxyIncomplete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incomplete {
    pub method: String,
    pub url: String,
    pub reason: &'static str,
}

/// A step's requests, materials and incomplete requests, in the order they came.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    pub requests: Vec<Request>,
    pub materials: Vec<Material>,
    pub incomplete: Vec<Incomplete>,
    /// Each request the policies let go, as they were asked of it (method, URL): asked
    /// again before the step is taken from the build cache.
    pub allowed: Vec<(String, String)>,
}

/// Every reason a request is no material.
pub const REASONS: [&str; 5] = [
    "method_not_materializable",
    "partial_response",
    "body_read_failed",
    "unsuccessful_response",
    "upstream_error",
];

impl Capture {
    /// `Materials`: each material, then the source of each redirect that leads to one, with
    /// the digest of where it led, as far as redirects chain. A URL recorded twice is known
    /// by its last digest, and a source redirected twice by its last target, as BuildKit's
    /// maps keep them; the sources come in their URLs' order (BuildKit's map order varies,
    /// and its provenance sorts them).
    pub fn materials(&self) -> Vec<Material> {
        use std::collections::BTreeMap;
        let mut out = self.materials.clone();
        let mut digests: BTreeMap<&str, &str> = BTreeMap::new();
        for m in &self.materials {
            digests.insert(&m.url, &m.digest);
        }
        let mut redirects: BTreeMap<&str, &str> = BTreeMap::new();
        for r in &self.requests {
            if !r.redirect.is_empty() && r.url != r.redirect {
                redirects.insert(&r.url, &r.redirect);
            }
        }
        loop {
            let mut added = Vec::new();
            for (&from, &to) in &redirects {
                if digests.contains_key(from) {
                    continue;
                }
                if let Some(&digest) = digests.get(to) {
                    added.push((from, digest));
                }
            }
            if added.is_empty() {
                break;
            }
            for (from, digest) in added {
                digests.insert(from, digest);
                out.push(Material {
                    url: from.to_string(),
                    digest: digest.to_string(),
                });
            }
        }
        out
    }

    /// `logProxyRequests`: the lines the step's log ends with, none where it made no
    /// request through the proxy.
    pub fn summary(&self) -> Vec<u8> {
        if self.requests.is_empty() {
            return Vec::new();
        }
        let mut out = b"proxy network requests:\n".to_vec();
        for r in &self.requests {
            out.extend_from_slice(format!("- {} {} -> {}\n", r.method, r.url, r.status).as_bytes());
        }
        out
    }
}

/// `proxyIncompleteReason`: why a response is no material, if it is not one: a method other
/// than GET; a part asked for (`Range`) or given (206); a body not read whole; a failure.
/// (BuildKit's `response_transformed` is a body its transport decompressed, which it never
/// asks for; nor does shards.)
pub fn incomplete_reason(method: &str, range: bool, status: u16, read_whole: bool) -> Option<&'static str> {
    if method != "GET" {
        return Some("method_not_materializable");
    }
    if range || status == 206 {
        return Some("partial_response");
    }
    if !read_whole {
        return Some("body_read_failed");
    }
    if status >= 400 {
        return Some("unsuccessful_response");
    }
    None
}

/// urlutil.RedactCredentials: `s` with its user and password each said as `xxxxx` where
/// given, as Go's `url.Parse` and `URL.String` read and write it; what Go cannot parse as
/// it is.
pub fn redact(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let Ok(mut u) = url::parse(s.as_bytes()) else {
        return s.to_string();
    };
    if let Some(user) = &mut u.user {
        let has_user = !user.username.is_empty();
        let has_password = user.password.is_some();
        const MASK: &[u8] = b"xxxxx";
        if has_user {
            user.username = MASK.to_vec();
        }
        if has_password {
            user.password = Some(MASK.to_vec());
        }
    }
    String::from_utf8_lossy(&u.string()).into_owned()
}

/// `captureURL`: `s` as BuildKit records it, its scheme's default port (http's 80, https'
/// 443) left out, then redacted.
pub fn capture_url(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match url::parse(s.as_bytes()) {
        Ok(mut u) if !u.scheme.is_empty() => {
            let (host, port) = u.host_port();
            if (u.scheme == b"http" && port == b"80") || (u.scheme == b"https" && port == b"443") {
                let host = host.to_vec();
                u.host = if host.contains(&b':') {
                    [b"[".as_slice(), &host, b"]"].concat()
                } else {
                    host
                };
            }
            redact(&String::from_utf8_lossy(&u.string()))
        }
        _ => redact(s),
    }
}

/// `finalURL`: where a response's `Location` leads, resolved against its request's URL as
/// Go's `URL.Parse` resolves it, or the `Location` as given where Go cannot parse it; empty
/// without one.
pub fn final_url(request: &Url, location: Option<&str>) -> String {
    match location.filter(|l| !l.is_empty()) {
        None => String::new(),
        Some(l) => match request.resolve(l.as_bytes()) {
            Ok(u) => String::from_utf8_lossy(&u.string()).into_owned(),
            Err(_) => l.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BuildKit's own cases (TestRedactCredentials, TestCaptureURLNormalizesDefaultPort).
    #[test]
    fn urls_are_recorded_as_buildkit_records_them() {
        for (given, want) in [
            (
                "https://user:password@host.tld/this:that",
                "https://xxxxx:xxxxx@host.tld/this:that",
            ),
            (
                "https://user@host.tld/this:that",
                "https://xxxxx@host.tld/this:that",
            ),
            (
                "https://:password@host.tld/this:that",
                "https://:xxxxx@host.tld/this:that",
            ),
            ("https://host.tld/this:that", "https://host.tld/this:that"),
            ("1https://foo.com", "1https://foo.com"),
        ] {
            assert_eq!(redact(given), want, "{given}");
        }
        for (given, want) in [
            (
                "https://dl-cdn.alpinelinux.org:443/alpine/v3.23/main/aarch64/APKINDEX.tar.gz",
                "https://dl-cdn.alpinelinux.org/alpine/v3.23/main/aarch64/APKINDEX.tar.gz",
            ),
            ("http://example.com:80/file", "http://example.com/file"),
            ("https://example.com:8443/file", "https://example.com:8443/file"),
            (
                "https://user:pass@example.com:443/file",
                "https://xxxxx:xxxxx@example.com/file",
            ),
            ("https://[2001:db8::1]:443/file", "https://[2001:db8::1]/file"),
            (
                "https://[2001:db8::1]:8443/file",
                "https://[2001:db8::1]:8443/file",
            ),
            ("http://example.com:443/", "http://example.com:443/"),
            ("", ""),
        ] {
            assert_eq!(capture_url(given), want, "{given}");
        }
    }

    /// Held to BuildKit v0.33.0's own captureURL, redactURL and finalURL, as
    /// scripts/proxy/generate records them.
    #[test]
    fn urls_are_captured_as_buildkits_proxy_captures_them() {
        let oracle: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/proxy-oracle.json")).unwrap();
        for u in oracle["urls"].as_array().unwrap() {
            let given = u["in"].as_str().unwrap();
            assert_eq!(
                capture_url(given),
                u["capture"].as_str().unwrap(),
                "capture {given:?}"
            );
            assert_eq!(redact(given), u["redact"].as_str().unwrap(), "redact {given:?}");
        }
        for f in oracle["finals"].as_array().unwrap() {
            let request = url::parse(f["request"].as_str().unwrap().as_bytes()).unwrap();
            let location = f["location"].as_str().unwrap();
            assert_eq!(
                final_url(&request, Some(location)),
                f["final"].as_str().unwrap(),
                "{location:?} from {}",
                f["request"]
            );
        }
    }

    /// The measured build's step (shards-dind, buildx v0.37.1 on BuildKit v0.33.0): its
    /// lines as its log had them, and a redirect's source a material by its target's digest.
    #[test]
    fn a_steps_requests_come_to_its_lines_and_materials() {
        let request = |method: &str, url: &str, redirect: &str, status| Request {
            method: method.into(),
            url: url.into(),
            redirect: redirect.into(),
            status,
        };
        let hello = "http://172.18.0.2/hello";
        let digest = "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";
        let c = Capture {
            requests: vec![
                request("GET", hello, "", 200),
                request("GET", "http://172.18.0.2/redirect", hello, 302),
                request("GET", "http://172.18.0.2/redirect", hello, 302),
                request("GET", hello, "", 200),
                request("POST", hello, "", 200),
                request("GET", hello, "", 200),
                request("GET", "https://example.com/", "", 200),
                request("GET", "http://172.18.0.2:81/", "", 502),
            ],
            materials: vec![Material {
                url: hello.into(),
                digest: digest.into(),
            }],
            incomplete: Vec::new(),
            allowed: Vec::new(),
        };
        assert_eq!(
            String::from_utf8(c.summary()).unwrap(),
            "proxy network requests:\n\
             - GET http://172.18.0.2/hello -> 200\n\
             - GET http://172.18.0.2/redirect -> 302\n\
             - GET http://172.18.0.2/redirect -> 302\n\
             - GET http://172.18.0.2/hello -> 200\n\
             - POST http://172.18.0.2/hello -> 200\n\
             - GET http://172.18.0.2/hello -> 200\n\
             - GET https://example.com/ -> 200\n\
             - GET http://172.18.0.2:81/ -> 502\n"
        );
        let urls: Vec<(String, String)> = c.materials().into_iter().map(|m| (m.url, m.digest)).collect();
        assert_eq!(
            urls,
            [
                (hello.to_string(), digest.to_string()),
                ("http://172.18.0.2/redirect".to_string(), digest.to_string())
            ]
        );
        assert!(Capture::default().summary().is_empty());
    }

    /// Redirects chain to a material however they are recorded; one leading nowhere known,
    /// or to itself, makes none.
    #[test]
    fn redirects_chain_to_their_material() {
        let r = |url: &str, to: &str| Request {
            method: "GET".into(),
            url: url.into(),
            redirect: to.into(),
            status: 302,
        };
        let c = Capture {
            requests: vec![
                r("http://h/a", "http://h/b"),
                r("http://h/b", "http://h/c"),
                r("http://h/x", "http://h/x"),
                r("http://h/y", "http://h/z"),
            ],
            materials: vec![Material {
                url: "http://h/c".into(),
                digest: "sha256:c".into(),
            }],
            incomplete: Vec::new(),
            allowed: Vec::new(),
        };
        let mut got: Vec<String> = c
            .materials()
            .into_iter()
            .map(|m| format!("{} {}", m.url, m.digest))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "http://h/a sha256:c",
                "http://h/b sha256:c",
                "http://h/c sha256:c"
            ]
        );
    }

    /// Each reason, in BuildKit's order of asking.
    #[test]
    fn incomplete_responses_say_why() {
        assert_eq!(
            incomplete_reason("POST", true, 500, false),
            Some("method_not_materializable")
        );
        assert_eq!(
            incomplete_reason("HEAD", false, 200, true),
            Some("method_not_materializable")
        );
        assert_eq!(
            incomplete_reason("GET", true, 200, true),
            Some("partial_response")
        );
        assert_eq!(
            incomplete_reason("GET", false, 206, false),
            Some("partial_response")
        );
        assert_eq!(
            incomplete_reason("GET", false, 200, false),
            Some("body_read_failed")
        );
        assert_eq!(
            incomplete_reason("GET", false, 404, false),
            Some("body_read_failed")
        );
        assert_eq!(
            incomplete_reason("GET", false, 404, true),
            Some("unsuccessful_response")
        );
        assert_eq!(incomplete_reason("GET", false, 302, true), None);
        assert_eq!(incomplete_reason("GET", false, 200, true), None);
    }

    /// A redirect's target is resolved against its request, a Location Go cannot parse
    /// kept as given.
    #[test]
    fn redirect_targets_resolve_against_their_request() {
        let base = url::parse(b"http://172.18.0.2/redirect").unwrap();
        assert_eq!(final_url(&base, Some("/hello")), "http://172.18.0.2/hello");
        assert_eq!(final_url(&base, Some("%zz")), "%zz");
        assert_eq!(final_url(&base, Some("")), "");
        assert_eq!(final_url(&base, None), "");
    }
}
