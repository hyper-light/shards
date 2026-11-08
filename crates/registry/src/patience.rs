//! How long shards waits on a registry, learned from that registry's own path as shards
//! runs (D222), never a figure measured on one network and frozen:
//! - how long a connection takes to make (its name's lookup included), its TLS
//!   handshake, a request without a body's answer, and how long a download goes without
//!   a byte before it is cut and resumed: each the path's own pace, the waits it has
//!   shown, estimated as TCP estimates a retransmission timeout (RFC 6298 §2, after
//!   Jacobson, "Congestion Avoidance and Control", SIGCOMM 1988): the smoothed wait and
//!   four times its smoothed deviation, at least a second, a second before any wait is
//!   measured, and doubled for each timeout in a row (§5.5) until one is answered;
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
//! it), merged into the record as the client that learned it is let go of (each wait's
//! estimator its newer sample's, the troubles passed added: [`Shown::merged`]), read as a
//! client first asks the path, and again once it knows the route, so the next pull or push
//! over it starts from what the last showed.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

/// The wait before any is measured, and the least: RFC 6298 §2.1 and §2.4.
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

/// What a path has shown, as kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Shown {
    /// Its answers' waits, of requests without a body.
    pub answer: Pace,
    /// Its downloads' silences: the waits between a body's reads.
    pub silence: Pace,
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
    /// what it read: each wait's estimator the newer sample's ([`Pace::merged`]); the
    /// longest trouble the longer; the troubles passed the record's and those the client
    /// passed itself.
    pub(crate) fn merged(self, base: &Shown, own: &Shown) -> Shown {
        Shown {
            answer: Pace::merged(self.answer, base.answer, own.answer),
            silence: Pace::merged(self.silence, base.silence, own.silence),
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
    /// A request without a body's, for its answer.
    Answer,
    /// A connection's, its name's lookup included.
    Connect,
    /// A TLS handshake's.
    Handshake,
}

/// A path's state while shards runs: what it has shown, and what it held when it was
/// read; the timeouts in a row of each of its waits and of its silences; and whether it
/// has shown anything since it was read.
#[derive(Debug, Clone, Copy, Default)]
struct Path {
    shown: Shown,
    base: Shown,
    answer_timeouts: u32,
    connect_timeouts: u32,
    handshake_timeouts: u32,
    silence_timeouts: u32,
    changed: bool,
}

impl Path {
    /// `wait`'s estimator, and its timeouts in a row.
    fn wait(&mut self, wait: Wait) -> (&mut Pace, &mut u32) {
        match wait {
            Wait::Answer => (&mut self.shown.answer, &mut self.answer_timeouts),
            Wait::Connect => (&mut self.shown.connect, &mut self.connect_timeouts),
            Wait::Handshake => (&mut self.shown.handshake, &mut self.handshake_timeouts),
        }
    }
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
}

impl Paths {
    fn with<T>(&self, key: &str, f: impl FnOnce(&mut Path) -> T) -> T {
        let mut paths = self.paths.lock().unwrap_or_else(PoisonError::into_inner);
        let path = paths.entry(key.to_string()).or_insert_with(|| {
            let shown = self.read(key).unwrap_or_default();
            Path {
                shown,
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
            record.shown = record.shown.merged(base, shown);
        })
    }

    /// What the path has shown.
    #[cfg(test)]
    pub(crate) fn shown(&self, key: &str) -> Shown {
        self.with(key, |p| p.shown)
    }

    /// Starts the path from what it showed before, as kept; what it has shown since this
    /// client asked it is kept over that.
    pub(crate) fn learn(&self, key: &str, shown: Shown) {
        self.with(key, |p| {
            if !p.changed {
                p.shown = shown;
                p.base = shown;
            }
        });
    }

    /// How long `key`'s `wait` goes before it is cut: RFC 6298's timeout of its
    /// estimator, doubled for each timeout in a row (§5.5).
    pub(crate) fn timeout(&self, key: &str, wait: Wait) -> Duration {
        self.with(key, |p| {
            let (pace, timeouts) = p.wait(wait);
            backed_off(pace.timeout(), *timeouts)
        })
    }

    /// `key`'s `wait` ended after `took`, in time.
    pub(crate) fn waited(&self, key: &str, wait: Wait, took: Duration) {
        self.with(key, |p| {
            let (pace, timeouts) = p.wait(wait);
            pace.add(took);
            pace.at_us = now_us();
            *timeouts = 0;
            p.changed = true;
        });
    }

    /// `key`'s `wait` was cut, its time out.
    pub(crate) fn timed_out(&self, key: &str, wait: Wait) {
        self.with(key, |p| {
            let (_, timeouts) = p.wait(wait);
            *timeouts = timeouts.saturating_add(1);
        });
    }

    /// How long a download goes without a byte before it is cut, and the silences a
    /// download starts from.
    pub(crate) fn silence(&self, key: &str) -> (Duration, Pace) {
        self.with(key, |p| {
            (
                backed_off(p.shown.silence.timeout(), p.silence_timeouts),
                p.shown.silence,
            )
        })
    }

    /// A download's silences, as it ends; whether it timed out, or brought a byte after its
    /// last timeout.
    pub(crate) fn silenced(&self, key: &str, silence: Pace, timed_out: Option<bool>) {
        self.with(key, |p| {
            if p.shown.silence != silence {
                p.changed = true;
                p.shown.silence = Pace {
                    at_us: now_us(),
                    ..silence
                };
            }
            match timed_out {
                Some(true) => p.silence_timeouts = p.silence_timeouts.saturating_add(1),
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

    /// A path that has passed no trouble rides one out for TCP's R2, one that has passed
    /// longer ones for twice the longest; its answer waits double while they time out,
    /// and come back as one is answered; what was kept starts it, unless it has shown
    /// something since.
    #[test]
    fn a_path_learns_its_waits() {
        let paths = Paths::default();
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(3));
        assert_eq!(paths.budget("r:443"), R2);
        paths.passed("r:443", Duration::from_secs(80));
        assert_eq!(paths.budget("r:443"), Duration::from_secs(160));
        assert_eq!(paths.shown("r:443").troubles, 2);
        for wait in [Wait::Answer, Wait::Connect, Wait::Handshake] {
            assert_eq!(paths.timeout("s:443", wait), FIRST, "{wait:?}");
            paths.timed_out("s:443", wait);
            paths.timed_out("s:443", wait);
            assert_eq!(paths.timeout("s:443", wait), FIRST * 4, "{wait:?}");
            paths.waited("s:443", wait, Duration::from_secs(3));
            // 3 + 4·1.5 s.
            assert_eq!(paths.timeout("s:443", wait), Duration::from_secs(9), "{wait:?}");
        }
        // Each wait its own: a connect's timeouts are not an answer's.
        paths.timed_out("v:443", Wait::Connect);
        assert_eq!(paths.timeout("v:443", Wait::Connect), FIRST * 2);
        assert_eq!(paths.timeout("v:443", Wait::Answer), FIRST);
        assert_eq!(paths.timeout("v:443", Wait::Handshake), FIRST);
        let kept = Shown {
            trouble_us: 200_000_000,
            ..Shown::default()
        };
        paths.learn("t:443", kept);
        assert_eq!(paths.budget("t:443"), Duration::from_secs(400));
        paths.learn("s:443", kept);
        assert_eq!(paths.budget("s:443"), R2, "what it showed since is kept");
        let (timeout, silence) = paths.silence("u:443");
        assert_eq!((timeout, silence.n), (FIRST, 0));
        paths.silenced("u:443", silence, Some(true));
        assert_eq!(paths.silence("u:443").0, FIRST * 2);
        paths.silenced("u:443", silence, Some(false));
        assert_eq!(paths.silence("u:443").0, FIRST);
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
        assert_eq!(next.shown("[::1]:5000").answer.n, 1, "asked after");
    }

    /// Two clients that asked one path at once, each from what its record held, both
    /// keep what they showed as they are let go: the troubles each passed added, the
    /// longest kept, and each wait's estimator the one of the newer sample, its waits
    /// both clients' own.
    #[test]
    fn clients_at_once_both_keep_what_they_showed() {
        let dir = shards_testdir::TempDir::new("paths-merge").unwrap();
        let before = Paths::default();
        before.keep_in(&dir);
        before.passed("r:443", Duration::from_secs(10));
        before.waited("r:443", Wait::Connect, Duration::from_millis(40));
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
        // The second's connect seen after the first's answer, in the clock's µs.
        std::thread::sleep(Duration::from_millis(2));
        second.waited("r:443", Wait::Answer, Duration::from_millis(600));
        let newer = second.shown("r:443").answer;
        // Let go the other way round: the newer answer is kept whatever the order.
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
        assert_eq!(
            (kept.answer.srtt_us, kept.answer.at_us),
            (newer.srtt_us, newer.at_us)
        );
        assert_eq!(kept.answer.n, 2, "each client's answer");
        assert_eq!(kept.connect.n, 2, "the one before and the second's");
        assert_eq!(kept.connect.srtt_us, (7 * 40_000 + 80_000) / 8);
    }

    /// A record of before connects and handshakes were kept is read whole.
    #[test]
    fn an_older_record_is_read() {
        let old = br#"{"shown":{"answer":{"srtt_us":5,"rttvar_us":2,"n":1},"silence":{"srtt_us":0,"rttvar_us":0,"n":0},"trouble_us":7,"troubles":1}}"#;
        let record: crate::links::PathRecord = serde_json::from_slice(old).unwrap();
        assert_eq!((record.shown.answer.srtt_us, record.shown.answer.at_us), (5, 0));
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
