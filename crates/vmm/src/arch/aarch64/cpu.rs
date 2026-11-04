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

/// Whether a host whose ID registers are `host` runs a guest that found `guest`: every
/// field of every register both name at least the guest's level, MIDR_EL1 aside. Why not,
/// naming each field short, where it does not.
pub fn covers(host: &[(u16, u64)], guest: &[(u16, u64)]) -> Result<(), String> {
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
                short.push(format!("{}[{}:{}] {g} here {h}", name(reg), shift + 3, shift));
            }
        }
    }
    if short.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "this CPU has less of what its guest uses: {} (the guest's level, then this host's)",
            short.join(", ")
        ))
    }
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
        let m5 = [(MIDR_EL1, 0x612f_0390), (isar2, 0x0010_0000_0000_1000 | (1 << 52))];
        let m4 = [(MIDR_EL1, 0x612f_0280), (isar2, 0x0000_0000_0000_1000)];
        assert!(covers(&m5, &m4).is_ok());
        let refused = covers(&m4, &m5).unwrap_err();
        assert!(refused.contains("ID_AA64ISAR2_EL1[55:52] 1 here 0"), "{refused}");
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
}
