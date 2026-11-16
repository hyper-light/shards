//! A share's bound (D281): what the files of the directory it serves may take, by one
//! measure its server keeps as each request changes it, and refuses past.
//!
//! The measure: each name (a directory's entry: a file, a directory, a link, a symlink, a
//! device node) takes one unit, its host file system's own allocation unit; each file but a
//! directory its length in whole units, once however many names it has; and each file its
//! guest's extended attributes, their names' and values' bytes, in whole units. A length is
//! what the server sees change, as every change of one is a request it answers (a DAX write
//! extends no file: fs/fuse/dax.c sends a file-extending write as a FUSE_WRITE); a name and
//! an attribute cost the host what no length shows. JuiceFS's directory quotas count so with
//! 4 KiB for the unit ("regular files are accounted by their file length, aligned to 4 KiB",
//! other files 4 KiB each).
//!
//! One count for each volume, shared by every server serving it (slice 2): the first word of
//! the volume's count file, which each maps shared ([`mapped`]). A change takes its room
//! before it is made, one compare-and-swap of that word ([`Bound::reserve`]), and gives back
//! what it did not use as it ends ([`Taken`]): never a lock, and no two servers' changes
//! together past the bound. The word orders nothing else, so each operation on it is
//! relaxed: one word's modifications are in one order in every process mapping it.

use std::fs::File;
use std::io;
use std::num::NonZeroU64;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a share's files may take, `limit`, in whole units of `unit` bytes, and the count of
/// what they take now, `charge`, which every server of its volume shares: past `limit` where
/// a bound was lowered below it, or a recount found more, until they give back.
#[derive(Debug, Clone, Copy)]
pub struct Bound {
    limit: u64,
    unit: NonZeroU64,
    charge: &'static AtomicU64,
}

impl Bound {
    /// `limit` bytes, counted in `charge`, in units of `unit` bytes: none for a unit of 0,
    /// which no file system allocates in.
    pub fn new(limit: u64, unit: u64, charge: &'static AtomicU64) -> Option<Bound> {
        Some(Bound {
            limit,
            unit: NonZeroU64::new(unit)?,
            charge,
        })
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn unit(&self) -> NonZeroU64 {
        self.unit
    }

    /// What its volume's files take now, by every server's count.
    pub fn charged(&self) -> u64 {
        self.charge.load(Ordering::Relaxed)
    }

    /// What is left below the bound: none past it.
    pub fn room(&self) -> u64 {
        self.limit.saturating_sub(self.charged())
    }

    /// `bytes` in whole units: what a length, or an attributes' bytes, take.
    pub fn whole(&self, bytes: u64) -> u64 {
        whole(bytes, self.unit)
    }

    /// What `from` bytes becoming `to` takes more, in whole units: none where it does not
    /// grow past its last unit.
    pub fn growth(&self, from: u64, to: u64) -> u64 {
        self.whole(to).saturating_sub(self.whole(from))
    }

    /// How many of `n` bytes written at `at` into a file of `len` bytes fit in the room
    /// left now ([`writable`]).
    pub fn writable(&self, len: u64, at: u64, n: u64) -> u64 {
        writable(self.unit, self.room(), len, at, n)
    }

    /// `bytes` taken before the change that needs them is made, where they fit below the
    /// bound, in one compare-and-swap of the count: none where they do not, and nothing
    /// taken. What takes nothing always fits, past the bound too. Given back as the
    /// returned [`Taken`] drops, unless the change was made ([`Taken::keep`]).
    pub fn reserve(&self, bytes: u64) -> Option<Taken> {
        if bytes > 0 {
            self.charge
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                    c.checked_add(bytes).filter(|n| *n <= self.limit)
                })
                .ok()?;
        }
        Some(Taken { bound: *self, bytes })
    }

    /// A write of `n` bytes at `at` into a file of `len` bytes held to the bound in one
    /// compare-and-swap: how many of them fit ([`writable`], with the room the count leaves
    /// as it is taken), and the whole units that takes, taken. None of them where none fit.
    pub fn reserve_write(&self, len: u64, at: u64, n: u64) -> (u64, Taken) {
        let (mut fits, mut takes) = (0, 0);
        let _ = self
            .charge
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                fits = writable(self.unit, self.limit.saturating_sub(c), len, at, n);
                takes = self.growth(len, len.max(at.saturating_add(fits)));
                Some(c.saturating_add(takes))
            });
        (
            fits,
            Taken {
                bound: *self,
                bytes: takes,
            },
        )
    }

    /// `bytes` taken, done already: a change it may not refuse.
    pub fn take(&self, bytes: u64) {
        let _ = self
            .charge
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_add(bytes))
            });
    }

    /// `bytes` given back.
    pub fn give(&self, bytes: u64) {
        let _ = self
            .charge
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_sub(bytes))
            });
    }

    /// `from` bytes become `to`: what that grew taken, what it shrank given back, in whole
    /// units; and the same change of `held`, where it is a removed file's charge held back.
    pub fn resize(&self, from: u64, to: u64, held: Option<&mut u64>) {
        let (grew, shrank) = (self.growth(from, to), self.growth(to, from));
        self.take(grew);
        self.give(shrank);
        if let Some(held) = held {
            *held = held.saturating_add(grew).saturating_sub(shrank);
        }
    }

    /// What the guest's statfs says of a share bounded so, on a host file system of
    /// `free` units free and `files_free` names (none counted where `files_free` is
    /// `None`, as NTFS counts none), in units: its blocks and names the bound's, free and
    /// available what is left of it where the host has as much, a name taking a unit.
    pub fn stats(&self, free: u64, files_free: Option<u64>) -> Stats {
        let unit = self.unit.get();
        let left = self.room() / unit;
        Stats {
            blocks: self.limit / unit,
            free: left.min(free),
            files: self.limit / unit,
            files_free: files_free.map_or(left, |f| left.min(f)),
        }
    }
}

