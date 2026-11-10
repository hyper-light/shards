//! Candidate-backend correctness only; no API binding or platform selection.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[path = "sync.rs"]
mod candidate;
mod allocation {
    include!("allocation.rs");
    pub fn set(refuse: bool) {
        REFUSE.set(refuse);
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Phase {
    Created,
    Waiting,
    Released,
}

#[test]
fn storage_refusal_returns_the_owned_value() {
    let value = Box::new(7u64);
    let identity = (&*value) as *const u64;
    allocation::set(true);
    let result = candidate::Mutex::new(value);
    allocation::set(false);
    let rejected = result.err().expect("storage must be refused");
    assert!(rejected.residual.is_none());
    let (value, failure) = (rejected.value, rejected.failure);
    assert_eq!((&*value) as *const u64, identity);
    assert_eq!(failure.operation, candidate::Operation::Storage);
    assert_eq!(failure.code, None);
    allocation::set(true);
    let result = candidate::Condvar::new();
    allocation::set(false);
    assert!(matches!(
        result,
        Err(candidate::Failure {
            operation: candidate::Operation::Storage,
            code: None,
            ..
        })
    ));
}

#[test]
fn cold_pair_wait_notify_and_timeout_under_refusal() {
    let millis: u64 = std::env::var("SHARDS_SYNC_CASE_MS")
        .expect("explicit bound")
        .parse()
        .unwrap();
    assert!(millis > 0);
    let deadline = Instant::now().checked_add(Duration::from_millis(millis)).unwrap();
    let pair = Arc::new((
        candidate::Mutex::new(Phase::Created)
            .map_err(|rejected| rejected.failure)
            .unwrap(),
        candidate::Condvar::new().unwrap(),
    ));
    let other = pair.clone();
    let worker = std::thread::Builder::new()
        .spawn(move || {
            allocation::set(true);
            let mut state = other.0.lock().unwrap();
            *state = Phase::Waiting;
            other.1.notify_all().unwrap();
            while *state != Phase::Released {
                assert!(Instant::now() < deadline);
                // SAFETY: this condition variable is paired with only pair.0.
                let (next, outcome) = unsafe {
                    other
                        .1
                        .wait(state, Some(deadline.saturating_duration_since(Instant::now())))
                };
                state = next;
                if outcome != Ok(candidate::WaitStatus::NotifiedOrSpurious) {
                    allocation::set(false);
                }
                assert_ne!(outcome.unwrap(), candidate::WaitStatus::TimedOut);
            }
            state.release().unwrap();
            allocation::set(false);
        })
        .unwrap();
    allocation::set(true);
    let mut state = pair.0.lock().unwrap();
    while *state != Phase::Waiting {
        assert!(Instant::now() < deadline);
        // SAFETY: same fixed pair, same original deadline/predicate loop.
        let (next, outcome) = unsafe {
            pair.1
                .wait(state, Some(deadline.saturating_duration_since(Instant::now())))
        };
        state = next;
        if outcome != Ok(candidate::WaitStatus::NotifiedOrSpurious) {
            allocation::set(false);
        }
        assert_ne!(outcome.unwrap(), candidate::WaitStatus::TimedOut);
    }
    *state = Phase::Released;
    pair.1.notify_all().unwrap();
    state.release().unwrap();
    allocation::set(false);
    worker.join().unwrap();
    allocation::set(true);
    let state = pair.0.lock().unwrap();
    // SAFETY: no remaining worker or different mutex can use this CV.
    let (state, outcome) = unsafe { pair.1.wait(state, Some(Duration::ZERO)) };
    assert_eq!(outcome.unwrap(), candidate::WaitStatus::TimedOut);
    assert_eq!(*state, Phase::Released);
    state.release().unwrap();
    allocation::set(false);
}

#[cfg(send_guard)]
fn rejected_guard_send(guard: candidate::Guard<'_, ()>) {
    fn requires_send<T: Send>(_: T) {}
    requires_send(guard);
}
