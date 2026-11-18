//! A test's party (D215, #77): its test's own thread, and every thread made from one of its
//! party's, by the code under test (the daemon's `DaemonThread`, shards-remote's threads, each
//! through [`carried`] under `cfg(test)`) or by the test ([`spawn_in`], [`spawn_member`]),
//! each made a member as it starts, of the party of the thread that made it. A test's judged
//! wait counts what its party's members ran, and the deadlines its party's threads wait
//! until, alone: no other test's threads in this process, nor what a test that ended left
//! running (CI 43990298220, aarch64-apple-darwin: a removal test's wait that could not end was
//! held 21 minutes by what the binary's other tests, all ended, had left; run alone it was
//! refused in 81 ms, ROOT's r669). As a test's wait on a home counts its home's daemon and
//! what descends from it (shards' tests/common `Patience`), never the test's process. A thread
//! of no party (made by another crate, or by std itself) counts for no party's wait, and a
//! wait on a thread of none counts this process's threads, as before parties were.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

thread_local! {
    /// The party this thread is of (0: none), and whether it is one of its members, whose
    /// run its party counts: made by one of its party's threads, or its test's own
    /// ([`of_test`]).
    static PARTY: Cell<(u64, bool)> = const { Cell::new((0, false)) };
    /// Its key among its party's members, where it is one: a wait it makes counts none of
    /// its own run ([`ran`]).
    static KEY: Cell<Option<u64>> = const { Cell::new(None) };
    /// A test's own thread as a member of the party it made ([`of_test`]), to its end.
    static OWN: RefCell<Option<Member>> = const { RefCell::new(None) };
}

static NEXT: AtomicU64 = AtomicU64::new(1);
static NEXT_MEMBER: AtomicU64 = AtomicU64::new(0);

/// A party: its members living, by a key of each one's, each one's clock and what it ran as
/// it waited in a judged wait; and what those that ended ran, less what they ran so.
#[derive(Default)]
struct Party {
    members: BTreeMap<u64, (Clock, u64)>,
    /// Its living members' threads, by the same keys, as another thread of this process
    /// reads their states ([`hold`]).
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    threads: BTreeMap<u64, Thread>,
    ended: u64,
}

static PARTIES: Mutex<BTreeMap<u64, Party>> = Mutex::new(BTreeMap::new());

fn parties() -> MutexGuard<'static, BTreeMap<u64, Party>> {
    PARTIES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The party of this thread, a test's: a new one where it is of none yet, as a test's
/// thread is as it begins (libtest runs each test on a thread of its own), this thread its
/// member from then to its end. What it runs between its waits is its test's progress
/// too, which a member's wait on its next step counts (shards-remote's stand-in hosts wait
/// on its steers): a test's thread its host holds, or runs, holds that wait as D215
/// counts a held host (ROOT, 2026-11-17). What it runs as it waits is none ([`waited`]),
/// and its own waits count none of its run ([`ran`]) and read none of its state
/// ([`hold`]): they are judged as before.
pub fn of_test() -> u64 {
    let id = current();
    if id != 0 {
        return id;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    // Of its party whether or not its clock reads: a member where it does.
    let own = Member::join(id);
    OWN.with(|slot| *slot.borrow_mut() = own);
    id
}

/// The party of this thread; 0 where it is of none.
pub fn current() -> u64 {
    PARTY.with(|party| party.get().0)
}

/// `f`, to run on a thread being made: a member, as it runs, of this thread's party.
pub fn carried<'a, F, T>(f: F) -> impl FnOnce() -> T + Send + 'a
where
    F: FnOnce() -> T + Send + 'a,
    T: 'a,
{
    let party = current();
    move || {
        let _member = Member::join(party);
        f()
    }
}

/// `scope.spawn(f)`, its thread a member of this thread's party.
pub fn spawn_in<'scope, 'env, F, T>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    f: F,
) -> std::thread::ScopedJoinHandle<'scope, T>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    scope.spawn(carried(f))
}

/// `std::thread::spawn(f)`, its thread a member of this thread's party.
pub fn spawn_member<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(carried(f))
}

/// What party `id`'s members ran, the living and those that ended, less what each ran as it
/// waited, in the units of this crate's CPU reads (nanoseconds on Unix, cycles on Windows):
/// all but the thread that asks, where it is one of them, whose own run is none of its own
/// wait's, its clock not read, so that no CPU it spends between its looks reads as its
/// party's.
pub(crate) fn ran(id: u64) -> i128 {
    let asks = KEY.with(Cell::get);
    let parties = parties();
    let Some(party) = parties.get(&id) else {
        return 0;
    };
    let living = party
        .members
        .iter()
        .filter(|(key, _)| Some(**key) != asks)
        .filter_map(|(_, (clock, waited))| Some(clock.ran()?.saturating_sub(*waited)))
        .fold(0, u64::saturating_add);
    i128::from(living.saturating_add(party.ended))
}

