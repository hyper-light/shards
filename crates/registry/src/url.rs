//! URLs a pull requests: registry endpoints, token realms and redirect targets. Parsed and
//! resolved as RFC 3986 specifies (iri-string). Resolution only merges paths and removes
//! dot segments. Nothing is normalized, so a presigned URL keeps the exact bytes its
//! signature covers.

use std::fmt;

use iri_string::types::{UriAbsoluteStr, UriReferenceStr};

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    Http,
    Https,
}

/// An absolute `http` or `https` URL with a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    text: String,
    scheme: Scheme,
    /// Lowercased; an IPv6 address keeps its brackets.
    host: String,
    port: u16,
    explicit_port: bool,
    /// The path and query: what the request line carries (RFC 9112 §3.2.1).
    target: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, Error> {
        let shown = redact(s);
        let uri = UriAbsoluteStr::new(s)
            .map_err(|e| Error::new(format!("{shown:?} is not an absolute URL: {e}")))?;
        let scheme = match uri.scheme_str().to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => return Err(Error::new(format!("{shown:?}: unsupported scheme {other:?}"))),
        };
        let authority = uri
            .authority_components()
            .ok_or_else(|| Error::new(format!("{shown:?} has no host")))?;
        if authority.userinfo().is_some() {
            return Err(Error::new(format!("{shown:?} carries credentials")));
        }
        let host = authority.host().to_ascii_lowercase();
        if host.is_empty() || host.contains('%') {
            return Err(Error::new(format!("{shown:?} has no usable host")));
        }
        let (port, explicit_port) = match authority.port().filter(|p| !p.is_empty()) {
            Some(p) => (
                p.parse()
                    .map_err(|_| Error::new(format!("{shown:?} has a bad port")))?,
                true,
            ),
            None => (scheme.default_port(), false),
        };
        let path = uri.path_str();
        let mut target = if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
        if let Some(query) = uri.query_str() {
            target.push('?');
            target.push_str(query);
        }
        Ok(Url {
            text: s.to_string(),
            scheme,
            host,
            port,
            explicit_port,
            target,
        })
    }

    /// A request's URL from its parts, as a build's proxy forwards one (D110): its target
    /// (path and query) taken as the client wrote it, as Go's transport writes what it was
    /// given, not held to RFC 3986; only a space or a control there, which would split the
    /// request line, refuses it. The host is a name of letters, digits, `-`, `.` and `_`,
    /// lowercased, or an IP address, an IPv6 one in brackets.
    pub fn request(scheme: Scheme, host: &str, port: Option<u16>, target: &str) -> Result<Url, Error> {
        let named = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
        let bracketed = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok());
        if !named && !bracketed {
            return Err(Error::new(format!("{host:?} is no host a request can name")));
        }
        if !target.starts_with('/') || target.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::new(format!("{target:?} is no request target")));
        }
        let host = host.to_ascii_lowercase();
        let (word, default) = match scheme {
            Scheme::Http => ("http", 80),
            Scheme::Https => ("https", 443),
        };
        let text = match port {
            Some(p) => format!("{word}://{host}:{p}{target}"),
            None => format!("{word}://{host}{target}"),
        };
        Ok(Url {
            text,
            scheme,
            host,
            port: port.unwrap_or(default),
            explicit_port: port.is_some(),
            target: target.to_string(),
        })
    }

    /// `reference` resolved against this URL (RFC 3986 §5.2), as a `Location` is
    /// (RFC 9110 §10.2.2). A fragment is dropped: it is never sent.
    pub fn join(&self, reference: &str) -> Result<Url, Error> {
        let shown = redact(reference);
        let reference = UriReferenceStr::new(reference)
            .map_err(|e| Error::new(format!("{shown:?} is not a URL reference: {e}")))?;
        let base = UriAbsoluteStr::new(self.text.split('#').next().unwrap_or_default())
            .map_err(|e| Error::new(format!("{self}: {e}")))?;
        let resolved = reference.resolve_against(base);
        resolved
            .ensure_rfc3986_normalizable()
            .map_err(|e| Error::new(format!("{shown:?} cannot be resolved against {self}: {e}")))?;
        let text = resolved.to_string();
        Url::parse(text.split('#').next().unwrap_or_default())
    }

    /// This URL with `key=value` added to its query, the value escaped as Go's
    /// url.QueryEscape escapes it (all but `A-Za-z0-9-_.~`, a space as `+`).
    pub fn with_query_pair(&self, key: &str, value: &str) -> Result<Url, Error> {
        let mut escaped = String::with_capacity(value.len());
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    escaped.push(b as char)
                }
                b' ' => escaped.push('+'),
                _ => escaped.push_str(&format!("%{b:02X}")),
            }
        }
        let text = self.text.split('#').next().unwrap_or_default();
        let sep = if text.contains('?') { '&' } else { '?' };
        Url::parse(&format!("{text}{sep}{key}={escaped}"))
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The host, lowercased; an IPv6 address in brackets.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host[:port]` for the `Host` field, with the port only when it is not the scheme's
    /// default (RFC 9110 §7.2).
    pub fn authority(&self) -> String {
        if self.explicit_port && self.port != self.scheme.default_port() {
            format!("{}:{}", self.host, self.port)
        } else {
            self.host.clone()
        }
    }

    /// The path and query, for the request line.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The origin as one key: scheme, host and port, the port given even when it is the
    /// scheme's default (RFC 6454 §4).
    pub fn origin(&self) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// The same URL in plain HTTP, its port kept.
    pub fn plain(&self) -> Result<Url, Error> {
        Url::parse(&format!("http://{}:{}{}", self.host, self.port, self.target))
    }

    /// Whether `other` is the same origin: scheme, host and port (RFC 6454 §4).
    pub fn same_origin(&self, other: &Url) -> bool {
        self.scheme == other.scheme && self.host == other.host && self.port == other.port
    }
}

/// A URL or reference for messages: without its query and fragment, and its password, if
/// it has one, shown as `xxxxx`, as Go's URL.Redacted shows it.
fn redact(s: &str) -> String {
    let s = s.split(['?', '#']).next().unwrap_or_default();
    let start = s.find("://").map_or(0, |i| i + 3);
    let rest = s.get(start..).unwrap_or_default();
    let authority = rest
        .get(..rest.find('/').unwrap_or(rest.len()))
        .unwrap_or_default();
    let Some(at) = authority.rfind('@') else {
        return s.to_string();
    };
    let Some(colon) = authority.get(..at).and_then(|userinfo| userinfo.find(':')) else {
        return s.to_string();
    };
    format!(
        "{}{}:xxxxx{}",
        s.get(..start).unwrap_or_default(),
        authority.get(..colon).unwrap_or_default(),
        rest.get(at..).unwrap_or_default()
    )
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

impl Url {
    /// The whole URL, query included.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

/// Shows the URL without its query, which can carry credentials: a presigned URL's
/// signature, for one. containerd redacts them in logs the same way.
impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, query) = match self.target.split_once('?') {
            Some((path, _)) => (path, "?…"),
            None => (self.target.as_str(), ""),
        };
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        write!(f, "{scheme}://{}{path}{query}", self.authority())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// A pair added as Go's url.Values encodes it, to a query or none.
    #[test]
    fn query_pairs_are_added_as_go_escapes_them() {
        let u = super::Url::parse("http://127.0.0.1:5000/v2/x/blobs/uploads/abc?_state=a%2Fb").unwrap();
        assert_eq!(
            u.with_query_pair("digest", "sha256:0a b~").unwrap().target(),
            "/v2/x/blobs/uploads/abc?_state=a%2Fb&digest=sha256%3A0a+b~"
        );
        let bare = super::Url::parse("https://r.example/v2/x/blobs/uploads/abc").unwrap();
        assert_eq!(
            bare.with_query_pair("digest", "d").unwrap().target(),
            "/v2/x/blobs/uploads/abc?digest=d"
        );
    }

    use super::*;

    #[test]
    fn urls_split_into_what_a_request_needs() {
        let u = Url::parse("https://Registry-1.Docker.IO/v2/library/alpine/manifests/latest").unwrap();
        assert_eq!(
            (u.scheme(), u.host(), u.port()),
            (Scheme::Https, "registry-1.docker.io", 443)
        );
        assert_eq!(u.authority(), "registry-1.docker.io");
        assert_eq!(u.target(), "/v2/library/alpine/manifests/latest");
        let u = Url::parse("http://[::1]:5000?x=1").unwrap();
        assert_eq!(
            (u.host(), u.port(), u.authority().as_str()),
            ("[::1]", 5000, "[::1]:5000")
        );
        assert_eq!(u.target(), "/?x=1");
        assert_eq!(Url::parse("https://h:443/").unwrap().authority(), "h");
        let signed = Url::parse("https://cdn.example/blob?X-Amz-Signature=secret").unwrap();
        assert_eq!(signed.to_string(), "https://cdn.example/blob?…");
        for bad in [
            "ftp://h/",
            "/relative",
            "https:///nohost",
            "https://user:pw@h/",
            "https://h:99999/",
            "https://h/a b",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
        // A password is never shown, as Go's URL.Redacted shows none.
        let said = Url::parse("https://user:s3cret@h/v2/?q=1")
            .unwrap_err()
            .to_string();
        assert!(
            said.contains("https://user:xxxxx@h/v2/") && !said.contains("s3cret"),
            "{said}"
        );
        for (given, shown) in [
            ("https://u:p:q@h/x", "https://u:xxxxx@h/x"),
            ("https://u@h/x", "https://u@h/x"),
            ("u:p@h/x", "u:xxxxx@h/x"),
            ("https://h/a@b:c", "https://h/a@b:c"),
            ("https://u:p@h", "https://u:xxxxx@h"),
        ] {
            assert_eq!(redact(given), shown, "{given}");
        }
    }

    /// A proxy's request keeps its target as written; only what would split a request
    /// line, or no host at all, is refused.
    #[test]
    fn requests_keep_their_targets_as_written() {
        let u = Url::request(Scheme::Https, "Example.COM", Some(443), "/a|b?q={x}&y=^").unwrap();
        assert_eq!(
            (u.host(), u.port(), u.authority().as_str()),
            ("example.com", 443, "example.com")
        );
        assert_eq!(u.target(), "/a|b?q={x}&y=^");
        let u = Url::request(Scheme::Http, "[::1]", None, "/").unwrap();
        assert_eq!((u.host(), u.port(), u.as_str()), ("[::1]", 80, "http://[::1]/"));
        assert_eq!(
            Url::request(Scheme::Http, "10.0.0.1", Some(8080), "/x")
                .unwrap()
                .as_str(),
            "http://10.0.0.1:8080/x"
        );
        for (host, target) in [
            ("", "/"),
            ("h", "x"),
            ("h", "/a b"),
            ("h", "/a\r\nX: y"),
            ("h", "/\u{7f}"),
            ("h/x", "/"),
            ("u@h", "/"),
            ("h%25eth0", "/"),
            ("[::1", "/"),
            ("[zz]", "/"),
            ("ä.example", "/"),
        ] {
            assert!(
                Url::request(Scheme::Http, host, None, target).is_err(),
                "{host:?} {target:?}"
            );
        }
    }

    /// RFC 3986 §5.4.1's normal examples and §5.4.2's abnormal ones, against its base.
    #[test]
    fn references_resolve_as_rfc_3986_resolves_them() {
        // The RFC's base uses scheme "http" and host "a"; a query and no fragment.
        let base = Url::parse("http://a/b/c/d;p?q").unwrap();
        for (reference, want) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q"),
            ("g#s", "http://a/b/c/g"),
            (";x", "http://a/b/c/;x"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            ("..g", "http://a/b/c/..g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("g?y/./x", "http://a/b/c/g?y/./x"),
            (
                "https://cdn.example/x?X-Amz-Signature=%2fAB",
                "https://cdn.example/x?X-Amz-Signature=%2fAB",
            ),
        ] {
            assert_eq!(base.join(reference).unwrap().as_str(), want, "{reference}");
        }
    }
}
