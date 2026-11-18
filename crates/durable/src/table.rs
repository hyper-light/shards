//! A home's declarations of durable writes (D286): `<home>/durable-v1`, a file of slots that
//! every shards process of the home maps shared as it writes durably, and each follower of one
//! maps to read (D215), as D281's count file is mapped by every server of a volume
//! (vmm's fs/bound.rs). Lock-free: a thread claims a slot once, with one compare-and-swap of its
//! owner from none to its own, and keeps it until it ends; a declaration stores its volume, then
//! its start (release), and its end stores none (release); a reader loads a slot's start
//! (acquire), its fields, and its start again, so that it pairs no fields with another
//! declaration's. Its layout is its name's version: a build of another layout uses another file.
//!
//! The file is a whole number of granules (the least a mapping's offset steps by: a page on
//! Unix, the allocation granularity on Windows). Its first slot's place is its head: the
//! granules it has, which a reader compares with what it mapped, remapping only where the file
//! grew. Where no slot is free, a claimer takes back the slots whose process has ended, else
//! grows the file by one granule, under the file's growth lock: so it grows only as its
//! claims need, and no constant of ours sizes it (R03).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::Target;
use crate::sys::{self, Locked, Map};

/// The slot file's name in its home, its layout's version in it.
pub const FILE: &str = "durable-v1";

/// One thread's declarations. Its words are atomics in memory shared by every process that
/// maps the file. Aligned to 64 bytes, the least power of two its five words (40 bytes) fit
/// in, so that slots tile every granule (a power of two, as pages and allocation granularities
/// are) and a view of the whole file finds each where its granule's own view does.
#[repr(C, align(64))]
#[derive(Debug)]
pub(crate) struct Slot {
    /// Its claimer: its process's ID in the high half, the low half of that process's start
    /// in the low ([`owner`]); none (0) where free.
    owner: AtomicU64,
    /// Its claimer's start as its OS keeps it (`sys::start_of`), which names it with its ID.
    start: AtomicU64,
    /// Its claiming thread's ID as its OS's listing of a process's threads names it.
    thread: AtomicU64,
    /// The volume its declared write is on (`sys::volume`).
    volume: AtomicU64,
    /// When its declared write began (`sys::now`, at least 1); none (0) where it declares none.
    since: AtomicU64,
}

/// A slot's bytes, which the head's place takes too.
const SLOT: usize = std::mem::size_of::<Slot>();

/// The owner word of process `pid` started at `start`: never 0, as no process has ID 0 here.
fn owner(pid: u32, start: u64) -> u64 {
    (u64::from(pid) << 32) | (start & u64::from(u32::MAX))
}

/// A declared write, as a reader finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declared {
    /// The declaring thread's ID as its OS's listing of a process's threads names it.
    pub thread: u64,
    /// The volume it is on: Unix's `st_dev`, Windows' volume serial number.
    pub volume: u64,
    /// When it began, on [`crate::now`]'s clock.
    pub since: u64,
}

/// The slots `map` holds, `first` of it the file's first granule (whose first slot's place
/// is its head).
///
/// # Safety
/// `map` is a mapping of the slot file at a granule's start, as long as it is used.
unsafe fn slots(map: &Map, first: bool) -> impl Iterator<Item = &Slot> {
    let count = map.len.checked_div(SLOT).unwrap_or(0);
    (usize::from(first)..count).filter_map(move |k| {
        let at = k.checked_mul(SLOT)?;
        // SAFETY: slot `k` lies within the mapping (k < len / SLOT), aligned as the mapping's
        // start (a granule's) and SLOT (a multiple of 8) align it, and is read and written
        // through atomics alone, by every process mapping the file.
        Some(unsafe { &*map.ptr.as_ptr().add(at).cast::<Slot>() })
    })
}

/// The head of the file's first granule: its granules.
///
/// # Safety
/// `map` maps the file's first granule, as long as it is used.
unsafe fn head(map: &Map) -> &AtomicU64 {
    // SAFETY: the first word of the mapping, aligned as its start is, an atomic for every
    // process mapping the file.
    unsafe { &*map.ptr.as_ptr().cast::<AtomicU64>() }
}

