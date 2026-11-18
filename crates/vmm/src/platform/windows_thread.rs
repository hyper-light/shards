//! vCPU and device thread priority on Windows (D191), measured on a contended host as Linux's
//! slice was (PM M440) and kept for what it showed: on windows-2025's 4 CPUs that 4 busy
//! loops held, a run's start reached its command 8.2 ms after its request at p50 where 217
//! without it, and 10.1 where 337 again (PM M495).
//!
//! Windows runs the ready threads of the highest priority, in turn, a time slice each; "if a
//! higher-priority thread becomes available to run, the system ceases to execute the
//! lower-priority thread (without allowing it to finish using its time slice)" (Microsoft,
//! Scheduling Priorities). On a Windows client the hypervisor leaves its guests' virtual
//! processors to the root's NT scheduler (the root scheduler, client systems' only one since
//! Windows 10 1803; Microsoft, Manage Hyper-V hypervisor scheduler types), so a WHP vCPU runs
//! as the thread that runs it does. A vCPU another wakes, at the priority of the host's
//! busy threads, may wait for one to finish its slice, as Linux's waited (PM M440), unless
//! the boost Windows gives a thread whose wait is satisfied (Microsoft, Priority Boosts)
//! lifts it past them already: it does not (PM M495).
//!
//! So each such thread runs at THREAD_PRIORITY_ABOVE_NORMAL: base priority 9 in a
//! NORMAL_PRIORITY_CLASS process, against its host's threads' 8, the least step above them,
//! within any user's rights, and below the 11 past which a thread "interferes with the normal
//! operation of the operating system" (Microsoft, SetThreadPriority). Not MMCSS, which moves
//! a thread into the real-time range (16 to 26, Multimedia Class Scheduler Service), nor
//! power throttling, which changes the cores and clock a thread runs at, "while scheduling
//! priority remains the main metric by which the system determines which thread to schedule
//! next" (Quality of Service).
//!
//! A priority is strict where Linux's slice kept a thread's share its weight's: a raised
//! thread that runs on takes its CPU from every thread below it, and Microsoft keeps raised
//! priorities for threads that respond and lowers processor-intensive ones (Scheduling
//! Priorities). So a governor watches each raised thread: every [`period`] it reads the CPU
//! time the thread used since its last look (GetThreadTimes, as the hold reads its vCPUs',
//! D191) and keeps its credit: half of the time that passed, less what it used, never more
//! than a period nor owing more than one (`granule::credited`). A thread out of credit goes
//! back to THREAD_PRIORITY_NORMAL, to share its host's CPUs as before, and is raised again
//! once a look finds its debt paid and some credit left. A raised thread so takes from its
//! host's threads half a CPU over any long stretch, and two periods more at most (its credit,
//! and what it runs past it before the next look, owed until paid): what a fair scheduler
//! gives one thread beside one busy thread of its weight (Linux's slice, `run_promptly`).
//! Kept to none, as it was until 2026-11-18, a credit forgave what a look ran past it, and a
//! thread raised on the few nanoseconds of credit that looks a little after their period
//! leave took two looks in three (D191).
//!
//! The period is D191's for one thread held to half a CPU (`granule::period`): 1 × g over a
//! half, 2g, g what this process has learned of its governors' step, its thread clock's
//! (GetThreadTimes' 15.625 ms, PM M294) where that is the coarser. Cold, the governor measures
//! both as it starts, before its first look (`granule::measure`: one shortest wait, and two of
//! its clock's steps of its own CPU time); a thread raised before that is credited at a g of
//! `granule::LEAST_WAIT`, a 2 ms period, so small a credit that it may go back to normal until
//! the first look: less authority, never more.
//!
//! A thread's half bounds what it takes alone, not what raised threads take together: three
//! that share one CPU each run a third of it, each under its half, and keep it from the
//! host's threads between them (platform-10). So the raised threads that may run on one set
//! of CPUs (GetThreadGroupAffinity: a processor group and a mask in it, read at each look)
//! also share a pool, credited half of each of those CPUs' time and charged what they ran
//! raised, kept to a period of each either way: a thread is raised only while its own credit
//! and its set's pool both have some. Raised threads so take half of the CPUs they run on
//! over any long stretch, and two periods of each more at most, however many share them. A
//! pool is the governor's, one a process: the raised threads of other processes (other VM
//! processes on the host) draw on pools of their own, which no owner here sees (owed, D191).
//!
//! The credit is what keeps a thread that waits raised, as GetThreadTimes counts: in whole
//! steps of its clock, 15.625 ms (PM M294), each charged to the thread that runs as it
//! ticks. Judged by each period alone, a thread that ran 1 ms of every 6 (17 ms of 100 at
//! most) was charged 4 steps, 62.5 ms, of one 100 ms period, more than its half, and went
//! back to normal for a period (`a_thread_that_waits_between_its_runs_stays_raised` on CI's
//! windows-2025, 2026-11-06: 187.5 ms, 12 steps, in its second). With its credit, what it
//! does not use of each period's half carries over. Its credit holds the most one look can
//! charge a thread, the whole period of its one CPU (every step of the period landing on its
//! runs), so that no single look's steps put a thread that keeps to its half back to normal:
//! at 2g, a look of two steps charged both.

