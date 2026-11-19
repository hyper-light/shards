//! The priority governor of crates/vmm (D191, platform/windows_thread.rs) on this host's own
//! scheduler, sampled: the scene of its test `raised_threads_sharing_a_cpu_take_no_more_than_half_of_it`,
//! three spinners raised by `prioritize_vcpu_thread` (their boost off, as the test sets it) and
//! a normal spinner, all four pinned to the process's highest CPU, for SECONDS. This thread,
//! pinned to the lowest at THREAD_PRIORITY_TIME_CRITICAL, reads each one's priority, its
//! cycles (QueryThreadCycleTime) and its GetThreadTimes every millisecond, and puts each
//! millisecond's cycles to the priority its thread was read at as the millisecond began. Each
//! governed thread also reads its own GetThreadTimes from its raise to its end, as the test
//! does, against the test's bound: half of the time, and two of the governor's periods
//! (31.25 ms at GetThreadTimes' 15.625 ms steps). LOAD normal spinners, unpinned, stand in for
//! the tests CI runs beside it.
//!
//!     cargo run --release -- [ROUNDS] [SECONDS] [LOAD]      (5, 3, 0)
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetProcessAffinityMask, GetThreadPriority, GetThreadTimes,
    OpenThread, SetThreadAffinityMask, SetThreadPriority, SetThreadPriorityBoost, THREAD_PRIORITY_TIME_CRITICAL,
    THREAD_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

const GOVERNED: usize = 3;
/// THREAD_PRIORITY_ABOVE_NORMAL and THREAD_PRIORITY_BELOW_NORMAL.
const RAISED: i32 = 1;
const LOWERED: i32 = -1;
/// The test's slack: two of the governor's periods at GetThreadTimes' step.
const SLACK: Duration = Duration::from_micros(62_500);

fn pin(mask: usize) {
    // SAFETY: the calling thread's pseudo-handle.
    assert_ne!(unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) }, 0);
}

/// A handle of the calling thread, to read it from another.
fn own() -> usize {
    // SAFETY: the calling thread, opened to query it.
    let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, GetCurrentThreadId()) };
    assert!(!h.is_null());
    h as usize
}

/// GetThreadTimes' kernel and user time, ns.
fn times(h: HANDLE) -> u64 {
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
    // SAFETY: a thread handle with query access, and four times to write.
    assert_ne!(unsafe { GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u) }, 0);
    let ns = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) * 100;
    ns(k) + ns(u)
}

fn cycles(h: HANDLE) -> u64 {
    let mut c = 0u64;
    // SAFETY: a thread handle with query access, and a count to write.
    assert_ne!(unsafe { QueryThreadCycleTime(h, &mut c) }, 0);
    c
}

fn priority(h: HANDLE) -> i32 {
    // SAFETY: a thread handle with query access.
    unsafe { GetThreadPriority(h) }
}

struct Sample {
    prio: [i32; GOVERNED + 1],
    cycles: [u64; GOVERNED + 1],
}

struct Round {
    elapsed: Duration,
    /// The time stamp counter's ticks over the window, which x86-64's Windows counts a
    /// thread's cycles in (QueryThreadCycleTime): the CPU's whole time, in cycles.
    stamps: u64,
    /// Each governed thread's own GetThreadTimes from its raise to its end, as the test reads.
    ran: Duration,
    /// And its own cycles over the same.
    ran_cycles: u64,
    normal_ran: Duration,
    samples: Vec<Sample>,
}

