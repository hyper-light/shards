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
//! (ROOT, 2026-11-13), but for the time its caller spent on its own work meanwhile, its own
//! polls (D209 Part 8), which is no hold ([`Span::own`]). A party that does progress is
//! refused too where it is too slow to be answering: past W* of its own unheld time, its
//! work since its start over that time below R*, and that work already more than any answer
//! of its kind took (C*) (D215, b24: a guest asleep for good, its VM process's housekeeping
//! ticks its only CPU, was "progressing" for 62 minutes), judged only over the windows its
//! caller saw it run unheld ([`Span::work`]), and only once two answers have taught R* and C*
//! ([`SPREAD`]). What a look finds is its
//! caller's to say ([`Found`]): shards-api's waits read their guest's VM process
//! (`progress::Follow`), and so do the daemon's waits on a microVM's own answers (D188).
//!
//! A crate of its own, of std alone, which shards-api re-exports: the tests' waits on their
//! own threads (shards-testwait) judge by the same rule, and shards-remote's tests, beneath
//! shards-api, take them.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

#[cfg(windows)]
pub mod threads;
#[cfg(windows)]
pub use threads::{ThreadsHeld, threads_held, threads_said, threads_switched};

/// A party's work, its threads' run, as its OS counts it exactly: D215's rate and C* are of it.
/// On Linux and macOS the CPU time its threads ran, in nanoseconds (Linux's CPU clock, macOS's
/// Mach units converted). On Windows the cycles they used (QueryProcessCycleTime,
/// QueryThreadCycleTime: "The number of CPU clock cycles used by the threads of the process.
/// This value includes cycles spent in both user mode and kernel mode", Microsoft Learn), which
/// Windows accumulates as they run, where it charges their kernel and user times a whole clock
/// tick at a time to the thread running as the tick comes (GetProcessTimes: steps of 15.625 ms,
/// PM M294; Windows Internals 6th ed., Part 1, ch. 5, "Examining Thread Activity"): a stand-in
/// busy 50 ms read as none (CI 44076016255), and R* and C* of two and eleven ticks refused a
/// party of thirteen (CI 44347282760). Cycles are never made a time: "Do not attempt to
/// convert the CPU clock cycles returned by QueryThreadCycleTime to elapsed time. This function
/// uses timer services provided by the CPU, which can vary in implementation" (Microsoft
/// Learn); so work is weighed only against work counted by the same unit on the same CPU
/// model, as a kept record names them ([`Work::UNIT`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Work(u64);

impl Work {
    /// None.
    pub const ZERO: Work = Work(0);

    /// What its counts are, as a record kept beyond its process names them beside its CPU's
    /// model: Linux's and macOS's CPU nanoseconds.
    #[cfg(not(windows))]
    pub const UNIT: &'static str = "cpu-ns";

    /// What its counts are, as a record kept beyond its process names them beside its CPU's
    /// model: Windows' cycles.
    #[cfg(windows)]
    pub const UNIT: &'static str = "cycles";

    /// `count` of [`Work::UNIT`]: a count its OS gave, or one a record kept of it.
    pub const fn of(count: u64) -> Work {
        Work(count)
    }

    /// CPU time, as Linux and macOS count work: its nanoseconds, past u64's (584 years) its
    /// most.
    #[cfg(not(windows))]
    pub fn of_cpu(cpu: Duration) -> Work {
        Work(ns(cpu))
    }

    /// Its count, of [`Work::UNIT`].
    pub const fn count(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn saturating_add(self, other: Work) -> Work {
        Work(self.0.saturating_add(other.0))
    }

    #[must_use]
    pub const fn saturating_sub(self, other: Work) -> Work {
        Work(self.0.saturating_sub(other.0))
    }
}

/// As an account says it: on Linux and macOS its CPU time ("12.5ms of CPU"), on Windows its
/// cycles ("31250000 cycles"), never a time made of them.
impl std::fmt::Display for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(not(windows))]
        return write!(f, "{:?} of CPU", Duration::from_nanos(self.0));
        #[cfg(windows)]
        return write!(f, "{} cycles", self.0);
    }
}

/// A rate of work: a party's work had over a wall time, kept as the pair it was measured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Its work, every thread's ([`Work`]).
    pub work: Work,
    /// The wall time it was had over.
    pub over: Duration,
}

impl Rate {
    /// Whether this rate is less than `other`: `work / over < other.work / other.over`, as
    /// `work × other.over < other.work × over`, with no division and no float. Each side is
    /// the product of two u64s, which no u128 overflows; a duration past u64's nanoseconds
    /// (584 years) counts as its most.
    pub fn below(&self, other: &Rate) -> bool {
        let wall = |d: Duration| u128::from(ns(d));
        let work = |w: Work| u128::from(w.count());
        work(self.work).saturating_mul(wall(other.over)) < work(other.work).saturating_mul(wall(self.over))
    }
}

/// What a wait that follows its party found as it refused it
/// (shards-api's `Refusal::Stalled`). Kept in nanoseconds, each past u64's (584
/// years) its most, and its kind's rates and its threads' account boxed apart, so that a
/// refusal is no larger for them (clippy's `result_large_err` over a pair of refusals,
/// crates/api/tests/sim.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stall {
    waited: u64,
    over: u64,
    slowest: u64,
    /// Its kind's R* and C* where an answer of it was rated, and its own rate where it was
    /// refused for running below R* ([`Stall::rate`]).
    rates: Option<Box<Rates>>,
    /// Its party's threads as its caller read them as it refused it ([`Stall::said`]).
    threads: Option<Box<str>>,
}

/// A [`Stall`]'s rates: R*'s work ([`Work`]'s count) and wall (nanoseconds), C*'s work, and
/// its own rate's work and wall where that was its refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rates {
    least: [u64; 2],
    most: u64,
    rate: Option<[u64; 2]>,
}

fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn rate_of([work, over]: [u64; 2]) -> Rate {
    Rate {
        work: Work(work),
        over: Duration::from_nanos(over),
    }
}

impl Stall {
    fn new(waited: Duration, over: Duration, seen: Learned) -> Stall {
        Stall {
            waited: ns(waited),
            over: ns(over),
            slowest: ns(seen.slowest),
            rates: seen.rated.map(|rated| {
                Box::new(Rates {
                    least: [rated.least.work.count(), ns(rated.least.over)],
                    most: rated.most.count(),
                    rate: None,
                })
            }),
            threads: None,
        }
    }

