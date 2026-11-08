//! How precise the clocks and waits are that the vCPU hold's governor uses on Windows
//! (crates/vmm/src/vm/hold.rs, D191 slice 2), and how much CPU three spinning threads,
//! as vCPUs, use in a second held to one CPU by a copy of its governor's loop: governed by
//! GetThreadTimes (`platform::ThreadClock`), at the governor's priority as the VMM had it
//! (THREAD_PRIORITY_NORMAL) and raised (THREAD_PRIORITY_TIME_CRITICAL). What they truly
//! used is read from their cycles at the TSC's rate, measured against `Instant`.
//!
//!     cargo run --release -- [CPUS]   (CPUS: this process held to its first CPUS CPUs,
//!                                      as a small host's; none, every one)
//!
//! One line a measurement.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetThreadTimes, OpenThread, SetProcessAffinityMask,
    SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, THREAD_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

/// A thread's clocks, as the VMM's ThreadClock opens it.
struct Clock(HANDLE);
// SAFETY: a query-only thread handle, read from any thread.
unsafe impl Send for Clock {}
unsafe impl Sync for Clock {}
impl Clock {
    fn current() -> Clock {
        // SAFETY: the calling thread, opened to query it.
        Clock(unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) })
    }
    /// Its kernel and user times, ns (GetThreadTimes).
    fn times_ns(&self) -> u64 {
        let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
        // SAFETY: a handle this value owns, and four times to write.
        unsafe { GetThreadTimes(self.0, &mut c, &mut e, &mut k, &mut u) };
        let ns = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) * 100;
        ns(k) + ns(u)
    }
    /// Its cycles (QueryThreadCycleTime).
    fn cycles(&self) -> u64 {
        let mut c = 0u64;
        // SAFETY: as above.
        unsafe { QueryThreadCycleTime(self.0, &mut c) };
        c
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        // SAFETY: the handle this value opened.
        unsafe { CloseHandle(self.0) };
    }
}

