//! How long shards waits on a registry, learned from that registry's own path as shards
//! runs (D222), never a figure measured on one network and frozen:
//! - how long a lookup (a GET or HEAD without a body) waits for its answer, and a
//!   download for its next byte, before it is cut and asked again (a download resumed
//!   where it was): the cutoff that makes an answer, or a byte, come soonest when cutting
//!   means asking again (Luby, Sinclair and Zuckerman, "Optimal speedup of Las Vegas
//!   algorithms", Inf. Process. Lett. 47(4), 1993), from the times the path has shown
//!   ([`Cutoff`]);
//! - how long a connection takes to make (its name's lookup included) and its TLS
//!   handshake: each the path's own pace, estimated as TCP estimates a retransmission
//!   timeout (RFC 6298 §2, after Jacobson, "Congestion Avoidance and Control", SIGCOMM
//!   1988): the smoothed wait and four times its smoothed deviation, at least a second, a
//!   second before any wait is measured, and doubled for each timeout in a row (§5.5)
//!   until one is answered;
//! - how long a trouble (a passing failure, RFC 9110 §15.6.3 to §15.6.5, or a download
//!   that brings nothing) is ridden out: twice the longest this path has passed (a
//!   doubling search past what is known, Baeza-Yates, Culberson and Rawlins, "Searching
//!   in the plane", Inf. Comput. 106(2), 1993), and never less than the 100 s TCP itself
//!   retransmits to an unanswering peer for before it gives up (RFC 1122 §4.2.3.5, R2),
//!   which is all a path that has passed none is given.
//!
//! What a path showed is kept in the home, in the image store's `links/`, in the one
//! record of the path that its transfers learn into too (D226, crate::links): by its host
//! and port and this host's address on its route there (as a connection there is given
//! it), merged into the record as the client that learned it is let go of (each pace its
//! newer sample's, the times and troubles counted added: [`Shown::merged`]), read as a
//! client first asks the path, and again once it knows the route, so the next pull or push
//! over it starts from what the last showed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// The wait before any is measured, and a pace's least: RFC 6298 §2.1 and §2.4.
pub(crate) const FIRST: Duration = Duration::from_secs(1);
/// The least a trouble is ridden out: TCP's R2 (RFC 1122 §4.2.3.5).
pub(crate) const R2: Duration = Duration::from_secs(100);
/// A [`Sketch`]'s relative accuracy α: DDSketch's (Masson, Rim and Lee, "DDSketch: a fast
/// and fully-mergeable quantile sketch with relative-error guarantees", PVLDB 12(12),
/// 2019), the accuracy its evaluation and Datadog's own sketches use.
const ALPHA: f64 = 0.01;

/// A wait's estimator: RFC 6298 §2's SRTT and RTTVAR, in µs, how many waits it has seen,
/// and when the last was seen, in µs since the Unix epoch (0 where never, or a record of
/// before it was kept), by which two clients' estimators of a path are told apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Pace {
    pub srtt_us: u64,
    pub rttvar_us: u64,
    pub n: u64,
    pub at_us: u64,
}

impl Pace {
    /// A wait seen: the first sets SRTT to it and RTTVAR to its half (§2.2), each after
    /// moves them by α = 1/8 and β = 1/4 (§2.3).
    pub(crate) fn add(&mut self, wait: Duration) {
        let r = u64::try_from(wait.as_micros()).unwrap_or(u64::MAX);
        if self.n == 0 {
            self.srtt_us = r;
            self.rttvar_us = r / 2;
        } else {
            let off = self.srtt_us.abs_diff(r);
            self.rttvar_us = self.rttvar_us.saturating_mul(3).saturating_add(off) / 4;
            self.srtt_us = self.srtt_us.saturating_mul(7).saturating_add(r) / 8;
        }
        self.n = self.n.saturating_add(1);
    }

    /// How long to wait: SRTT + 4·RTTVAR (§2.3), at least [`FIRST`] (§2.4), [`FIRST`]
    /// before any wait is seen (§2.1).
    pub(crate) fn timeout(&self) -> Duration {
        if self.n == 0 {
            return FIRST;
        }
        let us = self.srtt_us.saturating_add(self.rttvar_us.saturating_mul(4));
        Duration::from_micros(us).max(FIRST)
    }

    /// The estimator a record keeps of `kept`, as it holds it now, and `own`, a client's,
    /// which started from `base`: the one of the newer sample, whole. RFC 6298's SRTT and
    /// RTTVAR are its latest samples' (each moves them by an eighth and a quarter, §2.3),
    /// so the newer estimator holds the path as it is now; the waits seen are both
    /// clients' own.
    fn merged(kept: Pace, base: Pace, own: Pace) -> Pace {
        let added = own.n.saturating_sub(base.n);
        let mut out = if own.at_us > kept.at_us { own } else { kept };
        out.n = kept.n.saturating_add(added);
        out
    }
}

/// Times, counted in DDSketch's buckets (Masson, Rim and Lee 2019): bucket k holds the
/// times of (γ^(k-1), γ^k] µs, γ = (1 + α)/(1 − α), so a time is known within a factor γ
/// (2.02% at α = 1%) and every time from 1 µs to `u64::MAX` µs falls in one of at most
/// ⌈ln(2^64)/ln γ⌉ = 2,219 buckets, whatever the times' count. Two sketches merge by
/// adding their counts, exactly (the sketch is "fully mergeable", ibid. §2.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct Sketch(BTreeMap<u32, u64>);

