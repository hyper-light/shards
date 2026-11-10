// Exact daemon fragments; compile-only scaffold, never a complete daemon.
#![allow(dead_code)]
mod build {
    // Admission is unused by the fixture. This compile scaffold cannot admit.
    pub fn host_available() -> Option<u64> { None }
}
mod daemon {
use std::io::{self, Write as _};
use std::sync::{Mutex, MutexGuard};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
static LOGGED: AtomicU64 = AtomicU64::new(0);
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn log(message: impl std::fmt::Display) {
    #[cfg(test)]
    LOGGED.fetch_add(1, Ordering::Relaxed);
    let line = format!("shards daemon {}: {message}\n", std::process::id());
    let _ = io::stderr().write_all(line.as_bytes());
}

mod managed {
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use shards_abi::run::{self, managed::Channel};
use shards_remote::identity;
use shards_remote::managed::{Admitted, Control, Endpoint, Producer, Progress, RunId, Scope, State};
use super::{lock, log};
const HANDSHAKE_BYTES: usize = (1 << 24) - 1 + 4;
#[derive(Debug)]
pub(super) enum Admission {
    UnknownHeadroom,
    Unavailable,
    Insufficient { requested: u64, available: u64 },
    Overflow,
}

struct Charge {
    budget: Arc<Mutex<Option<u64>>>,
    bytes: Mutex<u64>,
}

impl Charge {
    fn reserve(budget: &Arc<Mutex<Option<u64>>>, bytes: u64) -> Result<Self, Admission> {
        let charge = Self {
            budget: Arc::clone(budget),
            bytes: Mutex::new(0),
        };
        charge.grow(bytes)?;
        Ok(charge)
    }