fn tsc() -> u64 {
    // SAFETY: RDTSC reads the time-stamp counter.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// TSC ticks per ns, against `Instant`, over `over`.
fn tsc_per_ns(over: Duration) -> f64 {
    let (t0, c0) = (Instant::now(), tsc());
    while t0.elapsed() < over {}
    (tsc() - c0) as f64 / t0.elapsed().as_nanos() as f64
}

/// How often a spinning thread's GetThreadTimes and QueryThreadCycleTime change in a second.
fn granularity(label: &str) {
    let (steps, median, cycles) = std::thread::scope(|s| {
        s.spawn(|| {
            let clock = Clock::current();
            let (mut last_t, mut last_c) = (clock.times_ns(), clock.cycles());
            let (mut steps, mut cycles) = (Vec::new(), 0usize);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(1) {
                let (t, c) = (clock.times_ns(), clock.cycles());
                if t != last_t {
                    steps.push(t - last_t);
                    last_t = t;
                }
                if c != last_c {
                    cycles += 1;
                    last_c = c;
                }
            }
            steps.sort();
            (steps.len(), steps.get(steps.len() / 2).copied().unwrap_or(0), cycles)
        })
        .join()
        .unwrap()
    });
    println!("{label}: a spinning thread's GetThreadTimes changed {steps} times in 1 s, median step {:.3} ms; its QueryThreadCycleTime {cycles} times", median as f64 / 1e6);
}

/// What 50 waits of 1 ms on a condition variable took, as the governor waits.
fn waits(label: &str) {
    let m = Mutex::new(());
    let cv = Condvar::new();
    let mut d: Vec<f64> = Vec::new();
    for _ in 0..50 {
        let g = m.lock().unwrap();
        let t = Instant::now();
        let _ = cv.wait_timeout(g, Duration::from_millis(1)).unwrap();
        d.push(t.elapsed().as_secs_f64() * 1e3);
    }
    d.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{label}: Condvar::wait_timeout(1 ms) took p50 {:.2} ms, p90 {:.2}, max {:.2}", d[25], d[45], d[49]);
}

/// `threads` spinning in slices of 200 µs, looking at the hold before each, as hold.rs's
/// test runs them, held to `cpus` CPUs by hold.rs's governor loop over `read`'s clock for
/// `window`, the governor at TIME_CRITICAL where `critical`: what they truly used, ns, and
/// the window's wall time.
fn hold(read: &(dyn Fn(&Clock) -> u64 + Sync), per_ns: f64, threads: usize, cpus: u64, window: Duration, critical: bool) -> (u64, u64) {
    const PERIOD: Duration = Duration::from_millis(100);
    const BUSY: Duration = Duration::from_millis(10);
    const SOONEST: Duration = Duration::from_millis(1);
    let held = AtomicBool::new(false);
    let ended = AtomicBool::new(false);
    let lock = Mutex::new(());
    let released = Condvar::new();
    let changed = Condvar::new();
    let clocks: Vec<Mutex<Option<Clock>>> = (0..threads).map(|_| Mutex::new(None)).collect();
    let ready = AtomicU64::new(0);
    let used = || clocks.iter().filter_map(|c| c.lock().unwrap().as_ref().map(|c| read(c))).sum::<u64>();
    let truth = || clocks.iter().filter_map(|c| c.lock().unwrap().as_ref().map(|c| (c.cycles() as f64 / per_ns) as u64)).sum::<u64>();
    std::thread::scope(|s| {
        for i in 0..threads {
            let (clocks, held, ended, lock, released, ready) = (&clocks, &held, &ended, &lock, &released, &ready);
            s.spawn(move || {
                *clocks[i].lock().unwrap() = Some(Clock::current());
                ready.fetch_add(1, Ordering::SeqCst);
                while !ended.load(Ordering::Relaxed) {
                    if held.load(Ordering::Acquire) {
                        let mut g = lock.lock().unwrap();
                        while held.load(Ordering::Acquire) && !ended.load(Ordering::Relaxed) {
                            g = released.wait(g).unwrap();
                        }
                    }
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_micros(200) {
                        std::hint::spin_loop();
                    }
                }
            });
        }
        while ready.load(Ordering::SeqCst) < threads as u64 {
            std::thread::yield_now();
        }
        let governor = s.spawn(|| {
            if critical {
                // SAFETY: the calling thread's pseudo-handle.
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
            }
            let vcpus = threads as u64;
            let period = PERIOD.as_nanos() as u64;
            let (mut debt, mut busy, mut start) = (0u64, true, Instant::now());
            while !ended.load(Ordering::Relaxed) {
                if Instant::now().saturating_duration_since(start) >= PERIOD {
                    start = Instant::now();
                }
                let base = used();
                let share = cpus * period;
                let allowed = share.saturating_sub(debt);
                let mut holding = false;
                while let Some(left) = PERIOD.checked_sub(start.elapsed()).filter(|l| !l.is_zero()) {
                    let u = used().saturating_sub(base);
                    if !holding && u >= allowed {
                        holding = true;
                        held.store(true, Ordering::Release);
                    }
                    let next = if holding || !busy {
                        left
                    } else {
                        Duration::from_nanos(allowed.saturating_sub(u) / vcpus).clamp(SOONEST, BUSY).min(left)
                    };
                    let g = lock.lock().unwrap();
                    let _ = changed.wait_timeout(g, next).unwrap();
                    if ended.load(Ordering::Relaxed) {
                        return;
                    }
                }
                let u = used().saturating_sub(base);
                debt = debt.saturating_add(u).saturating_sub(share);
                busy = holding || u * 2 >= share;
                start += PERIOD;
                held.store(false, Ordering::Release);
                drop(lock.lock().unwrap());
                released.notify_all();
            }
        });
        let (t0, b) = (Instant::now(), truth());
        std::thread::sleep(window);
        let u = truth() - b;
        let wall = t0.elapsed().as_nanos() as u64;
        ended.store(true, Ordering::Relaxed);
        held.store(false, Ordering::Release);
        drop(lock.lock().unwrap());
        released.notify_all();
        changed.notify_all();
        let _ = governor.join();
        (u, wall)
    })
}

fn main() {
    let per_ns = tsc_per_ns(Duration::from_millis(200));
    let held_to: u32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(0);
    if held_to > 0 {
        // SAFETY: this process's pseudo-handle, and a mask of its first CPUs.
        unsafe { SetProcessAffinityMask(GetCurrentProcess(), (1usize << held_to) - 1) };
    }
    println!(
        "{} CPUs, this process held to {}; the TSC {:.4} ticks a ns",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        if held_to > 0 { held_to.to_string() } else { "none".into() },
        per_ns
    );
    granularity("the default timer resolution");
    waits("the default timer resolution");
    // SAFETY: a matched pair, around the measurements it is for.
    unsafe { timeBeginPeriod(1) };
    granularity("timeBeginPeriod(1)");
    waits("timeBeginPeriod(1)");
    // SAFETY: as above.
    unsafe { timeEndPeriod(1) };
    let times = |c: &Clock| c.times_ns();
    for round in 0..10 {
        for critical in [false, true] {
            let (u, w) = hold(&times, per_ns, 3, 1, Duration::from_secs(1), critical);
            println!(
                "round {round}, the governor {}: three spinning threads held to one CPU used {:.3} s in {:.3} s",
                if critical { "THREAD_PRIORITY_TIME_CRITICAL" } else { "THREAD_PRIORITY_NORMAL" },
                u as f64 / 1e9,
                w as f64 / 1e9
            );
        }
    }
}
