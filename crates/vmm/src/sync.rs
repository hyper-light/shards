//! Poison-tolerant locking. Builds use `panic = "abort"`, so a lock can never actually
//! be poisoned; recovering the guard keeps every lock site free of unwraps.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

/// [`wait`], for `dur` at most.
pub fn wait_timeout<'a, T>(
    cv: &Condvar,
    guard: MutexGuard<'a, T>,
    dur: std::time::Duration,
) -> MutexGuard<'a, T> {
    match cv.wait_timeout(guard, dur) {
        Ok((guard, _)) => guard,
        Err(poisoned) => poisoned.into_inner().0,
    }
}
