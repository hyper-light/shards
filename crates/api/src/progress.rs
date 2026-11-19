//! Waits that follow a guest's VM process (D215): a wait for a guest's answer goes on while
//! that process runs, and is refused, typed, once it stalls. The daemon's waits on a
//! microVM's own answers follow its VM process so too (D188, its `Wait::WhileRuns`).
//!
//! This process learns each kind of answer ([`Awaited`]) apart, in its own memory, from the
//! answers it sees ([`Record`]): W*, the slowest, from a wait's start to its answer; and R*,
//! the least rate at which a VM process still answered, its work ([`Work`]: its CPU time, on
//! Windows its cycles) over that wait's wall time. A wait that begins once its kind has an
//! answer looks at its VM process once each W* past its start, and a look that finds it ran
//! nothing at all since the look before refuses the wait, [`Refusal::Stalled`]: a host
//! under load slows a VM process but never stops it, so one that ran nothing for as long as
//! the slowest answer took is stopped, deadlocked or frozen. R* is the refusal's diagnosis,
//! never its bound (ROOT, 2026-11-13: a rate below it refused a boot slowed by its host's
//! load). A wait that begins before its kind's first answer ([`Seen::Cold`]) follows its VM
//! process's liveness alone. The schedule and the verdict are [`Judge`]'s; what each look
//! finds is read here.
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
//!   gone), each held by a handle of its own: QueryProcessCycleTime's count of the cycles
//!   their threads used, which Windows accumulates as they run, for whether they ran at all
//!   and for their work, the record's R* and C* ([`Work`]); GetProcessTimes' kernel and user
//!   times (100 ns units, summed over a process's threads), which Windows charges a whole
//!   clock tick at a time (PM M294), for its trace alone. A daemon's VM process is followed
//!   so, by its pid ([`Followed::of_pid`]).

use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::Identity;
use crate::{End, ProcessFailure, Refusal};
use crate::{Found, Judge, Record, Seen, Span, Stall, Work};

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
    /// done `work` meanwhile ([`Record::teach`]): a test's record, taught by hand.
    #[cfg(test)]
    pub(crate) fn teach(&self, awaited: Awaited, wall: Duration, work: Work) {
        self.of(awaited).teach(wall, work);
    }
}

/// One look at a VM process.
#[derive(Debug, Clone, Copy)]
struct Look {
    /// The CPU time its threads have had, user and system: on Windows in its clock's ticks,
    /// read for its trace alone.
    cpu: Duration,
    /// The cycles its threads have used (Windows): what tells whether they ran, and its work.
    #[cfg(windows)]
    cycles: u64,
    /// It, or a process of its tree, has exited since the look before: its end is under way,
    /// so the time since is not judged. On macOS from its exit's start, before its kernel has
    /// done it ([`exited`](Self::exited)).
    ending: bool,
    /// Unix: its exit is done, a zombie its parent may reap, never without `ending`. Linux
    /// shows no exit before that (`ending`'s own reading); macOS stashes its usage with its
    /// exit time as its exit begins (`proc_prepareexit`), then frees its memory, closes its
    /// descriptors and drains its terminal, and only then makes it a zombie, `SZOMB`
    /// (xnu-12377.101.15 bsd/kern/kern_exit.c:2207-2215, 2269; osfmk/kern/task.c:3164;
    /// kern_exit.c:2384, 2427-2476, 2771).
    #[cfg(unix)]
    exited: bool,
}

impl Look {
    /// Whether any of its threads ran between `before` and this look.
    #[cfg(windows)]
    fn ran_since(&self, before: &Look) -> bool {
        self.cycles > before.cycles
    }

    /// Its threads' work by this look: their cycles, which Windows counts as they run, never
    /// its ticks' times (D215: R* and C* of whole ticks refused a party of thirteen, CI
    /// 44347282760).
    #[cfg(windows)]
    fn work(&self) -> Work {
        Work::of(self.cycles)
    }

    /// Its threads' work by this look: their CPU time, to the nanosecond.
    #[cfg(not(windows))]
    fn work(&self) -> Work {
        Work::of_cpu(self.cpu)
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
    /// A thread of it is runnable (running, or waiting for a CPU) as it is looked at.
    runnable: bool,
    /// It is stopped (a stop signal's, a debugger's, a suspension's) or frozen.
    stopped: bool,
    /// A thread of it waits uninterruptibly in its kernel as it is looked at (macOS
    /// `TH_STATE_UNINTERRUPTIBLE`, Linux `D`, proc(5)'s "uninterruptible disk sleep"; Windows a
    /// wait on its host's paging or a kernel lock: shards-judge's `threads::in_kernel`): its host's own work for it, a disk's flush or
    /// a page-in, which no rate of its own runs at (PM M600: `shards guest use` spent 52 to
    /// 172 ms of its 69 to 192 ms so on a loaded Mac, F_FULLFSYNC's waits, at 14 to 16 ms of
    /// CPU). Its look vouches for no window of its rate ([`vouches`]), and one that ran
    /// nothing is held where its host's disks completed I/O meanwhile
    /// ([`disks`](Self::disks)). Windows' wait reasons for a flush are unread here: a flush
    /// waits `Executive`, as a pipe's reader does (PM M606).
    uninterruptible: bool,
    /// The I/O its host's disks had completed, every disk's summed, and on Linux the time they
    /// spent doing I/O, which grows while one is in flight (`os::disks_worked`), read where a
    /// thread of it waited in its kernel as it was looked at (Linux `/proc/diskstats`, macOS IOKit's
    /// `IOBlockStorageDriver` statistics, Windows each physical disk's IOCTL_DISK_PERFORMANCE:
    /// `os::disks`), none elsewhere or where unread. Its growth since the look before says its
    /// host's disks were working meanwhile: a transaction commit another task runs, whose
    /// waiters wait in their kernels doing no I/O of their own (D215, 2026-11-16: two daemons'
    /// stops refused at once on adacachy2's btrfs, each in `D` on one commit), or a page-in
    /// that no count of its process's shows, as against a wait no device is serving, a dead
    /// NFS mount's, which is still.
    disks: Option<u64>,
    /// Linux: its host's CPU and memory stalls so far ([`Stalls`]), read where a thread of it
    /// waited in its kernel as it was looked at, as `disks` is. None elsewhere, and where the
    /// kernel keeps no pressure record (no CONFIG_PSI, or `psi=0`).
    stalls: Option<Stalls>,
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
    /// A durable write a member of it declared (D286), judged by its volume's own counters
    /// ([`Writing`]); none where it declares none.
    declared: Option<Writing>,
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

/// Its host's pressure totals, in microseconds, each the `some` line's `total` of
/// /proc/pressure (Documentation/accounting/psi.rst: "the share of time in which at least
/// some tasks are stalled on a given resource"): `cpu` grows while more of its tasks are
/// runnable than run on a CPU, `memory` while any of them stalls on memory (v6.18
/// kernel/sched/psi.c:243-262, `test_states`). A thread waiting in its kernel waits on its
/// kernel's work: a journal's commit, which first waits for every open handle of its
/// transaction (fs/jbd2/commit.c:454, transaction.c:816-844) and for its own thread to get a
/// CPU, or a folio's or buffer's lock another task holds (mm/filemap.c:1251, fs/buffer.c:
/// 121-124), none of it I/O its host's devices show. Growth of either says such work waited
/// for its host's CPUs or memory (D215, R-D: CI 44227612442's ten refusals, each with a thread
/// in `jbd2_log_wait_commit`, `folio_wait_bit_common` or `__wait_on_buffer`, its host's
/// devices idle between two looks). Its I/O line is not read: a thread that waits on a folio
/// or a buffer waits in `io_schedule`, its own I/O wait, so it would hold itself. Read on
/// Linux alone, which keeps the record; judged on every host, so that its tests run on each.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Stalls {
    cpu: u64,
    memory: u64,
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
    /// Whether its host held it since the look that saw `before`: a thread of it runnable
    /// now; or a thread of it waiting uninterruptibly in its kernel now, its host's disks
    /// having completed I/O since the look before ([`disks`](Self::disks)), or its host's
    /// tasks having waited for a CPU or stalled on memory ([`Stalls`]); or its threads'
    /// wait for a CPU grown since; or (Linux) its own cgroup's pressure; or its host's
    /// storage's work for it ([`Io`]).
    ///
    /// A party that ran nothing over a window and waits in its kernel at its end waited there
    /// at its start too, as it runs to enter a wait: so the look before, which found it so,
    /// read its host's disks then. One whose look before read none is not held by them.
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
        let in_kernel = self.uninterruptible
            && (matches!((before.disks, self.disks), (Some(b), Some(n)) if n > b)
                || matches!((before.stalls, self.stalls), (Some(b), Some(n)) if n.cpu > b.cpu || n.memory > b.memory));
        // A declared durable write is its host's while its volume works (D286), whatever its
        // thread's state: its volume's own counters, read since their last reading.
        let declared = self.declared.is_some_and(|w| w.moved == Moved::Worked);
        self.runnable || in_kernel || declared || queued || pressed || self.io.grown_since(&before.io)
    }

    /// Whether a thread of it waited uninterruptibly in its kernel, or in a durable write it
    /// declared, as it was looked at: its host's work for it, which vouches for no window of
    /// its rate.
    fn in_kernel(&self) -> bool {
        self.uninterruptible || self.declared.is_some()
    }

    /// What a look that found a thread of it in its kernel read of its host's work since the
    /// look that saw `before`, by which [`held_since`](Self::held_since) decided: its devices'
    /// work, and its host's CPU and memory stalls; none where no thread of it waited there.
    fn kernel_since(&self, before: &Hold) -> Option<String> {
        let writing = self.declared.map(|w| w.said());
        if !self.uninterruptible {
            return writing;
        }
        let devices = match (before.disks, self.disks) {
            (Some(b), Some(n)) => format!("its host's devices' work +{}", n.saturating_sub(b)),
            _ => "its host's devices' work unread before".to_string(),
        };
        let stalls = match (before.stalls, self.stalls) {
            (Some(b), Some(n)) => format!(
                "CPU some +{}us, memory some +{}us",
                n.cpu.saturating_sub(b.cpu),
                n.memory.saturating_sub(b.memory)
            ),
            (_, None) => "no pressure record (no CONFIG_PSI, or psi=0): its devices alone decide".to_string(),
            (None, Some(_)) => "its host's pressure unread before".to_string(),
        };
        let kernel = format!("in its kernel, since its look before: {devices}, {stalls}");
        Some(match writing {
            Some(writing) => format!("{writing}; {kernel}"),
            None => kernel,
        })
    }
}

/// A durable write a member of a party declared (D286), as a look found it: judged by its
/// volume's own counters, never by its thread's state (D215; the person's word of 2026-11-18:
/// no fallbacks). The longest of the party's declared writes stands for them in its account;
/// their volumes together decide whether its host works for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Writing {
    /// The volume the longest of them is on (`shards_durable::Declared::volume`).
    volume: u64,
    /// How long that write had gone on as the look found it.
    lasted: Duration,
    /// What their volumes did since their last reading.
    moved: Moved,
}

impl Writing {
    /// What its account says of it.
    fn said(&self) -> String {
        let (volume, lasted) = (os::volume_name(self.volume), self.lasted);
        match self.moved {
            Moved::Worked => format!("a durable write on {volume} for {lasted:?}, its volume working"),
            Moved::Cold => format!("a durable write on {volume} for {lasted:?}, its volume read first now"),
            Moved::Idle => format!(
                "a durable write on {volume} for {lasted:?}; {volume} moved nothing since the look before"
            ),
            Moved::Unreadable(_, Unread::Declarations(errno)) => {
                format!(
                    "its home's declarations unreadable: {}",
                    Unread::Declarations(errno)
                )
            }
            Moved::Unreadable(at, why) => format!(
                "a durable write on {volume} for {lasted:?}; {}'s counters unreadable: {why}",
                os::volume_name(at)
            ),
        }
    }
}

/// What the volumes of a party's declared writes did since their last reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    /// One completed I/O since its last reading, or holds I/O in flight now: its host works
    /// for the writes, which holds them.
    Worked,
    /// Read for the first time now, nothing in flight: no reading before to compare with, as
    /// a look whose look before read no disks holds nothing by them. Still.
    Cold,
    /// None completed any since its last reading, and none holds any in flight: the writes wait
    /// on what no counter shows (another task's lock, a frozen or hung device), which holding
    /// unbounded would make the hang D215 ends. Still, judged as any wait.
    Idle,
    /// What could not be read, of which volume: said in its account, never taken for none.
    /// Not held.
    Unreadable(u64, Unread),
}

/// Why a declared write's evidence could not be read (D286).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// Its home's declarations (`shards_durable::Reader`): their OS error.
    Declarations(Option<i32>),
    /// No device of counters lies beneath its volume: Linux its mount's source names no block
    /// device; macOS no IOBlockStorageDriver lies beneath its media; Windows no volume has its
    /// serial.
    NoDevice,
    /// Its device's counters could not be read: their OS error (macOS IOKit's `kern_return_t`).
    Counters(Option<i32>),
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let error = |errno: &Option<i32>| match errno {
            Some(code) => format!("{}", std::io::Error::from_raw_os_error(*code)),
            None => "no OS error said".to_string(),
        };
        match self {
            Unread::Declarations(errno) => write!(f, "{}", error(errno)),
            Unread::NoDevice => write!(f, "no device of counters lies beneath it"),
            Unread::Counters(errno) => write!(f, "{}", error(errno)),
        }
    }
}

/// What a volume's device had done by a reading: its completed operations (and their bytes,
/// where its OS counts them), the time it spent on them, and what it holds in flight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    done: u64,
    busy: u64,
    in_flight: u64,
}

impl Counters {
    /// Whether its device worked since `before`: completed an operation, spent time on one,
    /// or holds one in flight now.
    fn worked_since(&self, before: &Counters) -> bool {
        self.done > before.done || self.busy > before.busy || self.in_flight > 0
    }
}

/// Where a followed process's home keeps its declared writes (D286), and the last reading of
/// each of their volumes' devices, by which a look tells whether they worked since.
#[derive(Debug)]
pub(crate) struct Declarations {
    reader: Source,
    volumes: std::sync::Mutex<std::collections::BTreeMap<u64, Volume>>,
}

/// Whose reader of a home's declarations a follower reads.
#[derive(Debug)]
enum Source {
    /// This process's own home's (`shards_durable::installed`).
    Installed(&'static shards_durable::Reader),
    /// Another home's, which its follower names (`Followed::declaring`).
    Own(shards_durable::Reader),
    /// This process's own home's, which could not be opened: its OS error, which each read
    /// says (`Unread::Declarations`), never taken for no declarations.
    Failed(Option<i32>),
}

/// A volume's device as first found, and its last reading.
#[derive(Debug)]
struct Volume {
    device: Result<os::Device, Unread>,
    last: Option<Counters>,
}

impl Declarations {
    /// This process's own home's, where it installed its slot file or recorded its home
    /// (`shards_durable::install_at`), read once that home is made
    /// (`shards_durable::Reader::open`); none where it did neither.
    fn installed() -> Option<Declarations> {
        match shards_durable::installed() {
            Ok(reader) => reader.map(|reader| Declarations::of(Source::Installed(reader))),
            Err(e) => Some(Declarations::of(Source::Failed(e.raw_os_error()))),
        }
    }

    /// The home `home`'s.
    fn of_home(home: &std::path::Path) -> std::io::Result<Declarations> {
        Ok(Declarations::of(Source::Own(shards_durable::Reader::open(home)?)))
    }

