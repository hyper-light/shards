//! How long shards waits on a registry, learned from that registry's own path as shards
//! runs (D222), never a figure measured on one network and frozen:
//! - how long a lookup (a GET or HEAD without a body) waits for its answer, and a
//!   download for its next byte: while its connection lives, and past nothing longer than
//!   the slowest of its kind the path has shown, its slowest answer's or its slowest
//!   silence's, past which it is cut and asked again (a download resumed where it was). The
//!   person's rule (2026-11-13): "The cold start, and every wait after a run-out, follows the
//!   party's liveness and progress. After that the bound is the slowest answer seen." A
//!   path that has shown none of a kind waits for it as TCP and TLS keep its connection
//!   alone; so does its wait of that kind after one ran out, until one ends ([`Bounded`]). A
//!   stall is a gap with no progress, never an answer's whole length: an answer's first
//!   byte, then each silence, a read's own wait for bytes. A run-out learns nothing;
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
//! it), merged into the record as the client that learned it is let go of (each slowest
//! the slower, each pace its newer sample's, the troubles counted added:
//! [`Shown::merged`]), read as a client first asks the path, and again once it knows the
//! route, so the next pull or push over it starts from what the last showed.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

/// The wait before any is measured, and a pace's least: RFC 6298 §2.1 and §2.4.
pub(crate) const FIRST: Duration = Duration::from_secs(1);
/// The least a trouble is ridden out: TCP's R2 (RFC 1122 §4.2.3.5).
pub(crate) const R2: Duration = Duration::from_secs(100);

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

/// The slower of a slowest kept, in µs, none where nothing was, and a wait seen.
fn slower(slowest: Option<u64>, seen: Option<u64>) -> Option<u64> {
    match (slowest, seen) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// What a path has shown, as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Shown {
    /// The slowest answer to a lookup (a GET or HEAD without a body) it has shown, in µs:
    /// from its request's end to its head's first byte, the gap before any of the answer
    /// comes. None before any.
    pub slowest_answer_us: Option<u64>,
    /// The slowest silence of a download's body it has shown, in µs: the time between a
    /// body's reads that brought bytes. None before any.
    pub slowest_silence_us: Option<u64>,
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
    /// what it read: each slowest the slower, both seen; each pace the newer sample's
    /// ([`Pace::merged`]); the longest trouble the longer; the troubles passed the record's
    /// and those the client passed itself.
    pub(crate) fn merged(self, base: &Shown, own: &Shown) -> Shown {
        Shown {
            slowest_answer_us: slower(self.slowest_answer_us, own.slowest_answer_us),
            slowest_silence_us: slower(self.slowest_silence_us, own.slowest_silence_us),
            connect: Pace::merged(self.connect, base.connect, own.connect),
            handshake: Pace::merged(self.handshake, base.handshake, own.handshake),
            trouble_us: self.trouble_us.max(own.trouble_us),
            troubles: self
                .troubles
                .saturating_add(own.troubles.saturating_sub(base.troubles)),
        }
    }
}

/// Which of a path's waits RFC 6298's estimator paces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// A connection's, its name's lookup included.
    Connect,
    /// A TLS handshake's.
    Handshake,
}

/// What bounds a wait of one of a path's kinds, an answer's or a silence's, as it begins,
/// besides its connection's liveness, which every wait follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bounded {
    /// Its path has shown none of its kind: the cold start, which follows its connection's
    /// liveness and progress alone. A body begun so keeps to them all its life, as any wait
    /// begun before its kind's first answer does (D215's judge); its silences are then the
    /// path's slowest.
    Cold,
    /// The slowest of its kind its path has shown: past it, with nothing come, it is cut.
    Slowest(Duration),
    /// Its path's last of its kind ran out past the slowest it had shown, `.0`, and none has
    /// ended since: the path is slower now than it has shown, or that one was stuck, and
    /// what it has shown bounds nothing until one ends. Its liveness and progress alone,
    /// then: an answer until it comes; a silence until one longer than `.0` ends, as the
    /// first after its body's resume is mostly its head's own bytes and says nothing of the
    /// gaps that ran out. That one is then the path's slowest, which bounds the rest. Every
    /// wait cut at the slowest would let none slower through, and a path slower than it had
    /// ever shown (a manifest slower than the `/v2/` check that taught it) would be cut until
    /// its budget ran out, on every pull.
    RanOut(Duration),
}

