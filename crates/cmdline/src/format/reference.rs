//! Image references as the formatter reads them: distribution/reference v0.6.0's
//! ParseNormalizedNamed (normalize.go, reference.go, with regexp.go's grammar matched by
//! hand) and familiar names (helpers.go), with digests validated as go-digest v1.0.0's
//! Validate does for the algorithms Go's crypto registers (sha256, sha384, sha512).

/// A parsed reference: a name, with a tag, a digest or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Named {
    domain: String,
    path: String,
    pub(super) tag: String,
    pub(super) digest: String,
}

impl Named {
    /// reference.FamiliarName: `docker.io` left out, and `library/` for an official image.
    pub(super) fn familiar_name(&self) -> String {
        if self.domain == "docker.io" {
            if let Some(rest) = self.path.strip_prefix("library/")
                && !rest.contains('/')
            {
                return rest.to_string();
            }
            return self.path.clone();
        }
        if self.domain.is_empty() {
            return self.path.clone();
        }
        format!("{}/{}", self.domain, self.path)
    }

    /// reference.FamiliarString: the familiar name with the tag and digest there are.
    pub(super) fn familiar_string(&self) -> String {
        let mut s = self.familiar_name();
        if !self.tag.is_empty() {
            s.push(':');
            s.push_str(&self.tag);
        }
        if !self.digest.is_empty() {
            s.push('@');
            s.push_str(&self.digest);
        }
        s
    }
}

/// reference.ParseNormalizedNamed; None where it fails.
pub(super) fn parse_normalized_named(s: &str) -> Option<Named> {
    if s.len() == 64 && s.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9')) {
        return None;
    }
    let (domain, remainder) = split_docker_domain(s);
    let remote = remainder.split(':').next().unwrap_or_default();
    if remote.to_lowercase() != remote {
        return None;
    }
    parse(&format!("{domain}/{remainder}"))
}

/// normalize.go's splitDockerDomain.
fn split_docker_domain(name: &str) -> (String, String) {
    let Some((maybe_domain, maybe_remote)) = name.split_once('/') else {
        return ("docker.io".into(), format!("library/{name}"));
    };
    let (domain, mut remote) = if maybe_domain == "localhost" {
        (maybe_domain.to_string(), maybe_remote.to_string())
    } else if maybe_domain == "index.docker.io" {
        ("docker.io".to_string(), maybe_remote.to_string())
    } else if maybe_domain.contains(['.', ':']) || maybe_domain.to_lowercase() != maybe_domain {
        (maybe_domain.to_string(), maybe_remote.to_string())
    } else {
        ("docker.io".to_string(), name.to_string())
    };
    if domain == "docker.io" && !remote.contains('/') {
        remote = format!("library/{remote}");
    }
    (domain, remote)
}

/// reference.Parse, of a reference with a name.
fn parse(s: &str) -> Option<Named> {
    let (name_tag, digest) = match s.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (s, None),
    };
    if let Some(d) = digest
        && !valid_digest(d)
    {
        return None;
    }
    // A tag's colon is after the name's last slash: a port's comes before one.
    let slash = name_tag.rfind('/');
    let (name, tag) = match name_tag.rfind(':') {
        Some(colon) if slash.is_none_or(|sl| colon > sl) => (
            name_tag.get(..colon).unwrap_or_default(),
            Some(name_tag.get(colon + 1..).unwrap_or_default()),
        ),
        _ => (name_tag, None),
    };
    if let Some(t) = tag
        && !valid_tag(t)
    {
        return None;
    }
    // anchoredNameRegexp: a domain first when one matches, else all path.
    let (domain, path) = match name.split_once('/') {
        Some((d, p)) if valid_domain(d) && valid_path(p) => (d, p),
        _ if valid_path(name) => ("", name),
        _ => return None,
    };
    if path.len() > 255 {
        return None;
    }
    Some(Named {
        domain: domain.to_string(),
        path: path.to_string(),
        tag: tag.unwrap_or_default().to_string(),
        digest: digest.unwrap_or_default().to_string(),
    })
}

/// `[\w][\w.-]{0,127}`.
fn valid_tag(t: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let b = t.as_bytes();
    matches!(b.first(), Some(&c) if word(c))
        && b.len() <= 128
        && b.iter().all(|&c| word(c) || c == b'.' || c == b'-')
}

/// `host(:port)?`: dot-separated components of letters, digits and inner hyphens, or a
/// bracketed IPv6 address.
fn valid_domain(d: &str) -> bool {
    let (host, port) = match d.rfind(':') {
        Some(i) if !d.ends_with(']') => (d.get(..i).unwrap_or_default(), d.get(i + 1..)),
        _ => (d, None),
    };
    if let Some(port) = port
        && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
    }
    host.split('.').all(|c| {
        let b = c.as_bytes();
        !b.is_empty()
            && b.iter().all(|&x| x.is_ascii_alphanumeric() || x == b'-')
            && b.first().is_some_and(u8::is_ascii_alphanumeric)
            && b.last().is_some_and(u8::is_ascii_alphanumeric)
    })
}

/// remoteName: components of `[a-z0-9]+` runs joined by `.`, `_`, `__` or hyphens, split by
/// slashes.
fn valid_path(p: &str) -> bool {
    p.split('/').all(|component| {
        let b = component.as_bytes();
        let alnum = |x: u8| x.is_ascii_lowercase() || x.is_ascii_digit();
        if !b.first().is_some_and(|&x| alnum(x)) || !b.last().is_some_and(|&x| alnum(x)) {
            return false;
        }
        let mut i = 0;
        while i < b.len() {
            let x = b.get(i).copied().unwrap_or(0);
            if alnum(x) {
                i += 1;
                continue;
            }
            let run = b
                .get(i..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&y| !alnum(y))
                .count();
            let sep = b.get(i..i + run).unwrap_or_default();
            let ok = sep == b"." || sep == b"_" || sep == b"__" || sep.iter().all(|&y| y == b'-');
            if !ok {
                return false;
            }
            i += run;
        }
        true
    })
}

/// digestPat, then go-digest's Validate: a known algorithm and its length of lower-case hex.
fn valid_digest(d: &str) -> bool {
    let Some((algorithm, encoded)) = d.split_once(':') else {
        return false;
    };
    let hex = match algorithm {
        "sha256" => 64,
        "sha384" => 96,
        "sha512" => 128,
        _ => return false,
    };
    encoded.len() == hex && encoded.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1";

    #[test]
    fn references_parse_as_distribution_parses_them() {
        let r = parse_normalized_named("nginx:alpine").unwrap();
        assert_eq!((r.familiar_name(), r.tag.as_str()), ("nginx".into(), "alpine"));
        let r = parse_normalized_named(&format!("localhost:5000/team/app:v1@{DIGEST}")).unwrap();
        assert_eq!(
            r.familiar_string(),
            format!("localhost:5000/team/app:v1@{DIGEST}")
        );
        let r = parse_normalized_named("docker.io/library/a/b").unwrap();
        assert_eq!(r.familiar_name(), "library/a/b");
        assert!(parse_normalized_named("Upper/x").is_some());
        assert!(parse_normalized_named("x/Upper").is_none());
        assert!(parse_normalized_named("a..b").is_none());
        assert!(parse_normalized_named("a__b-----c").is_some());
        assert!(parse_normalized_named("<none>:<none>").is_none());
        assert!(parse_normalized_named("[::1]:5000/x").is_some());
    }
}
