//! vCPU and device thread scheduling policy on macOS.
//!
//! With the default policy, HVF's in-kernel WFI wake-ups for guest timers are late by
//! ~25% of the interval (258 µs on a 1 ms timer) because of timer coalescing. Under
//! Mach's time-constraint policy they are late by 5-19 µs, and interrupt-delivery p99
//! to an idle vCPU drops from 61-315 µs to 11 µs
//! (docs/research/platform-measurements.md M8, M10).
//!
//! XNU depresses a thread at that policy which runs 100 quanta, a second, without
//! blocking: to priority 0, below every other thread of the host's, for twice that, and on
//! a busy host for as long as it stays busy, since its release is looked for only as the
//! thread runs (osfmk/kern/priority.c, `thread_quantum_expire`; thread_policy.c,
//! DEPRESSPRI for a real-time thread the fail-safe demoted). A run's vCPU busy for a
//! second ran next to nothing for the eleven after (PM M317). So each thread made
//! real-time here is watched by a governor, which looks at it once a scheduler tick. One
//! that has run a whole tick since it was last seen waiting is looked at as finely as its host
//! wakes (the cold pair's constraint, below), for a tick more: found waiting at any look, it was
//! waiting between its runs, as an I/O-bound vCPU or a device worker does, and stays; found
//! running at every one, it goes to timeshare, as any program's thread runs, and is
//! real-time again once it is seen waiting. One so moved is at most three ticks into the
//! fail-safe's second, and a busy guest's vCPUs share the host's CPUs as its other programs
//! do. Moved at the first tick found running instead, a block device's vCPUs, waiting on
//! its answers half their time, were moved too and read fewer than with no governor
//! (PM M317).
//!
//! The policy's pair is each thread's own (platform-11), never a figure of a host's:
//! - *Its computation*, the CPU it may run before another real-time thread may preempt it, is
//!   its batch: the most CPU the governor has seen it run between two looks that each found
//!   it waiting, which bounds every run between its waits in that span from above, so that
//!   none is preempted for its computation's sake. XNU takes one from 50 µs to 50 ms
//!   (osfmk/kern/sched_rt.c `sched_realtime_timebase_init`: `min_rt_quantum`, "smallest rt
//!   computation (50 us)", `max_rt_quantum`, "maximum rt computation (50 ms)"); a batch
//!   outside is held to them.
//! - *Its constraint*, the time within which it asks to have run its computation, is D191's
//!   period for one thread at half a CPU (`granule::period(1, HALF_CPU, computation)`, twice
//!   the computation): the most a fair scheduler gives a thread that contends at all.
//! - *Cold*, before the governor has seen a batch, the computation is the host's measured
//!   wake (`granule::measure`: the lateness of its shortest wait), held to XNU's least: a
//!   thread asks for no more than the host's own wake grain until it is seen to need more.
//! - *Typed failure*: no wake measured, or a pair the timebase cannot say in a u32 of its
//!   ticks, leaves the thread at its policy, why said ([`Unconstrained`]).

use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::granule;
use crate::sync::{lock, wait};

/// The least and the most computation XNU takes of a time-constraint policy
/// (osfmk/kern/sched_rt.c `sched_realtime_timebase_init`: `min_rt_quantum`, 50 µs;
/// `max_rt_quantum`, 50 ms).
const MIN_RT_QUANTUM: Duration = Duration::from_micros(50);
const MAX_RT_QUANTUM: Duration = Duration::from_millis(50);

/// A thread's time-constraint pair: the CPU it may run before another real-time thread may
/// preempt it, and the time within which it asks to have run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Constraint {
    computation: Duration,
    constraint: Duration,
}

/// Why a thread is not made real-time by its own pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unconstrained {
    /// No wake of the host's was measured, which a cold thread's computation is.
    Unmeasured,
    /// Its constraint is past what D191's period says ([`granule::Outgrown`]).
    Outgrown(granule::Outgrown),
    /// The pair is past what the timebase says in a u32 of its ticks, or it is unread.
    Timebase(String),
}

impl std::fmt::Display for Unconstrained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unconstrained::Unmeasured => f.write_str("no wake of the host's measured"),
            Unconstrained::Outgrown(o) => write!(f, "its constraint: {o}"),
            Unconstrained::Timebase(why) => write!(f, "its pair in the timebase's ticks: {why}"),
        }
    }
}

