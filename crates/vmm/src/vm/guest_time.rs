//! Each vCPU's guest time and its machine's: the host CPU time each vCPU has spent running
//! its guest, as its hypervisor counts it (each backend's `GuestClock`), beside which a VM
//! process's own CPU time is its process's less this machine's. vCPUs are kept by index, as
//! a machine's may come and go while it runs: one that goes leaves its time in its
//! machine's, which never falls. The sources, their resolution and costs:
//! docs/research/measurements/guest-cpu-time-v1/README.md.

pub use shards_abi::guest_time::{GuestTimes, Unread, VcpuTime};

#[cfg(any(hv, test))]
pub(super) use clocks::{GuestClocks, Read};

#[cfg(any(hv, test))]
mod clocks {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::{GuestTimes, Unread, VcpuTime};
    use crate::sync::lock;

    /// What reads one vCPU's guest time, in nanoseconds, from any thread: its hypervisor's
    /// clock, or why it did not, typed.
    pub(in crate::vm) type Read = Box<dyn Fn() -> Result<u64, Unread> + Send + Sync>;

    /// A machine's vCPUs' guest-time clocks by index, and what those gone ran. A sample reads
    /// each under its lock, which only vCPUs joining and leaving, out of their guests, take:
    /// so every vCPU read is there, its thread alive, until the sample is done.
    #[derive(Default)]
    pub(in crate::vm) struct GuestClocks(Mutex<Clocks>);

