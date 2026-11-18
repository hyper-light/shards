//! A test's waits, judged as D215 judges a wait on a VM process (#77): on its party, its own
//! test's threads ([`party`]), by the slowest of its tests' waits seen (W*), with
//! shards-judge's rule. What a test waits for is waited for as long as its party runs, however
//! slow its host, and a wait on a party that stopped is given up on a whole W* after, its
//! account said: no test picks a patience of its own.
//!
//! Shards' tests take it as a dev-dependency, the daemon's and shards-remote's; their code
//! under test names it only under `cfg(test)`, where the threads it makes join their maker's
//! party ([`party::carried`]) and its timed waits say their deadlines ([`Waiting`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shards_judge::{Found, Judge, Seen};
/// What its waits take and say: the record of a kind of answer a wait is judged by, and the
/// stall it was given up on at (shards-judge's).
pub use shards_judge::{Record, Stall};

pub mod party;

/// The deadlines threads of this test process wait until ([`Waiting`]), each under its key,
/// with the party of the thread that waits ([`party`]; 0: none).
static DEADLINES: Mutex<Vec<(u64, Instant, u64)>> = Mutex::new(Vec::new());
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(0);

fn deadlines() -> MutexGuard<'static, Vec<(u64, Instant, u64)>> {
    DEADLINES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A thread's timed wait, held for its length (#77): its deadline registered under the party
/// of the thread that waits ([`party`]), by which a test's judged wait knows that a thread of
/// its own asleep until then has not stalled, its next progress scheduled. None, a wait with
/// no deadline, registers none.
#[must_use]
#[derive(Debug)]
pub struct Waiting(Option<u64>);

impl Waiting {
    /// A wait of at most `timeout`, from now.
    pub fn of(timeout: Option<Duration>) -> Waiting {
        Waiting::until(timeout.and_then(|t| Instant::now().checked_add(t)))
    }

    /// A wait until `until`.
    pub fn until(until: Option<Instant>) -> Waiting {
        Waiting(until.map(|until| {
            let key = NEXT_DEADLINE.fetch_add(1, Ordering::Relaxed);
            deadlines().push((key, until, party::current()));
            key
        }))
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(key) = self.0 {
            deadlines().retain(|(k, _, _)| *k != key);
        }
    }
}

/// The latest deadline a thread of this test process waits until now.
pub fn latest_deadline() -> Option<Instant> {
    deadlines().iter().map(|(_, until, _)| *until).max()
}

/// The latest deadline a thread of party `party` waits until now.
pub fn latest_deadline_of(party: u64) -> Option<Instant> {
    deadlines()
        .iter()
        .filter(|(_, _, of)| *of == party)
        .map(|(_, until, _)| *until)
        .max()
}

/// What this process has learned of its tests' waits (D215's record): one kind, as every one
/// waits on a party alike, its own test's threads ([`Counted`]).
static WAITS: Record = Record::new();

/// What a wait is judged by while its kind is cold, no answer of it come in this process: its
/// host's scheduling granularity as this process's waiters measured it, the longest a thread
/// of it asleep for the least it may ask (a nanosecond) took to run again, as D215 counts a
/// waiter's lateness its host's ([`Wait::overran`]). A wait judged by it refuses a party still
/// for a whole one of those at each of `LOOKS_TO_REFUSE` looks: no member ran, none was held,
/// and no deadline of its own was still to come. Before, a cold wait followed its process's
/// liveness alone and never gave up, so that a wait on what never came was a CI job lost at
/// its time limit (ROOT, 2026-11-17).
static COLD: Record = Record::new();

/// The record a wait of kind `record` is judged by as it begins: its kind's, once that has an
/// answer; else [`COLD`], taught the host's granularity as this wait measures it now.
fn judging(record: &Record) -> &Record {
    if matches!(record.seen(), Seen::Learned { .. }) {
        return record;
    }
    let asked = Instant::now();
    std::thread::sleep(Duration::from_nanos(1));
    COLD.teach_unrated(asked.elapsed());
    &COLD
}

/// The CPU the threads waiting in a [`Wait`] have run while they waited, in [`cpu`]'s
/// units: a waiter that polls, reads a clock or wakes runs too, and is no party to its
/// wait.
static WAITERS: AtomicU64 = AtomicU64::new(0);

