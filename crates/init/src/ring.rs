//! The relay's end of an engine ring (D230): slots of the relay's own memory, each made as
//! a stream starts, pinned, offered in its connection's hello, grown as the stream shows its
//! ring holds it back, and freed once the device has said it let go ([`abi::DETACH`]).
//!
//! A slot is a memory file the relay maps alone, registered as a fixed buffer of an
//! io_uring instance that init made before it forked the relay and holds until the VM
//! ends: io_uring pins a fixed buffer's pages long-term (`FOLL_LONGTERM`), so they stay at
//! the guest-physical addresses the host was given, and since init's descriptor keeps the
//! instance, a relay that dies leaves them pinned rather than handing them to the guest's
//! allocator while the host may still write them. The kernel counts them pinned to init,
//! whose memory the instance was made in (`VmPin` of `/proc/1/status`; Linux v6.18
//! io_uring/io_uring.c 3858, io_uring/rsrc.c 68-80).
//!
//! The relay frees a slot only once the device has said it let go of it: until then the
//! device may write it. A slot whose word never comes (its connection reset by the device's
//! own reset, by a restore, or by an RST the guest's credit had no room before) stays
//! pinned, and counted, until the VM ends.
//!
//! Supply (D188): pins come out of the room init is told. The relay joins the run's cgroup
//! as it makes its first slot (`run/engine`, a leaf beside the workload's), so that its
//! slots are the run's memory, which the budget's hold, the give-back and the run's metrics
//! read. A slot is made only where the room covers it past the room the run needs to go on
//! (with no budget, where the guest's available memory does): a stream the room cannot fit
//! goes over plain vsock. A told room short of the run's working room has idle slots let go
//! until it is not; a give-back (`RECLAIM`) lets every idle slot go. A live stream keeps
//! its slot, and grows no further, until it goes idle or ends.
//!
//! How large a stream's rings are it learns from itself (R03): a ring its producer filled
//! whole within one wake of its consumer held it back, and is swapped for one twice as large
//! ([`abi::SWAP`]); the most a ring held at its consumer's wake is what it needed, from which
//! the next stream starts.
//!
//! The host is trusted with nothing here: the relay never reaches past a slot whatever
//! positions it reads, and a host that writes nonsense spoils only the connection's own
//! stream.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use shards_abi::ring::{self as abi, Extent, Offer, Reply, field};
use shards_abi::run;

/// io_uring's register opcodes and flag this needs (include/uapi/linux/io_uring.h).
const IORING_REGISTER_BUFFERS2: libc::c_uint = 15;
const IORING_REGISTER_BUFFERS_UPDATE: libc::c_uint = 16;
const IORING_RSRC_REGISTER_SPARSE: u32 = 1;
/// The most fixed buffers an io_uring instance registers (io_uring/rsrc.c
/// `IORING_MAX_REG_BUFFERS`, Linux v6.18 l. 35).
const MOST_BUFFERS: u64 = 1 << 14;
/// The descriptors a ringed stream holds: its client's socket, its vsock socket, and the
/// eventfd its pool wakes it by (its slot's memory file is closed once mapped).
const STREAM_FDS: u64 = 3;
/// The slots a stream holds at most: its own, and as it swaps, the next.
const STREAM_SLOTS: u64 = 2;

/// `struct io_uring_rsrc_register`.
#[repr(C)]
struct RsrcRegister {
    nr: u32,
    flags: u32,
    resv2: u64,
    data: u64,
    tags: u64,
}

/// `struct io_uring_rsrc_update2`.
#[repr(C)]
struct RsrcUpdate2 {
    offset: u32,
    resv: u32,
    data: u64,
    tags: u64,
    nr: u32,
    resv2: u32,
}

/// An io_uring instance whose fixed buffers pin a relay's slots: init's, made before it
/// forks the relay, which inherits it. None where the guest's kernel has no io_uring, or
/// has it turned off (`kernel.io_uring_disabled`): its relay then offers no ring.
pub fn pinner() -> Option<OwnedFd> {
    // `struct io_uring_params`, 120 bytes, all zero: the kernel's defaults.
    let mut params = [0u64; 15];
    // SAFETY: io_uring_setup(2) with one entry and a zeroed params struct it fills.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1 as libc::c_long, params.as_mut_ptr()) };
    let fd = RawFd::try_from(fd).ok().filter(|fd| *fd >= 0)?;
    // SAFETY: a descriptor io_uring_setup just made, close-on-exec, which nothing else owns.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What init's builtins tell the relay of its run's room (D188), in a page init maps shared
/// before it forks the relay ([`share`]): `ROOM`'s room ([`told`]) and `RECLAIM`'s
/// give-backs ([`give_back`]), and what the relay gave back, for `RECLAIM`'s answer
/// ([`given`]).
#[repr(C)]
#[derive(Debug, Default)]
struct Shared {
    /// The relay has made a slot: before it, init's builtins tell it nothing, and a run
    /// whose relay makes none goes as it did before rings.
    made: AtomicU32,
    /// A room was told (1), or the run is held to no budget (0), as the last `ROOM` said.
    budgeted: AtomicU32,
    /// The last room told, in signed bytes.
    room: AtomicI64,
    /// `ROOM`s told and give-backs asked so far: the relay's watcher acts on each change.
    tells: AtomicU64,
    give_backs: AtomicU64,
    /// Bytes of slots let go for give-backs, in all; and of them those `RECLAIM` answered.
    given: AtomicU64,
    reported: AtomicU64,
}

impl Shared {
    fn told(&self, bell: RawFd, room: Option<i64>) {
        if self.made.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.room.store(room.unwrap_or(0), Ordering::SeqCst);
        self.budgeted.store(u32::from(room.is_some()), Ordering::SeqCst);
        self.tells.fetch_add(1, Ordering::SeqCst);
        ring(bell);
    }

    fn give_back(&self, bell: RawFd) {
        if self.made.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.give_backs.fetch_add(1, Ordering::SeqCst);
        ring(bell);
    }

    fn given(&self) -> u64 {
        if self.made.load(Ordering::SeqCst) == 0 {
            return 0;
        }
        let total = self.given.load(Ordering::SeqCst);
        total.saturating_sub(self.reported.swap(total, Ordering::SeqCst))
    }

    fn heard(&self) -> Told {
        Told {
            tells: self.tells.load(Ordering::SeqCst),
            room: (self.budgeted.load(Ordering::SeqCst) != 0).then(|| self.room.load(Ordering::SeqCst)),
            give_backs: self.give_backs.load(Ordering::SeqCst),
        }
    }
}

/// [`Shared`], and the eventfd that wakes the relay's watcher: init's, made before it forks
/// the relay and held for the VM's life.
#[derive(Debug)]
struct Bell {
    shared: &'static Shared,
    fd: RawFd,
}

static BELL: OnceLock<Bell> = OnceLock::new();

/// Makes the page and the eventfd that [`told`] and [`give_back`] reach the relay through:
/// init's, before it forks a relay that may make slots. The eventfd's descriptor, which the
/// relay keeps; none where either cannot be made, and then its relay hears no room.
pub fn share() -> Option<RawFd> {
    if let Some(bell) = BELL.get() {
        return Some(bell.fd);
    }
    let len = usize::try_from(page().ok()?).ok()?;
    // SAFETY: mmap(2) of a new shared anonymous mapping, which nothing references yet.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: eventfd(2) with constant flags.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        // SAFETY: the mapping just made, unmapped once.
        unsafe { libc::munmap(at, len) };
        return None;
    }
    // SAFETY: the page just mapped, zeroed and page-aligned, never unmapped: a `Shared` at
    // its start, atomics valid in their zeroes and far smaller than a page, lives as long as
    // the process, and every process init forks from now shares it.
    let shared = unsafe { &*at.cast::<Shared>() };
    let _ = BELL.set(Bell { shared, fd });
    Some(fd)
}

/// `ROOM`'s word (init's builtin, D188) to the relay: the room told, or none for no budget.
/// Nothing where the relay has made no slot. Never waits for the relay.
pub fn told(room: Option<i64>) {
    if let Some(bell) = BELL.get() {
        bell.shared.told(bell.fd, room);
    }
}

/// `RECLAIM`'s word to the relay: give back every idle slot. Nothing where the relay has
/// made no slot. Never waits for the relay: what it lets go `RECLAIM`'s next answer counts
/// ([`given`]).
pub fn give_back() {
    if let Some(bell) = BELL.get() {
        bell.shared.give_back(bell.fd);
    }
}

/// Bytes of slots the relay let go for give-backs since `RECLAIM` last answered, which its
/// answer adds to what the run's cgroup holds less: a give-back's slots go after the answer
/// that asked for them, and the next answer counts them, each once. A slot let go while a
/// later answer measures the run is counted by that measure too, which the daemon's use
/// bounds: it credits the least of what was said freed and what the microVM's resident
/// files fell by (daemon budget.rs `given_back`).
pub fn given() -> u64 {
    BELL.get().map_or(0, |bell| bell.shared.given())
}

/// What the relay's watcher heard last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    tells: u64,
    room: Option<i64>,
    give_backs: u64,
}

fn ring(bell: RawFd) {
    let one = 1u64;
    // SAFETY: write(2) of a u64 to an eventfd, never blocking; a counter at its most is
    // rung already.
    let _ = unsafe { libc::write(bell, (&raw const one).cast(), std::mem::size_of::<u64>()) };
}

/// The bytes of idle slots a told room has the relay let go (D188): what the room is short
/// of the room the run needs to go on (`working`), past all the microVM holds, its slots
/// among it; none with no budget.
fn owed_for(room: Option<i64>, working: u64) -> u64 {
    room.map_or(0, |room| {
        u64::try_from((i128::from(working) - i128::from(room)).max(0)).unwrap_or(u64::MAX)
    })
}

/// What the run's supply leaves the relay's pins (D188), read as a slot is made or grown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Supply {
    /// The budget's hold of the run (init's `plug::cap`), where a room was told.
    cap: Option<u64>,
    /// What the run's cgroup holds (`memory.current`), the relay's slots among it once the
    /// relay is in it.
    held: u64,
    /// What the guest has available (`MemAvailable`: its free pages less its zones' high
    /// watermarks and reserves, mm/page_alloc.c `calculate_totalreserve_pages`, with what of
    /// its page cache and slab it could reclaim, mm/show_mem.c `si_mem_available`).
    available: u64,
    /// The room the run needs to go on past what it uses (`plug::working_room`, D188), of
    /// the CPUs online as it is read.
    working: u64,
}

impl Supply {
    fn read() -> Supply {
        let cap = Some(crate::plug::cap()).filter(|&c| c != u64::MAX);
        let (held, available) = match cap {
            // Unread, it leaves nothing.
            Some(_) => (
                std::fs::read_to_string(format!("{}/memory.current", crate::run::RUN_CGROUP))
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(u64::MAX),
                0,
            ),
            None => (0, crate::domains::available_memory().unwrap_or(0)),
        };
        Supply {
            cap,
            held,
            available,
            working: crate::plug::working_room(),
        }
    }

    /// What pins may take more of it, `outside` bytes of them held outside the run's cgroup
    /// and `making` being made.
    fn room(&self, outside: u64, making: u64) -> u64 {
        let left = match self.cap {
            Some(cap) => cap.saturating_sub(self.held.saturating_add(outside)),
            None => self.available,
        };
        left.saturating_sub(self.working).saturating_sub(making)
    }
}

