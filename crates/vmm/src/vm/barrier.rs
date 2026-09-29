//! The snapshot barrier. A snapshot is a cut of the whole machine, so it goes in phases
//! (audit A02): every vCPU leaves the guest and parks; the coordinator quiesces the
//! devices; then each vCPU captures its own state, on its own thread (Hypervisor.framework
//! requires that), and only then does the coordinator save the interrupt controller, the
//! devices and memory, and release the vCPUs. No CPU's state is taken while another CPU
//! runs or a device can still complete a request or raise an interrupt.
//!
//! Every wait also ends when the machine is stopping, so a stop or an error at any phase
//! releases every thread.

use std::sync::{Condvar, Mutex};

use crate::sync::{lock, wait};

#[derive(Debug)]
pub(super) struct Barrier<C> {
    state: Mutex<State<C>>,
    changed: Condvar,
}

#[derive(Debug)]
struct State<C> {
    requested: bool,
    /// Per vCPU: out of the guest, parked.
    parked: Vec<bool>,
    /// Set once the devices are quiet: parked vCPUs capture their state.
    capture: bool,
    /// Per vCPU, once it has captured: its state, or why it could not.
    captured: Vec<Option<Result<C, String>>>,
    /// Bumped to release parked vCPUs.
    epoch: u64,
}

