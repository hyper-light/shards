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
use crate::judge::{Found, Judge, Record, Seen, Stall};
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
    /// A thread of it is runnable on no CPU, or waits on its host's memory, as it is looked
    /// at.
    runnable: bool,
    /// It is stopped (a stop signal's, a debugger's, a suspension's) or frozen.
    stopped: bool,
    /// Linux: the time its threads have waited for a CPU.
    #[cfg(target_os = "linux")]
    queued: Queued,
    /// Linux: the time its cgroup's tasks have waited for a CPU or stalled on memory, in
    /// microseconds (cpu.pressure's and memory.pressure's `some` totals, summed), read only
    /// where that cgroup holds this process alone, as a daemon's VM process its slice (D209):
    /// a shared cgroup's is its neighbours' too.
    #[cfg(target_os = "linux")]
    pressure: Option<u64>,
}

/// Linux: what a look read of a process's threads' wait for a CPU.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Queued {
    /// None read: the kernel keeps no schedstat (built without CONFIG_SCHED_INFO), or keeps
    /// it as zeros (fs/proc/base.c, where `sched_info_on()` is false), or the process went.
    /// Its looks are judged by its CPU and its runnable sample alone.
    #[default]
    Unread,
    /// Its threads' run-queue wait, in nanoseconds, summed: each one's
    /// `/proc/<pid>/task/<tid>/schedstat` second field, `run_delay`
    /// (Documentation/scheduler/sched-stats.rst).
    Waited(u64),
}

impl Hold {
    /// Whether its host held it since the look that saw `before`: a thread of it runnable,
    /// or waiting on its host's memory, now; or (Linux) its threads' wait for a CPU grown
    /// since, or its own cgroup's pressure.
    ///
    /// The kernel adds a thread's wait to its `run_delay` as it reaches a CPU or leaves the
    /// queue (kernel/sched/stats.h `sched_info_arrive`, `sched_info_dequeue`): a thread that
    /// waited the whole time between two looks shows by its runnable state instead. A thread
    /// gone since takes its wait out of the sum, which then shows growth only past it.
    fn held_since(&self, before: &Hold) -> bool {
        #[cfg(target_os = "linux")]
        let waited = {
            let queued = matches!(
                (before.queued, self.queued),
                (Queued::Waited(b), Queued::Waited(n)) if n > b
            );
            let pressed = matches!((before.pressure, self.pressure), (Some(b), Some(n)) if n > b);
            queued || pressed
        };
        #[cfg(not(target_os = "linux"))]
        let waited = {
            let _ = before;
            false
        };
        self.runnable || waited
    }
}

/// What a look found of a VM process since the look before: progress where it `ran` or its
/// end is under way (`ending`); else, stopped or frozen, none of its own, whatever its host
/// did; else held, where its host held it since (`before`, `now`); else none of its own.
fn found(ran: bool, ending: bool, before: &Hold, now: &Hold) -> Found {
    if ran || ending {
        Found::Progressed
    } else if now.stopped {
        Found::Still
    } else if now.held_since(before) {
        Found::Held
    } else {
        Found::Still
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
}

impl Watch {
    /// `followed`, looked at now: none where its process has gone already.
    pub fn of(mut followed: Followed) -> Result<Option<Watch>, ProcessFailure> {
        let Some(last) = followed.look()? else {
            return Ok(None);
        };
        let hold = followed.hold();
        Ok(Some(Watch { followed, last, hold }))
    }

    /// What it did since the look before ([`Found`]): progressed where it ran or its end is
    /// under way; held where its host held it from running; still where it did neither of
    /// its own, or is stopped or frozen; gone where its process has.
    pub fn look(&mut self) -> Result<Found, ProcessFailure> {
        let Some(look) = self.followed.look()? else {
            return Ok(Found::Gone);
        };
        let before = std::mem::replace(&mut self.last, look);
        let hold = std::mem::replace(&mut self.hold, self.followed.hold());
        let ran = look.ran_since(&before);
        // A diagnosis of what Windows shows of a VM process a look finds ran nothing since the
        // look before, the look that may cut it (W2: one inside WHP's partition calls, cut as
        // stalled on CI's windows-2022): said only of those, so a passing run says nothing.
        #[cfg(windows)]
        if !ran && std::env::var_os("SHARDS_JUDGE_TRACE").is_some() {
            self.followed.trace(&look);
        }
        Ok(found(ran, look.ending, &hold, &self.hold))
    }

    /// The CPU time its process's threads have had by the look before.
    pub fn cpu(&self) -> Duration {
        self.last.cpu
    }

    /// The CPU time its process's threads have had by now, the look before kept; none where
    /// it has gone or could not be read.
    fn cpu_now(&mut self) -> Option<Duration> {
        self.followed.look().ok().flatten().map(|look| look.cpu)
    }
}

/// A wait that follows a VM process, judged by the record of its kind of answer ([`Judge`])
/// on what each look at the process finds ([`Watch`]).
#[derive(Debug)]
pub struct Follow<'a> {
    judge: Judge<'a>,
    watch: Watch,
    /// Its VM process, for a refusal to name.
    pid: u32,
    started: u64,
    /// What its VM process had run as it began.
    from: Duration,
}

