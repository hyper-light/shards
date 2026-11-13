//! The groups shards keeps for `--cgroup-parent` where its host has none a user's own tools
//! make and cap (D183, slice 3): macOS, and Windows. A group is a directory of the home,
//! `groups/NAME`, made with the first run under it, whose budgets are files in cgroup v2's
//! own formats, as a Linux user writes a cgroup's (Documentation/admin-guide/cgroup-v2.rst):
//! `memory.high` and `memory.max`, bytes or `max`, and `cpu.max`, a quota and its period.
//! The daemon holds the group's microVMs to them; on Windows each VM process of a group's
//! is made in the group's job too, whose memory limit is its `memory.max` (PM M359). Its
//! `cpu.max` every microVM of the group holds itself to, charging one count of the group's
//! (`shards_vmm::vm::group`), which the daemon keeps in [`CPU_BUDGET`] and writes the
//! budget to.

#[cfg(any(target_os = "macos", windows))]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", windows))]
use std::sync::{Condvar, Mutex, PoisonError};
#[cfg(any(target_os = "macos", windows))]
use std::time::Instant;

#[cfg(any(target_os = "macos", windows))]
use super::reclaim::Gave;
#[cfg(any(target_os = "macos", windows))]
use super::{Daemon, RunState, Threads, lock, log};
#[cfg(any(target_os = "macos", windows))]
use crate::containers::Disk;

/// The directory of the home groups are kept in.
pub(super) const DIR: &str = "groups";

/// The file of a group's directory its microVMs' CPU count is kept in, mapped by the daemon
/// and by each of them (`shards_vmm::vm::group::GroupCpu`): the daemon's, not its user's.
#[cfg(any(target_os = "macos", windows))]
const CPU_BUDGET: &str = ".cpu-budget";

/// `cpu.max`'s period where its file names none: the kernel's default
/// (kernel/sched/core.c, `default_cfs_period`, 100 ms).
const CPU_PERIOD: u64 = 100_000;

/// What a group holds its microVMs to: each `None` where its file says `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Budget {
    /// `memory.high`: its microVMs told their room under it, and give back to it.
    pub high: Option<u64>,
    /// `memory.max`: as `high`, and past it with nothing more given back, a microVM of it
    /// is stopped, as a cgroup's OOM killer kills within it.
    pub max: Option<u64>,
    /// `cpu.max`: µs of CPU time its microVMs may use together each period of µs.
    pub cpu: Option<(u64, u64)>,
}

impl Budget {
    /// The least of its memory budgets, `high` and `max`.
    pub fn memory(&self) -> Option<u64> {
        match (self.high, self.max) {
            (Some(h), Some(m)) => Some(h.min(m)),
            (h, m) => h.or(m),
        }
    }

    /// What its microVMs' room is of: on macOS the least of its memory budgets, as nothing
    /// but the daemon holds them to `memory.max` there; on Windows `memory.high` alone, as
    /// D183 has it, as the group's job holds them to `memory.max` itself, refusing what
    /// would pass it (PM M359), and a guest refused memory runs on what it has, its own OOM
    /// killer ending what passes it. Told a room under `memory.max` there too, a run past
    /// it was held at that room by its guest's `memory.high` (init's `hold_within`), which
    /// slows and ends nothing, while the daemon's stop past `memory.max` never came: its VM
    /// processes' private working sets, within what they commit, never pass the job's limit.
    /// `a_groups_job_holds_its_microvms_to_its_memory_max`'s run of 2 GiB under 1 GiB so
    /// ended 69 s after its workload began, past its test's 60 s (CI 42046987905).
    pub fn room_of(&self) -> Option<u64> {
        if cfg!(windows) { self.high } else { self.memory() }
    }
}