/// `bytes` in whole units of `unit`.
pub fn whole(bytes: u64, unit: NonZeroU64) -> u64 {
    bytes.div_ceil(unit.get()).saturating_mul(unit.get())
}

/// How many of `n` bytes written at `at` into a file of `len` bytes fit in `room`: those
/// before the end the room lets the file reach, its last unit's and the room's whole units;
/// all of them where they grow it by none, as an overwrite within its length.
fn writable(unit: NonZeroU64, room: u64, len: u64, at: u64, n: u64) -> u64 {
    let u = unit.get();
    let most = whole(len, unit).saturating_add((room / u).saturating_mul(u));
    n.min(most.saturating_sub(at))
}

/// Bytes a change took of its bound before it was made ([`Bound::reserve`]): given back as
/// this drops, unless the change was made and they were kept.
#[must_use = "what is taken is given back as it drops"]
#[derive(Debug)]
pub struct Taken {
    bound: Bound,
    bytes: u64,
}

impl Taken {
    /// What was taken.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The change made: what it took stays taken.
    pub fn keep(mut self) {
        self.bytes = 0;
    }

    /// The change made, `bytes` of what it took used: they stay taken, the rest given back.
    pub fn keep_only(mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        self.bound.give(self.bytes);
    }
}

/// A bounded share's statfs numbers ([`Bound::stats`]), in units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub blocks: u64,
    pub free: u64,
    pub files: u64,
    pub files_free: u64,
}

/// The length a volume's count file is counted at: one page, what a shared mapping maps at
/// the least.
fn page() -> io::Result<u64> {
    u64::try_from(crate::platform::page_size()?).map_err(|_| io::Error::other("a page past u64"))
}

/// Whether `file`, a volume's count file, was counted: a page long, as [`store`] leaves it,
/// where it was made empty.
pub fn counted(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.len() >= page()?)
}

/// `charge` written as `file`'s count, the file made a page long where it is shorter: by
/// the holder that holds it alone, as no server maps it.
pub fn store(file: &File, charge: u64) -> io::Result<()> {
    let page = page()?;
    if file.metadata()?.len() < page {
        file.set_len(page)?;
    }
    with_count(file, |count| count.store(charge, Ordering::Relaxed))
}

/// What `file`'s count says now.
pub fn load(file: &File) -> io::Result<u64> {
    with_count(file, |count| count.load(Ordering::Relaxed))
}

/// `file`'s count, opened for reading and writing, mapped shared for as long as this
/// process lives, as a share process's servers are: each change of it every other process
/// mapping it sees. Refused where it was never counted.
pub fn mapped(file: &File) -> io::Result<&'static AtomicU64> {
    let (at, _) = map(file)?;
    // SAFETY: the first 8 bytes of a page-aligned shared mapping a page long, never unmapped,
    // so valid for the process's life; every access to them, in each process that maps
    // them, is atomic.
    Ok(unsafe { &*at.as_ptr().cast::<AtomicU64>() })
}

