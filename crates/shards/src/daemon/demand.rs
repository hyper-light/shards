//! How many warm VMs a template's pool keeps ready: as many as the largest burst of runs
//! it has seen arrive within one refill, while it is in use, and none once it has gone
//! unclaimed for its keep-alive (audit A13).
//!
//! - A refill takes as long as a warm VM takes to restore and say it is ready. Its time is
//!   estimated as TCP estimates a round trip's, and its window is that estimate's
//!   retransmission timeout, SRTT + 4·RTTVAR (RFC 6298 §2): runs arriving closer together
//!   than that cannot all be served by VMs refilled for one another.
//! - A burst is the runs claimed within one refill window of each other. The pool keeps
//!   the largest seen within the keep-alive, at least one, and at most its most.
//! - The keep-alive is a fixed time since the pool's last claim, as providers keep an
//!   idle function (AWS 10 minutes, Azure 20: Shahrad et al., "Serverless in the Wild",
//!   USENIX ATC 2020, §1). Past it the pool keeps nothing until its next claim.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A template's demand, as its pool has seen it.
#[derive(Debug, Default)]
pub struct Demand {
    /// When a run last took one of its VMs, or the pool began; `None` before either.
    last: Option<Instant>,
    /// Recent claims, within one refill window of the last, at most the pool's most.
    recent: VecDeque<Instant>,
    /// The largest burst within the keep-alive, and when it was seen.
    peak: usize,
    peak_at: Option<Instant>,
    /// The refill time's smoothed estimate and its variation (RFC 6298).
    srtt: Option<Duration>,
    rttvar: Duration,
}

/// The refill window before any refill has been timed: RFC 6298's initial RTO (§2.1). A
/// pool's first refill is timed as it begins, so this counts little more than the claims
/// that begin it.
const FIRST_WINDOW: Duration = Duration::from_secs(1);

impl Demand {
    /// A pool begins in use, at `now`: its template was just saved, or asked for.
    pub fn begin(&mut self, now: Instant) {
        self.last.get_or_insert(now);
    }

    /// A run took one of the pool's VMs, or waits for one, at `now`; the pool keeps at most
    /// `most`.
    pub fn claimed(&mut self, now: Instant, most: usize) {
        let window = self.window();
        while self
            .recent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) > window)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= most.max(1) {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        self.last = Some(now);
        let burst = self.recent.len();
        if burst >= self.peak {
            self.peak = burst;
            self.peak_at = Some(now);
        }
    }

    /// A refill took `took`, from its VM's start to its saying it was ready (RFC 6298 §2.2,
    /// §2.3, with its α of 1/8 and β of 1/4).
    pub fn refilled(&mut self, took: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(took);
                self.rttvar = took / 2;
            }
            Some(srtt) => {
                let off = srtt.abs_diff(took);
                self.rttvar = (self.rttvar * 3 + off) / 4;
                self.srtt = Some((srtt * 7 + took) / 8);
            }
        }
    }

    /// Runs this close together are one burst: a refill's retransmission timeout.
    fn window(&self) -> Duration {
        self.srtt.map_or(FIRST_WINDOW, |srtt| {
            srtt.saturating_add(self.rttvar.saturating_mul(4))
        })
    }

    /// When a run last took one of its VMs, or the pool began.
    pub fn last(&self) -> Option<Instant> {
        self.last
    }

    /// Whether the pool has gone unclaimed past `keep` at `now`.
    pub fn expired(&self, now: Instant, keep: Duration) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) > keep)
    }

    /// How many VMs the pool keeps ready at `now`: none past its keep-alive `keep`, and
    /// otherwise the largest burst within it, at least one and at most `most`.
    pub fn target(&mut self, now: Instant, most: usize, keep: Duration) -> usize {
        if most == 0 || self.expired(now, keep) {
            return 0;
        }
        if self
            .peak_at
            .is_some_and(|at| now.saturating_duration_since(at) > keep)
        {
            self.peak = self.recent.len();
            self.peak_at = Some(now);
        }
        self.peak.clamp(1, most)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEP: Duration = Duration::from_secs(600);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A pool keeps one VM while runs come one at a time, as many as came at once, at most
    /// its most, and none once unclaimed past its keep-alive, until the next claim.
    #[test]
    fn a_pool_keeps_what_its_bursts_need_while_it_is_used() {
        let t0 = Instant::now();
        let mut d = Demand::default();
        assert_eq!(d.target(t0, 4, KEEP), 0, "never used");
        d.begin(t0);
        assert_eq!(d.target(t0, 4, KEEP), 1, "a template just saved");
        assert_eq!(d.target(t0, 0, KEEP), 0, "a pool of none");
        for i in 0..20 {
            d.refilled(ms(20));
            d.claimed(t0 + ms(1000 * i), 4);
        }
        assert_eq!(d.target(t0 + ms(20_000), 4, KEEP), 1, "one at a time");
        // Three within a refill: a burst of three.
        let t1 = t0 + ms(30_000);
        for i in 0..3 {
            d.claimed(t1 + ms(i), 4);
        }
        assert_eq!(d.target(t1 + ms(10), 4, KEEP), 3);
        assert_eq!(d.target(t1 + ms(10), 2, KEEP), 2, "at most its most");
        // Ten at once: at most four are counted.
        let t2 = t1 + ms(1000);
        for i in 0..10 {
            d.claimed(t2 + ms(i), 4);
        }
        assert_eq!(d.target(t2 + ms(10), 4, KEEP), 4);
        assert!(d.recent.len() <= 4, "held claims are bounded");
        // Past the keep-alive: nothing, until claimed again, and the old burst is forgotten.
        let later = t2 + ms(10) + KEEP;
        assert!(d.expired(later, KEEP));
        assert_eq!(d.target(later, 4, KEEP), 0);
        d.claimed(later, 4);
        assert_eq!(d.target(later, 4, KEEP), 1);
    }

    /// A burst is measured by the refill's window: claims further apart than a refill
    /// takes are each served by the VM refilled for the one before.
    #[test]
    fn a_burst_is_what_arrives_within_a_refill() {
        let t0 = Instant::now();
        let mut d = Demand::default();
        d.begin(t0);
        for _ in 0..50 {
            d.refilled(ms(10));
        }
        assert!(d.window() < ms(12), "{:?}", d.window());
        // 15 ms apart, beyond the window: never a burst.
        for i in 0..10 {
            d.claimed(t0 + ms(15 * i), 8);
        }
        assert_eq!(d.target(t0 + ms(200), 8, KEEP), 1);
        // Slower refills widen the window, and the same spacing becomes a burst.
        for _ in 0..50 {
            d.refilled(ms(40));
        }
        let t1 = t0 + ms(1000);
        for i in 0..3 {
            d.claimed(t1 + ms(15 * i), 8);
        }
        assert_eq!(d.target(t1 + ms(50), 8, KEEP), 3);
    }

    /// Refill times are smoothed as RFC 6298 smooths round trips, and their variation
    /// widens the window.
    #[test]
    fn refill_times_are_smoothed_as_round_trips_are() {
        let mut d = Demand::default();
        assert_eq!(d.window(), FIRST_WINDOW);
        d.refilled(ms(8));
        assert_eq!((d.srtt, d.rttvar), (Some(ms(8)), ms(4)));
        assert_eq!(d.window(), ms(24));
        d.refilled(ms(16));
        // SRTT = 7/8·8 + 1/8·16 = 9; RTTVAR = 3/4·4 + 1/4·|8 − 16| = 5.
        assert_eq!((d.srtt, d.rttvar), (Some(ms(9)), ms(5)));
        assert_eq!(d.window(), ms(29));
    }
}