/// Whether `name` may name a group: a file name of letters, digits, `.`, `-` and `_`,
/// shorter than 256 bytes, not `.` or `..`, nothing that could name another directory.
pub(super) fn is_group_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 256
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Group `name`'s directory of `home`, made with its budget files, each `max`, where it is
/// not there yet; one there is as its user left it.
pub(super) fn made(home: &Path, name: &str) -> Result<PathBuf, String> {
    if !is_group_name(name) {
        return Err(format!(
            "--cgroup-parent {name}: a group is named by letters, digits, '.', '-' and '_' alone"
        ));
    }
    let dir = home.join(DIR).join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (file, unset) in [
        ("memory.high", "max\n"),
        ("memory.max", "max\n"),
        ("cpu.max", "max 100000\n"),
    ] {
        let path = dir.join(file);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(unset.as_bytes())
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(dir)
}

/// The budget group directory `dir`'s files say; a file not there is `max`.
pub(super) fn budget(dir: &Path) -> Result<Budget, String> {
    let read = |file: &str| -> Result<Option<String>, String> {
        let path = dir.join(file);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    };
    let memory = |file: &str| -> Result<Option<u64>, String> {
        match read(file)? {
            Some(text) => bytes(text.trim()).map_err(|e| format!("{}: {e}", dir.join(file).display())),
            None => Ok(None),
        }
    };
    let cpu = match read("cpu.max")? {
        Some(text) => cpu_max(&text).map_err(|e| format!("{}: {e}", dir.join("cpu.max").display()))?,
        None => None,
    };
    Ok(Budget {
        high: memory("memory.high")?,
        max: memory("memory.max")?,
        cpu,
    })
}

/// A CPU budget as the kernel reads one written to `cpu.max` (kernel/sched/core.c,
/// `cpu_period_quota_parse` and `tg_set_cfs_bandwidth`): `$MAX $PERIOD`, µs of CPU time
/// each period of µs, `$MAX` `max` for none and `$PERIOD` [`CPU_PERIOD`] where it is left
/// out; the period 1 ms to 1 s, a quota 1 ms or more and less than 2^44 µs.
fn cpu_max(text: &str) -> Result<Option<(u64, u64)>, String> {
    let bad = || format!("{:?}: not a quota and its period, or max", text.trim());
    let mut words = text.split_whitespace();
    let quota = words.next().ok_or_else(bad)?;
    let period = match words.next() {
        Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => p.parse::<u64>().map_err(|_| bad())?,
        Some(_) => return Err(bad()),
        None => CPU_PERIOD,
    };
    if !(1_000..=1_000_000).contains(&period) {
        return Err(format!("a period of {period} µs, where it is 1000 to 1000000"));
    }
    if quota == "max" {
        return Ok(None);
    }
    if !quota.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let quota = quota.parse::<u64>().map_err(|_| bad())?;
    if !(1_000..1 << 44).contains(&quota) {
        return Err(format!(
            "a quota of {quota} µs, where it is 1000 or more and less than 2^44"
        ));
    }
    Ok(Some((quota, period)))
}

/// A memory budget as the kernel reads one written to `memory.max` (mm/page_counter.c,
/// `page_counter_memparse`): `max`, or bytes, with a binary suffix of K, M, G, T, P or E
/// as lib/cmdline.c's `memparse` takes it.
fn bytes(text: &str) -> Result<Option<u64>, String> {
    if text == "max" {
        return Ok(None);
    }
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?}: not a number of bytes or max"))?;
    let shift = match suffix {
        "" => 0,
        "k" | "K" => 10,
        "m" | "M" => 20,
        "g" | "G" => 30,
        "t" | "T" => 40,
        "p" | "P" => 50,
        "e" | "E" => 60,
        _ => return Err(format!("{text:?}: not a number of bytes or max")),
    };
    n.checked_mul(1u64 << shift)
        .map(Some)
        .ok_or_else(|| format!("{text:?}: more bytes than a budget holds"))
}

/// A microVM of a group: its VM process, and the run its words go through.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone)]
struct Member {
    pid: u32,
    id: String,
}

/// What the groups' thread hears (D183, slice 3): runs begun, by VM process and microVM,
/// and ended, by VM process; each give-back of a round that ended; that a member's word
/// moved on, which may make a round due; and its test's end, where a test ends it (a
/// daemon exits from within, its threads unwaited for).
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
struct Woke {
    ran: Vec<Member>,
    left: Vec<u32>,
    gave: Vec<Answered>,
    spoke: bool,
    ended: bool,
}

#[cfg(any(target_os = "macos", windows))]
impl Woke {
    /// Whether it holds nothing to hear.
    fn quiet(&self) -> bool {
        self.ran.is_empty() && self.left.is_empty() && self.gave.is_empty() && !self.spoke && !self.ended
    }
}

/// A member's give-back of a round, ended: its VM process, the round, and what it gave; none
/// where it gave nothing back, its answer refused or cut, or its run gone.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone, Copy)]
struct Answered {
    pid: u32,
    round: u64,
    gave: Option<Gave>,
}

/// What a member is asked at a pass (R04): the room its group's budget leaves it, none for
/// no budget (init's `ROOM`); and, where its group is short, to give back for a round.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ask {
    room: Option<i64>,
    give: Option<u64>,
}

#[cfg(any(target_os = "macos", windows))]
impl Ask {
    /// This ask, made while `pending` waits behind a word under way: its room the newest,
    /// and a give-back of either kept, the newest round's first.
    fn over(self, pending: Option<Ask>) -> Ask {
        Ask {
            room: self.room,
            give: self.give.or(pending.and_then(|p| p.give)),
        }
    }
}

/// A member's word under way (R04), on a thread of its own: what it says now and since when,
/// which the groups' thread judges from outside by the record of its kind of answer
/// (`Record::stalled`), and what is asked of it once that has ended.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug)]
struct Word {
    saying: Saying,
    since: Instant,
    next: Option<Ask>,
}

/// What a member's word says: its room told (`ROOM`), or what it holds cold asked back for a
/// round (`RECLAIM`).
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Saying {
    Room,
    GiveBack(u64),
}

