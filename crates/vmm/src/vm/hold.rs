//! The host's hold on a machine's vCPUs (D191): together they use at most the CPU time
//! their run's limit gives them each period, whatever their guest does with its own
//! cgroups, as CFS bandwidth control holds a cgroup's threads
//! (Documentation/scheduler/sched-bwc.rst). A governor reads each vCPU thread's CPU time;
//! once a period's share is spent, every vCPU is held out of its guest until the period
//! ends, and what a period overran comes off the next ones. A pause holds every vCPU out
//! of its guest whatever its share, until the machine goes on. A machine of a group's
//! (D183, slice 3) is held by the group's budget too, which every machine of the group
//! charges ([`GroupCpu`]): a second governor charges its vCPUs' time to it and holds them
//! while the group runs past it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::group::GroupCpu;
use crate::platform::ThreadClock;
use crate::sync::{lock, wait, wait_timeout};

/// A period: CFS bandwidth control's default (`cpu.max`'s 100000 µs).
pub(super) const PERIOD: Duration = Duration::from_millis(100);
/// The longest the governor waits between reads while its vCPUs are busy, and so about
/// the most each may overrun a period's share by before they are held.
const BUSY: Duration = Duration::from_millis(10);
/// The least it waits between reads, however little of the share is left.
const SOONEST: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub(super) struct Hold {
    state: Mutex<State>,
    /// The governor waits on it: a new limit, or the machine's end.
    changed: Condvar,
    /// vCPUs wait on it while held.
    released: Condvar,
    /// A pause waits on it for its vCPUs to be held.
    waited: Condvar,
    /// `State::held`, `State::group_held` or `State::paused`, for a vCPU's look before
    /// each entry to its guest without the lock.
    held: AtomicBool,
    /// Each vCPU thread's clock, which it gives as it starts.
    clocks: Box<[OnceLock<ThreadClock>]>,
}

#[derive(Debug, Default)]
struct State {
    /// The CPUs whose time the vCPUs may use together; 0 for no hold.
    cpus: u32,
    /// Bumped by each new limit, which begins a period of its own.
    generation: u64,
    held: bool,
    /// Held by its group's budget, past which its group runs (D183).
    group_held: bool,
    /// Held whatever the share: the machine paused, until it goes on.
    paused: bool,
    /// The vCPUs waiting in [`Hold::wait`], each out of its guest.
    waiting: usize,
    ended: bool,
}

impl Hold {
    pub(super) fn new(vcpus: usize) -> Hold {
        Hold {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            released: Condvar::new(),
            waited: Condvar::new(),
            held: AtomicBool::new(false),
            clocks: (0..vcpus).map(|_| OnceLock::new()).collect(),
        }
    }

    /// On vCPU `index`'s thread: its clock, which the governor reads.
    pub(super) fn register(&self, index: usize) -> std::io::Result<()> {
        let clock = ThreadClock::current()?;
        if let Some(slot) = self.clocks.get(index) {
            let _ = slot.set(clock);
        }
        Ok(())
    }

    /// The vCPUs may use `cpus` CPUs' time of the host's together, from now; 0, or as
    /// many as there are vCPUs, lets them be.
    pub(super) fn set(&self, cpus: u32) {
        let mut s = lock(&self.state);
        s.cpus = cpus;
        s.generation = s.generation.wrapping_add(1);
        drop(s);
        self.changed.notify_all();
    }

    /// On a vCPU's thread, before it enters its guest: waits while the vCPUs are held,
    /// and returns at once once `stop` says so (the machine ends, or a snapshot needs it).
    pub(super) fn wait(&self, stop: &dyn Fn() -> bool) {
        if !self.held.load(Ordering::Acquire) {
            return;
        }
        let mut s = lock(&self.state);
        s.waiting = s.waiting.saturating_add(1);
        self.waited.notify_all();
        while (s.held || s.group_held || s.paused) && !stop() {
            s = wait(&self.released, s);
        }
        s.waiting = s.waiting.saturating_sub(1);
    }

    /// The machine paused: every vCPU held out of its guest whatever its share, `kick`
    /// taking each out, until [`resume`](Self::resume). Returns once all `vcpus` wait held,
    /// whether they entered their guest already or not (`out`: they never left their
    /// start, or are all out already); false where the machine ends first.
    #[cfg(any(hv, test))]
    pub(super) fn pause(&self, vcpus: usize, kick: &dyn Fn(), out: &dyn Fn() -> bool) -> bool {
        let mut s = lock(&self.state);
        if s.ended {
            return false;
        }
        s.paused = true;
        self.held.store(true, Ordering::Release);
        drop(s);
        kick();
        let mut s = lock(&self.state);
        while s.waiting < vcpus && !s.ended && !out() {
            s = wait(&self.waited, s);
        }
        !s.ended
    }