/// Opens (making it where none is) the slot file of the home `home`, its first granule made
/// where it has none.
pub fn open(home: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(FILE))?;
    let granule = sys::granule()?;
    made(&file, granule)?;
    Ok(file)
}

/// The slot file `file`'s first granule, made where it has none, under its growth lock.
fn made(file: &File, granule: usize) -> io::Result<()> {
    let _locked = Locked::of(file)?;
    if file.metadata()?.len() == 0 {
        let first = u64::try_from(granule).map_err(io::Error::other)?;
        file.set_len(first)?;
        let map = Map::of(file, 0, granule, true)?;
        // SAFETY: `map` maps the first granule, here.
        unsafe { head(&map) }.store(1, Ordering::Release);
    }
    Ok(())
}

/// A process's writer of its home's declarations.
#[derive(Debug)]
pub(crate) struct Writer {
    file: File,
    pid: u32,
    start: u64,
    granule: usize,
    /// Each granule of the file this process maps, in order, each a mapping of its own: none
    /// let go of while it lives, as its threads' slots lie in them.
    maps: Mutex<Vec<Map>>,
}

/// This process's writer, where it installed one, or opened the slot file of the home it
/// recorded ([`install_at`]).
static WRITER: OnceLock<Writer> = OnceLock::new();

/// The home this process recorded ([`install_at`]), whose slot file it opens at its first
/// declaration or read.
static HOME: OnceLock<PathBuf> = OnceLock::new();

fn installed_already() -> io::Error {
    io::Error::other("a slot file is installed already")
}

/// Declares this process's durable writes in the slot file of the home `home` ([`open`]).
/// Once a process: a second install is refused.
pub fn install(home: &Path) -> io::Result<()> {
    install_file(open(home)?)
}

/// Declares this process's durable writes in `file`, a home's slot file [`open`] made (a
/// sandboxed process is given it by its spawner), and reads its home's declarations from it
/// ([`installed`]). Once a process: a second install is refused.
pub fn install_file(file: File) -> io::Result<()> {
    if HOME.get().is_some() {
        return Err(installed_already());
    }
    let reader = Reader::of(file.try_clone()?)?;
    let writer = Writer::of(file)?;
    WRITER.set(writer).map_err(|_| installed_already())?;
    READER.set(reader).map_err(|_| installed_already())
}

/// Declares this process's durable writes in the slot file of the home `home`, recorded now
/// and opened at its first declaration (or [`installed`]), so that a process's start pays no
/// call for it, and one that writes nothing durable opens nothing (`shards`' launch, PM
/// M113). A home not made by then has no follower, as each opens its slot file in it: it
/// takes no declaration, and the next one opens it again. Any other failure to open it fails
/// the write. Once a process: a second install is refused.
pub fn install_at(home: PathBuf) -> io::Result<()> {
    if WRITER.get().is_some() {
        return Err(installed_already());
    }
    HOME.set(home).map_err(|_| installed_already())
}

/// The home this process recorded ([`install_at`]): where a process it starts that may not
/// open the slot file itself is told to declare, granted it by this one.
pub fn home() -> Option<&'static Path> {
    HOME.get().map(PathBuf::as_path)
}

/// The slot file of `home` ([`open`]); none where `home` is not made, as no follower reads
/// one there.
fn made_in(home: &Path) -> io::Result<Option<File>> {
    match open(home) {
        Err(e) if e.kind() == io::ErrorKind::NotFound && !home.is_dir() => Ok(None),
        opened => opened.map(Some),
    }
}

/// This process's writer: the one it installed, else one of the slot file of the home it
/// recorded, opened now with its reader ([`install_at`]); none where it did neither, or its
/// home is not made.
fn writer() -> io::Result<Option<&'static Writer>> {
    if let Some(writer) = WRITER.get() {
        return Ok(Some(writer));
    }
    let Some(home) = HOME.get() else {
        return Ok(None);
    };
    let Some(file) = made_in(home)? else {
        return Ok(None);
    };
    let reader = Reader::of(file.try_clone()?)?;
    let writer = Writer::of(file)?;
    // Another thread's, opened meanwhile, is kept where it was first: either is the home's.
    let _ = READER.set(reader);
    let _ = WRITER.set(writer);
    Ok(WRITER.get())
}

