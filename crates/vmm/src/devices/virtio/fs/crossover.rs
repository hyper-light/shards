//! Which of a share's files its guest maps through its DAX window, learned where it runs
//! (D189, memory-09). It was any file of 1 MiB or more, a size measured on two hosts (PM
//! M338) and held on every host. The device sees both ways a file's bytes reach its guest:
//! a READ, its server's answer timed from ask to reply, and a SETUPMAPPING, its own, timed
//! from its descriptor's ask to its window's mapping. Each is a cost a byte of the file it
//! served, kept by the file's size class (the power of two at or below its size) as the
//! cheapest and the dearest seen, the spread its uncertainty. A class whose window's dearest
//! costs no more a byte than FUSE's cheapest has its files mapped; one where they overlap,
//! or FUSE is cheaper, has them read. The least class at and past which every class known is
//! mapped is the crossover, and a class not yet known past it is mapped too.
//!
//! - **Cold start:** a class with fewer than two costs of either way (two, the fewest that
//!   show a spread) is calibrating: its files are told each way in turn as the guest first
//!   learns of them, so that both ways' costs come, each file keeping its way while it does.
//! - **Failure:** a class in which a mapping failed is read through FUSE, which works
//!   everywhere, its errno kept.
//! - **What it cannot see:** the guest's faults on a mapped page and its copies into its
//!   own page cache of what it read are its vCPUs', which the device never sees; nor what a
//!   file read twice costs again. Those are measured by D189's harness (owed, D189).
//! - **What it never changes:** the server says which files may be mapped at all (a regular
//!   file it may read, and write where the window needs it, `Server::dax_of`); this only
//!   takes that away, never gives it, so no cost moves a permission. A file the guest maps
//!   already keeps its mapping until its kernel lets the inode go (fs/fuse/dax.c,
//!   `fuse_dax_dontcache`).

use std::collections::HashMap;
use std::time::Duration;

/// A file's size class: the power of two at or below its size; none for an empty file,
/// which has nothing to map.
fn class(size: u64) -> Option<u32> {
    size.checked_ilog2()
}

/// A cost seen: how long serving took, for how many of a file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cost {
    nanos: u128,
    bytes: u64,
}

impl Cost {
    /// Whether it costs no more a byte than `other` does.
    fn at_most(self, other: Cost) -> bool {
        self.nanos.saturating_mul(u128::from(other.bytes))
            <= other.nanos.saturating_mul(u128::from(self.bytes))
    }
}

/// A way's costs a byte in a class: how many, the cheapest and the dearest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Spread {
    seen: u64,
    least: Option<Cost>,
    most: Option<Cost>,
}

impl Spread {
    fn add(&mut self, c: Cost) {
        self.seen = self.seen.saturating_add(1);
        if self.least.is_none_or(|l| !l.at_most(c)) {
            self.least = Some(c);
        }
        if self.most.is_none_or(|m| m.at_most(c)) {
            self.most = Some(c);
        }
    }

    /// Its cheapest and dearest, where it has two costs at least.
    fn known(&self) -> Option<(Cost, Cost)> {
        (self.seen >= 2).then_some((self.least?, self.most?))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Class {
    fuse: Spread,
    dax: Spread,
    failed: Option<i32>,
    /// Files told each way while it calibrates.
    told_dax: u64,
    told_fuse: u64,
}

/// What a size class's files are read through, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Too few costs of one way or the other: its files are told each way in turn.
    Calibrating,
    /// Its window's dearest cost a byte is no more than FUSE's cheapest, or it is past the
    /// crossover, unknown: mapped.
    Dax,
    /// FUSE's costs reach below the window's: read.
    Fuse,
    /// A mapping of one of its files failed, with this errno: read.
    Failed(i32),
}

/// What a share's device has learned of its two ways ([module](self)).
#[derive(Debug)]
pub struct Crossover {
    classes: [Class; 64],
    /// Each file the guest was told of: its class and, while its class calibrates, whether
    /// it was told to map it. Let go of as the guest forgets it, or as its session ends.
    files: HashMap<u64, (u32, Option<bool>)>,
}

impl Default for Crossover {
    fn default() -> Self {
        Crossover {
            classes: [Class::default(); 64],
            files: HashMap::new(),
        }
    }
}

impl Crossover {
    /// The least class at and past which every class known is mapped: none where no class
    /// is known, or the largest known is read.
    fn crossover(&self) -> Option<u32> {
        let mut from = None;
        for (c, class) in (0u32..).zip(self.classes.iter()) {
            match own(class) {
                Some(Verdict::Dax) => {
                    from.get_or_insert(c);
                }
                Some(_) => from = None,
                None => {}
            }
        }
        from
    }

