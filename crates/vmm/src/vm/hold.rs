//! The host's hold on a machine's vCPUs (D191): together they use at most the CPU time
//! their run's limit gives them each period, whatever their guest does with its own
//! cgroups, as CFS bandwidth control holds a cgroup's threads
//! (Documentation/scheduler/sched-bwc.rst). A governor reads each vCPU's CPU time: its
//! thread's, its guest time added where its hypervisor runs it apart from its thread
//! (D265's charge: WHP under the classic and core schedulers);
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

use super::group::GroupCpu;
use super::guest_time::Charged;
use super::{MILLI_PER_CPU, Rated};
use crate::platform::{ThreadClock, granule};
use crate::sync::{lock, wait, wait_timeout};

/// The least it waits between reads, however little of the share is left: as closely as
/// CFS bandwidth control holds a cgroup's threads to its quota (`granule::LEAST_WAIT`), as
/// vCPUs read no sooner may each run that far past their share, which comes off the next
/// period's.
const SOONEST: Duration = granule::LEAST_WAIT;

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
    clocks: Box<[OnceLock<VcpuClock>]>,
}

/// What reads a vCPU's guest time, in nanoseconds, from any thread: none where it could not.
pub(super) type GuestRead = Box<dyn Fn() -> Option<u64> + Send + Sync>;

/// A vCPU's clocks, as the governor reads them: its thread's CPU time and its guest time.
struct VcpuClock {
    thread: ThreadClock,
    guest: Option<GuestRead>,
}

impl std::fmt::Debug for VcpuClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcpuClock").finish_non_exhaustive()
    }
}

impl VcpuClock {
    /// The CPU time it has used, in ns: its thread's where that holds its guest time; its
    /// guest time added where its hypervisor runs it apart from its thread; the larger of the
    /// two while which is not known, the most either shows it used (D265). None where its
    /// thread's clock does not read.
    fn used(&self, charged: Charged) -> Option<u64> {
        let thread = self.thread.read().ok()?;
        let guest = || self.guest.as_ref().and_then(|read| read()).unwrap_or(0);
        Some(match charged {
            Charged::ToThread => thread,
            Charged::Apart => thread.saturating_add(guest()),
            Charged::Unknown => thread.max(guest()),
        })
    }
}

#[derive(Debug, Default)]
struct State {
    /// The CPU time the vCPUs may use together, in milli-CPU ([`MILLI_PER_CPU`]); 0 for no
    /// hold.
    milli: u64,
    /// Bumped by each new limit, which begins a period of its own.
    generation: u64,
    /// The period the limit is held over, derived as it was set (`granule::period`).
    period: Duration,
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

    /// On vCPU `index`'s thread: its clocks, which the governor reads: the thread's own, and
    /// its guest time, `guest`, where it has one.
    pub(super) fn register(&self, index: usize, guest: Option<GuestRead>) -> std::io::Result<()> {
        let clock = VcpuClock {
            thread: ThreadClock::current()?,
            guest,
        };
        if let Some(slot) = self.clocks.get(index) {
            let _ = slot.set(clock);
        }
        Ok(())
    }

    /// The vCPUs may use `milli` milli-CPU of the host's time together, from now; 0, or as
    /// much as every vCPU's whole CPU, lets them be. Returns what it holds them to and over
    /// what period, derived from g as this process knows it now: its wake and its thread
    /// clock's step measured the first time, on the calling thread, one shortest wait and a
    /// spin of two clock steps at most (`granule::measure`). Refused, the hold as it was, where the period outgrows a
    /// u64 of ns.
    pub(super) fn set(&self, milli: u64) -> Result<Rated, granule::Outgrown> {
        let clock = granule::measure();
        let g = granule::g();
        let rated = self.set_at(milli, g)?;
        crate::debug!(
            "the hold: {milli} milli-CPU, held to {:?} over {:?}, from g {g:?} ({:?}; its clock: {clock:?})",
            rated.milli,
            rated.period,
            granule::step()
        );
        Ok(rated)
    }