impl Bounded {
    /// How long it waits with nothing come, past its connection's liveness: none where
    /// nothing bounds it.
    pub(crate) fn bound(self) -> Option<Duration> {
        match self {
            Bounded::Slowest(d) => Some(d),
            Bounded::Cold | Bounded::RanOut(_) => None,
        }
    }
}

/// Whether a path's last wait of one kind ran out, none having ended since.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Out {
    #[default]
    No,
    Yes,
}

/// What a wait of a kind begins with: `slowest`, the slowest of its kind its path has shown,
/// and whether its last ran out.
fn bounded(slowest: Option<u64>, out: Out) -> Bounded {
    match (slowest.map(Duration::from_micros), out) {
        (None, _) => Bounded::Cold,
        (Some(d), Out::No) => Bounded::Slowest(d),
        (Some(d), Out::Yes) => Bounded::RanOut(d),
    }
}

/// A path's state while shards runs: what it has shown, and what it held when it was
/// read; the timeouts in a row of its connects and its handshakes; whether its last answer
/// and its last silence ran out; and whether it has shown anything since it was read.
#[derive(Debug, Clone, Default)]
struct Path {
    shown: Shown,
    base: Shown,
    connect_timeouts: u32,
    handshake_timeouts: u32,
    answer: Out,
    silence: Out,
    changed: bool,
}

impl Path {
    /// A connect's or a handshake's estimator, and its timeouts in a row.
    fn pace(&mut self, wait: Wait) -> (&mut Pace, &mut u32) {
        match wait {
            Wait::Connect => (&mut self.shown.connect, &mut self.connect_timeouts),
            Wait::Handshake => (&mut self.shown.handshake, &mut self.handshake_timeouts),
        }
    }
}

/// How a download body's last read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Last {
    /// It has read nothing.
    Unread,
    /// It brought bytes, or the body's end.
    Bytes,
    /// It ran out: its wait's own end stopped it, the socket's timeout of its path's slowest
    /// silence.
    RanOut,
}

/// What a download's body showed of its path's silences, as it is let go of.
#[derive(Debug)]
pub(crate) struct Silenced {
    /// Its slowest silence that ended with bytes: a read's own wait for them.
    pub slowest: Option<Duration>,
    /// How its last read ended.
    pub last: Last,
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
    /// Whether every wait of its paths follows its connection's liveness alone, where a test
    /// says ([`Paths::follow_liveness`]).
    #[cfg(test)]
    unbounded: bool,
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

    /// Every wait of its paths from now on follows its connection's liveness alone, as a
    /// path's that has shown nothing does ([`Bounded::Cold`]), whatever they show.
    #[cfg(test)]
    pub(crate) fn follow_liveness(&mut self) {
        self.unbounded = true;
    }

    /// What a wait of a kind begins with, its path having shown `slowest` and its last of
    /// the kind having ran out or not.
    fn bounded(&self, slowest: Option<u64>, out: Out) -> Bounded {
        #[cfg(test)]
        if self.unbounded {
            return Bounded::Cold;
        }
        bounded(slowest, out)
    }

    /// Starts the path from what it showed before, as kept; what it has shown since this
    /// client asked it is kept over that, each slowest the slower of the two, both seen.
    pub(crate) fn learn(&self, key: &str, shown: Shown) {
        self.with(key, |p| {
            if p.changed {
                p.shown.slowest_answer_us = slower(p.shown.slowest_answer_us, shown.slowest_answer_us);
                p.shown.slowest_silence_us = slower(p.shown.slowest_silence_us, shown.slowest_silence_us);
            } else {
                p.shown = shown.clone();
                p.base = shown;
            }
        });
    }

    /// How long `key`'s connect or handshake goes before it is cut: RFC 6298's timeout
    /// of its estimator, doubled for each timeout in a row (§5.5).
    pub(crate) fn timeout(&self, key: &str, wait: Wait) -> Duration {
        self.with(key, |p| {
            let (pace, timeouts) = p.pace(wait);
            backed_off(pace.timeout(), *timeouts)
        })
    }

    /// `key`'s connect or handshake ended after `took`, in time.
    pub(crate) fn waited(&self, key: &str, wait: Wait, took: Duration) {
        self.with(key, |p| {
            p.changed = true;
            let (pace, timeouts) = p.pace(wait);
            pace.add(took);
            pace.at_us = now_us();
            *timeouts = 0;
        });
    }

    /// `key`'s connect or handshake was cut, its time out.
    pub(crate) fn timed_out(&self, key: &str, wait: Wait) {
        self.with(key, |p| {
            let (_, timeouts) = p.pace(wait);
            *timeouts = timeouts.saturating_add(1);
        });
    }