    /// It, with its party's threads as `threads` says them, read as it was refused: each
    /// one's state and, where it waits, on what, as its OS keeps them (Windows: its state and
    /// wait reason, as `threads_said` reads them; Linux: its state and the kernel function it
    /// sleeps in; macOS: its run state), so that a refusal says what its party was doing, never
    /// at a look (ROOT, 2026-11-18: CI 44189693085's Windows refusals said "0 of 3 threads
    /// runnable" and no more). None where its caller read none.
    #[must_use]
    pub fn said(mut self, threads: Option<String>) -> Stall {
        self.threads = threads.map(String::into_boxed_str);
        self
    }

    /// Its party's threads as they were read as it was refused ([`Stall::said`]).
    pub fn threads(&self) -> Option<&str> {
        self.threads.as_deref()
    }

    /// Its threads' account as a refusal's sentence ends with it, after its times: "; its
    /// threads: " and what each was doing ([`Stall::threads`]); nothing where none was read.
    /// One clause for every refusal that says a stall.
    pub fn account(&self) -> String {
        self.threads()
            .map_or_else(String::new, |threads| format!("; its threads: {threads}"))
    }

    /// A wait refused for running below R*: `rate` its work over its unheld time, more work
    /// than C*.
    fn slow(waited: Duration, rate: Rate, seen: Learned) -> Stall {
        let mut stall = Stall::new(waited, rate.over, seen);
        if let Some(rates) = stall.rates.as_mut() {
            rates.rate = Some([rate.work.count(), ns(rate.over)]);
        }
        stall
    }

    /// Where it was refused for running too slowly to be answering: its party's work since
    /// its start over the time it was seen to run unheld (`over`), below [`least`](Stall::least)
    /// and more than [`most`](Stall::most). None where it was refused for making no progress
    /// at all.
    pub fn rate(&self) -> Option<Rate> {
        self.rates.as_ref()?.rate.map(rate_of)
    }

    /// The most work an answer of its kind took (C*), where one was rated.
    pub fn most(&self) -> Option<Work> {
        self.rates.as_ref().map(|rates| Work(rates.most))
    }

    /// How far into the wait it was found.
    pub fn waited(&self) -> Duration {
        Duration::from_nanos(self.waited)
    }

    /// The time, a whole [`slowest`](Stall::slowest) or more, over which its party made no
    /// progress at all since a look last found it progressed or held: the time its judge's
    /// own looks ran late, its host holding the judge, not counted. Of one refused for its
    /// [`rate`](Stall::rate), the time it was seen to run unheld, that rate's wall.
    pub fn over(&self) -> Duration {
        Duration::from_nanos(self.over)
    }

    /// The least rate at which a party of the wait's kind still answered (R*), where one was
    /// rated: the bound a party past W* runs below and is refused ([`rate`](Stall::rate)).
    pub fn least(&self) -> Option<Rate> {
        self.rates.as_ref().map(|rates| rate_of(rates.least))
    }

    /// The slowest answer of the wait's kind seen (W*), past which it was looked at.
    pub fn slowest(&self) -> Duration {
        Duration::from_nanos(self.slowest)
    }
}

/// What a process has seen of a kind of answer (shards-api's `Shards::seen`,
/// [`Record::seen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// No answer yet: a wait follows its party's liveness alone.
    Cold,
    /// The slowest answer seen (W*), from a wait's start to its answer, past which a wait
    /// looks at its party; and, once an answer's work was read through its end, its kind's
    /// rates ([`Rated`]). Without them a wait is judged by its stillness alone.
    Learned { slowest: Duration, rated: Option<Rated> },
}

/// What answers whose work was read through their end taught a kind (D215): the least rate
/// at which a party still answered (R*), the most work an answer took (C*), and how many such
/// answers taught them. A party past W* below R* that has done more than C* is too slow to be
/// answering ([`Stall::rate`]), once [`SPREAD`] answers have taught them. An answer whose work
/// was not read through its end (its process reaped before a look read its last CPU) teaches
/// neither: R* of none, from no reading, would hold no floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rated {
    pub least: Rate,
    pub most: Work,
    /// How many answers taught R* and C*, as far as this record knows: each answer taught it
    /// adds one ([`Record::teach`]); a record merged in ([`Record::merge`]) counts as many as
    /// the more of the two say, never their sum, as a kept record merged in again would count
    /// its answers twice. So no answer is counted twice, and two processes' answers may be
    /// counted as one's: a count low, which refuses by rate later, never sooner.
    pub answers: u64,
}

/// The fewest answers whose R* and C* refuse a party by its rate ([`Rated`]): one answer's
/// least rate and most work are that one answer, so two is the fewest with any spread. A
/// derivation, not a tuning (D215, ROOT's ruling of 2026-11-17: four waits, each refused
/// past a kind one answer had taught, on CI's loaded x86_64 runners, live and progressing,
/// one with its answer already printed). A teardown's C* refuses by the same count (D217,
/// the daemon's `ends`).
pub const SPREAD: u64 = 2;

/// The due looks within which a wait whose party is still throughout is refused, its judge
/// unheld: its first may find its party's baseline or its caller's own first run
/// ([`Found::Based`], [`Found::Progressed`]), a look that ends a moment past its due time, and
/// the stillness counted from that end ([`Judge::found_over`]) is less than a window at the
/// next due look and a whole one at the one after. A derivation of the rule, not a tuning, as
/// `SPREAD` is: what a test of a still party counts its due looks against.
pub const LOOKS_TO_REFUSE: u32 = 3;

/// [`Seen::Learned`]'s, together.
#[derive(Debug, Clone, Copy)]
struct Learned {
    slowest: Duration,
    rated: Option<Rated>,
}

impl Seen {
    fn learned(self) -> Option<Learned> {
        match self {
            Seen::Cold => None,
            Seen::Learned { slowest, rated } => Some(Learned { slowest, rated }),
        }
    }
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
    /// (read through its end): the slowest rises to it where it is slower, the least rate
    /// falls to its where its is less, the most work rises to its where it is more. A wait no
    /// time long has no rate and teaches nothing. One whose work read none teaches its slowest
    /// alone, as one not read through its end does: an answer its party ran nothing its
    /// counters show for says nothing of the least rate a working party answers at, and R* 0
    /// holds no floor. Windows' process times move in its clock's ticks (15.625 ms): a
    /// stand-in busy 50 ms on a busy host read as none, R* 0 ns over 70.6 ms, after which a
    /// party ticking 1 ms in 11 was served 22.9 s in (CI 44076016255, arm64 Windows,
    /// harness.rs' `a_command_asleep_but_ticking_is_refused_and_a_busy_one_goes_on`); its
    /// work is its cycles since ([`Work`]), which any run moves.
    pub fn teach(&self, wall: Duration, work: Work) {
        if wall.is_zero() {
            return;
        }
        if work.is_zero() {
            return self.teach_unrated(wall);
        }
        let answer = Learned {
            slowest: wall,
            rated: Some(Rated {
                least: Rate { work, over: wall },
                most: work,
                answers: 1,
            }),
        };
        self.fold(answer, u64::saturating_add);
    }

