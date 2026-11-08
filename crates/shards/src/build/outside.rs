//! A build's own writes to its host's disk, made outside the budget it is held to (D188,
//! PM M442), on Linux.
//!
//! A build held to a budget (its cgroup's, or one above it, `memory.high` or `memory.max`)
//! wrote the files it makes (a `RUN` step's changes, its layers, its image's root
//! filesystem) at its disk's pace, where with none they went at its memory's: the page
//! cache a write dirties is charged to the writer's cgroup, and the kernel holds a cgroup's
//! dirty pages to its own share of what its budget leaves it. `balance_dirty_pages`
//! (mm/page-writeback.c, Linux 7.0) throttles a writer against its own cgroup's domain as
//! well as the host's (`mdtc`, `mem_cgroup_wb_domain` of the writer's own wb,
//! `wb_get_create_current` in `balance_dirty_pages_ratelimited_flags`), whose memory
//! available to dirty is its file pages and no more of its headroom than the budget leaves
//! (`mdtc_calc_avail`: `filepages + min(headroom, other_clean)`; mm/memcontrol.c,
//! `mem_cgroup_wb_stats`: the headroom is `min(memory.max, memory.high)` less what the
//! cgroup uses, the least of it and each above it). A budget its build fills leaves no
//! headroom and few file pages, so its writer passes `dirty_ratio`'s share of them
//! (`domain_dirty_limits`) after a few megabytes and is paced to its writeback; what it
//! dirties past the budget waits besides in `memory.high`'s throttle until it is written
//! back. With no budget the same pages are the host's to dirty, flushed after.
//!
//! So a build held to one hands its writes to a process of its own placed outside it: its
//! user's manager makes that process a scope of its own in the user's application slice
//! (systemd.special(7), `app.slice`), as the daemon places a spawner (D183), and where that
//! scope is not under the budget it writes what the build gives it, each write to the
//! build's own open file (passed with it, SCM_RIGHTS), at that file's offset, as the build
//! would have (`shards_image::through`). The page cache they dirty is charged to it, and
//! paced by the host's dirty limit alone, as BuildKit's own writes are made by buildkitd,
//! never in the cgroup its `RUN` steps are given (docker build `--cgroup-parent`: "the
//! parent cgroup for the RUN instructions"). The builder's VM process, whose memory the
//! budget holds, stays in it. Where nothing places it there (no user manager), the build
//! writes its files itself.
//!
//! The bytes go in slots of memory the two share, not in the messages: a message names the
//! slot and how many bytes, the file comes with it, and the writer writes from the slot as
//! the build would have from its buffer. Sent in the messages, each MiB was copied into the
//! socket, out of it into memory zeroed for it, and only then written, and the build waited
//! for all of it: rust:1-slim's export, its 986 MiB root filesystem, took 0.22 to 0.29 s
//! longer held than free on each of CI's four runners, its sync no longer (PM M442). With
//! two slots the writer makes one write while the build fills the next and hands it on
//! (`through::Through` goes on once a write is handed), so that the build's work and the
//! writer's overlap, and the writer has its next write as soon as it is done. Once a write
//! fails, the writer makes none of its owner's that the build sent before it knew, which
//! would land past what the failed one left. The writer allocates the slots' pages itself, once its manager has placed
//! it, so that they are charged to it and not to the budget: shmem's fallocate charges a
//! page to the cgroup of the task that allocates it (Linux 7.0, mm/shmem.c
//! `shmem_fallocate`, `shmem_get_folio_gfp` with no VMA, so `shmem_alloc_and_add_folio`
//! with no `fault_mm`; mm/memcontrol.c `get_mem_cgroup_from_mm`, `current->mm`'s), the
//! build's faults on its mapping find them allocated, and a charge stays where it was made
//! when its task moves (Documentation/admin-guide/cgroup-v2.rst, "Memory Ownership").

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use shards_image::through::Owner;

/// Build → writer: write what a slot holds to the descriptor that comes with it. Its
/// payload: whose write it is (8 bytes), the slot and how many bytes (4 bytes each), each
/// big-endian.
const WRITE: u8 = 1;
/// Writer → build: it wrote them all (8 bytes, big-endian: how many).
const WROTE: u8 = 2;
/// Writer → build: why it did not: the error's number (4 bytes, big-endian, 0 for none),
/// then its words.
const FAILED: u8 = 3;
/// Build → writer: the memory file the slots are in, which comes with it, to allocate and
/// map. Answered `READY` or `FAILED`.
const SLOTS: u8 = 4;
/// Writer → build: the slots are allocated and mapped, or a range is served.
const READY: u8 = 5;
/// Build → writer: read and write, for the builder's VM process, the file that comes with
/// it first, at the offsets the VM process asks for on the socket that comes second
/// (`shards_vmm::devices::virtio::block::Through`), until it hangs up. Answered `READY`
/// once a thread of the writer's serves it, or `FAILED`.
const SERVE: u8 = 6;

