//! PSCI 1.1 and SMCCC 1.1 firmware interface, as the VMM presents it over HVC.
//!
//! Function IDs, return codes and Linux's probe order follow Arm DEN0022 and DEN0028
//! (docs/research/hvf-arm64-kvm-ground-truth.md §2.2-2.3). This module only decodes
//! calls and answers the stateless ones; power-state changes are applied by the vCPU
//! layer.

pub const PSCI_VERSION: u32 = 0x8400_0000;
pub const CPU_SUSPEND: u32 = 0x8400_0001;
pub const CPU_SUSPEND_64: u32 = 0xc400_0001;
pub const CPU_OFF: u32 = 0x8400_0002;
pub const CPU_ON: u32 = 0x8400_0003;
pub const CPU_ON_64: u32 = 0xc400_0003;
pub const AFFINITY_INFO: u32 = 0x8400_0004;
pub const AFFINITY_INFO_64: u32 = 0xc400_0004;
pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
pub const SYSTEM_OFF: u32 = 0x8400_0008;
pub const SYSTEM_RESET: u32 = 0x8400_0009;
pub const PSCI_FEATURES: u32 = 0x8400_000a;
pub const SMCCC_VERSION: u32 = 0x8000_0000;
pub const SMCCC_ARCH_FEATURES: u32 = 0x8000_0001;

pub const SUCCESS: i64 = 0;
pub const NOT_SUPPORTED: i64 = -1;
pub const INVALID_PARAMETERS: i64 = -2;
pub const ALREADY_ON: i64 = -4;
pub const ON_PENDING: i64 = -5;
pub const INVALID_ADDRESS: i64 = -9;

/// AFFINITY_INFO results.
pub const AFF_ON: i64 = 0;
pub const AFF_OFF: i64 = 1;
pub const AFF_ON_PENDING: i64 = 2;

/// PSCI 1.1: major << 16 | minor.
const VERSION_1_1: i64 = 0x0001_0001;
/// SMCCC 1.1 enables Linux's SMCCC conduit; it then probes TRNG and the vendor-hyp
/// UID, both of which answer NOT_SUPPORTED below.
const SMCCC_1_1: i64 = 0x0001_0001;
/// Bits of a target MPIDR that may be non-zero (Aff3, Aff2, Aff1, Aff0).
pub const MPIDR_AFFINITY_MASK: u64 = 0xff_00ff_ffff;

/// A decoded firmware call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// Answered without touching vCPU state.
    Immediate(i64),
    /// Standby request; treated like WFI and returns SUCCESS (spurious wakeups are
    /// permitted for standby states, DEN0022 §5.4).
    CpuSuspend,
    CpuOff,
    CpuOn {
        target: u64,
        entry: u64,
        context: u64,
    },
    AffinityInfo {
        target: u64,
    },
    SystemOff,
    SystemReset,
}

/// Decodes the call in `x[0]` with arguments `x[1..4]` (SMCCC calling convention).
pub fn decode([x0, x1, x2, x3]: [u64; 4]) -> Call {
    // Only the function ID is in W0; bits above 31 of X0 are not part of it.
    if x0 >> 32 != 0 {
        return Call::Immediate(NOT_SUPPORTED);
    }
    let func = x0 as u32;
    // SMC32/HVC32 calls pass 32-bit arguments; ignore upper halves (KVM does the same).
    let width = |v: u64| {
        if func & 0x4000_0000 == 0 {
            v & 0xffff_ffff
        } else {
            v
        }
    };
    let (a1, a2, a3) = (width(x1), width(x2), width(x3));
    match func {
        PSCI_VERSION => Call::Immediate(VERSION_1_1),
        CPU_SUSPEND | CPU_SUSPEND_64 => Call::CpuSuspend,
        CPU_OFF => Call::CpuOff,
        CPU_ON | CPU_ON_64 => Call::CpuOn {
            target: a1,
            entry: a2,
            context: a3,
        },
        AFFINITY_INFO | AFFINITY_INFO_64 => {
            // From PSCI 1.0, lowest_affinity_level > 0 may be rejected.
            if a2 != 0 {
                Call::Immediate(INVALID_PARAMETERS)
            } else {
                Call::AffinityInfo { target: a1 }
            }
        }
        // 2: no trusted OS; no migration needed. Keeps Linux from pinning a resident CPU.
        MIGRATE_INFO_TYPE => Call::Immediate(2),
        SYSTEM_OFF => Call::SystemOff,
        SYSTEM_RESET => Call::SystemReset,
        PSCI_FEATURES => Call::Immediate(features(a1 as u32)),
        SMCCC_VERSION => Call::Immediate(SMCCC_1_1),
        // Workaround discovery: we cannot vouch for a firmware mitigation, and Linux
        // only asks when the CPU's ID registers don't already report immunity.
        SMCCC_ARCH_FEATURES => Call::Immediate(match a1 as u32 {
            SMCCC_VERSION | SMCCC_ARCH_FEATURES => SUCCESS,
            _ => NOT_SUPPORTED,
        }),
        _ => Call::Immediate(NOT_SUPPORTED),
    }
}

