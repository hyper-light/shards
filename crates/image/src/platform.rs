//! Choosing an index's manifest for our guests, as containerd/platforms v1.0.0-rc.5
//! normalizes and orders platforms (docs/research/registry-pull.md §4, R6). Guests run
//! Linux on the host's architecture, so the target is always `linux/<guest arch>`.

use crate::oci::{Descriptor, Index, Platform};

/// A platform, normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub os: String,
    pub architecture: String,
    pub variant: String,
}

impl Target {
    fn new(os: &str, architecture: &str, variant: &str) -> Target {
        Target {
            os: os.into(),
            architecture: architecture.into(),
            variant: variant.into(),
        }
    }
}

/// containerd's `Normalize`: lowercased, with the aliases and implied variants folded.
pub fn normalize(p: &Platform) -> Target {
    let os = match p.os.to_lowercase() {
        o if o.is_empty() => "linux".to_string(),
        o if o == "macos" => "darwin".to_string(),
        o => o,
    };
    let arch = p.architecture.to_lowercase();
    let variant = p.variant.as_deref().unwrap_or_default().to_lowercase();
    let (architecture, variant) = match (arch.as_str(), variant.as_str()) {
        ("i386", _) => ("386", ""),
        ("x86_64" | "x86-64" | "amd64", "v1") => ("amd64", ""),
        ("x86_64" | "x86-64" | "amd64", v) => ("amd64", v),
        ("aarch64" | "arm64", "8" | "v8" | "v8.0") => ("arm64", ""),
        ("aarch64" | "arm64", "9" | "9.0" | "v9.0") => ("arm64", "v9"),
        ("aarch64" | "arm64", v) => ("arm64", v),
        ("armhf", _) => ("arm", "v7"),
        ("armel", _) => ("arm", "v6"),
        ("arm", "" | "7") => ("arm", "v7"),
        ("arm", "5") => ("arm", "v5"),
        ("arm", "6") => ("arm", "v6"),
        ("arm", "8") => ("arm", "v8"),
        (a, v) => (a, v),
    };
    Target::new(&os, architecture, variant)
}

/// What a guest on this host runs itself, best first:
/// - arm64: `arm64` alone. Our guests have no AArch32 EL0 (platform-measurements.md): 32-bit
///   Arm's images run under their emulator ([`pullable`], D179).
/// - amd64: `amd64`, then `386`, as containerd orders them. Our x86_64 kernel runs 32-bit
///   code. Newer `amd64/vN` levels wait on a guest CPUID policy.
pub fn guest() -> Vec<Target> {
    match std::env::consts::ARCH {
        "aarch64" => vec![Target::new("linux", "arm64", "")],
        "x86_64" => vec![Target::new("linux", "amd64", ""), Target::new("linux", "386", "")],
        other => vec![normalize(&Platform {
            os: "linux".into(),
            architecture: other.into(),
            ..Platform::default()
        })],
    }
}

/// containerd's `platforms.FormatAll` (v1.0.0-rc.2) of a Linux platform: `os/arch`, then
/// `/variant` where it has one; `unknown` where it names no OS.
pub fn format_all(t: &Target) -> String {
    match (t.os.as_str(), t.variant.as_str()) {
        ("", _) => "unknown".into(),
        (_, "") => format!("{}/{}", t.os, t.architecture),
        _ => format!("{}/{}/{}", t.os, t.architecture, t.variant),
    }
}

/// What a pull that names no platform takes of an index, best first, as dockerd takes it
/// (containerd/platforms v1.0.0-rc.2 compare.go, platformVector): our guests' platforms,
/// then on arm64 32-bit Arm's v8 to v5, which a guest runs under their emulator (D179).
pub fn pullable() -> Vec<Target> {
    let mut out = guest();
    if std::env::consts::ARCH == "aarch64" {
        out.extend(["v8", "v7", "v6", "v5"].map(|v| Target::new("linux", "arm", v)));
    }
    out
}