    /// [`set`](Self::set), at a g of `g`.
    fn set_at(&self, milli: u64, g: Duration) -> Result<Rated, granule::Outgrown> {
        let vcpus = self.vcpus();
        let held = self.holds(milli);
        let whole = NonZeroU64::new(vcpus.saturating_mul(MILLI_PER_CPU)).unwrap_or(NonZeroU64::MIN);
        let period = granule::period(vcpus, held.unwrap_or(whole), g)?;
        let mut s = lock(&self.state);
        s.milli = milli;
        s.period = period;
        s.generation = s.generation.wrapping_add(1);
        drop(s);
        self.changed.notify_all();
        Ok(Rated { milli: held, period })
    }

    /// How many vCPUs it holds: one at the least.
    fn vcpus(&self) -> u64 {
        u64::try_from(self.clocks.len()).unwrap_or(u64::MAX).max(1)
    }

    /// What a hold of `milli` holds these vCPUs to: none for 0, or for as much as every
    /// vCPU's whole CPU, which they can use no more of.
    fn holds(&self, milli: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(milli).filter(|m| m.get() < self.vcpus().saturating_mul(MILLI_PER_CPU))
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

    /// The CPU time the vCPUs have used, in nanoseconds, as `charged` says to count it
    /// ([`VcpuClock::used`]): those whose threads' clocks read.
    fn used(&self, charged: Charged) -> u64 {
        self.clocks
            .iter()
            .filter_map(|c| c.get()?.used(charged))
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
    /// `charged` says how a vCPU's time is counted (D265), read once a period, so that a
    /// period's reads are all of one count.
    pub(super) fn govern(&self, kick: &dyn Fn(), charged: &dyn Fn() -> Charged) {
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
        let vcpus = self.vcpus();
        let mut debt = 0u64;
        let mut busy = true;
        // What the period before the last used: none before a machine's first.
        let mut before = 0u64;
        let mut start = Instant::now();
        let mut last = None;
        // Until when its vCPUs are held: past this period's end where what they overran
        // already owes the periods after it theirs.
        let mut held_until: Option<Instant> = None;
        loop {
            let (milli, generation, period) = {
                let mut s = lock(&self.state);
                // Every vCPU's whole CPU is no hold: they can use no more.
                let mut held = self.holds(s.milli);
                while !s.ended && held.is_none() {
                    s = wait(&self.changed, s);
                    held = self.holds(s.milli);
                }
                match held {
                    Some(milli) if !s.ended => (milli, s.generation, s.period),
                    _ => return,
                }
            };
            // Periods follow one another on the clock, as late as the governor wakes; a
            // new limit, or a governor fallen a period behind, begins them again now.
            let now = Instant::now();
            if last != Some(generation) || now.saturating_duration_since(start) >= period {
                start = now;
            }
            last = Some(generation);
            let counted = charged();
            let base = self.used(counted);
            let share = share_of(milli, u64::try_from(period.as_nanos()).unwrap_or(u64::MAX));
            let allowed = share.saturating_sub(debt);
            // Held on from a period before, by what its vCPUs overran then.
            let mut holding = held_until.is_some_and(|until| until > start);
            let mut changed = false;
            while let Some(left) = period.checked_sub(start.elapsed()).filter(|l| !l.is_zero()) {
                let used = self.used(counted).saturating_sub(base);
                if used >= allowed {
                    // Held to this period's end, and through as many after it as what its
                    // vCPUs overran already owes, whole shares of it: a vCPU let go at each
                    // period's end, its debt unpaid, runs again until the governor wakes and
                    // its kick lands, every period (hex-winvm: 38.8% of the wall at a quarter
                    // CPU, its guest time seen in ~10 ms steps, D265).
                    let owed = debt.saturating_add(used).saturating_sub(share);
                    let periods = owed
                        .checked_div(share)
                        .and_then(|p| u32::try_from(p).ok())
                        .and_then(|p| p.checked_add(1))
                        .unwrap_or(1);
                    let until = period
                        .checked_mul(periods)
                        .and_then(|d| start.checked_add(d))
                        .unwrap_or(start + period);
                    if held_until.is_none_or(|held| until > held) {
                        held_until = Some(until);
                        self.set_held(Some(until));
                    }
                    if !holding {
                        holding = true;
                        kick();
                    }
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
                    let began = Instant::now();
                    s = wait_timeout(&self.changed, s, next);
                    // Its lateness learned where it ended on its own (`granule::Late`).
                    if !s.ended && s.generation == generation {
                        granule::LATE.waited(next, began);
                    }
                }
                if s.ended {
                    return;
                }
                if s.generation != generation {
                    changed = true;
                    break;
                }
            }
            let used = self.used(counted).saturating_sub(base);
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
            start += period;
            // Let go at its period's end unless what it overran holds it on; a new limit
            // owes nothing of the old one's.
            if changed || held_until.is_none_or(|until| until <= start) {
                held_until = None;
                self.set_held(None);
            }
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
    /// quota (`cpu.max`'s `max`) it looks again each period of the group's own, as its
    /// `cpu.max` says it; a budget never written (the daemon writes one before any machine
    /// maps it, `Running::hold_group`) ends it.
    /// `charged` says how a vCPU's time is counted (D265); a read of another count charges
    /// nothing, beginning the next from it.
    pub(super) fn govern_group(&self, group: &GroupCpu, kick: &dyn Fn(), charged: &dyn Fn() -> Charged) {
        let _prioritized = match crate::platform::prioritize_governor_thread() {
            Ok(prioritized) => Some(prioritized),
            Err(e) => {
                crate::debug!("the group's governor runs at its vCPUs' priority: {e}");
                None
            }
        };
        let vcpus = self.vcpus();
        let mut counted = charged();
        let mut last = self.used(counted);
        // The group's last read: when, and how far ahead of its budget's rate it ran.
        let mut seen: Option<(u64, u64)> = None;
        loop {
            let Some(budget) = group.budget() else {
                crate::warn!("the group's governor: its group's CPU budget was never written");
                self.set_group_held(false);
                return;
            };
            let next = match (budget.quota, crate::platform::monotonic_ns()) {
                (Some(quota), Ok(now)) => {
                    let (quota, period) = (quota.get(), budget.period.get());
                    let now_counted = charged();
                    let used = self.used(now_counted);
                    let spent = if now_counted == counted {
                        used.saturating_sub(last)
                    } else {
                        0
                    };
                    (counted, last) = (now_counted, used);
                    let ahead = group.charge(now, spent, quota, period);
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
                (quota, clock) => {
                    if let (Some(_), Err(e)) = (quota, clock) {
                        crate::warn!("the group's governor: the host's clock: {e}");
                    }
                    self.set_group_held(false);
                    counted = charged();
                    last = self.used(counted);
                    seen = None;
                    Duration::from_micros(u64::from(budget.period.get()))
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
            let governor = s.spawn(|| hold.govern(&|| {}, &|| Charged::ToThread));
            for index in 0..threads {
                let stop = &stop;
                s.spawn(move || {
                    hold.register(index, None).unwrap();
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
            let (t0, base) = (Instant::now(), hold.used(Charged::ToThread));
            std::thread::sleep(window);
            let used = hold.used(Charged::ToThread) - base;
            let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
            stop.store(true, Ordering::Relaxed);
            hold.end();
            governor.join().unwrap();
            (used, wall)
        })
    }

    /// A vCPU its hypervisor runs apart from its thread (WHP's classic and core schedulers,
    /// D265) is held by its guest time: its thread spends none of its CPU, its guest clock
    /// a whole CPU's, and a quarter CPU's hold holds it within a few periods. Counted by its
    /// thread alone, as the hold did before (hex-winvm: 88% of a quarter CPU's wall), it is
    /// never held.
    #[test]
    fn a_vcpu_run_apart_from_its_thread_is_held_by_its_guest_time() {
        for (charged, held) in [(Charged::Apart, true), (Charged::ToThread, false)] {
            let hold = Hold::new(1);
            let rated = hold.set(250).unwrap();
            let periods = 8;
            let guest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let (registered, done) = (AtomicBool::new(false), AtomicBool::new(false));
            let mut seen = false;
            std::thread::scope(|s| {
                // The vCPU's thread, which waits as its hypervisor runs its guest.
                let theirs = guest.clone();
                let (hold, registered, done) = (&hold, &registered, &done);
                s.spawn(move || {
                    let read: GuestRead = Box::new(move || Some(theirs.load(Ordering::SeqCst)));
                    hold.register(0, Some(read)).unwrap();
                    registered.store(true, Ordering::SeqCst);
                    while !done.load(Ordering::SeqCst) {
                        std::thread::sleep(rated.period);
                    }
                });
                while !registered.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                let governor = s.spawn(|| hold.govern(&|| {}, &|| charged));
                let began = Instant::now();
                while began.elapsed() < rated.period * periods && !seen {
                    // The guest runs a whole CPU: its time the wall's.
                    guest.store(
                        u64::try_from(began.elapsed().as_nanos()).unwrap(),
                        Ordering::SeqCst,
                    );
                    seen = hold.held.load(Ordering::Acquire);
                    std::thread::sleep(SOONEST);
                }
                done.store(true, Ordering::SeqCst);
                hold.end();
                governor.join().unwrap();
            });
            assert_eq!(seen, held, "{charged:?}: held within {periods} periods");
        }
    }

    /// A vCPU its governor sees only in steps past its share, half its period at a time, as
    /// on hex-winvm each release ran past a quarter CPU's share before it was held again (its
    /// guest time seen in ~10 ms steps and its governor waking late, PM M601), overruns its
    /// share each time it is let go: what it overran holds it through the periods after,
    /// across their ends, so that over many periods it runs its share. Let go at each
    /// period's end whatever it owed, it ran a step every period (hex-winvm: 38.8% of the
    /// wall at a quarter CPU).
    #[test]
    fn a_vcpu_seen_late_is_held_across_periods_by_what_it_overran() {
        let hold = Hold::new(1);
        let rated = hold.set(250).unwrap();
        let period = rated.period;
        let periods = 32;
        let guest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (registered, done) = (AtomicBool::new(false), AtomicBool::new(false));
        let (ran, wall) = std::thread::scope(|s| {
            let theirs = guest.clone();
            let (hold, registered, done) = (&hold, &registered, &done);
            // The vCPU: a run of half a period at a time, its guest time told once it is over.
            s.spawn(move || {
                let told = theirs.clone();
                let read: GuestRead = Box::new(move || Some(told.load(Ordering::SeqCst)));
                hold.register(0, Some(read)).unwrap();
                registered.store(true, Ordering::SeqCst);
                while !done.load(Ordering::SeqCst) {
                    hold.wait(&|| done.load(Ordering::SeqCst));
                    let run = Instant::now();
                    std::thread::sleep(period / 2);
                    let ns = u64::try_from(run.elapsed().as_nanos()).unwrap();
                    theirs.fetch_add(ns, Ordering::SeqCst);
                }
            });
            while !registered.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let governor = s.spawn(|| hold.govern(&|| {}, &|| Charged::Apart));
            let (t0, g0) = (Instant::now(), guest.load(Ordering::SeqCst));
            std::thread::sleep(period * periods);
            let ran = guest.load(Ordering::SeqCst) - g0;
            let wall = u64::try_from(t0.elapsed().as_nanos()).unwrap();
            done.store(true, Ordering::SeqCst);
            hold.end();
            governor.join().unwrap();
            (ran, wall)
        });
        let period_ns = u64::try_from(period.as_nanos()).unwrap();
        // A quarter of the wall, two of its periods where the window and the periods do not
        // line up, and one run, the step its guest time is seen in.
        let most = wall / 4 + 2 * period_ns + period_ns / 2;
        assert!(
            ran <= most,
            "{ran} ns run in {wall} ns at a quarter CPU, over {period_ns} ns periods"
        );
        assert!(
            ran >= wall / 20,
            "{ran} ns run in {wall} ns: held, and never let go"
        );
    }

    /// A rate's share of a period is that many thousandths of it, exact to the nanosecond
    /// below (D247): a whole CPU's, the period itself, as D191's whole CPUs had; a CPU and
    /// a half's, a period and a half; the least rate's, a thousandth; past what a u64 holds,
    /// the most it holds.
    #[test]
    fn a_rates_share_is_its_thousandths_of_the_period() {
        let period = 100_000_000;
        let milli = |m| NonZeroU64::new(m).unwrap();
        assert_eq!(share_of(milli(MILLI_PER_CPU), period), period);
        assert_eq!(share_of(milli(3 * MILLI_PER_CPU), period), 3 * period);
        assert_eq!(share_of(milli(1500), period), period + period / 2);
        assert_eq!(share_of(milli(1), period), period / MILLI_PER_CPU);
        assert_eq!(share_of(milli(1), MILLI_PER_CPU - 1), 0);
        assert_eq!(share_of(milli(u64::MAX), u64::MAX), u64::MAX);
    }

    /// A hold holds its vCPUs to a rate below every vCPU's whole CPU, and lets them be at
    /// none and at every vCPU's whole CPU or more (D247), as D191's whole CPUs did.
    #[test]
    fn a_hold_holds_a_rate_below_its_vcpus_alone() {
        let hold = Hold::new(2);
        let held = |milli| hold.set(milli).unwrap().milli;
        assert_eq!(held(0), None);
        assert_eq!(held(1), NonZeroU64::new(1));
        assert_eq!(held(1500), NonZeroU64::new(1500));
        assert_eq!(
            held(2 * MILLI_PER_CPU - 1),
            NonZeroU64::new(2 * MILLI_PER_CPU - 1)
        );
        assert_eq!(held(2 * MILLI_PER_CPU), None);
        assert_eq!(held(u64::MAX), None);
    }

    /// D191's period in force (migrate-09): each limit is held over the period derived from
    /// g as it is set, vcpus × g over its share, never a fixed one. Four vCPUs at 625
    /// milli-CPU by GetThreadTimes' 15.625 ms ticks (PM M294) over 100 ms, at 500 over 125;
    /// let be, over g itself; one whose period outgrows a u64 of ns refused, the hold as it
    /// was. At this process's g, as it knows it as the limit is set.
    #[test]
    fn a_limit_is_held_over_the_period_derived_from_g() {
        let tick = Duration::from_micros(15_625);
        let hold = Hold::new(4);
        let rated = |milli, period| {
            Ok(Rated {
                milli: NonZeroU64::new(milli),
                period,
            })
        };
        assert_eq!(hold.set_at(625, tick), rated(625, Duration::from_millis(100)));
        assert_eq!(lock(&hold.state).period, Duration::from_millis(100));
        assert_eq!(hold.set_at(500, tick), rated(500, Duration::from_millis(125)));
        let fine = Duration::from_micros(1_050);
        assert_eq!(hold.set_at(2000, fine), rated(2000, Duration::from_micros(2_100)));
        assert_eq!(hold.set_at(0, fine), rated(0, fine));
        assert_eq!(hold.set_at(4 * MILLI_PER_CPU, fine), rated(0, fine));
        let generation = lock(&hold.state).generation;
        assert!(hold.set_at(1, Duration::from_secs(u64::MAX)).is_err());
        let s = lock(&hold.state);
        assert_eq!(
            (s.generation, s.milli, s.period),
            (generation, 4 * MILLI_PER_CPU, fine)
        );
        drop(s);
        // Other tests' governors teach the process's g as this runs: it only falls.
        let before = granule::g();
        let period = hold.set(MILLI_PER_CPU).unwrap().period;
        let after = granule::g();
        assert!(
            (4 * after..=4 * before).contains(&period),
            "{period:?}, g {before:?} then {after:?}"
        );
        assert!(after >= SOONEST);
    }

    /// The governor holds each period over the limit's own period: a thread held to half a
    /// CPU at a g of 5 ms, a period of 10, is held to that period's end, never to a period
    /// of another's.
    #[test]
    fn the_governor_holds_over_the_limits_period() {
        /// Lets the thread go however the test ends.
        struct Ends<'a>(&'a Hold, &'a AtomicBool);
        impl Drop for Ends<'_> {
            fn drop(&mut self) {
                self.1.store(true, Ordering::Relaxed);
                self.0.end();
            }
        }
        let hold = Hold::new(1);
        let period = Duration::from_millis(10);
        assert_eq!(hold.set_at(500, Duration::from_millis(5)).unwrap().period, period);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let ends = Ends(&hold, &stop);
            let governor = s.spawn(|| hold.govern(&|| {}, &|| Charged::ToThread));
            s.spawn(|| {
                hold.register(0, None).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    hold.wait(&|| stop.load(Ordering::Relaxed));
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_micros(200) {
                        std::hint::spin_loop();
                    }
                }
            });
            let t = Instant::now();
            let until = loop {
                if let Some(until) = lock(&hold.state).held {
                    break until;
                }
                assert!(t.elapsed() < Duration::from_secs(5), "never held");
                std::hint::spin_loop();
            };
            let left = until.saturating_duration_since(Instant::now());
            assert!(left <= period, "held {left:?} more, its period {period:?}");
            drop(ends);
            governor.join().unwrap();
        });
    }

    /// Three threads held to one CPU use no more of it than the time passing, but what a
    /// period may overrun, and are let go each period: they run a share of it.
    #[test]
    fn threads_held_use_no_more_than_their_share() {
        let hold = Hold::new(3);
        hold.set(MILLI_PER_CPU).unwrap();
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

    /// A budget of `quota` µs each 100 ms.
    fn budget(quota: Option<u32>) -> super::super::group::Budget {
        super::super::group::Budget {
            quota: quota.and_then(std::num::NonZeroU32::new),
            period: std::num::NonZeroU32::new(100_000).unwrap(),
        }
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
                governors.push(s.spawn(move || hold.govern_group(group, &|| {}, &|| Charged::ToThread)));
                for index in 0..hold.clocks.len() {
                    let stop = &stop;
                    s.spawn(move || {
                        hold.register(index, None).unwrap();
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
            let all = || holds.iter().map(|h| h.used(Charged::ToThread)).sum::<u64>();
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
            .set_budget(budget(Some(100_000)));
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
        daemon.set_budget(budget(Some(1_000)));
        let group = GroupCpu::map(file).unwrap();
        let hold = Hold::new(1);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            let ends = Ends(&hold, &stop);
            let governor = s.spawn(|| hold.govern_group(&group, &|| {}, &|| Charged::ToThread));
            s.spawn(|| {
                hold.register(0, None).unwrap();
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
            daemon.set_budget(budget(None));
            let t = Instant::now();
            while held() {
                assert!(
                    t.elapsed() < Duration::from_secs(1),
                    "still held a second after its group's budget went"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            // Held no more: it runs.
            let base = hold.used(Charged::ToThread);
            std::thread::sleep(Duration::from_millis(200));
            assert!(!held());
            assert!(hold.used(Charged::ToThread) > base, "let go, it did not run");
            drop(ends);
            governor.join().unwrap();
        });
    }

    /// A hold of as many CPUs as there are vCPUs holds nothing, and its governor waits.
    #[test]
    fn a_hold_of_every_cpu_holds_nothing() {
        let hold = Hold::new(2);
        assert_eq!(hold.set(2 * MILLI_PER_CPU).unwrap().milli, None);
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
                    hold.register(index, None).unwrap();
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
            let base = hold.used(Charged::ToThread);
            std::thread::sleep(Duration::from_millis(200));
            let used = hold.used(Charged::ToThread) - base;
            assert!(used < 10_000_000, "a paused vCPU ran: {used} ns of CPU in 200 ms");
            hold.resume();
            let base = hold.used(Charged::ToThread);
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                hold.used(Charged::ToThread) > base,
                "the machine went on, its vCPUs did not"
            );
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
