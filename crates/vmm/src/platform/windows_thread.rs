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
//! to THREAD_PRIORITY_BELOW_NORMAL, base priority 7, the least step below its host's
//! threads' 8 as the raise is the least above them, and is raised again once a look finds its
//! debt paid and some credit left. A raised thread so takes from its host's threads half a
//! CPU over any long stretch, and two periods more at most (its credit, and what it runs past
//! it before the next look, owed until paid): what a fair scheduler gives one thread beside
//! one busy thread of its weight (Linux's slice, `run_promptly`). Kept to none, as it was
//! until 2026-11-18, a credit forgave what a look ran past it, and a thread raised on the few
//! nanoseconds of credit that looks a little after their period leave took two looks in three
//! (D191).
//!
//! Below its host's threads, not at their priority, so that a lowered thread takes none of
//! the CPU they want: threads of a level run "only when there are no executable threads at a
//! higher level" (SetThreadPriority), and a running thread lowered below a ready one gives way
//! to it at once (an "explicit lowering of priority" is a switch: Solomon, Russinovich and
//! Polze, Windows OS Internals curriculum, unit 4.4), where one of its own priority waits for
//! its quantum to expire (Microsoft, Context Switches), which no look ends: a fixed one on
//! Windows Server, of a length that "varies across different Windows platforms"
//! (Win32_OperatingSystem, `QuantumType`). Lowered to THREAD_PRIORITY_NORMAL, as until
//! 2026-11-18, a lowered thread that ran on kept its CPU past the look that raised it again,
//! and a host's thread on that CPU ran only where a quantum happened to expire in a lowered
//! look, or as Windows rescues a thread it has starved, "every 4 seconds or so" (Windows
//! Internals, 7th ed., part 1, p. 228): on windows-2022 three lowered spinners left the
//! normal thread beside them no clock step in 3 s (CI 44347282760, D191). The boost Windows
//! gives a thread whose wait is satisfied lifts a lowered one above its base as it lifts
//! theirs, a level less each time slice it completes (Priority Boosts).
//!
//! The period is D191's for one thread held to half a CPU (`granule::period`): 1 × g over a
//! half, 2g, g what this process has learned of its governors' step, its thread clock's
//! (GetThreadTimes' 15.625 ms, PM M294) where that is the coarser. Cold, both are measured as
//! the governor starts, before its first look, at normal priority on a thread of their own
//! while the governor answers (`granule::measure`: one shortest wait, and two of its clock's
//! steps of its own CPU time); a thread raised before that is credited at a g of
//! `granule::LEAST_WAIT`, a 2 ms period, so small a credit that it may be lowered until the
//! first look: less authority, never more.
//!
//! A thread's half bounds what it takes alone, not what raised threads take together: three
//! that share one CPU each run a third of it, each under its half, and keep it from the
//! host's threads between them (platform-10). So the watched threads that may run on one set
//! of CPUs (GetThreadGroupAffinity: a processor group and a mask in it, read at each look)
//! also share a pool, credited half of each of those CPUs' time and charged all they ran,
//! raised or lowered, as each thread's own credit is, kept to a period of each either way: a
//! thread is raised only while its own credit and its set's pool both have some, and lowered
//! while either has none. Watched threads so take half of the CPUs they run on over any long
//! stretch, by the clock the governor reads, and two periods of each more at most, however
//! many share them and whatever Windows runs them at. Lowered, they run where no host's
//! thread wants their CPU, and what they run there leaves their pool in debt, a period at
//! most, which they pay before they are raised again. A pool charged only what its threads
//! ran raised, as until 2026-11-19, left out what its clock charged them lowered: on
//! windows-2022 three spinners beside a normal thread on one CPU were raised in 49.8% of the
//! governor's looks, ran 53.4% of the CPU's cycles (QueryThreadCycleTime), and were charged
//! 78.6% of 3 s by GetThreadTimes, the steps it lands on them across the looks that lower
//! them charged to no pool (PM M620; the test below failed there twice, CI 44379986478 and
//! 44408928475). A pool is the governor's, one a process: the watched threads of other
//! processes (other VM processes on the host) draw on pools of their own, which no owner here
//! sees (owed, D191).
//!
//! The credit is what keeps a thread that waits raised, as GetThreadTimes counts: in whole
//! steps of its clock, 15.625 ms (PM M294), each charged to the thread that runs as it
//! ticks. Judged by each period alone, a thread that ran 1 ms of every 6 (17 ms of 100 at
//! most) was charged 4 steps, 62.5 ms, of one 100 ms period, more than its half, and went
//! back to normal for a period (`a_thread_that_waits_between_its_runs_stays_raised` on CI's
//! windows-2025, 2026-11-06: 187.5 ms, 12 steps, in its second). With its credit, what it
//! does not use of each period's half carries over. Its credit holds the most one look can
//! charge a thread, the whole period of its one CPU (every step of the period landing on its
//! runs), so that no single look's steps lower a thread that keeps to its half: at 2g, a look
//! of two steps charged both.
//!
//! The governor alone sets the priority of a thread it watches, from
//! THREAD_PRIORITY_TIME_CRITICAL, and it waits on none of them: a thread asks to be watched, and
//! to be let go of, by lending its ask to the governor's thread with an APC (QueueUserAPC, which
//! the kernel queues to that thread, to run in its next alertable wait, first in first out),
//! and waits on an event of its own for the answer (SetEvent). Neither way waits on the asker,
//! at whatever priority it runs, and no look under way sets a thread that has asked to leave.
//! A thread that set its own level as it registered, under the governor's lock, as until
//! 2026-11-19, could fall below a ready thread of its CPU while it held the lock, and the
//! governor waited for as long as that thread had the CPU: on windows-2022 and windows-2025 no
//! look for 2,743 and 2,892 ms of a 3 s window (windows-lower run 44596604981; D191, PM M621).
//! std's channels wait on their peers alike: a receiver spins on a slot its sender has claimed
//! and not yet written, and both take their waker's lock (library/std/src/sync/mpmc, `list.rs`,
//! `waker.rs`).

