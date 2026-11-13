//! Waits that follow a guest's VM process (D215): a wait for a guest's answer goes on while
//! that process runs, and is refused, typed, once it stalls.
//!
//! This process learns each kind of answer ([`Awaited`]) apart, in its own memory, from the
//! answers it sees: W*, the slowest, from a wait's start to its answer; and R*, the least CPU
//! rate at which a VM process still answered, its CPU over that wait's wall time. A wait that
//! begins once its kind has an answer looks at its VM process once each W* past its start,
//! and a look that finds it ran nothing at all since the look before refuses the wait,
//! [`Refusal::Stalled`]: a host under load slows a VM process but never stops it, so one that
//! ran nothing for as long as the slowest answer took is stopped, deadlocked or frozen. R* is
//! the refusal's diagnosis, never its bound (ROOT, 2026-11-13: a rate below it refused a boot
//! slowed by its host's load). A wait that begins before its kind's first answer
//! ([`Seen::Cold`]) follows its VM process's liveness alone.
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
//!   documents no resolution of its times.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::{End, Identity, Refusal};

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

/// A CPU rate: CPU time had over a wall time, kept as the pair it was measured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// CPU time, user and system, of every thread.
    pub cpu: Duration,
    /// The wall time it was had over.
    pub over: Duration,
}

impl Rate {
    /// Whether this rate is less than `other`: `cpu / over < other.cpu / other.over`, as
    /// `cpu × other.over < other.cpu × over` in nanoseconds, with no division and no float.
    /// Each side is the product of two u64s, which no u128 overflows; a duration past u64's
    /// nanoseconds (584 years) counts as its most.
    pub(crate) fn below(&self, other: &Rate) -> bool {
        let ns = |d: Duration| u128::from(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        ns(self.cpu).saturating_mul(ns(other.over)) < ns(other.cpu).saturating_mul(ns(self.over))
    }
}

/// What a wait that follows its guest's VM process found as it refused it
/// ([`Refusal::Stalled`]). Kept in nanoseconds, each past u64's (584 years) its most, so a
/// refusal is no larger for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    waited: u64,
    over: u64,
    least: [u64; 2],
    slowest: u64,
}

impl Stall {
    fn new(waited: Duration, over: Duration, least: Rate, slowest: Duration) -> Stall {
        let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        Stall {
            waited: ns(waited),
            over: ns(over),
            least: [ns(least.cpu), ns(least.over)],
            slowest: ns(slowest),
        }
    }

    /// How far into the wait it was found.
    pub fn waited(&self) -> Duration {
        Duration::from_nanos(self.waited)
    }

    /// The time since the look before, a whole [`slowest`](Stall::slowest) or more, over
    /// which its VM process ran no CPU at all.
    pub fn over(&self) -> Duration {
        Duration::from_nanos(self.over)
    }

    /// The least CPU rate at which a VM process here still answered (R*): diagnosis, never
    /// a bound.
    pub fn least(&self) -> Rate {
        let [cpu, over] = self.least;
        Rate {
            cpu: Duration::from_nanos(cpu),
            over: Duration::from_nanos(over),
        }
    }

    /// The slowest answer of the wait's kind this process has seen (W*), past which it was
    /// looked at.
    pub fn slowest(&self) -> Duration {
        Duration::from_nanos(self.slowest)
    }
}

/// What this process has seen of a kind of answer ([`Shards::seen`](crate::Shards::seen)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// No answer yet: a wait follows its VM process's liveness alone.
    Cold,
    /// The slowest answer seen (W*), from a wait's start to its answer, past which a wait
    /// looks at its VM process; and the least CPU rate at which a VM process still answered
    /// (R*), which a stall's refusal reports.
    Learned { slowest: Duration, least: Rate },
}

/// What this process has learned of each kind of answer, none until its first.
#[derive(Debug)]
pub(crate) struct Records {
    boot: Mutex<Seen>,
    connect: Mutex<Seen>,
    end: Mutex<Seen>,
}

/// This process's records: what every guest it boots or takes back learns from and is
/// judged by.
pub(crate) static RECORDS: Records = Records::new();