/// How many slots: one the writer writes from, and the next, which the build fills and
/// hands while it does, so that the writer has it as soon as it is done.
const SLOTS_N: usize = 2;
/// One slot's bytes, the most one write hands over: what a build's buffered writers gather.
const SLOT: usize = shards_image::through::CHUNK;

/// Where cgroup v2 is mounted.
const ROOT: &str = "/sys/fs/cgroup";

/// The slice the writer's scope is made in: the user's manager's own for applications
/// (systemd.special(7)).
const SLICE: &str = "app.slice";

/// The writer outside the build's budget: what is handed to it, and its process.
struct Outside {
    state: Mutex<State>,
    _child: shards_ipc::Child,
}

/// This process's writer, once one is placed outside its budget: for its life.
static OUTSIDE: std::sync::OnceLock<Outside> = std::sync::OnceLock::new();

/// The build's writes, handed to [`OUTSIDE`].
struct Handed;

impl shards_image::through::Writer for Handed {
    fn hand(&self, owner: Owner, file: &File, buf: &[u8]) -> io::Result<usize> {
        let outside = OUTSIDE.get().ok_or_else(|| io::Error::other("no writer"))?;
        let mut state = outside.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hand(owner, file, buf)
    }

    fn settle(&self, owner: Owner) -> io::Result<()> {
        let outside = OUTSIDE.get().ok_or_else(|| io::Error::other("no writer"))?;
        let mut state = outside.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.settle(owner)
    }
}

/// The connection a builder's VM process reads and writes `file`, a cache's layer or a
/// file for its changes (D134), through: to this build's writer, outside its budget, so
/// that the page cache the guest's reads and writes of it fault is charged there, where
/// the budget's reclaim does not take it (PM M442); none where no writer is placed there,
/// and the VM process reads and writes it itself.
pub fn through(file: &File) -> Option<UnixStream> {
    let outside = OUTSIDE.get()?;
    let mut state = outside.state.lock().unwrap_or_else(PoisonError::into_inner);
    match state.serve(file) {
        Ok(conn) => Some(conn),
        Err(e) => {
            shards_vmm::debug!(
                "a cache's file read and written by the builder itself, under its budget: {e}"
            );
            None
        }
    }
}

/// The build's side of its writer.
struct State {
    conn: UnixStream,
    slots: Slots,
    /// Slots no write is in.
    free: Vec<usize>,
    /// Writes sent and not yet answered, in the order they were sent: whose, the slot, how
    /// many bytes.
    flight: VecDeque<(Owner, usize, usize)>,
    /// Why a write of an owner's failed: told at its next hand, and at its settle.
    failed: HashMap<Owner, Failure>,
    /// Why the writer takes no more, once it takes none.
    ended: Option<String>,
}

impl State {
    fn new(conn: UnixStream, slots: Slots) -> State {
        State {
            conn,
            slots,
            free: (0..SLOTS_N).rev().collect(),
            flight: VecDeque::new(),
            failed: HashMap::new(),
            ended: None,
        }
    }

    /// Takes some of `buf` to write to `file`: once it is in a slot and sent, not once it
    /// is written.
    fn hand(&mut self, owner: Owner, file: &File, buf: &[u8]) -> io::Result<usize> {
        self.failure(owner)?;
        let chunk = buf.get(..buf.len().min(SLOT)).unwrap_or_default();
        if chunk.is_empty() {
            return Ok(0);
        }
        let slot = loop {
            if let Some(slot) = self.free.pop() {
                break slot;
            }
            if self.flight.is_empty() {
                return Err(io::Error::other("the build's writer has no slot"));
            }
            self.answer()?;
            self.failure(owner)?;
        };
        let sent = self.send(owner, slot, file, chunk);
        if sent.is_err() {
            self.free.push(slot);
        }
        sent.map(|()| chunk.len())
    }

    fn send(&mut self, owner: Owner, slot: usize, file: &File, chunk: &[u8]) -> io::Result<()> {
        // Filled while the writer makes the writes before.
        self.slots.put(slot, chunk)?;
        let word = |v: usize| u32::try_from(v).map_err(|_| io::Error::other("a slot past 4 GiB"));
        let mut payload = owner.id().to_be_bytes().to_vec();
        payload.extend_from_slice(&word(slot)?.to_be_bytes());
        payload.extend_from_slice(&word(chunk.len())?.to_be_bytes());
        if let Err(e) = shards_ipc::send(&self.conn, WRITE, &payload, &[file.as_fd()]) {
            self.end(format!("the build's writer: {e}"));
            return Err(e);
        }
        self.flight.push_back((owner, slot, chunk.len()));
        Ok(())
    }