use std::cell::Cell;
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, HANDLE, WAIT_IO_COMPLETION, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, GetCurrentProcess,
    GetCurrentThread, GetCurrentThreadId, GetThreadGroupAffinity, GetThreadPriority, INFINITE, QueueUserAPC,
    SYNCHRONIZATION_SYNCHRONIZE, SetEvent, SetThreadPriority, SetWaitableTimer, SleepEx, THREAD_PRIORITY,
    THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_LIMITED_INFORMATION, TIMER_MODIFY_STATE,
    WaitForSingleObject, WaitForSingleObjectEx,
};
use windows_sys::Win32::System::WindowsProgramming::THREAD_PRIORITY_ERROR_RETURN;

use super::granule;

/// The priority a watched thread runs at while it has credit.
const RAISED: THREAD_PRIORITY = THREAD_PRIORITY_ABOVE_NORMAL;
/// The priority a watched thread runs at while it, or its CPUs' pool, has none: below its
/// host's threads, which so have the CPU whenever they are ready (the module's doc).
const LOWERED: THREAD_PRIORITY = THREAD_PRIORITY_BELOW_NORMAL;

/// The priority the governor has a thread at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// THREAD_PRIORITY_NORMAL, its host's threads' own: before the governor first sets it,
    /// and once the governor lets go of it.
    Normal,
    /// [`RAISED`]: its own credit and its pool's both have some.
    Raised,
    /// [`LOWERED`]: either has none.
    Lowered,
}

impl Level {
    fn priority(self) -> THREAD_PRIORITY {
        match self {
            Level::Normal => THREAD_PRIORITY_NORMAL,
            Level::Raised => RAISED,
            Level::Lowered => LOWERED,
        }
    }
}

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
#[derive(Debug)]
struct Watched {
    /// Its thread's ID, by which it asks to be let go of.
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
    /// The priority the governor has it at.
    level: Level,
    /// The CPUs it may run on, as last read: its pool's.
    cpus: Cpus,
}

// SAFETY: a handle of a thread of this process, opened to read its times and set its
// priority alone, which any thread may do; its thread makes it and lends it, with its ask,
// to the governor's thread, which alone uses it from then on and closes it once, as its
// entry goes.
unsafe impl Send for Watched {}

impl Drop for Watched {
    fn drop(&mut self) {
        // SAFETY: the handle this entry duplicated, closed once.
        unsafe { CloseHandle(self.handle) };
    }
}

impl Watched {
    /// The calling thread's entry, at THREAD_PRIORITY_NORMAL as yet: a handle the governor
    /// may use, its CPU time and when read, and its CPUs. Its credit is the governor's to
    /// give as it watches it.
    fn current() -> Result<Watched, String> {
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
            credit: 0,
            level: Level::Normal,
            cpus: Cpus { group: 0, mask: 0 },
        };
        watched.ran = super::windows::thread_time(handle).map_err(|e| format!("GetThreadTimes: {e}"))?;
        watched.cpus = Cpus::of(handle)?;
        Ok(watched)
    }
}

/// The watched threads, and their sets of CPUs' pools: the governor's thread's alone.
struct Governed {
    threads: Vec<Watched>,
    pools: Vec<Pool>,
}

/// The governor's thread, to which threads lend their asks ([`Asking::ask`]).
#[derive(Debug)]
struct Governor(JoinHandle<()>);

static GOVERNOR: OnceLock<Result<Governor, String>> = OnceLock::new();
/// The looks the governor has made: its durable record for tests, which a governor that
/// waits on a thread it watches does not add to.
#[cfg(test)]
static LOOKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// The asks lent to the governor's thread and not yet answered, put here by their APCs
    /// as its alertable waits run them ([`lent`]).
    static LENT: Cell<Vec<usize>> = const { Cell::new(Vec::new()) };
}

/// An auto-reset event, made unsignalled (CreateEventW): what a thread waits on for the
/// governor's answer.
#[derive(Debug)]
struct Event(OwnedHandle);

impl Event {
    fn new() -> Result<Event, String> {
        // SAFETY: an unnamed auto-reset event of default security, not signalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(format!("CreateEventW: {}", io::Error::last_os_error()));
        }
        // SAFETY: the event just made, owned from here on.
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(event) }))
    }
}

/// The governor's clock between looks: a waitable timer of high resolution
/// (CreateWaitableTimerExW's CREATE_WAITABLE_TIMER_HIGH_RESOLUTION), the clock std's sleep
/// waits on, where a wait's timeout in milliseconds ends on the system's clock ticks.
#[derive(Debug)]
struct Timer(OwnedHandle);

