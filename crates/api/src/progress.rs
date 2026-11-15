//! Waits that follow a guest's VM process (D215): a wait for a guest's answer goes on while
//! that process runs, and is refused, typed, once it stalls. The daemon's waits on a
//! microVM's own answers follow its VM process so too (D188, its `Wait::WhileRuns`).
//!
//! This process learns each kind of answer ([`Awaited`]) apart, in its own memory, from the
//! answers it sees ([`Record`]): W*, the slowest, from a wait's start to its answer; and R*,
//! the least CPU rate at which a VM process still answered, its CPU over that wait's wall
//! time. A wait that begins once its kind has an answer looks at its VM process once each W*
//! past its start, and a look that finds it ran nothing at all since the look before refuses
//! the wait, [`Refusal::Stalled`]: a host under load slows a VM process but never stops it,
//! so one that ran nothing for as long as the slowest answer took is stopped, deadlocked or
//! frozen. R* is the refusal's diagnosis, never its bound (ROOT, 2026-11-13: a rate below it
//! refused a boot slowed by its host's load). A wait that begins before its kind's first
//! answer ([`Seen::Cold`]) follows its VM process's liveness alone. The schedule and the
//! verdict are [`Judge`]'s; what each look finds is read here.
//!
//! A VM process is read as each OS keeps it, with no VMM linked (D215's falsifier is a client
//! that needs to):
//! - Linux: its CPU clock (clock_getcpuclockid(3), clock_gettime(2)), its threads' run time
//!   in nanoseconds as the scheduler keeps it (kernel/time/posix-cpu-timers.c,
//!   `cpu_clock_sample_group`), read between two reads of its start and state
//!   (`/proc/<pid>/stat` fields 3 and 22, proc(5)), so a pid another process got between
//!   them shows. /proc's own utime and stime count clock ticks (USER_HZ, 100 a second), in
//!   which an answer within two ticks shows no CPU at all.
//! - macOS: proc_pid_rusage(2) (RUSAGE_INFO_V0): its threads' user and system times in Mach
//!   absolute-time units, converted by mach_timebase_info(3) as shards-vmm's
//!   `platform::mach_ns` does, with its start (which guards against a pid given to another
//!   process) and its exit time in the same call. XNU fills those times from
//!   `recount_task_times`, as it fills PROC_PIDTASKINFO's that shards-vmm's
//!   `platform::process_usage` reads, and answers it of a zombie too, so a VM process's last
//!   CPU is read before it is reaped. The duplication of shards-vmm's reader (R12) is the
//!   price of a client that links no VMM.
//! - Windows: the `shards` that booted the VM (the VM process is its child in its Job, D141)
//!   and every descendant, found in a Toolhelp32 snapshot (CreateToolhelp32Snapshot,
//!   Process32FirstW/NextW: `th32ParentProcessID`), a process kept only where it started no
//!   earlier than its listed parent (PM M528's rule: a pid is reused once its process has
//!   gone), each held by a handle of its own: GetProcessTimes' kernel and user times (100 ns
//!   units, summed over a process's threads) for the record, and QueryProcessCycleTime's
//!   count of the cycles their threads used for whether they ran at all, as GetProcessTimes
//!   documents no resolution of its times. A daemon's VM process is followed so, by its pid
//!   ([`Followed::of_pid`]).

use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::Identity;
use crate::judge::{Found, Judge, Record, Seen, Span, Stall};
use crate::{End, ProcessFailure, Refusal};

/// What a wait that follows its guest's VM process waits for. This process learns each
/// apart, as each takes its own time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Awaited {
    /// An `OK` from a guest none of whose connects has been answered yet: its boot to its
    /// first listener ([`Guest::connect_while_runs`](crate::Guest::connect_while_runs)).
    Boot,
    /// An `OK` from a guest that has answered a connect before.
    Connect,
    /// The guest's end ([`Guest::wait_while_runs`](crate::Guest::wait_while_runs)).
    End,
}

/// What this process has learned of each kind of answer, none until its first.
#[derive(Debug)]
pub(crate) struct Records {
    boot: Record,
    connect: Record,
    end: Record,
}

/// This process's records: what every guest it boots or takes back learns from and is
/// judged by.
pub(crate) static RECORDS: Records = Records::new();

impl Records {
    pub(crate) const fn new() -> Records {
        Records {
            boot: Record::new(),
            connect: Record::new(),
            end: Record::new(),
        }
    }

    /// The record of `awaited`'s kind.
    pub(crate) fn of(&self, awaited: Awaited) -> &Record {
        match awaited {
            Awaited::Boot => &self.boot,
            Awaited::Connect => &self.connect,
            Awaited::End => &self.end,
        }
    }

    pub(crate) fn seen(&self, awaited: Awaited) -> Seen {
        self.of(awaited).seen()
    }

    /// An answer of `awaited`'s kind came `wall` after its wait began, its VM process having
    /// run `cpu` meanwhile ([`Record::teach`]): a test's record, taught by hand.
    #[cfg(test)]
    pub(crate) fn teach(&self, awaited: Awaited, wall: Duration, cpu: Duration) {
        self.of(awaited).teach(wall, cpu);
    }
}

/// One look at a VM process.
#[derive(Debug, Clone, Copy)]
struct Look {
    /// The CPU time its threads have had, user and system.
    cpu: Duration,
    /// The cycles its threads have used (Windows): what tells whether they ran.
    #[cfg(windows)]
    cycles: u64,
    /// It, or a process of its tree, has exited since the look before: its end is under way,
    /// so the time since is not judged.
    ending: bool,
}

impl Look {
    /// Whether any of its threads ran between `before` and this look.
    #[cfg(windows)]
    fn ran_since(&self, before: &Look) -> bool {
        self.cycles > before.cycles
    }

    /// Whether any of its threads ran between `before` and this look.
    #[cfg(not(windows))]
    fn ran_since(&self, before: &Look) -> bool {
        self.cpu > before.cpu
    }
}

/// What its host was doing to a VM process as a look saw it, beside its CPU, by which one
/// that ran nothing is told from one its host held (ROOT's ruling of 2026-11-13: a party its
/// host starves is live, and a stopped or frozen one stalled). Each is the process's own, as
/// its OS keeps it (`os::hold`), never what shares a group with it: a sleeper beside a busy
/// neighbour is still (ROOT, 2026-11-13). What cannot be read shows nothing, and that look is
/// judged by its CPU alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Hold {
    /// A thread of it is runnable (running, or waiting for a CPU) as it is looked at; on
    /// Windows also one waiting on its host's memory.
    runnable: bool,
    /// It is stopped (a stop signal's, a debugger's, a suspension's) or frozen.
    stopped: bool,
    /// A thread of it waits uninterruptibly in its kernel as it is looked at (macOS
    /// `TH_STATE_UNINTERRUPTIBLE`, Linux `D`, proc(5)'s "uninterruptible disk sleep"): its
    /// host's own work for it, a disk's flush or a page-in, which no rate of its own runs at
    /// (PM M600: `shards guest use` spent 52 to 172 ms of its 69 to 192 ms so on a loaded
    /// Mac, F_FULLFSYNC's waits, at 14 to 16 ms of CPU). Its look vouches for no window of
    /// its rate ([`vouches`]). Windows' wait reasons for a flush are unread here.
    #[cfg(unix)]
    uninterruptible: bool,
    /// The scheduler's own count of its threads' wait for a CPU.
    #[cfg(unix)]
    queued: Queued,
    /// Linux: the time its cgroup's tasks have waited for a CPU or stalled on memory, in
    /// microseconds (cpu.pressure's and memory.pressure's `some` totals, summed), read only
    /// where that cgroup holds this process alone, as a daemon's VM process its slice (D209):
    /// a shared cgroup's is its neighbours' too.
    #[cfg(target_os = "linux")]
    pressure: Option<u64>,
    /// What its host's storage has done for it so far ([`Io`]).
    io: Io,
}

/// What its host's storage has done for a process so far, as its OS accumulates it: either
/// count grown since the look before says its host did I/O for it meanwhile, which it may
/// have waited on, so that window is its host's hold, never its own rest against R* (D215,
/// PM M601: of the waits in its kernel that a flushing process's samples found, these
/// counts grew across 54 of 55 on macOS, and across 95% of their time on Linux).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Io {
    /// The I/O its host's storage did for it, each count its whole process's and summed, of
    /// which only growth is read: macOS its page-ins, disk bytes read and written and logical
    /// writes (proc_pid_rusage(2) RUSAGE_INFO_V4 `ri_pageins`, `ri_diskio_bytesread`,
    /// `ri_diskio_byteswritten`, `ri_logical_writes`; a directory's flush grows the last
    /// alone); Linux its major faults (`/proc/<pid>/stat` field 12) and storage bytes
    /// (`/proc/<pid>/io` `read_bytes`, `write_bytes`, proc(5)); Windows its I/O operations and
    /// bytes, its pipes' and sockets' among them (GetProcessIoCounters). None where unread.
    done: Option<u64>,
    /// The time it waited on that I/O, where its OS keeps one: Linux its tasks' block I/O
    /// delays (`/proc/<pid>/task/<tid>/stat` field 42, kept only while `kernel.task_delayacct`
    /// is 1; a task gone since takes its delays out of the sum, as `Queued` says of its
    /// run-queue wait) and its own cgroup's `io.pressure` `some` total, read where that cgroup
    /// holds it alone (`Hold::pressure`'s rule). None where its OS keeps none (macOS,
    /// Windows) or keeps it off: its waits between due looks are then sampled ([`Follow`]).
    waited: Option<u64>,
}

impl Io {
    /// Whether either count grew since `before`.
    fn grown_since(&self, before: &Io) -> bool {
        let grown =
            |before: Option<u64>, now: Option<u64>| matches!((before, now), (Some(b), Some(n)) if n > b);
        grown(before.done, self.done) || grown(before.waited, self.waited)
    }
}

/// What a look read of a process's threads' wait for a CPU, as its scheduler counts it: only
/// its growth is read, and only of a look that found it ran no CPU.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Queued {
    /// None read: (Linux) the kernel keeps no schedstat (built without CONFIG_SCHED_INFO), or
    /// keeps it as zeros (fs/proc/base.c, where `sched_info_on()` is false); or the process
    /// went, or could not be read. Its looks are judged by its CPU and its runnable sample
    /// alone.
    #[default]
    Unread,
    /// Linux: its threads' run-queue wait, in nanoseconds, summed: each one's
    /// `/proc/<pid>/task/<tid>/schedstat` second field, `run_delay`
    /// (Documentation/scheduler/sched-stats.rst). macOS: the time its threads have been
    /// runnable, running or waiting for a CPU, in Mach absolute-time units
    /// (proc_pid_rusage(2)'s RUSAGE_INFO_V4 `ri_runnable_time`, sys/resource.h): grown over a
    /// time it ran no CPU, a wait for one.
    Waited(u64),
}

impl Hold {
    /// Whether its host held it since the look that saw `before`: a thread of it runnable,
    /// or waiting on its host's memory, now (macOS: a thread waiting uninterruptibly in its
    /// kernel, of which macOS keeps no pressure record); or its threads' wait for a CPU grown
    /// since; or (Linux) its own cgroup's pressure; or its host's storage's work for it
    /// ([`Io`]).
    ///
    /// The kernel adds a thread's wait to its `run_delay` as it reaches a CPU or leaves the
    /// queue (kernel/sched/stats.h `sched_info_arrive`, `sched_info_dequeue`): a thread that
    /// waited the whole time between two looks shows by its runnable state instead. A thread
    /// gone since takes its wait out of the sum, which then shows growth only past it.
    fn held_since(&self, before: &Hold) -> bool {
        #[cfg(unix)]
        let queued = matches!(
            (before.queued, self.queued),
            (Queued::Waited(b), Queued::Waited(n)) if n > b
        );
        #[cfg(windows)]
        let queued = {
            let _ = before;
            false
        };
        #[cfg(target_os = "linux")]
        let pressed = matches!((before.pressure, self.pressure), (Some(b), Some(n)) if n > b);
        #[cfg(not(target_os = "linux"))]
        let pressed = false;
        #[cfg(target_os = "macos")]
        let in_kernel = self.uninterruptible;
        #[cfg(not(target_os = "macos"))]
        let in_kernel = false;
        self.runnable || in_kernel || queued || pressed || self.io.grown_since(&before.io)
    }

    /// Whether a thread of it waited uninterruptibly in its kernel as it was looked at.
    fn in_kernel(&self) -> bool {
        #[cfg(unix)]
        return self.uninterruptible;
        #[cfg(windows)]
        false
    }
}

/// Whether a look that found `found`, its threads read `starved` meanwhile, its host doing
/// `hold`, its host's storage having worked for it since the look before where `stored`, and
/// its process `ended` or not, vouches for its window as the party's own running or rest
/// (D215's rate, [`Span::work`]): it ran or rested, and was neither starved of a CPU,
/// stopped, ending, waiting in its kernel on its host's work for it as looked at, nor served
/// by its host's storage since the look before ([`Io`]). A party that ran a moment and then
/// waited on its host's disk the rest of a window is held by its host, not slow (ROOT's
/// batch 32, HVF: `shards guest use` refused below R* past C*, 13.2 ms of CPU over 143 ms
/// taken for its own, its one thread blocked). One rule for every judge that rates a party.
fn vouches(found: Found, starved: bool, hold: &Hold, stored: bool, ended: bool) -> bool {
    matches!(found, Found::Progressed | Found::Still)
        && !starved
        && !hold.stopped
        && !ended
        && !hold.in_kernel()
        && !stored
}

/// What a look found of a VM process since the look before: progress where its end is under
/// way (`ending`); else, stopped or frozen, none of its own, whatever it ran or its host did
/// (a stop signal runs a sleeper to stop it: kernel/signal.c `do_signal_stop`); else progress
/// where it `ran`; else held, where its host held it since (`before`, `now`); else none of
/// its own.
fn found(ran: bool, ending: bool, before: &Hold, now: &Hold) -> Found {
    if ending {
        Found::Progressed
    } else if now.stopped {
        Found::Still
    } else if ran {
        Found::Progressed
    } else if now.held_since(before) {
        Found::Held
    } else {
        Found::Still
    }
}

/// The processes their owner holds from running on purpose (D267), each by its pid and start
/// as [`Followed`] reads them: a run its owner paused (`ImageGuest::pause`), its vCPUs held out
/// of its guest by its VMM, which no OS shows as held (its threads sleep as an idle VM's do).
/// Each is held so as long as a [`Held`] of it lives, in this process alone: the process that
/// paused it, whose waits on it are the ones that must not take its pause for a stall.
static HELD: std::sync::Mutex<Vec<(u32, u64)>> = std::sync::Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<(u32, u64)>> {
    HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Processes held by their owner on purpose while this lives (D267): a look of D215's readers
/// that finds one ran nothing finds it [`Found::Held`], not still.
#[derive(Debug)]
pub(crate) struct Held(Vec<(u32, u64)>);

impl Held {
    /// Processes `pids`, each as it is now: one gone already is passed over, as no look will
    /// find it.
    pub(crate) fn of(pids: &[u32]) -> Held {
        let ids: Vec<(u32, u64)> = pids
            .iter()
            .filter_map(|&pid| Followed::of_pid(pid).ok().flatten())
            .map(|f| (f.pid(), f.started()))
            .collect();
        held().extend_from_slice(&ids);
        Held(ids)
    }

    /// Whether `followed` is held by its owner now.
    fn holds(followed: &Followed) -> bool {
        let id = (followed.pid(), followed.started());
        held().contains(&id)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut all = held();
        for id in &self.0 {
            if let Some(i) = all.iter().position(|h| h == id) {
                all.swap_remove(i);
            }
        }
    }
}

/// Why a wait that follows its VM process gave it up, each as [`Refusal`] carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfollowed {
    /// It ran no CPU at all over a whole W* or more since the look before
    /// ([`Refusal::Stalled`]).
    Stalled(Stall),
    /// It could not be read while it lived ([`Refusal::Process`]): never taken for no
    /// progress.
    Process {
        pid: u32,
        expected_started: u64,
        failure: ProcessFailure,
    },
}