/// The relay's slots and streams, behind the lock its streams and its watcher share.
#[derive(Debug)]
pub struct Pool {
    /// The io_uring instance its slots are pinned on: init's (tests' pools have none).
    pinner: Option<OwnedFd>,
    /// Its fixed buffers' entries, each in use or not: registered with its first slot.
    table: Vec<bool>,
    /// What made a slot fail for good: the guest cannot pin, or hides frame numbers.
    broken: Option<io::Error>,
    /// Whether the relay is in the run's cgroup, its slots the run's memory.
    joined: Joined,
    /// Bytes its slots pin: in use, and lost.
    pinned: u64,
    lost: u64,
    /// Bytes of slots being made: admitted, not yet in `pinned`.
    making: u64,
    /// The ring the stream that ended last needed: where the next starts.
    start: u32,
    live: Vec<Live>,
    next: u64,
    /// Rooms told and give-backs asked, as its watcher last heard them.
    tells: u64,
    give_backs: u64,
    /// Bytes of idle slots the last told room still has let go.
    owed: u64,
    /// Bumped as room may have come back: a stream refused growth asks again once it moves.
    room_gen: u64,
    /// Whether its watcher runs.
    watching: bool,
    /// A test's supply, in place of the run's.
    #[cfg(test)]
    supply: Option<Supply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Joined {
    NotYet,
    In,
    Out,
}

/// A ringed stream, as its pool sees it.
#[derive(Debug)]
struct Live {
    id: u64,
    /// The eventfd that wakes it; none where it could not be made, and then it sees the
    /// room as it next goes idle.
    wake: Option<RawFd>,
    /// Asleep with both its rings empty and nothing owed.
    idle: bool,
    /// Bytes its slots pin.
    bytes: u64,
    /// Give-backs it has answered.
    gave: u64,
}

/// A slot being made: its rings, its fixed buffer's entry and the instance it is pinned on.
#[derive(Debug, Clone, Copy)]
struct Making {
    ring: u32,
    index: Option<u32>,
    pinner: Option<RawFd>,
}

/// Why no slot was made.
#[derive(Debug)]
enum Unmade {
    /// Short of memory or descriptors now: this stream goes plain, a later one may not.
    Short,
    /// For good: the guest cannot pin, or hides frame numbers.
    Broken(io::Error),
}

fn unmade(e: io::Error) -> Unmade {
    match e.raw_os_error() {
        Some(libc::ENOMEM | libc::EAGAIN | libc::EMFILE | libc::ENFILE | libc::ENOSPC | libc::EINTR) => {
            Unmade::Short
        }
        _ => Unmade::Broken(e),
    }
}

static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();

fn lock(pool: &Mutex<Pool>) -> MutexGuard<'_, Pool> {
    pool.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The relay's pool, pinned on `pinner`: no slot is made, and nothing pinned, before its
/// first stream.
pub fn start(pinner: OwnedFd) {
    let _ = POOL.set(Mutex::new(Pool::new(Some(pinner))));
}

impl Pool {
    fn new(pinner: Option<OwnedFd>) -> Pool {
        Pool {
            pinner,
            table: Vec::new(),
            broken: None,
            joined: Joined::NotYet,
            pinned: 0,
            lost: 0,
            making: 0,
            start: abi::MIN_RING,
            live: Vec::new(),
            next: 0,
            tells: 0,
            give_backs: 0,
            owed: 0,
            room_gen: 0,
            watching: false,
            #[cfg(test)]
            supply: None,
        }
    }

    fn supply(&self) -> Supply {
        #[cfg(test)]
        if let Some(supply) = self.supply {
            return supply;
        }
        Supply::read()
    }

    /// Bytes of its slots held outside the run's cgroup: all, until the relay is in it.
    fn outside(&self) -> u64 {
        if self.joined == Joined::In { 0 } else { self.pinned }
    }

    fn fits(&self, ring: u32, supply: &Supply) -> bool {
        abi::slot_len(ring) <= supply.room(self.outside(), self.making)
    }

    /// The rings a new stream starts with: the largest the room fits, up to those the last
    /// stream needed; none where not even the smallest fits, or the guest cannot pin.
    fn starting(&self, supply: &Supply) -> Option<u32> {
        if self.broken.is_some() {
            return None;
        }
        let mut ring = self.start.clamp(abi::MIN_RING, abi::MAX_RING);
        loop {
            if self.fits(ring, supply) {
                return Some(ring);
            }
            if ring <= abi::MIN_RING {
                return None;
            }
            ring /= 2;
        }
    }

    /// Takes a fixed buffer's entry for a slot of `ring`'s rings and counts its bytes as
    /// being made, where the room fits it. Its first has the relay join the run's cgroup,
    /// before any slot's pages are charged.
    fn reserve(&mut self, ring: u32, supply: &Supply) -> Option<Making> {
        if self.broken.is_some() || !self.fits(ring, supply) {
            return None;
        }
        let index = match self.pinner.as_ref().map(|p| p.as_raw_fd()) {
            Some(pinner) => Some(self.entry(pinner)?),
            None => None,
        };
        if self.joined == Joined::NotYet && self.pinner.is_some() {
            self.joined = match join(std::path::Path::new(crate::run::RUN_CGROUP)) {
                Ok(()) => Joined::In,
                Err(e) => {
                    let _ = writeln!(
                        io::stderr(),
                        "shards-init: engine rings: the run's cgroup: {e}; its slots counted beside it"
                    );
                    Joined::Out
                }
            };
        }
        self.making = self.making.saturating_add(abi::slot_len(ring));
        Some(Making {
            ring,
            index,
            pinner: self.pinner.as_ref().map(|p| p.as_raw_fd()),
        })
    }

    /// A free entry of its fixed-buffer table, registered with the first.
    fn entry(&mut self, pinner: RawFd) -> Option<u32> {
        if self.table.is_empty() {
            let size = table_size();
            let table = RsrcRegister {
                nr: size,
                flags: IORING_RSRC_REGISTER_SPARSE,
                resv2: 0,
                data: 0,
                tags: 0,
            };
            if let Err(e) = register(pinner, IORING_REGISTER_BUFFERS2, &table) {
                self.fail(unmade(e));
                return None;
            }
            self.table = vec![false; usize::try_from(size).ok()?];
        }
        let at = self.table.iter().position(|used| !used)?;
        *self.table.get_mut(at)? = true;
        u32::try_from(at).ok()
    }

    fn free(&mut self, index: u32) {
        if let Some(used) = usize::try_from(index).ok().and_then(|i| self.table.get_mut(i)) {
            *used = false;
        }
    }

    fn fail(&mut self, why: Unmade) {
        if let Unmade::Broken(e) = why {
            let _ = writeln!(
                io::stderr(),
                "shards-init: engine rings: {e}; plain vsock from here"
            );
            self.broken = Some(e);
        }
    }

    /// A slot made, or not: counted, or its entry and bytes given back.
    fn made(&mut self, making: &Making, made: Result<Slot, Unmade>) -> Option<Slot> {
        let len = abi::slot_len(making.ring);
        self.making = self.making.saturating_sub(len);
        match made {
            Ok(slot) => {
                self.pinned = self.pinned.saturating_add(len);
                Some(slot)
            }
            Err(why) => {
                if let Some(index) = making.index {
                    self.free(index);
                }
                self.fail(why);
                None
            }
        }
    }

    /// Whether its watcher is to start: as its first slot is made, which also lets init's
    /// builtins tell the relay of its room from now ([`Shared::made`]).
    fn first(&mut self) -> bool {
        if self.watching || self.pinned == 0 {
            return false;
        }
        self.watching = true;
        if let Some(bell) = BELL.get() {
            bell.shared.made.store(1, Ordering::SeqCst);
        }
        true
    }

    /// Frees a slot the device has let go of: its pins dropped, its pages the guest's again.
    /// One whose pins cannot be dropped stays, counted, lost.
    fn release(&mut self, slot: Slot, given_back: bool) {
        let len = slot.len_u64();
        if let (Some(pinner), Some(index)) = (self.pinner.as_ref().map(|p| p.as_raw_fd()), slot.index) {
            if let Err(e) = unregister(pinner, index) {
                let _ = writeln!(io::stderr(), "shards-init: engine ring: its pins kept: {e}");
                self.lose(slot);
                return;
            }
            self.free(index);
        }
        drop(slot);
        self.pinned = self.pinned.saturating_sub(len);
        self.room_gen = self.room_gen.wrapping_add(1);
        if given_back && let Some(bell) = BELL.get() {
            bell.shared.given.fetch_add(len, Ordering::SeqCst);
        }
    }

    /// Keeps a slot pinned whose device never said it let go of it: its pages, and its
    /// entry, stay the instance's until the VM ends.
    fn lose(&mut self, slot: Slot) {
        self.lost = self.lost.saturating_add(slot.len_u64());
        let _ = writeln!(
            io::stderr(),
            "shards-init: engine ring: no word of its device; its {} bytes stay pinned",
            slot.len_u64()
        );
    }

    fn enlist(&mut self, wake: Option<RawFd>, bytes: u64) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.live.push(Live {
            id,
            wake,
            idle: false,
            bytes,
            gave: self.give_backs,
        });
        id
    }

    fn discharge(&mut self, id: u64, need: Option<u32>) {
        self.live.retain(|l| l.id != id);
        if let Some(need) = need {
            self.start = need;
        }
    }

    fn resize(&mut self, id: u64, bytes: u64) {
        if let Some(l) = self.live.iter_mut().find(|l| l.id == id) {
            l.bytes = bytes;
        }
    }

    /// Marks stream `id` idle or busy. Whether, idle, the room asks for its slot now: a
    /// give-back it has not answered, or a told room still short, which its slot then pays.
    fn idle(&mut self, id: u64, idle: bool) -> Option<bool> {
        let (give_backs, owed) = (self.give_backs, self.owed);
        let l = self.live.iter_mut().find(|l| l.id == id)?;
        l.idle = idle;
        if !idle {
            return None;
        }
        let given_back = l.gave < give_backs;
        if !given_back && owed == 0 {
            return None;
        }
        l.gave = give_backs;
        let bytes = l.bytes;
        self.owed = owed.saturating_sub(bytes);
        Some(given_back)
    }

    /// What its watcher heard: the idle streams' wakes, each to see whether the room asks
    /// for its slot.
    fn heard(&mut self, told: Told, working: u64) -> Vec<RawFd> {
        if told.tells != self.tells {
            self.tells = told.tells;
            self.owed = owed_for(told.room, working);
        }
        self.give_backs = told.give_backs;
        self.room_gen = self.room_gen.wrapping_add(1);
        self.live
            .iter()
            .filter(|l| l.idle)
            .filter_map(|l| l.wake)
            .collect()
    }
}

/// The entries of the relay's fixed-buffer table, of its descriptor limit as the table is
/// made ([`table_for`]).
fn table_size() -> u32 {
    // SAFETY: an all-zero rlimit, filled by getrlimit(2).
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: getrlimit(2) into the struct just made.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } == 0 {
        lim.rlim_cur
    } else {
        0
    };
    u32::try_from(table_for(soft)).unwrap_or(1)
}

/// As many slots as the relay's streams can hold at once, which its descriptors bound
/// (`soft`: [`STREAM_FDS`] each, [`STREAM_SLOTS`] slots each), within the most an
/// instance registers ([`MOST_BUFFERS`]); at least one.
fn table_for(soft: u64) -> u64 {
    (soft / STREAM_FDS)
        .saturating_mul(STREAM_SLOTS)
        .clamp(1, MOST_BUFFERS)
}

/// Puts the relay in the leaf `engine` of the run's cgroup `run`, so that its slots are
/// the run's memory: a leaf, since a parent with controllers enabled holds no process
/// (cgroup-v2.rst, "No Internal Process Constraint"); before any slot is made, since a
/// page's charge stays where it was made (cgroup-v2.rst, "Memory Ownership").
fn join(run: &std::path::Path) -> io::Result<()> {
    let leaf = run.join("engine");
    match std::fs::create_dir(&leaf) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    std::fs::write(leaf.join("cgroup.procs"), "0")
}