/// What the groups' thread keeps of a group between its passes (R04): whether it was said
/// short of its budget, its round of give-backs under way, and the members told a room, who
/// are told it has none once its budget has none.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
struct Hold {
    short: bool,
    round: Option<Round>,
    told: HashSet<u32>,
}

/// A group's round of give-backs (R04): opened at a pass that finds it short of its budget,
/// each of its members then asked once, by VM process; done once each has answered, ended,
/// or run past the slowest answer of its kind seen with none, and judged then.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug)]
struct Round {
    id: u64,
    of: HashMap<u32, Given>,
}

/// What a round knows of a member's give-back.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone, Copy)]
enum Given {
    /// Asked, its answer to come.
    Asked,
    /// Answered: what it gave back.
    Gave(Gave),
    /// Nothing: its answer refused or cut, its run ended, or its word past the slowest answer
    /// of its kind seen with none.
    Nothing,
}

#[cfg(any(target_os = "macos", windows))]
impl Round {
    /// What its members gave back, in all.
    fn gave(&self) -> Gave {
        self.of.values().fold(Gave::default(), |all, g| match g {
            Given::Gave(gave) => all.and(*gave),
            Given::Asked | Given::Nothing => all,
        })
    }

    /// Whether every member it asked is done.
    fn done(&self) -> bool {
        !self.of.values().any(|g| matches!(g, Given::Asked))
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Hold {
    /// Member `pid`'s run ended: its group's round waits for nothing more of it, and it is
    /// told nothing more.
    fn left(&mut self, pid: u32) {
        self.told.remove(&pid);
        if let Some(given) = self.round.as_mut().and_then(|r| r.of.get_mut(&pid))
            && matches!(given, Given::Asked)
        {
            *given = Given::Nothing;
        }
    }

    /// A member's give-back ended: what it gave, where it was its round's.
    fn answered(&mut self, answered: Answered) {
        if let Some(given) = self
            .round
            .as_mut()
            .filter(|r| r.id == answered.round)
            .and_then(|r| r.of.get_mut(&answered.pid))
        {
            *given = answered.gave.map_or(Given::Nothing, Given::Gave);
        }
    }
}

/// What a group's microVMs hold in all: `held`'s footprints summed.
#[cfg(any(target_os = "macos", windows))]
fn sum(held: &[(usize, u64)]) -> u64 {
    held.iter().fold(0u64, |s, (_, b)| s.saturating_add(*b))
}

/// The groups' thread's word from the daemon, and what wakes it; each member's word under
/// way; each group's CPU budget, by its name, its file and the daemon's mapping of it; on
/// Windows each group's job too, with the `memory.max` it is held to.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
pub(super) struct Groups {
    woke: Mutex<Woke>,
    wake: Condvar,
    /// Each member's word under way, by its VM process: one at a time, so that none is told
    /// over itself, and none waits on another's.
    words: Mutex<HashMap<u32, Word>>,
    cpus: Mutex<HashMap<String, (std::fs::File, shards_vmm::vm::group::GroupCpu)>>,
    #[cfg(windows)]
    jobs: Mutex<HashMap<String, (shards_ipc::GroupJob, Option<u64>)>>,
    /// The footprints a test plays for its microVMs' VM processes, by process, in place of
    /// what their kernels count.
    #[cfg(test)]
    played: Mutex<HashMap<u32, u64>>,
}

#[cfg(all(test, any(target_os = "macos", windows)))]
impl Groups {
    /// VM process `pid` holds `footprint` bytes from now, as its group sums it.
    pub(super) fn play(&self, pid: u32, footprint: u64) {
        lock(&self.played).insert(pid, footprint);
    }

    /// Its test has ended: the groups' thread returns.
    pub(super) fn end(&self) {
        lock(&self.woke).ended = true;
        self.wake.notify_all();
    }
}

/// What a group's microVMs hold themselves to of its `cpu.max` (`GroupCpu::set_budget`):
/// none where it lets them use every CPU of the host's, which they cannot pass, and none
/// where a quota is past what the count's word holds, which takes a host of more than
/// 4,295 CPUs.
#[cfg(any(target_os = "macos", windows))]
fn held_cpu(cpu: Option<(u64, u64)>) -> Option<(u32, u32)> {
    let (quota, period) = cpu?;
    let host = u64::try_from(crate::resources::host_cpus()).unwrap_or(u64::MAX);
    if quota >= period.saturating_mul(host) {
        return None;
    }
    Some((u32::try_from(quota).ok()?, u32::try_from(period).ok()?))
}

#[cfg(any(target_os = "macos", windows))]
impl<D: Disk> Daemon<D> {
    /// Group `name`'s CPU budget (D183, slice 3): the file its microVMs' count is kept in
    /// ([`CPU_BUDGET`]), which every one of them charges, made with the group's first run,
    /// its directory and budget files with it, its budget the group's `cpu.max` as the file
    /// says now; a handle of its own, for a run's VM process.
    pub(super) fn group_cpu(&self, name: &str) -> Result<std::fs::File, String> {
        let dir = made(&self.home, name)?;
        let at = |e: &dyn std::fmt::Display| format!("group {name}'s CPU budget: {e}");
        let mut cpus = lock(&self.groups.cpus);
        if let Some((file, _)) = cpus.get(name) {
            return file.try_clone().map_err(|e| at(&e));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(CPU_BUDGET))
            .map_err(|e| at(&e))?;
        file.set_len(shards_vmm::vm::group::LEN as u64)
            .map_err(|e| at(&e))?;
        let mapped = shards_vmm::vm::group::GroupCpu::map(file.try_clone().map_err(|e| at(&e))?)
            .map_err(|e| at(&e))?;
        mapped.set_budget(held_cpu(budget(&dir)?.cpu));
        let given = file.try_clone().map_err(|e| at(&e))?;
        cpus.insert(name.to_string(), (file, mapped));
        Ok(given)
    }

    /// Group `name`'s CPU budget written as its `cpu.max` says now, where its count is kept.
    fn budget_group_cpu(&self, name: &str, cpu: Option<(u64, u64)>) {
        if let Some((_, mapped)) = lock(&self.groups.cpus).get(name) {
            mapped.set_budget(held_cpu(cpu));
        }
    }
}

#[cfg(windows)]
impl<D: Disk> Daemon<D> {
    /// Group `name`'s job, which a VM process of the group's is made in (D183, slice 3, PM
    /// M359): made with the group's first, its directory and budget files with it, held to
    /// its `memory.max`, and kept as long as the daemon runs, as the group's directory
    /// stays; a handle of its own, for the VM process's making.
    pub(super) fn group_job(&self, name: &str) -> Result<shards_ipc::GroupJob, String> {
        let dir = made(&self.home, name)?;
        let at = |e: &dyn std::fmt::Display| format!("group {name}'s job: {e}");
        let mut jobs = lock(&self.groups.jobs);
        if let Some((job, _)) = jobs.get(name) {
            return job.try_clone().map_err(|e| at(&e));
        }
        let max = budget(&dir)?.max;
        let job = shards_ipc::GroupJob::new().map_err(|e| at(&e))?;
        job.limit_memory(max).map_err(|e| at(&e))?;
        let given = job.try_clone().map_err(|e| at(&e))?;
        jobs.insert(name.to_string(), (job, max));
        Ok(given)
    }

    /// Group `name`'s job held to `max` from now, where it is held to another.
    fn limit_group_job(&self, name: &str, max: Option<u64>) {
        let mut jobs = lock(&self.groups.jobs);
        let Some((job, held)) = jobs.get_mut(name) else {
            return;
        };
        if *held == max {
            return;
        }
        match job.limit_memory(max) {
            Ok(()) => *held = max,
            Err(e) => log(format!("group {name}: its job's memory limit: {e}")),
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
impl<D: Disk> Daemon<D> {
    /// Starts the thread that holds the groups shards keeps to their budgets ([`Groups`]),
    /// as long as the daemon runs: it exits from within, its threads unwaited for.
    pub(super) fn start_groups<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        if let Err(e) = std::thread::Builder::new()
            .name("groups".into())
            .spawn_scoped(threads, move || self.hold_groups(threads))
        {
            log(format!(
                "the groups' thread: {e}; no group's budget holds its microVMs"
            ));
        }
    }

    /// Run `id`, of VM process `pid`, began: held to its group's budget, where it has one.
    pub(super) fn group_ran(&self, pid: u32, id: &str) {
        lock(&self.groups.woke).ran.push(Member {
            pid,
            id: id.to_string(),
        });
        self.groups.wake.notify_one();
    }

    /// VM process `pid`'s run ended: held to its group no more.
    pub(super) fn group_left(&self, pid: u32) {
        lock(&self.groups.woke).left.push(pid);
        self.groups.wake.notify_one();
    }

    /// The groups' thread: each group's microVMs held to its budget as runs begin and end,
    /// and every pressure window while a group has any, its files read again then, so that
    /// a budget written is heard within one. It asks its members and hears their answers,
    /// and waits on none of them, each member's words its own ([`speak`](Self::speak)):
    /// between its passes it wakes as one answers, and as a round's member runs past the
    /// slowest answer of its kind seen with none.
    fn hold_groups<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let mut members: HashMap<String, Vec<Member>> = HashMap::new();
        let mut holds: HashMap<String, Hold> = HashMap::new();
        let mut rounds = 0u64;
        let mut passed: Option<Instant> = None;
        loop {
            let look = self.next_look(&members, &holds, passed);
            let woke = {
                let mut woke = lock(&self.groups.woke);
                if woke.quiet() {
                    woke = match look {
                        None => self
                            .groups
                            .wake
                            .wait(woke)
                            .unwrap_or_else(PoisonError::into_inner),
                        Some(at) => {
                            self.groups
                                .wake
                                .wait_timeout(woke, at.saturating_duration_since(Instant::now()))
                                .unwrap_or_else(PoisonError::into_inner)
                                .0
                        }
                    };
                }
                std::mem::take(&mut *woke)
            };
            if woke.ended {
                return;
            }
            let moved = !woke.ran.is_empty() || !woke.left.is_empty();
            for m in woke.ran {
                if let Some(group) = self.parent_of(&m.id).filter(|p| !p.is_empty()) {
                    members.entry(group).or_default().push(m);
                }
            }
            for pid in woke.left {
                for list in members.values_mut() {
                    list.retain(|m| m.pid != pid);
                }
                for hold in holds.values_mut() {
                    hold.left(pid);
                }
            }
            for answered in woke.gave {
                for hold in holds.values_mut() {
                    hold.answered(answered);
                }
            }
            members.retain(|_, list| !list.is_empty());
            holds.retain(|name, _| members.contains_key(name));
            let pass = moved || passed.is_none_or(|p| p.elapsed() >= shards_vmm::platform::PRESSURE_WINDOW);
            if pass {
                passed = Some(Instant::now());
            }
            for (name, list) in &members {
                let hold = holds.entry(name.clone()).or_default();
                if pass {
                    self.hold_group(threads, name, list, hold, &mut rounds);
                }
                self.judge_round(name, list, hold);
            }
        }
    }

    /// When the groups' thread looks next, while a group has members: its next pass, or the
    /// first moment before it that a round's member runs past the slowest answer of its kind
    /// seen with none (R05: a deadline of what this daemon has learned, never a probe).
    fn next_look(
        &self,
        members: &HashMap<String, Vec<Member>>,
        holds: &HashMap<String, Hold>,
        passed: Option<Instant>,
    ) -> Option<Instant> {
        if members.is_empty() {
            return None;
        }
        let pass = passed
            .and_then(|p| p.checked_add(shards_vmm::platform::PRESSURE_WINDOW))
            .unwrap_or_else(Instant::now);
        let words = lock(&self.groups.words);
        let due = holds
            .values()
            .filter_map(|h| h.round.as_ref())
            .flat_map(|r| r.of.iter())
            .filter(|(_, given)| matches!(given, Given::Asked))
            .filter_map(|(pid, _)| {
                let word = words.get(pid)?;
                let shards_api::Seen::Learned { slowest, .. } = self.record_of(word.saying).seen() else {
                    return None;
                };
                word.since.checked_add(slowest)
            })
            .min();
        Some(due.map_or(pass, |due| due.min(pass)))
    }

    /// A pass over group `name` and its microVMs `members` (D183, slice 3): its files read
    /// again, what its microVMs hold, their VM processes' footprints, written to its
    /// `memory.current` and its processes to its `cgroup.procs`; each microVM asked, on a
    /// thread of its own, its room under its budget told (init's `ROOM`, D188), or that it
    /// has none once its budget has none; and where it is short with no round of give-backs
    /// under way, one begun, each asked to give back what it holds cold. Said once as it
    /// falls short of its budget, and once as it is within it again.
    fn hold_group<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        name: &str,
        members: &[Member],
        hold: &mut Hold,
        rounds: &mut u64,
    ) {
        let dir = self.home.join(DIR).join(name);
        let budget = match budget(&dir) {
            Ok(b) => b,
            Err(e) => {
                log(format!("group {name}: {e}"));
                return;
            }
        };
        // Its microVMs' CPU budget its `cpu.max`, as the file now says it, and on Windows its
        // job's memory limit its `memory.max`.
        self.budget_group_cpu(name, budget.cpu);
        #[cfg(windows)]
        self.limit_group_job(name, budget.max);
        let current = sum(&self.footprints(members));
        let _ = std::fs::write(dir.join("memory.current"), format!("{current}\n"));
        // Its processes, a line each, as a cgroup's `cgroup.procs` lists its own: on Windows
        // its job's, every process of its microVMs; on macOS, which has no group of
        // processes, its microVMs' VM processes, whose footprints it holds.
        #[cfg(windows)]
        let procs = lock(&self.groups.jobs)
            .get(name)
            .map(|(job, _)| job.processes())
            .transpose()
            .unwrap_or_else(|e| {
                log(format!("group {name}: its job's processes: {e}"));
                None
            });
        #[cfg(target_os = "macos")]
        let procs = Some(members.iter().map(|m| m.pid).collect::<Vec<_>>());
        if let Some(procs) = procs {
            let listed: String = procs.iter().map(|p| format!("{p}\n")).collect();
            let _ = std::fs::write(dir.join("cgroup.procs"), listed);
        }
        let Some(limit) = budget.room_of() else {
            // Its budget gone: each told a room is told it has none, its hold let go of.
            for m in members {
                if hold.told.remove(&m.pid) {
                    self.ask(
                        threads,
                        m,
                        Ask {
                            room: None,
                            give: None,
                        },
                    );
                }
            }
            return;
        };
        let signed = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
        let room = signed(limit).saturating_sub(signed(current));
        let mut give = None;
        if room < 0 && hold.round.is_none() {
            *rounds = rounds.saturating_add(1);
            give = Some(*rounds);
            hold.round = Some(Round {
                id: *rounds,
                of: members.iter().map(|m| (m.pid, Given::Asked)).collect(),
            });
        }
        for m in members {
            hold.told.insert(m.pid);
            self.ask(
                threads,
                m,
                Ask {
                    room: Some(room),
                    give,
                },
            );
        }
        if room >= 0 {
            if std::mem::take(&mut hold.short) {
                log(format!(
                    "group {name}: its microVMs hold {} MiB, within its budget of {} MiB again",
                    current >> 20,
                    limit >> 20
                ));
            }
        } else if !std::mem::replace(&mut hold.short, true) {
            log(format!(
                "group {name}: its microVMs hold {} MiB, {} MiB past its budget of {} MiB: each told so, and asked to give back what it holds cold",
                current >> 20,
                room.unsigned_abs() >> 20,
                limit >> 20
            ));
        }
    }

    /// Judges group `name`'s round of give-backs once it is done (D183, slice 3): a member
    /// whose word has run past the slowest answer of its kind seen with none gave nothing;
    /// and past its `memory.max`, with nothing given back, the one of its microVMs that
    /// holds the most is stopped, its record marked OOM-killed, as a cgroup's OOM killer
    /// takes its largest (mm/oom_kill.c, `oom_badness`).
    fn judge_round(&self, name: &str, members: &[Member], hold: &mut Hold) {
        let Some(round) = hold.round.as_mut() else {
            return;
        };
        let now = Instant::now();
        for (pid, given) in &mut round.of {
            if matches!(given, Given::Asked) && self.overdue(*pid, now) {
                *given = Given::Nothing;
            }
        }
        if !round.done() {
            return;
        }
        let gave = round.gave();
        hold.round = None;
        if gave.freed > 0 || gave.unplugging > 0 {
            log(format!(
                "group {name}: its microVMs gave back {} MiB they held cold of their files, and asked to unplug {} MiB",
                gave.freed >> 20,
                gave.unplugging >> 20
            ));
            return;
        }
        let Some(max) = budget(&self.home.join(DIR).join(name)).ok().and_then(|b| b.max) else {
            return;
        };
        let held = self.footprints(members);
        let after = sum(&held);
        if after <= max {
            return;
        }
        let Some(&(largest, bytes)) = held.iter().max_by_key(|(_, b)| *b) else {
            return;
        };
        let Some(m) = members.get(largest) else { return };
        let facts = format!(
            "its group {name} held {} MiB past its memory.max of {} MiB, {} MiB of it this microVM's, and gave nothing back",
            after.saturating_sub(max) >> 20,
            max >> 20,
            bytes >> 20
        );
        self.oom_killed(&m.id, facts.as_bytes());
        if !self.signal(&m.id, 9) {
            log(format!(
                "container {}: stopping it past its group's memory.max: it does not run",
                m.id
            ));
        }
    }

    /// Asks member `m` `ask` (R04): now, on a thread of its own, where it has no word under
    /// way; else once that has ended, the newest room told and a give-back of either kept.
    fn ask<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, m: &Member, ask: Ask) {
        {
            let mut words = lock(&self.groups.words);
            if let Some(word) = words.get_mut(&m.pid) {
                word.next = Some(ask.over(word.next));
                return;
            }
            words.insert(
                m.pid,
                Word {
                    saying: Saying::Room,
                    since: Instant::now(),
                    next: None,
                },
            );
        }
        let member = m.clone();
        let spawned = std::thread::Builder::new()
            .name("group-word".into())
            .spawn_scoped(threads, move || self.speak(&member, ask));
        if let Err(e) = spawned {
            lock(&self.groups.words).remove(&m.pid);
            if let Some(round) = ask.give {
                self.heard(Answered {
                    pid: m.pid,
                    round,
                    gave: None,
                });
            }
            log(format!("container {}: its group's word: its thread: {e}", m.id));
        }
    }

    /// Member `m`'s words, from `ask` on, until none is due (D183, slice 3): its room told,
    /// then, for a round, what it holds cold asked back, once a give-back of its host's
    /// pressure under way has ended, as a budget's waits it out (reclaim.rs `give_after`);
    /// each waited for while its VM process runs, as D215's judge judges a wait
    /// (`Wait::WhileRuns`). Its own: nothing else waits on it.
    fn speak(&self, m: &Member, mut ask: Ask) {
        loop {
            self.tell_group_room(&m.id, m.pid, ask.room);
            if let Some(round) = ask.give {
                self.saying(m.pid, Saying::GiveBack(round));
                let vm = self.own_run(&m.id);
                self.give_after(&vm);
                let gave = self.give_back_of(&m.id, m.pid);
                self.given_back(&vm);
                self.heard(Answered {
                    pid: m.pid,
                    round,
                    gave,
                });
            }
            let next = {
                let mut words = lock(&self.groups.words);
                let next = words.get_mut(&m.pid).and_then(|word| {
                    let next = word.next.take()?;
                    word.saying = Saying::Room;
                    word.since = Instant::now();
                    Some(next)
                });
                if next.is_none() {
                    words.remove(&m.pid);
                }
                next
            };
            self.spoke();
            match next {
                Some(next) => ask = next,
                None => return,
            }
        }
    }

    /// Member `pid`'s word says `saying` from now.
    fn saying(&self, pid: u32, saying: Saying) {
        if let Some(word) = lock(&self.groups.words).get_mut(&pid) {
            word.saying = saying;
            word.since = Instant::now();
        }
        self.spoke();
    }

    /// A member's word moved on: the groups' thread looks again.
    fn spoke(&self) {
        lock(&self.groups.woke).spoke = true;
        self.groups.wake.notify_one();
    }

    /// A member's give-back of a round ended: the groups' thread hears what it gave.
    fn heard(&self, answered: Answered) {
        lock(&self.groups.woke).gave.push(answered);
        self.groups.wake.notify_one();
    }

    /// Whether member `pid`'s word has run past the slowest answer of its kind seen with none
    /// by `now`, judged from outside by D215's record (`Record::stalled`): never while its
    /// kind has had no answer, whose wait follows its microVM's liveness alone, nor with no
    /// word under way, its answer on its way.
    fn overdue(&self, pid: u32, now: Instant) -> bool {
        let words = lock(&self.groups.words);
        words
            .get(&pid)
            .is_some_and(|word| self.record_of(word.saying).stalled(word.since, now).is_some())
    }

    /// The record of the kind of answer `saying` waits on (D215).
    fn record_of(&self, saying: Saying) -> &shards_api::Record {
        match saying {
            Saying::Room => &self.answers.room,
            Saying::GiveBack(_) => &self.answers.reclaim,
        }
    }

    /// The microVM run `id` is of, by its own run's ID: its own, or the provider's of a
    /// joiner (D119), as a give-back of its host's pressure names it (reclaim.rs `giving`).
    fn own_run(&self, id: &str) -> String {
        match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => t.joined.clone().unwrap_or_else(|| id.to_string()),
            _ => id.to_string(),
        }
    }