impl Timer {
    fn new() -> Result<Timer, String> {
        // SAFETY: an unnamed timer of default security, with the rights to set it and wait
        // on it.
        let timer = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            )
        };
        if timer.is_null() {
            return Err(format!("CreateWaitableTimerExW: {}", io::Error::last_os_error()));
        }
        // SAFETY: the timer just made, owned from here on.
        Ok(Timer(unsafe { OwnedHandle::from_raw_handle(timer) }))
    }

    /// Sets it to signal `after` from now and no sooner: a due time relative to now, so
    /// negative, in 100 ns units rounded up (SetWaitableTimer).
    fn set(&self, after: Duration) -> Result<(), String> {
        let due = i64::try_from(after.as_nanos().div_ceil(100))
            .ok()
            .and_then(i64::checked_neg)
            .ok_or_else(|| format!("a look {after:?} on"))?;
        // SAFETY: the timer this holds, a due time to read, and no completion routine.
        if unsafe { SetWaitableTimer(self.0.as_raw_handle(), &due, 0, None, std::ptr::null(), 0) } == 0 {
            return Err(format!("SetWaitableTimer: {}", io::Error::last_os_error()));
        }
        Ok(())
    }
}

/// What a thread asks of the governor.
#[derive(Debug)]
enum What {
    /// To be watched: its entry, at THREAD_PRIORITY_NORMAL as yet.
    Watch(Watched),
    /// To be let go of, and set back to THREAD_PRIORITY_NORMAL: its thread's ID.
    Leave(u32),
}

/// A thread's ask of the governor, and the answer: its thread's, lent to the governor's
/// thread for each ask ([`Asking::ask`]).
#[derive(Debug)]
struct Asking {
    what: Option<What>,
    answer: Option<Result<(), String>>,
    /// Signalled once the governor has answered.
    answered: Event,
}

impl Asking {
    fn new() -> Result<Box<Asking>, String> {
        Ok(Box::new(Asking {
            what: None,
            answer: None,
            answered: Event::new()?,
        }))
    }

