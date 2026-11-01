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
use super::{Daemon, TAKE_TIMEOUT, Threads, lock, log};
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
    /// The least of its memory budgets, `high` and `max`: what its microVMs' room is of.
    pub fn memory(&self) -> Option<u64> {
        match (self.high, self.max) {
            (Some(h), Some(m)) => Some(h.min(m)),
            (h, m) => h.or(m),
        }
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

#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Clone)]
struct Member {
    pid: u32,
    id: String,
}

/// What the groups' thread hears of runs (D183, slice 3): those begun, by VM process and
/// container, and those ended, by VM process.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
struct Woke {
    ran: Vec<Member>,
    left: Vec<u32>,
}

/// The groups' thread's word from the daemon, and what wakes it; each group's CPU budget,
/// by its name, its file and the daemon's mapping of it; on Windows each group's job too,
/// with the `memory.max` it is held to.
#[cfg(any(target_os = "macos", windows))]
#[derive(Debug, Default)]
pub(super) struct Groups {
    woke: Mutex<Woke>,
    wake: Condvar,
    cpus: Mutex<HashMap<String, (std::fs::File, shards_vmm::vm::group::GroupCpu)>>,
    #[cfg(windows)]
    jobs: Mutex<HashMap<String, (shards_ipc::GroupJob, Option<u64>)>>,
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
            .spawn_scoped(threads, move || self.hold_groups())
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
    /// a budget written is heard within one.
    fn hold_groups(&self) {
        let mut members: HashMap<String, Vec<Member>> = HashMap::new();
        // The groups short of their budgets, which are said again once they are within it.
        let mut short: HashSet<String> = HashSet::new();
        loop {
            let woke = {
                let mut woke = lock(&self.groups.woke);
                if woke.ran.is_empty() && woke.left.is_empty() {
                    woke = if members.is_empty() {
                        self.groups
                            .wake
                            .wait(woke)
                            .unwrap_or_else(PoisonError::into_inner)
                    } else {
                        self.groups
                            .wake
                            .wait_timeout(woke, shards_vmm::platform::PRESSURE_WINDOW)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    };
                }
                std::mem::take(&mut *woke)
            };
            for m in woke.ran {
                if let Some(group) = self.parent_of(&m.id).filter(|p| !p.is_empty()) {
                    members.entry(group).or_default().push(m);
                }
            }
            for pid in woke.left {
                for list in members.values_mut() {
                    list.retain(|m| m.pid != pid);
                }
            }
            members.retain(|_, list| !list.is_empty());
            short.retain(|name| members.contains_key(name));
            for (name, list) in &members {
                self.hold_group(name, list, &mut short);
            }
        }
    }

    /// Holds group `name`'s microVMs `members` to its budget: each told the room it leaves
    /// them (init's `ROOM`, D188), and where it is short each asked to give back what it
    /// holds cold; past `memory.max` with nothing given back, the one that holds the most
    /// stopped, as a cgroup's OOM killer kills within it. What they hold is their VM
    /// processes' footprints (proc_pid_rusage), written to the group's `memory.current`.
    fn hold_group(&self, name: &str, members: &[Member], short: &mut HashSet<String>) {
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
        let footprints = || -> Vec<(usize, u64)> {
            members
                .iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    shards_vmm::platform::process_usage(m.pid)
                        .ok()
                        .map(|u| (i, u.footprint))
                })
                .collect()
        };
        let mut held = footprints();
        let sum = |held: &[(usize, u64)]| held.iter().fold(0u64, |s, (_, b)| s.saturating_add(*b));
        let current = sum(&held);
        let _ = std::fs::write(dir.join("memory.current"), format!("{current}\n"));
        let Some(limit) = budget.memory() else {
            return;
        };
        let signed = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
        let room = signed(limit).saturating_sub(signed(current));
        for m in members {
            self.tell_group_room(&m.id, room);
        }
        if room >= 0 {
            if short.remove(name) {
                log(format!(
                    "group {name}: its microVMs hold {} MiB, within its budget of {} MiB again",
                    current >> 20,
                    limit >> 20
                ));
            }
            return;
        }
        if short.insert(name.to_string()) {
            log(format!(
                "group {name}: its microVMs hold {} MiB, {} MiB past its budget of {} MiB: each told so, and asked to give back what it holds cold",
                current >> 20,
                room.unsigned_abs() >> 20,
                limit >> 20
            ));
        }
        let (mut freed, mut unplugging) = (0u64, 0u64);
        for m in members {
            if let Some(gave) = self.give_back_of(&m.id) {
                freed = freed.saturating_add(gave.freed);
                unplugging = unplugging.saturating_add(gave.unplugging);
            }
        }
        if freed > 0 || unplugging > 0 {
            log(format!(
                "group {name}: its microVMs gave back {} MiB they held cold of their files, and asked to unplug {} MiB",
                freed >> 20,
                unplugging >> 20
            ));
        }
        let Some(max) = budget.max else {
            return;
        };
        held = footprints();
        let after = sum(&held);
        if after <= max || freed > 0 || unplugging > 0 {
            return;
        }
        // Past its hard budget with nothing given back: the one that holds the most goes,
        // as a cgroup's OOM killer takes its largest (mm/oom_kill.c, `oom_badness`).
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

    /// Tells run `id`'s guest the room its group's budget leaves it (init's `ROOM`, D188):
    /// past what its run uses, the least room a run goes on in, as a slice's is told.
    fn tell_group_room(&self, id: &str, room: i64) {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::ROOM,
            argv: vec![room.to_string().into_bytes()],
            ..Default::default()
        };
        match self.exec_quietly(id, &spec, None, 1 << 12, TAKE_TIMEOUT) {
            Ok(q) if q.status == Some(0) && !q.timed_out => {}
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
            (held.high, held.max, held.memory()),
            (Some(1 << 30), None, Some(1 << 30))
        );
        assert!(made(&home, "../out").is_err());
        std::fs::write(dir.join("memory.max"), "lots\n").unwrap();
        assert!(budget(&dir).unwrap_err().contains("memory.max"));
    }
}
