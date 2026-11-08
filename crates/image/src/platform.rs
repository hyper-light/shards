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
/// - amd64: the x86-64 levels of the host's CPU, the highest first, then the level-less
///   `amd64` (v1's, every amd64 CPU's), then `386`, which our x86_64 kernel runs. A guest's
///   CPU is the host's as its hypervisor gives it: KVM's supported CPUID and a WHP
///   partition keep the host's feature bits (vmm cpuid.rs rewrites topology alone), so it
///   runs the host's level, which tests/images.rs `a_guest_runs_its_hosts_x86_64_level`
///   pins on every x86_64 host CI boots guests on.
pub fn guest() -> Vec<Target> {
    match std::env::consts::ARCH {
        "aarch64" => vec![Target::new("linux", "arm64", "")],
        "x86_64" => {
            let mut levels: Vec<Target> = (2..=amd64_level())
                .rev()
                .map(|l| Target::new("linux", "amd64", &format!("v{l}")))
                .collect();
            levels.push(Target::new("linux", "amd64", ""));
            levels.push(Target::new("linux", "386", ""));
            levels
        }
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

/// What a pull that names no platform takes of an index, best first: our guests' own
/// platforms, then the other 64-bit CPU's and 32-bit Arm's, which a guest runs under their
/// emulators (D179), as it runs a lone manifest of either. The 64-bit before the 32-bit: a
/// 32-bit process addresses 4 GiB at most, where a microVM's memory can be the host's
/// (D148), and on arm64 the two cost alike emulated (PM M250). The emulator runs amd64's
/// v2 and v3 (PM M251), taken after its level-less build, and none of v4's AVX-512. So on
/// arm64: arm64, amd64, its v2 and v3, then 32-bit Arm's v8 to v5; on x86_64: the CPU's
/// amd64 levels and 386, arm64, then 32-bit Arm's.
pub fn pullable() -> Vec<Target> {
    let mut out = guest();
    match std::env::consts::ARCH {
        "aarch64" => out.extend(["", "v2", "v3"].map(|v| Target::new("linux", "amd64", v))),
        "x86_64" => out.push(Target::new("linux", "arm64", "")),
        _ => return out,
    }
    out.extend(["v8", "v7", "v6", "v5"].map(|v| Target::new("linux", "arm", v)));
    out
}

/// The highest x86-64 level the emulator runs (PM M251: v3's instructions ran under it,
/// v4's AVX-512 did not).
pub const EMULATED_AMD64_LEVEL: u32 = 3;

/// The x86-64 level a platform's variant names, `v2` to `v4`; 1 for one of no level.
pub fn amd64_variant_level(variant: &str) -> u32 {
    variant
        .strip_prefix('v')
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

/// Whether the emulator runs programs built for `t`: every platform's but amd64's levels
/// past its own (PM M251).
pub fn emulator_runs(t: &Target) -> bool {
    t.architecture != "amd64" || amd64_variant_level(&t.variant) <= EMULATED_AMD64_LEVEL
}

/// The platform a run that names none is told is this host's, where its image runs
/// emulated (run.rs), in the warning's own words: `linux/arm64/v8`, and on amd64 the CPU's
/// x86-64 level as go-archvariant v1.0.0 names it (AMD64Variant).
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

    /// A pull that names no platform takes, of an index with none of this host's own, the
    /// other 64-bit CPU's before 32-bit Arm's (PM M250: on arm64 they cost alike emulated,
    /// and a 32-bit process addresses 4 GiB at most), then 32-bit Arm's best variant; and
    /// none of a CPU it has no reason to emulate unasked.
    #[test]
    fn a_pull_of_no_platform_takes_the_other_64_bit_cpus_before_32_bit_arms() {
        let chosen = |manifests: Vec<Descriptor>| {
            select(&index(manifests), &pullable()).map(|d| {
                let p = d.platform.clone().unwrap();
                (p.architecture, p.variant)
            })
        };
        let arch = |a: &str| (a.to_string(), None);
        let arm = |v: &str| ("arm".to_string(), Some(v.to_string()));
        let both = || {
            vec![
                desc("amd64", None, 1),
                desc("arm", Some("v6"), 2),
                desc("arm", Some("v7"), 3),
            ]
        };
        assert_eq!(chosen(both()), Some(arch("amd64")));
        assert_eq!(
            chosen(vec![desc("arm", Some("v6"), 1), desc("arm", Some("v7"), 2)]),
            Some(arm("v7"))
        );
        assert_eq!(chosen(vec![desc("s390x", None, 1)]), None);
        if cfg!(target_arch = "aarch64") {
            assert_eq!(chosen(vec![desc("amd64", None, 1)]), Some(arch("amd64")));
            // The emulator's levels (PM M251): a v3 build before 32-bit Arm's, the level-less
            // one before a level's, and none of v4's.
            let level = |v: &str| ("amd64".to_string(), Some(v.to_string()));
            assert_eq!(
                chosen(vec![desc("arm", Some("v7"), 1), desc("amd64", Some("v3"), 2)]),
                Some(level("v3"))
            );
            assert_eq!(
                chosen(vec![desc("amd64", Some("v3"), 1), desc("amd64", None, 2)]),
                Some(arch("amd64"))
            );
            assert_eq!(chosen(vec![desc("amd64", Some("v4"), 1)]), None);
        }
        if cfg!(target_arch = "x86_64") {
            assert_eq!(
                chosen(vec![desc("arm", Some("v7"), 1), desc("arm64", None, 2)]),
                Some(arch("arm64"))
            );
        }
    }

    /// On x86_64 a guest runs its CPU's x86-64 levels, the highest first, then the
    /// level-less amd64 and 386: of an index with amd64's v1 and v2 builds, v2's is taken
    /// where the CPU has it.
    #[test]
    fn a_guest_runs_its_cpus_x86_64_levels_best_first() {
        if !cfg!(target_arch = "x86_64") {
            return;
        }
        let level = amd64_level();
        let mut want: Vec<Target> = (2..=level)
            .rev()
            .map(|l| Target::new("linux", "amd64", &format!("v{l}")))
            .collect();
        want.extend([Target::new("linux", "amd64", ""), Target::new("linux", "386", "")]);
        assert_eq!(guest(), want);
        let both = index(vec![desc("amd64", None, 1), desc("amd64", Some("v2"), 2)]);
        let chosen = select(&both, &pullable()).unwrap();
        let variant = chosen.platform.as_ref().unwrap().variant.clone();
        assert_eq!(variant.as_deref(), (level >= 2).then_some("v2"));
        assert_eq!(amd64_variant_level("v4"), 4);
        assert_eq!(amd64_variant_level(""), 1);
    }

    /// The emulator runs every platform's programs but amd64's v4, whose AVX-512 it has
    /// none of (PM M251).
    #[test]
    fn the_emulator_runs_amd64_up_to_its_own_level() {
        for (arch, variant, runs) in [
            ("amd64", "", true),
            ("amd64", "v2", true),
            ("amd64", "v3", true),
            ("amd64", "v4", false),
            ("arm", "v7", true),
            ("arm64", "", true),
        ] {
            assert_eq!(
                emulator_runs(&Target::new("linux", arch, variant)),
                runs,
                "{arch} {variant}"
            );
        }
    }

    /// The platform the warning names as this host's: arm64's `v8`, or amd64's x86-64 level.
    #[test]
    fn the_warning_names_this_hosts_platform() {
        if cfg!(target_arch = "aarch64") {
            assert_eq!(maximum(), Target::new("linux", "arm64", "v8"));
        }
        if cfg!(target_arch = "x86_64") {
            let level = maximum().variant;
            assert!(matches!(level.as_str(), "v1" | "v2" | "v3" | "v4"), "{level}");
        }
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
