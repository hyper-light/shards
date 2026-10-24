//! Starting warm VMs outside the pools' lock (review 7.8, 7.22). What a pool needs is
//! planned under the lock and counted as starting at once, so that no other claim or
//! refill starts it again, then started without the lock: a VM's start spawns its process
//! and its network process, milliseconds (PM M26) that no other claim should wait for. A
//! claim starts its own run's VM on its own thread; the VMs a pool keeps ahead of its runs
//! are started by one refiller thread, which takes every pool asked for meanwhile at once,
//! each once.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Instant;

use super::{Daemon, For, MAX_FAILURES, PoolKey, State, Threads, lock, log, resolved};
use crate::containers::Disk;

/// Warm VMs of one pool to start, counted as starting already
/// ([`Daemon::plan_refill`]), for [`Daemon::start_planned`] to start.
pub(super) struct Planned {
    key: PoolKey,
    pub(super) args: Vec<OsString>,
    /// Its template's network device's MAC, if read already.
    net: Option<Option<[u8; 6]>>,
    /// Whether the most bytes its working sets may take has been read.
    limited: bool,
    count: usize,
}

/// What the refiller's thread is asked for.
#[derive(Default)]
pub(super) struct Refills {
    asked: Mutex<Asked>,
    queued: Condvar,
    pub(super) started: AtomicBool,
    /// The thread is to return: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

#[derive(Default)]
struct Asked {
    pools: HashSet<PoolKey>,
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
    /// pools' lock. A daemon that has closed starts none: no run will claim it, and it
    /// would end with the daemon as the next one starts.
    pub(super) fn refill_soon<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, pool: Option<&PoolKey>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let r = &self.refills;
        {
            let mut asked = lock(&r.asked);
            if let Some(key) = pool {
                asked.pools.insert(key.clone());
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
            for key in &asked.pools {
                let planned = self.plan_refill(&mut lock(&self.state), key, true);
                if let Some(planned) = planned {
                    let _ = self.start_planned(threads, planned);
                }
            }
            if asked.spare {
                self.make_spare();
            }
        }
    }