/// This process's reader of its own home's declarations, where it installed its slot file
/// ([`install`]) or opened the one of the home it recorded ([`install_at`]): what a follower
/// of the home's processes reads by default.
static READER: OnceLock<Reader> = OnceLock::new();

/// The reader of the home this process installed its slot file of ([`install`]), or recorded
/// ([`install_at`]), the slot file opened then at the first call; none where it did neither,
/// or its home is not made (none declares in it). A failure to open it is its error, which a
/// follower says, never taken for no declarations.
pub fn installed() -> io::Result<Option<&'static Reader>> {
    if let Some(reader) = READER.get() {
        return Ok(Some(reader));
    }
    let Some(home) = HOME.get() else {
        return Ok(None);
    };
    let Some(file) = made_in(home)? else {
        return Ok(None);
    };
    // Another thread's, opened meanwhile, is kept where it was first: either is the home's.
    let _ = READER.set(Reader::of(file)?);
    Ok(READER.get())
}

impl Writer {
    pub(crate) fn of(file: File) -> io::Result<Writer> {
        let writer = Writer {
            file,
            pid: std::process::id(),
            start: sys::start()?,
            granule: sys::granule()?,
            maps: Mutex::new(Vec::new()),
        };
        let len = writer.file.metadata()?.len();
        if len == 0 {
            return Err(io::Error::other(
                "a slot file never made (its first granule missing)",
            ));
        }
        writer.map_to(len)?;
        Ok(writer)
    }

    /// Maps each granule of the file's first `len` bytes it has not; whether it mapped any.
    fn map_to(&self, len: u64) -> io::Result<bool> {
        let granule = u64::try_from(self.granule).map_err(io::Error::other)?;
        let mut maps = self.maps.lock().unwrap_or_else(PoisonError::into_inner);
        let mut grew = false;
        loop {
            let mapped = u64::try_from(maps.len())
                .ok()
                .and_then(|n| n.checked_mul(granule))
                .ok_or_else(|| io::Error::other("a slot file past 64 bits"))?;
            if mapped.checked_add(granule).is_none_or(|end| end > len) {
                return Ok(grew);
            }
            maps.push(Map::of(&self.file, mapped, self.granule, true)?);
            grew = true;
        }
    }

    /// A free slot of what it maps, claimed for `owner`: none where every one is taken.
    fn free(&self, owner: u64) -> Option<&'static Slot> {
        let maps = self.maps.lock().unwrap_or_else(PoisonError::into_inner);
        for (k, map) in maps.iter().enumerate() {
            // SAFETY: a granule's mapping this writer keeps as long as the process lives (it is
            // static where its slots are handed out), so its slots are too.
            for slot in unsafe { slots(map, k == 0) } {
                if slot.owner.load(Ordering::Relaxed) == 0
                    && slot
                        .owner
                        .compare_exchange(0, owner, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                {
                    // SAFETY: as above: the mapping outlives every reference handed out.
                    return Some(unsafe { &*std::ptr::from_ref(slot) });
                }
            }
        }
        None
    }

    /// A slot of what it maps whose owner has ended, taken for `owner`: none where every
    /// owner lives. An owner whose start is not yet stored is being claimed, and one whose
    /// start cannot be read (another user's, a protected process) is not shown ended: both are
    /// left, and the file grows instead.
    fn reclaim(&self, owner: u64) -> io::Result<Option<&'static Slot>> {
        let maps = self.maps.lock().unwrap_or_else(PoisonError::into_inner);
        for (k, map) in maps.iter().enumerate() {
            // SAFETY: as in `free`.
            for slot in unsafe { slots(map, k == 0) } {
                let was = slot.owner.load(Ordering::Acquire);
                let start = slot.start.load(Ordering::Acquire);
                if was == 0 || start == 0 {
                    continue;
                }
                let pid = u32::try_from(was >> 32).map_err(io::Error::other)?;
                match sys::start_of(pid) {
                    Ok(Some(now)) if now == start => continue,
                    Ok(_) => {}
                    Err(_) => continue,
                }
                if slot
                    .owner
                    .compare_exchange(was, owner, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    // What its ended owner declared ends, before it is anyone's.
                    slot.since.store(0, Ordering::Release);
                    slot.start.store(0, Ordering::Release);
                    // SAFETY: as in `free`.
                    return Ok(Some(unsafe { &*std::ptr::from_ref(slot) }));
                }
            }
        }
        Ok(None)
    }

    /// The file one granule longer than `len`, mapped, its head saying so.
    fn grow(&self, len: u64) -> io::Result<()> {
        let granule = u64::try_from(self.granule).map_err(io::Error::other)?;
        let longer = len
            .checked_add(granule)
            .ok_or_else(|| io::Error::other("a slot file past 64 bits"))?;
        self.file.set_len(longer)?;
        self.map_to(longer)?;
        let granules = longer.checked_div(granule).unwrap_or(0);
        let maps = self.maps.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(first) = maps.first() {
            // SAFETY: the first granule's mapping, which this writer keeps.
            unsafe { head(first) }.store(granules, Ordering::Release);
        }
        Ok(())
    }
}

