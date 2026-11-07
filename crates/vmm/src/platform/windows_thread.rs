//! vCPU and device thread priority on Windows (D191): a candidate, built to be measured on a
//! contended host as Linux's slice was (PM M440), and kept or taken out by what it shows.
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
//! lifts it past them already: which the measurement tells.
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
//! Priorities). So a governor watches each raised thread: every [`PERIOD`] it reads the CPU
//! time the thread used since its last look (GetThreadTimes, as the hold reads its vCPUs',
//! D191) and keeps its credit: half of the time that passed, less what it used, never more
//! than half a period nor less than none. A thread out of credit goes back to
//! THREAD_PRIORITY_NORMAL, to share its host's CPUs as before, and is raised again once a
//! look finds it some. A raised thread so takes from its host's threads half a CPU over any
//! long stretch, and a period more at most (its credit, and what it runs past it before
//! the next look): what a fair scheduler gives one thread beside one busy thread of its
//! weight (Linux's slice, `run_promptly`).
//!
//! The credit is what keeps a thread that waits raised, as GetThreadTimes counts: in whole
//! steps of its clock, 15.625 ms (PM M294), each charged to the thread that runs as it
//! ticks, 6.4 a period. Judged by each period alone, a thread that ran 1 ms of every 6 (17
//! ms a period at most) was charged 4 steps, 62.5 ms, of one period, more than its half,
//! and went back to normal for a period (`a_thread_that_waits_between_its_runs_stays_raised`
//! on CI's windows-2025, 2026-11-06: 187.5 ms, 12 steps, in its second). With its credit,
//! what it does not use of each period's half carries over, and the steps that land on its
//! runs must outrun half of a stretch by half a period more.

use std::io;
use std::marker::PhantomData;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetThreadPriority, SetThreadPriority,
    THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_NORMAL, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SET_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::WindowsProgramming::THREAD_PRIORITY_ERROR_RETURN;

use crate::sync::{lock, wait};

/// The time over which the governor counts each thread's use: the hold's period (D191),
/// over which GetThreadTimes' clock held three spinning threads to within a tenth of a CPU of
/// their share (PM M294).
const PERIOD: Duration = Duration::from_millis(100);
/// The priority a watched thread runs at while it has credit.
const RAISED: THREAD_PRIORITY = THREAD_PRIORITY_ABOVE_NORMAL;

/// The most credit a thread keeps, in nanoseconds: what one period gives, half of it.
fn most_credit() -> u64 {
    u64::try_from(PERIOD.as_nanos() / 2).unwrap_or(u64::MAX)
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
    /// Its credit, in nanoseconds, [`most_credit`] as it is watched: half of each stretch
    /// between looks added and the CPU time it used taken, kept to [`most_credit`] and none.
    credit: u64,
    /// Whether the governor has it at [`RAISED`].
    raised: bool,
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

static WATCHED: Mutex<Vec<Watched>> = Mutex::new(Vec::new());
/// The governor waits on it while it watches no thread.
static ADDED: Condvar = Condvar::new();
static GOVERNOR: OnceLock<Result<(), String>> = OnceLock::new();

/// The calling thread raised while the governor finds it credit: while it uses no more
/// than half of its CPU, by half a period over any stretch.
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
        lock(&WATCHED).retain(|w| w.id != self.id);
        // SAFETY: the calling thread's pseudo-handle, which is not closed: a Watch is dropped
        // on its own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
    }
}

/// Raises the calling thread to [`RAISED`] and has the governor watch it ([`Watch`]). A
/// thread its program or its user set at another priority is left at it, unwatched: `None`.
pub fn raise_current() -> Result<Option<Watch>, String> {
    GOVERNOR.get_or_init(start_governor).clone()?;
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
        credit: most_credit(),
        raised: false,
    };
    watched.ran = super::windows::thread_time(handle).map_err(|e| format!("GetThreadTimes: {e}"))?;
    set(handle, RAISED)?;
    watched.raised = true;
    lock(&WATCHED).push(watched);
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
/// it watches none, a wait for one.
fn govern() {
    // It looks on time however busy the threads it raises keep the CPUs: above them, as the
    // hold's governor runs above its vCPUs (PM M294), for the moment a look takes.
    if let Err(e) = super::windows::run_time_critical() {
        crate::debug!("the priority governor runs at its default priority: {e}");
    }
    loop {
        {
            let mut watched = lock(&WATCHED);
            while watched.is_empty() {
                watched = wait(&ADDED, watched);
            }
            // Under the lock, which a thread takes to leave the watch before it ends: no
            // thread is set once its Watch is gone.
            let now = Instant::now();
            for w in watched.iter_mut() {
                look(w, now);
            }
        }
        std::thread::sleep(PERIOD);
    }
}

/// A thread's `credit` once `passed` has gone by since the last look and it used `used`
/// nanoseconds of CPU time.
fn credited(credit: u64, passed: Duration, used: u64) -> u64 {
    let half = u64::try_from(passed.as_nanos() / 2).unwrap_or(u64::MAX);
    credit
        .saturating_add(half)
        .saturating_sub(used)
        .min(most_credit())
}

/// One look at a watched thread at `now`, once a period has passed since its last: its
/// credit given half of the time since and charged the CPU time it used, it is back at
/// THREAD_PRIORITY_NORMAL with none left, raised again with some.
fn look(w: &mut Watched, now: Instant) {
    let passed = now.saturating_duration_since(w.at);
    if passed < PERIOD {
        return;
    }
    let ran = match super::windows::thread_time(w.handle) {
        Ok(ran) => ran,
        Err(e) => {
            crate::debug!("a watched thread's time is unread: {e}");
            return;
        }
    };
    let used = ran.saturating_sub(w.ran);
    w.ran = ran;
    w.at = now;
    w.credit = credited(w.credit, passed, used);
    let raise = w.credit > 0;
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

    /// What CI's clock charged a thread that ran 1 ms of every 6: whole 15.625 ms steps
    /// (PM M294), none to two a period and 4 in one (62.5 ms, more than its half), which
    /// put it back to normal where each period was judged alone. It keeps credit through
    /// them; one that spins, charged 7 steps of a period, has none left at its first look;
    /// one charged 4 steps of every period runs out of it at its fourth.
    #[test]
    fn credit_carries_a_thread_that_waits_across_its_clocks_steps() {
        const STEP: u64 = 15_625_000;
        let mut credit = most_credit();
        for steps in [1, 0, 1, 2, 0, 1, 4, 0, 1, 1, 2, 1] {
            credit = credited(credit, PERIOD, steps * STEP);
            assert!(credit > 0, "out of credit at {steps} steps");
        }
        assert_eq!(credited(most_credit(), PERIOD, 7 * STEP), 0);
        let (mut credit, mut looks) = (most_credit(), 0);
        while credit > 0 {
            credit = credited(credit, PERIOD, 4 * STEP);
            looks += 1;
        }
        assert_eq!(looks, 4);
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
