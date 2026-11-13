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

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::MILLI_PER_CPU;
use super::group::GroupCpu;
use crate::platform::ThreadClock;
use crate::sync::{lock, wait, wait_timeout};

/// A period: CFS bandwidth control's default, `cpu.max`'s 100000 µs
/// (kernel/sched/sched.h l. 445-447, `default_bw_period_us`, Linux 1590cf03), which Docker's
/// `--cpus` is a quota of.
pub(super) const PERIOD: Duration = Duration::from_millis(100);
/// The least it waits between reads, however little of the share is left: as closely as
/// CFS bandwidth control holds a cgroup's threads to its quota, 1 ms. CFS takes no quota
/// or period below it (kernel/sched/core.c l. 10054, `min_bw_quota_period_us`), and leaves
/// each CPU that much of its runtime past the quota (kernel/sched/fair.c l. 7179,
/// `min_cfs_rq_runtime`; Documentation/scheduler/sched-bwc.rst l. 94-95, 179-186; Linux
/// 1590cf03), as vCPUs read no sooner may each run that far past their share, which comes
/// off the next period's.
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
    /// The CPU time the vCPUs may use together, in milli-CPU ([`MILLI_PER_CPU`]); 0 for no
    /// hold.
    milli: u64,
    /// Bumped by each new limit, which begins a period of its own.
    generation: u64,
    /// Held by its share until its period ends, as the governor said as it held them: a
    /// vCPU goes on at that end by itself, however late the governor wakes to let it go.
    held: Option<Instant>,
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

    /// The vCPUs may use `milli` milli-CPU of the host's time together, from now; 0, or as
    /// much as every vCPU's whole CPU, lets them be. Returns what it holds them to: none
    /// where it lets them be.
    pub(super) fn set(&self, milli: u64) -> Option<NonZeroU64> {
        let mut s = lock(&self.state);
        s.milli = milli;
        s.generation = s.generation.wrapping_add(1);
        drop(s);
        self.changed.notify_all();
        self.holds(milli)
    }

    /// What a hold of `milli` holds these vCPUs to: none for 0, or for as much as every
    /// vCPU's whole CPU, which they can use no more of.
    fn holds(&self, milli: u64) -> Option<NonZeroU64> {
        let vcpus = u64::try_from(self.clocks.len()).unwrap_or(u64::MAX).max(1);
        NonZeroU64::new(milli).filter(|m| m.get() < vcpus.saturating_mul(MILLI_PER_CPU))
    }

    /// On a vCPU's thread, before it enters its guest: waits while the vCPUs are held,
    /// and returns at once once `stop` says so (the machine ends, or a snapshot needs it).
    /// Held by its share, it waits no longer than its period: a governor that wakes late
    /// on a busy host held its vCPUs as late, up to 57 s past their period on an overloaded
    /// one (PM M441).
    pub(super) fn wait(&self, stop: &dyn Fn() -> bool) {
        if !self.held.load(Ordering::Acquire) {
            return;
        }
        let mut s = lock(&self.state);
        s.waiting = s.waiting.saturating_add(1);
        self.waited.notify_all();
        while !stop() {
            if s.group_held || s.paused {
                s = wait(&self.released, s);
                continue;
            }
            let Some(until) = s.held else {
                break;
            };
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = wait_timeout(&self.released, s, left);
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
        self.held
            .store(s.held.is_some() || s.group_held, Ordering::Release);
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
        s.held = None;
        s.group_held = false;
        s.paused = false;
        self.held.store(false, Ordering::Release);
        drop(s);
        self.changed.notify_all();
        self.released.notify_all();
        self.waited.notify_all();
    }

    /// Holds the vCPUs by their share until `until`, the period's end; `None` lets them go.
    fn set_held(&self, until: Option<Instant>) {
        let mut s = lock(&self.state);
        s.held = until.filter(|_| !s.ended);
        self.held
            .store(s.held.is_some() || s.group_held || s.paused, Ordering::Release);
        drop(s);
        if until.is_none() {
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
            .store(s.held.is_some() || s.group_held || s.paused, Ordering::Release);
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
    /// the period ends. While they are busy (holding, in a new limit's first period, or
    /// spending the share in the next period as their last two foretell, [`busy_next`]) it
    /// reads their time as soon as the share could be spent were every vCPU to run, no
    /// sooner than [`SOONEST`]: none can spend it before, as vCPUs use at most their
    /// number's CPU time; while they are not, once a period, so that vCPUs gone from quiet
    /// to busy at once may run that period through before they are held. What a period
    /// overran comes off the next ones.
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
        // What the period before the last used: none before a machine's first.
        let mut before = 0u64;
        let mut start = Instant::now();
        let mut last = None;
        loop {
            let (milli, generation) = {
                let mut s = lock(&self.state);
                // Every vCPU's whole CPU is no hold: they can use no more.
                let mut held = self.holds(s.milli);
                while !s.ended && held.is_none() {
                    s = wait(&self.changed, s);
                    held = self.holds(s.milli);
                }
                match held {
                    Some(milli) if !s.ended => (milli, s.generation),
                    _ => return,
                }
            };
            // Periods follow one another on the clock, as late as the governor wakes; a
            // new limit, or a governor fallen a period behind, begins them again now.
            let now = Instant::now();
            if last != Some(generation) || now.saturating_duration_since(start) >= PERIOD {
                start = now;
            }
            last = Some(generation);
            let base = self.used();
            let share = share_of(milli, period);
            let allowed = share.saturating_sub(debt);
            let mut holding = false;
            let mut changed = false;
            while let Some(left) = PERIOD.checked_sub(start.elapsed()).filter(|l| !l.is_zero()) {
                let used = self.used().saturating_sub(base);
                if !holding && used >= allowed {
                    holding = true;
                    self.set_held(Some(start + PERIOD));
                    kick();
                }
                let next = if holding || !busy {
                    left
                } else {
                    Duration::from_nanos(allowed.saturating_sub(used) / vcpus)
                        .max(SOONEST)
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
            (debt, busy, before) = if changed {
                (0, true, 0)
            } else {
                (
                    debt.saturating_add(used).saturating_sub(share),
                    holding || busy_next(before, used, share),
                    used,
                )
            };
            start += PERIOD;
            self.set_held(None);
        }
    }

    /// The group's governor (D183, slice 3), on a thread of its own until the machine
    /// ends: charges the vCPUs' time to their group's budget, `group`, which every machine
    /// of the group charges, and holds them, `kick` taking each out of its guest, while the
    /// group runs more than a period ahead of its budget's rate, until it is within it. As
    /// [`govern`](Self::govern) reads, it reads their time as soon as the group's room
    /// could be spent, no sooner than [`SOONEST`], while the group eats into its room at a
    /// rate that would spend it within a period ([`room_lasts`], read from its last two
    /// reads): were every vCPU of this machine's to spend it, or, sooner, at that rate,
    /// which its other machines' vCPUs spend too; else once a period, what a quiet machine
    /// gone busy runs past it within that period coming off the group's next ones. With no
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
        // The group's last read: when, and how far ahead of its budget's rate it ran.
        let mut seen: Option<(u64, u64)> = None;
        loop {
            let next = match (group.budget(), crate::platform::monotonic_ns()) {
                (Some((quota, period)), Ok(now)) => {
                    let used = self.used();
                    let ahead = group.charge(now, used.saturating_sub(last), quota, period);
                    last = used;
                    let before = seen.replace((now, ahead));
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
                        match before
                            .and_then(|b| room_lasts(b, (now, ahead), room))
                            .filter(|&at_rate| at_rate < whole)
                        {
                            Some(at_rate) => {
                                // The CPU time the room left is worth at the budget's rate,
                                // were every vCPU of this machine's to spend it.
                                let left = u128::from(room - ahead) * u128::from(quota)
                                    / u128::from(period)
                                    / u128::from(vcpus);
                                let own = Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX));
                                own.min(at_rate).max(SOONEST).min(whole)
                            }
                            None => whole,
                        }
                    }
                }
                (None, _) => {
                    self.set_group_held(false);
                    last = self.used();
                    seen = None;
                    PERIOD
                }
                (Some(_), Err(e)) => {
                    crate::warn!("the group's governor: the host's clock: {e}");
                    self.set_group_held(false);
                    last = self.used();
                    seen = None;
                    PERIOD
                }
            };
            if self.wait_or_end(next) {
                return;
            }
        }
    }
}