/// This thread, waiting judged on party `id`, ran `cpu` more as it waited: none of its
/// party's run as another's wait on it counts it, where it is one of its members (its
/// test's own thread among them, [`of_test`]).
pub(crate) fn waited(id: u64, cpu: u64) {
    let Some(key) = KEY.with(Cell::get) else {
        return;
    };
    if PARTY.with(Cell::get).0 == id
        && let Some((_, waited)) = parties()
            .get_mut(&id)
            .and_then(|party| party.members.get_mut(&key))
    {
        *waited = waited.saturating_add(cpu);
    }
}

/// This thread as a member of party `party`, from its start to its end, when it adds
/// what it ran to what its party's members that ended ran.
struct Member {
    party: u64,
    key: u64,
}

impl Member {
    fn join(party: u64) -> Option<Member> {
        if party == 0 {
            return None;
        }
        // Of its party whatever its clock: the threads it makes are too.
        PARTY.with(|of| of.set((party, false)));
        let clock = Clock::own()?;
        let key = NEXT_MEMBER.fetch_add(1, Ordering::Relaxed);
        {
            let mut parties = parties();
            let of = parties.entry(party).or_default();
            of.members.insert(key, (clock, 0));
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            of.threads.insert(key, Thread::own());
        }
        PARTY.with(|of| of.set((party, true)));
        KEY.with(|of| of.set(Some(key)));
        Some(Member { party, key })
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let mut parties = parties();
        let Some(party) = parties.get_mut(&self.party) else {
            return;
        };
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        party.threads.remove(&self.key);
        if let Some((clock, waited)) = party.members.remove(&self.key) {
            let ran = clock.ran().unwrap_or(0).saturating_sub(waited);
            party.ended = party.ended.saturating_add(ran);
        }
    }
}

/// What party `id`'s living members but the thread that asks are doing as it asks: D215's
/// hold (shards-api `Hold::held_since`, ROOT's rule for runnable threads and `run_delay`),
/// read of threads of this process. Windows keeps its threads' states in the system's whole
/// listing alone, read once for every member (shards-judge's `threads_held`, the rule
/// shards-api's judge holds a VM process's threads by: wanting a CPU, or waiting on its
/// host's paging), 13.1 to 13.8 ms a read (PM M606), so that a wait reads it only at a look
/// that found its party ran nothing.
pub fn hold(id: u64) -> Hold {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Read outside the parties' lock, which every member takes as it starts and ends: a
        // thread gone since reads as none.
        let threads: Vec<(u64, Thread)> = parties()
            .get(&id)
            .map(|party| {
                party
                    .threads
                    .iter()
                    .map(|(&key, &thread)| (key, thread))
                    .collect()
            })
            .unwrap_or_default();
        let asks = Thread::own();
        let mut hold = Hold::default();
        for (key, thread) in threads {
            if thread == asks {
                continue;
            }
            let Some(state) = thread.read() else {
                continue;
            };
            hold.read += 1;
            hold.runnable += usize::from(state.runnable);
            hold.in_kernel += usize::from(state.in_kernel);
            if let Some(queued) = state.queued {
                hold.queued.insert(key, queued);
            }
        }
        hold
    }
    #[cfg(windows)]
    {
        let asks = Thread::own();
        let tids: Vec<u32> = parties()
            .get(&id)
            .map(|party| {
                party
                    .threads
                    .values()
                    .filter(|thread| **thread != asks)
                    .map(|thread| thread.0)
                    .collect()
            })
            .unwrap_or_default();
        if tids.is_empty() {
            return Hold::default();
        }
        let Some(held) = shards_judge::threads_held(std::process::id(), &tids) else {
            return Hold::default();
        };
        Hold {
            read: held.read,
            runnable: held.runnable,
            in_kernel: held.paging,
            ..Hold::default()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = id;
        Hold::default()
    }
}

/// What a look read of a party's members ([`hold`]): how many it read, how many were
/// runnable (running, or waiting for a CPU) and how many waited uninterruptibly in their
/// kernel (on Windows, on their host's paging); and (Linux) each one's wait for a CPU so far,
/// in nanoseconds, by its key.
#[derive(Debug, Default)]
pub struct Hold {
    pub read: usize,
    pub runnable: usize,
    pub in_kernel: usize,
    queued: BTreeMap<u64, u64>,
}

impl Hold {
    /// Whether its host held a member since the look that read `before`, as D215's
    /// `Hold::held_since` reads a VM process's threads: one runnable now, or waiting
    /// uninterruptibly in its kernel now, or one's wait for a CPU grown since (the kernel
    /// adds a wait to `run_delay` as its thread reaches a CPU, so one runnable throughout
    /// shows by its state instead). A daemon's thread waits uninterruptibly in its kernel
    /// only on its host's work (its storage, its pages), never on another of its party.
    pub(crate) fn since(&self, before: &Hold) -> bool {
        self.runnable > 0
            || self.in_kernel > 0
            || self
                .queued
                .iter()
                .any(|(key, now)| before.queued.get(key).is_some_and(|was| now > was))
    }