impl Sketch {
    fn gamma() -> f64 {
        (1.0 + ALPHA) / (1.0 - ALPHA)
    }

    /// The bucket of `time`: ⌈log_γ µs⌉, a time under a µs in the first.
    fn bucket(time: Duration) -> u32 {
        let us = u64::try_from(time.as_micros()).unwrap_or(u64::MAX).max(1);
        // A float to an integer saturates; ln of at least 1 is finite and not negative.
        ((us as f64).ln() / Self::gamma().ln()).ceil() as u32
    }

    /// The upper bound of bucket `k`, in µs.
    fn upper(k: u32) -> f64 {
        Self::gamma().powi(i32::try_from(k).unwrap_or(i32::MAX))
    }

    pub(crate) fn add(&mut self, time: Duration) {
        let count = self.0.entry(Self::bucket(time)).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// How many times it holds in bucket `k`.
    fn count(&self, k: u32) -> u64 {
        self.0.get(&k).copied().unwrap_or(0)
    }

    pub(crate) fn total(&self) -> u64 {
        self.0.values().fold(0u64, |n, c| n.saturating_add(*c))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `other`'s times added.
    fn add_all(&mut self, other: &Sketch) {
        for (k, c) in &other.0 {
            let count = self.0.entry(*k).or_insert(0);
            *count = count.saturating_add(*c);
        }
    }

    /// What a record keeps of `kept`, as it holds it now, and `own`, a client's, which
    /// started from `base`: the record's counts and those the client added.
    fn merged(kept: &Sketch, base: &Sketch, own: &Sketch) -> Sketch {
        let mut out = kept.clone();
        for (k, c) in &own.0 {
            let added = c.saturating_sub(base.count(*k));
            if added > 0 {
                let count = out.0.entry(*k).or_insert(0);
                *count = count.saturating_add(added);
            }
        }
        out
    }
}

/// How long one of a path's waits goes before it is cut and asked again: an answer to a
/// lookup (a GET or HEAD without a body, safe to ask again, RFC 9110 §9.2.1), or a
/// download's next byte (which a cut resumes where it was).
///
/// The wait is drawn uniformly from [t, 2t), t·2^k after k cuts in a row (RFC 6298
/// §5.5's doubling, which reaches past anything the path has shown), and t is the cutoff
/// that makes the answer come soonest when every cut asks again. Luby, Sinclair and
/// Zuckerman (1993) prove that, for a known distribution F of the time to an answer and
/// restarts that each draw anew, the optimal strategy cuts at one fixed t*, the one that
/// minimises E(t) = ∫0^t (1 − F(u)) du / F(t): the expected time of a renewal process
/// whose cycles each cost min(X, t) and succeed with F(t) (renewal-reward). shards keeps
/// that renewal and measures the rest:
/// - F is the path's own, as Kaplan and Meier ("Nonparametric estimation from incomplete
///   observations", JASA 53(282), 1958) estimate it from the times that were answered
///   ([`answered`](Self::answered)) and those cut before an answer came
///   ([`cut`](Self::cut)): a cut time says only that its answer came later, if ever, so a
///   request that never answers keeps F below 1, and E grows without end past what the
///   path answers in;
/// - an asking again is not assumed to draw anew: what a cut costs until its answer, R,
///   is measured ([`recovered`](Self::recovered)), the mean time from a cut to the answer
///   of an asking begun after it, so a path whose slow answers come in spells, where the
///   asking again is slow too, is cut later. The expected time of a wait W is then
///   ∫0^W S + S(W)·R, S = 1 − F; before any R is measured, it is Luby's (asking again is
///   a fresh draw): E[∫0^W S] / E[F(W)];
/// - the wait is drawn from [t, 2t), and t chosen for that draw: a fixed cutoff cuts
///   every asking at it, so F past it is never seen, and t could only ever fall (replayed
///   on M486's records: t fell below the path's p99 and lost more than RFC 6298's
///   estimator). The draw spreads the cuts over [t, 2t), Kaplan-Meier sees past t, and
///   t rises where the path answers later. For an answer it is the trouble's pause
///   (registry.rs `Trouble`, a uniform share of how long the trouble has lasted, t at
///   the first cut) spent listening to the asking it cut instead of asleep: an overdue
///   answer is asked again at once (`Error::is_overdue`), at the moment the pause would
///   have ended, and an answer that comes meanwhile is taken;
/// - t is an occupied bucket's upper bound: E(γt) ≤ γ·E(t), as ∫t^γt S ≤ (γ − 1)·t·S(t) ≤
///   (γ − 1)·∫0^t S, so the wait at most 2.02% slower in expectation than the best of
///   the times as bucketed;
/// - t is at least a second, RFC 6298 §2.4's least retransmission timeout, which keeps a
///   timeout conservative against what its samples do not show (it cites Allman and
///   Paxson, "On estimating end-to-end network path properties", SIGCOMM 1999: a large
///   minimum avoids spurious retransmissions). Seen here without it: this crate's tests,
///   on a host at a load of 190, cut healthy loopback answers their paths had learned
///   were quick; and replayed on M486's records, the floor lost on the Studio in one
///   model and won on Fedora in the other (D222);
/// - before any answer, a second (RFC 6298 §2.1), doubled for each cut in a row: no wait
///   at all would hold a request that is never answered forever, and the doubling reaches
///   any answer within twice its time (Baeza-Yates, Culberson and Rawlins 1993).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Cutoff {
    /// The times answered.
    pub answered: Sketch,
    /// The times waited before a cut, the answer not come.
    pub cut: Sketch,
    /// The times from a cut to its answer, in µs in all, and how many.
    pub again_us: u64,
    pub agains: u64,
}

/// One of the steps S is constant on: where it starts, in µs; S there; A = ∫0^x S and
/// IA = ∫0^x A at its start.
#[derive(Debug, Clone, Copy)]
struct Step {
    start: f64,
    s: f64,
    a: f64,
    ia: f64,
}

impl Step {
    /// A and IA at `x`, within the step.
    fn at(&self, x: f64) -> (f64, f64) {
        let h = x - self.start;
        (self.a + self.s * h, self.ia + self.a * h + self.s * h * h / 2.0)
    }
}

impl Cutoff {
    pub(crate) fn answered(&mut self, took: Duration) {
        self.answered.add(took);
    }

