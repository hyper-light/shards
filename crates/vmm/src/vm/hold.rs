//! The host's hold on a machine's vCPUs (D191): together they use at most the CPU time
//! their run's limit gives them each period, whatever their guest does with its own
//! cgroups, as CFS bandwidth control holds a cgroup's threads
//! (Documentation/scheduler/sched-bwc.rst). A governor reads each vCPU thread's CPU time;
//! once a period's share is spent, every vCPU is held out of its guest until the period
//! ends, and what a period overran comes off the next ones.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    /// `State::held`, for a vCPU's look before each entry to its guest without the lock.
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
    ended: bool,
}

impl Hold {
    pub(super) fn new(vcpus: usize) -> Hold {
        Hold {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            released: Condvar::new(),
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
        while s.held && !stop() {
            s = wait(&self.released, s);
        }
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
        self.held.store(false, Ordering::Release);
        drop(s);
        self.changed.notify_all();
        self.released.notify_all();
    }

    fn set_held(&self, held: bool) {
        let mut s = lock(&self.state);
        s.held = held && !s.ended;
        self.held.store(s.held, Ordering::Release);
        drop(s);
        if !held {
            self.released.notify_all();
        }
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
        // Its waits end on time, as a vCPU's timers do (PM M8, M10): one that ends late
        // holds the vCPUs past their period.
        if let Err(e) = crate::platform::prioritize_vcpu_thread() {
            crate::debug!("the hold's governor runs without real-time policy: {e}");
        }
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