/// This process's CPU (`process`), or this thread's, in one clock's units: on Unix the
/// nanoseconds of their CPU clocks (clock_gettime(2): CLOCK_PROCESS_CPUTIME_ID,
/// CLOCK_THREAD_CPUTIME_ID); on Windows the cycles their threads used
/// (QueryProcessCycleTime, QueryThreadCycleTime), as GetProcessTimes documents no
/// resolution of its times (D215).
#[cfg(unix)]
fn cpu(process: bool) -> u64 {
    let clock = if process {
        libc::CLOCK_PROCESS_CPUTIME_ID
    } else {
        libc::CLOCK_THREAD_CPUTIME_ID
    };
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) writes one timespec into a local.
    if unsafe { libc::clock_gettime(clock, &raw mut now) } != 0 {
        // POSIX's CPU-time clocks, which every target has: one unread counts nothing.
        return 0;
    }
    let secs = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[cfg(windows)]
fn cpu(process: bool) -> u64 {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
    use windows_sys::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
    let mut cycles = 0u64;
    // SAFETY: each writes one count into a local, of the pseudo-handle of this process or
    // this thread, which needs no closing.
    let read = unsafe {
        if process {
            QueryProcessCycleTime(GetCurrentProcess(), &raw mut cycles)
        } else {
            QueryThreadCycleTime(GetCurrentThread(), &raw mut cycles)
        }
    };
    // Of pseudo-handles that need no opening: a count unread counts nothing.
    if read == 0 {
        return 0;
    }
    cycles
}

/// The CPU this process's threads have run apart from its waiters' waits ([`WAITERS`]):
/// its threads under test, and its other tests'.
fn others() -> i128 {
    i128::from(cpu(true)) - i128::from(WAITERS.load(Ordering::Relaxed))
}

/// Whose run and deadlines a [`Wait`] counts as its party's (D215): its test's own
/// threads ([`party`]), where the thread that waits is of a test's party; this process's,
/// where it is of none; or a test's stand-in.
#[derive(Debug, Clone, Copy)]
pub enum Counted {
    Party(u64),
    Process,
    StandIn {
        ran: fn() -> i128,
        until: fn() -> Option<Instant>,
        /// How many of its members its host holds now.
        held: fn() -> usize,
    },
}

impl Counted {
    /// The party of the thread that asks, or this process where it is of none.
    pub fn here() -> Counted {
        match party::current() {
            0 => Counted::Process,
            id => Counted::Party(id),
        }
    }

    /// What it has run.
    fn ran(self) -> i128 {
        match self {
            Counted::Party(id) => party::ran(id),
            Counted::Process => others(),
            Counted::StandIn { ran, .. } => ran(),
        }
    }

    /// The latest deadline it waits until.
    fn until(self) -> Option<Instant> {
        match self {
            Counted::Party(id) => latest_deadline_of(id),
            Counted::Process => latest_deadline(),
            Counted::StandIn { until, .. } => until(),
        }
    }

    /// What its members are doing now ([`party::hold`]): a test's party's, whose threads
    /// it knows; a stand-in's as its test says; none read of this process's, of which
    /// every thread that has run counts already.
    fn hold(self) -> party::Hold {
        match self {
            Counted::Party(id) => party::hold(id),
            Counted::Process => party::Hold::default(),
            Counted::StandIn { held, .. } => party::Hold::standing(held()),
        }
    }

    /// What its members were doing as a wait on it was refused, for its account
    /// ([`party::account`]): a test's party's, each read; this process's, whose threads no wait
    /// reads; a stand-in's as its test says.
    fn account(self) -> String {
        match self {
            Counted::Party(id) => party::account(id),
            Counted::Process => "this process's threads, of no party, their states unread".to_string(),
            Counted::StandIn { held, .. } => format!("a stand-in's, {} of them held", held()),
        }
    }

    /// A thread waiting on it ran `cpu` more as it waited: no run of a party's.
    fn waited(self, cpu: u64) {
        WAITERS.fetch_add(cpu, Ordering::Relaxed);
        if let Counted::Party(id) = self {
            party::waited(id, cpu);
        }
    }
}

