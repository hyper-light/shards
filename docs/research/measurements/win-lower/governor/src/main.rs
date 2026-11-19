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
use windows_sys::Wdk::System::Threading::{NtQueryInformationThread, ThreadBasicInformation};

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

/// THREAD_BASIC_INFORMATION (winternl.h): its current priority, with any boost, and its base.
#[repr(C)]
struct BasicInformation {
    exit_status: i32,
    teb: *mut core::ffi::c_void,
    process: usize,
    thread: usize,
    affinity: usize,
    priority: i32,
    base_priority: i32,
}

/// A thread's current priority, 0 to 31, its boosts with it (NtQueryInformationThread's
/// ThreadBasicInformation).
fn current(h: HANDLE) -> i32 {
    let mut info = BasicInformation {
        exit_status: 0,
        teb: std::ptr::null_mut(),
        process: 0,
        thread: 0,
        affinity: 0,
        priority: 0,
        base_priority: 0,
    };
    let len = u32::try_from(std::mem::size_of::<BasicInformation>()).unwrap();
    // SAFETY: a thread handle with query access, and a structure of the class's size to fill.
    let status = unsafe {
        NtQueryInformationThread(h, ThreadBasicInformation, (&raw mut info).cast(), len, std::ptr::null_mut())
    };
    if status < 0 { -1 } else { info.priority }
}

struct Sample {
    prio: [i32; GOVERNED + 1],
    /// Each one's current priority, boosts with it.
    now: [i32; GOVERNED + 1],
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
        // Sampled for the window from the moment all three are watched, however late that is.
        let watched = Instant::now();
        while watched.elapsed() < window {
            let mut sample = Sample { prio: [0; GOVERNED + 1], now: [0; GOVERNED + 1], cycles: [0; GOVERNED + 1] };
            for (k, h) in handles.iter().enumerate() {
                sample.prio[k] = priority(*h as HANDLE);
                sample.now[k] = current(*h as HANDLE);
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

/// The governor's pool lines (shards-vmm's `pooled`, at SHARDS_LOG=debug) logged between
/// `from` and `to` (µs since the log began), each the time since the last look and its
/// credit before, charged and after, and its most: looks, their spacing, and what the pool's
/// bounds took off, every pool of the CPU set `mask`.
fn looks(log: &str, mask: usize, from: u128, to: u128) -> String {
    let key = format!("pool 0:{mask:#x} after ");
    let mut gaps = Vec::new();
    let (mut forgiven, mut capped, mut charged) = (0i128, 0i128, 0i128);
    for line in log.lines() {
        let Some(at) = line.split_whitespace().nth(1).and_then(|t| t.strip_suffix("us")).and_then(|t| t.parse::<u128>().ok()) else {
            continue;
        };
        if at < from || at > to {
            continue;
        }
        let Some(rest) = line.split_once(&key).map(|(_, r)| r) else {
            continue;
        };
        let nums: Vec<i128> = rest
            .split(|c: char| !(c.is_ascii_digit() || c == '-'))
            .filter_map(|w| w.parse().ok())
            .collect();
        let [passed, was, used, now, most] = nums[..] else {
            continue;
        };
        gaps.push(passed);
        charged += used;
        let raw = was + passed / 2 - used;
        if raw < -most {
            forgiven += -most - raw;
        } else if raw > most {
            capped += raw - most;
        }
        let _ = now;
    }
    gaps.sort_unstable();
    let pct = |q: f64| gaps.get(((q * gaps.len() as f64).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0);
    format!(
        "{} looks, apart p50 {} us p90 {} us max {} us; charged {} us, forgiven at the floor {} us, kept from above the most {} us",
        gaps.len(),
        pct(0.5),
        pct(0.9),
        gaps.last().copied().unwrap_or(0),
        charged,
        forgiven,
        capped
    )
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().unwrap()).collect();
    // The governor's own account of each look, to a file of this run's.
    // SAFETY: set before any thread of this process reads the environment.
    unsafe { std::env::set_var("SHARDS_LOG", "debug") };
    shards_vmm::log::init();
    let log_path = std::env::temp_dir().join(format!("win-governor-{}.log", std::process::id()));
    shards_vmm::log::to(std::fs::File::create(&log_path).unwrap());
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
    let mut shown = 0;
    for r in 1..=rounds {
        let began_us = shards_vmm::log::uptime_us();
        let round = round(seconds, load, x);
        let ended_us = shards_vmm::log::uptime_us();
        let bound = round.elapsed / 2 + SLACK;
        let (mut raised, mut lowered, mut normal_level, mut normal_thread) = (0u64, 0u64, 0u64, 0u64);
        // Lowered cycles run at a current priority above its base's 7, and the highest seen.
        let (mut lowered_boosted, mut highest) = (0u64, 0i32);
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
                    LOWERED => {
                        lowered += dc;
                        if a.now[k] > 7 || b.now[k] > 7 {
                            lowered_boosted += dc;
                            highest = highest.max(a.now[k]).max(b.now[k]);
                        }
                    }
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
            if line.len() < 7 * 300 && !round.samples.is_empty() && shown == 0 {
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
                // The runner's current priority, where it is a governed thread.
                if most.0 != 0 && most.1 != '0' {
                    let k = usize::from(u8::try_from(most.1).unwrap() - b'0');
                    line.push_str(&format!("@{}", b.now[k]));
                }
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
        let interval = if n > 0 { round.elapsed.as_secs_f64() * 1000.0 / (n as f64) } else { 0.0 };
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        println!("  round {r}: the governor's pool of the three's CPU: {}", looks(&log, x, began_us, ended_us));
        println!(
            "  round {r}: {} samples, one each {interval:.2} ms of the window; lowered cycles run above base priority: {:.1}% of all, the highest current priority {highest}",
            round.samples.len(),
            pct(lowered_boosted, all)
        );
        if shown == 0 && !line.is_empty() {
            shown = r;
            println!("\nRound {r}'s first samples: the three's levels (R all raised, L all lowered, m mixed, N normal), then who ran most of the interval (0 the normal thread, 1-3 the governed, . none), @ its current priority:\n{line}\n");
        }
    }
}
