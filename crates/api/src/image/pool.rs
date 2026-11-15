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
//! - **Its room.** Every warm host is charged to the orchestrator's room before it starts
//!   ([`WarmRoom::claim`], its spec's memory and vCPUs), and its room comes back exactly once
//!   ([`WarmRoom::give`]): to a run that takes it, as it ends, or as its start fails. A size a
//!   spec leaves to `shards run` (`0`) is resolved first as its run host would give it
//!   (`shards_ipc::size`), so it pools with the spec that names that size and claims its real
//!   room. A run that asks its GPU report is no pool's.
//! - **Its size** is D26's, by the one rule ([`crate::demand`]): the largest burst of its runs
//!   within one refill window, an RFC 6298 retransmission timeout of its warm hosts' starts as
//!   timed here, while it is in use, at least one; none once idle past its keep-alive. What it
//!   wants past what its room admits is unmet, and nothing errs.
//! - **Its cold start.** Nothing is warm until a pool's first run, which starts a host of its
//!   own; its first window is RFC 6298's initial RTO, as the daemon's pools' is.
//!
//! One thread keeps the pools, their one owner, under no lock: it hands out and reserves warm
//! hosts, claims and starts more where a pool wants them (each host's start awaited on a
//! thread of its own), and hands those a pool keeps no more to be ended, each followed to its
//! end before its room is given. It never calls the room itself, so no answer of the pools'
//! ever waits on one (see [`WarmRoom`]).

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::wire;
use super::{BootStop, ImageGuest, ImageRefusal, ImageSpec};
use crate::demand::Demand;

/// The orchestrator's room, which every warm host is charged to (D253): a Hex node's, which
/// admits its boxes from the same room. It decides alone. A claim is refused only where the
/// host does not fit the room free now, with no headroom of its own kept: a box that needs
/// room a warm host holds wins it back through [`ImagePools::release`](super::ImagePools::release).
///
/// Where each call is made:
/// - `claim`, and `give` for a start that failed, on the pools' room thread, in order: never on
///   the pools' own thread, so no answer of the pools' (`release`, `reserve`, `held`, a boot's
///   take) waits on a claim or a give.
/// - `give(Taken)` on the caller's thread, within [`ImagePools::reserve`](super::ImagePools::reserve)
///   or the boot that takes the host, before it returns.
/// - `give(Ended)` within [`ImagePools::release`](super::ImagePools::release) on its caller's
///   thread before it returns; for a host past its pool's target or keep-alive on a thread
///   ending it; for every host as the pools drop, before the drop returns.
///
/// So `claim` and `give` must not block on a lock that their owner holds across a call of the
/// pools (dropping them included); an atomic count of the room meets that. A call of the pools
/// from within `claim` or `give` waits on nothing they wait on, and is answered.
pub trait WarmRoom: Send + Sync + 'static {
    /// Before a warm host starts: claim its room. false → it isn't started.
    fn claim(&self, host: &WarmHost) -> bool;
    /// Its room comes back, exactly once per successful claim.
    fn give(&self, host: &WarmHost, why: Gave);
}

/// A warm host's charge: its microVM's memory in bytes and its vCPUs, as its spec names them,
/// and its pool's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WarmHost {
    pub memory: u64,
    pub vcpus: u32,
    pub pool: PoolId,
}

/// Why a warm host's room came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Gave {
    /// A run took it, or a box reserved it: its room is the box's now, and that guest gives
    /// nothing as it drops.
    Taken,
    /// It ended, its processes gone: past its pool's target or keep-alive, released, or with
    /// its pools.
    Ended,
    /// Its start failed.
    Failed,
}

/// What [`ImagePools::release`](super::ImagePools::release) ended: how many hosts, and their
/// memory in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Released {
    pub hosts: usize,
    pub memory: u64,
}

/// What one pool holds of its room now: its warm and starting hosts, and their memory in bytes
/// and vCPUs, as claimed (each its spec's whole size); and each warm host's resident footprint
/// in bytes, as its VM process reports it, one reading for each warm host whose reading is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolHeld {
    pub pool: PoolId,
    pub memory: u64,
    pub vcpus: u32,
    pub warm: usize,
    pub starting: usize,
    pub footprints: Vec<u64>,
}

/// A pool, by the SHA-256 of its key (its run host and its runs' exemplar): the same while
/// the pool lives, and again for a pool of the same key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId([u8; 32]);

impl std::fmt::Display for PoolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sha256:")?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl std::fmt::Debug for PoolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PoolId({self})")
    }
}

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

/// A run's pool: its key, its exemplar, its hosts' charge, and the run's request sized. None
/// for a run that is no pool's: one asking its GPU report (D233: a warm host's VM process
/// starts before its run names the report's scope, which that process frames its report with
/// as its renderer is made), and one whose memory's bytes pass a u64.
struct Of {
    key: Key,
    exemplar: wire::Request,
    size: WarmHost,
    request: wire::Request,
}

/// `request` with each size it leaves to `shards run` (`0`) as `shards run` gives it on this
/// host (`shards_ipc::size::unsized_run`, the run host's own sizing): its run host would boot
/// it so, so its pool, its claim and its warm host's exemplar are of that size.
fn sized(request: &wire::Request) -> wire::Request {
    let mut sized = request.clone();
    if sized.vcpus == 0 || sized.memory_mib == 0 {
        let (vcpus, memory_mib) = shards_ipc::size::unsized_run();
        if sized.vcpus == 0 {
            sized.vcpus = vcpus;
        }
        if sized.memory_mib == 0 {
            sized.memory_mib = memory_mib;
        }
    }
    sized
}

fn pool_of(spec: &ImageSpec, request: &wire::Request) -> Result<Option<Of>, ImageRefusal> {
    if request.gpu_report.is_some() {
        return Ok(None);
    }
    let request = sized(request);
    let Some(memory) = request.memory_mib.checked_mul(1 << 20) else {
        return Ok(None);
    };
    let exemplar = request.exemplar();
    let key = Key {
        host: HostOf::of(spec),
        exemplar: exemplar.encode().map_err(|e| ImageRefusal::Spec {
            what: "the spec",
            why: e.to_string(),
        })?,
    };
    let pool = PoolId(key.digest());
    Ok(Some(Of {
        key,
        exemplar,
        size: WarmHost {
            memory,
            vcpus: request.vcpus,
            pool,
        },
        request,
    }))
}

impl Key {
    fn digest(&self) -> [u8; 32] {
        let mut sha = Sha256::new();
        let mut field = |bytes: &[u8]| {
            sha.update((bytes.len() as u64).to_le_bytes());
            sha.update(bytes);
        };
        field(
            self.host
                .binary
                .as_ref()
                .map_or(&[][..], |b| b.as_os_str().as_encoded_bytes()),
        );
        for (name, value) in &self.host.env {
            field(name.as_encoded_bytes());
            field(value.as_encoded_bytes());
        }
        field(&self.exemplar);
        sha.finalize().into()
    }
}

/// Why a warm host did not take a run.
#[derive(Debug)]
pub(super) enum Taken {
    /// The host ended before CREATE was attempted; it certainly never received this run.
    Untaken,
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
    /// Waits for `starting` to be warm; refused only once its host has ended.
    fn warmed(&self, starting: Self::Starting) -> Result<Self::Warm, ImageRefusal>;
    /// Ends a host that is starting, at once: its [`warmed`](Self::warmed) refuses it once it
    /// has ended.
    fn kill(&self, kill: &Self::Kill);
    /// Warm host `warm`'s resident footprint in bytes, as its VM process last reported it:
    /// read at once, never waiting on its guest; none before a reading is in.
    fn footprint(&self, warm: &Self::Warm) -> Option<u64>;
    /// Ends warm host `warm`, returning once its processes have ended, followed to their end
    /// and never timed (D215, D217); or why they could not be followed, typed.
    fn end(&self, warm: Self::Warm) -> Result<(), ImageRefusal>;
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
    /// A warm host for a run of `key`, if one is ready, with its charge, for the run's asker
    /// to give its room to the run (`Taken`); its pool begun with `exemplar` and `size`, which
    /// its later hosts are started with.
    Take {
        key: Key,
        exemplar: Box<wire::Request>,
        size: WarmHost,
        reply: mpsc::Sender<Option<W>>,
    },
    /// A warm host of `key`'s pool held for one box, if one is ready; counted as its pool's
    /// run, as a take is.
    Reserve {
        key: Key,
        reply: mpsc::Sender<Option<W>>,
    },
    /// A reserved host its box did not use, back to its pool under a claim of its own, its run
    /// uncounted.
    Return { key: Key, warm: W },
    /// The room's answer to claim `id`.
    Claimed { id: u64, claimed: bool },
    /// A host of `key`'s started warm, `took` from its start to its readiness; or why not.
    Warmed {
        key: Key,
        id: u64,
        warm: Result<W, ImageRefusal>,
        took: Duration,
    },
    /// A run of `key` ended: its pool is idle from its last run's end.
    Ended { key: Key },
    /// A warm host of `key`'s handed out, its room given (`Taken`): its pool refilled, its
    /// claims after that give, so that a room sees the room it gave back. A take's is asked once
    /// the run it took the host for has started or failed to ([`Pooling::boot`]), a
    /// reservation's as it is reserved.
    Refill { key: Key },
    /// The coldest hosts, until `memory` bytes of them, handed to `reply` to be ended.
    Release {
        memory: u64,
        reply: mpsc::Sender<Releasing<W>>,
    },
    /// What each pool holds of its room.
    Held { reply: mpsc::Sender<Vec<PoolHeld>> },
    /// The pools' owner is gone: every host ends.
    Stop,
}