fn round(seconds: u64, load: usize, x: usize) -> Round {
    let spinning = AtomicBool::new(true);
    std::thread::scope(|s| {
        let spinning = &spinning;
        let (tx, rx) = mpsc::channel();
        let normal = s.spawn(move || {
            pin(x);
            let h = own();
            tx.send(h).unwrap();
            let from = times(h as HANDLE);
            while spinning.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
            let ran = times(h as HANDLE) - from;
            // SAFETY: the handle `own` opened, closed once.
            unsafe { CloseHandle(h as HANDLE) };
            ran
        });
        let normal_handle = rx.recv().unwrap();
        for _ in 0..load {
            s.spawn(move || {
                while spinning.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
        }
        let began = Instant::now();
        // SAFETY: RDTSC reads the counter, as every x86-64 CPU has it.
        let stamped = unsafe { std::arch::x86_64::_rdtsc() };
        let (gtx, grx) = mpsc::channel();
        let governed: Vec<_> = (0..GOVERNED)
            .map(|_| {
                let gtx = gtx.clone();
                s.spawn(move || {
                    // SAFETY: the calling thread's pseudo-handle.
                    assert_ne!(unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) }, 0);
                    pin(x);
                    let raised = shards_vmm::platform::prioritize_vcpu_thread().unwrap();
                    let h = own();
                    gtx.send(h).unwrap();
                    let (from, from_cycles) = (times(h as HANDLE), cycles(h as HANDLE));
                    while spinning.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                    let ran = (times(h as HANDLE) - from, cycles(h as HANDLE) - from_cycles);
                    drop(raised);
                    // SAFETY: the handle `own` opened, closed once.
                    unsafe { CloseHandle(h as HANDLE) };
                    ran
                })
            })
            .collect();
        drop(gtx);
        let mut handles = [normal_handle; GOVERNED + 1];
        for slot in handles.iter_mut().skip(1) {
            *slot = grx.recv().unwrap();
        }
        let window = Duration::from_secs(seconds);
        let mut samples = Vec::with_capacity(usize::try_from(seconds * 1100).unwrap_or(4096));
        while began.elapsed() < window {
            let mut sample = Sample { prio: [0; GOVERNED + 1], cycles: [0; GOVERNED + 1] };
            for (k, h) in handles.iter().enumerate() {
                sample.prio[k] = priority(*h as HANDLE);
                sample.cycles[k] = cycles(*h as HANDLE);
            }
            samples.push(sample);
            std::thread::sleep(Duration::from_millis(1));
        }
        spinning.store(false, Ordering::Relaxed);
        let (ran, ran_cycles) = governed
            .into_iter()
            .map(|t| t.join().unwrap())
            .fold((0, 0), |(t, c), (dt, dc)| (t + dt, c + dc));
        let elapsed = began.elapsed();
        // SAFETY: as above.
        let stamps = unsafe { std::arch::x86_64::_rdtsc() } - stamped;
        Round {
            elapsed,
            stamps,
            ran: Duration::from_nanos(ran),
            ran_cycles,
            normal_ran: Duration::from_nanos(normal.join().unwrap()),
            samples,
        }
    })
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().unwrap()).collect();
    let rounds = args.first().copied().unwrap_or(5);
    let seconds = args.get(1).copied().unwrap_or(3);
    let load = usize::try_from(args.get(2).copied().unwrap_or(0)).unwrap();
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: this process's pseudo-handle, and two masks to write.
    assert_ne!(unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) }, 0);
    assert!(process.count_ones() > 1, "one CPU: the sampler needs one of its own");
    let x = 1usize << (usize::BITS - 1 - process.leading_zeros());
    let low = process & process.wrapping_neg();
    pin(low);
    // SAFETY: the calling thread's pseudo-handle.
    assert_ne!(unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }, 0);
    println!("CPUs {process:#x}, governed on {x:#x}, sampler on {low:#x}; {rounds} rounds of {seconds} s, {load} unpinned normal spinners beside");
    println!(
        "| round | three's GetThreadTimes / elapsed | test's bound | normal's | three's cycles: share | raised | lowered | normal | three's whole cycles, of the window's ticks | samples: any raised | all lowered |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for r in 1..=rounds {
        let round = round(seconds, load, x);
        let bound = round.elapsed / 2 + SLACK;
        let (mut raised, mut lowered, mut normal_level, mut normal_thread) = (0u64, 0u64, 0u64, 0u64);
        let (mut any_raised, mut all_lowered) = (0usize, 0usize);
        let mut line = String::new();
        for pair in round.samples.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            normal_thread += b.cycles[0] - a.cycles[0];
            let mut most = (b.cycles[0] - a.cycles[0], '0');
            for k in 1..=GOVERNED {
                let dc = b.cycles[k] - a.cycles[k];
                match a.prio[k] {
                    RAISED => raised += dc,
                    LOWERED => lowered += dc,
                    _ => normal_level += dc,
                }
                if dc > most.0 {
                    most = (dc, char::from(b'0' + u8::try_from(k).unwrap()));
                }
            }
            let levels = &a.prio[1..];
            if levels.contains(&RAISED) {
                any_raised += 1;
            }
            if levels.iter().all(|p| *p == LOWERED) {
                all_lowered += 1;
            }
            if r == 1 && line.len() < 3 * 400 {
                let tag = if levels.iter().all(|p| *p == RAISED) {
                    'R'
                } else if levels.iter().all(|p| *p == LOWERED) {
                    'L'
                } else if levels.iter().all(|p| *p == 0) {
                    'N'
                } else {
                    'm'
                };
                line.push(tag);
                line.push(if most.0 == 0 { '.' } else { most.1 });
                line.push(' ');
            }
        }
        let three = raised + lowered + normal_level;
        let all = three + normal_thread;
        let pct = |v: u64, of: u64| if of == 0 { 0.0 } else { 100.0 * v as f64 / of as f64 };
        let n = round.samples.len().saturating_sub(1).max(1);
        println!(
            "| {r} | {:.3} s / {:.3} s ({:.1}%) | {:.3} s {} | {:.3} s | {:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.1}% | {:.1}% |",
            round.ran.as_secs_f64(),
            round.elapsed.as_secs_f64(),
            100.0 * round.ran.as_secs_f64() / round.elapsed.as_secs_f64(),
            bound.as_secs_f64(),
            if round.ran <= bound { "kept" } else { "PAST" },
            round.normal_ran.as_secs_f64(),
            pct(three, all),
            pct(raised, all),
            pct(lowered, all),
            pct(normal_level, all),
            pct(round.ran_cycles, round.stamps),
            pct(any_raised as u64, n as u64),
            pct(all_lowered as u64, n as u64),
        );
        if r == 1 {
            println!("\nRound 1's first {} ms: the three's levels (R all raised, L all lowered, m mixed, N normal), then who ran most of the ms (0 the normal thread, 1-3 the governed, . none):\n{line}\n", line.len() / 3);
        }
    }
}