    /// Has the writer serve `file` for a builder's VM process: the connection the VM
    /// process asks on. Every write in flight is answered first, so that the answer read
    /// next is this one's.
    fn serve(&mut self, file: &File) -> io::Result<UnixStream> {
        if let Some(why) = &self.ended {
            return Err(io::Error::other(why.clone()));
        }
        while !self.flight.is_empty() {
            self.answer()?;
        }
        let (ours, theirs) = UnixStream::pair()?;
        if let Err(e) = shards_ipc::send(&self.conn, SERVE, &[], &[file.as_fd(), theirs.as_fd()]) {
            self.end(format!("the build's writer: {e}"));
            return Err(e);
        }
        // Held until it answers, which it does holding its own (M24).
        let answer = shards_ipc::recv(&self.conn);
        drop(theirs);
        let answer = match answer.and_then(|m| m.ok_or_else(|| io::Error::other("the build's writer ended")))
        {
            Ok(m) => m,
            Err(e) => {
                self.end(format!("the build's writer: {e}"));
                return Err(e);
            }
        };
        match answer.kind {
            READY => Ok(ours),
            FAILED => Err(failure(&answer.payload)
                .map_or_else(|| io::Error::other("the build's writer failed"), |f| f.error())),
            other => Err(io::Error::other(format!("the build's writer answered {other}"))),
        }
    }

    /// Waits until every write of `owner`'s is answered: why one failed, if one did.
    fn settle(&mut self, owner: Owner) -> io::Result<()> {
        while self.flight.iter().any(|&(o, _, _)| o == owner) {
            // Ended, its writes in flight are failed, and none is left.
            if self.answer().is_err() {
                break;
            }
        }
        match self.failed.remove(&owner) {
            Some(f) => Err(f.error()),
            None => Ok(()),
        }
    }

    fn failure(&self, owner: Owner) -> io::Result<()> {
        if let Some(f) = self.failed.get(&owner) {
            return Err(f.error());
        }
        if let Some(why) = &self.ended {
            return Err(io::Error::other(why.clone()));
        }
        Ok(())
    }

    /// The writer's answer to the oldest write in flight.
    fn answer(&mut self) -> io::Result<()> {
        let Some(&(owner, slot, asked)) = self.flight.front() else {
            return Ok(());
        };
        let got = shards_ipc::recv(&self.conn)
            .and_then(|m| m.ok_or_else(|| io::Error::other("the build's writer ended")));
        match got {
            Ok(m) => {
                self.flight.pop_front();
                self.free.push(slot);
                if let Err(f) = answered(m.kind, &m.payload, asked) {
                    self.failed.entry(owner).or_insert(f);
                }
                Ok(())
            }
            Err(e) => {
                self.end(format!("the build's writer: {e}"));
                Err(e)
            }
        }
    }

    /// The writer gone: every write in flight failed, and every one to come.
    fn end(&mut self, why: String) {
        for (owner, slot, _) in self.flight.drain(..) {
            self.free.push(slot);
            self.failed.entry(owner).or_insert(Failure {
                code: 0,
                words: why.clone(),
            });
        }
        if self.ended.is_none() {
            self.ended = Some(why);
        }
    }
}

/// A write that failed: the error's number (0 for none), and its words.
#[derive(Debug)]
struct Failure {
    code: i32,
    words: String,
}

impl Failure {
    fn error(&self) -> io::Error {
        match self.code {
            0 => io::Error::other(self.words.clone()),
            code => io::Error::from_raw_os_error(code),
        }
    }
}

/// A writer's answer to a write of `asked` bytes: all written, or why not.
fn answered(kind: u8, payload: &[u8], asked: usize) -> Result<(), Failure> {
    let malformed = || Failure {
        code: 0,
        words: "the build's writer answered a write malformed".into(),
    };
    match kind {
        WROTE => {
            let n = payload
                .first_chunk::<8>()
                .and_then(|n| usize::try_from(u64::from_be_bytes(*n)).ok())
                .ok_or_else(malformed)?;
            if n != asked {
                return Err(Failure {
                    code: 0,
                    words: format!("the build's writer wrote {n} of {asked} bytes"),
                });
            }
            Ok(())
        }
        FAILED => Err(failure(payload).ok_or_else(malformed)?),
        _ => Err(malformed()),
    }
}

/// A `FAILED` answer's failure.
fn failure(payload: &[u8]) -> Option<Failure> {
    let (code, words) = payload.split_first_chunk::<4>()?;
    Some(Failure {
        code: i32::from_be_bytes(*code),
        words: String::from_utf8_lossy(words).into_owned(),
    })
}

/// The slots, mapped: the build's to fill, the writer's to write from.
struct Slots {
    at: NonNull<u8>,
}

