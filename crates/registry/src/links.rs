//! What transfers learn of their network path, kept in the home (D226): each pull's and
//! each push's, by the path it went over (this host's address on its route to the
//! registry, and the registry's host and port), the count of transfers it converged to,
//! what each second at that count carried, the least a request took to answer, and how
//! often a send failed. A file of its own each, `links/<pull|push>/<registry>@<address>`
//! in the image store, written whole by a rename, so a reader never sees half of one and
//! two transfers that end together leave one of theirs.
//!
//! A push's chunk size is solved from them ([`optimum`]): a chunk costs a request's
//! answer, and a send that fails loses what of its chunk was sent, so the chunk that costs
//! least a byte is Young's optimum interval between checkpoints (CACM 17(9), 1974).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::climb::Spread;

/// What a transfer learned of its path.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Learned {
    /// The transfers at once it converged to, and what each second at that count carried.
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
}

/// A path's record.
#[derive(Debug)]
pub(crate) struct Record {
    file: PathBuf,
}

impl Record {
    /// The record of `direction` ("pull", "push") over the path to `host`:`port` from
    /// `from` (this host's address on its route there), kept under `root` (the image
    /// store's directory).
    pub(crate) fn of(root: &Path, direction: &str, host: &str, port: u16, from: Option<IpAddr>) -> Record {
        let from = from.map_or_else(|| "unrouted".to_string(), |ip| ip.to_string());
        let name = format!("{}@{}", encode(&format!("{host}:{port}")), encode(&from));
        Record {
            file: root.join("links").join(direction).join(name),
        }
    }

    /// What it holds, where it holds a record this build reads.
    pub(crate) fn read(&self) -> Option<Learned> {
        serde_json::from_slice(&std::fs::read(&self.file).ok()?).ok()
    }

    /// Keeps `learned`, whole.
    pub(crate) fn write(&self, learned: &Learned) -> std::io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = self
            .file
            .parent()
            .ok_or_else(|| std::io::Error::other("no directory"))?;
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec(learned).map_err(std::io::Error::other)?;
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{}.{n}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.file).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// `s` as a file's name on every host: letters, digits, `.` and `-` as they are, every
/// other byte `%XX`, so that no two names are one.
fn encode(s: &str) -> String {
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
        let record = Record::of(&dir, "push", "127.0.0.1", 5000, loopback);
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
        record.write(&learned).unwrap();
        assert_eq!(record.read(), Some(learned.clone()));
        assert_eq!(learned.climb(), Some((8, rate)));
        let name = record.file.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(name, "127.0.0.1%3A5000@127.0.0.1");
        assert!(
            Record::of(&dir, "pull", "127.0.0.1", 5000, loopback)
                .read()
                .is_none()
        );
        let wifi = Some(IpAddr::from([192, 168, 1, 72]));
        assert!(
            Record::of(&dir, "push", "127.0.0.1", 5000, wifi).read().is_none(),
            "another route is another path"
        );
        assert_eq!(encode("[::1]:5000"), "%5B%3A%3A1%5D%3A5000");
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