    /// Plans the warm VMs of pool `key` to start: one for each run waiting that none is
    /// starting for, and with `ahead`, more until its pool will hold its target, as far as
    /// `warm_max` allows all pools together, after evicting the ready VMs of the pools
    /// least recently claimed from. Those planned count as starting at once.
    pub(super) fn plan_refill(&self, state: &mut State, key: &PoolKey, ahead: bool) -> Option<Planned> {
        let dir = key.dir.as_path();
        // A template collected is restored no more, and its files are held no more.
        if !shards_vmm::snapshot::exists(dir) {
            state.pools.remove(key);
            self.let_go_template(dir);
            return None;
        }
        let (for_runs, more) = {
            let pool = state.pools.entry(key.clone()).or_default();
            if pool.failures >= MAX_FAILURES {
                return None;
            }
            // A pool forgotten (age_pools) before its refill comes is made again here,
            // knowing its root filesystem only as its template records it. Its VMs are
            // given that or none start: the file a restore names is never what the VM
            // saving the template happened to write.
            if pool.rootfs.is_none() {
                pool.rootfs = crate::run::Origin::read(dir).map(|o| resolved(o.rootfs()));
            }
            if pool.rootfs.is_none() {
                log(format!(
                    "{}: no root filesystem recorded; no warm VM restores it",
                    dir.display()
                ));
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
        // Under a budget (D188), what its VMs copy ahead of a request is none, as every
        // warm VM would spend it of the budget before any run came; and none is made ahead
        // of the runs that wait where the budget has no room left.
        let budget = self
            .cgroup_of_parent(&key.parent)
            .and_then(|d| super::budget::room(&d, Path::new(super::budget::ROOT)))
            .map(|(room, _)| room);
        let more = if budget.is_some_and(|r| r < 0) { 0 } else { more };
        let more = more.min(self.room(state, key, more));
        let count = for_runs + more;
        let pool = state.pools.get_mut(key)?;
        if count == 0 {
            return None;
        }
        pool.starting += count;
        // It writes no file of the home: the daemon gives it its container's log (D30).
        let mut args: Vec<OsString> = vec!["restore".into(), dir.into(), "--warm".into(), "3".into()];
        if budget.is_some() {
            args.extend(["--prefetch-private".into(), "0".into()]);
        }
        if let Some(rootfs) = &pool.rootfs {
            let mut backing = rootfs.as_os_str().to_os_string();
            backing.push(":ro");
            args.extend(["--backing".into(), backing]);
        }
        // An Agentfile's template restores against the in-VM server's device too (D60),
        // the daemon's own, as its cold boot was given it: without it every restore is
        // refused and each run boots.
        if *pool
            .server
            .get_or_insert_with(|| crate::run::Origin::read(dir).is_some_and(|o| o.server()))
        {
            match crate::guest::server_device(&self.home) {
                Ok(device) => {
                    // As the VM saving the template recorded it: resolved, as its root
                    // filesystem is.
                    let mut backing = resolved(&device).into_os_string();
                    backing.push(":ro");
                    args.extend(["--backing".into(), backing]);
                }
                Err(e) => log(format!("{}: the in-VM server's device: {e}", dir.display())),
            }
        }
        // Another CPU's image's template restores against the emulator image's root too
        // (D179), last, as its cold boot was given it.
        if let Some(root) = pool
            .emulator
            .get_or_insert_with(|| {
                crate::run::Origin::read(dir).and_then(|o| o.emulator().map(Path::to_path_buf))
            })
            .as_ref()
        {
            let mut backing = resolved(root).into_os_string();
            backing.push(":ro");
            args.extend(["--backing".into(), backing]);
        }
        // A GPU's template restores in the GPU's VM process (D150).
        if *pool
            .gpu
            .get_or_insert_with(|| crate::run::Origin::read(dir).is_some_and(|o| o.gpu()))
        {
            args.push("--gpu".into());
        }
        Some(Planned {
            key: key.clone(),
            args,
            net: pool.net,
            limited: pool.working_set_limit.is_some(),
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
            key,
            args,
            net,
            limited,
            count,
        } = planned;
        let dir = key.dir.as_path();
        // A template that cannot be read gets no network process: its VM then fails to
        // restore it, as any broken template's does, and the claim finds it broken.
        let net = match net {
            Some(net) => net,
            None => match shards_vmm::snapshot::net(dir) {
                Ok(net) => {
                    if let Some(pool) = lock(&self.state).pools.get_mut(&key) {
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
        // What its VMs' working sets may take, read once, here, off the followers' loop,
        // which gathers them (files.rs). A template that cannot be read has its VMs' sets
        // refused, as the VMs fail to restore it.
        if !limited {
            let limit = shards_vmm::vm::working_set_limit(dir)
                .map_err(|e| log(format!("reading the template {}: {e}", dir.display())))
                .ok();
            if let Some(pool) = lock(&self.state).pools.get_mut(&key) {
                pool.working_set_limit.get_or_insert(limit);
            }
        }
        // Whether its template keeps a writable layer (D190), read once: each VM is given a
        // file of its own for it, a clone of it where the home's file system makes one.
        let cached = lock(&self.state).pools.get(&key).and_then(|p| p.layer);
        let layered = match cached {
            Some(layered) => layered,
            None => {
                let layered = shards_vmm::snapshot::has_layer(dir)
                    .map_err(|e| log(format!("reading the template {}: {e}", dir.display())))
                    .unwrap_or(false);
                if let Some(pool) = lock(&self.state).pools.get_mut(&key) {
                    pool.layer = Some(layered);
                }
                layered
            }
        };
        for started in 0..count {
            // Its own MAC, made as it is, as dockerd makes each start one (PM M173): its
            // network process frames to it, and its guest takes it as it resumes, before
            // any run comes (D31, E1).
            let started_vm = net
                .map(|_| shards_net::random_mac())
                .transpose()
                .map_err(|e| format!("a warm VM's MAC: {e}"))
                .and_then(|mac| {
                    let layer = layered
                        .then(|| {
                            shards_vmm::snapshot::generation(dir)
                                .and_then(|g| super::layers::LayerFile::for_restore(&self.home, &g))
                        })
                        .transpose()?;
                    self.start(threads, &args, mac, For::Pool(key.clone()), &key.parent, layer)
                });
            if let Err(e) = started_vm {
                let e = format!("starting a warm VM of {}: {e}", dir.display());
                log(&e);
                if let Some(pool) = lock(&self.state).pools.get_mut(&key) {
                    pool.starting = pool.starting.saturating_sub(count - started);
                }
                self.changed.notify_all();
                return Err(e);
            }
        }
        // As its VMs restore, not before them.
        self.hold_template(dir);
        Ok(())
    }

    /// Holds template `dir`'s generation's files open ([`shards_vmm::snapshot::Held`])
    /// where they are not, or are another generation's: a restore of a template whose
    /// files its host had let go of read its whole working set back from the disk a page
    /// at a time, and its guest its programs from its root filesystem, a cold run of 0.3 s
    /// taking 9 (PM M315).
    pub(super) fn hold_template(&self, dir: &Path) {
        if lock(&self.held).get(dir).is_some_and(|h| h.is_current(dir)) {
            return;
        }
        match shards_vmm::snapshot::hold(dir) {
            Ok(held) => {
                lock(&self.held).insert(dir.to_path_buf(), held);
            }
            Err(e) => log(format!("{}: its files are not held open: {e}", dir.display())),
        }
    }

    /// Lets go of template `dir`'s files ([`hold_template`](Self::hold_template)), as it
    /// goes: held, a file removed keeps its blocks until it is closed.
    pub(super) fn let_go_template(&self, dir: &Path) {
        lock(&self.held).remove(dir);
    }
}