    /// The verdict on class `c`.
    pub fn verdict(&self, c: u32) -> Verdict {
        let Some(class) = self.classes.get(c as usize) else {
            return Verdict::Fuse;
        };
        own(class).unwrap_or_else(|| match self.crossover() {
            Some(from) if c >= from => Verdict::Dax,
            _ => Verdict::Calibrating,
        })
    }

    /// The guest is told of node `node`, a regular file of `size` bytes, which its server
    /// lets it map where `may`: whether it is told to map it.
    pub fn told(&mut self, node: u64, size: u64, may: bool) -> bool {
        let Some(c) = class(size) else {
            self.files.remove(&node);
            return false;
        };
        let verdict = self.verdict(c);
        let kept = self
            .files
            .get(&node)
            .filter(|(was, _)| *was == c)
            .and_then(|&(_, way)| way);
        let way = match verdict {
            Verdict::Dax => Some(true),
            Verdict::Fuse | Verdict::Failed(_) => Some(false),
            Verdict::Calibrating if !may => None,
            Verdict::Calibrating => Some(kept.unwrap_or_else(|| self.next_way(c))),
        };
        let calibrating = (verdict == Verdict::Calibrating).then_some(way).flatten();
        self.files.insert(node, (c, calibrating));
        may && way == Some(true)
    }

    /// The way the next file of calibrating class `c` is told: the way told fewer files so
    /// far, the window first.
    fn next_way(&mut self, c: u32) -> bool {
        let Some(class) = self.classes.get_mut(c as usize) else {
            return false;
        };
        let dax = class.told_dax <= class.told_fuse;
        match dax {
            true => class.told_dax = class.told_dax.saturating_add(1),
            false => class.told_fuse = class.told_fuse.saturating_add(1),
        }
        dax
    }

    fn class_of(&mut self, node: u64) -> Option<&mut Class> {
        let &(c, _) = self.files.get(&node)?;
        self.classes.get_mut(c as usize)
    }

    /// A READ of node `node` served `bytes` of it, its server answering `took` after it was
    /// asked.
    pub fn read(&mut self, node: u64, bytes: u64, took: Duration) {
        if bytes == 0 {
            return;
        }
        if let Some(class) = self.class_of(node) {
            class.fuse.add(Cost {
                nanos: took.as_nanos(),
                bytes,
            });
        }
    }

    /// A SETUPMAPPING of node `node` mapped `bytes` of it, `took` from its descriptor's ask
    /// to its mapping; or failed with an errno.
    pub fn mapped(&mut self, node: u64, done: Result<u64, i32>, took: Duration) {
        let Some(class) = self.class_of(node) else {
            return;
        };
        match done {
            Ok(0) => {}
            Ok(bytes) => class.dax.add(Cost {
                nanos: took.as_nanos(),
                bytes,
            }),
            Err(errno) => class.failed = Some(errno),
        }
    }

    /// The guest let go of node `node`.
    pub fn forget(&mut self, node: u64) {
        self.files.remove(&node);
    }

    /// Its guest's session with the share ended, or a new one began (mod.rs `turns_session`,
    /// D197): every node of the session is gone with it, none forgotten one by one, as the
    /// guest sends no FORGET once its session ends (linux v6.18 fs/fuse/virtio_fs.c
    /// `virtio_fs_conn_destroy` stops the forget queue before the share's inodes are evicted,
    /// and `send_forget_request` frees each FORGET then unsent). What it learned of each size
    /// class stays: those are its host's costs, not the session's.
    pub fn session_turned(&mut self) {
        self.files.clear();
    }

