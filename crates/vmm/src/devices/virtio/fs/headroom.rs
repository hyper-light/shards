//! What the budgets over a share process's cgroups leave its page cache (D188): of its
//! cgroup and each above it with a `memory.high` or `memory.max` short of `max`, the least
//! left below it. A share's copy of what its guest reads is a second one, its guest keeping
//! the read in its own page cache, and under a budget it is charged to the run. On an EPYC
//! 9V74 a lone microVM's slice, a mebibyte short of what it held, filled with the share's
//! copy as its guest read; its host took back its guest's RAM, its template's mapped pages,
//! which its daemon then counted as room and gave the guest, whose page cache filled the
//! budget as anonymous memory: 201,127 refaults a read at the median, 1.9 times its time with
//! none. Kept until the budgets had no room left, the copy held the slice within half a
//! mebibyte of its budget, its template's pages taken in 2 of 5 reads (PM M318). So the share
//! keeps its copy of what it reads only while the budgets leave room past it for its own
//! reads ([`Headroom::fits`], [`reads_room`]), for its host's next reader, and drops it
//! otherwise.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

/// A look at the budgets' room waits this many times its own time after the last: the
/// share spends at most a hundredth of its time looking.
const LOOKS_APART: u32 = 100;

/// The budgets over a process's cgroups ([`fits`](Headroom::fits)).
#[derive(Debug)]
pub struct Headroom {
    /// Each cgroup's `memory.high`, `memory.max` and `memory.current`, from the process's
    /// own up.
    groups: Vec<[File; 3]>,
    /// What the budgets left at the last look, less what has been read since and kept.
    left: u64,
    /// When it last looked, and how long the look took.
    looked: Option<(Instant, Duration)>,
    /// What its reads need below the budgets past what it keeps ([`reads_room`]).
    margin: u64,
}

impl Headroom {
    /// The calling process's: its cgroup of the v2 hierarchy (/proc/self/cgroup) and each
    /// above it. None where there are no cgroups.
    pub fn of_this_process() -> Result<Headroom, String> {
        #[cfg(target_os = "linux")]
        {
            let own = std::fs::read_to_string("/proc/self/cgroup")
                .map_err(|e| format!("/proc/self/cgroup: {e}"))?;
            let path = own
                .lines()
                .find_map(|l| l.strip_prefix("0::"))
                .ok_or("/proc/self/cgroup: no cgroup of the v2 hierarchy")?;
            Ok(Headroom::under(
                Path::new("/sys/fs/cgroup"),
                Path::new(path.trim_start_matches('/')),
            ))
        }
        #[cfg(not(target_os = "linux"))]
        Err("this host has no cgroups".into())
    }

    /// Of the cgroup `own` below the hierarchy's `root`, and each above it below `root`:
    /// those with a budget's files alone (the root has none).
    pub fn under(root: &Path, own: &Path) -> Headroom {
        let mut groups = Vec::new();
        let mut at = root.join(own);
        while at.starts_with(root) && at != root {
            let open = |name: &str| File::open(at.join(name));
            if let (Ok(high), Ok(max), Ok(current)) =
                (open("memory.high"), open("memory.max"), open("memory.current"))
            {
                groups.push([high, max, current]);
            }
            if !at.pop() {
                break;
            }
        }
        Headroom {
            groups,
            left: 0,
            looked: None,
            margin: reads_room(),
        }
    }

    /// Whether `n` more bytes of page cache fit below every budget over the process: as of
    /// its last look, less what it has kept since, and looked at again as soon as a look's
    /// hundredfold time has passed, so that one set or met later counts within a few looks'
    /// time. Kept bytes come off what is left; bytes that do not fit, which its caller drops,
    /// do not.
    pub fn fits(&mut self, n: u64) -> bool {
        let now = Instant::now();
        let due = self
            .looked
            .is_none_or(|(at, took)| now.saturating_duration_since(at) >= took.saturating_mul(LOOKS_APART));
        if due {
            self.left = self.room();
            self.looked = Some((now, now.elapsed()));
        }
        if self.left < n.saturating_add(self.margin) {
            return false;
        }
        self.left -= n;
        true
    }

    /// The least, over its cgroups with a budget, of what is left below it: none where none
    /// has one, or its files cannot be read.
    fn room(&self) -> u64 {
        self.groups
            .iter()
            .map(|[high, max, current]| value(high).min(value(max)).saturating_sub(value(current)))
            .min()
            .unwrap_or(u64::MAX)
    }
}

