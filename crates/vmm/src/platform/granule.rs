//! The finest step this process's governors resolve, D191's g: the greater of how soon their
//! timed waits end and the step their threads' CPU clocks count in, each measured as the
//! process runs, never a figure of a host's baked in (R03).
//!
//! - **Its wake** ([`Late`]): the least a governor's timed wait was seen to end past what it
//!   asked, over every such wait of the process's. A wait woken before its time (a new limit,
//!   the machine's end) says nothing of it and is not counted. The least, as the soonest a wait
//!   ends is what the host resolves; one late on a loaded host only adds above it.
//! - **Its clock** ([`clock_step`]): the step a thread's CPU clock counts in, read on a thread
//!   that spins until its clock has moved twice: the first move may be part of a step, the
//!   second is a whole one. GetThreadTimes' ticks on Windows (15.625 ms, PM M294), nanoseconds'
//!   reads where the clock counts them (clock_gettime(2)'s thread CPU clock on Linux; XNU's
//!   `thread_info`). Measured once a process, the first time a governor needs it: it spins two
//!   of its steps at most.
//!
//! Cold, neither is known: so before a governor holds anything, both are measured
//! ([`measure`]): its wake by one timed wait of the least a wait can ask, a nanosecond, whose
//! lateness is the host's own wake granularity (a timer tick where the host waits in ticks,
//! microseconds where it waits on high-resolution timers), and its clock as below. Until a
//! governor first asks, g is the least wait it asks for, [`LEAST_WAIT`]. A clock that cannot
//! be read leaves its step unknown, typed ([`Unclocked`]), and g its wake's.
//!
//! [`period`] is D191's derivation from g, shared by every governor that holds threads to a
//! share of a CPU: the hold's (vm/hold.rs), a share its rate's, and Windows' priority
//! governor's (`windows_thread`), one thread's half a CPU. `credited` is that governor's
//! account of a share from one look to the next, pure, so that its bound is held on every host.

use std::io;
use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The least a governor waits between reads: CFS bandwidth control's own precision, 1 ms,
/// as D191 holds vCPUs to a rate as CFS holds a cgroup's threads to its quota, no closer. Not
/// a host's timer resolution: g adds the host's measured wake lateness above it ([`measure`]). CFS takes no quota or period below it (kernel/sched/core.c l. 10054,
/// `min_bw_quota_period_us`), and leaves each CPU that much of its runtime past the quota
/// (kernel/sched/fair.c l. 7179, `min_cfs_rq_runtime`; Documentation/scheduler/sched-bwc.rst
/// l. 94-95, 179-186; Linux 1590cf03).
pub const LEAST_WAIT: Duration = Duration::from_millis(1);

/// What is known of the step: none of either, cold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// The least a timed wait ended past what it asked: none until one has ended on its own.
    pub late: Option<Duration>,
    /// The step a thread's CPU clock counts in: none until measured, or where none could be.
    pub clock: Option<Duration>,
}

impl Step {
    /// g, for a governor whose least wait is `least`: as late as its least wait ends at the
    /// soonest seen, or its clock's step, whichever is the greater; `least` cold.
    pub fn g(self, least: Duration) -> Duration {
        let woke = least.saturating_add(self.late.unwrap_or(Duration::ZERO));
        woke.max(self.clock.unwrap_or(Duration::ZERO))
    }
}

/// The least lateness seen of the timed waits counted, in nanoseconds; `u64::MAX` before any.
#[derive(Debug)]
pub struct Late(AtomicU64);

impl Late {
    pub const fn new() -> Late {
        Late(AtomicU64::new(u64::MAX))
    }

    /// A timed wait that asked `asked` and began at `began` has ended: counted where it ended
    /// on its own, no sooner than it asked.
    pub fn waited(&self, asked: Duration, began: Instant) {
        if let Some(late) = began.elapsed().checked_sub(asked) {
            let ns = u64::try_from(late.as_nanos()).unwrap_or(u64::MAX - 1);
            self.0.fetch_min(ns, Ordering::Relaxed);
        }
    }

