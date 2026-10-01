//! Platforms as containerd's `platforms` package (v1.0.0-rc.4, the version BuildKit
//! dockerfile/1.27.1 vendors) parses, normalizes and formats them.
//!
//! One difference, deliberate: a specifier of an OS alone (`linux`) or an architecture
//! alone (`arm64`) takes what it leaves out from the build platform, where containerd takes
//! it from the machine the frontend happens to run on.

use crate::go;

/// An OCI platform. Strings are Go strings, bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: Vec<u8>,
    pub architecture: Vec<u8>,
    pub variant: Vec<u8>,
    pub os_version: Vec<u8>,
    pub os_features: Vec<Vec<u8>>,
}

impl Platform {
    pub fn new(os: &str, architecture: &str) -> Platform {
        Platform {
            os: os.as_bytes().to_vec(),
            architecture: architecture.as_bytes().to_vec(),
            ..Platform::default()
        }
    }
}

const INVALID: &[u8] = b"invalid argument";
const OS_RE: &str = r"^([A-Za-z0-9_-]+)(?:\(([A-Za-z0-9_.%-]*)((?:\+[A-Za-z0-9_.%-]+)*)\))?$";
const SPECIFIER_RE: &str = "^[A-Za-z0-9_.-]+$";

fn is_known_os(os: &[u8]) -> bool {
    matches!(
        os,
        b"aix"
            | b"android"
            | b"darwin"
            | b"dragonfly"
            | b"freebsd"
            | b"hurd"
            | b"illumos"
            | b"ios"
            | b"js"
            | b"linux"
            | b"nacl"
            | b"netbsd"
            | b"openbsd"
            | b"plan9"
            | b"solaris"
            | b"windows"
            | b"zos"
    )
}

fn is_known_arch(arch: &[u8]) -> bool {
    matches!(
        arch,
        b"386"
            | b"amd64"
            | b"amd64p32"
            | b"arm"
            | b"armbe"
            | b"arm64"
            | b"arm64be"
            | b"ppc64"
            | b"ppc64le"
            | b"loong64"
            | b"mips"
            | b"mipsle"
            | b"mips64"
            | b"mips64le"
            | b"mips64p32"
            | b"mips64p32le"
            | b"ppc"
            | b"riscv"
            | b"riscv64"
            | b"s390"
            | b"s390x"
            | b"sparc"
            | b"sparc64"
            | b"wasm"
    )
}

/// `normalizeOS`, for an OS that is not empty.
fn normalize_os(os: &[u8]) -> Vec<u8> {
    let os = go::to_lower(os);
    if os == b"macos" { b"darwin".to_vec() } else { os }
}

/// `normalizeArch`.
fn normalize_arch(arch: &[u8], variant: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let arch = go::to_lower(arch);
    let variant = go::to_lower(variant);
    match arch.as_slice() {
        b"i386" => (b"386".to_vec(), Vec::new()),
        b"x86_64" | b"x86-64" | b"amd64" => {
            let variant = if variant == b"v1" { Vec::new() } else { variant };
            (b"amd64".to_vec(), variant)
        }
        b"aarch64" | b"arm64" => {
            let variant = match variant.as_slice() {
                b"8" | b"v8" | b"v8.0" => Vec::new(),
                b"9" | b"9.0" | b"v9.0" => b"v9".to_vec(),
                _ => variant,
            };
            (b"arm64".to_vec(), variant)
        }
        b"armhf" => (b"arm".to_vec(), b"v7".to_vec()),
        b"armel" => (b"arm".to_vec(), b"v6".to_vec()),
        b"arm" => {
            let variant = match variant.as_slice() {
                b"" | b"7" => b"v7".to_vec(),
                b"5" | b"6" | b"8" => [b"v".as_slice(), &variant].concat(),
                _ => variant,
            };
            (arch, variant)
        }
        _ => (arch, variant),
    }
}

/// `platforms.Normalize`.
pub fn normalize(p: &Platform) -> Platform {
    let os = if p.os.is_empty() {
        // containerd takes runtime.GOOS; BuildKit never normalizes a platform without one.
        Vec::new()
    } else {
        normalize_os(&p.os)
    };
    let (architecture, variant) = normalize_arch(&p.architecture, &p.variant);
    let mut os_features = p.os_features.clone();
    os_features.sort();
    os_features.dedup();
    Platform {
        os,
        architecture,
        variant,
        os_version: p.os_version.clone(),
        os_features,
    }
}

fn option_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'%' | b'-')
}