    /// What each of `members` holds, by its index, as its group sums it: none of one whose
    /// VM process cannot be read.
    fn footprints(&self, members: &[Member]) -> Vec<(usize, u64)> {
        members
            .iter()
            .enumerate()
            .filter_map(|(i, m)| self.footprint_of(m.pid).map(|f| (i, f)))
            .collect()
    }

    /// What VM process `pid` holds, as its group sums it: its footprint (proc_pid_rusage's
    /// `ri_phys_footprint`, Windows' private working set); none where it cannot be read.
    fn footprint_of(&self, pid: u32) -> Option<u64> {
        #[cfg(test)]
        if let Some(&played) = lock(&self.groups.played).get(&pid) {
            return Some(played);
        }
        shards_vmm::platform::process_usage(pid).ok().map(|u| u.footprint)
    }

    /// Tells run `id`'s guest the room its group's budget leaves it (init's `ROOM`, D188):
    /// past what its run uses, the least room a run goes on in, as a slice's is told; or,
    /// with none, that it has no budget, its hold let go of.
    fn tell_group_room(&self, id: &str, pid: u32, room: Option<i64>) {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::ROOM,
            argv: room.map(|r| r.to_string().into_bytes()).into_iter().collect(),
            ..Default::default()
        };
        match self.exec_waiting(
            id,
            &spec,
            None,
            1 << 12,
            super::health::Wait::WhileRuns(pid, &self.answers.room),
        ) {
            Ok(q) if q.status == Some(0) && q.cut.is_none() => {}
            Ok(q) => log(format!(
                "container {id}: telling its microVM its group's room: {}",
                super::reclaim::said_of(&q)
            )),
            Err(_) if !self.running(id) => {}
            Err(e) => log(format!(
                "container {id}: telling its microVM its group's room: {e}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_named_as_a_file_of_the_home_alone() {
        for good in ["agents", "ci-42", "a.b_c", "A9"] {
            assert!(is_group_name(good), "{good}");
        }
        let long = "a".repeat(256);
        for bad in ["", ".", "..", "a/b", "/abs", "a b", "a\\b", "ä", long.as_str()] {
            assert!(!is_group_name(bad), "{bad}");
        }
    }

    #[test]
    fn memory_budgets_read_as_the_kernel_reads_them() {
        assert_eq!(bytes("max"), Ok(None));
        assert_eq!(bytes("0"), Ok(Some(0)));
        assert_eq!(bytes("4096"), Ok(Some(4096)));
        assert_eq!(bytes("512M"), Ok(Some(512 << 20)));
        assert_eq!(bytes("4g"), Ok(Some(4 << 30)));
        assert_eq!(bytes("1T"), Ok(Some(1 << 40)));
        for bad in ["", "M", "-1", "1.5G", "1GB", "max ", "20000000000E"] {
            assert!(bytes(bad).is_err(), "{bad}");
        }
    }

    /// `cpu.max` read as the kernel reads it: a quota and its period in µs, `max` for none,
    /// the period the kernel's default where it is left out; each within the kernel's bounds.
    #[test]
    fn cpu_budgets_read_as_the_kernel_reads_them() {
        assert_eq!(cpu_max("max 100000\n"), Ok(None));
        assert_eq!(cpu_max("max"), Ok(None));
        assert_eq!(cpu_max("50000 100000"), Ok(Some((50_000, 100_000))));
        assert_eq!(cpu_max("250000\n"), Ok(Some((250_000, 100_000))));
        assert_eq!(cpu_max("1000 1000"), Ok(Some((1_000, 1_000))));
        assert_eq!(cpu_max("200000 1000000 more"), Ok(Some((200_000, 1_000_000))));
        for bad in [
            "",
            "lots",
            "50000 lots",
            "-1 100000",
            "999 100000",
            "50000 999",
            "50000 1000001",
            "max 999",
            "17592186044416 100000",
        ] {
            assert!(cpu_max(bad).is_err(), "{bad:?}");
        }
    }

    /// A group's budget its microVMs hold themselves to: none where it lets them use every
    /// CPU of the host's.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_cpu_budget_of_every_cpu_holds_nothing() {
        let host = u64::try_from(crate::resources::host_cpus()).unwrap();
        assert_eq!(held_cpu(None), None);
        assert_eq!(held_cpu(Some((50_000, 100_000))), Some((50_000, 100_000)));
        assert_eq!(held_cpu(Some((host * 100_000, 100_000))), None);
    }

    /// An ask made while a member's word is under way waits behind it, the newest room told
    /// and a give-back of either kept; a round is done once each member it asked has given
    /// back, gone, or given nothing, and what it gave is theirs summed (D183, R04).
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn an_ask_behind_a_word_keeps_its_newest_room_and_any_give_back() {
        let ask = |room, give| Ask { room, give };
        assert_eq!(ask(Some(5), None).over(None), ask(Some(5), None));
        assert_eq!(
            ask(Some(-3), None).over(Some(ask(Some(5), Some(1)))),
            ask(Some(-3), Some(1))
        );
        assert_eq!(
            ask(None, Some(2)).over(Some(ask(Some(5), Some(1)))),
            ask(None, Some(2))
        );
        let gave = |freed| Given::Gave(Gave { freed, unplugging: 0 });
        let mut round = Round {
            id: 1,
            of: [(1, gave(4)), (2, Given::Asked), (3, Given::Nothing)].into(),
        };
        assert!(!round.done());
        let mut hold = Hold {
            round: Some(round),
            ..Hold::default()
        };
        hold.answered(Answered {
            pid: 2,
            round: 0,
            gave: Some(Gave {
                freed: 8,
                unplugging: 0,
            }),
        });
        assert!(
            !hold.round.as_ref().unwrap().done(),
            "another round's answer taken"
        );
        hold.left(2);
        round = hold.round.take().unwrap();
        assert!(round.done());
        assert_eq!(round.gave().freed, 4);
    }

    #[test]
    fn a_group_is_made_once_and_kept_as_its_user_left_it() {
        let home = shards_testdir::TempDir::new("groups-made").unwrap();
        let dir = made(&home, "agents").unwrap();
        assert_eq!(budget(&dir), Ok(Budget::default()));
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.max")).unwrap(),
            "max 100000\n"
        );
        std::fs::write(dir.join("cpu.max"), "50000\n").unwrap();
        assert_eq!(budget(&dir).unwrap().cpu, Some((50_000, 100_000)));
        std::fs::write(dir.join("cpu.max"), "max 100000\n").unwrap();
        std::fs::write(dir.join("memory.high"), "1G\n").unwrap();
        assert_eq!(made(&home, "agents").unwrap(), dir);
        let held = budget(&dir).unwrap();
        assert_eq!(
            (held.high, held.max, held.memory(), held.room_of()),
            (Some(1 << 30), None, Some(1 << 30), Some(1 << 30))
        );
        // Under `memory.max` alone, its microVMs' room is of it where the daemon alone
        // holds them to it, and of nothing on Windows, whose job holds them to it.
        std::fs::write(dir.join("memory.high"), "max\n").unwrap();
        std::fs::write(dir.join("memory.max"), "1G\n").unwrap();
        let held = budget(&dir).unwrap();
        assert_eq!(held.memory(), Some(1 << 30));
        assert_eq!(held.room_of(), if cfg!(windows) { None } else { Some(1 << 30) });
        assert!(made(&home, "../out").is_err());
        std::fs::write(dir.join("memory.max"), "lots\n").unwrap();
        assert!(budget(&dir).unwrap_err().contains("memory.max"));
    }
}