    fn grow(&self, extra: u64) -> Result<(), Admission> {
        let available = crate::build::host_available().ok_or(Admission::UnknownHeadroom)?;
        let mut held = lock(&self.bytes);
        let mut budget = lock(&self.budget);
        let used = budget.ok_or(Admission::Unavailable)?;
        let next = used.checked_add(extra).ok_or(Admission::Overflow)?;
        if next > available {
            return Err(Admission::Insufficient {
                requested: extra,
                available: available.saturating_sub(used),
            });
        }
        *held = held.checked_add(extra).ok_or(Admission::Overflow)?;
        *budget = Some(next);
        Ok(())
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let held = *lock(&self.bytes);
        let mut budget = lock(&self.budget);
        *budget = budget.and_then(|used| used.checked_sub(held));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    End,
    Aborted(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Serving,
    Draining(Terminal),
    Ended,
    // None means no canonical terminal was submitted before interruption.
    Aborted(Option<Terminal>),
}

#[derive(Clone, Copy)]
enum Resolve {
    Complete,
    Stop,
}

impl Life {
    fn drain(&mut self, terminal: Terminal) -> io::Result<()> {
        if !matches!(self, Self::Serving) {
            return Err(io::Error::other("capture scope ended"));
        }
        *self = Self::Draining(terminal);
        Ok(())
    }

    fn resolve(&mut self, state: State, event: Resolve) -> Option<Self> {
        let next = match (*self, state, event) {
            (Self::Draining(Terminal::End), State::Finished(_), _) => Self::Ended,
            (Self::Draining(terminal), State::Finished(_) | State::Aborted(_), _)
            | (Self::Draining(terminal), _, Resolve::Stop) => Self::Aborted(Some(terminal)),
            (Self::Serving, State::Aborted(_), _) | (Self::Serving, _, Resolve::Stop) => Self::Aborted(None),
            _ => return None,
        };
        *self = next;
        Some(next)
    }

    fn write(self, metadata: &mut impl io::Write, scope: Scope, at: Progress) -> io::Result<()> {
        let (state, tail) = match self {
            Self::Ended => ("ended", "complete"),
            Self::Aborted(_) => ("aborted", "unknown"),
            _ => return Ok(()),
        };
        if let Self::Aborted(Some(Terminal::Aborted(errno))) = self {
            writeln!(
                metadata,
                "member {} channel {} canonical aborted errno {errno}",
                scope.member, scope.channel as u8
            )?;
        }
        writeln!(
            metadata,
            "member {} channel {} state {state} accepted {} consumed {} tail {tail}",
            scope.member, scope.channel as u8, at.accepted, at.consumed
        )
    }
}

struct Output {
    scope: Scope,
    address: std::net::SocketAddr,
    control: Control,
    metadata: Arc<Mutex<File>>,
    life: Mutex<Life>,
    credits: std::sync::Weak<Credits>,
    index: usize,
    learning: Mutex<Learning>,
    charge: Arc<Charge>,
}

struct Sample {
    began: Instant,
    end: u64,
    bytes: u64,
}
struct Learning {
    quantum: shards_registry::Quantum,
    sample: Option<Sample>,
    admitted: usize,
}

impl Output {
    fn observed(&self) {
        let at = self.control.progress().consumed;
        let mut learning = lock(&self.learning);
        if learning.sample.as_ref().is_some_and(|sample| sample.end <= at)
            && let Some(sample) = learning.sample.take()
        {
            learning
                .quantum
                .consumed(sample.began, Instant::now(), sample.bytes);
        }
    }

    fn capacity(&self, producer: &Producer) -> io::Result<usize> {
        let mut learning = lock(&self.learning);
        let desired = learning.quantum.desired();
        if desired > learning.admitted {
            let extra = desired
                .checked_sub(learning.admitted)
                .and_then(|n| n.checked_mul(2))
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| io::Error::other("capture growth overflow"))?;
            // One VM-owned record and one daemon-owned record may overlap while IPC copies.
            // QUIC's in-flight window remains fixed and separately reserved.
            if self.charge.grow(extra).is_ok() {
                producer.admit(desired).map_err(io::Error::other)?;
                learning.admitted = desired;
            }
        }
        Ok(desired.min(learning.admitted))
    }

    fn resolve(&self, event: Resolve) -> Option<(Life, State)> {
        let mut life = lock(&self.life);
        // Stop must observe Finished before retiring Shared, even if the credit
        // worker has not yet published it. Life arbitrates that single winner.
        let state = self.control.state();
        life.resolve(state, event).map(|next| (next, state))
    }

    fn publish(&self, terminal: Option<(Life, State)>) {
        let Some((life, state)) = terminal else { return };
        let result = life.write(&mut *lock(&self.metadata), self.scope, state.progress());
        if let Err(error) = result {
            // Logical consumption is sticky, but a failed append is not publication.
            // Retire this transport without holding metadata/Life/credit locks.
            self.stop_transport();
            log(format!(
                "managed output metadata member {} channel {}: {error}",
                self.scope.member, self.scope.channel as u8
            ));
        }
    }

    fn abort(&self) {
        let terminal = self.resolve(Resolve::Stop);
        self.stop_transport();
        self.publish(terminal);
    }

    fn stop_transport(&self) {
        if let Some(credits) = self.credits.upgrade() {
            credits.revoke(self.index);
        }
        // Logical terminal does not release ownership of the endpoint's driver.
        // Stop is unconditional and precedes fallible metadata I/O.
        self.control.abort();
    }

    fn finish(&self, terminal: Terminal) -> io::Result<()> {
        lock(&self.life).drain(terminal)?;
        // Shared invokes the wake inline; never hold Life across it. A concurrent
        // stop remains terminal, and remote finish cannot revive an aborted slot.
        self.control.finish().map_err(io::Error::other)
    }

    fn complete(&self) {
        self.observed();
        let terminal = self.resolve(Resolve::Complete);
        if matches!(terminal, Some((_, State::Aborted(_)))) {
            self.stop_transport();
        }
        self.publish(terminal);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone, Copy)]
enum After {
    More,
    Terminal(Terminal),
}

enum CreditSlot {
    Inactive,
    Waiting,
    Granted { capacity: usize, began: Instant },
    Captured(Vec<u8>, After),
    Sending,
    Draining,
    Ended,
    Revoked,
    RevokedGranted { capacity: usize },
    Receiving,
    RevokedReceiving,
}

enum QueueLife {
    Open,
    Closed,
}
struct Queue {
    slots: Vec<CreditSlot>,
    next: usize,
    life: QueueLife,
}
struct Credits {
    state: Mutex<Queue>,
    changed: Condvar,
}
enum Action {
    Grant(usize),
    Submit(usize, Vec<u8>, After),
    Complete(usize),
}

impl Credits {
    fn revoke(&self, index: usize) {
        let mut state = lock(&self.state);
        if let Some(slot) = state.slots.get_mut(index) {
            *slot = match slot {
                CreditSlot::Granted { capacity, .. } => CreditSlot::RevokedGranted { capacity: *capacity },
                CreditSlot::Receiving => CreditSlot::RevokedReceiving,
                _ => CreditSlot::Revoked,
            };
        }
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.life = QueueLife::Closed;
        for slot in &mut state.slots {
            *slot = CreditSlot::Ended;
        }
        self.changed.notify_all();
    }

