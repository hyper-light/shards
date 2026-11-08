//! A group's CPU budget, shared by its microVMs' VM processes (D183, slice 3): its
//! `cpu.max`, a quota of CPU time each period, held across every microVM of the group
//! together, as CFS bandwidth control holds a cgroup's threads
//! (Documentation/scheduler/sched-bwc.rst), where the host has no group of processes a
//! user may cap. It lives in a file of the group's the daemon makes, mapped shared by the
//! daemon, which writes the budget, and by each of the group's VM processes, whose
//! governors charge their vCPUs' time to it and hold them by it.
//!
//! What it counts is the group's theoretical arrival time, the generic cell rate
//! algorithm's (ITU-T I.371, ATM Forum TM 4.0): the time, on the host's monotonic clock,
//! by which the CPU time its microVMs have used would have been spent at the budget's
//! rate. Each charge moves it on by what the time charged costs at that rate, from now
//! where it lags; the group is past its budget while it runs more than a period ahead of
//! now, and within it again once now catches up. Over any stretch of time the group uses
//! at most the budget's rate over it and a period's quota more, as a cgroup's periodic
//! quota allows, and over a long one its rate. One word, moved by compare-and-swap: no
//! process waits on another, and nothing of a period's wraps.

use std::fs::File;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::platform;

/// The bytes its file holds and each process maps: a page of every host's (4, 16 and 64
/// KiB), as a mapping is made of pages.
pub const LEN: usize = 1 << 16;

/// A group's CPU budget, mapped ([`GroupCpu::map`]).
#[derive(Debug)]
pub struct GroupCpu {
    map: NonNull<u8>,
    _file: File,
}

// SAFETY: the mapping is this value's alone, read and written through atomics only.
unsafe impl Send for GroupCpu {}
// SAFETY: as above: every access is atomic.
unsafe impl Sync for GroupCpu {}

impl GroupCpu {
    /// The budget kept in `file`, a group's, opened for reading and writing, [`LEN`] bytes
    /// long (the daemon makes it so).
    pub fn map(file: File) -> io::Result<GroupCpu> {
        let len = file.metadata()?.len();
        if len < LEN as u64 {
            return Err(io::Error::other(format!(
                "a group's CPU budget of {len} bytes, short of its {LEN}"
            )));
        }
        let map = platform::map_file_shared(&file, LEN)?;
        Ok(GroupCpu { map, _file: file })
    }

    /// Word `i` of its two: the budget, then the arrival time.
    fn word(&self, i: usize) -> &AtomicU64 {
        debug_assert!(i < 2);
        // SAFETY: the two words begin a mapping of LEN bytes this value holds, aligned as
        // a page is; shared memory every process reaches through atomics alone.
        unsafe { &*self.map.as_ptr().cast::<AtomicU64>().add(i) }
    }

    /// The daemon's: `quota` µs of CPU time each `period` µs from now, or none.
    pub fn set_budget(&self, budget: Option<(u32, u32)>) {
        let word = budget.map_or(0, |(quota, period)| (u64::from(quota) << 32) | u64::from(period));
        self.word(0).store(word, Ordering::Release);
    }

    /// The budget the daemon wrote: `quota` µs of CPU time each `period` µs, or none.
    pub fn budget(&self) -> Option<(u32, u32)> {
        let word = self.word(0).load(Ordering::Acquire);
        let (quota, period) = ((word >> 32) as u32, word as u32);
        (quota > 0 && period > 0).then_some((quota, period))
    }

