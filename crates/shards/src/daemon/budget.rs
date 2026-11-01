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
pub(crate) const ROOT: &str = "/sys/fs/cgroup";

/// What the budget over the cgroup `dir` leaves the microVMs made in it: of `dir` and each
/// cgroup above it below `root` held to a budget (`memory.high` or `memory.max` short of
/// `max`), the least one may still take before its host must take back memory its
/// processes map or hold as their own ([`Holding::room_with`]), the file pages they map
/// counted in wherever they are charged, and those of them their host took back under it
/// that `evicted` says of what they map now, by process ([`Budgets::evicted`]); and which
/// cgroup that is. None where no cgroup it is in has a budget.
pub(crate) fn room(
    dir: &Path,
    root: &Path,
    evicted: &mut dyn FnMut(&Mapped) -> u64,
) -> Option<(i64, PathBuf)> {
    let mut least: Option<(i64, PathBuf)> = None;
    let mut at = dir;
    while at.starts_with(root) && at != root {
        if let Some(held) = held_at(at) {
            let mapped = mapped_under(at);
            let files = mapped.iter().fold(0u64, |sum, &(_, b)| sum.saturating_add(b));
            let room = held.room_with(files, evicted(&mapped));
            if least.as_ref().is_none_or(|(l, _)| room < *l) {
                least = Some((room, at.to_path_buf()));
            }
        }
        at = at.parent()?;
    }
    least
}

/// What a cgroup with a budget holds that its host may take back from its processes alone,
/// which with no swap is from its microVMs' guests, by their own kernels (D188): all but its
/// page cache no process maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Holding {
    /// Its budget: the least of its `memory.high` and `memory.max`.
    pub budget: u64,
    /// Its anonymous memory, its shared memory and its kernel's (memory.stat's `anon`,
    /// `shmem`, `kernel`, `sock` and `zswap`), from one read of its statistics.
    pub own: u64,
    /// Its files' pages a process maps (its `file_mapped`), from that read.
    pub mapped: u64,
    /// What it holds in all (`memory.current`), which its kernel counts at once, where its
    /// statistics may be behind it either way (PM M309): just after the kernel reclaimed a
    /// gigabyte of a slice's page cache they still counted it, a room of all its budget told
    /// a microVM whose guest then filled it; just after a microVM ended they still counted
    /// its memory, a budget short by 577 MiB told the next, which then read a gigabyte at
    /// 1.4 MiB/s.
    pub current: u64,
    /// What it holds less its page cache no process maps.
    pub floor: u64,
}

impl Holding {
    /// What it holds that its host may take back from its processes alone, its processes
    /// mapping `files` bytes of files wherever their pages are charged: no less than all
    /// it holds but its page cache no process maps, and of what is charged to it no more
    /// than it holds, with what its processes map past what is charged to it.
    pub fn held(&self, files: u64) -> u64 {
        let charged = self
            .own
            .saturating_add(self.mapped)
            .max(self.floor)
            .min(self.current);
        charged.saturating_add(files.saturating_sub(self.mapped))
    }

    /// The bytes of its budget not [`held`](Self::held), less the inactive pages its kernel
    /// keeps beside the mapped ones charged to it ([`lru_buffer`]): short of none where it
    /// holds past it.
    pub fn room(&self, files: u64) -> i64 {
        self.room_with(files, 0)
    }

    /// Its [`room`](Self::room) with `evicted` bytes more of its microVMs' guests' RAM held,
    /// file pages they mapped that their host took back under the budget, as though they
    /// were mapped still: its guests read them again as they run, and their going is no
    /// room. Counted as room, a lone microVM's template's pages taken back, 71 MiB of them,
    /// told its guest 111 MiB of room where its budget was 23 MiB short, which its page
    /// cache filled in 3 s, and its host then took back all of its RAM it could, its vCPUs
    /// throttled 68 s (PM M318).
    pub fn room_with(&self, files: u64, evicted: u64) -> i64 {
        let signed = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
        signed(self.budget)
            .saturating_sub(signed(self.held(files.saturating_add(evicted))))
            .saturating_sub(signed(lru_buffer(self.mapped.saturating_add(evicted))))
    }
}