/// What the room's thread is asked: the pools' thread's every call of the room, made in order
/// on a thread of their own, so that a room that waits (on a lock its owner holds across a
/// call of the pools) holds up no answer of the pools'.
enum ToRoom {
    Claim { id: u64, size: WarmHost },
    Give { size: WarmHost, why: Gave },
}

/// The room's thread: each claim answered back to the pools (a claim the pools are gone for
/// given back at once), each give made, until the pools are gone.
fn keep_room<W>(room: &dyn WarmRoom, asks: &mpsc::Receiver<ToRoom>, pools: &mpsc::Sender<Ask<W>>) {
    for ask in asks {
        match ask {
            ToRoom::Claim { id, size } => {
                let claimed = room.claim(&size);
                if pools.send(Ask::Claimed { id, claimed }).is_err() && claimed {
                    room.give(&size, Gave::Ended);
                }
            }
            ToRoom::Give { size, why } => room.give(&size, why),
        }
    }
}

/// What a release ends: the warm hosts handed over, and the starting ones killed, whose ends
/// come on `ends`.
struct Releasing<W> {
    warm: Vec<(WarmHost, W)>,
    starting: usize,
    ends: mpsc::Receiver<Ending<W>>,
}

/// Where a release hears of the starting hosts it killed.
type Ends<W> = mpsc::Sender<Ending<W>>;

/// A starting host a release killed, as its start's wait ended: gone, or warm all the same,
/// its kill having come as it said `WARMED`.
enum Ending<W> {
    Gone(WarmHost),
    Warm(WarmHost, W),
}

/// What a claim asked of the room is for: a host to start, or a reserved one back.
enum Claim<W> {
    Start,
    Back(W),
}

/// A warm host, ready since `since`.
struct Ready<W> {
    warm: W,
    since: Instant,
}

/// One microVM's pool.
struct Pool<W, K> {
    exemplar: wire::Request,
    size: WarmHost,
    /// Its warm hosts, the longest ready first.
    ready: VecDeque<Ready<W>>,
    /// The hosts starting, by their start's number, and what ends each.
    starting: HashMap<u64, K>,
    /// Its claims the room has yet to answer.
    claiming: usize,
    demand: Demand,
}

/// A pool's target with no count of ours: what its room admits bounds it
/// ([`WarmRoom::claim`]), and its demand's held claims are those of one refill window.
const UNCOUNTED: usize = usize::MAX;

/// The pools, kept by their thread, which calls no room itself.
struct Pools<H: Hosts> {
    hosts: H,
    /// For the threads that end hosts, each giving its room as its processes end.
    room: Arc<dyn WarmRoom>,
    to_room: mpsc::Sender<ToRoom>,
    keep: Duration,
    pools: HashMap<Key, Pool<H::Warm, H::Kill>>,
    /// Claims the room has yet to answer, by number: whose, and what for.
    claims: HashMap<u64, (Key, WarmHost, Claim<H::Warm>)>,
    /// Starting hosts a release killed, by their start's number: what each was charged, and
    /// where its end goes.
    ending: HashMap<u64, (WarmHost, Ends<H::Warm>)>,
    /// The threads ending hosts the pools keep no more.
    enders: Vec<JoinHandle<()>>,
    asks: mpsc::Sender<Ask<H::Warm>>,
    next: u64,
}

impl<H: Hosts> Pools<H> {
    fn give(&self, size: WarmHost, why: Gave) {
        // The room's thread outlives this one (`Pooling::drop`).
        let _ = self.to_room.send(ToRoom::Give { size, why });
    }

    fn number(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }

    fn take(&mut self, key: Key, exemplar: wire::Request, size: WarmHost, now: Instant) -> Option<H::Warm> {
        let pool = self.pools.entry(key.clone()).or_insert_with(|| Pool {
            exemplar: exemplar.clone(),
            size,
            ready: VecDeque::new(),
            starting: HashMap::new(),
            claiming: 0,
            demand: Demand::default(),
        });
        // Its later hosts take its last run's exemplar: the same microVM's, by its key.
        pool.exemplar = exemplar;
        self.counted(&key, now)
    }

    /// A run of pool `key` at `now`: counted, and a warm host popped for it where one is
    /// ready, its pool refilled once its asker has given its room and its run has started
    /// ([`Ask::Refill`]); else refilled now.
    fn counted(&mut self, key: &Key, now: Instant) -> Option<H::Warm> {
        let pool = self.pools.get_mut(key)?;
        pool.demand.begin(now);
        pool.demand.claimed(now, UNCOUNTED);
        pool.demand.started();
        let warm = pool.ready.pop_front().map(|r| r.warm);
        if warm.is_none() {
            self.refill(key, now);
        }
        warm
    }

    /// A warm host for one box of `key`'s pool, where one is ready; nothing counted where
    /// none is, as the box's boot then counts its run.
    fn reserve(&mut self, key: &Key, now: Instant) -> Option<H::Warm> {
        if self.pools.get(key).is_none_or(|p| p.ready.is_empty()) {
            return None;
        }
        self.counted(key, now)
    }

    /// A reserved host back unused: claimed again where its pool still wants it, else ended;
    /// its old claim's room went to its box, so neither gives for that one.
    fn returned(&mut self, key: Key, warm: H::Warm, now: Instant) {
        let keep = self.keep;
        let wanted = self.pools.get_mut(&key).and_then(|pool| {
            pool.demand.ended(now);
            let target = pool.demand.target(now, UNCOUNTED, keep);
            (pool.ready.len() + pool.starting.len() + pool.claiming < target).then(|| {
                pool.claiming += 1;
                pool.size
            })
        });
        match wanted {
            Some(size) => self.claim(key, size, Claim::Back(warm)),
            None => self.end_off(None, warm),
        }
    }

    /// Asks the room's thread to claim `size` for `why`; its pool has counted the claim.
    fn claim(&mut self, key: Key, size: WarmHost, why: Claim<H::Warm>) {
        let id = self.number();
        self.claims.insert(id, (key, size, why));
        if self.to_room.send(ToRoom::Claim { id, size }).is_err() {
            self.claimed(id, false, Instant::now());
        }
    }

    /// The room's answer to claim `id`: a host started, or a reserved one put back, where it
    /// claimed; else none started, and a reserved one ended.
    fn claimed(&mut self, id: u64, claimed: bool, now: Instant) {
        let Some((key, size, why)) = self.claims.remove(&id) else {
            return;
        };
        let Some(pool) = self.pools.get_mut(&key) else {
            // Pools keep a pool while it claims; held all the same, as no pool's.
            if claimed {
                self.give(size, Gave::Ended);
            }
            if let Claim::Back(warm) = why {
                self.end_off(None, warm);
            }
            return;
        };
        pool.claiming = pool.claiming.saturating_sub(1);
        match (why, claimed) {
            (Claim::Start, true) => self.start(&key, size),
            (Claim::Start, false) => {}
            (Claim::Back(warm), true) => pool.ready.push_back(Ready { warm, since: now }),
            (Claim::Back(warm), false) => self.end_off(None, warm),
        }
    }

