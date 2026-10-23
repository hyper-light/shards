//! What a host short of memory gets back from its microVMs (docs/design/architecture.md
//! D188): each time its kernel says it is short ([`MemoryPressure`]), every running microVM
//! reclaims what it holds cold of its files and reports it free at once, which gives it back
//! to the host (D151), as a container's page cache is its host's to drop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_vmm::platform::{MemoryPressure, PressureWatches};

use super::health::Quiet;
use super::{Daemon, RunState, TAKE_TIMEOUT, Threads, lock, log};
use crate::containers::Disk;

/// What the daemon's memory pressure watcher watches besides the host and its own cgroup:
/// each scope its microVMs are made in under a user's slice (D183), which may hold them to
/// a budget, once.
#[derive(Debug, Default)]
pub(super) struct Watching {
    watches: Mutex<Option<PressureWatches>>,
    /// The spawners whose scopes are watched, by process ID.
    scopes: Mutex<HashSet<u32>>,
    /// The microVMs giving back what they hold cold now, each by its own run's ID: none is
    /// asked again until its last give-back has ended, and none waits on another's, so
    /// that one whose guest cannot answer, stalled on memory its host has taken from it,
    /// keeps no other's from the host (PM M308: a pass that waited on each in turn gave
    /// back 3 times in 600 s).
    giving: Mutex<HashSet<String>>,
    /// What they gave back since it was last said.
    unsaid: Mutex<Unsaid>,
}

/// What microVMs gave back since it was last said: since when, how much of files and of
/// memory plugged, in how many passes; and when it was last said.
#[derive(Debug, Default)]
struct Unsaid {
    since: Option<Instant>,
    gave: Gave,
    passes: u32,
    said: Option<Instant>,
}

/// How often what microVMs gave back is said, at most: a pressure that goes on says it
/// once in this long, with all it gave back since.
const SAY_EVERY: Duration = Duration::from_secs(60);