/// A test's wait on its party, whose threads run in this process: judged as D215 judges
/// a wait on a VM process (#77), its party its test's own threads ([`Counted`],
/// [`party`]). Past the slowest wait of its tests seen ([`WAITS`], W*), it looks once
/// each W*, and gives up only where no thread of its party but its waiters has run for a
/// whole W* its host did not hold it in, counted from no earlier than the latest deadline
/// a thread of its party waits until ([`Waiting`]): a party asleep on a deadline of its
/// own has not stalled before it.
/// Before its kind's first answer it is judged so by its host's scheduling granularity
/// ([`COLD`]), and by its kind's record from the first answer on.
///
/// Not shards-api's `progress::Watch`: of its own process every look finds it ran and
/// runnable, the looking thread among its threads. How long its host held it is what
/// its own sleeps and timed waits overran (`overran`): a host that holds the process
/// holds its waiter, and a stretch of it so held is no stretch its party could run in.
/// And, as D215 reads a VM process's threads, its members' own states ([`party::hold`]):
/// one runnable on no CPU, grown in its wait for one since the look before, or waiting in
/// its kernel is held by its host, which no overrun of its waiter's shows where its host
/// runs the waiter and not the member (CI 44164945050, aarch64 gnu,
/// `a_change_while_its_record_is_written_is_written_after`: refused, W* 1.11 ms, while its
/// record's writer, done with its disk, had yet to run or take the registry's lock).
struct Wait<'r> {
    record: &'r Record,
    /// What it is judged by: its kind's record, or while that is cold, [`COLD`].
    judging: &'r Record,
    /// Whose run and deadlines it counts.
    party: Counted,
    judge: Judge<'r>,
    began: Instant,
    /// What its party had run as it began (Unix: Windows keeps no rate in cycles), and at
    /// the look before.
    #[cfg(unix)]
    from: i128,
    others: i128,
    /// This thread's CPU as it last added its own to [`WAITERS`].
    mine: u64,
    /// When its party last progressed, or the wait began or was first judged; and how
    /// long its host has held it since.
    since: Instant,
    held: Duration,
    /// What its members were doing at the look before ([`party::hold`]).
    hold: party::Hold,
}