    /// Asks `what` of the governor and waits for the answer: lent to the governor's thread
    /// by an APC (QueueUserAPC), which the kernel queues to it and it runs in its next
    /// alertable wait, first in first out; the governor answers in place and then signals
    /// `answered`, and the asker touches it only before it lends it and once signalled. The
    /// kernel carries both ways, so that the governor waits on nothing the asker holds, at
    /// whatever priority the asker runs (the module's doc). Its wait failing (no wait on an
    /// event its asker holds open fails), the ask may yet be answered: it is left lent,
    /// never freed under the governor.
    fn ask(mut self: Box<Self>, what: What) -> Result<(Box<Asking>, Result<(), String>), String> {
        let governor = match GOVERNOR.get() {
            Some(Ok(governor)) => governor,
            Some(Err(e)) => return Err(e.clone()),
            None => return Err("the priority governor has not started".into()),
        };
        self.what = Some(what);
        self.answer = None;
        let answered = self.answered.0.as_raw_handle();
        let asking = Box::into_raw(self);
        // SAFETY: the governor's thread's handle, which its JoinHandle holds open for as long
        // as the process runs, with all access (CreateThread), THREAD_SET_CONTEXT as
        // QueueUserAPC needs among it; `lent` takes the ask once, as the APC runs.
        if unsafe { QueueUserAPC(Some(lent), governor.0.as_raw_handle(), asking.expose_provenance()) } == 0 {
            let e = io::Error::last_os_error();
            // SAFETY: not queued, so still this thread's alone, as Box::into_raw left it.
            drop(unsafe { Box::from_raw(asking) });
            return Err(format!("QueueUserAPC: {e}"));
        }
        // SAFETY: the event the ask holds, open while it is lent.
        if unsafe { WaitForSingleObject(answered, INFINITE) } != WAIT_OBJECT_0 {
            return Err(format!(
                "waiting on the priority governor: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: signalled once answered: the governor is done with it, and it is this
        // thread's again, as Box::into_raw left it.
        let mut asking = unsafe { Box::from_raw(asking) };
        let answer = asking.answer.take().unwrap_or_else(|| Err("unanswered".into()));
        Ok((asking, answer))
    }
}

/// The calling thread raised while the governor finds it credit: while it uses no more
/// than half of its CPU, by a period over any stretch; lowered while it finds none. Held by
/// the thread while it runs, and dropped before it ends, which has the governor let go of
/// it, back at THREAD_PRIORITY_NORMAL.
#[derive(Debug)]
pub struct Watch {
    id: u32,
    /// Its thread's asks: lent once more as it drops, to be let go of.
    asking: Option<Box<Asking>>,
    /// Its own thread's: it neither moves nor is shared.
    _thread: PhantomData<*const ()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        // Let go of by the governor, which sets it back to normal: a thread that set itself
        // could be set again by a look already under way, and could hold what the governor
        // waits on below the threads it governs (the module's doc).
        let Some(asking) = self.asking.take() else {
            return;
        };
        match asking.ask(What::Leave(self.id)) {
            Ok((_, Ok(()))) => {}
            Ok((_, Err(e))) | Err(e) => {
                crate::debug!("a watched thread let go of stays at its priority: {e}")
            }
        }
    }
}

/// Has the governor watch the calling thread ([`Watch`]), which raises it to [`RAISED`], or
/// lowers it to [`LOWERED`] where its CPUs' pool has no credit, as it answers. A thread its
/// program or its user set at another priority is left at it, unwatched: `None`. Refused,
/// the thread left as it is, where the governor derives no period ([`granule::Outgrown`]) or
/// cannot set it.
pub fn raise_current() -> Result<Option<Watch>, String> {
    GOVERNOR
        .get_or_init(start_governor)
        .as_ref()
        .map_err(Clone::clone)?;
    // SAFETY: the calling thread's pseudo-handle, which is not closed.
    let current = unsafe { GetThreadPriority(GetCurrentThread()) };
    if u32::try_from(current).is_ok_and(|p| p == THREAD_PRIORITY_ERROR_RETURN) {
        return Err(format!("GetThreadPriority: {}", io::Error::last_os_error()));
    }
    if current != THREAD_PRIORITY_NORMAL {
        return Ok(None);
    }
    let watched = Watched::current()?;
    let id = watched.id;
    let (asking, answer) = Asking::new()?.ask(What::Watch(watched))?;
    answer?;
    Ok(Some(Watch {
        id,
        asking: Some(asking),
        _thread: PhantomData,
    }))
}

/// The APC by which a thread lends its ask to the governor's thread ([`Asking::ask`]), run
/// in one of its alertable waits: kept for it to answer as the wait returns. The governor's
/// thread runs for as long as the process: its thread-locals are never destroyed, which
/// alone would end `with` in a panic.
extern "system" fn lent(asking: usize) {
    LENT.with(|lent| {
        let mut asks = lent.take();
        asks.push(asking);
        lent.set(asks);
    });
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

fn start_governor() -> Result<Governor, String> {
    let timer = Timer::new()?;
    std::thread::Builder::new()
        .name("priority-governor".into())
        .spawn(move || govern(&timer))
        .map(Governor)
        .map_err(|e| format!("spawning the priority governor: {e}"))
}

/// What the governor's thread waits for, besides asks, which end any of its waits.
#[derive(Debug)]
enum Waiting {
    /// g measured, by a thread of its own: no look before.
    Measure(JoinHandle<Result<Duration, granule::Unclocked>>),
    /// Asks alone: it watches no thread.
    Ask,
    /// Its next look, `period` after `began`, for when its timer is set.
    Look { began: Instant, period: Duration },
}

/// How an alertable wait of the governor's ended.
#[derive(Debug)]
enum Woke {
    /// Its object signalled.
    Signalled,
    /// Asks lent to it, kept as their APCs ran ([`lent`]).
    Asked,
    /// Neither: why.
    Failed(String),
}

/// Waits for `object` alertably (WaitForSingleObjectEx), so that the APC of an ask lent to
/// this thread ends the wait as it runs.
fn wait_alertably(object: HANDLE) -> Woke {
    // SAFETY: a handle its caller holds open, waited on.
    match unsafe { WaitForSingleObjectEx(object, INFINITE, 1) } {
        WAIT_OBJECT_0 => Woke::Signalled,
        WAIT_IO_COMPLETION => Woke::Asked,
        other => Woke::Failed(format!(
            "WaitForSingleObjectEx answered {other}: {}",
            io::Error::last_os_error()
        )),
    }
}

/// For as long as the process runs: each ask answered as it is lent, and once a period a
/// look at every thread watched. Each period derived as its look begins, from g as the
/// process knows it then; the lateness of each wait its timer ended taught to it
/// (`granule::LATE`), and of none an ask ended.
fn govern(timer: &Timer) {
    // Above the threads it watches from the first, as the hold's governor runs above its
    // vCPUs (PM M294): it answers and looks on time however busy they keep the CPUs, and
    // waits on none of them.
    if let Err(e) = super::windows::run_time_critical() {
        crate::debug!("the priority governor runs at its default priority: {e}");
    }
    let mut governed = Governed {
        threads: Vec::new(),
        pools: Vec::new(),
    };
    // g's wake and thread clock's step measured before its first look, at normal priority
    // on a thread of their own: one shortest wait, and two of its clock's steps of its own
    // CPU time at most, which no ask waits for.
    let mut waiting = match std::thread::Builder::new()
        .name("priority-governor-measure".into())
        .spawn(granule::measure)
    {
        Ok(measuring) => Waiting::Measure(measuring),
        Err(e) => {
            crate::debug!("the priority governor knows no clock step: {e}");
            Waiting::Ask
        }
    };
    loop {
        governed.answer();
        waiting = match waiting {
            Waiting::Measure(measuring) => match wait_alertably(measuring.as_raw_handle()) {
                Woke::Asked => Waiting::Measure(measuring),
                Woke::Signalled => {
                    match measuring.join() {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => crate::debug!("the priority governor knows no clock step: {}", e.0),
                        Err(_) => {
                            crate::debug!("the priority governor knows no clock step: its measure panicked")
                        }
                    }
                    Waiting::Ask
                }
                Woke::Failed(e) => {
                    crate::debug!("the priority governor looks before its measure ends: {e}");
                    Waiting::Ask
                }
            },
            Waiting::Ask if governed.threads.is_empty() => {
                // SAFETY: an alertable wait with no end, which an ask's APC ends.
                unsafe { SleepEx(INFINITE, 1) };
                Waiting::Ask
            }
            Waiting::Ask => governed.look_now(timer),
            Waiting::Look { .. } if governed.threads.is_empty() => Waiting::Ask,
            Waiting::Look { began, period } => match wait_alertably(timer.0.as_raw_handle()) {
                Woke::Asked => Waiting::Look { began, period },
                Woke::Signalled => {
                    granule::LATE.waited(period, began);
                    governed.look_now(timer)
                }
                Woke::Failed(e) => {
                    crate::warn!("the priority governor's clock: {e}");
                    governed.let_go_all();
                    Waiting::Ask
                }
            },
        };
    }
}

impl Governed {
    /// Answers each ask lent to the governor's thread since it last did, in the order lent.
    fn answer(&mut self) {
        for asking in LENT.with(Cell::take) {
            // SAFETY: an ask its asker lent this thread alone (`Asking::ask`), which it
            // touches again only once signalled, below.
            let asking = unsafe { &mut *std::ptr::with_exposed_provenance_mut::<Asking>(asking) };
            let answer = match asking.what.take() {
                Some(What::Watch(watched)) => self.watch(watched),
                Some(What::Leave(id)) => self.let_go(id),
                None => Err("an ask of nothing".into()),
            };
            let answered = asking.answered.0.as_raw_handle();
            asking.answer = Some(answer);
            // SAFETY: the asker's event, open while its ask is lent; the ask, its asker's again
            // once signalled, is not touched here again.
            if unsafe { SetEvent(answered) } == 0 {
                crate::warn!(
                    "the priority governor's answer is unsignalled: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }

    /// Watches `w`, its thread at THREAD_PRIORITY_NORMAL as yet, credited a period: raised
    /// where its CPUs' pool has credit, as each look raises it, else lowered. Refused, the
    /// thread left at normal and unwatched, where no period is derived or its level is not
    /// set.
    fn watch(&mut self, mut w: Watched) -> Result<(), String> {
        let period = period().map_err(|e| format!("the priority governor's period: {e}"))?;
        w.credit = granule::most_credit(period, 1);
        let level = if self.pooled(w.cpus, w.at, 0, period) > 0 {
            Level::Raised
        } else {
            Level::Lowered
        };
        hold(&mut w, level)?;
        self.threads.push(w);
        Ok(())
    }

    /// Lets go of thread `id`, back at THREAD_PRIORITY_NORMAL: of none where every thread was
    /// let go of already ([`Governed::let_go_all`]).
    fn let_go(&mut self, id: u32) -> Result<(), String> {
        match self.threads.iter().position(|w| w.id == id) {
            Some(at) => hold(&mut self.threads.remove(at), Level::Normal),
            None => Ok(()),
        }
    }

    /// Lets go of every thread, each back at THREAD_PRIORITY_NORMAL, as no other is watched:
    /// no look without a period derived, or without its clock.
    fn let_go_all(&mut self) {
        for w in &mut self.threads {
            if let Err(e) = hold(w, Level::Normal) {
                crate::debug!("a watched thread let go of stays at its priority: {e}");
            }
        }
        self.threads.clear();
        self.pools.clear();
    }

    /// A look now ([`Governed::look`]) and the timer set for the next a period on: what the
    /// governor waits for next. With no period derived or its timer unset, every thread let
    /// go of.
    fn look_now(&mut self, timer: &Timer) -> Waiting {
        let period = match period() {
            Ok(period) => period,
            Err(e) => {
                crate::warn!("the priority governor: {e}");
                self.let_go_all();
                return Waiting::Ask;
            }
        };
        self.look(Instant::now(), period);
        #[cfg(test)]
        LOOKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let began = Instant::now();
        match timer.set(period) {
            Ok(()) => Waiting::Look { began, period },
            Err(e) => {
                crate::warn!("the priority governor's clock: {e}");
                self.let_go_all();
                Waiting::Ask
            }
        }
    }

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
    /// since its last ([`charge`]), and its CPUs' pool all of it, raised or lowered; then each
    /// raised where its own credit and its pool's both have some, else lowered.
    fn look(&mut self, now: Instant, period: Duration) {
        // Each set of CPUs a thread may run on, and what its threads ran.
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
            let level = if w.credit > 0 && pooled {
                Level::Raised
            } else {
                Level::Lowered
            };
            if let Err(e) = hold(w, level) {
                crate::debug!("a watched thread stays at its priority: {e}");
            }
        }
    }
}

/// A watched thread's look at `now`, once a `period` has passed since its last: its credit
/// given half of the time since and charged the CPU time it used, and its CPUs read again.
/// What it used, its pool's to be charged too, whatever its level: `None` where no look was
/// due or its time is unread.
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
    Some(used)
}

/// Sets a watched thread at `level`, where it is not at it already; refused, it stays at the
/// level it was at.
fn hold(w: &mut Watched, level: Level) -> Result<(), String> {
    if level != w.level {
        set(w.handle, level.priority())?;
        w.level = level;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ThreadClock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use windows_sys::Win32::System::Threading::{OpenThread, SetThreadPriorityBoost};
    #[cfg(target_arch = "x86_64")]
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

    /// The calling thread's CPU cycles (QueryThreadCycleTime), which x86-64's Windows counts
    /// in its time stamp counter's ticks, one rate on every CPU (an invariant TSC): a thread's
    /// share of a window's ticks is its share of that time, with none of the clock steps
    /// GetThreadTimes charges in, which on windows-2022 charged three governed spinners 78.6% of
    /// a window whose cycles they ran 53.4% of (PM M620).
    #[cfg(target_arch = "x86_64")]
    fn cycles() -> u64 {
        let mut cycles = 0u64;
        // SAFETY: the calling thread's pseudo-handle, and a count to write.
        let read = unsafe { QueryThreadCycleTime(GetCurrentThread(), &raw mut cycles) };
        assert_ne!(read, 0, "{}", io::Error::last_os_error());
        cycles
    }

    /// The time stamp counter now: the window [`cycles`] are a share of.
    #[cfg(target_arch = "x86_64")]
    fn stamp() -> u64 {
        // SAFETY: RDTSC reads the counter, as every x86-64 CPU has it.
        unsafe { std::arch::x86_64::_rdtsc() }
    }

    /// What a thread used, by GetThreadTimes ([`ThreadClock`]) and, on x86-64, in cycles.
    #[derive(Debug, Clone, Copy, Default)]
    struct Used {
        clock: u64,
        cycles: u64,
    }

    impl Used {
        /// The calling thread's, so far.
        fn now(clock: &ThreadClock) -> Used {
            Used {
                clock: clock.read().unwrap(),
                #[cfg(target_arch = "x86_64")]
                cycles: cycles(),
                #[cfg(not(target_arch = "x86_64"))]
                cycles: 0,
            }
        }

        fn since(self, from: Used) -> Used {
            Used {
                clock: self.clock - from.clock,
                cycles: self.cycles - from.cycles,
            }
        }

        fn add(self, other: Used) -> Used {
            Used {
                clock: self.clock + other.clock,
                cycles: self.cycles + other.cycles,
            }
        }
    }

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

    /// A raised thread that runs on is lowered, and is raised again once it waits.
    #[test]
    fn a_thread_run_long_is_lowered_and_raised_again_as_it_waits() {
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
        while *seen.last().unwrap() != LOWERED && start.elapsed() < Duration::from_secs(25) {
            std::thread::sleep(Duration::from_millis(10));
            seen.push(priority_of(id));
        }
        let moved = *seen.last().unwrap() == LOWERED;
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
        assert!(moved, "never lowered while it ran: {seen:?}");
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

    /// The CPUs this process may run on, as a mask.
    fn process_cpus() -> usize {
        use windows_sys::Win32::System::Threading::GetProcessAffinityMask;
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: this process's pseudo-handle, and two words for the masks.
        let ok = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &raw mut process, &raw mut system) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        assert_ne!(process, 0);
        process
    }

    /// The highest CPU of `mask`, as a mask of it alone.
    fn highest(mask: usize) -> usize {
        1 << (usize::BITS - 1 - mask.leading_zeros())
    }

    /// The highest CPU this process may run on, as a mask of it alone.
    fn last_cpu() -> usize {
        highest(process_cpus())
    }

    /// The highest CPU this process may run on below [`last_cpu`], which the bound's test
    /// pins its threads to: none where it may run on one CPU alone.
    fn next_to_last_cpu() -> Option<usize> {
        let below = process_cpus() & !last_cpu();
        (below != 0).then(|| highest(below))
    }

    /// Pins the calling thread to the CPUs `mask` names.
    fn pin(mask: usize) {
        use windows_sys::Win32::System::Threading::SetThreadAffinityMask;
        // SAFETY: the calling thread's pseudo-handle.
        assert_ne!(unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) }, 0);
    }

    /// Ends the test's spinners as it drops, at the window's end or as a failure unwinds,
    /// before their scope joins them.
    struct Stop<'a>(&'a AtomicBool);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }

    /// Do: pin a thread of normal priority to one CPU, spinning, then three threads there
    /// that the governor watches, spinning too, for 3 s. Expect the three to have used no
    /// more than half of the time and two periods, the module's bound for one CPU, and the
    /// normal thread to have run. Their pool is credited half of the CPU and charged all
    /// they ran (its arithmetic held by granule's
    /// `a_share_takes_half_its_cpu_and_two_periods_over_any_stretch`); lowered, they run
    /// "only when there are no executable threads at a higher level" (SetThreadPriority), and
    /// the normal thread, ready on their CPU throughout, is at one. Their boost on waking is
    /// off: one woken from a wait for the governor's lock would be lifted to the normal
    /// thread's priority or past it, a level less each time slice it completed (Priority
    /// Boosts). Their use is read from their own clocks (GetThreadTimes), whose steps the
    /// governor charges them. Lowered to the normal thread's priority, as until 2026-11-18,
    /// they kept the CPU between them, and on windows-2022 the normal thread was charged no
    /// step in the 3 s (CI 44347282760); their pool charged only what they ran raised, as until
    /// 2026-11-19, they were charged 2.25 s of 3.0 there, the steps of their lowered looks
    /// uncharged (CI 44408928475, PM M620).
    #[test]
    fn raised_threads_sharing_a_cpu_take_no_more_than_half_of_it() {
        const RAISED_THREADS: usize = 3;
        let cpu = last_cpu();
        let spinning = AtomicBool::new(true);
        let (raised_ran, host_ran, elapsed, stamps) = std::thread::scope(|s| {
            let stop = Stop(&spinning);
            let spinning = &spinning;
            // The normal thread, on the CPU before the three are watched: ready there
            // throughout.
            let (on_cpu, is_on_cpu) = mpsc::channel();
            let normal = s.spawn(move || {
                pin(cpu);
                let clock = ThreadClock::current().unwrap();
                let from = Used::now(&clock);
                on_cpu.send(()).unwrap();
                while spinning.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
                Used::now(&clock).since(from)
            });
            is_on_cpu.recv().unwrap();
            let began = Instant::now();
            #[cfg(target_arch = "x86_64")]
            let stamped = stamp();
            let raised: Vec<_> = (0..RAISED_THREADS)
                .map(|_| {
                    s.spawn(move || {
                        // SAFETY: the calling thread's pseudo-handle.
                        assert_ne!(unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) }, 0);
                        pin(cpu);
                        let watch = raise_current().unwrap().unwrap();
                        let clock = ThreadClock::current().unwrap();
                        let from = Used::now(&clock);
                        while spinning.load(Ordering::Relaxed) {
                            std::hint::spin_loop();
                        }
                        let ran = Used::now(&clock).since(from);
                        drop(watch);
                        ran
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_secs(3));
            drop(stop);
            let raised_ran = raised
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .fold(Used::default(), Used::add);
            let elapsed = began.elapsed();
            #[cfg(target_arch = "x86_64")]
            let stamps = stamp() - stamped;
            #[cfg(not(target_arch = "x86_64"))]
            let stamps = 0;
            (raised_ran, normal.join().unwrap(), elapsed, stamps)
        });
        let period = period().unwrap();
        let bound = elapsed / 2 + period * 2;
        let said = format!(
            "the three were charged {:?} of {elapsed:?} by GetThreadTimes (half of it and two \
             periods of {period:?}: {bound:?}), and ran {} of its {stamps} counter ticks; the normal \
             thread was charged {} ns, and ran {} ticks",
            Duration::from_nanos(raised_ran.clock),
            raised_ran.cycles,
            host_ran.clock,
            host_ran.cycles,
        );
        // On x86-64, what they ran, in cycles of the window's counter ticks, not the steps
        // GetThreadTimes charged them in (PM M620); the bound's periods as many ticks as the
        // window's ticks a nanosecond.
        #[cfg(target_arch = "x86_64")]
        {
            let per_ns = |d: Duration| {
                u64::try_from(u128::from(stamps) * d.as_nanos() / elapsed.as_nanos().max(1)).unwrap()
            };
            let most = stamps / 2 + per_ns(period * 2);
            assert!(
                raised_ran.cycles <= most,
                "past their bound of {most} ticks: {said}"
            );
            assert!(host_ran.cycles > 0, "the normal thread never ran: {said}");
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            assert!(
                Duration::from_nanos(raised_ran.clock) <= bound,
                "past their bound: {said}"
            );
            assert!(host_ran.clock > 0, "the normal thread never ran: {said}");
        }
    }

    /// The looks the governor has made so far.
    fn looks() -> u64 {
        LOOKS.load(Ordering::Relaxed)
    }

    /// Waits until the governor has made `more` looks.
    fn wait_looks(more: u64) {
        let until = looks() + more;
        while looks() < until {
            std::thread::sleep(granule::LEAST_WAIT);
        }
    }

    /// Sets the calling thread at `priority`.
    fn set_own(priority: THREAD_PRIORITY) {
        // SAFETY: the calling thread's pseudo-handle.
        let set = unsafe { SetThreadPriority(GetCurrentThread(), priority) };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
    }

    /// Sets thread `id` of this process at `priority`, from another thread.
    fn set_priority_of(id: u32, priority: THREAD_PRIORITY) {
        // SAFETY: opens thread `id` to set its priority alone.
        let handle = unsafe { OpenThread(THREAD_SET_LIMITED_INFORMATION, 0, id) };
        assert!(!handle.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: the handle just opened, with the right to set its priority.
        let set = unsafe { SetThreadPriority(handle, priority) };
        let error = io::Error::last_os_error();
        // SAFETY: the handle just opened, closed once.
        unsafe { CloseHandle(handle) };
        assert_ne!(set, 0, "{error}");
    }

    /// An auto-reset event, not signalled.
    fn event() -> OwnedHandle {
        // SAFETY: an unnamed auto-reset event, not signalled.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: the event just made, owned from here on.
        unsafe { OwnedHandle::from_raw_handle(event) }
    }

    /// Do: on one CPU, two watched spinners that this thread sets back at normal priority once
    /// the governor has lowered them (the governor sets a thread only as its level changes), so
    /// that their pool is charged all of the CPU they hold and stays a period in debt however long
    /// they hold it; then a thread there registers, woken above normal and set back at normal once
    /// it has the CPU, the spinners ready behind it. Expect the governor to look on at the
    /// period's pace while the registering thread is starved, and its registration to complete:
    /// the thread lowered by the governor before it runs again. Two, so that a normal thread is
    /// ready on the CPU whenever the other leaves it: beside one, the registering thread, lowered,
    /// now and then had the CPU while that one was ready at 8 or chosen to run next, last switched
    /// out at a dispatch interrupt (WrDispatchInt), in 8 of 1,850 registrations on jupiter, 5 of
    /// them run through (PM M621). Their boost on waking is off, so that the registering thread,
    /// answered, stays below them; this thread is off their CPU, where it would wait behind them.
    /// A thread that lowered itself as it registered, holding the governor's lock, as until
    /// 2026-11-19, kept the governor from every look while the CPU was the spinners': 2,743 ms of
    /// a 3 s window on windows-2022 and 2,892 on windows-2025 (windows-lower run 44596604981).
    #[test]
    fn a_thread_starved_as_it_registers_stops_no_look() {
        // The looks due while it is starved: one a period, each allowed a period late. Fewer than
        // two could be a look under way as it registered.
        const DUE: u64 = 4;
        const SPINNERS: usize = 2;
        let Some(cpu) = next_to_last_cpu() else {
            println!("SKIP: this process may run on one CPU alone");
            return;
        };
        pin(process_cpus() & !cpu);
        let spinning = AtomicBool::new(true);
        let registering = AtomicBool::new(false);
        let returned = AtomicBool::new(false);
        let go = event();
        let (period, looked, waited, level, starved, (registered, level_once_run)) =
            std::thread::scope(|s| {
                let stop = Stop(&spinning);
                let (spinning, registering, returned, go) = (&spinning, &registering, &returned, &go);
                let (spinner_tx, spinner_rx) = mpsc::channel();
                for _ in 0..SPINNERS {
                    let spinner_tx = spinner_tx.clone();
                    s.spawn(move || {
                        // SAFETY: the calling thread's pseudo-handle.
                        assert_ne!(unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) }, 0);
                        pin(cpu);
                        let watch = raise_current().unwrap().unwrap();
                        // SAFETY: no preconditions.
                        spinner_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
                        while spinning.load(Ordering::Relaxed) {
                            std::hint::spin_loop();
                        }
                        drop(watch);
                    });
                }
                let spinners: Vec<u32> = (0..SPINNERS).map(|_| spinner_rx.recv().unwrap()).collect();
                // Their pool's credit, a period of the CPU, loses half a look at each look that
                // charges it all of the CPU (`a_look_charged_its_whole_period_keeps_a_thread_raised`):
                // none by its fourth look, its first coming before they have run a period.
                wait_looks(5);
                for spinner in &spinners {
                    assert_eq!(
                        priority_of(*spinner),
                        LOWERED,
                        "the spinners alone on their CPU were not lowered"
                    );
                }
                // From this thread, both at once: one set back by its own program would starve the
                // other, still lowered, before it could set itself.
                for spinner in &spinners {
                    set_priority_of(*spinner, THREAD_PRIORITY_NORMAL);
                }
                // Two looks more of all of its CPU: their pool at its floor, a period in debt.
                wait_looks(2);
                let period = period().unwrap();
                let window = period * 2 * u32::try_from(DUE).unwrap();
                let (registering_tx, registering_rx) = mpsc::channel();
                let thread = s.spawn(move || {
                    // SAFETY: the calling thread's pseudo-handle.
                    assert_ne!(unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) }, 0);
                    // Above the spinners while it waits, so that it has the CPU once woken; then
                    // at normal, as the governor watches a thread from.
                    set_own(THREAD_PRIORITY_ABOVE_NORMAL);
                    pin(cpu);
                    // SAFETY: no preconditions.
                    registering_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
                    // SAFETY: a wait on the event the scope holds open.
                    let woke = unsafe { WaitForSingleObject(go.as_raw_handle(), INFINITE) };
                    assert_eq!(woke, WAIT_OBJECT_0);
                    set_own(THREAD_PRIORITY_NORMAL);
                    registering.store(true, Ordering::Relaxed);
                    let watch = raise_current();
                    // SAFETY: the calling thread's pseudo-handle.
                    let level = unsafe { GetThreadPriority(GetCurrentThread()) };
                    returned.store(true, Ordering::Relaxed);
                    (watch.map(|watch| watch.is_some()), level)
                });
                let id = registering_rx.recv().unwrap();
                // SAFETY: the event the scope holds open.
                assert_ne!(unsafe { SetEvent(go.as_raw_handle()) }, 0);
                while !registering.load(Ordering::Relaxed) {
                    std::thread::sleep(granule::LEAST_WAIT);
                }
                let (from, began) = (looks(), Instant::now());
                while looks() < from + DUE && began.elapsed() < window {
                    std::thread::sleep(granule::LEAST_WAIT);
                }
                let (looked, waited, level) = (looks() - from, began.elapsed(), priority_of(id));
                let starved = !returned.load(Ordering::Relaxed);
                // The registering thread, starved, runs once the spinners stop.
                drop(stop);
                (period, looked, waited, level, starved, thread.join().unwrap())
            });
        let said = format!(
            "{looked} looks in {waited:?} at a period of {period:?}; the registering thread at \
             {level}, {}; registered {registered:?}, at {level_once_run} once it ran",
            if starved { "starved" } else { "run" }
        );
        assert!(starved, "the registering thread was not starved: {said}");
        assert_eq!(
            level, LOWERED,
            "the registration did not lower its thread: {said}"
        );
        assert!(looked >= DUE, "the governor stopped looking: {said}");
        assert_eq!(registered, Ok(true), "{said}");
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
