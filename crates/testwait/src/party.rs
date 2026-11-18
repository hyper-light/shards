//! A test's party (D215, #77): its test's own thread, and every thread made from one of its
//! party's, by the code under test (the daemon's `DaemonThread`, shards-remote's threads, each
//! through [`carried`] under `cfg(test)`) or by the test ([`spawn_in`], [`spawn_member`]),
//! each of the party of the thread that made it from its making ([`Made`]): held until it first
//! runs, as a thread wanting a CPU is, and a member from then to its end. A test's judged
//! wait counts what its party's members ran, and the deadlines its party's threads wait
//! until, alone: no other test's threads in this process, nor what a test that ended left
//! running (CI 43990298220, aarch64-apple-darwin: a removal test's wait that could not end was
//! held 21 minutes by what the binary's other tests, all ended, had left; run alone it was
//! refused in 81 ms, ROOT's r669). As a test's wait on a home counts its home's daemon and
//! what descends from it (shards' tests/common `Patience`), never the test's process. A thread
//! of no party (made by another crate, or by std itself) counts for no party's wait, and a
//! wait on a thread of none counts this process's threads, as before parties were.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
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
    /// Its living members' threads' names, by the same keys, where they have one: a refusal's
    /// account names them ([`account`]).
    names: BTreeMap<u64, Box<str>>,
    /// The keys of the threads made for it that have not yet run ([`Made`]).
    unstarted: BTreeSet<u64>,
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

