//! containerd/platforms (as buildx v0.37.1 vendors it): Normalize, the matcher, and
//! `Only`'s ordered comparer, with which policy-helpers picks an index's manifest for a
//! platform. Run as buildx's policy runs it, in the client: an empty OS is the host's
//! (runtime.GOOS), and Windows' version matching applies as on a host that is not
//! Windows, without Windows' own comparer.

/// specs.Platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    pub os_version: String,
    pub os_features: Vec<String>,
    pub variant: String,
}

fn normalize_arch(arch: &str, variant: &str) -> (String, String) {
    let (arch, mut variant) = (arch.to_lowercase(), variant.to_lowercase());
    let arch = match arch.as_str() {
        "i386" => {
            variant.clear();
            "386".to_string()
        }
        "x86_64" | "x86-64" | "amd64" => {
            if variant == "v1" {
                variant.clear();
            }
            "amd64".to_string()
        }
        "aarch64" | "arm64" => {
            match variant.as_str() {
                "8" | "v8" | "v8.0" => variant.clear(),
                "9" | "9.0" | "v9.0" => variant = "v9".into(),
                _ => {}
            }
            "arm64".to_string()
        }
        "armhf" => {
            variant = "v7".into();
            "arm".to_string()
        }
        "armel" => {
            variant = "v6".into();
            "arm".to_string()
        }
        "arm" => {
            match variant.as_str() {
                "" | "7" => variant = "v7".into(),
                "5" | "6" | "8" => variant = format!("v{variant}"),
                _ => {}
            }
            arch
        }
        _ => arch,
    };
    (arch, variant)
}

/// This host's GOOS.
pub fn host_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    }
}

/// Normalize, on this host.
pub fn normalize(p: &Platform) -> Platform {
    normalize_on(p, host_os())
}

/// Normalize, on a host whose GOOS is `goos`.
pub fn normalize_on(p: &Platform, goos: &str) -> Platform {
    let os = if p.os.is_empty() {
        goos.to_string()
    } else {
        match p.os.to_lowercase().as_str() {
            "macos" => "darwin".to_string(),
            o => o.to_string(),
        }
    };
    let (architecture, variant) = normalize_arch(&p.architecture, &p.variant);
    let mut os_features = p.os_features.clone();
    os_features.sort();
    os_features.dedup();
    Platform {
        architecture,
        os,
        os_version: p.os_version.clone(),
        os_features,
        variant,
    }
}

/// A Windows OS version (windowsOSVersion).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WindowsVersion {
    major: u8,
    minor: u8,
    build: u16,
}

fn windows_version(v: &str) -> WindowsVersion {
    if v.matches('.').count() < 2 {
        return WindowsVersion::default();
    }
    let mut parts = v.splitn(4, '.');
    let (Some(major), Some(minor), Some(build)) = (parts.next(), parts.next(), parts.next()) else {
        return WindowsVersion::default();
    };
    let parse = |s: &str, bits: u32| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>().ok().filter(|n| *n < 1u64 << bits)
    };
    match (parse(major, 8), parse(minor, 8), parse(build, 16)) {
        (Some(a), Some(b), Some(c)) => WindowsVersion {
            major: u8::try_from(a).unwrap_or(0),
            minor: u8::try_from(b).unwrap_or(0),
            build: u16::try_from(c).unwrap_or(0),
        },
        _ => WindowsVersion::default(),
    }
}

/// checkWindowsHostAndContainerCompat.
fn windows_compatible(host: WindowsVersion, ctr: WindowsVersion) -> bool {
    const LTSC2022: u16 = 20348;
    const LTSC_RELEASES: [u16; 2] = [LTSC2022, 26100];
    if host.major != ctr.major || host.minor != ctr.minor {
        return false;
    }
    if host.build < LTSC2022 {
        return host.build == ctr.build;
    }
    let mut supported = LTSC2022;
    for i in (0..LTSC_RELEASES.len()).rev() {
        if let Some(&r) = LTSC_RELEASES.get(i)
            && host.build >= r
        {
            supported = if i == 0 {
                r
            } else {
                LTSC_RELEASES.get(i - 1).copied().unwrap_or(r)
            };
            break;
        }
    }
    supported <= ctr.build && ctr.build <= host.build
}

