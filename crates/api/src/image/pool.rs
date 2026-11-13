//! Warm run hosts kept ahead of an orchestrator's runs (D253's pools, D26 through shards-api).
//! The orchestrator owns each microVM's life (D231), so its pools live with it, in this
//! process: a run host started ahead of a run, its microVM restored from its image's
//! template and ready, waits for its run's request, as the daemon's warm VMs wait for `RUN`.
//!
//! - **Its key.** A pool is the runs of one microVM: requests alike in all but their members'
//!   stages, their deadline and their control ask ([`wire::Request::exemplar`]), of one run
//!   host program and environment. Its warm hosts are started with its last run's exemplar,
//!   which carries none of a run's secrets, and each checks the request it is handed against
//!   it, refusing one of another microVM (`Unfit`), whose run then gets a host of its own.
//! - **Its size** is D26's, by the one rule ([`crate::demand`]): the largest burst of its runs
//!   within one refill window, an RFC 6298 retransmission timeout of its warm hosts' starts as
//!   timed here, while it is in use, at least one and at most the orchestrator's bound; none
//!   once idle past its keep-alive. All pools together keep at most that bound.
//! - **Its cold start.** Nothing is warm until a pool's first run, which starts a host of its
//!   own; its first window is RFC 6298's initial RTO, as the daemon's pools' is.
//!
//! One thread keeps the pools: it hands out warm hosts, starts more where a pool wants them
//! (each host's start awaited on a thread of its own), and ends those a pool no longer
//! keeps. The pools end with their owner, every warm or starting host with them.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::wire;
use super::{BootStop, ImageGuest, ImageRefusal, ImageSpec};
use crate::demand::Demand;

/// The run host a pool's hosts are: its program, as [`ImageSpec::binary`] names it, and its
/// environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct HostOf {
    pub(super) binary: Option<PathBuf>,
    pub(super) env: Vec<(OsString, OsString)>,
}

impl HostOf {
    pub(super) fn of(spec: &ImageSpec) -> HostOf {
        HostOf {
            binary: spec.binary.clone(),
            env: spec.env.clone(),
        }
    }
}

/// A pool's key: its run host, and its runs' exemplar as the channel carries it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    host: HostOf,
    exemplar: Vec<u8>,
}

/// Why a warm host did not take a run.
#[derive(Debug)]
pub(super) enum Taken {
    /// Its microVM is another request's (`wire::Cause::Unfit`): the run gets a host of its
    /// own.
    Unfit,
    /// It refused the run, as a host of its own would have.
    Refused(ImageRefusal),
}