impl<'r> Wait<'r> {
    fn on(record: &'r Record, party: Counted) -> Wait<'r> {
        let judging = judging(record);
        let (began, mine, others) = (Instant::now(), cpu(false), party.ran());
        Wait {
            record,
            judging,
            party,
            judge: Judge::begin(judging),
            began,
            #[cfg(unix)]
            from: others,
            others,
            mine,
            since: began,
            held: Duration::ZERO,
            // Its members as it begins, which a look's growth in their waits for a CPU is
            // judged from (Linux); Windows' and macOS's holds are their states at a look
            // alone, and Windows lists every thread of the system to read them.
            hold: if cfg!(target_os = "linux") {
                party.hold()
            } else {
                party::Hold::default()
            },
        }
    }

    /// How long to wait before it looks: until its look is due; where it is not judged, as
    /// long as it has waited.
    fn slice(&self) -> Duration {
        self.judge.due().map_or_else(
            || self.began.elapsed(),
            |due| due.saturating_duration_since(Instant::now()),
        )
    }

    /// Its thread slept, or waited timed, `asked` and woke after `took`: its host held
    /// it for the rest.
    fn overran(&mut self, asked: Duration, took: Duration) {
        self.held = self.held.saturating_add(took.saturating_sub(asked));
    }

    /// Looks at its party where a look is due ([`Wait`]): refused where none of its
    /// threads but its waiters has run for a whole W* its host did not hold it in, past
    /// the latest deadline it waits until.
    fn look(&mut self) -> Result<(), Stall> {
        let mine = cpu(false);
        self.party.waited(mine.saturating_sub(self.mine));
        self.mine = mine;
        let now = Instant::now();
        // Begun while its kind was cold, judged by its host's granularity: from its kind's first
        // answer, judged by its kind's record, its stillness counted from here.
        if !std::ptr::eq(self.judging, self.record) && matches!(self.record.seen(), Seen::Learned { .. }) {
            self.judging = self.record;
            self.judge = Judge::begin(self.record);
            (self.others, self.since, self.held) = (self.party.ran(), now, Duration::ZERO);
            return Ok(());
        }
        let Some(due) = self.judge.due() else {
            return Ok(());
        };
        let Seen::Learned { slowest, .. } = self.judging.seen() else {
            return Ok(());
        };
        if now < due {
            return Ok(());
        }
        let others = self.party.ran();
        let ran = others > std::mem::replace(&mut self.others, others);
        // A member its host holds since the look before: its stillness counted from here,
        // as D215's judge counts it from a look that found its party held. Read only where
        // its party ran nothing, which has progressed whatever its host did.
        let held = !ran && {
            let hold = self.party.hold();
            let held = hold.since(&self.hold);
            self.hold = hold;
            held
        };
        if ran || held {
            (self.since, self.held) = (now, Duration::ZERO);
        }
        let from = self
            .party
            .until()
            .map_or(self.since, |until| until.max(self.since));
        let free = now.saturating_duration_since(from).saturating_sub(self.held);
        let judged = self.judge.found(if ran {
            Found::Progressed
        } else if held || free < slowest {
            Found::Held
        } else {
            Found::Still
        });
        #[cfg(test)]
        looked(self.party, false);
        judged.map_err(|stall| self.said(stall, now))
    }

    /// `stall`, refused at `now`, with its account ([`Stall::said`]): what its party's members
    /// were doing, whether it was judged cold, by its host's granularity, or by its kind's
    /// record, and the latest deadline its party waited until; so that a refusal says why.
    fn said(&self, stall: Stall, now: Instant) -> Stall {
        let judged = if std::ptr::eq(self.judging, &COLD) {
            "judged cold, before any answer in this process, by its host's scheduling granularity"
        } else {
            "judged by the slowest of its tests' waits seen"
        };
        let deadline = self.party.until().map_or_else(
            || "no deadline of its party's".to_string(),
            |until| {
                format!(
                    "its party's latest deadline {:?} before",
                    now.saturating_duration_since(until)
                )
            },
        );
        let account = format!(
            "{}; {judged}, {:?}; {deadline}",
            self.party.account(),
            stall.slowest()
        );
        stall.said(Some(account))
    }

    /// What it waited for came: its kind learns from it, the CPU its party ran meanwhile
    /// its work.
    fn answered(&self) {
        #[cfg(unix)]
        let work = Duration::from_nanos(u64::try_from(self.party.ran() - self.from).unwrap_or(0));
        // Cycles, which no rate of D215's is kept in: R* is a refusal's diagnosis alone.
        #[cfg(windows)]
        let work = Duration::ZERO;
        self.record.teach(self.began.elapsed(), work);
    }
}

/// What a test says of a [`Wait`] it gave up on, after what it waited for.
pub fn stalled(stall: &Stall) -> String {
    format!(
        "no thread of its party ran for {:?} its host did not hold it in, {:?} into the \
         wait, its window {:?} (D215){}",
        stall.over(),
        stall.waited(),
        stall.slowest(),
        stall.account()
    )
}

/// How long a polled wait sleeps between its polls ([`waited_on`]), as the daemon's tests
/// polled before their waits were judged.
const POLL: Duration = Duration::from_millis(1);

/// Waits until `f` holds, polled as the tests poll ([`POLL`]), `party` judged on `record`'s
/// judgment ([`Wait`]).
pub fn waited_on(record: &Record, party: Counted, mut f: impl FnMut() -> bool) -> Result<(), Stall> {
    let mut wait = Wait::on(record, party);
    loop {
        if f() {
            wait.answered();
            return Ok(());
        }
        wait.look()?;
        let (asked, slept) = (POLL, Instant::now());
        std::thread::sleep(asked);
        wait.overran(asked, slept.elapsed());
    }
}

/// Waits until `f` holds ([`Wait`]).
pub fn waited(f: impl FnMut() -> bool) -> Result<(), Stall> {
    waited_on(&WAITS, Counted::here(), f)
}

/// Waits until `f` holds ([`Wait`]); a wait given up on fails the test, saying `what`.
#[expect(
    clippy::panic,
    reason = "a test's wait given up on fails its test, as its assert would"
)]
pub fn until(what: &str, f: impl FnMut() -> bool) {
    if let Err(stall) = waited(f) {
        panic!("{what}: {}", stalled(&stall));
    }
}

/// What `attempt` gives, waited for on `record` and `party` ([`Wait`]): each attempt waits
/// at most the time it is given, until its wait's next look, and says none where nothing came
/// in it, as a wait of its caller's patience says it (a lane's ask, a steer, a wake, a
/// channel's receive); then its wait looks. Its answer, or what it said instead; the stall
/// where its wait was given up on.
pub fn answered_on<T, E>(
    record: &Record,
    party: Counted,
    mut attempt: impl FnMut(Duration) -> Result<Option<T>, E>,
) -> Result<Result<T, E>, Stall> {
    let mut wait = Wait::on(record, party);
    loop {
        let (asked, began) = (wait.slice(), Instant::now());
        match attempt(asked) {
            Ok(Some(value)) => {
                wait.answered();
                return Ok(Ok(value));
            }
            Err(said) => return Ok(Err(said)),
            Ok(None) => {
                wait.overran(asked, began.elapsed());
                wait.look()?;
            }
        }
    }
}