/// `f`, to run on a thread being made: of this thread's party from now, made and not yet run
/// ([`Made`]), and a member of it from its first run to its end.
pub fn carried<'a, F, T>(f: F) -> impl FnOnce() -> T + Send + 'a
where
    F: FnOnce() -> T + Send + 'a,
    T: 'a,
{
    let made = Made::new(current());
    move || {
        let _member = made.and_then(Made::join);
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

/// What this thread's clock has counted, as its party reads it, where it is one of its
/// party's members: a mark for [`own_timer_since`].
pub fn mark() -> Option<u64> {
    let key = KEY.with(Cell::get)?;
    let id = PARTY.with(Cell::get).0;
    parties().get(&id)?.members.get(&key)?.0.ran()
}

/// This thread, one of its party's members, ran what its clock counted since `mark` on its own
/// timer alone: a turn that took nothing in, of no peer's and no caller's (a driver's
/// retransmissions, probes and keep-alives). That is no progress of its party's: a wait on
/// what it serves is not held by its own clock, which a driver probing a vanished peer keeps
/// for ever (ROOT, 2026-11-18, #36; CI 44227612442, aarch64 gnu: two of shards-remote's tests
/// held to the step's limit).
pub fn own_timer_since(mark: Option<u64>) {
    if let (Some(from), Some(now)) = (mark, self::mark()) {
        waited(PARTY.with(Cell::get).0, now.saturating_sub(from));
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
        Member::join_as(party, NEXT_MEMBER.fetch_add(1, Ordering::Relaxed))
    }

    /// This thread as member `key` of party `party` from now: no longer one of its unstarted
    /// ([`Made`]), whether or not its clock reads.
    fn join_as(party: u64, key: u64) -> Option<Member> {
        if party == 0 {
            return None;
        }
        // Of its party whatever its clock: the threads it makes are too.
        PARTY.with(|of| of.set((party, false)));
        let clock = Clock::own();
        let name = std::thread::current().name().map(Box::<str>::from);
        {
            let mut parties = parties();
            let of = parties.entry(party).or_default();
            of.unstarted.remove(&key);
            of.members.insert(key, (clock?, 0));
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            of.threads.insert(key, Thread::own());
            if let Some(name) = name {
                of.names.insert(key, name);
            }
        }
        PARTY.with(|of| of.set((party, true)));
        KEY.with(|of| of.set(Some(key)));
        Some(Member { party, key })
    }
}

/// A thread made for party `party` that has not yet run: one of its party's from its making, in
/// the thread that makes it, before its spawn returns, to its first run ([`Made::join`]). Until
/// then it reads as held ([`hold`]), as a member that wants a CPU does: it wants one it has never
/// had. One that joined only as it first ran left a wait begun as it was made a party of its
/// waiter alone, refused at its first look while its host had yet to run the thread its answer
/// needed (ROOT, 2026-11-18, #36: under load, shards-api's `take_judged` 5 of 25 and its
/// session's feeder 3 of 10). One whose thread was never made goes as its closure drops unrun
/// (a spawn that failed).
struct Made {
    party: u64,
    key: u64,
}

impl Made {
    fn new(party: u64) -> Option<Made> {
        if party == 0 {
            return None;
        }
        let key = NEXT_MEMBER.fetch_add(1, Ordering::Relaxed);
        parties().entry(party).or_default().unstarted.insert(key);
        Some(Made { party, key })
    }

    /// Its first run: its party's member from now to its end.
    fn join(self) -> Option<Member> {
        Member::join_as(self.party, self.key)
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if let Some(party) = parties().get_mut(&self.party) {
            party.unstarted.remove(&self.key);
        }
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
        party.names.remove(&self.key);
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
/// that found its party ran nothing. Its threads made and not yet run ([`Made`]) are runnable,
/// on every OS: each wants a CPU it has never had.
pub fn hold(id: u64) -> Hold {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // Read outside the parties' lock, which every member takes as it starts and ends: a
        // thread gone since reads as none. Its unstarted with them, so that one that starts
        // meanwhile is read as one or the other.
        let (threads, unstarted): (Vec<(u64, Thread)>, usize) = parties()
            .get(&id)
            .map(|party| {
                let threads = party
                    .threads
                    .iter()
                    .map(|(&key, &thread)| (key, thread))
                    .collect();
                (threads, party.unstarted.len())
            })
            .unwrap_or_default();
        let asks = Thread::own();
        let mut hold = Hold::made(unstarted);
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
        let (tids, unstarted): (Vec<u32>, usize) = parties()
            .get(&id)
            .map(|party| {
                let tids = party
                    .threads
                    .values()
                    .filter(|thread| **thread != asks)
                    .map(|thread| thread.0)
                    .collect();
                (tids, party.unstarted.len())
            })
            .unwrap_or_default();
        let made = Hold::made(unstarted);
        if tids.is_empty() {
            return made;
        }
        let Some(held) = shards_judge::threads_held(std::process::id(), &tids) else {
            return made;
        };
        Hold {
            read: held.read.saturating_add(made.read),
            runnable: held.runnable.saturating_add(made.runnable),
            in_kernel: held.paging,
            ..Hold::default()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Hold::made(parties().get(&id).map_or(0, |party| party.unstarted.len()))
    }
}

/// What party `id`'s members but the thread that asks were doing as a wait on it was refused,
/// for its account (shards-judge's `Stall::said`), as D215's judge says a VM process's threads:
/// each by its thread's name where it has one, and its key, runnable (running, or wanting a
/// CPU), in its kernel (waiting uninterruptibly there; on Windows, on its host's paging),
/// waiting, or unread (gone since, or its OS's reading failed); and each thread made for it and
/// not yet run ([`Made`]).
pub(crate) fn account(id: u64) -> String {
    let asks = KEY.with(Cell::get);
    let read = parties().get(&id).map(|party| {
        let living: Vec<Living> = party
            .members
            .keys()
            .filter(|key| Some(**key) != asks)
            .map(|&key| Living {
                key,
                name: party.names.get(&key).cloned(),
                #[cfg(any(target_os = "linux", target_os = "macos", windows))]
                thread: party.threads.get(&key).copied(),
            })
            .collect();
        let unstarted: Vec<u64> = party.unstarted.iter().copied().collect();
        (living, unstarted)
    });
    let Some((living, unstarted)) = read else {
        return "no member but its waiter".to_string();
    };
    // Read outside the parties' lock, as a hold is.
    let states = states(&living);
    let mut said: Vec<String> = living
        .iter()
        .map(|member| {
            let state = states.get(&member.key).copied().unwrap_or("unread");
            match &member.name {
                Some(name) => format!("{name} (#{}) {state}", member.key),
                None => format!("#{} {state}", member.key),
            }
        })
        .collect();
    said.extend(unstarted.iter().map(|key| format!("#{key} made, not yet run")));
    if said.is_empty() {
        return "no member but its waiter".to_string();
    }
    said.join(", ")
}

/// A living member as a refusal's account reads it ([`account`]).
struct Living {
    key: u64,
    name: Option<Box<str>>,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    thread: Option<Thread>,
}

/// Each of `living`'s states as its OS reads them now, by its key, as its hold reads them
/// ([`hold`]); none for one unread.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn states(living: &[Living]) -> BTreeMap<u64, &'static str> {
    living
        .iter()
        .filter_map(|member| {
            let state = member.thread?.read()?;
            let said = if state.runnable {
                "runnable"
            } else if state.in_kernel {
                "in its kernel"
            } else {
                "waiting"
            };
            Some((member.key, said))
        })
        .collect()
}

#[cfg(windows)]
fn states(living: &[Living]) -> BTreeMap<u64, &'static str> {
    use shards_judge::threads::{each_thread, pages, records, wants_cpu};
    let mut states = BTreeMap::new();
    let Ok(records) = records() else {
        return states;
    };
    let pid = std::process::id();
    each_thread(&records, |of, t| {
        if of != pid {
            return;
        }
        let tid = u32::try_from(t.ClientId.UniqueThread as usize).unwrap_or(u32::MAX);
        let Some(member) = living
            .iter()
            .find(|member| member.thread.is_some_and(|thread| thread.0 == tid))
        else {
            return;
        };
        let said = if wants_cpu(t.ThreadState) {
            "runnable"
        } else if pages(t.ThreadState, t.WaitReason) {
            "in its kernel"
        } else {
            "waiting"
        };
        states.insert(member.key, said);
    });
    states
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn states(_living: &[Living]) -> BTreeMap<u64, &'static str> {
    BTreeMap::new()
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

    /// A party's `unstarted` threads made and not yet run ([`Made`]), each runnable.
    fn made(unstarted: usize) -> Hold {
        Hold {
            read: unstarted,
            runnable: unstarted,
            ..Hold::default()
        }
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
