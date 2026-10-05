//! Image references as `docker pull` parses them: distribution/reference v0.6.0's grammar
//! (regexp.go, compiled here with the same patterns) and normalization (normalize.go:
//! `ParseNormalizedNamed`, then `TagNameOnly`), with digests validated as go-digest v1.0.0
//! validates them. Docker CLI v29.8.1 and containerd v2.4.1 both use those versions
//! (docs/research/registry-pull.md §1).

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

use crate::{Error, bad};

/// Docker Hub's name in references.
pub const DOCKER_HUB: &str = "docker.io";
const LEGACY_DOCKER_HUB: &str = "index.docker.io";
const OFFICIAL_PREFIX: &str = "library/";
const DEFAULT_TAG: &str = "latest";
/// The longest repository path, without its domain.
const PATH_MAX: usize = 255;

// The patterns of regexp.go. Go's `\w` is ASCII, spelled out here as `[0-9A-Za-z_]`.
const ALPHANUMERIC: &str = "[a-z0-9]+";
const SEPARATOR: &str = "(?:[._]|__|[-]+)";
const DOMAIN_NAME_COMPONENT: &str = "(?:[a-zA-Z0-9]|[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9])";
const OPTIONAL_PORT: &str = "(?::[0-9]+)?";
const TAG: &str = "[0-9A-Za-z_][0-9A-Za-z_.-]{0,127}";
const DIGEST: &str = "[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*[:][[:xdigit:]]{32,}";
const IPV6: &str = r"\[(?:[a-fA-F0-9:]+)\]";

fn domain_and_port() -> String {
    let domain_name = format!(r"{DOMAIN_NAME_COMPONENT}(?:\.{DOMAIN_NAME_COMPONENT})*");
    format!("(?:{domain_name}|{IPV6}){OPTIONAL_PORT}")
}

fn remote_name() -> String {
    let path_component = format!("{ALPHANUMERIC}(?:{SEPARATOR}{ALPHANUMERIC})*");
    format!("{path_component}(?:/{path_component})*")
}

/// `ReferenceRegexp`: name, tag and digest.
static REFERENCE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let name = format!("(?:{}/)?{}", domain_and_port(), remote_name());
    Regex::new(&format!("^({name})(?::({TAG}))?(?:@({DIGEST}))?$")).ok()
});

/// `anchoredNameRegexp`: a name's domain and path.
static NAME: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(&format!("^(?:({})/)?({})$", domain_and_port(), remote_name())).ok());

/// `anchoredIdentifierRegexp`: a bare image ID.
static IDENTIFIER: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new("^[a-f0-9]{64}$").ok());

fn regex(r: &'static LazyLock<Option<Regex>>) -> Result<&'static Regex, Error> {
    r.as_ref()
        .ok_or_else(|| Error("reference patterns did not compile".into()))
}

/// A content digest, as go-digest validates one: sha256, sha384 or sha512, and exactly
/// that many lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algorithm: Algorithm,
    hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Algorithm {
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Sha256 => "sha256",
            Algorithm::Sha384 => "sha384",
            Algorithm::Sha512 => "sha512",
        }
    }

    /// Bytes of hash output.
    pub fn size(self) -> usize {
        match self {
            Algorithm::Sha256 => 32,
            Algorithm::Sha384 => 48,
            Algorithm::Sha512 => 64,
        }
    }
}

/// go-digest's `DigestRegexp` for a digest split at its first `:`: components of
/// lowercase letters and digits joined by single `.+_-`, then `[a-zA-Z0-9=_-]+`.
fn well_formed(algorithm: &str, encoded: &str) -> bool {
    algorithm
        .split(['.', '+', '_', '-'])
        .all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        && encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'_' | b'-'))
}