/// NewMatcher's matcher.
#[derive(Debug, Clone)]
struct Matcher {
    platform: Platform,
    /// Windows' version, where the platform's OS is Windows.
    windows: Option<WindowsVersion>,
    goos: &'static str,
}

impl Matcher {
    fn new(p: &Platform, goos: &'static str) -> Matcher {
        Matcher {
            platform: normalize_on(p, goos),
            windows: (p.os == "windows").then(|| windows_version(&p.os_version)),
            goos,
        }
    }

    fn matches(&self, p: &Platform) -> bool {
        let mut p = p.clone();
        if self.windows.is_some()
            && let Some(i) = p.os_features.iter().position(|f| f == "win32k")
        {
            p.os_features.remove(i);
        }
        let n = normalize_on(&p, self.goos);
        let m = &self.platform;
        let version = match self.windows {
            Some(w) if w != WindowsVersion::default() && !p.os_version.is_empty() => {
                windows_compatible(w, windows_version(&p.os_version))
            }
            _ => true,
        };
        if m.os != n.os || m.architecture != n.architecture || m.variant != n.variant || !version {
            return false;
        }
        if n.os_features.is_empty() {
            return true;
        }
        if m.os_features.len() < n.os_features.len() {
            return false;
        }
        let mut j = 0;
        for feature in &n.os_features {
            let mut found = false;
            while let Some(f) = m.os_features.get(j) {
                if feature == f {
                    found = true;
                    j += 1;
                    break;
                }
                if feature < f {
                    return false;
                }
                j += 1;
            }
            if !found {
                return false;
            }
        }
        true
    }
}

/// platformVector.
fn vector(p: &Platform) -> Vec<Platform> {
    let mut out = vec![p.clone()];
    let with = |arch: &str, variant: String| Platform {
        architecture: arch.into(),
        os: p.os.clone(),
        os_version: p.os_version.clone(),
        os_features: p.os_features.clone(),
        variant,
    };
    let version = |v: &str| -> Option<i64> { v.strip_prefix('v').unwrap_or(v).parse::<i64>().ok() };
    match p.architecture.as_str() {
        "amd64" => {
            if let Some(mut v) = version(&p.variant).filter(|v| *v > 1) {
                v -= 1;
                while v >= 1 {
                    out.push(with("amd64", format!("v{v}")));
                    v -= 1;
                }
            }
            out.push(with("386", String::new()));
        }
        "arm" => {
            if let Some(mut v) = version(&p.variant).filter(|v| *v > 5) {
                v -= 1;
                while v >= 5 {
                    out.push(with("arm", format!("v{v}")));
                    v -= 1;
                }
            }
        }
        "arm64" => {
            let variant = if p.variant.is_empty() {
                "v8".to_string()
            } else {
                p.variant.clone()
            };
            out.clear();
            let versions: Option<(&[i64], &[i64])> = match variant.as_str() {
                "v8" | "v8.0" => Some((&[8], &[0])),
                "v8.1" => Some((&[8], &[1])),
                "v8.2" => Some((&[8], &[2])),
                "v8.3" => Some((&[8], &[3])),
                "v8.4" => Some((&[8], &[4])),
                "v8.5" => Some((&[8], &[5])),
                "v8.6" => Some((&[8], &[6])),
                "v8.7" => Some((&[8], &[7])),
                "v8.8" => Some((&[8], &[8])),
                "v8.9" => Some((&[8], &[9])),
                "v9" | "v9.0" => Some((&[9, 8], &[0, 5])),
                "v9.1" => Some((&[9, 8], &[1, 6])),
                "v9.2" => Some((&[9, 8], &[2, 7])),
                "v9.3" => Some((&[9, 8], &[3, 8])),
                "v9.4" => Some((&[9, 8], &[4, 9])),
                "v9.5" => Some((&[9, 8], &[5, 9])),
                "v9.6" => Some((&[9, 8], &[6, 9])),
                "v9.7" => Some((&[9, 8], &[7, 9])),
                _ => None,
            };
            // An unknown arm64 variant matches nothing at all, as Go's breaks out early.
            let Some((majors, minors)) = versions else {
                return out;
            };
            for (major, first) in majors.iter().zip(minors) {
                let mut minor = *first;
                while minor >= 0 {
                    let v = if minor == 0 {
                        format!("v{major}")
                    } else {
                        format!("v{major}.{minor}")
                    };
                    out.push(with("arm64", v));
                    minor -= 1;
                }
            }
            let arm = if variant.starts_with("v8") || variant.starts_with("v9") {
                "v8".to_string()
            } else {
                variant
            };
            out.extend(vector(&with("arm", arm)));
        }
        _ => {}
    }
    out
}

