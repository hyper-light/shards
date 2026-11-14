//! A guest's CPU as its ID registers describe it, field by field (D176 part 6): what a
//! snapshot taken on one host needs of another to resume there, and what a CPU baseline
//! common to several hosts is.
//!
//! Each AArch64 ID register is made of 4-bit fields, each a feature's level: unsigned where
//! a higher value is more of it, signed where 0xF says the feature is absent (FP,
//! AdvSIMD, TGran4, TGran64, PMUVer), as the Arm ARM's ID scheme gives them (DDI 0487,
//! D19.1.3, "Principles of the ID scheme for fields in ID registers") and as Linux reads
//! them across a system's CPUs into its own sanitised view (arch/arm64/kernel/cpufeature.c,
//! `S_ARM64_FTR_BITS` and `ARM64_FTR_BITS`, each field's lower value the safe one). A
//! guest's kernel and programs use a feature only where a field says so: a host whose
//! every field is at least the guest's runs everything the guest found, and a guest told
//! the lower of two hosts' fields runs on either.

/// MIDR_EL1: which CPU it is, no feature's level: a guest keeps the one it booted on.
pub const MIDR_EL1: u16 = 0xc000;

/// The fields read as signed, by register and shift: 0xF is the feature's absence.
const SIGNED: &[(u16, u32)] = &[
    (0xc020, 16), // ID_AA64PFR0_EL1.FP
    (0xc020, 20), // ID_AA64PFR0_EL1.AdvSIMD
    (0xc028, 8),  // ID_AA64DFR0_EL1.PMUVer
    (0xc038, 24), // ID_AA64MMFR0_EL1.TGran64
    (0xc038, 28), // ID_AA64MMFR0_EL1.TGran4
];

/// A register's name, for what a refusal says.
pub fn name(reg: u16) -> String {
    let known = match reg {
        0xc000 => "MIDR_EL1",
        0xc020 => "ID_AA64PFR0_EL1",
        0xc021 => "ID_AA64PFR1_EL1",
        0xc022 => "ID_AA64PFR2_EL1",
        0xc024 => "ID_AA64ZFR0_EL1",
        0xc025 => "ID_AA64SMFR0_EL1",
        0xc028 => "ID_AA64DFR0_EL1",
        0xc029 => "ID_AA64DFR1_EL1",
        0xc030 => "ID_AA64ISAR0_EL1",
        0xc031 => "ID_AA64ISAR1_EL1",
        0xc032 => "ID_AA64ISAR2_EL1",
        0xc033 => "ID_AA64ISAR3_EL1",
        0xc038 => "ID_AA64MMFR0_EL1",
        0xc039 => "ID_AA64MMFR1_EL1",
        0xc03a => "ID_AA64MMFR2_EL1",
        0xc03b => "ID_AA64MMFR3_EL1",
        0xc03c => "ID_AA64MMFR4_EL1",
        _ => return format!("register {reg:#06x}"),
    };
    known.to_string()
}

/// Field `shift` of `value`, as a number to compare: a signed field's 0xF below 0.
fn level(reg: u16, shift: u32, value: u64) -> i8 {
    let field = ((value >> shift) & 0xf) as u8;
    if SIGNED.contains(&(reg, shift)) {
        // Sign-extended from 4 bits: 0x8..=0xF are below 0.
        ((field << 4) as i8) >> 4
    } else {
        field as i8
    }
}

/// A field of an ID register a host has less of than its guest found: the register by its
/// encoding, the field's lowest bit, and the two levels as the ID scheme reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short {
    pub reg: u16,
    pub shift: u32,
    pub guest: i8,
    pub host: i8,
}

impl std::fmt::Display for Short {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}[{}:{}] {} here {}",
            name(self.reg),
            self.shift.saturating_add(3),
            self.shift,
            self.guest,
            self.host
        )
    }
}

/// Why a host does not run a guest ([`covers`]): each field it has less of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lacks(pub Vec<Short>);

impl std::fmt::Display for Lacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields: Vec<String> = self.0.iter().map(Short::to_string).collect();
        write!(
            f,
            "this CPU has less of what its guest uses: {} (the guest's level, then this host's)",
            fields.join(", ")
        )
    }
}