impl From<Unfollowed> for Refusal {
    fn from(unfollowed: Unfollowed) -> Refusal {
        match unfollowed {
            Unfollowed::Stalled(stall) => Refusal::Stalled(stall),
            Unfollowed::Process {
                pid,
                expected_started,
                failure,
            } => Refusal::Process {
                pid,
                expected_started,
                failure,
            },
        }
    }
}

/// What a VM process did since the look before, as D215's readers see it: the observation
/// beneath [`Follow`], apart from any judge, for a wait whose slices are a record of its own
/// (the daemon's ends, D217, whose W* its home keeps).
#[derive(Debug)]
pub struct Watch {
    followed: Followed,
    /// The look before, and what its host was found doing to it then.
    last: Look,
    hold: Hold,
    /// The least time a change of its CPU counter stands for ([`Span::resolution`]).
    resolution: Duration,
    /// Its threads as [`starved`](Watch::starved) last read them, none before its first.
    threads: Option<Threads>,
    /// Whether its host's storage worked for it between the look before and the last
    /// ([`Io::grown_since`]).
    stored: bool,
}

/// A process's threads as one read saw them, each by its OS's ID for it: its own CPU time,
/// in nanoseconds, and whether it was runnable on no CPU or running ([`Watch::starved`]).
pub(crate) type Threads = std::collections::BTreeMap<u64, (u64, bool)>;

/// How many of process `pid`'s threads are runnable (running, or waiting for a CPU), of how
/// many, as [`Watch::starved`] reads them: for a waiter's account of its party, so that a
/// refusal names what its members were doing. None where they cannot be read (Windows reads
/// none outside the system's whole listing).
pub fn runnable_threads(pid: u32) -> Option<(usize, usize)> {
    #[cfg(target_os = "linux")]
    let threads = os::threads(pid)?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let threads = os::threads_cpu(pid)?;
    #[cfg(windows)]
    {
        let _ = pid;
        None
    }
    #[cfg(not(windows))]
    Some((
        threads.values().filter(|(_, runnable)| *runnable).count(),
        threads.len(),
    ))
}

/// Whether a thread runnable in `now` had no CPU at all since `before`: its host kept it
/// from a CPU through the whole time between the two reads (or it ran less than its OS's
/// counter shows). A thread that ran any CPU, or that `before` did not see, shows nothing:
/// a process that wakes for a moment's housekeeping each window is never starved by it
/// (the gitssh lane's wedged run, ~750 µs a window, which must stay refusable).
pub(crate) fn starved(before: &Threads, now: &Threads) -> bool {
    now.iter()
        .any(|(id, &(cpu, runnable))| runnable && before.get(id).is_some_and(|&(was, _)| was == cpu))
}

impl Watch {
    /// `followed`, looked at now: none where its process has gone already.
    pub fn of(mut followed: Followed) -> Result<Option<Watch>, ProcessFailure> {
        let Some(resolution) = followed.resolution()? else {
            return Ok(None);
        };
        let Some(last) = followed.look()? else {
            return Ok(None);
        };
        let hold = followed.hold();
        Ok(Some(Watch {
            followed,
            last,
            hold,
            resolution,
            threads: None,
            stored: false,
        }))
    }

    /// Whether its host held a thread of it from any CPU since the call before ([`starved`]):
    /// for a waiter whose party's CPU is its liveness alone, whose looks find it ran (some of
    /// its threads did) while the one its answer waits on sat runnable on no CPU, as a guest of
    /// more vCPUs than its host has CPUs, the host busy besides (ROOT's batch 26, HVF: a
    /// 64-vCPU storm's stream refused 170 ms into its quiet). Its threads are read only here,
    /// so a waiter that never asks pays nothing; false at its first call, and where they
    /// cannot be read (Windows reads none: its threads' states are in the system's whole
    /// listing alone).
    pub fn starved(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        let now = os::threads(self.followed.pid());
        #[cfg(all(unix, not(target_os = "linux")))]
        let now = os::threads_cpu(self.followed.pid());
        #[cfg(windows)]
        let now: Option<Threads> = None;
        let was = std::mem::replace(&mut self.threads, now);
        match (&was, &self.threads) {
            (Some(before), Some(now)) => starved(before, now),
            _ => false,
        }
    }

    /// The least time a change of its CPU counter stands for, as its OS says it
    /// ([`Span::resolution`]): a stillness shorter than it is no evidence of none.
    pub fn resolution(&self) -> Duration {
        self.resolution
    }

    /// What it did since the look before ([`Found`]): progressed where its end is under way;
    /// still where it is stopped or frozen, whatever it ran to its stop; progressed where it
    /// ran; held where its host held it from running; still where it did neither of its own;
    /// gone where its process has.
    pub fn look(&mut self) -> Result<Found, ProcessFailure> {
        let Some(look) = self.followed.look()? else {
            return Ok(Found::Gone);
        };
        let before = std::mem::replace(&mut self.last, look);
        let hold = std::mem::replace(&mut self.hold, self.followed.hold());
        self.stored = self.hold.io.grown_since(&hold.io);
        let ran = look.ran_since(&before);
        // A diagnosis of what Windows shows of a VM process a look finds ran nothing since the
        // look before, the look that may cut it (W2: one inside WHP's partition calls, cut as
        // stalled on CI's windows-2022): said only of those, so a passing run says nothing.
        #[cfg(windows)]
        if !ran && std::env::var_os("SHARDS_JUDGE_TRACE").is_some() {
            self.followed.trace(&look);
        }
        let found = found(ran, look.ending, &hold, &self.hold);
        // Still of its own, unless its owner holds it on purpose (D267): a pause is its host's
        // hold, as a starved thread is, never a stall. A stopped one stays still.
        if found == Found::Still && !self.hold.stopped && Held::holds(&self.followed) {
            return Ok(Found::Held);
        }
        Ok(found)
    }

    /// The CPU time its process's threads have had by the look before.
    pub fn cpu(&self) -> Duration {
        self.last.cpu
    }

    /// Whether the look before found it ended: on Unix its process exited and not yet
    /// reaped (a zombie, whose every look says its end under way), on Windows every process
    /// of its tree exited (their handles read them still). A waiter with no reap of its own
    /// to come (a test's wait on a state, #77 waits-09) takes one found so twice for gone,
    /// rather than for an end under way for ever.
    pub fn ended(&self) -> bool {
        #[cfg(unix)]
        return self.last.ending;
        #[cfg(windows)]
        return self.followed.ended();
    }

    /// Whether the look before found it stopped or frozen (a stop signal's, a debugger's, a
    /// suspension's), which [`look`](Self::look) finds `Still`: for a waiter that counts a
    /// stop its host made as its host's hold, as a test's waits count a kiln runner's
    /// headroom guard suspending them (ROOT, 2026-11-13).
    pub fn stopped(&self) -> bool {
        self.hold.stopped
    }

    /// Whether a look of it that found `found`, its threads read `starved` meanwhile
    /// ([`starved`](Self::starved)), vouches for its window as its own running or rest, its
    /// host not the reason ([`vouches`]).
    pub fn vouched_by(&self, found: Found, starved: bool) -> bool {
        vouches(found, starved, &self.hold, self.stored, self.ended())
    }

    /// What its host is doing to it now, read apart from its looks: a sample between them
    /// ([`Follow`]), which moves none of what its next look compares with.
    fn held_now(&self) -> Hold {
        self.followed.hold()
    }

    /// The CPU time its process's threads have had by now, the look before kept; none where
    /// it has gone or could not be read.
    fn cpu_now(&mut self) -> Option<Duration> {
        self.followed.look().ok().flatten().map(|look| look.cpu)
    }
}

/// What a wait on a VM process holds it to (D215), which every wait names, as what it waits
/// for says: there is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// A reply of bounded work, a protocol's (a VM's READY, its next request for access, a
    /// builtin the daemon asked, a held visit, the network process's start): refused where it
    /// is still a whole W*, and where it ticks on past W* below R* having done more than C*
    /// (b24's shape: a guest asleep on an event that never comes).
    Rate,
    /// An end that comes when its work does (a user's command, a build, a guest's first
    /// listener after its own start): refused only where it is still a whole W*. A guest
    /// asleep by design (`sleep 3600`) ticks on, its caller's to end.
    Stillness,
}

/// A wait that follows a VM process, judged by the record of its kind of answer ([`Judge`])
/// on what each look at the process finds ([`Watch`]), to its [`Bound`].
#[derive(Debug)]
pub struct Follow<'a> {
    judge: Judge<'a>,
    bound: Bound,
    watch: Watch,
    /// Its VM process, for a refusal to name.
    pid: u32,
    started: u64,
    /// What its VM process had run as it began.
    from: Duration,
    /// Whether a sample since its last due look found a thread of its process waiting in its
    /// kernel: that window is its host's, and vouches for nothing.
    held: bool,
    /// When its next sample between due looks is ([`schedule`](Self::schedule)), if any.
    sample: Option<Instant>,
}

impl<'a> Follow<'a> {
    /// A wait beginning now that follows `followed`, learning into and judged by `record` to
    /// `bound`: none where its process has gone already, whose end is the wait's.
    pub fn begin(
        record: &'a Record,
        followed: Followed,
        bound: Bound,
    ) -> Result<Option<Follow<'a>>, Unfollowed> {
        let mut judge = Judge::begin(record);
        let (pid, started) = (followed.pid(), followed.started());
        let began = Instant::now();
        let watched = Watch::of(followed).map_err(|failure| Unfollowed::Process {
            pid,
            expected_started: started,
            failure,
        })?;
        let Some(watch) = watched else {
            return Ok(None);
        };
        // Its evidence begins as its first look has read its process: a based look judges
        // nothing, so refuses nothing.
        let span = Span {
            began,
            ended: Instant::now(),
            resolution: watch.resolution(),
            own: Duration::ZERO,
            work: None,
        };
        judge
            .found_over(Found::Based, span)
            .map_err(Unfollowed::Stalled)?;
        let hold = watch.hold;
        let mut follow = Follow {
            judge,
            bound,
            from: watch.cpu(),
            watch,
            pid,
            started,
            held: false,
            sample: None,
        };
        follow.schedule(&hold, Instant::now());
        Ok(Some(follow))
    }

    /// When it next looks: its judge's due, W* after the look before, or a sample between
    /// ([`schedule`](Self::schedule)) where one comes first; none while it is not judged.
    pub fn due(&self) -> Option<Instant> {
        let due = self.judge.due()?;
        Some(self.sample.map_or(due, |sample| sample.min(due)))
    }

    /// Its next sample between due looks: where its bound is a rate, `hold` found its process
    /// not runnable (every thread of it waiting), and its OS keeps no time it waited on its
    /// host's I/O ([`Io::waited`]), whose sum would show such a wait between looks without
    /// one. Each comes its judge's least window past `at`, the finest its looks resolve
    /// ([`Judge::resolved`]: its counters' tick, or a look's own length where longer), as a
    /// window's evidence is read no finer; none once a sample found its window its host's
    /// (D215, ROOT's ruling of 2026-11-14: a hold between looks was missed). None on Windows,
    /// whose samples could show nothing: a flush and a peer's unanswered pipe both wait
    /// `Executive` there, and one read of the system's records took 13 ms (PM M603).
    fn schedule(&mut self, hold: &Hold, at: Instant) {
        let blocked = !hold.runnable && hold.io.waited.is_none();
        self.sample = (cfg!(unix) && self.bound == Bound::Rate && blocked && !self.held)
            .then(|| at.checked_add(self.judge.resolved()))
            .flatten();
    }

    /// A sample between its due looks: its window its host's where a thread of its process
    /// waits in its kernel, and another to come while its process stays blocked.
    fn sampled(&mut self, at: Instant) {
        let hold = self.watch.held_now();
        self.held |= hold.in_kernel();
        self.schedule(&hold, at);
    }

    /// Looks at its VM process where a look is due: refused `Stalled` where it ran nothing
    /// since the look before of its own, its host not holding it (or it stopped or frozen),
    /// and `Process` where it could not be read while it lives. A process that has gone since
    /// is no longer looked at: its end is the wait's.
    pub fn look(&mut self) -> Result<(), Unfollowed> {
        let Some(due) = self.judge.due() else {
            return Ok(());
        };
        let began = Instant::now();
        if began < due {
            if self.sample.is_some_and(|sample| began >= sample) {
                self.sampled(began);
            }
            return Ok(());
        }
        let (pid, expected_started) = (self.pid, self.started);
        let found = self.watch.look().map_err(|failure| Unfollowed::Process {
            pid,
            expected_started,
            failure,
        })?;
        // Its work since it began, for its rate, where its bound is one and this look, and
        // every sample since the look before, vouches for its window; its threads read only
        // then.
        let held = std::mem::take(&mut self.held);
        let work = (self.bound == Bound::Rate && !held && {
            let starved = self.watch.starved();
            self.watch.vouched_by(found, starved)
        })
        .then(|| self.watch.cpu().saturating_sub(self.from));
        let span = Span {
            began,
            ended: Instant::now(),
            resolution: self.watch.resolution(),
            own: Duration::ZERO,
            work,
        };
        let judged = self.judge.found_over(found, span).map_err(Unfollowed::Stalled);
        let hold = self.watch.hold;
        self.schedule(&hold, Instant::now());
        judged
    }

    /// Its party answered: its kind learns from the answer, its VM process read now; its
    /// slowest alone where it could not be read (D215: only work read through an answer
    /// teaches R* and C*).
    pub fn answered(&mut self) {
        match self.watch.cpu_now() {
            Some(cpu) => self.judge.answered(cpu.saturating_sub(self.from)),
            None => self.judge.answered_unrated(),
        }
    }

    /// Its guest ended as `end`, its VM process having run `cpu` by its end where its watcher
    /// read it before it was reaped: its kind learns from the end, unless this client stopped
    /// it, which is no answer of the guest's.
    pub(crate) fn ended(&mut self, end: &End, cpu: Option<Duration>) {
        if matches!(end, End::Stopped) {
            return;
        }
        // On Windows the tree's handles read its processes after they have ended.
        #[cfg(windows)]
        let cpu = self.watch.cpu_now().or(cpu);
        match cpu {
            Some(cpu) => self.judge.answered(cpu.saturating_sub(self.from)),
            None => self.judge.answered_unrated(),
        }
    }
}