/// The relay's watcher: wakes on init's bell, and has the pool act on what was told.
fn watch(pool: &'static Mutex<Pool>) {
    let Some(bell) = BELL.get() else {
        return;
    };
    let mut counter = [0u8; 8];
    loop {
        let mut fds = [libc::pollfd {
            fd: bell.fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: poll(2) of one initialized entry.
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return;
        }
        // SAFETY: read(2) of the eventfd's counter into a local, never blocking.
        let _ = unsafe { libc::read(bell.fd, counter.as_mut_ptr().cast(), counter.len()) };
        let wakes = lock(pool).heard(bell.shared.heard(), crate::plug::working_room());
        for fd in wakes {
            ring(fd);
        }
    }
}

/// A slot for a stream: of `ring`'s rings, or where none is asked the stream's start, made
/// where the room fits it; none where it does not, or the guest cannot make one now.
fn take(pool: &'static Mutex<Pool>, supply: &Supply, ring: Option<u32>) -> Option<Slot> {
    let making = {
        let mut p = lock(pool);
        let ring = match ring {
            Some(ring) => ring,
            None => p.starting(supply)?,
        };
        p.reserve(ring, supply)?
    };
    let made = Slot::make(&making);
    let (slot, first) = {
        let mut p = lock(pool);
        let slot = p.made(&making, made);
        (slot, p.first())
    };
    if first && BELL.get().is_some() {
        // One that cannot run leaves the room unheard, and the slots to their streams' ends.
        let _ = std::thread::Builder::new()
            .name("engine-room".into())
            .spawn(move || watch(pool));
    }
    slot
}

/// A slot: the control page and both rings, mapped here, pinned, and named by the
/// guest-physical addresses of its pages.
#[derive(Debug)]
pub struct Slot {
    base: NonNull<u8>,
    len: usize,
    ring: u32,
    extents: Vec<Extent>,
    /// Its fixed buffer's entry, where it is pinned.
    index: Option<u32>,
}

// SAFETY: the mapping is this slot's own, reached only through atomics and system calls;
// moving it between the relay's threads changes nothing of that.
unsafe impl Send for Slot {}
// SAFETY: as above: shared, its control page is reached through atomics alone and its
// rings through the kernel's copies and copies between slots, never through a reference.
unsafe impl Sync for Slot {}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: the mapping `make` made, unmapped once; its pages stay where pinned.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

impl Slot {
    /// A slot as `making` says: a memory file of its length, mapped, populated, the file
    /// closed (the mapping keeps it), kept from forks, pinned at its entry, its frames read.
    fn make(making: &Making) -> Result<Slot, Unmade> {
        let len = usize::try_from(abi::slot_len(making.ring))
            .map_err(|_| Unmade::Broken(io::Error::other("slot length")))?;
        // SAFETY: memfd_create(2) of a NUL-terminated name.
        let fd = unsafe { libc::memfd_create(c"shards-engine-ring".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(unmade(io::Error::last_os_error()));
        }
        // SAFETY: a descriptor just made, which nothing else owns.
        let memory = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = libc::off_t::try_from(len).map_err(|_| Unmade::Broken(io::Error::other("slot length")))?;
        // SAFETY: ftruncate(2) of our own memory file.
        if unsafe { libc::ftruncate(memory.as_raw_fd(), size) } != 0 {
            return Err(unmade(io::Error::last_os_error()));
        }
        // SAFETY: a shared mapping of the whole file, populated, owned by the slot.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                memory.as_raw_fd(),
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(unmade(io::Error::last_os_error()));
        }
        drop(memory);
        let base = NonNull::new(at.cast::<u8>())
            .ok_or_else(|| Unmade::Broken(io::Error::other("a null mapping")))?;
        let mut slot = Slot {
            base,
            len,
            ring: making.ring,
            extents: Vec::new(),
            index: None,
        };
        // No process init forks later gets it.
        // SAFETY: madvise(2) of the slot's own mapping.
        if unsafe { libc::madvise(at, len, libc::MADV_DONTFORK) } != 0 {
            return Err(unmade(io::Error::last_os_error()));
        }
        match (making.pinner, making.index) {
            (Some(pinner), Some(index)) => {
                pin(pinner, index, at, len).map_err(unmade)?;
                slot.index = Some(index);
                let named = frames(at as usize, len).and_then(|frames| {
                    abi::extents(&frames, page()?).ok_or_else(|| io::Error::other("frame numbers"))
                });
                match named {
                    Ok(extents) => slot.extents = extents,
                    Err(e) => {
                        // Never offered: its pins go with it.
                        let _ = unregister(pinner, index);
                        slot.index = None;
                        return Err(unmade(e));
                    }
                }
            }
            #[cfg(test)]
            // A test's slot names itself by its address in this process, where its test's
            // device stand-in reads it.
            _ => {
                slot.extents = vec![Extent {
                    gpa: at as u64,
                    len: len as u64,
                }];
            }
            #[cfg(not(test))]
            _ => return Err(Unmade::Broken(io::Error::other("no instance to pin on"))),
        }
        Ok(slot)
    }

    fn len_u64(&self) -> u64 {
        // A slot is at most `slot_len(MAX_RING)`.
        self.len as u64
    }

    fn size(&self) -> u64 {
        u64::from(self.ring)
    }

    /// The offer that names it.
    fn offer(&self) -> Offer {
        Offer {
            ring: self.ring,
            extents: self.extents.clone(),
        }
    }

    fn word(&self, offset: u64) -> &AtomicU64 {
        // SAFETY: an aligned u64 of the slot's control page, mapped while the slot lives,
        // reached only atomically, by this relay and by the host.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset as usize).cast()) }
    }

    fn flag(&self, offset: u64) -> &AtomicU32 {
        // SAFETY: as in `word`, an aligned u32.
        unsafe { AtomicU32::from_ptr(self.base.as_ptr().add(offset as usize).cast()) }
    }

    /// Every position and flag back to zero, as an offer starts.
    fn reset(&self) {
        for f in [
            field::TO_HOST_HEAD,
            field::TO_HOST_TAIL,
            field::TO_GUEST_HEAD,
            field::TO_GUEST_TAIL,
        ] {
            self.word(f).store(0, Ordering::Relaxed);
        }
        for f in [
            field::HOST_WAITS_DATA,
            field::GUEST_WAITS_ROOM,
            field::GUEST_WAITS_DATA,
            field::HOST_WAITS_ROOM,
        ] {
            self.flag(f).store(0, Ordering::Relaxed);
        }
        fence(Ordering::SeqCst);
    }

    /// The ring for the host's bytes `from..from + len`, `from` a ring offset, as at most
    /// two runs wrapping at its end.
    fn to_host(&self, from: u64, len: u64) -> [libc::iovec; 2] {
        self.runs(abi::CONTROL, from, len)
    }

    fn to_guest(&self, from: u64, len: u64) -> [libc::iovec; 2] {
        self.runs(abi::CONTROL + self.size(), from, len)
    }

    fn runs(&self, ring_at: u64, from: u64, len: u64) -> [libc::iovec; 2] {
        let size = self.size();
        let from = from % size;
        let len = len.min(size);
        let first = len.min(size - from);
        // SAFETY: both runs lie within the ring at `ring_at`, within the mapping: `from` is
        // within the ring, and neither run reaches past its end.
        let at = |offset: u64| unsafe { self.base.as_ptr().add((ring_at + offset) as usize) };
        [
            libc::iovec {
                iov_base: at(from).cast(),
                iov_len: first as usize,
            },
            libc::iovec {
                iov_base: at(0).cast(),
                iov_len: (len - first) as usize,
            },
        ]
    }
}

/// Pins `len` bytes at `at` as fixed buffer `index`.
fn pin(pinner: RawFd, index: u32, at: *mut libc::c_void, len: usize) -> io::Result<()> {
    let iov = libc::iovec {
        iov_base: at,
        iov_len: len,
    };
    update(pinner, index, &iov)
}

/// Empties fixed buffer `index`: io_uring drops its pins at once, no request holding it
/// (io_uring/rsrc.c `__io_sqe_buffers_update`, `io_buffer_unmap`).
fn unregister(pinner: RawFd, index: u32) -> io::Result<()> {
    let empty = libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    };
    update(pinner, index, &empty)
}

fn update(pinner: RawFd, index: u32, iov: &libc::iovec) -> io::Result<()> {
    let update = RsrcUpdate2 {
        offset: index,
        resv: 0,
        data: std::ptr::from_ref(iov) as u64,
        tags: 0,
        nr: 1,
        resv2: 0,
    };
    register(pinner, IORING_REGISTER_BUFFERS_UPDATE, &update)
}

fn register<T>(ring: RawFd, opcode: libc::c_uint, arg: &T) -> io::Result<()> {
    let size = libc::c_uint::try_from(std::mem::size_of::<T>())
        .map_err(|_| io::Error::other("io_uring argument"))?;
    // SAFETY: io_uring_register(2) of our instance, with an argument of the size it reads.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            libc::c_long::from(ring),
            libc::c_long::from(opcode),
            std::ptr::from_ref(arg),
            libc::c_long::from(size),
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The frame numbers of the pages at `at..at + len`, pinned, from /proc/self/pagemap,
/// which shows them to CAP_SYS_ADMIN alone (fs/proc/task_mmu.c): bits 0-54 the frame,
/// bit 63 present (Documentation/admin-guide/mm/pagemap.rst).
fn frames(at: usize, len: usize) -> io::Result<Vec<u64>> {
    let pagemap = File::open("/proc/self/pagemap")?;
    let page = page()?;
    let first = (at as u64) / page;
    let count = (len as u64) / page;
    let mut raw = vec![0u8; usize::try_from(count * 8).map_err(|_| io::Error::other("pagemap length"))?];
    std::os::unix::fs::FileExt::read_exact_at(&pagemap, &mut raw, first * 8)?;
    let (entries, []) = raw.as_chunks::<8>() else {
        return Err(io::Error::other("pagemap entries"));
    };
    entries
        .iter()
        .map(|e| {
            let e = u64::from_le_bytes(*e);
            let frame = e & ((1 << 55) - 1);
            if e >> 63 == 1 && frame != 0 {
                Ok(frame)
            } else {
                Err(io::Error::other("pagemap shows no frame numbers"))
            }
        })
        .collect()
}

fn page() -> io::Result<u64> {
    // SAFETY: sysconf(3) of a constant name.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| io::Error::other("the page size"))
}

/// What became of a connection's offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offered {
    /// The relay made no slot: a plain hello is to be said.
    Plain,
    /// Its hello said, the connection goes on over plain vsock: the device refused its slot,
    /// or let it go, and what the slot held was passed on.
    Unringed,
    /// The connection is over.
    Done,
}

/// Says the connection's hello on `host`, with a slot's offer where the room fits one, and
/// runs the connection through the slot's rings where the device takes it.
pub fn hello(conn: &UnixStream, host: &File, token: &[u8; run::TOKEN]) -> io::Result<Offered> {
    let Some(pool) = POOL.get() else {
        return Ok(Offered::Plain);
    };
    hello_in(pool, conn, host, token)
}

fn hello_in(
    pool: &'static Mutex<Pool>,
    conn: &UnixStream,
    host: &File,
    token: &[u8; run::TOKEN],
) -> io::Result<Offered> {
    let supply = lock(pool).supply();
    let Some(slot) = take(pool, &supply, None) else {
        return Ok(Offered::Plain);
    };
    slot.reset();
    let offer = slot.offer().encode();
    let hello = u32::try_from(run::TOKEN + offer.len())
        .map(|len| [&run::header(run::kind::HELLO, len)[..], token, &offer].concat());
    // An offer not said whole is one no device took.
    if let Err(e) = hello
        .map_err(|_| io::Error::other("offer length"))
        .and_then(|h| (&*host).write_all(&h))
    {
        lock(pool).release(slot, false);
        return Err(e);
    }
    let mut reply = [0u8; abi::REPLY];
    match (&*host).read_exact(&mut reply).map(|()| Reply::parse(reply)) {
        Ok(Some(Reply::Attached)) => Ok(Stream::serve(pool, conn, host, slot)),
        Ok(Some(Reply::Refused(why))) => {
            // Refused, the device took nothing of it.
            let _ = writeln!(io::stderr(), "shards-init: engine ring refused: {why:?}");
            lock(pool).release(slot, false);
            Ok(Offered::Unringed)
        }
        // No word of the slot: it stays pinned.
        Ok(None) => {
            lock(pool).lose(slot);
            Err(io::Error::new(io::ErrorKind::InvalidData, "not a ring's reply"))
        }
        Err(e) => {
            lock(pool).lose(slot);
            Err(e)
        }
    }
}

