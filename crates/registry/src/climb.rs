//! How many transfers go at once: as many as pay (D226). A pull's connections (fetch.rs)
//! and a push's uploads (push.rs) climb as TCP's slow start does (RFC 5681 §3.1): from one
//! where the path is new, doubling, and straight to what the path needs in flight once its
//! answers say so, its bandwidth-delay product over what one transfer holds; never past
//! the host's cores. A doubling is kept only where it carried more than the path's own
//! noise: the gain between the two counts' windows' samples past their pooled standard
//! deviation (Welford's running variance). A doubling that does not pay goes back to the
//! count before, and that is kept. What a path converged to is learned (crate::links), and
//! the next transfer over it starts there, until its samples say the path changed. Each
//! sample is a [`Window`]'s: as many of the path's answer times as its rate needs to be
//! told from none.

use std::time::{Duration, Instant};

/// Samples at a count before it is judged, and of a window before it ends: the fewest
/// that have a variance.
const SAMPLES: u64 = 2;

/// A running mean and variance, Welford's (Technometrics 4(3), 1962; Knuth TAOCP vol. 2
/// §4.2.2): what a count's windows carried a second, and how much they spread.
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

    /// The standard error of their mean, s/√n (Student, "The probable error of a mean",
    /// Biometrika 6(1), 1908), once there are two.
    pub(crate) fn error(&self) -> Option<f64> {
        Some((self.variance()? / self.n as f64).sqrt())
    }

    /// `self`'s samples and `other`'s as one: Chan, Golub and LeVeque's pairwise
    /// combination ("Updating formulae and a pairwise algorithm for computing sample
    /// variances", Stanford STAN-CS-79-773, 1979): n = nₐ + n_b, δ = mean_b − meanₐ,
    /// mean = meanₐ + δ·n_b/n, M2 = M2ₐ + M2_b + δ²·nₐ·n_b/n; what Welford's running
    /// update over all of them would give.
    pub(crate) fn combined(&self, other: &Spread) -> Spread {
        let n = self.n.saturating_add(other.n);
        if n == 0 {
            return Spread::default();
        }
        let (na, nb, all) = (self.n as f64, other.n as f64, n as f64);
        let d = other.mean - self.mean;
        Spread {
            n,
            mean: self.mean + d * nb / all,
            m2: self.m2 + other.m2 + d * d * na * nb / all,
        }
    }
}

/// How many of its own standard errors a window's rate must be to resolve a doubling that
/// pays in full: `pays` keeps a gain past the pooled standard deviation s of the windows,
/// and a doubling that pays in full gains the count's own rate m (twice the transfers,
/// twice the bytes). The gain measured between two counts' means of [`SAMPLES`] windows
/// each has the standard error s·√(1/SAMPLES + 1/SAMPLES) (the difference of two means,
/// Welch, "The generalization of 'Student's' problem when several different population
/// variances are involved", Biometrika 34(1/2), 1947), so it is past the line by one of
/// its own standard errors, the one-standard-deviation line every test here draws, where
/// m ≥ s·(1 + √(2/SAMPLES)). A window's samples spread as its rate would between windows
/// (s, its standard error), so a window ends once its error times this is under its mean.
fn resolving() -> f64 {
    1.0 + (2.0 / SAMPLES as f64).sqrt()
}

/// How often a window samples what the transfers carried: once each of the path's
/// answer times, the least a request took to answer (as BBR takes the least round trip
/// for the path's delay: Cardwell et al., ACM Queue 14(5), 2016), RFC 6298's first round
/// trip (§2.1, a second) where none is known yet: a sample shorter than a round trip can
/// see none of what a transfer's answer brings.
pub(crate) fn period(answer: Option<Duration>) -> Duration {
    // An answer under the record's microsecond is no measurement of one.
    answer.filter(|a| !a.is_zero()).unwrap_or(crate::patience::FIRST)
}

/// A window of a count's measurement: a sample of what the transfers carried each
/// [`period`], until the samples tell their rate from none, their mean's standard error
/// under the mean, the one-standard-deviation line every test of the climb draws. A
/// doubling that pays in full adds the count's own rate (twice the transfers, twice the
/// bytes), so a window that resolves its rate resolves the gain the climb looks for: as
/// many answer times as the samples need, at least two (the fewest with a variance). One
/// whose transfers carry nothing never ends; nothing it would say is a rate.
#[derive(Debug)]
pub(crate) struct Window {
    /// When it began and the bytes carried then; when it was last sampled, and theirs.
    began: (Instant, u64),
    last: (Instant, u64),
    samples: Spread,
}

impl Window {
    pub(crate) fn new(now: Instant, bytes: u64) -> Window {
        Window {
            began: (now, bytes),
            last: (now, bytes),
            samples: Spread::default(),
        }
    }