/// The page cache a cgroup's kernel keeps inactive beside `mapped` bytes of its files that
/// processes map, its working set, before it takes the working set itself: its reclaim
/// deactivates active pages once the inactive are fewer than the active by its ratio, 1
/// under a GiB of them and the square root of ten times their GiB past it (mm/vmscan.c,
/// `inactive_is_low`). A room that let a guest fill its budget to its mapped pages had it
/// read its own RAM back, 596,000 file refaults in a 9.5 s read where one with them beside
/// it refaulted 300 (PM M309).
pub(super) fn lru_buffer(mapped: u64) -> u64 {
    let gb = mapped.saturating_mul(2) >> 30;
    let ratio = if gb == 0 { 1 } else { gb.saturating_mul(10).isqrt() };
    mapped / ratio.max(1)
}

/// What the cgroup `dir` holds of its budget ([`Holding`]): none where it has no budget.
pub(super) fn held_at(dir: &Path) -> Option<Holding> {
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
    };
    let get = |key: &str| of(key).unwrap_or(0);
    // Its kernel's in all, where the kernel counts it so (Linux 5.18); else its parts.
    let kernel = of("kernel").unwrap_or_else(|| {
        [
            "kernel_stack",
            "pagetables",
            "sec_pagetables",
            "percpu",
            "vmalloc",
            "slab",
        ]
        .iter()
        .map(|k| get(k))
        .fold(0u64, u64::saturating_add)
    });
    let own = ["anon", "shmem", "sock", "zswap"]
        .iter()
        .map(|k| get(k))
        .fold(kernel, u64::saturating_add);
    let unmapped = get("file")
        .saturating_sub(get("file_mapped"))
        .saturating_sub(get("shmem"));
    Some(Holding {
        budget,
        own,
        mapped: get("file_mapped"),
        current,
        floor: current.saturating_sub(unmapped),
    })
}

/// What each process maps of files, by process ID ([`mapped_under`]).
pub(crate) type Mapped = [(u32, u64)];