use std::io;
use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, HANDLE};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetThreadGroupAffinity, GetThreadPriority,
    SetThreadPriority, THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::WindowsProgramming::THREAD_PRIORITY_ERROR_RETURN;

use super::granule;
use crate::sync::{lock, wait};

/// The priority a watched thread runs at while it has credit.
const RAISED: THREAD_PRIORITY = THREAD_PRIORITY_ABOVE_NORMAL;

/// The time over which the governor counts each thread's use at `g`: D191's period for one
/// thread held to half a CPU (`granule::HALF_CPU`), 2g (`granule::period`).
fn period_at(g: Duration) -> Result<Duration, granule::Outgrown> {
    granule::period(1, granule::HALF_CPU, g)
}

/// [`period_at`] g as this process knows it now (`granule::g`).
fn period() -> Result<Duration, granule::Outgrown> {
    period_at(granule::g())
}

/// The CPUs a thread may run on: a processor group, and its mask of them in it
/// (GetThreadGroupAffinity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cpus {
    group: u16,
    mask: usize,
}

impl Cpus {
    /// How many CPUs they are.
    fn count(self) -> u64 {
        u64::from(self.mask.count_ones())
    }

    /// The CPUs the thread `handle` names may run on now.
    fn of(handle: HANDLE) -> Result<Cpus, String> {
        let mut affinity = GROUP_AFFINITY::default();
        // SAFETY: a thread handle with THREAD_QUERY_LIMITED_INFORMATION, as
        // GetThreadGroupAffinity needs, and a structure for it to fill.
        if unsafe { GetThreadGroupAffinity(handle, &raw mut affinity) } == 0 {
            return Err(format!("GetThreadGroupAffinity: {}", io::Error::last_os_error()));
        }
        Ok(Cpus {
            group: affinity.Group,
            mask: affinity.Mask,
        })
    }
}

/// The credit the raised threads that may run on `cpus` share, in nanoseconds, and when it
/// was last given its due: [`granule::credited`] over the set's CPUs.
struct Pool {
    cpus: Cpus,
    credit: i64,
    at: Instant,
}

/// A thread the governor watches.
struct Watched {
    /// Its thread's ID, by which its [`Watch`] takes it from the watch.
    id: u32,
    /// A handle of its thread's, to read its times and set its priority.
    handle: HANDLE,
    /// Its CPU time, in nanoseconds, and when the governor read it last.
    ran: u64,
    at: Instant,
    /// Its credit, in nanoseconds, a period's as it is watched: half of each stretch between
    /// looks added and the CPU time it used taken, below none what it owes
    /// ([`granule::credited`]).
    credit: i64,
    /// Whether the governor has it at [`RAISED`].
    raised: bool,
    /// The CPUs it may run on, as last read: its pool's.
    cpus: Cpus,
}

// SAFETY: a handle of a thread of this process, opened to read its times and set its
// priority alone, which any thread may do; only the governor's lock reaches it, and it is
// closed once, as its entry goes.
unsafe impl Send for Watched {}

