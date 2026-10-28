//! The groups shards keeps for `--cgroup-parent` where its host has none a user's own tools
//! make and cap (D183, slice 3): macOS, and Windows. A group is a directory of the home,
//! `groups/NAME`, made with the first run under it, whose budgets are files in cgroup v2's
//! own formats, as a Linux user writes a cgroup's (Documentation/admin-guide/cgroup-v2.rst):
//! `memory.high` and `memory.max`, bytes or `max`. The daemon holds the group's microVMs to
//! them.

#[cfg(target_os = "macos")]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::{Condvar, Mutex, PoisonError};

#[cfg(target_os = "macos")]
use super::{Daemon, TAKE_TIMEOUT, Threads, lock, log};
#[cfg(target_os = "macos")]
use crate::containers::Disk;

/// The directory of the home groups are kept in.
pub(super) const DIR: &str = "groups";

/// What a group holds its microVMs to: each `None` where its file says `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Budget {
    /// `memory.high`: its microVMs told their room under it, and give back to it.
    pub high: Option<u64>,
    /// `memory.max`: as `high`, and past it with nothing more given back, a microVM of it
    /// is stopped, as a cgroup's OOM killer kills within it.
    pub max: Option<u64>,
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
    for file in ["memory.high", "memory.max"] {
        let path = dir.join(file);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(b"max\n")
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
    Ok(Budget {
        high: memory("memory.high")?,
        max: memory("memory.max")?,
    })
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

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct Member {
    pid: u32,
    id: String,
}

/// What the groups' thread hears of runs (D183, slice 3): those begun, by VM process and
/// container, and those ended, by VM process.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct Woke {
    ran: Vec<Member>,
    left: Vec<u32>,
}

/// The groups' thread's word from the daemon, and what wakes it.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub(super) struct Groups {
    woke: Mutex<Woke>,
    wake: Condvar,
}

#[cfg(target_os = "macos")]
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

    #[test]
    fn a_group_is_made_once_and_kept_as_its_user_left_it() {
        let home = shards_testdir::TempDir::new("groups-made").unwrap();
        let dir = made(&home, "agents").unwrap();
        assert_eq!(budget(&dir), Ok(Budget::default()));
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