impl<'a> Follow<'a> {
    /// A wait beginning now that follows `followed`, learning into and judged by `record`:
    /// none where its process has gone already, whose end is the wait's.
    pub fn begin(record: &'a Record, followed: Followed) -> Result<Option<Follow<'a>>, Unfollowed> {
        let judge = Judge::begin(record);
        let (pid, started) = (followed.pid(), followed.started());
        let watched = Watch::of(followed).map_err(|failure| Unfollowed::Process {
            pid,
            expected_started: started,
            failure,
        })?;
        let Some(watch) = watched else {
            return Ok(None);
        };
        Ok(Some(Follow {
            judge,
            from: watch.cpu(),
            watch,
            pid,
            started,
        }))
    }

    /// When its next look is due, W* after the look before: none while it is not judged.
    pub fn due(&self) -> Option<Instant> {
        self.judge.due()
    }

    /// Looks at its VM process where a look is due: refused `Stalled` where it ran nothing
    /// since the look before of its own, its host not holding it (or it stopped or frozen),
    /// and `Process` where it could not be read while it lives. A process that has gone since
    /// is no longer looked at: its end is the wait's.
    pub fn look(&mut self) -> Result<(), Unfollowed> {
        let Some(due) = self.judge.due() else {
            return Ok(());
        };
        if Instant::now() < due {
            return Ok(());
        }
        let (pid, expected_started) = (self.pid, self.started);
        let found = self.watch.look().map_err(|failure| Unfollowed::Process {
            pid,
            expected_started,
            failure,
        })?;
        self.judge.found(found).map_err(Unfollowed::Stalled)
    }

    /// Its party answered: its kind learns from the answer, its VM process read now.
    pub fn answered(&mut self) {
        if let Some(cpu) = self.watch.cpu_now() {
            self.judge.answered(cpu.saturating_sub(self.from));
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
        if let Some(cpu) = cpu {
            self.judge.answered(cpu.saturating_sub(self.from));
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
}

/// What process `pid` had run by its end, read by its watcher before it is reaped: a child
/// of this process booted and not yet reaped is its alone (`started` none), and one taken back
/// is its only where it is the one that started at `started`. None where it could not be read.
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
    /// thread of it runnable, its state `R` (`/proc/<pid>/task/<tid>/stat` field 3, proc(5));
    /// it stopped, its leader's state `T` or `t`, or its cgroup frozen (`cgroup.events`'
    /// `frozen 1`, Documentation/admin-guide/cgroup-v2.rst); its threads' run-queue wait
    /// ([`Queued`]); and, where its cgroup holds it alone, that cgroup's pressure
    /// (Documentation/accounting/psi.rst), whose `some` totals grow while any of its tasks
    /// waits for a CPU, or is stalled on memory, a task held over its `memory.high` among them
    /// (mm/memcontrol.c, `mem_cgroup_handle_over_high`). A cgroup it shares, the root's among
    /// them (the host's, `/proc/pressure`), is not read: its pressure is its other tasks' as
    /// much as this one's. What it cannot read shows nothing.
    pub(super) fn hold(pid: u32) -> Hold {
        let mut hold = Hold::default();
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                let dir = task.path();
                if let Some(delay) = run_delay(&dir) {
                    hold.queued = Queued::Waited(match hold.queued {
                        Queued::Waited(sum) => sum.saturating_add(delay),
                        Queued::Unread => delay,
                    });
                }
                let Some(state) = std::fs::read(dir.join("stat")).ok().and_then(|stat| state(&stat)) else {
                    continue;
                };
                hold.runnable |= state == b'R';
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
            }
        }
        hold
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
        // SAFETY: a timespec, plain data, for clock_gettime to fill.
        let mut at: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_gettime(2) of that clock into a local timespec.
        if unsafe { libc::clock_gettime(clock, &raw mut at) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINVAL) {
                return Ok(None);
            }
            return Err(crate::taken::io_failure(ProcessOperation::CpuClock, e));
        }
        // A timespec's nanoseconds are below a second's (POSIX <time.h>).
        let secs = u64::try_from(at.tv_sec).map_err(|_| ProcessFailure::InvalidCpuTime)?;
        let nanos = u32::try_from(at.tv_nsec)
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .ok_or(ProcessFailure::InvalidCpuTime)?;
        Ok(Some(Duration::new(secs, nanos)))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod os {
    use super::{Hold, Look};
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
    /// `TH_STATE_RUNNING` (running, or waiting for a CPU) or `TH_STATE_UNINTERRUPTIBLE` (a wait
    /// in the kernel, a page-in among them, which macOS keeps no pressure record of), as
    /// `PROC_PIDTHREADINFO` reports each (sys/proc_info.h, mach/thread_info.h). What it cannot
    /// read shows nothing.
    pub(super) fn hold(pid: u32) -> Hold {
        let Ok(id) = libc::c_int::try_from(pid) else {
            return Hold::default();
        };
        Hold {
            runnable: threads(id).is_some_and(|handles| {
                handles.iter().any(|&handle| {
                    run_state(id, handle).is_some_and(|state| {
                        matches!(state, libc::TH_STATE_RUNNING | libc::TH_STATE_UNINTERRUPTIBLE)
                    })
                })
            }),
            stopped: status(id) == Some(libc::SSTOP),
        }
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

    /// The run state of process `id`'s thread `handle` (`pth_run_state`); none where it could
    /// not be read, as of a thread that ended since it was listed.
    fn run_state(id: libc::c_int, handle: u64) -> Option<libc::c_int> {
        // SAFETY: a proc_threadinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_threadinfo>()).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got =
            unsafe { libc::proc_pidinfo(id, libc::PROC_PIDTHREADINFO, handle, (&raw mut info).cast(), size) };
        (got == size).then_some(info.pth_run_state)
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
        let ns = ticks.saturating_mul(u128::from(base.numer)) / u128::from(base.denom);
        Ok(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)))
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
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
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
            }
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
    /// the first look or two taking in its start's own CPU.
    #[cfg(unix)]
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
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        let mut follow = Follow::begin(&record, followed).unwrap().unwrap();
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
            let mut follow = Follow::begin(&record, followed).unwrap().unwrap();
            for _ in 0..3 {
                until_due(&follow);
                assert_eq!(follow.look(), Ok(()));
            }
            busy.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// What a look finds of a VM process that ran no CPU since the look before is its host's
    /// doing, or its own: held where its host holds it (a thread runnable on no CPU, or
    /// waiting on its host's memory; on Linux, its cgroup's pressure grown), which is no stall;
    /// still where it waits on itself, and where it is stopped or frozen, whatever else shows.
    /// One that ran, or whose end is under way, progressed (ROOT's ruling of 2026-11-13).
    #[test]
    fn a_process_its_host_holds_is_held_and_a_stopped_one_still() {
        let hold = |runnable, stopped| Hold {
            runnable,
            stopped,
            #[cfg(target_os = "linux")]
            pressure: None,
        };
        let (idle, throttled, stopped) = (hold(false, false), hold(true, false), hold(true, true));
        assert_eq!(found(true, false, &idle, &idle), Found::Progressed);
        assert_eq!(found(false, true, &idle, &stopped), Found::Progressed);
        assert_eq!(found(false, false, &idle, &throttled), Found::Held);
        assert_eq!(found(false, false, &idle, &idle), Found::Still);
        assert_eq!(found(false, false, &idle, &stopped), Found::Still);
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
        let mut follow = Follow::begin(&record, followed).unwrap().unwrap();
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
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sleeper_is_still_whatever_its_cgroups_others_wait_for() {
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        // Asleep in its nanosleep (`S`), its start's own CPU and page-ins (`D`) behind it.
        while !std::fs::read_to_string(format!("/proc/{}/stat", sleeper.id()))
            .unwrap()
            .rsplit_once(')')
            .is_some_and(|(_, fields)| fields.trim_start().starts_with('S'))
        {
            std::thread::yield_now();
        }
        let before = followed.hold();
        let ran = followed.look().unwrap().unwrap();
        let shared = shared_cpu_pressure(sleeper.id());
        let cpu = first_cpu();
        let mut spinners = [spinner_on(cpu), spinner_on(cpu)];
        // Waited for, where the kernel keeps it: the spinners' wait for their CPU shown in the
        // pressure the sleeper's cgroup shares.
        if let Some(from) = shared {
            while shared_cpu_pressure(sleeper.id()).is_none_or(|now| now <= from) {
                std::thread::yield_now();
            }
        }
        let now = followed.hold();
        let look = followed.look().unwrap().unwrap();
        for spinner in &mut spinners {
            drop(spinner.stdin.take());
            spinner.wait().unwrap();
        }
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert!(!look.ran_since(&ran), "the sleeper ran: {ran:?} {look:?}");
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
            least: Rate { cpu, over },
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
        assert_eq!(ran_by_end(pid, None), Some(look.cpu));
        assert_eq!(ran_by_end(pid, Some(started)), Some(look.cpu));
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