impl Drop for Watched {
    fn drop(&mut self) {
        // SAFETY: the handle this entry duplicated, closed once.
        unsafe { CloseHandle(self.handle) };
    }
}

/// The watched threads, and their sets of CPUs' pools.
struct Governed {
    threads: Vec<Watched>,
    pools: Vec<Pool>,
}

static WATCHED: Mutex<Governed> = Mutex::new(Governed {
    threads: Vec::new(),
    pools: Vec::new(),
});
/// The governor waits on it while it watches no thread.
static ADDED: Condvar = Condvar::new();
static GOVERNOR: OnceLock<Result<(), String>> = OnceLock::new();

/// The calling thread raised while the governor finds it credit: while it uses no more
/// than half of its CPU, by a period over any stretch.
/// Held by the thread while it runs, and dropped before it ends, which takes it from the
/// governor's watch and sets it back to THREAD_PRIORITY_NORMAL.
#[derive(Debug)]
pub struct Watch {
    id: u32,
    /// Its own thread's: it neither moves nor is shared.
    _thread: PhantomData<*const ()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        lock(&WATCHED).threads.retain(|w| w.id != self.id);
        // SAFETY: the calling thread's pseudo-handle, which is not closed: a Watch is dropped
        // on its own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
    }
}

/// Raises the calling thread to [`RAISED`] and has the governor watch it ([`Watch`]). A
/// thread its program or its user set at another priority is left at it, unwatched: `None`.
/// Refused, the thread left as it is, where no period is derived ([`granule::Outgrown`]).
pub fn raise_current() -> Result<Option<Watch>, String> {
    GOVERNOR.get_or_init(start_governor).clone()?;
    let period = period().map_err(|e| format!("the priority governor's period: {e}"))?;
    // SAFETY: the calling thread's pseudo-handle, which is not closed.
    let current = unsafe { GetThreadPriority(GetCurrentThread()) };
    if u32::try_from(current).is_ok_and(|p| p == THREAD_PRIORITY_ERROR_RETURN) {
        return Err(format!("GetThreadPriority: {}", io::Error::last_os_error()));
    }
    if current != THREAD_PRIORITY_NORMAL {
        return Ok(None);
    }
    // A handle the governor may use, from the thread's pseudo-handle (GetCurrentThread: "A
    // thread can create a 'real' handle to itself that can be used by other threads").
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's and thread's pseudo-handles, and a handle to write.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &raw mut handle,
            THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_LIMITED_INFORMATION,
            0,
            0,
        )
    };
    if duplicated == 0 {
        return Err(format!("DuplicateHandle: {}", io::Error::last_os_error()));
    }
    // SAFETY: no preconditions.
    let id = unsafe { GetCurrentThreadId() };
    let mut watched = Watched {
        id,
        handle,
        ran: 0,
        at: Instant::now(),
        credit: granule::most_credit(period, 1),
        raised: false,
        cpus: Cpus { group: 0, mask: 0 },
    };
    watched.ran = super::windows::thread_time(handle).map_err(|e| format!("GetThreadTimes: {e}"))?;
    watched.cpus = Cpus::of(handle)?;
    let mut governed = lock(&WATCHED);
    // Raised only where its CPUs' pool has credit, as each look raises it.
    if governed.pooled(watched.cpus, watched.at, 0, period) > 0 {
        set(handle, RAISED)?;
        watched.raised = true;
    }
    governed.threads.push(watched);
    drop(governed);
    ADDED.notify_one();
    Ok(Some(Watch {
        id,
        _thread: PhantomData,
    }))
}