/// What a share's reads need below its budgets past what it keeps, in bytes, as init's
/// least room has them for its guest's run (D188): what its host's kernel charges ahead of
/// the pages it takes, a batch of 64 pages on each CPU (`MEMCG_CHARGE_BATCH`, mm/memcontrol.c
/// `refill_stock`), the read in flight, as large as a share's largest request
/// ([`shards_abi::SHARE_MAX_PAGES`]), and the two windows of 128 KiB its file is read ahead
/// in (`VM_READAHEAD_PAGES`, mm/readahead.c); this host's, by its own CPUs and pages.
fn reads_room() -> u64 {
    // SAFETY: sysconf(3) reads values of the running kernel's.
    let (cpus, page) = unsafe {
        (
            libc::sysconf(libc::_SC_NPROCESSORS_ONLN),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    reads_room_of(
        u64::try_from(cpus).map_or(1, |c| c.max(1)),
        u64::try_from(page).unwrap_or(4096),
    )
}

/// [`reads_room`] of a host with `cpus` CPUs online and pages of `page` bytes.
fn reads_room_of(cpus: u64, page: u64) -> u64 {
    const CHARGE_BATCH: u64 = 64;
    const READ_AHEAD: u64 = 128 << 10;
    CHARGE_BATCH
        .saturating_mul(cpus)
        .saturating_add(u64::from(shards_abi::SHARE_MAX_PAGES))
        .saturating_mul(page)
        .saturating_add(2 * READ_AHEAD)
}

/// A cgroup file's one number: `max`, or one that cannot be read, as no bound.
fn value(file: &File) -> u64 {
    use std::os::unix::fs::FileExt as _;
    let mut buf = [0u8; 32];
    let Ok(n) = file.read_at(&mut buf, 0) else {
        return u64::MAX;
    };
    std::str::from_utf8(buf.get(..n).unwrap_or_default())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup tree as a directory holds it: the slice with a budget, the scope below it
    /// with none.
    fn tree(high: &str, current: u64) -> shards_testdir::TempDir {
        let root = shards_testdir::TempDir::new("headroom").unwrap();
        let slice = root.join("a.slice");
        let scope = slice.join("b.scope");
        std::fs::create_dir_all(&scope).unwrap();
        for (dir, high, max, current) in [(&slice, high, "max", current), (&scope, "max", "max", current)] {
            std::fs::write(dir.join("memory.high"), format!("{high}\n")).unwrap();
            std::fs::write(dir.join("memory.max"), format!("{max}\n")).unwrap();
            std::fs::write(dir.join("memory.current"), format!("{current}\n")).unwrap();
        }
        root
    }

    /// What fits is what the tightest budget above leaves past the room the share's reads
    /// need, less what was kept since the last look; none is bounded where no cgroup has a
    /// budget.
    #[test]
    fn what_fits_is_what_the_tightest_budget_leaves_past_its_reads_room() {
        const MIB: u64 = 1 << 20;
        let need = reads_room();
        let high = 100 * MIB;
        let root = tree(&high.to_string(), high - need);
        let mut h = Headroom::under(&root, Path::new("a.slice/b.scope"));
        assert!(
            h.fits(0),
            "a budget leaving its reads' room leaves nothing past it"
        );
        assert!(!h.fits(1), "nothing fits past its reads' room");
        // What is kept is charged as the kernel charges it, so that a look between the
        // reads, however soon, finds what the kept bytes left.
        let root = tree(&high.to_string(), high - need - MIB);
        let charge = |current: u64| {
            for dir in ["a.slice", "a.slice/b.scope"] {
                std::fs::write(root.join(dir).join("memory.current"), format!("{current}\n")).unwrap();
            }
        };
        let mut h = Headroom::under(&root, Path::new("a.slice/b.scope"));
        assert!(h.fits(MIB / 2));
        charge(high - need - MIB / 2);
        assert!(h.fits(MIB / 2), "what was kept comes off what is left");
        charge(high - need);
        assert!(!h.fits(1), "a MiB left past its reads' room, a MiB kept");
        let root = tree("max", 99 * MIB);
        let mut h = Headroom::under(&root, Path::new("a.slice/b.scope"));
        assert!(h.fits(u64::MAX / 2), "no budget bounds nothing");
    }

    /// The room a share's reads need covers what its host's kernel charges ahead on each
    /// CPU, the read in flight and two read-ahead windows: on 4 CPUs of 4 KiB pages, 2.25
    /// MiB, and on 18, 5.75 MiB, as init's least room is for a guest of as many (PM M309);
    /// this host's, by its own CPUs.
    #[test]
    fn a_share_keeps_the_room_its_reads_need() {
        const MIB: u64 = 1 << 20;
        assert_eq!(reads_room_of(4, 4096), 9 * MIB / 4);
        assert_eq!(reads_room_of(18, 4096), 23 * MIB / 4);
        assert!(reads_room_of(1, 4096) >= u64::from(shards_abi::SHARE_MAX_PAGES) * 4096);
        // SAFETY: sysconf(3) reads values of the running kernel's.
        let (cpus, page) = unsafe {
            (
                libc::sysconf(libc::_SC_NPROCESSORS_ONLN),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        assert_eq!(
            reads_room(),
            reads_room_of(u64::try_from(cpus).unwrap().max(1), u64::try_from(page).unwrap())
        );
    }

    /// A budget set after the first look counts once a look's hundredfold time has passed.
    #[test]
    fn a_budget_set_later_counts_at_a_later_look() {
        let root = tree("max", 99 << 20);
        let mut h = Headroom::under(&root, Path::new("a.slice/b.scope"));
        assert!(h.fits(1 << 30));
        std::fs::write(root.join("a.slice/memory.high"), "104857600\n").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while h.fits(2 << 20) {
            assert!(Instant::now() < until, "a budget set later never counted");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