impl<C> Barrier<C> {
    pub(super) fn new(vcpus: usize) -> Barrier<C> {
        Barrier {
            state: Mutex::new(State {
                requested: false,
                parked: vec![false; vcpus],
                capture: false,
                captured: (0..vcpus).map(|_| None).collect(),
                epoch: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// The guest asked for a snapshot. False if one is under way already.
    pub(super) fn request(&self) -> bool {
        let mut s = lock(&self.state);
        if s.requested {
            return false;
        }
        s.requested = true;
        drop(s);
        self.changed.notify_all();
        true
    }

    /// On vCPU `index`'s thread, out of the guest: if a snapshot is requested, parks, runs
    /// `capture` once the devices are quiet, and waits until the snapshot is written.
    /// Returns at once while none is requested, and whenever `stopping`.
    pub(super) fn park(
        &self,
        index: usize,
        stopping: &dyn Fn() -> bool,
        capture: impl FnOnce() -> Result<C, String>,
    ) {
        let mut s = lock(&self.state);
        if !s.requested {
            return;
        }
        let epoch = s.epoch;
        if let Some(parked) = s.parked.get_mut(index) {
            *parked = true;
        }
        self.changed.notify_all();
        while !s.capture && s.epoch == epoch && !stopping() {
            s = wait(&self.changed, s);
        }
        if s.capture && s.epoch == epoch && !stopping() {
            drop(s);
            let captured = capture();
            s = lock(&self.state);
            if let Some(slot) = s.captured.get_mut(index) {
                *slot = Some(captured);
            }
            self.changed.notify_all();
        }
        while s.epoch == epoch && !stopping() {
            s = wait(&self.changed, s);
        }
    }

    /// On the coordinator: waits until a snapshot is requested and every vCPU has parked.
    /// False once `stopping`.
    pub(super) fn wait_parked(&self, stopping: &dyn Fn() -> bool) -> bool {
        let mut s = lock(&self.state);
        loop {
            if stopping() {
                return false;
            }
            if s.requested && s.parked.iter().all(|&p| p) {
                return true;
            }
            s = wait(&self.changed, s);
        }
    }

    /// On the coordinator, once the devices are quiet: lets the parked vCPUs capture their
    /// state, and waits for all of it, in vCPU order, or the first vCPU's error. None once
    /// `stopping`.
    pub(super) fn capture(&self, stopping: &dyn Fn() -> bool) -> Option<Result<Vec<C>, String>> {
        let mut s = lock(&self.state);
        s.capture = true;
        self.changed.notify_all();
        loop {
            if stopping() {
                return None;
            }
            if s.captured.iter().all(Option::is_some) {
                return Some(s.captured.iter_mut().filter_map(Option::take).collect());
            }
            s = wait(&self.changed, s);
        }
    }

    /// On the coordinator: lets the vCPUs run again, and readies the barrier for the next
    /// snapshot.
    pub(super) fn release(&self) {
        let mut s = lock(&self.state);
        s.requested = false;
        s.capture = false;
        s.parked.iter_mut().for_each(|p| *p = false);
        s.captured.iter_mut().for_each(|c| *c = None);
        s.epoch = s.epoch.wrapping_add(1);
        drop(s);
        self.changed.notify_all();
    }

    /// Wakes every waiter to look at `stopping` again. Taking the lock first orders the
    /// wakeup after any waiter's last look.
    pub(super) fn wake(&self) {
        drop(lock(&self.state));
        self.changed.notify_all();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Parked(usize),
        DevicesQuiet,
        Captured(usize),
        Written,
        Resumed(usize),
    }

    /// A pseudo-random delay from `seed`, up to about a millisecond.
    fn delay(seed: &mut u64) {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        std::thread::sleep(Duration::from_micros(*seed % 1000));
    }

    /// Whatever each thread is delayed by at each step, every vCPU parks before the
    /// devices go quiet, every capture follows that, and the snapshot is written only
    /// after every capture and before any vCPU runs again (audit A02).
    #[test]
    fn phases_keep_their_order_whatever_the_delays() {
        for round in 0..20u64 {
            let vcpus = 1 + (round % 8) as usize;
            let barrier = Barrier::<usize>::new(vcpus);
            let log = Mutex::new(Vec::new());
            let never = || false;
            // The guest's request comes first; the kick that follows it parks the vCPUs.
            assert!(barrier.request());
            assert!(!barrier.request(), "one snapshot at a time");
            std::thread::scope(|s| {
                for index in 0..vcpus {
                    let (barrier, log) = (&barrier, &log);
                    s.spawn(move || {
                        let mut seed = round * 64 + index as u64 + 1;
                        delay(&mut seed);
                        lock(log).push(Event::Parked(index));
                        barrier.park(index, &never, || {
                            delay(&mut seed);
                            lock(log).push(Event::Captured(index));
                            Ok(index)
                        });
                        lock(log).push(Event::Resumed(index));
                    });
                }
                assert!(barrier.wait_parked(&never));
                let mut seed = round + 7;
                delay(&mut seed);
                lock(&log).push(Event::DevicesQuiet);
                let captured = barrier.capture(&never).unwrap().unwrap();
                assert_eq!(captured, (0..vcpus).collect::<Vec<_>>());
                delay(&mut seed);
                lock(&log).push(Event::Written);
                barrier.release();
            });
            let log = log.into_inner().unwrap();
            let at = |e: Event| log.iter().position(|&x| x == e).unwrap();
            for i in 0..vcpus {
                assert!(at(Event::Parked(i)) < at(Event::DevicesQuiet), "{log:?}");
                assert!(at(Event::DevicesQuiet) < at(Event::Captured(i)), "{log:?}");
                assert!(at(Event::Captured(i)) < at(Event::Written), "{log:?}");
                assert!(at(Event::Written) < at(Event::Resumed(i)), "{log:?}");
            }
        }
    }

    /// A vCPU that could not capture its state fails the snapshot, and the barrier takes
    /// the next one.
    #[test]
    fn a_capture_error_reaches_the_coordinator() {
        let barrier = Barrier::<u8>::new(2);
        let never = || false;
        assert!(barrier.request());
        std::thread::scope(|s| {
            for index in 0..2 {
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.park(index, &never, || {
                        if index == 1 { Err("no state".into()) } else { Ok(0) }
                    });
                });
            }
            assert!(barrier.wait_parked(&never));
            assert_eq!(barrier.capture(&never).unwrap(), Err("no state".into()));
            barrier.release();
        });
        // Nothing requested: a vCPU passes straight through.
        barrier.park(0, &never, || panic!("captured without a request"));
    }

    /// A stop at any phase releases every thread, and nothing is captured after it.
    #[test]
    fn a_stop_at_any_phase_releases_everyone() {
        for phase in 0..3 {
            let barrier = Barrier::<u8>::new(3);
            let stop = AtomicBool::new(false);
            let stopping = || stop.load(Ordering::Acquire);
            assert!(barrier.request());
            std::thread::scope(|s| {
                for index in 0..3 {
                    let (barrier, stopping) = (&barrier, &stopping);
                    // One vCPU never parks when the stop comes before they all have.
                    if phase == 0 && index == 2 {
                        continue;
                    }
                    s.spawn(move || barrier.park(index, stopping, || Ok(0)));
                }
                let stop_now = || {
                    stop.store(true, Ordering::Release);
                    barrier.wake();
                };
                match phase {
                    0 => {
                        std::thread::sleep(Duration::from_millis(5));
                        stop_now();
                        assert!(!barrier.wait_parked(&stopping));
                    }
                    1 => {
                        assert!(barrier.wait_parked(&stopping));
                        stop_now();
                        assert!(barrier.capture(&stopping).is_none());
                    }
                    _ => {
                        assert!(barrier.wait_parked(&stopping));
                        assert!(barrier.capture(&stopping).unwrap().is_ok());
                        stop_now();
                    }
                }
            });
        }
    }
}