/// The pair of a thread whose batch is `batch`: its computation the batch held to XNU's
/// bounds, its constraint D191's period for one thread at half a CPU of it.
fn constraint_of(batch: Duration) -> Result<Constraint, Unconstrained> {
    let computation = batch.clamp(MIN_RT_QUANTUM, MAX_RT_QUANTUM);
    let constraint = granule::period(1, granule::HALF_CPU, computation).map_err(Unconstrained::Outgrown)?;
    Ok(Constraint {
        computation,
        constraint,
    })
}

/// A cold thread's pair: its batch the host's measured wake, before the governor has seen
/// it run ([`granule::measure`], once a process).
fn cold() -> Result<Constraint, Unconstrained> {
    if let Err(e) = granule::measure() {
        crate::debug!("the real-time policy knows no clock step: {}", e.0);
    }
    constraint_of(granule::step().late.ok_or(Unconstrained::Unmeasured)?)
}

/// The batch learned of a thread that ran `span` of CPU between two looks that each found it
/// waiting, `batch` before: the most seen.
fn learned(batch: Option<Duration>, span: Duration) -> Option<Duration> {
    match (batch, span.is_zero()) {
        (batch, true) => batch,
        (None, false) => Some(span),
        (Some(batch), false) => Some(batch.max(span)),
    }
}

/// How often the governor looks: XNU's scheduler tick, at which it ages timeshare threads'
/// usage (osfmk/kern/sched.h, `SCHED_TICK_SHIFT`: a second shifted right by 3).
const TICK: Duration = Duration::from_millis(1000 >> 3);
/// What XNU's fail-safe lets a real-time thread run without blocking: 100 quanta
/// (sched_prim.c, `MAX_UNSAFE_RT_QUANTA`) of 10 ms (`DEFAULT_PREEMPTION_RATE`, 100 a
/// second). A thread suspected of running without a wait is looked at every constraint of
/// the cold pair, as finely as the host wakes ([`govern`]).
const FAIL_SAFE: Duration = Duration::from_millis(100 * 10);
// A suspect, found once it has run a tick past its last wait (two at most past the look
// before) and moved a tick after, has run at most three by then.
const _: () = assert!(3 * TICK.as_millis() < FAIL_SAFE.as_millis());

/// mach/policy.h: what thread_info says of a thread at timeshare.
const POLICY_TIMESHARE: i32 = 1;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

/// A thread the governor watches.
struct Watched {
    port: libc::mach_port_t,
    /// Its CPU time, in nanoseconds, when it was last seen waiting: XNU counts the time it
    /// runs from its last wake, and that wake came after.
    waited_at: u64,
    /// Whether the governor has it at the time-constraint policy.
    realtime: bool,
    /// When it was found to have run a tick past its last wait, if it has not been seen
    /// waiting since: it is looked at every constraint of the cold pair until a tick from then.
    suspected: Option<Instant>,
    /// The most CPU it was seen to run between two looks that each found it waiting.
    batch: Option<Duration>,
    /// The pair it was last set at, or would be once it waits.
    asked: Constraint,
}

static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());
/// The governor waits on it while it watches no thread.
static ADDED: Condvar = Condvar::new();
static GOVERNOR: OnceLock<Result<(), String>> = OnceLock::new();

/// The calling thread kept at the time-constraint policy while the governor sees it wait
/// between its runs. Held by the thread while it runs, and dropped before it ends, which
/// takes it from the governor's watch: a thread's port is its own, and goes with it.
#[derive(Debug)]
pub struct Watch {
    port: libc::mach_port_t,
    /// Its own thread's: it neither moves nor is shared.
    _thread: PhantomData<*const ()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        lock(&WATCHED).retain(|w| w.port != self.port);
    }
}

/// Asks QoS user-interactive for the calling thread, then sets the Mach time-constraint
/// policy, which takes the QoS class off again as it is set (thread_policy.c; PM M317), and
/// has the governor watch it ([`Watch`]). Failure is reported, not fatal: the thread runs
/// on at user-interactive, only with coarser timers. A thread the governor could not watch
/// is not made real-time: it would meet the fail-safe.
pub fn make_current_realtime() -> Result<Watch, String> {
    GOVERNOR.get_or_init(start_governor).clone()?;
    // SAFETY: the current thread's own QoS.
    let rc = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        return Err(format!(
            "pthread_set_qos_class_self_np: {}",
            std::io::Error::from_raw_os_error(rc)
        ));
    }
    // SAFETY: the calling thread's own port, which lives as long as it does.
    let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
    let asked = cold().map_err(|e| e.to_string())?;
    set_realtime(port, asked)?;
    lock(&WATCHED).push(Watched {
        port,
        // As if last seen waiting as it began, its time before now counted.
        waited_at: 0,
        realtime: true,
        suspected: None,
        batch: None,
        asked,
    });
    ADDED.notify_one();
    Ok(Watch {
        port,
        _thread: PhantomData,
    })
}