impl Digest {
    /// go-digest's `Parse`: `algorithm:hex`.
    pub fn parse(s: &str) -> Result<Digest, Error> {
        let Some((algorithm, hex)) = s.split_once(':').filter(|(a, h)| !a.is_empty() && !h.is_empty()) else {
            return bad("invalid checksum digest format");
        };
        let algorithm = match algorithm {
            "sha256" => Algorithm::Sha256,
            "sha384" => Algorithm::Sha384,
            "sha512" => Algorithm::Sha512,
            // An unknown algorithm is unsupported only when the digest is well formed:
            // `DigestRegexpAnchored`, `[a-z0-9]+(?:[.+_-][a-z0-9]+)*:[a-zA-Z0-9=_-]+`.
            _ if well_formed(algorithm, hex) => return bad("unsupported digest algorithm"),
            _ => return bad("invalid checksum digest format"),
        };
        if hex.len() != 2 * algorithm.size() {
            return bad("invalid checksum digest length");
        }
        if !hex
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return bad("invalid checksum digest format");
        }
        Ok(Digest {
            algorithm,
            hex: hex.to_string(),
        })
    }

    /// A digest of `algorithm` over output `bytes`.
    pub fn from_hash(algorithm: Algorithm, bytes: &[u8]) -> Digest {
        Digest {
            algorithm,
            hex: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.name(), self.hex)
    }
}

/// What `ParseAnyReference` reads (distribution/reference normalize.go): a digest alone,
/// as 64 hex digits or `algorithm:hex`, or a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnyReference {
    Digest(Digest),
    Named(Reference),
}

impl AnyReference {
    pub fn parse(s: &str) -> Result<AnyReference, Error> {
        if regex(&IDENTIFIER)?.is_match(s) {
            return Digest::parse(&format!("sha256:{s}")).map(AnyReference::Digest);
        }
        if let Ok(digest) = Digest::parse(s) {
            return Ok(AnyReference::Digest(digest));
        }
        Reference::parse_normalized(s).map(AnyReference::Named)
    }
}

/// A normalized reference: a domain and path, a tag (`latest` when neither a tag nor a
/// digest was given), and maybe a digest, which wins over the tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub domain: String,
    pub path: String,
    pub tag: Option<String>,
    pub digest: Option<Digest>,
}

impl Reference {
    /// Parses a reference as `docker pull` does: `latest` when it names no tag or digest.
    pub fn parse(s: &str) -> Result<Reference, Error> {
        Reference::parse_normalized(s).map(Reference::tag_name_only)
    }

    /// `TagNameOnly`: `latest` as the tag of a reference that names neither tag nor digest.
    #[must_use]
    pub fn tag_name_only(mut self) -> Reference {
        if self.tag.is_none() && self.digest.is_none() {
            self.tag = Some(DEFAULT_TAG.into());
        }
        self
    }

    /// Parses a reference as `ParseNormalizedNamed` does, with the tag or digest it names
    /// and no other.
    pub fn parse_normalized(s: &str) -> Result<Reference, Error> {
        if regex(&IDENTIFIER)?.is_match(s) {
            return bad(format!(
                "invalid repository name ({s}), cannot specify 64-byte hexadecimal strings"
            ));
        }
        let (domain, remainder) = split_docker_domain(s);
        let remote = remainder.split_once(':').map_or(remainder.as_str(), |(r, _)| r);
        if remote.to_lowercase() != remote {
            return bad(format!(
                "invalid reference format: repository name ({remote}) must be lowercase"
            ));
        }
        parse(&format!("{domain}/{remainder}"))
    }

    /// The name alone: `docker.io/library/alpine`; the path alone for a name with no
    /// domain, as `repository.Name` prints it (distribution/reference reference.go).
    pub fn name(&self) -> String {
        if self.domain.is_empty() {
            return self.path.clone();
        }
        format!("{}/{}", self.domain, self.path)
    }

    /// `FamiliarName`: the name alone as users write it, `alpine` for
    /// `docker.io/library/alpine:3.20`.
    pub fn familiar_name(&self) -> String {
        let mut name = self.clone();
        name.tag = None;
        name.digest = None;
        name.familiar()
    }

    /// `FamiliarString`: the name as users write it, `alpine:3.20` for
    /// `docker.io/library/alpine:3.20`.
    pub fn familiar(&self) -> String {
        let mut path = self.path.as_str();
        let mut out = String::new();
        if self.domain == DOCKER_HUB {
            if let Some(rest) = path.strip_prefix(OFFICIAL_PREFIX)
                && !rest.contains('/')
            {
                path = rest;
            }
        } else if !self.domain.is_empty() {
            out.push_str(&self.domain);
            out.push('/');
        }
        out.push_str(path);
        self.push_suffix(&mut out);
        out
    }