/// `osRe`'s submatches: the OS, its version, and its `+`-led features.
fn match_os(part: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let name_end = part
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))
        .unwrap_or(part.len());
    if name_end == 0 {
        return None;
    }
    let name = go::head(part, name_end);
    let rest = go::tail(part, name_end);
    if rest.is_empty() {
        return Some((name, b"", b""));
    }
    let inner = rest.strip_prefix(b"(")?.strip_suffix(b")")?;
    let version_end = inner.iter().position(|&c| c == b'+').unwrap_or(inner.len());
    let version = go::head(inner, version_end);
    let features = go::tail(inner, version_end);
    if !version.iter().all(|&c| option_char(c)) {
        return None;
    }
    // Each feature: `+` and at least one option character.
    if !features.is_empty() {
        let mut parts = features.split(|&c| c == b'+');
        parts.next();
        for f in parts {
            if f.is_empty() || !f.iter().all(|&c| option_char(c)) {
                return None;
            }
        }
    }
    Some((name, version, features))
}

/// `url.PathUnescape`.
fn path_unescape(s: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut at = 0;
    while let Some(&c) = s.get(at) {
        if c == b'%' {
            let hex = |i: usize| s.get(i).and_then(|&d| char::from(d).to_digit(16));
            match (hex(at + 1), hex(at + 2)) {
                (Some(h), Some(l)) => {
                    out.push(u8::try_from(h * 16 + l).unwrap_or_default());
                    at += 3;
                }
                _ => {
                    let bad = go::span(s, at, (at + 3).min(s.len()));
                    return Err([b"invalid URL escape ".as_slice(), go::quote(bad).as_bytes()].concat());
                }
            }
        } else {
            out.push(c);
            at += 1;
        }
    }
    Ok(out)
}

/// `decodeOSOption`.
fn decode_option(v: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    if v.contains(&b'%') {
        path_unescape(v)
    } else {
        Ok(v.to_vec())
    }
}

/// `encodeOSOption`.
fn encode_option(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len());
    for &c in v {
        match c {
            b'%' => out.extend_from_slice(b"%25"),
            b'+' => out.extend_from_slice(b"%2B"),
            b'(' => out.extend_from_slice(b"%28"),
            b')' => out.extend_from_slice(b"%29"),
            b'/' => out.extend_from_slice(b"%2F"),
            _ => out.push(c),
        }
    }
    out
}

fn wrap(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = parts.concat();
    out.extend_from_slice(b": ");
    out.extend_from_slice(INVALID);
    out
}

/// `platforms.Parse`, a specifier of an OS or an architecture alone completed from `build`.
pub fn parse(specifier: &[u8], build: &Platform) -> Result<Platform, Vec<u8>> {
    let q = |b: &[u8]| go::quote(b).into_bytes();
    if specifier.contains(&b'*') {
        return Err(wrap(&[&q(specifier), b": wildcards not yet supported"]));
    }
    // strings.SplitN(specifier, "/", 4)
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut rest = specifier;
    while parts.len() < 3 {
        match rest.iter().position(|&c| c == b'/') {
            Some(at) => {
                parts.push(go::head(rest, at));
                rest = go::tail(rest, at + 1);
            }
            None => break,
        }
    }
    parts.push(rest);

    let mut p = Platform::default();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some((os, version, features)) = match_os(part) else {
                return Err(wrap(&[
                    &q(part),
                    b" is an invalid OS component of ",
                    &q(specifier),
                    b": OSAndVersion specifier component must match ",
                    &q(OS_RE.as_bytes()),
                ]));
            };
            p.os = normalize_os(os);
            p.os_version = decode_option(version).map_err(|e| {
                [
                    &q(specifier),
                    b" has an invalid OS version ".as_slice(),
                    &q(version),
                    b": ",
                    &e,
                ]
                .concat()
            })?;
            if let Some(features) = features.strip_prefix(b"+") {
                for raw in features.split(|&c| c == b'+') {
                    let raw = go::trim_space(raw);
                    let feature = decode_option(raw).map_err(|e| {
                        [
                            &q(specifier),
                            b" has invalid OS features: invalid os feature ".as_slice(),
                            &q(raw),
                            b": ",
                            &e,
                        ]
                        .concat()
                    })?;
                    if !feature.is_empty() {
                        p.os_features.push(feature);
                    }
                }
            }
        } else if part.is_empty()
            || !part
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
        {
            return Err(wrap(&[
                &q(part),
                b" is an invalid component of ",
                &q(specifier),
                b": platform specifier component must match ",
                &q(SPECIFIER_RE.as_bytes()),
            ]));
        }
    }

    match parts.as_slice() {
        [only] => {
            if is_known_os(&p.os) {
                p.architecture = build.architecture.clone();
                p.variant = build.variant.clone();
                return Ok(p);
            }
            (p.architecture, p.variant) = normalize_arch(only, b"");
            if p.architecture == b"arm" && p.variant == b"v7" {
                p.variant.clear();
            }
            if is_known_arch(&p.architecture) {
                p.os = build.os.clone();
                return Ok(p);
            }
            Err(wrap(&[
                &q(specifier),
                b": unknown operating system or architecture",
            ]))
        }
        [_, arch] => {
            (p.architecture, p.variant) = normalize_arch(arch, b"");
            if p.architecture == b"arm" && p.variant == b"v7" {
                p.variant.clear();
            }
            Ok(p)
        }
        [_, arch, variant] => {
            (p.architecture, p.variant) = normalize_arch(arch, variant);
            if p.architecture == b"arm64" && p.variant.is_empty() {
                p.variant = b"v8".to_vec();
            }
            Ok(p)
        }
        _ => Err(wrap(&[&q(specifier), b": cannot parse platform specifier"])),
    }
}