impl Records {
    pub(crate) const fn new() -> Records {
        Records {
            boot: Mutex::new(Seen::Cold),
            connect: Mutex::new(Seen::Cold),
            end: Mutex::new(Seen::Cold),
        }
    }

    fn of(&self, awaited: Awaited) -> MutexGuard<'_, Seen> {
        match awaited {
            Awaited::Boot => &self.boot,
            Awaited::Connect => &self.connect,
            Awaited::End => &self.end,
        }
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn seen(&self, awaited: Awaited) -> Seen {
        *self.of(awaited)
    }

    /// An answer of `awaited`'s kind came `wall` after its wait began, its VM process having
    /// run `cpu` meanwhile: the slowest rises to it where it is slower, the least rate falls to
    /// its where its is less. A wait no time long has no rate and teaches nothing.
    pub(crate) fn teach(&self, awaited: Awaited, wall: Duration, cpu: Duration) {
        if wall.is_zero() {
            return;
        }
        let rate = Rate { cpu, over: wall };
        let mut seen = self.of(awaited);
        *seen = match *seen {
            Seen::Cold => Seen::Learned {
                slowest: wall,
                least: rate,
            },
            Seen::Learned { slowest, least } => Seen::Learned {
                slowest: slowest.max(wall),
                least: if rate.below(&least) { rate } else { least },
            },
        };
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

/// A wait that follows its guest's VM process.
pub(crate) struct Follow {
    awaited: Awaited,
    records: &'static Records,
    followed: Followed,
    /// Its VM process, for a refusal to name.
    pid: u32,
    started: u64,
    /// Whether it is judged: its kind had an answer as it began, and its VM process has not
    /// gone since. One that began before its kind's first answer follows liveness alone.
    judged: bool,
    began: Instant,
    /// What its VM process had run as it began.
    from: Duration,
    /// The look before, and when it was.
    last: (Instant, Look),
}

impl Follow {
    /// A wait for an answer of `awaited`'s kind, beginning now, that follows `followed`, the
    /// VM process `identity` names, learning into and judged by `records`: none where that
    /// process has gone already, whose end is the wait's.
    pub(crate) fn begin(
        awaited: Awaited,
        records: &'static Records,
        identity: &Identity,
        mut followed: Followed,
    ) -> Result<Option<Follow>, Refusal> {
        let judged = matches!(records.seen(awaited), Seen::Learned { .. });
        let began = Instant::now();
        let refused = |failure| Refusal::Process {
            pid: identity.pid,
            expected_started: identity.started,
            failure,
        };
        let Some(look) = followed.look().map_err(refused)? else {
            return Ok(None);
        };
        Ok(Some(Follow {
            awaited,
            records,
            followed,
            pid: identity.pid,
            started: identity.started,
            judged,
            began,
            from: look.cpu,
            last: (began, look),
        }))
    }

    /// When its next look is due, W* after the look before: none while it is not judged.
    pub(crate) fn due(&self) -> Option<Instant> {
        match self.records.seen(self.awaited) {
            Seen::Learned { slowest, .. } if self.judged => self.last.0.checked_add(slowest),
            _ => None,
        }
    }

    /// Looks at its VM process where a look is due: refused `Stalled` where it ran nothing
    /// since the look before, and `Process` where it could not be read while it lives. A
    /// process that has gone since is no longer looked at: its end is the wait's.
    pub(crate) fn look(&mut self) -> Result<(), Refusal> {
        let Some(due) = self.due() else {
            return Ok(());
        };
        if Instant::now() < due {
            return Ok(());
        }
        let Seen::Learned { slowest, least } = self.records.seen(self.awaited) else {
            return Ok(());
        };
        let (pid, expected_started) = (self.pid, self.started);
        let looked = self.followed.look().map_err(|failure| Refusal::Process {
            pid,
            expected_started,
            failure,
        })?;
        let Some(look) = looked else {
            self.judged = false;
            return Ok(());
        };
        let now = Instant::now();
        let (then, before) = std::mem::replace(&mut self.last, (now, look));
        if look.ending || look.ran_since(&before) {
            return Ok(());
        }
        Err(Refusal::Stalled(Stall::new(
            now.saturating_duration_since(self.began),
            now.saturating_duration_since(then),
            least,
            slowest,
        )))
    }

    /// Its guest answered: its kind learns from the answer, its VM process read now.
    pub(crate) fn answered(&mut self) {
        if let Ok(Some(look)) = self.followed.look() {
            self.teach(look.cpu);
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
        let cpu = self.followed.look().ok().flatten().map(|look| look.cpu).or(cpu);
        if let Some(cpu) = cpu {
            self.teach(cpu);
        }
    }

    fn teach(&self, cpu: Duration) {
        self.records
            .teach(self.awaited, self.began.elapsed(), cpu.saturating_sub(self.from));
    }
}

/// A VM process as a wait follows it on Unix: its pid, and the start that tells it from a
/// process given that pid since.
#[cfg(unix)]
pub(crate) struct Followed {
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

    /// What it has run, now; none where it has gone (its pid another's, or no process's).
    fn look(&mut self) -> Result<Option<Look>, crate::ProcessFailure> {
        os::look(self.pid, self.started)
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
pub(crate) use os::Followed;

#[cfg(target_os = "linux")]
mod os {
    use super::Look;
    use crate::{ProcessFailure, ProcessOperation};
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
    use super::Look;
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
    use super::Look;
    use crate::taken::io_failure;
    use crate::{ProcessFailure, ProcessOperation};
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE,
        WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };
    use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;

    /// A VM process as a wait follows it on Windows: the `shards` that booted it, which holds
    /// it in a Job of its own as its child (D141), and every process that descends from that
    /// `shards`, each held by a handle of its own, which keeps the process, its pid and its
    /// times its own while held, after it has exited too.
    pub(crate) struct Followed {
        /// The root, the `shards`, first.
        members: Vec<Member>,
    }

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
            Ok(Followed { members })
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
                        && let Some(member) = Member::open(found.th32ProcessID)?
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
        fn open(pid: u32) -> Result<Option<Member>, ProcessFailure> {
            // SAFETY: OpenProcess of `pid` for query and synchronize access alone, its handle
            // not inherited.
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            if raw.is_null() {
                let e = io::Error::last_os_error();
                if is(&e, ERROR_INVALID_PARAMETER) || is(&e, ERROR_ACCESS_DENIED) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rate is less than another's as cross-multiplication says, with no float: equal
    /// rates are neither below the other, none at all is below any, and a duration past
    /// u64's nanoseconds is its most rather than an overflow (D215).
    #[test]
    fn a_rate_is_compared_by_cross_multiplication() {
        let rate = |cpu: u64, over: u64| Rate {
            cpu: Duration::from_nanos(cpu),
            over: Duration::from_nanos(over),
        };
        assert!(rate(1, 2).below(&rate(1, 1)));
        assert!(!rate(1, 1).below(&rate(1, 2)));
        assert!(!rate(2, 4).below(&rate(1, 2)));
        assert!(!rate(1, 2).below(&rate(2, 4)));
        assert!(rate(0, 1).below(&rate(1, u64::MAX)));
        let most = Rate {
            cpu: Duration::MAX,
            over: Duration::MAX,
        };
        assert!(!most.below(&rate(u64::MAX, u64::MAX)));
        assert!(rate(1, 2).below(&most));
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

    /// proc_pid_rusage's user and system times are PROC_PIDTASKINFO's, the ones shards-vmm's
    /// `platform::process_usage` reads (R12): equal, in the same Mach units, for a process
    /// stopped between the two reads (SIGSTOP), which runs nothing meanwhile (D215).
    #[cfg(target_os = "macos")]
    #[test]
    fn rusage_times_are_task_info_times() {
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 100000 ]; do i=$((i+1)); done; kill -STOP $$; exit 0",
            ])
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
        // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
        let mut rusage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of the stopped child into a rusage_info_v0 of ours.
        let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut rusage).cast()) };
        assert_eq!(r, 0);
        // SAFETY: a zeroed proc_taskinfo is a valid out-parameter, which proc_pidinfo fills.
        let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).unwrap();
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `task`.
        let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size) };
        assert_eq!(got, size);
        assert_eq!(
            (rusage.ri_user_time, rusage.ri_system_time),
            (task.pti_total_user, task.pti_total_system)
        );
        // SAFETY: kill(2) of this test's own stopped child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();
    }
}
