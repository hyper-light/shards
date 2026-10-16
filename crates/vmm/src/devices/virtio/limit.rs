//! A microVM's I/O limits (D178): the bytes and the requests a second that its devices serve
//! of the host's storage, its shared directories' (virtio-fs) and its disks' (virtio-blk), at
//! most. The VMM keeps them, outside the guest: no guest, its root or not, lifts them, where
//! the limits a container's cgroup sets in the guest are the guest's own to keep (D44).
//!
//! Each request is charged as it is served, by each measure's theoretical arrival time
//! (ITU-T I.371's generic cell rate algorithm): when the requests served so far would have
//! been done at its rate. A device left idle serves at once; one kept busy is paced at the
//! limit, a request waiting until the requests before it would have been done; so no window
//! of time holds more than its rate's worth, and one request more. A request larger than a
//! second's worth is served whole, and the requests after it wait it out. Two things a busy
//! host does would otherwise cost the rate what they take (PM M240), and do not:
//!
//! - A request that costs a measure nothing, one of no bytes, neither waits on it nor moves
//!   it: the next write waits only for the write before it, and whatever comes between
//!   them, the guest's turning round and the requests of no bytes it asks between, takes its
//!   time within that wait. One that waited would leave the next write to start late.
//! - A wait that runs past its deadline, as a busy host's timers and scheduler leave it,
//!   leaves the next request a measure charges late by as much: that request keeps its place
//!   in the measure's schedule, where it would otherwise start the schedule over. It never
//!   goes before its place, and time left idle earns nothing.
//!
//! While its devices pause, as a snapshot pauses them (D176), no request waits: the one a
//! worker has in hand is served at once, its cost kept, so that the pause waits for no limit.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The limits of the microVM its devices serve, one a VM process (D178).
#[derive(Debug)]
pub struct Limiter {
    state: Mutex<State>,
    paused: Condvar,
}

impl Default for Limiter {
    fn default() -> Limiter {
        Limiter::new()
    }
}

#[derive(Debug)]
struct State {
    bytes: Measure,
    requests: Measure,
    /// How many of its devices are paused ([`Limiter::pause`]).
    pauses: u64,
}

/// One of its limits: bytes, or requests.
#[derive(Debug)]
struct Measure {
    /// How many a second it serves at most, 0 for no limit.
    rate: u64,
    /// When what it charged so far would have been done at its rate.
    done: Option<Instant>,
    /// How long the wait of the last request it charged ran past its deadline, which the
    /// next request it charges is late by.
    late: Duration,
}

impl Measure {
    const NONE: Measure = Measure {
        rate: 0,
        done: None,
        late: Duration::ZERO,
    };

    /// Charges `cost` of a request at `now`: how long the request waits for it, none where
    /// this measure has no limit or the request costs it nothing.
    fn charge(&mut self, now: Instant, cost: u64) -> Option<Duration> {
        if self.rate == 0 || cost == 0 {
            return None;
        }
        // Late by the last wait's overrun, the request keeps the place it would have had.
        let arrived = now.checked_sub(std::mem::take(&mut self.late)).unwrap_or(now);
        let start = self.done.map_or(now, |d| d.max(arrived));
        let took = u128::from(cost) * 1_000_000_000 / u128::from(self.rate);
        let took = Duration::from_nanos(u64::try_from(took).unwrap_or(u64::MAX));
        self.done = Some(start.checked_add(took).unwrap_or(start));
        Some(start.saturating_duration_since(now))
    }
}

impl Limiter {
    /// One with no limits, for a `static`: its run's come with its request.
    pub const fn new() -> Limiter {
        Limiter {
            state: Mutex::new(State {
                bytes: Measure::NONE,
                requests: Measure::NONE,
                pauses: 0,
            }),
            paused: Condvar::new(),
        }
    }