impl<D: Disk> Daemon<D> {
    /// Watches the host's memory pressure on a thread of its own, where its kernel lets
    /// this process watch it; says what it watches, or why it watches nothing.
    pub(super) fn start_pressure<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let pressure = match MemoryPressure::new() {
            Ok((pressure, watches)) => {
                *lock(&self.watching.watches) = Some(watches);
                pressure
            }
            Err(e) => {
                log(format!(
                    "the host's memory pressure is not watched ({e}): microVMs give back what they hold cold only as their own memory runs short"
                ));
                return;
            }
        };
        log(format!(
            "watching the host's memory pressure: {}",
            pressure.describe()
        ));
        if let Err(e) = std::thread::Builder::new()
            .name("pressure".into())
            .spawn_scoped(threads, move || self.watch_pressure(threads, pressure))
        {
            log(format!(
                "the memory pressure watcher's thread: {e}; nothing is given back at it"
            ));
        }
    }

    /// Has the memory pressure of the scope `group` is the one process of watched as well
    /// (D183): where the user's slice holds its microVMs to a budget, they are short of
    /// memory before their host says so. Once each.
    pub(super) fn watch_scope_of(&self, group: shards_ipc::Group) {
        if !lock(&self.watching.scopes).insert(group.pid()) {
            return;
        }
        let Some(path) = std::fs::read_to_string(format!("/proc/{}/cgroup", group.pid()))
            .ok()
            .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::")).map(str::to_string))
        else {
            return;
        };
        let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        if let Some(watches) = lock(&self.watching.watches).as_ref()
            && let Err(e) = watches.watch(&dir)
        {
            log(format!(
                "the memory pressure of {}: {e}; its microVMs give back what they hold cold at the host's alone",
                dir.display()
            ));
        }
    }

    /// Has the microVMs give back what they hold cold each time the host is short of
    /// memory, until the daemon closes.
    fn watch_pressure<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, mut pressure: MemoryPressure) {
        loop {
            // With no microVM running, none is asked to give back anything: the host's
            // pressure, which its kernel says again each window while it lasts, is not
            // listened to until one runs, so that an idle daemon wakes for nothing. A
            // trigger fired meanwhile is heard as the first runs (psi.c, psi_trigger_poll).
            if !self.await_a_microvm() {
                return;
            }
            if let Err(e) = pressure.wait() {
                log(format!(
                    "watching the host's memory pressure: {e}; nothing more is given back at it"
                ));
                return;
            }
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            self.give_back_cold(threads);
        }
    }

    /// Waits until a microVM runs: true then, false once the daemon closes ([`close`]
    /// wakes this).
    fn await_a_microvm(&self) -> bool {
        let mut runs = lock(&self.runs);
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            if runs.values().any(|r| matches!(r, RunState::Tracked(_))) {
                return true;
            }
            runs = self.resolved.wait(runs).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Has each running microVM give back what it holds cold of its files, and what it
    /// holds plugged past twice what it uses and its peak since the pass before, none as it
    /// grows (M275), each through one run of it (its own, else a joiner's, D119), each on a
    /// thread of its own: but one still giving back since a pass before, which is asked
    /// nothing more until it has answered or been ended.
    pub(super) fn give_back_cold<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let mut vms: HashMap<String, String> = HashMap::new();
        for (id, run) in lock(&self.runs).iter() {
            if let RunState::Tracked(t) = run {
                match &t.joined {
                    None => {
                        vms.insert(id.clone(), id.clone());
                    }
                    Some(provider) => {
                        vms.entry(provider.clone()).or_insert_with(|| id.clone());
                    }
                }
            }
        }
        let mut asked = false;
        for (vm, id) in vms {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            if !lock(&self.watching.giving).insert(vm.clone()) {
                continue;
            }
            asked = true;
            let gone = vm.clone();
            let spawned =
                std::thread::Builder::new()
                    .name("give-back".into())
                    .spawn_scoped(threads, move || {
                        let gave = self.give_back_of(&id);
                        lock(&self.watching.giving).remove(&vm);
                        if let Some(gave) = gave {
                            self.gave(gave);
                        }
                    });
            if let Err(e) = spawned {
                lock(&self.watching.giving).remove(&gone);
                log(format!(
                    "container {gone}: giving back what it holds cold: its thread: {e}"
                ));
            }
        }
        if asked {
            let mut unsaid = lock(&self.watching.unsaid);
            unsaid.since.get_or_insert_with(Instant::now);
            unsaid.passes = unsaid.passes.saturating_add(1);
        }
        self.say_given();
    }

    /// What container `id`'s microVM gave back of what it holds cold, asked through it;
    /// none where it gave nothing back, and why said.
    fn give_back_of(&self, id: &str) -> Option<Gave> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::RECLAIM,
            ..Default::default()
        };
        match self.exec_quietly(id, &spec, None, 64, TAKE_TIMEOUT) {
            Ok(q) => match given(&q) {
                Ok(gave) => Some(gave),
                Err(why) => {
                    log(format!("container {id}: giving back what it holds cold: {why}"));
                    None
                }
            },
            // One that ended meanwhile has nothing to give back.
            Err(_) if !self.running(id) => None,
            Err(e) => {
                log(format!("container {id}: giving back what it holds cold: {e}"));
                None
            }
        }
    }

    /// Counts what a microVM gave back, and says it if it is due.
    fn gave(&self, gave: Gave) {
        {
            let mut unsaid = lock(&self.watching.unsaid);
            unsaid.since.get_or_insert_with(Instant::now);
            unsaid.gave = unsaid.gave.and(gave);
        }
        self.say_given();
    }

    /// Says what microVMs gave back since it was last said, where they gave back anything,
    /// at most once in [`SAY_EVERY`].
    fn say_given(&self) {
        let mut unsaid = lock(&self.watching.unsaid);
        if unsaid.gave.freed == 0 && unsaid.gave.unplugging == 0 {
            return;
        }
        if unsaid.said.is_some_and(|at| at.elapsed() < SAY_EVERY) {
            return;
        }
        log(format!(
            "the host is short of memory: its microVMs gave back {} MiB they held cold of their files, and asked to unplug {} MiB they held plugged past twice what they use and their peak since, in {} passes over {:?}",
            unsaid.gave.freed >> 20,
            unsaid.gave.unplugging >> 20,
            unsaid.passes.max(1),
            unsaid.since.map_or(Duration::ZERO, |at| at.elapsed())
        ));
        *unsaid = Unsaid {
            said: Some(Instant::now()),
            ..Unsaid::default()
        };
    }
}