    fn warmed(
        &mut self,
        key: &Key,
        id: u64,
        warm: Result<H::Warm, ImageRefusal>,
        took: Duration,
        now: Instant,
    ) {
        if let Some((size, to)) = self.ending.remove(&id) {
            let ended = match warm {
                Ok(warm) => Ending::Warm(size, warm),
                Err(_) => Ending::Gone(size),
            };
            // Its releaser waits on it while it holds `&ImagePools`, so is never gone.
            let _ = to.send(ended);
            return;
        }
        let keep = self.keep;
        let Some(pool) = self.pools.get_mut(key) else {
            return;
        };
        if pool.starting.remove(&id).is_none() {
            return;
        }
        let size = pool.size;
        match warm {
            Ok(warm) => {
                pool.demand.refilled(took);
                let target = pool.demand.target(now, UNCOUNTED, keep);
                if pool.ready.len() < target {
                    pool.ready.push_back(Ready { warm, since: now });
                } else {
                    self.end_off(Some(size), warm);
                }
            }
            // One that does not start is no warm host: its runs get hosts of their own, and
            // its pool starts no more until its next run asks.
            Err(_) => {
                self.give(size, Gave::Failed);
                return;
            }
        }
        self.refill(key, now);
    }

    fn ended(&mut self, key: &Key, now: Instant) {
        if let Some(pool) = self.pools.get_mut(key) {
            pool.demand.ended(now);
        }
    }

    /// Claims the hosts pool `key` wants to keep the target its demand says, each of the room
    /// before it starts: one it refuses is not started, and the target stays unmet until the
    /// pool's next run or start.
    fn refill(&mut self, key: &Key, now: Instant) {
        let keep = self.keep;
        let Some(pool) = self.pools.get_mut(key) else {
            return;
        };
        let target = pool.demand.target(now, UNCOUNTED, keep);
        let want = target.saturating_sub(pool.ready.len() + pool.starting.len() + pool.claiming);
        pool.claiming = pool.claiming.saturating_add(want);
        let size = pool.size;
        for _ in 0..want {
            self.claim(key.clone(), size, Claim::Start);
        }
    }

    /// Starts a host of pool `key` warm, its room claimed: its start awaited on a thread of
    /// its own; its room given back where it does not start.
    fn start(&mut self, key: &Key, size: WarmHost) {
        let Some(pool) = self.pools.get(key) else {
            self.give(size, Gave::Failed);
            return;
        };
        let Ok((starting, kill)) = self.hosts.begin(&key.host, &pool.exemplar) else {
            self.give(size, Gave::Failed);
            return;
        };
        let id = self.number();
        let (hosts, asks, owned) = (self.hosts.clone(), self.asks.clone(), key.clone());
        let began = Instant::now();
        let (hand, handed) = mpsc::channel::<H::Starting>();
        let waiting = std::thread::Builder::new()
            .name("shards-api-image-warm".into())
            .spawn(move || {
                let Ok(starting) = handed.recv() else {
                    return;
                };
                let warm = hosts.warmed(starting);
                let took = began.elapsed();
                // The pools gone: the host ends as its value drops.
                let _ = asks.send(Ask::Warmed {
                    key: owned,
                    id,
                    warm,
                    took,
                });
            });
        let handed = match waiting {
            Ok(_) => hand.send(starting).map_err(|mpsc::SendError(starting)| starting),
            Err(_) => Err(starting),
        };
        match handed {
            Ok(()) => {
                if let Some(pool) = self.pools.get_mut(key) {
                    pool.starting.insert(id, kill);
                }
            }
            // Ended here, followed to its end, before its room is given.
            Err(starting) => {
                self.hosts.kill(&kill);
                drop(self.hosts.warmed(starting));
                self.give(size, Gave::Failed);
            }
        }
    }

    /// Ends the warm hosts each pool keeps no more at `now`: past its target, the last
    /// started first; every one past its keep-alive. A pool idle with nothing left goes.
    fn expire(&mut self, now: Instant) {
        let keep = self.keep;
        let mut ends = Vec::new();
        self.pools.retain(|_, pool| {
            let target = pool.demand.target(now, UNCOUNTED, keep);
            while pool.ready.len() > target {
                if let Some(r) = pool.ready.pop_back() {
                    ends.push((pool.size, r.warm));
                }
            }
            !(pool.ready.is_empty()
                && pool.starting.is_empty()
                && pool.claiming == 0
                && pool.demand.expired(now, keep))
        });
        for (size, warm) in ends {
            self.end_off(Some(size), warm);
        }
        self.enders.retain(|e| !e.is_finished());
    }

    /// Ends `warm` on a thread of its own, giving its room `size` once its processes have
    /// ended, where it holds a claim; here, where no thread starts.
    fn end_off(&mut self, size: Option<WarmHost>, warm: H::Warm) {
        let (hosts, room) = (self.hosts.clone(), self.room.clone());
        let (hand, handed) = mpsc::channel::<H::Warm>();
        let ender = std::thread::Builder::new()
            .name("shards-api-image-end".into())
            .spawn(move || {
                if let Ok(warm) = handed.recv() {
                    let _ = hosts.end(warm);
                    if let Some(size) = size {
                        room.give(&size, Gave::Ended);
                    }
                }
            });
        let handed = match ender {
            Ok(ender) => {
                self.enders.push(ender);
                hand.send(warm).map_err(|mpsc::SendError(warm)| warm)
            }
            Err(_) => Err(warm),
        };
        if let Err(warm) = handed {
            let _ = self.hosts.end(warm);
            if let Some(size) = size {
                self.give(size, Gave::Ended);
            }
        }
    }

    /// Hands over the coldest hosts until `memory` bytes of them: warm ones first, of the
    /// pools wanting fewest, then the longest ready; then those starting, killed here, of the
    /// pools wanting fewest, then the last begun.
    fn release(&mut self, memory: u64, now: Instant) -> Releasing<H::Warm> {
        let keep = self.keep;
        let mut cold = Vec::new();
        let mut late = Vec::new();
        for (key, pool) in &mut self.pools {
            let target = pool.demand.target(now, UNCOUNTED, keep);
            cold.extend(pool.ready.iter().map(|r| (target, r.since, key.clone())));
            late.extend(
                pool.starting
                    .keys()
                    .map(|&id| (target, std::cmp::Reverse(id), key.clone())),
            );
        }
        cold.sort_by_key(|(target, since, _)| (*target, *since));
        late.sort_by_key(|(target, id, _)| (*target, *id));
        let mut freed = 0u64;
        let mut warm = Vec::new();
        for (_, _, key) in cold {
            if freed >= memory {
                break;
            }
            // A pool's ready hosts are the longest ready first, as sorted here.
            if let Some(pool) = self.pools.get_mut(&key)
                && let Some(r) = pool.ready.pop_front()
            {
                freed = freed.saturating_add(pool.size.memory);
                warm.push((pool.size, r.warm));
            }
        }
        let (to, ends) = mpsc::channel();
        let mut starting = 0;
        for (_, std::cmp::Reverse(id), key) in late {
            if freed >= memory {
                break;
            }
            if let Some(pool) = self.pools.get_mut(&key)
                && let Some(kill) = pool.starting.remove(&id)
            {
                self.hosts.kill(&kill);
                freed = freed.saturating_add(pool.size.memory);
                self.ending.insert(id, (pool.size, to.clone()));
                starting += 1;
            }
        }
        Releasing { warm, starting, ends }
    }

    fn held(&self) -> Vec<PoolHeld> {
        self.pools
            .values()
            .map(|pool| {
                let hosts = pool.ready.len() + pool.starting.len();
                PoolHeld {
                    pool: pool.size.pool,
                    memory: pool
                        .size
                        .memory
                        .saturating_mul(u64::try_from(hosts).unwrap_or(u64::MAX)),
                    vcpus: pool
                        .size
                        .vcpus
                        .saturating_mul(u32::try_from(hosts).unwrap_or(u32::MAX)),
                    warm: pool.ready.len(),
                    starting: pool.starting.len(),
                    footprints: pool
                        .ready
                        .iter()
                        .filter_map(|r| self.hosts.footprint(&r.warm))
                        .collect(),
                }
            })
            .collect()
    }

