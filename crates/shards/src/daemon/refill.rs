//! Starting warm VMs outside the pools' lock (review 7.8, 7.22). What a pool needs is
//! planned under the lock and counted as starting at once, so that no other claim or
//! refill starts it again, then started without the lock: a VM's start spawns its process
//! and its network process, milliseconds (PM M26) that no other claim should wait for. A
//! claim starts its own run's VM on its own thread; the VMs a pool keeps ahead of its runs
//! are started by one refiller thread, which takes every pool asked for meanwhile at once,
//! each once.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use super::{Daemon, For, MAX_FAILURES, State, Threads, lock, log};
use crate::containers::Disk;

/// Warm VMs of one pool to start, counted as starting already
/// ([`Daemon::plan_refill`]), for [`Daemon::start_planned`] to start.
pub(super) struct Planned {
    dir: PathBuf,
    args: Vec<OsString>,
    /// Its template's network device's MAC, if read already.
    net: Option<Option<[u8; 6]>>,
    count: usize,
}

/// What the refiller's thread is asked for.
#[derive(Default)]
pub(super) struct Refills {
    asked: Mutex<Asked>,
    queued: Condvar,
    started: AtomicBool,
    /// The thread is to return: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

#[derive(Default)]
struct Asked {
    pools: HashSet<PathBuf>,
    /// A spare container, for the next run to take.
    spare: bool,
}

impl Refills {
    /// Ends the refiller's thread once nothing is asked of it: a test's daemon's.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.asked);
        self.queued.notify_all();
    }
}

impl<D: Disk> Daemon<D> {
    /// Has `pool` refilled, if given, and a spare container made, on the refiller's
    /// thread, which the first ask starts. Waits for nothing: its callers may hold the
    /// pools' lock.
    pub(super) fn refill_soon<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, pool: Option<&Path>) {
        let r = &self.refills;
        {
            let mut asked = lock(&r.asked);
            if let Some(dir) = pool {
                asked.pools.insert(dir.to_path_buf());
            }
            asked.spare = true;
        }
        r.queued.notify_one();
        if !r.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("refiller".into())
                .spawn_scoped(threads, move || self.refill_all(threads))
        {
            r.started.store(false, Ordering::SeqCst);
            log(format!(
                "the refiller's thread: {e}; pools start VMs only as their runs claim them"
            ));
        }
    }

    /// Refills the pools asked for, each once however often it was asked meanwhile, then
    /// makes the spare, until [`Refills::end`].
    fn refill_all<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let r = &self.refills;
        loop {
            let asked = {
                let mut asked = lock(&r.asked);
                while asked.pools.is_empty() && !asked.spare && !r.ended.load(Ordering::SeqCst) {
                    asked = r.queued.wait(asked).unwrap_or_else(PoisonError::into_inner);
                }
                if asked.pools.is_empty() && !asked.spare {
                    return;
                }
                std::mem::take(&mut *asked)
            };
            for dir in &asked.pools {
                let planned = self.plan_refill(&mut lock(&self.state), dir, true);
                if let Some(planned) = planned {
                    let _ = self.start_planned(threads, planned);
                }
            }
            if asked.spare {
                self.make_spare();
            }
        }
    }

    /// Plans the warm VMs of the template in `dir` to start: one for each run waiting
    /// that none is starting for, and with `ahead`, more until its pool will hold its
    /// target, as far as `warm_max` allows all pools together, after evicting the ready
    /// VMs of the pools least recently claimed from. Those planned count as starting at
    /// once.
    pub(super) fn plan_refill(&self, state: &mut State, dir: &Path, ahead: bool) -> Option<Planned> {
        // A template collected is restored no more.
        if !shards_vmm::snapshot::exists(dir) {
            state.pools.remove(dir);
            return None;
        }
        let (for_runs, more) = {
            let pool = state.pools.entry(dir.to_path_buf()).or_default();
            if pool.failures >= MAX_FAILURES {
                return None;
            }
            let now = Instant::now();
            pool.demand.begin(now);
            let for_runs = pool.waiting.saturating_sub(pool.starting);
            let more = if ahead {
                let target = pool.demand.target(now, self.target, self.keep);
                target.saturating_sub(pool.ready.len() + pool.starting + for_runs)
            } else {
                0
            };
            (for_runs, more)
        };
        let more = more.min(self.room(state, dir, more));
        let count = for_runs + more;
        let pool = state.pools.get_mut(dir)?;
        if count == 0 {
            return None;
        }
        pool.starting += count;
        // It writes no file of the home: the daemon gives it its container's log (D30).
        let mut args: Vec<OsString> = vec!["restore".into(), dir.into(), "--warm".into(), "3".into()];
        if let Some(rootfs) = &pool.rootfs {
            let mut backing = rootfs.as_os_str().to_os_string();
            backing.push(":ro");
            args.extend(["--backing".into(), backing]);
        }
        Some(Planned {
            dir: dir.to_path_buf(),
            args,
            net: pool.net,
            count,
        })
    }

    /// Starts the warm VMs [`plan_refill`](Self::plan_refill) planned, outside the pools'
    /// lock. Those that do not start count as starting no more, and the claims waiting are
    /// told: not the template's failure, as a VM failing to restore it is, but the
    /// host's, which saving the template again would not mend.
    pub(super) fn start_planned<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        planned: Planned,
    ) -> Result<(), String> {
        let Planned {
            dir,
            args,
            net,
            count,
        } = planned;
        // A template that cannot be read gets no network process: its VM then fails to
        // restore it, as any broken template's does, and the claim finds it broken.
        let net = match net {
            Some(net) => net,
            None => match shards_vmm::snapshot::net(&dir) {
                Ok(net) => {
                    if let Some(pool) = lock(&self.state).pools.get_mut(&dir) {
                        pool.net = Some(net);
                    }
                    net
                }
                Err(e) => {
                    log(format!("reading the template {}: {e}", dir.display()));
                    None
                }
            },
        };
        for started in 0..count {
            if let Err(e) = self.start(threads, &args, net, For::Pool(dir.clone())) {
                let e = format!("starting a warm VM of {}: {e}", dir.display());
                log(&e);
                if let Some(pool) = lock(&self.state).pools.get_mut(&dir) {
                    pool.starting = pool.starting.saturating_sub(count - started);
                }
                self.changed.notify_all();
                return Err(e);
            }
        }
        Ok(())
    }
}
