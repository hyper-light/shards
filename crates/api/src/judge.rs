//! The judge of a wait that follows its party (D215), apart from how the party is looked at:
//! what a kind of answer has taught ([`Record`]: W*, the slowest answer seen, and R*, the
//! least rate a party still answered at), and a wait's schedule and verdict ([`Judge`]). A
//! wait that begins before its kind's first answer follows its party's liveness alone; one
//! that begins once its kind has an answer looks at its party once each W* past its start,
//! and is refused, [`Stall`], at a look that finds it made no progress at all since the
//! look before. R* is the refusal's diagnosis, never its bound. What a look finds is its
//! caller's to say ([`Found`]): shards-api's waits read their guest's VM process's CPU
//! (`progress::Follow`), and so do the daemon's waits on a microVM's own answers (D188).

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A CPU rate: CPU time had over a wall time, kept as the pair it was measured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// CPU time, user and system, of every thread.
    pub cpu: Duration,
    /// The wall time it was had over.
    pub over: Duration,
}

impl Rate {
    /// Whether this rate is less than `other`: `cpu / over < other.cpu / other.over`, as
    /// `cpu × other.over < other.cpu × over` in nanoseconds, with no division and no float.
    /// Each side is the product of two u64s, which no u128 overflows; a duration past u64's
    /// nanoseconds (584 years) counts as its most.
    pub(crate) fn below(&self, other: &Rate) -> bool {
        let ns = |d: Duration| u128::from(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        ns(self.cpu).saturating_mul(ns(other.over)) < ns(other.cpu).saturating_mul(ns(self.over))
    }
}

/// What a wait that follows its party found as it refused it
/// ([`Refusal::Stalled`](crate::Refusal::Stalled)). Kept in nanoseconds, each past u64's (584
/// years) its most, so a refusal is no larger for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    waited: u64,
    over: u64,
    least: [u64; 2],
    slowest: u64,
}

impl Stall {
    fn new(waited: Duration, over: Duration, least: Rate, slowest: Duration) -> Stall {
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        Stall {
            waited: ns(waited),
            over: ns(over),
            least: [ns(least.cpu), ns(least.over)],
            slowest: ns(slowest),
        }
    }

    /// How far into the wait it was found.
    pub fn waited(&self) -> Duration {
        Duration::from_nanos(self.waited)
    }

    /// The time since the look before, a whole [`slowest`](Stall::slowest) or more, over
    /// which its party made no progress at all.
    pub fn over(&self) -> Duration {
        Duration::from_nanos(self.over)
    }

    /// The least rate at which a party of the wait's kind still answered (R*): diagnosis,
    /// never a bound.
    pub fn least(&self) -> Rate {
        let [cpu, over] = self.least;
        Rate {
            cpu: Duration::from_nanos(cpu),
            over: Duration::from_nanos(over),
        }
    }

    /// The slowest answer of the wait's kind seen (W*), past which it was looked at.
    pub fn slowest(&self) -> Duration {
        Duration::from_nanos(self.slowest)
    }
}

/// What a process has seen of a kind of answer ([`Shards::seen`](crate::Shards::seen),
/// [`Record::seen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// No answer yet: a wait follows its party's liveness alone.
    Cold,
    /// The slowest answer seen (W*), from a wait's start to its answer, past which a wait
    /// looks at its party; and the least rate at which a party still answered (R*), which a
    /// stall's refusal reports.
    Learned { slowest: Duration, least: Rate },
}

/// What a process has learned of one kind of answer, in its own memory, none until its first
/// ([`Seen`]): every wait of that kind learns from it and is judged by it.
#[derive(Debug)]
pub struct Record {
    seen: Mutex<Seen>,
}

impl Record {
    pub const fn new() -> Record {
        Record {
            seen: Mutex::new(Seen::Cold),
        }
    }