/// One direction's state, as the relay's plain pump keeps its halves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Up {
    /// The client's bytes go into the ring for the host.
    Reading,
    /// The client ended its writes; the device is told ([`abi::END`]).
    Ended,
}

/// What a stream learns of the rings it needs (R03).
#[derive(Debug, Default)]
struct Learn {
    /// The device's tail as the relay rang it asleep on an empty ring for the host: its
    /// first take after that ends its wake.
    rang: Option<u64>,
    /// The relay slept on an empty ring for the guest, waiting for its bytes alone.
    slept_empty: bool,
    /// The most bytes a ring held at its consumer's wake: the stream's rate times the wake.
    most: u64,
    /// A producer filled its ring whole within its consumer's wake, the producer waiting:
    /// the ring held the stream back.
    limited: bool,
}

impl Learn {
    /// Rung asleep on an empty ring for the host at its tail `taken`.
    fn rang(&mut self, taken: u64) {
        self.rang.get_or_insert(taken);
    }

    /// The ring for the host, as the relay finds it: the device's first take since it was
    /// rung, `head - rang` bytes in it then; or the ring filled whole before any.
    fn host_ring(&mut self, taken: u64, head: u64, size: u64) {
        let Some(rang) = self.rang else {
            return;
        };
        let held = head.saturating_sub(rang);
        if taken > rang {
            self.most = self.most.max(held.min(size));
            self.rang = None;
        } else if held >= size {
            // It needed more than the ring: what is known of it is that much.
            self.most = self.most.max(size.saturating_add(1));
            self.limited = true;
            self.rang = None;
        }
    }

    /// The ring for the guest as the relay woke to it: `ready` bytes, the device waiting
    /// for room or not.
    fn guest_ring(&mut self, ready: u64, size: u64, device_waits: bool) {
        if !std::mem::take(&mut self.slept_empty) {
            return;
        }
        self.most = self.most.max(ready.min(size));
        if ready >= size && device_waits {
            self.most = self.most.max(size.saturating_add(1));
            self.limited = true;
        }
    }

    /// Its ring grown: what held it back is passed.
    fn grown(&mut self) {
        self.limited = false;
        self.rang = None;
        self.slept_empty = false;
    }

    /// The rings it needed, of `ring`'s it had: the least power of two past the most a ring
    /// of its held at a wake (past a ring's length where that ring held it back), twice its
    /// own where its own holds it back still; none where it learned nothing.
    fn need(&self, ring: u32) -> Option<u32> {
        let want = if self.limited {
            u64::from(ring).saturating_mul(2).max(self.most)
        } else {
            self.most
        };
        (want > 0).then(|| {
            let ring = want
                .checked_next_power_of_two()
                .unwrap_or(u64::MAX)
                .clamp(u64::from(abi::MIN_RING), u64::from(abi::MAX_RING));
            u32::try_from(ring).unwrap_or(abi::MAX_RING)
        })
    }
}

/// What a stream's pump came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turned {
    /// The device said DETACH: it let go of the slot.
    DeviceLetGo,
    /// Idle, the room asks for its slot: for a give-back or not.
    LetGo { given_back: bool },
    /// Its ring held it back.
    Grow,
    /// Its client is gone, or the device's positions could not be: it ends.
    Ended,
    /// The connection ended with no word of the device's: its slot stays pinned.
    Lost,
}

/// What a swap came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swapped {
    /// Its new slot attached, the old one's bytes passed on into it.
    Grown,
    /// No room, or no slot: the stream goes on as it was.
    Kept,
    /// The new slot refused, the old one let go: plain vsock.
    Unringed,
    /// The device let go of the old slot and said nothing of the new: the connection is
    /// over.
    Done,
    /// No word of the old slot.
    Lost,
}

/// What a stream's slot comes to as it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposal {
    Release { given_back: bool },
    Lose,
}

/// A connection through a slot's rings: its client's bytes into the ring for the host, the
/// ring for the guest's to its client, each side rung only where it said it waits.
struct Stream<'a> {
    pool: &'static Mutex<Pool>,
    conn: &'a UnixStream,
    host: &'a File,
    id: u64,
    wake: Option<OwnedFd>,
    slot: Slot,
    /// Bytes the relay put in the ring for the host, and took from the ring for the guest.
    head: u64,
    tail: u64,
    up: Up,
    owed: Owed,
    learn: Learn,
    /// The room's generation at which growth was last refused: tried again once it moves.
    refused_at: Option<u64>,
}

impl<'a> Stream<'a> {
    fn serve(pool: &'static Mutex<Pool>, conn: &'a UnixStream, host: &'a File, slot: Slot) -> Offered {
        // SAFETY: eventfd(2) with constant flags.
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        // SAFETY: a descriptor just made, which nothing else owns.
        let wake = (wake >= 0).then(|| unsafe { OwnedFd::from_raw_fd(wake) });
        let id = lock(pool).enlist(wake.as_ref().map(|w| w.as_raw_fd()), slot.len_u64());
        let mut stream = Stream {
            pool,
            conn,
            host,
            id,
            wake,
            slot,
            head: 0,
            tail: 0,
            up: Up::Reading,
            owed: Owed::default(),
            learn: Learn::default(),
            refused_at: None,
        };
        let (offered, disposal) = stream.run();
        let need = stream.learn.need(stream.slot.ring);
        let Stream { slot, wake, .. } = stream;
        let mut p = lock(pool);
        p.discharge(id, need);
        match disposal {
            Disposal::Release { given_back } => p.release(slot, given_back),
            Disposal::Lose => p.lose(slot),
        }
        drop(p);
        drop(wake);
        offered
    }

    fn run(&mut self) -> (Offered, Disposal) {
        loop {
            match self.pump() {
                Turned::DeviceLetGo => {
                    // The device let go on its own: the relay's own DETACH, then plain bytes.
                    let _ = write_all(self.host.as_raw_fd(), &[abi::DETACH]);
                    return self.unringed(false);
                }
                Turned::LetGo { given_back } => {
                    if self.ask_detach().is_err() {
                        return (Offered::Done, Disposal::Lose);
                    }
                    return self.unringed(given_back);
                }
                Turned::Grow => match self.swap() {
                    Swapped::Grown | Swapped::Kept => {}
                    Swapped::Unringed => return self.unringed(false),
                    Swapped::Done => return (Offered::Done, Disposal::Release { given_back: false }),
                    Swapped::Lost => return (Offered::Done, Disposal::Lose),
                },
                Turned::Ended => {
                    let _ = self.conn.shutdown(std::net::Shutdown::Both);
                    if self.ask_detach().is_err() {
                        return (Offered::Done, Disposal::Lose);
                    }
                    return (Offered::Done, Disposal::Release { given_back: false });
                }
                Turned::Lost => {
                    let _ = self.conn.shutdown(std::net::Shutdown::Both);
                    return (Offered::Done, Disposal::Lose);
                }
            }
        }
    }

    /// The slot let go, both sides' DETACHes said: what it holds is passed on, and the
    /// connection goes on over plain vsock; a slot whose last positions cannot be believed
    /// ends it.
    fn unringed(&mut self, given_back: bool) -> (Offered, Disposal) {
        if self.settle() {
            (Offered::Unringed, Disposal::Release { given_back })
        } else {
            let _ = self.conn.shutdown(std::net::Shutdown::Both);
            (Offered::Done, Disposal::Release { given_back })
        }
    }