    /// Ends every host, each followed to its end and its room given: those warm ended, each
    /// on a thread of its own, those starting killed and awaited as their starts' waits end,
    /// and each claim the room has yet to answer given back as it answers. What else is asked
    /// meanwhile is answered as by pools that hold nothing.
    fn stop(&mut self, asks: &mpsc::Receiver<Ask<H::Warm>>) {
        let mut killed: HashMap<u64, WarmHost> = HashMap::new();
        let mut warm = Vec::new();
        for (_, pool) in self.pools.drain() {
            for (id, kill) in &pool.starting {
                self.hosts.kill(kill);
                killed.insert(*id, pool.size);
            }
            warm.extend(pool.ready.into_iter().map(|r| (pool.size, r.warm)));
        }
        let hosts = &self.hosts;
        let ended: Vec<WarmHost> = std::thread::scope(|scope| {
            let mut ending = Vec::new();
            let mut ended = Vec::new();
            for (size, warm) in warm {
                let (hand, handed) = mpsc::channel::<H::Warm>();
                let ender = std::thread::Builder::new()
                    .name("shards-api-image-end".into())
                    .spawn_scoped(scope, move || {
                        if let Ok(warm) = handed.recv() {
                            let _ = hosts.end(warm);
                        }
                    });
                match ender {
                    Ok(ender) => match hand.send(warm) {
                        Ok(()) => ending.push((size, ender)),
                        Err(mpsc::SendError(warm)) => {
                            let _ = hosts.end(warm);
                            ended.push(size);
                        }
                    },
                    Err(_) => {
                        let _ = hosts.end(warm);
                        ended.push(size);
                    }
                }
            }
            for (size, ender) in ending {
                let _ = ender.join();
                ended.push(size);
            }
            ended
        });
        for size in ended {
            self.give(size, Gave::Ended);
        }
        while !killed.is_empty() || !self.claims.is_empty() {
            let Ok(ask) = asks.recv() else {
                break;
            };
            match ask {
                Ask::Warmed { id, warm, .. } => {
                    if let Some(size) = killed.remove(&id) {
                        if let Ok(warm) = warm {
                            let _ = self.hosts.end(warm);
                        }
                        self.give(size, Gave::Ended);
                    }
                }
                Ask::Claimed { id, claimed } => {
                    if let Some((_, size, why)) = self.claims.remove(&id) {
                        if let Claim::Back(warm) = why {
                            let _ = self.hosts.end(warm);
                        }
                        if claimed {
                            self.give(size, Gave::Ended);
                        }
                    }
                }
                Ask::Return { warm, .. } => drop(self.hosts.end(warm)),
                Ask::Take { reply, .. } | Ask::Reserve { reply, .. } => drop(reply.send(None)),
                Ask::Held { reply } => drop(reply.send(Vec::new())),
                Ask::Release { reply, .. } => drop(reply.send(Releasing {
                    warm: Vec::new(),
                    starting: 0,
                    ends: mpsc::channel().1,
                })),
                Ask::Ended { .. } | Ask::Refill { .. } | Ask::Stop => {}
            }
        }
        for ender in self.enders.drain(..) {
            let _ = ender.join();
        }
    }
}

/// The pools' thread: their asks, and the keep-alives ending, until their owner is gone.
fn serve<H: Hosts>(mut pools: Pools<H>, asks: &mpsc::Receiver<Ask<H::Warm>>) {
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
                exemplar,
                size,
                reply,
            }) => {
                let warm = pools.take(key, *exemplar, size, now);
                // Its asker waits on it while it holds `&ImagePools`, so is never gone.
                let _ = reply.send(warm);
            }
            Some(Ask::Reserve { key, reply }) => {
                let warm = pools.reserve(&key, now);
                let _ = reply.send(warm);
            }
            Some(Ask::Return { key, warm }) => pools.returned(key, warm, now),
            Some(Ask::Claimed { id, claimed }) => pools.claimed(id, claimed, now),
            Some(Ask::Warmed { key, id, warm, took }) => pools.warmed(&key, id, warm, took, now),
            Some(Ask::Ended { key }) => pools.ended(&key, now),
            Some(Ask::Refill { key }) => pools.refill(&key, now),
            Some(Ask::Release { memory, reply }) => {
                let _ = reply.send(pools.release(memory, now));
            }
            Some(Ask::Held { reply }) => {
                let _ = reply.send(pools.held());
            }
            Some(Ask::Stop) => {
                pools.stop(asks);
                return;
            }
        }
        pools.expire(Instant::now());
    }
}

impl<H: Hosts> Pools<H> {
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
}

/// Pools of warm run hosts started by `hosts` (D253), [`super::ImagePools`]'s: a boot takes a
/// warm host of its run's pool where one is ready, else starts a host of its own; either way
/// its pool learns of the run. Dropping them ends every warm and starting host, each followed
/// to its end and its room given back, before the drop returns.
pub(super) struct Pooling<H: Hosts> {
    hosts: H,
    room: Arc<dyn WarmRoom>,
    asks: mpsc::Sender<Ask<H::Warm>>,
    thread: Option<JoinHandle<()>>,
    room_thread: Option<JoinHandle<()>>,
}

impl<H: Hosts> std::fmt::Debug for Pooling<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pooling").finish_non_exhaustive()
    }
}

impl<H: Hosts> Pooling<H> {
    /// Pools of warm hosts each charged to `room` before it starts, each pool keeping its
    /// hosts `keep` once idle.
    pub(super) fn new(hosts: H, room: impl WarmRoom, keep: Duration) -> Result<Pooling<H>, ImageRefusal> {
        let room: Arc<dyn WarmRoom> = Arc::new(room);
        let (asks, received) = mpsc::channel();
        let (to_room, room_asks) = mpsc::channel();
        let os = |what| move |e: std::io::Error| ImageRefusal::Os { what, kind: e.kind() };
        let (kept, back) = (room.clone(), asks.clone());
        let room_thread = std::thread::Builder::new()
            .name("shards-api-image-room".into())
            .spawn(move || keep_room(&*kept, &room_asks, &back))
            .map_err(os("the pools' room thread"))?;
        let pools = Pools {
            hosts: hosts.clone(),
            room: room.clone(),
            to_room,
            keep,
            pools: HashMap::new(),
            claims: HashMap::new(),
            ending: HashMap::new(),
            enders: Vec::new(),
            asks: asks.clone(),
            next: 0,
        };
        // Its room thread ends as `pools` goes, with the last sender to it.
        let thread = std::thread::Builder::new()
            .name("shards-api-image-pools".into())
            .spawn(move || serve(pools, &received))
            .map_err(os("the pools' thread"))?;
        Ok(Pooling {
            hosts,
            room,
            asks,
            thread: Some(thread),
            room_thread: Some(room_thread),
        })
    }

    /// The run `request` of `spec`: on a warm host of its pool where one is ready and takes
    /// it, its room given to the run (`Taken`) here, else on a host of its own; its pool told
    /// of it, and of its end as its guest drops. A run that is no pool's ([`pool_of`]) gets a
    /// host of its own and its pool hears nothing. A pool whose warm host a run took is
    /// refilled once that run has started, or failed to: a host starting warm meanwhile would
    /// spend the CPU its start runs on (D253 amended, PM M593).
    pub(super) fn boot(
        &self,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        let Some(Of {
            key,
            exemplar,
            size,
            request,
        }) = pool_of(spec, request)?
        else {
            return self.hosts.fresh(spec, request, patience, stop);
        };
        let (reply, warm) = mpsc::channel();
        let asked = self.asks.send(Ask::Take {
            key: key.clone(),
            exemplar: Box::new(exemplar),
            size,
            reply,
        });
        let warm = asked.ok().and_then(|()| warm.recv().ok()).flatten();
        let took = warm.is_some();
        if took {
            self.room.give(&size, Gave::Taken);
        }
        let booted = run(
            &self.hosts,
            &self.asks,
            warm,
            key.clone(),
            spec,
            &request,
            patience,
            stop,
        );
        if took {
            let _ = self.asks.send(Ask::Refill { key });
        }
        booted
    }

    /// Holds a warm host of `spec`'s pool for one box, at once, where one is ready: its room
    /// given to the box (`Taken`) here, and counted as its pool's run. None where none is, or
    /// the run is no pool's.
    pub(super) fn reserve(&self, spec: &ImageSpec, request: &wire::Request) -> Option<Ticket<H>> {
        let Of { key, size, .. } = pool_of(spec, request).ok().flatten()?;
        let (reply, warm) = mpsc::channel();
        self.asks
            .send(Ask::Reserve {
                key: key.clone(),
                reply,
            })
            .ok()?;
        let warm = warm.recv().ok().flatten()?;
        self.room.give(&size, Gave::Taken);
        let _ = self.asks.send(Ask::Refill { key: key.clone() });
        Some(Ticket {
            hosts: self.hosts.clone(),
            asks: self.asks.clone(),
            warm: Some(warm),
            key,
            size,
        })
    }