/// A slot of `writer`'s claimed by this thread: a free one, else, under the growth lock, one
/// another process mapped since, one whose owner ended, or one of a granule more.
pub(crate) fn claim(writer: &'static Writer) -> io::Result<&'static Slot> {
    let owner = owner(writer.pid, writer.start);
    let thread = sys::thread()?;
    let take = |slot: &'static Slot| {
        slot.thread.store(thread, Ordering::Relaxed);
        slot.start.store(writer.start, Ordering::Release);
        slot
    };
    loop {
        if let Some(slot) = writer.free(owner) {
            return Ok(take(slot));
        }
        let _locked = Locked::of(&writer.file)?;
        let len = writer.file.metadata()?.len();
        if writer.map_to(len)? {
            continue;
        }
        if let Some(slot) = writer.reclaim(owner)? {
            return Ok(take(slot));
        }
        writer.grow(len)?;
    }
}

/// A slot's owner gone: what it declared, then the slot, given back.
fn release(slot: &Slot) {
    slot.since.store(0, Ordering::Release);
    slot.start.store(0, Ordering::Release);
    slot.owner.store(0, Ordering::Release);
}

/// This thread's slot, and how deep in a declared write it is (an outer write's declaration
/// stands for those it makes inside).
struct Thread {
    slot: std::cell::Cell<Option<&'static Slot>>,
    depth: std::cell::Cell<u32>,
}

impl Drop for Thread {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.get() {
            release(slot);
        }
    }
}

thread_local! {
    static THREAD: Thread = const {
        Thread {
            slot: std::cell::Cell::new(None),
            depth: std::cell::Cell::new(0),
        }
    };
}

/// A write declared in `slot`, ended as this drops, whatever its call did: its start stored
/// none, and the slot given back where it was this write's alone.
struct Declaring {
    slot: &'static Slot,
    alone: bool,
}

impl Drop for Declaring {
    fn drop(&mut self) {
        if self.alone {
            release(self.slot);
        } else {
            self.slot.since.store(0, Ordering::Release);
            let _ = THREAD.try_with(|t| t.depth.set(0));
        }
    }
}

/// `write`, a durable write of `file`, declared where this process installed a slot file (or
/// recorded its home, its slot file opened now: [`writer`]): its volume, then its start,
/// stored in this thread's slot (claimed once) before it, its start stored none after it. A
/// write made inside another's declaration is that one's. A thread whose slot is gone (its
/// storage torn down as it ends) claims one for this write alone.
pub(crate) fn declared<T>(file: &impl Target, write: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let Some(writer) = writer()? else {
        return write();
    };
    let volume = sys::volume(file)?;
    let nested = THREAD.try_with(|t| t.depth.get() > 0).unwrap_or(false);
    if nested {
        return write();
    }
    let held = THREAD.try_with(|t| t.slot.get());
    let declaring = match held {
        Ok(Some(slot)) => Declaring { slot, alone: false },
        Ok(None) => {
            let slot = claim(writer)?;
            let kept = THREAD.try_with(|t| t.slot.set(Some(slot))).is_ok();
            Declaring { slot, alone: !kept }
        }
        Err(_) => Declaring {
            slot: claim(writer)?,
            alone: true,
        },
    };
    if !declaring.alone {
        let _ = THREAD.try_with(|t| t.depth.set(1));
    }
    declaring.slot.volume.store(volume, Ordering::Relaxed);
    declaring.slot.since.store(sys::now()?.max(1), Ordering::Release);
    let wrote = write();
    drop(declaring);
    wrote
}

