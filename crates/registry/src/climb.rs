//! How many transfers go at once: as many as pay (D226). A pull's connections (fetch.rs)
//! and a push's uploads (push.rs) climb as TCP's slow start does (RFC 5681 §3.1): from one
//! where the path is new, doubling, and straight to what the path needs in flight once its
//! answers say so, its bandwidth-delay product over what one transfer holds; never past
//! the host's cores. A doubling is kept only where it carried more than the path's own
//! noise: the gain between the two counts' per-second samples past their pooled standard
//! deviation (Welford's running variance). A doubling that does not pay goes back to the
//! count before, and that is kept. What a path converged to is learned (crate::links), and
//! the next transfer over it starts there, until its samples say the path changed.

use std::time::Duration;

/// How long each count is measured, after as long again to settle. Not derived: the
/// period M114 measured over, kept (D226's table).
pub(crate) const WINDOW: Duration = Duration::from_secs(1);

/// Samples at a count before it is judged: the fewest that have a variance.
const SAMPLES: u64 = 2;

/// A running mean and variance, Welford's (Technometrics 4(3), 1962; Knuth TAOCP vol. 2
/// §4.2.2): what a count's per-second samples carried, and how much they spread.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Spread {
    pub n: u64,
    pub mean: f64,
    /// The sum of squared differences from the mean.
    pub m2: f64,
}

impl Spread {
    pub(crate) fn add(&mut self, x: f64) {
        self.n += 1;
        let d = x - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (x - self.mean);
    }

    /// The samples' variance, once there are two.
    pub(crate) fn variance(&self) -> Option<f64> {
        (self.n >= SAMPLES).then(|| self.m2 / (self.n - 1) as f64)
    }
}

/// The standard deviation of `a`'s and `b`'s samples pooled, each about its own mean.
fn pooled(a: &Spread, b: &Spread) -> Option<f64> {
    let (va, vb) = (a.variance()?, b.variance()?);
    let df = (a.n + b.n).checked_sub(2).filter(|&d| d > 0)? as f64;
    Some((((a.n - 1) as f64 * va + (b.n - 1) as f64 * vb) / df).sqrt())
}

/// Whether `after` carried more than `before` by more than their noise.
pub(crate) fn pays(before: &Spread, after: &Spread) -> bool {
    pooled(before, after).is_some_and(|s| after.mean - before.mean > s)
}

/// Whether `now` is the path `then` was: the two means apart by no more than their noise.
pub(crate) fn same(then: &Spread, now: &Spread) -> bool {
    pooled(then, now).is_none_or(|s| (now.mean - then.mean).abs() <= s)
}

/// The cores this process may run on: transfers past them only contend for the hashing,
/// TLS and decompressing each does.
pub(crate) fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// The count of transfers, climbing while it pays.
#[derive(Debug)]
pub(crate) struct Climb {
    most: usize,
    target: usize,
    /// The samples at `target`.
    here: Spread,
    /// The count before the last change, and its samples.
    before: Option<(usize, Spread)>,
    /// What the path carried when it was learned, until the first count is measured.
    learned: Option<Spread>,
    /// The window transfers just started in, which is not theirs.
    settling: bool,
    settled: bool,
    /// The most a window carried, and the least a request took to answer: the path's
    /// bandwidth and delay (as BBR estimates them: Cardwell et al., ACM Queue 14(5), 2016).
    peak: f64,
    answer: Option<Duration>,
}

impl Climb {
    /// At most `most`, from what the path was learned at (its count and what it carried),
    /// or from one.
    pub(crate) fn new(learned: Option<(usize, Spread)>, most: usize) -> Climb {
        let most = most.max(1);
        Climb {
            most,
            target: learned.map_or(1, |(count, _)| count.clamp(1, most)),
            here: Spread::default(),
            before: None,
            learned: learned.map(|(_, spread)| spread),
            settling: true,
            settled: false,
            peak: 0.0,
            answer: None,
        }
    }

    pub(crate) fn target(&self) -> usize {
        self.target
    }

    /// A request answered in `took`: the least is the path's delay.
    pub(crate) fn answered(&mut self, took: Duration) {
        self.answer = Some(self.answer.map_or(took, |a| a.min(took)));
    }

    pub(crate) fn answer(&self) -> Option<Duration> {
        self.answer
    }

    /// A window's end: `rate`, what the transfers carried in it, every one of them busy
    /// throughout or not; `unit`, the bytes one transfer holds (a blob, a range). The count
    /// to go to, where it changes.
    pub(crate) fn window(&mut self, rate: f64, all_busy: bool, unit: f64) -> Option<usize> {
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
        self.here.add(rate);
        self.peak = self.peak.max(rate);
        if self.here.n < SAMPLES {
            return None;
        }
        if let Some(then) = self.learned.take()
            && !same(&then, &self.here)
        {
            // The path changed since it was learned: learned again, from one.
            return self.go(1, None);
        }
        if let Some((count, before)) = self.before
            && !pays(&before, &self.here)
        {
            // Not worth it: back to what it was, and kept.
            self.settled = true;
            self.before = None;
            self.here = before;
            self.target = count;
            return Some(count);
        }
        if self.target >= self.most {
            self.settled = true;
            return None;
        }
        let next = (self.target * 2).max(self.need(unit)).min(self.most);
        let before = (self.target, self.here);
        self.go(next, Some(before))
    }

    fn go(&mut self, to: usize, before: Option<(usize, Spread)>) -> Option<usize> {
        self.before = before;
        self.here = Spread::default();
        self.settling = true;
        let changed = to != self.target;
        self.target = to;
        changed.then_some(to)
    }