    /// Ends the coldest hosts until at least `memory` bytes of them are freed, or none is
    /// left: warm ones first, of the pools wanting fewest, then the longest ready; then those
    /// starting. Returns once each one's processes have ended, followed to their end and never
    /// timed (D215, D217), each one's room given (`Ended`) here, with what was freed; or the
    /// first end that could not be followed, typed, every room given all the same. A reserved
    /// host is its box's, never ended here.
    pub(super) fn release(&self, memory: u64) -> Result<Released, ImageRefusal> {
        let (reply, releasing) = mpsc::channel();
        let gone = || ImageRefusal::Host("the pools' thread is gone".into());
        self.asks
            .send(Ask::Release { memory, reply })
            .map_err(|_| gone())?;
        let Releasing { warm, starting, ends } = releasing.recv().map_err(|_| gone())?;
        let mut released = Released::default();
        let mut first = None;
        let mut ended = |size: &WarmHost, outcome: Result<(), ImageRefusal>| {
            self.room.give(size, Gave::Ended);
            released.hosts += 1;
            released.memory = released.memory.saturating_add(size.memory);
            if let Err(e) = outcome {
                first.get_or_insert(e);
            }
        };
        let hosts = &self.hosts;
        let outcomes: Vec<(WarmHost, Result<(), ImageRefusal>)> = std::thread::scope(|scope| {
            let mut ending = Vec::new();
            let mut outcomes = Vec::new();
            for (size, warm) in warm {
                let (hand, handed) = mpsc::channel::<H::Warm>();
                let ender = std::thread::Builder::new()
                    .name("shards-api-image-end".into())
                    .spawn_scoped(scope, move || {
                        handed.recv().map_or(Ok(()), |warm| hosts.end(warm))
                    });
                let handed = match ender {
                    Ok(ender) => hand
                        .send(warm)
                        .map(|()| ending.push((size, ender)))
                        .map_err(|mpsc::SendError(warm)| warm),
                    Err(_) => Err(warm),
                };
                if let Err(warm) = handed {
                    outcomes.push((size, hosts.end(warm)));
                }
            }
            for (size, ender) in ending {
                let outcome = ender.join().unwrap_or_else(|_| {
                    Err(ImageRefusal::Host("a released host's end did not return".into()))
                });
                outcomes.push((size, outcome));
            }
            outcomes
        });
        for (size, outcome) in outcomes {
            ended(&size, outcome);
        }
        for _ in 0..starting {
            match ends.recv() {
                Ok(Ending::Gone(size)) => ended(&size, Ok(())),
                Ok(Ending::Warm(size, warm)) => ended(&size, hosts.end(warm)),
                Err(_) => break,
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(released),
        }
    }

    /// What each pool holds of its room now: its warm and starting hosts as claimed, and each
    /// warm one's footprint as its VM process has reported it.
    pub(super) fn held(&self) -> Vec<PoolHeld> {
        let (reply, held) = mpsc::channel();
        if self.asks.send(Ask::Held { reply }).is_err() {
            return Vec::new();
        }
        held.recv().unwrap_or_default()
    }
}

impl<H: Hosts> Drop for Pooling<H> {
    fn drop(&mut self) {
        let _ = self.asks.send(Ask::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Its last sender went with the pools' thread: it ends once it has made every call.
        if let Some(thread) = self.room_thread.take() {
            let _ = thread.join();
        }
    }
}

/// A warm host held for one box ([`super::WarmTicket`]): its room the box's since it was
/// reserved. Dropped unused, it goes back to its pool under a claim of its own, or ends.
pub(super) struct Ticket<H: Hosts> {
    hosts: H,
    asks: mpsc::Sender<Ask<H::Warm>>,
    warm: Option<H::Warm>,
    key: Key,
    size: WarmHost,
}

impl<H: Hosts> Ticket<H> {
    /// The run `request` of `spec` on the reserved host, as [`Pooling::boot`] runs one, with
    /// no room given again; a spec of another pool refused, the host back to its pool.
    pub(super) fn boot(
        mut self,
        spec: &ImageSpec,
        request: &wire::Request,
        patience: Duration,
        stop: &BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        let of = pool_of(spec, request)?;
        let asked = of.as_ref().map(|of| of.size.pool);
        let Some(Of { request, .. }) = of.filter(|_| asked == Some(self.size.pool)) else {
            return Err(ImageRefusal::OtherPool {
                reserved: self.size.pool,
                asked,
            });
        };
        let warm = self.warm.take();
        run(
            &self.hosts,
            &self.asks,
            warm,
            self.key.clone(),
            spec,
            &request,
            patience,
            stop,
        )
    }
}

impl<H: Hosts> Drop for Ticket<H> {
    fn drop(&mut self) {
        if let Some(warm) = self.warm.take() {
            // The pools gone, it ends as its value drops: its room is its box's.
            let _ = self.asks.send(Ask::Return {
                key: self.key.clone(),
                warm,
            });
        }
    }
}

/// The run on `warm` where it takes it, else on a host of its own, as a pooled run: its pool
/// told of its end.
#[allow(clippy::too_many_arguments)]
fn run<H: Hosts>(
    hosts: &H,
    asks: &mpsc::Sender<Ask<H::Warm>>,
    warm: Option<H::Warm>,
    key: Key,
    spec: &ImageSpec,
    request: &wire::Request,
    patience: Duration,
    stop: &BootStop,
) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
    let guest = match warm.map(|w| hosts.take(w, spec, request, patience, stop)) {
        Some(Ok(guest)) => Ok(guest),
        Some(Err(Taken::Refused(refused))) => Err(refused),
        Some(Err(Taken::Untaken | Taken::Unfit)) | None => hosts.fresh(spec, request, patience, stop),
    };
    match guest {
        Ok(guest) => Ok(Box::new(Pooled {
            guest,
            key: Some(key),
            asks: asks.clone(),
        })),
        Err(refused) => {
            let _ = asks.send(Ask::Ended { key });
            Err(refused)
        }
    }
}

/// A run of a pool's: its guest, whose drop tells its pool the run has ended. Its room went to
/// its run as it took its host, so it gives none.
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

    fn published(&self) -> Vec<wire::Bound> {
        self.guest.published()
    }

    fn publish(&self, publish: wire::Publish, patience: Duration) -> Result<Vec<wire::Bound>, ImageRefusal> {
        self.guest.publish(publish, patience)
    }

