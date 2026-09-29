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
        let uri =
            UriAbsoluteStr::new(s).map_err(|e| Error(format!("{shown:?} is not an absolute URL: {e}")))?;
        let scheme = match uri.scheme_str().to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => return Err(Error(format!("{shown:?}: unsupported scheme {other:?}"))),
        };
        let authority = uri
            .authority_components()
            .ok_or_else(|| Error(format!("{shown:?} has no host")))?;
        if authority.userinfo().is_some() {
            return Err(Error(format!("{shown:?} carries credentials")));
        }
        let host = authority.host().to_ascii_lowercase();
        if host.is_empty() || host.contains('%') {
            return Err(Error(format!("{shown:?} has no usable host")));
        }
        let (port, explicit_port) = match authority.port().filter(|p| !p.is_empty()) {
            Some(p) => (
                p.parse()
                    .map_err(|_| Error(format!("{shown:?} has a bad port")))?,
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

    /// `reference` resolved against this URL (RFC 3986 §5.2), as a `Location` is
    /// (RFC 9110 §10.2.2). A fragment is dropped: it is never sent.
    pub fn join(&self, reference: &str) -> Result<Url, Error> {
        let shown = redact(reference);
        let reference = UriReferenceStr::new(reference)
            .map_err(|e| Error(format!("{shown:?} is not a URL reference: {e}")))?;
        let base = UriAbsoluteStr::new(self.text.split('#').next().unwrap_or_default())
            .map_err(|e| Error(format!("{self}: {e}")))?;
        let resolved = reference.resolve_against(base);
        resolved
            .ensure_rfc3986_normalizable()
            .map_err(|e| Error(format!("{shown:?} cannot be resolved against {self}: {e}")))?;
        let text = resolved.to_string();
        Url::parse(text.split('#').next().unwrap_or_default())
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

    /// Whether `other` is the same origin: scheme, host and port (RFC 6454 §4).
    pub fn same_origin(&self, other: &Url) -> bool {
        self.scheme == other.scheme && self.host == other.host && self.port == other.port
    }
}

/// A URL or reference without its query and fragment, for messages.
fn redact(s: &str) -> &str {
    s.split(['?', '#']).next().unwrap_or_default()
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