    impl std::fmt::Debug for GuestClocks {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GuestClocks").finish_non_exhaustive()
        }
    }

    #[derive(Default)]
    struct Clocks {
        vcpus: BTreeMap<u32, Clock>,
        departed: u64,
        /// Joins so far, which tell a vCPU from a later one of its index.
        joins: u64,
    }

    struct Clock {
        joined: u64,
        /// Its hypervisor's clock.
        read: Read,
        /// The most it has read, which a sample never says less than.
        last: u64,
    }

    impl Clock {
        fn sample(&mut self) -> (u64, Option<Unread>) {
            match (self.read)() {
                Ok(ns) => {
                    self.last = self.last.max(ns);
                    (self.last, None)
                }
                Err(why) => (self.last, Some(why)),
            }
        }
    }

    impl GuestClocks {
        /// vCPU `index` joins its machine, its guest time read by `read`, until the [`Joined`]
        /// returned goes. One of its index still there is taken as gone, what it read kept as
        /// its machine's.
        pub(in crate::vm) fn join(&self, index: u32, read: Read) -> Joined<'_> {
            let mut clocks = lock(&self.0);
            clocks.joins = clocks.joins.wrapping_add(1);
            let joined = clocks.joins;
            let clock = Clock {
                joined,
                read,
                last: 0,
            };
            if let Some(stale) = clocks.vcpus.insert(index, clock) {
                clocks.departed = clocks.departed.saturating_add(stale.last);
            }
            Joined {
                clocks: self,
                index,
                joined,
            }
        }

        /// Every vCPU's guest time, each read now, and what those gone ran.
        pub(in crate::vm) fn sample(&self) -> GuestTimes {
            let mut clocks = lock(&self.0);
            let departed_ns = clocks.departed;
            let vcpus = clocks
                .vcpus
                .iter_mut()
                .map(|(&index, clock)| {
                    let (guest_ns, unread) = clock.sample();
                    VcpuTime {
                        index,
                        guest_ns,
                        unread,
                    }
                })
                .collect();
            GuestTimes { vcpus, departed_ns }
        }

        fn leave(&self, index: u32, joined: u64) {
            let mut clocks = lock(&self.0);
            if clocks.vcpus.get(&index).is_none_or(|c| c.joined != joined) {
                return;
            }
            if let Some(mut clock) = clocks.vcpus.remove(&index) {
                let (ns, _) = clock.sample();
                clocks.departed = clocks.departed.saturating_add(ns);
            }
        }
    }

    /// A vCPU's place among its machine's ([`GuestClocks::join`]): as this goes, on the
    /// vCPU's own thread, its guest time is read a last time and kept as its machine's.
    pub(in crate::vm) struct Joined<'a> {
        clocks: &'a GuestClocks,
        index: u32,
        joined: u64,
    }

    impl std::fmt::Debug for Joined<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Joined")
                .field("index", &self.index)
                .finish_non_exhaustive()
        }
    }

    impl Drop for Joined<'_> {
        fn drop(&mut self) {
            self.clocks.leave(self.index, self.joined);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A clock that reads what `at` holds.
    fn clock(at: &Arc<AtomicU64>) -> Read {
        let at = at.clone();
        Box::new(move || Ok(at.load(Ordering::SeqCst)))
    }

    fn times(t: &GuestTimes) -> Vec<(u32, u64)> {
        t.vcpus.iter().map(|v| (v.index, v.guest_ns)).collect()
    }

    /// Several vCPUs, each read by its index, and the machine's their sum.
    #[test]
    fn several_vcpus_are_each_read_and_summed() {
        let clocks = GuestClocks::default();
        let at: Vec<_> = [5u64, 70, 900].map(|ns| Arc::new(AtomicU64::new(ns))).into();
        // Joined out of index order: a sample is in it.
        let _two = clocks.join(2, clock(&at[2]));
        let _zero = clocks.join(0, clock(&at[0]));
        let _one = clocks.join(1, clock(&at[1]));
        let t = clocks.sample();
        assert_eq!(times(&t), [(0, 5), (1, 70), (2, 900)]);
        assert_eq!((t.departed_ns, t.total_ns()), (0, 975));
        at[1].store(100, Ordering::SeqCst);
        assert_eq!(clocks.sample().total_ns(), 1005, "each read anew");
        assert!(t.vcpus.iter().all(|v| v.unread.is_none()));
    }

    /// A vCPU that leaves is read a last time as it goes, and its time stays the machine's:
    /// the total never falls, and the vCPUs left are listed alone.
    #[test]
    fn a_vcpu_that_leaves_keeps_its_time_in_the_machines() {
        let clocks = GuestClocks::default();
        let at: Vec<_> = [10u64, 20, 30].map(|ns| Arc::new(AtomicU64::new(ns))).into();
        let zero = clocks.join(0, clock(&at[0]));
        let one = clocks.join(1, clock(&at[1]));
        let _two = clocks.join(2, clock(&at[2]));
        assert_eq!(clocks.sample().total_ns(), 60);
        // It ran more after the last sample: what it read as it left counts.
        at[1].store(25, Ordering::SeqCst);
        drop(one);
        let t = clocks.sample();
        assert_eq!(times(&t), [(0, 10), (2, 30)]);
        assert_eq!((t.departed_ns, t.total_ns()), (25, 65));
        drop(zero);
        let t = clocks.sample();
        assert_eq!(times(&t), [(2, 30)]);
        assert_eq!((t.departed_ns, t.total_ns()), (35, 65));
    }

    /// Every vCPU gone, as a machine is taken apart: its total all theirs.
    #[test]
    fn a_machine_with_no_vcpus_left_keeps_their_total() {
        let clocks = GuestClocks::default();
        let at = Arc::new(AtomicU64::new(42));
        drop(clocks.join(7, clock(&at)));
        let t = clocks.sample();
        assert!(t.vcpus.is_empty());
        assert_eq!((t.departed_ns, t.total_ns()), (42, 42));
        assert_eq!(GuestClocks::default().sample(), GuestTimes::default());
    }

    /// A clock that reads less than it did before is not believed: the vCPU's time, and its
    /// machine's, never fall.
    #[test]
    fn a_vcpus_time_never_falls_nor_does_the_machines() {
        let clocks = GuestClocks::default();
        let at = Arc::new(AtomicU64::new(500));
        let _zero = clocks.join(0, clock(&at));
        assert_eq!(clocks.sample().total_ns(), 500);
        at.store(400, Ordering::SeqCst);
        let t = clocks.sample();
        assert_eq!((times(&t), t.total_ns()), (vec![(0, 500)], 500));
        at.store(600, Ordering::SeqCst);
        assert_eq!(clocks.sample().total_ns(), 600);
    }

    /// A read that fails says why, typed, beside the most read before; one that never read
    /// says why at none; neither keeps the others from being read.
    #[test]
    fn an_unread_vcpu_says_why_and_keeps_its_last() {
        let clocks = GuestClocks::default();
        let fails = Arc::new(AtomicU64::new(0));
        let failing = fails.clone();
        let read: Read = Box::new(move || match failing.load(Ordering::SeqCst) {
            0 => Ok(300),
            _ => Err(Unread::ThreadStat(Some(2))),
        });
        let _zero = clocks.join(0, read);
        let _one = clocks.join(1, Box::new(|| Err(Unread::Gone)));
        let at = Arc::new(AtomicU64::new(8));
        let _two = clocks.join(2, clock(&at));
        assert_eq!(clocks.sample().vcpus[0].unread, None);
        fails.store(1, Ordering::SeqCst);
        let t = clocks.sample();
        assert_eq!(times(&t), [(0, 300), (1, 0), (2, 8)]);
        assert_eq!(t.vcpus[0].unread, Some(Unread::ThreadStat(Some(2))));
        assert_eq!(t.vcpus[1].unread, Some(Unread::Gone));
        assert_eq!(t.vcpus[2].unread, None);
        assert_eq!(t.total_ns(), 308);
    }

    /// An index that comes back is a new vCPU, from none, the first's time kept as the
    /// machine's; one that joins where another of its index never left takes its place, the
    /// other's time kept, and the other's leaving later takes nothing from the new one.
    #[test]
    fn an_index_that_comes_back_starts_again_and_the_total_keeps_the_first() {
        let clocks = GuestClocks::default();
        let first = Arc::new(AtomicU64::new(1_000));
        drop(clocks.join(3, clock(&first)));
        let again = Arc::new(AtomicU64::new(2));
        let back = clocks.join(3, clock(&again));
        let t = clocks.sample();
        assert_eq!(
            (times(&t), t.departed_ns, t.total_ns()),
            (vec![(3, 2)], 1_000, 1_002)
        );
        // Another of index 3 while `back` is still there: `back`'s 2 is the machine's.
        let newer = Arc::new(AtomicU64::new(50));
        let _newest = clocks.join(3, clock(&newer));
        let t = clocks.sample();
        assert_eq!(
            (times(&t), t.departed_ns, t.total_ns()),
            (vec![(3, 50)], 1_002, 1_052)
        );
        drop(back);
        let t = clocks.sample();
        assert_eq!(
            (times(&t), t.total_ns()),
            (vec![(3, 50)], 1_052),
            "the newer stays"
        );
    }

    /// Samples on another thread, as vCPUs run, leave and come back: each vCPU's time and
    /// the machine's never fall from one sample to the next. Sampled until every vCPU has
    /// come and gone `ROUNDS` times, however late its thread first runs: a fixed count of
    /// samples ended before any had on CI's arm64 runners (run 43230466376), and the test
    /// then found nothing gone.
    #[test]
    fn samples_never_see_the_total_fall_as_vcpus_come_and_go() {
        const VCPUS: u32 = 4;
        const ROUNDS: u64 = 100;
        /// Ends the vCPUs' loops however the sampling ends, a failed check's panic included,
        /// which the scope would otherwise wait out for ever.
        struct Done<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let clocks = GuestClocks::default();
        let done = std::sync::atomic::AtomicBool::new(false);
        let rounds: Vec<AtomicU64> = (0..VCPUS).map(|_| AtomicU64::new(0)).collect();
        let samples = std::thread::scope(|s| {
            let _done = Done(&done);
            for (index, rounds) in (0..VCPUS).zip(&rounds) {
                let (clocks, done) = (&clocks, &done);
                s.spawn(move || {
                    while !done.load(Ordering::SeqCst) {
                        let at = Arc::new(AtomicU64::new(0));
                        let joined = clocks.join(index, clock(&at));
                        for step in 1..=50 {
                            at.store(step * (u64::from(index) + 1), Ordering::SeqCst);
                        }
                        drop(joined);
                        rounds.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
            let (mut last, mut samples) = (None::<GuestTimes>, 0u64);
            while rounds.iter().any(|r| r.load(Ordering::SeqCst) < ROUNDS) {
                let t = clocks.sample();
                if let Some(before) = &last {
                    assert!(t.total_ns() >= before.total_ns(), "{before:?} then {t:?}");
                    assert!(t.departed_ns >= before.departed_ns);
                }
                last = Some(t);
                samples += 1;
                std::thread::yield_now();
            }
            samples
        });
        let t = clocks.sample();
        let rounds: Vec<u64> = rounds.iter().map(|r| r.load(Ordering::SeqCst)).collect();
        assert!(
            t.vcpus.is_empty() && t.departed_ns > 0,
            "{t:?} after {samples} samples, the vCPUs' rounds {rounds:?}"
        );
    }
}