/// The platform this host's microVMs run, as dockerd names its own where a run's image is
/// another's (moby docker-v29.3.1 daemon/create.go, maximumSpec): containerd's
/// `DefaultSpec`, arm64's variant `v8`, and on amd64 the CPU's x86-64 level
/// (tonistiigi/go-archvariant v1.0.0, AMD64Variant).
pub fn maximum() -> Target {
    match std::env::consts::ARCH {
        "aarch64" => Target::new("linux", "arm64", "v8"),
        "x86_64" => Target::new("linux", "amd64", &format!("v{}", amd64_level())),
        _ => guest()
            .into_iter()
            .next()
            .unwrap_or_else(|| Target::new("linux", "", "")),
    }
}

/// Whether this host runs an image for `p` as dockerd finds one it runs, saying nothing of
/// another's (moby docker-v29.3.1 daemon/images OnlyPlatformWithFallback over
/// [`maximum`]; containerd/platforms v1.0.0-rc.2 compare.go, platformVector): its own
/// architecture's variants no later than its own, 32-bit Arm's v5 to v8 beside arm64,
/// 386 beside amd64, and its own architecture of no variant.
pub fn host_runs(p: &Platform) -> bool {
    let host = maximum();
    if !p.os_features.is_empty() {
        return false;
    }
    let image = normalize(p);
    if image.os != host.os {
        return false;
    }
    let level = |v: &str| v.strip_prefix('v').and_then(|n| n.parse::<u32>().ok());
    match (host.architecture.as_str(), image.architecture.as_str()) {
        // Normalized, arm64/v8 has no variant.
        ("arm64", "arm64") => image.variant.is_empty(),
        ("arm64", "arm") => matches!(image.variant.as_str(), "v5" | "v6" | "v7" | "v8"),
        ("amd64", "amd64") => {
            image.variant.is_empty() || level(&image.variant).is_some_and(|l| Some(l) <= level(&host.variant))
        }
        ("amd64", "386") => true,
        (h, i) => h == i && (image.variant.is_empty() || image.variant == host.variant),
    }
}

/// go-archvariant v1.0.0's detectVersion: the x86-64 microarchitecture level, 1 to 4, of
/// this CPU's features and those its OS keeps the state of (XCR0).
#[cfg(target_arch = "x86_64")]
fn amd64_level() -> u32 {
    use std::arch::x86_64::{__cpuid_count, _xgetbv};
    // CPUID.1:ECX, CPUID.7.0:EBX and CPUID.80000001h:ECX bits, and XCR0's.
    const V2: u32 = 1 << 0 | 1 << 9 | 1 << 13 | 1 << 19 | 1 << 20 | 1 << 23;
    const V3: u32 = V2 | 1 << 12 | 1 << 22 | 1 << 26 | 1 << 27 | 1 << 28 | 1 << 29;
    const V3_BX: u32 = 1 << 3 | 1 << 5 | 1 << 8;
    const V4_BX: u32 = V3_BX | 1 << 16 | 1 << 17 | 1 << 28 | 1 << 30 | 1 << 31;
    const V2_CX: u32 = 1 << 0;
    const V3_CX: u32 = V2_CX | 1 << 5;
    const V3_OS: u64 = 1 << 1 | 1 << 2;
    const V4_OS: u64 = V3_OS | 1 << 5 | 1 << 6 | 1 << 7;
    let cpuid = |leaf: u32| __cpuid_count(leaf, 0);
    if cpuid(0).eax < 7 || cpuid(0x8000_0000).eax < 0x8000_0001 {
        return 1;
    }
    let mut level = 4;
    let cx = cpuid(1).ecx;
    if cx & V3 != V3 {
        level = 2;
        if cx & V2 != V2 {
            return 1;
        }
    }
    let bx = cpuid(7).ebx;
    if level == 4 && bx & V4_BX != V4_BX {
        level = 3;
    }
    if level == 3 && bx & V3_BX != V3_BX {
        level = 2;
    }
    let cx = cpuid(0x8000_0001).ecx;
    if level >= 3 && cx & V3_CX != V3_CX {
        level = 2;
    }
    if level == 2 && cx & V2_CX != V2_CX {
        return 1;
    }
    if level >= 3 {
        // SAFETY: XGETBV is there where CPUID says OSXSAVE, which level 3 holds.
        let os = unsafe { _xgetbv(0) };
        if level == 4 && os & V4_OS != V4_OS {
            level = 3;
        }
        if os & V3_OS != V3_OS {
            level = 2;
        }
    }
    level
}

