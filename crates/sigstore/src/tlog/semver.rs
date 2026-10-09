//! Versions as blang/semver v3.5.1 parses them for rekor's version maps (semver.Parse)
//! and its ranges match them: a range of one bare version is that version, compared
//! without build metadata.

/// A parsed version: major, minor, patch, and whether it has a pre-release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: bool,
}

const ALPHANUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-0123456789";

fn only(s: &str, set: &str) -> bool {
    s.chars().all(|c| set.contains(c))
}

fn leading_zero(s: &str) -> bool {
    s.len() > 1 && s.starts_with('0')
}

/// strconv.ParseUint(s, 10, 64) of digits.
fn parse_uint(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Err(format!(
            "strconv.ParseUint: parsing {}: invalid syntax",
            shards_dockerfile::go::quote(s.as_bytes())
        ));
    }
    s.parse::<u64>().map_err(|_| {
        format!(
            "strconv.ParseUint: parsing {}: value out of range",
            shards_dockerfile::go::quote(s.as_bytes())
        )
    })
}

fn q(s: &str) -> String {
    shards_dockerfile::go::quote(s.as_bytes())
}

/// semver.Parse.
pub fn parse(s: &str) -> Result<Version, String> {
    if s.is_empty() {
        return Err("Version string empty".into());
    }
    let parts: Vec<&str> = s.splitn(3, '.').collect();
    let [major, minor, patch] = parts.as_slice() else {
        return Err("No Major.Minor.Patch elements found".into());
    };
    if !only(major, "0123456789") {
        return Err(format!("Invalid character(s) found in major number {}", q(major)));
    }
    if leading_zero(major) {
        return Err(format!(
            "Major number must not contain leading zeroes {}",
            q(major)
        ));
    }
    let major = parse_uint(major)?;
    if !only(minor, "0123456789") {
        return Err(format!("Invalid character(s) found in minor number {}", q(minor)));
    }
    if leading_zero(minor) {
        return Err(format!(
            "Minor number must not contain leading zeroes {}",
            q(minor)
        ));
    }
    let minor = parse_uint(minor)?;
    let mut patch_str: &str = patch;
    let mut build: Vec<&str> = Vec::new();
    let mut pre: Vec<&str> = Vec::new();
    if let Some((p, b)) = patch_str.split_once('+') {
        build = b.split('.').collect();
        patch_str = p;
    }
    if let Some((p, r)) = patch_str.split_once('-') {
        pre = r.split('.').collect();
        patch_str = p;
    }
    if !only(patch_str, "0123456789") {
        return Err(format!(
            "Invalid character(s) found in patch number {}",
            q(patch_str)
        ));
    }
    if leading_zero(patch_str) {
        return Err(format!(
            "Patch number must not contain leading zeroes {}",
            q(patch_str)
        ));
    }
    let patch = parse_uint(patch_str)?;
    for p in &pre {
        if p.is_empty() {
            return Err("Prerelease is empty".into());
        }
        if only(p, "0123456789") {
            if leading_zero(p) {
                return Err(format!(
                    "Numeric PreRelease version must not contain leading zeroes {}",
                    q(p)
                ));
            }
            parse_uint(p)?;
        } else if !only(p, ALPHANUM) {
            return Err(format!("Invalid character(s) found in prerelease {}", q(p)));
        }
    }
    for b in &build {
        if b.is_empty() {
            return Err("Build meta data is empty".into());
        }
        if !only(b, ALPHANUM) {
            return Err(format!("Invalid character(s) found in build meta data {}", q(b)));
        }
    }
    Ok(Version {
        major,
        minor,
        patch,
        pre: !pre.is_empty(),
    })
}

impl Version {
    /// Whether the range of the bare version `v` (a release, `x.y.z`) holds this one.
    pub fn equals(&self, v: &str) -> bool {
        parse(v).is_ok_and(|o| {
            self.major == o.major && self.minor == o.minor && self.patch == o.patch && !self.pre
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_as_blang_s() {
        assert!(parse("0.0.1+b.2").unwrap().equals("0.0.1"));
        assert!(!parse("0.0.1-rc").unwrap().equals("0.0.1"));
        assert_eq!(parse("1.2").unwrap_err(), "No Major.Minor.Patch elements found");
        assert_eq!(
            parse("01.0.0").unwrap_err(),
            "Major number must not contain leading zeroes \"01\""
        );
        assert_eq!(
            parse("0..1").unwrap_err(),
            "strconv.ParseUint: parsing \"\": invalid syntax"
        );
    }
}