    /// The least lateness seen: none before a wait was counted.
    pub fn least(&self) -> Option<Duration> {
        let ns = self.0.load(Ordering::Relaxed);
        (ns != u64::MAX).then(|| Duration::from_nanos(ns))
    }
}

impl Default for Late {
    fn default() -> Late {
        Late::new()
    }
}

/// Why a thread's CPU clock's step is not known: its clock could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unclocked(pub String);

/// The process's governors' waits ([`Late::waited`]).
pub static LATE: Late = Late::new();
static CLOCK: OnceLock<Result<Duration, Unclocked>> = OnceLock::new();

/// The step known now: the process's waits' least lateness, and its clock's step where it has
/// been measured ([`measure_clock`]).
pub fn step() -> Step {
    Step {
        late: LATE.least(),
        clock: CLOCK.get().and_then(|c| c.as_ref().ok().copied()),
    }
}

/// Half a CPU, in milli-CPU: the most a fair scheduler gives a thread that contends for its
/// CPU at all, the share Windows' raised threads are credited and macOS's real-time threads'
/// constraint is derived at. A thread's priority matters only where another thread is ready on
/// that CPU; beside k ≥ 1 ready threads of its priority, Windows gives each a time slice in
/// turn (Microsoft, Scheduling Priorities), as Linux's fair scheduler gives threads of one
/// weight w each w / (w + k·w) (kernel/sched/core.c `sched_prio_to_weight`, one weight for
/// every nice-0 thread): 1 / (k + 1) ≤ 1/2, at k = 1 the most. A bound, not a pick: no
/// contended thread is owed more.
pub const HALF_CPU: std::num::NonZeroU64 = match std::num::NonZeroU64::new(crate::vm::MILLI_PER_CPU / 2) {
    Some(half) => half,
    None => std::num::NonZeroU64::MIN,
};

/// g now, for a governor whose least wait is [`LEAST_WAIT`]: [`Step::g`] of what is known.
pub fn g() -> Duration {
    step().g(LEAST_WAIT)
}

/// Why D191 derives no period: vcpus × g over the share outgrows what a `Duration` of
/// nanoseconds in a u64 holds (585 years), never saturated into a period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outgrown {
    pub vcpus: u64,
    pub milli: NonZeroU64,
    pub g: Duration,
}

impl std::fmt::Display for Outgrown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} vCPUs held to {} milli-CPU at a step of {:?}: a period past what a u64 of ns holds",
            self.vcpus, self.milli, self.g
        )
    }
}

/// The period D191 derives for `vcpus` threads held together to `milli` milli-CPU by a
/// governor that resolves `g`: the shortest in which it resolves their share, vcpus × g over
/// the share, the share `milli` over `vm::MILLI_PER_CPU` CPUs. A share of every thread's
/// whole CPU is held over g itself; no share is held over less, as none is below g.
pub fn period(vcpus: u64, milli: NonZeroU64, g: Duration) -> Result<Duration, Outgrown> {
    let outgrown = Outgrown { vcpus, milli, g };
    let ns = g
        .as_nanos()
        .checked_mul(u128::from(vcpus))
        .and_then(|n| n.checked_mul(u128::from(crate::vm::MILLI_PER_CPU)))
        .ok_or(outgrown)?
        / u128::from(milli.get());
    u64::try_from(ns).map(Duration::from_nanos).map_err(|_| outgrown)
}

/// The most credit a share of `cpus` CPUs keeps over `period`, in nanoseconds, and the most it
/// owes: what one look can charge it, the whole period of each CPU (every step of the period
/// landing on its runs).
#[cfg(any(windows, test))]
pub fn most_credit(period: Duration, cpus: u64) -> i64 {
    let period = i64::try_from(period.as_nanos()).unwrap_or(i64::MAX);
    period.saturating_mul(i64::try_from(cpus).unwrap_or(i64::MAX))
}