    /// The connection's bytes through the slot's rings, until something ends them.
    fn pump(&mut self) -> Turned {
        loop {
            let size = self.slot.size();
            let mut progressed = false;
            // The client's bytes, into the ring for the host.
            let taken = self.slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire);
            if taken > self.head || self.head - taken > size {
                return Turned::Ended;
            }
            self.learn.host_ring(taken, self.head, size);
            let room = size - (self.head - taken);
            if self.up == Up::Reading && room > 0 {
                match recv(self.conn.as_raw_fd(), &self.slot.to_host(self.head, room)) {
                    Ok(0) => {
                        self.up = Up::Ended;
                        self.owed.end = true;
                        progressed = true;
                    }
                    Ok(n) => {
                        self.head += n as u64;
                        self.slot
                            .word(field::TO_HOST_HEAD)
                            .store(self.head, Ordering::Release);
                        if rung(&self.slot, field::HOST_WAITS_DATA) {
                            self.owed.doorbell = true;
                            self.learn.rang(taken);
                        }
                        self.learn.host_ring(taken, self.head, size);
                        progressed = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => return Turned::Ended,
                }
            }
            // The ring for the guest's bytes, to the client.
            let given = self.slot.word(field::TO_GUEST_HEAD).load(Ordering::Acquire);
            if given < self.tail || given - self.tail > size {
                return Turned::Ended;
            }
            let ready = given - self.tail;
            let device_waits = self.slot.flag(field::HOST_WAITS_ROOM).load(Ordering::SeqCst) != 0;
            self.learn.guest_ring(ready, size, device_waits);
            let mut client_full = false;
            if ready > 0 {
                match send(self.conn.as_raw_fd(), &self.slot.to_guest(self.tail, ready)) {
                    Ok(0) => return Turned::Ended,
                    Ok(n) => {
                        self.tail += n as u64;
                        self.slot
                            .word(field::TO_GUEST_TAIL)
                            .store(self.tail, Ordering::Release);
                        self.owed.doorbell |= rung(&self.slot, field::HOST_WAITS_ROOM);
                        progressed = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => client_full = true,
                    Err(_) => return Turned::Ended,
                }
            }
            // A socket that takes no signal now has its end in what it says next.
            let _ = self.owed.send(self.host.as_raw_fd());
            match signals(self.host.as_raw_fd()) {
                Signals::Detach => return Turned::DeviceLetGo,
                Signals::Rung => progressed = true,
                Signals::Quiet => {}
                Signals::Closed => return Turned::Lost,
            }
            if self.learn.limited
                && self.slot.ring < abi::MAX_RING
                && self.refused_at != Some(lock(self.pool).room_gen)
            {
                return Turned::Grow;
            }
            if progressed {
                continue;
            }
            // Nothing moved: idle where both rings are empty and nothing is owed, when the
            // room may ask for its slot.
            let taken = self.slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire);
            let idle = self.head == taken && ready == 0 && !self.owed.any();
            if let Some(given_back) = lock(self.pool).idle(self.id, idle) {
                return Turned::LetGo { given_back };
            }
            let room = size.saturating_sub(self.head.saturating_sub(taken));
            // Say what this side waits for, look once more, then sleep.
            let want_room = self.up == Up::Reading && room == 0;
            let want_data = ready == 0;
            if want_room {
                self.slot.flag(field::GUEST_WAITS_ROOM).store(1, Ordering::SeqCst);
            }
            if want_data {
                self.slot.flag(field::GUEST_WAITS_DATA).store(1, Ordering::SeqCst);
            }
            fence(Ordering::SeqCst);
            let came = (want_room && self.slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire) != taken)
                || (want_data && self.slot.word(field::TO_GUEST_HEAD).load(Ordering::Acquire) != given);
            if !came {
                let client = (if self.up == Up::Reading && room > 0 {
                    libc::POLLIN
                } else {
                    0
                }) | (if client_full { libc::POLLOUT } else { 0 });
                let host = libc::POLLIN | if self.owed.any() { libc::POLLOUT } else { 0 };
                let wake = self.wake.as_ref().map(|w| w.as_raw_fd());
                if wait(self.conn.as_raw_fd(), client, self.host.as_raw_fd(), host, wake).is_err() {
                    return Turned::Ended;
                }
                if let Some(wake) = wake {
                    drain(wake);
                }
            }
            if idle {
                let _ = lock(self.pool).idle(self.id, false);
            }
            self.slot.flag(field::GUEST_WAITS_ROOM).store(0, Ordering::SeqCst);
            self.slot.flag(field::GUEST_WAITS_DATA).store(0, Ordering::SeqCst);
            self.learn.slept_empty = want_data && !client_full && !came;
        }
    }

    /// Asks the device to let go of the slot and hears it do so.
    fn ask_detach(&mut self) -> io::Result<()> {
        write_all(self.host.as_raw_fd(), &[abi::DETACH])?;
        hear_detach(self.host.as_raw_fd())
    }

    /// Passes on what the slot still holds once the device has let go of it: the host's
    /// bytes its client has not had, to the client; its client's bytes the device had not
    /// taken, to the host as plain vsock; then its client's end, where it came. Whether the
    /// device's last positions could be believed: where not, nothing is passed on.
    fn settle(&mut self) -> bool {
        let Some((taken, given)) = finals(&self.slot, self.head, self.tail) else {
            return false;
        };
        let _ = write_runs(
            self.conn.as_raw_fd(),
            &self.slot.to_guest(self.tail, given - self.tail),
        );
        self.tail = given;
        let _ = write_runs(
            self.host.as_raw_fd(),
            &self.slot.to_host(taken, self.head - taken),
        );
        if self.up == Up::Ended {
            // SAFETY: shutdown(2) of the vsock socket's writes: the client's end, after its
            // last bytes.
            let _ = unsafe { libc::shutdown(self.host.as_raw_fd(), libc::SHUT_WR) };
        }
        true
    }

    /// Swaps the slot for one of twice its rings ([`abi::SWAP`]), where the room fits it.
    fn swap(&mut self) -> Swapped {
        let next = self.slot.ring.saturating_mul(2).min(abi::MAX_RING);
        let (supply, generation) = {
            let p = lock(self.pool);
            (p.supply(), p.room_gen)
        };
        let Some(new) = take(self.pool, &supply, Some(next)) else {
            self.refused_at = Some(generation);
            return Swapped::Kept;
        };
        new.reset();
        let swap = [&[abi::SWAP][..], &new.offer().encode()].concat();
        if write_all(self.host.as_raw_fd(), &swap).is_err() {
            // An offer not whole at the device is one it took nothing of.
            lock(self.pool).release(new, false);
            return Swapped::Lost;
        }
        if hear_detach(self.host.as_raw_fd()).is_err() {
            lock(self.pool).lose(new);
            return Swapped::Lost;
        }
        let mut reply = [0u8; abi::REPLY];
        let reply = read_exact(self.host.as_raw_fd(), &mut reply).map(|()| Reply::parse(reply));
        let old = std::mem::replace(&mut self.slot, new);
        match reply {
            Ok(Some(Reply::Attached)) => {
                let Some((taken, given)) = finals(&old, self.head, self.tail) else {
                    lock(self.pool).release(old, false);
                    return Swapped::Done;
                };
                // The host's bytes the old ring still held, to the client before any of the
                // new ring's.
                let _ = write_runs(self.conn.as_raw_fd(), &old.to_guest(self.tail, given - self.tail));
                // The client's bytes the device had not taken, at the new ring's start.
                let rest = self.head - taken;
                copy_runs(&old.to_host(taken, rest), &self.slot.to_host(0, rest));
                self.head = rest;
                self.tail = 0;
                self.slot
                    .word(field::TO_HOST_HEAD)
                    .store(self.head, Ordering::Release);
                if rest > 0 && rung(&self.slot, field::HOST_WAITS_DATA) {
                    self.owed.doorbell = true;
                }
                // A client that had ended its writes says so again on the new ring: the old
                // ring's word went with it.
                if self.up == Up::Ended {
                    self.owed.end = true;
                }
                let mut p = lock(self.pool);
                p.release(old, false);
                p.resize(self.id, self.slot.len_u64());
                self.learn.grown();
                Swapped::Grown
            }
            Ok(Some(Reply::Refused(_))) => {
                // Refused, the device took nothing of the new slot; the old one it let go.
                let new = std::mem::replace(&mut self.slot, old);
                let mut p = lock(self.pool);
                p.release(new, false);
                p.resize(self.id, self.slot.len_u64());
                Swapped::Unringed
            }
            Ok(None) | Err(_) => {
                // No word of the new slot; the old one the device let go.
                let new = std::mem::replace(&mut self.slot, old);
                lock(self.pool).lose(new);
                Swapped::Done
            }
        }
    }
}

/// The device's last positions of `slot`, where they can be believed: its tail of the ring
/// for the host within a ring behind what the relay put (`head`), its head of the ring for
/// the guest within a ring past what the relay took (`tail`).
fn finals(slot: &Slot, head: u64, tail: u64) -> Option<(u64, u64)> {
    let size = slot.size();
    let taken = slot.word(field::TO_HOST_TAIL).load(Ordering::Acquire);
    let given = slot.word(field::TO_GUEST_HEAD).load(Ordering::Acquire);
    (taken <= head && head - taken <= size && given >= tail && given - tail <= size).then_some((taken, given))
}

/// Whether the host said it waits at `flag`, which is cleared: after this side published
/// and fenced (Dekker's handshake: the host sets its flag, fences, then looks again).
fn rung(slot: &Slot, flag: u64) -> bool {
    fence(Ordering::SeqCst);
    slot.flag(flag).swap(0, Ordering::SeqCst) != 0
}

/// Signals the host is owed and its socket had no room for yet.
#[derive(Debug, Default)]
struct Owed {
    doorbell: bool,
    end: bool,
}

impl Owed {
    fn any(&self) -> bool {
        self.doorbell || self.end
    }