/// What starts run hosts: shards' own (`session`), or a test's stand-in.
pub(super) trait Hosts: Clone + Send + Sync + 'static {
    /// A host starting warm, until it is ready.
    type Starting: Send + 'static;
    /// A warm host, its microVM ready for a run; ended as it drops.
    type Warm: Send + 'static;
    /// Ends a host that is starting.
    type Kill: Send + 'static;
    /// Starts a host of `host` warm for runs of `exemplar`: at once, its readiness awaited by
    /// [`warmed`](Self::warmed); and what ends it meanwhile.
    fn begin(
        &self,
        host: &HostOf,
        exemplar: &wire::Request,
    ) -> Result<(Self::Starting, Self::Kill), ImageRefusal>;
    /// Waits for `starting` to be warm.
    fn warmed(&self, starting: Self::Starting) -> Result<Self::Warm, ImageRefusal>;
    /// Ends a host that is starting, as its pool ends.
    fn kill(&self, kill: &Self::Kill);
    /// Hands warm host `warm` the run `request` of `spec`, waiting at most `patience` for its
    /// start, which `stop` ends ([`BootStop`]).
    fn take(
        &self,
        warm: Self::Warm,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, Taken>;
    /// Starts the run `request` on a host of its own, its start ended by `stop`.
    fn fresh(
        &self,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal>;
}

/// What the pools' thread is asked.
enum Ask<W> {
    /// A warm host for a run of `key`, if one is ready; its pool begun with `host` and
    /// `exemplar`, which its later hosts are started with.
    Take {
        key: Key,
        host: HostOf,
        exemplar: Box<wire::Request>,
        reply: mpsc::Sender<Option<W>>,
    },
    /// A host of `key`'s started warm, `took` from its start to its readiness; or why not.
    Warmed {
        key: Key,
        id: u64,
        warm: Result<W, ImageRefusal>,
        took: Duration,
    },
    /// A run of `key` ended: its pool is idle from its last run's end.
    Ended { key: Key },
    /// The pools' owner is gone: every host ends.
    Stop,
    /// How many warm hosts the pools hold ready: a test's view of them.
    #[cfg(test)]
    Ready { reply: mpsc::Sender<usize> },
}

/// One microVM's pool.
struct Pool<W, K> {
    host: HostOf,
    exemplar: wire::Request,
    ready: VecDeque<W>,
    /// The hosts starting, by their start's number, and what ends each.
    starting: HashMap<u64, K>,
    demand: Demand,
}

/// The pools, kept by their thread.
struct Pools<H: Hosts> {
    hosts: H,
    most: usize,
    keep: Duration,
    pools: HashMap<Key, Pool<H::Warm, H::Kill>>,
    asks: mpsc::Sender<Ask<H::Warm>>,
    next: u64,
}

impl<H: Hosts> Pools<H> {
    /// The hosts all pools hold or are starting.
    fn held(&self) -> usize {
        self.pools
            .values()
            .map(|p| p.ready.len() + p.starting.len())
            .sum()
    }

    fn take(&mut self, key: Key, host: HostOf, exemplar: wire::Request, now: Instant) -> Option<H::Warm> {
        let most = self.most;
        let pool = self.pools.entry(key.clone()).or_insert_with(|| Pool {
            host,
            exemplar: exemplar.clone(),
            ready: VecDeque::new(),
            starting: HashMap::new(),
            demand: Demand::default(),
        });
        // Its later hosts take its last run's exemplar: the same microVM's, by its key.
        pool.exemplar = exemplar;
        pool.demand.begin(now);
        pool.demand.claimed(now, most);
        pool.demand.started();
        let warm = pool.ready.pop_front();
        self.refill(&key, now);
        warm
    }

    fn warmed(
        &mut self,
        key: &Key,
        id: u64,
        warm: Result<H::Warm, ImageRefusal>,
        took: Duration,
        now: Instant,
    ) {
        let Some(pool) = self.pools.get_mut(key) else {
            return;
        };
        if pool.starting.remove(&id).is_none() {
            return;
        }
        match warm {
            Ok(warm) => {
                pool.demand.refilled(took);
                let target = pool.demand.target(now, self.most, self.keep);
                if pool.ready.len() < target {
                    pool.ready.push_back(warm);
                }
            }
            // One that does not start is no warm host: its runs get hosts of their own, and
            // its pool starts no more until its next run asks.
            Err(_) => return,
        }
        self.refill(key, now);
    }

    fn ended(&mut self, key: &Key, now: Instant) {
        if let Some(pool) = self.pools.get_mut(key) {
            pool.demand.ended(now);
        }
    }

    /// Starts the hosts pool `key` wants to keep the target its demand says, as far as all
    /// pools together may hold.
    fn refill(&mut self, key: &Key, now: Instant) {
        let held = self.held();
        let (most, keep) = (self.most, self.keep);
        let Some(pool) = self.pools.get_mut(key) else {
            return;
        };
        let target = pool.demand.target(now, most, keep);
        let want = target
            .saturating_sub(pool.ready.len() + pool.starting.len())
            .min(most.saturating_sub(held));
        for _ in 0..want {
            let id = self.next;
            self.next += 1;
            let Ok((starting, kill)) = self.hosts.begin(&pool.host, &pool.exemplar) else {
                return;
            };
            pool.starting.insert(id, kill);
            let (hosts, asks, key) = (self.hosts.clone(), self.asks.clone(), key.clone());
            let began = Instant::now();
            let waiting = std::thread::Builder::new()
                .name("shards-api-image-warm".into())
                .spawn(move || {
                    let warm = hosts.warmed(starting);
                    let took = began.elapsed();
                    // A pool gone: the host ends as its value drops.
                    let _ = asks.send(Ask::Warmed { key, id, warm, took });
                });
            if waiting.is_err() {
                if let Some(kill) = pool.starting.remove(&id) {
                    self.hosts.kill(&kill);
                }
                return;
            }
        }
    }

    /// Ends the warm hosts each pool keeps no more at `now`: past its target, the last
    /// started first; every one past its keep-alive. A pool idle with nothing left goes.
    fn expire(&mut self, now: Instant) {
        let (most, keep) = (self.most, self.keep);
        self.pools.retain(|_, pool| {
            let target = pool.demand.target(now, most, keep);
            while pool.ready.len() > target {
                drop(pool.ready.pop_back());
            }
            !(pool.ready.is_empty() && pool.starting.is_empty() && pool.demand.expired(now, keep))
        });
    }

    /// When the next pool's keep-alive ends, from which it keeps nothing: none where no pool
    /// keeps a host.
    fn next_expiry(&self) -> Option<Instant> {
        self.pools
            .values()
            .filter(|p| !p.ready.is_empty() && !p.demand.going())
            .filter_map(|p| p.demand.last())
            .map(|last| last + self.keep)
            .min()
    }

    /// Ends every host: those warm as they drop, those starting killed.
    fn stop(&mut self) {
        for (_, pool) in self.pools.drain() {
            for kill in pool.starting.values() {
                self.hosts.kill(kill);
            }
            drop(pool.ready);
        }
    }
}

/// The pools' thread: their asks, and the keep-alives ending, until their owner is gone.
fn serve<H: Hosts>(mut pools: Pools<H>, asks: mpsc::Receiver<Ask<H::Warm>>) {
    loop {
        let ask = match pools.next_expiry() {
            Some(at) => match asks.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(ask) => Some(ask),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => Some(Ask::Stop),
            },
            None => Some(asks.recv().unwrap_or(Ask::Stop)),
        };
        let now = Instant::now();
        match ask {
            None => pools.expire(now),
            Some(Ask::Take {
                key,
                host,
                exemplar,
                reply,
            }) => {
                let warm = pools.take(key, host, *exemplar, now);
                // Its asker gone: the host ends as its value drops.
                let _ = reply.send(warm);
            }
            Some(Ask::Warmed { key, id, warm, took }) => pools.warmed(&key, id, warm, took, now),
            Some(Ask::Ended { key }) => pools.ended(&key, now),
            Some(Ask::Stop) => {
                pools.stop();
                return;
            }
            #[cfg(test)]
            Some(Ask::Ready { reply }) => {
                let _ = reply.send(pools.pools.values().map(|p| p.ready.len()).sum());
            }
        }
        pools.expire(Instant::now());
    }
}