    pub(crate) fn cut(&mut self, waited: Duration) {
        self.cut.add(waited);
    }

    /// A cut's answer came `took` after it.
    pub(crate) fn recovered(&mut self, took: Duration) {
        let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.again_us = self.again_us.saturating_add(us);
        self.agains = self.agains.saturating_add(1);
    }

    /// The cutoff t, before any answer `None`.
    pub(crate) fn base(&self) -> Option<Duration> {
        if self.answered.is_empty() {
            return None;
        }
        let again = (self.agains > 0).then(|| self.again_us as f64 / self.agains as f64);
        // Kaplan-Meier's S, an answer's bucket before a cut's within one: the steps it is
        // constant on, from 0 and from each occupied bucket's upper bound.
        let mut risk = self.answered.total().saturating_add(self.cut.total()) as f64;
        let mut steps = vec![Step {
            start: 0.0,
            s: 1.0,
            a: 0.0,
            ia: 0.0,
        }];
        let mut buckets: Vec<u32> = self.answered.0.keys().chain(self.cut.0.keys()).copied().collect();
        buckets.sort_unstable();
        buckets.dedup();
        for k in buckets {
            let last = steps.last().copied()?;
            let b = Sketch::upper(k);
            let (a, ia) = last.at(b);
            let answered = self.answered.count(k) as f64;
            let mut s = last.s;
            if answered > 0.0 && risk > 0.0 {
                s *= 1.0 - answered / risk;
            }
            risk -= answered + self.cut.count(k) as f64;
            steps.push(Step { start: b, s, a, ia });
        }
        let at = |x: f64| {
            let i = steps.partition_point(|step| step.start <= x).saturating_sub(1);
            steps.get(i).map(|step| step.at(x))
        };
        // At least RFC 6298 §2.4's second, and each occupied bucket's upper bound past it.
        let least = FIRST.as_secs_f64() * 1e6;
        let past = steps.iter().skip(1).map(|step| step.start).filter(|t| *t > least);
        let mut best: Option<(f64, f64)> = None;
        for t in std::iter::once(least).chain(past) {
            let (Some((a0, ia0)), Some((a1, ia1))) = (at(t), at(2.0 * t)) else {
                continue;
            };
            // Over W uniform in [t, 2t): E[∫0^W S] and E[S(W)]·t.
            let (area, unanswered) = ((ia1 - ia0) / t, a1 - a0);
            let e = match again {
                Some(r) => area + unanswered / t * r,
                None => {
                    let answers = 1.0 - unanswered / t;
                    if answers <= 0.0 {
                        continue;
                    }
                    area / answers
                }
            };
            if e.is_finite() && best.is_none_or(|(least, _)| e < least) {
                best = Some((e, t));
            }
        }
        best.map(|(_, t)| Duration::from_micros(t.ceil() as u64))
    }

    /// How long to wait after `timeouts` cuts in a row, `u` a uniform draw from [0, 1).
    pub(crate) fn wait(&self, timeouts: u32, u: f64) -> Duration {
        let t = backed_off(self.base().unwrap_or(FIRST), timeouts);
        Duration::try_from_secs_f64(t.as_secs_f64() * (1.0 + u.clamp(0.0, 1.0))).unwrap_or(Duration::MAX)
    }

    /// What a record keeps of `kept`, as it holds it now, and `own`, a client's, which
    /// started from `base`: the record's times and those the client added.
    fn merged(kept: &Cutoff, base: &Cutoff, own: &Cutoff) -> Cutoff {
        Cutoff {
            answered: Sketch::merged(&kept.answered, &base.answered, &own.answered),
            cut: Sketch::merged(&kept.cut, &base.cut, &own.cut),
            again_us: kept
                .again_us
                .saturating_add(own.again_us.saturating_sub(base.again_us)),
            agains: kept.agains.saturating_add(own.agains.saturating_sub(base.agains)),
        }
    }
}

/// Now, in µs since the Unix epoch: when a wait was seen, which is compared across
/// processes; 0 where the clock says a time before it.
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// `timeout` doubled for each of `timeouts` in a row (RFC 6298 §5.5).
pub(crate) fn backed_off(timeout: Duration, timeouts: u32) -> Duration {
    timeout.saturating_mul(1u32.checked_shl(timeouts.min(31)).unwrap_or(u32::MAX))
}

/// What a path has shown, as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Shown {
    /// Its answers' times, of lookups (GETs and HEADs without a body).
    pub answers: Cutoff,
    /// Its downloads' silences: the times between a body's reads.
    pub silences: Cutoff,
    /// Its connections' waits: a TCP connection's, its name's lookup included, and its
    /// TLS handshake's.
    pub connect: Pace,
    pub handshake: Pace,
    /// The longest trouble it passed, in µs, and how many it passed.
    pub trouble_us: u64,
    pub troubles: u64,
}

