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

use std::num::NonZeroU64;

/// What a share's files may take, `limit`, in whole units of `unit` bytes, and what they
/// take now, `charged`: past `limit` where a bound was lowered below it, or a recount found
/// more, until they give back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    limit: u64,
    unit: NonZeroU64,
    charged: u64,
}

impl Bound {
    /// `limit` bytes, none taken yet, in units of `unit` bytes: none for a unit of 0, which
    /// no file system allocates in.
    pub fn new(limit: u64, unit: u64) -> Option<Bound> {
        Some(Bound {
            limit,
            unit: NonZeroU64::new(unit)?,
            charged: 0,
        })
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn unit(&self) -> NonZeroU64 {
        self.unit
    }

    pub fn charged(&self) -> u64 {
        self.charged
    }

    /// What is left below the bound: none past it.
    pub fn room(&self) -> u64 {
        self.limit.saturating_sub(self.charged)
    }

    /// `bytes` in whole units: what a length, or an attributes' bytes, take.
    pub fn whole(&self, bytes: u64) -> u64 {
        bytes.div_ceil(self.unit.get()).saturating_mul(self.unit.get())
    }

    /// What `from` bytes becoming `to` takes more, in whole units: none where it does not
    /// grow past its last unit.
    pub fn growth(&self, from: u64, to: u64) -> u64 {
        self.whole(to).saturating_sub(self.whole(from))
    }

    /// Whether `bytes` more fit below the bound.
    pub fn fits(&self, bytes: u64) -> bool {
        bytes <= self.room()
    }

    /// How many of `n` bytes written at `at` into a file of `len` bytes fit: those before
    /// the end the room lets the file reach, its last unit's and the room's whole units; all
    /// of them where they grow it by none, as an overwrite within its length.
    pub fn writable(&self, len: u64, at: u64, n: u64) -> u64 {
        let unit = self.unit.get();
        let most = self
            .whole(len)
            .saturating_add((self.room() / unit).saturating_mul(unit));
        n.min(most.saturating_sub(at))
    }

    /// `bytes` taken, done already: a change the bound's room was asked for first, or one
    /// it may not refuse.
    pub fn take(&mut self, bytes: u64) {
        self.charged = self.charged.saturating_add(bytes);
    }

    /// `bytes` given back.
    pub fn give(&mut self, bytes: u64) {
        self.charged = self.charged.saturating_sub(bytes);
    }

    /// `from` bytes become `to`: what that grew taken, what it shrank given back, in whole
    /// units; and the same change of `held`, where it is a removed file's charge held back.
    pub fn resize(&mut self, from: u64, to: u64, held: Option<&mut u64>) {
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

/// A bounded share's statfs numbers ([`Bound::stats`]), in units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub blocks: u64,
    pub free: u64,
    pub files: u64,
    pub files_free: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    fn bound(limit: u64, unit: u64, charged: u64) -> Bound {
        let mut b = Bound::new(limit, unit).unwrap();
        b.take(charged);
        b
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
        assert_eq!(Bound::new(1, 0), None);
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
        let mut b = bound(64 * KIB, 4 * KIB, 4 * KIB);
        let mut held = 4 * KIB;
        b.resize(1, 9 * KIB, Some(&mut held));
        assert_eq!((b.charged(), held), (12 * KIB, 12 * KIB));
        b.resize(9 * KIB, 0, Some(&mut held));
        assert_eq!((b.charged(), held), (0, 0));
        b.resize(0, 1, None);
        assert_eq!(b.charged(), 4 * KIB);
        b.give(u64::MAX);
        assert_eq!(b.charged(), 0);
        assert!(b.fits(64 * KIB) && !b.fits(64 * KIB + 1));
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
}