    /// What bounds a lookup's answer of `key`'s as it is asked: the time to its first byte.
    pub(crate) fn answer(&self, key: &str) -> Bounded {
        self.with(key, |p| self.bounded(p.shown.slowest_answer_us, p.answer))
    }

    /// A lookup of `key`'s was answered, its first byte `took` after it was asked: the
    /// slowest answer where it is slower; a run-out over, the path's answers bounded by
    /// their slowest again.
    pub(crate) fn answered(&self, key: &str, took: Duration) {
        self.with(key, |p| {
            let slowest = slower(p.shown.slowest_answer_us, Some(micros(took)));
            p.changed |= slowest != p.shown.slowest_answer_us;
            p.shown.slowest_answer_us = slowest;
            p.answer = Out::No;
        });
    }

    /// A lookup of `key`'s ran out, nothing of its answer come past the slowest the path
    /// has shown. It learns nothing: that answer came later, or never, and none of it is a
    /// time the path took; until one comes, the path's answers follow their connections'
    /// liveness alone ([`Bounded::RanOut`]).
    pub(crate) fn answer_ran_out(&self, key: &str) {
        self.with(key, |p| p.answer = Out::Yes);
    }

    /// What bounds a download's silences from `key` as its body begins: each read's own
    /// wait for bytes.
    pub(crate) fn silence(&self, key: &str) -> Bounded {
        self.with(key, |p| self.bounded(p.shown.slowest_silence_us, p.silence))
    }

    /// A silence of `key`'s, `lasted` long, ended with bytes after one had run out: the
    /// path's slowest, now, where it is slower, and the run-out then over
    /// ([`Bounded::RanOut`]). Its slowest silence, which bounds the rest.
    pub(crate) fn outlasted(&self, key: &str, lasted: Duration) -> Option<Duration> {
        self.with(key, |p| {
            let slowest = slower(p.shown.slowest_silence_us, Some(micros(lasted)));
            if slowest != p.shown.slowest_silence_us {
                p.changed = true;
                p.shown.slowest_silence_us = slowest;
                p.silence = Out::No;
            }
            slowest.map(Duration::from_micros)
        })
    }

    /// A silence of `key`'s ran out, past the slowest the path has shown. It learns nothing;
    /// until one longer ends, the path's silences follow their connections' liveness alone
    /// ([`Bounded::RanOut`]).
    pub(crate) fn silence_ran_out(&self, key: &str) {
        self.with(key, |p| p.silence = Out::Yes);
    }

    /// A download's silences, as its body is let go of: its slowest the path's where it is
    /// slower; a body whose last read brought bytes ends a run-out, its silences having
    /// come within what the path had shown, and one that ran out begins one.
    pub(crate) fn silenced(&self, key: &str, body: &Silenced) {
        self.with(key, |p| {
            let slowest = slower(p.shown.slowest_silence_us, body.slowest.map(micros));
            p.changed |= slowest != p.shown.slowest_silence_us;
            p.shown.slowest_silence_us = slowest;
            match body.last {
                Last::Bytes => p.silence = Out::No,
                Last::RanOut => p.silence = Out::Yes,
                Last::Unread => {}
            }
        });
    }

    /// How long a trouble is ridden out.
    pub(crate) fn budget(&self, key: &str) -> Duration {
        self.with(key, |p| p.shown.budget())
    }

