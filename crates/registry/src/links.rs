//! What transfers learn of their network path, kept in the home (D226): each pull's and
//! each push's, by the path it went over (this host's address on its route to the
//! registry, as a connection there is given it, and the registry's host and port), the
//! count of transfers it converged to, what each window at that count carried a second, the least
//! a request took to answer, and how often a send failed. One file a path,
//! `links/<registry>@<address>` in the image store, which holds D222's waits and troubles
//! of the path too (crate::patience), written whole by a rename, so a reader never sees
//! half of one; and changed only under `links/.lock`, each change read, merged and
//! written while it is held, so two transfers that change one record at once both keep
//! what they learned.
//!
//! A push's chunk size is solved from them ([`optimum`]): a chunk costs a request's
//! answer, and a send that fails loses what of its chunk was sent, so the chunk that costs
//! least a byte is Young's optimum interval between checkpoints (CACM 17(9), 1974).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use shards_image::store::Locked;

use crate::climb::Spread;

/// What a transfer learned of its path.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Learned {
    /// The transfers at once it converged to, and what each window at that count carried a
    /// second.
    pub count: usize,
    pub rate: Spread,
    /// The least a request took to answer, in µs.
    pub answer_us: Option<u64>,
    /// Bytes of bodies sent, and the sends that failed: reset, timed out, cut short.
    pub bytes: u64,
    pub failures: u64,
}

impl Learned {
    /// Its count and samples, for [`crate::climb::Climb::new`], where it has both.
    pub(crate) fn climb(&self) -> Option<(usize, Spread)> {
        (self.count > 0 && self.rate.variance().is_some()).then_some((self.count, self.rate))
    }

    /// Adds `own`, what one transfer learned itself, to what the record holds now, which
    /// other transfers may have added to since this one read it:
    /// - its samples, where they are of the count the record holds and the path the
    ///   record's are (their means within the pooled standard deviation, `climb::same`),
    ///   combined with the record's ([`Spread::combined`], Chan, Golub and LeVeque 1979);
    ///   samples of another count, or of a path that changed, measure something else, and
    ///   replace them, as the climb itself goes on from them;
    /// - the least answer, the least of the two;
    /// - its bytes and failures, added to the record's where the failures are what the
    ///   record's rate expected, else in its place ([`aged`]).
    pub(crate) fn merge(&mut self, own: &Learned) {
        if own.count > 0 && own.rate.n > 0 {
            if own.count == self.count && crate::climb::same(&self.rate, &own.rate) {
                self.rate = self.rate.combined(&own.rate);
            } else {
                self.count = own.count;
                self.rate = own.rate;
            }
        }
        self.answer_us = match (self.answer_us, own.answer_us) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let (bytes, failures) = aged(Some(self), own.bytes, own.failures);
        self.bytes = bytes;
        self.failures = failures;
    }
}

/// A path's one record: what its requests showed of its waits and troubles (D222,
/// crate::patience) and what its pulls and its pushes learned (D226), each kept as it is
/// learned, the rest of the file as it was.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PathRecord {
    #[serde(default)]
    pub shown: crate::patience::Shown,
    #[serde(default)]
    pub pull: Learned,
    #[serde(default)]
    pub push: Learned,
}

/// The file of the path to `key` (`host:port`, crate::patience `path_of`) from `from`
/// (this host's address on its route there, where known), under `root` (the image
/// store's directory): `links/<host:port>@<address>`.
pub(crate) fn file(root: &Path, key: &str, from: Option<IpAddr>) -> PathBuf {
    let from = from.map_or_else(|| "unrouted".to_string(), |ip| ip.to_string());
    root.join("links")
        .join(format!("{}@{}", encode(key), encode(&from)))
}

/// What `file` holds, where it holds a record this build reads.
pub(crate) fn read(file: &Path) -> Option<PathRecord> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

/// Changes `file`'s record by `change`: read, changed and written whole by a rename (a
/// reader never sees half of one), all under the lock of the records' directory, the
/// image store's own (`Locked`: flock(2), `LockFileEx` on Windows, let go before its file
/// closes, PM M244), which every process and thread changing a record takes: so a change
/// is made to what the last left, and none is lost.
pub(crate) fn update(file: &Path, change: impl FnOnce(&mut PathRecord)) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = file
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    std::fs::create_dir_all(dir)?;
    let _held = Locked::exclusive(&dir.join(LOCK))?;
    let mut record = read(file).unwrap_or_default();
    change(&mut record);
    let bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{n}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The file whose lock a record's change holds, beside the records, a name no record has
/// (each holds its port's `%3A` and an `@`).
const LOCK: &str = ".lock";