/// `platforms.Format`.
pub fn format(p: &Platform) -> Vec<u8> {
    if p.os.is_empty() {
        return b"unknown".to_vec();
    }
    go::join(&[&p.os, &p.architecture, &p.variant])
}

/// `platforms.FormatAll`: with the OS version and features.
pub fn format_all(p: &Platform) -> Vec<u8> {
    if p.os.is_empty() {
        return b"unknown".to_vec();
    }
    if p.os_version.is_empty() && p.os_features.is_empty() {
        return go::join(&[&p.os, &p.architecture, &p.variant]);
    }
    let mut os = p.os.clone();
    let version = encode_option(&p.os_version);
    let mut features = p.os_features.clone();
    features.sort();
    let mut formatted: Vec<u8> = Vec::new();
    let mut prev: Option<&[u8]> = None;
    for f in &features {
        if f.is_empty() || prev == Some(f.as_slice()) {
            continue;
        }
        prev = Some(f);
        if !formatted.is_empty() {
            formatted.push(b'+');
        }
        formatted.extend_from_slice(&encode_option(f));
    }
    if !version.is_empty() || !formatted.is_empty() {
        os.push(b'(');
        os.extend_from_slice(&version);
        if !formatted.is_empty() {
            os.push(b'+');
            os.extend_from_slice(&formatted);
        }
        os.push(b')');
    }
    go::join(&[&os, &p.architecture, &p.variant])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Specifiers parse, normalize and format as containerd's do.
    #[test]
    fn specifiers_parse_as_containerds() {
        let build = Platform::new("linux", "arm64");
        let p = |s: &str| parse(s.as_bytes(), &build).map(|p| String::from_utf8(format_all(&p)).unwrap());
        assert_eq!(p("linux/amd64").unwrap(), "linux/amd64");
        assert_eq!(p("Linux/x86_64").unwrap(), "linux/amd64");
        assert_eq!(p("linux/arm64/v8").unwrap(), "linux/arm64/v8");
        assert_eq!(p("linux/arm/7").unwrap(), "linux/arm/v7");
        assert_eq!(p("linux/arm").unwrap(), "linux/arm");
        assert_eq!(
            p("linux/armhf").unwrap(),
            "linux/arm",
            "v7 is dropped for two parts"
        );
        assert_eq!(p("linux").unwrap(), "linux/arm64");
        assert_eq!(p("amd64").unwrap(), "linux/amd64");
        assert_eq!(
            p("windows(10.0.17763+win32k+a%2Bb)/amd64").unwrap(),
            "windows(10.0.17763+a%2Bb+win32k)/amd64"
        );
        assert_eq!(
            String::from_utf8(p("linux/*").unwrap_err()).unwrap(),
            "\"linux/*\": wildcards not yet supported: invalid argument"
        );
        assert_eq!(
            String::from_utf8(p("nope").unwrap_err()).unwrap(),
            "\"nope\": unknown operating system or architecture: invalid argument"
        );
        assert!(p("linux/a b").is_err());
        assert!(p("linux/amd64/v1/x").is_err());
        assert!(p("lin(ux/amd64").is_err());
        assert_eq!(
            String::from_utf8(p("linux(%zz)/amd64").unwrap_err()).unwrap(),
            "\"linux(%zz)/amd64\" has an invalid OS version \"%zz\": invalid URL escape \"%zz\""
        );
    }
}