/// The processes of cgroup `dir` and those below it, each with the bytes of files it maps,
/// each page once among them (/proc/PID/smaps_rollup's `Pss_File`, which shares a page's
/// bytes among the processes that map it), wherever its page is charged: a restored guest's
/// own RAM, its template's file pages, among them, which its budget must hold however they
/// came to be charged elsewhere, as any evicted comes back charged to it as its guest reads
/// it again (PM M309). A process whose entry says none counts its `RssFile`.
fn mapped_under(dir: &Path) -> Vec<(u32, u64)> {
    let mut pids = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        if let Ok(procs) = std::fs::read_to_string(d.join("cgroup.procs")) {
            pids.extend(procs.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
        }
        if let Ok(entries) = std::fs::read_dir(&d) {
            dirs.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
    }
    pids.into_iter()
        .filter_map(|pid| Some((pid, mapped_by(pid)?)))
        .collect()
}

/// The bytes of files process `pid` maps, a page shared among the processes that map it
/// counted in their parts (its /proc/PID/smaps_rollup's `Pss_File`), or its `RssFile` where
/// its entry says none.
fn mapped_by(pid: u32) -> Option<u64> {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok();
    rollup
        .as_deref()
        .and_then(|r| kib(r, "Pss_File:"))
        .or_else(|| resident_file(pid))
}

/// The bytes of files process `pid` holds resident, each page it maps whole however many
/// others map it too (its /proc/PID/status's `RssFile`).
pub(super) fn resident_file(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    kib(&status, "RssFile:")
}

/// The bytes a /proc file's line `key` says in kB.
fn kib(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| {
            l.strip_prefix(key)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|k| k.saturating_mul(1024))
}

/// Whether the cgroup `dir`, or one above it below `root`, holds its processes to a budget:
/// its limits read alone.
pub(super) fn budgeted(dir: &Path, root: &Path) -> bool {
    let mut at = dir;
    while at.starts_with(root) && at != root {
        if ["memory.high", "memory.max"]
            .iter()
            .any(|f| limit(&at.join(f)).is_some())
        {
            return true;
        }
        let Some(up) = at.parent() else { return false };
        at = up;
    }
    false
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

impl Budgets {
    /// Of the processes `mapped` names, what the microVMs followed among them hold resident
    /// no more of the most of files each held since it was followed ([`resident_file`]):
    /// their guests' RAM their host took back, which they read again as they run
    /// ([`Holding::room_with`]); each one's most raised to what it holds now.
    pub(super) fn evicted(&self, mapped: &Mapped) -> u64 {
        let followed: Vec<u32> = {
            let held = lock(&self.held);
            mapped
                .iter()
                .map(|&(pid, _)| pid)
                .filter(|pid| held.contains_key(pid))
                .collect()
        };
        let resident: Vec<(u32, u64)> = followed
            .into_iter()
            .filter_map(|pid| Some((pid, resident_file(pid)?)))
            .collect();
        evicted_of(&mut lock(&self.held), &resident)
    }

    /// VM process `pid` gave back `freed` bytes of what it held cold, holding `before` bytes
    /// of files resident before it did ([`resident_file`]): its most held lowered by what of
    /// its files it holds no more ([`given_back`]).
    pub(super) fn gave_back(&self, pid: u32, before: Option<u64>, freed: u64) {
        let (Some(before), Some(now)) = (before, resident_file(pid)) else {
            return;
        };
        if let Some(h) = lock(&self.held).get_mut(&pid) {
            h.most = given_back(h.most, before, now, freed);
        }
    }
}

/// The most a microVM held resident of files, `most`, once it has given back `freed` bytes
/// of what it held cold, holding `before` bytes of files resident before it did and `now`
/// after: lowered by its files it let go, which are room, no more than it gave back. A give-
/// back is mostly its guest's page cache, its host's anonymous memory, whose going is room
/// already: taken off its files too, 144 MiB given back made room of RAM its host had taken,
/// and a lone microVM was told room until it ran away (PM M318).
fn given_back(most: u64, before: u64, now: u64, freed: u64) -> u64 {
    most.saturating_sub(before.saturating_sub(now).min(freed))
}

/// What the microVMs `held` hold no more of the most each held resident, of what each holds
/// now (`resident`, by VM process), each one's most raised to it ([`Budgets::evicted`]).
fn evicted_of(held: &mut HashMap<u32, Held>, resident: &[(u32, u64)]) -> u64 {
    resident
        .iter()
        .fold(0u64, |sum, &(pid, now)| match held.get_mut(&pid) {
            Some(h) => {
                h.most = h.most.max(now);
                sum.saturating_add(h.most.saturating_sub(now))
            }
            None => sum,
        })
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
    /// The most of files its VM process has held resident since its run began, its guest's
    /// RAM of its template among them, less its files it let go as it gave back: what of it its
    /// host took back under a budget is no room ([`Holding::room_with`]). Counted whole,
    /// as another VM process restored from the same template, which shares its pages,
    /// leaves it as it is.
    most: u64,
}

impl Held {
    /// VM process `pid`, its run begun in cgroup `dir`, followed from now: what it holds
    /// resident of files read at once, before a budget set later takes any of it back.
    /// Read only once a budget was told, a budget set a mebibyte short of a lone microVM
    /// took its template's pages back as it was written, which then counted as room: 87 MiB
    /// told where none was (budget E2E 40118457230's e, PM M318).
    fn of(dir: PathBuf, pid: u32) -> Held {
        Held {
            dir,
            told: None,
            short: false,
            telling: false,
            again: false,
            most: resident_file(pid).unwrap_or(0),
        }
    }
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
                let _ = warm.vm.kill(shards_ipc::signal::SIGKILL);
                ended += 1;
            }
        }
        ended
    }

    /// Tells each microVM what its budget leaves it as something changes it: a run begun,
    /// a budget set or changed, or met, a microVM held to it gone.
    fn watch_budgets<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        loop {
            // While a microVM is held short of its budget, its room is told it again each
            // window: a guest held within the least room a run goes on in asks for nothing
            // its host can hear, and a budget it is held under is not met.
            let short: Vec<u32> = lock(&self.budgets.held)
                .iter()
                .filter(|(_, h)| h.short)
                .map(|(&pid, _)| pid)
                .collect();
            let woke = {
                let mut woke = lock(&WOKE);
                while woke.fired.is_empty() && woke.ran.is_empty() && woke.left.is_empty() {
                    if short.is_empty() {
                        woke = WAKE.wait(woke).unwrap_or_else(PoisonError::into_inner);
                    } else {
                        let (w, waited) = WAKE
                            .wait_timeout(woke, shards_vmm::platform::PRESSURE_WINDOW)
                            .unwrap_or_else(PoisonError::into_inner);
                        woke = w;
                        if waited.timed_out() {
                            break;
                        }
                    }
                }
                std::mem::take(&mut *woke)
            };
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            let mut tell: HashSet<u32> = short.into_iter().collect();
            for pid in woke.ran {
                if let Some(dir) = cgroup_of(pid) {
                    self.watch_chain(&dir);
                    let followed = Held::of(dir, pid);
                    lock(&self.budgets.held).entry(pid).or_insert(followed);
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
                    // Read from the statistics alone, at as many events as the kernel
                    // sends (one each 10 ms at most, cgroup_file_notify): what its
                    // processes map but its cgroup is not charged for is counted as the
                    // microVM is told.
                    match met
                        .iter()
                        .filter(|l| h.dir.starts_with(l))
                        .filter_map(|l| held_at(l).map(|held| held.room(0)))
                        .min()
                    {
                        // Fallen short, or with room again: told either way.
                        Some(r) if (r < 0) != h.short => {
                            tell.insert(pid);
                        }
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
        if held_at(dir).is_some() && lock(&self.budgets.watched).events.insert(dir.to_path_buf()) {
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

    /// Tells microVM `pid` the room its budget leaves it now, on a thread of its own; once
    /// more after a word under way, which another change came during.
    pub(super) fn tell_room<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, pid: u32) {
        {
            let mut held = lock(&self.budgets.held);
            let Some(h) = held.get_mut(&pid) else { return };
            // One under no budget, never told one, has nothing to hear.
            if h.told.is_none() && !h.telling && !budgeted(&h.dir, Path::new(ROOT)) {
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

    /// What the cgroup `level`, which holds a budget, holds that its host may take back from
    /// its processes alone, as its budget's word says it ([`Holding::held`]): none where it
    /// has no budget.
    fn held_now(&self, level: &Path) -> Option<u64> {
        let h = held_at(level)?;
        let mapped = mapped_under(level);
        let files = mapped.iter().fold(0u64, |sum, &(_, b)| sum.saturating_add(b));
        Some(h.held(files.saturating_add(self.budgets.evicted(&mapped))))
    }

    /// Tells microVM `pid` the room its budget leaves it now, through one of its runs
    /// (init's `ROOM`), which holds its run within it. Where the budget is short, the warm
    /// VMs it holds go first, which are speculation; then the microVM gives back what it
    /// holds cold (`RECLAIM`), which an idle one over its budget, asking for nothing, never
    /// would at its host's pressure; and then it is told the room as the budget has it
    /// after: where it is short still, its run held within what it uses and the least room
    /// a run goes on in, which the next word to it raises once the budget has room again.
    /// What it is held within after a give-back its budget asked for is said, whatever the
    /// give-back counted: one whose answer came too late, its guest having given back as
    /// it was ended, left its microVM within a budget nothing said (PM M391). Nothing where
    /// it has no budget and was told of none.
    fn tell_room_now(&self, pid: u32) {
        let Some(dir) = lock(&self.budgets.held).get(&pid).map(|h| h.dir.clone()) else {
            return;
        };
        let Some((id, vm)) = self.run_of_vm(pid) else {
            return;
        };
        let (had, was_short) = lock(&self.budgets.held)
            .get(&pid)
            .map_or((None, false), |h| (h.told, h.short));
        let evicted = &mut |mapped: &Mapped| self.budgets.evicted(mapped);
        let mut room = room(&dir, Path::new(ROOT), evicted);
        let (mut ended, mut gave, mut asked, mut less) = (0, None, false, 0);
        if let Some((r, level)) = room.clone()
            && r < 0
        {
            ended = self.end_warm_under(&level);
            if ended > 0 {
                room = self::room(&dir, Path::new(ROOT), evicted);
            }
            if room.as_ref().is_some_and(|(r, _)| *r < 0) && lock(self.watching_giving()).insert(vm.clone()) {
                asked = true;
                let before = resident_file(pid);
                let held_before = room.as_ref().and_then(|(_, level)| self.held_now(level));
                gave = self.give_back_of(&id);
                lock(self.watching_giving()).remove(&vm);
                // Said with its budget's word below, its own: not its host's shortage,
                // which says what its pressure had given back at most once a minute.
                if let Some(g) = gave {
                    self.budgets.gave_back(pid, before, g.freed);
                }
                room = self::room(&dir, Path::new(ROOT), evicted);
                // With no answer, what it gave back as its host counts it: what it holds
                // less. One slowed by its host past the deadline went on giving back as its
                // exec was ended, and its word said it gave back nothing as what it held fell
                // from 824 MiB to 514 (CI 41167803629).
                if gave.is_none() {
                    let held_after = room.as_ref().and_then(|(_, level)| self.held_now(level));
                    less = held_before
                        .zip(held_after)
                        .map_or(0, |(b, a)| b.saturating_sub(a));
                }
            }
        }
        if room.is_none() && had.is_none() {
            return;
        }
        let told = room.as_ref().map(|(r, _)| *r);
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::ROOM,
            argv: told.map(|r| r.to_string().into_bytes()).into_iter().collect(),
            ..Default::default()
        };
        // What it is held within past what it uses, as its init answers (none with no
        // budget): never less than the least room a run goes on in, however short.
        let held = match self.exec_quietly(&id, &spec, None, 1 << 12, TAKE_TIMEOUT) {
            Ok(q) if q.status == Some(0) && !q.timed_out => std::str::from_utf8(&q.output)
                .ok()
                .and_then(|o| o.trim().parse::<u64>().ok()),
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
        };
        let short = told.is_some_and(|r| r < 0);
        if let Some(h) = lock(&self.budgets.held).get_mut(&pid) {
            h.told = told;
            h.short = short;
        }
        // Said as it is first held, as its budget falls short and as it has room again.
        let parts = || {
            room.as_ref()
                .and_then(|(_, level)| held_at(level).map(|h| (level, h)))
                .map_or(String::new(), |(level, h)| {
                    let mapped = mapped_under(level);
                    let files = mapped.iter().fold(0u64, |sum, &(_, b)| sum.saturating_add(b));
                    let evicted = self.budgets.evicted(&mapped);
                    format!(
                        ": {}'s budget, {} MiB, holds {} MiB its host may take back from its processes alone, {} MiB of it files they map{}",
                        level.display(),
                        h.budget >> 20,
                        h.held(files.saturating_add(evicted)) >> 20,
                        h.mapped.max(files) >> 20,
                        if evicted >> 20 == 0 {
                            String::new()
                        } else {
                            format!(
                                " and {} MiB its host took back of what they mapped, which their guests read again",
                                evicted >> 20
                            )
                        }
                    )
                })
        };
        // Where the room its budget leaves is short of the least room a run goes on in,
        // what it is held within past what it uses.
        let least = |r: i64| match held {
            Some(h) if h > r.max(0).unsigned_abs() => format!(
                ": held within {} KiB past what it uses, the least room a run goes on in",
                h >> 10
            ),
            _ => String::new(),
        };
        // What its budget falling short had it do first, said with its word: the warm VMs
        // it ended, what it gave back of what it held cold and what it asked to unplug, a
        // MiB or more of either.
        let (freed, unplugging) = gave.map_or((0, 0), |g| (g.freed >> 20, g.unplugging >> 20));
        let less = less >> 20;
        let did = ended > 0 || freed > 0 || unplugging > 0 || less > 0;
        let done = if gave.is_none() && less > 0 {
            format!(
                ", past {ended} warm VMs ended and {less} MiB its host holds less of it as it gave back what it held cold, with no answer"
            )
        } else {
            format!(
                ", past {ended} warm VMs ended and {freed} MiB it gave back of what it held cold of its files, asking to unplug {unplugging} MiB"
            )
        };
        let done_if = || if did { done.as_str() } else { "" };
        match (had, told) {
            // As it is first held, and as it is held within it again by what it did or asked.
            (_, Some(r)) if r >= 0 && !was_short && (had.is_none() || did || asked) => log(format!(
                "container {id}: its microVM held within its budget, {} MiB of room{}{}{}",
                r >> 20,
                done_if(),
                least(r),
                parts()
            )),
            (_, Some(r)) if r < 0 && (!was_short || did) => log(format!(
                "container {id}: its microVM's budget is short by {} MiB{done}{}{}",
                r.unsigned_abs() >> 20,
                least(r),
                parts()
            )),
            (_, Some(r)) if r >= 0 && was_short => log(format!(
                "container {id}: its microVM's budget has room again, {} MiB{}{}",
                r >> 20,
                done_if(),
                parts()
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
        if b.starts_with(at) && held_at(at).is_some() {
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
        // 384 MiB less what it holds but its 32 MiB of page cache no process maps, and less
        // the inactive pages its kernel keeps beside the 117 MiB processes map: short.
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some(((384 - (383 - 32) - 117) as i64 * MIB as i64, slice.clone()))
        );
        // Its shared memory is not page cache its kernel may drop.
        cgroup(
            &slice,
            Some(384 * MIB),
            None,
            383 * MIB,
            &stat(149 * MIB, 117 * MIB, 32 * MIB),
        );
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some(((1 - 117) * MIB as i64, slice.clone()))
        );
        // A hard limit under the high counts, and short is below none.
        cgroup(
            &slice,
            Some(384 * MIB),
            Some(300 * MIB),
            383 * MIB,
            &stat(0, 0, 0),
        );
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some((-83 * MIB as i64, slice.clone()))
        );
        // The least of two: the scope's own, tighter than its slice's.
        cgroup(&scope, Some(350 * MIB), None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some((-83 * MIB as i64, slice.clone()))
        );
        cgroup(&scope, Some(250 * MIB), None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some((-133 * MIB as i64, scope.clone()))
        );
        // Statistics read behind the counter, still counting a gigabyte of page cache the
        // kernel has just taken back: what the processes hold is counted from them, never
        // all the budget for room.
        let stale = "anon 210000000\nfile 1100000000\nkernel 10000000\nfile_mapped 100000000\nshmem 0\n";
        cgroup(&scope, None, None, 383 * MIB, stale);
        cgroup(&slice, Some(384 * MIB), None, 383 * MIB, stale);
        assert_eq!(
            room(&scope, &root, &mut |_| 0),
            Some((384 * MIB as i64 - 320_000_000 - 100_000_000, slice.clone()))
        );
        // None held to a budget: no room to tell.
        cgroup(&scope, None, None, 383 * MIB, &stat(0, 0, 0));
        cgroup(&slice, None, None, 383 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root, &mut |_| 0), None);
        // The root itself holds nothing to a budget.
        cgroup(&root, Some(MIB), None, 900 * MIB, &stat(0, 0, 0));
        assert_eq!(room(&scope, &root, &mut |_| 0), None);
    }

    /// The files a budget's processes map count as held, wherever their pages are charged:
    /// more than the cgroup counts mapped, they take room from it; no more, none.
    #[test]
    fn the_files_a_budgets_processes_map_are_held() {
        let held = Holding {
            budget: 384 * MIB,
            own: 145 * MIB,
            mapped: 16 * MIB,
            current: 383 * MIB,
            floor: 168 * MIB,
        };
        assert_eq!(held.held(0), 168 * MIB);
        assert_eq!(held.held(83 * MIB), 235 * MIB);
        // Less the 16 MiB of inactive pages beside its 16 MiB mapped.
        assert_eq!(held.room(83 * MIB), 133 * MIB as i64);
        assert_eq!(held.room(0), 200 * MIB as i64);
        // Statistics behind its counter, still counting a microVM that has ended: no more
        // held of what is charged than it holds.
        let ended = Holding {
            budget: 384 * MIB,
            own: 881 * MIB,
            mapped: 107 * MIB,
            current: 265 * MIB,
            floor: 179 * MIB,
        };
        assert_eq!(ended.held(136 * MIB), 294 * MIB);
        assert_eq!(lru_buffer(16 * MIB), 16 * MIB);
        // A GiB and more of them: the square root of ten times their GiB.
        assert_eq!(lru_buffer(1 << 30), (1 << 30) / 4);
        assert_eq!(lru_buffer(8 << 30), (8 << 30) / 12);
        // A process of the cgroup's maps its own program at least: this test's, on a host
        // whose processes have /proc entries.
        if std::path::Path::new("/proc/self/smaps_rollup").exists() {
            let tmp = shards_testdir::TempDir::new("budget-files").unwrap();
            let scope = tmp.join("shards-1-0.scope");
            std::fs::create_dir_all(&scope).unwrap();
            std::fs::write(scope.join("cgroup.procs"), format!("{}\n", std::process::id())).unwrap();
            assert!(mapped_under(&tmp).iter().any(|&(_, b)| b > 0));
        }
    }

    /// A guest's RAM its host took back under a budget is no room: a lone microVM's 71 MiB
    /// of its template's pages, taken back where its budget was 23 MiB short, left its
    /// budget as short as before, where counted as room they had told its guest 111 MiB
    /// (PM M318). Its own memory and the files its processes map read after the pages went.
    #[test]
    fn a_guests_ram_its_host_took_back_is_no_room() {
        let before = Holding {
            budget: 275 * MIB,
            own: 75 * MIB,
            mapped: 123 * MIB,
            current: 274 * MIB,
            floor: 198 * MIB,
        };
        let short = before.room(123 * MIB);
        assert!(short < 0, "short before: {short}");
        let after = Holding {
            mapped: 52 * MIB,
            current: 203 * MIB,
            floor: 127 * MIB,
            ..before
        };
        assert!(
            after.room(52 * MIB) > 0,
            "counted as room, its RAM taken back made some"
        );
        assert_eq!(after.room_with(52 * MIB, 71 * MIB), short);
        assert_eq!(before.room_with(123 * MIB, 0), short);
    }

    /// The most a microVM followed has held resident is kept as what it holds falls, raised
    /// as it rises, and lowered by what it gives back; a process not followed counts
    /// nothing.
    #[test]
    fn a_microvms_most_resident_is_kept_until_it_gives_back() {
        let budgets = Budgets::default();
        lock(&budgets.held).insert(
            7,
            Held {
                dir: PathBuf::from("/sys/fs/cgroup/a.slice"),
                told: None,
                short: false,
                telling: false,
                again: false,
                most: 0,
            },
        );
        let evicted = |resident: &[(u32, u64)]| evicted_of(&mut lock(&budgets.held), resident);
        assert_eq!(evicted(&[(7, 100 * MIB), (8, 50 * MIB)]), 0);
        assert_eq!(evicted(&[(7, 60 * MIB)]), 40 * MIB);
        assert_eq!(evicted(&[(7, 120 * MIB)]), 0);
        assert_eq!(evicted(&[(7, 60 * MIB)]), 60 * MIB);
        // A give-back lowers it by the files let go as it gave back, no more than it gave
        // back; what it gave back of its anonymous memory lowers it none.
        assert_eq!(given_back(120 * MIB, 60 * MIB, 40 * MIB, 30 * MIB), 100 * MIB);
        assert_eq!(given_back(120 * MIB, 60 * MIB, 50 * MIB, 30 * MIB), 110 * MIB);
        assert_eq!(given_back(120 * MIB, 60 * MIB, 60 * MIB, 144 * MIB), 120 * MIB);
        assert_eq!(given_back(120 * MIB, 60 * MIB, 70 * MIB, 30 * MIB), 120 * MIB);
        assert_eq!(given_back(10 * MIB, 60 * MIB, 0, u64::MAX), 0);
        // Files let go past what it gave back went otherwise: taken back, no room.
        assert_eq!(given_back(120 * MIB, 80 * MIB, 40 * MIB, 30 * MIB), 90 * MIB);
        budgets.gave_back(9, Some(MIB), MIB);
        // Its own process, on a host whose processes have /proc entries: followed with what
        // it holds resident of files read at once, and its most raised to it at a look. (Not
        // that none goes between two looks: the test binary's other tests, beside it, unmap
        // files as they run.)
        if std::path::Path::new("/proc/self/status").exists() {
            let me = std::process::id();
            assert!(Held::of(PathBuf::from("/sys/fs/cgroup/a.slice"), me).most > 0);
            let followed = lock(&budgets.held).remove(&7).unwrap();
            lock(&budgets.held).insert(me, followed);
            lock(&budgets.held).get_mut(&me).unwrap().most = 0;
            budgets.evicted(&[(me, 0)]);
            assert!(
                lock(&budgets.held)[&me].most > 0,
                "its most raised to what it holds"
            );
        }
    }

    /// A budget's room hears of each process under it what it maps, by process: this
    /// test's own, on a host whose processes have /proc entries.
    #[test]
    fn a_budgets_room_hears_what_each_process_maps() {
        if !std::path::Path::new("/proc/self/smaps_rollup").exists() {
            return;
        }
        let tmp = shards_testdir::TempDir::new("budget-evicted").unwrap();
        let root = tmp.join("cg");
        let scope = root.join("a.slice");
        let stat = "anon 1\nfile 0\nkernel 2\nfile_mapped 0\nshmem 0\n";
        cgroup(&scope, Some(384 * MIB), None, 100 * MIB, stat);
        std::fs::write(scope.join("cgroup.procs"), format!("{}\n", std::process::id())).unwrap();
        let mut heard = Vec::new();
        let taken = room(&scope, &root, &mut |mapped| {
            heard = mapped.to_vec();
            10 * MIB
        });
        assert!(
            heard.iter().any(|&(pid, b)| pid == std::process::id() && b > 0),
            "{heard:?}"
        );
        let files = heard.iter().map(|&(_, b)| b).sum::<u64>();
        let held = held_at(&scope).unwrap();
        assert_eq!(taken, Some((held.room_with(files, 10 * MIB), scope.clone())));
        // The 10 MiB taken back held, and as many inactive pages kept beside them.
        assert_eq!(
            held.room_with(files, 10 * MIB),
            held.room_with(files, 0) - 20 * MIB as i64
        );
    }
}