/// Which of a path's transfers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Pull,
    Push,
}

/// What a path's pulls or pushes learned, in its record.
#[derive(Debug)]
pub(crate) struct Record {
    file: PathBuf,
    direction: Direction,
}

impl Record {
    /// The `direction`'s part of the record of the path to `host`:`port` from `from`, under
    /// `root`.
    pub(crate) fn of(
        root: &Path,
        direction: Direction,
        host: &str,
        port: u16,
        from: Option<IpAddr>,
    ) -> Record {
        Record {
            file: file(root, &format!("{host}:{port}"), from),
            direction,
        }
    }

    /// What it holds, where it learned anything.
    pub(crate) fn read(&self) -> Option<Learned> {
        let record = read(&self.file)?;
        let learned = match self.direction {
            Direction::Pull => record.pull,
            Direction::Push => record.push,
        };
        (learned != Learned::default()).then_some(learned)
    }

    /// Adds `own`, what one transfer learned itself, to its part ([`Learned::merge`]).
    pub(crate) fn add(&self, own: &Learned) -> std::io::Result<()> {
        update(&self.file, |record| match self.direction {
            Direction::Pull => record.pull.merge(own),
            Direction::Push => record.push.merge(own),
        })
    }
}

/// `s` as a file's name on every host: letters, digits, `.` and `-` as they are, every
/// other byte `%XX`, so that no two names are one.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The chunk that costs a push least a byte, in bytes, over a path whose requests take
/// `answer` seconds to answer, each upload carrying `rate` bytes a second, and whose sends
/// fail `loss` times a byte: a chunk of c bytes costs c/rate to send and a request's
/// answer, and a send that fails loses, on average, half a chunk sent (the registry keeps
/// none of a chunk it did not take: distribution-spec v1.1.1 resumes from the `Range` it
/// acknowledged). The time a byte, 1/rate + answer/c + loss·c/(2·rate), is least at
/// c = √(2·answer·rate/loss). `None` where no send failed: one request is the least.
pub(crate) fn optimum(answer: f64, rate: f64, loss: f64) -> Option<f64> {
    (loss > 0.0 && answer > 0.0 && rate > 0.0).then(|| (2.0 * answer * rate / loss).sqrt())
}

/// What a path's sends say of it once a transfer has sent `bytes` and failed `failures`
/// times, given what was learned before: added to it where the failures are what its
/// rate expected, within a Poisson count's spread (its standard deviation, the root of
/// its mean); else the path changed, and this transfer's alone are kept.
pub(crate) fn aged(before: Option<&Learned>, bytes: u64, failures: u64) -> (u64, u64) {
    match before.filter(|b| b.bytes > 0) {
        Some(b) => {
            let expected = b.failures as f64 / b.bytes as f64 * bytes as f64;
            if (failures as f64 - expected).abs() <= expected.sqrt() {
                (b.bytes.saturating_add(bytes), b.failures.saturating_add(failures))
            } else {
                (bytes, failures)
            }
        }
        None => (bytes, failures),
    }
}

/// A push's path as its uploads go: shared by them, read by each for its chunks.
#[derive(Debug)]
pub struct Link {
    started: Instant,
    sent: AtomicU64,
    failures: AtomicU64,
    /// The least a request took to answer, in µs; `u64::MAX` before any.
    answer_us: AtomicU64,
    /// Uploads at once now.
    count: AtomicUsize,
    /// What was learned before: bytes, failures, and what each upload carried a second.
    before: (u64, u64, Option<f64>),
}

impl Link {
    pub(crate) fn new(learned: Option<&Learned>) -> Link {
        Link {
            started: Instant::now(),
            sent: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            answer_us: AtomicU64::new(learned.and_then(|l| l.answer_us).unwrap_or(u64::MAX)),
            count: AtomicUsize::new(1),
            before: learned.map_or((0, 0, None), |l| {
                let each = (l.count > 0).then(|| l.rate.mean / l.count as f64);
                (l.bytes, l.failures, each)
            }),
        }
    }

    /// Where each byte of a body is counted as it goes.
    pub(crate) fn sent(&self) -> &AtomicU64 {
        &self.sent
    }