/// PSCI_FEATURES: 0 (no feature flags) for implemented functions.
pub fn features(func: u32) -> i64 {
    match func {
        PSCI_VERSION | CPU_SUSPEND | CPU_SUSPEND_64 | CPU_OFF | CPU_ON | CPU_ON_64 | AFFINITY_INFO
        | AFFINITY_INFO_64 | MIGRATE_INFO_TYPE | SYSTEM_OFF | SYSTEM_RESET | PSCI_FEATURES
        | SMCCC_VERSION => SUCCESS,
        _ => NOT_SUPPORTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_probe_sequence() {
        // psci_probe order (Linux 7.2 drivers/firmware/psci/psci.c:690-717).
        assert_eq!(decode([PSCI_VERSION as u64, 0, 0, 0]), Call::Immediate(0x1_0001));
        assert_eq!(decode([MIGRATE_INFO_TYPE as u64, 0, 0, 0]), Call::Immediate(2));
        assert_eq!(
            decode([PSCI_FEATURES as u64, SMCCC_VERSION as u64, 0, 0]),
            Call::Immediate(SUCCESS)
        );
        assert_eq!(decode([SMCCC_VERSION as u64, 0, 0, 0]), Call::Immediate(0x1_0001));
        assert_eq!(
            decode([PSCI_FEATURES as u64, 0xc400_0001, 0, 0]),
            Call::Immediate(SUCCESS)
        );
        // SYSTEM_SUSPEND, SYSTEM_RESET2, SYSTEM_OFF2 must read as unsupported so Linux
        // keeps plain SYSTEM_RESET/SYSTEM_OFF.
        for f in [0xc400_000eu64, 0xc400_0012, 0xc400_0015] {
            assert_eq!(
                decode([PSCI_FEATURES as u64, f, 0, 0]),
                Call::Immediate(NOT_SUPPORTED)
            );
        }
        assert_eq!(decode([0x8600_ff01, 0, 0, 0]), Call::Immediate(NOT_SUPPORTED)); // vendor hyp UID
        assert_eq!(decode([0x8400_0050, 0, 0, 0]), Call::Immediate(NOT_SUPPORTED)); // TRNG_VERSION
    }

    #[test]
    fn cpu_on_and_argument_width() {
        let x = [CPU_ON_64 as u64, 0x1_0000_0003, 0x8000_1000, 7];
        assert_eq!(
            decode(x),
            Call::CpuOn {
                target: 0x1_0000_0003,
                entry: 0x8000_1000,
                context: 7
            }
        );
        // SMC32 variant truncates arguments to 32 bits.
        let x = [CPU_ON as u64, 0xdead_0000_0003, 0x1_8000_1000, 0x2_0000_0007];
        assert_eq!(
            decode(x),
            Call::CpuOn {
                target: 3,
                entry: 0x8000_1000,
                context: 7
            }
        );
        assert_eq!(
            decode([AFFINITY_INFO_64 as u64, 1, 1, 0]),
            Call::Immediate(INVALID_PARAMETERS)
        );
        assert_eq!(
            decode([AFFINITY_INFO_64 as u64, 1, 0, 0]),
            Call::AffinityInfo { target: 1 }
        );
        assert_eq!(
            decode([1 << 32 | PSCI_VERSION as u64, 0, 0, 0]),
            Call::Immediate(NOT_SUPPORTED)
        );
    }
}