    /// A stand-in's members as its test says they are: `runnable` of them runnable.
    pub(crate) fn standing(runnable: usize) -> Hold {
        Hold {
            read: runnable,
            runnable,
            ..Hold::default()
        }
    }
}

/// A member's thread as another thread of this process reads its state ([`hold`]): its
/// task's ID on Linux.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Thread(libc::pid_t);

/// A member's thread as another thread of this process reads its state ([`hold`]): its
/// Mach port on macOS (pthread_mach_thread_np(3)), held by its thread while it lives.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Thread(libc::mach_port_t);

/// A member's thread as another thread of this process reads its state ([`hold`]): its ID on
/// Windows, by which the system's listing names it.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Thread(u32);

#[cfg(windows)]
impl Thread {
    fn own() -> Thread {
        // SAFETY: GetCurrentThreadId takes nothing and always succeeds.
        Thread(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() })
    }
}

/// What a read of a member's thread found: runnable, waiting uninterruptibly in its
/// kernel, and (Linux) its wait for a CPU so far, in nanoseconds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct State {
    runnable: bool,
    in_kernel: bool,
    queued: Option<u64>,
}

#[cfg(target_os = "linux")]
impl Thread {
    fn own() -> Thread {
        // SAFETY: gettid(2) takes nothing and always succeeds.
        Thread(unsafe { libc::gettid() })
    }

    /// Its state, `/proc/self/task/<tid>/stat`'s third field (proc(5)): R runnable, D
    /// waiting uninterruptibly; and its `schedstat`'s second, `run_delay`, its wait for a
    /// CPU so far (Documentation/scheduler/sched-stats.rst). None once it has gone.
    fn read(self) -> Option<State> {
        let stat = std::fs::read_to_string(format!("/proc/self/task/{}/stat", self.0)).ok()?;
        let state = stat.rsplit_once(')')?.1.trim_start().bytes().next()?;
        let queued = std::fs::read_to_string(format!("/proc/self/task/{}/schedstat", self.0))
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse().ok());
        Some(State {
            runnable: state == b'R',
            in_kernel: state == b'D',
            queued,
        })
    }
}

#[cfg(target_os = "macos")]
impl Thread {
    fn own() -> Thread {
        // SAFETY: this thread's own pthread, whose port it returns without a new reference.
        Thread(unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) })
    }

    /// Its run state (thread_info(2), THREAD_BASIC_INFO): TH_STATE_RUNNING runnable or
    /// running, TH_STATE_UNINTERRUPTIBLE waiting in its kernel. macOS keeps no thread's
    /// wait for a CPU. None once it has gone.
    fn read(self) -> Option<State> {
        // SAFETY: plain data, for thread_info to fill.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let flavor = libc::thread_flavor_t::try_from(libc::THREAD_BASIC_INFO).ok()?;
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: THREAD_BASIC_INFO of a member's port into a local thread_basic_info of
        // its count's size (mach/thread_info.h); a port its thread has let go of fails.
        let code = unsafe { libc::thread_info(self.0, flavor, (&raw mut info).cast(), &raw mut count) };
        (code == libc::KERN_SUCCESS).then_some(State {
            runnable: info.run_state == libc::TH_STATE_RUNNING,
            in_kernel: info.run_state == libc::TH_STATE_UNINTERRUPTIBLE,
            queued: None,
        })
    }
}

/// A member's CPU clock, which another thread reads while the member lives, in the units of
/// this crate's CPU reads: on Unix its thread's CPU clock in nanoseconds
/// (shards-vmm's `ThreadClock`, D191); on Windows the cycles its
/// thread used (QueryThreadCycleTime), as `ThreadClock` there reads GetThreadTimes, whose
/// count moves in its clock's ticks (15.625 ms), coarser than a W*.
#[cfg(unix)]
struct Clock(shards_vmm::platform::ThreadClock);

#[cfg(windows)]
struct Clock(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
// SAFETY: a thread handle any thread may use; this one is read, and closed, under the
// parties' lock alone.
unsafe impl Send for Clock {}

#[cfg(unix)]
impl Clock {
    fn own() -> Option<Clock> {
        shards_vmm::platform::ThreadClock::current().ok().map(Clock)
    }

    fn ran(&self) -> Option<u64> {
        self.0.read().ok()
    }
}

#[cfg(windows)]
impl Clock {
    fn own() -> Option<Clock> {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: a handle to this thread, by its own ID, closed as its clock goes.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
        (!handle.is_null()).then_some(Clock(handle))
    }

    fn ran(&self) -> Option<u64> {
        use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
        let mut cycles = 0u64;
        // SAFETY: one count into a local, of a handle this clock holds open.
        (unsafe { QueryThreadCycleTime(self.0, &raw mut cycles) } != 0).then_some(cycles)
    }
}

#[cfg(windows)]
impl Drop for Clock {
    fn drop(&mut self) {
        // SAFETY: the handle OpenThread gave this clock, closed once.
        let _ = unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