/// What a microVM's give-back said it gave: the bytes of files its run's cgroup holds less,
/// then the bytes plugged it asked to unplug (init's `RECLAIM`); else why it gave nothing.
fn given(q: &Quiet) -> Result<Gave, String> {
    let said = String::from_utf8_lossy(&q.output);
    let said = said.trim();
    match (q.timed_out, q.status) {
        (true, _) => Err(format!(
            "no answer within {:?}, and so it was ended{}",
            TAKE_TIMEOUT,
            if said.is_empty() {
                String::new()
            } else {
                format!(": {said}")
            }
        )),
        (false, Some(0)) => {
            let mut amounts = said.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
            Ok(Gave {
                freed: amounts.next().unwrap_or(0),
                unplugging: amounts.next().unwrap_or(0),
            })
        }
        (false, Some(code)) if said.is_empty() => Err(format!("it ended with status {code}")),
        (false, Some(_)) => Err(said.to_string()),
        (false, None) => Err("its microVM ended it".into()),
    }
}

/// What microVMs gave back: the bytes of files their runs' cgroups hold less, and the bytes
/// of memory plugged they asked to be unplugged.
#[derive(Debug, Default, Clone, Copy)]
struct Gave {
    freed: u64,
    unplugging: u64,
}

impl Gave {
    fn and(self, other: Gave) -> Gave {
        Gave {
            freed: self.freed.saturating_add(other.freed),
            unplugging: self.unplugging.saturating_add(other.unplugging),
        }
    }
}

#[cfg(test)]
impl Watching {
    /// Whether microVM `vm` is giving back what it holds cold now.
    pub(super) fn giving(&self, vm: &str) -> bool {
        lock(&self.giving).contains(vm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(output: &str, status: Option<i64>, timed_out: bool) -> Quiet {
        Quiet {
            output: output.as_bytes().to_vec(),
            more: false,
            status,
            timed_out,
        }
    }

    /// What init's `RECLAIM` says it gave back is taken; a give-back that had no answer in
    /// time is said to have had none, and was ended (PM M308: it was said as nothing at
    /// all), and one that failed says why.
    #[test]
    fn a_give_back_says_what_it_gave_or_why_it_gave_nothing() {
        let gave = given(&quiet("1048576 2097152\n", Some(0), false)).unwrap();
        assert_eq!((gave.freed, gave.unplugging), (1 << 20, 2 << 20));
        assert_eq!(
            given(&quiet("", None, true)).unwrap_err(),
            "no answer within 10s, and so it was ended"
        );
        assert_eq!(
            given(&quiet("", Some(137), true)).unwrap_err(),
            "no answer within 10s, and so it was ended"
        );
        assert_eq!(
            given(&quiet(
                "memory.reclaim: Device or resource busy\n",
                Some(1),
                false
            ))
            .unwrap_err(),
            "memory.reclaim: Device or resource busy"
        );
        assert_eq!(
            given(&quiet("", Some(1), false)).unwrap_err(),
            "it ended with status 1"
        );
        assert_eq!(
            given(&quiet("", None, false)).unwrap_err(),
            "its microVM ended it"
        );
    }
}