    fn wake(&self) {
        // The predicate is read under this same mutex in next. Shared publishers
        // release Shared before this callback, so credit -> Shared has no reverse edge.
        let _state = lock(&self.state);
        self.changed.notify_all();
    }

    fn begin_record(&self, index: usize, len: usize) -> io::Result<(usize, Instant)> {
        let mut state = lock(&self.state);
        let slot = state
            .slots
            .get_mut(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown capture route"))?;
        match slot {
            CreditSlot::Granted { capacity, began } if len <= *capacity => {
                let grant = (*capacity, *began);
                *slot = CreditSlot::Receiving;
                Ok(grant)
            }
            CreditSlot::RevokedGranted { capacity } if len <= *capacity => {
                let grant = (*capacity, Instant::now());
                *slot = CreditSlot::RevokedReceiving;
                Ok(grant)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture record exceeds issued channel credit",
            )),
        }
    }

    fn captured(&self, index: usize, bytes: Vec<u8>, after: After) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Some(slot @ CreditSlot::RevokedReceiving) = state.slots.get_mut(index) {
            *slot = CreditSlot::Revoked;
            return Ok(());
        }
        let slot = state
            .slots
            .get_mut(index)
            .filter(|slot| matches!(slot, CreditSlot::Receiving))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "capture record has no channel credit")
            })?;
        *slot = CreditSlot::Captured(bytes, after);
        self.changed.notify_all();
        Ok(())
    }

    fn next(&self, observe: impl Fn(usize) -> io::Result<State>) -> io::Result<Option<Action>> {
        let mut state = lock(&self.state);
        loop {
            if matches!(state.life, QueueLife::Closed)
                || state.slots.iter().all(|s| {
                    matches!(
                        s,
                        CreditSlot::Inactive
                            | CreditSlot::Ended
                            | CreditSlot::Revoked
                            | CreditSlot::RevokedGranted { .. }
                            | CreditSlot::RevokedReceiving
                    )
                })
            {
                return Ok(None);
            }
            let len = state.slots.len();
            if len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture has no admitted channels",
                ));
            }
            for step in 0..len {
                let index = state.next.checked_add(step).map(|n| n % len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                })?;
                let Some(slot) = state.slots.get_mut(index) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "capture scheduler index",
                    ));
                };
                if !matches!(
                    slot,
                    CreditSlot::Waiting | CreditSlot::Captured(..) | CreditSlot::Draining
                ) {
                    continue;
                }
                // Observation releases Shared before an action leaves this credit
                // guard. Life and metadata are touched only by the worker afterward.
                let remote = observe(index)?;
                let action = match (std::mem::replace(slot, CreditSlot::Sending), remote) {
                    (CreditSlot::Waiting, State::Ready(_)) => Some(Action::Grant(index)),
                    (CreditSlot::Captured(bytes, after), State::Ready(_)) => {
                        Some(Action::Submit(index, bytes, after))
                    }
                    (CreditSlot::Draining, State::Finished(_))
                    | (
                        CreditSlot::Waiting | CreditSlot::Captured(..) | CreditSlot::Draining,
                        State::Aborted(_),
                    ) => {
                        *slot = CreditSlot::Ended;
                        Some(Action::Complete(index))
                    }
                    (old, _) => {
                        *slot = old;
                        None
                    }
                };
                if action.is_some() {
                    state.next = index.checked_add(1).map(|n| n % len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "capture scheduler overflow")
                    })?;
                    return Ok(action);
                }
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
#[path = "/home/adalundhe/native-terminal-order-light-b02e-v1/source/crates/shards/src/daemon/managed/terminal_order_tests.rs"]
mod terminal_order_tests;
}
}
