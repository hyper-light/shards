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
//! that has run a whole tick since it was last seen waiting is looked at every millisecond,
//! the constraint its policy asks for, for a tick more: found waiting at any look, it was
//! waiting between its runs, as an I/O-bound vCPU or a device worker does, and stays; found
//! running at every one, it goes to timeshare, as any program's thread runs, and is
//! real-time again once it is seen waiting. One so moved is at most three ticks into the
//! fail-safe's second, and a busy guest's vCPUs share the host's CPUs as its other programs
//! do. Moved at the first tick found running instead, a block device's vCPUs, waiting on
//! its answers half their time, were moved too and read fewer than with no governor
//! (PM M317).

use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::sync::{lock, wait};

/// CPU time a vCPU may use per scheduling period before XNU's real-time fail-safe
/// demotes it, and the latency window it asks for. These are the values measured in M10.
const COMPUTATION_NS: u64 = 500_000;
const CONSTRAINT_NS: u64 = 1_000_000;

/// How often the governor looks: XNU's scheduler tick, at which it ages timeshare threads'
/// usage (osfmk/kern/sched.h, `SCHED_TICK_SHIFT`: a second shifted right by 3).
const TICK: Duration = Duration::from_millis(1000 >> 3);
/// What XNU's fail-safe lets a real-time thread run without blocking: 100 quanta
/// (sched_prim.c, `MAX_UNSAFE_RT_QUANTA`) of 10 ms (`DEFAULT_PREEMPTION_RATE`, 100 a
/// second).
const FAIL_SAFE: Duration = Duration::from_millis(100 * 10);
/// How often a thread suspected of running without a wait is looked at: the constraint its
/// policy asks for.
const SUSPECT_LOOK: Duration = Duration::from_nanos(CONSTRAINT_NS);
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
    /// waiting since: it is looked at every [`SUSPECT_LOOK`] until a tick from then.
    suspected: Option<Instant>,
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
    set_realtime(port)?;
    lock(&WATCHED).push(Watched {
        port,
        // As if last seen waiting as it began, its time before now counted.
        waited_at: 0,
        realtime: true,
        suspected: None,
    });
    ADDED.notify_one();
    Ok(Watch {
        port,
        _thread: PhantomData,
    })
}

/// The time-constraint policy, for the thread `port` names.
fn set_realtime(port: libc::mach_port_t) -> Result<(), String> {
    let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: a struct for mach_timebase_info to fill.
    if unsafe { mach_timebase_info(&raw mut tb) } != 0 || tb.numer == 0 {
        return Err("mach_timebase_info failed".into());
    }
    let to_abs = |ns: u64| {
        ns.checked_mul(u64::from(tb.denom))
            .map(|v| v / u64::from(tb.numer))
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("thread policy interval out of range")
    };
    let mut policy = libc::thread_time_constraint_policy {
        period: 0,
        computation: to_abs(COMPUTATION_NS)?,
        constraint: to_abs(CONSTRAINT_NS)?,
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
/// every [`SUSPECT_LOOK`] at those suspected of running without a wait; while it watches
/// none, a wait for one.
fn govern() {
    // It looks on time however busy the threads it watches keep the CPUs: at their
    // policy, which its sleeps between looks keep it clear of the fail-safe in.
    // SAFETY: the calling thread's own port, which lives as long as it does.
    if let Err(e) = set_realtime(unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }) {
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
        std::thread::sleep(if suspects { next.min(SUSPECT_LOOK) } else { next });
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
        w.waited_at = ran;
        w.suspected = None;
        if !w.realtime {
            match set_realtime(w.port) {
                Ok(()) => w.realtime = true,
                Err(e) => crate::debug!("a thread waiting stays at timeshare: {e}"),
            }
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
mod tests {
    use super::*;
    use std::sync::mpsc;

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
