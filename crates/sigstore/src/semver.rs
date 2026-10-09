//! golang.org/x/mod/semver's IsValid and Compare (semver.go), which sigstore-go compares
//! bundle versions with.

#[derive(Debug, Default)]
struct Parsed<'a> {
    major: &'a str,
    minor: &'a str,
    patch: &'a str,
    prerelease: &'a str,
}

fn int(v: &str) -> Option<(&str, &str)> {
    let b = v.as_bytes();
    if !b.first()?.is_ascii_digit() {
        return None;
    }
    let i = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if b.first() == Some(&b'0') && i != 1 {
        return None;
    }
    Some((v.get(..i)?, v.get(i..)?))
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-'
}

fn bad_num(v: &str) -> bool {
    let b = v.as_bytes();
    let i = b.iter().take_while(|c| c.is_ascii_digit()).count();
    i == b.len() && i > 1 && b.first() == Some(&b'0')
}

fn prerelease(v: &str) -> Option<(&str, &str)> {
    let b = v.as_bytes();
    if b.first() != Some(&b'-') {
        return None;
    }
    let (mut i, mut start) = (1, 1);
    while let Some(&c) = b.get(i) {
        if c == b'+' {
            break;
        }
        if !ident(c) && c != b'.' {
            return None;
        }
        if c == b'.' {
            if start == i || bad_num(v.get(start..i)?) {
                return None;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start == i || bad_num(v.get(start..i)?) {
        return None;
    }
    Some((v.get(..i)?, v.get(i..)?))
}

fn build(v: &str) -> Option<(&str, &str)> {
    let b = v.as_bytes();
    if b.first() != Some(&b'+') {
        return None;
    }
    let (mut i, mut start) = (1, 1);
    while let Some(&c) = b.get(i) {
        if !ident(c) && c != b'.' {
            return None;
        }
        if c == b'.' {
            if start == i {
                return None;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start == i {
        return None;
    }
    Some((v.get(..i)?, v.get(i..)?))
}

fn parse(v: &str) -> Option<Parsed<'_>> {
    let rest = v.strip_prefix('v')?;
    let mut p = Parsed::default();
    let (major, rest) = int(rest)?;
    p.major = major;
    if rest.is_empty() {
        p.minor = "0";
        p.patch = "0";
        return Some(p);
    }
    let (minor, rest) = int(rest.strip_prefix('.')?)?;
    p.minor = minor;
    if rest.is_empty() {
        p.patch = "0";
        return Some(p);
    }
    let (patch, mut rest) = int(rest.strip_prefix('.')?)?;
    p.patch = patch;
    if rest.starts_with('-') {
        let (pre, r) = prerelease(rest)?;
        p.prerelease = pre;
        rest = r;
    }
    if rest.starts_with('+') {
        let (_, r) = build(rest)?;
        rest = r;
    }
    rest.is_empty().then_some(p)
}

/// IsValid.
pub fn is_valid(v: &str) -> bool {
    parse(v).is_some()
}

fn compare_int(x: &str, y: &str) -> std::cmp::Ordering {
    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
}

fn compare_prerelease(x: &str, y: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Greater, Less};
    if x == y {
        return std::cmp::Ordering::Equal;
    }
    if x.is_empty() {
        return Greater;
    }
    if y.is_empty() {
        return Less;
    }
    let (mut x, mut y) = (x, y);
    while !x.is_empty() && !y.is_empty() {
        x = x.get(1..).unwrap_or_default();
        y = y.get(1..).unwrap_or_default();
        let (dx, rx) = x.split_at(x.find('.').unwrap_or(x.len()));
        let (dy, ry) = y.split_at(y.find('.').unwrap_or(y.len()));
        x = rx;
        y = ry;
        if dx != dy {
            let num = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
            let (ix, iy) = (num(dx), num(dy));
            if ix != iy {
                return if ix { Less } else { Greater };
            }
            if ix && dx.len() != dy.len() {
                return dx.len().cmp(&dy.len());
            }
            return dx.cmp(dy);
        }
    }
    if x.is_empty() { Less } else { Greater }
}

/// Compare: invalid versions below valid ones, and equal to each other.
pub fn compare(v: &str, w: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match (parse(v), parse(w)) {
        (None, None) => Equal,
        (None, _) => Less,
        (_, None) => Greater,
        (Some(a), Some(b)) => compare_int(a.major, b.major)
            .then_with(|| compare_int(a.minor, b.minor))
            .then_with(|| compare_int(a.patch, b.patch))
            .then_with(|| compare_prerelease(a.prerelease, b.prerelease)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn versions_compare_as_x_mod_s_do() {
        assert_eq!(compare("v0.3", "v0.3.0"), Equal);
        assert_eq!(compare("v0.10", "v0.9"), Greater);
        assert_eq!(compare("v1.0.0-rc.1", "v1.0.0"), Less);
        assert_eq!(compare("v1.0.0-rc.2", "v1.0.0-rc.10"), Less);
        assert_eq!(compare("bad", "v0.1"), Less);
        assert!(!is_valid("v01"));
        assert!(!is_valid("v0.4+meta"));
        assert!(is_valid("v0.4.0+meta"));
    }
}
