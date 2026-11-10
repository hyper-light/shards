//! Pure test-only interpretation of existing platform::process_usage output.
//! No OS calls or sampling happen here. See FAULT-PROPOSAL.md for native source
//! contracts and the loss of information in the existing shared platform seam.
#![cfg(test)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    LinuxMinfltPlusMajflt,
    MacProcTaskInfoFaults,
    WindowsPageFaultCount,
}

impl Source {
    pub fn this_host() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::LinuxMinfltPlusMajflt
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacProcTaskInfoFaults
        }
        #[cfg(windows)]
        {
            Self::WindowsPageFaultCount
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LinuxMinfltPlusMajflt => "LinuxMinfltPlusMajfltViaPlatformUsage",
            Self::MacProcTaskInfoFaults => "MacProcTaskInfoFaultsViaPlatformUsage",
            Self::WindowsPageFaultCount => "WindowsPageFaultCountViaPlatformUsage",
        }
    }

    pub fn native_bits(self) -> u32 {
        match self {
            Self::LinuxMinfltPlusMajflt => u64::BITS,
            Self::MacProcTaskInfoFaults | Self::WindowsPageFaultCount => u32::BITS,
        }
    }

    pub fn caveat(self) -> &'static str {
        match self {
            Self::LinuxMinfltPlusMajflt => {
                "existing Usage sums minor+major, defaults malformed/missing fields to0, saturates sum; no separate categories or complete process-tree/kernel accounting"
            }
            Self::MacProcTaskInfoFaults => {
                "existing Usage widens signed32 pti_faults and maps negative to0; not Linux minor/major; pageins and COW are different native counters"
            }
            Self::WindowsPageFaultCount => {
                "DWORD32 producer widened by existing Usage; combined native soft/hard events, no split or disk-I/O equivalence"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub start: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unknown {
    MissingIdentity,
    IdentityUnverified,
    ReadUnavailable,
    ProcessExitedOrHandleReleased,
    NativeRange,
    LossyProducerZero,
    SaturatedAggregate,
    SourceMismatch,
    DifferentIdentity,
    CounterDecreased,
    UnorderedPhase,
    PhaseNotReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    ObservedBestEffort(u64),
    Unknown(Unknown),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub source: Source,
    pub identity: Option<Identity>,
    pub observation: Observation,
}

impl Sample {
    /// Only the caller can validate held identity around its actual platform
    /// call. This constructor checks representation, not liveness/quiescence.
    pub fn observed(source: Source, identity: Identity, value: u64) -> Self {
        let observation = if identity.pid == 0 {
            Observation::Unknown(Unknown::MissingIdentity)
        } else {
            match (source, value) {
                (Source::LinuxMinfltPlusMajflt, u64::MAX) => {
                    Observation::Unknown(Unknown::SaturatedAggregate)
                }
                (Source::LinuxMinfltPlusMajflt | Source::MacProcTaskInfoFaults, 0) => {
                    Observation::Unknown(Unknown::LossyProducerZero)
                }
                (Source::MacProcTaskInfoFaults, n) if n > i32::MAX as u64 => {
                    Observation::Unknown(Unknown::NativeRange)
                }
                (Source::WindowsPageFaultCount, n) if n > u64::from(u32::MAX) => {
                    Observation::Unknown(Unknown::NativeRange)
                }
                _ => Observation::ObservedBestEffort(value),
            }
        };
        Self {
            source,
            identity: Some(identity),
            observation,
        }
    }

    pub fn unknown(source: Source, identity: Option<Identity>, reason: Unknown) -> Self {
        Self {
            source,
            identity,
            observation: Observation::Unknown(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delta {
    /// A nondecreasing native pair cannot prove absence of a full wrap/reset
    /// or the shared API's lossy fallback. Never label this an exact count.
    ConditionalDifference(u64),
    Unknown(Unknown),
}

pub fn between(before: Sample, after: Sample, phases_ordered: bool) -> Delta {
    if !phases_ordered {
        return Delta::Unknown(Unknown::UnorderedPhase);
    }
    if before.source != after.source {
        return Delta::Unknown(Unknown::SourceMismatch);
    }
    let (Some(left), Some(right)) = (before.identity, after.identity) else {
        return Delta::Unknown(Unknown::MissingIdentity);
    };
    if left != right {
        return Delta::Unknown(Unknown::DifferentIdentity);
    }
    match (before.observation, after.observation) {
        (Observation::Unknown(reason), _) | (_, Observation::Unknown(reason)) => Delta::Unknown(reason),
        (Observation::ObservedBestEffort(left), Observation::ObservedBestEffort(right)) => {
            match right.checked_sub(left) {
                Some(value) => Delta::ConditionalDifference(value),
                None => Delta::Unknown(Unknown::CounterDecreased),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: Identity = Identity { pid: 12, start: 34 };

    #[test]
    fn native_labels_and_widths_remain_distinct() {
        assert_eq!(Source::LinuxMinfltPlusMajflt.native_bits(), 64);
        assert_eq!(Source::MacProcTaskInfoFaults.native_bits(), 32);
        assert_eq!(Source::WindowsPageFaultCount.native_bits(), 32);
        assert_ne!(
            Source::MacProcTaskInfoFaults.label(),
            Source::LinuxMinfltPlusMajflt.label()
        );
        assert!(!Source::this_host().caveat().is_empty());
    }

    #[test]
    fn native_range_and_lossy_zero_cannot_become_observed_zero() {
        for source in [Source::LinuxMinfltPlusMajflt, Source::MacProcTaskInfoFaults] {
            assert_eq!(
                Sample::observed(source, OWN, 0).observation,
                Observation::Unknown(Unknown::LossyProducerZero)
            );
        }
        assert_eq!(
            Sample::observed(Source::WindowsPageFaultCount, OWN, 0).observation,
            Observation::ObservedBestEffort(0)
        );
        assert_eq!(
            Sample::observed(Source::MacProcTaskInfoFaults, OWN, i32::MAX as u64 + 1).observation,
            Observation::Unknown(Unknown::NativeRange)
        );
        assert_eq!(
            Sample::observed(Source::WindowsPageFaultCount, OWN, u64::from(u32::MAX) + 1).observation,
            Observation::Unknown(Unknown::NativeRange)
        );
        assert_eq!(
            Sample::observed(Source::LinuxMinfltPlusMajflt, OWN, u64::MAX).observation,
            Observation::Unknown(Unknown::SaturatedAggregate)
        );
    }

    #[test]
    fn valid_pairs_are_conditional_and_decreasing_pairs_are_unknown() {
        let a = Sample::observed(Source::WindowsPageFaultCount, OWN, 40);
        let b = Sample::observed(Source::WindowsPageFaultCount, OWN, 42);
        assert_eq!(between(a, b, true), Delta::ConditionalDifference(2));
        assert_eq!(between(b, a, true), Delta::Unknown(Unknown::CounterDecreased));
        assert_eq!(between(a, a, true), Delta::ConditionalDifference(0));
    }

    #[test]
    fn source_identity_and_phase_mismatches_refuse_deltas() {
        let a = Sample::observed(Source::WindowsPageFaultCount, OWN, 40);
        let b = Sample::observed(Source::LinuxMinfltPlusMajflt, OWN, 42);
        assert_eq!(between(a, b, true), Delta::Unknown(Unknown::SourceMismatch));
        let b = Sample::observed(Source::WindowsPageFaultCount, Identity { start: 35, ..OWN }, 42);
        assert_eq!(between(a, b, true), Delta::Unknown(Unknown::DifferentIdentity));
        assert_eq!(between(a, a, false), Delta::Unknown(Unknown::UnorderedPhase));
        let missing = Sample::unknown(a.source, None, Unknown::MissingIdentity);
        assert_eq!(
            between(a, missing, true),
            Delta::Unknown(Unknown::MissingIdentity)
        );
    }

    #[test]
    fn read_exit_and_unreached_states_remain_unknown() {
        let a = Sample::observed(Source::LinuxMinfltPlusMajflt, OWN, 40);
        for reason in [
            Unknown::IdentityUnverified,
            Unknown::ReadUnavailable,
            Unknown::ProcessExitedOrHandleReleased,
            Unknown::PhaseNotReached,
        ] {
            let b = Sample::unknown(a.source, Some(OWN), reason);
            assert_eq!(between(a, b, true), Delta::Unknown(reason));
        }
        let invalid = Sample::observed(a.source, Identity { pid: 0, ..OWN }, 42);
        assert_eq!(
            invalid.observation,
            Observation::Unknown(Unknown::MissingIdentity)
        );
    }
}