// SAFETY: a mapping of this process's own for its life, reached only through its owner
// (the build's under `Outside`'s lock).
unsafe impl Send for Slots {}

impl Slots {
    /// The slots' memory file `fd`, mapped shared, `prot`.
    fn map(fd: &OwnedFd, prot: libc::c_int) -> io::Result<Slots> {
        // SAFETY: mmap(2) of the file's SLOTS_N slots, shared, wherever the kernel puts it.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SLOTS_N * SLOT,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(at.cast::<u8>())
            .map(|at| Slots { at })
            .ok_or_else(|| io::Error::other("the slots mapped at 0"))
    }

    /// Where slot `slot` starts, if it holds `len` bytes.
    fn range(&self, slot: usize, len: usize) -> Option<*mut u8> {
        if slot >= SLOTS_N || len > SLOT {
            return None;
        }
        // SAFETY: slot < SLOTS_N, so its start is within the mapping.
        Some(unsafe { self.at.as_ptr().add(slot * SLOT) })
    }

    /// Puts `bytes` in slot `slot`, which no write is in.
    fn put(&mut self, slot: usize, bytes: &[u8]) -> io::Result<()> {
        let to = self
            .range(slot, bytes.len())
            .ok_or_else(|| io::Error::other("bytes past the writer's slots"))?;
        // SAFETY: `range` holds them within the mapping, which the writer reads a slot of
        // only while a write from it is in flight, and none is from this one.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
        Ok(())
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, of that length, unmapped once.
        unsafe { libc::munmap(self.at.as_ptr().cast(), SLOTS_N * SLOT) };
    }
}

/// Hands this build's writes to a writer outside the budget it is held to, where one holds
/// it and its user's manager places the writer outside: from here on, for the build's life.
/// Where none holds it, or nothing places the writer there, its writes stay its own.
pub fn begin() {
    // Once a process: each platform of a build of several is built in it.
    static BEGUN: std::sync::Once = std::sync::Once::new();
    BEGUN.call_once(begin_once);
}

fn begin_once() {
    let Some(own) = cgroup_of("self") else {
        return;
    };
    let Some(budget) = budgeted_at(&own, Path::new(ROOT)) else {
        return;
    };
    match start(&budget) {
        Ok(outside) => {
            if OUTSIDE.set(outside).is_err() || !shards_image::through::set(Box::new(Handed)) {
                shards_vmm::debug!("a build's writes were handed to a writer already");
            }
        }
        Err(e) => shards_vmm::debug!("a build's writes stay its own, under its budget: {e}"),
    }
}

/// The writer, started and placed outside `budget`.
fn start(budget: &Path) -> Result<Outside, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a connection: {e}"))?;
    let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let err = io::stderr();
    let fds = [
        (null.as_fd(), 0),
        (err.as_fd(), 1),
        (err.as_fd(), 2),
        (theirs.as_fd(), 3),
    ];
    let child = shards_ipc::spawn_in(&exe, &[OsStr::new("build-writer")], &fds, false, &[])
        .map_err(|e| format!("starting it: {e}"))?;
    drop(theirs);
    let ended = |child: shards_ipc::Child, ours: UnixStream, why: String| {
        // Its connection closed, it ends.
        drop(ours);
        let _ = child.wait();
        Err(why)
    };
    let pid = child.id();
    // SAFETY: getuid(2) takes nothing and cannot fail.
    let uid = unsafe { libc::getuid() };
    let unit = format!("shards-build-writer-{}.scope", std::process::id());
    let description = "shards: a build's writes, outside its budget";
    let placed = shards_bus::user_bus(uid)
        .and_then(|mut bus| shards_bus::place(&mut bus, &unit, SLICE, pid, description, false));
    if let Err(e) = placed {
        return ended(
            child,
            ours,
            format!("your user's systemd manager placed no writer: {e}"),
        );
    }
    match cgroup_of(&pid.to_string()) {
        // Its slots made only now, so that they are charged where it is placed.
        Some(at) if !at.starts_with(budget) => match slots(&ours) {
            Ok(slots) => Ok(Outside {
                state: Mutex::new(State::new(ours, slots)),
                _child: child,
            }),
            Err(e) => ended(child, ours, format!("its slots: {e}")),
        },
        Some(at) => ended(
            child,
            ours,
            format!(
                "its scope, {}, is under the budget's {}",
                at.display(),
                budget.display()
            ),
        ),
        None => ended(child, ours, "its cgroup could not be read".into()),
    }
}