    /// An answer came `wall` after its wait began, its work not read through its end: it
    /// teaches the slowest alone ([`Rated`]).
    pub fn teach_unrated(&self, wall: Duration) {
        if wall.is_zero() {
            return;
        }
        let answer = Learned {
            slowest: wall,
            rated: None,
        };
        self.fold(answer, u64::saturating_add);
    }

    /// What another record saw, taught in: each of W*, R* and C* where it goes past this
    /// one's, as a record kept by another process comes back whole, and the more of the two
    /// counts of the answers that taught them ([`Rated::answers`]).
    pub fn merge(&self, other: Seen) {
        let Some(other) = other.learned() else {
            return;
        };
        self.fold(other, u64::max);
    }

    /// `other` taught in: each of W*, R* and C* where it goes past this record's, and the
    /// answers that taught R* and C* counted by `count`, an answer's added, a record's merged.
    fn fold(&self, other: Learned, count: fn(u64, u64) -> u64) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let was = seen.learned();
        let rated = match (was.and_then(|was| was.rated), other.rated) {
            (Some(was), Some(other)) => Some(Rated {
                least: if other.least.below(&was.least) {
                    other.least
                } else {
                    was.least
                },
                most: was.most.max(other.most),
                answers: count(was.answers, other.answers),
            }),
            (was, other) => was.or(other),
        };
        *seen = Seen::Learned {
            slowest: was.map_or(other.slowest, |was| was.slowest.max(other.slowest)),
            rated,
        };
    }
}

impl Record {
    /// Whether a wait of this kind whose party has made no progress since `since` is stalled
    /// by `now`, judged from outside the wait, as one that cannot look itself is (a writer
    /// blocked in its write): none while the kind is cold, or less than W* has passed; else
    /// the stall, `now - since` both how far into its stillness and the time judged over.
    pub fn stalled(&self, since: Instant, now: Instant) -> Option<Stall> {
        let seen = self.seen().learned()?;
        let over = now.saturating_duration_since(since);
        (over >= seen.slowest).then(|| Stall::new(over, over, seen))
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
///
/// `own`: of the time this look ran late (from [`Judge::late_from`] to its beginning), how
/// long its caller spent on its own work, not looking, as a test's waiter polls a state
/// with commands of its own: lateness its caller made, no hold of its host's, so counted in
/// its party's window (D209 Part 8: a poll loop whose every poll outlasted W* had nearly
/// all its time taken for the judge's hold, and was refused only once its looks' own
/// durations added up to a W*).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub began: Instant,
    pub ended: Instant,
    pub resolution: Duration,
    pub own: Duration,
    /// Its party's work ([`Work`]) since the wait began, where its caller saw it run unheld
    /// over every look since the look before (none held from running, stopped, starved of a
    /// CPU, nor ending, and its progress its own processes'): the window the judge's rate is
    /// had over. None where it did not, or cannot say: that window is no part of the rate's
    /// wall.
    pub work: Option<Work>,
    /// The time since the look before that its party's host held it, as its own subject
    /// measured it, exactly: its declared durable writes (D286 amended, the slot file's
    /// `spent`), its threads' wait for a CPU (Linux `run_delay`). Out of its stillness, as the
    /// judge's own lateness is, and out of its rate's wall: its host's work for it, or its
    /// host's keeping it from a CPU, is no rest of its own nor its own slowness (#36, ROOT's
    /// ruling of 2026-11-18). Zero where its caller measured none.
    pub held: Duration,
}

impl Span {
    /// A look made at `at` that took no time, of counters that show every change.
    pub fn at(at: Instant) -> Span {
        Span {
            began: at,
            ended: at,
            resolution: Duration::ZERO,
            own: Duration::ZERO,
            work: None,
            held: Duration::ZERO,
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
    /// The time its party's host held it since a look last found it progressed, held or based,
    /// as its subject measured it ([`Span::held`]): out of its stillness, as `held` is; and
    /// what the looks since its due look before measured, not yet judged by a due look.
    hosted: Duration,
    pending: Duration,
    /// The unheld time it has been seen to run over, its rate's wall: each window between
    /// due looks whose looks all saw its party run unheld ([`Span::work`]), less the time the
    /// judge's looks ran late in it; and whether a look since the due one before could not
    /// say it ran unheld, which keeps the window out.
    unheld: Duration,
    unsure: bool,
    /// The least window its looks can show a stillness over: the coarsest resolution of the
    /// counters they read, and the longest a look of this wait took, whichever is longer.
    /// Its looks come and its refusals stand on W* or this, whichever is longer: a W* below
    /// it (an answer quicker than one look, or than its counters' least step) is looked at
    /// no more often, and refuses over no shorter a window, than its looks can resolve.
    floor: Duration,
    /// Whether its party is refused for its work alone ([`by_work`](Self::by_work)): past
    /// C*, whatever its rate.
    by_work: bool,
    /// Its due looks that found its party past C* and waited on
    /// ([`waited_past_most`](Self::waited_past_most)).
    waited_past_most: u32,
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
            hosted: Duration::ZERO,
            pending: Duration::ZERO,
            unheld: Duration::ZERO,
            unsure: false,
            floor: Duration::ZERO,
            by_work: false,
            waited_past_most: 0,
        }
    }

    /// It, refusing a party that progressed for its work alone: seen to run unheld a whole
    /// window, its work since its start more than any answer of its kind took (C*), once
    /// [`SPREAD`] answers have taught it, whatever its rate. For an answer of bounded work
    /// that no time spent working brings nearer, a VM's READY (D215 amended 2026-11-18, CI
    /// 44245219858: nine VMs ran at their host's fair share for 348 s, never ready, their rate
    /// above R*, so held for ever, and with them the runs and the stop that waited on them).
    #[must_use]
    pub fn by_work(mut self) -> Judge<'a> {
        self.by_work = true;
        self
    }

    /// What its judging stands on now, for its caller's account: the time its party has been
    /// still since a look last found it progressed, held or based, less the time its looks
    /// ran late (its host's hold of the judge); that time's own lateness; and the window a
    /// stillness is refused over (W*, or what its looks resolve where longer). None while it
    /// is not judged.
    pub fn stillness(&self) -> Option<(Duration, Duration, Duration)> {
        let slowest = self.record.seen().learned().filter(|_| self.judged)?.slowest;
        let free = self
            .last
            .saturating_duration_since(self.since)
            .saturating_sub(self.held)
            .saturating_sub(self.hosted);
        Some((free, self.held, self.window(slowest)))
    }