    pub fn seen(&self) -> Seen {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An answer came `wall` after its wait began, its party having done `work` meanwhile
    /// (its CPU time): the slowest rises to it where it is slower, the least rate falls to its
    /// where its is less. A wait no time long has no rate and teaches nothing.
    pub fn teach(&self, wall: Duration, work: Duration) {
        if wall.is_zero() {
            return;
        }
        let rate = Rate {
            cpu: work,
            over: wall,
        };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        *seen = match *seen {
            Seen::Cold => Seen::Learned {
                slowest: wall,
                least: rate,
            },
            Seen::Learned { slowest, least } => Seen::Learned {
                slowest: slowest.max(wall),
                least: if rate.below(&least) { rate } else { least },
            },
        };
    }
}

impl Default for Record {
    fn default() -> Record {
        Record::new()
    }
}

/// What a look at a wait's party found since the look before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// It made progress, or its end is under way, which judges nothing: its end is the
    /// wait's answer.
    Progressed,
    /// It made none at all.
    Still,
    /// It has gone: the wait follows its liveness alone from here, its end the wait's.
    Gone,
}

/// A wait judged by the record of its kind of answer: its schedule of looks and its verdict.
#[derive(Debug)]
pub struct Judge<'a> {
    record: &'a Record,
    /// Whether it is judged: its kind had an answer as it began, and its party has not gone
    /// since. One that began before its kind's first answer follows liveness alone, as does
    /// one that finds the record learned meanwhile.
    judged: bool,
    began: Instant,
    /// When the look before was, or its beginning.
    last: Instant,
}