/// A VM process as a wait follows it on Unix: its pid, and the start that tells it from a
/// process given that pid since.
#[cfg(unix)]
#[derive(Debug)]
pub struct Followed {
    pid: u32,
    started: u64,
}

#[cfg(unix)]
impl Followed {
    pub(crate) fn of(identity: &Identity) -> Followed {
        Followed {
            pid: identity.pid,
            started: identity.started,
        }
    }

    /// Process `pid` as it is now, its start read, which tells it from a process given its
    /// pid later; none where no process has it.
    pub fn of_pid(pid: u32) -> Result<Option<Followed>, ProcessFailure> {
        Ok(os::started(pid)?.map(|started| Followed { pid, started }))
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn started(&self) -> u64 {
        self.started
    }

    /// What it has run, now; none where it has gone (its pid another's, or no process's).
    fn look(&mut self) -> Result<Option<Look>, ProcessFailure> {
        os::look(self.pid, self.started)
    }

    /// What its host is doing to it, now ([`Hold`]).
    fn hold(&self) -> Hold {
        os::hold(self.pid)
    }

    /// The least time a change of its CPU counter stands for; none where it has gone.
    fn resolution(&self) -> Result<Option<Duration>, ProcessFailure> {
        os::resolution(self.pid)
    }
}

/// What process `pid` had run by its end, read by its watcher before it is reaped: a child
/// of this process booted and not yet reaped is its alone (`started` none), and one taken back
/// is its only where it is the one that started at `started`. None where it could not be read.
/// On Linux it is what it had run by its exit's notice: what its exit runs after, to its last
/// switch from its CPU, is charged to it later (PM M584).
#[cfg(unix)]
pub(crate) fn ran_by_end(pid: u32, started: Option<u64>) -> Option<Duration> {
    match started {
        None => os::cpu(pid).ok().flatten(),
        Some(started) => os::look(pid, started).ok().flatten().map(|look| look.cpu),
    }
}

#[cfg(windows)]
pub use os::Followed;

#[cfg(target_os = "linux")]
mod os {
    use super::{Hold, Look, Queued};
    use crate::{ProcessFailure, ProcessOperation};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Process `pid`'s look where it is the one that started at `started`; none where it has
    /// gone. Its start is read before and after its CPU clock: a pid another process got
    /// between them shows as that process's start, or as none.
    pub(super) fn look(pid: u32, started: u64) -> Result<Option<Look>, ProcessFailure> {
        let Some((before, _)) = stat(pid)? else {
            return Ok(None);
        };
        let Some(cpu) = cpu(pid)? else {
            return Ok(None);
        };
        let Some((after, ending)) = stat(pid)? else {
            return Ok(None);
        };
        if before != started || after != started {
            return Ok(None);
        }
        Ok(Some(Look { cpu, ending }))
    }

    /// Process `pid`'s start (`/proc/<pid>/stat` field 22); none where no process has it.
    pub(super) fn started(pid: u32) -> Result<Option<u64>, ProcessFailure> {
        Ok(stat(pid)?.map(|(started, _)| started))
    }