    pub(crate) fn failed(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    pub(crate) fn answered(&self, took: Duration) {
        let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.answer_us.fetch_min(us, Ordering::Relaxed);
    }

    pub(crate) fn answer(&self) -> Option<Duration> {
        let us = self.answer_us.load(Ordering::Relaxed);
        (us != u64::MAX).then(|| Duration::from_micros(us))
    }

    pub(crate) fn set_count(&self, n: usize) {
        self.count.store(n.max(1), Ordering::Relaxed);
    }

    /// The chunk a blob goes in now ([`optimum`]); `None`: whole.
    pub(crate) fn chunk(&self) -> Option<u64> {
        let sent = self.sent.load(Ordering::Relaxed);
        let (bytes, failures) = (
            self.before.0.saturating_add(sent),
            self.before.1.saturating_add(self.failures()),
        );
        if bytes == 0 {
            return None;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let count = self.count.load(Ordering::Relaxed).max(1) as f64;
        let each = match self.before.2 {
            _ if sent > 0 && elapsed > 0.0 => sent as f64 / elapsed / count,
            Some(each) => each,
            None => return None,
        };
        let c = optimum(self.answer()?.as_secs_f64(), each, failures as f64 / bytes as f64)?;
        Some(c.ceil() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record is read back as it was written, under a name of its path's that every
    /// host can hold, one file a direction and path.
    #[test]
    fn a_paths_record_is_kept_by_its_path() {
        let dir = shards_testdir::TempDir::new("links").unwrap();
        let loopback = Some(IpAddr::from([127, 0, 0, 1]));
        let record = Record::of(&dir, Direction::Push, "127.0.0.1", 5000, loopback);
        assert!(record.read().is_none());
        let mut rate = Spread::default();
        rate.add(10.0);
        rate.add(12.0);
        let learned = Learned {
            count: 8,
            rate,
            answer_us: Some(1500),
            bytes: 1 << 30,
            failures: 3,
        };
        record.add(&learned).unwrap();
        assert_eq!(record.read(), Some(learned.clone()));
        assert_eq!(learned.climb(), Some((8, rate)));
        let name = record.file.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(name, "127.0.0.1%3A5000@127.0.0.1");
        let pull = Record::of(&dir, Direction::Pull, "127.0.0.1", 5000, loopback);
        assert!(pull.read().is_none(), "the path's pulls learned nothing");
        pull.add(&learned).unwrap();
        assert_eq!(
            record.read(),
            Some(learned.clone()),
            "one file, each its own part"
        );
        assert_eq!(
            read(&record.file).unwrap().shown,
            crate::patience::Shown::default()
        );
        let wifi = Some(IpAddr::from([192, 168, 1, 72]));
        assert!(
            Record::of(&dir, Direction::Push, "127.0.0.1", 5000, wifi)
                .read()
                .is_none(),
            "another route is another path"
        );
        assert_eq!(encode("[::1]:5000"), "%5B%3A%3A1%5D%3A5000");
    }

    /// A transfer's own is added to what the record holds: samples of the same count and
    /// path combined, of another count or a changed path in their place; the least
    /// answer kept; bytes and failures added where the failures are the path's.
    #[test]
    fn a_transfers_own_is_merged_into_the_record() {
        let spread = |xs: &[f64]| {
            let mut s = Spread::default();
            xs.iter().for_each(|&x| s.add(x));
            s
        };
        let mut record = Learned {
            count: 4,
            rate: spread(&[100.0, 104.0]),
            answer_us: Some(2000),
            bytes: 1_000_000,
            failures: 100,
        };
        record.merge(&Learned {
            count: 4,
            rate: spread(&[101.0, 103.0]),
            answer_us: Some(3000),
            bytes: 1_000_000,
            failures: 105,
        });
        // All four samples: mean 102, squared differences 4 + 4 + 1 + 1.
        assert_eq!(record.rate.n, 4);
        assert!((record.rate.mean - 102.0).abs() < 1e-12 && (record.rate.m2 - 10.0).abs() < 1e-9);
        assert_eq!((record.count, record.answer_us), (4, Some(2000)));
        assert_eq!((record.bytes, record.failures), (2_000_000, 205));
        let mut changed = record.clone();
        changed.merge(&Learned {
            count: 4,
            rate: spread(&[50.0, 52.0]),
            ..Learned::default()
        });
        assert_eq!(changed.rate, spread(&[50.0, 52.0]), "a changed path");
        assert_eq!((changed.bytes, changed.failures), (2_000_000, 205));
        let mut other = record.clone();
        other.merge(&Learned {
            count: 8,
            rate: spread(&[101.0, 103.0]),
            answer_us: Some(900),
            ..Learned::default()
        });
        assert_eq!((other.count, other.rate.n, other.answer_us), (8, 2, Some(900)));
        let mut none = record.clone();
        none.merge(&Learned::default());
        assert_eq!(none, record, "a transfer that learned nothing changes nothing");
    }

    /// Transfers that add to one record at once, from threads as from processes, each
    /// under the records' lock, all keep what they learned: every sample, byte and
    /// failure is in the record after.
    #[test]
    fn transfers_at_once_keep_every_sample() {
        let dir = shards_testdir::TempDir::new("links-merge").unwrap();
        let record = Record::of(&dir, Direction::Push, "127.0.0.1", 5000, None);
        let own = |i: u32| {
            let mut rate = Spread::default();
            rate.add(100.0);
            rate.add(102.0);
            Learned {
                count: 4,
                rate,
                answer_us: Some(1000 + u64::from(i)),
                bytes: 1000,
                failures: 1,
            }
        };
        let (threads, each) = (8u32, 25u32);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (record, own) = (&record, &own);
                s.spawn(move || {
                    for i in 0..each {
                        record.add(&own(t * each + i)).unwrap();
                    }
                });
            }
        });
        let all = u64::from(threads * each);
        let kept = record.read().unwrap();
        assert_eq!(kept.rate.n, 2 * all, "a sample lost");
        assert_eq!((kept.bytes, kept.failures), (1000 * all, all));
        assert_eq!((kept.count, kept.answer_us), (4, Some(1000)));
        let names: Vec<String> = std::fs::read_dir(dir.join("links"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "the record and its lock: {names:?}");
    }

    /// Young's optimum is the least of the time a byte: smaller chunks and larger both
    /// cost more; more failures make it smaller, a longer answer larger; none, whole.
    #[test]
    fn a_chunk_is_the_least_time_a_byte() {
        let (answer, rate, loss) = (0.05, 4e6, 1e-7);
        let cost = |c: f64| 1.0 / rate + answer / c + loss * c / (2.0 * rate);
        let c = optimum(answer, rate, loss).unwrap();
        assert!((c - (2.0 * answer * rate / loss).sqrt()).abs() < 1e-6);
        assert!(cost(c) < cost(c * 0.9) && cost(c) < cost(c * 1.1));
        assert!(optimum(answer, rate, loss * 4.0).unwrap() < c);
        assert!(optimum(answer * 4.0, rate, loss).unwrap() > c);
        assert_eq!(
            optimum(answer, rate, 0.0),
            None,
            "a path that never failed: whole"
        );
    }

    /// What a path's sends said before is added to where this transfer's failures are
    /// within a Poisson count's spread of what it expected, and dropped where not.
    #[test]
    fn failures_age_by_the_change_measured() {
        let before = Learned {
            bytes: 1_000_000,
            failures: 100,
            ..Learned::default()
        };
        // 100 expected of a million bytes: ±10.
        assert_eq!(aged(Some(&before), 1_000_000, 110), (2_000_000, 210));
        assert_eq!(aged(Some(&before), 1_000_000, 111), (1_000_000, 111));
        assert_eq!(aged(Some(&before), 1_000_000, 89), (1_000_000, 89));
        let clean = Learned {
            bytes: 1_000_000,
            ..Learned::default()
        };
        assert_eq!(aged(Some(&clean), 5, 0), (1_000_005, 0));
        assert_eq!(
            aged(Some(&clean), 5, 1),
            (5, 1),
            "a clean path that failed changed"
        );
        assert_eq!(aged(None, 5, 1), (5, 1));
    }

    /// A push's chunk follows its path as it goes: whole until a send fails, then
    /// Young's optimum of what it measured; what was learned before counts too.
    #[test]
    fn a_links_chunk_follows_its_failures() {
        let link = Link::new(None);
        assert_eq!(link.chunk(), None, "nothing measured");
        // Long past the microseconds between this test's two readings of the clock.
        std::thread::sleep(Duration::from_millis(100));
        link.answered(Duration::from_millis(20));
        link.sent().fetch_add(4_000_000, Ordering::Relaxed);
        assert_eq!(link.chunk(), None, "no send failed");
        link.failed();
        let c = link.chunk().unwrap();
        let each = 4_000_000.0 / link.started.elapsed().as_secs_f64();
        let want = optimum(0.02, each, 1.0 / 4e6).unwrap();
        assert!((c as f64 - want).abs() / want < 0.05, "{c} {want}");
        let learned = Learned {
            count: 2,
            rate: Spread {
                n: 2,
                mean: 2e6,
                m2: 0.0,
            },
            answer_us: Some(20_000),
            bytes: 1_000_000,
            failures: 10,
        };
        let warm = Link::new(Some(&learned));
        // 1 MB/s each, 20 ms, 1e-5 a byte: √(2·0.02·1e6/1e-5) = 63,246.
        assert_eq!(warm.chunk(), Some(63_246));
    }
}