    /// A trouble that lasted `lasted` passed.
    pub(crate) fn passed(&self, key: &str, lasted: Duration) {
        let us = micros(lasted);
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

    /// A path's answers and silences, as the person's rule has them: "The cold start, and
    /// every wait after a run-out, follows the party's liveness and progress. After that the
    /// bound is the slowest answer seen." None bounded before the path has shown one, each
    /// then bounded by the slowest it has shown, which only rises. A run-out learns nothing:
    /// the next answer follows its connection alone until one comes, which ends it whatever
    /// its time and is the slowest where slower; the next silence until one longer than what
    /// the path had shown ends, or a body that brought bytes does. Each kind its own (D222).
    #[test]
    fn a_paths_waits_follow_the_slowest_and_a_run_out_learns_nothing() {
        let ms = Duration::from_millis;
        let paths = Paths::default();
        assert_eq!(paths.answer("r:443"), Bounded::Cold, "the cold start");
        paths.answered("r:443", ms(300));
        assert_eq!(paths.answer("r:443"), Bounded::Slowest(ms(300)));
        paths.answered("r:443", ms(100));
        assert_eq!(paths.answer("r:443"), Bounded::Slowest(ms(300)), "the slowest");
        paths.answered("r:443", ms(500));
        assert_eq!(paths.answer("r:443"), Bounded::Slowest(ms(500)));
        paths.answer_ran_out("r:443");
        assert_eq!(paths.answer("r:443"), Bounded::RanOut(ms(500)));
        assert_eq!(paths.answer("r:443").bound(), None, "liveness alone");
        assert_eq!(
            paths.shown("r:443").slowest_answer_us,
            Some(500_000),
            "learned nothing"
        );
        // The asking after it, come at 2 s: the slowest now.
        paths.answered("r:443", Duration::from_secs(2));
        assert_eq!(paths.answer("r:443"), Bounded::Slowest(Duration::from_secs(2)));
        // A stuck one's asking again, come within what the path had shown: over all the same.
        paths.answer_ran_out("r:443");
        paths.answered("r:443", ms(50));
        assert_eq!(paths.answer("r:443"), Bounded::Slowest(Duration::from_secs(2)));
        // Silences alike, and apart.
        let body = |slowest: Option<Duration>, last| Silenced { slowest, last };
        assert_eq!(paths.silence("r:443"), Bounded::Cold);
        paths.silenced("r:443", &body(None, Last::Unread));
        assert_eq!(paths.silence("r:443"), Bounded::Cold, "a body that read nothing");
        paths.silenced("r:443", &body(Some(ms(40)), Last::Bytes));
        assert_eq!(paths.silence("r:443"), Bounded::Slowest(ms(40)));
        paths.answer_ran_out("r:443");
        assert_eq!(
            paths.silence("r:443"),
            Bounded::Slowest(ms(40)),
            "an answer's run-out"
        );
        paths.silenced("r:443", &body(Some(ms(10)), Last::RanOut));
        assert_eq!(paths.silence("r:443"), Bounded::RanOut(ms(40)));
        assert_eq!(
            paths.shown("r:443").slowest_silence_us,
            Some(40_000),
            "learned nothing"
        );
        // One longer than what the path had shown ends it, and bounds the rest.
        assert_eq!(paths.outlasted("r:443", ms(900)), Some(ms(900)));
        assert_eq!(paths.silence("r:443"), Bounded::Slowest(ms(900)));
        // A body that brought bytes ends one too, its silences within what the path showed.
        paths.silence_ran_out("r:443");
        assert_eq!(paths.silence("r:443"), Bounded::RanOut(ms(900)));
        paths.silenced("r:443", &body(Some(ms(5)), Last::Bytes));
        assert_eq!(paths.silence("r:443"), Bounded::Slowest(ms(900)));
        assert_eq!(
            paths.answer("r:443"),
            Bounded::RanOut(Duration::from_secs(2)),
            "and a silence's none of an answer's"
        );
    }

    /// A path that has passed no trouble rides one out for TCP's R2, one that has passed
    /// longer ones for twice the longest; its connects and handshakes double while they
    /// time out, and come back to RFC 6298's as one is answered, each its own; what was
    /// kept starts it, unless it has shown something since.
    #[test]
    fn a_path_learns_its_waits() {
        let paths = Paths::default();
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(3));
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(80));
        assert_eq!(paths.budget("r:443"), Duration::from_secs(160));
        assert_eq!(paths.shown("r:443").troubles, 2);
        for wait in [Wait::Connect, Wait::Handshake] {
            assert_eq!(paths.timeout("s:443", wait), FIRST, "{wait:?}");
            paths.timed_out("s:443", wait);
            paths.timed_out("s:443", wait);
            assert_eq!(paths.timeout("s:443", wait), FIRST * 4, "{wait:?}");
            paths.waited("s:443", wait, Duration::from_secs(3));
        }
        // 3 + 4·1.5 s, RFC 6298's.
        assert_eq!(paths.timeout("s:443", Wait::Connect), Duration::from_secs(9));
        assert_eq!(paths.timeout("s:443", Wait::Handshake), Duration::from_secs(9));
        // Each wait its own: a connect's timeouts are not a handshake's.
        paths.timed_out("v:443", Wait::Connect);
        assert_eq!(paths.timeout("v:443", Wait::Connect), FIRST * 2);
        assert_eq!(paths.timeout("v:443", Wait::Handshake), FIRST);
        let kept = Shown {
            trouble_us: 200_000_000,
            slowest_answer_us: Some(7_000_000),
            ..Shown::default()
        };
        paths.learn("t:443", kept.clone());
        assert_eq!(paths.budget("t:443"), Duration::from_secs(400));
        assert_eq!(paths.answer("t:443"), Bounded::Slowest(Duration::from_secs(7)));
        paths.answered("s:443", Duration::from_secs(2));
        paths.learn("s:443", kept);
        assert_eq!(paths.budget("s:443"), R2, "what it showed since is kept");
        assert_eq!(
            paths.answer("s:443"),
            Bounded::Slowest(Duration::from_secs(7)),
            "and the slowest the slower of the two"
        );
    }

    /// What a client's paths showed is kept in the home as it lets them go, and the next
    /// client's start from it: those asked before it knew where, and those after; a path
    /// that showed nothing, or only a run-out, is not written.
    #[test]
    fn what_paths_show_is_kept_for_the_next() {
        let dir = shards_testdir::TempDir::new("paths").unwrap();
        let paths = Paths::default();
        paths.passed("r:443", Duration::from_secs(150));
        paths.answered("[::1]:5000", Duration::from_secs(2));
        let _ = paths.budget("quiet:443");
        paths.answer_ran_out("out:443");
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
            next.answer("[::1]:5000"),
            Bounded::Slowest(Duration::from_secs(2)),
            "asked after"
        );
    }

    /// Two clients that asked one path at once, each from what its record held, both
    /// keep what they showed as they are let go: the troubles each passed added, the
    /// longest kept, each slowest the slowest of the record's and both clients', and each
    /// pace the one of the newer sample, its waits both clients' own.
    #[test]
    fn clients_at_once_both_keep_what_they_showed() {
        let dir = shards_testdir::TempDir::new("paths-merge").unwrap();
        let ms = Duration::from_millis;
        let before = Paths::default();
        before.keep_in(&dir);
        before.passed("r:443", Duration::from_secs(10));
        before.waited("r:443", Wait::Connect, ms(40));
        before.answered("r:443", ms(200));
        before.silenced(
            "r:443",
            &Silenced {
                slowest: Some(ms(700)),
                last: Last::Bytes,
            },
        );
        drop(before);
        let (first, second) = (Paths::default(), Paths::default());
        first.keep_in(&dir);
        second.keep_in(&dir);
        assert_eq!(first.shown("r:443").troubles, 1);
        assert_eq!(second.shown("r:443").troubles, 1);
        first.passed("r:443", Duration::from_secs(150));
        first.answered("r:443", ms(900));
        second.passed("r:443", Duration::from_secs(30));
        second.passed("r:443", Duration::from_secs(20));
        second.waited("r:443", Wait::Connect, ms(80));
        second.answered("r:443", ms(600));
        second.answer_ran_out("r:443");
        second.silenced(
            "r:443",
            &Silenced {
                slowest: Some(ms(300)),
                last: Last::Bytes,
            },
        );
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
        assert_eq!(kept.slowest_answer_us, Some(900_000), "the first's");
        assert_eq!(kept.slowest_silence_us, Some(700_000), "the record's");
        assert_eq!(kept.connect.n, 2, "the one before and the second's");
        assert_eq!(kept.connect.srtt_us, (7 * 40_000 + 80_000) / 8);
    }

    /// Records of before the slowest were kept are read whole: what they held of answers
    /// and silences, RFC 6298's estimators and then the restart cutoff's sketches, none of
    /// this build's, so the path waits on them by liveness alone until it shows one.
    #[test]
    fn an_older_record_is_read() {
        let paced = br#"{"shown":{"answer":{"srtt_us":5,"rttvar_us":2,"n":1},"silence":{"srtt_us":0,"rttvar_us":0,"n":0},"trouble_us":7,"troubles":1}}"#;
        let cutoff = br#"{"shown":{"answers":{"answered":{"1391":2},"cut":{"1700":1},"again_us":9,"agains":1},"silences":{"answered":{},"cut":{},"again_us":0,"agains":0},"connect":{"srtt_us":4,"rttvar_us":2,"n":1,"at_us":3},"trouble_us":7,"troubles":1}}"#;
        for old in [&paced[..], &cutoff[..]] {
            let record: crate::links::PathRecord = serde_json::from_slice(old).unwrap();
            assert_eq!(record.shown.slowest_answer_us, None);
            assert_eq!(record.shown.slowest_silence_us, None);
            assert_eq!(record.shown.trouble_us, 7);
        }
        let record: crate::links::PathRecord = serde_json::from_slice(cutoff).unwrap();
        assert_eq!(record.shown.connect.n, 1);
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