/// [`answered_on`] this process's tests' waits, its party the waiting thread's: its answer, or
/// what it said instead; the stall where its wait was given up on, for its caller to fail its
/// test with ([`stalled`] says it).
pub fn answered<T, E>(attempt: impl FnMut(Duration) -> Result<Option<T>, E>) -> Result<Result<T, E>, Stall> {
    answered_on(&WAITS, Counted::here(), attempt)
}

/// Why [`received`] has no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreceived {
    Stalled(Stall),
    /// Its sender went, none sent.
    Gone,
}

/// What `rx` gives, waited for on `record` and `party` ([`answered_on`]).
pub fn received_on<T>(record: &Record, party: Counted, rx: &mpsc::Receiver<T>) -> Result<T, Unreceived> {
    let got = answered_on(record, party, |patience| match rx.recv_timeout(patience) {
        Ok(value) => Ok(Some(value)),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Unreceived::Gone),
    });
    got.unwrap_or_else(|stall| Err(Unreceived::Stalled(stall)))
}

/// What `rx` gives, waited for ([`Wait`]).
pub fn received<T>(rx: &mpsc::Receiver<T>) -> Result<T, Unreceived> {
    received_on(&WAITS, Counted::here(), rx)
}

/// [`received`]; one not received fails the test, saying `what`.
#[expect(
    clippy::panic,
    reason = "a test's wait given up on fails its test, as its assert would"
)]
pub fn receive<T>(what: &str, rx: &mpsc::Receiver<T>) -> T {
    match received(rx) {
        Ok(value) => value,
        Err(Unreceived::Stalled(stall)) => panic!("{what}: {}", stalled(&stall)),
        Err(Unreceived::Gone) => panic!("{what}: its sender ended"),
    }
}

/// Under test, each party's due looks so far and whether a wait on it has ended, so that a test
/// may hold a member back until a wait on its party has looked ([`looked_at_least`]), never by a
/// time.
#[cfg(test)]
static LOOKED: Looked = Looked {
    looks: Mutex::new(std::collections::BTreeMap::new()),
    changed: std::sync::Condvar::new(),
};

/// [`LOOKED`]'s: by party, its due looks and whether a wait on it ended; and its changes.
#[cfg(test)]
struct Looked {
    looks: Mutex<std::collections::BTreeMap<u64, (u32, bool)>>,
    changed: std::sync::Condvar,
}

/// Under test, a wait on `party` made a due look, or (`ended`) ended.
#[cfg(test)]
fn looked(party: Counted, ended: bool) {
    let Counted::Party(id) = party else {
        return;
    };
    let mut looks = LOOKED.looks.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = looks.entry(id).or_default();
    if ended {
        entry.1 = true;
    } else {
        entry.0 = entry.0.saturating_add(1);
    }
    LOOKED.changed.notify_all();
}