impl Shown {
    /// How long a trouble of this path's is ridden out.
    pub(crate) fn budget(&self) -> Duration {
        Duration::from_micros(self.trouble_us).saturating_mul(2).max(R2)
    }

    /// What a record keeps of `self`, as it holds it now (which other clients may have
    /// merged into since it was read), and `own`, a client's, which started from `base`,
    /// what it read: the times each counted added ([`Cutoff::merged`]); each pace the
    /// newer sample's ([`Pace::merged`]); the longest trouble the longer; the troubles
    /// passed the record's and those the client passed itself.
    pub(crate) fn merged(self, base: &Shown, own: &Shown) -> Shown {
        Shown {
            answers: Cutoff::merged(&self.answers, &base.answers, &own.answers),
            silences: Cutoff::merged(&self.silences, &base.silences, &own.silences),
            connect: Pace::merged(self.connect, base.connect, own.connect),
            handshake: Pace::merged(self.handshake, base.handshake, own.handshake),
            trouble_us: self.trouble_us.max(own.trouble_us),
            troubles: self
                .troubles
                .saturating_add(own.troubles.saturating_sub(base.troubles)),
        }
    }
}

/// Which of a path's waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// A lookup's (a GET or HEAD without a body), for its answer.
    Answer,
    /// A connection's, its name's lookup included.
    Connect,
    /// A TLS handshake's.
    Handshake,
}

/// A path's state while shards runs: what it has shown, and what it held when it was
/// read; the timeouts in a row of each of its waits and of its silences, and when the
/// first of those of its answers and its silences in a row was (a cut not yet answered by
/// an asking begun after it); and whether it has shown anything since it was read.
#[derive(Debug, Clone, Default)]
struct Path {
    shown: Shown,
    base: Shown,
    answer_timeouts: u32,
    connect_timeouts: u32,
    handshake_timeouts: u32,
    silence_timeouts: u32,
    answer_cut: Option<Instant>,
    silence_cut: Option<Instant>,
    changed: bool,
}

impl Path {
    /// A connect's or a handshake's estimator, and its timeouts in a row.
    fn pace(&mut self, wait: Wait) -> Option<(&mut Pace, &mut u32)> {
        match wait {
            Wait::Answer => None,
            Wait::Connect => Some((&mut self.shown.connect, &mut self.connect_timeouts)),
            Wait::Handshake => Some((&mut self.shown.handshake, &mut self.handshake_timeouts)),
        }
    }
}

/// What a download's body showed of its path's silences, as it is let go of.
#[derive(Debug)]
pub(crate) struct Silenced {
    /// Its silences that ended with bytes.
    pub seen: Sketch,
    /// When it began, and when its first bytes came.
    pub began: Instant,
    pub first: Option<Instant>,
    /// Whether its last read timed out (`Some(true)`, after `waited`, at `cut`), or
    /// brought bytes after one or from the start (`Some(false)`).
    pub timed_out: Option<bool>,
    pub waited: Duration,
    pub cut: Option<Instant>,
}

/// A URL's path's name: its host, without an IPv6 address's brackets, and port, as
/// crate::links keeps a path by.
pub(crate) fn path_of(url: &crate::url::Url) -> String {
    format!(
        "{}:{}",
        url.host().trim_start_matches('[').trim_end_matches(']'),
        url.port()
    )
}

/// The paths a client has asked, by their host and port ([`path_of`]), the address this
/// host's route to each leaves from, as its connections were given it, and the directory
/// they are kept in, once it is known.
#[derive(Debug, Default)]
pub(crate) struct Paths {
    paths: Mutex<HashMap<String, Path>>,
    routes: Mutex<HashMap<String, std::net::IpAddr>>,
    kept: OnceLock<PathBuf>,
    /// The draw every wait takes, where a test says, in place of a uniform one.
    #[cfg(test)]
    drawn: Option<f64>,
}

impl Paths {
    fn with<T>(&self, key: &str, f: impl FnOnce(&mut Path) -> T) -> T {
        let mut paths = self.paths.lock().unwrap_or_else(PoisonError::into_inner);
        let path = paths.entry(key.to_string()).or_insert_with(|| {
            let shown = self.read(key).unwrap_or_default();
            Path {
                shown: shown.clone(),
                base: shown,
                ..Path::default()
            }
        });
        f(path)
    }

    /// Keeps the paths in `root`'s `links/` (`root` an image store's directory): those
    /// asked already start from what they showed before, as [`learn`](Self::learn) has it,
    /// and those asked after are read as they are first asked.
    pub(crate) fn keep_in(&self, root: &FsPath) {
        if self.kept.set(root.to_path_buf()).is_err() {
            return;
        }
        let keys: Vec<String> = self
            .paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for key in keys {
            if let Some(shown) = self.read(&key) {
                self.learn(&key, shown);
            }
        }
    }