/// The time-constraint policy of `pair`, for the thread `port` names.
fn set_realtime(port: libc::mach_port_t, pair: Constraint) -> Result<(), String> {
    let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: a struct for mach_timebase_info to fill.
    if unsafe { mach_timebase_info(&raw mut tb) } != 0 || tb.numer == 0 {
        return Err("mach_timebase_info failed".into());
    }
    let to_abs = |d: Duration| {
        u64::try_from(d.as_nanos())
            .ok()
            .and_then(|ns| ns.checked_mul(u64::from(tb.denom)))
            .map(|v| v / u64::from(tb.numer))
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| Unconstrained::Timebase(format!("{d:?} past a u32 of its ticks")).to_string())
    };
    let mut policy = libc::thread_time_constraint_policy {
        period: 0,
        computation: to_abs(pair.computation)?,
        constraint: to_abs(pair.constraint)?,
        preemptible: 1,
    };
    // SAFETY: a thread port of this task, and a policy of the flavor's count of words.
    let kr = unsafe {
        libc::thread_policy_set(
            port,
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            (&raw mut policy).cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(format!("thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY) = {kr}"));
    }
    Ok(())
}

/// Timeshare, as a program's thread runs, for the thread `port` names: out of the fail-safe's
/// depression too, should it have met it (thread_policy.c: a demoted thread's saved mode).
fn set_timeshare(port: libc::mach_port_t) -> Result<(), String> {
    let mut policy = libc::thread_extended_policy { timeshare: 1 };
    // SAFETY: a thread port of this task, and a policy of the flavor's count of words.
    let kr = unsafe {
        libc::thread_policy_set(
            port,
            libc::THREAD_EXTENDED_POLICY as libc::thread_policy_flavor_t,
            (&raw mut policy).cast(),
            libc::THREAD_EXTENDED_POLICY_COUNT,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(format!("thread_policy_set(THREAD_EXTENDED_POLICY) = {kr}"));
    }
    Ok(())
}

/// What thread_info says of the thread `port` names; `None` once it is gone.
fn info(port: libc::mach_port_t) -> Option<libc::thread_extended_info> {
    // SAFETY: zeroed plain data, which thread_info fills.
    let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
    let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
    // SAFETY: a port of a thread of this process; a buffer of `count` words.
    let r = unsafe {
        libc::thread_info(
            port,
            libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
            (&raw mut info).cast(),
            &raw mut count,
        )
    };
    (r == libc::KERN_SUCCESS).then_some(info)
}

fn start_governor() -> Result<(), String> {
    std::thread::Builder::new()
        .name("realtime-governor".into())
        .spawn(govern)
        .map(drop)
        .map_err(|e| format!("spawning the real-time governor: {e}"))
}

/// Once a tick, for as long as the process runs, a look at every thread it watches, and
/// every constraint of the cold pair at those suspected of running without a wait; while it
/// watches none, a wait for one.
fn govern() {
    // It looks on time however busy the threads it watches keep the CPUs: at their
    // policy, its own pair the cold one (a look's batch is no more than the host's wake),
    // which its sleeps between looks keep it clear of the fail-safe in.
    // SAFETY: the calling thread's own port, which lives as long as it does.
    let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
    if let Err(e) = cold()
        .map_err(|e| e.to_string())
        .and_then(|pair| set_realtime(port, pair))
    {
        crate::debug!("the real-time governor runs at its default policy: {e}");
    }
    let mut ticked: Option<Instant> = None;
    loop {
        let suspects = {
            let mut watched = lock(&WATCHED);
            while watched.is_empty() {
                watched = wait(&ADDED, watched);
            }
            let now = Instant::now();
            let every = ticked.is_none_or(|t| now.saturating_duration_since(t) >= TICK);
            if every {
                ticked = Some(now);
            }
            // Under the lock, which a thread takes to leave the watch before it ends: no
            // port is looked at once its thread is gone.
            watched.retain_mut(|w| !(every || w.suspected.is_some()) || look(w, now));
            watched.iter().any(|w| w.suspected.is_some())
        };
        let next = ticked.map_or(TICK, |t| TICK.saturating_sub(t.elapsed()));
        // A suspect is looked at as finely as the host wakes, the cold pair's constraint, so
        // that a look lands in one of its waits where it waits at all (M317's rule): its own
        // constraint, twice a batch of up to 50 ms, would look too seldom to see one.
        let suspect_look = cold().map(|pair| pair.constraint);
        std::thread::sleep(match (suspects, suspect_look) {
            (true, Ok(look)) => next.min(look),
            _ => next,
        });
    }
}

/// One look at a watched thread at `now`: real-time again if it waits; suspected once it
/// has run a whole tick since it was last seen waiting, and at timeshare if a tick of
/// looks since has found it running at every one, or the fail-safe has it already. False
/// once its thread is gone.
fn look(w: &mut Watched, now: Instant) -> bool {
    let Some(info) = info(w.port) else {
        return false;
    };
    let ran = info.pth_user_time.saturating_add(info.pth_system_time);
    let tick = u64::try_from(TICK.as_nanos()).unwrap_or(u64::MAX);
    if info.pth_run_state == libc::TH_STATE_WAITING || info.pth_run_state == libc::TH_STATE_UNINTERRUPTIBLE {
        // What it ran since it was last seen waiting: its batch's bound, while it was
        // real-time (a span at timeshare is no real-time run's).
        if w.realtime {
            w.batch = learned(w.batch, Duration::from_nanos(ran.saturating_sub(w.waited_at)));
        }
        w.waited_at = ran;
        w.suspected = None;
        let pair = w.batch.map_or(Ok(w.asked), constraint_of);
        match pair {
            Ok(pair) if !w.realtime || pair != w.asked => match set_realtime(w.port, pair) {
                Ok(()) => {
                    w.realtime = true;
                    w.asked = pair;
                }
                Err(e) => crate::debug!("a thread waiting stays at its policy: {e}"),
            },
            Ok(_) => {}
            Err(e) => crate::debug!("a thread waiting stays at its policy: {e}"),
        }
        return true;
    }
    if !w.realtime {
        return true;
    }
    let moved = info.pth_policy == POLICY_TIMESHARE
        || w.suspected
            .is_some_and(|since| now.saturating_duration_since(since) >= TICK);
    if moved {
        match set_timeshare(w.port) {
            Ok(()) => {
                w.realtime = false;
                w.suspected = None;
            }
            Err(e) => crate::debug!("a thread run long stays real-time: {e}"),
        }
    } else if w.suspected.is_none() && ran.saturating_sub(w.waited_at) >= tick {
        w.suspected = Some(now);
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A thread's pair is its own batch's (platform-11): its computation the batch held to
    /// XNU's 50 µs to 50 ms, its constraint twice it (D191 at half a CPU); never M10's fixed
    /// 0.5 and 1 ms whatever it runs.
    #[test]
    fn a_threads_pair_is_its_batchs() {
        let pair = |c, k| {
            Ok(Constraint {
                computation: c,
                constraint: k,
            })
        };
        let us = Duration::from_micros;
        assert_eq!(constraint_of(us(3_000)), pair(us(3_000), us(6_000)));
        assert_eq!(constraint_of(us(120)), pair(us(120), us(240)));
        assert_eq!(constraint_of(us(7)), pair(MIN_RT_QUANTUM, 2 * MIN_RT_QUANTUM));
        assert_eq!(
            constraint_of(Duration::from_secs(1)),
            pair(MAX_RT_QUANTUM, 2 * MAX_RT_QUANTUM)
        );
        // Cold: the host's measured wake, held to XNU's least.
        let cold = cold().unwrap();
        assert!(cold.computation >= MIN_RT_QUANTUM && cold.constraint == 2 * cold.computation);
        assert_eq!(cold, constraint_of(granule::step().late.unwrap()).unwrap());
    }

    /// A thread's batch is the most it was seen to run between two waiting looks; a span of
    /// none teaches nothing.
    #[test]
    fn a_batch_is_the_most_run_between_two_waits() {
        let ms = Duration::from_millis;
        assert_eq!(learned(None, Duration::ZERO), None);
        assert_eq!(learned(None, ms(2)), Some(ms(2)));
        assert_eq!(learned(Some(ms(2)), ms(1)), Some(ms(2)));
        assert_eq!(learned(Some(ms(2)), ms(5)), Some(ms(5)));
        assert_eq!(learned(Some(ms(5)), Duration::ZERO), Some(ms(5)));
    }

    /// The calling thread's CPU time.
    fn ran_here() -> Duration {
        // SAFETY: the calling thread's own port, which lives as long as it does.
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        info(port).map_or(Duration::ZERO, |i| {
            Duration::from_nanos(i.pth_user_time + i.pth_system_time)
        })
    }

    /// Spins until the calling thread has run `cpu` of its own time, or `wall` has passed:
    /// what it runs, not the clock, however many threads share its host's CPUs.
    fn spin_for(cpu: Duration, wall: Duration) {
        let until = Instant::now() + wall;
        while ran_here() < cpu && Instant::now() < until {
            let chunk = Instant::now() + Duration::from_millis(1);
            while Instant::now() < chunk {
                std::hint::spin_loop();
            }
        }
    }

    /// What thread_info says of a thread until `done` says it has finished, looked at every
    /// 10 ms: at each look, the seconds since the first, its policy, its base priority and
    /// its CPU time.
    fn watch_until(port: libc::mach_port_t, done: &mpsc::Receiver<()>) -> Vec<(f64, i32, i32, Duration)> {
        let start = Instant::now();
        let mut seen = Vec::new();
        while done.try_recv().is_err() {
            if let Some(i) = info(port) {
                let ran = Duration::from_nanos(i.pth_user_time + i.pth_system_time);
                seen.push((start.elapsed().as_secs_f64(), i.pth_policy, i.pth_priority, ran));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        seen
    }

    /// A real-time thread that runs without waiting goes to timeshare, at a program's
    /// priority, before XNU's fail-safe would depress it to priority 0 after a second's
    /// run: as it runs on, and never depressed.
    #[test]
    fn a_thread_run_long_goes_to_timeshare_before_the_fail_safe_depresses_it() {
        let (tx, rx) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let spinner = std::thread::spawn(move || {
            let watch = make_current_realtime().unwrap();
            tx.send(watch.port).unwrap();
            spin_for(Duration::from_millis(1600), Duration::from_secs(30));
            done.send(()).unwrap();
        });
        let port = rx.recv().unwrap();
        let seen = watch_until(port, &finished);
        spinner.join().unwrap();
        let moved = seen.iter().find(|s| s.1 == POLICY_TIMESHARE).map(|s| s.3);
        assert!(
            moved.is_some_and(|ran| ran < FAIL_SAFE),
            "not at timeshare before a second's run: {seen:?}"
        );
        assert!(seen.iter().all(|s| s.2 > 0), "depressed to priority 0: {seen:?}");
    }

    /// One moved to timeshare is real-time again once it waits.
    #[test]
    fn a_thread_at_timeshare_is_real_time_again_once_it_waits() {
        let (tx, rx) = mpsc::channel();
        let (spun, spun_rx) = mpsc::channel();
        let (go, wait_for_go) = mpsc::channel::<()>();
        let spinner = std::thread::spawn(move || {
            let watch = make_current_realtime().unwrap();
            tx.send(watch.port).unwrap();
            spin_for(Duration::from_millis(700), Duration::from_secs(30));
            spun.send(()).unwrap();
            let _ = wait_for_go.recv();
        });
        let port = rx.recv().unwrap();
        let seen = watch_until(port, &spun_rx);
        let moved = seen.iter().any(|s| s.1 == POLICY_TIMESHARE);
        // Waiting now: real-time again at the governor's next look.
        let start = Instant::now();
        let mut back = false;
        while !back && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
            back = info(port).is_some_and(|i| i.pth_policy != POLICY_TIMESHARE);
        }
        go.send(()).unwrap();
        spinner.join().unwrap();
        assert!(moved, "never at timeshare while it ran: {seen:?}");
        assert!(back, "not real-time again as it waited: {seen:?}");
    }

    /// One that runs most of its time but waits between its runs, as an I/O-bound vCPU
    /// waits on its device's answers, stays real-time: 4 ms run, 1 ms waited, for a second
    /// of its own time.
    #[test]
    fn a_thread_that_waits_between_its_runs_stays_real_time() {
        let (tx, rx) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let watch = make_current_realtime().unwrap();
            tx.send(watch.port).unwrap();
            let until = Instant::now() + Duration::from_secs(30);
            while ran_here() < Duration::from_secs(1) && Instant::now() < until {
                let run = Instant::now() + Duration::from_millis(4);
                while Instant::now() < run {
                    std::hint::spin_loop();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            done.send(()).unwrap();
        });
        let port = rx.recv().unwrap();
        let seen = watch_until(port, &finished);
        worker.join().unwrap();
        assert!(
            seen.iter().all(|s| s.1 != POLICY_TIMESHARE),
            "moved to timeshare though it waited between its runs: {seen:?}"
        );
    }
}
