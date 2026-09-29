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

/// What a guest on this host runs, best first:
/// - arm64: `arm64` alone. Our guests have no AArch32 EL0 (platform-measurements.md), so
///   containerd's `arm/v*` fallbacks would pick images they cannot run.
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

/// The manifest to use from `index`: the first entry whose platform ranks best in
/// `wanted`, keeping the index's order among equals, and an entry without a platform
/// only when nothing labelled matches. Its config must then be checked instead.
pub fn select<'a>(index: &'a Index, wanted: &[Target]) -> Option<&'a Descriptor> {
    let rank = |d: &Descriptor| match &d.platform {
        None => Some(wanted.len()),
        Some(p) if !p.os_features.is_empty() => None,
        Some(p) => wanted.iter().position(|w| *w == normalize(p)),
    };
    index
        .manifests
        .iter()
        .filter_map(|d| rank(d).map(|r| (r, d)))
        .min_by_key(|(r, _)| *r)
        .map(|(_, d)| d)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

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
    fn aliases_normalize_as_containerd_normalizes_them() {
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