/// A follower's reader of a home's declarations ([`Reader::declared`]): the slot file mapped
/// once, again only as it grows.
#[derive(Debug)]
pub struct Reader {
    file: File,
    granule: usize,
    map: Mutex<Option<Map>>,
}

impl Reader {
    /// A reader of the home `home`'s slot file, made where none is ([`open`]).
    pub fn open(home: &Path) -> io::Result<Reader> {
        Reader::of(open(home)?)
    }

    /// A reader of `file`, a home's slot file [`open`] made.
    pub fn of(file: File) -> io::Result<Reader> {
        Ok(Reader {
            file,
            granule: sys::granule()?,
            map: Mutex::new(None),
        })
    }

    /// The writes process `pid`, started at `start` (as its OS keeps it, `sys::start_of`),
    /// declares now: each its thread's, its volume and when it began. Memory alone, but where
    /// the file grew since the last read: then it is mapped again, whole.
    pub fn declared(&self, pid: u32, start: u64) -> io::Result<Vec<Declared>> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let granule = u64::try_from(self.granule).map_err(io::Error::other)?;
        let grown = match map.as_ref() {
            None => true,
            Some(mapped) => {
                // SAFETY: `mapped` maps the file from its start, as long as it is held here.
                let granules = unsafe { head(mapped) }.load(Ordering::Acquire);
                let mapped_len = u64::try_from(mapped.len).map_err(io::Error::other)?;
                granules.checked_mul(granule).is_none_or(|len| len > mapped_len)
            }
        };
        if grown {
            let len = usize::try_from(self.file.metadata()?.len()).map_err(io::Error::other)?;
            *map = Some(Map::of(&self.file, 0, len, false)?);
        }
        let Some(mapped) = map.as_ref() else {
            return Ok(Vec::new());
        };
        let owner = owner(pid, start);
        let mut found = Vec::new();
        // SAFETY: `mapped` maps the whole file from its start, its first granule's head
        // skipped; held as long as the guard is.
        for slot in unsafe { slots(mapped, true) } {
            if let Some(declared) = read(slot, owner, start) {
                found.push(declared);
            }
        }
        Ok(found)
    }
}