/// The writer's slots, made here, allocated by the writer at the other end of `conn`, so
/// that their pages are charged to it ([`allocate`]), then mapped here.
fn slots(conn: &UnixStream) -> io::Result<Slots> {
    let fd = memory()?;
    shards_ipc::send(conn, SLOTS, &[], &[fd.as_fd()])?;
    let answer = shards_ipc::recv(conn)?.ok_or_else(|| io::Error::other("the build's writer ended"))?;
    match answer.kind {
        READY => Slots::map(&fd, libc::PROT_READ | libc::PROT_WRITE),
        FAILED => Err(failure(&answer.payload)
            .map_or_else(|| io::Error::other("the build's writer failed"), |f| f.error())),
        other => Err(io::Error::other(format!("the build's writer answered {other}"))),
    }
}

/// The slots' memory file, its pages not yet allocated.
fn memory() -> io::Result<OwnedFd> {
    // SAFETY: memfd_create(2) with a NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"shards-build-writes".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, which nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    File::from(fd.try_clone()?).set_len((SLOTS_N * SLOT) as u64)?;
    Ok(fd)
}

/// The cgroup v2 directory of process `pid` (`self` for this one), as its /proc entry says.
fn cgroup_of(pid: &str) -> Option<PathBuf> {
    let said = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = said.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new(ROOT).join(path.trim_start_matches('/')))
}

/// The lowest cgroup at or above `dir`, below `root`, that holds what is in it to a budget:
/// a `memory.high` or `memory.max` other than `max`.
fn budgeted_at(dir: &Path, root: &Path) -> Option<PathBuf> {
    let limited = |file: &Path| {
        std::fs::read_to_string(file)
            .ok()
            .is_some_and(|v| v.trim().parse::<u64>().is_ok())
    };
    let mut at = dir;
    while at.starts_with(root) && at != root {
        if limited(&at.join("memory.high")) || limited(&at.join("memory.max")) {
            return Some(at.to_path_buf());
        }
        at = at.parent()?;
    }
    None
}

/// `shards build-writer`: writes what the build that started it hands it on descriptor 3,
/// each to the file that comes with it, from the slots it is given first, until the build
/// closes its end. Not for people to run.
pub fn serve() -> ExitCode {
    // SAFETY: fcntl(2) asks whether descriptor 3 is open.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
        let _ = writeln!(
            io::stderr(),
            "shards: a build's writer: descriptor 3 is not the build's"
        );
        return ExitCode::FAILURE;
    }
    // SAFETY: the connection's end the build left at descriptor 3 for this process alone.
    let conn = unsafe { UnixStream::from_raw_fd(3) };
    match serve_on(&conn) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards: a build's writer: {e}");
            ExitCode::FAILURE
        }
    }
}

/// [`serve`]'s work, on `conn`, until the build closes its end.
fn serve_on(conn: &UnixStream) -> io::Result<()> {
    let mut slots: Option<Slots> = None;
    // Owners a write of whose failed: their writes the build sent before it knew are not
    // made, as they would land past what the one that failed left. A failed write ends
    // what made it, so a build's life holds few.
    let mut failed = HashSet::new();
    loop {
        let Some(m) = shards_ipc::recv(conn)? else {
            return Ok(());
        };
        let mut fds = m.fds.into_iter();
        let (kind, payload) = match (m.kind, fds.next()) {
            (SERVE, Some(fd)) => match fds.next() {
                Some(conn) => serving(File::from(fd), UnixStream::from(conn)),
                None => refused(0, "a range to serve with no connection"),
            },
            (SLOTS, Some(fd)) if slots.is_none() => match allocate(&fd) {
                Ok(made) => {
                    slots = Some(made);
                    (READY, Vec::new())
                }
                Err(e) => refused(e.raw_os_error().unwrap_or(0), &e.to_string()),
            },
            (WRITE, Some(fd)) => match &slots {
                Some(slots) => wrote(&File::from(fd), slots, &m.payload, &mut failed),
                None => refused(0, "a write before its slots"),
            },
            (WRITE, None) => refused(0, "a write with no file"),
            (other, _) => refused(0, &format!("message {other}")),
        };
        shards_ipc::send(conn, kind, &payload, &[])?;
    }
}