    fn of(reader: Source) -> Declarations {
        Declarations {
            reader,
            volumes: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Its reader, or the OS error its home's slot file could not be opened with.
    fn reader(&self) -> Result<&shards_durable::Reader, Option<i32>> {
        match &self.reader {
            Source::Installed(reader) => Ok(reader),
            Source::Own(reader) => Ok(reader),
            Source::Failed(errno) => Err(*errno),
        }
    }

    /// The slots processes `members` (each its ID and its start as its OS keeps it) hold in
    /// its home's slot file, each with what its writes took, by its process and that process's
    /// start: what a look reads a window's growth of (D286 amended, [`counted`]). Why not where
    /// its home's declarations could not be read.
    fn held(&self, members: &[(u32, u64)]) -> Result<Vec<(u32, u64, shards_durable::Held)>, Unread> {
        let reader = self.reader().map_err(Unread::Declarations)?;
        let mut held = Vec::new();
        for &(pid, start) in members {
            let found = reader
                .held(pid, start)
                .map_err(|e| Unread::Declarations(e.raw_os_error()))?;
            held.extend(found.into_iter().map(|slot| (pid, start, slot)));
        }
        Ok(held)
    }

    /// What processes `members` (each its ID and its start as its OS keeps it) declare now:
    /// the writing a look's hold says, and the declaring threads, which the threads' own
    /// witnesses then leave out (D286: a declared write is judged by its volume alone).
    fn writing(&self, members: &[(u32, u64)]) -> (Option<Writing>, Vec<u64>) {
        let mut declared = Vec::new();
        for &(pid, start) in members {
            let found = self
                .reader()
                .and_then(|reader| reader.declared(pid, start).map_err(|e| e.raw_os_error()));
            match found {
                Ok(found) => declared.extend(found.into_iter().map(|d| (pid, d))),
                Err(errno) => {
                    let moved = Moved::Unreadable(0, Unread::Declarations(errno));
                    return (
                        Some(Writing {
                            volume: 0,
                            lasted: Duration::ZERO,
                            moved,
                        }),
                        Vec::new(),
                    );
                }
            }
        }
        let Some(&(_, longest)) = declared.iter().min_by_key(|(_, d)| d.since) else {
            return (None, Vec::new());
        };
        let threads = declared.iter().map(|(_, d)| d.thread).collect();
        let lasted = shards_durable::now().map_or(Duration::ZERO, |now| {
            Duration::from_nanos(now.saturating_sub(longest.since))
        });
        let mut volumes = self
            .volumes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut read: Vec<u64> = Vec::new();
        let mut moved: Option<Moved> = None;
        for &(pid, d) in &declared {
            if read.contains(&d.volume) {
                continue;
            }
            read.push(d.volume);
            let volume = volumes.entry(d.volume).or_insert_with(|| Volume {
                device: os::device(d.volume, pid),
                last: None,
            });
            let this = match &volume.device {
                Err(why) => Moved::Unreadable(d.volume, *why),
                Ok(device) => match os::counters(device) {
                    Err(why) => Moved::Unreadable(d.volume, why),
                    Ok(now) => {
                        let this = match volume.last {
                            Some(before) if now.worked_since(&before) => Moved::Worked,
                            Some(_) => Moved::Idle,
                            None if now.in_flight > 0 => Moved::Worked,
                            None => Moved::Cold,
                        };
                        volume.last = Some(now);
                        this
                    }
                },
            };
            moved = Some(match (moved, this) {
                (Some(Moved::Worked), _) | (_, Moved::Worked) => Moved::Worked,
                (Some(unread @ Moved::Unreadable(..)), _) | (_, unread @ Moved::Unreadable(..)) => unread,
                (Some(Moved::Cold), _) | (_, Moved::Cold) => Moved::Cold,
                _ => Moved::Idle,
            });
        }
        let writing = moved.map(|moved| Writing {
            volume: longest.volume,
            lasted,
            moved,
        });
        (writing, threads)
    }
}

/// A process's slots' time counted so far by its follower (D286 amended): by its process, that
/// process's start, and the slot's place in the file.
type Counted = std::collections::BTreeMap<(u32, u64, u64), u64>;

/// The most any one slot's declared writes held their process since the look that counted
/// `before`, and what this look counts of each, from `slots` as it read them: each slot's ended
/// writes' time (`spent`), and the time of its write under way (from its start to `now`, on
/// `shards_durable::now`'s clock) where `worked`, its volume having worked since its last
/// reading (D286's rule: a write on a volume that does nothing is no hold), less what was
/// counted of it before. A slot first seen after the first look counts whole, as it was
/// claimed since; at the first (`before` none), every slot is its baseline, its write under
/// way counted to `now`. The most, not the sum: two threads' writes at once are one window of
/// the host's work, which the most never overstates; and what a look counts of a slot never
/// falls.
fn counted(
    before: Option<&Counted>,
    slots: &[(u32, u64, shards_durable::Held)],
    worked: bool,
    now: u64,
) -> (Duration, Counted) {
    let mut most = 0u64;
    let mut after = Counted::new();
    for &(pid, start, held) in slots {
        let under_way = if held.since != 0 && (worked || before.is_none()) {
            now.saturating_sub(held.since)
        } else {
            0
        };
        let total = held.spent.saturating_add(under_way);
        let key = (pid, start, held.slot);
        let had = before.map(|before| before.get(&key).copied().unwrap_or(0));
        if let Some(had) = had {
            most = most.max(total.saturating_sub(had));
        }
        after.insert(key, had.unwrap_or(0).max(total));
    }
    (Duration::from_nanos(most), after)
}

/// The most any one thread waited for a CPU between two readings of each's run-queue wait so
/// far (`before`, `now`: Linux's schedstat `run_delay`, by thread ID): its host keeping it
/// from every CPU (Documentation/scheduler/sched-stats.rst). A thread in one reading alone
/// shows nothing. The most, not the sum: its party's progress may wait on any one of its
/// threads, and two waiting at once are one window of its host's hold, so that a storm of
/// more vCPUs than its host has CPUs, each waiting most of a window, is never held for ever
/// by their sum (D215, Liveness; ROOT's ruling of 2026-11-18). Read on Linux alone, which keeps
/// each thread's wait; tested on every host.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn withheld(
    before: &std::collections::BTreeMap<u64, u64>,
    now: &std::collections::BTreeMap<u64, u64>,
) -> Duration {
    Duration::from_nanos(
        now.iter()
            .filter_map(|(thread, &delay)| before.get(thread).map(|&was| delay.saturating_sub(was)))
            .max()
            .unwrap_or(0),
    )
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
    /// Why [`starved`](Watch::starved) last could not read its threads, where it could not.
    unread: Option<String>,
    /// Whether its host's storage worked for it between the look before and the last
    /// ([`Io::grown_since`]).
    stored: bool,
    /// What its last look read of its host's work for a thread of it waiting in its kernel
    /// ([`Hold::kernel_since`]): for a refusal's or a hold's account (D215, R-D).
    kernel: Option<String>,
    /// Its threads' wait for a CPU in the window its last look closed ([`waited`](Watch::waited)).
    waited: Result<Option<Duration>, String>,
    /// Linux: each CPU's idle and iowait ticks as the last look that found a thread of it in
    /// its kernel read them, and when (`os::idle`): unused by the rule, said in its reading.
    #[cfg(target_os = "linux")]
    idle: Option<(Instant, Vec<u64>)>,
    /// Linux: its threads' user, system and guest ticks and its host's steal, as its last
    /// look and the look before read them (`os::times`): for a refusal's account, which then
    /// says whether a VM's vCPUs ran its guest, their kernel, or nothing (D215, ROOT's ruling
    /// of 2026-11-18; CI 44245219858's VMs that ran at their host's share, never ready).
    #[cfg(target_os = "linux")]
    times: [Option<os::Times>; 2],
    /// Windows: whether a look that finds it ran nothing says what it saw of it on stderr
    /// (SHARDS_JUDGE_TRACE set as it began to be watched, `os::Followed::trace`).
    #[cfg(windows)]
    trace: bool,
    /// Each of its slots' time as its looks counted it ([`counted`]), and the most any one of
    /// them held it over the window its last look closed (D286 amended).
    counted: Counted,
    in_writes: Duration,
    /// Linux: each of its tasks' run-queue wait as its last look read it (`os::delays`), and
    /// the most any one of them waited for a CPU over the window that look closed
    /// ([`withheld`]). Elsewhere none: macOS keeps a task's wait alone, every thread's summed
    /// (`ri_runnable_time`), and Windows none (its looks' samples of a thread ready or standby
    /// are its readings there, [`starved`]).
    #[cfg(target_os = "linux")]
    delays: std::collections::BTreeMap<u64, u64>,
    withheld: Duration,
}

/// A process's threads as one read saw them, each by its OS's ID for it: its own CPU time,
/// in nanoseconds, and whether it was runnable on no CPU or running ([`Watch::starved`]).
/// Public, with [`starved`], for a reader of its own process's threads (the tests' machine
/// parties, D209 Part 5), which judges them by the same rule (R12).
pub type Threads = std::collections::BTreeMap<u64, (u64, bool)>;

/// What a look reads of every process's threads at once, read once at most for all the
/// readings of one look (D215): on Windows the system's whole listing of every process's
/// threads (NtQuerySystemInformation's SystemProcessInformation, shards-judge's
/// `threads::records`), where alone Windows keeps a thread's state, 13.1 to 13.8 ms of CPU a
/// read on jupiter (PM M606). It is read as the first of a look's readings needs it, and the
/// rest read that read: each followed process's hold ([`Watch::look`]), its threads
/// ([`Watch::starved`]) and its trace (SHARDS_JUDGE_TRACE), and its party's account
/// ([`runnable_threads`](Listing::runnable_threads)). Each reading read the listing anew
/// before: a tests' wait's look at a party of four members read it 16 to 20 times, as its
/// account read each member's runnable threads and thread states again, the judge's own load
/// on the 4-CPU hosts it judged (CI 44347282760, x86_64 Windows, 478,767 trace lines). Linux
/// and macOS keep each process's threads apart (`/proc/<pid>/task`, proc_pidinfo(2)), each
/// read for its process alone, and it holds nothing there. A look makes one, as a party makes
/// one for each of its looks; one kept past its look says what that look saw.
#[derive(Default)]
pub struct Listing {
    /// The system's listing as the look's first reading of it read it, or why it could not.
    #[cfg(windows)]
    records: std::sync::OnceLock<Result<Vec<u64>, String>>,
}

/// Whether it was read, and how many words it holds: never its words, hundreds of KiB.
impl std::fmt::Debug for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut listing = f.debug_struct("Listing");
        #[cfg(windows)]
        listing.field(
            "words",
            &self.records.get().map(|read| read.as_ref().map(Vec::len)),
        );
        listing.finish()
    }
}

impl Listing {
    /// None read yet: each of a look's readings reads it as the first needs it.
    pub fn new() -> Listing {
        Listing::default()
    }

    /// The system's listing as this look read it, or why it could not be read: read now
    /// where no reading of this look has read it yet.
    #[cfg(windows)]
    fn records(&self) -> Result<&[u64], &str> {
        match self.records.get_or_init(shards_judge::threads::records) {
            Ok(words) => Ok(words.as_slice()),
            Err(why) => Err(why.as_str()),
        }
    }

    /// How many of process `pid`'s threads are runnable (running, or waiting for a CPU), of
    /// how many, as [`Watch::starved`] reads them (on Windows, from this listing): for a
    /// waiter's account of its party, so that a refusal names what its members were doing,
    /// and for a test that begins once a process is at rest. None where they cannot be read.
    pub fn runnable_threads(&self, pid: u32) -> Option<(usize, usize)> {
        #[cfg(windows)]
        {
            os::runnable_threads(self.records().ok()?, pid)
        }
        #[cfg(target_os = "linux")]
        let threads = os::threads(pid).ok()?;
        #[cfg(all(unix, not(target_os = "linux")))]
        let threads = os::threads_cpu(pid).ok()?;
        #[cfg(unix)]
        Some((
            threads.values().filter(|(_, runnable)| *runnable).count(),
            threads.len(),
        ))
    }

    /// Each thread of process `pid`, its state and, where it waits, its wait reason, as this
    /// listing shows them (shards-judge's `threads_said`): none where it could not be read or
    /// lists no thread of `pid`.
    #[cfg(windows)]
    pub fn threads_said(&self, pid: u32) -> Option<String> {
        shards_judge::threads_said(self.records().ok()?, pid)
    }

    /// Each thread of processes `pids` but thread `except`, its switches to a CPU and whether
    /// it waits for one, as this listing shows them (shards-judge's `threads_switched`), for
    /// D215's rule of a thread its host keeps from any CPU ([`starved`]); why not, where it
    /// could not be read.
    #[cfg(windows)]
    pub fn threads_switched(&self, pids: &[u32], except: Option<u32>) -> Result<Threads, String> {
        self.records()
            .map(|words| shards_judge::threads_switched(words, pids, except))
            .map_err(str::to_string)
    }
}

/// Each thread of process `pid` as its OS keeps it, for a refusal's account of its party
/// (`Stall::said`): Windows its state and wait reason (`listing`, the refusing look's,
/// [`Listing::threads_said`]), Linux its state and the kernel function it sleeps in, macOS its
/// run state. None where they could not be read.
fn threads_account(pid: u32, listing: &Listing) -> Option<String> {
    #[cfg(windows)]
    {
        listing.threads_said(pid)
    }
    #[cfg(unix)]
    {
        let _ = listing;
        os::threads_said(pid)
    }
}

/// [`threads_account`] of process `pid` from `listing`, with what `watch`'s last look read of
/// its host's work for a thread of it waiting in its kernel ([`Watch::kernel`]).
fn account_of(pid: u32, watch: &Watch, listing: &Listing) -> Option<String> {
    #[cfg(target_os = "linux")]
    let times = os::times_said(&watch.times);
    #[cfg(not(target_os = "linux"))]
    let times: Option<String> = None;
    let unread = watch
        .unread()
        .map(|why| format!("its threads unread at its last look: {why}"));
    let said: Vec<String> = [
        threads_account(pid, listing),
        watch.kernel().map(str::to_string),
        times,
        unread,
    ]
    .into_iter()
    .flatten()
    .collect();
    (!said.is_empty()).then(|| said.join("; "))
}

/// Whether a thread runnable in `now` had no CPU at all since `before`: its host kept it
/// from a CPU through the whole time between the two reads (or it ran less than its OS's
/// counter shows). A thread that ran any CPU, or that `before` did not see, shows nothing:
/// a process that wakes for a moment's housekeeping each window is never starved by it
/// (the gitssh lane's wedged run, ~750 µs a window, which must stay refusable).
pub fn starved(before: &Threads, now: &Threads) -> bool {
    now.iter()
        .any(|(id, &(cpu, runnable))| runnable && before.get(id).is_some_and(|&(was, _)| was == cpu))
}

impl Watch {
    /// `followed`, looked at now, its threads read from `listing` (this look's): none where
    /// its process has gone already.
    pub fn of(mut followed: Followed, listing: &Listing) -> Result<Option<Watch>, ProcessFailure> {
        let Some(resolution) = followed.resolution()? else {
            return Ok(None);
        };
        let Some(last) = followed.look()? else {
            return Ok(None);
        };
        let hold = followed.hold(listing);
        // Its slots' time so far, its baseline: none of it is a window's of this watch's.
        let counted = followed.slots().map_or_else(
            |_| Counted::new(),
            |slots| counted(None, &slots, true, shards_durable::now().unwrap_or(0)).1,
        );
        #[cfg(target_os = "linux")]
        let delays = os::delays(followed.pid());
        Ok(Some(Watch {
            followed,
            last,
            hold,
            resolution,
            threads: None,
            unread: None,
            stored: false,
            kernel: None,
            waited: Ok(None),
            #[cfg(target_os = "linux")]
            idle: None,
            #[cfg(target_os = "linux")]
            times: [None, None],
            #[cfg(windows)]
            trace: std::env::var_os("SHARDS_JUDGE_TRACE").is_some(),
            counted,
            in_writes: Duration::ZERO,
            #[cfg(target_os = "linux")]
            delays,
            withheld: Duration::ZERO,
        }))
    }

    /// Whether its host held a thread of it from any CPU since the call before ([`starved`]):
    /// for a waiter whose party's CPU is its liveness alone, whose looks find it ran (some of
    /// its threads did) while the one its answer waits on sat runnable on no CPU, as a guest of
    /// more vCPUs than its host has CPUs, the host busy besides (ROOT's batch 26, HVF: a
    /// 64-vCPU storm's stream refused 170 ms into its quiet). Its threads are read only here,
    /// so a waiter that never asks pays nothing; on every OS ([`Followed::threads`]: Windows
    /// from `listing`, the look's, its threads' switches to a CPU). False at its first call,
    /// which has nothing before it. Why not where its threads could not be read, kept for its
    /// account ([`unread`](Self::unread)) until a read succeeds: no hold is taken from it, nor
    /// none.
    pub fn starved(&mut self, listing: &Listing) -> Result<bool, String> {
        let now = match self.followed.threads(listing) {
            Ok(now) => now,
            Err(why) => {
                self.threads = None;
                self.unread = Some(why.clone());
                return Err(why);
            }
        };
        self.unread = None;
        let was = self.threads.replace(now);
        Ok(was
            .as_ref()
            .zip(self.threads.as_ref())
            .is_some_and(|(before, now)| starved(before, now)))
    }

    /// Why its threads could not be read at its last call of [`starved`](Self::starved), where
    /// they could not: for its account.
    pub fn unread(&self) -> Option<&str> {
        self.unread.as_deref()
    }

    /// The least time a change of its CPU counter stands for, as its OS says it
    /// ([`Span::resolution`]): a stillness shorter than it is no evidence of none.
    pub fn resolution(&self) -> Duration {
        self.resolution
    }

