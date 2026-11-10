//! Checked CALL-SHAPE example only: no mutex, waiter, backend or runtime owner is
//! implemented. No OS operation, child launch or synchronization probe is run.
//! PreparedOwnedSync is an opaque mock receipt, not evidence of initialization,
//! Send/Sync, pinning, allocation freedom or native cleanup. See PROPOSAL.md.
#![forbid(unsafe_code)]

use std::fmt;
use std::io;
use std::process::Child;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPart {
    State,
    Changed,
    Tail,
    Drained,
    Child,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOperation {
    StableStorage,
    MutexAttributes,
    Mutex,
    CondvarAttributes,
    Condvar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCode {
    Posix(i32),
    Win32(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailure {
    Allocation {
        part: SyncPart,
    },
    Native {
        part: SyncPart,
        operation: InitOperation,
        code: NativeCode,
    },
    Unsupported {
        part: SyncPart,
    },
}

impl fmt::Display for SyncFailure {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocation { part } => write!(out, "{part:?}: synchronization storage refused"),
            Self::Native {
                part,
                operation,
                code,
            } => write!(out, "{part:?}/{operation:?}: {code:?}"),
            Self::Unsupported { part } => write!(out, "{part:?}: synchronization unavailable"),
        }
    }
}

// Deliberately no public constructor and no Clone/Copy/unsafe trait impl. The
// chosen real backend must establish its own full initialization/lifetime proof.
pub struct PreparedOwnedSync {
    _example_seal: (),
}

pub struct ReadyOwned {
    pub child: Child,
    pub prepared: PreparedOwnedSync,
}

pub enum BeforePublicationFailure {
    Synchronization(SyncFailure),
    Spawn {
        error: io::Error,
        prepared: PreparedOwnedSync,
    },
}

/// Common fallibility/ownership obligation, not an implementation choice:
/// no child is launched after preparation refusal; failed spawn returns prepared
/// resources for explicit cleanup instead of assuming infallible destruction.
pub fn prepare_then_launch(
    prepare: fn() -> Result<PreparedOwnedSync, SyncFailure>,
    launch: fn() -> io::Result<Child>,
) -> Result<ReadyOwned, BeforePublicationFailure> {
    let prepared = prepare().map_err(BeforePublicationFailure::Synchronization)?;
    match launch() {
        Ok(child) => Ok(ReadyOwned { child, prepared }),
        Err(error) => Err(BeforePublicationFailure::Spawn { error, prepared }),
    }
}

pub struct AfterChildFailure {
    pub child: Child,
    pub failure: SyncFailure,
}

/// The current start path already has a Child before Shared construction. If a
/// future reviewed change retains that ordering, the refusal must retain Child
/// for the EXISTING owner's kill/reap path. This example performs neither action.
pub fn retain_child_on_refusal(
    child: Child,
    prepared: Result<PreparedOwnedSync, SyncFailure>,
) -> Result<ReadyOwned, AfterChildFailure> {
    match prepared {
        Ok(prepared) => Ok(ReadyOwned { child, prepared }),
        Err(failure) => Err(AfterChildFailure { child, failure }),
    }
}

// Negative type check only. A second owner cannot be made by using the moved
// Child again. `--cfg duplicate_owner` must fail with E0382, never run.
#[cfg(duplicate_owner)]
pub fn rejected_duplicate(child: Child, prepared: Result<PreparedOwnedSync, SyncFailure>) {
    let _result = retain_child_on_refusal(child, prepared);
    let _stale_owner = child.id();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static LAUNCH_CALLED: AtomicBool = AtomicBool::new(false);

    fn refused() -> Result<PreparedOwnedSync, SyncFailure> {
        Err(SyncFailure::Allocation {
            part: SyncPart::State,
        })
    }

    fn mock_prepared() -> Result<PreparedOwnedSync, SyncFailure> {
        Ok(PreparedOwnedSync { _example_seal: () })
    }

    fn no_child_launch() -> io::Result<Child> {
        LAUNCH_CALLED.store(true, Ordering::SeqCst);
        Err(io::ErrorKind::PermissionDenied.into())
    }

    #[test]
    fn preparation_refusal_never_calls_launch() {
        LAUNCH_CALLED.store(false, Ordering::SeqCst);
        let result = prepare_then_launch(refused, no_child_launch);
        assert!(matches!(
            result,
            Err(BeforePublicationFailure::Synchronization(
                SyncFailure::Allocation {
                    part: SyncPart::State
                }
            ))
        ));
        assert!(!LAUNCH_CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_launch_returns_prepared_resource() {
        LAUNCH_CALLED.store(false, Ordering::SeqCst);
        let result = prepare_then_launch(mock_prepared, no_child_launch);
        assert!(matches!(
            result,
            Err(BeforePublicationFailure::Spawn { prepared: _, .. })
        ));
        assert!(LAUNCH_CALLED.load(Ordering::SeqCst));
    }
}