/// Pools of warm run hosts started by `hosts` (D253), [`super::ImagePools`]'s: a boot takes a
/// warm host of its run's pool where one is ready, else starts a host of its own; either way
/// its pool learns of the run. Dropping them ends every warm and starting host.
pub(super) struct Pooling<H: Hosts> {
    hosts: H,
    asks: mpsc::Sender<Ask<H::Warm>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<H: Hosts> std::fmt::Debug for Pooling<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pooling").finish_non_exhaustive()
    }
}

impl<H: Hosts> Pooling<H> {
    /// Pools of warm hosts that all together hold at most `most`, the orchestrator's bound,
    /// each pool keeping its hosts `keep` once idle.
    pub(super) fn new(hosts: H, most: usize, keep: Duration) -> Result<Pooling<H>, ImageRefusal> {
        let (asks, received) = mpsc::channel();
        let pools = Pools {
            hosts: hosts.clone(),
            most,
            keep,
            pools: HashMap::new(),
            asks: asks.clone(),
            next: 0,
        };
        let thread = std::thread::Builder::new()
            .name("shards-api-image-pools".into())
            .spawn(move || serve(pools, received))
            .map_err(|e| ImageRefusal::Os {
                what: "the pools' thread",
                kind: e.kind(),
            })?;
        Ok(Pooling {
            hosts,
            asks,
            thread: Some(thread),
        })
    }