    /// The path to `key` leaves this host from `from`: its record is the one of that route,
    /// read now where nothing has been shown since it was first asked.
    pub(crate) fn located(&self, key: &str, from: std::net::IpAddr) {
        let new = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_string(), from)
            .is_none_or(|was| was != from);
        if new && let Some(shown) = self.read(key) {
            self.learn(key, shown);
        }
    }

    fn file(&self, key: &str) -> Option<PathBuf> {
        let from = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied();
        Some(crate::links::file(self.kept.get()?, key, from))
    }

    /// What `key`'s record holds of it, where it holds what this build reads.
    fn read(&self, key: &str) -> Option<Shown> {
        Some(crate::links::read(&self.file(key)?)?.shown)
    }

    /// Merges `shown`, what `key` has shown since it held `base`, into its record.
    fn write(&self, key: &str, base: &Shown, shown: &Shown) -> std::io::Result<()> {
        let Some(file) = self.file(key) else {
            return Ok(());
        };
        crate::links::update(&file, |record| {
            record.shown = std::mem::take(&mut record.shown).merged(base, shown);
        })
    }

    /// What the path has shown.
    #[cfg(test)]
    pub(crate) fn shown(&self, key: &str) -> Shown {
        self.with(key, |p| p.shown.clone())
    }

    /// Every wait drawn as `u` in [0, 1) says, in place of a uniform draw.
    #[cfg(test)]
    pub(crate) fn draw_as(&mut self, u: f64) {
        self.drawn = Some(u);
    }

    /// A uniform draw from [0, 1), for a wait.
    fn draw(&self) -> f64 {
        #[cfg(test)]
        if let Some(u) = self.drawn {
            return u;
        }
        crate::registry::jitter()
    }

    /// Starts the path from what it showed before, as kept; what it has shown since this
    /// client asked it is kept over that.
    pub(crate) fn learn(&self, key: &str, shown: Shown) {
        self.with(key, |p| {
            if !p.changed {
                p.shown = shown.clone();
                p.base = shown;
            }
        });
    }

    /// How long `key`'s `wait` goes before it is cut: an answer's its [`Cutoff`]'s, a
    /// connect's or a handshake's RFC 6298's timeout of its estimator; each doubled for
    /// each timeout in a row (§5.5).
    pub(crate) fn timeout(&self, key: &str, wait: Wait) -> Duration {
        let u = self.draw();
        self.with(key, |p| match p.pace(wait) {
            Some((pace, timeouts)) => backed_off(pace.timeout(), *timeouts),
            None => p.shown.answers.wait(p.answer_timeouts, u),
        })
    }

    /// `key`'s `wait` ended after `took`, in time.
    pub(crate) fn waited(&self, key: &str, wait: Wait, took: Duration) {
        let now = Instant::now();
        self.with(key, |p| {
            p.changed = true;
            if let Some((pace, timeouts)) = p.pace(wait) {
                pace.add(took);
                pace.at_us = now_us();
                *timeouts = 0;
                return;
            }
            p.shown.answers.answered(took);
            p.answer_timeouts = 0;
            // The answer of an asking begun after the cuts in a row began: what they cost.
            if let Some(cut) = p.answer_cut
                && now.checked_sub(took).is_some_and(|began| began >= cut)
            {
                p.shown.answers.recovered(now.saturating_duration_since(cut));
                p.answer_cut = None;
            }
        });
    }

    /// `key`'s `wait` was cut after `waited`, its time out.
    pub(crate) fn timed_out(&self, key: &str, wait: Wait, waited: Duration) {
        let now = Instant::now();
        self.with(key, |p| {
            if let Some((_, timeouts)) = p.pace(wait) {
                *timeouts = timeouts.saturating_add(1);
                return;
            }
            p.changed = true;
            p.shown.answers.cut(waited);
            p.answer_timeouts = p.answer_timeouts.saturating_add(1);
            p.answer_cut.get_or_insert(now);
        });
    }

    /// How long a download goes without a byte before it is cut: its silences'
    /// [`Cutoff`]'s wait.
    pub(crate) fn silence(&self, key: &str) -> Duration {
        let u = self.draw();
        self.with(key, |p| p.shown.silences.wait(p.silence_timeouts, u))
    }

    /// A download's silences, as its body is let go of.
    pub(crate) fn silenced(&self, key: &str, body: &Silenced) {
        self.with(key, |p| {
            if !body.seen.is_empty() {
                p.changed = true;
                p.shown.silences.answered.add_all(&body.seen);
            }
            // A body begun after the cuts in a row began brought a byte: what they cost.
            if let (Some(cut), Some(first)) = (p.silence_cut, body.first)
                && body.began >= cut
            {
                p.changed = true;
                p.shown.silences.recovered(first.saturating_duration_since(cut));
                p.silence_cut = None;
            }
            match body.timed_out {
                Some(true) => {
                    p.changed = true;
                    p.shown.silences.cut(body.waited);
                    p.silence_timeouts = p.silence_timeouts.saturating_add(1);
                    if let Some(at) = body.cut {
                        p.silence_cut.get_or_insert(at);
                    }
                }
                Some(false) => p.silence_timeouts = 0,
                None => {}
            }
        });
    }

    /// How long a trouble is ridden out.
    pub(crate) fn budget(&self, key: &str) -> Duration {
        self.with(key, |p| p.shown.budget())
    }

    /// A trouble that lasted `lasted` passed.
    pub(crate) fn passed(&self, key: &str, lasted: Duration) {
        let us = u64::try_from(lasted.as_micros()).unwrap_or(u64::MAX);
        self.with(key, |p| {
            p.shown.trouble_us = p.shown.trouble_us.max(us);
            p.shown.troubles = p.shown.troubles.saturating_add(1);
            p.changed = true;
        });
    }
}