    /// The machine goes on: its vCPUs held no more but by their share.
    #[cfg(any(hv, test))]
    pub(super) fn resume(&self) {
        let mut s = lock(&self.state);
        s.paused = false;
        self.held.store(s.held || s.group_held, Ordering::Release);
        drop(s);
        self.released.notify_all();
    }

    /// Wakes vCPUs held, to look at what their `stop` says again. Only the running
    /// machine's kick does (`cfg(hv)`).
    #[cfg(hv)]
    pub(super) fn wake(&self) {
        // Taking the lock orders the wakeup after a waiter's look at its `stop`.
        drop(lock(&self.state));
        self.released.notify_all();
    }

    /// The machine ends: nothing is held from now on, and the governor returns.
    pub(super) fn end(&self) {
        let mut s = lock(&self.state);
        s.ended = true;
        s.held = false;
        s.group_held = false;
        s.paused = false;
        self.held.store(false, Ordering::Release);
        drop(s);
        self.changed.notify_all();
        self.released.notify_all();
        self.waited.notify_all();
    }

    fn set_held(&self, held: bool) {
        let mut s = lock(&self.state);
        s.held = held && !s.ended;
        self.held
            .store(s.held || s.group_held || s.paused, Ordering::Release);
        drop(s);
        if !held {
            self.released.notify_all();
        }
    }

    /// Holds the vCPUs by their group's budget, or lets them go by it; true where they were
    /// not held by it before.
    fn set_group_held(&self, held: bool) -> bool {
        let mut s = lock(&self.state);
        let newly = held && !s.group_held && !s.ended;
        s.group_held = held && !s.ended;
        self.held
            .store(s.held || s.group_held || s.paused, Ordering::Release);
        drop(s);
        if !held {
            self.released.notify_all();
        }
        newly
    }

    /// Waits up to `timeout`, or until the machine ends: whether it has.
    fn wait_or_end(&self, timeout: Duration) -> bool {
        let s = lock(&self.state);
        let s = if s.ended {
            s
        } else {
            wait_timeout(&self.changed, s, timeout)
        };
        s.ended
    }

    /// The CPU time the vCPU threads have used, in nanoseconds: those whose clocks read.
    fn used(&self) -> u64 {
        self.clocks
            .iter()
            .filter_map(|c| c.get()?.read().ok())
            .fold(0, u64::saturating_add)
    }

    /// The governor, on a thread of its own until the machine ends: holds the vCPUs once
    /// a period's share is spent, `kick` taking each out of its guest, and lets them go as
    /// the period ends. While they are busy (holding, using half the share or more, or
    /// in a new limit's first period) it reads their time as soon as the share could be
    /// spent were every vCPU to run, no sooner than [`SOONEST`] and no later than
    /// [`BUSY`]; while they are not, once a period, so that vCPUs gone from quiet to busy
    /// may run that period through before they are held. What a period overran comes off
    /// the next ones.
    pub(super) fn govern(&self, kick: &dyn Fn()) {
        // It runs as it wakes, its waits ending on time, as a vCPU's timers do (PM M8,
        // M10, M294): one that runs late lets the vCPUs overrun, or holds them past their
        // period.
        let _prioritized = match crate::platform::prioritize_governor_thread() {
            Ok(prioritized) => Some(prioritized),
            Err(e) => {
                crate::debug!("the hold's governor runs at its vCPUs' priority: {e}");
                None
            }
        };
        let vcpus = u64::try_from(self.clocks.len()).unwrap_or(u64::MAX).max(1);
        let period = u64::try_from(PERIOD.as_nanos()).unwrap_or(u64::MAX);
        let mut debt = 0u64;
        let mut busy = true;
        let mut start = Instant::now();
        let mut last = None;
        loop {
            let (cpus, generation) = {
                let mut s = lock(&self.state);
                // As many CPUs as vCPUs is no hold: they can use no more.
                while !s.ended && (s.cpus == 0 || u64::from(s.cpus) >= vcpus) {
                    s = wait(&self.changed, s);
                }
                if s.ended {
                    return;
                }
                (s.cpus, s.generation)
            };
            // Periods follow one another on the clock, as late as the governor wakes; a
            // new limit, or a governor fallen a period behind, begins them again now.
            let now = Instant::now();
            if last != Some(generation) || now.saturating_duration_since(start) >= PERIOD {
                start = now;
            }
            last = Some(generation);
            let base = self.used();
            let share = u64::from(cpus).saturating_mul(period);
            let allowed = share.saturating_sub(debt);
            let mut holding = false;
            let mut changed = false;
            while let Some(left) = PERIOD.checked_sub(start.elapsed()).filter(|l| !l.is_zero()) {
                let used = self.used().saturating_sub(base);
                if !holding && used >= allowed {
                    holding = true;
                    self.set_held(true);
                    kick();
                }
                let next = if holding || !busy {
                    left
                } else {
                    Duration::from_nanos(allowed.saturating_sub(used) / vcpus)
                        .clamp(SOONEST, BUSY)
                        .min(left)
                };
                let mut s = lock(&self.state);
                if !s.ended && s.generation == generation {
                    s = wait_timeout(&self.changed, s, next);
                }
                if s.ended {
                    return;
                }
                if s.generation != generation {
                    changed = true;
                    break;
                }
            }
            let used = self.used().saturating_sub(base);
            // A new limit begins anew; otherwise what is owed past this period's share
            // comes off the next ones.
            (debt, busy) = if changed {
                (0, true)
            } else {
                (
                    debt.saturating_add(used).saturating_sub(share),
                    holding || used.saturating_mul(2) >= share,
                )
            };
            start += PERIOD;
            self.set_held(false);
        }
    }