    /// The transfers the path needs in flight: its bandwidth-delay product over what one
    /// holds, where its delay is known.
    pub(crate) fn need(&self, unit: f64) -> usize {
        match self.answer {
            Some(answer) if unit > 0.0 => (self.peak * answer.as_secs_f64() / unit).ceil() as usize,
            _ => 0,
        }
    }

    /// The count the path converged to, or the last it measured, and its samples.
    pub(crate) fn learned(&self) -> Option<(usize, Spread)> {
        if self.here.n >= SAMPLES {
            return Some((self.target, self.here));
        }
        self.before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spread(xs: &[f64]) -> Spread {
        let mut s = Spread::default();
        for &x in xs {
            s.add(x);
        }
        s
    }

    /// Welford's mean and variance are the samples' own: the mean, and the sum of squared
    /// differences over n - 1.
    #[test]
    fn a_spread_is_its_samples_mean_and_variance() {
        let xs = [10.0, 12.0, 9.0, 13.0, 11.0];
        let s = spread(&xs);
        let mean = xs.iter().sum::<f64>() / 5.0;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / 4.0;
        assert!((s.mean - mean).abs() < 1e-12);
        assert!((s.variance().unwrap() - var).abs() < 1e-12);
        assert_eq!(spread(&[1.0]).variance(), None, "one sample has no spread");
    }

    /// A doubling pays where its mean is past the one before by more than their pooled
    /// standard deviation, and not at it: the decision is at the spread measured.
    #[test]
    fn a_doubling_pays_past_the_noise_measured() {
        // Each pair 2 apart: variance 2 each, pooled sd √2 = 1.414.
        let before = spread(&[9.0, 11.0]);
        assert!(
            !pays(&before, &spread(&[10.41, 12.41])),
            "1.41 is within the spread"
        );
        assert!(pays(&before, &spread(&[10.42, 12.42])), "1.42 is past it");
        // A noisier path needs more to pay.
        let noisy = spread(&[5.0, 15.0]);
        assert!(!pays(&noisy, &spread(&[14.0, 16.0])));
        assert!(pays(&spread(&[9.9, 10.1]), &spread(&[11.0, 11.2])));
    }

    /// A path never seen starts at one and doubles while each doubling pays, and goes back
    /// to the last that did once one does not; the window a count began in, and one where
    /// a transfer had nothing to do, measure nothing.
    #[test]
    fn a_new_path_starts_at_one_and_doubles_while_it_pays() {
        let mut c = Climb::new(None, 64);
        assert_eq!(c.target(), 1);
        assert_eq!(c.window(10.0, true, 1.0), None, "the first window settles");
        assert_eq!(c.window(10.0, false, 1.0), None, "not all busy");
        assert_eq!(c.window(10.0, true, 1.0), None, "one sample");
        assert_eq!(c.window(10.2, true, 1.0), Some(2));
        assert_eq!(c.window(99.0, true, 1.0), None, "settling");
        c.window(20.0, true, 1.0);
        assert_eq!(c.window(20.2, true, 1.0), Some(4), "twice as much");
        c.window(0.0, true, 1.0);
        c.window(20.1, true, 1.0);
        assert_eq!(c.window(20.3, true, 1.0), Some(2), "no more than the noise");
        assert_eq!(c.learned().map(|(n, s)| (n, s.n)), Some((2, 2)));
        assert_eq!(c.window(1e9, true, 1.0), None, "kept for good");
    }

    /// Once the path's delay is known, a count goes straight to the transfers its
    /// bandwidth-delay product needs, where that is past a doubling; never past the most.
    #[test]
    fn a_count_jumps_to_the_paths_bandwidth_delay_product() {
        let mut c = Climb::new(None, 64);
        // 10 MB/s and 300 ms: 3 MB in flight, 256 KiB a transfer: 12.
        c.answered(Duration::from_millis(400));
        c.answered(Duration::from_millis(300));
        assert_eq!(c.answer(), Some(Duration::from_millis(300)));
        c.window(0.0, true, 0.0);
        c.window(10e6, true, 262_144.0);
        assert_eq!(c.need(262_144.0), 12);
        assert_eq!(c.window(10e6, true, 262_144.0), Some(12));
        let mut c = Climb::new(None, 8);
        c.answered(Duration::from_millis(300));
        c.window(0.0, true, 0.0);
        c.window(10e6, true, 1.0);
        assert_eq!(c.window(10e6, true, 1.0), Some(8), "the most");
    }

    /// A learned path starts where it converged, held to the most; one whose samples are
    /// what it was learned at climbs on from there, and one that changed starts at one.
    #[test]
    fn a_learned_path_starts_where_it_converged_until_it_changed() {
        let then = spread(&[100.0, 104.0]);
        assert_eq!(Climb::new(Some((8, then)), 4).target(), 4, "never past the most");
        let mut c = Climb::new(Some((8, then)), 64);
        assert_eq!(c.target(), 8);
        c.window(0.0, true, 1.0);
        c.window(101.0, true, 1.0);
        assert_eq!(c.window(103.0, true, 1.0), Some(16), "the same path climbs on");
        let mut c = Climb::new(Some((8, then)), 64);
        c.window(0.0, true, 1.0);
        c.window(50.0, true, 1.0);
        assert_eq!(
            c.window(52.0, true, 1.0),
            Some(1),
            "a changed path is learned again"
        );
    }

    /// At its most a count is kept.
    #[test]
    fn a_count_stops_at_its_most() {
        let mut c = Climb::new(Some((4, spread(&[1.0, 1.1]))), 4);
        c.window(0.0, true, 1.0);
        c.window(1.0, true, 1.0);
        assert_eq!(c.window(1.1, true, 1.0), None, "at its most");
        assert_eq!(c.window(100.0, true, 1.0), None, "kept there");
        assert_eq!(c.learned().map(|(n, _)| n), Some(4));
    }
}