/// What the paths showed is kept as the client lets them go; a path whose file cannot be
/// written is learned again by the next.
impl Drop for Paths {
    fn drop(&mut self) {
        let paths = std::mem::take(self.paths.get_mut().unwrap_or_else(PoisonError::into_inner));
        for (key, path) in paths {
            if path.changed {
                let _ = self.write(&key, &path.base, &path.shown);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6298's estimator: a second before any wait and at least a second, the first
    /// wait and its half, then the smoothed wait and four smoothed deviations; doubled for
    /// each timeout in a row.
    #[test]
    fn a_pace_is_rfc_6298s() {
        let mut pace = Pace::default();
        assert_eq!(pace.timeout(), FIRST);
        pace.add(Duration::from_millis(100));
        assert_eq!((pace.srtt_us, pace.rttvar_us), (100_000, 50_000));
        // 100 + 4·50 ms: under a second.
        assert_eq!(pace.timeout(), FIRST);
        pace.add(Duration::from_secs(2));
        // RTTVAR = (3·50 + 1900)/4 ms, SRTT = (7·100 + 2000)/8 ms.
        assert_eq!((pace.srtt_us, pace.rttvar_us), (337_500, 512_500));
        assert_eq!(pace.timeout(), Duration::from_micros(337_500 + 4 * 512_500));
        assert_eq!(backed_off(FIRST, 0), FIRST);
        assert_eq!(backed_off(FIRST, 3), FIRST * 8);
        assert_eq!(backed_off(FIRST, 200), FIRST * (1 << 31));
    }

    /// A sketch's bucket holds a time within a factor γ of its upper bound, and merges as
    /// counts added: the record's, and what a client added to what it read.
    #[test]
    fn a_sketch_keeps_times_within_its_accuracy() {
        let gamma = Sketch::gamma();
        for us in [1u64, 2, 999, 1_000_000, 1_500_000, 3_600_000_000, u64::MAX] {
            let upper = Sketch::upper(Sketch::bucket(Duration::from_micros(us)));
            assert!(upper >= us as f64 * (1.0 - 1e-12), "{us}: {upper}");
            assert!(upper < us as f64 * gamma * (1.0 + 1e-12), "{us}: {upper}");
        }
        assert!(Sketch::bucket(Duration::MAX) <= 2219);
        let (mut kept, mut base, mut own) = (Sketch::default(), Sketch::default(), Sketch::default());
        kept.add(Duration::from_millis(100));
        kept.add(Duration::from_secs(2));
        base.add(Duration::from_millis(100));
        own.add(Duration::from_millis(100));
        own.add(Duration::from_millis(100));
        own.add(Duration::from_secs(5));
        let merged = Sketch::merged(&kept, &base, &own);
        assert_eq!(merged.total(), 4, "the record's two and the client's own two");
        assert_eq!(merged.count(Sketch::bucket(Duration::from_millis(100))), 2);
        assert_eq!(merged.count(Sketch::bucket(Duration::from_secs(5))), 1);
    }

    /// `n` answers of `at` in a cutoff.
    fn answers(cutoff: &mut Cutoff, n: usize, at: Duration) {
        for _ in 0..n {
            cutoff.answered(at);
        }
    }

    /// Luby's cutoff: where nine answers in ten take 2 s and the tenth 100 s, asking
    /// again at 2 s beats waiting (an asking again a fresh draw: E ≈ 2.3 s, against 11.8 s
    /// for waiting); where an asking again is measured to take an hour, waiting beats it;
    /// a path that answers alike, with one request never answered, is cut just past its
    /// answers, and one that answers in 100 ms at RFC 6298's least second. The cutoff is a
    /// bucket's upper bound, within γ of the time.
    #[test]
    fn the_cutoff_makes_answers_come_soonest() {
        let fast = Duration::from_secs(2);
        let slow = Duration::from_secs(100);
        let within = |t: Option<Duration>, of: Duration| {
            let t = t.unwrap().as_secs_f64();
            t >= of.as_secs_f64() && t <= of.as_secs_f64() * Sketch::gamma() + 1e-6
        };
        let mut spread = Cutoff::default();
        assert_eq!(spread.base(), None);
        answers(&mut spread, 90, fast);
        answers(&mut spread, 10, slow);
        assert!(within(spread.base(), fast), "{:?}", spread.base());
        let mut costly = spread.clone();
        costly.recovered(Duration::from_secs(3600));
        assert!(within(costly.base(), slow), "{:?}", costly.base());
        let mut alike = Cutoff::default();
        answers(&mut alike, 99, Duration::from_millis(2500));
        alike.cut(Duration::from_secs(30));
        assert!(
            within(alike.base(), Duration::from_millis(2500)),
            "{:?}",
            alike.base()
        );
        let mut quick = Cutoff::default();
        answers(&mut quick, 99, Duration::from_millis(100));
        assert_eq!(quick.base(), Some(FIRST));
        // Drawn from [t, 2t), doubled for each cut in a row; a second before any answer.
        let t = spread.base().unwrap();
        assert_eq!(spread.wait(0, 0.0), t);
        assert_eq!(spread.wait(0, 0.5), t.mul_f64(1.5));
        assert_eq!(spread.wait(2, 0.0), t * 4);
        assert_eq!(Cutoff::default().wait(0, 0.0), FIRST);
        assert_eq!(Cutoff::default().wait(3, 0.0), FIRST * 8);
    }

    /// A path that has passed no trouble rides one out for TCP's R2, one that has passed
    /// longer ones for twice the longest; its waits double while they time out, and come
    /// back as one is answered, an answer's to its cutoff and a connect's or handshake's
    /// to RFC 6298's; what was kept starts it, unless it has shown something since.
    #[test]
    fn a_path_learns_its_waits() {
        let mut paths = Paths::default();
        paths.draw_as(0.0);
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(3));
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(80));
        assert_eq!(paths.budget("r:443"), Duration::from_secs(160));
        assert_eq!(paths.shown("r:443").troubles, 2);
        for wait in [Wait::Answer, Wait::Connect, Wait::Handshake] {
            assert_eq!(paths.timeout("s:443", wait), FIRST, "{wait:?}");
            paths.timed_out("s:443", wait, FIRST);
            paths.timed_out("s:443", wait, FIRST * 2);
            assert_eq!(paths.timeout("s:443", wait), FIRST * 4, "{wait:?}");
            paths.waited("s:443", wait, Duration::from_secs(3));
        }
        // 3 + 4·1.5 s, RFC 6298's.
        assert_eq!(paths.timeout("s:443", Wait::Connect), Duration::from_secs(9));
        assert_eq!(paths.timeout("s:443", Wait::Handshake), Duration::from_secs(9));
        // Its one answer's bucket: past it, nothing was ever answered.
        let answer = paths.timeout("s:443", Wait::Answer).as_secs_f64();
        assert!((3.0..=3.0 * Sketch::gamma()).contains(&answer), "{answer}");
        // Each wait its own: a connect's timeouts are not an answer's.
        paths.timed_out("v:443", Wait::Connect, FIRST);
        assert_eq!(paths.timeout("v:443", Wait::Connect), FIRST * 2);
        assert_eq!(paths.timeout("v:443", Wait::Answer), FIRST);
        assert_eq!(paths.timeout("v:443", Wait::Handshake), FIRST);
        let kept = Shown {
            trouble_us: 200_000_000,
            ..Shown::default()
        };
        paths.learn("t:443", kept.clone());
        assert_eq!(paths.budget("t:443"), Duration::from_secs(400));
        paths.learn("s:443", kept);
        assert_eq!(paths.budget("s:443"), R2, "what it showed since is kept");
        let now = Instant::now();
        let body = |timed_out| Silenced {
            seen: Sketch::default(),
            began: now,
            first: None,
            timed_out,
            waited: FIRST,
            cut: Some(now),
        };
        assert_eq!(paths.silence("u:443"), FIRST);
        paths.silenced("u:443", &body(Some(true)));
        assert_eq!(paths.silence("u:443"), FIRST * 2);
        paths.silenced("u:443", &body(Some(false)));
        assert_eq!(paths.silence("u:443"), FIRST);
        assert_eq!(paths.shown("u:443").silences.cut.total(), 1);
    }

    /// What a cut cost is measured from the cut to the answer of an asking begun after
    /// it, an answer of one begun before it not counted; and from a silence's cut to the
    /// first byte of a body begun after it.
    #[test]
    fn what_a_cut_costs_is_measured() {
        let paths = Paths::default();
        paths.timed_out("r:443", Wait::Answer, FIRST);
        std::thread::sleep(Duration::from_millis(20));
        // Asked before the cut, a minute ago.
        paths.waited("r:443", Wait::Answer, Duration::from_secs(60));
        assert_eq!(paths.shown("r:443").answers.agains, 0);
        paths.waited("r:443", Wait::Answer, Duration::from_millis(1));
        let answers = paths.shown("r:443").answers;
        assert_eq!(answers.agains, 1);
        assert!(answers.again_us >= 20_000, "{answers:?}");
        let cut = Instant::now();
        let mut body = Silenced {
            seen: Sketch::default(),
            began: cut,
            first: None,
            timed_out: Some(true),
            waited: FIRST,
            cut: Some(cut),
        };
        paths.silenced("r:443", &body);
        std::thread::sleep(Duration::from_millis(20));
        body.began = Instant::now();
        body.first = Some(Instant::now());
        body.timed_out = Some(false);
        body.seen.add(Duration::from_millis(5));
        paths.silenced("r:443", &body);
        let silences = paths.shown("r:443").silences;
        assert_eq!((silences.agains, silences.answered.total()), (1, 1));
        assert!(silences.again_us >= 20_000, "{silences:?}");
    }

    /// What a client's paths showed is kept in the home as it lets them go, and the next
    /// client's start from it: those asked before it knew where, and those after; a path
    /// that showed nothing is not written.
    #[test]
    fn what_paths_show_is_kept_for_the_next() {
        let dir = shards_testdir::TempDir::new("paths").unwrap();
        let paths = Paths::default();
        paths.passed("r:443", Duration::from_secs(150));
        paths.waited("[::1]:5000", Wait::Answer, Duration::from_secs(2));
        let _ = paths.budget("quiet:443");
        paths.keep_in(&dir);
        drop(paths);
        let kept = dir.join("links");
        let mut names: Vec<String> = std::fs::read_dir(&kept)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|name| name != ".lock")
            .collect();
        names.sort();
        assert_eq!(names, ["%5B%3A%3A1%5D%3A5000@unrouted", "r%3A443@unrouted"]);
        let next = Paths::default();
        let _ = next.budget("r:443");
        next.keep_in(&dir);
        assert_eq!(
            next.budget("r:443"),
            Duration::from_secs(300),
            "asked before it knew where"
        );
        assert_eq!(
            next.shown("[::1]:5000").answers.answered.total(),
            1,
            "asked after"
        );
    }

