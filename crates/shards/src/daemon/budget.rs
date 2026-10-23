//! What a budget leaves its microVMs (docs/design/architecture.md D188): where the cgroup
//! a microVM's VM process is made in, or one above it, holds its processes to a budget
//! (`memory.high`, `memory.max`), the bytes that budget still has before its host must
//! take back memory its microVMs hold, which with no swap is their guests' own RAM, a
//! template's file pages among it (PM M309). Each microVM's guest is told them as its run
//! begins, each time a budget over it is set, changed or met, and as another microVM held
//! to it ends (init's `ROOM`), and as it gives back what it holds cold (`RECLAIM`, reclaim.rs):
//! it holds its run within them, so that its own kernel drops its run's page cache first, as
//! a container's kernel drops a container's within its cgroup.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use super::{Daemon, RunState, TAKE_TIMEOUT, Threads, lock, log};
use crate::containers::Disk;

/// The cgroup v2 hierarchy's root, which holds nothing to a budget (cgroup-v2.rst).
pub(super) const ROOT: &str = "/sys/fs/cgroup";

/// What the budget over the cgroup `dir` leaves the microVMs made in it: of `dir` and each
/// cgroup above it below `root` held to a budget (`memory.high` or `memory.max` short of
/// `max`), the least one may still take before its host must take back memory a process
/// maps or holds as its own: its budget less all it holds but its page cache no process
/// maps, which its kernel takes first (memory.stat's `file` less its `file_mapped` and its
/// `shmem`); and which cgroup that is. None where no cgroup it is in has a budget.
pub(super) fn room(dir: &Path, root: &Path) -> Option<(i64, PathBuf)> {
    let mut least: Option<(i64, PathBuf)> = None;
    let mut at = dir;
    while at.starts_with(root) && at != root {
        if let Some(room) = room_at(at)
            && least.as_ref().is_none_or(|(l, _)| room < *l)
        {
            least = Some((room, at.to_path_buf()));
        }
        at = at.parent()?;
    }
    least
}

/// What the budget of the cgroup `dir` itself leaves ([`room`]): none where it has none.
fn room_at(dir: &Path) -> Option<i64> {
    let budget = ["memory.high", "memory.max"]
        .iter()
        .filter_map(|f| limit(&dir.join(f)))
        .min()?;
    let current = std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let stat = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
    let of = |key: &str| {
        stat.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let unmapped = of("file")
        .saturating_sub(of("file_mapped"))
        .saturating_sub(of("shmem"));
    let held = current.saturating_sub(unmapped);
    let signed = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
    Some(signed(budget).saturating_sub(signed(held)))
}

/// A cgroup's limit as its file says it: none for `max`.
fn limit(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The cgroup process `pid` is in, as its /proc entry says (its cgroup v2 line, `0::`).
pub(super) fn cgroup_of(pid: u32) -> Option<PathBuf> {
    let said = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = said.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new(ROOT).join(path.trim_start_matches('/')))
}

/// Of each microVM the rooms of the cgroups that hold it to a budget: `room` of each, its
/// short room shared with the others held by the same budget, so that what all of them
/// are asked to give back is what that budget is short of (D188): by its VM process's ID.
pub(super) fn shares(rooms: &HashMap<u32, Option<(i64, PathBuf)>>) -> HashMap<u32, i64> {
    let mut under: HashMap<&Path, i64> = HashMap::new();
    for (_, by) in rooms.values().flatten() {
        let n = under.entry(by.as_path()).or_default();
        *n = n.saturating_add(1);
    }
    rooms
        .iter()
        .filter_map(|(&pid, r)| {
            let (room, by) = r.as_ref()?;
            let n = under.get(by.as_path()).copied().unwrap_or(1).max(1);
            Some((pid, if *room < 0 { room / n } else { *room }))
        })
        .collect()
}

/// The microVMs held to a budget the daemon tells of the room it leaves them, and the
/// cgroups it watches for their budgets: on Linux, where its thread runs
/// ([`Daemon::start_budgets`]); none in a daemon's unit tests, whose VMs a test plays.
#[derive(Debug, Default)]
pub(super) struct Budgets {
    running: AtomicBool,
    /// Each microVM followed, by its VM process's ID.
    held: Mutex<HashMap<u32, Held>>,
    watched: Mutex<Watched>,
}

/// The cgroups watched: by its watches' numbers, which cgroup each is of and what it
/// watches; those whose budgets are watched, and those of them whose events are, each
/// once it has a budget, as the events of one without are no microVM's.
#[derive(Debug, Default)]
struct Watched {
    by: HashMap<i32, (PathBuf, Watch)>,
    budgets: HashSet<PathBuf>,
    events: HashSet<PathBuf>,
}

/// A microVM followed for its budget.
#[derive(Debug)]
struct Held {
    /// The cgroup its VM process is in.
    dir: PathBuf,
    /// The room it was last told, `None` for none: no budget, or never told one.
    told: Option<i64>,
    /// Whether its last word said its room was short, nothing told it since: it holds
    /// at what it uses, and the host's passes take what it holds cold.
    short: bool,
    /// A word to it is under way; and another is due once that has ended.
    telling: bool,
    again: bool,
}

/// What a watch of a cgroup's watches: a file of its budget, or its events, which say it
/// was met (cgroup-v2.rst, `memory.events`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    Budget,
    Events,
}