/// The slots' memory file `fd`, its pages allocated by this process, so charged to its
/// cgroup (mm/shmem.c `shmem_fallocate`), and mapped to be read.
fn allocate(fd: &OwnedFd) -> io::Result<Slots> {
    let len = SLOTS_N * SLOT;
    if File::from(fd.try_clone()?).metadata()?.len() != len as u64 {
        return Err(io::Error::other("slots of another size"));
    }
    let len = libc::off_t::try_from(len).map_err(|_| io::Error::other("slots past off_t"))?;
    // SAFETY: fallocate(2) of the file's whole length.
    if unsafe { libc::fallocate(fd.as_raw_fd(), 0, 0, len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Slots::map(fd, libc::PROT_READ)
}

/// Writes what `payload` names of `slots` to `file` at its offset, all of it: the answer,
/// how many bytes it wrote, or why it did not. The build has gone on, so a write cut short
/// failed, and its owner's later ones are not made (`failed`): no rest of it comes.
fn wrote(file: &File, slots: &Slots, payload: &[u8], failed: &mut HashSet<u64>) -> (u8, Vec<u8>) {
    let owner = payload.first_chunk::<8>().map(|b| u64::from_be_bytes(*b));
    let word = |at: usize| {
        payload
            .get(at..)
            .and_then(|b| b.first_chunk::<4>())
            .map(|b| u32::from_be_bytes(*b) as usize)
    };
    let (Some(owner), Some(slot), Some(len), 16) = (owner, word(8), word(12), payload.len()) else {
        return refused(0, "a write malformed");
    };
    if failed.contains(&owner) {
        return refused(0, "a write after one of its file's that failed");
    }
    let Some(from) = slots.range(slot, len) else {
        failed.insert(owner);
        return refused(0, "a write past its slots");
    };
    let mut done = 0usize;
    while done < len {
        // SAFETY: write(2) of the slot's bytes, within the mapping (`range`), which the
        // build fills again only once this write is answered.
        let n = unsafe { libc::write(file.as_raw_fd(), from.add(done).cast(), len - done) };
        match usize::try_from(n) {
            Ok(0) => {
                failed.insert(owner);
                return refused(0, "a write of no bytes");
            }
            Ok(n) => done += n,
            Err(_) => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    failed.insert(owner);
                    return refused(e.raw_os_error().unwrap_or(0), &e.to_string());
                }
            }
        }
    }
    (WROTE, (done as u64).to_be_bytes().to_vec())
}

/// Serves `file` to a builder's VM process on `conn`, on a thread of its own: the answer
/// to the build.
fn serving(file: File, conn: UnixStream) -> (u8, Vec<u8>) {
    match std::thread::Builder::new()
        .name("range".into())
        .spawn(move || serve_range(&file, conn))
    {
        Ok(_) => (READY, Vec::new()),
        Err(e) => refused(
            e.raw_os_error().unwrap_or(0),
            &format!("a thread for a range: {e}"),
        ),
    }
}