    /// The group's governor (D183, slice 3), on a thread of its own until the machine
    /// ends: charges the vCPUs' time to their group's budget, `group`, which every machine
    /// of the group charges, and holds them, `kick` taking each out of its guest, while the
    /// group runs more than a period ahead of its budget's rate, until it is within it. As
    /// [`govern`](Self::govern) reads, it reads their time as soon as the group's room
    /// could be spent were every vCPU to run, between [`SOONEST`] and [`BUSY`], while the
    /// group uses half its room or more; else once a period, what a quiet machine gone
    /// busy runs past it within that period coming off the group's next ones. With no
    /// budget written it looks again each period of D191's.
    pub(super) fn govern_group(&self, group: &GroupCpu, kick: &dyn Fn()) {
        let _prioritized = match crate::platform::prioritize_governor_thread() {
            Ok(prioritized) => Some(prioritized),
            Err(e) => {
                crate::debug!("the group's governor runs at its vCPUs' priority: {e}");
                None
            }
        };
        let vcpus = u64::try_from(self.clocks.len()).unwrap_or(u64::MAX).max(1);
        let mut last = self.used();
        loop {
            let next = match (group.budget(), crate::platform::monotonic_ns()) {
                (Some((quota, period)), Ok(now)) => {
                    let used = self.used();
                    let ahead = group.charge(now, used.saturating_sub(last), quota, period);
                    last = used;
                    // A period of the budget's, in ns: how far ahead of its rate the
                    // group may run.
                    let room = u64::from(period).saturating_mul(1000);
                    let whole = Duration::from_nanos(room);
                    if ahead > room {
                        if self.set_group_held(true) {
                            kick();
                        }
                        // Until the group is within its budget again, looking at it at
                        // least once a period, as the daemon may have written another.
                        Duration::from_nanos(ahead - room).clamp(SOONEST, whole.max(SOONEST))
                    } else {
                        self.set_group_held(false);
                        if ahead.saturating_mul(2) >= room {
                            // The CPU time the room left is worth at the budget's rate,
                            // were every vCPU of this machine's to spend it.
                            let left = u128::from(room - ahead) * u128::from(quota)
                                / u128::from(period)
                                / u128::from(vcpus);
                            Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX))
                                .clamp(SOONEST, BUSY)
                                .min(whole)
                        } else {
                            whole
                        }
                    }
                }
                (None, _) => {
                    self.set_group_held(false);
                    last = self.used();
                    PERIOD
                }
                (Some(_), Err(e)) => {
                    crate::warn!("the group's governor: the host's clock: {e}");
                    self.set_group_held(false);
                    last = self.used();
                    PERIOD
                }
            };
            if self.wait_or_end(next) {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Threads as vCPUs: each spins in slices of 200 µs, looking at the hold before each,
    /// as a vCPU does at each of its exits, until `stop`. The CPU time they used in about
    /// `window`, once every one has given its clock, and how long that window was.
    fn spin(hold: &Hold, threads: usize, window: Duration) -> (u64, u64) {
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let governor = s.spawn(|| hold.govern(&|| {}));
            for index in 0..threads {
                let stop = &stop;
                s.spawn(move || {
                    hold.register(index).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        hold.wait(&|| stop.load(Ordering::Relaxed));
                        let t = Instant::now();
                        while t.elapsed() < Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                    }
                });
            }
            while hold.clocks.iter().any(|c| c.get().is_none()) {
                std::thread::yield_now();
            }
            let (t0, base) = (Instant::now(), hold.used());
            std::thread::sleep(window);
            let used = hold.used() - base;
            let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
            stop.store(true, Ordering::Relaxed);
            hold.end();
            governor.join().unwrap();
            (used, wall)
        })
    }

    /// Three threads held to one CPU use no more of it than the time passing, but what a
    /// period may overrun, and are let go each period: they run a share of it.
    #[test]
    fn threads_held_use_no_more_than_their_share() {
        let hold = Hold::new(3);
        hold.set(1);
        let (used, wall) = spin(&hold, 3, Duration::from_secs(1));
        // One CPU's time over the window, a period's share more where the window and the
        // periods do not line up, and what three may overrun before they are held, their
        // governor woken late on a busy host: under the three CPUs' time three would use.
        let most = wall + 300_000_000;
        assert!(used <= most, "{used} ns of CPU in {wall} ns, held to one CPU");
        assert!(
            used >= wall / 4,
            "{used} ns of CPU in {wall} ns: held, and never let go"
        );
    }

    /// A group's budget file, as the daemon makes it, in `dir`.
    fn group_file(dir: &std::path::Path) -> std::fs::File {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("cpu"))
            .unwrap();
        file.set_len(super::super::group::LEN as u64).unwrap();
        file
    }

    /// As [`spin`], for machines of one group: each machine's threads spin, its group's
    /// governor charging `groups`' mapping of its own of the group's budget. The CPU time
    /// all of them used in about `window`, and how long that window was.
    fn spin_group(holds: &[Hold], groups: &[GroupCpu], window: Duration) -> (u64, u64) {
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let mut governors = Vec::new();
            for (hold, group) in holds.iter().zip(groups) {
                governors.push(s.spawn(move || hold.govern_group(group, &|| {})));
                for index in 0..hold.clocks.len() {
                    let stop = &stop;
                    s.spawn(move || {
                        hold.register(index).unwrap();
                        while !stop.load(Ordering::Relaxed) {
                            hold.wait(&|| stop.load(Ordering::Relaxed));
                            let t = Instant::now();
                            while t.elapsed() < Duration::from_micros(200) {
                                std::hint::spin_loop();
                            }
                        }
                    });
                }
            }
            while holds.iter().any(|h| h.clocks.iter().any(|c| c.get().is_none())) {
                std::thread::yield_now();
            }
            let all = || holds.iter().map(Hold::used).sum::<u64>();
            let (t0, base) = (Instant::now(), all());
            std::thread::sleep(window);
            let used = all() - base;
            let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
            stop.store(true, Ordering::Relaxed);
            for hold in holds {
                hold.end();
            }
            for governor in governors {
                governor.join().unwrap();
            }
            (used, wall)
        })
    }

    /// Two machines of a group whose budget is one CPU, two threads each, each machine's
    /// governor charging its own mapping of the budget: together they use no more than the
    /// time passing, but the period's quota the group may run ahead and what each may run
    /// past it before it is held; and they are let go, running a share of it.
    #[test]
    fn machines_of_a_group_are_held_to_its_budget_together() {
        let dir = shards_testdir::TempDir::new("hold-group").unwrap();
        let file = group_file(&dir);
        GroupCpu::map(file.try_clone().unwrap())
            .unwrap()
            .set_budget(Some((100_000, 100_000)));
        let groups = [
            GroupCpu::map(file.try_clone().unwrap()).unwrap(),
            GroupCpu::map(file).unwrap(),
        ];
        let holds = [Hold::new(2), Hold::new(2)];
        let (used, wall) = spin_group(&holds, &groups, Duration::from_secs(1));
        // One CPU's time over the window; the period's quota the group may run ahead, and
        // what four threads may run past it before they are held, their governors woken
        // late on a busy host: under the four CPUs' time four would use.
        let most = wall + 300_000_000;
        assert!(
            used <= most,
            "{used} ns of CPU in {wall} ns, two machines held to one CPU together"
        );
        assert!(
            used >= wall / 4,
            "{used} ns of CPU in {wall} ns: held, and never let go"
        );
    }

    /// A machine held by its group's budget is let go within a period of the budget's
    /// going (`max` written in its place), and held by it no more.
    #[test]
    fn a_group_whose_budget_goes_holds_nothing() {
        /// Lets the thread go however the test ends.
        struct Ends<'a>(&'a Hold, &'a AtomicBool);
        impl Drop for Ends<'_> {
            fn drop(&mut self) {
                self.1.store(true, Ordering::Relaxed);
                self.0.end();
            }
        }
        let dir = shards_testdir::TempDir::new("hold-group-none").unwrap();
        let file = group_file(&dir);
        let daemon = GroupCpu::map(file.try_clone().unwrap()).unwrap();
        // A hundredth of a CPU: spent within the first period.
        daemon.set_budget(Some((1_000, 100_000)));
        let group = GroupCpu::map(file).unwrap();
        let hold = Hold::new(1);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let ends = Ends(&hold, &stop);
            let governor = s.spawn(|| hold.govern_group(&group, &|| {}));
            s.spawn(|| {
                hold.register(0).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    hold.wait(&|| stop.load(Ordering::Relaxed));
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_micros(200) {
                        std::hint::spin_loop();
                    }
                }
            });
            let held = || lock(&hold.state).group_held;
            let t = Instant::now();
            while !held() {
                assert!(t.elapsed() < Duration::from_secs(5), "never held by its group");
                std::thread::sleep(Duration::from_millis(1));
            }
            daemon.set_budget(None);
            let t = Instant::now();
            while held() {
                assert!(
                    t.elapsed() < Duration::from_secs(1),
                    "still held a second after its group's budget went"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            // Held no more: it runs.
            let base = hold.used();
            std::thread::sleep(Duration::from_millis(200));
            assert!(!held());
            assert!(hold.used() > base, "let go, it did not run");
            drop(ends);
            governor.join().unwrap();
        });
    }

    /// A hold of as many CPUs as there are vCPUs holds nothing, and its governor waits.
    #[test]
    fn a_hold_of_every_cpu_holds_nothing() {
        let hold = Hold::new(2);
        hold.set(2);
        let (used, _) = spin(&hold, 2, Duration::from_millis(300));
        assert!(!hold.held.load(Ordering::Acquire));
        // Whatever the host gave them: nothing held them.
        assert!(used > 0);
    }

    /// A pause returns once every vCPU is held, and they use next to no CPU time until the
    /// machine goes on, though a share of the host's would let them; then they run again.
    #[test]
    fn a_pause_holds_every_vcpu_until_the_machine_goes_on() {
        /// Lets the threads go however the test ends: a failed assertion with them held
        /// would leave the scope waiting on them.
        struct Ends<'a>(&'a Hold, &'a AtomicBool);
        impl Drop for Ends<'_> {
            fn drop(&mut self) {
                self.1.store(true, Ordering::Relaxed);
                self.0.end();
            }
        }
        let hold = Hold::new(2);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let _ends = Ends(&hold, &stop);
            for index in 0..2 {
                let (hold, stop) = (&hold, &stop);
                s.spawn(move || {
                    hold.register(index).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        hold.wait(&|| stop.load(Ordering::Relaxed));
                        let t = Instant::now();
                        while t.elapsed() < Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                    }
                });
            }
            while hold.clocks.iter().any(|c| c.get().is_none()) {
                std::thread::yield_now();
            }
            assert!(hold.pause(2, &|| {}, &|| false));
            assert_eq!(lock(&hold.state).waiting, 2);
            // Held, each thread's clock moves no more than its way into its wait does: well
            // under the 400 ms the two would spin in the window.
            let base = hold.used();
            std::thread::sleep(Duration::from_millis(200));
            let used = hold.used() - base;
            assert!(used < 10_000_000, "a paused vCPU ran: {used} ns of CPU in 200 ms");
            hold.resume();
            let base = hold.used();
            std::thread::sleep(Duration::from_millis(200));
            assert!(hold.used() > base, "the machine went on, its vCPUs did not");
        });
    }

    /// A pause waits for no vCPU that is out of its guest by other means, and returns
    /// false once the machine ends before its vCPUs are held.
    #[test]
    fn a_pause_returns_for_vcpus_out_already_and_for_the_end() {
        let hold = Hold::new(1);
        assert!(hold.pause(1, &|| {}, &|| true));
        hold.resume();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                hold.end();
            });
            assert!(!hold.pause(1, &|| {}, &|| false));
        });
        assert!(
            !hold.pause(1, &|| {}, &|| false),
            "an ended machine is no pause's"
        );
    }

    /// A vCPU waiting while held goes once the machine ends.
    #[test]
    fn the_end_lets_a_held_vcpu_go() {
        let hold = Hold::new(1);
        hold.set_held(true);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| hold.wait(&|| false));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!waiting.is_finished());
            hold.end();
            waiting.join().unwrap();
        });
    }
}