/// What woke the budget's thread: watches written, microVMs that began a run and microVMs
/// that may have ended, by VM process ID.
#[derive(Debug, Default)]
struct Woke {
    fired: Vec<i32>,
    ran: Vec<u32>,
    left: Vec<u32>,
}

static WOKE: Mutex<Woke> = Mutex::new(Woke {
    fired: Vec::new(),
    ran: Vec::new(),
    left: Vec::new(),
});
static WAKE: Condvar = Condvar::new();

/// A watched file was written: called on the inotify instance's reader thread, under its
/// lock, so only noted.
#[cfg(target_os = "linux")]
fn fired(watch: i32) {
    lock(&WOKE).fired.push(watch);
    WAKE.notify_one();
}

impl<D: Disk> Daemon<D> {
    /// Starts the thread that tells microVMs what their budgets leave them, on a host whose
    /// processes have cgroups to be held to one in (Linux).
    pub(super) fn start_budgets<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        if !Path::new(ROOT).join("cgroup.controllers").exists() {
            return;
        }
        match std::thread::Builder::new()
            .name("budgets".into())
            .spawn_scoped(threads, move || self.watch_budgets(threads))
        {
            Ok(_) => self.budgets.running.store(true, Ordering::SeqCst),
            Err(e) => log(format!(
                "the budgets' thread: {e}; microVMs are held to their budgets by their host's passes alone"
            )),
        }
    }

    /// Run `pid`'s VM process began a run: followed for its budget, as it ends.
    pub(super) fn budget_ran(&self, pid: u32) {
        if self.budgets.running.load(Ordering::SeqCst) {
            lock(&WOKE).ran.push(pid);
            WAKE.notify_one();
        }
    }

    /// A run of VM process `pid` ended: the microVM followed no more once it has no run,
    /// and the others its budget held told what it left them.
    pub(super) fn budget_left(&self, pid: u32) {
        if self.budgets.running.load(Ordering::SeqCst) {
            lock(&WOKE).left.push(pid);
            WAKE.notify_one();
        }
    }

    /// The cgroup the microVMs under `parent` are made in (D183): its spawner's scope, where
    /// it is placed; the daemon's own, for none.
    pub(super) fn cgroup_of_parent(&self, parent: &str) -> Option<PathBuf> {
        if parent.is_empty() {
            return cgroup_of(std::process::id());
        }
        self.parents.placed(parent).and_then(|g| cgroup_of(g.pid()))
    }

    /// Ends the warm VMs whose processes a budget over cgroup `level` holds, of every pool:
    /// what they hold is speculation, which a budget with no room for its runs gives back
    /// first (D188). How many it ended.
    fn end_warm_under(&self, level: &Path) -> usize {
        let mut ended = 0;
        let mut state = lock(&self.state);
        for (key, pool) in &mut state.pools {
            let (gone, kept): (Vec<_>, Vec<_>) = pool
                .ready
                .drain(..)
                .partition(|r| cgroup_of(r.vm.id()).is_some_and(|d| d.starts_with(level)));
            pool.ready = kept.into();
            for warm in gone {
                log(format!(
                    "ending warm VM {} of {}: its budget, {}, has no room for it",
                    warm.vm.id(),
                    key.dir.display(),
                    level.display()
                ));
                let _ = warm.vm.kill(libc::SIGKILL);
                ended += 1;
            }
        }
        ended
    }

    /// The cgroup VM process `pid` is in, as it was found as it began its run; else as
    /// its /proc entry says now.
    pub(super) fn cgroup_of_vm(&self, pid: u32) -> Option<PathBuf> {
        lock(&self.budgets.held)
            .get(&pid)
            .map(|h| h.dir.clone())
            .or_else(|| cgroup_of(pid))
    }

    /// Tells each microVM what its budget leaves it as something changes it: a run begun,
    /// a budget set or changed, or met, a microVM held to it gone.
    fn watch_budgets<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        loop {
            let woke = {
                let mut woke = lock(&WOKE);
                while woke.fired.is_empty() && woke.ran.is_empty() && woke.left.is_empty() {
                    woke = WAKE.wait(woke).unwrap_or_else(PoisonError::into_inner);
                }
                std::mem::take(&mut *woke)
            };
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            let mut tell: HashSet<u32> = HashSet::new();
            for pid in woke.ran {
                if let Some(dir) = cgroup_of(pid) {
                    self.watch_chain(&dir);
                    lock(&self.budgets.held).entry(pid).or_insert(Held {
                        dir,
                        told: None,
                        short: false,
                        telling: false,
                        again: false,
                    });
                    tell.insert(pid);
                }
            }
            for pid in woke.left {
                if self.vm_runs(pid) {
                    continue;
                }
                let Some(gone) = lock(&self.budgets.held).remove(&pid) else {
                    continue;
                };
                // What it held goes to every other microVM a budget it was under holds.
                for (&other, held) in lock(&self.budgets.held).iter() {
                    if shares_a_budget(&gone.dir, &held.dir) {
                        tell.insert(other);
                    }
                }
            }
            let fired: Vec<(PathBuf, Watch)> = {
                let watched = lock(&self.budgets.watched);
                woke.fired
                    .iter()
                    .filter_map(|w| watched.by.get(w).cloned())
                    .collect()
            };
            let mut met: HashSet<&Path> = HashSet::new();
            for (level, what) in &fired {
                if *what == Watch::Budget {
                    // A budget set: its events watched from now on.
                    self.watch_events(level);
                }
                for (&pid, held) in lock(&self.budgets.held).iter() {
                    if !held.dir.starts_with(level) {
                        continue;
                    }
                    match what {
                        Watch::Budget => {
                            tell.insert(pid);
                        }
                        Watch::Events => {
                            met.insert(level);
                        }
                    }
                }
            }
            // A budget met: told where it is short now, and not told so before; one with
            // room again no longer short.
            if !met.is_empty() {
                let mut held = lock(&self.budgets.held);
                for (&pid, h) in held.iter_mut() {
                    if !met.iter().any(|l| h.dir.starts_with(l)) {
                        continue;
                    }
                    match room(&h.dir, Path::new(ROOT)) {
                        Some((r, _)) if r < 0 && !h.short => {
                            tell.insert(pid);
                        }
                        Some((r, _)) if r >= 0 => h.short = false,
                        _ => {}
                    }
                }
            }
            for pid in tell {
                self.tell_room(threads, pid);
            }
        }
    }

    /// Watches the cgroup `dir` and each above it below the root, once each, for writes to
    /// their budgets, and the events of each with a budget.
    fn watch_chain(&self, dir: &Path) {
        let mut at = dir;
        while at.starts_with(ROOT) && at != Path::new(ROOT) {
            if !lock(&self.budgets.watched).budgets.insert(at.to_path_buf()) {
                return;
            }
            self.watch_file(at, "memory.high", Watch::Budget);
            self.watch_file(at, "memory.max", Watch::Budget);
            self.watch_events(at);
            let Some(up) = at.parent() else { return };
            at = up;
        }
    }

    /// Watches the events of cgroup `dir`, once, where it has a budget.
    fn watch_events(&self, dir: &Path) {
        if room_at(dir).is_some() && lock(&self.budgets.watched).events.insert(dir.to_path_buf()) {
            self.watch_file(dir, "memory.events", Watch::Events);
        }
    }

    /// Watches cgroup `dir`'s `file` for writes (Linux's inotify, the process's one
    /// instance, PM M257).
    fn watch_file(&self, dir: &Path, file: &str, what: Watch) {
        #[cfg(target_os = "linux")]
        match std::fs::File::open(dir.join(file)).and_then(|f| shards_vmm::platform::watch_writes(&f, fired))
        {
            Ok(w) => {
                lock(&self.budgets.watched)
                    .by
                    .insert(w, (dir.to_path_buf(), what));
            }
            Err(e) => log(format!(
                "watching {}: {e}; its microVMs are held to its budget by their host's passes alone",
                dir.join(file).display()
            )),
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (dir, file, what);
    }

    /// Whether VM process `pid` has a run still: its own, or a joiner's.
    fn vm_runs(&self, pid: u32) -> bool {
        lock(&self.runs)
            .values()
            .any(|r| matches!(r, RunState::Tracked(t) if t.vm.id() == pid))
    }

    /// A run of VM process `pid`'s, through which it is told, its own, else a joiner's; and
    /// the microVM's own container's ID, which its giving back is kept by (reclaim.rs).
    fn run_of_vm(&self, pid: u32) -> Option<(String, String)> {
        let runs = lock(&self.runs);
        let of = |own: bool| {
            runs.iter().find_map(|(id, r)| match r {
                RunState::Tracked(t) if t.vm.id() == pid && (t.joined.is_none() || !own) => {
                    Some((id.clone(), t.joined.clone().unwrap_or_else(|| id.clone())))
                }
                _ => None,
            })
        };
        of(true).or_else(|| of(false))
    }

    /// How many microVMs followed the budget of cgroup `level` holds.
    fn held_under(&self, level: &Path) -> i64 {
        let n = lock(&self.budgets.held)
            .values()
            .filter(|h| h.dir.starts_with(level))
            .count();
        i64::try_from(n).unwrap_or(i64::MAX)
    }

    /// Tells microVM `pid` the room its budget leaves it now, on a thread of its own; once
    /// more after a word under way, which another change came during.
    fn tell_room<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, pid: u32) {
        {
            let mut held = lock(&self.budgets.held);
            let Some(h) = held.get_mut(&pid) else { return };
            // One under no budget, never told one, has nothing to hear.
            if h.told.is_none() && !h.telling && room(&h.dir, Path::new(ROOT)).is_none() {
                return;
            }
            if h.telling {
                h.again = true;
                return;
            }
            h.telling = true;
        }
        let spawned = std::thread::Builder::new()
            .name("room".into())
            .spawn_scoped(threads, move || {
                loop {
                    self.tell_room_now(pid);
                    let mut held = lock(&self.budgets.held);
                    let Some(h) = held.get_mut(&pid) else { return };
                    if !h.again {
                        h.telling = false;
                        return;
                    }
                    h.again = false;
                }
            });
        if let Err(e) = spawned {
            if let Some(h) = lock(&self.budgets.held).get_mut(&pid) {
                h.telling = false;
            }
            log(format!("VM {pid}: telling it its budget's room: its thread: {e}"));
        }
    }

    /// Tells microVM `pid` the room its budget leaves it now, through one of its runs
    /// (init's `ROOM`). Where the budget is short, the warm VMs it holds go first, which are
    /// speculation; and then it gives back what it holds cold and is held within its share of
    /// what is short (`RECLAIM`), as at its host's pressure, which an idle microVM over its
    /// budget, asking for nothing, never raises. Nothing where it has no budget and was told
    /// of none.
    fn tell_room_now(&self, pid: u32) {
        let Some(dir) = lock(&self.budgets.held).get(&pid).map(|h| h.dir.clone()) else {
            return;
        };
        let Some((id, vm)) = self.run_of_vm(pid) else {
            return;
        };
        let mut room = room(&dir, Path::new(ROOT));
        if let Some((r, level)) = &room
            && *r < 0
            && self.end_warm_under(level) > 0
        {
            room = self::room(&dir, Path::new(ROOT));
        }
        let had = lock(&self.budgets.held).get(&pid).and_then(|h| h.told);
        if room.is_none() && had.is_none() {
            return;
        }
        let told = match &room {
            Some((r, level)) if *r < 0 => {
                // Its share of what the budget is short of, so that all it holds give back
                // that, no more; none where it gives back already.
                let share = r / self.held_under(level).max(1);
                if !lock(self.watching_giving()).insert(vm.clone()) {
                    return;
                }
                let gave = self.give_back_of(&id, Some(share));
                lock(self.watching_giving()).remove(&vm);
                match gave {
                    Some(gave) => self.gave(gave),
                    None => return,
                }
                Some(share)
            }
            room => {
                let room = room.as_ref().map(|(r, _)| *r);
                let spec = shards_abi::run::Spec {
                    builtin: shards_abi::run::builtin::ROOM,
                    argv: room.map(|r| r.to_string().into_bytes()).into_iter().collect(),
                    ..Default::default()
                };
                match self.exec_quietly(&id, &spec, None, 1 << 12, TAKE_TIMEOUT) {
                    Ok(q) if q.status == Some(0) && !q.timed_out => room,
                    Ok(q) => {
                        log(format!(
                            "container {id}: telling its microVM its budget's room: {}",
                            super::reclaim::said_of(&q)
                        ));
                        return;
                    }
                    Err(_) if !self.running(&id) => return,
                    Err(e) => {
                        log(format!(
                            "container {id}: telling its microVM its budget's room: {e}"
                        ));
                        return;
                    }
                }
            }
        };
        if let Some(h) = lock(&self.budgets.held).get_mut(&pid) {
            h.told = told;
            h.short = told.is_some_and(|r| r < 0);
        }
        match (had, told) {
            (None, Some(r)) => log(format!(
                "container {id}: its microVM held within its budget, {}, {} MiB of room",
                dir.display(),
                r.max(0) >> 20
            )),
            (Some(_), None) => log(format!("container {id}: its microVM held to no budget")),
            _ => {}
        }
    }
}