/// Whether a host whose ID registers are `host` runs a guest that found `guest`: every
/// field of every register both name at least the guest's level, MIDR_EL1 aside. Why not,
/// each field short, where it does not.
pub fn covers(host: &[(u16, u64)], guest: &[(u16, u64)]) -> Result<(), Lacks> {
    let mut short = Vec::new();
    for &(reg, want) in guest {
        if reg == MIDR_EL1 {
            continue;
        }
        let Some(&(_, have)) = host.iter().find(|(r, _)| *r == reg) else {
            continue;
        };
        for shift in (0..64).step_by(4) {
            let (g, h) = (level(reg, shift, want), level(reg, shift, have));
            if g > h {
                short.push(Short {
                    reg,
                    shift,
                    guest: g,
                    host: h,
                });
            }
        }
    }
    if short.is_empty() {
        Ok(())
    } else {
        Err(Lacks(short))
    }
}

/// `regs` as text: `REG=VALUE` by encoding, in hex, comma-separated, as a home keeps a CPU
/// baseline and a VM process is given one (D176 part 6).
pub fn encode(regs: &[(u16, u64)]) -> String {
    regs.iter()
        .map(|(r, v)| format!("{r:04x}={v:016x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// [`encode`]'s text read back; empty for none.
pub fn decode(text: &str) -> Result<Vec<(u16, u64)>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let bad = || format!("{pair:?} is no ID register's REG=VALUE, in hex");
            let (r, v) = pair.split_once('=').ok_or_else(bad)?;
            Ok((
                u16::from_str_radix(r, 16).map_err(|_| bad())?,
                u64::from_str_radix(v, 16).map_err(|_| bad())?,
            ))
        })
        .collect()
}

/// The CPU every one of `cpus` runs ([`meet`] of them all), the first's MIDR_EL1; none of
/// none.
pub fn baseline(cpus: &[Vec<(u16, u64)>]) -> Vec<(u16, u64)> {
    let mut all = cpus.iter();
    let Some(first) = all.next() else {
        return Vec::new();
    };
    all.fold(first.clone(), |acc, cpu| meet(&acc, cpu))
}

