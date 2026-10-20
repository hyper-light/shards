//! What a host short of memory gets back from its microVMs (docs/design/architecture.md
//! D188): each time its kernel says it is short ([`MemoryPressure`]), every running microVM
//! reclaims what it holds cold of its files and reports it free at once, which gives it back
//! to the host (D151), as a container's page cache is its host's to drop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_vmm::platform::{MemoryPressure, PressureWatches};

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
            .spawn_scoped(threads, move || self.watch_pressure(pressure))
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
    fn watch_pressure(&self, mut pressure: MemoryPressure) {
        // What was given back since it was last said: since when, how much of files and of
        // memory plugged, in how many passes.
        let mut unsaid: Option<(Instant, Gave, u32)> = None;
        let mut said: Option<Instant> = None;
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
            let gave = self.give_back_cold();
            if gave.freed > 0 || gave.unplugging > 0 {
                let (since, total, passes) = unsaid.unwrap_or((Instant::now(), Gave::default(), 0));
                unsaid = Some((since, total.and(gave), passes.saturating_add(1)));
            }
            if let Some((since, total, passes)) = unsaid
                && said.is_none_or(|at| at.elapsed() >= SAY_EVERY)
            {
                log(format!(
                    "the host is short of memory: its microVMs gave back {} MiB they held cold of their files, and asked to unplug {} MiB they held plugged past twice what they use and their peak since, in {passes} passes over {:?}",
                    total.freed >> 20,
                    total.unplugging >> 20,
                    since.elapsed()
                ));
                said = Some(Instant::now());
                unsaid = None;
            }
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
    /// grows (M275), one after another, each through one run of it (its own, else a
    /// joiner's, D119): what they gave, all together.
    fn give_back_cold(&self) -> Gave {
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
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::RECLAIM,
            ..Default::default()
        };
        let mut gave = Gave::default();
        for id in vms.values() {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            match self.exec_quietly(id, &spec, None, 64, TAKE_TIMEOUT) {
                Ok(q) if q.status == Some(0) => {
                    let said = String::from_utf8_lossy(&q.output);
                    let mut amounts = said.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
                    gave = gave.and(Gave {
                        freed: amounts.next().unwrap_or(0),
                        unplugging: amounts.next().unwrap_or(0),
                    });
                }
                Ok(q) => log(format!(
                    "container {id}: giving back what it holds cold: {}",
                    String::from_utf8_lossy(&q.output).trim()
                )),
                // One that ended meanwhile has nothing to give back.
                Err(_) if !self.running(id) => {}
                Err(e) => log(format!("container {id}: giving back what it holds cold: {e}")),
            }
        }
        gave
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