    /// Two clients that asked one path at once, each from what its record held, both
    /// keep what they showed as they are let go: the troubles each passed added, the
    /// longest kept, the answers each counted added, and each pace the one of the newer
    /// sample, its waits both clients' own.
    #[test]
    fn clients_at_once_both_keep_what_they_showed() {
        let dir = shards_testdir::TempDir::new("paths-merge").unwrap();
        let before = Paths::default();
        before.keep_in(&dir);
        before.passed("r:443", Duration::from_secs(10));
        before.waited("r:443", Wait::Connect, Duration::from_millis(40));
        before.waited("r:443", Wait::Answer, Duration::from_millis(200));
        drop(before);
        let (first, second) = (Paths::default(), Paths::default());
        first.keep_in(&dir);
        second.keep_in(&dir);
        assert_eq!(first.shown("r:443").troubles, 1);
        assert_eq!(second.shown("r:443").troubles, 1);
        first.passed("r:443", Duration::from_secs(150));
        first.waited("r:443", Wait::Answer, Duration::from_millis(200));
        second.passed("r:443", Duration::from_secs(30));
        second.passed("r:443", Duration::from_secs(20));
        second.waited("r:443", Wait::Connect, Duration::from_millis(80));
        second.waited("r:443", Wait::Answer, Duration::from_millis(600));
        second.timed_out("r:443", Wait::Answer, Duration::from_secs(2));
        // Let go the other way round: both kept whatever the order.
        drop(second);
        drop(first);
        let kept = crate::links::read(&crate::links::file(&dir, "r:443", None))
            .unwrap()
            .shown;
        assert_eq!(
            kept.troubles, 4,
            "one before, one of the first's, two of the second's"
        );
        assert_eq!(kept.trouble_us, 150_000_000);
        let answers = &kept.answers;
        assert_eq!(answers.answered.total(), 3, "the one before and each client's");
        assert_eq!(
            answers.answered.count(Sketch::bucket(Duration::from_millis(200))),
            2
        );
        assert_eq!(
            answers.answered.count(Sketch::bucket(Duration::from_millis(600))),
            1
        );
        assert_eq!(answers.cut.total(), 1, "the second's");
        assert_eq!(kept.connect.n, 2, "the one before and the second's");
        assert_eq!(kept.connect.srtt_us, (7 * 40_000 + 80_000) / 8);
    }