/// The CPU both `a` and `b` run: each field the lower of the two, by registers both name;
/// `a`'s MIDR_EL1, and its registers `b` does not name.
pub fn meet(a: &[(u16, u64)], b: &[(u16, u64)]) -> Vec<(u16, u64)> {
    a.iter()
        .map(|&(reg, va)| {
            let Some(&(_, vb)) = b.iter().find(|(r, _)| *r == reg) else {
                return (reg, va);
            };
            if reg == MIDR_EL1 {
                return (reg, va);
            }
            let mut out = 0u64;
            for shift in (0..64).step_by(4) {
                let pick = if level(reg, shift, vb) < level(reg, shift, va) {
                    vb
                } else {
                    va
                };
                out |= pick & (0xf << shift);
            }
            (reg, out)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An M5 Max's ISAR2 and an M4 Max's differ by CSSC and HBC, which the M5 has
    /// (sysctl hw.optional.arm.FEAT_CSSC, FEAT_HBC): a guest of the M4 runs on the M5, one
    /// of the M5 not on the M4, and their meet runs on both.
    #[test]
    fn a_host_runs_a_guest_whose_every_field_it_has() {
        let isar2 = 0xc032;
        // CSSC [55:52] and HBC... as levels 1; the rest alike.
        let m5 = [
            (MIDR_EL1, 0x612f_0390),
            (isar2, 0x0010_0000_0000_1000 | (1 << 52)),
        ];
        let m4 = [(MIDR_EL1, 0x612f_0280), (isar2, 0x0000_0000_0000_1000)];
        assert!(covers(&m5, &m4).is_ok());
        let refused = covers(&m4, &m5).unwrap_err();
        assert_eq!(
            refused.0,
            [Short {
                reg: isar2,
                shift: 52,
                guest: 1,
                host: 0
            }]
        );
        assert!(
            refused.to_string().contains("ID_AA64ISAR2_EL1[55:52] 1 here 0"),
            "{refused}"
        );
        let both = meet(&m5, &m4);
        assert!(covers(&m5, &both).is_ok() && covers(&m4, &both).is_ok());
        assert_eq!(both[0], (MIDR_EL1, 0x612f_0390), "the first's own CPU");
        // A signed field's 0xF is its absence: no FP is less than FP.
        let pfr0 = 0xc020;
        let no_fp = [(pfr0, 0xf << 16)];
        let fp = [(pfr0, 0x1 << 16)];
        assert!(covers(&fp, &no_fp).is_ok());
        assert!(covers(&no_fp, &fp).is_err());
        assert_eq!(meet(&fp, &no_fp), no_fp.to_vec());
        // A register one does not name is no refusal.
        assert!(covers(&m4, &[(0xc03c, 0xffff)]).is_ok());
    }

    /// A fleet's baseline is each field's lowest across its machines: a guest told it finds
    /// nothing any of them lacks, and every one of them runs it. Two Studios as each one's
    /// own lacks shows (PM M462): one with SSBS (ID_AA64PFR1_EL1[7:4] 2) and no FPAC
    /// combined (ISAR1's API 4), the other the other way (API 5).
    #[test]
    fn a_baseline_masks_what_any_machine_lacks() {
        let (pfr1, isar1) = (0xc021, 0xc031);
        let m3 = vec![(MIDR_EL1, 0x612f_0320), (pfr1, 0x20), (isar1, 0x4 << 8)];
        let m4 = vec![(MIDR_EL1, 0x612f_0280), (pfr1, 0x0), (isar1, 0x5 << 8)];
        assert!(covers(&m4, &m3).is_err(), "SSBS");
        assert!(covers(&m3, &m4).is_err(), "FPACCOMBINE");
        let base = baseline(&[m3.clone(), m4.clone()]);
        assert_eq!(
            base,
            vec![(MIDR_EL1, 0x612f_0320), (pfr1, 0x0), (isar1, 0x4 << 8)]
        );
        assert!(covers(&m3, &base).is_ok() && covers(&m4, &base).is_ok());
        // Told on either host, the lower of the host's and the baseline's: the baseline.
        assert_eq!(meet(&m3, &base)[1..], base[1..]);
        assert_eq!(meet(&m4, &base)[1..], base[1..]);
        assert_eq!(baseline(&[]), Vec::new());
        assert_eq!(baseline(std::slice::from_ref(&m4)), m4);
        // As a home keeps it and a VM is given it.
        assert_eq!(decode(&encode(&base)).unwrap(), base);
        assert_eq!(decode("").unwrap(), Vec::new());
        assert!(decode("c021").is_err());
        assert!(decode("zz=1").is_err());
    }
}

/// The ID register containing an OS-named feature. Baseline readiness may ignore the
/// name only when the guest baseline names that register; unknown names stay checked.
pub fn feature_register(feature: &str) -> Option<u16> {
    Some(match feature {
        "FEAT_CSV2" | "FEAT_CSV3" | "FEAT_DIT" | "FEAT_FP16" => 0xc020,
        "FEAT_SSBS" | "FEAT_BTI" => 0xc021,
        "FEAT_CRC32" | "FEAT_FlagM" | "FEAT_FlagM2" | "FEAT_FHM" | "FEAT_DotProd" | "FEAT_SHA3"
        | "FEAT_RDM" | "FEAT_LSE" | "FEAT_SHA256" | "FEAT_SHA512" | "FEAT_SHA1" | "FEAT_AES"
        | "FEAT_PMULL" => 0xc030,
        "FEAT_SPECRES" | "FEAT_SPECRES2" | "FEAT_SB" | "FEAT_FRINTTS" | "FEAT_PACIMP" | "FEAT_LRCPC"
        | "FEAT_LRCPC2" | "FEAT_FCMA" | "FEAT_JSCVT" | "FEAT_PAuth" | "FEAT_PAuth2" | "FEAT_FPAC"
        | "FEAT_FPACCOMBINE" | "FEAT_DPB" | "FEAT_DPB2" | "FEAT_BF16" | "FEAT_EBF16" | "FEAT_I8MM" => 0xc031,
        "FEAT_WFxT" | "FEAT_RPRES" | "FEAT_CSSC" | "FEAT_HBC" => 0xc032,
        "FEAT_ECV" => 0xc038,
        "FEAT_AFP" => 0xc039,
        "FEAT_LSE2" => 0xc03a,
        _ => return None,
    })
}


#[cfg(test)]
mod feature_name_tests {
    #[test]
    fn linux_arm64_feature_names_follow_their_baseline_registers() {
        for name in ["aes", "pmull", "sha1", "sha2", "crc32", "atomics"] {
            assert_eq!(super::feature_register(name), Some(0xc030), "{name}");
        }
        assert_eq!(super::feature_register("FEAT_AES"), Some(0xc030));
        assert_eq!(super::feature_register("unknown_future_feature"), None);
    }
}