    fn unpublish(
        &self,
        proto: wire::Proto,
        host: std::net::SocketAddr,
        patience: Duration,
    ) -> Result<(), ImageRefusal> {
        self.guest.unpublish(proto, host, patience)
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

    fn wait_channel(
        &self,
        member: &super::MemberName,
        channel: wire::Channel,
        timeout: Duration,
    ) -> Result<Option<super::ChannelEnd>, ImageRefusal> {
        self.guest.wait_channel(member, channel, timeout)
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

    fn rate_period(&self) -> Option<Duration> {
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

    fn pause(&self, patience: Duration) -> Result<wire::Paused, ImageRefusal> {
        self.guest.pause(patience)
    }

    fn resume(&self, patience: Duration) -> Result<wire::Paused, ImageRefusal> {
        self.guest.resume(patience)
    }

    fn stop(&self) -> Result<(), ImageRefusal> {
        self.guest.stop()
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<wire::RunEnded>, ImageRefusal> {
        self.guest.wait_for(timeout)
    }

    fn keep_diagnostics(&self, to: std::fs::File, bound: u64) -> Result<(), ImageRefusal> {
        self.guest.keep_diagnostics(to, bound)
    }

    fn take_gpu_report(&self, patience: Duration) -> Result<crate::physical::GpuReport, ImageRefusal> {
        self.guest.take_gpu_report(patience)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Condvar, Mutex};

    /// How long a test waits for what its pools' thread does at most: a hang's bound alone,
    /// each wait ending on the fact it waits for.
    const PATIENCE: Duration = Duration::from_secs(30);

    const MIB: u64 = 1 << 20;

    /// What a simulated run host has done, and how it is told to behave.
    #[derive(Debug, Default)]
    struct Log {
        begun: u64,
        /// Warm hosts alive: ready, held by a pool or taken, and not yet ended.
        warm: usize,
        taken: usize,
        fresh: usize,
        killed: HashSet<u64>,
        ended_runs: usize,
        /// Warm hosts ended by their pools, each followed to its end.
        ended_warm: usize,
        /// Its warm starts wait until let go, or killed.
        hold: bool,
        /// Its warm starts fail.
        failing: bool,
        /// Warm starts that failed.
        failed: usize,
        /// Its warm hosts refuse their runs as another microVM's.
        unfit: bool,
        /// One spare ended before its CREATE.
        untaken: bool,
        /// What the hosts that warm from now on report of their footprint.
        report: Option<u64>,
        /// Its takes wait until let go.
        hold_take: bool,
        /// A take is under way.
        taking: bool,
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
    struct Warmed {
        sim: Arc<Sim>,
        footprint: Option<u64>,
    }

    impl Drop for Warmed {
        fn drop(&mut self) {
            self.sim.log.lock().unwrap().warm -= 1;
            self.sim.changed.notify_all();
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
        fn published(&self) -> Vec<wire::Bound> {
            Vec::new()
        }
        fn publish(&self, _: wire::Publish, _: Duration) -> Result<Vec<wire::Bound>, ImageRefusal> {
            none()
        }
        fn unpublish(
            &self,
            _: wire::Proto,
            _: std::net::SocketAddr,
            _: Duration,
        ) -> Result<(), ImageRefusal> {
            none()
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
        fn wait_channel(
            &self,
            _: &super::super::MemberName,
            _: wire::Channel,
            _: Duration,
        ) -> Result<Option<super::super::ChannelEnd>, ImageRefusal> {
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
        fn rate_period(&self) -> Option<Duration> {
            None
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
        fn pause(&self, _: Duration) -> Result<wire::Paused, ImageRefusal> {
            none()
        }
        fn resume(&self, _: Duration) -> Result<wire::Paused, ImageRefusal> {
            none()
        }
        fn stop(&self) -> Result<(), ImageRefusal> {
            Ok(())
        }
        fn wait_for(&self, _: Duration) -> Result<Option<wire::RunEnded>, ImageRefusal> {
            Ok(None)
        }
        fn keep_diagnostics(&self, _: std::fs::File, _: u64) -> Result<(), ImageRefusal> {
            none()
        }
        fn take_gpu_report(&self, _: Duration) -> Result<crate::physical::GpuReport, ImageRefusal> {
            none()
        }
    }

    impl Hosts for Hosted {
        type Starting = u64;
        type Warm = Warmed;
        type Kill = u64;

        fn begin(&self, _: &HostOf, _: &wire::Request) -> Result<(u64, u64), ImageRefusal> {
            let mut log = self.log();
            let id = log.begun;
            log.begun += 1;
            drop(log);
            self.0.changed.notify_all();
            Ok((id, id))
        }

        fn warmed(&self, id: u64) -> Result<Warmed, ImageRefusal> {
            let mut log = self
                .0
                .changed
                .wait_while(self.log(), |l| l.hold && !l.killed.contains(&id))
                .unwrap();
            if log.killed.contains(&id) {
                return none();
            }
            if log.failing {
                log.failed += 1;
                drop(log);
                self.0.changed.notify_all();
                return none();
            }
            log.warm += 1;
            let footprint = log.report;
            drop(log);
            self.0.changed.notify_all();
            Ok(Warmed {
                sim: self.0.clone(),
                footprint,
            })
        }

        fn kill(&self, id: &u64) {
            self.tell(|l| {
                l.killed.insert(*id);
            });
        }

        fn footprint(&self, warm: &Warmed) -> Option<u64> {
            warm.footprint
        }

        fn end(&self, warm: Warmed) -> Result<(), ImageRefusal> {
            drop(warm);
            self.tell(|l| l.ended_warm += 1);
            Ok(())
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
            self.tell(|l| l.taking = true);
            let held = self.0.changed.wait_while(self.log(), |l| l.hold_take).unwrap();
            drop(held);
            self.tell(|l| l.taking = false);
            if std::mem::take(&mut self.log().untaken) {
                return Err(Taken::Untaken);
            }
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

    /// What a counting room has been asked, and what it holds.
    #[derive(Debug, Default)]
    struct RoomLog {
        /// How many hosts fit it.
        fits: usize,
        held: usize,
        held_memory: u64,
        claims: usize,
        refused: usize,
        taken: usize,
        ended: usize,
        failed: usize,
        /// Gives with no claim held: none, ever.
        overgiven: usize,
        /// Each claim's host's vCPUs, in the order claimed.
        claimed: Vec<u32>,
    }

    #[derive(Debug, Default)]
    struct RoomSim {
        log: Mutex<RoomLog>,
        changed: Condvar,
        /// What each claim takes first: a lock its owner may hold across a call of the pools.
        gate: Mutex<()>,
    }

    /// A counting room, of `fits` hosts.
    #[derive(Debug, Clone, Default)]
    struct Room(Arc<RoomSim>);

    impl Room {
        fn fits(fits: usize) -> Room {
            let room = Room::default();
            room.log().fits = fits;
            room
        }

        fn log(&self) -> std::sync::MutexGuard<'_, RoomLog> {
            self.0.log.lock().unwrap()
        }

        fn until(&self, what: &str, holds: impl Fn(&RoomLog) -> bool) {
            let (log, waited) = self
                .0
                .changed
                .wait_timeout_while(self.log(), PATIENCE, |l| !holds(l))
                .unwrap();
            assert!(!waited.timed_out(), "{what}: {log:?}");
        }

        /// Every claim given back exactly once: what a room holds once its pools are gone.
        fn balanced(&self) {
            let log = self.log();
            assert_eq!(log.held, 0, "{log:?}");
            assert_eq!(log.held_memory, 0, "{log:?}");
            assert_eq!(log.claims, log.taken + log.ended + log.failed, "{log:?}");
            assert_eq!(log.overgiven, 0, "{log:?}");
        }
    }

    impl WarmRoom for Room {
        fn claim(&self, host: &WarmHost) -> bool {
            let _gate = self.0.gate.lock().unwrap();
            let mut log = self.log();
            let claimed = log.held < log.fits;
            log.claimed.push(host.vcpus);
            if claimed {
                log.held += 1;
                log.held_memory += host.memory;
                log.claims += 1;
            } else {
                log.refused += 1;
            }
            drop(log);
            self.0.changed.notify_all();
            claimed
        }

        fn give(&self, host: &WarmHost, why: Gave) {
            let mut log = self.log();
            if log.held == 0 {
                log.overgiven += 1;
            } else {
                log.held -= 1;
                log.held_memory -= host.memory;
            }
            match why {
                Gave::Taken => log.taken += 1,
                Gave::Ended => log.ended += 1,
                Gave::Failed => log.failed += 1,
            }
            drop(log);
            self.0.changed.notify_all();
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
            upstream_roots: Vec::new(),
            gpu_report: None,
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
            upstream_roots: Vec::new(),
            gpu_report: None,
        }
    }

    fn sized(memory_mib: u64, vcpus: u32) -> wire::Request {
        wire::Request {
            memory_mib,
            ..request(vcpus, None)
        }
    }

    fn boot(pools: &Pooling<Hosted>, request: &wire::Request) -> Box<dyn ImageGuest> {
        pools.boot(&spec(), request, PATIENCE, &BootStop::new()).unwrap()
    }

    /// D253's pools, cold: a pool's first run gets a host of its own, nothing being warm, and
    /// its pool starts one ahead of the next; the next run of the same microVM, its deadline
    /// its own, takes that warm host, and its pool starts more (as many as its burst, which
    /// its warm hosts' start times as this host runs them make one or two runs); a run of
    /// another microVM's starts a pool of its own, its first run a host of its own.
    #[test]
    fn a_pools_first_run_starts_its_own_host_and_the_next_takes_a_warm_one() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        let first = boot(&pools, &request(1, Some(5)));
        hosted.until("one warm ahead of the next run", |l| l.fresh == 1 && l.warm == 1);
        until_ready(&pools, 1);
        let next = boot(&pools, &request(1, Some(9)));
        assert_eq!(next.started_from(), wire::StartedFrom::Warm);
        hosted.until("the warm one taken, another started", |l| {
            l.taken == 1 && l.fresh == 1 && l.begun >= 2 && l.warm >= 1
        });
        let other = boot(&pools, &request(2, None));
        hosted.until("another microVM's run on a host of its own", |l| l.fresh == 2);
        drop((first, next, other));
        hosted.until("every run's end told", |l| l.ended_runs == 3);
        drop(pools);
        hosted.until("every warm host ended with its pools", |l| l.warm == 0);
        room.balanced();
    }

    /// A pool whose warm host a run takes is refilled once that run has started, never while
    /// it starts (PM M593: a host starting warm would spend the CPU its start runs on). Do: hold
    /// a take of pool 1's warm host, then boot pool 2's first run, whose pool claims a host of
    /// the room as its run starts. Expect: the room sees pool 2's claim alone while pool 1's
    /// take is held; pool 1's after it is let go.
    #[test]
    fn a_taken_pool_is_refilled_once_its_run_has_started() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        let first = boot(&pools, &request(1, None));
        until_ready(&pools, 1);
        let before = room.log().claimed.len();
        hosted.tell(|l| l.hold_take = true);
        std::thread::scope(|s| {
            let taking = s.spawn(|| boot(&pools, &request(1, None)));
            hosted.until("pool 1's warm host being taken", |l| l.taking);
            let other = boot(&pools, &request(2, None));
            room.until("pool 2's claim", |l| l.claimed[before..].contains(&2));
            assert_eq!(
                room.log().claimed[before..],
                [2],
                "claimed while pool 1's take is held"
            );
            hosted.tell(|l| l.hold_take = false);
            let taken = taking.join().unwrap();
            assert_eq!(taken.started_from(), wire::StartedFrom::Warm);
            // Its refill claims as many as its burst says: one or two (its first run's and this).
            room.until("pool 1 refilled once its run has started", |l| {
                l.claimed[before..].starts_with(&[2, 1])
            });
            drop((taken, other));
        });
        drop(first);
        hosted.until("every run's end told", |l| l.ended_runs == 3);
        drop(pools);
        hosted.until("every warm host ended with its pools", |l| l.warm == 0);
        room.balanced();
    }

    #[test]
    fn a_definitely_untaken_spare_leaves_its_run_a_fresh_host() {
        let (hosted, room) = (Hosted::default(), Room::fits(1));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 1);
        hosted.tell(|l| l.untaken = true);
        let guest = boot(&pools, &request(1, None));
        assert_eq!(hosted.log().fresh, 2);
        assert_eq!(hosted.log().taken, 0);
        assert_eq!(room.log().taken, 1, "its room went to the run that took it");
        drop(guest);
        drop(pools);
        room.balanced();
    }

    /// D253's pools: a warm host that finds its run another microVM's (`Unfit`) gives the run
    /// a host of its own; one whose start fails is no warm host, its runs getting hosts of
    /// their own, and its room is given back as `Failed`.
    #[test]
    fn a_warm_host_that_does_not_fit_or_start_leaves_its_run_a_host_of_its_own() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        hosted.until("one warm", |l| l.warm == 1);
        until_ready(&pools, 1);
        hosted.tell(|l| {
            l.unfit = true;
            l.failing = true;
        });
        let run = boot(&pools, &request(1, None));
        // Every start after the first failed: one or two, as many as its burst.
        hosted.until(
            "the unfit one's run on a host of its own, its refill failed",
            |l| l.fresh == 2 && l.taken == 0 && l.failed >= 1 && l.failed + 1 == l.begun as usize,
        );
        room.until("each failed start's room given back", |r| r.failed >= 1);
        assert_eq!(hosted.log().warm, 0);
        drop(run);
        drop(boot(&pools, &request(1, None)));
        hosted.until("a failed start's run on a host of its own", |l| l.fresh == 3);
        drop(pools);
        room.balanced();
    }

    /// D253's pools sized by D26's rule: runs that come within one refill window are one
    /// burst, which a pool keeps warm, as far as its room admits; its cold start is RFC
    /// 6298's initial window, as no refill has been timed while its warm starts wait (`hold`).
    /// Dropping the pools ends every warm host and kills every starting one, each one's room
    /// given back.
    #[test]
    fn a_pool_keeps_its_burst_within_its_room_and_its_owner_ends_them_all() {
        let (hosted, room) = (Hosted::default(), Room::fits(2));
        hosted.tell(|l| l.hold = true);
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        let runs: Vec<_> = (0..3).map(|_| boot(&pools, &request(1, None))).collect();
        hosted.until("the burst's hosts starting, as many as its room admits", |l| {
            l.fresh == 3 && l.begun == 2
        });
        room.until("the claim past its room refused", |r| r.refused >= 1);
        drop(runs);
        drop(pools);
        hosted.until("the starting ones killed with the pools", |l| {
            l.killed.len() == 2 && l.warm == 0
        });
        room.balanced();
        assert_eq!(room.log().ended, 2);
    }

    /// D253's pools: a pool keeps nothing past its keep-alive from its last run's end, its
    /// warm hosts ended then and their room given back; while a run of it goes on, its
    /// keep-alive has not begun.
    #[test]
    fn a_pools_warm_hosts_end_past_its_keep_alive() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let keep = Duration::from_millis(200);
        let pools = Pooling::new(hosted.clone(), room.clone(), keep).unwrap();
        let run = boot(&pools, &request(1, None));
        hosted.until("one warm", |l| l.warm == 1);
        std::thread::sleep(keep * 2);
        assert_eq!(hosted.log().warm, 1, "a pool whose run goes on keeps its host");
        drop(run);
        hosted.until("its warm host ended past its keep-alive", |l| l.warm == 0);
        room.until("its room given back as it ended", |r| r.ended == 1 && r.held == 0);
        drop(pools);
        room.balanced();
    }

    /// D233 through D253's pools: a run that asks for its GPU report gets a host of its own and
    /// starts no warm one, as a warm host's VM starts before its run names the report's scope;
    /// the same run asking none is pooled as every run is.
    #[test]
    fn a_run_asking_its_gpu_report_gets_a_host_of_its_own_and_no_pool() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        let gpus = vec![crate::PhysicalGpu::MetalRegistry(7)];
        let scoped = wire::Request {
            gpu_devices: gpus.clone(),
            gpu_report: Some([3; 16]),
            ..request(1, None)
        };
        let run = boot(&pools, &scoped);
        let other = boot(&pools, &scoped);
        assert_eq!(hosted.log().fresh, 2);
        drop((run, other));
        assert_eq!(hosted.log().begun, 0, "no warm host for a run that is no pool's");
        assert!(pools.held().is_empty());
        let unscoped = wire::Request {
            gpu_devices: gpus,
            ..request(1, None)
        };
        let pooled = boot(&pools, &unscoped);
        hosted.until("the unscoped run's pool warms a host", |l| {
            l.fresh == 3 && l.begun == 1
        });
        drop(pooled);
        drop(pools);
        room.balanced();
    }

    /// D253's room: a size a spec leaves to `shards run` (`0`) is the size `shards run` gives
    /// it on this host, so its spec pools with the one that names that size, its warm host's
    /// exemplar and its claim that size; and a run of it takes the other's warm host.
    #[test]
    fn a_spec_of_no_size_pools_with_one_of_the_size_run_gives_it() {
        let (vcpus, memory_mib) = shards_ipc::size::unsized_run();
        let pool = |r: &wire::Request| pool_of(&spec(), r).unwrap().unwrap();
        let named = pool(&sized(memory_mib, vcpus));
        for left in [sized(0, 0), sized(0, vcpus), sized(memory_mib, 0)] {
            let of = pool(&left);
            assert_eq!(of.size, named.size, "{left:?}");
            assert_eq!(of.key, named.key);
            assert_eq!((of.request.vcpus, of.request.memory_mib), (vcpus, memory_mib));
        }
        assert_eq!(named.size.memory, memory_mib << 20);
        assert_eq!(named.size.vcpus, vcpus);
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &sized(memory_mib, vcpus)));
        until_ready(&pools, 1);
        let guest = boot(&pools, &sized(0, 0));
        assert_eq!(guest.started_from(), wire::StartedFrom::Warm);
        assert_eq!(pools.held().len(), 1, "one pool");
        drop(guest);
        drop(pools);
        room.balanced();
    }

    /// D253's room: a room that admits no host starts none, its pool's target simply unmet, and
    /// every run gets a host of its own with nothing refused.
    #[test]
    fn a_room_that_refuses_starts_no_host() {
        let (hosted, room) = (Hosted::default(), Room::fits(0));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        for _ in 0..3 {
            drop(boot(&pools, &request(1, None)));
        }
        room.until("its claims refused", |r| r.refused >= 1);
        assert_eq!(hosted.log().begun, 0);
        assert_eq!(hosted.log().fresh, 3);
        let held = pools.held();
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].warm, held[0].starting, held[0].memory), (0, 0, 0));
        drop(pools);
        room.balanced();
        assert_eq!(room.log().claims, 0);
    }

    /// D253's room: what the pools hold is what the room has claimed less what it was given,
    /// each pool's at its spec's whole size, with one footprint for each warm host whose
    /// reading is in, none for one still starting.
    #[test]
    fn held_is_its_claims_less_its_gives_with_each_warm_hosts_footprint() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        hosted.tell(|l| l.report = Some(300 * MIB));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 1);
        hosted.tell(|l| l.hold = true);
        drop(boot(&pools, &sized(1024, 2)));
        hosted.until("the other pool's host starting", |l| l.begun == 2);
        let mut held = pools.held();
        held.sort_by_key(|h| h.memory);
        let log = room.log();
        assert_eq!(held.iter().map(|h| h.memory).sum::<u64>(), log.held_memory);
        assert_eq!(held.iter().map(|h| h.warm + h.starting).sum::<usize>(), log.held);
        drop(log);
        assert_eq!(
            (held[0].memory, held[0].vcpus, held[0].warm, held[0].starting),
            (512 * MIB, 1, 1, 0)
        );
        assert_eq!(held[0].footprints, vec![300 * MIB]);
        assert_eq!(
            (held[1].memory, held[1].vcpus, held[1].warm, held[1].starting),
            (1024 * MIB, 2, 0, 1)
        );
        assert!(
            held[1].footprints.is_empty(),
            "a host still starting has no reading"
        );
        assert_ne!(held[0].pool, held[1].pool);
        assert!(held[0].pool.to_string().starts_with("sha256:"));
        drop(pools);
        room.balanced();
    }

    /// D253's room: a run that takes a warm host gives its room once (`Taken`), its guest
    /// nothing more as it drops; a reserved host gives `Taken` as it is reserved and not again
    /// as its ticket boots.
    #[test]
    fn a_taken_host_gives_taken_once_and_its_guest_nothing_more() {
        let (hosted, room) = (Hosted::default(), Room::fits(1));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 1);
        let guest = boot(&pools, &request(1, None));
        assert_eq!(guest.started_from(), wire::StartedFrom::Warm);
        assert_eq!(room.log().taken, 1);
        drop(guest);
        hosted.until("its run's end told", |l| l.ended_runs == 2);
        until_ready(&pools, 1);
        assert_eq!(room.log().taken, 1, "its guest gave nothing as it dropped");
        let ticket = pools.reserve(&spec(), &request(1, None)).unwrap();
        assert_eq!(room.log().taken, 2, "Taken at its reserve");
        let guest = ticket
            .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
            .unwrap();
        assert_eq!(guest.started_from(), wire::StartedFrom::Warm);
        assert_eq!(room.log().taken, 2, "nothing at its ticket's boot");
        drop(guest);
        hosted.until("its run's end told", |l| l.ended_runs == 3);
        assert_eq!(room.log().taken, 2);
        drop(pools);
        room.balanced();
    }

    /// D253's release: the coldest warm hosts end first, of the pools wanting fewest, then the
    /// longest idle; then those starting; each followed to its end and its room given before
    /// it returns, with exactly the bytes it freed.
    #[test]
    fn release_ends_the_coldest_first_and_says_what_it_freed() {
        let (hosted, room) = (Hosted::default(), Room::fits(8));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        // A pool of a burst of two (a GiB each), started first: the longest ready.
        hosted.tell(|l| l.hold = true);
        let burst = [boot(&pools, &sized(1024, 2)), boot(&pools, &sized(1024, 2))];
        hosted.until("its burst's hosts starting", |l| l.begun == 2);
        hosted.tell(|l| l.hold = false);
        until_ready(&pools, 2);
        drop(burst);
        // A pool of one (512 MiB), ready last.
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 3);
        let released = pools.release(1).unwrap();
        assert_eq!(
            released,
            Released {
                hosts: 1,
                memory: 512 * MIB
            },
            "the pool wanting fewest first, however recent"
        );
        assert_eq!(hosted.log().ended_warm, 1, "ended before release returned");
        assert_eq!(room.log().ended, 1, "its room given before release returned");
        let released = pools.release(1024 * MIB + 1).unwrap();
        assert_eq!(
            released,
            Released {
                hosts: 2,
                memory: 2048 * MIB
            }
        );
        assert_eq!(room.log().held, 0);
        // A host still starting is ended too, its room given as its start's wait ends.
        hosted.tell(|l| l.hold = true);
        drop(boot(&pools, &request(1, None)));
        hosted.until("one starting", |l| l.begun == 4);
        room.until("its room claimed", |r| r.held == 1);
        let released = pools.release(u64::MAX).unwrap();
        assert_eq!(
            released,
            Released {
                hosts: 1,
                memory: 512 * MIB
            }
        );
        assert_eq!(room.log().held, 0, "its room given before release returned");
        assert_eq!(pools.release(u64::MAX).unwrap(), Released::default());
        drop(pools);
        room.balanced();
    }

    /// D253's reservation: one warm host for one box, so a second reserve finds none; a
    /// release leaves the reserved host alone, as its room is its box's.
    #[test]
    fn a_reserved_host_is_one_boxs_and_release_leaves_it() {
        let (hosted, room) = (Hosted::default(), Room::fits(1));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 1);
        // Its refill refused, so that the pools hold the reserved host alone.
        room.0.log.lock().unwrap().fits = 0;
        let ticket = pools.reserve(&spec(), &request(1, None)).unwrap();
        room.until("its refill refused", |r| r.refused >= 1);
        assert!(pools.reserve(&spec(), &request(1, None)).is_none());
        assert_eq!(pools.release(u64::MAX).unwrap(), Released::default());
        assert_eq!(hosted.log().warm, 1, "the reserved host lives");
        let guest = ticket
            .boot(&spec(), &request(1, None), PATIENCE, &BootStop::new())
            .unwrap();
        assert_eq!(guest.started_from(), wire::StartedFrom::Warm);
        drop(guest);
        drop(pools);
        room.balanced();
    }

    /// D253's reservation: a ticket dropped unused puts its host back under one new claim, or
    /// ends it where that claim is refused, never giving for its first claim again; one booted
    /// with another pool's spec is refused, typed, and its host goes back. Each reserve's
    /// refill is refused (`fits` 0), so that its pool wants the host back.
    #[test]
    fn a_dropped_ticket_returns_under_a_new_claim_or_ends() {
        let (hosted, room) = (Hosted::default(), Room::fits(1));
        let fits = |n| room.0.log.lock().unwrap().fits = n;
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        drop(boot(&pools, &request(1, None)));
        until_ready(&pools, 1);
        let reserve = |refused| {
            fits(0);
            let ticket = pools.reserve(&spec(), &request(1, None)).unwrap();
            room.until("its refill refused", |r| r.refused >= refused);
            ticket
        };
        let ticket = reserve(1);
        fits(1);
        drop(ticket);
        room.until("its host back under one new claim", |r| r.claims == 2);
        until_ready(&pools, 1);
        let ticket = reserve(2);
        fits(1);
        let other = ticket.boot(&spec(), &sized(1024, 2), PATIENCE, &BootStop::new());
        assert!(
            matches!(other, Err(ImageRefusal::OtherPool { asked: Some(_), .. })),
            "{:?}",
            other.map(|_| ())
        );
        room.until("its host back under one new claim", |r| r.claims == 3);
        until_ready(&pools, 1);
        let ticket = reserve(3);
        let ended = hosted.log().ended_warm;
        let given = {
            let r = room.log();
            r.taken + r.ended + r.failed
        };
        drop(ticket);
        hosted.until("its host ended, its new claim refused", |l| {
            l.ended_warm == ended + 1
        });
        let r = room.log();
        assert_eq!(
            r.taken + r.ended + r.failed,
            given,
            "no second give for its first claim"
        );
        assert_eq!(r.taken, 3);
        drop(r);
        drop(pools);
        room.balanced();
    }

    /// D253's room: a room whose claim waits on a lock its owner holds across a call of the
    /// pools holds up none of their answers; its claim goes on once the lock is let go.
    #[test]
    fn a_room_waiting_on_its_owners_lock_holds_up_no_answer() {
        let (hosted, room) = (Hosted::default(), Room::fits(4));
        let pools = Pooling::new(hosted.clone(), room.clone(), KEEP_TEST).unwrap();
        let owner = room.0.gate.lock().unwrap();
        // Its refill's claim waits on the lock.
        drop(boot(&pools, &request(1, None)));
        assert_eq!(pools.release(u64::MAX).unwrap(), Released::default());
        assert!(pools.reserve(&spec(), &request(1, None)).is_none());
        assert_eq!(pools.held().len(), 1);
        drop(boot(&pools, &request(1, None)));
        assert_eq!(hosted.log().begun, 0);
        drop(owner);
        hosted.until("its claim went on", |l| l.begun >= 1);
        drop(pools);
        room.balanced();
    }

    /// Waits until `pools` hold `ready` warm hosts ready, as their thread holds them: a host
    /// warm is its pool's only once its thread has taken it in, after the host's own word.
    fn until_ready(pools: &Pooling<Hosted>, ready: usize) {
        let deadline = Instant::now() + PATIENCE;
        while pools.held().iter().map(|h| h.warm).sum::<usize>() != ready {
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