    /// How many files it knows of: what a session's end lets go of, for its tests.
    #[cfg(test)]
    pub fn files_known(&self) -> usize {
        self.files.len()
    }
}

/// A class's own verdict, where it is known: failed, or both ways' spreads.
fn own(class: &Class) -> Option<Verdict> {
    if let Some(errno) = class.failed {
        return Some(Verdict::Failed(errno));
    }
    let ((_, dax_most), (fuse_least, _)) = (class.dax.known()?, class.fuse.known()?);
    Some(match dax_most.at_most(fuse_least) {
        true => Verdict::Dax,
        false => Verdict::Fuse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: Duration = Duration::from_micros(1);
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;

    /// Feeds class `size`'s file `node` FUSE reads of `fuse` µs and mappings of `dax` µs, each
    /// of the whole file, as calibration tells it each way.
    fn learn(x: &mut Crossover, node: u64, size: u64, fuse: [u64; 2], dax: [u64; 2]) {
        x.told(node, size, true);
        for us in fuse {
            x.read(node, size, US * u32::try_from(us).unwrap());
        }
        for us in dax {
            x.mapped(node, Ok(size), US * u32::try_from(us).unwrap());
        }
    }

    /// The cold start maps no class whole: its files are told each way in turn, each file
    /// keeping its way, so that both ways' costs come; never a size fixed beforehand.
    #[test]
    fn a_cold_share_tells_each_class_both_ways_in_turn() {
        let mut x = Crossover::default();
        assert_eq!(x.verdict(class(512 * KIB).unwrap()), Verdict::Calibrating);
        let ways: Vec<bool> = (1..=4).map(|n| x.told(n, 512 * KIB, true)).collect();
        assert_eq!(ways, [true, false, true, false]);
        // Told again, each keeps its way.
        assert!(x.told(1, 512 * KIB, true));
        assert!(!x.told(2, 512 * KIB, true));
        // One its server may not map is never mapped, nor counted.
        assert!(!x.told(5, 512 * KIB, false));
        assert!(x.told(6, 512 * KIB, true));
        // An empty file has nothing to map.
        assert!(!x.told(7, 0, true));
    }

    /// Where it runs decides: a class whose window costs no more a byte at its dearest than
    /// FUSE at its cheapest is mapped, and every class past it; one where the two overlap,
    /// or FUSE is cheaper, is read. A host whose crossover is 256 KiB maps a 512 KiB file,
    /// which a fixed 1 MiB never did (memory-09); one whose window costs more reads it.
    #[test]
    fn the_crossover_is_where_the_windows_costs_fall_under_fuses() {
        let mut fast = Crossover::default();
        learn(&mut fast, 1, 64 * KIB, [30, 35], [60, 90]);
        learn(&mut fast, 2, 256 * KIB, [120, 130], [80, 110]);
        assert_eq!(fast.verdict(class(64 * KIB).unwrap()), Verdict::Fuse);
        assert_eq!(fast.verdict(class(256 * KIB).unwrap()), Verdict::Dax);
        assert_eq!(fast.verdict(class(512 * KIB).unwrap()), Verdict::Dax);
        assert!(fast.told(10, 512 * KIB, true));
        assert!(!fast.told(11, 64 * KIB, true));

        let mut slow = Crossover::default();
        learn(&mut slow, 1, 512 * KIB, [200, 260], [250, 300]);
        assert_eq!(slow.verdict(class(512 * KIB).unwrap()), Verdict::Fuse);
        assert!(!slow.told(10, 512 * KIB, true));
        // Spreads that overlap are no evidence the window is cheaper.
        let mut overlap = Crossover::default();
        learn(&mut overlap, 1, MIB, [400, 600], [300, 500]);
        assert_eq!(overlap.verdict(class(MIB).unwrap()), Verdict::Fuse);
    }

    /// A known class read past a mapped one moves the crossover past it: classes past the
    /// largest read one are calibrated, not assumed.
    #[test]
    fn a_class_read_past_one_mapped_moves_the_crossover() {
        let mut x = Crossover::default();
        learn(&mut x, 1, 256 * KIB, [120, 130], [80, 110]);
        learn(&mut x, 2, MIB, [400, 410], [500, 600]);
        assert_eq!(x.crossover(), None);
        assert_eq!(x.verdict(class(4 * MIB).unwrap()), Verdict::Calibrating);
        learn(&mut x, 3, 2 * MIB, [800, 900], [300, 400]);
        assert_eq!(x.crossover(), Some(class(2 * MIB).unwrap()));
        assert_eq!(x.verdict(class(4 * MIB).unwrap()), Verdict::Dax);
    }

    /// A class whose mapping failed is read through FUSE, its errno kept; a READ of a file
    /// the guest was never told of, or one it forgot, teaches nothing.
    #[test]
    fn a_failed_mapping_leaves_its_class_to_fuse() {
        let mut x = Crossover::default();
        x.told(1, MIB, true);
        x.mapped(1, Err(12), US);
        assert_eq!(x.verdict(class(MIB).unwrap()), Verdict::Failed(12));
        assert!(!x.told(2, MIB, true));
        x.read(9, MIB, US);
        x.forget(1);
        x.read(1, MIB, US);
        let c = x.classes.get(class(MIB).unwrap() as usize).unwrap();
        assert_eq!(c.fuse.seen, 0);
    }
}
