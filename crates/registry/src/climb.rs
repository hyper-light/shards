//! How many transfers go at once: as many as pay. A pull's connections (fetch.rs, PM M114)
//! and a push's uploads (push.rs, PM M488) start at a count each measured, and double
//! while a doubling brings at least a tenth more of what they carry together, measured
//! over a window only while every one of them has work, the window they began in passed
//! over; a doubling that does not pay goes back to the count before, and that is kept.

use std::time::Duration;

/// How long each count is measured, after as long again to settle.
pub(crate) const WINDOW: Duration = Duration::from_secs(1);
/// What a doubling must bring to be kept: more than four connections' spread from second
/// to second (PM M114).
const GAIN: f64 = 1.1;

/// The count of transfers, climbing while it pays.
pub(crate) struct Climb {
    first: usize,
    most: usize,
    /// The rate the count before the last doubling carried.
    best: Option<f64>,
    /// Kept from here on.
    settled: bool,
    /// The window transfers just started in, which is not theirs.
    settling: bool,
}

impl Climb {
    /// From `first`, at most `most`.
    pub(crate) fn new(first: usize, most: usize) -> Climb {
        Climb {
            first,
            most: most.max(first),
            best: None,
            settled: false,
            settling: true,
        }
    }

    /// A window's end: `rate`, what `target` transfers carried in it, every one of them busy
    /// throughout or not. The count to go to, where it changes.
    pub(crate) fn window(&mut self, rate: f64, target: usize, all_busy: bool) -> Option<usize> {
        if self.settled {
            return None;
        }
        if self.settling {
            self.settling = false;
            return None;
        }
        // With less work than transfers, more would show nothing.
        if !all_busy {
            return None;
        }
        if self.best.is_some_and(|before| rate < before * GAIN) {
            // Not worth it: back to what it was, and kept.
            self.settled = true;
            let back = (target / 2).max(self.first);
            return (back != target).then_some(back);
        }
        if target >= self.most {
            self.settled = true;
            return None;
        }
        self.best = Some(rate);
        self.settling = true;
        Some((target * 2).min(self.most))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A count doubles while each doubling brings a tenth more, and goes back to the last
    /// that did once one does not; the window a count began in, and one where a transfer
    /// had nothing to do, measure nothing.
    #[test]
    fn a_count_climbs_while_it_pays_and_keeps_the_last_that_did() {
        let mut c = Climb::new(2, 16);
        assert_eq!(c.window(10.0, 2, true), None, "the first window settles");
        assert_eq!(c.window(10.0, 2, false), None, "not all busy");
        assert_eq!(c.window(10.0, 2, true), Some(4));
        assert_eq!(c.window(99.0, 4, true), None, "settling");
        assert_eq!(c.window(15.0, 4, true), Some(8), "half again more");
        assert_eq!(c.window(0.0, 8, true), None, "settling");
        assert_eq!(c.window(16.0, 8, true), Some(4), "a fifteenth more does not pay");
        assert_eq!(c.window(1.0, 4, true), None, "kept");
        assert_eq!(c.window(1e9, 4, true), None, "kept for good");
    }

    /// At its most a count is kept; and one that does not pay at its first doubling goes
    /// back to its first.
    #[test]
    fn a_count_stops_at_its_most_and_never_falls_below_its_first() {
        let mut c = Climb::new(4, 8);
        c.window(1.0, 4, true);
        assert_eq!(c.window(1.0, 4, true), Some(8));
        c.window(1.0, 8, true);
        assert_eq!(c.window(100.0, 8, true), None, "at its most");
        assert_eq!(c.window(1.0, 8, true), None, "kept there");
        let mut c = Climb::new(4, 16);
        c.window(1.0, 4, true);
        assert_eq!(c.window(10.0, 4, true), Some(8));
        c.window(1.0, 8, true);
        assert_eq!(c.window(10.5, 8, true), Some(4));
        let mut c = Climb::new(1, 1);
        c.window(1.0, 1, true);
        assert_eq!(c.window(1.0, 1, true), None, "already at its most");
    }
}