    /// What it did since the look before ([`Found`]): progressed where its end is under way;
    /// still where it is stopped or frozen, whatever it ran to its stop; progressed where it
    /// ran; held where its host held it from running; still where it did neither of its own;
    /// gone where its process has, or was found exited at the look before and is again.
    ///
    /// An exit its kernel still does is its end under way at each look; an exit done is
    /// progress at the look that first finds it, never again: a process that has exited and
    /// is not yet reaped (a zombie) says its end under way at every look, and one its parent
    /// never reaps would be followed for ever (#77 waits-09, a test's wait on a state; D217, a
    /// killed VM process the daemon's unit test never reaped followed each slice for 5 h 37
    /// min on macOS). On macOS an exit begun is not yet done ([`exited`](Self::exited)): taken
    /// for done, ROOT's first harness runs after a build (r106, r116, r122) found stand-ins
    /// whose exits XNU still did gone at two looks, 18 to 72 ms and more before they could be
    /// reaped, and refused their waits. One rule for every follower (R12): the tests' parties
    /// held it apart before.
    /// Its threads are read from `listing`, the look's.
    pub fn look(&mut self, listing: &Listing) -> Result<Found, ProcessFailure> {
        (self.in_writes, self.withheld) = (Duration::ZERO, Duration::ZERO);
        let was_exited = self.exited();
        let Some(look) = self.followed.look()? else {
            return Ok(Found::Gone);
        };
        let before = std::mem::replace(&mut self.last, look);
        if was_exited && self.exited() {
            return Ok(Found::Gone);
        }
        let hold = std::mem::replace(&mut self.hold, self.followed.hold(listing));
        // Its host's hold its own subject measured over the window this look closes
        // ([`held`](Self::held)): its declared writes' time, the one under way while its volume
        // works; and on Linux its threads' waits for a CPU.
        let worked = self.hold.declared.is_some_and(|w| w.moved == Moved::Worked);
        if let Ok(slots) = self.followed.slots() {
            let now = shards_durable::now().unwrap_or(0);
            let (grown, counted) = counted(Some(&self.counted), &slots, worked, now);
            (self.in_writes, self.counted) = (grown, counted);
        }
        #[cfg(target_os = "linux")]
        {
            let delays = os::delays(self.followed.pid());
            self.withheld = withheld(&self.delays, &delays);
            self.delays = delays;
        }
        #[cfg(unix)]
        {
            self.waited =
                os::waited(hold.queued, self.hold.queued, look.cpu.saturating_sub(before.cpu)).map(Some);
        }
        self.stored = self.hold.io.grown_since(&hold.io);
        self.kernel = self.hold.kernel_since(&hold);
        // Each CPU's idle and iowait ticks since the look before, beside the stalls: unused by
        // the rule, the data for a bound on it (D215, R-D, ROOT's ruling of 2026-11-18).
        #[cfg(target_os = "linux")]
        {
            let (before, now) = (self.idle.take(), self.kernel.is_some().then(os::idle).flatten());
            if let (Some(kernel), Some((at, cpus))) = (self.kernel.as_mut(), now.as_ref()) {
                let read = match &before {
                    Some((then, was)) => {
                        os::idle_since(was, cpus, at.saturating_duration_since(*then), os::stat_tick())
                    }
                    None => "each CPU's idle+iowait unread before".to_string(),
                };
                kernel.push_str(&format!(", {read}"));
            }
            self.idle = now;
            let last = self.times[1].take();
            self.times = [last, os::times(self.followed.pid())];
        }
        let ran = look.ran_since(&before);
        // A diagnosis of what Windows shows of a VM process a look finds ran nothing since the
        // look before, the look that may cut it (W2: one inside WHP's partition calls, cut as
        // stalled on CI's windows-2022): said only of those, so a passing run says nothing.
        #[cfg(windows)]
        if !ran && self.trace {
            self.followed.trace(&look, listing);
        }
        let found = found(ran, look.ending, &hold, &self.hold);
        // Still of its own, unless its owner holds it on purpose (D267): a pause is its host's
        // hold, as a starved thread is, never a stall. A stopped one stays still.
        if found == Found::Still && !self.hold.stopped && Held::holds(&self.followed) {
            return Ok(Found::Held);
        }
        Ok(found)
    }

    /// The time over the window its last look closed that its host held it, as it measured
    /// it, exactly: the most any one of its slots' declared writes took (D286 amended), or,
    /// on Linux, the most any one of its threads waited for a CPU, whichever is more. What its
    /// follower's judge takes out of a stillness and of its rate's wall ([`Span::held`]).
    pub fn held(&self) -> Duration {
        self.in_writes.max(self.withheld)
    }

    /// What its last look read of its host's work for a thread of it waiting in its kernel,
    /// by which that look found it held or still (D215, R-D): none where none waited there.
    pub fn kernel(&self) -> Option<&str> {
        self.kernel.as_deref()
    }

    /// Its process's threads' work by the look before ([`Work`]: their CPU time, on Windows
    /// their cycles): what a rate and C* weigh.
    pub fn work(&self) -> Work {
        self.last.work()
    }

    /// Its threads' wait for a CPU in the window its last look closed, from the look before it,
    /// as its scheduler counts it: the time in that window its host did not let it run, which a
    /// joiner's provider's work is judged without (D217). Linux: their `run_delay` grown
    /// (Documentation/scheduler/sched-stats.rst), which the kernel adds as a wait ends
    /// (kernel/sched/stats.h `sched_info_arrive`), so a wait under way as a look reads it shows
    /// at the first look after it ends, all of it; macOS: their runnable time grown
    /// (proc_pid_rusage(2)'s `ri_runnable_time`, running or waiting), less the CPU they ran.
    /// None before its second look, and on Windows, whose threads keep no count of a wait
    /// that has ended (its listing reads a thread's state as it is looked at, and its switches
    /// in; a wait's length is its event tracing's alone, ETW's context switch events). Why
    /// not where its count could not be read at either end of the window.
    pub fn waited(&self) -> Result<Option<Duration>, String> {
        self.waited.clone()
    }

    /// Whether the look before found its end under way or done: on Unix its process's exit
    /// begun (macOS, before its kernel has done it) or done (a zombie, whose every look says
    /// its end under way), its CPU read through its end; on Windows every process of its tree
    /// exited (their handles read them still).
    pub fn ended(&self) -> bool {
        #[cfg(unix)]
        return self.last.ending;
        #[cfg(windows)]
        return self.followed.ended();
    }