/// Elsewhere none: go-archvariant's `v1`.
#[cfg(not(target_arch = "x86_64"))]
fn amd64_level() -> u32 {
    1
}

/// The manifest to use from `index`: the first entry whose platform ranks best in
/// `wanted`, keeping the index's order among equals, and an entry without a platform
/// only when nothing labelled matches. Its config must then be checked instead.
pub fn select<'a>(index: &'a Index, wanted: &[Target]) -> Option<&'a Descriptor> {
    let order = |d: &Descriptor| match &d.platform {
        None => Some(wanted.len()),
        Some(p) => rank(p, wanted),
    };
    index
        .manifests
        .iter()
        .filter_map(|d| order(d).map(|r| (r, d)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, d)| d)
}

/// Whether an image for `p` runs as one of `wanted`.
pub fn runs(p: &Platform, wanted: &[Target]) -> bool {
    rank(p, wanted).is_some()
}

/// Where an image for `p` ranks in `wanted`, if it runs as one of them.
fn rank(p: &Platform, wanted: &[Target]) -> Option<usize> {
    if !p.os_features.is_empty() {
        return None;
    }
    wanted.iter().position(|w| *w == normalize(p))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A pull that names no platform takes what dockerd takes of an index: on arm64, after
    /// its own, 32-bit Arm's best variant, where the index has no arm64 one.
    #[test]
    fn a_pull_of_no_platform_takes_what_dockerd_takes() {
        let index = Index {
            schema_version: 2,
            media_type: None,
            manifests: vec![
                desc("amd64", None, 1),
                desc("arm", Some("v6"), 2),
                desc("arm", Some("v7"), 3),
            ],
        };
        let chosen = select(&index, &pullable()).unwrap();
        if cfg!(target_arch = "aarch64") {
            assert_eq!(chosen.platform.as_ref().unwrap().variant.as_deref(), Some("v7"));
        } else {
            assert_eq!(chosen.platform.as_ref().unwrap().architecture, "amd64");
        }
    }

    /// dockerd's match of an image's platform with its host's (OnlyPlatformWithFallback):
    /// on arm64 its own and 32-bit Arm's v5 to v8, on amd64 its levels up to the CPU's and
    /// 386, and no other CPU's or OS's.
    #[test]
    fn the_host_runs_what_dockerd_finds_it_runs() {
        let p = |a: &str, v: Option<&str>| Platform {
            os: "linux".into(),
            architecture: a.into(),
            variant: v.map(String::from),
            ..Platform::default()
        };
        if cfg!(target_arch = "aarch64") {
            assert_eq!(maximum(), Target::new("linux", "arm64", "v8"));
            for (a, v) in [
                ("arm64", None),
                ("arm64", Some("v8")),
                ("arm", Some("v7")),
                ("arm", None),
            ] {
                assert!(host_runs(&p(a, v)), "{a} {v:?}");
            }
            for (a, v) in [("amd64", None), ("arm64", Some("v9")), ("s390x", None)] {
                assert!(!host_runs(&p(a, v)), "{a} {v:?}");
            }
        }
        if cfg!(target_arch = "x86_64") {
            assert!(maximum().variant.starts_with('v'), "{:?}", maximum());
            for (a, v) in [("amd64", None), ("amd64", Some("v1")), ("386", None)] {
                assert!(host_runs(&p(a, v)), "{a} {v:?}");
            }
            for (a, v) in [("arm64", None), ("amd64", Some("v5")), ("arm", Some("v7"))] {
                assert!(!host_runs(&p(a, v)), "{a} {v:?}");
            }
        }
        let windows = Platform {
            os: "windows".into(),
            ..p(std::env::consts::ARCH, None)
        };
        assert!(!host_runs(&windows));
    }

    fn desc(arch: &str, variant: Option<&str>, n: u8) -> Descriptor {
        Descriptor {
            media_type: String::new(),
            digest: format!(
                "sha256:{}",
                format!("{n:x}").repeat(64).get(..64).unwrap_or_default()
            ),
            size: 1,
            platform: Some(Platform {
                architecture: arch.into(),
                os: "linux".into(),
                variant: variant.map(Into::into),
                os_features: Vec::new(),
            }),
            annotations: Default::default(),
        }
    }

    fn index(manifests: Vec<Descriptor>) -> Index {
        Index {
            schema_version: 2,
            media_type: None,
            manifests,
        }
    }

    #[test]
    fn platform_aliases_normalize() {
        let n = |arch: &str, variant: Option<&str>| {
            let t = normalize(&Platform {
                architecture: arch.into(),
                os: "Linux".into(),
                variant: variant.map(Into::into),
                os_features: Vec::new(),
            });
            (t.os, t.architecture, t.variant)
        };
        let t = |a: &str, v: &str| ("linux".to_string(), a.to_string(), v.to_string());
        assert_eq!(n("x86_64", None), t("amd64", ""));
        assert_eq!(n("amd64", Some("v1")), t("amd64", ""));
        assert_eq!(n("amd64", Some("v3")), t("amd64", "v3"));
        assert_eq!(n("aarch64", Some("v8")), t("arm64", ""));
        assert_eq!(n("arm64", Some("8")), t("arm64", ""));
        assert_eq!(n("arm64", Some("9.0")), t("arm64", "v9"));
        assert_eq!(n("i386", Some("x")), t("386", ""));
        assert_eq!(n("armhf", None), t("arm", "v7"));
        assert_eq!(n("arm", Some("6")), t("arm", "v6"));
    }

    #[test]
    fn the_best_match_wins_and_equals_keep_index_order() {
        let arm64 = vec![Target::new("linux", "arm64", "")];
        let i = index(vec![
            desc("arm", Some("v7"), 1),
            desc("arm64", Some("v8"), 2),
            desc("aarch64", None, 3),
        ]);
        assert_eq!(
            select(&i, &arm64).map(|d| d.digest.clone()),
            Some(i.manifests[1].digest.clone())
        );
        // No 32-bit ARM for arm64 guests.
        assert_eq!(select(&index(vec![desc("arm", Some("v7"), 1)]), &arm64), None);
        let amd64 = vec![Target::new("linux", "amd64", ""), Target::new("linux", "386", "")];
        let i = index(vec![
            desc("386", None, 4),
            desc("amd64", Some("v3"), 5),
            desc("x86_64", None, 6),
        ]);
        assert_eq!(
            select(&i, &amd64).map(|d| d.digest.clone()),
            Some(i.manifests[2].digest.clone())
        );
    }

    #[test]
    fn unlabelled_entries_come_last_and_attestations_never() {
        let arm64 = vec![Target::new("linux", "arm64", "")];
        let mut unlabelled = desc("arm64", None, 7);
        unlabelled.platform = None;
        let mut attestation = desc("unknown", None, 8);
        attestation.platform = Some(Platform {
            architecture: "unknown".into(),
            os: "unknown".into(),
            ..Platform::default()
        });
        let i = index(vec![
            unlabelled.clone(),
            attestation.clone(),
            desc("arm64", None, 9),
        ]);
        assert_eq!(
            select(&i, &arm64).map(|d| d.digest.clone()),
            Some(i.manifests[2].digest.clone())
        );
        let i = index(vec![attestation, unlabelled]);
        assert_eq!(
            select(&i, &arm64).map(|d| d.digest.clone()),
            Some(i.manifests[1].digest.clone())
        );
    }
}