    /// A record of before answers were counted is read whole, what it held of answers
    /// and silences (RFC 6298's estimators) left.
    #[test]
    fn an_older_record_is_read() {
        let old = br#"{"shown":{"answer":{"srtt_us":5,"rttvar_us":2,"n":1},"silence":{"srtt_us":0,"rttvar_us":0,"n":0},"trouble_us":7,"troubles":1}}"#;
        let record: crate::links::PathRecord = serde_json::from_slice(old).unwrap();
        assert_eq!(record.shown.answers, Cutoff::default());
        assert_eq!(record.shown.connect, Pace::default());
        assert_eq!(record.shown.trouble_us, 7);
    }

    /// A path's record is its route's: one asked before its route was known starts from
    /// its record once a connection says the route, unless it has shown something since;
    /// and what it shows then is kept in that route's record.
    #[test]
    fn a_path_is_kept_by_its_route() {
        let dir = shards_testdir::TempDir::new("routes").unwrap();
        let wired = std::net::IpAddr::from([192, 168, 1, 91]);
        let kept = Shown {
            trouble_us: 200_000_000,
            ..Shown::default()
        };
        crate::links::update(&crate::links::file(&dir, "r:443", Some(wired)), |r| {
            r.shown = kept
        })
        .unwrap();
        let paths = Paths::default();
        paths.keep_in(&dir);
        assert_eq!(paths.budget("r:443"), R2, "the route not known yet");
        paths.located("r:443", wired);
        assert_eq!(
            paths.budget("r:443"),
            Duration::from_secs(400),
            "its route's record"
        );
        paths.passed("r:443", Duration::from_secs(300));
        drop(paths);
        let record = crate::links::read(&crate::links::file(&dir, "r:443", Some(wired))).unwrap();
        assert_eq!(record.shown.trouble_us, 300_000_000);
    }
}