    /// The run `request` of `spec`: on a warm host of its pool where one is ready and takes
    /// it, else on a host of its own; its pool told of it, and of its end as its guest drops.
    pub(super) fn boot(
        &self,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        let host = HostOf::of(spec);
        let exemplar = request.exemplar();
        let key = Key {
            host: host.clone(),
            exemplar: exemplar.encode().map_err(|e| ImageRefusal::Spec {
                what: "the spec",
                why: e.to_string(),
            })?,
        };
        let (reply, warm) = mpsc::channel();
        let asked = self.asks.send(Ask::Take {
            key: key.clone(),
            host,
            exemplar: Box::new(exemplar),
            reply,
        });
        let warm = asked.ok().and_then(|()| warm.recv().ok()).flatten();
        let guest = match warm.map(|w| self.hosts.take(w, spec, request, patience, stop)) {
            Some(Ok(guest)) => Ok(guest),
            Some(Err(Taken::Refused(refused))) => Err(refused),
            Some(Err(Taken::Unfit)) | None => self.hosts.fresh(spec, request, patience, stop),
        };
        match guest {
            Ok(guest) => Ok(Box::new(Pooled {
                guest,
                key: Some(key),
                asks: self.asks.clone(),
            })),
            Err(refused) => {
                let _ = self.asks.send(Ask::Ended { key });
                Err(refused)
            }
        }
    }
}

#[cfg(test)]
impl<H: Hosts> Pooling<H> {
    /// How many warm hosts its pools hold ready, as their thread holds them now.
    fn ready(&self) -> usize {
        let (reply, ready) = mpsc::channel();
        let _ = self.asks.send(Ask::Ready { reply });
        ready.recv().unwrap_or(0)
    }
}

impl<H: Hosts> Drop for Pooling<H> {
    fn drop(&mut self) {
        let _ = self.asks.send(Ask::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A run of a pool's: its guest, whose drop tells its pool the run has ended.
struct Pooled<W> {
    guest: Box<dyn ImageGuest>,
    key: Option<Key>,
    asks: mpsc::Sender<Ask<W>>,
}

impl<W> Drop for Pooled<W> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let _ = self.asks.send(Ask::Ended { key });
        }
    }
}

impl<W: Send> ImageGuest for Pooled<W> {
    fn members(&self) -> &[wire::MemberInfo] {
        self.guest.members()
    }

    fn published(&self) -> &[wire::Bound] {
        self.guest.published()
    }

    fn control(&self) -> Option<&wire::ControlAt> {
        self.guest.control()
    }

    fn take_output(
        &self,
        member: &super::MemberName,
        channel: wire::Channel,
    ) -> Result<std::io::PipeReader, ImageRefusal> {
        self.guest.take_output(member, channel)
    }

    fn take_input(&self, member: &super::MemberName) -> Result<std::io::PipeWriter, ImageRefusal> {
        self.guest.take_input(member)
    }

    fn wait_member(
        &self,
        member: &super::MemberName,
        timeout: Duration,
    ) -> Result<Option<super::MemberEnd>, ImageRefusal> {
        self.guest.wait_member(member, timeout)
    }

    fn stats(&self, scope: wire::StatsScope, patience: Duration) -> Result<wire::Stats, ImageRefusal> {
        self.guest.stats(scope, patience)
    }

    fn room(&self, room: Option<i64>, patience: Duration) -> Result<Option<u64>, ImageRefusal> {
        self.guest.room(room, patience)
    }