    /// Sends what is owed, a doorbell before the end, as far as the socket takes them.
    fn send(&mut self, host: RawFd) -> io::Result<()> {
        for (owed, byte) in [(&mut self.doorbell, abi::DOORBELL), (&mut self.end, abi::END)] {
            if !*owed {
                continue;
            }
            // SAFETY: send(2) of one byte from a local, never blocking, never raising SIGPIPE.
            let n = unsafe {
                libc::send(
                    host,
                    (&raw const byte).cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            match n {
                1 => *owed = false,
                _ => {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::WouldBlock {
                        return Ok(());
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// What the device's side of the connection said since it was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signals {
    Quiet,
    Rung,
    /// Its DETACH: it let go of the slot. What follows it is plain, and left unread.
    Detach,
    /// The connection ended, or the device said a byte it never says: no word of the slot.
    Closed,
}

/// The most the device says between two of a stream's reads of it ([`signals`], once a
/// turn): a doorbell for each of the two flags this side set before it slept (the device
/// rings once a flag it finds set, and clears it), and its DETACH. More is read on.
const MOST_SAID: usize = 3;

/// Reads the device's doorbells, up to and with its DETACH and nothing past it.
fn signals(host: RawFd) -> Signals {
    let mut said = Signals::Quiet;
    let mut buf = [0u8; MOST_SAID];
    loop {
        // SAFETY: recv(2) peeking into a local buffer, never blocking.
        let n = unsafe {
            libc::recv(
                host,
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT | libc::MSG_PEEK,
            )
        };
        let n = match usize::try_from(n) {
            Ok(0) => return Signals::Closed,
            Ok(n) => n,
            Err(_) => {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => said,
                    io::ErrorKind::Interrupted => continue,
                    _ => Signals::Closed,
                };
            }
        };
        let seen = buf.get(..n).unwrap_or_default();
        let (take, detach) = match seen.iter().position(|&b| b != abi::DOORBELL) {
            Some(at) if seen.get(at) == Some(&abi::DETACH) => (at + 1, true),
            Some(_) => return Signals::Closed,
            None => (n, false),
        };
        let mut taken = 0;
        while taken < take {
            let Some(rest) = buf.get_mut(taken..take) else {
                return Signals::Closed;
            };
            // SAFETY: recv(2) of bytes the peek just saw into a local buffer, never blocking.
            let got = unsafe { libc::recv(host, rest.as_mut_ptr().cast(), rest.len(), libc::MSG_DONTWAIT) };
            match usize::try_from(got) {
                Ok(0) => return Signals::Closed,
                Ok(got) => taken += got,
                Err(_) if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Signals::Closed,
            }
        }
        if detach {
            return Signals::Detach;
        }
        said = Signals::Rung;
    }
}

/// Reads the device's bytes until its DETACH, a byte at a time so that nothing after it is
/// taken: doorbells it rang before it are passed over. An error where the connection ended
/// first, or the device said a byte it never says.
fn hear_detach(host: RawFd) -> io::Result<()> {
    loop {
        let mut byte = [0u8; 1];
        read_exact(host, &mut byte)?;
        match byte {
            [abi::DOORBELL] => {}
            [abi::DETACH] => return Ok(()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a byte no device says",
                ));
            }
        }
    }
}

fn recv(fd: RawFd, iov: &[libc::iovec; 2]) -> io::Result<usize> {
    msg(fd, iov, false)
}

fn send(fd: RawFd, iov: &[libc::iovec; 2]) -> io::Result<usize> {
    msg(fd, iov, true)
}

/// One recvmsg(2) or sendmsg(2) of `iov`, never blocking, never raising SIGPIPE.
fn msg(fd: RawFd, iov: &[libc::iovec; 2], out: bool) -> io::Result<usize> {
    let used = if iov.get(1).is_none_or(|v| v.iov_len == 0) {
        1
    } else {
        2
    };
    // SAFETY: an all-zero msghdr is valid; its iovecs are set below.
    let mut m: libc::msghdr = unsafe { std::mem::zeroed() };
    m.msg_iov = iov.as_ptr().cast_mut();
    m.msg_iovlen = used as _;
    loop {
        // SAFETY: the iovecs lie within a slot's mapping, which outlives the call; the
        // kernel copies to or from them without Rust aliasing them.
        let n = unsafe {
            if out {
                libc::sendmsg(fd, &raw const m, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
            } else {
                libc::recvmsg(fd, &raw mut m, libc::MSG_DONTWAIT)
            }
        };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Sends all of `runs`, waiting for room where the socket has none.
fn write_runs(fd: RawFd, runs: &[libc::iovec; 2]) -> io::Result<()> {
    let mut runs = *runs;
    while runs.iter().any(|r| r.iov_len > 0) {
        match send(fd, &runs) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(mut n) => {
                for r in &mut runs {
                    let step = n.min(r.iov_len);
                    // SAFETY: `step` lies within the run, within the slot's mapping.
                    r.iov_base = unsafe { r.iov_base.cast::<u8>().add(step) }.cast();
                    r.iov_len -= step;
                    n -= step;
                }
                if runs.first().is_some_and(|r| r.iov_len == 0) {
                    runs.swap(0, 1);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_one(fd, libc::POLLOUT)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copies the bytes of `from`'s runs into `to`'s, which hold as many.
fn copy_runs(from: &[libc::iovec; 2], to: &[libc::iovec; 2]) {
    let (mut src, mut dst) = (from.iter().copied(), to.iter().copied());
    let (mut s, mut d) = (src.next(), dst.next());
    while let (Some(sr), Some(dr)) = (s.as_mut(), d.as_mut()) {
        let n = sr.iov_len.min(dr.iov_len);
        // SAFETY: both runs lie in slots' mappings, `n` bytes within each, in two slots
        // (the old and the new), so they do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(sr.iov_base.cast::<u8>(), dr.iov_base.cast::<u8>(), n) };
        // SAFETY: `n` lies within each run.
        sr.iov_base = unsafe { sr.iov_base.cast::<u8>().add(n) }.cast();
        sr.iov_len -= n;
        // SAFETY: as above.
        dr.iov_base = unsafe { dr.iov_base.cast::<u8>().add(n) }.cast();
        dr.iov_len -= n;
        if sr.iov_len == 0 {
            s = src.next();
        }
        if d.as_ref().is_some_and(|r| r.iov_len == 0) {
            d = dst.next();
        }
    }
}

/// Sends all of `bytes`, waiting for room where the socket has none.
fn write_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut at = 0;
    while let Some(rest) = bytes.get(at..).filter(|r| !r.is_empty()) {
        // SAFETY: send(2) from a local slice, never blocking, never raising SIGPIPE.
        let n = unsafe {
            libc::send(
                fd,
                rest.as_ptr().cast(),
                rest.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        match usize::try_from(n) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => at += n,
            Err(_) => {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => wait_one(fd, libc::POLLOUT)?,
                    io::ErrorKind::Interrupted => {}
                    _ => return Err(e),
                }
            }
        }
    }
    Ok(())
}

/// Reads exactly `buf`'s length, waiting for bytes where none have come.
fn read_exact(fd: RawFd, buf: &mut [u8]) -> io::Result<()> {
    let mut at = 0;
    while let Some(rest) = buf.get_mut(at..).filter(|r| !r.is_empty()) {
        // SAFETY: recv(2) into a local slice, never blocking.
        let n = unsafe { libc::recv(fd, rest.as_mut_ptr().cast(), rest.len(), libc::MSG_DONTWAIT) };
        match usize::try_from(n) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => at += n,
            Err(_) => {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => wait_one(fd, libc::POLLIN)?,
                    io::ErrorKind::Interrupted => {}
                    _ => return Err(e),
                }
            }
        }
    }
    Ok(())
}

fn wait_one(fd: RawFd, events: libc::c_short) -> io::Result<()> {
    let mut fds = [libc::pollfd {
        fd,
        events,
        revents: 0,
    }];
    // SAFETY: poll(2) of one initialized entry.
    if unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

/// Reads an eventfd's counter back to zero.
fn drain(fd: RawFd) {
    let mut counter = [0u8; 8];
    // SAFETY: read(2) of the eventfd's counter into a local, never blocking.
    let _ = unsafe { libc::read(fd, counter.as_mut_ptr().cast(), counter.len()) };
}

/// Sleeps until the client or the host has what this side waits for, or its pool wakes it.
fn wait(
    client: RawFd,
    client_events: libc::c_short,
    host: RawFd,
    host_events: libc::c_short,
    wake: Option<RawFd>,
) -> io::Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: if client_events == 0 { -1 } else { client },
            events: client_events,
            revents: 0,
        },
        libc::pollfd {
            fd: host,
            events: host_events,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: poll(2) of three initialized entries; a negative descriptor is skipped.
    if unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use shards_abi::ring::Refusal;

    const PLENTY: u64 = 1 << 40;
    const TOKEN: [u8; run::TOKEN] = *b"0123456789abcdef";

    /// A test's pool: no instance (its slots unpinned, each named by its address in this
    /// process), the supply `supply`.
    fn pool(supply: Supply) -> &'static Mutex<Pool> {
        let mut p = Pool::new(None);
        p.supply = Some(supply);
        Box::leak(Box::new(Mutex::new(p)))
    }

    /// No budget, `available` bytes available, none working: room for that many.
    fn room(available: u64) -> Supply {
        Supply {
            cap: None,
            held: 0,
            available,
            working: 0,
        }
    }

    fn pattern(n: usize, salt: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(salt))
            .collect()
    }

    /// A slot as the device stand-in reaches it: its address here, its rings' length.
    #[derive(Debug, Clone, Copy)]
    struct Mapped {
        base: usize,
        size: u64,
    }

    impl Mapped {
        fn of(offer: &Offer) -> Mapped {
            assert_eq!(offer.extents.len(), 1, "a test's slot names itself whole");
            Mapped {
                base: offer.extents[0].gpa as usize,
                size: u64::from(offer.ring),
            }
        }

        fn word(&self, at: u64) -> &AtomicU64 {
            // SAFETY: an aligned u64 of a test slot's control page, mapped while the relay
            // holds the slot, which it does until the stand-in says DETACH.
            unsafe { AtomicU64::from_ptr((self.base + at as usize) as *mut u64) }
        }

        fn flag(&self, at: u64) -> &AtomicU32 {
            // SAFETY: as in `word`.
            unsafe { AtomicU32::from_ptr((self.base + at as usize) as *mut u32) }
        }

        fn byte(&self, at: u64) -> *mut u8 {
            (self.base + at as usize) as *mut u8
        }
    }

    /// What the host's side does in a test: the device, and the daemon behind it.
    #[derive(Debug, Clone, Default)]
    struct Script {
        /// Refuse the hello's offer.
        refuse: bool,
        /// The daemon's bytes, into the ring for the guest.
        answer: Vec<u8>,
        /// Once all of them are given, the daemon's end; else it stays, idle, until the
        /// relay lets go.
        end: bool,
        /// The pool whose hold of the slot the stand-in looks at as it answers the relay's
        /// DETACH: the guard of the slot's freeing.
        pool: Option<&'static Mutex<Pool>>,
        /// Ends the connection with no word: drops its socket as the answer is given.
        vanish: bool,
        /// The daemon ends as soon as its last byte is in the ring, before the relay took
        /// it: the relay passes on what the slot still holds after the device's DETACH.
        early: bool,
    }

    /// What the device stand-in saw.
    #[derive(Debug, Default)]
    struct Saw {
        /// The relay's bytes: through the rings, then plain.
        took: Vec<u8>,
        /// The rings of each slot offered, in order.
        rings: Vec<u32>,
        /// The client's end said.
        ended: bool,
        /// The relay asked to let go (its DETACH came first).
        asked: bool,
        /// Whether the pool held the slot as the stand-in answered the relay's DETACH.
        held: Option<bool>,
        /// A plain hello: no offer.
        plain: bool,
    }

    /// The stand-in's end of the vsock connection: what came and is not yet taken.
    struct Wire {
        socket: UnixStream,
        inbox: VecDeque<u8>,
        closed: bool,
    }

    impl Wire {
        /// What has come, without waiting.
        fn fill(&mut self) {
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: recv(2) into a local buffer, never blocking.
                let n = unsafe {
                    libc::recv(
                        self.socket.as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                match usize::try_from(n) {
                    Ok(0) => {
                        self.closed = true;
                        return;
                    }
                    Ok(n) => self.inbox.extend(&buf[..n]),
                    Err(_) => return,
                }
            }
        }

        /// The next byte, waiting for it; none at the end.
        fn byte(&mut self) -> Option<u8> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(b) = self.inbox.pop_front() {
                    return Some(b);
                }
                if self.closed {
                    return None;
                }
                assert!(Instant::now() < deadline, "the relay said nothing");
                self.fill();
                if self.inbox.is_empty() && !self.closed {
                    wait_one(self.socket.as_raw_fd(), libc::POLLIN).unwrap();
                }
            }
        }

        fn take(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.byte().expect("bytes")).collect()
        }

        /// A swap's offer, after its byte: its fixed part, then as much as that says.
        fn offer(&mut self) -> Offer {
            let head: [u8; abi::OFFER_HEADER] = self.take(abi::OFFER_HEADER).try_into().unwrap();
            let rest = abi::offer_len(&head).unwrap() - abi::OFFER_HEADER;
            Offer::parse(&[head.to_vec(), self.take(rest)].concat()).unwrap()
        }

        fn say(&self, bytes: &[u8]) {
            write_all(self.socket.as_raw_fd(), bytes).unwrap();
        }

        /// Plain bytes until the end, then the daemon's end.
        fn plain_to_end(&mut self, saw: &mut Saw) {
            while let Some(b) = self.byte() {
                saw.took.push(b);
            }
            saw.ended = true;
            let _ = self.socket.shutdown(std::net::Shutdown::Write);
        }
    }

    /// The device's side of a connection (protocol v2) and the daemon's behind it, on
    /// `socket`, the vsock connection's host end: the hello and its offer read, answered,
    /// then the rings' bytes moved as the device moves them, sleeping as it does until a
    /// doorbell; the daemon's end said as the device says it (DETACH, then its shutdown
    /// once the relay's DETACH came), the relay's DETACH and SWAP answered.
    fn device(socket: UnixStream, script: Script) -> Saw {
        let mut saw = Saw::default();
        let mut wire = Wire {
            socket,
            inbox: VecDeque::new(),
            closed: false,
        };
        let head: [u8; run::HEADER] = wire.take(run::HEADER).try_into().unwrap();
        let (kind, len) = run::parse_header(head).unwrap();
        assert_eq!(kind, run::kind::HELLO);
        let hello = wire.take(len as usize);
        assert_eq!(hello[..run::TOKEN], TOKEN);
        if hello.len() == run::TOKEN {
            saw.plain = true;
            wire.say(&script.answer);
            wire.plain_to_end(&mut saw);
            return saw;
        }
        let offer = Offer::parse(&hello[run::TOKEN..]).unwrap();
        saw.rings.push(offer.ring);
        if script.refuse {
            wire.say(&Reply::Refused(Refusal::InUse).encode());
            wire.say(&script.answer);
            wire.plain_to_end(&mut saw);
            return saw;
        }
        let mut slot = Mapped::of(&offer);
        slot.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
        wire.say(&Reply::Attached.encode());
        let (mut taken, mut given, mut sent) = (0u64, 0u64, 0usize);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "the device stand-in waited too long");
            let mut moved = false;
            let head = slot.word(field::TO_HOST_HEAD).load(Ordering::Acquire);
            assert!(
                head >= taken && head - taken <= slot.size,
                "a head no relay reaches"
            );
            while taken < head {
                // SAFETY: a byte of the ring for the host the relay published.
                saw.took
                    .push(unsafe { slot.byte(abi::CONTROL + taken % slot.size).read() });
                taken += 1;
                moved = true;
            }
            slot.word(field::TO_HOST_TAIL).store(taken, Ordering::Release);
            fence(Ordering::SeqCst);
            if slot.flag(field::GUEST_WAITS_ROOM).swap(0, Ordering::SeqCst) != 0 {
                wire.say(&[abi::DOORBELL]);
            }
            let freed = slot.word(field::TO_GUEST_TAIL).load(Ordering::Acquire);
            let mut room = slot.size - (given - freed);
            while sent < script.answer.len() && room > 0 {
                // SAFETY: a byte of the ring for the guest with room.
                unsafe {
                    slot.byte(abi::CONTROL + slot.size + given % slot.size)
                        .write(script.answer[sent]);
                }
                given += 1;
                sent += 1;
                room -= 1;
                moved = true;
            }
            slot.word(field::TO_GUEST_HEAD).store(given, Ordering::Release);
            fence(Ordering::SeqCst);
            if slot.flag(field::GUEST_WAITS_DATA).swap(0, Ordering::SeqCst) != 0 {
                wire.say(&[abi::DOORBELL]);
            }
            if script.vanish && sent == script.answer.len() {
                return saw;
            }
            let drained = given == slot.word(field::TO_GUEST_TAIL).load(Ordering::Acquire);
            if script.end && sent == script.answer.len() && (drained || script.early) {
                // The daemon's end: the device lets go, then waits for the relay's DETACH
                // before its shutdown, the relay's remainder plain after it.
                wire.say(&[abi::DETACH]);
                loop {
                    match wire.byte() {
                        Some(abi::DOORBELL) => {}
                        Some(abi::END) => saw.ended = true,
                        Some(abi::DETACH) => break,
                        // A swap crossing the device's DETACH: refused, the host having
                        // ended, as the device refuses it (`Refusal::Ended`); plain after.
                        Some(abi::SWAP) => {
                            let _ = wire.offer();
                            wire.say(&Reply::Refused(Refusal::Ended).encode());
                            break;
                        }
                        other => panic!("the relay said {other:?} after the device's DETACH"),
                    }
                }
                wire.plain_to_end(&mut saw);
                return saw;
            }
            wire.fill();
            while let Some(b) = wire.inbox.pop_front() {
                moved = true;
                match b {
                    abi::DOORBELL => {}
                    abi::END => saw.ended = true,
                    abi::DETACH => {
                        saw.asked = true;
                        if let Some(pool) = script.pool {
                            // Time for a relay that would free its slot before this answer to
                            // do so; one that waits for it holds the slot however long.
                            std::thread::sleep(Duration::from_millis(50));
                            saw.held = Some(lock(pool).pinned >= abi::slot_len(offer.ring));
                        }
                        // Positions final, nothing more written: the device's DETACH.
                        wire.say(&[abi::DETACH]);
                        wire.plain_to_end(&mut saw);
                        return saw;
                    }
                    abi::SWAP => {
                        let offer = wire.offer();
                        wire.say(&[abi::DETACH]);
                        saw.rings.push(offer.ring);
                        slot = Mapped::of(&offer);
                        (taken, given) = (0, 0);
                        slot.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
                        wire.say(&Reply::Attached.encode());
                    }
                    other => panic!("the relay sent {other}"),
                }
            }
            if wire.closed {
                return saw;
            }
            if moved {
                continue;
            }
            // Nothing moved: say what the device waits for, look once more, then sleep.
            slot.flag(field::HOST_WAITS_DATA).store(1, Ordering::SeqCst);
            slot.flag(field::HOST_WAITS_ROOM).store(
                u32::from(room == 0 && sent < script.answer.len()),
                Ordering::SeqCst,
            );
            fence(Ordering::SeqCst);
            if slot.word(field::TO_HOST_HEAD).load(Ordering::Acquire) == taken
                && slot.word(field::TO_GUEST_TAIL).load(Ordering::Acquire) == freed
            {
                let mut fds = [libc::pollfd {
                    fd: wire.socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                // SAFETY: poll(2) of one entry; a long silence is a lost wakeup, or a relay
                // asleep for good, which the test's deadline says.
                unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
            }
            slot.flag(field::HOST_WAITS_DATA).store(0, Ordering::SeqCst);
            slot.flag(field::HOST_WAITS_ROOM).store(0, Ordering::SeqCst);
        }
    }

    /// A connection through `pool`: the client writes `request` then ends its writes, and
    /// reads to its end; the relay says its hello and runs the rings, then plain vsock
    /// where it comes to it, as engine.rs does; the host's side does `script`. What the
    /// client got, what the stand-in saw, and the offer's outcome.
    fn connection(pool: &'static Mutex<Pool>, request: &[u8], script: Script) -> (Vec<u8>, Saw, Offered) {
        let (client, relay_side) = UnixStream::pair().unwrap();
        let (vsock_guest, vsock_host) = UnixStream::pair().unwrap();
        std::thread::scope(|s| {
            let device = s.spawn(move || device(vsock_host, script));
            let writer = s.spawn(|| {
                (&client).write_all(request).unwrap();
                client.shutdown(std::net::Shutdown::Write).unwrap();
            });
            let relay = s.spawn(move || {
                let host = File::from(OwnedFd::from(vsock_guest));
                let offered = hello_in(pool, &relay_side, &host, &TOKEN).unwrap();
                if offered != Offered::Done {
                    plain(&relay_side, &host, offered == Offered::Plain);
                }
                offered
            });
            let mut got = Vec::new();
            (&client).read_to_end(&mut got).unwrap();
            writer.join().unwrap();
            let offered = relay.join().unwrap();
            (got, device.join().unwrap(), offered)
        })
    }

    /// The plain relay engine.rs runs after a ring (or with none): its hello where none was
    /// said, then the bytes each way, the host's end ending the client's.
    fn plain(conn: &UnixStream, host: &File, hello: bool) {
        if hello {
            let said = [&run::header(run::kind::HELLO, run::TOKEN as u32)[..], &TOKEN].concat();
            (&*host).write_all(&said).unwrap();
        }
        std::thread::scope(|s| {
            s.spawn(|| {
                let _ = io::copy(&mut &*conn, &mut &*host);
                // SAFETY: shutdown(2) of the vsock socket's writes.
                unsafe { libc::shutdown(host.as_raw_fd(), libc::SHUT_WR) };
            });
            let _ = io::copy(&mut &*host, &mut &*conn);
            let _ = conn.shutdown(std::net::Shutdown::Both);
        });
    }

    fn held(pool: &Mutex<Pool>) -> (u64, u64) {
        let p = lock(pool);
        (p.pinned, p.lost)
    }

    /// Bytes each way, each larger than many rings, cross whole: through rings that grow as
    /// the stream shows they hold it back, then as the daemon ends, the device's DETACH, the
    /// relay's, and what the slot held passed on. Every slot is freed after its DETACH.
    #[test]
    fn bytes_cross_whole_and_every_slot_is_freed_after_its_detach() {
        for early in [false, true] {
            let pool = pool(room(PLENTY));
            let request = pattern(3 * 64 * 1024 + 17, 3);
            let answer = pattern(2 * 64 * 1024 + 5, 9);
            let (got, saw, offered) = connection(
                pool,
                &request,
                Script {
                    answer: answer.clone(),
                    end: true,
                    early,
                    ..Script::default()
                },
            );
            assert_eq!(saw.took, request, "early {early}");
            assert_eq!(got, answer, "early {early}");
            assert!(saw.ended, "the client's end reached the device");
            assert_eq!(offered, Offered::Unringed);
            assert_eq!(held(pool), (0, 0), "every slot freed, none lost");
            assert!(saw.rings.len() > 1, "the ring grew: {:?}", saw.rings);
            assert!(saw.rings.windows(2).all(|w| w[1] == 2 * w[0]), "{:?}", saw.rings);
        }
    }

    /// The guard of the slot's freeing: as the device answers the relay's DETACH, the relay
    /// still holds the slot; it frees it only once that answer has come. And the connection
    /// goes on over plain vsock after.
    #[test]
    fn the_relay_frees_a_slot_only_after_the_devices_detach() {
        let pool = pool(room(PLENTY));
        let (client, relay_side) = UnixStream::pair().unwrap();
        let (vsock_guest, vsock_host) = UnixStream::pair().unwrap();
        let answer = pattern(1000, 5);
        let (got, saw) = std::thread::scope(|s| {
            let device = s.spawn(|| {
                device(
                    vsock_host,
                    Script {
                        answer: answer.clone(),
                        pool: Some(pool),
                        ..Script::default()
                    },
                )
            });
            let relay = s.spawn(move || {
                let host = File::from(OwnedFd::from(vsock_guest));
                let offered = hello_in(pool, &relay_side, &host, &TOKEN).unwrap();
                assert_eq!(offered, Offered::Unringed);
                plain(&relay_side, &host, false);
            });
            (&client).write_all(b"ask").unwrap();
            let mut got = vec![0u8; answer.len()];
            (&client).read_exact(&mut got).unwrap();
            // Idle now: the room, told short, asks for its slot.
            settle_idle(pool);
            tell(pool, Some(-1), 0);
            // Plain vsock after.
            (&client).write_all(b" then plain").unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut rest = Vec::new();
            (&client).read_to_end(&mut rest).unwrap();
            relay.join().unwrap();
            (got, device.join().unwrap())
        });
        assert_eq!(got, answer);
        assert!(saw.asked);
        assert_eq!(
            saw.held,
            Some(true),
            "the relay let go of its slot before the device's DETACH"
        );
        assert_eq!(saw.took, b"ask then plain");
        assert_eq!(held(pool), (0, 0));
    }

    /// Waits until the pool's one stream is asleep, idle.
    fn settle_idle(pool: &Mutex<Pool>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lock(pool).live.iter().all(|l| l.idle) || lock(pool).live.is_empty() {
            assert!(Instant::now() < deadline, "the stream never went idle");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// What the watcher does with a word: `room` told, `give_backs` asked.
    fn tell(pool: &Mutex<Pool>, room: Option<i64>, give_backs: u64) {
        let tells = lock(pool).tells + 1;
        let wakes = lock(pool).heard(
            Told {
                tells,
                room,
                give_backs,
            },
            0,
        );
        for fd in wakes {
            ring(fd);
        }
    }

    /// A stream the room cannot fit goes over plain vsock: no offer, nothing pinned.
    #[test]
    fn a_stream_past_the_room_goes_over_plain_vsock() {
        let pool = pool(room(abi::slot_len(abi::MIN_RING) - 1));
        let request = pattern(5000, 1);
        let answer = pattern(7000, 2);
        let (got, saw, offered) = connection(
            pool,
            &request,
            Script {
                answer: answer.clone(),
                end: true,
                ..Script::default()
            },
        );
        assert!(saw.plain, "no offer said");
        assert_eq!(offered, Offered::Plain);
        assert_eq!((got, saw.took), (answer, request));
        assert_eq!(held(pool), (0, 0));
    }

    /// With no budget, pins are held within the guest's available memory: the ring starts
    /// and grows no larger than it fits, and the stream still crosses whole.
    #[test]
    fn with_no_budget_pins_are_bounded_by_the_available_memory() {
        let fits = abi::slot_len(16 << 10);
        let pool = pool(room(fits));
        lock(pool).start = abi::MAX_RING;
        let request = pattern(200 * 1024, 4);
        let answer = pattern(300 * 1024, 6);
        let (got, saw, _) = connection(
            pool,
            &request,
            Script {
                answer: answer.clone(),
                end: true,
                ..Script::default()
            },
        );
        assert_eq!((got, saw.took), (answer, request));
        assert_eq!(saw.rings, [16 << 10], "started at what fits, grew past nothing");
        assert_eq!(held(pool), (0, 0));
    }

    /// A refused offer is freed at once (the device took nothing of it), and the
    /// connection goes on plain.
    #[test]
    fn a_refused_slot_is_freed_and_the_connection_goes_on_plain() {
        let pool = pool(room(PLENTY));
        let (got, saw, offered) = connection(
            pool,
            b"plain request",
            Script {
                refuse: true,
                answer: b"plain answer".to_vec(),
                end: true,
                ..Script::default()
            },
        );
        assert_eq!(offered, Offered::Unringed);
        assert_eq!(
            (got, saw.took),
            (b"plain answer".to_vec(), b"plain request".to_vec())
        );
        assert_eq!(held(pool), (0, 0));
    }

    /// A connection that ends with no word of the device keeps its slot pinned, counted.
    #[test]
    fn a_slot_with_no_word_of_its_device_stays_pinned() {
        let pool = pool(room(PLENTY));
        let (_, _, offered) = connection(
            pool,
            b"x",
            Script {
                answer: b"y".to_vec(),
                vanish: true,
                ..Script::default()
            },
        );
        assert_eq!(offered, Offered::Done);
        let slot = abi::slot_len(abi::MIN_RING);
        assert_eq!(held(pool), (slot, slot));
    }

    /// A ring that held its stream back is swapped for twice it, and the next stream starts
    /// at what the last one needed.
    #[test]
    fn a_stream_starts_where_the_last_one_ended() {
        let pool = pool(room(PLENTY));
        let answer = pattern(512 * 1024, 7);
        let (_, first, _) = connection(
            pool,
            b"",
            Script {
                answer: answer.clone(),
                end: true,
                ..Script::default()
            },
        );
        let last = *first.rings.last().unwrap();
        assert!(last > abi::MIN_RING, "{:?}", first.rings);
        assert!(lock(pool).start >= last);
        let (_, next, _) = connection(
            pool,
            b"",
            Script {
                answer,
                end: true,
                ..Script::default()
            },
        );
        assert!(next.rings[0] >= last, "{:?} after {:?}", next.rings, first.rings);
    }

    /// Streams on many threads at once, as on a microVM of many vCPUs: each its own slot,
    /// pins summed in one pool, each slot freed as its stream ends.
    #[test]
    fn concurrent_streams_each_have_a_slot_and_free_it() {
        let pool = pool(room(PLENTY));
        std::thread::scope(|s| {
            let streams: Vec<_> = (0..8u8)
                .map(|i| {
                    s.spawn(move || {
                        let request = pattern(100_000 + usize::from(i) * 977, i);
                        let answer = pattern(150_000 + usize::from(i) * 613, i.wrapping_add(50));
                        let (got, saw, _) = connection(
                            pool,
                            &request,
                            Script {
                                answer: answer.clone(),
                                end: true,
                                ..Script::default()
                            },
                        );
                        assert_eq!((got, saw.took), (answer, request), "stream {i}");
                    })
                })
                .collect();
            for t in streams {
                t.join().unwrap();
            }
        });
        assert_eq!(held(pool), (0, 0));
    }

    /// A give-back lets every idle slot go; a live stream keeps its slot until it is idle,
    /// and the told room's debt is paid by idle slots only.
    #[test]
    fn the_room_asks_idle_streams_alone_and_a_give_back_asks_them_all() {
        let mut p = Pool::new(None);
        let a = p.enlist(None, 100);
        let b = p.enlist(None, 100);
        let c = p.enlist(None, 100);
        assert_eq!(p.idle(a, true), None, "no word, no ask");
        // A room 150 short: two idle slots pay it, a busy one none.
        p.heard(
            Told {
                tells: 1,
                room: Some(-150),
                give_backs: 0,
            },
            0,
        );
        assert_eq!(p.idle(b, false), None, "a live stream is not asked");
        assert_eq!(p.idle(a, true), Some(false));
        assert_eq!(p.idle(b, true), Some(false));
        assert_eq!(p.idle(c, true), None, "the debt paid");
        // A give-back asks every idle one, each once.
        p.heard(
            Told {
                tells: 1,
                room: Some(-150),
                give_backs: 1,
            },
            0,
        );
        assert_eq!(p.idle(c, true), Some(true));
        assert_eq!(p.idle(c, true), None);
        // A stream enlisted after a give-back owes it nothing.
        let d = p.enlist(None, 100);
        assert_eq!(p.idle(d, true), None);
    }

    /// The watcher wakes idle streams alone.
    #[test]
    fn a_word_wakes_the_idle_streams() {
        let mut p = Pool::new(None);
        let a = p.enlist(Some(11), 1);
        let _b = p.enlist(Some(12), 1);
        let _ = p.idle(a, true);
        assert_eq!(
            p.heard(
                Told {
                    tells: 1,
                    room: None,
                    give_backs: 0
                },
                0
            ),
            [11]
        );
    }

    /// `owed_for`'s basis: what a told room is short of the run's working room.
    #[test]
    fn a_told_room_owes_what_it_is_short_of_the_working_room() {
        assert_eq!(owed_for(None, 100), 0, "no budget, no debt");
        assert_eq!(owed_for(Some(100), 100), 0);
        assert_eq!(owed_for(Some(40), 100), 60);
        assert_eq!(owed_for(Some(-40), 100), 140);
        assert_eq!(owed_for(Some(i64::MIN), u64::MAX), u64::MAX);
    }

    /// The supply's basis: a budget's hold past what the run holds and its working room;
    /// with none, the guest's available memory past it; slots outside the run and those
    /// being made counted against it.
    #[test]
    fn the_supply_leaves_pins_what_the_run_does_not_need() {
        let budget = Supply {
            cap: Some(1000),
            held: 600,
            available: 0,
            working: 100,
        };
        assert_eq!(budget.room(0, 0), 300);
        assert_eq!(budget.room(50, 25), 225);
        assert_eq!(budget.room(500, 0), 0);
        let none = Supply {
            cap: None,
            held: 0,
            available: 1000,
            working: 100,
        };
        assert_eq!(none.room(0, 0), 900);
        assert_eq!(none.room(0, 1000), 0);
    }

    /// The table's basis: as many slots as the relay's descriptors let its streams hold.
    #[test]
    fn the_table_holds_as_many_slots_as_the_descriptors_let_streams_hold() {
        // Linux's default for init: RLIMIT_NOFILE 1024 (include/uapi/linux/fs.h INR_OPEN_CUR).
        assert_eq!(table_for(1024), 2 * (1024 / 3));
        assert_eq!(table_for(0), 1);
        assert_eq!(table_for(u64::MAX), MOST_BUFFERS);
    }

    /// What a stream learns: a ring filled within its consumer's wake held it back; the most
    /// it held at a wake is what it needs, in a power of two.
    #[test]
    fn a_stream_learns_the_rings_it_needs() {
        let mut l = Learn::default();
        assert_eq!(l.need(abi::MIN_RING), None, "nothing learned");
        // The relay rang a sleeping device at tail 0; it took after 3000 bytes.
        l.rang(0);
        l.host_ring(0, 3000, 4096);
        assert!(!l.limited);
        l.host_ring(1, 3000, 4096);
        assert_eq!(l.need(abi::MIN_RING), Some(4096));
        // Rung again at 3000, the ring filled before its first take.
        l.rang(3000);
        l.host_ring(3000, 7096, 4096);
        assert!(l.limited);
        assert_eq!(l.need(4096), Some(8192));
        l.grown();
        // Woke to a full ring for the guest, the device waiting: held back.
        l.slept_empty = true;
        l.guest_ring(8192, 8192, true);
        assert!(l.limited);
        // Grown, it needs at least the ring it grew to, though no later wake filled it.
        let mut g = Learn::default();
        g.rang(0);
        g.host_ring(0, 4096, 4096);
        g.grown();
        g.slept_empty = true;
        g.guest_ring(100, 8192, false);
        assert_eq!(g.need(8192), Some(8192));
        // Woke to a full ring it did not wait at: not a wake's measure.
        let mut m = Learn::default();
        m.guest_ring(8192, 8192, true);
        assert!(!m.limited);
    }

    /// The relay joins the run's cgroup at a leaf of its own, as its first slot is made.
    #[test]
    fn the_relay_joins_a_leaf_of_the_runs_cgroup() {
        let dir = shards_testdir::TempDir::new("ring-join").unwrap();
        join(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("engine/cgroup.procs")).unwrap(),
            "0"
        );
        // Again, the leaf there already.
        join(dir.path()).unwrap();
    }

    /// Init's builtins say nothing to a relay that made no slot (a run with no ring goes as
    /// before); once it has, each word is told and rings, and each give-back's slots are
    /// counted by one answer alone.
    #[test]
    fn init_says_nothing_to_a_relay_that_made_no_slot() {
        let shared = Shared::default();
        // SAFETY: eventfd(2) with constant flags.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(fd >= 0);
        // SAFETY: a descriptor just made.
        let bell = unsafe { OwnedFd::from_raw_fd(fd) };
        shared.told(fd, Some(5));
        shared.give_back(fd);
        assert_eq!(
            shared.heard(),
            Told {
                tells: 0,
                room: None,
                give_backs: 0
            }
        );
        let mut counter = [0u8; 8];
        let read = |counter: &mut [u8; 8]| {
            // SAFETY: read(2) of the eventfd's counter into a local, never blocking.
            unsafe { libc::read(bell.as_raw_fd(), counter.as_mut_ptr().cast(), 8) }
        };
        assert!(read(&mut counter) < 0, "rung");
        shared.given.store(10, Ordering::SeqCst);
        assert_eq!(shared.given(), 0, "no slot, nothing given");
        shared.made.store(1, Ordering::SeqCst);
        shared.told(fd, Some(-7));
        shared.give_back(fd);
        assert_eq!(
            shared.heard(),
            Told {
                tells: 1,
                room: Some(-7),
                give_backs: 1
            }
        );
        assert_eq!(read(&mut counter), 8);
        assert_eq!(u64::from_ne_bytes(counter), 2);
        shared.told(fd, None);
        assert_eq!(shared.heard().room, None, "no budget told");
        // Bytes given back are each answered once, however late they come.
        assert_eq!(shared.given(), 10);
        assert_eq!(shared.given(), 0);
        shared.given.fetch_add(4, Ordering::SeqCst);
        assert_eq!(shared.given(), 4);
    }

    /// A host byte that is no device's ends the connection with no word of the slot.
    #[test]
    fn a_host_byte_no_device_says_is_no_word_of_the_slot() {
        let (_client, relay_side) = UnixStream::pair().unwrap();
        let (vsock_guest, vsock_host) = UnixStream::pair().unwrap();
        (&vsock_host).write_all(&[0x7f]).unwrap();
        assert_eq!(signals(vsock_guest.as_raw_fd()), Signals::Closed);
        drop(relay_side);
    }

    /// The device's DETACH ends the doorbells read, and nothing past it is taken.
    #[test]
    fn the_devices_detach_is_read_and_nothing_past_it() {
        let (guest, host) = UnixStream::pair().unwrap();
        (&host)
            .write_all(&[abi::DOORBELL, abi::DOORBELL, abi::DETACH, b'p', b'l'])
            .unwrap();
        assert_eq!(signals(guest.as_raw_fd()), Signals::Detach);
        let mut rest = [0u8; 2];
        (&guest).read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"pl");
    }

    /// This process's pinned memory, in kB, as its kernel counts it (`VmPin`).
    fn vm_pin() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmPin:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap()
    }

    /// A slot's pages pinned as a fixed buffer of an io_uring instance count as pinned to
    /// the instance's maker (`VmPin`, as init's count for its relay's slots), and stop as
    /// its entry is emptied, no request holding it: the calls the relay pins and frees its
    /// slots by, on this host's kernel. Where it gives this process no io_uring, or no
    /// sparse table, nothing is checked.
    #[test]
    fn a_slots_pins_count_to_the_instances_maker_and_go_with_its_entry() {
        let Some(instance) = pinner() else {
            eprintln!("SKIP: no io_uring for this process here");
            return;
        };
        let fd = instance.as_raw_fd();
        let mut p = Pool::new(Some(instance));
        let Some(index) = p.entry(fd) else {
            eprintln!("SKIP: no fixed-buffer table here: {:?}", p.broken);
            return;
        };
        let len = usize::try_from(abi::slot_len(abi::MIN_RING)).unwrap();
        let page = usize::try_from(page().unwrap()).unwrap();
        // SAFETY: memfd_create(2) of a NUL-terminated name.
        let memory = unsafe { libc::memfd_create(c"ring-pin-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(memory >= 0);
        // SAFETY: a descriptor just made, which nothing else owns.
        let memory = unsafe { OwnedFd::from_raw_fd(memory) };
        // SAFETY: ftruncate(2) of the test's own memory file.
        assert_eq!(
            unsafe { libc::ftruncate(memory.as_raw_fd(), len as libc::off_t) },
            0
        );
        // SAFETY: a shared, populated mapping of the whole file, unmapped below.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                memory.as_raw_fd(),
                0,
            )
        };
        assert_ne!(at, libc::MAP_FAILED);
        let before = vm_pin();
        pin(fd, index, at, len).unwrap();
        assert_eq!(
            vm_pin(),
            before + (len.div_ceil(page) * page / 1024) as u64,
            "pinned"
        );
        unregister(fd, index).unwrap();
        assert_eq!(vm_pin(), before, "its pins kept after its entry emptied");
        // SAFETY: the mapping made above, unmapped once.
        unsafe { libc::munmap(at, len) };
    }

    #[test]
    fn a_fresh_slot_is_reset_before_its_offer() {
        let pool = pool(room(PLENTY));
        let slot = take(pool, &room(PLENTY), None).unwrap();
        slot.word(field::TO_GUEST_HEAD).store(99, Ordering::Relaxed);
        slot.flag(field::HOST_WAITS_ROOM).store(1, Ordering::Relaxed);
        slot.reset();
        assert_eq!(slot.word(field::TO_GUEST_HEAD).load(Ordering::Relaxed), 0);
        assert_eq!(slot.flag(field::HOST_WAITS_ROOM).load(Ordering::Relaxed), 0);
        lock(pool).release(slot, false);
        assert_eq!(held(pool), (0, 0));
    }
}