/// Sets the thread `handle` names, of this process, at `priority`.
fn set(handle: HANDLE, priority: THREAD_PRIORITY) -> Result<(), String> {
    // SAFETY: a thread handle with THREAD_SET_LIMITED_INFORMATION, as SetThreadPriority needs.
    if unsafe { SetThreadPriority(handle, priority) } == 0 {
        return Err(format!(
            "SetThreadPriority({priority}): {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn start_governor() -> Result<(), String> {
    std::thread::Builder::new()
        .name("priority-governor".into())
        .spawn(govern)
        .map(drop)
        .map_err(|e| format!("spawning the priority governor: {e}"))
}

/// Once a period, for as long as the process runs, a look at every thread it watches; while
/// it watches none, a wait for one. Each period derived as its look begins, from g as the
/// process knows it then; each wait's lateness taught to it (`granule::LATE`).
fn govern() {
    // Its wake and its thread clock's step, g's, measured before it is raised: one shortest
    // wait, and two of its clock's steps of its own CPU time at most.
    if let Err(e) = granule::measure() {
        crate::debug!("the priority governor knows no clock step: {}", e.0);
    }
    // It looks on time however busy the threads it raises keep the CPUs: above them, as the
    // hold's governor runs above its vCPUs (PM M294), for the moment a look takes.
    if let Err(e) = super::windows::run_time_critical() {
        crate::debug!("the priority governor runs at its default priority: {e}");
    }
    loop {
        let period = {
            let mut governed = lock(&WATCHED);
            while governed.threads.is_empty() {
                governed = wait(&ADDED, governed);
            }
            // Under the lock, which a thread takes to leave the watch before it ends: no
            // thread is set once its Watch is gone.
            match period() {
                Ok(period) => {
                    governed.look(Instant::now(), period);
                    period
                }
                // No period, no look: every thread back to normal and let go of, as no
                // other is raised (`raise_current`).
                Err(e) => {
                    crate::warn!("the priority governor: {e}");
                    for w in &mut governed.threads {
                        raise(w, false);
                    }
                    governed.threads.clear();
                    governed.pools.clear();
                    continue;
                }
            }
        };
        let began = Instant::now();
        std::thread::sleep(period);
        granule::LATE.waited(period, began);
    }
}

impl Governed {
    /// The credit of the pool of the threads that may run on `cpus`, once given its due at
    /// `now` and charged `used` ([`granule::credited`], a thread alone one CPU's): made full
    /// where there was none, as a thread's own credit starts.
    fn pooled(&mut self, cpus: Cpus, now: Instant, used: u64, period: Duration) -> i64 {
        let count = cpus.count();
        match self.pools.iter_mut().find(|p| p.cpus == cpus) {
            Some(pool) => {
                let passed = now.saturating_duration_since(pool.at);
                pool.credit = granule::credited(pool.credit, passed, used, count, period);
                pool.at = now;
                pool.credit
            }
            None => {
                let full = granule::most_credit(period, count);
                let credit = granule::credited(full, Duration::ZERO, used, count, period);
                self.pools.push(Pool {
                    cpus,
                    credit,
                    at: now,
                });
                credit
            }
        }
    }

    /// One look at every watched thread at `now`, over `period`: each charged what it used
    /// since its last ([`charge`]), and its CPUs' pool what it ran raised; then each raised
    /// where its own credit and its pool's both have some, else back at
    /// THREAD_PRIORITY_NORMAL.
    fn look(&mut self, now: Instant, period: Duration) {
        // Each set of CPUs a thread may run on, and what its threads ran raised.
        let mut sets: Vec<(Cpus, u64)> = Vec::new();
        for w in &mut self.threads {
            let used = charge(w, now, period).unwrap_or(0);
            match sets.iter_mut().find(|(cpus, _)| *cpus == w.cpus) {
                Some((_, sum)) => *sum = sum.saturating_add(used),
                None => sets.push((w.cpus, used)),
            }
        }
        self.pools
            .retain(|p| sets.iter().any(|(cpus, _)| *cpus == p.cpus));
        let credits: Vec<(Cpus, i64)> = sets
            .into_iter()
            .map(|(cpus, used)| (cpus, self.pooled(cpus, now, used, period)))
            .collect();
        for w in &mut self.threads {
            let pooled = credits
                .iter()
                .any(|(cpus, credit)| *cpus == w.cpus && *credit > 0);
            raise(w, w.credit > 0 && pooled);
        }
    }
}

/// A watched thread's look at `now`, once a `period` has passed since its last: its credit
/// given half of the time since and charged the CPU time it used, and its CPUs read again.
/// What it ran raised, its pool's to be charged: `None` where no look was due or its time is
/// unread.
fn charge(w: &mut Watched, now: Instant, period: Duration) -> Option<u64> {
    let passed = now.saturating_duration_since(w.at);
    if passed < period {
        return None;
    }
    let ran = match super::windows::thread_time(w.handle) {
        Ok(ran) => ran,
        Err(e) => {
            crate::debug!("a watched thread's time is unread: {e}");
            return None;
        }
    };
    let used = ran.saturating_sub(w.ran);
    w.ran = ran;
    w.at = now;
    w.credit = granule::credited(w.credit, passed, used, 1, period);
    match Cpus::of(w.handle) {
        Ok(cpus) => w.cpus = cpus,
        Err(e) => crate::debug!("a watched thread's CPUs are those last read: {e}"),
    }
    Some(if w.raised { used } else { 0 })
}

/// Sets a watched thread at [`RAISED`] where `raise`, else at THREAD_PRIORITY_NORMAL, where it
/// is not at it already.
fn raise(w: &mut Watched, raise: bool) {
    if raise != w.raised {
        let priority = if raise { RAISED } else { THREAD_PRIORITY_NORMAL };
        match set(w.handle, priority) {
            Ok(()) => w.raised = raise,
            Err(e) => crate::debug!("a watched thread stays at its priority: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ThreadClock;
    use std::sync::mpsc;
    use windows_sys::Win32::System::Threading::{OpenThread, THREAD_PRIORITY_BELOW_NORMAL};

    /// The priority of thread `id` of this process, read from another thread.
    fn priority_of(id: u32) -> i32 {
        // SAFETY: opens thread `id` to query it alone.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id) };
        assert!(!handle.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: the handle just opened, with query access.
        let priority = unsafe { GetThreadPriority(handle) };
        // SAFETY: the handle just opened, closed once.
        unsafe { CloseHandle(handle) };
        priority
    }

    /// Spins until `stop` says to or `wall` has passed.
    fn spin_until(stop: &mpsc::Receiver<()>, wall: Duration) {
        let until = Instant::now() + wall;
        while stop.try_recv().is_err() && Instant::now() < until {
            std::hint::spin_loop();
        }
    }

    /// A raised thread that runs on goes back to normal, and is raised again once it waits.
    #[test]
    fn a_thread_run_long_goes_back_to_normal_and_is_raised_again_as_it_waits() {
        let (tx, rx) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let (go, wait_for_go) = mpsc::channel::<()>();
        let spinner = std::thread::spawn(move || {
            let watch = raise_current().unwrap().unwrap();
            // SAFETY: no preconditions.
            tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            spin_until(&stopped, Duration::from_secs(30));
            let _ = wait_for_go.recv();
            drop(watch);
        });
        let id = rx.recv().unwrap();
        let first = priority_of(id);
        let start = Instant::now();
        let mut seen = vec![first];
        while *seen.last().unwrap() != THREAD_PRIORITY_NORMAL && start.elapsed() < Duration::from_secs(25) {
            std::thread::sleep(Duration::from_millis(10));
            seen.push(priority_of(id));
        }
        let moved = *seen.last().unwrap() == THREAD_PRIORITY_NORMAL;
        stop.send(()).unwrap();
        // Waiting now: raised again within a few periods.
        let start = Instant::now();
        let mut back = false;
        while !back && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
            back = priority_of(id) == RAISED;
        }
        go.send(()).unwrap();
        spinner.join().unwrap();
        assert_eq!(first, RAISED, "{seen:?}");
        assert!(moved, "never back to normal while it ran: {seen:?}");
        assert!(back, "not raised again as it waited: {seen:?}");
    }

    /// One that waits between short runs, as a vCPU between its guest's wakeups, stays
    /// raised: 1 ms run, then a sleep, for a second; and is normal again once it lets go.
    #[test]
    fn a_thread_that_waits_between_its_runs_stays_raised() {
        let (tx, rx) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let (go, wait_for_go) = mpsc::channel::<()>();
        let (dropped, let_go) = mpsc::channel();
        let (end, wait_for_end) = mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let watch = raise_current().unwrap().unwrap();
            let clock = ThreadClock::current().unwrap();
            // SAFETY: no preconditions.
            tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until {
                let run = Instant::now() + Duration::from_millis(1);
                while Instant::now() < run {
                    std::hint::spin_loop();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            done.send(clock.read().unwrap()).unwrap();
            let _ = wait_for_go.recv();
            drop(watch);
            dropped.send(()).unwrap();
            let _ = wait_for_end.recv();
        });
        let id = rx.recv().unwrap();
        let mut seen = Vec::new();
        let ran = loop {
            seen.push(priority_of(id));
            if let Ok(ran) = finished.try_recv() {
                break ran;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        go.send(()).unwrap();
        let_go.recv().unwrap();
        let after = priority_of(id);
        end.send(()).unwrap();
        worker.join().unwrap();
        assert!(seen.iter().all(|p| *p == RAISED), "{seen:?}, {ran} ns run");
        assert_eq!(after, THREAD_PRIORITY_NORMAL);
    }

    /// GetThreadTimes' step (PM M294), as a test's g.
    const STEP: u64 = 15_625_000;

    /// The governor's look is D191's period for one thread at half a CPU, 2g (platform-11):
    /// 31.25 ms at GetThreadTimes' step, 2 ms cold, never a fixed 100 ms.
    #[test]
    fn the_look_is_d191s_period_for_half_a_cpu() {
        assert_eq!(
            period_at(Duration::from_nanos(STEP)),
            Ok(Duration::from_nanos(2 * STEP))
        );
        assert_eq!(period_at(granule::LEAST_WAIT), Ok(2 * granule::LEAST_WAIT));
        assert!(period_at(Duration::MAX).is_err());
        assert!(period().unwrap() >= 2 * granule::LEAST_WAIT);
    }

    /// One look can charge a thread its whole period, every step of it landing on its runs: a
    /// thread with its credit keeps some, and stays raised, as one at half a period's credit
    /// did not; one that spins two periods through, charged all of each, has none.
    #[test]
    fn a_look_charged_its_whole_period_keeps_a_thread_raised() {
        let period = Duration::from_nanos(2 * STEP);
        let full = granule::most_credit(period, 1);
        assert_eq!(full, i64::try_from(2 * STEP).unwrap());
        let once = granule::credited(full, period, 2 * STEP, 1, period);
        assert!(once > 0, "out of credit at one look's steps");
        assert_eq!(granule::credited(once, period, 2 * STEP, 1, period), 0);
    }

    /// What CI's clock charged a thread that ran 1 ms of every 6: whole 15.625 ms steps
    /// (PM M294), none to two a 2g look and two in two looks running. It keeps credit
    /// through them; one charged a step more than half of each look's runs out of it.
    #[test]
    fn credit_carries_a_thread_that_waits_across_its_clocks_steps() {
        let period = Duration::from_nanos(2 * STEP);
        let mut credit = granule::most_credit(period, 1);
        for steps in [1, 0, 1, 2, 0, 1, 0, 2, 0, 1, 1, 0, 1] {
            credit = granule::credited(credit, period, steps * STEP, 1, period);
            assert!(credit > 0, "out of credit at {steps} steps");
        }
        let (mut credit, mut looks) = (granule::most_credit(period, 1), 0);
        while credit > 0 {
            credit = granule::credited(credit, period, 2 * STEP, 1, period);
            looks += 1;
        }
        assert_eq!(looks, 2);
    }

    /// The highest CPU this process may run on, as a mask of it alone.
    fn last_cpu() -> usize {
        use windows_sys::Win32::System::Threading::GetProcessAffinityMask;
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: this process's pseudo-handle, and two words for the masks.
        let ok = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &raw mut process, &raw mut system) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        assert_ne!(process, 0);
        1 << (usize::BITS - 1 - process.leading_zeros())
    }

    /// Pins the calling thread to the CPUs `mask` names.
    fn pin(mask: usize) {
        use windows_sys::Win32::System::Threading::SetThreadAffinityMask;
        // SAFETY: the calling thread's pseudo-handle.
        assert_ne!(unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) }, 0);
    }

    /// Do: raise three threads pinned to one CPU, each spinning, beside a thread at normal
    /// priority pinned there and spinning too, for 3 s. Expect each raised thread seen back
    /// at normal, and the normal one to have run: three raised threads each took a third of
    /// the CPU, under the half each is credited, so all three kept their credit and the CPU,
    /// which a thread of normal priority, a host's, then had only as Windows rescues a thread
    /// it has kept from running for seconds (platform-10).
    #[test]
    fn raised_threads_sharing_a_cpu_take_no_more_than_half_of_it() {
        const RAISED_THREADS: usize = 3;
        let cpu = last_cpu();
        let (ids, said) = mpsc::channel();
        let spinning = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let raised: Vec<_> = (0..RAISED_THREADS)
            .map(|_| {
                let (ids, spinning) = (ids.clone(), spinning.clone());
                std::thread::spawn(move || {
                    pin(cpu);
                    let watch = raise_current().unwrap().unwrap();
                    // SAFETY: no preconditions.
                    ids.send(unsafe { GetCurrentThreadId() }).unwrap();
                    while spinning.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                    drop(watch);
                })
            })
            .collect();
        drop(ids);
        let ids: Vec<u32> = said.iter().take(RAISED_THREADS).collect();
        let normal = {
            let spinning = spinning.clone();
            std::thread::spawn(move || {
                pin(cpu);
                let clock = ThreadClock::current().unwrap();
                while spinning.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
                clock.read().unwrap()
            })
        };
        let mut lowered = [false; RAISED_THREADS];
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            for (seen, id) in lowered.iter_mut().zip(&ids) {
                *seen |= priority_of(*id) == THREAD_PRIORITY_NORMAL;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        spinning.store(false, std::sync::atomic::Ordering::Relaxed);
        let host_ran = normal.join().unwrap();
        for thread in raised {
            thread.join().unwrap();
        }
        assert_eq!(
            lowered, [true; RAISED_THREADS],
            "raised throughout; the normal thread ran {host_ran} ns"
        );
        assert!(host_ran > 0, "the normal thread never ran");
    }

    /// A set of CPUs' pool is credited half of each CPU's time and kept to a period of each:
    /// three threads of one CPU running all of it between them drain it, at a third of it
    /// each, under what each alone is credited; on two CPUs they take three quarters of
    /// them, and drain it too, as two would not.
    #[test]
    fn a_pool_is_half_of_its_cpus_time() {
        let period = Duration::from_nanos(2 * STEP);
        // A period of one CPU's time, as what a look charges and as a credit.
        let (whole, full) = (2 * STEP, granule::most_credit(period, 1));
        let mut own = full;
        for _ in 0..4 {
            own = granule::credited(own, period, whole / 3, 1, period);
            assert!(own > 0, "each thread's own credit, at a third of the CPU");
        }
        let mut pool = full;
        for _ in 0..2 {
            pool = granule::credited(pool, period, whole, 1, period);
        }
        assert_eq!(pool, 0, "one CPU's pool, all of it run");
        let two = 2 * full;
        assert_eq!(
            granule::credited(two, period, 0, 2, period),
            two,
            "kept to a period of each CPU"
        );
        assert_eq!(
            granule::credited(two, period, whole, 2, period),
            two,
            "half of two CPUs used"
        );
        let three_quarters = whole * 3 / 2;
        let mut drained = two;
        for _ in 0..4 {
            drained = granule::credited(drained, period, three_quarters, 2, period);
        }
        assert_eq!(drained, 0);
    }

    /// A thread its program set at another priority is left at it.
    #[test]
    fn a_thread_set_otherwise_is_left_as_it_is() {
        std::thread::spawn(|| {
            // SAFETY: the calling thread's pseudo-handle.
            let set = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
            assert_ne!(set, 0);
            assert!(raise_current().unwrap().is_none());
            // SAFETY: the calling thread's pseudo-handle.
            let left = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(left, THREAD_PRIORITY_BELOW_NORMAL);
        })
        .join()
        .unwrap();
    }
}