    fn rate(
        &self,
        rate: Option<std::num::NonZeroU64>,
        patience: Duration,
    ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal> {
        self.guest.rate(rate, patience)
    }

    fn rate_period(&self) -> Duration {
        self.guest.rate_period()
    }

    fn started_from(&self) -> wire::StartedFrom {
        self.guest.started_from()
    }

    fn resize(
        &self,
        member: &super::MemberName,
        rows: u16,
        cols: u16,
        patience: Duration,
    ) -> Result<(), ImageRefusal> {
        self.guest.resize(member, rows, cols, patience)
    }

    fn wait_ready(
        &self,
        seen: Option<wire::Ready>,
        timeout: Duration,
    ) -> Result<Option<wire::Ready>, ImageRefusal> {
        self.guest.wait_ready(seen, timeout)
    }

    fn plug(
        &self,
        member: &super::MemberName,
        host: &std::path::Path,
        at: &str,
        read_only: bool,
        patience: Duration,
    ) -> Result<(), ImageRefusal> {
        self.guest.plug(member, host, at, read_only, patience)
    }

    fn unplug(&self, at: &str, patience: Duration) -> Result<(), ImageRefusal> {
        self.guest.unplug(at, patience)
    }

    fn save(&self, to: &std::path::Path, patience: Duration) -> Result<(), ImageRefusal> {
        self.guest.save(to, patience)
    }

    fn stop(&self) -> Result<(), ImageRefusal> {
        self.guest.stop()
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<wire::RunEnd>, ImageRefusal> {
        self.guest.wait_for(timeout)
    }

    fn keep_diagnostics(&self, to: std::fs::File) -> Result<(), ImageRefusal> {
        self.guest.keep_diagnostics(to)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};

    /// How long a test waits for what its pools' thread does at most: a hang's bound alone,
    /// each wait ending on the fact it waits for.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// What a simulated run host has done, and how it is told to behave.
    #[derive(Debug, Default)]
    struct Log {
        begun: usize,
        /// Warm hosts alive: ready, held by a pool or taken, and not yet ended.
        warm: usize,
        taken: usize,
        fresh: usize,
        killed: usize,
        ended_runs: usize,
        /// Its warm starts wait until let go.
        hold: bool,
        /// Its warm starts fail.
        failing: bool,
        /// Warm starts that failed.
        failed: usize,
        /// Its warm hosts refuse their runs as another microVM's.
        unfit: bool,
    }

    #[derive(Debug, Default)]
    struct Sim {
        log: Mutex<Log>,
        changed: Condvar,
    }

    /// A simulated run host's starts (no process): each warm one ready as told.
    #[derive(Debug, Clone, Default)]
    struct Hosted(Arc<Sim>);

    impl Hosted {
        fn log(&self) -> std::sync::MutexGuard<'_, Log> {
            self.0.log.lock().unwrap()
        }

        fn tell(&self, f: impl FnOnce(&mut Log)) {
            f(&mut self.log());
            self.0.changed.notify_all();
        }

        /// Waits until `holds` holds of the log, at most [`PATIENCE`].
        fn until(&self, what: &str, holds: impl Fn(&Log) -> bool) {
            let (log, waited) = self
                .0
                .changed
                .wait_timeout_while(self.log(), PATIENCE, |l| !holds(l))
                .unwrap();
            assert!(!waited.timed_out(), "{what}: {log:?}");
        }
    }

    /// A warm host of the simulation's: ended as it drops.
    #[derive(Debug)]
    struct Warmed(Arc<Sim>);

    impl Drop for Warmed {
        fn drop(&mut self) {
            self.0.log.lock().unwrap().warm -= 1;
            self.0.changed.notify_all();
        }
    }

    /// A run of the simulation's: its end counted as it drops.
    struct Guest(Arc<Sim>);

    impl Drop for Guest {
        fn drop(&mut self) {
            self.0.log.lock().unwrap().ended_runs += 1;
            self.0.changed.notify_all();
        }
    }

    fn none<T>() -> Result<T, ImageRefusal> {
        Err(ImageRefusal::Host("a simulated run".into()))
    }

    impl ImageGuest for Guest {
        fn members(&self) -> &[wire::MemberInfo] {
            &[]
        }
        fn published(&self) -> &[wire::Bound] {
            &[]
        }
        fn control(&self) -> Option<&wire::ControlAt> {
            None
        }
        fn take_output(
            &self,
            _: &super::super::MemberName,
            _: wire::Channel,
        ) -> Result<std::io::PipeReader, ImageRefusal> {
            none()
        }
        fn take_input(&self, _: &super::super::MemberName) -> Result<std::io::PipeWriter, ImageRefusal> {
            none()
        }
        fn wait_member(
            &self,
            _: &super::super::MemberName,
            _: Duration,
        ) -> Result<Option<super::super::MemberEnd>, ImageRefusal> {
            none()
        }
        fn stats(&self, _: wire::StatsScope, _: Duration) -> Result<wire::Stats, ImageRefusal> {
            none()
        }
        fn room(&self, _: Option<i64>, _: Duration) -> Result<Option<u64>, ImageRefusal> {
            none()
        }
        fn rate(
            &self,
            _: Option<std::num::NonZeroU64>,
            _: Duration,
        ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal> {
            none()
        }
        fn rate_period(&self) -> Duration {
            Duration::from_millis(100)
        }
        fn started_from(&self) -> wire::StartedFrom {
            wire::StartedFrom::Warm
        }
        fn resize(
            &self,
            _: &super::super::MemberName,
            _: u16,
            _: u16,
            _: Duration,
        ) -> Result<(), ImageRefusal> {
            none()
        }
        fn wait_ready(
            &self,
            _: Option<wire::Ready>,
            _: Duration,
        ) -> Result<Option<wire::Ready>, ImageRefusal> {
            none()
        }
        fn plug(
            &self,
            _: &super::super::MemberName,
            _: &std::path::Path,
            _: &str,
            _: bool,
            _: Duration,
        ) -> Result<(), ImageRefusal> {
            none()
        }
        fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
            none()
        }
        fn save(&self, _: &std::path::Path, _: Duration) -> Result<(), ImageRefusal> {
            none()
        }
        fn stop(&self) -> Result<(), ImageRefusal> {
            Ok(())
        }
        fn wait_for(&self, _: Duration) -> Result<Option<wire::RunEnd>, ImageRefusal> {
            Ok(None)
        }
        fn keep_diagnostics(&self, _: std::fs::File) -> Result<(), ImageRefusal> {
            none()
        }
    }

    impl Hosts for Hosted {
        type Starting = ();
        type Warm = Warmed;
        type Kill = ();

        fn begin(&self, _: &HostOf, _: &wire::Request) -> Result<((), ()), ImageRefusal> {
            self.tell(|l| l.begun += 1);
            Ok(((), ()))
        }

        fn warmed(&self, (): ()) -> Result<Warmed, ImageRefusal> {
            let mut log = self.0.changed.wait_while(self.log(), |l| l.hold).unwrap();
            if log.failing {
                log.failed += 1;
                drop(log);
                self.0.changed.notify_all();
                return none();
            }
            log.warm += 1;
            drop(log);
            self.0.changed.notify_all();
            Ok(Warmed(self.0.clone()))
        }

        fn kill(&self, _: &()) {
            self.tell(|l| {
                l.killed += 1;
                l.hold = false;
            });
        }

        fn take(
            &self,
            warm: Warmed,
            _: &ImageSpec,
            _: &wire::Request,
            _: Duration,
            _: &BootStop,
        ) -> Result<Box<dyn ImageGuest>, Taken> {
            drop(warm);
            if self.log().unfit {
                return Err(Taken::Unfit);
            }
            self.tell(|l| l.taken += 1);
            Ok(Box::new(Guest(self.0.clone())))
        }

        fn fresh(
            &self,
            _: &ImageSpec,
            _: &wire::Request,
            _: Duration,
            _: &BootStop,
        ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
            self.tell(|l| l.fresh += 1);
            Ok(Box::new(Guest(self.0.clone())))
        }
    }

    fn spec() -> ImageSpec {
        ImageSpec {
            image: "agents/codex:1".into(),
            vcpus: 1,
            memory_mib: 512,
            command: None,
            members: Vec::new(),
            publish: Vec::new(),
            egress: wire::Narrowing::default(),
            credentials: Vec::new(),
            lessor: None,
            shares: Vec::new(),
            nesting: false,
            ready_by: None,
            binary: None,
            env: Vec::new(),
            control: None,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
        }
    }

    fn request(vcpus: u32, ready_by: Option<u64>) -> wire::Request {
        wire::Request {
            image: "agents/codex:1".into(),
            vcpus,
            memory_mib: 512,
            command: None,
            members: Vec::new(),
            publish: Vec::new(),
            egress: wire::Narrowing::default(),
            shares: Vec::new(),
            nesting: false,
            ready_by,
            credentials: Vec::new(),
            control: None,
            gpu_devices: Vec::new(),
            sign_ins: Vec::new(),
        }
    }

    /// D253's pools, cold: a pool's first run gets a host of its own, nothing being warm, and
    /// its pool starts one ahead of the next; the next run of the same microVM, its deadline
    /// its own, takes that warm host, and its pool starts more (as many as its burst, which
    /// its warm hosts' start times as this host runs them make one or two runs); a run of
    /// another microVM's starts a pool of its own, its first run a host of its own.
    #[test]
    fn a_pools_first_run_starts_its_own_host_and_the_next_takes_a_warm_one() {
        let hosted = Hosted::default();
        let pools = Pooling::new(hosted.clone(), 4, KEEP_TEST).unwrap();
        let first = pools
            .boot(&spec(), &request(1, Some(5)), PATIENCE, &BootStop::new())
            .unwrap();
        hosted.until("one warm ahead of the next run", |l| l.fresh == 1 && l.warm == 1);
        until_ready(&pools, 1);
        let next = pools
            .boot(&spec(), &request(1, Some(9)), PATIENCE, &BootStop::new())
            .unwrap();
        assert_eq!(next.started_from(), wire::StartedFrom::Warm);
        hosted.until("the warm one taken, another started", |l| {
            l.taken == 1 && l.fresh == 1 && l.begun >= 2 && l.warm >= 1
        });
        let other = pools
            .boot(&spec(), &request(2, None), PATIENCE, &BootStop::new())
            .unwrap();
        hosted.until("another microVM's run on a host of its own", |l| l.fresh == 2);
        drop((first, next, other));
        hosted.until("every run's end told", |l| l.ended_runs == 3);
        drop(pools);
        hosted.until("every warm host ended with its pools", |l| l.warm == 0);
    }

    /// D253's pools: a warm host that finds its run another microVM's (`Unfit`) gives the run
    /// a host of its own; one whose start fails is no warm host, its runs getting hosts of
    /// their own.
    #[test]
    fn a_warm_host_that_does_not_fit_or_start_leaves_its_run_a_host_of_its_own() {
        let hosted = Hosted::default();
        let pools = Pooling::new(hosted.clone(), 4, KEEP_TEST).unwrap();
        drop(
            pools
                .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
                .unwrap(),
        );
        hosted.until("one warm", |l| l.warm == 1);
        until_ready(&pools, 1);
        hosted.tell(|l| {
            l.unfit = true;
            l.failing = true;
        });
        let run = pools
            .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
            .unwrap();
        // Every start after the first failed: one or two, as many as its burst.
        hosted.until(
            "the unfit one's run on a host of its own, its refill failed",
            |l| l.fresh == 2 && l.taken == 0 && l.failed >= 1 && l.failed + 1 == l.begun,
        );
        assert_eq!(hosted.log().warm, 0);
        drop(run);
        drop(
            pools
                .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
                .unwrap(),
        );
        hosted.until("a failed start's run on a host of its own", |l| l.fresh == 3);
    }

    /// D253's pools sized by D26's rule: runs that come within one refill window are one
    /// burst, which a pool keeps warm, at most the orchestrator's bound for all pools; its
    /// cold start is RFC 6298's initial window, as no refill has been timed while its warm
    /// starts wait (`hold`). Dropping the pools ends every warm host and kills every starting
    /// one.
    #[test]
    fn a_pool_keeps_its_burst_within_the_bound_and_its_owner_ends_them_all() {
        let hosted = Hosted::default();
        hosted.tell(|l| l.hold = true);
        let pools = Pooling::new(hosted.clone(), 2, KEEP_TEST).unwrap();
        let runs: Vec<_> = (0..3)
            .map(|_| {
                pools
                    .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
                    .unwrap()
            })
            .collect();
        hosted.until("the burst's hosts starting, as many as the bound", |l| {
            l.fresh == 3 && l.begun == 2
        });
        drop(runs);
        drop(pools);
        hosted.until("the starting ones killed with the pools", |l| {
            l.killed == 2 && l.warm == 0
        });
    }

    /// D253's pools: a pool keeps nothing past its keep-alive from its last run's end, its
    /// warm hosts ended then; while a run of it goes on, its keep-alive has not begun.
    #[test]
    fn a_pools_warm_hosts_end_past_its_keep_alive() {
        let hosted = Hosted::default();
        let keep = Duration::from_millis(200);
        let pools = Pooling::new(hosted.clone(), 4, keep).unwrap();
        let run = pools
            .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
            .unwrap();
        hosted.until("one warm", |l| l.warm == 1);
        std::thread::sleep(keep * 2);
        assert_eq!(hosted.log().warm, 1, "a pool whose run goes on keeps its host");
        drop(run);
        hosted.until("its warm host ended past its keep-alive", |l| l.warm == 0);
        drop(pools);
    }

    /// Waits until `pools` hold `ready` warm hosts ready, as their thread holds them: a host
    /// warm is its pool's only once its thread has taken it in, after the host's own word.
    fn until_ready(pools: &Pooling<Hosted>, ready: usize) {
        let deadline = Instant::now() + PATIENCE;
        while pools.ready() != ready {
            assert!(
                Instant::now() < deadline,
                "{ready} warm hosts ready within {PATIENCE:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A keep-alive past any test's longest step: a test's input, no orchestrator's.
    const KEEP_TEST: Duration = Duration::from_secs(600);
}