/// `file`'s count mapped for `f` alone, unmapped as it returns.
fn with_count<T>(file: &File, f: impl FnOnce(&AtomicU64) -> T) -> io::Result<T> {
    let (at, len) = map(file)?;
    // SAFETY: as in `mapped`; the mapping goes only once `f` has returned, and no reference
    // into it outlives `f`.
    let out = f(unsafe { &*at.as_ptr().cast::<AtomicU64>() });
    // SAFETY: the mapping made above, which nothing references any more.
    unsafe { crate::platform::unmap_file_shared(at, len) };
    Ok(out)
}

/// `file`'s first page, mapped shared and writable, where it was counted.
fn map(file: &File) -> io::Result<(NonNull<u8>, usize)> {
    if !counted(file)? {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the volume's count was never written",
        ));
    }
    let len = crate::platform::page_size()?;
    Ok((crate::platform::map_file_shared(file, len)?, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    fn count(at: u64) -> &'static AtomicU64 {
        Box::leak(Box::new(AtomicU64::new(at)))
    }

    fn bound(limit: u64, unit: u64, charged: u64) -> Bound {
        Bound::new(limit, unit, count(charged)).unwrap()
    }

    /// A length takes its whole units; growth is the units it adds, none within its last;
    /// no unit is none.
    #[test]
    fn a_length_takes_its_whole_units() {
        let b = bound(64 * KIB, 4 * KIB, 0);
        assert_eq!(b.whole(0), 0);
        assert_eq!(b.whole(1), 4 * KIB);
        assert_eq!(b.whole(4 * KIB), 4 * KIB);
        assert_eq!(b.whole(4 * KIB + 1), 8 * KIB);
        assert_eq!(b.whole(u64::MAX), u64::MAX, "saturates, never wraps");
        assert_eq!(b.growth(1, 4 * KIB), 0, "within its last unit");
        assert_eq!(b.growth(4 * KIB, 4 * KIB + 1), 4 * KIB);
        assert_eq!(b.growth(9 * KIB, 1), 0, "a shrink grows nothing");
        assert!(Bound::new(1, 0, count(0)).is_none());
    }

    /// Near its bound a write is cut to the end the room lets the file reach; within its
    /// length it is whole; past that end, or with no room, none of it.
    #[test]
    fn a_write_is_cut_where_the_room_ends() {
        // 3 units of 4 KiB left; a file of 5 KiB holds 8 KiB of units.
        let b = bound(20 * KIB, 4 * KIB, 8 * KIB);
        assert_eq!(b.room(), 12 * KIB);
        assert_eq!(b.writable(5 * KIB, 0, 5 * KIB), 5 * KIB, "an overwrite");
        assert_eq!(
            b.writable(5 * KIB, 5 * KIB, 3 * KIB),
            3 * KIB,
            "its last unit's slack"
        );
        assert_eq!(b.writable(5 * KIB, 5 * KIB, 64 * KIB), 15 * KIB, "to 20 KiB");
        assert_eq!(b.writable(5 * KIB, 20 * KIB, 1), 0, "at the end it may reach");
        assert_eq!(b.writable(5 * KIB, 1 << 40, 1), 0, "a hole past it counts");
        let full = bound(20 * KIB, 4 * KIB, 20 * KIB);
        assert_eq!(full.writable(5 * KIB, 7 * KIB, 1), 1, "its last unit still");
        assert_eq!(full.writable(5 * KIB, 8 * KIB, 1), 0);
        let past = bound(20 * KIB, 4 * KIB, 30 * KIB);
        assert_eq!(past.room(), 0);
        assert_eq!(
            past.writable(5 * KIB, 0, 5 * KIB),
            5 * KIB,
            "an overwrite past it"
        );
        assert_eq!(past.writable(5 * KIB, 8 * KIB, 1), 0);
    }

    /// What grows is taken and what shrinks given back, in whole units, and a removed
    /// file's held charge follows its file; giving back stops at none.
    #[test]
    fn a_resize_takes_and_gives_whole_units() {
        let b = bound(64 * KIB, 4 * KIB, 4 * KIB);
        let mut held = 4 * KIB;
        b.resize(1, 9 * KIB, Some(&mut held));
        assert_eq!((b.charged(), held), (12 * KIB, 12 * KIB));
        b.resize(9 * KIB, 0, Some(&mut held));
        assert_eq!((b.charged(), held), (0, 0));
        b.resize(0, 1, None);
        assert_eq!(b.charged(), 4 * KIB);
        b.give(u64::MAX);
        assert_eq!(b.charged(), 0);
        b.take(u64::MAX);
        b.take(1);
        assert_eq!(b.charged(), u64::MAX, "saturates, never wraps");
    }

    /// A reservation takes its room before its change, all of it or none; what it took goes
    /// back as it drops unless kept, and a write's unused units go back once it is made.
    /// What takes nothing is never refused, past the bound too.
    #[test]
    fn what_is_taken_first_goes_back_unless_kept() {
        let b = bound(16 * KIB, 4 * KIB, 4 * KIB);
        assert!(b.reserve(12 * KIB + 1).is_none(), "past the room");
        assert_eq!(b.charged(), 4 * KIB, "a refusal takes nothing");
        let taken = b.reserve(8 * KIB).unwrap();
        assert_eq!((taken.bytes(), b.charged()), (8 * KIB, 12 * KIB));
        drop(taken);
        assert_eq!(b.charged(), 4 * KIB, "the change not made");
        b.reserve(4 * KIB).unwrap().keep();
        assert_eq!(b.charged(), 8 * KIB, "the change made");
        // A write of 9 KiB at 0 into an empty file: 8 KiB of room, 8 KiB of it fit.
        let (fits, taken) = b.reserve_write(0, 0, 9 * KIB);
        assert_eq!((fits, taken.bytes(), b.charged()), (8 * KIB, 8 * KIB, 16 * KIB));
        // Of it, 3 KiB written: one unit kept, the other given back.
        taken.keep_only(4 * KIB);
        assert_eq!(b.charged(), 12 * KIB);
        let past = bound(16 * KIB, 4 * KIB, 20 * KIB);
        assert!(past.reserve(0).is_some(), "nothing taken, past the bound too");
        let (fits, taken) = past.reserve_write(5 * KIB, 0, 5 * KIB);
        assert_eq!((fits, taken.bytes()), (5 * KIB, 0), "an overwrite");
        drop(taken);
        assert_eq!(past.charged(), 20 * KIB);
    }

    /// Servers of one volume share its count: each takes from the one room, so that their
    /// changes together never pass the bound, and each sees what the others gave back.
    #[test]
    fn the_servers_of_a_volume_share_one_count() {
        let shared = count(0);
        let (a, b) = (
            Bound::new(8 * KIB, 4 * KIB, shared).unwrap(),
            Bound::new(8 * KIB, 4 * KIB, shared).unwrap(),
        );
        let first = a.reserve(4 * KIB).unwrap();
        let second = b.reserve(4 * KIB).unwrap();
        assert!(
            a.reserve(1).is_none() && b.reserve(1).is_none(),
            "the room is taken"
        );
        first.keep();
        drop(second);
        assert_eq!((a.charged(), b.charged()), (4 * KIB, 4 * KIB));
        // 4 KiB of room, asked a KiB at a time by many threads at once: four of them taken.
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || (0..64).filter_map(|_| b.reserve(KIB).map(Taken::keep)).count())
            })
            .collect();
        let kept: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(kept, 4, "the room taken once, never past it");
        assert_eq!(a.charged(), 8 * KIB);
    }

    /// The guest's statfs: the bound's blocks and names, what is left of it free where the
    /// host has as much, and the host's room where it has less; a host that counts no
    /// names leaves the bound's.
    #[test]
    fn statfs_says_the_bound_and_its_room() {
        let b = bound(40 * KIB, 4 * KIB, 12 * KIB);
        let s = b.stats(100, Some(100));
        assert_eq!((s.blocks, s.free, s.files, s.files_free), (10, 7, 10, 7));
        let s = b.stats(3, Some(2));
        assert_eq!((s.free, s.files_free), (3, 2), "the host's room is the tighter");
        assert_eq!(b.stats(100, None).files_free, 7);
        let past = bound(40 * KIB, 4 * KIB, 80 * KIB);
        assert_eq!(
            past.stats(100, Some(100)).free,
            0,
            "a share past its bound is full"
        );
    }

    /// A volume's count file: never counted until its first holder writes it, then a page
    /// long; read back and mapped by each server alike, a change through one mapping seen
    /// by every other and by a read.
    #[test]
    fn a_count_file_is_written_once_and_shared_by_its_mappings() {
        let dir = shards_testdir::TempDir::new("vol-count").unwrap();
        let path = dir.path().join(".v.charge");
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let file = open();
        assert!(!counted(&file).unwrap());
        assert_eq!(mapped(&file).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        store(&file, 12345).unwrap();
        assert!(counted(&file).unwrap());
        assert_eq!(load(&open()).unwrap(), 12345);
        let (one, two) = (mapped(&open()).unwrap(), mapped(&open()).unwrap());
        let b = Bound::new(u64::MAX, 4 * KIB, one).unwrap();
        b.take(5);
        assert_eq!(two.load(Ordering::Relaxed), 12350);
        assert_eq!(load(&file).unwrap(), 12350);
    }
}