/// What `slot` declares for the process whose owner word is `owner` and start `start`: its
/// start read before and after its fields, so that none of them is another declaration's;
/// none where it declares nothing, is another's, or changed as it was read twice over.
fn read(slot: &Slot, owner: u64, start: u64) -> Option<Declared> {
    for _ in 0..2 {
        let since = slot.since.load(Ordering::Acquire);
        if since == 0 {
            return None;
        }
        let ours = slot.owner.load(Ordering::Acquire) == owner && slot.start.load(Ordering::Acquire) == start;
        let declared = Declared {
            thread: slot.thread.load(Ordering::Relaxed),
            volume: slot.volume.load(Ordering::Relaxed),
            since,
        };
        if slot.since.load(Ordering::Acquire) == since {
            return ours.then_some(declared);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use shards_testdir::TempDir;
    use std::io::{BufRead as _, Write as _};

    /// The slot file every test of this binary declares in through [`declared`]: one a process,
    /// as a process installs one; its directory left for the next test process to sweep.
    fn installed() -> &'static Path {
        static HOME: OnceLock<TempDir> = OnceLock::new();
        let home = HOME.get_or_init(|| {
            let home = TempDir::new("durable").unwrap();
            install(home.path()).unwrap();
            home
        });
        home.path()
    }

    /// A writer of its own over a slot file of its own, for a test that fills one.
    fn writer_of(home: &Path) -> &'static Writer {
        Box::leak(Box::new(Writer::of(open(home).unwrap()).unwrap()))
    }

    fn slots_per_granule() -> usize {
        sys::granule().unwrap() / SLOT
    }

    /// A write is read as declared while it goes on, by its thread, on its file's volume, and
    /// not once it has ended; its thread's slot is kept for its next.
    #[test]
    fn a_write_is_declared_while_it_goes_on() {
        let home = installed();
        let file = File::create(home.join("declared")).unwrap();
        let reader = Reader::open(home).unwrap();
        let (pid, start) = (std::process::id(), sys::start().unwrap());
        let seen = declared(&file, || reader.declared(pid, start)).unwrap();
        let ours: Vec<_> = seen
            .iter()
            .filter(|d| d.thread == sys::thread().unwrap())
            .collect();
        assert_eq!(ours.len(), 1, "{seen:?}");
        assert_eq!(ours[0].volume, sys::volume(&file).unwrap());
        assert!(ours[0].since > 0 && ours[0].since <= crate::now().unwrap());
        let after = reader.declared(pid, start).unwrap();
        assert!(
            after.iter().all(|d| d.thread != sys::thread().unwrap()),
            "{after:?}"
        );
        // A write inside another's is the outer's: one declaration, ended with the outer.
        let inner = declared(&file, || declared(&file, || reader.declared(pid, start))).unwrap();
        assert_eq!(
            inner
                .iter()
                .filter(|d| d.thread == sys::thread().unwrap())
                .count(),
            1
        );
        crate::data(&file).unwrap();
        crate::all(&file).unwrap();
    }

    /// Claims past a granule's slots grow the file by one granule, and each claim is a slot of
    /// its own.
    #[test]
    fn claims_past_a_granule_grow_the_file_by_one() {
        let home = TempDir::new("durable-grow").unwrap();
        let writer = writer_of(home.path());
        let granule = u64::try_from(sys::granule().unwrap()).unwrap();
        assert_eq!(writer.file.metadata().unwrap().len(), granule);
        // The first granule's slots but its head's place, then one more.
        let mut claimed = Vec::new();
        for _ in 0..slots_per_granule() {
            claimed.push(std::ptr::from_ref(claim(writer).unwrap()));
        }
        assert_eq!(writer.file.metadata().unwrap().len(), 2 * granule);
        claimed.sort();
        claimed.dedup();
        assert_eq!(claimed.len(), slots_per_granule());
        let reader = Reader::open(home.path()).unwrap();
        assert!(
            reader
                .declared(std::process::id(), sys::start().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    /// A process that ended leaves its slot to the next claimer, which takes it before the
    /// file grows; one that lives keeps its own.
    #[test]
    fn an_ended_processs_slot_is_taken_back_before_the_file_grows() {
        let home = TempDir::new("durable-reclaim").unwrap();
        let writer = writer_of(home.path());
        // Every slot of the first granule claimed by this process, one then handed to a
        // process that has ended.
        let mut claimed = Vec::new();
        for _ in 1..slots_per_granule() {
            claimed.push(claim(writer).unwrap());
        }
        // A process of this binary's that declares in a home of its own, read as it lives,
        // then ended and waited for.
        let theirs = TempDir::new("durable-ended").unwrap();
        let (mut child, _) = declaring_child(theirs.path(), false);
        let (pid, start) = (child.id(), sys::start_of(child.id()).unwrap().unwrap());
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        let ended = claimed[0];
        ended.owner.store(owner(pid, start), Ordering::Release);
        ended.start.store(start, Ordering::Release);
        ended.since.store(1, Ordering::Release);
        let len = writer.file.metadata().unwrap().len();
        let taken = claim(writer).unwrap();
        assert!(std::ptr::eq(taken, ended));
        assert_eq!(taken.since.load(Ordering::Acquire), 0);
        assert_eq!(writer.file.metadata().unwrap().len(), len);
        let reader = Reader::open(home.path()).unwrap();
        assert!(reader.declared(pid, start).unwrap().is_empty());
    }

    /// The test binary's own process, run by [`a_write_another_process_declares_is_read`] and
    /// [`a_recorded_home_is_opened_at_its_first_declaration`]: it declares a write in the slot
    /// file of `SHARDS_DURABLE_TEST_HOME`, installed there, or with `SHARDS_DURABLE_TEST_AT`
    /// its home recorded alone ([`install_at`]), which opens nothing before that write; says
    /// so on its stdout, and holds it until a line comes on its stdin.
    #[test]
    #[ignore = "a child of a_write_another_process_declares_is_read"]
    fn declares_until_told() {
        let Some(home) = std::env::var_os("SHARDS_DURABLE_TEST_HOME") else {
            return;
        };
        let home = Path::new(&home);
        if std::env::var_os("SHARDS_DURABLE_TEST_AT").is_some() {
            install_at(home.to_path_buf()).unwrap();
            assert!(
                !home.join(FILE).exists(),
                "its slot file opened before a declaration"
            );
        } else {
            install(home).unwrap();
        }
        let file = File::create(home.join("theirs")).unwrap();
        declared(&file, || {
            println!("declared {}", sys::thread()?);
            std::io::stdout().flush()?;
            std::io::stdin().lock().read_line(&mut String::new())
        })
        .unwrap();
        assert!(super::installed().unwrap().is_some());
    }

    /// [`declares_until_told`] run as a process of its own declaring in `home`, its home
    /// recorded alone where `at`, once it has said so: it and its declaring thread's ID.
    fn declaring_child(home: &Path, at: bool) -> (std::process::Child, u64) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "table::tests::declares_until_told",
                "--ignored",
                "--nocapture",
            ])
            .env("SHARDS_DURABLE_TEST_HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        if at {
            command.env("SHARDS_DURABLE_TEST_AT", "1");
        }
        let mut child = command.spawn().unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let thread: u64 = lines
            .by_ref()
            .map(|l| l.unwrap())
            .find_map(|l| l.strip_prefix("declared ").map(|t| t.parse().unwrap()))
            .expect("the child declared nothing");
        (child, thread)
    }

    /// A reader sees the write another process declares, by its thread, on its file's volume,
    /// while it goes on, and nothing once it has ended.
    #[test]
    fn a_write_another_process_declares_is_read() {
        let home = TempDir::new("durable-another").unwrap();
        let (mut child, thread) = declaring_child(home.path(), false);
        let (pid, start) = (child.id(), sys::start_of(child.id()).unwrap().unwrap());
        let reader = Reader::open(home.path()).unwrap();
        let seen = reader.declared(pid, start).unwrap();
        let volume = sys::volume(&File::open(home.path().join("theirs")).unwrap()).unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!((seen[0].thread, seen[0].volume), (thread, volume));
        assert!(seen[0].since > 0);
        assert!(reader.declared(pid, start.wrapping_add(1)).unwrap().is_empty());
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(reader.declared(pid, start).unwrap().is_empty());
    }

    /// A process that recorded its home ([`install_at`]) opens its slot file at its first
    /// declaration, not before (the child checks), and that write is read as declared as an
    /// installed process's is. A home not made has no slot file to open: a write there is
    /// made undeclared.
    #[test]
    fn a_recorded_home_is_opened_at_its_first_declaration() {
        let home = TempDir::new("durable-at").unwrap();
        let (mut child, thread) = declaring_child(home.path(), true);
        let (pid, start) = (child.id(), sys::start_of(child.id()).unwrap().unwrap());
        let seen = Reader::open(home.path()).unwrap().declared(pid, start).unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].thread, thread);
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(made_in(&home.path().join("not-made")).unwrap().is_none());
    }

    /// What a file system without F_FULLFSYNC holds is made durable with fsync instead, as
    /// SQLite's `full_fsync` falls back: /dev/null refuses F_FULLFSYNC and takes fsync, as an
    /// SMB share does, where std's `sync_all` fails (review 1.13); and a directory's names.
    /// One test for the three copies of this fallback that vmm, ipc and image had.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_file_without_full_fsync_is_synced_still() {
        use std::os::fd::AsRawFd as _;
        let null = OpenOptions::new().write(true).open("/dev/null").unwrap();
        // SAFETY: fcntl(2) of a descriptor this test holds.
        let full = unsafe { libc::fcntl(null.as_raw_fd(), libc::F_FULLFSYNC) };
        assert_eq!(full, -1, "/dev/null took F_FULLFSYNC");
        crate::data(&null).unwrap();
        crate::all(&null).unwrap();
        let held = TempDir::new("sync").unwrap();
        crate::entries(File::open(held.path()).unwrap()).unwrap();
    }
}