    /// Charges `used` ns of CPU time at `now` (ns of [`platform::monotonic_ns`]) to a
    /// budget of `quota` µs each `period` µs: what it costs at that rate moves the group's
    /// arrival time on, from `now` where it lags. Returns how far ahead of `now` the group
    /// runs: past `period` µs, it is past its budget.
    pub fn charge(&self, now: u64, used: u64, quota: u32, period: u32) -> u64 {
        let cost = u128::from(used).saturating_mul(u128::from(period)) / u128::from(quota.max(1));
        let cost = u64::try_from(cost).unwrap_or(u64::MAX);
        let word = self.word(1);
        let mut seen = word.load(Ordering::Acquire);
        loop {
            let next = seen.max(now).saturating_add(cost);
            if next == seen {
                return next.saturating_sub(now);
            }
            match word.compare_exchange_weak(seen, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return next.saturating_sub(now),
                Err(now_seen) => seen = now_seen,
            }
        }
    }
}

impl Drop for GroupCpu {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, which nothing refers to past this value.
        unsafe { platform::unmap_file_shared(self.map, LEN) };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn budget_file(dir: &std::path::Path) -> File {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("cpu"))
            .unwrap();
        file.set_len(LEN as u64).unwrap();
        file
    }

    /// The budget written is the budget read, through two mappings of one file, as the
    /// daemon's and a VM process's are; none is none.
    #[test]
    fn a_budget_written_is_read_through_another_mapping() {
        let dir = shards_testdir::TempDir::new("group-cpu").unwrap();
        let file = budget_file(&dir);
        let (daemon, member) = (
            GroupCpu::map(file.try_clone().unwrap()).unwrap(),
            GroupCpu::map(file).unwrap(),
        );
        assert_eq!(member.budget(), None);
        daemon.set_budget(Some((50_000, 100_000)));
        assert_eq!(member.budget(), Some((50_000, 100_000)));
        daemon.set_budget(None);
        assert_eq!(member.budget(), None);
    }

    /// A file shorter than the budget's page is refused, not mapped past its end.
    #[test]
    fn a_short_file_is_refused() {
        let dir = shards_testdir::TempDir::new("group-cpu-short").unwrap();
        let file = budget_file(&dir);
        file.set_len(8).unwrap();
        assert!(GroupCpu::map(file).is_err());
    }

    /// Charges move the arrival time on by what they cost at the budget's rate, from now
    /// where it lags: half a CPU's budget charged a period's CPU time runs two periods
    /// ahead; members' charges add up, as one budget's; and once now has caught up, it
    /// lags no more than now, its idle time no credit past a period's.
    #[test]
    fn charges_move_the_arrival_time_on_at_the_budgets_rate() {
        let dir = shards_testdir::TempDir::new("group-cpu-charge").unwrap();
        let file = budget_file(&dir);
        let (a, b) = (
            GroupCpu::map(file.try_clone().unwrap()).unwrap(),
            GroupCpu::map(file).unwrap(),
        );
        let (quota, period) = (50_000, 100_000);
        let now = 1_000_000_000;
        // 100 ms of CPU at half a CPU's rate costs 200 ms.
        assert_eq!(a.charge(now, 100_000_000, quota, period), 200_000_000);
        // Another member's 50 ms more: 100 ms more.
        assert_eq!(b.charge(now, 50_000_000, quota, period), 300_000_000);
        // Nothing charged: as far ahead as it was, less the time gone.
        assert_eq!(a.charge(now + 100_000_000, 0, quota, period), 200_000_000);
        // Long after: it lags, and a charge counts from now.
        let later = now + 10_000_000_000;
        assert_eq!(b.charge(later, 10_000_000, quota, period), 20_000_000);
    }

    /// Threads charging one budget at once lose nothing: their charges add up to what
    /// they charged.
    #[test]
    fn charges_at_once_add_up() {
        let dir = shards_testdir::TempDir::new("group-cpu-racing").unwrap();
        let file = budget_file(&dir);
        let shared = GroupCpu::map(file).unwrap();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..10_000 {
                        shared.charge(0, 1_000, 100_000, 100_000);
                    }
                });
            }
        });
        // 40,000 charges of 1 µs at a whole CPU's rate: 40 ms.
        assert_eq!(shared.charge(0, 0, 100_000, 100_000), 40_000_000);
    }
}