    /// Whether the look before found its exit done: on Unix exited and not yet reaped, a
    /// zombie (on macOS once XNU has freed it, its exit begun before then: `SZOMB`); on
    /// Windows every process of its tree exited. [`look`](Self::look) takes one found so at
    /// two looks running for gone, rather than for an end under way for ever.
    pub fn exited(&self) -> bool {
        #[cfg(unix)]
        return self.last.exited;
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
    /// ([`Follow`]), which moves none of what its next look compares with, and reads what it
    /// reads anew.
    fn held_now(&self) -> Hold {
        self.followed.hold(&Listing::new())
    }

    /// Its process's threads' work by now, the look before kept; none where it has gone or
    /// could not be read.
    fn work_now(&mut self) -> Option<Work> {
        self.followed.look().ok().flatten().map(|look| look.work())
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
    /// A reply of bounded work that no time spent working brings nearer (a VM's READY):
    /// refused as [`Rate`](Self::Rate) is, and past W* once it has done more than C*,
    /// whatever its rate (`Judge::by_work`; D215 amended 2026-11-18, CI 44245219858: nine
    /// VMs ran at their host's fair share for 348 s, never ready, above R*, and were held).
    Work,
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
    /// Its VM process's work as it began.
    from: Work,
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
        let judge = Judge::begin(record);
        let mut judge = match bound {
            Bound::Work => judge.by_work(),
            Bound::Rate | Bound::Stillness => judge,
        };
        let (pid, started) = (followed.pid(), followed.started());
        let began = Instant::now();
        let watched = Watch::of(followed, &Listing::new()).map_err(|failure| Unfollowed::Process {
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
            held: Duration::ZERO,
        };
        judge
            .found_over(Found::Based, span)
            .map_err(Unfollowed::Stalled)?;
        let hold = watch.hold;
        let mut follow = Follow {
            judge,
            bound,
            from: watch.work(),
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
    /// whose samples could show no flush: a flush and a peer's unanswered pipe both wait
    /// `Executive` there, and one read of the system's records took 13 ms (PM M606); its
    /// paging waits are read at its due looks.
    fn schedule(&mut self, hold: &Hold, at: Instant) {
        let blocked = !hold.runnable && hold.io.waited.is_none();
        self.sample = (cfg!(unix) && self.bound != Bound::Stillness && blocked && !self.held)
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
        // One read of what its OS lists of every thread, for all this look reads of them: its
        // hold, its threads, its trace and a refusal's account.
        let listing = Listing::new();
        let found = self.watch.look(&listing).map_err(|failure| Unfollowed::Process {
            pid,
            expected_started,
            failure,
        })?;
        // Its work since it began, for its rate, where its bound is one and this look, and
        // every sample since the look before, vouches for its window; its threads read only
        // then.
        let held = std::mem::take(&mut self.held);
        // A failed read of its threads vouches for nothing; its account says it.
        let work = (self.bound != Bound::Stillness
            && !held
            && self
                .watch
                .starved(&listing)
                .is_ok_and(|starved| self.watch.vouched_by(found, starved)))
        .then(|| self.watch.work().saturating_sub(self.from));
        let span = Span {
            began,
            ended: Instant::now(),
            resolution: self.watch.resolution(),
            own: Duration::ZERO,
            work,
            held: self.watch.held(),
        };
        // Refused, its party's threads as this look read them: what each was doing.
        let judged = self
            .judge
            .found_over(found, span)
            .map_err(|stall| Unfollowed::Stalled(stall.said(account_of(pid, &self.watch, &listing))));
        let hold = self.watch.hold;
        self.schedule(&hold, Instant::now());
        judged
    }

    /// How many of its due looks found its VM process past C* and waited on
    /// ([`Judge::waited_past_most`]).
    pub fn waited_past_most(&self) -> u32 {
        self.judge.waited_past_most()
    }

    /// Its party answered: its kind learns from the answer, its VM process read now; its
    /// slowest alone where it could not be read (D215: only work read through an answer
    /// teaches R* and C*).
    pub fn answered(&mut self) {
        match self.watch.work_now() {
            Some(work) => self.judge.answered(work.saturating_sub(self.from)),
            None => self.judge.answered_unrated(),
        }
    }

    /// Its guest ended as `end`, its VM process having run `cpu` by its end where its watcher
    /// read it before it was reaped (Linux and macOS: its CPU time, its work there): its kind
    /// learns from the end, unless this client stopped it, which is no answer of the guest's.
    pub(crate) fn ended(&mut self, end: &End, cpu: Option<Duration>) {
        if matches!(end, End::Stopped) {
            return;
        }
        // On Windows the tree's handles read its processes' cycles after they have ended; no
        // watcher reads its CPU time, a count of whole ticks, which is no work.
        #[cfg(windows)]
        let work = {
            let _ = cpu;
            self.watch.work_now()
        };
        #[cfg(not(windows))]
        let work = cpu.map(Work::of_cpu);
        match work {
            Some(work) => self.judge.answered(work.saturating_sub(self.from)),
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
    /// Where its home keeps the durable writes it declares (D286): this process's own home's,
    /// unless its follower names another ([`declaring`](Self::declaring)); none where neither.
    declarations: Option<Declarations>,
}

#[cfg(unix)]
impl Followed {
    pub(crate) fn of(identity: &Identity) -> Followed {
        Followed {
            pid: identity.pid,
            started: identity.started,
            declarations: Declarations::installed(),
        }
    }

    /// Process `pid` as it is now, its start read, which tells it from a process given its
    /// pid later; none where no process has it.
    pub fn of_pid(pid: u32) -> Result<Option<Followed>, ProcessFailure> {
        Ok(os::started(pid)?.map(|started| Followed {
            pid,
            started,
            declarations: Declarations::installed(),
        }))
    }

    /// It, its durable writes read from the declarations of the home `home` (D286), where its
    /// follower is not of that home (a test's party, a client of another's).
    pub fn declaring(self, home: &std::path::Path) -> std::io::Result<Followed> {
        Ok(Followed {
            declarations: Some(Declarations::of_home(home)?),
            ..self
        })
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

    /// The slots it holds in its home's slot file ([`Declarations::held`]); none where it reads
    /// no home's declarations.
    fn slots(&self) -> Result<Vec<(u32, u64, shards_durable::Held)>, Unread> {
        self.declarations
            .as_ref()
            .map_or(Ok(Vec::new()), |d| d.held(&[(self.pid, self.started)]))
    }

    /// What its host is doing to it, now ([`Hold`]): its declared durable writes judged by
    /// their volumes, its other threads by their own states, read from its own entries (a
    /// [`Listing`] holds nothing here).
    fn hold(&self, _listing: &Listing) -> Hold {
        let (declared, threads) = self
            .declarations
            .as_ref()
            .map_or((None, Vec::new()), |d| d.writing(&[(self.pid, self.started)]));
        Hold {
            declared,
            ..os::hold(self.pid, &threads)
        }
    }

    /// Its threads now, for D215's rule of a thread its host keeps from any CPU ([`starved`]):
    /// Linux each task's schedstat run and state, macOS each thread's times and run state, read
    /// from its own entries (a [`Listing`] holds nothing here); why not, where they could not
    /// be read.
    fn threads(&self, _listing: &Listing) -> Result<Threads, String> {
        #[cfg(target_os = "linux")]
        return os::threads(self.pid);
        #[cfg(not(target_os = "linux"))]
        return os::threads_cpu(self.pid);
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
        Ok(Some(Look {
            cpu,
            ending,
            exited: ending,
        }))
    }

    /// Process `pid`'s start (`/proc/<pid>/stat` field 22); none where no process has it.
    pub(super) fn started(pid: u32) -> Result<Option<u64>, ProcessFailure> {
        Ok(stat(pid)?.map(|(started, _)| started))
    }

    /// A volume's device, as its counters are read: its block device's statistics file
    /// (`/sys/dev/block/<major>:<minor>/stat`, Documentation/block/stat.rst).
    pub(super) type Device = PathBuf;

    /// The device beneath volume `volume` (a `st_dev`) of process `pid`'s: its own where it is
    /// a block device's; else (an anonymous device's: btrfs's, overlayfs') its mount's in that
    /// process's mount table (`/proc/<pid>/mountinfo`, proc(5)): its source's, a block device's
    /// node, or an overlay's upper layer's, followed until a block device's, each once.
    pub(super) fn device(volume: u64, pid: u32) -> Result<Device, super::Unread> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        let mut seen: Vec<u64> = Vec::new();
        let mut at = volume;
        loop {
            if seen.contains(&at) {
                return Err(super::Unread::NoDevice);
            }
            seen.push(at);
            let (major, minor) = (libc::major(at), libc::minor(at));
            let stat = PathBuf::from(format!("/sys/dev/block/{major}:{minor}/stat"));
            if stat.exists() {
                return Ok(stat);
            }
            let table = std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
                .map_err(|e| super::Unread::Counters(e.raw_os_error()))?;
            let wanted = format!("{major}:{minor}");
            let mount = table
                .lines()
                .find(|line| line.split(' ').nth(2) == Some(wanted.as_str()))
                .and_then(|line| line.split_once(" - "))
                .ok_or(super::Unread::NoDevice)?;
            let mut fields = mount.1.split(' ');
            let (kind, source, options) = (fields.next(), fields.next(), fields.next());
            let beneath = if kind == Some("overlay") {
                options
                    .and_then(|o| o.split(',').find_map(|option| option.strip_prefix("upperdir=")))
                    .and_then(|upper| std::fs::metadata(unescaped(upper)).ok())
                    .map(|upper| upper.dev())
            } else {
                source
                    .filter(|source| source.starts_with('/'))
                    .map(unescaped)
                    .and_then(|source| match std::fs::metadata(&source) {
                        Ok(node) => node.file_type().is_block_device().then(|| node.rdev()),
                        // Its node absent from this process's /dev, as a container's holds
                        // none of its host's disks: its number by its name, from sysfs, which
                        // the container sees.
                        Err(_) => block_number(source.file_name()?),
                    })
            };
            at = beneath.ok_or(super::Unread::NoDevice)?;
        }
    }

    /// A block device's number by its name (`/sys/class/block/<name>/dev`, "MAJOR:MINOR",
    /// Documentation/ABI/stable/sysfs-block): a mount's source whose node this process cannot
    /// see. None where sysfs names no such device.
    fn block_number(name: &std::ffi::OsStr) -> Option<u64> {
        let text = std::fs::read_to_string(Path::new("/sys/class/block").join(name).join("dev")).ok()?;
        let (major, minor) = text.trim().split_once(':')?;
        Some(libc::makedev(major.parse().ok()?, minor.parse().ok()?))
    }

    /// A mount table's field as its path: proc(5)'s octal escapes (`\040` for a space, `\011`,
    /// `\012`, `\134`) undone.
    fn unescaped(field: &str) -> PathBuf {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut k = 0;
        while let Some(&b) = bytes.get(k) {
            let octal = bytes
                .get(k.saturating_add(1)..k.saturating_add(4))
                .filter(|digits| b == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)))
                .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
            match octal {
                Some(byte) => {
                    out.push(byte);
                    k = k.saturating_add(4);
                }
                None => {
                    out.push(b);
                    k = k.saturating_add(1);
                }
            }
        }
        PathBuf::from(std::ffi::OsString::from_vec(out))
    }

    /// What `device` has done (Documentation/block/stat.rst): its read, write, discard and
    /// flush I/Os completed (fields 1, 5, 12 and 16, the last two where its kernel counts
    /// them), its `io_ticks` (field 10) and its I/Os in flight (field 9).
    pub(super) fn counters(device: &Device) -> Result<super::Counters, super::Unread> {
        let text = std::fs::read_to_string(device).map_err(|e| super::Unread::Counters(e.raw_os_error()))?;
        let fields: Vec<u64> = text
            .split_whitespace()
            .map(|field| field.parse().ok())
            .collect::<Option<_>>()
            .ok_or(super::Unread::Counters(None))?;
        let at = |k: usize| fields.get(k).copied();
        let (Some(reads), Some(writes), Some(in_flight), Some(busy)) = (at(0), at(4), at(8), at(9)) else {
            return Err(super::Unread::Counters(None));
        };
        let done = [Some(reads), Some(writes), at(11), at(15)]
            .into_iter()
            .flatten()
            .fold(0u64, u64::saturating_add);
        Ok(super::Counters {
            done,
            busy,
            in_flight,
        })
    }

    /// A volume as its account names it: its device's `major:minor`.
    pub(super) fn volume_name(volume: u64) -> String {
        format!("{}:{}", libc::major(volume), libc::minor(volume))
    }

    /// What its host is doing to process `pid` ([`Hold`]), from its own evidence alone: a
    /// thread of it runnable, its state `R` (`/proc/<pid>/task/<tid>/stat` field 3, proc(5)),
    /// or waiting uninterruptibly in its kernel, `D`, which vouches for no window of its rate,
    /// and then its host's block devices' completed I/O ([`disks`]); it stopped, its leader's
    /// state `T` or `t`, or its cgroup frozen (`cgroup.events`'
    /// `frozen 1`, Documentation/admin-guide/cgroup-v2.rst); its threads' run-queue wait
    /// ([`Queued`]); where its cgroup holds it alone, that cgroup's pressure
    /// (Documentation/accounting/psi.rst), whose `some` totals grow while any of its tasks
    /// waits for a CPU, or is stalled on memory, a task held over its `memory.high` among them
    /// (mm/memcontrol.c, `mem_cgroup_handle_over_high`), or waits on I/O (`io.pressure`, its
    /// [`Io::waited`](super::Io)); and its storage's work for it ([`Io`](super::Io)): its major
    /// faults and storage bytes, and its tasks' block I/O delays while task delay accounting is
    /// on (`kernel.task_delayacct`, Documentation/accounting/delay-accounting.rst). A cgroup it
    /// shares, the root's among them (the host's, `/proc/pressure`), is not read: its pressure
    /// is its other tasks' as much as this one's. What it cannot read shows nothing. A thread
    /// of `declared`, in a durable write it declared (D286), is its declaration's to judge, by
    /// its volume's own counters: its `D` is no witness here.
    pub(super) fn hold(pid: u32, declared: &[u64]) -> Hold {
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
                let tid = task.file_name().to_str().and_then(|tid| tid.parse::<u64>().ok());
                hold.runnable |= state == b'R';
                hold.uninterruptible |= state == b'D' && !tid.is_some_and(|tid| declared.contains(&tid));
                if delays && let Some(ticks) = field(&stat, BLKIO_TICKS) {
                    waited = Some(waited.unwrap_or(0).saturating_add(ticks));
                }
                let leader = tid == Some(u64::from(pid));
                hold.stopped |= leader && matches!(state, b'T' | b't');
            }
        }
        if hold.uninterruptible {
            hold.disks = disks();
            hold.stalls = stalls();
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

    /// The work the host's block devices have done ([`Hold::disks`](super::Hold)), from
    /// `/proc/diskstats`; none where it cannot be read.
    pub(super) fn disks() -> Option<u64> {
        std::fs::read_to_string("/proc/diskstats")
            .ok()
            .map(|text| disks_worked(&text))
    }

    /// Of `/proc/diskstats`' `text`, each line's reads, writes, discards (since 4.18) and
    /// flushes (since 5.5) completed, and the milliseconds its device has spent doing I/O, as
    /// many of them as its kernel lists, summed over every line
    /// (Documentation/admin-guide/iostats.rst: after a line's major, minor and name, its
    /// fields 1, 5, 12 and 16, and field 10, which "increases so long as field 9 is nonzero",
    /// field 9 its I/Os in flight). Field 10 is brought up to date as the file is read while a
    /// device has I/O in flight (block/genhd.c `diskstats_show` calls `update_io_ticks` where
    /// `bdev_count_inflight` is nonzero, linux v6.18.48), so that a flush in flight through a
    /// whole look's window, which completes nothing in it, shows its device working: a journal
    /// commit's waiter in `jbd2_log_wait_commit` (CI 44205094460, x86_64 gnu, referrers.rs:437,
    /// refused as still 46.7 ms in). A partition's are its disk's too, and a mapped device's
    /// its disks': only the sum's growth is read, which any of them shows.
    pub(super) fn disks_worked(text: &str) -> u64 {
        const WORKED: [usize; 5] = [0, 4, 9, 11, 15];
        text.lines()
            .flat_map(|line| {
                line.split_ascii_whitespace()
                    .skip(3)
                    .enumerate()
                    .filter(|(i, _)| WORKED.contains(i))
                    .filter_map(|(_, n)| n.parse::<u64>().ok())
            })
            .fold(0, u64::saturating_add)
    }

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
    /// (`/proc/<pid>/task/<tid>/stat` field 3). A task gone since its tasks were listed has
    /// none to read, and one whose schedstat reads "0 0 0" has never run, which is a reading:
    /// it is left out, as nothing of it has run to show. Any other failure is said: a task
    /// still there with no schedstat (its kernel keeps none, as without CONFIG_SCHED_INFO,
    /// which CONFIG_SCHEDSTATS or CONFIG_TASK_DELAY_ACCT selects; fs/proc/base.c), or one
    /// that cannot be read or parsed.
    pub(super) fn threads(pid: u32) -> Result<super::Threads, String> {
        let listed = |e: std::io::Error| format!("process {pid}'s tasks, /proc/{pid}/task: {e}");
        let mut threads = super::Threads::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).map_err(listed)? {
            let task = task.map_err(listed)?;
            let dir = task.path();
            let tid = task
                .file_name()
                .to_str()
                .and_then(|tid| tid.parse::<u64>().ok())
                .ok_or_else(|| format!("{}: no task ID", dir.display()))?;
            let text = match std::fs::read_to_string(dir.join("schedstat")) {
                Ok(text) => text,
                Err(_) if !dir.exists() => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "{}/schedstat absent: its kernel keeps no schedstat, as without \
                         CONFIG_SCHED_INFO (selected by CONFIG_SCHEDSTATS or CONFIG_TASK_DELAY_ACCT)",
                        dir.display()
                    ));
                }
                Err(e) => return Err(format!("{}/schedstat: {e}", dir.display())),
            };
            let mut fields = text.split_whitespace().map(str::parse::<u64>);
            let (Some(Ok(ran)), Some(Ok(waited)), Some(Ok(slices))) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("{}/schedstat: not three counts: {text:?}", dir.display()));
            };
            if ran == 0 && waited == 0 && slices == 0 {
                continue;
            }
            let stat = match std::fs::read(dir.join("stat")) {
                Ok(stat) => stat,
                Err(_) if !dir.exists() => continue,
                Err(e) => return Err(format!("{}/stat: {e}", dir.display())),
            };
            let state = state(&stat).ok_or_else(|| format!("{}/stat: names no state", dir.display()))?;
            threads.insert(tid, (ran, state == b'R'));
        }
        Ok(threads)
    }

    /// Each thread of process `pid` as its task's files show it ([`super::threads_account`]):
    /// its ID, its state (`/proc/<pid>/task/<tid>/stat` field 3, proc(5)), and, not running,
    /// the kernel function it sleeps in (its `wchan`; "0" where it names none). None where its
    /// tasks cannot be listed or none of them read.
    pub(super) fn threads_said(pid: u32) -> Option<String> {
        let mut said = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.flatten() {
            let Some(tid) = task.file_name().to_str().and_then(|tid| tid.parse::<u64>().ok()) else {
                continue;
            };
            let dir = task.path();
            let Some(state) = std::fs::read(dir.join("stat")).ok().and_then(|stat| state(&stat)) else {
                continue;
            };
            let waits = std::fs::read_to_string(dir.join("wchan"))
                .ok()
                .filter(|waits| state != b'R' && !waits.is_empty() && waits != "0");
            said.push(match waits {
                Some(waits) => format!("{tid} {} in {waits}", char::from(state)),
                None => format!("{tid} {}", char::from(state)),
            });
        }
        (!said.is_empty()).then(|| said.join(", "))
    }

    /// Each task of process `pid` by its ID, and its run-queue wait so far ([`run_delay`]):
    /// what its follower reads the most any one of them waited for a CPU over a window from
    /// ([`super::withheld`]). A task with none to read is left out, and a process whose tasks
    /// cannot be listed has none: no evidence of its host's hold, which then takes nothing out
    /// of its stillness.
    pub(super) fn delays(pid: u32) -> std::collections::BTreeMap<u64, u64> {
        let mut delays = std::collections::BTreeMap::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return delays;
        };
        for task in tasks.flatten() {
            let Some(tid) = task.file_name().to_str().and_then(|tid| tid.parse::<u64>().ok()) else {
                continue;
            };
            if let Some(delay) = run_delay(&task.path()) {
                delays.insert(tid, delay);
            }
        }
        delays
    }

    /// A task's run-queue wait in nanoseconds, its `schedstat`'s second field; none where it
    /// could not be read, or reads as zeros (no accounting, [`Queued::Unread`]).
    pub(super) fn run_delay(task: &Path) -> Option<u64> {
        let text = std::fs::read_to_string(task.join("schedstat")).ok()?;
        let mut fields = text.split_whitespace().map(str::parse::<u64>);
        let (ran, waited, slices) = (fields.next()?.ok()?, fields.next()?.ok()?, fields.next()?.ok()?);
        (ran != 0 || waited != 0 || slices != 0).then_some(waited)
    }

    /// Its threads' wait for a CPU between the looks that read `before` and `now` ([`Watch::waited`](super::Watch::waited)):
    /// their `run_delay` grown, a thread gone meanwhile taking its own out of the sum (none
    /// below none); why not where either look read none.
    pub(super) fn waited(before: Queued, now: Queued, _ran: Duration) -> Result<Duration, String> {
        match (before, now) {
            (Queued::Waited(was), Queued::Waited(is)) => Ok(Duration::from_nanos(is.saturating_sub(was))),
            _ => Err(
                "its threads' wait for a CPU unread: no task of it had a schedstat to read (a kernel without CONFIG_SCHED_INFO, or its tasks unreadable)"
                    .to_string(),
            ),
        }
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

    /// Each thread of a process's user, system and guest time so far, in USER_HZ ticks, by its
    /// ID (proc(5) /proc/<pid>/task/<tid>/stat fields 14, 15 and 43), and its host's time
    /// stolen by the hypervisor it runs under (/proc/stat's `cpu` line's eighth value).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Times {
        threads: Vec<(u32, [u64; 3])>,
        steal: Option<u64>,
    }

    /// Process `pid`'s [`Times`] now; none where none of its threads could be read.
    pub(super) fn times(pid: u32) -> Option<Times> {
        let mut threads = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()?.flatten() {
            let Some(tid) = task.file_name().to_str().and_then(|t| t.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read(task.path().join("stat")) else {
                continue;
            };
            if let (Some(user), Some(system), Some(guest)) =
                (field(&stat, 14), field(&stat, 15), field(&stat, 43))
            {
                threads.push((tid, [user, system, guest]));
            }
        }
        threads.sort_unstable();
        let steal = std::fs::read_to_string("/proc/stat").ok().and_then(|text| {
            let line = text.lines().find(|line| line.starts_with("cpu "))?;
            line.split_ascii_whitespace().nth(8)?.parse().ok()
        });
        (!threads.is_empty()).then_some(Times { threads, steal })
    }

    /// What `times`, its look before's and its last's, say for an account: each thread's
    /// user, system and guest ticks at each, and its host's steal grown between them; none
    /// where its last read none.
    pub(super) fn times_said(times: &[Option<Times>; 2]) -> Option<String> {
        let [before, last] = times;
        let last = last.as_ref()?;
        let at = |tid: u32| {
            before
                .as_ref()
                .and_then(|b| b.threads.iter().find(|(t, _)| *t == tid))
                .map_or_else(|| "unread".to_string(), |(_, [u, s, g])| format!("{u}/{s}/{g}"))
        };
        let threads: Vec<String> = last
            .threads
            .iter()
            .map(|(tid, [u, s, g])| format!("{tid} {} -> {u}/{s}/{g}", at(*tid)))
            .collect();
        let steal = match (before.as_ref().and_then(|b| b.steal), last.steal) {
            (Some(b), Some(n)) => format!("its host's steal +{} ticks", n.saturating_sub(b)),
            _ => "its host's steal unread".to_string(),
        };
        Some(format!(
            "its threads' user/system/guest ticks at its look before -> its last: {}; {steal}",
            threads.join(", ")
        ))
    }

    /// Each CPU's idle and iowait time so far, in USER_HZ ticks: /proc/stat's `cpuN` lines'
    /// fourth and fifth fields (fs/proc/stat.c), read with when; none where unread.
    pub(super) fn idle() -> Option<(std::time::Instant, Vec<u64>)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let at = std::time::Instant::now();
        let cpus: Vec<u64> = text
            .lines()
            .filter(|line| {
                line.strip_prefix("cpu")
                    .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
            })
            .map(|line| {
                let fields: Vec<u64> = line
                    .split_ascii_whitespace()
                    .skip(1)
                    .filter_map(|n| n.parse().ok())
                    .collect();
                let field = |i: usize| fields.get(i).copied().unwrap_or(0);
                field(3).saturating_add(field(4))
            })
            .collect();
        (!cpus.is_empty()).then_some((at, cpus))
    }

    /// The length of one of /proc/stat's ticks (`_SC_CLK_TCK`, USER_HZ), apart from the
    /// kernel's own tick (`tick`); none where unread.
    pub(super) fn stat_tick() -> Option<Duration> {
        // SAFETY: sysconf(3) of a name, which reads nothing of this process's memory.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        u64::try_from(hz)
            .ok()
            .filter(|hz| *hz > 0)
            .map(|hz| Duration::from_nanos(1_000_000_000 / hz))
    }

    /// Each CPU's idle+iowait ticks grown from `was` to `now` over `elapsed`, for a reading:
    /// "under a tick" where `elapsed` is shorter than one of them, as no count could show it.
    pub(super) fn idle_since(was: &[u64], now: &[u64], elapsed: Duration, tick: Option<Duration>) -> String {
        if tick.is_none_or(|tick| elapsed < tick) {
            return format!("each CPU's idle+iowait under a tick ({elapsed:?})");
        }
        let grown: Vec<String> = now
            .iter()
            .zip(was.iter().chain(std::iter::repeat(&0)))
            .map(|(n, b)| format!("+{}", n.saturating_sub(*b)))
            .collect();
        format!(
            "each CPU's idle+iowait ticks {} over {elapsed:?}",
            grown.join(",")
        )
    }

    /// Its host's CPU and memory `some` totals ([`Stalls`](super::Stalls)): none where either
    /// cannot be read, as where the kernel keeps no pressure record (no CONFIG_PSI, or `psi=0`).
    fn stalls() -> Option<super::Stalls> {
        Some(super::Stalls {
            cpu: some_total(Path::new("/proc/pressure/cpu"))?,
            memory: some_total(Path::new("/proc/pressure/memory"))?,
        })
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
    pub(super) fn tick() -> Result<Duration, ProcessFailure> {
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
    /// gone. One whose exit has begun answers with its exit time set, its usage stashed then
    /// (`proc_prepareexit`), and its exit is done once it is a zombie ([`zombie`]), read
    /// between two reads of its start, so that a pid another process got between them shows.
    pub(super) fn look(pid: u32, started: u64) -> Result<Option<Look>, ProcessFailure> {
        let Some(info) = rusage(pid)? else {
            return Ok(None);
        };
        if info.ri_proc_start_abstime != started {
            return Ok(None);
        }
        let ending = info.ri_proc_exit_abstime != 0;
        let exited = if ending {
            let Some(done) = zombie(pid)? else {
                return Ok(None);
            };
            if rusage(pid)?.is_none_or(|again| again.ri_proc_start_abstime != started) {
                return Ok(None);
            }
            done
        } else {
            false
        };
        Ok(Some(Look {
            cpu: times(&info)?,
            ending,
            exited,
        }))
    }

    /// Whether process `pid` is a zombie, its exit done (`pbsi_status` `SZOMB`, sys/proc.h:
    /// "awaiting collection by parent"), asked with a nonzero `arg`, which has XNU look among
    /// the processes whose exit has begun too (`proc_find_zombref`): `proc_find` finds none of
    /// them from `proc_prepareexit`'s `P_REF_DEAD` on, and the pid hash keeps them until they
    /// are reaped (xnu-12377.101.15 bsd/kern/proc_info.c:2198-2214, kern_proc.c:1105-1127,
    /// kern_exit.c:2981). None where no process has `pid`.
    fn zombie(pid: u32) -> Result<Option<bool>, ProcessFailure> {
        /// Any nonzero `arg` has `PROC_PIDT_SHORTBSDINFO` look among exiting processes too.
        const EXITING_TOO: u64 = 1;
        let id = libc::c_int::try_from(pid).map_err(|_| ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::ProcBsdInfo,
        })?;
        let size = libc::c_int::try_from(size_of::<libc::proc_bsdshortinfo>())
            .map_err(|_| ProcessFailure::BufferBounds)?;
        // SAFETY: a proc_bsdshortinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pidinfo(3) of `id` writes at most `size` bytes into `info`, a
        // proc_bsdshortinfo, as PROC_PIDT_SHORTBSDINFO names.
        let got = unsafe {
            libc::proc_pidinfo(
                id,
                libc::PROC_PIDT_SHORTBSDINFO,
                EXITING_TOO,
                (&raw mut info).cast(),
                size,
            )
        };
        if got == size {
            return Ok(Some(info.pbsi_status == libc::SZOMB));
        }
        // libproc answers a failure with 0 and errno (xnu-12377.101.15
        // libsyscall/wrappers/libproc/libproc.c:105-107), and XNU fills this flavor whole or
        // not at all: any other count is an answer it does not give.
        if got != 0 {
            return Err(ProcessFailure::BufferBounds);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        Err(crate::taken::io_failure(ProcessOperation::ProcBsdInfo, e))
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
    /// mach/thread_info.h), and then its host's block storage's completed I/O
    /// (`shards_apple::storage::completed`); its threads' time runnable ([`Queued`]), which
    /// grows over a time it ran no CPU only while one waited for a CPU, as a thread its host
    /// holds through a look's sample does; and its storage's work for it
    /// ([`Io::done`](super::Io)), none of it a time waited, which macOS keeps no count of.
    /// What it cannot read shows nothing.
    pub(super) fn hold(pid: u32, declared: &[u64]) -> Hold {
        let Ok(id) = libc::c_int::try_from(pid) else {
            return Hold::default();
        };
        let states: Vec<libc::c_int> = threads(id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|handle| run_state(id, handle))
            .collect();
        let usage = rusage_v4(id);
        // A thread of `declared`, in a durable write it declared (D286), is its declaration's
        // to judge by its volume's own counters: an uninterruptible wait witnesses here only
        // past those threads' own, read by their IDs (`PROC_PIDTHREADID64INFO`), as the
        // listing's handles name no thread by its ID.
        let waiting = |state: &&libc::c_int| **state == libc::TH_STATE_UNINTERRUPTIBLE;
        let declaring = declared
            .iter()
            .filter(|&&thread| run_state_of_id(id, thread) == Some(libc::TH_STATE_UNINTERRUPTIBLE))
            .count();
        let uninterruptible = states.iter().filter(waiting).count() > declaring;
        Hold {
            queued: usage.map_or(Queued::Unread, |u| Queued::Waited(u.ri_runnable_time)),
            runnable: states.contains(&libc::TH_STATE_RUNNING),
            uninterruptible,
            disks: uninterruptible.then(shards_apple::storage::completed).flatten(),
            stalls: None,
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
            declared: None,
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

    /// Its threads' wait for a CPU between the looks that read `before` and `now`, `ran` the
    /// CPU they ran meanwhile ([`Watch::waited`](super::Watch::waited)): their runnable time grown, less
    /// what they ran (none below none); why not where either look read none.
    pub(super) fn waited(before: Queued, now: Queued, ran: Duration) -> Result<Duration, String> {
        match (before, now) {
            (Queued::Waited(was), Queued::Waited(is)) => {
                let runnable = mach(u128::from(is.saturating_sub(was))).map_err(|e| e.to_string())?;
                Ok(runnable.saturating_sub(ran))
            }
            _ => Err(
                "its threads' runnable time unread: proc_pid_rusage(2) RUSAGE_INFO_V4 read none".to_string(),
            ),
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

    /// Process `pid`'s threads ([`Threads`](super::Threads)), by their handles: each one's user
    /// and system time (`pth_user_time`, `pth_system_time`, nanoseconds, sys/proc_info.h), and
    /// whether its run state is `TH_STATE_RUNNING` (running, or waiting for a CPU). A thread
    /// that ended since it was listed has none to read ([`thread_info_read`]); any other
    /// failure is said.
    pub(super) fn threads_cpu(pid: u32) -> Result<super::Threads, String> {
        let id = libc::c_int::try_from(pid).map_err(|e| format!("process {pid}: {e}"))?;
        let handles = threads(id).ok_or_else(|| {
            format!(
                "process {pid}'s threads (PROC_PIDTASKINFO, PROC_PIDLISTTHREADS): {}",
                std::io::Error::last_os_error()
            )
        })?;
        let mut read = super::Threads::new();
        for handle in handles {
            if let Some(info) = thread_info_read(id, handle)? {
                let cpu = info.pth_user_time.saturating_add(info.pth_system_time);
                read.insert(handle, (cpu, info.pth_run_state == libc::TH_STATE_RUNNING));
            }
        }
        Ok(read)
    }

    /// Process `id`'s thread `handle` (`PROC_PIDTHREADINFO`); none where it could not be
    /// read, as of a thread that ended since it was listed.
    fn thread_info(id: libc::c_int, handle: u64) -> Option<libc::proc_threadinfo> {
        thread_info_read(id, handle).ok().flatten()
    }

    /// Process `id`'s thread `handle` (`PROC_PIDTHREADINFO`): none where it has ended since it
    /// was listed (ESRCH), and why not where it could not be read otherwise.
    fn thread_info_read(id: libc::c_int, handle: u64) -> Result<Option<libc::proc_threadinfo>, String> {
        // SAFETY: a proc_threadinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_threadinfo>())
            .map_err(|e| format!("a proc_threadinfo's size: {e}"))?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got =
            unsafe { libc::proc_pidinfo(id, libc::PROC_PIDTHREADINFO, handle, (&raw mut info).cast(), size) };
        if got == size {
            return Ok(Some(info));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        Err(format!(
            "process {id}'s thread {handle:#x} (PROC_PIDTHREADINFO): {got} bytes: {e}"
        ))
    }

    /// The run state of process `id`'s thread `handle` (`pth_run_state`); none where it could
    /// not be read, as of a thread that ended since it was listed.
    fn run_state(id: libc::c_int, handle: u64) -> Option<libc::c_int> {
        thread_info(id, handle).map(|info| info.pth_run_state)
    }

    /// `PROC_PIDTHREADID64INFO` (sys/proc_info.h; libc binds no name for it): a thread's
    /// `proc_threadinfo` by its system-wide ID (pthread_threadid_np(3)), as a declaration
    /// names it.
    const PROC_PIDTHREADID64INFO: libc::c_int = 15;

    /// The run state of process `id`'s thread whose system-wide ID is `thread`; none where it
    /// could not be read.
    fn run_state_of_id(id: libc::c_int, thread: u64) -> Option<libc::c_int> {
        // SAFETY: a proc_threadinfo, plain data, for proc_pidinfo to fill.
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(size_of::<libc::proc_threadinfo>()).ok()?;
        // SAFETY: proc_pidinfo(3) writes at most `size` bytes into `info`.
        let got =
            unsafe { libc::proc_pidinfo(id, PROC_PIDTHREADID64INFO, thread, (&raw mut info).cast(), size) };
        (got == size).then_some(info.pth_run_state)
    }

    /// A volume's device, as its counters are read: its BSD name (`disk3s5`).
    pub(super) type Device = std::ffi::CString;

    unsafe extern "C" {
        /// devname_r(3): a device number's name in /dev, into a buffer of the caller's.
        fn devname_r(
            dev: libc::dev_t,
            kind: libc::mode_t,
            buf: *mut libc::c_char,
            len: libc::c_int,
        ) -> *mut libc::c_char;
    }

    /// The BSD name of the block device `volume` (a `st_dev`) names (devname_r(3)); none
    /// where /dev has none. Its room: a directory entry's most, `MAXNAMLEN` (255, sys/dirent.h),
    /// and its NUL.
    fn bsd_name(volume: u64) -> Option<std::ffi::CString> {
        // dev_t is i32 here: the volume's low 32 bits are its.
        #[allow(clippy::cast_possible_truncation)]
        let dev = volume as libc::dev_t;
        let mut name = [0 as libc::c_char; 256];
        let len = libc::c_int::try_from(name.len()).ok()?;
        // SAFETY: devname_r(3) into a buffer of ours of `len` bytes, which it NUL-terminates.
        let named = unsafe { devname_r(dev, libc::S_IFBLK, name.as_mut_ptr(), len) };
        if named.is_null() {
            return None;
        }
        // SAFETY: devname_r returned `name`, NUL-terminated within it.
        let found = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        (!found.is_empty()).then(|| found.to_owned())
    }

    /// The device beneath volume `volume` (a `st_dev`): its block device's BSD name, whose
    /// media IOKit finds by it.
    pub(super) fn device(volume: u64, _pid: u32) -> Result<Device, super::Unread> {
        bsd_name(volume).ok_or(super::Unread::NoDevice)
    }

    /// What `device`'s storage driver has completed (shards-apple `volume_completed`).
    pub(super) fn counters(device: &Device) -> Result<super::Counters, super::Unread> {
        use shards_apple::storage::VolumeUnread;
        match shards_apple::storage::volume_completed(device) {
            Ok(completed) => Ok(super::Counters {
                done: completed.done,
                busy: completed.busy,
                in_flight: 0,
            }),
            Err(VolumeUnread::NoMedia | VolumeUnread::NoDriver) => Err(super::Unread::NoDevice),
            Err(VolumeUnread::Parents(code)) => Err(super::Unread::Counters(Some(code))),
            Err(VolumeUnread::Framework | VolumeUnread::NoStatistics) => Err(super::Unread::Counters(None)),
        }
    }

    /// A volume as its account names it: its BSD name, else its number.
    pub(super) fn volume_name(volume: u64) -> String {
        bsd_name(volume).map_or_else(
            || format!("device {volume:#x}"),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// Each thread of process `pid` as `PROC_PIDTHREADINFO` shows it
    /// ([`super::threads_account`]): its handle and its run state (`pth_run_state`, mach's
    /// TH_STATE_*). macOS keeps no wait reason another process may read. None where its threads
    /// cannot be listed or none of them read.
    pub(super) fn threads_said(pid: u32) -> Option<String> {
        let id = libc::c_int::try_from(pid).ok()?;
        let said: Vec<String> = threads(id)?
            .into_iter()
            .filter_map(|handle| {
                let state = match run_state(id, handle)? {
                    libc::TH_STATE_RUNNING => "running".to_string(),
                    libc::TH_STATE_STOPPED => "stopped".to_string(),
                    libc::TH_STATE_WAITING => "waiting".to_string(),
                    libc::TH_STATE_UNINTERRUPTIBLE => "waiting uninterruptibly".to_string(),
                    libc::TH_STATE_HALTED => "halted".to_string(),
                    other => format!("in state {other}"),
                };
                Some(format!("{handle:#x} {state}"))
            })
            .collect();
        (!said.is_empty()).then(|| said.join(", "))
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
    use super::{Declarations, Hold, Listing, Look};
    use crate::taken::io_failure;
    use crate::{ProcessFailure, ProcessOperation};
    use shards_judge::threads::{WAITING, each_thread, in_kernel, is_suspension, wants_cpu};
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
        FILETIME, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, QueryDosDeviceW,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE};
    use windows_sys::Win32::System::Threading::{
        GetProcessIoCounters, GetProcessTimes, IO_COUNTERS, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;

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
        /// Its root alone, none of what descends from it ([`of_pid_alone`](Followed::of_pid_alone)).
        alone: bool,
        /// Where its home keeps the durable writes its processes declare (D286): this
        /// process's own home's, unless its follower names another
        /// ([`declaring`](Followed::declaring)); none where neither.
        declarations: Option<Declarations>,
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
                alone: false,
                declarations: Declarations::installed(),
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
                alone: false,
                declarations: Declarations::installed(),
            }))
        }

        /// It, the durable writes its processes declare read from the declarations of the home
        /// `home` (D286), where its follower is not of that home (a test's party, a client of
        /// another's).
        pub fn declaring(self, home: &std::path::Path) -> io::Result<Followed> {
            Ok(Followed {
                declarations: Some(Declarations::of_home(home)?),
                ..self
            })
        }

        /// Process `pid` alone, as on Unix: none of what descends from it, for a follower
        /// that follows each process of a tree as its own (the E2E harness's party, whose
        /// daemon's run is told from its VM processes', D215). None where no process has
        /// `pid`; one this user may not open is refused.
        pub fn of_pid_alone(pid: u32) -> Result<Option<Followed>, ProcessFailure> {
            Ok(Followed::of_pid(pid)?.map(|followed| Followed {
                alone: true,
                ..followed
            }))
        }

        pub(super) fn pid(&self) -> u32 {
            self.pid
        }

        /// Whether every process of its tree has been seen to exit.
        pub(super) fn ended(&self) -> bool {
            self.members.iter().all(|m| m.exited)
        }

        /// The threads of its processes not seen to exit, from `listing`, the look's read of the
        /// system's whole listing, for D215's rule of a thread its host keeps from any CPU
        /// ([`super::starved`]): each one's switches to a CPU, doubled and one more while it
        /// runs, and whether it waits for one (shards-judge's `threads_switched`); why not,
        /// where the listing could not be read.
        pub(super) fn threads(&self, listing: &Listing) -> Result<super::Threads, String> {
            let pids: Vec<u32> = self.members.iter().filter(|m| !m.exited).map(|m| m.pid).collect();
            listing.threads_switched(&pids, None)
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

        /// The slots its live processes hold in its home's slot file
        /// ([`Declarations::held`](super::Declarations)); none where it reads no home's
        /// declarations.
        pub(super) fn slots(&self) -> Result<Vec<(u32, u64, shards_durable::Held)>, super::Unread> {
            let live: Vec<(u32, u64)> = self
                .members
                .iter()
                .filter(|m| !m.exited)
                .map(|m| (m.pid, m.created))
                .collect();
            self.declarations
                .as_ref()
                .map_or(Ok(Vec::new()), |d| d.held(&live))
        }

        /// What its host is doing to its processes ([`Hold`]), from `listing`, the look's read of
        /// the system's records of every process and its threads (NtQuerySystemInformation's
        /// SystemProcessInformation: each process's record followed by its threads', their
        /// states and wait reasons). A thread of a live one runnable where its state is ready,
        /// running or standby; in its kernel where it waits on its host's paging or a kernel lock ([`in_kernel`]),
        /// its host's disks then read ([`disks`]); stopped where every thread of its live
        /// processes is suspended (Suspended, WrSuspended), as a frozen Job's are. What it
        /// cannot read shows nothing.
        pub(super) fn hold(&self, listing: &Listing) -> Hold {
            // Its live processes' declared durable writes (D286), judged by their volumes alone:
            // their threads' paging waits witness nothing here.
            let live: Vec<(u32, u64)> = self
                .members
                .iter()
                .filter(|m| !m.exited)
                .map(|m| (m.pid, m.created))
                .collect();
            let (declared, declaring) = self
                .declarations
                .as_ref()
                .map_or((None, Vec::new()), |d| d.writing(&live));
            let Ok(records) = listing.records() else {
                return Hold {
                    declared,
                    ..Hold::default()
                };
            };
            let (mut runnable, mut kernel, mut threads, mut suspended) = (false, false, 0usize, 0usize);
            each_thread(records, |pid, t| {
                if !self.members.iter().any(|m| m.pid == pid && !m.exited) {
                    return;
                }
                let (state, reason) = (t.ThreadState, t.WaitReason);
                let tid = t.ClientId.UniqueThread as usize as u64;
                threads = threads.saturating_add(1);
                runnable |= wants_cpu(state);
                kernel |= in_kernel(state, reason) && !declaring.contains(&tid);
                suspended = suspended.saturating_add(usize::from(state == WAITING && is_suspension(reason)));
            });
            Hold {
                runnable,
                stopped: threads > 0 && suspended == threads,
                uninterruptible: kernel,
                disks: kernel.then(disks).flatten(),
                stalls: None,
                io: super::Io {
                    done: self.io_done(),
                    waited: None,
                },
                declared,
            }
        }

        /// Its processes' I/O so far, each one's read, write and other operations and their
        /// bytes summed (GetProcessIoCounters, IO_COUNTERS, processthreadsapi.h), an exited one's
        /// as its handle keeps them, so the sum only grows; none where none could be read.
        /// Windows counts its storage's work with its other I/O, a pipe's or a socket's, so a
        /// window in which a process did any I/O vouches for none of its rate ([`Io`]): a wait
        /// reason cannot tell them apart instead, as a thread in FlushFileBuffers and one in a
        /// pipe's read no one answers both wait `Executive` (PM M606), and Windows keeps no time
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
        /// QueryProcessCycleTime) and each thread of its live processes as `listing`, the
        /// look's read of the system's records, shows it: its process and thread, state and
        /// wait reason (KWAIT_REASON), context switches, and kernel and user time in 100 ns
        /// units. What cannot be read is said so. Each line names its VM process, and all go
        /// in one write, so that a log that interleaves other tests' output keeps each line
        /// whole and its own.
        pub(super) fn trace(&self, look: &Look, listing: &Listing) {
            use std::io::Write as _;
            let mut said = format!(
                "shards judge: VM process {}: cpu {:?}, cycles {}, ending {}",
                self.pid, look.cpu, look.cycles, look.ending
            );
            match listing.records() {
                Err(why) => said.push_str(&format!("; its threads unread: {why}")),
                Ok(records) => each_thread(records, |pid, t| {
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
            if !self.alone {
                self.grow()?;
            }
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

    /// How many of process `pid`'s threads want or have a CPU (ready, running, standby), of
    /// how many, from `words`, a read of the system's whole listing, as [`Followed::hold`]
    /// reads its members' ([`Listing::runnable_threads`]); none where it lists no thread of
    /// `pid`.
    pub(super) fn runnable_threads(words: &[u64], pid: u32) -> Option<(usize, usize)> {
        let (mut runnable, mut threads) = (0usize, 0usize);
        each_thread(words, |of, t| {
            if of == pid {
                threads = threads.saturating_add(1);
                runnable = runnable.saturating_add(usize::from(wants_cpu(t.ThreadState)));
            }
        });
        (threads > 0).then_some((runnable, threads))
    }

    /// A volume's device, as its counters are read: a handle to the volume itself
    /// (`\\?\Volume{GUID}`), opened for no access, its metadata alone.
    #[derive(Debug)]
    pub(super) struct Device(OwnedHandle);

    /// The volume whose serial number is `volume` (GetVolumeInformationW's, as a declaration
    /// names it): each volume of the host by its GUID path (FindFirstVolumeW, FindNextVolumeW),
    /// its serial read at its root, the one that has it opened as itself (its path without its
    /// trailing backslash, which names its root directory) for no access, as IOCTL_DISK_PERFORMANCE
    /// asks FILE_ANY_ACCESS and the volume manager answers it for volumes (ntdddisk.h).
    /// `MAX_PATH` characters: the most a volume's GUID path takes, as FindFirstVolumeW's
    /// documentation sizes its buffer.
    pub(super) fn device(volume: u64, _pid: u32) -> Result<Device, super::Unread> {
        use windows_sys::Win32::Foundation::MAX_PATH;
        use windows_sys::Win32::Storage::FileSystem::{
            FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetVolumeInformationW,
        };
        let failed = || super::Unread::Counters(io::Error::last_os_error().raw_os_error());
        let room = usize::try_from(MAX_PATH).map_err(|_| super::Unread::Counters(None))?;
        let mut name: Vec<u16> = vec![0; room];
        // SAFETY: a buffer of MAX_PATH characters for the first volume's GUID path; the search
        // handle closed below.
        let search = unsafe { FindFirstVolumeW(name.as_mut_ptr(), MAX_PATH) };
        if search == INVALID_HANDLE_VALUE {
            return Err(failed());
        }
        let found = loop {
            let mut serial = 0u32;
            // SAFETY: a NUL-terminated root path, and the serial alone asked for.
            let read = unsafe {
                GetVolumeInformationW(
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    &raw mut serial,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if read != 0 && u64::from(serial) == volume {
                break Some(name.clone());
            }
            // SAFETY: the search handle above, and the buffer for the next volume's path.
            if unsafe { FindNextVolumeW(search, name.as_mut_ptr(), MAX_PATH) } == 0 {
                break None;
            }
        };
        // SAFETY: the search handle FindFirstVolumeW made, closed once.
        unsafe { FindVolumeClose(search) };
        let mut path = found.ok_or(super::Unread::NoDevice)?;
        let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        if let Some(last) = end.checked_sub(1).and_then(|at| path.get_mut(at))
            && *last == u16::from(b'\\')
        {
            *last = 0;
        }
        // SAFETY: a NUL-terminated volume path, opened for no access, shared both ways, with
        // no security attributes or template; the handle owned below.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(failed());
        }
        // SAFETY: the handle CreateFileW just made, owned from here alone.
        Ok(Device(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }

    /// What `device`'s volume has done (IOCTL_DISK_PERFORMANCE, the volume manager's
    /// DISK_PERFORMANCE): the bytes it read and wrote, the time its reads and writes took, and
    /// its I/Os queued now (`QueueDepth`). Bytes, as its counts of reads and writes are 32
    /// bits wide and wrap.
    pub(super) fn counters(device: &Device) -> Result<super::Counters, super::Unread> {
        let mut performance = DISK_PERFORMANCE::default();
        let size = u32::try_from(size_of::<DISK_PERFORMANCE>()).map_err(|_| super::Unread::Counters(None))?;
        let mut returned = 0u32;
        // SAFETY: an IOCTL with no input, into a DISK_PERFORMANCE of ours of `size` bytes, on
        // the volume handle `device` holds, waited for (no OVERLAPPED).
        let asked = unsafe {
            DeviceIoControl(
                device.0.as_raw_handle(),
                IOCTL_DISK_PERFORMANCE,
                std::ptr::null(),
                0,
                (&raw mut performance).cast(),
                size,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if asked == 0 {
            return Err(super::Unread::Counters(io::Error::last_os_error().raw_os_error()));
        }
        if returned < size {
            return Err(super::Unread::Counters(None));
        }
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        Ok(super::Counters {
            done: n(performance.BytesRead).saturating_add(n(performance.BytesWritten)),
            busy: n(performance.ReadTime).saturating_add(n(performance.WriteTime)),
            in_flight: u64::from(performance.QueueDepth),
        })
    }

    /// A volume as its account names it: its serial number as `vol` says it.
    pub(super) fn volume_name(volume: u64) -> String {
        format!("volume {:04X}-{:04X}", (volume >> 16) & 0xFFFF, volume & 0xFFFF)
    }

    /// The MS-DOS device names of physical disks (`\\.\PhysicalDrive0`, Win32's "Naming Files,
    /// Paths, and Namespaces"): this, then the disk's number.
    const PHYSICAL_DRIVE: &str = "PhysicalDrive";

    /// The bytes its host's physical disks have read and written ([`Hold::disks`]), every
    /// disk's summed: each found by its MS-DOS name ([`PHYSICAL_DRIVE`], in QueryDosDeviceW's
    /// list of every name), opened for no access, its metadata alone, as Win32's "Calling
    /// DeviceIoControl" opens `\\.\PhysicalDrive0`, and asked IOCTL_DISK_PERFORMANCE, an IOCTL
    /// of any access (winioctl.h's DISK_PERFORMANCE, `BytesRead` and `BytesWritten`). Bytes, as
    /// its counts of reads and writes are 32 bits wide and wrap, which would read as no I/O;
    /// only growth is read. None where no disk could be read. Every call is kernel32's, loaded
    /// in every process.
    pub(super) fn disks() -> Option<u64> {
        let names = dos_device_names()?;
        let mut drives: Vec<String> = names
            .split(|&c| c == 0)
            .filter_map(|name| String::from_utf16(name).ok())
            .filter(|name| {
                name.strip_prefix(PHYSICAL_DRIVE)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .collect();
        // A name a session's devices and the system's both list is one disk.
        drives.sort_unstable();
        drives.dedup();
        drives
            .iter()
            .filter_map(|name| disk_bytes(name))
            .fold(None, |sum, bytes| Some(sum.unwrap_or(0u64).saturating_add(bytes)))
    }

    /// Every MS-DOS device name this process sees, each ended by a NUL (QueryDosDeviceW with no
    /// name), in room grown from one character, twice as much each time the call refuses it as
    /// too little (ERROR_INSUFFICIENT_BUFFER), as it says no size it needs; none where it fails
    /// otherwise or would need more characters than a u32 counts.
    fn dos_device_names() -> Option<Vec<u16>> {
        let mut names: Vec<u16> = vec![0];
        loop {
            let room = u32::try_from(names.len()).ok()?;
            // SAFETY: no device named (null), and `room` characters of `names` for the call to
            // fill.
            let got = unsafe { QueryDosDeviceW(std::ptr::null(), names.as_mut_ptr(), room) };
            if got != 0 {
                names.truncate(usize::try_from(got).ok()?);
                return Some(names);
            }
            if !is(&io::Error::last_os_error(), ERROR_INSUFFICIENT_BUFFER) {
                return None;
            }
            let want = names.len().checked_mul(2)?;
            names.try_reserve_exact(want.saturating_sub(names.len())).ok()?;
            names.resize(want, 0);
        }
    }

    /// The bytes physical disk `name` (a [`PHYSICAL_DRIVE`]) has read and written
    /// (IOCTL_DISK_PERFORMANCE); none where it could not be opened or asked.
    fn disk_bytes(name: &str) -> Option<u64> {
        let path: Vec<u16> = format!(r"\\.\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: a NUL-terminated path, opened for no access, shared both ways, with no
        // security attributes or template; the handle owned below.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: the handle CreateFileW just made, owned from here alone.
        let disk = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut performance = DISK_PERFORMANCE::default();
        let size = u32::try_from(size_of::<DISK_PERFORMANCE>()).ok()?;
        let mut returned = 0u32;
        // SAFETY: an IOCTL with no input, into a DISK_PERFORMANCE of ours of `size` bytes, on
        // the disk handle this holds, waited for (no OVERLAPPED).
        let asked = unsafe {
            DeviceIoControl(
                disk.as_raw_handle(),
                IOCTL_DISK_PERFORMANCE,
                std::ptr::null(),
                0,
                (&raw mut performance).cast(),
                size,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if asked == 0 || returned < size {
            return None;
        }
        let bytes = |n: i64| u64::try_from(n).unwrap_or(0);
        Some(bytes(performance.BytesRead).saturating_add(bytes(performance.BytesWritten)))
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

    /// A look counts each slot's growth since the look before, the write under way only while
    /// its volume works, and the most of its slots, never their sum; a slot first seen after
    /// the first look counts whole, and none counts at the first; what a look counts of a
    /// slot never falls (D286 amended, #36).
    #[test]
    fn a_look_counts_the_most_any_slot_of_its_party_held_it() {
        let slot = |slot: u64, since: u64, spent: u64| {
            (
                7u32,
                11u64,
                shards_durable::Held {
                    slot,
                    thread: 1,
                    volume: 3,
                    since,
                    spent,
                },
            )
        };
        let (first, before) = counted(None, &[slot(0, 900, 100), slot(1, 0, 50)], false, 1_000);
        assert_eq!(first, Duration::ZERO, "its baseline");
        // Slot 0's write, counted to 1000 at the first, ended with 600 more; slot 1 wrote 30
        // more; slot 2, claimed since, wrote 40.
        let (grown, after) = counted(
            Some(&before),
            &[slot(0, 0, 700), slot(1, 0, 80), slot(2, 0, 40)],
            false,
            2_000,
        );
        assert_eq!(grown, Duration::from_nanos(500), "slot 0's, the most");
        let (worked, _) = counted(Some(&after), &[slot(0, 1_800, 700)], true, 2_000);
        assert_eq!(worked, Duration::from_nanos(200), "under way, its volume working");
        let (idle, kept) = counted(Some(&after), &[slot(0, 1_800, 700)], false, 2_000);
        assert_eq!(idle, Duration::ZERO, "under way, its volume idle");
        assert_eq!(kept.get(&(7, 11, 0)), Some(&700), "what it counted never falls");
    }

    /// A window's withholding is the most any one thread waited for a CPU over it, a thread
    /// read at one end alone showing nothing (D215, Liveness; #36).
    #[test]
    fn a_windows_withholding_is_the_most_any_one_thread_waited() {
        let map = |pairs: &[(u64, u64)]| {
            pairs
                .iter()
                .copied()
                .collect::<std::collections::BTreeMap<u64, u64>>()
        };
        let before = map(&[(1, 100), (2, 1_000), (3, 50)]);
        let now = map(&[(1, 400), (2, 1_100), (4, 9_999)]);
        assert_eq!(withheld(&before, &now), Duration::from_nanos(300), "thread 1's");
        assert_eq!(withheld(&now, &now), Duration::ZERO);
    }
    use crate::Rate;

    /// Some work an answer did, where a test weighs only that it did some: the record it
    /// teaches is rated, its R* and C* this, and judged by its W* alone where its bound is
    /// stillness.
    const SOME_WORK: Work = Work::of(1);

    /// A declared durable write is its host's while its volume works, whatever its thread's
    /// state (D286): held where its volume worked since its last reading or holds I/O in
    /// flight; still where its volume was read first now, moved nothing, or could not be read;
    /// vouching for no window either way; and its account says which, an unreadable volume
    /// with why, never silently.
    #[test]
    fn a_declared_write_is_held_while_its_volume_works() {
        let writing = |moved| Hold {
            declared: Some(Writing {
                volume: 0,
                lasted: Duration::from_millis(1800),
                moved,
            }),
            ..Hold::default()
        };
        let before = Hold::default();
        assert!(writing(Moved::Worked).held_since(&before));
        assert!(writing(Moved::Worked).in_kernel());
        assert_eq!(found(false, false, &before, &writing(Moved::Worked)), Found::Held);
        for still in [Moved::Cold, Moved::Idle, Moved::Unreadable(0, Unread::NoDevice)] {
            assert!(!writing(still).held_since(&before), "{still:?}");
            assert!(writing(still).in_kernel(), "{still:?}");
            assert_eq!(
                found(false, false, &before, &writing(still)),
                Found::Still,
                "{still:?}"
            );
        }
        let idle = writing(Moved::Idle).kernel_since(&before).unwrap();
        assert!(idle.contains("moved nothing since the look before"), "{idle}");
        let unread = writing(Moved::Unreadable(7, Unread::Counters(Some(5))))
            .kernel_since(&before)
            .unwrap();
        assert!(unread.contains("counters unreadable"), "{unread}");
        let declarations = writing(Moved::Unreadable(0, Unread::Declarations(Some(2))))
            .kernel_since(&before)
            .unwrap();
        assert!(
            declarations.contains("its home's declarations unreadable"),
            "{declarations}"
        );
    }

    /// A file's volume has its device's counters read, the device found from the volume as a
    /// declaration names it (D286): this test's directory's, on each host; and a durable write
    /// there is work they count.
    #[test]
    fn a_files_volume_has_its_devices_counters_read() {
        let dir = shards_testdir::TempDir::new("volume").unwrap();
        let mut file = std::fs::File::create(dir.join("written")).unwrap();
        #[cfg(unix)]
        let volume = std::os::unix::fs::MetadataExt::dev(&file.metadata().unwrap());
        // Its serial on Windows as shards-durable reads it (GetVolumeInformationByHandleW, which
        // asks no access right of a handle opened to write).
        #[cfg(windows)]
        let volume = {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
            let mut serial = 0u32;
            // SAFETY: a handle the test holds; its serial alone asked, into a u32 of its own.
            let read = unsafe {
                GetVolumeInformationByHandleW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    &raw mut serial,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_ne!(read, 0, "{}", std::io::Error::last_os_error());
            u64::from(serial)
        };
        let device = os::device(volume, std::process::id()).unwrap();
        let first = os::counters(&device).unwrap();
        std::io::Write::write_all(&mut file, &vec![0x5a_u8; 1 << 20]).unwrap();
        shards_durable::all(&file).unwrap();
        let then = os::counters(&device).unwrap();
        assert!(then.worked_since(&first), "{first:?} then {then:?}");
        assert!(!os::volume_name(volume).is_empty());
    }

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
        record.teach(slowest, SOME_WORK);
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
        // Its threads said as it was refused: what each was doing (ROOT, 2026-11-18).
        assert!(stall.threads().is_some_and(|said| !said.is_empty()), "{stall:?}");
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
        record.teach(slowest, SOME_WORK);
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

    /// D215's rate bound on real processes, a kind taught by two answers, each a burst of
    /// 100 ms of work in a W* of 200 ms, as R* and C* refuse once two have taught them: R* 1/2
    /// and C* that burst's work, as this host counts a child's ([`Work`]: on Windows its
    /// cycles, which no rate makes a time). Each burst of work is 10 ms or more, so that a
    /// macOS CPU counter moving in ticks of 10 ms shows it in every window, and stillness never
    /// decides: a process asleep for good but ticking, 10 ms of work in each 100 ms, is refused
    /// for its rate under `Bound::Rate` and goes on under `Bound::Stillness` (a user's command
    /// asleep by design); a busy one goes on; and one doing its kind's work over more time
    /// than any answer took (a peer it waits on, or a host that slows it: 60 ms of work over
    /// some 600 ms) goes on to its end, its work within C*.
    #[test]
    fn a_rate_bound_refuses_only_a_ticking_sleeper() {
        let ms = Duration::from_millis;
        let record = Record::new();
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
        // C*: a burst of 100 ms's work, as this host counts it, a child's: its watch begun
        // before its go, and looked at once it says it spun.
        let most = {
            let mut burst = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", SPINNING, "--nocapture"])
                .env(SPIN, "burst")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut lines = std::io::BufRead::lines(std::io::BufReader::new(burst.stdout.take().unwrap()));
            let mut said = |word: &str| {
                lines
                    .by_ref()
                    .any(|line| line.is_ok_and(|line| line.starts_with(word)))
            };
            assert!(said("started"), "the burst said nothing of its start");
            let mut watch = Watch::of(followed(&burst), &Listing::new()).unwrap().unwrap();
            let from = watch.work();
            std::io::Write::write_all(burst.stdin.as_mut().unwrap(), b"g").unwrap();
            assert!(said("spun"), "the burst said nothing of its spin");
            watch.look(&Listing::new()).unwrap();
            end(burst);
            watch.work().saturating_sub(from)
        };
        record.teach(ms(200), most);
        record.teach(ms(200), most);
        let mut ticker = started("tick");
        let refused = judged(&mut ticker, Bound::Rate, Duration::from_secs(20));
        end(ticker);
        let Some((Unfollowed::Stalled(stall), at)) = refused else {
            panic!("a ticking sleeper was not refused under its rate: {refused:?}");
        };
        let rate = stall
            .rate()
            .unwrap_or_else(|| panic!("refused for stillness, {at:?} in: {stall:?}"));
        assert!(rate.work > most, "{stall:?}");
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
    /// doing, or its own: held where its host holds it (a thread runnable on no CPU; on Linux,
    /// its threads' run-queue wait or its own cgroup's pressure grown), which is no stall;
    /// still where it waits on itself, and where it is stopped or frozen, whatever else shows.
    /// One whose end is under way progressed; one that ran progressed unless it is stopped,
    /// whose stop ran it (ROOT's ruling of 2026-11-13).
    #[test]
    fn a_process_its_host_holds_is_held_and_a_stopped_one_still() {
        let hold = |runnable, stopped| Hold {
            runnable,
            stopped,
            uninterruptible: false,
            disks: None,
            stalls: None,
            #[cfg(unix)]
            queued: Queued::Unread,
            #[cfg(target_os = "linux")]
            pressure: None,
            io: Io::default(),
            declared: None,
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

    /// A reading's CPU idle (D215, R-D): each CPU's idle+iowait ticks grown since the look
    /// before, and a window shorter than one tick said as such, as no count could show it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_readings_cpu_idle_is_said_per_cpu_and_under_a_tick_as_such() {
        let tick = Some(Duration::from_millis(10));
        assert_eq!(
            os::idle_since(&[5, 9], &[5, 12], Duration::from_millis(15), tick),
            "each CPU's idle+iowait ticks +0,+3 over 15ms"
        );
        assert_eq!(
            os::idle_since(&[5, 9], &[5, 9], Duration::from_millis(4), tick),
            "each CPU's idle+iowait under a tick (4ms)"
        );
    }

    /// A thread waiting uninterruptibly in its kernel is held where its host's devices worked
    /// since the look before, as before, and where its host's tasks waited for a CPU or stalled
    /// on memory meanwhile (D215, R-D): a journal's commit that waits for its own thread's CPU,
    /// or for the open handles of its transaction, shows no I/O on any device, as CI
    /// 44227612442's ten refused builds and commit found it, each with a thread in
    /// `jbd2_log_wait_commit`, `folio_wait_bit_common` or `__wait_on_buffer`. With none of
    /// those grown it is still, as a dead mount's wait is; and where its host keeps no pressure
    /// record, its devices alone decide, as before. Each look's reading says which decided.
    #[test]
    fn a_wait_in_its_kernel_is_held_by_its_hosts_cpu_and_memory_stalls() {
        let waiting = |disks, stalls: Option<(u64, u64)>| Hold {
            uninterruptible: true,
            disks: Some(disks),
            stalls: stalls.map(|(cpu, memory)| Stalls { cpu, memory }),
            ..Hold::default()
        };
        let before = waiting(7, Some((100, 50)));
        assert_eq!(
            found(false, false, &before, &waiting(7, Some((100, 50)))),
            Found::Still
        );
        assert_eq!(
            found(false, false, &before, &waiting(7, Some((101, 50)))),
            Found::Held,
            "CPU some alone"
        );
        assert_eq!(
            found(false, false, &before, &waiting(7, Some((100, 51)))),
            Found::Held,
            "memory some alone"
        );
        assert_eq!(
            found(false, false, &before, &waiting(8, Some((100, 50)))),
            Found::Held,
            "its devices alone"
        );
        // No pressure record: its devices alone decide.
        let unrecorded = waiting(7, None);
        assert_eq!(found(false, false, &unrecorded, &waiting(7, None)), Found::Still);
        assert_eq!(found(false, false, &unrecorded, &waiting(8, None)), Found::Held);
        assert_eq!(
            found(false, false, &unrecorded, &waiting(7, Some((900, 900)))),
            Found::Still,
            "none read before"
        );
        // A sleeper's are no hold of its: only a thread in its kernel waits on its kernel.
        let sleeping = |cpu| Hold {
            stalls: Some(Stalls { cpu, memory: 0 }),
            ..Hold::default()
        };
        assert_eq!(found(false, false, &sleeping(1), &sleeping(2)), Found::Still);
        assert_eq!(
            waiting(8, Some((101, 51))).kernel_since(&before).as_deref(),
            Some(
                "in its kernel, since its look before: its host's devices' work +1, CPU some +1us, memory some +1us"
            )
        );
        assert_eq!(
            waiting(7, None).kernel_since(&unrecorded).as_deref(),
            Some(
                "in its kernel, since its look before: its host's devices' work +0, no pressure record (no CONFIG_PSI, or psi=0): its devices alone decide"
            )
        );
        assert_eq!(sleeping(2).kernel_since(&sleeping(1)), None);
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
        // Its work in its CPU's microseconds, as a count: the rule weighs counts alone.
        let work = |us: u64| Work::of(us * 1000);
        let record = Record::new();
        // W* and R* from the slowest answer; C* from a quicker one, at a rate above R*.
        record.teach(us(55_119), work(11_874));
        record.teach(us(50_000), work(12_580));
        let judged = |hold: &Hold, stored: bool| {
            let mut judge = Judge::begin(&record);
            let began = Instant::now();
            // Its work by each look a W* apart: its hashing at first, then a flush's moments.
            [work(9_000), work(12_100), work(13_228), work(13_300)]
                .into_iter()
                .zip(1u32..)
                .try_for_each(|(done, n)| {
                    let at = began + us(56_000) * n;
                    let work = vouches(Found::Progressed, false, hold, stored, false).then_some(done);
                    let span = Span {
                        began: at,
                        ended: at,
                        resolution: Duration::ZERO,
                        own: Duration::ZERO,
                        work,
                        held: Duration::ZERO,
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

    /// A look that finds a party that ran nothing with a thread waiting in its kernel finds it
    /// held only where its host's block devices completed I/O since the look before (D215,
    /// 2026-11-16): a transaction commit another task runs, which its waiters share with no
    /// I/O of their own (two daemons' stops refused at once on adacachy2's btrfs, each in `D`
    /// on one commit, Linux's look reading `D` as still), or a page-in its process's counts do
    /// not show (Windows' paging waits, 2026-11-17, which had held it whatever the disks did).
    /// Still where no device completed any (a dead NFS mount's waiter), where the look before
    /// read no count, where the count fell (a device gone), and where no thread waits in its
    /// kernel, whatever the disks did. Its rate's windows are its host's either way. One rule on
    /// every OS.
    #[test]
    fn a_party_in_its_kernel_is_held_only_while_its_hosts_disks_complete_io() {
        let hold = |uninterruptible, disks| Hold {
            uninterruptible,
            disks,
            ..Hold::default()
        };
        let (from, busy) = (hold(true, Some(10)), hold(true, Some(11)));
        assert_eq!(found(false, false, &from, &busy), Found::Held);
        assert_eq!(
            found(false, false, &from, &hold(true, Some(10))),
            Found::Still,
            "no device completed any I/O"
        );
        assert_eq!(
            found(false, false, &hold(true, None), &busy),
            Found::Still,
            "none read before"
        );
        assert_eq!(
            found(false, false, &busy, &from),
            Found::Still,
            "a count that fell"
        );
        assert_eq!(
            found(false, false, &hold(false, Some(10)), &hold(false, Some(11))),
            Found::Still,
            "no thread in its kernel"
        );
        assert_eq!(found(true, false, &from, &from), Found::Progressed);
        assert!(!vouches(Found::Still, false, &hold(true, Some(10)), false, false));
        assert!(!vouches(Found::Still, false, &hold(true, None), false, false));
    }

    /// `/proc/diskstats`' completed reads, writes, discards and flushes, and each device's
    /// milliseconds spent doing I/O, are summed over its lines, as many of those fields as each
    /// line's kernel lists (iostats.rst: 11 fields before 4.18, 15 since, 17 since 5.5), its
    /// other fields left out.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hosts_disk_work_is_summed_from_every_diskstats_line() {
        let text = concat!(
            "   8       0 sda 10 1 2 3 20 1 2 3 0 4 5 30 1 2 3 40 1\n",
            " 259       1 nvme0n1p1 100 7 7 7 200 7 7 7 7 7 7 300 7 7 7\n",
            "   7       0 loop0 1000 9 9 9 2000 9 9 9 9 9 9\n",
        );
        assert_eq!(os::disks_worked(text), 3720);
        assert_eq!(os::disks_worked(""), 0);
    }

    /// A flush in flight through a whole look's window completes nothing in it, while its device
    /// spends that time doing I/O (iostats.rst's field 10, its field 9 nonzero): a party waiting
    /// on it in its kernel is held by its host's disk, not still (CI 44205094460, x86_64 gnu: a
    /// client in `jbd2_log_wait_commit`, refused as still 46.7 ms in).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_flush_in_flight_through_a_look_holds_its_party() {
        let before = "   8       0 sda 10 0 0 0 20 0 0 0 1 40 40 0 0 0 0 5 0\n";
        let after = "   8       0 sda 10 0 0 0 20 0 0 0 1 55 55 0 0 0 0 5 0\n";
        let (from, now) = (os::disks_worked(before), os::disks_worked(after));
        assert!(now > from, "{from} {now}");
        let hold = |disks| Hold {
            uninterruptible: true,
            disks: Some(disks),
            ..Hold::default()
        };
        assert_eq!(found(false, false, &hold(from), &hold(now)), Found::Held);
    }

    /// A process whose thread writes a file and flushes it to its disk again and again
    /// (macOS F_FULLFSYNC; Linux fdatasync(2) on a disk, never a tmpfs, whose flush waits on
    /// nothing) is seen waiting uninterruptibly in its kernel (`TH_STATE_UNINTERRUPTIBLE`,
    /// `D`), its look then vouching for no window of its rate, though it ran; and its
    /// storage's counts grow between two looks ([`Io`]: PM M600, M601). A look that finds it
    /// in its kernel reads its host's disks, and a later one finds them grown, its flushes
    /// being their writes: held, had it run nothing between (D215, 2026-11-16). Each looked
    /// at until seen so, a wait for that fact, with no clock.
    #[cfg(unix)]
    #[test]
    fn a_process_flushing_to_its_disk_is_seen_waiting_in_its_kernel() {
        let mut child = this_binary("flush");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        let hold = loop {
            let hold = followed.hold(&Listing::new());
            if hold.uninterruptible {
                break hold;
            }
            std::thread::yield_now();
        };
        assert!(hold.disks.is_some(), "its host's disks unread: {hold:?}");
        assert!(!vouches(Found::Progressed, false, &hold, false, false));
        while !followed.hold(&Listing::new()).io.grown_since(&hold.io) {
            std::thread::yield_now();
        }
        loop {
            let later = followed.hold(&Listing::new());
            if later.uninterruptible && matches!((hold.disks, later.disks), (Some(b), Some(n)) if n > b) {
                assert_eq!(found(false, false, &hold, &later), Found::Held, "{later:?}");
                break;
            }
            std::thread::yield_now();
        }
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    /// On Windows, a process whose thread writes a page and flushes it to its disk again and
    /// again (FlushFileBuffers) shows its storage's work between two looks ([`Io`]), its one
    /// sign there: its wait reason, `Executive`, is a pipe reader's too (PM M606). Looked at
    /// until seen so, a wait for that fact, with no clock.
    #[cfg(windows)]
    #[test]
    fn a_process_flushing_to_its_disk_shows_its_storages_work() {
        let mut child = this_binary("flush");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        let first = loop {
            if let Some(done) = followed.hold(&Listing::new()).io.done {
                break done;
            }
            std::thread::yield_now();
        };
        let grown = loop {
            let hold = followed.hold(&Listing::new());
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

    /// On Windows its host's physical disks are read (IOCTL_DISK_PERFORMANCE on each
    /// `PhysicalDriveN`), and their bytes grow while a process writes a page and flushes it to
    /// its disk again and again (FlushFileBuffers): a look that finds a thread of a party that
    /// ran nothing waiting on its host's paging, its disks so grown since the look before, finds
    /// it held. Looked at until seen so, a wait for that fact, with no clock.
    #[cfg(windows)]
    #[test]
    fn a_hosts_disks_are_read_and_grow_with_a_flush() {
        let mut child = this_binary("flush");
        let first = os::disks().expect("its host's disks unread");
        let grown = loop {
            if let Some(now) = os::disks().filter(|&now| now > first) {
                break now;
            }
            std::thread::yield_now();
        };
        let paging = |disks| Hold {
            uninterruptible: true,
            disks: Some(disks),
            ..Hold::default()
        };
        assert_eq!(found(false, false, &paging(first), &paging(grown)), Found::Held);
        assert_eq!(found(false, false, &paging(grown), &paging(grown)), Found::Still);
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
        record.teach(Duration::from_secs(1), Work::of_cpu(Duration::from_millis(100)));
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
            let hold = followed.hold(&Listing::new());
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
        assert!(
            followed.hold(&Listing::new()).stopped,
            "{:?}",
            followed.hold(&Listing::new())
        );
        let slowest = Duration::from_millis(20);
        let record = Record::new();
        record.teach(slowest, SOME_WORK);
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
        let mut watch = Watch::of(Followed::of_pid(pid).unwrap().unwrap(), &Listing::new())
            .unwrap()
            .unwrap();
        // Asleep: looked at until a look finds it did nothing since the look before.
        while watch.look(&Listing::new()).unwrap() != Found::Still {
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
        let found = watch.look(&Listing::new()).unwrap();
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
        let waited = |followed: &Followed| match followed.hold(&Listing::new()).queued {
            Queued::Waited(ticks) => ticks,
            Queued::Unread => panic!("unread: {:?}", followed.hold(&Listing::new())),
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
        while followed.hold(&Listing::new()).runnable {
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
        while !followed.hold(&Listing::new()).runnable {
            std::thread::yield_now();
        }
        assert!(!followed.hold(&Listing::new()).stopped);
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
    /// Each window is the spinners' start and their wait on each other for a tick
    /// ([`waited_a_tick`]); they end past it. The pressure the sleeper's cgroup shares is read
    /// at its two ends alone, for the failure's words: each read of a pressure record moves
    /// it, and one under a jiffy after the read before adds nothing to it, so a wait that
    /// read it until it grew stood still for ever once no other thread took its CPU (CI
    /// 44282442232, x86_64 musl: 64 minutes; D215, R-D).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sleeper_is_still_whatever_its_cgroups_others_wait_for() {
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut followed = Followed::of_pid(sleeper.id()).unwrap().unwrap();
        let cpu = first_cpu();
        let (before, now, shared) = loop {
            let ran = followed.look().unwrap().unwrap();
            let before = followed.hold(&Listing::new());
            let from = shared_cpu_pressure(sleeper.id());
            let mut spinners = [spinner_on(cpu), spinner_on(cpu)];
            waited_a_tick(&mut spinners);
            let now = followed.hold(&Listing::new());
            let shared = (from, shared_cpu_pressure(sleeper.id()));
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
            "{before:?} {now:?} (shared pressure from {:?} to {:?})",
            shared.0,
            shared.1
        );
    }

    /// Waits until `spinners`, pinned to one CPU, have waited on each other for a tick: one's
    /// own wait for its CPU ([`run_queue_wait`]), which a read does not move, grown by a tick
    /// since this began. The tick is the kernel's own (clock_getres(2) of
    /// CLOCK_MONOTONIC_COARSE), the unit in which PSI weights each CPU's stall time (v6.18
    /// kernel/sched/psi.c `collect_percpu_times`), so that the pressure their wait makes is in
    /// a record read at either end. A spinner that ends fails the wait, as no wait of theirs is
    /// to come; so does a wait the kernel keeps no count of ([`run_queue_wait`]).
    #[cfg(target_os = "linux")]
    fn waited_a_tick(spinners: &mut [std::process::Child]) {
        let tick = os::tick().unwrap();
        let pids: Vec<u32> = spinners.iter().map(std::process::Child::id).collect();
        let waited = |pid: u32| run_queue_wait(pid).unwrap_or_else(|why| panic!("{why}"));
        let from: Vec<Duration> = pids.iter().map(|&pid| waited(pid)).collect();
        loop {
            for spinner in spinners.iter_mut() {
                if let Some(status) = spinner.try_wait().unwrap() {
                    panic!("a spinner ended before it had waited a tick: {status}");
                }
            }
            if pids
                .iter()
                .zip(&from)
                .any(|(&pid, &from)| waited(pid).saturating_sub(from) >= tick)
            {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Process `pid`'s tasks' wait for a CPU: each task's schedstat `run_delay`, its second
    /// field (Documentation/scheduler/sched-stats.rst), summed, whatever it is: a task that has
    /// not waited for a CPU, or not run at all ("0 0 0"), reads 0, a reading like any other.
    /// Read alone, as a hold also reads its host's pressure where a task of it waits in its
    /// kernel (R-D), and each read moves that record. A task gone since its process's tasks
    /// were listed has none to read. A failure where its tasks cannot be listed, where none is
    /// left, where a task still there has no schedstat (its kernel keeps none: it keeps
    /// `/proc/<pid>/task/<tid>/schedstat` only with CONFIG_SCHED_INFO, which CONFIG_SCHEDSTATS,
    /// lib/Kconfig.debug, or CONFIG_TASK_DELAY_ACCT, init/Kconfig, selects; fs/proc/base.c),
    /// or where one cannot be read or parsed.
    #[cfg(target_os = "linux")]
    fn run_queue_wait(pid: u32) -> Result<Duration, String> {
        let listed = |e: std::io::Error| format!("process {pid}'s tasks, /proc/{pid}/task: {e}");
        let mut waited: Option<u64> = None;
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).map_err(listed)? {
            let dir = task.map_err(listed)?.path();
            let file = dir.join("schedstat");
            let text = match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) if !dir.exists() => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "{} absent: its kernel keeps no schedstat, as without CONFIG_SCHED_INFO \
                         (selected by CONFIG_SCHEDSTATS or CONFIG_TASK_DELAY_ACCT)",
                        file.display()
                    ));
                }
                Err(e) => return Err(format!("{}: {e}", file.display())),
            };
            let delay = text
                .split_whitespace()
                .nth(1)
                .and_then(|field| field.parse::<u64>().ok())
                .ok_or_else(|| format!("{}: no run_delay, its second field, in {text:?}", file.display()))?;
            waited = Some(waited.map_or(delay, |sum| sum.saturating_add(delay)));
        }
        waited
            .map(Duration::from_nanos)
            .ok_or_else(|| format!("process {pid} has no task left: it has ended"))
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
        let before = followed.hold(&Listing::new());
        // Its own wait grown where the kernel keeps it, else its runnable sample.
        let now = loop {
            let now = followed.hold(&Listing::new());
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

    /// A look's work is what its OS counts exactly: on Windows its cycles, whatever its CPU
    /// times, which Windows charges a whole clock tick at a time to the thread running as the
    /// tick comes (15.625 ms steps, PM M294); elsewhere its CPU time to the nanosecond. So a
    /// window no tick was charged in shows the cycles run in it, and one charged a whole tick
    /// shows only the cycles run in it, never a tick's time (D215; CI 44347282760: R* and C* of
    /// two and eleven ticks refused a party of thirteen; CI 44076016255: a stand-in busy 50 ms
    /// read as none). Red with Windows' work read from its ticks.
    #[test]
    fn a_looks_work_is_its_cycles_on_windows_and_its_cpu_time_elsewhere() {
        let tick = Duration::from_nanos(15_625_000);
        #[cfg(windows)]
        {
            let look = |cpu, cycles| Look {
                cpu,
                cycles,
                ending: false,
            };
            let before = look(tick * 2, 1_000);
            let unticked = look(tick * 2, 51_000_000);
            let ticked = look(tick * 3, 1_200);
            assert_eq!(
                unticked.work().saturating_sub(before.work()),
                Work::of(50_999_000)
            );
            assert_eq!(ticked.work().saturating_sub(before.work()), Work::of(200));
        }
        #[cfg(not(windows))]
        {
            let look = |cpu| Look {
                cpu,
                ending: false,
                exited: false,
            };
            let (before, now) = (look(tick * 2), look(tick * 2 + Duration::from_nanos(1_234)));
            assert_eq!(now.work().saturating_sub(before.work()), Work::of(1_234));
        }
    }

    /// Each kind learns apart, from its first answer on: its slowest answer only rises and its
    /// quickest only falls, its least rate only falls, and a wait no time long teaches nothing
    /// (D215).
    #[test]
    fn a_kind_learns_its_slowest_answer_and_its_least_rate_apart() {
        let ms = Duration::from_millis;
        let records = Records::new();
        assert_eq!(records.seen(Awaited::Boot), Seen::Cold);
        records.teach(Awaited::Boot, ms(2), SOME_WORK);
        let learned = |slowest, quickest, over, answers| Seen::Learned {
            slowest,
            quickest,
            rated: Some(crate::Rated {
                least: Rate {
                    work: SOME_WORK,
                    over,
                },
                most: SOME_WORK,
                lightest: SOME_WORK,
                answers,
            }),
        };
        assert_eq!(records.seen(Awaited::Boot), learned(ms(2), ms(2), ms(2), 1));
        records.teach(Awaited::Boot, ms(1), SOME_WORK);
        assert_eq!(records.seen(Awaited::Boot), learned(ms(2), ms(1), ms(2), 2));
        records.teach(Awaited::Boot, ms(4), SOME_WORK);
        assert_eq!(records.seen(Awaited::Boot), learned(ms(4), ms(1), ms(4), 3));
        records.teach(Awaited::Boot, Duration::ZERO, Work::ZERO);
        assert_eq!(records.seen(Awaited::Boot), learned(ms(4), ms(1), ms(4), 3));
        assert_eq!(records.seen(Awaited::Connect), Seen::Cold);
        assert_eq!(records.seen(Awaited::End), Seen::Cold);
    }

    /// Set in the children of `the_cpu_reader_sees_a_spinning_childs_cpu_grow`: `spin`
    /// spins until its stdin ends; `parent` (Windows) starts a spinner and waits on its own
    /// stdin, the spinner its grandchild, so the walk of descendants is what reads it; `idle`
    /// waits on its stdin alone; `burst` spins 100 ms once told to.
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
            // does (PM M600, M601), by `shards_durable::data`, declaring nothing (this process
            // installs no slot file). macOS: a page, then F_FULLFSYNC (fcntl(2)), as `record`.
            // Linux: a MiB, then fdatasync(2), to a file beside this test binary, on the disk
            // cargo built it on: $TMPDIR may be a tmpfs (adacachy2's is), whose flush waits on
            // nothing; a MiB keeps the device writing at each flush (on adacachy2's NVMe, 100
            // flushes of a MiB waited in `D` 117 times for 1 ms or more, PM M601). Windows: a
            // page, then FlushFileBuffers, as `record` there (PM M606).
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
                        shards_durable::data(&file).unwrap();
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
            // C*'s burst for D215's rate bound: says it has started; given its go on its stdin,
            // spins 100 ms and says it spun; then waits for its stdin's end.
            Ok("burst") => {
                use std::io::Write as _;
                let _ = writeln!(std::io::stdout(), "started");
                let _ = std::io::stdout().flush();
                let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                let until = Instant::now() + Duration::from_millis(100);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
                let _ = writeln!(std::io::stdout(), "spun");
                let _ = std::io::stdout().flush();
                let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
                return;
            }
            // Asleep on its stdin until it ends, running nothing meanwhile.
            Ok("idle") => {
                let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0u8; 1]);
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

    /// A look at several processes reads Windows' listing of every thread once, whatever their
    /// number and however many of its readings need it: each one's hold and threads, its trace
    /// (SHARDS_JUDGE_TRACE) where it ran nothing, and a refusal's account of its threads (D215;
    /// CI 44347282760, x86_64 Windows: a tests' party of four read it 16 to 20 times a look,
    /// each read 13.1 to 13.8 ms of CPU on jupiter, PM M606). Do: watch four idle children of
    /// this test binary from one listing, each traced; then, from one listing a round, look at
    /// each, read its threads, its account and its runnable threads, round after round until a
    /// round finds none of them ran, each of its looks traced. Expect: one read of the listing
    /// as the four watches begin, and one in each round. Red where a reading reads the listing
    /// itself (5 a round and 4 as they begin, with the hold's own: the reads before).
    #[cfg(windows)]
    #[test]
    fn a_look_at_several_processes_reads_the_systems_listing_once() {
        let reads = shards_judge::threads::reads;
        let mut children: Vec<std::process::Child> = (0..4).map(|_| this_binary("idle")).collect();
        let listing = Listing::new();
        let before = reads();
        let mut watches: Vec<(u32, Watch)> = children
            .iter()
            .map(|child| {
                let followed = Followed::of_pid_alone(child.id()).unwrap().unwrap();
                let mut watch = Watch::of(followed, &listing).unwrap().unwrap();
                watch.trace = true;
                (child.id(), watch)
            })
            .collect();
        assert_eq!(reads() - before, 1, "as four watches began");
        let mut round = 0u32;
        loop {
            round += 1;
            let listing = Listing::new();
            let before = reads();
            let mut ran = false;
            for (pid, watch) in &mut watches {
                ran |= watch.look(&listing).unwrap() == Found::Progressed;
                watch.starved(&listing).unwrap();
                assert!(account_of(*pid, watch, &listing).is_some(), "{pid}");
                assert!(listing.runnable_threads(*pid).is_some(), "{pid}");
            }
            assert_eq!(reads() - before, 1, "round {round}");
            if !ran {
                break;
            }
        }
        for child in &mut children {
            drop(child.stdin.take());
            child.wait().unwrap();
        }
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
        let mut followed = Followed {
            pid,
            started,
            declarations: Declarations::installed(),
        };
        let look = followed.look().unwrap().unwrap();
        assert!(look.ending, "a zombie's look says its end is under way");
        assert!(look.exited, "a zombie's look says its exit is done");
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

    /// A process that has exited and is not yet reaped is progress at the look that first finds
    /// its end under way, and gone at the next: one its parent never reaps is followed no
    /// longer than that (D217: a killed VM process the daemon's unit test never reaped was
    /// followed each slice for 5 h 37 min on macOS; #77 waits-09).
    #[cfg(unix)]
    #[test]
    fn an_unreaped_end_is_progress_once_then_gone() {
        let mut child = this_binary("spin");
        let followed = Followed::of_pid(child.id()).unwrap().unwrap();
        let mut watch = Watch::of(followed, &Listing::new()).unwrap().unwrap();
        drop(child.stdin.take());
        // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) of this test's own child into `info`; WNOWAIT leaves it unreaped.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0);
        assert_eq!(
            watch.look(&Listing::new()).unwrap(),
            Found::Progressed,
            "its end under way, first found"
        );
        assert!(watch.ended() && watch.exited());
        assert_eq!(
            watch.look(&Listing::new()).unwrap(),
            Found::Gone,
            "found exited again: gone"
        );
        child.wait().unwrap();
    }

    /// An exit its kernel still does is its end under way at every look, never gone: macOS
    /// stashes a process's usage with its exit time as its exit begins (`proc_prepareexit`),
    /// and makes it a zombie only once `proc_exit` is done (xnu-12377.101.15
    /// bsd/kern/kern_exit.c:2207-2215, 2771), which drains a session leader's controlling
    /// terminal first, without limit while a writer of it is kept (kern_exit.c:2427-2476;
    /// tty.c:1763-1793, `ttywait`, its timeout the tty's, which only the replica's last close
    /// sets, tty_dev.c:325-350; PM M246's `leader kept`, whose exits waited for their reader,
    /// up to 2.005 s). Red so (its exit begun taken for its exit done, D215 at 94373c6ea3):
    /// gone at the look after the one that found its exit begun, waitid(2) finding no end, as
    /// ROOT's first harness runs after a build (r106, r116, r122) found stand-ins whose exits
    /// XNU still did gone at two looks and refused their waits. Do: a child leads a session
    /// whose controlling terminal is a pty's replica, writes a line there and exits, a writer
    /// of the replica kept and its controller unread; look at it until a look finds its exit
    /// begun, then twice more, the rule's two looks running; then read the line. Expect: its
    /// end under way at each look, its exit not done and waitid(2) finding no end; once the
    /// line is read, a zombie, its end progress once, its exit done, then gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_exit_its_kernel_still_does_is_its_end_under_way() {
        use std::io::Read as _;
        use std::os::fd::{FromRawFd as _, OwnedFd};
        use std::os::unix::process::CommandExt as _;
        let (mut controller, mut replica) = (0, 0);
        // SAFETY: openpty(3) fills both descriptors; it is given no name, settings or size.
        let opened = unsafe {
            libc::openpty(
                &raw mut controller,
                &raw mut replica,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        for fd in [controller, replica] {
            // SAFETY: fcntl(2) of a descriptor openpty just made, which the child gets only
            // as its stdout.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }
        // SAFETY: fresh descriptors nothing else owns.
        let (mut controller, kept) = unsafe {
            (
                std::fs::File::from_raw_fd(controller),
                OwnedFd::from_raw_fd(replica),
            )
        };
        let mut leader = std::process::Command::new("/bin/echo");
        leader
            .arg("its last line")
            .stdin(std::process::Stdio::null())
            .stdout(kept.try_clone().unwrap())
            .stderr(std::process::Stdio::null());
        // SAFETY: runs in the child between fork and exec, its stdout the replica already,
        // calling only setsid(2) and ioctl(2), which are async-signal-safe: it leads a session
        // whose controlling terminal is the replica.
        unsafe {
            leader.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(1, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = leader.spawn().unwrap();
        let pid = leader.id();
        let reapable = |wait: libc::c_int| {
            // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of this test's own child into `info`; WNOWAIT leaves it
            // unreaped.
            let waited = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT | wait,
                )
            };
            assert_eq!(waited, 0);
            info.si_pid != 0
        };
        let mut watch = Watch::of(Followed::of_pid(pid).unwrap().unwrap(), &Listing::new())
            .unwrap()
            .unwrap();
        while !watch.ended() {
            assert_ne!(
                watch.look(&Listing::new()).unwrap(),
                Found::Gone,
                "gone before its exit began"
            );
        }
        for look in ["the look after its exit was found begun", "the next"] {
            assert_eq!(
                watch.look(&Listing::new()).unwrap(),
                Found::Progressed,
                "{look}: its end under way"
            );
            assert!(
                !watch.exited(),
                "{look}: its exit done before its terminal was read"
            );
            assert!(
                !reapable(libc::WNOHANG),
                "{look}: reapable before its terminal was read"
            );
        }
        // Its line read whole at one read, which takes all its output queue holds up to the
        // read's size and then wakes its drain (bsd/kern/tty_dev.c:699-705, `ptcread`, its
        // line discipline's start `ttwwakeup`: tty_conf.c:101-102, tty.c:2909-2913): its exit
        // goes on to its terminal's revoke, then its zombie, which waitid(2) waits for.
        let mut line = [0u8; 64];
        let read = controller.read(&mut line).unwrap();
        assert!(
            line[..read].ends_with(b"\n"),
            "{:?}",
            String::from_utf8_lossy(&line[..read])
        );
        assert!(reapable(0));
        assert_eq!(
            watch.look(&Listing::new()).unwrap(),
            Found::Progressed,
            "a zombie: its end, first found done"
        );
        assert!(watch.exited());
        assert_eq!(
            watch.look(&Listing::new()).unwrap(),
            Found::Gone,
            "found exited again: gone"
        );
        leader.wait().unwrap();
    }

    /// A watch reads its threads' wait for a CPU over the window its last look closed (D217, a
    /// joiner's provider): none before its second look; after it, on Linux and macOS, a reading,
    /// whose failure would be said, never taken for none; on Windows none, which counts no wait
    /// that has ended.
    #[test]
    fn a_watch_reads_its_threads_wait_for_a_cpu_over_its_last_window() {
        let followed = Followed::of_pid(std::process::id()).unwrap().unwrap();
        let mut watch = Watch::of(followed, &Listing::new()).unwrap().unwrap();
        assert_eq!(watch.waited(), Ok(None));
        watch.look(&Listing::new()).unwrap();
        #[cfg(unix)]
        assert!(matches!(watch.waited(), Ok(Some(_))), "{:?}", watch.waited());
        #[cfg(windows)]
        assert_eq!(watch.waited(), Ok(None));
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