/// A share's credit at a look, in nanoseconds: `credit`, once `passed` has gone by since the
/// last look and its threads used `used` nanoseconds of CPU time on `cpus` CPUs, given half of
/// each CPU's time ([`HALF_CPU`]), charged what they used, and kept to a period of each CPU
/// ([`most_credit`]) above none, and to a period and a `step` of each below it. Above none, its
/// threads may be raised; what a look charges past it is owed, and the halves of later looks pay
/// it before they are raised again. Kept to none, a share raised on a few nanoseconds of credit
/// was forgiven the rest of a look it ran past it each time it drained (D191, 2026-11-18). At
/// D191's period of 2g, no look within a step of its period charges a share it found above none
/// a period past its credit: three steps at most, of which it credits one and more. And a clock
/// that counts in steps (GetThreadTimes', 15.625 ms, PM M294) charges a thread the step it ran
/// in as the step ends, so the look after the one that lowered it may still be charged a step
/// of its raised run: kept to a period's debt alone, windows-2022's pools were forgiven 78 to 146
/// ms of it in each 3.5 s their three spinners ran beside a normal one (PM M620). So `step`,
/// g, the finest the governor resolves, its clock's step where that is the coarser: the floor
/// forgives no raised run, only what a share runs lowered on CPUs its host's threads do not want
/// past a period's debt and a step.
#[cfg(any(windows, test))]
pub fn credited(
    credit: i64,
    passed: Duration,
    used: u64,
    cpus: u64,
    period: Duration,
    step: Duration,
) -> i64 {
    let half =
        passed.as_nanos().saturating_mul(u128::from(HALF_CPU.get())) / u128::from(crate::vm::MILLI_PER_CPU);
    let half = i64::try_from(half).unwrap_or(i64::MAX);
    let most = most_credit(period, cpus);
    let owed = most.saturating_add(most_credit(step, cpus));
    credit
        .saturating_add(half.saturating_mul(i64::try_from(cpus).unwrap_or(i64::MAX)))
        .saturating_sub(i64::try_from(used).unwrap_or(i64::MAX))
        .min(most)
        .max(owed.saturating_neg())
}

static WOKE: OnceLock<()> = OnceLock::new();

/// What a governor needs known before it holds anything, measured once a process on the
/// calling thread: its wake ([`measure_wake`]) and its clock's step ([`measure_clock`]).
pub fn measure() -> Result<Duration, Unclocked> {
    WOKE.get_or_init(|| measure_wake(&LATE));
    measure_clock()
}

/// One timed wait of the least a wait can ask, a nanosecond, counted in `late`: how late the
/// host ends the shortest wait it is asked for, its own wake granularity, as no wait it ends
/// is ended sooner.
pub fn measure_wake(late: &Late) {
    let asked = Duration::from_nanos(1);
    let began = Instant::now();
    std::thread::sleep(asked);
    late.waited(asked, began);
}

/// The step the calling thread's CPU clock counts in, measured the first time any thread of
/// the process asks, on that thread ([`clock_step`]); what it was after.
pub fn measure_clock() -> Result<Duration, Unclocked> {
    CLOCK
        .get_or_init(|| {
            let clock =
                super::ThreadClock::current().map_err(|e| Unclocked(format!("its thread's clock: {e}")))?;
            clock_step(|| clock.read()).map_err(|e| Unclocked(format!("reading its thread's clock: {e}")))
        })
        .clone()
}

/// The step a clock `read` counts in: read until it has moved twice, the second move's size.
/// The thread reading spins, and a thread's CPU clock moves as it runs.
pub fn clock_step(mut read: impl FnMut() -> io::Result<u64>) -> io::Result<Duration> {
    let first = read()?;
    let once = moved(&mut read, first)?;
    let twice = moved(&mut read, once)?;
    Ok(Duration::from_nanos(twice.saturating_sub(once)))
}