/// Reads and writes `file` as the VM process at the other end of `conn` asks
/// (`shards_vmm::devices::virtio::block::Through`), until it hangs up or asks what it may
/// not: each at the offset asked, at most `THROUGH_MAX` bytes, answered with the error's
/// number (0 for none) and how many bytes it moved, a read's after them.
fn serve_range(file: &File, mut conn: UnixStream) {
    use shards_vmm::devices::virtio::block::{THROUGH_MAX, THROUGH_READ, THROUGH_WRITE};
    use std::io::Read as _;
    use std::os::unix::fs::FileExt as _;
    let mut buf = vec![0u8; THROUGH_MAX];
    loop {
        let mut head = [0u8; 13];
        if conn.read_exact(&mut head).is_err() {
            return;
        }
        let (op, rest) = head.split_at(1);
        let (offset, len) = rest.split_at(8);
        let offset = u64::from_be_bytes(offset.try_into().unwrap_or_default());
        let len = u32::from_be_bytes(len.try_into().unwrap_or_default()) as usize;
        let Some(at) = buf.get_mut(..len) else {
            return;
        };
        let (done, code) = match op.first().copied() {
            Some(THROUGH_READ) => match file.read_at(at, offset) {
                Ok(n) => (n, 0),
                Err(e) => (0, e.raw_os_error().unwrap_or(libc::EIO)),
            },
            Some(THROUGH_WRITE) => {
                if conn.read_exact(at).is_err() {
                    return;
                }
                match file.write_all_at(at, offset) {
                    Ok(()) => (len, 0),
                    Err(e) => (0, e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            _ => return,
        };
        let mut answer = code.to_be_bytes().to_vec();
        answer.extend_from_slice(&u32::try_from(done).unwrap_or(0).to_be_bytes());
        if op.first() == Some(&THROUGH_READ) {
            answer.extend_from_slice(at.get(..done).unwrap_or_default());
        }
        if conn.write_all(&answer).is_err() {
            return;
        }
    }
}

fn refused(code: i32, why: &str) -> (u8, Vec<u8>) {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(why.as_bytes());
    (FAILED, payload)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read as _, Seek as _};

    /// A budget is found at the cgroup, or the lowest above it, whose `memory.high` or
    /// `memory.max` is a number; `max` holds nothing, and the root is never one.
    #[test]
    fn a_budget_is_the_lowest_limit_above_the_build() {
        let dir = shards_testdir::TempDir::new("outside-budget").unwrap();
        let root = dir.path().join("cg");
        let slice = root.join("budget.slice");
        let scope = slice.join("run-1.scope");
        std::fs::create_dir_all(&scope).unwrap();
        for (at, high, max) in [
            (&root, "1024", "1024"),
            (&slice, "max", "max"),
            (&scope, "max", "max"),
        ] {
            std::fs::write(at.join("memory.high"), format!("{high}\n")).unwrap();
            std::fs::write(at.join("memory.max"), format!("{max}\n")).unwrap();
        }
        assert_eq!(budgeted_at(&scope, &root), None, "the root holds no budget");
        std::fs::write(slice.join("memory.max"), "1073741824\n").unwrap();
        assert_eq!(budgeted_at(&scope, &root), Some(slice.clone()));
        std::fs::write(scope.join("memory.high"), "536870912\n").unwrap();
        assert_eq!(budgeted_at(&scope, &root), Some(scope.clone()));
        assert_eq!(budgeted_at(&dir.path().join("elsewhere"), &root), None);
    }

    /// The build's side of a writer serving on a thread of this process.
    fn writer() -> (State, std::thread::JoinHandle<io::Result<()>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let serving = std::thread::spawn(move || serve_on(&theirs));
        let slots = slots(&ours).unwrap();
        (State::new(ours, slots), serving)
    }

    fn hand_all(state: &mut State, owner: Owner, file: &File, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = state.hand(owner, file, buf)?;
            buf = buf.get(n..).unwrap();
        }
        Ok(())
    }

    /// Writes handed on land where the build would have made them, in order, at the
    /// offset their file's description has, more than the slots hold at once among them,
    /// and two files' writes interleaved; each is made once its owner settles.
    #[test]
    fn writes_handed_on_land_in_order_at_their_files_offsets() {
        let dir = shards_testdir::TempDir::new("outside-write").unwrap();
        let open = |name: &str| {
            File::options()
                .create_new(true)
                .read(true)
                .write(true)
                .open(dir.path().join(name))
                .unwrap()
        };
        let (mut a, mut b) = (open("a"), open("b"));
        a.write_all(b"abc").unwrap();
        let (mut state, serving) = writer();
        let (oa, ob) = (Owner::unique(), Owner::unique());
        let big: Vec<u8> = (0..SLOT * SLOTS_N + 12_345).map(|i| (i % 251) as u8).collect();
        hand_all(&mut state, oa, &a, b"defg").unwrap();
        hand_all(&mut state, ob, &b, b"one").unwrap();
        hand_all(&mut state, oa, &a, &big).unwrap();
        hand_all(&mut state, ob, &b, b" two").unwrap();
        state.settle(oa).unwrap();
        state.settle(ob).unwrap();
        assert!(state.flight.is_empty());
        let mut back = Vec::new();
        a.seek(io::SeekFrom::Start(0)).unwrap();
        a.read_to_end(&mut back).unwrap();
        let mut want = b"abcdefg".to_vec();
        want.extend_from_slice(&big);
        assert!(back == want, "a: {} bytes, {} wanted", back.len(), want.len());
        let mut back = String::new();
        b.seek(io::SeekFrom::Start(0)).unwrap();
        b.read_to_string(&mut back).unwrap();
        assert_eq!(back, "one two");
        drop(state);
        serving.join().unwrap().unwrap();
    }

    /// A write the writer cannot make is told at its owner's settle, its error's kind kept;
    /// its owner's writes after it are refused unsent, and once settled the owner may write
    /// again. Another owner's writes are not its.
    #[test]
    fn a_failed_write_is_told_at_its_settle_and_stops_its_owners_rest() {
        let dir = shards_testdir::TempDir::new("outside-fail").unwrap();
        let path = dir.path().join("f");
        let mut f = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let read_only = File::open(&path).unwrap();
        let (mut state, serving) = writer();
        let (bad, good) = (Owner::unique(), Owner::unique());
        assert_eq!(state.hand(bad, &read_only, b"x").unwrap(), 1, "taken unwritten");
        hand_all(&mut state, good, &f, b"kept").unwrap();
        let e = state.hand(bad, &read_only, b"y").unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EBADF), "the one before failed");
        let e = state.settle(bad).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EBADF));
        state.settle(good).unwrap();
        state.settle(bad).unwrap();
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "kept");
        drop(state);
        serving.join().unwrap().unwrap();
    }

    /// The writer gone, what was in flight fails at its settle, and every write after it
    /// at its hand.
    #[test]
    fn a_writer_gone_fails_its_writes() {
        let dir = shards_testdir::TempDir::new("outside-gone").unwrap();
        let f = File::create(dir.path().join("f")).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let serving = std::thread::spawn(move || {
            // Its slots allocated, it ends before it answers a write.
            let m = shards_ipc::recv(&theirs).unwrap().unwrap();
            let fd = m.fds.into_iter().next().unwrap();
            let _slots = allocate(&fd).unwrap();
            shards_ipc::send(&theirs, READY, &[], &[]).unwrap();
            let _ = shards_ipc::recv(&theirs);
        });
        let slots = slots(&ours).unwrap();
        let mut state = State::new(ours, slots);
        let owner = Owner::unique();
        assert_eq!(state.hand(owner, &f, b"lost").unwrap(), 4);
        serving.join().unwrap();
        assert!(state.settle(owner).is_err());
        assert!(state.hand(Owner::unique(), &f, b"more").is_err());
    }

    /// A write after one of its owner's that failed is not made, though its file would take
    /// it; another owner's is, and one past the slots is not.
    #[test]
    fn a_write_after_its_owners_failed_one_is_not_made() {
        let dir = shards_testdir::TempDir::new("outside-after").unwrap();
        let path = dir.path().join("f");
        let mut f = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let read_only = File::open(&path).unwrap();
        let fd = memory().unwrap();
        let theirs = allocate(&fd).unwrap();
        let mut ours = Slots::map(&fd, libc::PROT_READ | libc::PROT_WRITE).unwrap();
        ours.put(1, b"made").unwrap();
        let write = |owner: u64, slot: u32, len: u32| {
            let mut p = owner.to_be_bytes().to_vec();
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&len.to_be_bytes());
            p
        };
        let mut failed = HashSet::new();
        let (kind, payload) = wrote(&read_only, &theirs, &write(7, 1, 4), &mut failed);
        assert_eq!(failure(&payload).unwrap().code, libc::EBADF);
        assert_eq!(kind, FAILED);
        let (kind, _) = wrote(&f, &theirs, &write(7, 1, 4), &mut failed);
        assert_eq!(kind, FAILED, "after its owner's failed write");
        let (kind, payload) = wrote(&f, &theirs, &write(8, 1, 4), &mut failed);
        assert!(answered(kind, &payload, 4).is_ok());
        let (kind, _) = wrote(&f, &theirs, &write(9, 2, 4), &mut failed);
        assert_eq!(kind, FAILED, "past the slots");
        let (kind, _) = wrote(&f, &theirs, &write(9, 0, 5)[..15], &mut failed);
        assert_eq!(kind, FAILED, "malformed");
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "made");
    }

    /// A range served for a VM process is read and written where it asks, its writes
    /// landing among the build's own handed on, and a read past the file's end moves what
    /// there is.
    #[test]
    fn a_served_range_reads_and_writes_where_it_is_asked() {
        use shards_vmm::devices::virtio::block::{THROUGH_READ, THROUGH_WRITE};
        let dir = shards_testdir::TempDir::new("outside-range").unwrap();
        let mut f = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(dir.path().join("f"))
            .unwrap();
        f.write_all(b"0123456789").unwrap();
        let (mut state, serving) = writer();
        let mut conn = state.serve(&f).unwrap();
        let ask = |conn: &mut UnixStream, op: u8, offset: u64, bytes: &[u8], len: u32| {
            let mut head = vec![op];
            head.extend_from_slice(&offset.to_be_bytes());
            head.extend_from_slice(&len.to_be_bytes());
            head.extend_from_slice(bytes);
            conn.write_all(&head).unwrap();
            let mut answer = [0u8; 8];
            conn.read_exact(&mut answer).unwrap();
            let code = i32::from_be_bytes(answer[..4].try_into().unwrap());
            let n = u32::from_be_bytes(answer[4..].try_into().unwrap()) as usize;
            let mut got = vec![0u8; if op == THROUGH_READ { n } else { 0 }];
            conn.read_exact(&mut got).unwrap();
            (code, n, got)
        };
        assert_eq!(ask(&mut conn, THROUGH_WRITE, 4, b"ab", 2), (0, 2, Vec::new()));
        assert_eq!(ask(&mut conn, THROUGH_READ, 2, b"", 5), (0, 5, b"23ab6".to_vec()));
        assert_eq!(ask(&mut conn, THROUGH_READ, 8, b"", 9), (0, 2, b"89".to_vec()));
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "0123ab6789");
        drop(conn);
        drop(state);
        serving.join().unwrap().unwrap();
    }

    /// An answer that says fewer bytes than were handed, or is no answer, fails its write.
    #[test]
    fn an_answer_short_or_malformed_fails_its_write() {
        assert!(answered(WROTE, &4u64.to_be_bytes(), 4).is_ok());
        assert!(answered(WROTE, &3u64.to_be_bytes(), 4).is_err(), "fewer");
        assert!(answered(WROTE, &9u64.to_be_bytes(), 4).is_err(), "more");
        assert!(answered(WROTE, b"", 4).is_err());
        assert!(answered(7, b"", 4).is_err());
        let (kind, payload) = refused(libc::ENOSPC, "no room");
        let f = answered(kind, &payload, 4).unwrap_err();
        assert_eq!(f.error().kind(), io::ErrorKind::StorageFull);
    }
}