/// Only: the platform and those it runs, best first.
#[derive(Debug, Clone)]
pub struct Only {
    matchers: Vec<Matcher>,
}

impl Only {
    /// Only, on this host.
    pub fn new(p: &Platform) -> Only {
        Only::on(p, host_os())
    }

    /// Only, on a host whose GOOS is `goos`.
    pub fn on(p: &Platform, goos: &'static str) -> Only {
        Only {
            matchers: vector(&normalize_on(p, goos))
                .iter()
                .map(|v| Matcher::new(v, goos))
                .collect(),
        }
    }

    pub fn matches(&self, p: &Platform) -> bool {
        self.matchers.iter().any(|m| m.matches(p))
    }

    /// orderedPlatformComparer.Less.
    pub fn less(&self, a: &Platform, b: &Platform) -> bool {
        for m in &self.matchers {
            let (am, bm) = (m.matches(a), m.matches(b));
            if am && !bm {
                return true;
            }
            if am || bm {
                if am && bm && a.os_features.len() != b.os_features.len() {
                    return a.os_features.len() > b.os_features.len();
                }
                return false;
            }
        }
        if !a.os_features.is_empty() || !b.os_features.is_empty() {
            let strip = |p: &Platform| Platform {
                os_features: Vec::new(),
                ..p.clone()
            };
            return self.less(&strip(a), &strip(b));
        }
        false
    }
}

/// FormatAll.
pub fn format_all(p: &Platform) -> String {
    if p.os.is_empty() {
        return "unknown".into();
    }
    let mut first = p.os.clone();
    if !p.os_version.is_empty() || !p.os_features.is_empty() {
        let osv = encode_os_option(&p.os_version);
        // formatOSFeatures: sorted, empties and repeats left out.
        let mut sorted = p.os_features.clone();
        sorted.sort();
        sorted.dedup();
        let features = sorted
            .iter()
            .filter(|f| !f.is_empty())
            .map(|f| encode_os_option(f))
            .collect::<Vec<_>>()
            .join("+");
        if !osv.is_empty() || !features.is_empty() {
            first.push('(');
            first.push_str(&osv);
            if !features.is_empty() {
                first.push('+');
                first.push_str(&features);
            }
            first.push(')');
        }
    }
    path_join(&[&first, &p.architecture, &p.variant])
}

/// encodeOSOption: `%`, `+`, `(`, `)` and `/` percent-encoded.
fn encode_os_option(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '+' => out.push_str("%2B"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '/' => out.push_str("%2F"),
            c => out.push(c),
        }
    }
    out
}

/// path.Join of the non-empty parts. An architecture or variant holding `/` or `..`
/// would be cleaned by Go's Join; neither comes from a platform BuildKit accepts.
fn path_join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(os: &str, arch: &str, variant: &str) -> Platform {
        Platform {
            os: os.into(),
            architecture: arch.into(),
            variant: variant.into(),
            ..Platform::default()
        }
    }

    #[test]
    fn only_matches_and_orders_as_containerd_s() {
        let only = Only::new(&p("linux", "arm64", ""));
        assert!(only.matches(&p("linux", "arm64", "v8")));
        assert!(only.matches(&p("linux", "arm", "v7")));
        assert!(!only.matches(&p("linux", "amd64", "")));
        assert!(only.less(&p("linux", "arm64", ""), &p("linux", "arm", "v7")));
        let amd = Only::new(&p("linux", "amd64", "v3"));
        assert!(amd.matches(&p("linux", "amd64", "v2")));
        assert!(amd.matches(&p("linux", "386", "")));
        assert!(!amd.matches(&p("linux", "amd64", "v4")));
        assert!(!Only::new(&p("linux", "arm64", "v10")).matches(&p("linux", "arm64", "v10")));
        assert_eq!(format_all(&p("linux", "arm", "v7")), "linux/arm/v7");
    }
}