    /// The time its party's host held it, as its subject measured it, since a look last found
    /// it progressed, held or based ([`Span::held`]): out of the stillness its judging stands
    /// on ([`stillness`](Self::stillness)), for its caller's account.
    pub fn hosted(&self) -> Duration {
        self.hosted
    }

    /// From when a look made now runs late: its due, or the look before's end where that is
    /// later; none while it is not judged. Its caller's own work since then is a [`Span`]'s
    /// `own`.
    pub fn late_from(&self) -> Option<Instant> {
        Some(self.due()?.max(self.last))
    }

    /// The least time its looks resolve, its window's least: its counters' coarsest
    /// resolution, or the longest one of its looks took, whichever is longer.
    pub fn resolved(&self) -> Duration {
        self.floor
    }

    /// When its next look is due, its window ([`window`](Self::window)) after the look before
    /// was meant: none while it is not judged.
    pub fn due(&self) -> Option<Instant> {
        match self.record.seen() {
            Seen::Learned { slowest, .. } if self.judged => self.meant.checked_add(self.window(slowest)),
            _ => None,
        }
    }

    /// The window its looks come each of and a stillness is refused over: W* (`slowest`), or
    /// the least its looks can resolve ([`Judge::floor`]) where that is longer.
    fn window(&self, slowest: Duration) -> Duration {
        slowest.max(self.floor)
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
    /// where that is a whole window, W* or what its looks can resolve where that is longer:
    /// its counters' resolution, or the time one of its looks took (#77: a W* below one
    /// look's span refused waits whose first look had nothing before it; b26: a W* of 9.6 µs,
    /// a send's that found room at once, refused a live VM process 16.6 µs past its first
    /// look, its macOS reading moving in steps of a clock tick). A based look
    /// ([`Found::Based`]) starts its evidence anew whether it is due or not.
    pub fn found_over(&mut self, found: Found, span: Span) -> Result<(), Stall> {
        self.floor = self
            .floor
            .max(span.resolution)
            .max(span.ended.saturating_duration_since(span.began));
        // Its subject's measure of its host's hold, gathered over every look until a due one
        // judges it; a based look's evidence begins anew at its end, without it.
        self.pending = self.pending.saturating_add(span.held);
        if found == Found::Based {
            (self.since, self.held, self.last) = (span.ended, Duration::ZERO, span.ended);
            (self.hosted, self.pending) = (Duration::ZERO, Duration::ZERO);
        }
        // A look that ran nothing (`Still`) vouches for its window: what it saw is the party's
        // own. One held, gone or based cannot.
        self.unsure |= span.work.is_none() || matches!(found, Found::Held | Found::Gone | Found::Based);
        let Some(due) = self.due() else {
            return Ok(());
        };
        let now = span.began;
        if now < due {
            return Ok(());
        }
        let Some(seen) = self.record.seen().learned() else {
            return Ok(());
        };
        let slowest = seen.slowest;
        // Held from when it was meant, or from the look before's end where that ended later,
        // but while its caller worked on its own.
        let late = now
            .saturating_duration_since(due.max(self.last))
            .saturating_sub(span.own);
        self.held = self.held.saturating_add(late);
        // Its host's hold its subject measured since the look before: within that window, past
        // what the judge's own lateness already took out of it.
        let between = now.saturating_duration_since(self.last).saturating_sub(late);
        let hosted = std::mem::take(&mut self.pending).min(between);
        self.hosted = self.hosted.saturating_add(hosted);
        let window = self.window(slowest);
        if !std::mem::take(&mut self.unsure) {
            self.unheld = self.unheld.saturating_add(between.saturating_sub(hosted));
        }
        self.meant = Self::latest_meant(due, window, now);
        self.last = span.ended;
        match found {
            Found::Progressed => {
                (self.since, self.held, self.hosted) = (span.ended, Duration::ZERO, Duration::ZERO);
                let judged = self.slower_than_any_answer(span.work, now, window, seen);
                if judged.is_ok()
                    && let (Some(work), Some(rated)) = (span.work, seen.rated)
                    && self.past_most(work, window, rated)
                {
                    self.waited_past_most = self.waited_past_most.saturating_add(1);
                }
                judged
            }
            Found::Held | Found::Based => {
                (self.since, self.held, self.hosted) = (span.ended, Duration::ZERO, Duration::ZERO);
                Ok(())
            }
            Found::Gone => {
                self.judged = false;
                Ok(())
            }
            Found::Still => {
                let free = now
                    .saturating_duration_since(self.since)
                    .saturating_sub(self.held)
                    .saturating_sub(self.hosted);
                if free < window {
                    return Ok(());
                }
                Err(Stall::new(now.saturating_duration_since(self.began), free, seen))
            }
        }
    }

    /// A party that progressed, refused where it is too slow to be answering: seen to run
    /// unheld a whole window or more, its work since its start over that time below the
    /// least rate any answer of its kind came at (R*), and that work more than any answer
    /// took (C*), once two answers have taught them ([`SPREAD`]). Without C*, a party whose
    /// answers wait on a peer (a registry, a server),
    /// their work alike and their wall not, would be refused for its first answer slower than
    /// all before, its rate then below all before. Its work in windows its looks could not
    /// vouch for counts, their time does not: a doubt raises its rate, never lowers it. A kind
    /// no rated answer taught ([`Rated`]) has no floor. A judge [`by_work`](Self::by_work)
    /// refuses past C* whatever the rate.
    fn slower_than_any_answer(
        &self,
        work: Option<Work>,
        now: Instant,
        window: Duration,
        seen: Learned,
    ) -> Result<(), Stall> {
        let (Some(work), Some(rated)) = (work, seen.rated) else {
            return Ok(());
        };
        let rate = Rate {
            work,
            over: self.unheld,
        };
        let slow = self.by_work || rate.below(&rated.least);
        if !slow || !self.past_most(work, window, rated) {
            return Ok(());
        }
        Err(Stall::slow(now.saturating_duration_since(self.began), rate, seen))
    }

    /// Whether a party that has done `work` since its start is past its kind's C*: seen to run
    /// unheld a whole `window` or more, its work more than any answer took, once [`SPREAD`]
    /// answers have taught it.
    fn past_most(&self, work: Work, window: Duration, rated: Rated) -> bool {
        rated.answers >= SPREAD && self.unheld >= window && work > rated.most
    }

    /// How many of its due looks found its party past C* ([`past_most`](Self::past_most))
    /// and waited on: none of a judge [`by_work`](Self::by_work), which refuses at the first;
    /// a judge by rate may wait on such a party, its rate above R*. What a test of a judge by
    /// work tells its red from a host that held its party by (D215, CI 44268931283: a test
    /// that counted its own looks past C* beside the judge's ended a wait its judge rightly
    /// held, its host holding the party, as the red's).
    pub fn waited_past_most(&self) -> u32 {
        self.waited_past_most
    }

    /// [`found`](Self::found), the look made at `now`: the look clock, which a test sets.
    #[cfg(test)]
    fn found_at(&mut self, found: Found, now: Instant) -> Result<(), Stall> {
        self.found_over(found, Span::at(now))
    }

    /// The last look's time meant at or before `now`, of those each `window` from `due`: a
    /// judge its host held past several looks makes one, not one at once for each it missed.
    /// `window` is never zero (an answer no time long teaches nothing); past u64's
    /// nanoseconds, or past what an `Instant` holds, it is `now`.
    fn latest_meant(due: Instant, window: Duration, now: Instant) -> Instant {
        let behind = now.saturating_duration_since(due).as_nanos();
        let missed = behind.checked_div(window.as_nanos()).unwrap_or(0);
        let skip = missed.saturating_mul(window.as_nanos());
        u64::try_from(skip)
            .ok()
            .and_then(|ns| due.checked_add(Duration::from_nanos(ns)))
            .unwrap_or(now)
    }

    /// Its party answered, having done `work` since the wait began: its kind learns from
    /// the answer. Only the party's own answers teach: never an end its waiter made.
    pub fn answered(&self, work: Work) {
        self.record.teach(self.began.elapsed(), work);
    }

    /// Its party answered, its work not read through its answer: its kind learns its slowest
    /// alone ([`Record::teach_unrated`]).
    pub fn answered_unrated(&self) {
        self.record.teach_unrated(self.began.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cpu` as a count of work: the rule weighs counts alone, whatever their unit ([`Work`]).
    fn w(cpu: Duration) -> Work {
        Work::of(ns(cpu))
    }

    /// A stall says its party's threads as its caller read them as it refused it, and none
    /// where its caller read none: the account rides with the stall, apart from its times.
    #[test]
    fn a_stall_says_its_partys_threads_as_they_were_read() {
        let record = Record::new();
        record.teach(Duration::from_millis(1), Work::ZERO);
        let now = Instant::now();
        let stall = record
            .stalled(now.checked_sub(Duration::from_millis(5)).unwrap(), now)
            .unwrap();
        assert_eq!(stall.threads(), None, "unread until its caller says them");
        let said = stall
            .clone()
            .said(Some("7 waiting (WrUserRequest), 9 ready".into()));
        assert_eq!(said.threads(), Some("7 waiting (WrUserRequest), 9 ready"));
        assert_eq!((said.waited(), said.over()), (stall.waited(), stall.over()));
        assert_eq!(said.said(None).threads(), None);
    }

    /// A rate is less than another's as cross-multiplication says, with no float: equal
    /// rates are neither below the other, none at all is below any, and a duration past
    /// u64's nanoseconds is its most rather than an overflow (D215).
    #[test]
    fn a_rate_is_compared_by_cross_multiplication() {
        let rate = |cpu: u64, over: u64| Rate {
            work: Work::of(cpu),
            over: Duration::from_nanos(over),
        };
        assert!(rate(1, 2).below(&rate(1, 1)));
        assert!(!rate(1, 1).below(&rate(1, 2)));
        assert!(!rate(2, 4).below(&rate(1, 2)));
        assert!(!rate(1, 2).below(&rate(2, 4)));
        assert!(rate(0, 1).below(&rate(1, u64::MAX)));
        let most = Rate {
            work: Work::of(u64::MAX),
            over: Duration::MAX,
        };
        assert!(!most.below(&rate(u64::MAX, u64::MAX)));
        assert!(rate(1, 2).below(&most));
    }

    /// A record learns its slowest answer, its least rate and its most work from its first
    /// answer on, and counts the answers that taught them: its slowest only rises, its least
    /// rate only falls, its most work only rises, each answer counts one, and a wait no time
    /// long teaches nothing (D215).
    #[test]
    fn a_record_learns_its_slowest_answer_its_least_rate_and_its_most_work() {
        let ms = Duration::from_millis;
        let record = Record::new();
        assert_eq!(record.seen(), Seen::Cold);
        let learned = |slowest, cpu, over, most, answers| Seen::Learned {
            slowest,
            rated: Some(Rated {
                least: Rate { work: w(cpu), over },
                most: w(most),
                answers,
            }),
        };
        record.teach(ms(2), w(ms(1)));
        assert_eq!(record.seen(), learned(ms(2), ms(1), ms(2), ms(1), 1));
        record.teach(ms(1), w(ms(1)));
        assert_eq!(record.seen(), learned(ms(2), ms(1), ms(2), ms(1), 2));
        record.teach(ms(4), w(ms(1)));
        assert_eq!(record.seen(), learned(ms(4), ms(1), ms(4), ms(1), 3));
        record.teach(ms(1), w(ms(3)));
        assert_eq!(record.seen(), learned(ms(4), ms(1), ms(4), ms(3), 4));
        record.teach(Duration::ZERO, Work::ZERO);
        assert_eq!(record.seen(), learned(ms(4), ms(1), ms(4), ms(3), 4));
    }

    /// A kept record merged in raises W* and C* and lowers R* where it goes past, and leaves
    /// each that it does not; its count of answers is the more of the two, never their sum, as
    /// the same kept record merged in again adds none (D215).
    #[test]
    fn a_record_merged_in_keeps_each_bound_past_the_others() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.merge(Seen::Cold);
        assert_eq!(record.seen(), Seen::Cold);
        let learned = |slowest, cpu, over, most, answers| Seen::Learned {
            slowest,
            rated: Some(Rated {
                least: Rate { work: w(cpu), over },
                most: w(most),
                answers,
            }),
        };
        record.merge(learned(ms(10), ms(2), ms(4), ms(5), 1));
        assert_eq!(record.seen(), learned(ms(10), ms(2), ms(4), ms(5), 1));
        record.merge(learned(ms(8), ms(1), ms(4), ms(6), 3));
        assert_eq!(record.seen(), learned(ms(10), ms(1), ms(4), ms(6), 3));
        record.merge(learned(ms(12), ms(3), ms(4), ms(1), 2));
        assert_eq!(record.seen(), learned(ms(12), ms(1), ms(4), ms(6), 3));
        record.merge(learned(ms(12), ms(3), ms(4), ms(1), 2));
        assert_eq!(
            record.seen(),
            learned(ms(12), ms(1), ms(4), ms(6), 3),
            "merged again"
        );
    }

    /// An answer whose work was not read through its end teaches W* alone: a kind no rated
    /// answer taught has no rates, and its first rated answer gives them whole, an unrated
    /// one after it leaving them as they are (D215: kept records of R* 0 ns, from answers
    /// reaped before a look read their CPU, held no floor).
    #[test]
    fn an_unrated_answer_teaches_its_slowest_alone() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach_unrated(ms(5));
        assert_eq!(
            record.seen(),
            Seen::Learned {
                slowest: ms(5),
                rated: None
            }
        );
        record.teach(ms(3), w(ms(2)));
        let rated = Some(Rated {
            least: Rate {
                work: w(ms(2)),
                over: ms(3),
            },
            most: w(ms(2)),
            answers: 1,
        });
        assert_eq!(
            record.seen(),
            Seen::Learned {
                slowest: ms(5),
                rated
            }
        );
        record.teach_unrated(ms(9));
        assert_eq!(
            record.seen(),
            Seen::Learned {
                slowest: ms(9),
                rated
            }
        );
    }

    /// An answer whose work read none teaches its slowest alone, as an unrated one does: R*
    /// and C* stay as answers that worked taught them, never R* 0 ns, a floor that refuses
    /// nothing (CI 44076016255, arm64 Windows: a 50 ms stand-in read as none in Windows'
    /// ticks). Red with such an answer rated, its R* 0 ns over its wall.
    #[test]
    fn an_answer_of_no_work_teaches_its_slowest_alone() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach(ms(70), Work::ZERO);
        assert_eq!(
            record.seen(),
            Seen::Learned {
                slowest: ms(70),
                rated: None
            }
        );
        record.teach(ms(3), w(ms(2)));
        record.teach(ms(80), Work::ZERO);
        assert_eq!(
            record.seen(),
            Seen::Learned {
                slowest: ms(80),
                rated: Some(Rated {
                    least: Rate {
                        work: w(ms(2)),
                        over: ms(3),
                    },
                    most: w(ms(2)),
                    answers: 1,
                })
            }
        );
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
        record.teach(Duration::from_nanos(1), Work::ZERO);
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
        record.teach(slowest, w(Duration::from_millis(3)));
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
            Some(Rate {
                work: w(Duration::from_millis(3)),
                over: slowest
            })
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
        record.teach(slowest, w(Duration::from_millis(1)));
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
        record.teach(slowest, w(ms(1)));
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
        record.teach(slowest, w(Duration::from_millis(3)));
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

    /// A look its caller made late by its own work is not its host's hold (D209 Part 8):
    /// harness.rs' `a_wait_is_refused_though_its_own_polls_keep_its_daemon_busy` on
    /// adas-mac-studio-3.local, busy, "not refused 177-197 ms in (W* 59.554375ms)", its
    /// waiter polling with a command of its own between looks, each poll and look slower than
    /// when W* was taught. Replayed: a base look 0-33 ms in; a poll 33-93 ms, past the first
    /// due; a look 93-123 ms that finds its party still. Its lateness its host's hold, as
    /// a judge whose host held it counts it, the look sees 26 ms of a W* and goes on; its
    /// lateness its caller's own poll, the look has seen 60 ms, a whole W*, and refuses it.
    #[test]
    fn a_look_its_caller_made_late_by_its_own_work_is_not_held() {
        let ms = Duration::from_millis;
        let slowest = Duration::from_nanos(59_554_375);
        let record = Record::new();
        record.teach(slowest, w(ms(3)));
        let judged = |own: bool| {
            let mut judge = Judge::begin(&record);
            let base = span(&judge, Duration::ZERO, ms(33), Duration::ZERO);
            assert_eq!(judge.found_over(Found::Based, base), Ok(()));
            let from = judge.late_from().unwrap();
            let mut look = span(&judge, ms(93), ms(123), Duration::ZERO);
            if own {
                look.own = look.began - from;
            }
            judge.found_over(Found::Still, look)
        };
        assert_eq!(judged(false), Ok(()), "a host's hold of its judge refused");
        let stall = judged(true).unwrap_err();
        assert_eq!(stall.over(), ms(60));
        assert_eq!(stall.waited(), ms(93));
    }

    /// A judge that looks on time at each due, its party stopped from the start, refuses it
    /// at its first look, over a whole W* (ROOT's ruling, 2026-11-13: a host's hold of the
    /// judge is never one it did not show).
    #[test]
    fn a_judge_on_time_refuses_a_stopped_party_at_its_first_look() {
        let slowest = Duration::from_millis(30);
        let record = Record::new();
        record.teach(slowest, w(Duration::from_millis(3)));
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
        record.teach(ms(30), w(ms(3)));
        assert_eq!(record.stalled(since, since + ms(29)), None);
        let stall = record.stalled(since, since + ms(31)).unwrap();
        assert_eq!(
            (stall.waited(), stall.over(), stall.slowest()),
            (ms(31), ms(31), ms(30))
        );
        assert_eq!(
            stall.least(),
            Some(Rate {
                work: w(ms(3)),
                over: ms(30)
            })
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
            own: Duration::ZERO,
            work: None,
            held: Duration::ZERO,
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
                own: Duration::ZERO,
                work: None,
                held: Duration::ZERO,
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
        record.teach(slowest, Work::ZERO);
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
    /// own lateness, not a W*: it goes on. Its first look took 29 ms, longer than W*, so its
    /// looks come and refuse on that (b26: no window shorter than its looks resolve).
    /// Looked at on time from then, it is refused at the first look that has seen a whole
    /// such window past the first look's end, the lateness not counted: its third due.
    #[test]
    fn a_window_is_judged_from_its_baselines_end() {
        let us = Duration::from_micros;
        let slowest = Duration::from_nanos(13_996_303);
        let record = Record::new();
        record.teach(slowest, w(Duration::from_millis(1)));
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
        let window = us(29_000);
        assert_eq!(at, window * 3);
        let late = us(31_066) - window;
        assert_eq!(stall.over(), window * 3 - window - late);
        assert_eq!((stall.waited(), stall.slowest()), (window * 3, slowest));
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
        record.teach(slowest, Work::ZERO);
        let mut judge = Judge::begin(&record);
        let first = span(&judge, Duration::ZERO, Duration::ZERO, tick);
        assert_eq!(judge.found_over(Found::Based, first), Ok(()));
        let (at, stall) = still_until_refused(&mut judge, tick, 40).unwrap();
        assert_eq!(at, tick);
        assert_eq!((stall.over(), stall.slowest()), (tick, slowest));
    }

    /// b26's run-socket send (ROOT's HVF batch at 1f71dea19, moved.rs:645: "its process ran no
    /// CPU in 16.584µs past the slowest answer seen (9.583µs), 29.958µs into the wait"),
    /// replayed: a W* below what its looks resolve, its first look 13.374 µs long. Of
    /// counters that show every change, its looks come no more often than one takes; of a
    /// reading in clock ticks (a macOS process's times, 10 ms), a tick apart, and it is
    /// refused only once they have seen a whole tick still, never 16.584 µs in.
    #[test]
    fn a_w_star_below_what_its_looks_resolve_is_judged_over_what_they_do() {
        let ns = Duration::from_nanos;
        let slowest = ns(9_583);
        let first_look = ns(13_374);
        let tick = Duration::from_millis(10);
        let record = Record::new();
        record.teach(slowest, Work::ZERO);
        let mut exact = Judge::begin(&record);
        let first = span(&exact, Duration::ZERO, first_look, Duration::ZERO);
        assert_eq!(exact.found_over(Found::Based, first), Ok(()));
        assert_eq!(
            exact.due(),
            Some(exact.began + first_look),
            "looked at no more often than a look takes"
        );
        let mut judge = Judge::begin(&record);
        let first = span(&judge, Duration::ZERO, first_look, tick);
        assert_eq!(judge.found_over(Found::Based, first), Ok(()));
        assert_eq!(judge.due(), Some(judge.began + tick), "looked at a tick apart");
        let early = span(&judge, ns(29_958), ns(30_000), tick);
        assert_eq!(judge.found_over(Found::Still, early), Ok(()), "16.584 µs seen");
        let (at, stall) = still_until_refused(&mut judge, tick, 8).unwrap();
        assert!(stall.over() >= tick, "{stall:?}");
        assert!(at >= first_look + tick, "{at:?}");
        assert_eq!(stall.slowest(), slowest);
    }

    /// A based look starts the evidence anew whether or not it is due: one that found a
    /// process of its party first, late in the wait, leaves the time before it unjudged.
    #[test]
    fn a_based_look_starts_the_evidence_anew_before_its_due() {
        let ms = Duration::from_millis;
        let slowest = ms(10);
        let record = Record::new();
        record.teach(slowest, Work::ZERO);
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

    /// A look at `at` past the judge's start that found `found`, its party having done `work`
    /// since the wait began where its caller saw it run unheld.
    fn look_with(
        judge: &mut Judge<'_>,
        at: Duration,
        found: Found,
        work: Option<Duration>,
    ) -> Result<(), Stall> {
        let at = judge.began + at;
        judge.found_over(
            found,
            Span {
                work: work.map(w),
                ..Span::at(at)
            },
        )
    }

    /// Due looks on time from the first, each finding its party progressed with `work(n)` at
    /// the `n`th (from 1), until one refuses it: that look's number and the stall. None in 40.
    fn ticking_until_refused(
        judge: &mut Judge<'_>,
        window: Duration,
        work: impl Fn(u32) -> Option<Duration>,
    ) -> Option<(u32, Stall)> {
        (1..=40u32).find_map(|n| {
            look_with(judge, window * n, Found::Progressed, work(n))
                .err()
                .map(|stall| (n, stall))
        })
    }

    /// b24's shape (D215): a party that runs a little in every window, as a sleeping guest's
    /// VM process ticks, its rate far below R*, is refused at the first due look at which its
    /// work since its start passes the most any answer took (C*), and not at the look before;
    /// a look between that ran nothing vouches for its window; the refusal says its rate. Its
    /// kind taught twice, as R* and C* refuse once two answers taught them ([`SPREAD`]).
    #[test]
    fn a_wait_slower_than_any_answer_is_refused_though_it_progresses() {
        let (ms, us) = (Duration::from_millis, Duration::from_micros);
        let record = Record::new();
        record.teach(ms(30), w(ms(3)));
        record.teach(ms(30), w(ms(3)));
        let mut judge = Judge::begin(&record);
        assert_eq!(
            look_with(&mut judge, ms(10), Found::Still, Some(Duration::ZERO)),
            Ok(())
        );
        let (n, stall) = ticking_until_refused(&mut judge, ms(30), |n| Some(us(200) * n)).unwrap();
        assert_eq!(
            n,
            16,
            "refused at look {n}, its work {:?} past C* 3ms at look 16",
            us(200) * n
        );
        assert_eq!(stall.waited(), ms(480));
        assert_eq!(
            stall.rate(),
            Some(Rate {
                work: w(us(3200)),
                over: ms(480)
            })
        );
        assert_eq!(stall.most(), Some(w(ms(3))));
        assert_eq!(
            stall.least(),
            Some(Rate {
                work: w(ms(3)),
                over: ms(30)
            })
        );
    }

    /// A party past W* at R* or above goes on look after look, its work past C* or not:
    /// busy, not asleep (D215). Its kind taught twice, so that its rates judge it.
    #[test]
    fn a_busy_wait_past_w_star_goes_on() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach(ms(30), w(ms(36)));
        record.teach(ms(30), w(ms(36)));
        let mut judge = Judge::begin(&record);
        assert!(ticking_until_refused(&mut judge, ms(30), |n| Some(ms(36) * n)).is_none());
    }

    /// A judge by its party's work alone ([`Judge::by_work`]) refuses the party
    /// `a_busy_wait_past_w_star_goes_on` keeps, at R* throughout, at the first due look at
    /// which its work passes C*, and not at the look before: an answer of bounded work that
    /// no time spent working brings nearer, a VM's READY (D215 amended 2026-11-18; CI
    /// 44245219858, nine VMs at their host's fair share for 348 s, never ready). Its kind
    /// taught twice ([`SPREAD`]).
    #[test]
    fn a_busy_wait_judged_by_its_work_is_refused_past_c_star() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach(ms(30), w(ms(36)));
        record.teach(ms(30), w(ms(36)));
        let mut judge = Judge::begin(&record).by_work();
        let (n, stall) = ticking_until_refused(&mut judge, ms(30), |n| Some(ms(36) * n)).unwrap();
        assert_eq!(n, 2, "{stall:?}");
        assert_eq!(stall.rate().map(|rate| rate.work), Some(w(ms(72))));
        assert_eq!(stall.most(), Some(w(ms(36))));
    }

    /// A party past W* below R* that has not done the work an answer took (C*) goes on: its
    /// answers wait on a peer, and this one waits longer than any before (D215). Its kind
    /// taught twice, so that its rates judge it.
    #[test]
    fn a_wait_on_a_peer_past_w_star_goes_on_within_its_kinds_work() {
        let (ms, us) = (Duration::from_millis, Duration::from_micros);
        let record = Record::new();
        record.teach(ms(30), w(ms(3)));
        record.teach(ms(30), w(ms(3)));
        let mut judge = Judge::begin(&record);
        assert!(ticking_until_refused(&mut judge, ms(30), |n| Some(us(75) * n)).is_none());
    }

    /// A window whose looks could not vouch that its party ran unheld (one held, or whose
    /// caller could not say) is no part of the rate's wall, while its work still counts (D215).
    /// Its kind taught twice, so that its rates judge it.
    #[test]
    fn a_window_its_caller_cannot_vouch_for_is_out_of_the_rate() {
        let (ms, us) = (Duration::from_millis, Duration::from_micros);
        let record = Record::new();
        record.teach(ms(30), w(ms(3)));
        record.teach(ms(30), w(ms(3)));
        let mut judge = Judge::begin(&record);
        assert_eq!(
            look_with(&mut judge, ms(10), Found::Held, Some(Duration::ZERO)),
            Ok(())
        );
        let (n, stall) =
            ticking_until_refused(&mut judge, ms(30), |n| (n != 2).then_some(us(200) * n)).unwrap();
        assert_eq!(n, 16);
        assert_eq!(
            stall.rate(),
            Some(Rate {
                work: w(us(3200)),
                over: ms(420)
            }),
            "its held and unsaid windows out"
        );
    }

    /// An answer teaches its kind the wall time since its wait began and the work done
    /// meanwhile (D215).
    #[test]
    fn an_answer_teaches_its_kind() {
        let record = Record::new();
        let judge = Judge::begin(&record);
        std::thread::sleep(Duration::from_millis(2));
        judge.answered(w(Duration::from_millis(1)));
        let Seen::Learned {
            slowest,
            rated: Some(Rated { least, most, answers }),
        } = record.seen()
        else {
            panic!("an answer taught nothing");
        };
        assert!(slowest >= Duration::from_millis(2), "{slowest:?}");
        assert_eq!(least.work, w(Duration::from_millis(1)));
        assert_eq!(least.over, slowest);
        assert_eq!(most, w(Duration::from_millis(1)));
        assert_eq!(answers, 1);
    }

    /// One answer's least rate and most work are that one answer: they refuse no party by its
    /// rate, and two answers' do (D215, ROOT's ruling of 2026-11-17). CI 44076016255's x86_64
    /// musl job, engine.rs' `shards exec prov /bin/testguest api POST /containers/web2/wait`:
    /// its kind, one answer's (W* 13.66951 ms, R* 12.055769 ms of CPU in 13.66951 ms, C*
    /// 12.055769 ms), refused a live party, its answer already printed, at 12.888561 ms of CPU
    /// over 26.602167 ms seen unheld: 1.07× C* at 0.55× R*. The same looks here, its first
    /// due one 736.853 µs late as CI's was. Red without the count: refused at the second look.
    #[test]
    fn one_answers_rates_refuse_nothing() {
        let ns = Duration::from_nanos;
        let (slowest, most) = (ns(13_669_510), ns(12_055_769));
        let looks = |record: &Record| {
            let mut judge = Judge::begin(record);
            look_with(
                &mut judge,
                slowest + ns(736_853),
                Found::Progressed,
                Some(most / 2),
            )?;
            look_with(&mut judge, slowest * 2, Found::Progressed, Some(ns(12_888_561)))
        };
        let record = Record::new();
        record.teach(slowest, w(most));
        assert_eq!(looks(&record), Ok(()), "refused by one answer's rates");
        record.teach(slowest, w(most));
        let stall = looks(&record).unwrap_err();
        assert_eq!(
            stall.rate(),
            Some(Rate {
                work: w(ns(12_888_561)),
                over: ns(26_602_167)
            })
        );
        assert_eq!(stall.most(), Some(w(most)));
    }

    /// b24's sleeper is still refused (D215, ROOT's ruling of 2026-11-17): a party that runs a
    /// little in every window, as a sleeping guest's VM process ticks, goes on through 40 looks
    /// while one answer alone has taught its kind, and once a second has, R* and C* each from
    /// its own answer, is refused at the first due look at which its work passes C*.
    #[test]
    fn a_sleeper_is_refused_once_two_answers_taught_its_kind() {
        let (ms, us) = (Duration::from_millis, Duration::from_micros);
        let record = Record::new();
        record.teach(ms(30), w(ms(3)));
        let mut judge = Judge::begin(&record);
        assert!(
            ticking_until_refused(&mut judge, ms(30), |n| Some(us(200) * n)).is_none(),
            "refused by one answer's rates"
        );
        record.teach(ms(25), w(ms(2)));
        let mut judge = Judge::begin(&record);
        let (n, stall) = ticking_until_refused(&mut judge, ms(30), |n| Some(us(200) * n)).unwrap();
        assert_eq!(n, 16, "refused at look {n}");
        assert_eq!(
            stall.least(),
            Some(Rate {
                work: w(ms(2)),
                over: ms(25)
            })
        );
        assert_eq!(stall.most(), Some(w(ms(3))));
    }

    /// What its subject measured of its host's hold since the look before ([`Span::held`]) is
    /// out of its stillness, as its judge's lateness is, and out of its rate's wall (#36,
    /// ROOT's ruling of 2026-11-18; D286 amended). Do: a still party whose host held it half of
    /// each window; and a party progressing at 30% of its wall, below R* (50%) past C* (50 ms),
    /// held 60 ms of each 100 or none. Expect: the still one refused at its second due look,
    /// its stillness net of the hold a whole window then, where one held none is refused at
    /// its first; the progressing one held none refused at its second due look, and the held
    /// one, its rate over its unheld wall 75%, going on.
    #[test]
    fn its_hosts_hold_its_subject_measured_is_out_of_its_stillness_and_its_rate() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach(ms(100), w(ms(50)));
        record.teach(ms(100), w(ms(50)));
        let look = |judge: &Judge<'_>, at: u64, held: u64| Span {
            held: ms(held),
            ..Span::at(judge.began + ms(at))
        };
        let mut judge = Judge::begin(&record);
        assert_eq!(
            judge.found_over(Found::Still, look(&judge, 100, 50)),
            Ok(()),
            "still half its window"
        );
        let stall = judge
            .found_over(Found::Still, look(&judge, 200, 50))
            .expect_err("still a whole window");
        assert_eq!(stall.over(), ms(100));
        assert_eq!(judge.hosted(), ms(100));
        let mut bare = Judge::begin(&record);
        assert!(bare.found_over(Found::Still, look(&bare, 100, 0)).is_err());
        let progress = |judge: &mut Judge<'_>, held: u64| {
            [30u64, 60, 90].into_iter().zip(1u64..).try_for_each(|(cpu, n)| {
                let span = Span {
                    work: Some(w(ms(cpu))),
                    held: ms(held),
                    ..Span::at(judge.began + ms(100 * n))
                };
                judge.found_over(Found::Progressed, span)
            })
        };
        assert!(progress(&mut Judge::begin(&record), 0).is_err(), "unheld, slow");
        assert_eq!(progress(&mut Judge::begin(&record), 60), Ok(()), "held, not slow");
    }
}