/// Under test, until a wait on party `id` has made `looks` due looks, or one on it has ended.
#[cfg(test)]
fn looked_at_least(id: u64, looks: u32) {
    let made = LOOKED.looks.lock().unwrap_or_else(PoisonError::into_inner);
    // The lock let go at once: nothing more is read under it.
    drop(LOOKED.changed.wait_while(made, |made| {
        made.get(&id).is_none_or(|&(due, ended)| due < looks && !ended)
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    /// A thread made and not yet run is its party's from its making ([`party::carried`]): a
    /// wait begun as it is made, on what it alone answers, is held while its host has yet to
    /// run it, never refused at its first look. Its first run is held back by the test, never
    /// by load: its closure runs, on a thread of no party, only once the wait has made
    /// `LOOKS_TO_REFUSE` due looks, as the wait's own looks say ([`looked_at_least`]), within
    /// which a party still throughout is refused. Its record's own W*, 20 ms, as the cases
    /// below. Red (lanes/redgreen/testwait-made-member.patch) where a thread joins its party
    /// only as it first runs: refused at a look before it ran (ROOT, 2026-11-18, #36: under
    /// load, shards-api's `take_judged` 5 of 25 and its session's feeder 3 of 10).
    #[test]
    fn a_thread_made_and_not_yet_run_holds_its_partys_wait() {
        static MADE: Record = Record::new();
        MADE.teach(Duration::from_millis(20), Duration::ZERO);
        let id = party::of_test();
        let (answer, answered) = mpsc::channel();
        let made = party::carried(move || {
            let _ = answer.send(());
        });
        let runner = std::thread::spawn(move || {
            looked_at_least(id, shards_judge::LOOKS_TO_REFUSE);
            made();
        });
        let got = received_on(&MADE, Counted::here(), &answered);
        looked(Counted::Party(id), true);
        runner.join().unwrap();
        assert_eq!(got, Ok(()), "a wait on a thread made and not yet run was refused");
    }

    /// A test's wait gives up on a party that stopped, a whole W* gone by with nothing run;
    /// waits on one that is slow but runs, many W* past the slowest seen; and waits on one
    /// asleep until a deadline of its own, giving up a whole W* past the deadline where
    /// nothing follows (#77). Each party is a stand-in whose run is a count, its deadline a
    /// stand-in's, each case's record its own, taught a W* of 20 ms. Red where the wait is
    /// bounded by wall time alone (the stopped one waited on for good, or the slow or
    /// sleeping one cut), takes its own looks for progress, or judges a sleeping party
    /// before its deadline.
    #[test]
    fn a_wait_gives_up_on_a_stopped_party_and_waits_on_a_slow_or_sleeping_one() {
        static STOPPED: Record = Record::new();
        static SLOW: Record = Record::new();
        static ASLEEP: Record = Record::new();
        static WOKE_TO_NOTHING: Record = Record::new();
        static RUN: AtomicU64 = AtomicU64::new(0);
        static UNTIL: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let slowest = Duration::from_millis(20);
        for record in [&STOPPED, &SLOW, &ASLEEP, &WOKE_TO_NOTHING] {
            record.teach(slowest, Duration::ZERO);
        }
        let still = || 0;
        let none = || None;
        // Each stand-in's host holds none of its members.
        let unheld = || 0;
        let stopped = Counted::StandIn {
            ran: still,
            until: none,
            held: unheld,
        };
        let stall = waited_on(&STOPPED, stopped, || false).unwrap_err();
        assert!(stall.waited() >= slowest, "{stall:?}");

        // Asleep until 10 W* from now: waited on to its deadline, then a whole W* more.
        let until = *UNTIL.get_or_init(|| Instant::now() + slowest * 10);
        let asleep = || UNTIL.get().copied();
        let began = Instant::now();
        assert_eq!(
            waited_on(
                &ASLEEP,
                Counted::StandIn {
                    ran: still,
                    until: asleep,
                    held: unheld,
                },
                || { began.elapsed() >= slowest * 5 }
            ),
            Ok(())
        );
        let woke = Counted::StandIn {
            ran: still,
            until: asleep,
            held: unheld,
        };
        let stall = waited_on(&WOKE_TO_NOTHING, woke, || false).unwrap_err();
        assert!(Instant::now() >= until + slowest, "{stall:?}");

        let runs = || i128::from(RUN.load(Ordering::Relaxed));
        let done = AtomicBool::new(false);
        let began = Instant::now();
        std::thread::scope(|s| {
            party::spawn_in(s, || {
                while !done.load(Ordering::Relaxed) {
                    RUN.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
            });
            let slow = Counted::StandIn {
                ran: runs,
                until: none,
                held: unheld,
            };
            let waited = waited_on(&SLOW, slow, || began.elapsed() >= slowest * 25);
            done.store(true, Ordering::Relaxed);
            assert_eq!(waited, Ok(()));
        });
    }

    /// A wait whose party ran nothing over windows in which its host held a member of it
    /// (runnable on no CPU, grown in its wait for one, or waiting in its kernel) is held, not
    /// refused: D215's hold, of its members (`party::hold`). No overrun of its waiter shows a
    /// member its host held while it ran the waiter (CI 44164945050, aarch64 gnu,
    /// `a_change_while_its_record_is_written_is_written_after`: refused at W* 1.11 ms while its
    /// record's writer had yet to run). Its member never runs, and its waiter's attempts sleep
    /// nothing, so that no overrun of its own holds the wait either (a 1 ms sleep a Mac's
    /// timers coalesced held one alone, ROOT's red run): its member's hold is all that keeps
    /// it. Waited on past the due looks within which a still party is refused, and one more;
    /// red where a wait reads no member's hold, refused about a W* in.
    #[test]
    fn a_wait_whose_member_its_host_holds_is_held_not_refused() {
        static HELD: Record = Record::new();
        let slowest = Duration::from_millis(20);
        HELD.teach(slowest, Duration::ZERO);
        let held = Counted::StandIn {
            ran: || 0,
            until: || None,
            held: || 1,
        };
        let began = Instant::now();
        let past = slowest * (shards_judge::LOOKS_TO_REFUSE + 2);
        let waited = answered_on(&HELD, held, |_| {
            Ok::<_, ()>((began.elapsed() >= past).then_some(()))
        });
        assert_eq!(waited, Ok(Ok(())));
    }

    /// A party's members are read as their OS shows them (`party::hold`): one that spins is
    /// runnable, one asleep on a channel neither runnable nor waiting in its kernel, and the
    /// thread that reads is none of them. On Windows from the system's listing (shards-judge's
    /// `threads_held`); red there where it reads none, as before (0 of 0 read).
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_partys_members_are_read_as_their_os_shows_them() {
        let mine = party::of_test();
        let (spinning, spin) = (AtomicBool::new(false), AtomicBool::new(true));
        std::thread::scope(|s| {
            party::spawn_in(s, || {
                spinning.store(true, Ordering::SeqCst);
                while spin.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            until("its member did not spin", || spinning.load(Ordering::SeqCst));
            let hold = party::hold(mine);
            spin.store(false, Ordering::Relaxed);
            assert_eq!((hold.read, hold.runnable, hold.in_kernel), (1, 1, 0), "{hold:?}");
        });
        // Its member asleep, its party runs nothing: a reading that never says so is refused.
        let (keep, rest) = mpsc::channel::<()>();
        let (read, last) = std::thread::scope(|s| {
            party::spawn_in(s, move || {
                let _ = rest.recv();
            });
            let mut last = party::Hold::default();
            let read = waited(|| {
                last = party::hold(mine);
                (last.read, last.runnable, last.in_kernel) == (1, 0, 0)
            });
            drop(keep);
            (read, last)
        });
        assert!(read.is_ok(), "its member asleep not read so: {last:?}");
    }

    /// A test's wait counts its own test's threads alone ([`party`], D215): a party whose
    /// threads stopped is given up on while another test's party beside it in this process
    /// runs on, and then sleeps until a deadline of its own. Each party is made as a test's is
    /// ([`party::of_test`]), its members by [`party::spawn_member`], the record taught a W* of
    /// 20 ms. Red where a wait counts this process's threads and deadlines (`Counted::here`
    /// as `Counted::Process`): the stopped party's wait is held by the other's run, then by
    /// its deadline, and given up on long after the other stopped running (CI 43990298220,
    /// aarch64-apple-darwin: a removal test's wait that could not end was held 21 minutes by
    /// what the binary's other tests, all ended, had left; run alone it was refused in 81 ms,
    /// ROOT's r669).
    #[test]
    fn a_waits_party_is_its_own_tests_threads_alone() {
        static STOPPED: Record = Record::new();
        let slowest = Duration::from_millis(20);
        STOPPED.teach(slowest, Duration::ZERO);
        // Another test's party, made first: a thread of none, which makes its own, and a member
        // of it that runs until this test's wait has ended, or for 250 W*, and then sleeps,
        // its deadline 500 W* from its start, until this test's wait has ended.
        let (stop, stopping) = mpsc::channel::<()>();
        let (started, running) = mpsc::channel::<()>();
        let other = party::spawn_member(move || {
            party::of_test();
            party::spawn_member(move || {
                let began = Instant::now();
                let _asleep = Waiting::of(Some(slowest * 500));
                started.send(()).unwrap();
                let mut told = false;
                while !told && began.elapsed() < slowest * 250 {
                    told = stopping.try_recv().is_ok();
                    std::hint::spin_loop();
                }
                let spun = began.elapsed();
                if !told {
                    let _ = stopping.recv();
                }
                spun
            })
            .join()
            .unwrap()
        });
        running.recv().unwrap();
        // This test's party: a member waiting on nothing that comes.
        let mine = party::of_test();
        let (never, idle) = mpsc::channel::<()>();
        let idler = party::spawn_member(move || {
            let _ = idle.recv();
        });
        let began = Instant::now();
        let stall = waited_on(&STOPPED, Counted::here(), || false).unwrap_err();
        let given_up = began.elapsed();
        stop.send(()).unwrap();
        let spun = other.join().unwrap();
        assert!(
            given_up < spun,
            "party {mine}'s wait was given up on {given_up:?} in, the other party having stopped \
             running {spun:?} after it began: {stall:?}"
        );
        drop(never);
        idler.join().unwrap();
    }

    /// A wait whose kind is cold, no answer of it come in this process, refuses a party still
    /// throughout (no member ran, none held, no deadline to come) within `LOOKS_TO_REFUSE` of
    /// its judged looks, each a host's granularity ([`COLD`]). Its condition is the test's own
    /// judged wait, on a stand-in whose progress is each judged look the cold wait makes
    /// (its stand-in's `ran`, read at each), of a W* of `LOOKS_TO_REFUSE` of the cold wait's
    /// polls ([`POLL`] and a granularity each), so that it is given up on only once the cold
    /// wait judges no more. Red where a cold wait is not judged, as before: its looks read
    /// nothing, the test's own wait is given up on and ends it, answered.
    #[test]
    fn a_cold_wait_refuses_a_still_party() {
        static KIND: Record = Record::new();
        static OWN: Record = Record::new();
        static LOOKS: AtomicU64 = AtomicU64::new(0);
        let grain = match judging(&KIND).seen() {
            Seen::Learned { slowest, .. } => slowest,
            Seen::Cold => panic!("its host's granularity unmeasured"),
        };
        OWN.teach_unrated((POLL + grain) * shards_judge::LOOKS_TO_REFUSE);
        let still = Counted::StandIn {
            ran: || {
                LOOKS.fetch_add(1, Ordering::Relaxed);
                0
            },
            until: || None,
            held: || 0,
        };
        let judging_looks = Counted::StandIn {
            ran: || i128::from(LOOKS.load(Ordering::Relaxed)),
            until: || None,
            held: || 0,
        };
        let mut own = Wait::on(&OWN, judging_looks);
        let refused = waited_on(&KIND, still, || own.look().is_err());
        let stall = refused.expect_err("a still party's cold wait never gave up");
        let Seen::Learned { slowest, .. } = COLD.seen() else {
            panic!("refused by no granularity");
        };
        assert!(
            stall.slowest() <= slowest,
            "judged by its host's granularity: {stall:?}"
        );
        assert_eq!(KIND.seen(), Seen::Cold, "a refusal teaches its kind nothing");
    }

    /// A wait of attempts ([`answered_on`]), each of which says none where nothing came in the
    /// time it is given: waited on while its party runs, many W* past the slowest seen, until
    /// its answer; ended at once by what an attempt says instead; and given up on a whole W*
    /// after its party stopped. Each case's record its own, taught a W* of 20 ms. Red where an
    /// attempt is given a patience of the test's own (the slow party cut, the stopped one
    /// attempted for good) or an attempt's error is taken for none.
    #[test]
    fn attempts_are_waited_on_while_their_party_runs_and_given_up_once_it_stops() {
        static SAID: Record = Record::new();
        static STOPPED: Record = Record::new();
        static SLOW: Record = Record::new();
        static RUN: AtomicU64 = AtomicU64::new(0);
        let slowest = Duration::from_millis(20);
        for record in [&SAID, &STOPPED, &SLOW] {
            record.teach(slowest, Duration::ZERO);
        }
        let still = Counted::StandIn {
            ran: || 0,
            until: || None,
            held: || 0,
        };
        assert_eq!(
            answered_on(&SAID, still, |_| Err::<Option<()>, _>("refused")),
            Ok(Err("refused"))
        );
        let stall = answered_on(&STOPPED, still, |patience| {
            std::thread::sleep(patience);
            Ok::<Option<()>, ()>(None)
        })
        .unwrap_err();
        assert!(stall.waited() >= slowest, "{stall:?}");
        let runs = Counted::StandIn {
            ran: || i128::from(RUN.load(Ordering::Relaxed)),
            until: || None,
            held: || 0,
        };
        let began = Instant::now();
        let answered = answered_on(&SLOW, runs, |patience| {
            // Its party runs throughout each attempt.
            let attempted = Instant::now();
            while attempted.elapsed() < patience {
                RUN.fetch_add(1, Ordering::Relaxed);
                std::hint::spin_loop();
            }
            Ok::<_, ()>((began.elapsed() >= slowest * 25).then_some(()))
        });
        assert_eq!(answered, Ok(Ok(())));
    }
}