    /// What its host is doing to process `pid` ([`Hold`]), from its own evidence alone: a
    /// thread of it runnable, its state `R` (`/proc/<pid>/task/<tid>/stat` field 3, proc(5)),
    /// or waiting uninterruptibly in its kernel, `D`, which vouches for no window of its rate;
    /// it stopped, its leader's state `T` or `t`, or its cgroup frozen (`cgroup.events`'
    /// `frozen 1`, Documentation/admin-guide/cgroup-v2.rst); its threads' run-queue wait
    /// ([`Queued`]); where its cgroup holds it alone, that cgroup's pressure
    /// (Documentation/accounting/psi.rst), whose `some` totals grow while any of its tasks
    /// waits for a CPU, or is stalled on memory, a task held over its `memory.high` among them
    /// (mm/memcontrol.c, `mem_cgroup_handle_over_high`), or waits on I/O (`io.pressure`, its
    /// [`Io::waited`](super::Io)); and its storage's work for it ([`Io`](super::Io)): its major
    /// faults and storage bytes, and its tasks' block I/O delays while task delay accounting is
    /// on (`kernel.task_delayacct`, Documentation/accounting/delay-accounting.rst). A cgroup it
    /// shares, the root's among them (the host's, `/proc/pressure`), is not read: its pressure
    /// is its other tasks' as much as this one's. What it cannot read shows nothing.
    pub(super) fn hold(pid: u32) -> Hold {
        let mut hold = Hold::default();
        let delays = std::fs::read("/proc/sys/kernel/task_delayacct").is_ok_and(|on| on.trim_ascii() == b"1");
        let mut waited: Option<u64> = None;
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                let dir = task.path();
                if let Some(delay) = run_delay(&dir) {
                    hold.queued = Queued::Waited(match hold.queued {
                        Queued::Waited(sum) => sum.saturating_add(delay),
                        Queued::Unread => delay,
                    });
                }
                let Some(stat) = std::fs::read(dir.join("stat")).ok() else {
                    continue;
                };
                let Some(state) = state(&stat) else {
                    continue;
                };
                hold.runnable |= state == b'R';
                hold.uninterruptible |= state == b'D';
                if delays && let Some(ticks) = field(&stat, BLKIO_TICKS) {
                    waited = Some(waited.unwrap_or(0).saturating_add(ticks));
                }
                let leader = task.file_name().to_str().and_then(|tid| tid.parse::<u32>().ok()) == Some(pid);
                hold.stopped |= leader && matches!(state, b'T' | b't');
            }
        }
        if let Some((dir, root)) = cgroup(pid) {
            hold.stopped |= frozen(&dir);
            if !root && alone(&dir, pid) {
                hold.pressure = match (
                    some_total(&dir.join("cpu.pressure")),
                    some_total(&dir.join("memory.pressure")),
                ) {
                    (None, None) => None,
                    (cpu, memory) => Some(cpu.unwrap_or(0).saturating_add(memory.unwrap_or(0))),
                };
                if let Some(io) = some_total(&dir.join("io.pressure")) {
                    waited = Some(waited.unwrap_or(0).saturating_add(io));
                }
            }
        }
        let faults = std::fs::read(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| field(&stat, MAJOR_FAULTS));
        let bytes = std::fs::read_to_string(format!("/proc/{pid}/io"))
            .ok()
            .and_then(|io| {
                let read = |key: &str| {
                    io.lines()
                        .find_map(|line| line.strip_prefix(key)?.trim().parse::<u64>().ok())
                };
                Some(read("read_bytes:")?.saturating_add(read("write_bytes:")?))
            });
        hold.io = super::Io {
            done: match (faults, bytes) {
                (None, None) => None,
                (faults, bytes) => Some(faults.unwrap_or(0).saturating_add(bytes.unwrap_or(0))),
            },
            waited,
        };
        hold
    }

    /// `/proc/<pid>/stat`'s field 12, `majflt`: the faults that loaded a page from storage, of
    /// the whole process where read of its own stat (proc(5)).
    const MAJOR_FAULTS: usize = 12;

    /// `/proc/<pid>/task/<tid>/stat`'s field 42, `delayacct_blkio_ticks`: the task's block I/O
    /// delays, in clock ticks (proc(5)).
    const BLKIO_TICKS: usize = 42;

    /// Field `n` of a stat file (proc(5)'s numbering, from 1), a number, where `n` is past the
    /// state (field 3): counted after the command's last `)`, which may hold any byte.
    fn field(stat: &[u8], n: usize) -> Option<u64> {
        let after = stat.iter().rposition(|byte| *byte == b')')?;
        let fields = std::str::from_utf8(stat.get(after.saturating_add(1)..)?).ok()?;
        fields
            .split_ascii_whitespace()
            .nth(n.checked_sub(3)?)?
            .parse()
            .ok()
    }

    /// Process `pid`'s threads ([`Threads`]): each task's run time, its `schedstat`'s first
    /// field (`sum_exec_runtime`, nanoseconds; a thread's own CPU clock is its own thread
    /// group's alone, posix-cpu-timers.c `pid_for_clock`), and whether its state is `R`
    /// (`/proc/<pid>/task/<tid>/stat` field 3). A task whose schedstat reads as zeros (no
    /// accounting) is left out: its run would not show. None where its tasks cannot be listed.
    pub(super) fn threads(pid: u32) -> Option<super::Threads> {
        let mut threads = super::Threads::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.flatten() {
            let Some(tid) = task.file_name().to_str().and_then(|tid| tid.parse::<u64>().ok()) else {
                continue;
            };
            let dir = task.path();
            let Some(ran) = std::fs::read_to_string(dir.join("schedstat"))
                .ok()
                .and_then(|text| {
                    let mut fields = text.split_whitespace().map(str::parse::<u64>);
                    let (ran, waited, slices) =
                        (fields.next()?.ok()?, fields.next()?.ok()?, fields.next()?.ok()?);
                    (ran != 0 || waited != 0 || slices != 0).then_some(ran)
                })
            else {
                continue;
            };
            let Some(state) = std::fs::read(dir.join("stat")).ok().and_then(|stat| state(&stat)) else {
                continue;
            };
            threads.insert(tid, (ran, state == b'R'));
        }
        Some(threads)
    }

    /// A task's run-queue wait in nanoseconds, its `schedstat`'s second field; none where it
    /// could not be read, or reads as zeros (no accounting, [`Queued::Unread`]).
    fn run_delay(task: &Path) -> Option<u64> {
        let text = std::fs::read_to_string(task.join("schedstat")).ok()?;
        let mut fields = text.split_whitespace().map(str::parse::<u64>);
        let (ran, waited, slices) = (fields.next()?.ok()?, fields.next()?.ok()?, fields.next()?.ok()?);
        (ran != 0 || waited != 0 || slices != 0).then_some(waited)
    }

    /// Whether the cgroup at `dir` holds process `pid` alone: its `cgroup.procs` lists that
    /// pid and no other.
    fn alone(dir: &Path, pid: u32) -> bool {
        std::fs::read_to_string(dir.join("cgroup.procs")).is_ok_and(|procs| {
            let mut listed = procs.split_whitespace();
            listed.next().and_then(|p| p.parse::<u32>().ok()) == Some(pid) && listed.next().is_none()
        })
    }

    /// A task's state, its stat's field 3: the first after its command's last `)`, which may
    /// hold any byte.
    fn state(stat: &[u8]) -> Option<u8> {
        let after = stat.iter().rposition(|byte| *byte == b')')?;
        let fields = stat.get(after.saturating_add(1)..)?;
        fields.iter().copied().find(|byte| !byte.is_ascii_whitespace())
    }

    /// Process `pid`'s cgroup's directory (`/proc/<pid>/cgroup`'s `0::` line, cgroup v2), and
    /// whether it is the root; none where the hierarchy is not v2's.
    fn cgroup(pid: u32) -> Option<(PathBuf, bool)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
        let relative = path.trim().trim_start_matches('/');
        Some((Path::new("/sys/fs/cgroup").join(relative), relative.is_empty()))
    }

    /// Whether the cgroup at `dir` is frozen.
    fn frozen(dir: &Path) -> bool {
        std::fs::read_to_string(dir.join("cgroup.events"))
            .is_ok_and(|events| events.lines().any(|line| line.trim() == "frozen 1"))
    }

    /// A pressure file's `some` line's `total`, in microseconds.
    fn some_total(file: &Path) -> Option<u64> {
        let text = std::fs::read_to_string(file).ok()?;
        let some = text.lines().find_map(|line| line.strip_prefix("some "))?;
        some.split_whitespace()
            .find_map(|field| field.strip_prefix("total="))?
            .parse()
            .ok()
    }

    /// Its start and whether it has exited; none where no process has `pid`: its
    /// `/proc/<pid>` gone (ENOENT), or a stat opened as it went (ESRCH).
    fn stat(pid: u32) -> Result<Option<(u64, bool)>, ProcessFailure> {
        match crate::taken::stat(pid) {
            Ok(stat) => Ok(Some(stat)),
            Err(ProcessFailure::Os { code, .. }) if code == libc::ENOENT || code == libc::ESRCH => Ok(None),
            Err(failure) => Err(failure),
        }
    }

    /// The CPU time process `pid`'s threads have had, as its CPU clock keeps it; none where it
    /// has gone: its clock unknown (ESRCH), or, the clock's ID being its pid, unread as it went
    /// (EINVAL, posix-cpu-timers.c `pid_for_clock`).
    pub(super) fn cpu(pid: u32) -> Result<Option<Duration>, ProcessFailure> {
        read_clock(pid, libc::clock_gettime)
    }

    /// The least time a change of process `pid`'s CPU clock shows: its resolution
    /// (clock_getres(2); one nanosecond for the scheduler's clock that clock_getcpuclockid(3)
    /// names, kernel/time/posix-cpu-timers.c `posix_cpu_clock_getres`), or the kernel's tick
    /// where that is longer. Read from another process, that clock brings only the reader's
    /// own thread up to date: a thread of `pid` running on another CPU is in it as of that
    /// CPU's last tick or scheduler event (kernel/sched/cputime.c `thread_group_cputime`,
    /// `read_sum_exec_runtime`), so between ticks it shows none of that thread's run (CI's
    /// aarch64 run 43575280954: a thread spinning throughout, its process's clock unchanged
    /// for 2.6 ms). The tick is the kernel's own, the resolution of its coarse clock
    /// (clock_getres(2) of CLOCK_MONOTONIC_COARSE: `LOW_RES_NSEC`, one jiffy,
    /// kernel/time/posix-timers.c `posix_get_coarse_res`). None where it has gone.
    pub(super) fn resolution(pid: u32) -> Result<Option<Duration>, ProcessFailure> {
        let Some(clock) = read_clock(pid, libc::clock_getres)? else {
            return Ok(None);
        };
        Ok(Some(clock.max(tick()?)))
    }

    /// The kernel's tick: the resolution of CLOCK_MONOTONIC_COARSE (clock_getres(2)).
    fn tick() -> Result<Duration, ProcessFailure> {
        // SAFETY: a timespec, plain data, for the call to fill.
        let mut at: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_getres(2) of a clock every Linux since 2.6.32 has, into a local.
        if unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC_COARSE, &raw mut at) } != 0 {
            return Err(crate::taken::io_failure(
                ProcessOperation::CpuClock,
                std::io::Error::last_os_error(),
            ));
        }
        timespec(&at)
    }

    /// `read` (clock_gettime(2) or clock_getres(2)) of process `pid`'s CPU clock; none where
    /// it has gone: its clock unknown (ESRCH), or, the clock's ID being its pid, unread as it
    /// went (EINVAL, posix-cpu-timers.c `pid_for_clock`).
    fn read_clock(
        pid: u32,
        read: unsafe extern "C" fn(libc::clockid_t, *mut libc::timespec) -> libc::c_int,
    ) -> Result<Option<Duration>, ProcessFailure> {
        let id = libc::pid_t::try_from(pid).map_err(|_| ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::CpuClock,
        })?;
        let mut clock: libc::clockid_t = 0;
        // SAFETY: clock_getcpuclockid(3) writes one clock ID into a local; it returns its
        // error rather than setting errno.
        match unsafe { libc::clock_getcpuclockid(id, &raw mut clock) } {
            0 => {}
            libc::ESRCH => return Ok(None),
            code => {
                return Err(ProcessFailure::Os {
                    operation: ProcessOperation::CpuClock,
                    code,
                });
            }
        }
        // SAFETY: a timespec, plain data, for the call to fill.
        let mut at: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_gettime(2) or clock_getres(2) of that clock into a local timespec.
        if unsafe { read(clock, &raw mut at) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINVAL) {
                return Ok(None);
            }
            return Err(crate::taken::io_failure(ProcessOperation::CpuClock, e));
        }
        timespec(&at).map(Some)
    }

    /// A timespec as a duration; its nanoseconds are below a second's (POSIX <time.h>).
    fn timespec(at: &libc::timespec) -> Result<Duration, ProcessFailure> {
        let secs = u64::try_from(at.tv_sec).map_err(|_| ProcessFailure::InvalidCpuTime)?;
        let nanos = u32::try_from(at.tv_nsec)
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .ok_or(ProcessFailure::InvalidCpuTime)?;
        Ok(Duration::new(secs, nanos))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod os {
    use super::{Hold, Look, Queued};
    use crate::{ProcessFailure, ProcessOperation};
    use std::time::Duration;

    /// Process `pid`'s look where it is the one that started at `started` (its
    /// `ri_proc_start_abstime`, as [`Identity`](crate::Identity) keeps it); none where it has
    /// gone. A zombie answers with its exit time set.
    pub(super) fn look(pid: u32, started: u64) -> Result<Option<Look>, ProcessFailure> {
        let Some(info) = rusage(pid)? else {
            return Ok(None);
        };
        if info.ri_proc_start_abstime != started {
            return Ok(None);
        }
        Ok(Some(Look {
            cpu: times(&info)?,
            ending: info.ri_proc_exit_abstime != 0,
        }))
    }

    /// The CPU time process `pid`'s threads have had; none where it has gone.
    pub(super) fn cpu(pid: u32) -> Result<Option<Duration>, ProcessFailure> {
        rusage(pid)?.map(|info| times(&info)).transpose()
    }

    /// Process `pid`'s start (`ri_proc_start_abstime`); none where no process has it.
    pub(super) fn started(pid: u32) -> Result<Option<u64>, ProcessFailure> {
        Ok(rusage(pid)?.map(|info| info.ri_proc_start_abstime))
    }

    /// `PROC_PIDLISTTHREADS` (sys/proc_info.h; libc binds no name for it): a process's
    /// threads, a 64-bit handle each, which `PROC_PIDTHREADINFO` takes.
    const PROC_PIDLISTTHREADS: libc::c_int = 6;

    /// What its host is doing to process `pid` ([`Hold`]), read by libproc, which needs no
    /// task port (`task_for_pid` is refused for a hardened process not built to allow it): it
    /// stopped where its status is `SSTOP` (`PROC_PIDTBSDINFO`'s `pbi_status`, sys/proc.h:
    /// "debugging or suspension"); a thread of it runnable where one's run state is
    /// `TH_STATE_RUNNING` (running, or waiting for a CPU), and one waiting in its kernel where
    /// it is `TH_STATE_UNINTERRUPTIBLE` (a page-in, a disk's flush, PM M600, of which macOS
    /// keeps no pressure record), as `PROC_PIDTHREADINFO` reports each (sys/proc_info.h,
    /// mach/thread_info.h); its threads' time runnable ([`Queued`]), which grows over a time
    /// it ran no CPU only while one waited for a CPU, as a thread its host holds through a
    /// look's sample does; and its storage's work for it ([`Io::done`](super::Io)), none of
    /// it a time waited, which macOS keeps no count of. What it cannot read shows nothing.
    pub(super) fn hold(pid: u32) -> Hold {
        let Ok(id) = libc::c_int::try_from(pid) else {
            return Hold::default();
        };
        let states: Vec<libc::c_int> = threads(id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|handle| run_state(id, handle))
            .collect();
        let usage = rusage_v4(id);
        Hold {
            queued: usage.map_or(Queued::Unread, |u| Queued::Waited(u.ri_runnable_time)),
            runnable: states.contains(&libc::TH_STATE_RUNNING),
            uninterruptible: states.contains(&libc::TH_STATE_UNINTERRUPTIBLE),
            stopped: status(id) == Some(libc::SSTOP),
            io: super::Io {
                done: usage.map(|u| {
                    u.ri_pageins
                        .saturating_add(u.ri_diskio_bytesread)
                        .saturating_add(u.ri_diskio_byteswritten)
                        .saturating_add(u.ri_logical_writes)
                }),
                waited: None,
            },
        }
    }

    /// Process `id`'s usage (proc_pid_rusage(2) RUSAGE_INFO_V4): its threads' time runnable,
    /// running or waiting for a CPU (`ri_runnable_time`, Mach absolute-time units), and its
    /// storage's work for it; none where it could not be read.
    fn rusage_v4(id: libc::c_int) -> Option<libc::rusage_info_v4> {
        // SAFETY: a rusage_info_v4, plain data, for proc_pid_rusage to fill.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of `id` into a rusage_info_v4 of ours, which
        // RUSAGE_INFO_V4 names.
        let code = unsafe { libc::proc_pid_rusage(id, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
        (code == 0).then_some(info)
    }

    /// Process `id`'s BSD status (`pbi_status`); none where it could not be read.
    fn status(id: libc::c_int) -> Option<u32> {
        // SAFETY: a proc_bsdinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got = unsafe { libc::proc_pidinfo(id, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (got == size).then_some(info.pbi_status)
    }

    /// Process `id`'s threads' handles, as many as its task counts now
    /// (`PROC_PIDTASKINFO`'s `pti_threadnum`), one more besides to tell a list cut short;
    /// none where they could not be read.
    fn threads(id: libc::c_int) -> Option<Vec<u64>> {
        // SAFETY: a proc_taskinfo, plain data, for proc_pidinfo to fill.
        let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `task`.
        let got = unsafe { libc::proc_pidinfo(id, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size) };
        if got != size {
            return None;
        }
        let count = usize::try_from(task.pti_threadnum).ok()?.checked_add(1)?;
        let mut handles: Vec<u64> = Vec::new();
        handles.try_reserve_exact(count).ok()?;
        handles.resize(count, 0);
        let bytes = libc::c_int::try_from(count.checked_mul(size_of::<u64>())?).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `bytes` bytes, the handles' own, into them.
        let got =
            unsafe { libc::proc_pidinfo(id, PROC_PIDLISTTHREADS, 0, handles.as_mut_ptr().cast(), bytes) };
        let listed = usize::try_from(got).ok()? / size_of::<u64>();
        handles.truncate(listed);
        Some(handles)
    }

    /// Process `pid`'s threads ([`Threads`](super::Threads)), by their handles: each one's user
    /// and system time (`pth_user_time`, `pth_system_time`, nanoseconds, sys/proc_info.h), and
    /// whether its run state is `TH_STATE_RUNNING` (running, or waiting for a CPU). A thread
    /// that ended since it was listed is left out; none where they cannot be listed.
    pub(super) fn threads_cpu(pid: u32) -> Option<super::Threads> {
        let id = libc::c_int::try_from(pid).ok()?;
        let mut read = super::Threads::new();
        for handle in threads(id)? {
            if let Some(info) = thread_info(id, handle) {
                let cpu = info.pth_user_time.saturating_add(info.pth_system_time);
                read.insert(handle, (cpu, info.pth_run_state == libc::TH_STATE_RUNNING));
            }
        }
        Some(read)
    }

    /// Process `id`'s thread `handle` (`PROC_PIDTHREADINFO`); none where it could not be
    /// read, as of a thread that ended since it was listed.
    fn thread_info(id: libc::c_int, handle: u64) -> Option<libc::proc_threadinfo> {
        // SAFETY: a proc_threadinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_threadinfo>()).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got =
            unsafe { libc::proc_pidinfo(id, libc::PROC_PIDTHREADINFO, handle, (&raw mut info).cast(), size) };
        (got == size).then_some(info)
    }

    /// The run state of process `id`'s thread `handle` (`pth_run_state`); none where it could
    /// not be read, as of a thread that ended since it was listed.
    fn run_state(id: libc::c_int, handle: u64) -> Option<libc::c_int> {
        thread_info(id, handle).map(|info| info.pth_run_state)
    }

    fn rusage(pid: u32) -> Result<Option<libc::rusage_info_v0>, ProcessFailure> {
        let id = libc::c_int::try_from(pid).map_err(|_| ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::ProcPidRusage,
        })?;
        // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
        let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of `id` into a rusage_info_v0 of ours, which
        // RUSAGE_INFO_V0 names.
        if unsafe { libc::proc_pid_rusage(id, libc::RUSAGE_INFO_V0, (&raw mut info).cast()) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(crate::taken::io_failure(ProcessOperation::ProcPidRusage, e));
        }
        Ok(Some(info))
    }

    /// Its threads' user and system times, in Mach absolute-time units, as nanoseconds.
    fn times(info: &libc::rusage_info_v0) -> Result<Duration, ProcessFailure> {
        mach(u128::from(info.ri_user_time).saturating_add(u128::from(info.ri_system_time)))
    }

    /// `ticks` of mach_absolute_time(3)'s clock in nanoseconds, by its timebase
    /// (mach_timebase_info(3): 125/3 on Apple silicon, whose counter runs at 24 MHz), as
    /// shards-vmm's `platform::mach_ns`; past u64's nanoseconds, its most.
    fn mach(ticks: u128) -> Result<Duration, ProcessFailure> {
        let (numer, denom) = timebase()?;
        let ns = ticks.saturating_mul(numer) / denom;
        Ok(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)))
    }

    /// The least time a change of a process's CPU times shows: one Mach absolute-time unit,
    /// rounded up to whole nanoseconds (42 ns on Apple silicon's 125/3), the units in which
    /// proc_pid_rusage(2) keeps them; or its kernel's clock tick where that is longer
    /// (`kern.clockrate`'s `tick`, sysctl(3) KERN_CLOCKRATE: 10,000 µs, hz 100, on macOS 26).
    /// XNU adds a thread's run to its task's times as the thread leaves its CPU or enters the
    /// kernel, of which its scheduler's quantum timer makes one each quantum (osfmk
    /// sched_prim.c, `default_preemption_rate`, 100 a second, as hz): a thread running
    /// throughout shows nothing between. Measured
    /// (docs/research/measurements/judge-resolution/rusage-steps.c, Mac17,6, macOS 26, load
    /// 30): a spinning child's times read back to back, each read about 1 µs, grew in steps
    /// of p50 12 µs, p90 182 µs, p99 3.7 ms, max 12.9 ms; the test judge's `Beside` saw none
    /// of a spinning thread within 450 µs windows on HVF. Every process's alike.
    pub(super) fn resolution(_pid: u32) -> Result<Option<Duration>, ProcessFailure> {
        let (numer, denom) = timebase()?;
        let ns = numer.div_ceil(denom);
        let unit = Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX));
        Ok(Some(unit.max(tick()?)))
    }

    /// The kernel's clock tick, `kern.clockrate`'s `tick` (sys/time.h `struct clockinfo`,
    /// microseconds).
    fn tick() -> Result<Duration, ProcessFailure> {
        #[repr(C)]
        struct Clockinfo {
            hz: libc::c_int,
            tick: libc::c_int,
            tickadj: libc::c_int,
            stathz: libc::c_int,
            profhz: libc::c_int,
        }
        let mut info = Clockinfo {
            hz: 0,
            tick: 0,
            tickadj: 0,
            stathz: 0,
            profhz: 0,
        };
        let mut len = size_of::<Clockinfo>();
        // SAFETY: sysctlbyname(3) of a NUL-terminated name, writing at most `len` bytes into
        // `info`, a struct clockinfo's layout, and its length into `len`; nothing is set.
        let code = unsafe {
            libc::sysctlbyname(
                c"kern.clockrate".as_ptr(),
                (&raw mut info).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if code != 0 {
            return Err(crate::taken::io_failure(
                ProcessOperation::ClockRate,
                std::io::Error::last_os_error(),
            ));
        }
        // An answer of another size is no clockinfo, refused as `timebase` refuses a zero
        // denominator: by the call's own code.
        if len != size_of::<Clockinfo>() {
            return Err(ProcessFailure::Os {
                operation: ProcessOperation::ClockRate,
                code,
            });
        }
        let us = u64::try_from(info.tick).map_err(|_| ProcessFailure::InvalidCpuTime)?;
        Ok(Duration::from_micros(us))
    }

    /// mach_timebase_info(3)'s numerator and denominator, the denominator never zero.
    fn timebase() -> Result<(u128, u128), ProcessFailure> {
        // <mach/mach_time.h>'s, bound here as shards-vmm binds it: libc's binding is
        // deprecated in favour of a crate, the call itself is not.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
        }
        let mut base = Timebase { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info(3) fills `base`.
        let code = unsafe { mach_timebase_info(&raw mut base) };
        if code != 0 || base.denom == 0 {
            return Err(ProcessFailure::Os {
                operation: ProcessOperation::MachTimebase,
                code,
            });
        }
        Ok((u128::from(base.numer), u128::from(base.denom)))
    }
}

#[cfg(windows)]
mod os {
    use super::{Hold, Look};
    use crate::taken::io_failure;
    use crate::{ProcessFailure, ProcessOperation};
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::time::Duration;
    use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
    use windows_sys::Wdk::System::SystemServices::{
        FreePage, PageIn, PoolAllocation, Suspended, WrFreePage, WrPageIn, WrPoolAllocation, WrSuspended,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE,
        STATUS_INFO_LENGTH_MISMATCH, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessIoCounters, GetProcessTimes, IO_COUNTERS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;
    use windows_sys::Win32::System::WindowsProgramming::{
        SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION,
    };

    /// A VM process as a wait follows it on Windows: the `shards` that booted it, which holds
    /// it in a Job of its own as its child (D141), and every process that descends from that
    /// `shards`, each held by a handle of its own, which keeps the process, its pid and its
    /// times its own while held, after it has exited too. A daemon's VM process, the root
    /// itself ([`of_pid`](Followed::of_pid)), is followed with whatever descends from it.
    #[derive(Debug)]
    pub struct Followed {
        /// The root's pid and creation time, as a refusal names it.
        pid: u32,
        created: u64,
        /// The root first.
        members: Vec<Member>,
    }

    #[derive(Debug)]
    struct Member {
        pid: u32,
        /// GetProcessTimes' creation time, 100 ns units since 1601.
        created: u64,
        handle: OwnedHandle,
        /// Its exit has been seen.
        exited: bool,
    }

    impl Followed {
        /// The tree whose root `root` holds: process `pid`, created at `created`.
        pub(crate) fn of(root: OwnedHandle, pid: u32, created: u64) -> Result<Followed, ProcessFailure> {
            let mut members = Vec::new();
            members
                .try_reserve(1)
                .map_err(|_| ProcessFailure::CapacityUnavailable)?;
            members.push(Member {
                pid,
                created,
                handle: root,
                exited: false,
            });
            Ok(Followed {
                pid,
                created,
                members,
            })
        }

        /// Process `pid`, held from now, and what descends from it: none where no process has
        /// `pid`. One this user may not open is refused, not taken for gone.
        pub fn of_pid(pid: u32) -> Result<Option<Followed>, ProcessFailure> {
            let Some(root) = Member::open(pid, false)? else {
                return Ok(None);
            };
            let mut members = Vec::new();
            members
                .try_reserve(1)
                .map_err(|_| ProcessFailure::CapacityUnavailable)?;
            let created = root.created;
            members.push(root);
            Ok(Some(Followed {
                pid,
                created,
                members,
            }))
        }

        pub(super) fn pid(&self) -> u32 {
            self.pid
        }

        /// Whether every process of its tree has been seen to exit.
        pub(super) fn ended(&self) -> bool {
            self.members.iter().all(|m| m.exited)
        }

        /// The least time a change of what tells whether its tree ran stands for: none,
        /// as QueryProcessCycleTime counts every cycle its threads use, so that any run
        /// shows ([`Look::ran_since`]).
        pub(super) fn resolution(&self) -> Result<Option<Duration>, ProcessFailure> {
            Ok(Some(Duration::ZERO))
        }

        pub(super) fn started(&self) -> u64 {
            self.created
        }

        /// What its host is doing to its processes ([`Hold`]), from the system's records of
        /// every process and its threads (NtQuerySystemInformation's SystemProcessInformation:
        /// each process's record followed by its threads', their states and wait reasons). A
        /// thread of a live one runnable where its state is ready, running, standby or
        /// transition, or it waits on its host's memory (wdm.h's KWAIT_REASON: FreePage,
        /// PageIn, PoolAllocation and their Wr forms); stopped where every thread of its live
        /// processes is suspended (Suspended, WrSuspended), as a frozen Job's are. What it
        /// cannot read shows nothing.
        pub(super) fn hold(&self) -> Hold {
            let Some(records) = records() else {
                return Hold::default();
            };
            let (mut runnable, mut threads, mut suspended) = (false, 0usize, 0usize);
            each_thread(&records, |pid, t| {
                if !self.members.iter().any(|m| m.pid == pid && !m.exited) {
                    return;
                }
                let (state, reason) = (t.ThreadState, t.WaitReason);
                threads = threads.saturating_add(1);
                let waiting = state == WAITING;
                runnable |=
                    matches!(state, READY | RUNNING | STANDBY | TRANSITION) || (waiting && on_memory(reason));
                suspended = suspended.saturating_add(usize::from(waiting && is_suspension(reason)));
            });
            Hold {
                runnable,
                stopped: threads > 0 && suspended == threads,
                io: super::Io {
                    done: self.io_done(),
                    waited: None,
                },
            }
        }

        /// Its processes' I/O so far, each one's read, write and other operations and their
        /// bytes summed (GetProcessIoCounters, IO_COUNTERS, processthreadsapi.h), an exited one's
        /// as its handle keeps them, so the sum only grows; none where none could be read.
        /// Windows counts its storage's work with its other I/O, a pipe's or a socket's, so a
        /// window in which a process did any I/O vouches for none of its rate ([`Io`]): a wait
        /// reason cannot tell them apart instead, as a thread in FlushFileBuffers and one in a
        /// pipe's read no one answers both wait `Executive` (PM M603), and Windows keeps no time
        /// a thread waited.
        fn io_done(&self) -> Option<u64> {
            self.members.iter().fold(None, |sum, member| {
                let mut counts = IO_COUNTERS::default();
                // SAFETY: GetProcessIoCounters of a process handle this holds, with query
                // access, into an IO_COUNTERS of ours.
                if unsafe { GetProcessIoCounters(member.handle.as_raw_handle(), &raw mut counts) } == 0 {
                    return sum;
                }
                let done = [
                    counts.ReadOperationCount,
                    counts.WriteOperationCount,
                    counts.OtherOperationCount,
                    counts.ReadTransferCount,
                    counts.WriteTransferCount,
                    counts.OtherTransferCount,
                ]
                .into_iter()
                .fold(0u64, u64::saturating_add);
                Some(sum.unwrap_or(0).saturating_add(done))
            })
        }

        /// Says on stderr what a look found of its processes, for a diagnosis of a wait on
        /// Windows (SHARDS_JUDGE_TRACE set): the look's CPU and cycles (GetProcessTimes,
        /// QueryProcessCycleTime) and each thread of its live processes as the system's records
        /// show it now: its process and thread, state and wait reason (KWAIT_REASON), context
        /// switches, and kernel and user time in 100 ns units. What cannot be read is said so.
        /// Each line names its VM process, and all go in one write, so that a log that
        /// interleaves other tests' output keeps each line whole and its own.
        pub(super) fn trace(&self, look: &Look) {
            use std::io::Write as _;
            let mut said = format!(
                "shards judge: VM process {}: cpu {:?}, cycles {}, ending {}",
                self.pid, look.cpu, look.cycles, look.ending
            );
            match records() {
                None => said.push_str("; its threads unread"),
                Some(records) => each_thread(&records, |pid, t| {
                    if self.members.iter().any(|m| m.pid == pid && !m.exited) {
                        said.push_str(&format!(
                            "\nshards judge: VM process {}: thread pid {pid} tid {} state {} reason {} switches {} kernel {} user {}",
                            self.pid,
                            t.ClientId.UniqueThread as usize,
                            t.ThreadState,
                            t.WaitReason,
                            t.Reserved3,
                            t.Reserved1[0],
                            t.Reserved1[1]
                        ));
                    }
                }),
            }
            let _ = writeln!(std::io::stderr(), "{said}");
        }

        /// What its processes have run, now: their handles read them, gone or not.
        pub(super) fn look(&mut self) -> Result<Option<Look>, ProcessFailure> {
            self.grow()?;
            let (mut cpu, mut cycles, mut ending) = (0u64, 0u64, false);
            for member in &mut self.members {
                cpu = cpu.saturating_add(times(&member.handle)?.cpu);
                cycles = cycles.saturating_add(cycles_of(&member.handle)?);
                if !member.exited && exited(&member.handle)? {
                    member.exited = true;
                    ending = true;
                }
            }
            Ok(Some(Look {
                cpu: Duration::from_nanos(cpu.saturating_mul(100)),
                cycles,
                ending,
            }))
        }

        /// Takes in each process of a snapshot of the system's whose listed parent is one of
        /// its processes and that started no earlier than that parent (PM M528: a process
        /// listed under a parent that started after it is no child of that parent, whose pid
        /// it got once the one that started it had gone), until a pass takes in none. One gone
        /// since the snapshot, or not this user's to open, is no process of its tree, all of
        /// which this user runs.
        fn grow(&mut self) -> Result<(), ProcessFailure> {
            let snapshot = Snapshot::take()?;
            loop {
                let mut grew = false;
                let mut entry = snapshot.entry(true)?;
                while let Some(found) = entry {
                    let parent = self
                        .members
                        .iter()
                        .find(|m| m.pid == found.th32ParentProcessID)
                        .map(|m| m.created);
                    let known = self.members.iter().any(|m| m.pid == found.th32ProcessID);
                    if let (Some(parent), false) = (parent, known)
                        && let Some(member) = Member::open(found.th32ProcessID, true)?
                        && member.created >= parent
                    {
                        self.members
                            .try_reserve(1)
                            .map_err(|_| ProcessFailure::CapacityUnavailable)?;
                        self.members.push(member);
                        grew = true;
                    }
                    entry = snapshot.entry(false)?;
                }
                if !grew {
                    return Ok(());
                }
            }
        }
    }

    impl Member {
        /// Process `pid`, held: none where no process has it (ERROR_INVALID_PARAMETER), nor,
        /// one found in a snapshot as a `descendant`, where it is not this user's to open
        /// (ERROR_ACCESS_DENIED), which is no process of the tree.
        fn open(pid: u32, descendant: bool) -> Result<Option<Member>, ProcessFailure> {
            // SAFETY: OpenProcess of `pid` for query and synchronize access alone, its handle
            // not inherited.
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            if raw.is_null() {
                let e = io::Error::last_os_error();
                if is(&e, ERROR_INVALID_PARAMETER) || (descendant && is(&e, ERROR_ACCESS_DENIED)) {
                    return Ok(None);
                }
                return Err(io_failure(ProcessOperation::OpenProcess, e));
            }
            // SAFETY: the handle OpenProcess just made, owned from here alone.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let created = times(&handle)?.created;
            Ok(Some(Member {
                pid,
                created,
                handle,
                exited: false,
            }))
        }
    }

    /// A snapshot of the system's processes, closed as it drops (CloseHandle, as
    /// CreateToolhelp32Snapshot's page asks).
    struct Snapshot(OwnedHandle);

    impl Snapshot {
        fn take() -> Result<Snapshot, ProcessFailure> {
            // SAFETY: a snapshot of every process, its handle owned below.
            let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            if raw == INVALID_HANDLE_VALUE {
                return Err(io_failure(ProcessOperation::Snapshot, io::Error::last_os_error()));
            }
            // SAFETY: the handle just made, owned from here alone.
            Ok(Snapshot(unsafe { OwnedHandle::from_raw_handle(raw) }))
        }

        /// Its first process, or its next; none past its last (ERROR_NO_MORE_FILES).
        fn entry(&self, first: bool) -> Result<Option<PROCESSENTRY32W>, ProcessFailure> {
            let mut entry = PROCESSENTRY32W {
                dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).unwrap_or(u32::MAX),
                ..Default::default()
            };
            let snapshot = self.0.as_raw_handle();
            // SAFETY: the snapshot this holds, and an entry of the size its dwSize says.
            let got = unsafe {
                if first {
                    Process32FirstW(snapshot, &raw mut entry)
                } else {
                    Process32NextW(snapshot, &raw mut entry)
                }
            };
            if got != 0 {
                return Ok(Some(entry));
            }
            let e = io::Error::last_os_error();
            if is(&e, ERROR_NO_MORE_FILES) {
                return Ok(None);
            }
            Err(io_failure(ProcessOperation::ListProcesses, e))
        }
    }

    struct Times {
        created: u64,
        /// Kernel and user, 100 ns units.
        cpu: u64,
    }

    fn times(handle: &OwnedHandle) -> Result<Times, ProcessFailure> {
        let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
        // SAFETY: a process handle this holds open, and four FILETIMEs for the call to fill.
        let ok = unsafe {
            GetProcessTimes(
                handle.as_raw_handle(),
                &raw mut created,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        };
        if ok == 0 {
            return Err(io_failure(
                ProcessOperation::GetProcessTimes,
                io::Error::last_os_error(),
            ));
        }
        let count = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Ok(Times {
            created: count(created),
            cpu: count(kernel).saturating_add(count(user)),
        })
    }

    fn cycles_of(handle: &OwnedHandle) -> Result<u64, ProcessFailure> {
        let mut cycles = 0u64;
        // SAFETY: a process handle this holds open, and a count for the call to fill.
        if unsafe { QueryProcessCycleTime(handle.as_raw_handle(), &raw mut cycles) } == 0 {
            return Err(io_failure(
                ProcessOperation::CycleTime,
                io::Error::last_os_error(),
            ));
        }
        Ok(cycles)
    }

    fn exited(handle: &OwnedHandle) -> Result<bool, ProcessFailure> {
        // SAFETY: a zero-time wait on a process handle this holds, signalled as it exits.
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => Err(io_failure(
                ProcessOperation::WaitForExit,
                io::Error::last_os_error(),
            )),
            returned => Err(ProcessFailure::UnexpectedWait { returned }),
        }
    }

    fn is(e: &io::Error, code: u32) -> bool {
        e.raw_os_error().and_then(|c| u32::try_from(c).ok()) == Some(code)
    }

    // A thread's state in its record (`SYSTEM_THREAD_INFORMATION.ThreadState`), as WMI's
    // Win32_Thread class documents its values: ready (1), running (2) and standby (3) want or
    // have a CPU; transition (6) waits for its kernel stack to come back into memory; waiting
    // (5) says on what by its wait reason.
    const READY: u32 = 1;
    const RUNNING: u32 = 2;
    const STANDBY: u32 = 3;
    const WAITING: u32 = 5;
    const TRANSITION: u32 = 6;

    /// Whether a thread's wait `reason` is on its host's memory.
    fn on_memory(reason: u32) -> bool {
        i32::try_from(reason).is_ok_and(|reason| {
            [
                FreePage,
                PageIn,
                PoolAllocation,
                WrFreePage,
                WrPageIn,
                WrPoolAllocation,
            ]
            .contains(&reason)
        })
    }

    /// Whether a thread's wait `reason` is its suspension.
    fn is_suspension(reason: u32) -> bool {
        i32::try_from(reason).is_ok_and(|reason| [Suspended, WrSuspended].contains(&reason))
    }

    /// The system's process records (NtQuerySystemInformation, SystemProcessInformation), in
    /// words of 8 bytes, as they hold pointers; none where they could not be read. Asked again
    /// with room for what the call said they need, and at least twice the last, until they fit.
    fn records() -> Option<Vec<u64>> {
        let mut words: Vec<u64> = Vec::new();
        loop {
            let bytes = u32::try_from(words.len().checked_mul(size_of::<u64>())?).ok()?;
            let mut needed = 0u32;
            // SAFETY: `bytes` bytes of the words for the call to fill (none, as it is first
            // asked), and a u32 for the size it needs.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    words.as_mut_ptr().cast(),
                    bytes,
                    &raw mut needed,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                let want = usize::try_from(needed)
                    .ok()?
                    .div_ceil(size_of::<u64>())
                    .max(words.len().saturating_mul(2))
                    .max(1);
                words.try_reserve_exact(want.saturating_sub(words.len())).ok()?;
                words.resize(want, 0);
                continue;
            }
            if status < 0 {
                return None;
            }
            let filled = usize::try_from(needed).ok()?.div_ceil(size_of::<u64>());
            words.truncate(filled.min(words.len()));
            return Some(words);
        }
    }

    /// Each thread the records in `words` hold: its process's id and its thread's record
    /// (state, wait reason, times, context switches). A record that would pass the records'
    /// end ends them.
    fn each_thread(words: &[u64], mut thread: impl FnMut(u32, &SYSTEM_THREAD_INFORMATION)) {
        let Some(len) = words.len().checked_mul(size_of::<u64>()) else {
            return;
        };
        // SAFETY: the words' `len` bytes, which any u8 may view, borrowed as long as they are.
        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len) };
        let process = size_of::<SYSTEM_PROCESS_INFORMATION>();
        let each = size_of::<SYSTEM_THREAD_INFORMATION>();
        let mut at = 0usize;
        while let Some(record) = bytes.get(at..).filter(|record| record.len() >= process) {
            // SAFETY: `record` holds a whole SYSTEM_PROCESS_INFORMATION, plain data, read where
            // it lies, aligned or not.
            let info = unsafe {
                record
                    .as_ptr()
                    .cast::<SYSTEM_PROCESS_INFORMATION>()
                    .read_unaligned()
            };
            let pid = u32::try_from(info.UniqueProcessId as usize).unwrap_or(u32::MAX);
            for n in 0..usize::try_from(info.NumberOfThreads).unwrap_or(0) {
                let Some(entry) = n
                    .checked_mul(each)
                    .and_then(|offset| offset.checked_add(process))
                    .and_then(|offset| record.get(offset..))
                    .filter(|entry| entry.len() >= each)
                else {
                    break;
                };
                // SAFETY: `entry` holds a whole SYSTEM_THREAD_INFORMATION, plain data, read
                // where it lies, aligned or not.
                let t = unsafe {
                    entry
                        .as_ptr()
                        .cast::<SYSTEM_THREAD_INFORMATION>()
                        .read_unaligned()
                };
                thread(pid, &t);
            }
            let next = usize::try_from(info.NextEntryOffset).unwrap_or(0);
            match at.checked_add(next) {
                Some(after) if next > 0 => at = after,
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod starved_tests {
    use super::{Threads, starved};

    fn threads(of: &[(u64, u64, bool)]) -> Threads {
        of.iter()
            .map(|&(id, cpu, runnable)| (id, (cpu, runnable)))
            .collect()
    }

    /// A thread runnable now that ran nothing since the read before is its host's hold;
    /// one that ran any CPU, a sleeper, and one the read before did not see are not
    /// (ROOT's batch 26: a 64-vCPU storm's stream; the gitssh lane's ~750 µs a window).
    #[test]
    fn a_thread_runnable_and_unrun_since_the_read_before_is_starved() {
        let before = threads(&[(1, 100, true), (2, 500, false), (3, 900, true)]);
        assert!(starved(&before, &threads(&[(1, 100, true)])), "runnable, unrun");
        assert!(!starved(&before, &threads(&[(1, 101, true)])), "it ran");
        assert!(!starved(&before, &threads(&[(2, 500, false)])), "a sleeper");
        assert!(!starved(&before, &threads(&[(4, 0, true)])), "a thread new since");
        assert!(
            !starved(&before, &threads(&[(1, 175, true), (3, 950, false)])),
            "housekeeping: each ran a moment"
        );
        assert!(
            starved(&before, &threads(&[(1, 175, false), (3, 900, true)])),
            "one of many"
        );
    }

    /// A live process's threads, read twice: a thread sleeping throughout is never starved,
    /// and a thread spinning throughout ran, so is not either.
    #[cfg(unix)]
    #[test]
    fn a_live_processs_threads_read_twice_show_no_starved_sleeper_or_spinner() {
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let _sleeper = scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            let _spinner = scope.spawn(|| {
                let mut spun = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    spun = std::hint::black_box(spun.wrapping_add(1));
                }
            });
            let pid = std::process::id();
            #[cfg(target_os = "linux")]
            let read = || super::os::threads(pid);
            #[cfg(not(target_os = "linux"))]
            let read = || super::os::threads_cpu(pid);
            let before = read().expect("this process's threads");
            std::thread::sleep(std::time::Duration::from_millis(50));
            let now = read().expect("this process's threads");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(now.len() >= 3, "{now:?}");
            assert!(!starved(&before, &now), "{before:?} -> {now:?}");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::Rate;

    /// A process is followed by its pid alone, as the daemon follows its VM process: read
    /// with the start that tells it from a process given its pid later, so that one reaped is
    /// read as gone (or as another's), never as it (D215).
    #[test]
    fn a_process_is_followed_by_its_pid() {
        let me = std::process::id();
        let mut mine = Followed::of_pid(me).unwrap().unwrap();
        assert_eq!(mine.pid(), me);
        assert!(mine.look().unwrap().is_some());
        #[cfg(unix)]
        {
            assert_eq!(mine.started(), crate::taken::started(me).unwrap());
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let mut followed = Followed::of_pid(child.id()).unwrap().unwrap();
            child.wait().unwrap();
            assert!(followed.look().unwrap().is_none(), "reaped, it has gone");
        }
    }

    /// A wait that follows a process by its pid is refused `Stalled` at a look past W* that
    /// finds it ran nothing since the look before, and goes on at each look while it runs
    /// (D215; the daemon's `Wait::WhileRuns`). The sleeper is looked at until it is refused,
    /// the first look or two taking in its start's own CPU. On Windows the sleeper is cmd's
    /// pings of the loopback a second apart, its tree followed with it.
    #[test]
    fn a_process_followed_by_its_pid_is_judged_by_its_cpu() {
        let slowest = Duration::from_millis(20);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(1));
        let until_due = |follow: &Follow<'_>| {
            let due = follow.due().unwrap();
            while Instant::now() < due {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        };
        #[cfg(unix)]
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        #[cfg(windows)]
        let mut sleeper = std::process::Command::new("cmd")
            .args(["/c", "ping -n 61 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        let mut follow = Follow::begin(&record, followed, Bound::Stillness)
            .unwrap()
            .unwrap();
        let refused = loop {
            until_due(&follow);
            if let Err(refused) = follow.look() {
                break refused;
            }
        };
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let Unfollowed::Stalled(stall) = refused else {
            panic!("{refused:?}");
        };
        assert_eq!(stall.slowest(), slowest);
        assert!(stall.over() >= slowest, "{stall:?}");
        let busy = std::sync::atomic::AtomicBool::new(true);
        std::thread::scope(|s| {
            s.spawn(|| {
                while busy.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            let followed = Followed::of_pid(std::process::id()).unwrap().unwrap();
            let mut follow = Follow::begin(&record, followed, Bound::Stillness)
                .unwrap()
                .unwrap();
            for _ in 0..3 {
                until_due(&follow);
                assert_eq!(follow.look(), Ok(()));
            }
            busy.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// A process its owner holds on purpose (D267's pause, [`Held`]) is its host's hold to a
    /// wait that follows it, never a stall: do follow a sleeper whose kind has an answer (W* 20
    /// ms) while a `Held` of it lives, looking past W* again and again; then let it go and look
    /// on. Expect: every look while held goes on (`Found::Held`); once let go, it is held no
    /// more, and refused `Stalled`, as any sleeper is.
    #[test]
    fn a_process_its_owner_holds_is_held_not_stalled() {
        let slowest = Duration::from_millis(20);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(1));
        let until_due = |follow: &Follow<'_>| {
            let due = follow.due().unwrap();
            while Instant::now() < due {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
        };
        #[cfg(unix)]
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        #[cfg(windows)]
        let mut sleeper = std::process::Command::new("cmd")
            .args(["/c", "ping -n 61 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let held = Held::of(&[sleeper.id()]);
        let followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        let mut follow = Follow::begin(&record, followed, Bound::Rate).unwrap().unwrap();
        // Its start's own CPU taken in first, then ten windows held.
        for _ in 0..12 {
            until_due(&follow);
            assert_eq!(follow.look(), Ok(()), "a held process was refused");
        }
        drop(held);
        let refused = loop {
            until_due(&follow);
            if let Err(refused) = follow.look() {
                break refused;
            }
        };
        let let_go = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        assert!(!Held::holds(&let_go), "a hold outlived its Held");
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(matches!(refused, Unfollowed::Stalled(_)), "{refused:?}");
    }

    /// D215's rate bound on real processes, a kind taught W* 200 ms, R* 1/2 and C* 100 ms
    /// (each burst of work 10 ms or more, so that a macOS CPU counter moving in ticks of
    /// 10 ms shows it in every window, and stillness never decides): a process asleep for
    /// good but ticking, 10 ms of work in each 100 ms, is refused for its rate under
    /// `Bound::Rate` and goes on under `Bound::Stillness` (a user's command asleep by design);
    /// a busy one goes on; and one doing its kind's work over more time than any answer took
    /// (a peer it waits on, or a host that slows it: 60 ms of work over some 600 ms) goes on
    /// to its end, its work within C*.
    #[test]
    fn a_rate_bound_refuses_only_a_ticking_sleeper() {
        let ms = Duration::from_millis;
        let record = Record::new();
        record.teach(ms(200), ms(100));
        let started = |mode: &str| {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", SPINNING, "--nocapture"])
                .env(SPIN, mode)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            // Its harness's own lines first ("running 1 test"), then its word. Its stdout is
            // kept open, read on, so that what it says after is no write to a closed pipe.
            let mut lines = std::io::BufRead::lines(std::io::BufReader::new(child.stdout.take().unwrap()));
            assert!(
                lines
                    .by_ref()
                    .any(|line| line.is_ok_and(|line| line.starts_with("started"))),
                "{mode} said nothing of its start"
            );
            std::thread::spawn(move || lines.for_each(drop));
            child
        };
        let followed = |child: &std::process::Child| {
            #[cfg(unix)]
            return Followed::of_pid(child.id()).unwrap().unwrap();
            #[cfg(windows)]
            {
                use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
                let created = crate::taken::created(child.as_raw_handle()).unwrap();
                Followed::of(
                    child.as_handle().try_clone_to_owned().unwrap(),
                    child.id(),
                    created,
                )
                .unwrap()
            }
        };
        // Looked at as each look is due, until refused, its end, or `long` past its start.
        let judged = |child: &mut std::process::Child, bound: Bound, long: Duration| {
            let mut follow = Follow::begin(&record, followed(child), bound).unwrap().unwrap();
            let start = Instant::now();
            loop {
                let due = follow.due().unwrap();
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                if child.try_wait().unwrap().is_some() || start.elapsed() > long {
                    return None;
                }
                if let Err(refused) = follow.look() {
                    return Some((refused, start.elapsed()));
                }
            }
        };
        let end = |mut child: std::process::Child| {
            drop(child.stdin.take());
            let _ = child.kill();
            child.wait().unwrap();
        };
        let mut ticker = started("tick");
        let refused = judged(&mut ticker, Bound::Rate, Duration::from_secs(20));
        end(ticker);
        let Some((Unfollowed::Stalled(stall), at)) = refused else {
            panic!("a ticking sleeper was not refused under its rate: {refused:?}");
        };
        let rate = stall
            .rate()
            .unwrap_or_else(|| panic!("refused for stillness, {at:?} in: {stall:?}"));
        assert!(rate.cpu > ms(100), "{stall:?}");
        let mut ticker = started("tick");
        let kept = judged(&mut ticker, Bound::Stillness, Duration::from_secs(3));
        end(ticker);
        assert!(
            kept.is_none(),
            "a ticking sleeper refused under stillness: {kept:?}"
        );
        let mut spinner = this_binary("spin");
        let kept = judged(&mut spinner, Bound::Rate, Duration::from_secs(3));
        end(spinner);
        assert!(kept.is_none(), "a busy process refused: {kept:?}");
        let mut slow = started("pulse");
        let kept = judged(&mut slow, Bound::Rate, Duration::from_secs(20));
        end(slow);
        assert!(
            kept.is_none(),
            "one doing its kind's work over more time refused: {kept:?}"
        );
    }

    /// What a look finds of a VM process that ran no CPU since the look before is its host's
    /// doing, or its own: held where its host holds it (a thread runnable on no CPU, or
    /// waiting on its host's memory; on Linux, its threads' run-queue wait or its own cgroup's
    /// pressure grown), which is no stall;
    /// still where it waits on itself, and where it is stopped or frozen, whatever else shows.
    /// One whose end is under way progressed; one that ran progressed unless it is stopped,
    /// whose stop ran it (ROOT's ruling of 2026-11-13).
    #[test]
    fn a_process_its_host_holds_is_held_and_a_stopped_one_still() {
        let hold = |runnable, stopped| Hold {
            runnable,
            stopped,
            #[cfg(unix)]
            uninterruptible: false,
            #[cfg(unix)]
            queued: Queued::Unread,
            #[cfg(target_os = "linux")]
            pressure: None,
            io: Io::default(),
        };
        let (idle, throttled, stopped) = (hold(false, false), hold(true, false), hold(true, true));
        assert_eq!(found(true, false, &idle, &idle), Found::Progressed);
        assert_eq!(found(false, true, &idle, &stopped), Found::Progressed);
        assert_eq!(found(false, false, &idle, &throttled), Found::Held);
        assert_eq!(found(false, false, &idle, &idle), Found::Still);
        assert_eq!(found(false, false, &idle, &stopped), Found::Still);
        assert_eq!(
            found(true, false, &idle, &stopped),
            Found::Still,
            "its stop ran it"
        );
        #[cfg(target_os = "linux")]
        {
            let pressed = |total| Hold {
                pressure: Some(total),
                ..Hold::default()
            };
            assert_eq!(found(false, false, &pressed(10), &pressed(11)), Found::Held);
            assert_eq!(found(false, false, &pressed(10), &pressed(10)), Found::Still);
            assert_eq!(
                found(false, false, &idle, &pressed(11)),
                Found::Still,
                "no total before"
            );
        }
        #[cfg(unix)]
        {
            let waited = |ns| Hold {
                queued: Queued::Waited(ns),
                ..Hold::default()
            };
            assert_eq!(found(false, false, &waited(10), &waited(11)), Found::Held);
            assert_eq!(found(false, false, &waited(10), &waited(10)), Found::Still);
            assert_eq!(
                found(false, false, &idle, &waited(11)),
                Found::Still,
                "none read before"
            );
            assert_eq!(found(false, false, &waited(11), &stopped), Found::Still);
        }
    }

    /// A look that finds a thread of its party waiting uninterruptibly in its kernel, its host
    /// doing its work for it (a disk's flush, F_FULLFSYNC), vouches for no window of its rate,
    /// though the party ran a moment in that window: its time there is its host's hold, never
    /// its own rest against R* (D215). ROOT's batch 32 (HVF, adas-mac-studio-3, 2 of 14 cores
    /// idle): `shards guest use`'s kind had learned W* 55.118958 ms, R* 11.873833 ms of CPU in
    /// 55.118958 ms and C* 12.580208 ms, and its wait was refused 224.7 ms in as "too slowly
    /// to be answering", 13.227875 ms of CPU over 142.993081 ms taken for unheld, its one
    /// thread not runnable, blocked. The same looks, its thread found in its kernel at each,
    /// or found resting but its storage's counts grown since the look before (a wait between
    /// looks, D215's ruling of 2026-11-14), are never refused; found vouching, they are, as
    /// that batch's were.
    #[cfg(unix)]
    #[test]
    fn a_party_waiting_on_its_hosts_disk_vouches_for_no_window_of_its_rate() {
        let hold = |uninterruptible| Hold {
            uninterruptible,
            ..Hold::default()
        };
        let (flushing, resting) = (hold(true), hold(false));
        assert!(vouches(Found::Progressed, false, &resting, false, false));
        assert!(vouches(Found::Still, false, &resting, false, false));
        assert!(
            !vouches(Found::Progressed, false, &flushing, false, false),
            "it ran a moment, then waited on its host's disk"
        );
        assert!(!vouches(Found::Still, false, &flushing, false, false));
        assert!(
            !vouches(Found::Progressed, false, &resting, true, false),
            "its storage worked for it between the looks"
        );
        assert!(
            !vouches(Found::Progressed, true, &resting, false, false),
            "starved"
        );
        assert!(
            !vouches(Found::Progressed, false, &resting, false, true),
            "ending"
        );
        let io = |done, waited| Io { done, waited };
        assert!(io(Some(2), None).grown_since(&io(Some(1), None)));
        assert!(io(None, Some(5)).grown_since(&io(None, Some(4))));
        assert!(!io(Some(1), Some(4)).grown_since(&io(Some(1), Some(4))));
        assert!(
            !io(Some(9), None).grown_since(&io(None, None)),
            "none read before"
        );
        let stored = Hold {
            io: io(Some(2), None),
            ..Hold::default()
        };
        let before = Hold {
            io: io(Some(1), None),
            ..Hold::default()
        };
        assert_eq!(
            found(false, false, &before, &stored),
            Found::Held,
            "its host's storage worked for it, though it ran nothing"
        );
        let us = |us: u64| Duration::from_nanos(us * 1000);
        let record = Record::new();
        // W* and R* from the slowest answer; C* from a quicker one, at a rate above R*.
        record.teach(us(55_119), us(11_874));
        record.teach(us(50_000), us(12_580));
        let judged = |hold: &Hold, stored: bool| {
            let mut judge = Judge::begin(&record);
            let began = Instant::now();
            // Its work by each look a W* apart: its hashing at first, then a flush's moments.
            [us(9_000), us(12_100), us(13_228), us(13_300)]
                .into_iter()
                .zip(1u32..)
                .try_for_each(|(cpu, n)| {
                    let at = began + us(56_000) * n;
                    let work = vouches(Found::Progressed, false, hold, stored, false).then_some(cpu);
                    let span = Span {
                        began: at,
                        ended: at,
                        resolution: Duration::ZERO,
                        own: Duration::ZERO,
                        work,
                    };
                    judge.found_over(Found::Progressed, span)
                })
        };
        assert_eq!(judged(&flushing, false), Ok(()), "held by its host, it goes on");
        assert_eq!(
            judged(&resting, true),
            Ok(()),
            "held between its looks, it goes on"
        );
        let refused = judged(&resting, false).expect_err("found vouching, it runs below R* past C*");
        assert!(refused.rate().is_some(), "{refused:?}");
    }

    /// A process whose thread writes a file and flushes it to its disk again and again
    /// (macOS F_FULLFSYNC; Linux fdatasync(2) on a disk, never a tmpfs, whose flush waits on
    /// nothing) is seen waiting uninterruptibly in its kernel (`TH_STATE_UNINTERRUPTIBLE`,
    /// `D`), its look then vouching for no window of its rate, though it ran; and its
    /// storage's counts grow between two looks ([`Io`]: PM M600, M601). Each looked at until
    /// seen so, a wait for that fact, with no clock.
    #[cfg(unix)]
    #[test]
    fn a_process_flushing_to_its_disk_is_seen_waiting_in_its_kernel() {
        let mut child = this_binary("flush");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        let hold = loop {
            let hold = followed.hold();
            if hold.uninterruptible {
                break hold;
            }
            std::thread::yield_now();
        };
        assert!(!vouches(Found::Progressed, false, &hold, false, false));
        while !followed.hold().io.grown_since(&hold.io) {
            std::thread::yield_now();
        }
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    /// On Windows, a process whose thread writes a page and flushes it to its disk again and
    /// again (FlushFileBuffers) shows its storage's work between two looks ([`Io`]), its one
    /// sign there: its wait reason, `Executive`, is a pipe reader's too (PM M603). Looked at
    /// until seen so, a wait for that fact, with no clock.
    #[cfg(windows)]
    #[test]
    fn a_process_flushing_to_its_disk_shows_its_storages_work() {
        let mut child = this_binary("flush");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        let first = loop {
            if let Some(done) = followed.hold().io.done {
                break done;
            }
            std::thread::yield_now();
        };
        let grown = loop {
            let hold = followed.hold();
            if hold.io.done.is_some_and(|done| done > first) {
                break hold;
            }
            std::thread::yield_now();
        };
        let before = Hold {
            io: Io {
                done: Some(first),
                waited: None,
            },
            ..Hold::default()
        };
        assert!(grown.io.grown_since(&before.io));
        assert!(!vouches(Found::Progressed, false, &grown, true, false));
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    /// A wait bound by its rate looks at its process between due looks while it finds it
    /// blocked, where its OS keeps no time it waited on its host's I/O, as often as its judge
    /// resolves; a look there that finds a thread of it waiting in its kernel keeps that
    /// window out of its rate (D215, ROOT's ruling of 2026-11-14: a hold between looks was
    /// missed). A spinner, never blocked, and a wait bound by stillness are looked at only
    /// when due. Where its OS counts its waits, a due look's window shows a flush between
    /// looks by its count instead. Looked at as its follow says, with no clock of the test's.
    #[cfg(unix)]
    #[test]
    fn a_rate_wait_samples_a_blocked_process_between_its_looks() {
        let record = Record::new();
        record.teach(Duration::from_secs(1), Duration::from_millis(100));
        let follow = |pid: u32, bound| {
            Follow::begin(&record, Followed::of_pid(pid).unwrap().unwrap(), bound)
                .unwrap()
                .unwrap()
        };
        let mut spinner = this_binary("spin");
        let spun = loop {
            let spun = follow(spinner.id(), Bound::Rate);
            if spun.watch.hold.runnable {
                break spun;
            }
        };
        assert_eq!(
            spun.due(),
            spun.judge.due(),
            "a runnable process is looked at when due"
        );
        drop(spinner.stdin.take());
        spinner.wait().unwrap();
        let mut flusher = this_binary("flush");
        let still = follow(flusher.id(), Bound::Stillness);
        assert_eq!(still.due(), still.judge.due(), "a stillness bound keeps no rate");
        let mut rate = follow(flusher.id(), Bound::Rate);
        loop {
            let next = rate.due().unwrap();
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
            let between = Instant::now() < rate.judge.due().unwrap();
            rate.look().unwrap();
            if between && rate.held {
                break;
            }
            if rate.watch.hold.io.waited.is_some() && rate.watch.stored {
                break;
            }
        }
        drop(flusher.stdin.take());
        assert!(flusher.wait().unwrap().success());
    }

    /// The readers see what a host does to a process: a sleeping one neither runnable nor
    /// stopped; stopped by SIGSTOP, stopped, and a wait that follows it refused at its first
    /// look past W* with no CPU, its host not the reason (ROOT's ruling of 2026-11-13).
    #[cfg(unix)]
    #[test]
    fn a_stopped_process_is_seen_stopped_and_refused() {
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let pid = sleeper.id();
        let followed = Followed::of_pid(pid).unwrap().unwrap();
        // Once its start's own CPU is spent, it sleeps: neither runnable nor stopped.
        let asleep = loop {
            let hold = followed.hold();
            if !hold.runnable {
                break hold;
            }
            std::thread::yield_now();
        };
        assert!(!asleep.stopped, "{asleep:?}");
        let id = libc::pid_t::try_from(pid).unwrap();
        // SAFETY: kill(2) of this test's own child.
        assert_eq!(unsafe { libc::kill(id, libc::SIGSTOP) }, 0);
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child, to its stop; WNOWAIT leaves it so.
        let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WSTOPPED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        assert!(followed.hold().stopped, "{:?}", followed.hold());
        let slowest = Duration::from_millis(20);
        let record = Record::new();
        record.teach(slowest, Duration::from_millis(1));
        let mut follow = Follow::begin(&record, followed, Bound::Stillness)
            .unwrap()
            .unwrap();
        let refused = loop {
            let due = follow.due().unwrap();
            while Instant::now() < due {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
            }
            if let Err(refused) = follow.look() {
                break refused;
            }
        };
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        let Unfollowed::Stalled(stall) = refused else {
            panic!("{refused:?}");
        };
        assert_eq!(stall.slowest(), slowest);
    }

    /// A process stopped since the look before is still, though its stop ran it: a stop
    /// signal wakes a sleeper to stop itself (kernel/signal.c `do_signal_stop`), CPU of no
    /// progress of its own (ROOT, 2026-11-13). Red while a look that found CPU grown called it
    /// progressed whatever its state: ends.rs's "stopped, it goes on with nothing" on
    /// adacachy2 at ba81cd9d2.
    #[cfg(unix)]
    #[test]
    fn a_process_stopped_since_the_look_before_is_still_though_its_stop_ran_it() {
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let pid = sleeper.id();
        let mut watch = Watch::of(Followed::of_pid(pid).unwrap().unwrap())
            .unwrap()
            .unwrap();
        // Asleep: looked at until a look finds it did nothing since the look before.
        while watch.look().unwrap() != Found::Still {
            std::thread::yield_now();
        }
        let id = libc::pid_t::try_from(pid).unwrap();
        // SAFETY: kill(2) of this test's own child.
        assert_eq!(unsafe { libc::kill(id, libc::SIGSTOP) }, 0);
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child, to its stop; WNOWAIT leaves it so.
        let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WSTOPPED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        let found = watch.look().unwrap();
        let stopped = watch.stopped();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(stopped, "{watch:?}");
        assert_eq!(found, Found::Still, "{watch:?}");
    }

    /// macOS's count of a process's threads' time runnable is read (#77): a spinner's grows,
    /// running counting, and a sleeper's does not, so that a look that finds it ran no CPU
    /// reads its growth as a wait for a CPU. The spinner read until seen so; the sleeper over a
    /// pause, its own CPU the witness of a wake meanwhile.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_processs_runnable_time_is_read() {
        let waited = |followed: &Followed| match followed.hold().queued {
            Queued::Waited(ticks) => ticks,
            Queued::Unread => panic!("unread: {:?}", followed.hold()),
        };
        let mut child = this_binary("spin");
        let spinner = Followed::of_pid(child.id()).unwrap().unwrap();
        let first = waited(&spinner);
        while waited(&spinner) <= first {
            std::thread::yield_now();
        }
        drop(child.stdin.take());
        child.wait().unwrap();
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        while followed.hold().runnable {
            std::thread::yield_now();
        }
        let asleep = waited(&followed);
        let cpu = os::cpu(sleeper.id()).unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let (after, ran) = (waited(&followed), os::cpu(sleeper.id()).unwrap().unwrap());
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(after == asleep || ran > cpu, "{asleep} {after} {cpu:?} {ran:?}");
    }

    /// A process whose thread spins is seen runnable: running, or waiting for a CPU (ROOT's
    /// ruling of 2026-11-13). Looked at until seen so, a wait for that fact, with no clock.
    #[cfg(unix)]
    #[test]
    fn a_spinning_process_is_seen_runnable() {
        let mut child = this_binary("spin");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        while !followed.hold().runnable {
            std::thread::yield_now();
        }
        assert!(!followed.hold().stopped);
        drop(child.stdin.take());
        child.wait().unwrap();
    }

    /// A sleeping process is still, whatever the processes beside it in its cgroup wait for:
    /// their wait is no evidence of its own (D215). Two spinners pinned to one CPU wait on each
    /// other in the cgroup this test shares with the sleeper, which ran nothing meanwhile. Red
    /// while a shared cgroup's pressure counted as the sleeper's (main's CI at 5af644eab:
    /// ends.rs's `/bin/sleep 60` judged going on).
    ///
    /// Judged over a window in which the sleeper was asleep at either end and ran nothing
    /// between, as its looks show, and looked at again over another: a sleeper is woken by a
    /// cgroup freezer's cycles, which a kiln run's cgroup goes through about six times a
    /// second, each a few µs of its CPU (PM M562), so no window is a sleeper's by its clock.
    /// Each window is the spinners' start and the shared pressure's growth; they end past it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sleeper_is_still_whatever_its_cgroups_others_wait_for() {
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        let cpu = first_cpu();
        let (before, now, shared) = loop {
            let ran = followed.look().unwrap().unwrap();
            let before = followed.hold();
            let shared = shared_cpu_pressure(sleeper.id());
            let mut spinners = [spinner_on(cpu), spinner_on(cpu)];
            // Waited for, where the kernel keeps it: the spinners' wait for their CPU shown in
            // the pressure the sleeper's cgroup shares.
            if let Some(from) = shared {
                while shared_cpu_pressure(sleeper.id()).is_none_or(|now| now <= from) {
                    std::thread::yield_now();
                }
            }
            let now = followed.hold();
            let asleep =
                !followed.look().unwrap().unwrap().ran_since(&ran) && !before.runnable && !now.runnable;
            for spinner in &mut spinners {
                drop(spinner.stdin.take());
                spinner.wait().unwrap();
            }
            if asleep {
                break (before, now, shared);
            }
        };
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(
            found(false, false, &before, &now),
            Found::Still,
            "{before:?} {now:?} (shared pressure from {shared:?})"
        );
    }

    /// A process runnable on a CPU that others hold is held by its host (D215): it spins
    /// pinned to one CPU beside two spinners pinned there too, so that the CPU always has one
    /// waiting. Looked at until its hold shows, a wait for that fact.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_starved_of_its_cpu_is_held() {
        let cpu = first_cpu();
        let mut starved = spinner_on(cpu);
        let mut others = [spinner_on(cpu), spinner_on(cpu)];
        let followed = Followed::of_pid(starved.id()).unwrap().unwrap();
        let before = followed.hold();
        // Its own wait grown where the kernel keeps it, else its runnable sample.
        let now = loop {
            let now = followed.hold();
            let shown = match (before.queued, now.queued) {
                (Queued::Waited(b), Queued::Waited(n)) => n > b,
                _ => now.runnable,
            };
            if shown {
                break now;
            }
            std::thread::yield_now();
        };
        assert_eq!(now.pressure, None, "this test's cgroup is shared: {now:?}");
        for child in others.iter_mut().chain([&mut starved]) {
            drop(child.stdin.take());
            child.wait().unwrap();
        }
        assert_eq!(
            found(false, false, &before, &now),
            Found::Held,
            "{before:?} {now:?}"
        );
    }

    /// The first CPU this process may run on (sched_getaffinity(2)).
    #[cfg(target_os = "linux")]
    fn first_cpu() -> usize {
        // SAFETY: a cpu_set_t, plain data, for sched_getaffinity to fill.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: sched_getaffinity(2) of this thread into a local set of its size.
        let got = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut set) };
        assert_eq!(got, 0);
        let cpus = usize::try_from(libc::CPU_SETSIZE).unwrap();
        // SAFETY: CPU_ISSET reads a bit of a set filled above, below CPU_SETSIZE.
        (0..cpus)
            .find(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
            .unwrap()
    }

    /// This test binary spinning until its stdin ends, each of its threads pinned to `cpu`
    /// alone: pinned before its exec, so that every thread it starts (the test harness's that
    /// spins among them) keeps that affinity.
    #[cfg(target_os = "linux")]
    fn spinner_on(cpu: usize) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: a cpu_set_t, plain data; CPU_SET sets a bit below CPU_SETSIZE in it.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::CPU_SET(cpu, &mut set) };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", SPINNING, "--nocapture"])
            .env(SPIN, "spin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null());
        // SAFETY: sched_setaffinity(2) of the child itself, a system call, which is safe
        // between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        command.spawn().unwrap()
    }

    /// The `some` total of the CPU pressure that process `pid`'s cgroup shows, as a judge that
    /// read any cgroup's pressure read it: its cgroup's `cpu.pressure`, or the host's where it
    /// is the root (as in a container's own namespace); none where none is kept.
    #[cfg(target_os = "linux")]
    fn shared_cpu_pressure(pid: u32) -> Option<u64> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
        let relative = path.trim().trim_start_matches('/');
        let file = if relative.is_empty() {
            std::path::PathBuf::from("/proc/pressure/cpu")
        } else {
            std::path::Path::new("/sys/fs/cgroup")
                .join(relative)
                .join("cpu.pressure")
        };
        let text = std::fs::read_to_string(file).ok()?;
        let some = text.lines().find_map(|line| line.strip_prefix("some "))?;
        some.split_whitespace()
            .find_map(|field| field.strip_prefix("total="))?
            .parse()
            .ok()
    }

    /// Each kind learns apart, from its first answer on: its slowest answer only rises, its
    /// least rate only falls, and a wait no time long teaches nothing (D215).
    #[test]
    fn a_kind_learns_its_slowest_answer_and_its_least_rate_apart() {
        let ms = Duration::from_millis;
        let records = Records::new();
        assert_eq!(records.seen(Awaited::Boot), Seen::Cold);
        records.teach(Awaited::Boot, ms(2), ms(1));
        let learned = |slowest, cpu, over| Seen::Learned {
            slowest,
            rated: Some(crate::Rated {
                least: Rate { cpu, over },
                most: ms(1),
            }),
        };
        assert_eq!(records.seen(Awaited::Boot), learned(ms(2), ms(1), ms(2)));
        records.teach(Awaited::Boot, ms(1), ms(1));
        assert_eq!(records.seen(Awaited::Boot), learned(ms(2), ms(1), ms(2)));
        records.teach(Awaited::Boot, ms(4), ms(1));
        assert_eq!(records.seen(Awaited::Boot), learned(ms(4), ms(1), ms(4)));
        records.teach(Awaited::Boot, Duration::ZERO, Duration::ZERO);
        assert_eq!(records.seen(Awaited::Boot), learned(ms(4), ms(1), ms(4)));
        assert_eq!(records.seen(Awaited::Connect), Seen::Cold);
        assert_eq!(records.seen(Awaited::End), Seen::Cold);
    }

    /// Set in the children of `the_cpu_reader_sees_a_spinning_childs_cpu_grow`: `spin`
    /// spins until its stdin ends; `parent` (Windows) starts a spinner and waits on its own
    /// stdin, the spinner its grandchild, so the walk of descendants is what reads it.
    const SPIN: &str = "SHARDS_API_PROGRESS_SPIN";
    const SPINNING: &str = "progress::tests::the_cpu_reader_sees_a_spinning_childs_cpu_grow";

    /// This test binary, run as one of its own children in `mode`.
    fn this_binary(mode: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", SPINNING, "--nocapture"])
            .env(SPIN, mode)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The reader sees a spinning process's CPU grow: on Unix the process itself, on Windows
    /// a grandchild the walk of descendants finds. Looked at until it has grown, a wait for
    /// that fact, with no clock (D215).
    #[test]
    fn the_cpu_reader_sees_a_spinning_childs_cpu_grow() {
        match std::env::var(SPIN).as_deref() {
            Ok("spin") => {
                std::thread::spawn(|| {
                    let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                    std::process::exit(0);
                });
                loop {
                    std::hint::spin_loop();
                }
            }
            // Written to a file of its own and flushed to its disk, again and again until its
            // stdin ends: a thread that waits on its host's disk, as `record`'s in guest.rs
            // does (PM M600, M601). macOS: a page, then F_FULLFSYNC (fcntl(2)), as `record`.
            // Linux: a MiB, then fdatasync(2), to a file beside this test binary, on the disk
            // cargo built it on: $TMPDIR may be a tmpfs (adacachy2's is), whose flush waits on
            // nothing; a MiB keeps the device writing at each flush (on adacachy2's NVMe, 100
            // flushes of a MiB waited in `D` 117 times for 1 ms or more, PM M601). Windows: a
            // page, then FlushFileBuffers (std's `sync_data`), as `record` there (PM M603).
            Ok("flush") => {
                let ended = std::sync::atomic::AtomicBool::new(false);
                #[cfg(not(target_os = "linux"))]
                let (dir, chunk) = (shards_testdir::TempDir::new("flush").unwrap(), 4096);
                #[cfg(not(target_os = "linux"))]
                let path = dir.join("flushed");
                #[cfg(target_os = "linux")]
                let (path, chunk) = (
                    std::env::current_exe()
                        .unwrap()
                        .with_file_name(format!("flushed-{}", std::process::id())),
                    1 << 20,
                );
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::ffi::OsStrExt as _;
                    let at = std::ffi::CString::new(path.parent().unwrap().as_os_str().as_bytes()).unwrap();
                    // SAFETY: a statfs, plain data, for statfs(2) to fill from a path of ours.
                    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
                    // SAFETY: statfs(2) of a NUL-terminated path into a statfs of ours.
                    assert_eq!(unsafe { libc::statfs(at.as_ptr(), &raw mut fs) }, 0);
                    assert_ne!(
                        i128::from(fs.f_type),
                        i128::from(libc::TMPFS_MAGIC),
                        "{}: a tmpfs, its flush no wait",
                        path.display()
                    );
                }
                let file = std::fs::File::create(&path).unwrap();
                let bytes = vec![1u8; chunk];
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                        ended.store(true, std::sync::atomic::Ordering::Relaxed);
                    });
                    // Written over its start each time, so the file stays one chunk long.
                    while !ended.load(std::sync::atomic::Ordering::Relaxed) {
                        #[cfg(unix)]
                        std::os::unix::fs::FileExt::write_all_at(&file, &bytes, 0).unwrap();
                        #[cfg(windows)]
                        assert_eq!(
                            std::os::windows::fs::FileExt::seek_write(&file, &bytes, 0).unwrap(),
                            chunk
                        );
                        #[cfg(target_os = "macos")]
                        {
                            use std::os::fd::AsRawFd as _;
                            // SAFETY: fcntl(2) of a descriptor this holds open.
                            assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) }, 0);
                        }
                        #[cfg(not(target_os = "macos"))]
                        file.sync_data().unwrap();
                    }
                });
                drop(file);
                #[cfg(not(target_os = "linux"))]
                drop(dir);
                #[cfg(target_os = "linux")]
                std::fs::remove_file(&path).unwrap();
                std::process::exit(0);
            }
            // D215's rate bound's children, each saying it has started first: `tick`, 10 ms
            // of work in each 100 ms until its stdin ends; `pulse`, 10 ms of work in each
            // 100 ms six times, then its end.
            Ok(mode @ ("tick" | "pulse")) => {
                use std::io::Write as _;
                if mode == "tick" {
                    std::thread::spawn(|| {
                        let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                        std::process::exit(0);
                    });
                }
                let _ = writeln!(std::io::stdout(), "started");
                let _ = std::io::stdout().flush();
                let rounds = if mode == "tick" { u32::MAX } else { 6 };
                for _ in 0..rounds {
                    let until = Instant::now() + Duration::from_millis(10);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    std::thread::sleep(Duration::from_millis(90));
                }
                return;
            }
            Ok("parent") => {
                let mut spinner = this_binary("spin");
                let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                drop(spinner.stdin.take());
                let _ = spinner.wait();
                return;
            }
            _ => {}
        }
        #[cfg(unix)]
        let mut child = this_binary("spin");
        #[cfg(windows)]
        let mut child = this_binary("parent");
        let pid = child.id();
        #[cfg(unix)]
        let mut followed = Followed::of(&Identity {
            pid,
            started: crate::taken::started(pid).unwrap(),
            vsock: "unused".into(),
        });
        #[cfg(windows)]
        let mut followed = {
            use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
            let created = crate::taken::created(child.as_raw_handle()).unwrap();
            Followed::of(child.as_handle().try_clone_to_owned().unwrap(), pid, created).unwrap()
        };
        let first = followed.look().unwrap().unwrap();
        let grown = loop {
            let look = followed.look().unwrap().unwrap();
            if look.ran_since(&first) && look.cpu > first.cpu {
                break look;
            }
            std::thread::yield_now();
        };
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(!first.ending && !grown.ending);
    }

    /// A VM process that has exited and is not yet reaped is read with its end under way, its
    /// last CPU readable, which a wait for its end learns from; once reaped, it is gone (D215).
    #[cfg(unix)]
    #[test]
    fn an_exited_process_is_read_until_it_is_reaped() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let started = crate::taken::started(pid).unwrap();
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child into `info`; WNOWAIT leaves it unreaped.
        let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        let mut followed = Followed { pid, started };
        let look = followed.look().unwrap().unwrap();
        assert!(look.ending, "a zombie's look says its end is under way");
        // Linux tells its exit (waitid, its state `Z`) before its last switch from its CPU,
        // which charges its clock with what its exit ran after (kernel/exit.c, `do_exit`'s
        // `exit_notify` before `do_task_dead`): a later read is its own, and no less (PM M584).
        let by_end = ran_by_end(pid, None).unwrap();
        assert!(by_end >= look.cpu, "{by_end:?} read after {:?}", look.cpu);
        let its_own = ran_by_end(pid, Some(started)).unwrap();
        assert!(its_own >= by_end, "{its_own:?} read after {by_end:?}");
        assert_eq!(ran_by_end(pid, Some(started.wrapping_add(1))), None);
        child.wait().unwrap();
        assert!(followed.look().unwrap().is_none(), "reaped, it has gone");
    }

    /// A child of this test's running `script` in a shell: its pid once waitid(2) has seen it
    /// stopped, which leaves it as it is (WNOWAIT).
    #[cfg(target_os = "macos")]
    fn stopped(script: &str) -> (std::process::Child, libc::pid_t) {
        let child = std::process::Command::new("sh")
            .args(["-c", script])
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child, to its stop; WNOWAIT leaves it as it is.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &raw mut info,
                libc::WSTOPPED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0);
        (child, pid)
    }

    /// A child that counts in its shell, then stops itself (SIGSTOP), and again whenever it is
    /// continued, a hang-up, an interrupt and a terminate ignored, so that only this test's
    /// SIGKILL ends it. A run's end orphans the process group a test's children are in, and
    /// where one of them is stopped the kernel sends each of them SIGHUP, then SIGCONT (POSIX
    /// `_exit`'s orphaned process groups): one that stopped itself once was continued and hung
    /// up, gone between this test's two reads (PM M554).
    #[cfg(target_os = "macos")]
    fn stopped_child() -> (std::process::Child, libc::pid_t) {
        stopped(
            "trap '' HUP INT TERM; i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done; \
             while :; do kill -STOP $$; done",
        )
    }

    /// Process `pid`'s user and system times as proc_pid_rusage(3) says them, a zombie's too.
    #[cfg(target_os = "macos")]
    fn rusage_times(pid: libc::pid_t) -> (u64, u64) {
        // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
        let mut rusage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of a child of this test's into a rusage_info_v0 of ours.
        let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut rusage).cast()) };
        assert_eq!(r, 0);
        (rusage.ri_user_time, rusage.ri_system_time)
    }

    /// Process `pid`'s user and system times as PROC_PIDTASKINFO says them, the ones
    /// shards-vmm's `platform::process_usage` reads: none where it has no task, gone.
    #[cfg(target_os = "macos")]
    fn task_times(pid: libc::pid_t) -> Option<(u64, u64)> {
        // SAFETY: a zeroed proc_taskinfo is a valid out-parameter, which proc_pidinfo fills.
        let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).unwrap();
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `task`.
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size) };
        (got == size).then_some((task.pti_total_user, task.pti_total_system))
    }

    /// The two readers' times of a stopped process, read where it ran nothing between them:
    /// PROC_PIDTASKINFO's the same on either side of proc_pid_rusage's read, as one continued
    /// runs until it stops itself again.
    #[cfg(target_os = "macos")]
    fn times_still(pid: libc::pid_t) -> ((u64, u64), (u64, u64)) {
        loop {
            let before = task_times(pid).expect("a stopped child gone");
            let rusage = rusage_times(pid);
            if task_times(pid) == Some(before) {
                return (rusage, before);
            }
        }
    }

    /// proc_pid_rusage's user and system times are PROC_PIDTASKINFO's, the ones shards-vmm's
    /// `platform::process_usage` reads (R12): equal, in the same Mach units, for a process
    /// stopped (SIGSTOP) and running nothing between the two reads (D215). Its child stopped
    /// itself once, and was read once each: a run whose end orphaned its process group hung
    /// it up, and it was gone before PROC_PIDTASKINFO's read (kiln r227, PM M554).
    #[cfg(target_os = "macos")]
    #[test]
    fn rusage_times_are_task_info_times() {
        let (mut child, pid) = stopped_child();
        let (rusage, task) = times_still(pid);
        assert_eq!(rusage, task);
        // SAFETY: kill(2) of this test's own stopped child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();
    }

    /// Why one read of each of the two readers differed (PM M554), on a host each CPU of which
    /// a thread of this test's spins on. A run's end orphans the process group its tests'
    /// children are in, and where one of them is stopped the kernel sends each SIGHUP, then
    /// SIGCONT: "If the exit of the process causes a process group to become orphaned, and if
    /// any member of the newly-orphaned process group is stopped, then a SIGHUP signal followed
    /// by a SIGCONT signal shall be sent to each process in the newly-orphaned process group"
    /// (POSIX.1-2017, `_exit`). Each of `ROUNDS` children stopped as the test stopped its own
    /// before is sent the two and read once each, as the test read it: hung up, it is gone, and
    /// PROC_PIDTASKINFO reads nothing of it, each time. Each stopped as it stops its own now,
    /// sent the two, stops itself again, and its two readers agree, each time. A measurement,
    /// run by hand (`--ignored --nocapture`).
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn a_stopped_child_is_hung_up_by_its_runs_end() {
        const ROUNDS: usize = 100;
        let hang_up = |pid: libc::pid_t| {
            // SAFETY: kill(2) of this test's own stopped child, as the kernel signals an
            // orphaned process group's members.
            unsafe {
                assert_eq!(libc::kill(pid, libc::SIGHUP), 0);
                assert_eq!(libc::kill(pid, libc::SIGCONT), 0);
            }
        };
        let spinning = std::sync::atomic::AtomicBool::new(true);
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let (mut before, mut now) = (0usize, 0usize);
        std::thread::scope(|s| {
            for _ in 0..cpus {
                s.spawn(|| {
                    while spinning.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                });
            }
            for _ in 0..ROUNDS {
                let (mut child, pid) =
                    stopped("i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done; kill -STOP $$; exit 0");
                hang_up(pid);
                // Its end, before its reads: what the test read in r227's run.
                // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: waitid(2) of this test's own child, to its end, left a zombie.
                let waited = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        child.id(),
                        &raw mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                assert_eq!(waited, 0);
                before += usize::from(task_times(pid) != Some(rusage_times(pid)));
                let _ = child.wait();
                let (mut child, pid) = stopped_child();
                hang_up(pid);
                let (rusage, task) = times_still(pid);
                now += usize::from(rusage != task);
                // SAFETY: kill(2) of this test's own child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = child.wait();
            }
            spinning.store(false, std::sync::atomic::Ordering::Relaxed);
        });
        eprintln!(
            "{ROUNDS} stopped children hung up, {cpus} spinners: one read of each disagreed in {before} as the test stopped its child before, in {now} as it stops it now"
        );
        assert_eq!((before, now), (ROUNDS, 0));
    }
}
