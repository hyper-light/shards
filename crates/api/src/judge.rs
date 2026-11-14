//! The judge of a wait that follows its party (D215), apart from how the party is looked at:
//! what a kind of answer has taught ([`Record`]: W*, the slowest answer seen, and R*, the
//! least rate a party still answered at), and a wait's schedule and verdict ([`Judge`]). A
//! wait that begins before its kind's first answer follows its party's liveness alone; one
//! that begins once its kind has an answer looks at its party once each W* past its start,
//! and is refused, [`Stall`], at a look that finds it has made no progress at all over a
//! whole W* since a look last found it progressed or held, as far as the looks saw it (from
//! that look's end to this one's beginning, [`Span`], a window never shorter than their
//! counters' resolution; a look with nothing before it, [`Found::Based`], shows nothing),
//! its host not the reason: a party
//! its host held from running (runnable on no CPU, stalled on its host's memory) is live,
//! and a stopped or frozen one stalled; and the time the judge's own looks ran late, past
//! when each was meant, is its host's hold of everyone, which no party's window counts
//! (ROOT, 2026-11-13). R* is the refusal's diagnosis, never its bound. What a look finds is its
//! caller's to say ([`Found`]): shards-api's waits read their guest's VM process
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
    pub fn below(&self, other: &Rate) -> bool {
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

    /// The time, a whole [`slowest`](Stall::slowest) or more, over which its party made no
    /// progress at all since a look last found it progressed or held: the time its judge's
    /// own looks ran late, its host holding the judge, not counted.
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

impl Record {
    /// Whether a wait of this kind whose party has made no progress since `since` is stalled
    /// by `now`, judged from outside the wait, as one that cannot look itself is (a writer
    /// blocked in its write): none while the kind is cold, or less than W* has passed; else
    /// the stall, `now - since` both how far into its stillness and the time judged over.
    pub fn stalled(&self, since: Instant, now: Instant) -> Option<Stall> {
        let Seen::Learned { slowest, least } = self.seen() else {
            return None;
        };
        let over = now.saturating_duration_since(since);
        (over >= slowest).then(|| Stall::new(over, over, least, slowest))
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
    /// Nothing to judge by: the look had no look before it to compare with (its party's
    /// first, or one that first found a process of it), so it shows neither progress nor
    /// stillness. The evidence of a stillness begins at its end ([`Span::ended`]).
    Based,
    /// It made progress, or its end is under way, which judges nothing: its end is the
    /// wait's answer.
    Progressed,
    /// It made none, its host holding it from running (runnable on no CPU, or stalled on its
    /// host's memory): live, not stalled.
    Held,
    /// It made none of its own: it waits on what has not come, or is stopped or frozen.
    Still,
    /// It has gone: the wait follows its liveness alone from here, its end the wait's.
    Gone,
}

/// When a look read its party, and how finely: it began reading at `began` and had read all
/// it reads by `ended`, so that two looks that found no change show none over at least the
/// time from the first's end to the second's beginning, and no less; and `resolution`, the
/// least time its counters' least change stands for (a CPU clock's own, clock_getres(2)), so
/// that a stillness shorter than it is no evidence of none. A look of no counter that coarse
/// has a resolution of zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub began: Instant,
    pub ended: Instant,
    pub resolution: Duration,
}

impl Span {
    /// A look made at `at` that took no time, of counters that show every change.
    pub fn at(at: Instant) -> Span {
        Span {
            began: at,
            ended: at,
            resolution: Duration::ZERO,
        }
    }
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
    /// When the look before was meant to be, or its beginning: its looks are due each W* past
    /// its start, whenever each ran.
    meant: Instant,
    /// When the look before ended, or its beginning.
    last: Instant,
    /// The end of the look from which every judged look since found its party still: the
    /// last that found it progressed, held or based ([`Found::Based`]), or its beginning,
    /// where its caller took its baseline as it began; and how long its own looks have run
    /// late since, past when each was meant and after the look before ended: the time its
    /// host held the judge itself, which no party's window without progress counts (ROOT's
    /// ruling, 2026-11-13).
    since: Instant,
    held: Duration,
}

impl<'a> Judge<'a> {
    /// A wait beginning now, learning into and judged by `record`, its caller's first look at
    /// its party taken as it begins: one taken later says so ([`Found::Based`]).
    pub fn begin(record: &'a Record) -> Judge<'a> {
        let began = Instant::now();
        Judge {
            record,
            judged: matches!(record.seen(), Seen::Learned { .. }),
            began,
            meant: began,
            last: began,
            since: began,
            held: Duration::ZERO,
        }
    }

    /// When its next look is due, W* after the look before was meant: none while it is not
    /// judged.
    pub fn due(&self) -> Option<Instant> {
        match self.record.seen() {
            Seen::Learned { slowest, .. } if self.judged => self.meant.checked_add(slowest),
            _ => None,
        }
    }

    /// What a look at its party found since the look before, made now: refused where the
    /// party made no progress at all over a whole W* or more since a look last found it
    /// progressed or held, the time its own looks ran late not counted. A look before it is
    /// due, or of a wait not judged, judges nothing.
    pub fn found(&mut self, found: Found) -> Result<(), Stall> {
        self.found_over(found, Span::at(Instant::now()))
    }

    /// [`found`](Self::found), of a look made over `span`. Its party is judged still only
    /// over what the looks saw: from the end of the look from which it has been still to
    /// this look's beginning, less the time the judge's own looks ran late, and refused only
    /// where that is a whole window, W* or its counters' resolution where that is coarser
    /// (#77: a W* below one look's span refused waits whose first look had nothing before it).
    /// A based look ([`Found::Based`]) starts its evidence anew whether it is due or not.
    pub fn found_over(&mut self, found: Found, span: Span) -> Result<(), Stall> {
        if found == Found::Based {
            (self.since, self.held, self.last) = (span.ended, Duration::ZERO, span.ended);
        }
        let Some(due) = self.due() else {
            return Ok(());
        };
        let now = span.began;
        if now < due {
            return Ok(());
        }
        let Seen::Learned { slowest, least } = self.record.seen() else {
            return Ok(());
        };
        // Held from when it was meant, or from the look before's end where that ended later.
        let late = now.saturating_duration_since(due.max(self.last));
        self.held = self.held.saturating_add(late);
        self.meant = Self::latest_meant(due, slowest, now);
        self.last = span.ended;
        match found {
            Found::Progressed | Found::Held | Found::Based => {
                (self.since, self.held) = (span.ended, Duration::ZERO);
                Ok(())
            }
            Found::Gone => {
                self.judged = false;
                Ok(())
            }
            Found::Still => {
                let free = now
                    .saturating_duration_since(self.since)
                    .saturating_sub(self.held);
                if free < slowest.max(span.resolution) {
                    return Ok(());
                }
                Err(Stall::new(
                    now.saturating_duration_since(self.began),
                    free,
                    least,
                    slowest,
                ))
            }
        }
    }

    /// [`found`](Self::found), the look made at `now`: the look clock, which a test sets.
    #[cfg(test)]
    fn found_at(&mut self, found: Found, now: Instant) -> Result<(), Stall> {
        self.found_over(found, Span::at(now))
    }

    /// The last look's time meant at or before `now`, of those each `slowest` from `due`: a
    /// judge its host held past several looks makes one, not one at once for each it missed.
    /// `slowest` is never zero (an answer no time long teaches nothing); past u64's
    /// nanoseconds, or past what an `Instant` holds, it is `now`.
    fn latest_meant(due: Instant, slowest: Duration, now: Instant) -> Instant {
        let behind = now.saturating_duration_since(due).as_nanos();
        let missed = behind.checked_div(slowest.as_nanos()).unwrap_or(0);
        let skip = missed.saturating_mul(slowest.as_nanos());
        u64::try_from(skip)
            .ok()
            .and_then(|ns| due.checked_add(Duration::from_nanos(ns)))
            .unwrap_or(now)
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

    /// A judged wait's first look is due W* past its start and each after it W* past when the
    /// look before was meant; a look before it is due judges nothing; a look that finds
    /// progress starts its party's window anew; one that finds none over a whole W* refuses
    /// it, with how far into the wait, the time looked over, R* and W*; and one that finds its
    /// party gone ends its judging (D215). Its looks run on an injected clock, on time.
    #[test]
    fn a_learned_wait_is_refused_at_a_look_that_finds_no_progress() {
        let slowest = Duration::from_millis(30);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(3));
        let mut judge = Judge::begin(&record);
        let began = judge.began;
        let first = judge.due().unwrap();
        assert_eq!(first, began + slowest);
        assert_eq!(
            judge.found_at(Found::Still, first - Duration::from_nanos(1)),
            Ok(()),
            "before it is due"
        );
        assert_eq!(judge.due(), Some(first));
        assert_eq!(judge.found_at(Found::Progressed, first), Ok(()));
        let second = judge.due().unwrap();
        assert_eq!(second, first + slowest);
        let stall = judge.found_at(Found::Still, second).unwrap_err();
        assert_eq!(stall.slowest(), slowest);
        assert_eq!(
            stall.least(),
            Rate {
                cpu: Duration::from_millis(3),
                over: slowest
            }
        );
        assert_eq!((stall.over(), stall.waited()), (slowest, slowest * 2));
        let mut gone = Judge::begin(&record);
        let due = gone.due().unwrap();
        assert_eq!(gone.found_at(Found::Gone, due), Ok(()));
        assert_eq!(gone.due(), None);
        assert_eq!(gone.found_at(Found::Still, due + slowest * 9), Ok(()));
    }

    /// A party its host holds from running, which made no progress over a whole W*, is live:
    /// its look refuses nothing and the next is due W* after it was meant; one that, so held
    /// no more, made none of its own over a whole W* since is refused (ROOT's ruling of
    /// 2026-11-13).
    #[test]
    fn a_party_its_host_holds_is_live_not_stalled() {
        let slowest = Duration::from_millis(20);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(1));
        let mut judge = Judge::begin(&record);
        for _ in 0..3 {
            let due = judge.due().unwrap();
            assert_eq!(judge.found_at(Found::Held, due), Ok(()));
            assert_eq!(judge.due(), Some(due + slowest));
        }
        let due = judge.due().unwrap();
        let stall = judge.found_at(Found::Still, due).unwrap_err();
        assert_eq!((stall.over(), stall.waited()), (slowest, slowest * 4));
    }

    /// A judge its host held past several of its looks makes one look as it runs, not one at
    /// once for each it missed, and its next is due at the next W* past its start. The time
    /// it ran late is held: a party it found progressed at a late look, still since, has had
    /// less than a whole W* free at the next look however late that ran, and is refused only
    /// at a look on time once it has had a whole W* free (ROOT's ruling, 2026-11-13).
    #[test]
    fn a_judge_held_past_several_looks_looks_once_and_keeps_its_schedule() {
        let ms = Duration::from_millis;
        let slowest = ms(10);
        let record = Record::new();
        record.teach(slowest, ms(1));
        let mut judge = Judge::begin(&record);
        let began = judge.began;
        assert_eq!(judge.found_at(Found::Progressed, began + ms(14)), Ok(()));
        assert_eq!(judge.due(), Some(began + ms(20)));
        assert_eq!(judge.found_at(Found::Still, began + ms(75)), Ok(()), "6 ms free");
        assert_eq!(judge.due(), Some(began + ms(80)));
        let stall = judge.found_at(Found::Still, began + ms(80)).unwrap_err();
        assert_eq!((stall.over(), stall.waited()), (ms(11), ms(80)));
    }

    /// The start that failed on a loaded host (ROOT's HVF run at d513d0a0b, containers.rs
    /// `a_runs_log_is_sent_to_gelf_as_its_options_say`: "it ran nothing for 101.091209ms,
    /// past the slowest start seen (98.915708ms), 202.999ms after it began"), replayed on an
    /// injected look clock: its first look ran 101.908ms in and found progress, its second
    /// 202.999ms in found none. That second look was meant W* past the first's meant time
    /// (D215's looks each W* past the wait's start), and the time the judge ran late past it
    /// was its host's hold, not the party's window: its party had less than a whole W* free,
    /// and goes on. A look on time at the next due, its party still, is refused, over the
    /// window its host did not hold the judge in (ROOT's ruling, 2026-11-13).
    #[test]
    fn a_look_its_host_held_late_counts_its_lateness_as_held() {
        let us = Duration::from_micros;
        let slowest = Duration::from_nanos(98_915_708);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(3));
        let mut judge = Judge::begin(&record);
        let began = judge.began;
        assert_eq!(judge.found_at(Found::Progressed, began + us(101_908)), Ok(()));
        assert_eq!(
            judge.found_at(Found::Still, began + us(202_999)),
            Ok(()),
            "the judge's own lateness counted as the party's stillness"
        );
        let due = judge.due().unwrap();
        assert_eq!(due, began + slowest * 3, "looks each W* past the wait's start");
        let stall = judge.found_at(Found::Still, due).unwrap_err();
        let late = us(202_999) - slowest * 2;
        assert_eq!(stall.over(), slowest * 3 - us(101_908) - late);
        assert_eq!(stall.waited(), slowest * 3);
        assert_eq!(stall.slowest(), slowest);
    }

    /// A judge that looks on time at each due, its party stopped from the start, refuses it
    /// at its first look, over a whole W* (ROOT's ruling, 2026-11-13: a host's hold of the
    /// judge is never one it did not show).
    #[test]
    fn a_judge_on_time_refuses_a_stopped_party_at_its_first_look() {
        let slowest = Duration::from_millis(30);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(3));
        let mut judge = Judge::begin(&record);
        let due = judge.due().unwrap();
        let stall = judge.found_at(Found::Still, due).unwrap_err();
        assert_eq!((stall.over(), stall.waited()), (slowest, slowest));
    }

    /// A wait judged from outside, as one blocked in a write is, is stalled once its party
    /// has made no progress for a whole W* since: never while its kind is cold, nor before
    /// W* has passed, nor for a `now` before its `since` (D215; ROOT, 2026-11-13).
    #[test]
    fn a_wait_judged_from_outside_is_stalled_a_whole_w_star_after_its_last_progress() {
        let ms = Duration::from_millis;
        let record = Record::new();
        let since = Instant::now();
        assert_eq!(
            record.stalled(since, since + Duration::from_secs(3600)),
            None,
            "cold"
        );
        record.teach(ms(30), ms(3));
        assert_eq!(record.stalled(since, since + ms(29)), None);
        let stall = record.stalled(since, since + ms(31)).unwrap();
        assert_eq!(
            (stall.waited(), stall.over(), stall.slowest()),
            (ms(31), ms(31), ms(30))
        );
        assert_eq!(
            stall.least(),
            Rate {
                cpu: ms(3),
                over: ms(30)
            }
        );
        assert_eq!(record.stalled(since + ms(40), since), None, "no time has passed");
    }

    /// A look over `began..ended` past the judge's start, of counters as fine as
    /// `resolution`.
    fn span(judge: &Judge<'_>, began: Duration, ended: Duration, resolution: Duration) -> Span {
        Span {
            began: judge.began + began,
            ended: judge.began + ended,
            resolution,
        }
    }

    /// Looks on time at each due, each no earlier than the look before ended, all finding
    /// its party still, until one refuses it: when that look began past the judge's start,
    /// and the stall. None in `looks` looks.
    fn still_until_refused(
        judge: &mut Judge<'_>,
        resolution: Duration,
        looks: usize,
    ) -> Option<(Duration, Stall)> {
        for _ in 0..looks {
            let at = judge.due()?.max(judge.last);
            let look = Span {
                began: at,
                ended: at,
                resolution,
            };
            if let Err(stall) = judge.found_over(Found::Still, look) {
                return Some((at - judge.began, stall));
            }
        }
        None
    }

    /// #77's stream wait (ROOT's KVM run at c120c8a95, managed_lifecycle.rs:209: "a guest's
    /// stream made no progress for 3.527µs, past the slowest answer of its kind seen on this
    /// host (3.527µs), 18.416389ms into its wait ... its last progress its start"), replayed:
    /// its W* far below what one look at its party takes, its first look, about 18.416 ms
    /// long, found every process of it anew, with nothing before it to compare with, and the
    /// look right after refused it. That first look is based and judges nothing, nor does
    /// the next, which began 1 µs after its end, having seen 1 µs; a wait
    /// whose looks then see it still is refused, but never before they have seen a whole W*
    /// past the first look's end.
    #[test]
    fn a_first_look_longer_than_w_star_is_based_not_still() {
        let us = Duration::from_micros;
        let ns = Duration::from_nanos;
        let slowest = ns(3_527);
        let record = Record::new();
        record.teach(slowest, Duration::ZERO);
        let mut judge = Judge::begin(&record);
        let first = span(&judge, Duration::ZERO, us(18_416), ns(1));
        assert_eq!(
            judge.found_over(Found::Based, first),
            Ok(()),
            "a first look judges nothing"
        );
        let next = span(&judge, us(18_417), us(18_418), ns(1));
        assert_eq!(
            judge.found_over(Found::Still, next),
            Ok(()),
            "1 µs seen of a 3.527 µs W*"
        );
        let (at, stall) = still_until_refused(&mut judge, ns(1), 8).unwrap();
        assert!(stall.over() >= slowest, "{stall:?}");
        assert!(at >= us(18_416) + slowest, "{at:?}");
        assert_eq!(stall.waited(), at);
    }

    /// #77's `shards top` wait (ROOT's KVM run at c120c8a95, common/mod.rs:2601: "made no
    /// progress for 13.996303ms, past the slowest answer of its kind seen on this host
    /// (13.996303ms), 31.066531ms into its wait ... its last progress its start"), replayed:
    /// its first look, which found its processes anew, began as the wait did and ended
    /// about 29 ms in, past its first due (its judged look began 31.066 ms in, after 2 ms of
    /// sleep). Those looks saw 2 ms of stillness, all of it the judge's
    /// own lateness, not a W*: it goes on. Looked at on time from then, it is refused at the
    /// first look that has seen a whole W* past the first look's end, the lateness not
    /// counted: its fourth due.
    #[test]
    fn a_window_is_judged_from_its_baselines_end() {
        let us = Duration::from_micros;
        let slowest = Duration::from_nanos(13_996_303);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(1));
        let mut judge = Judge::begin(&record);
        let first = span(&judge, Duration::ZERO, us(29_000), Duration::ZERO);
        assert_eq!(judge.found_over(Found::Based, first), Ok(()));
        let second = span(&judge, us(31_066), us(31_100), Duration::ZERO);
        assert_eq!(
            judge.found_over(Found::Still, second),
            Ok(()),
            "2 ms seen of a 14 ms W*"
        );
        let (at, stall) = still_until_refused(&mut judge, Duration::ZERO, 8).unwrap();
        assert_eq!(at, slowest * 4);
        let late = us(31_066) - us(29_000);
        assert_eq!(stall.over(), slowest * 4 - us(29_000) - late);
        assert_eq!(stall.waited(), slowest * 4);
    }

    /// A party read by a counter coarser than W* (one that counts clock ticks, as /proc's
    /// utime does at USER_HZ) is judged over its counter's resolution where that is longer:
    /// looked at on time each W*, it goes on while its looks have seen less than a whole
    /// tick of stillness, and is refused at the look that has seen one.
    #[test]
    fn a_counter_coarser_than_w_star_widens_the_window_to_its_resolution() {
        let ms = Duration::from_millis;
        let slowest = Duration::from_micros(500);
        let tick = ms(10);
        let record = Record::new();
        record.teach(slowest, Duration::ZERO);
        let mut judge = Judge::begin(&record);
        let first = span(&judge, Duration::ZERO, Duration::ZERO, tick);
        assert_eq!(judge.found_over(Found::Based, first), Ok(()));
        let (at, stall) = still_until_refused(&mut judge, tick, 40).unwrap();
        assert_eq!(at, tick);
        assert_eq!((stall.over(), stall.slowest()), (tick, slowest));
    }

    /// A based look starts the evidence anew whether or not it is due: one that found a
    /// process of its party first, late in the wait, leaves the time before it unjudged.
    #[test]
    fn a_based_look_starts_the_evidence_anew_before_its_due() {
        let ms = Duration::from_millis;
        let slowest = ms(10);
        let record = Record::new();
        record.teach(slowest, Duration::ZERO);
        let mut judge = Judge::begin(&record);
        assert_eq!(
            judge.found_over(Found::Based, span(&judge, ms(8), ms(9), Duration::ZERO)),
            Ok(())
        );
        assert_eq!(
            judge.found_over(Found::Still, span(&judge, ms(10), ms(10), Duration::ZERO)),
            Ok(()),
            "1 ms seen"
        );
        let stall = judge
            .found_over(Found::Still, span(&judge, ms(20), ms(20), Duration::ZERO))
            .unwrap_err();
        assert_eq!(stall.over(), ms(11));
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