/// Whether a cgroup above `a` that holds to a budget holds `b` too.
fn shares_a_budget(a: &Path, b: &Path) -> bool {
    let mut at = a;
    while at.starts_with(ROOT) && at != Path::new(ROOT) {
        if b.starts_with(at) && room_at(at).is_some() {
            return true;
        }
        let Some(up) = at.parent() else { return false };
        at = up;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// A cgroup of a fake hierarchy: its budget's two files and what it holds.
    fn cgroup(dir: &Path, high: Option<u64>, max: Option<u64>, current: u64, stat: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let said = |l: Option<u64>| l.map_or("max\n".to_string(), |b| format!("{b}\n"));
        std::fs::write(dir.join("memory.high"), said(high)).unwrap();
        std::fs::write(dir.join("memory.max"), said(max)).unwrap();
        std::fs::write(dir.join("memory.current"), format!("{current}\n")).unwrap();
        std::fs::write(dir.join("memory.stat"), stat).unwrap();
    }

    /// A budget's room is what it may still take before its host takes back memory a
    /// process maps or holds, its page cache that none maps counted as room: the least of
    /// every cgroup held to one, from a microVM's own up, and which it is; none with none.
    #[test]
    fn a_budgets_room_is_the_least_any_cgroup_over_a_microvm_leaves() {
        let tmp = shards_testdir::TempDir::new("budget-room").unwrap();
        let root = tmp.join("cg");
        let slice = root.join("user.slice").join("a.slice");
        let scope = slice.join("shards-1-0.scope");
        let stat = |file: u64, mapped: u64, shmem: u64| {
            format!("anon 1\nfile {file}\nkernel 2\nfile_mapped {mapped}\nfile_dirty 0\nshmem {shmem}\n")
        };
        cgroup(&root.join("user.slice"), None, None, 900 * MIB, &stat(0, 0, 0));
        cgroup(
            &slice,
            Some(384 * MIB),
            None,
            383 * MIB,
            &stat(149 * MIB, 117 * MIB, 0),
        );
        cgroup(&scope, None, None, 383 * MIB, &stat(149 * MIB, 117 * MIB, 0));
        // 384 MiB less what it holds but its 32 MiB of page cache no process maps.
        assert_eq!(
            room(&scope, &root),
            Some(((384 - (383 - 32)) as i64 * MIB as i64, slice.clone()))
        );
        // Its shared memory is not page cache its kernel may drop.
        cgroup(
            &slice,
            Some(384 * MIB),
            None,
            383 * MIB,
            &stat(149 * MIB, 117 * MIB, 32 * MIB),
        );
        assert_eq!(room(&scope, &root), Some((MIB as i64, slice.clone())));
        // A hard limit under the high counts, and short is below none.
        cgroup(
            &slice,
            Some(384 * MIB),
            Some(300 * MIB),
            383 * MIB,
            &stat(0, 0, 0),
        );
        assert_eq!(room(&scope, &root), Some((-83 * MIB as i64, slice.clone())));
        // The least of two: the scope's own, tighter than its slice's.
        cgroup(&scope, Some(350 * MIB), None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root), Some((-83 * MIB as i64, slice.clone())));
        cgroup(&scope, Some(250 * MIB), None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root), Some((-133 * MIB as i64, scope.clone())));
        // None held to a budget: no room to tell.
        cgroup(&scope, None, None, 383 * MIB, &stat(0, 0, 0));
        cgroup(&slice, None, None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root), None);
        // The root itself holds nothing to a budget.
        cgroup(&root, Some(MIB), None, 900 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root), None);
    }

    /// A room short of what its budget holds is shared among the microVMs it holds, so
    /// that they give back what it is short of, no more; a room to spare is each's whole.
    #[test]
    fn a_short_room_is_shared_among_the_microvms_its_budget_holds() {
        let a = PathBuf::from("/sys/fs/cgroup/a.slice");
        let b = PathBuf::from("/sys/fs/cgroup/b.slice");
        let rooms = HashMap::from([
            (1, Some((-90, a.clone()))),
            (2, Some((-90, a.clone()))),
            (3, Some((-90, a))),
            (4, Some((50, b))),
            (5, None),
        ]);
        let shared = shares(&rooms);
        assert_eq!(shared.get(&1), Some(&-30));
        assert_eq!(shared.get(&3), Some(&-30));
        assert_eq!(shared.get(&4), Some(&50));
        assert_eq!(shared.get(&5), None);
    }
}