/// What `read` says once it says other than `from`.
fn moved(read: &mut impl FnMut() -> io::Result<u64>, from: u64) -> io::Result<u64> {
    loop {
        let now = read()?;
        if now != from {
            return Ok(now);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// g is the least wait cold; as late as it ends at the soonest once a wait has; the
    /// clock's step where that is coarser, as GetThreadTimes' ticks are (PM M294).
    #[test]
    fn g_is_the_least_wait_cold_then_the_greater_of_its_wake_and_its_clock() {
        let least = Duration::from_millis(1);
        assert_eq!(Step::default().g(least), least);
        let woke = Step {
            late: Some(Duration::from_micros(50)),
            clock: Some(Duration::from_nanos(40)),
        };
        assert_eq!(woke.g(least), Duration::from_micros(1050));
        let ticked = Step {
            late: Some(Duration::from_micros(50)),
            clock: Some(Duration::from_micros(15_625)),
        };
        assert_eq!(ticked.g(least), Duration::from_micros(15_625));
        let unclocked = Step {
            late: None,
            clock: Some(Duration::from_micros(15_625)),
        };
        assert_eq!(unclocked.g(least), Duration::from_micros(15_625));
    }

    /// A wait's lateness counts once it ends on its own, the least of all kept; one woken
    /// before its time says nothing.
    #[test]
    fn a_wake_counts_its_least_lateness_and_none_of_early_ends() {
        let late = Late::new();
        assert_eq!(late.least(), None);
        late.waited(Duration::from_secs(3600), Instant::now());
        assert_eq!(late.least(), None, "an early end was counted");
        let asked = Duration::from_millis(2);
        let began = Instant::now();
        std::thread::sleep(asked);
        late.waited(asked, began);
        let first = late.least().unwrap();
        let began = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        late.waited(asked, began);
        assert_eq!(late.least(), Some(first), "a later, later wake raised the least");
    }

    /// D191's derived period: vcpus × g over the share, the shortest in which a governor
    /// resolves it. Four vCPUs held under 625 milli-CPU by GetThreadTimes' 15.625 ms ticks
    /// (PM M294) take CFS's 100 ms; one thread's half a CPU, 2g; a whole CPU's share of each
    /// thread's, g; one past what a u64 of ns holds, typed.
    #[test]
    fn a_derived_period_resolves_its_share() {
        let tick = Duration::from_micros(15_625);
        let milli = |m| NonZeroU64::new(m).unwrap();
        assert_eq!(period(4, milli(625), tick), Ok(Duration::from_millis(100)));
        assert_eq!(period(4, milli(500), tick), Ok(Duration::from_millis(125)));
        assert_eq!(period(1, milli(500), tick), Ok(Duration::from_micros(31_250)));
        assert_eq!(period(4, milli(4000), LEAST_WAIT), Ok(LEAST_WAIT));
        assert_eq!(period(1, milli(250), LEAST_WAIT), Ok(Duration::from_millis(4)));
        let huge = Duration::from_secs(u64::MAX);
        assert_eq!(
            period(u64::MAX, milli(1), huge),
            Err(Outgrown {
                vcpus: u64::MAX,
                milli: milli(1),
                g: huge
            })
        );
        assert!(period(1, milli(1), huge).is_err());
    }

    /// D191's bound over any stretch: a share of one CPU, raised while its credit is above none
    /// and then running all of it, its looks each a little after its period and charged the
    /// clock's whole steps (GetThreadTimes', PM M294), runs raised no more than half of the time
    /// passed and two periods (its credit, and one look's run past it) at every look; it says
    /// the share of its CPU it took. Its credit kept to none forgave what each draining look
    /// charged past it: raised two looks in three past the first drain, 2/3 of its CPU, it
    /// passed the bound at the ninth look.
    #[test]
    fn a_share_takes_half_its_cpu_and_two_periods_over_any_stretch() {
        let step = Duration::from_micros(15_625);
        let period = period(1, HALF_CPU, step).unwrap();
        // The least a look can come after its period and be credited more than half of one:
        // `credited` halves whole nanoseconds.
        let late = Duration::from_nanos(2);
        // To the first look at which a share whose overruns are forgiven passes the bound:
        // charged two steps a look and credited one and a nanosecond, its full credit of two
        // steps is gone at the third look, and it is raised two looks in three after it, 7
        // periods by the ninth against 4.5 and two.
        const LOOKS: usize = 9;
        let (mut credit, mut raised) = (most_credit(period, 1), true);
        let (mut at, mut ran) = (Duration::ZERO, Duration::ZERO);
        // Each look: raised through it, what it had run raised by its end, and its bound then.
        let mut looks = Vec::new();
        for _ in 0..LOOKS {
            let next = at + period + late;
            // The clock's steps that fell in the look, each charged to the share where raised.
            let steps = next.as_nanos() / step.as_nanos() - at.as_nanos() / step.as_nanos();
            let used = if raised {
                step * u32::try_from(steps).unwrap()
            } else {
                Duration::ZERO
            };
            credit = credited(
                credit,
                next - at,
                u64::try_from(used.as_nanos()).unwrap(),
                1,
                period,
                step,
            );
            (at, ran) = (next, ran + used);
            looks.push((raised, ran, at / 2 + period * 2));
            raised = credit > 0;
        }
        let ways: String = looks
            .iter()
            .map(|(raised, ..)| if *raised { 'R' } else { 'L' })
            .collect();
        let drained = looks
            .iter()
            .position(|(raised, ..)| !raised)
            .expect("never lowered");
        let after = &looks[drained..];
        let past = after.iter().filter(|(raised, ..)| *raised).count() as f64 / after.len() as f64;
        let said = format!(
            "raised {ran:?} of {at:?}, {:.3} of its CPU, {past:.3} past its first drain ({ways})",
            ran.as_secs_f64() / at.as_secs_f64()
        );
        println!("{said}");
        for (look, (_, ran, bound)) in looks.iter().enumerate() {
            assert!(
                ran <= bound,
                "look {}: raised {ran:?}, past its bound {bound:?}; {said}",
                look + 1
            );
        }
    }

    /// The cold wake is measured, not assumed: one shortest wait, its lateness the host's own
    /// granularity, counted before any governor has waited; g then rests on it.
    #[test]
    fn a_cold_wake_is_measured_before_any_wait() {
        let late = Late::new();
        assert_eq!(late.least(), None);
        let began = Instant::now();
        measure_wake(&late);
        let woke = late.least().unwrap();
        assert!(woke <= began.elapsed(), "{woke:?}");
        measure().unwrap();
        assert!(super::step().late.is_some(), "measure leaves g its wake");
    }

    /// A clock's step is its second move: the first may be part of one, as a read can begin
    /// mid-tick.
    #[test]
    fn a_clocks_step_is_its_second_move() {
        let mut reads = [0u64, 0, 7, 7, 7, 15_625_007, 15_625_007, 31_250_007].into_iter();
        let step = clock_step(|| reads.next().ok_or_else(|| io::Error::other("no more reads"))).unwrap();
        assert_eq!(step, Duration::from_nanos(15_625_000));
        let mut failing = [0u64, 0].into_iter();
        assert!(clock_step(|| failing.next().ok_or_else(|| io::Error::other("gone"))).is_err());
        // This host's thread clock moves, and its step is no longer than the spin took: a spin
        // of this test's own, as the process's step is spun once a process, by whichever of
        // its callers comes first (another test's `measure`, a hold's), and then only read.
        let clock = crate::platform::ThreadClock::current().unwrap();
        let began = Instant::now();
        let step = clock_step(|| clock.read()).unwrap();
        assert!(!step.is_zero() && step <= began.elapsed(), "{step:?}");
        let kept = measure_clock().unwrap();
        assert!(!kept.is_zero(), "{kept:?}");
        assert_eq!(super::step().clock, Some(kept));
    }
}