impl<'a> Judge<'a> {
    /// A wait beginning now, learning into and judged by `record`.
    pub fn begin(record: &'a Record) -> Judge<'a> {
        let began = Instant::now();
        Judge {
            record,
            judged: matches!(record.seen(), Seen::Learned { .. }),
            began,
            last: began,
        }
    }

    /// When its next look is due, W* after the look before: none while it is not judged.
    pub fn due(&self) -> Option<Instant> {
        match self.record.seen() {
            Seen::Learned { slowest, .. } if self.judged => self.last.checked_add(slowest),
            _ => None,
        }
    }

    /// What a look at its party found since the look before, made now: refused where the
    /// party made no progress at all over the time since, a whole W* or more. A look before
    /// it is due, or of a wait not judged, judges nothing.
    pub fn found(&mut self, found: Found) -> Result<(), Stall> {
        let Some(due) = self.due() else {
            return Ok(());
        };
        let now = Instant::now();
        if now < due {
            return Ok(());
        }
        let Seen::Learned { slowest, least } = self.record.seen() else {
            return Ok(());
        };
        let then = std::mem::replace(&mut self.last, now);
        match found {
            Found::Progressed => Ok(()),
            Found::Gone => {
                self.judged = false;
                Ok(())
            }
            Found::Still => Err(Stall::new(
                now.saturating_duration_since(self.began),
                now.saturating_duration_since(then),
                least,
                slowest,
            )),
        }
    }

    /// Its party answered, having done `work` since the wait began: its kind learns from
    /// the answer. Only the party's own answers teach: never an end its waiter made.
    pub fn answered(&self, work: Duration) {
        self.record.teach(self.began.elapsed(), work);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rate is less than another's as cross-multiplication says, with no float: equal
    /// rates are neither below the other, none at all is below any, and a duration past
    /// u64's nanoseconds is its most rather than an overflow (D215).
    #[test]
    fn a_rate_is_compared_by_cross_multiplication() {
        let rate = |cpu: u64, over: u64| Rate {
            cpu: Duration::from_nanos(cpu),
            over: Duration::from_nanos(over),
        };
        assert!(rate(1, 2).below(&rate(1, 1)));
        assert!(!rate(1, 1).below(&rate(1, 2)));
        assert!(!rate(2, 4).below(&rate(1, 2)));
        assert!(!rate(1, 2).below(&rate(2, 4)));
        assert!(rate(0, 1).below(&rate(1, u64::MAX)));
        let most = Rate {
            cpu: Duration::MAX,
            over: Duration::MAX,
        };
        assert!(!most.below(&rate(u64::MAX, u64::MAX)));
        assert!(rate(1, 2).below(&most));
    }

    /// A record learns its slowest answer and its least rate from its first answer on: its
    /// slowest only rises, its least rate only falls, and a wait no time long teaches nothing
    /// (D215).
    #[test]
    fn a_record_learns_its_slowest_answer_and_its_least_rate() {
        let ms = Duration::from_millis;
        let record = Record::new();
        assert_eq!(record.seen(), Seen::Cold);
        let learned = |slowest, cpu, over| Seen::Learned {
            slowest,
            least: Rate { cpu, over },
        };
        record.teach(ms(2), ms(1));
        assert_eq!(record.seen(), learned(ms(2), ms(1), ms(2)));
        record.teach(ms(1), ms(1));
        assert_eq!(record.seen(), learned(ms(2), ms(1), ms(2)));
        record.teach(ms(4), ms(1));
        assert_eq!(record.seen(), learned(ms(4), ms(1), ms(4)));
        record.teach(Duration::ZERO, Duration::ZERO);
        assert_eq!(record.seen(), learned(ms(4), ms(1), ms(4)));
    }

    /// A wait whose kind has no answer is never due and judges nothing, whatever a look says,
    /// however long it runs: liveness alone. One that began cold stays so though its record
    /// learns meanwhile (D215).
    #[test]
    fn a_cold_wait_follows_liveness_alone() {
        let record = Record::new();
        let mut judge = Judge::begin(&record);
        assert_eq!(judge.due(), None);
        assert_eq!(judge.found(Found::Still), Ok(()));
        record.teach(Duration::from_nanos(1), Duration::ZERO);
        assert_eq!(judge.due(), None);
        assert_eq!(judge.found(Found::Still), Ok(()));
    }

    /// A judged wait's first look is due W* past its start and each after it W* past the look
    /// before; a look before it is due judges nothing; a look that finds progress moves the
    /// next one on; one that finds none refuses it, with how far into the wait, the time
    /// looked over, R* and W*; and one that finds its party gone ends its judging (D215).
    #[test]
    fn a_learned_wait_is_refused_at_a_look_that_finds_no_progress() {
        let slowest = Duration::from_millis(30);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(3));
        let mut judge = Judge::begin(&record);
        let first = judge.due().unwrap();
        assert_eq!(first, judge.began + slowest);
        assert_eq!(judge.found(Found::Still), Ok(()), "before it is due");
        assert_eq!(judge.due(), Some(first));
        while Instant::now() < first {
            std::thread::sleep(first.saturating_duration_since(Instant::now()));
        }
        assert_eq!(judge.found(Found::Progressed), Ok(()));
        let second = judge.due().unwrap();
        assert!(second >= first + slowest, "{second:?} {first:?}");
        while Instant::now() < second {
            std::thread::sleep(second.saturating_duration_since(Instant::now()));
        }
        let stall = judge.found(Found::Still).unwrap_err();
        assert_eq!(stall.slowest(), slowest);
        assert_eq!(
            stall.least(),
            Rate {
                cpu: Duration::from_millis(3),
                over: slowest
            }
        );
        assert!(stall.over() >= slowest, "{stall:?}");
        assert!(stall.waited() >= slowest * 2, "{stall:?}");
        let mut gone = Judge::begin(&record);
        let due = gone.due().unwrap();
        while Instant::now() < due {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        assert_eq!(gone.found(Found::Gone), Ok(()));
        assert_eq!(gone.due(), None);
        assert_eq!(gone.found(Found::Still), Ok(()));
    }

    /// An answer teaches its kind the wall time since its wait began and the work done
    /// meanwhile (D215).
    #[test]
    fn an_answer_teaches_its_kind() {
        let record = Record::new();
        let judge = Judge::begin(&record);
        std::thread::sleep(Duration::from_millis(2));
        judge.answered(Duration::from_millis(1));
        let Seen::Learned { slowest, least } = record.seen() else {
            panic!("an answer taught nothing");
        };
        assert!(slowest >= Duration::from_millis(2), "{slowest:?}");
        assert_eq!(least.cpu, Duration::from_millis(1));
        assert_eq!(least.over, slowest);
    }
}