/// Whether vCPUs that used `before` and then `used` of a period's CPU time spend `share`
/// in the next, as the two foretell: `used` again and as much more as it grew by, a
/// machine's own trend, where none of their use is known before them. A machine's first
/// period, after none, is foretold twice its use.
fn busy_next(before: u64, used: u64, share: u64) -> bool {
    used.saturating_add(used.saturating_sub(before)) >= share
}

/// How long a group's room lasts, read `before` and `now` as `(when, how far ahead of its
/// budget's rate)`, each in ns: at the rate it ran further ahead between the two, until it
/// is `room` ahead. None where it did not run further ahead: its spending is then no
/// faster than its budget's rate, at which the room lasts.
fn room_lasts(before: (u64, u64), now: (u64, u64), room: u64) -> Option<Duration> {
    let elapsed = now.0.checked_sub(before.0).filter(|&e| e > 0)?;
    let grew = now.1.checked_sub(before.1).filter(|&g| g > 0)?;
    let left = u128::from(room.saturating_sub(now.1)) * u128::from(elapsed) / u128::from(grew);
    Some(Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX)))
}

/// A period's share of `period` nanoseconds at `milli` milli-CPU, exact to the nanosecond
/// below: `milli` thousandths of the period, as many CPUs' worth of it as `milli` holds.
fn share_of(milli: NonZeroU64, period: u64) -> u64 {
    let share = u128::from(milli.get()).saturating_mul(u128::from(period)) / u128::from(MILLI_PER_CPU);
    u64::try_from(share).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A machine is busy next period where its use grows to its share as its last two
    /// periods foretell: its first, after none, at half the share; one that holds steady
    /// below it, not; one growing toward it, before it gets there.
    #[test]
    fn a_machine_is_busy_as_its_last_two_periods_foretell() {
        assert!(busy_next(0, 50, 100));
        assert!(!busy_next(0, 49, 100));
        assert!(!busy_next(90, 90, 100), "steady below its share");
        assert!(!busy_next(95, 90, 100), "falling");
        assert!(busy_next(70, 85, 100), "growing to it");
        assert!(busy_next(100, 100, 100), "at it");
        assert!(busy_next(0, u64::MAX, u64::MAX), "saturating");
    }

    /// A group's room lasts as long as the rate it was last seen eating into it leaves it:
    /// 6 ms ahead of 10 after 2 ms of 4 more lasts 2 ms more; a group no further ahead, or
    /// read at one instant, has no such rate.
    #[test]
    fn a_groups_room_lasts_at_the_rate_it_was_seen_spent() {
        let ms = 1_000_000;
        assert_eq!(
            room_lasts((0, 2 * ms), (2 * ms, 6 * ms), 10 * ms),
            Some(Duration::from_millis(2))
        );
        assert_eq!(room_lasts((0, 6 * ms), (2 * ms, 6 * ms), 10 * ms), None);
        assert_eq!(room_lasts((0, 7 * ms), (2 * ms, 6 * ms), 10 * ms), None);
        assert_eq!(room_lasts((2 * ms, 2 * ms), (2 * ms, 6 * ms), 10 * ms), None);
        assert_eq!(room_lasts((0, 0), (ms, 11 * ms), 10 * ms), Some(Duration::ZERO));
    }

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

    /// A rate's share of a period is that many thousandths of it, exact to the nanosecond
    /// below (D243): a whole CPU's, the period itself, as D191's whole CPUs had; a CPU and
    /// a half's, a period and a half; the least rate's, a thousandth; past what a u64 holds,
    /// the most it holds.
    #[test]
    fn a_rates_share_is_its_thousandths_of_the_period() {
        let period = u64::try_from(PERIOD.as_nanos()).unwrap();
        let milli = |m| NonZeroU64::new(m).unwrap();
        assert_eq!(share_of(milli(MILLI_PER_CPU), period), period);
        assert_eq!(share_of(milli(3 * MILLI_PER_CPU), period), 3 * period);
        assert_eq!(share_of(milli(1500), period), period + period / 2);
        assert_eq!(share_of(milli(1), period), period / MILLI_PER_CPU);
        assert_eq!(share_of(milli(1), MILLI_PER_CPU - 1), 0);
        assert_eq!(share_of(milli(u64::MAX), u64::MAX), u64::MAX);
    }

    /// A hold holds its vCPUs to a rate below every vCPU's whole CPU, and lets them be at
    /// none and at every vCPU's whole CPU or more (D243), as D191's whole CPUs did.
    #[test]
    fn a_hold_holds_a_rate_below_its_vcpus_alone() {
        let hold = Hold::new(2);
        assert_eq!(hold.set(0), None);
        assert_eq!(hold.set(1), NonZeroU64::new(1));
        assert_eq!(hold.set(1500), NonZeroU64::new(1500));
        assert_eq!(hold.set(2 * MILLI_PER_CPU - 1), NonZeroU64::new(2 * MILLI_PER_CPU - 1));
        assert_eq!(hold.set(2 * MILLI_PER_CPU), None);
        assert_eq!(hold.set(u64::MAX), None);
    }

    /// Three threads held to one CPU use no more of it than the time passing, but what a
    /// period may overrun, and are let go each period: they run a share of it.
    #[test]
    fn threads_held_use_no_more_than_their_share() {
        let hold = Hold::new(3);
        hold.set(MILLI_PER_CPU);
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
        assert_eq!(hold.set(2 * MILLI_PER_CPU), None);
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
        hold.set_held(Some(Instant::now() + Duration::from_secs(3600)));
        std::thread::scope(|s| {
            let waiting = s.spawn(|| hold.wait(&|| false));
            std::thread::sleep(Duration::from_millis(50));
            assert!(!waiting.is_finished());
            hold.end();
            waiting.join().unwrap();
        });
    }

    /// A vCPU held by its share goes on as its period ends by itself, its governor never
    /// waking to let it go (one starved on a busy host, PM M441), and held no longer than
    /// that: it waits the period out, no less.
    #[test]
    fn a_held_vcpu_goes_on_at_its_periods_end_without_its_governor() {
        let hold = Hold::new(1);
        let t = Instant::now();
        hold.set_held(Some(t + Duration::from_millis(100)));
        hold.wait(&|| false);
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        // Its thread woken on time, but on a busy host.
        assert!(waited < Duration::from_secs(2), "{waited:?}");
        // Still held by the governor's word, a later look returns at once.
        let t = Instant::now();
        hold.wait(&|| false);
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