    /// How long until it is next sampled, over a path whose answers take `answer`.
    pub(crate) fn due(&self, answer: Option<Duration>) -> Duration {
        period(answer).saturating_sub(self.last.0.elapsed())
    }

    /// A sample at `now`, `bytes` carried by then: once the samples resolve their rate,
    /// what the window carried a second, and the next window begun.
    pub(crate) fn sample(&mut self, now: Instant, bytes: u64) -> Option<f64> {
        let secs = now.checked_duration_since(self.last.0)?.as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        self.samples.add(bytes.saturating_sub(self.last.1) as f64 / secs);
        self.last = (now, bytes);
        let resolved = self
            .samples
            .error()
            .is_some_and(|e| e * resolving() < self.samples.mean);
        if !resolved {
            return None;
        }
        let whole = now.checked_duration_since(self.began.0)?.as_secs_f64();
        let rate = bytes.saturating_sub(self.began.1) as f64 / whole;
        *self = Window::new(now, bytes);
        Some(rate)
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

/// Whether `now` carries less than `then` did by more than their noise: the path got
/// worse at the count it was learned at.
fn worse(then: &Spread, now: &Spread) -> bool {
    pooled(then, now).is_some_and(|s| then.mean - now.mean > s)
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

    /// Whether the count is kept for the rest of the transfer: no window measures more.
    pub(crate) fn settled(&self) -> bool {
        self.settled
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
            && worse(&then, &self.here)
        {
            // The path got worse since it was learned: halved, as TCP's window is on a
            // congestion signal (RFC 5681 §3.1, equation 4), and climbed again from
            // there. One better than it was goes on doubling as it pays.
            return self.go((self.target / 2).max(1), None);
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
    /// what it was learned at, or better, climbs on from there, and one that got worse is
    /// halved and climbs again from there.
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
            Some(4),
            "a path that got worse is halved"
        );
        assert_eq!(c.window(0.0, true, 1.0), None, "and measured there");
        c.window(50.0, true, 1.0);
        assert_eq!(c.window(51.0, true, 1.0), Some(8), "climbing again from it");
        let mut c = Climb::new(Some((8, then)), 64);
        c.window(0.0, true, 1.0);
        c.window(200.0, true, 1.0);
        assert_eq!(
            c.window(204.0, true, 1.0),
            Some(16),
            "a better one climbs on from where it was"
        );
    }

    /// Chan, Golub and LeVeque's combination of two spreads is the spread of all their
    /// samples, as Welford's update over them all makes it; with nothing, the other.
    #[test]
    fn two_spreads_combine_as_their_samples_do() {
        let (a, b) = ([10.0, 12.0, 9.0], [20.0, 22.5, 19.0, 21.0]);
        let all: Vec<f64> = a.iter().chain(&b).copied().collect();
        let (whole, combined) = (spread(&all), spread(&a).combined(&spread(&b)));
        assert_eq!(combined.n, 7);
        assert!((combined.mean - whole.mean).abs() < 1e-12);
        assert!((combined.m2 - whole.m2).abs() < 1e-9);
        assert_eq!(Spread::default().combined(&spread(&b)), spread(&b));
        assert_eq!(spread(&a).combined(&Spread::default()), spread(&a));
    }

    /// A sample a period: the path's least answer, a second where none is known (and a
    /// zero answer is none).
    #[test]
    fn a_window_samples_once_an_answer_time() {
        assert_eq!(period(None), crate::patience::FIRST);
        assert_eq!(period(Some(Duration::ZERO)), crate::patience::FIRST);
        let answer = Duration::from_millis(40);
        assert_eq!(period(Some(answer)), answer);
        let w = Window::new(Instant::now(), 0);
        assert!(w.due(Some(answer)) <= answer);
        assert!(w.due(Some(answer)) > Duration::from_millis(30));
    }

    /// A window ends once its samples' mean is past their standard error: a steady rate
    /// at its second sample, a bursty one only once enough have come, one that carries
    /// nothing never; and its rate is what it carried over all of it.
    #[test]
    fn a_window_lasts_until_its_rate_is_resolved() {
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let mut steady = Window::new(t, 0);
        assert_eq!(steady.sample(at(100), 1000), None, "one sample has no error");
        let rate = steady.sample(at(200), 2100).unwrap();
        assert!((rate - 10_500.0).abs() < 1e-6, "{rate}");
        // Started again where it ended.
        assert_eq!(steady.sample(at(300), 3100), None);
        // Bursts: four periods' bytes in one sample of four. The error must be under half
        // the mean ([`resolving`]: 1 + √(2/2)): five samples (mean 0.4x, error 0.245x) are
        // not, nine are at the line (mean and twice the error both x/3), thirteen past it
        // (mean 0.308x, twice the error 0.266x).
        let mut bursty = Window::new(t, 0);
        let mut bytes = 0;
        let mut ended = None;
        for i in 1..=16u64 {
            if i % 4 == 1 {
                bytes += 4000;
            }
            if let Some(rate) = bursty.sample(at(i * 100), bytes) {
                ended = Some((i, rate));
                break;
            }
        }
        let (n, rate) = ended.unwrap();
        assert!(
            (9..=13).contains(&n),
            "a bursty window needs more samples: ended at {n}"
        );
        assert!((rate - bytes as f64 / (n as f64 / 10.0)).abs() < 1e-6);
        let mut stalled = Window::new(t, 7);
        for i in 1..=50u64 {
            assert_eq!(stalled.sample(at(i * 100), 7), None, "nothing carried");
        }
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

#[cfg(test)]
mod simulated {
    //! A push simulated over a path like containers.rs' pacing registry's: each upload
    //! taken at 4 MiB a second in 64 KiB pieces, a blob of 2 MiB, the client a socket
    //! buffer ahead, answers on loopback; the climb and its windows as push.rs drives them.
    use super::*;
    use crate::links::Learned;

    const PIECE: u64 = 64 << 10;
    const BLOB: u64 = 2 << 20;
    /// 4 MiB a second: a piece every 15.625 ms.
    const PIECE_US: u64 = 15_625;
    const GAP_US: u64 = 300;
    const ANSWER_US: u64 = 100;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Pushes `blobs` from `learned`, at most `most` at once, sockets `buffer` bytes ahead;
    /// what it learned, the count it started at, and the counts it went to.
    fn push(
        rng: &mut Rng,
        learned: Option<&Learned>,
        blobs: u64,
        most: usize,
        buffer: u64,
    ) -> (Learned, usize, Vec<usize>) {
        let base = Instant::now();
        let at = |us: u64| base + Duration::from_micros(us);
        let mut climb = Climb::new(learned.and_then(Learned::climb), most);
        climb.answered(Duration::from_micros(ANSWER_US));
        let first = climb.target();
        let mut went = vec![first];
        // Each upload's blob: when it began.
        let mut slots: Vec<Option<u64>> = vec![None; most];
        let (mut next, mut done, mut bytes_done) = (0u64, 0u64, 0u64);
        let mut t = 0u64;
        let mut window = Window::new(at(0), 0);
        let mut target = first;
        while done < blobs {
            let mut sent = bytes_done;
            let mut busy = 0;
            for (i, slot) in slots.iter_mut().enumerate() {
                loop {
                    match *slot {
                        Some(began) => {
                            let end = began + (BLOB / PIECE) * PIECE_US;
                            if t >= end {
                                done += 1;
                                bytes_done += BLOB;
                                sent += BLOB;
                                *slot = (i < target && next < blobs).then(|| {
                                    next += 1;
                                    end + GAP_US
                                });
                                continue;
                            }
                            if t >= began {
                                let taken = PIECE * ((t - began) / PIECE_US + 1);
                                sent += (taken + buffer).min(BLOB);
                            }
                            busy += 1;
                        }
                        None if i < target && next < blobs => {
                            next += 1;
                            *slot = Some(t);
                            continue;
                        }
                        None => {}
                    }
                    break;
                }
            }
            if !climb.settled()
                && let Some(rate) = window.sample(at(t), sent)
                && let Some(to) = climb.window(rate, busy >= target, BLOB as f64)
            {
                target = to;
                went.push(to);
            }
            // A wait of one answer time, as a condvar's timeout gives it: up to 60 µs late.
            t += ANSWER_US + rng.next() % 60;
        }
        let (count, rate) = climb.learned().unwrap_or_default();
        let own = Learned {
            count,
            rate,
            answer_us: Some(ANSWER_US),
            ..Learned::default()
        };
        (own, first, went)
    }

    fn rng(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    /// Two pushes over one path whose windows are as noisy as containers.rs' pacing
    /// registry makes them (pieces of 64 KiB every 15.6 ms an upload, a socket buffer
    /// ahead, samples a loopback answer apart and up to 60 µs late), each upload carrying
    /// its own, so every doubling pays: for each seed the first climbs to at least four,
    /// its count is what the record keeps, and the second starts at that count and never
    /// goes back to one.
    #[test]
    fn a_second_push_starts_where_a_noisy_first_converged() {
        for seed in 0..32 {
            let mut r = rng(seed);
            let mut record = Learned::default();
            let (own, first, went) = push(&mut r, None, 41, 16, 256 << 10);
            assert_eq!(first, 1, "seed {seed}: a new path starts at one");
            assert!(own.count >= 4, "seed {seed}: settled at {}: {went:?}", own.count);
            record.merge(&own);
            assert_eq!(record.count, own.count, "seed {seed}");
            let (_, first, again) = push(&mut r, Some(&record), 41, 16, 256 << 10);
            assert_eq!(first, record.count, "seed {seed}: {again:?}");
            assert!(
                !again.contains(&1),
                "seed {seed}: back to one: {went:?} then {again:?}"
            );
        }
    }
}
