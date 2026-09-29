//! Poison-tolerant locking. Builds use `panic = "abort"`, so a lock can never actually
//! be poisoned; recovering the guard keeps every lock site free of unwraps.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
}