    /// Its limits from here on: bytes and requests a second, 0 for none of either. What was
    /// served under no limit, or another, is not charged to these.
    pub fn set(&self, bytes: u64, requests: u64) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.bytes = Measure {
            rate: bytes,
            ..Measure::NONE
        };
        s.requests = Measure {
            rate: requests,
            ..Measure::NONE
        };
    }

    /// Charges a request of `bytes`, before it is served: back once its limits let it go,
    /// at once where it has none.
    pub fn take(&self, bytes: u64) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let (by_bytes, by_requests) = (s.bytes.charge(now, bytes), s.requests.charge(now, 1));
        let wait = by_bytes.max(by_requests).unwrap_or_default();
        let Some(deadline) = now.checked_add(wait).filter(|_| !wait.is_zero()) else {
            return;
        };
        while s.pauses == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = self
                .paused
                .wait_timeout(s, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        // Its overrun, which the next request each measure it charged charges is late by.
        let over = Instant::now().saturating_duration_since(deadline);
        if by_bytes.is_some() {
            s.bytes.late = over;
        }
        if by_requests.is_some() {
            s.requests.late = over;
        }
    }

    /// One of its devices pauses (D176): no request waits until it resumes, every one
    /// waiting goes at once, since a device pauses only once its worker has answered the
    /// request in hand.
    pub fn pause(&self) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.pauses = s.pauses.saturating_add(1);
        drop(s);
        self.paused.notify_all();
    }

    /// One of its paused devices resumes.
    pub fn resume(&self) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.pauses = s.pauses.saturating_sub(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn limited(rate: u64) -> Measure {
        Measure {
            rate,
            ..Measure::NONE
        }
    }

    const MS: Duration = Duration::from_millis(1);

    /// Paced at its rate: a request waits until those before it would have been done, a
    /// large one is served whole and waited out after, an idle device's credit is no more
    /// than the moment it is asked in, and a request that costs it nothing neither waits
    /// on it nor moves it.
    #[test]
    fn requests_are_paced_at_their_rates() {
        let t = Instant::now();
        let mut m = limited(1000);
        assert_eq!(m.charge(t, 500), Some(Duration::ZERO));
        assert_eq!(m.charge(t, 500), Some(500 * MS));
        assert_eq!(m.charge(t, 0), None);
        assert_eq!(m.charge(t, 1), Some(1000 * MS));
        // Larger than a second's worth: served, and the next waits it out.
        let mut m = limited(1000);
        assert_eq!(m.charge(t, 3000), Some(Duration::ZERO));
        assert_eq!(m.charge(t + 1000 * MS, 1), Some(2000 * MS));
        // Idle a minute: no credit saved up, the next is paced from when it comes.
        assert_eq!(m.charge(t + 60_000 * MS, 1000), Some(Duration::ZERO));
        assert_eq!(m.charge(t + 60_000 * MS, 1), Some(1000 * MS));
        // Ten requests at once, at ten a second: a tenth of a second apart.
        let mut m = limited(10);
        let waits: Vec<_> = (0..10).map(|_| m.charge(t, 1)).collect();
        assert_eq!(waits, (0..10).map(|i| Some(i * 100 * MS)).collect::<Vec<_>>());
        let mut none = Measure::NONE;
        assert_eq!(none.charge(t, 1), None);
    }

    /// A wait that overran its place by more than the next request's: that request keeps its
    /// place, where it would have started the schedule over, and never goes before it; a
    /// request that costs the measure nothing between them leaves it so.
    #[test]
    fn a_late_request_keeps_its_place() {
        let t = Instant::now();
        let mut m = limited(1000);
        assert_eq!(m.charge(t, 500), Some(Duration::ZERO));
        assert_eq!(m.charge(t, 500), Some(500 * MS));
        m.late = 600 * MS;
        assert_eq!(m.charge(t + 1100 * MS, 0), None);
        assert_eq!(m.charge(t + 1100 * MS, 500), Some(Duration::ZERO));
        assert_eq!(m.charge(t + 1100 * MS, 1), Some(400 * MS));
        // Within its place, the overrun costs nothing either way.
        let mut m = limited(1000);
        assert_eq!(m.charge(t, 500), Some(Duration::ZERO));
        assert_eq!(m.charge(t, 500), Some(500 * MS));
        m.late = 200 * MS;
        assert_eq!(m.charge(t + 700 * MS, 1), Some(300 * MS));
    }

    /// None limited: nothing waits, whatever it is asked.
    #[test]
    fn no_limit_waits_for_nothing() {
        let l = Limiter::new();
        let started = Instant::now();
        for _ in 0..1000 {
            l.take(u64::MAX);
        }
        assert!(started.elapsed() < 100 * MS);
    }

    /// A wait ends as its devices pause, its request then served, and none waits until
    /// they have all resumed; then the debt is waited out again.
    #[test]
    fn a_pause_lets_a_waiting_request_go() {
        static L: Limiter = Limiter::new();
        L.set(1, 0);
        L.take(3600);
        let waiting = std::thread::spawn(|| {
            let started = Instant::now();
            L.take(1);
            started.elapsed()
        });
        std::thread::sleep(50 * MS);
        L.pause();
        let waited = waiting.join().unwrap();
        assert!(waited >= 40 * MS && waited < 10_000 * MS, "{waited:?}");
        L.pause();
        let started = Instant::now();
        L.take(1);
        L.resume();
        L.take(1);
        assert!(started.elapsed() < 1000 * MS, "paused still: none waits");
        L.resume();
        let state = L.state.lock().unwrap();
        assert!(
            state.bytes.done.unwrap() > Instant::now() + 3000 * 1000 * MS,
            "its debt kept"
        );
    }
}
