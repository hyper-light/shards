//! What each phase of a build costs in memory, for measurements only (feature
//! `alloc-count`, docs/research/measurements/build-memory): the global allocator counts
//! allocations, reallocations, bytes asked for, and the heap held and its peak, and
//! [`phase`] prints them, with getrusage's page faults, CPU time and maximum resident set,
//! as each phase ends, then starts the next phase's peak from what is held.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static REQUESTED: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = LIVE.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call goes to the system allocator with the caller's own arguments; the
// counters are only read.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, as GlobalAlloc::alloc requires.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            REQUESTED.fetch_add(l.size() as u64, Ordering::Relaxed);
            grew(l.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            REQUESTED.fetch_add(l.size() as u64, Ordering::Relaxed);
            grew(l.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from this allocator with layout `l`.
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        // SAFETY: `p` came from this allocator with layout `l`, as the caller promises.
        let q = unsafe { System.realloc(p, l, size) };
        if !q.is_null() {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            REQUESTED.fetch_add(size as u64, Ordering::Relaxed);
            // Counted as the new block held while the old one still is, as a move holds
            // both.
            grew(size);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

/// getrusage(RUSAGE_SELF): minor and major faults, user and system CPU in µs, and the
/// maximum resident set in bytes.
fn usage() -> (i64, i64, i64, i64, i64) {
    // SAFETY: a zeroed rusage is a valid out-parameter, which getrusage fills.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is valid for writes.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return (0, 0, 0, 0, 0);
    }
    let us = |t: libc::timeval| t.tv_sec * 1_000_000 + i64::from(t.tv_usec);
    // Linux reports ru_maxrss in KiB, macOS in bytes.
    let rss = if cfg!(target_os = "macos") {
        ru.ru_maxrss
    } else {
        ru.ru_maxrss * 1024
    };
    (ru.ru_minflt, ru.ru_majflt, us(ru.ru_utime), us(ru.ru_stime), rss)
}

struct Last {
    at: Instant,
    allocs: u64,
    reallocs: u64,
    requested: u64,
    usage: (i64, i64, i64, i64, i64),
}

static LAST: std::sync::Mutex<Option<Last>> = std::sync::Mutex::new(None);

/// Ends the phase `name`: prints what it allocated, reallocated and asked for, the heap
/// held at its end and at its peak, its faults and CPU, and the maximum resident set so
/// far, as one `alloc-phase` line on stderr.
pub fn phase(name: &str) {
    let now = Last {
        at: Instant::now(),
        allocs: ALLOCS.load(Ordering::Relaxed),
        reallocs: REALLOCS.load(Ordering::Relaxed),
        requested: REQUESTED.load(Ordering::Relaxed),
        usage: usage(),
    };
    let (live, peak) = (LIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed));
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(l) = last.as_ref() {
        let _ = writeln!(
            std::io::stderr(),
            "alloc-phase {name} ms={:.1} allocs={} reallocs={} requested_mb={:.1} live_mb={:.1} peak_mb={:.1} minflt={} majflt={} user_ms={:.1} sys_ms={:.1} maxrss_mb={:.1}",
            now.at.duration_since(l.at).as_secs_f64() * 1e3,
            now.allocs - l.allocs,
            now.reallocs - l.reallocs,
            (now.requested - l.requested) as f64 / 1e6,
            live as f64 / 1e6,
            peak as f64 / 1e6,
            now.usage.0 - l.usage.0,
            now.usage.1 - l.usage.1,
            (now.usage.2 - l.usage.2) as f64 / 1e3,
            (now.usage.3 - l.usage.3) as f64 / 1e3,
            now.usage.4 as f64 / 1e6,
        );
    }
    PEAK.store(live, Ordering::Relaxed);
    *last = Some(now);
}
