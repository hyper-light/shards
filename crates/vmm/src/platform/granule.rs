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
//! governor's (`windows_thread`), one thread's half a CPU.

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
        // This host's thread clock moves, and its step is no longer than the spin took.
        let began = Instant::now();
        let step = measure_clock().unwrap();
        assert!(!step.is_zero() && step <= began.elapsed(), "{step:?}");
        assert_eq!(super::step().clock, Some(step));
    }
}