    fn push_suffix(&self, out: &mut String) {
        if let Some(tag) = &self.tag {
            out.push(':');
            out.push_str(tag);
        }
        if let Some(digest) = &self.digest {
            out.push('@');
            out.push_str(&digest.to_string());
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = self.name();
        self.push_suffix(&mut out);
        f.write_str(&out)
    }
}

/// `Parse`: a reference that already carries its domain.
fn parse(s: &str) -> Result<Reference, Error> {
    let Some(m) = regex(&REFERENCE)?.captures(s) else {
        if s.is_empty() {
            return bad("repository name must have at least one component");
        }
        if regex(&REFERENCE)?.is_match(&s.to_lowercase()) {
            return bad("repository name must be lowercase");
        }
        return bad("invalid reference format");
    };
    let name = m.get(1).map_or("", |n| n.as_str());
    let (domain, path) = match regex(&NAME)?.captures(name) {
        Some(n) => (
            n.get(1).map_or("", |d| d.as_str()),
            n.get(2).map_or("", |p| p.as_str()),
        ),
        None => ("", name),
    };
    if path.len() > PATH_MAX {
        return bad(format!(
            "repository name must not be more than {PATH_MAX} characters"
        ));
    }
    let digest = m.get(3).map(|d| Digest::parse(d.as_str())).transpose()?;
    Ok(Reference {
        domain: domain.to_string(),
        path: path.to_string(),
        tag: m.get(2).map(|t| t.as_str().to_string()),
        digest,
    })
}

/// `splitDockerDomain`: the domain, if the first component names one, else Docker Hub,
/// where one-component names are official images.
fn split_docker_domain(name: &str) -> (String, String) {
    let Some((first, rest)) = name.split_once('/') else {
        return (DOCKER_HUB.into(), format!("{OFFICIAL_PREFIX}{name}"));
    };
    // In Go's order: the legacy Hub name is rewritten before any dotted name is taken.
    let (domain, remote) = if first == "localhost" {
        (first.to_string(), rest.to_string())
    } else if first == LEGACY_DOCKER_HUB {
        (DOCKER_HUB.to_string(), rest.to_string())
    } else if first.contains(['.', ':']) || first.to_lowercase() != first {
        (first.to_string(), rest.to_string())
    } else {
        (DOCKER_HUB.to_string(), name.to_string())
    };
    if domain == DOCKER_HUB && !remote.contains('/') {
        return (domain, format!("{OFFICIAL_PREFIX}{remote}"));
    }
    (domain, remote)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const SHA: &str = "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    /// From normalize_test.go (TestParseRepositoryInfo, TestParseDockerRef): input →
    /// (domain, path, tag, familiar form).
    #[test]
    fn references_normalize_as_docker_normalizes_them() {
        for (input, domain, path, tag, familiar) in [
            (
                "fooo/bar",
                "docker.io",
                "fooo/bar",
                Some("latest"),
                "fooo/bar:latest",
            ),
            (
                "library/ubuntu",
                "docker.io",
                "library/ubuntu",
                Some("latest"),
                "ubuntu:latest",
            ),
            (
                "nonlibrary/ubuntu",
                "docker.io",
                "nonlibrary/ubuntu",
                Some("latest"),
                "nonlibrary/ubuntu:latest",
            ),
            (
                "ubuntu",
                "docker.io",
                "library/ubuntu",
                Some("latest"),
                "ubuntu:latest",
            ),
            (
                "other/library",
                "docker.io",
                "other/library",
                Some("latest"),
                "other/library:latest",
            ),
            (
                "127.0.0.1:8000/private/moonbase",
                "127.0.0.1:8000",
                "private/moonbase",
                Some("latest"),
                "127.0.0.1:8000/private/moonbase:latest",
            ),
            (
                "127.0.0.1:8000/privatebase",
                "127.0.0.1:8000",
                "privatebase",
                Some("latest"),
                "127.0.0.1:8000/privatebase:latest",
            ),
            (
                "example.com/private/moonbase",
                "example.com",
                "private/moonbase",
                Some("latest"),
                "example.com/private/moonbase:latest",
            ),
            (
                "example.com/privatebase",
                "example.com",
                "privatebase",
                Some("latest"),
                "example.com/privatebase:latest",
            ),
            (
                "example.com:8000/private/moonbase",
                "example.com:8000",
                "private/moonbase",
                Some("latest"),
                "example.com:8000/private/moonbase:latest",
            ),
            (
                "localhost:8000/private/moonbase",
                "localhost:8000",
                "private/moonbase",
                Some("latest"),
                "localhost:8000/private/moonbase:latest",
            ),
            (
                "localhost/privatebase",
                "localhost",
                "privatebase",
                Some("latest"),
                "localhost/privatebase:latest",
            ),
            (
                "index.docker.io/library/ubuntu",
                "docker.io",
                "library/ubuntu",
                Some("latest"),
                "ubuntu:latest",
            ),
            (
                "docker.io/ubuntu",
                "docker.io",
                "library/ubuntu",
                Some("latest"),
                "ubuntu:latest",
            ),
            (
                "registry-1.docker.io/ubuntu",
                "registry-1.docker.io",
                "ubuntu",
                Some("latest"),
                "registry-1.docker.io/ubuntu:latest",
            ),
            (
                "ubuntu:18.04",
                "docker.io",
                "library/ubuntu",
                Some("18.04"),
                "ubuntu:18.04",
            ),
            (
                "[::1]:5000/repo",
                "[::1]:5000",
                "repo",
                Some("latest"),
                "[::1]:5000/repo:latest",
            ),
            (
                "Foo.com/bar:TAG",
                "Foo.com",
                "bar",
                Some("TAG"),
                "Foo.com/bar:TAG",
            ),
            (
                "localhost:5000",
                "docker.io",
                "library/localhost",
                Some("5000"),
                "localhost:5000",
            ),
        ] {
            let r = Reference::parse(input).unwrap();
            assert_eq!(
                (
                    r.domain.as_str(),
                    r.path.as_str(),
                    r.tag.as_deref(),
                    r.familiar().as_str()
                ),
                (domain, path, tag, familiar),
                "{input}"
            );
        }
    }

    #[test]
    fn digests_win_and_keep_their_tag() {
        let r = Reference::parse(&format!("foo/bar:tag@{SHA}")).unwrap();
        assert_eq!(
            (r.tag.as_deref(), r.digest.as_ref().unwrap().to_string()),
            (Some("tag"), SHA.to_string())
        );
        let r = Reference::parse(&format!("foo@{SHA}")).unwrap();
        assert_eq!(
            (r.tag.clone(), r.to_string()),
            (None, format!("docker.io/library/foo@{SHA}"))
        );
    }

    /// From reference_test.go and normalize_test.go: inputs Docker refuses.
    #[test]
    fn invalid_references_are_refused() {
        for input in [
            "",
            ":justtag",
            "@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "validname@invaliddigest:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "Uppercase:tag",
            "docker/Docker",
            "-docker",
            "-docker/docker",
            "docker///docker",
            "docker.io/docker/Docker",
            "docker.io/docker///docker",
            "1a3f5e7d9c1b3a5f7e9d1c3b5a7f9e1d3c5b7a9f1e3d5d7c9b1a3f5e7d9c1b3a",
            "aa/asdf$$^/aa",
            "foo/bar@sha256:abc",
            "foo/bar@sha1:0123456789abcdef0123456789abcdef01234567",
            "foo/bar@SHA256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "foo/bar@sha256:FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "foo/bar:tag with space",
            "foo/bar:é",
        ] {
            assert!(Reference::parse(input).is_err(), "{input:?} parsed");
        }
    }

    /// A first component with a dot but not a domain's characters is a path component,
    /// as distribution/reference's regexp takes it: the name has no domain, and prints as
    /// its path alone, which parses to the same (found by fuzz/fuzz_targets/reference.rs).
    #[test]
    fn a_name_without_a_domain_prints_as_its_path() {
        let r = Reference::parse("zz-zz-4z.z4_zz/9").unwrap();
        assert_eq!((r.domain.as_str(), r.path.as_str()), ("", "zz-zz-4z.z4_zz/9"));
        assert_eq!(r.to_string(), "zz-zz-4z.z4_zz/9:latest");
        assert_eq!(r.familiar(), "zz-zz-4z.z4_zz/9:latest");
        assert_eq!(Reference::parse(&r.to_string()).unwrap(), r);
    }

    #[test]
    fn names_are_limited_to_255_characters_after_the_domain() {
        assert!(Reference::parse(&format!("example.com/{}:tag", "a".repeat(255))).is_ok());
        assert!(Reference::parse(&format!("example.com/{}", "a".repeat(256))).is_err());
        let tag128 = format!("foo:{}", "t".repeat(128));
        assert!(Reference::parse(&tag128).is_ok());
        assert!(Reference::parse(&format!("{tag128}t")).is_err());
    }
}
