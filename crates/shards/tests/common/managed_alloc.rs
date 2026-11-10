//! Test-only System events for managed_output's cost fixture. Registration is in
//! that one caller, never in production. See managed-allocation-metrics/README.md.
//!
//! All bytes are requested Layout sizes, not usable sizes, RSS, copies, native
//! transient realloc storage, kernel charges or other processes' allocations.
//! Snapshots load independent counters; an in-flight hook can split any event.
//! The recorder itself allocates nothing and cannot unwind. System retains its
//! native failure behavior; a native abort cannot be counted after it occurs.
#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

// Reserve one atomic representation for sticky uncertainty, including an exact
// result of MAX. No separate overflow flag can race its counter's snapshot.
const UNKNOWN: usize = usize::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Known(usize),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotSemantics {
    ConcurrentNonAtomic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub semantics: SnapshotSemantics,
    pub alloc_successes: Count,
    pub alloc_failures: Count,
    pub zeroed_successes: Count,
    pub zeroed_failures: Count,
    pub realloc_successes: Count,
    pub realloc_failures: Count,
    pub dealloc_calls: Count,
    pub allocation_requested_bytes: Count,
    pub allocation_success_bytes: Count,
    pub realloc_requested_bytes: Count,
    pub realloc_grown_bytes: Count,
    pub realloc_shrunk_bytes: Count,
    pub deallocated_bytes: Count,
    pub live_requested_bytes: Count,
    pub peak_recorded_live_requested_bytes: Count,
}

struct Counter(AtomicUsize);

enum Change {
    Add(usize),
    Remove(usize),
}

impl Counter {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    fn change(&self, change: Change) -> usize {
        let mut previous = self.0.load(Relaxed);
        loop {
            let next = if previous == UNKNOWN {
                UNKNOWN
            } else {
                match change {
                    Change::Add(bytes) => previous.checked_add(bytes),
                    Change::Remove(bytes) => previous.checked_sub(bytes),
                }
                .unwrap_or(UNKNOWN)
            };
            match self.0.compare_exchange_weak(previous, next, Relaxed, Relaxed) {
                Ok(_) => return next,
                Err(observed) => previous = observed,
            }
        }
    }

    fn add(&self, value: usize) -> usize {
        self.change(Change::Add(value))
    }

    fn remove(&self, value: usize) -> usize {
        self.change(Change::Remove(value))
    }

    fn sample(&self) -> Count {
        match self.0.load(Relaxed) {
            UNKNOWN => Count::Unknown,
            value => Count::Known(value),
        }
    }
}

enum AllocationKind {
    Ordinary,
    Zeroed,
}

pub struct ManagedAlloc {
    alloc_successes: Counter,
    alloc_failures: Counter,
    zeroed_successes: Counter,
    zeroed_failures: Counter,
    realloc_successes: Counter,
    realloc_failures: Counter,
    dealloc_calls: Counter,
    allocation_requested_bytes: Counter,
    allocation_success_bytes: Counter,
    realloc_requested_bytes: Counter,
    realloc_grown_bytes: Counter,
    realloc_shrunk_bytes: Counter,
    deallocated_bytes: Counter,
    live_requested_bytes: Counter,
    peak_recorded_live_requested_bytes: Counter,
}

impl Default for ManagedAlloc {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedAlloc {
    pub const fn new() -> Self {
        Self {
            alloc_successes: Counter::new(),
            alloc_failures: Counter::new(),
            zeroed_successes: Counter::new(),
            zeroed_failures: Counter::new(),
            realloc_successes: Counter::new(),
            realloc_failures: Counter::new(),
            dealloc_calls: Counter::new(),
            allocation_requested_bytes: Counter::new(),
            allocation_success_bytes: Counter::new(),
            realloc_requested_bytes: Counter::new(),
            realloc_grown_bytes: Counter::new(),
            realloc_shrunk_bytes: Counter::new(),
            deallocated_bytes: Counter::new(),
            live_requested_bytes: Counter::new(),
            peak_recorded_live_requested_bytes: Counter::new(),
        }
    }

    /// Independent Relaxed loads, never a coherent transaction or a quiescence
    /// test. Even live and peak can straddle one hook's two updates. The peak is
    /// process-lifetime bookkeeping, not a phase peak; do not subtract peaks.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            semantics: SnapshotSemantics::ConcurrentNonAtomic,
            alloc_successes: self.alloc_successes.sample(),
            alloc_failures: self.alloc_failures.sample(),
            zeroed_successes: self.zeroed_successes.sample(),
            zeroed_failures: self.zeroed_failures.sample(),
            realloc_successes: self.realloc_successes.sample(),
            realloc_failures: self.realloc_failures.sample(),
            dealloc_calls: self.dealloc_calls.sample(),
            allocation_requested_bytes: self.allocation_requested_bytes.sample(),
            allocation_success_bytes: self.allocation_success_bytes.sample(),
            realloc_requested_bytes: self.realloc_requested_bytes.sample(),
            realloc_grown_bytes: self.realloc_grown_bytes.sample(),
            realloc_shrunk_bytes: self.realloc_shrunk_bytes.sample(),
            deallocated_bytes: self.deallocated_bytes.sample(),
            live_requested_bytes: self.live_requested_bytes.sample(),
            peak_recorded_live_requested_bytes: self.peak_recorded_live_requested_bytes.sample(),
        }
    }

    fn change_live(&self, change: Change) {
        let live = self.live_requested_bytes.change(change);
        // MAX propagates Unknown; fetch_max cannot restore a known peak later.
        self.peak_recorded_live_requested_bytes.0.fetch_max(live, Relaxed);
    }

    fn allocated(&self, size: usize, kind: AllocationKind, returned: *mut u8) {
        self.allocation_requested_bytes.add(size);
        let (successes, failures) = match kind {
            AllocationKind::Ordinary => (&self.alloc_successes, &self.alloc_failures),
            AllocationKind::Zeroed => (&self.zeroed_successes, &self.zeroed_failures),
        };
        if returned.is_null() {
            failures.add(1);
        } else {
            successes.add(1);
            self.allocation_success_bytes.add(size);
            self.change_live(Change::Add(size));
        }
    }

    fn reallocated(&self, old: usize, new: usize, returned: *mut u8) {
        self.realloc_requested_bytes.add(new);
        if returned.is_null() {
            self.realloc_failures.add(1);
            return;
        }
        self.realloc_successes.add(1);
        if let Some(growth) = new.checked_sub(old) {
            self.realloc_grown_bytes.add(growth);
            self.change_live(Change::Add(growth));
        } else if let Some(shrink) = old.checked_sub(new) {
            self.realloc_shrunk_bytes.add(shrink);
            self.change_live(Change::Remove(shrink));
        }
    }

    fn deallocated(&self, size: usize) {
        self.dealloc_calls.add(1);
        self.deallocated_bytes.add(size);
        self.change_live(Change::Remove(size));
    }
}

// SAFETY: All caller contracts are forwarded unchanged to the matching System
// method, exactly once. No pointer/size/alignment is changed or dereferenced by
// accounting. The recorder only uses checked integer arithmetic and atomics;
// no callback, allocating synchronization, formatting, panic or unwinding path.
// Rust1.98.1 library/{core/src/alloc/global.rs,std/src/alloc.rs}; linked research
// records the nonzero/layout/live-pointer requirements and direct-System limits.
unsafe impl GlobalAlloc for ManagedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies GlobalAlloc::alloc's nonzero valid layout.
        let returned = unsafe { System.alloc(layout) };
        self.allocated(layout.size(), AllocationKind::Ordinary, returned);
        returned
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies alloc_zeroed's nonzero valid layout.
        let returned = unsafe { System.alloc_zeroed(layout) };
        self.allocated(layout.size(), AllocationKind::Zeroed, returned);
        returned
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: The caller supplies a live pointer from this System-backed
        // instance, its matching layout, and a nonzero valid new size.
        let returned = unsafe { System.realloc(ptr, layout, new_size) };
        self.reallocated(layout.size(), new_size, returned);
        returned
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The caller supplies this allocator's live pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
        self.deallocated(layout.size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure accounting witness only: these nonnull sentinels are never passed
    // to GlobalAlloc/System or dereferenced. They do not simulate native OOM.
    fn success() -> *mut u8 {
        std::ptr::dangling_mut::<u8>()
    }

    #[test]
    fn failed_requests_do_not_become_live_or_successful_bytes() {
        let a = ManagedAlloc::new();
        a.allocated(8, AllocationKind::Ordinary, success());
        a.allocated(16, AllocationKind::Zeroed, success());
        a.allocated(32, AllocationKind::Ordinary, std::ptr::null_mut());
        a.allocated(64, AllocationKind::Zeroed, std::ptr::null_mut());
        a.reallocated(8, 128, std::ptr::null_mut());
        let s = a.snapshot();
        assert_eq!(
            (s.alloc_successes, s.alloc_failures),
            (Count::Known(1), Count::Known(1))
        );
        assert_eq!(
            (s.zeroed_successes, s.zeroed_failures),
            (Count::Known(1), Count::Known(1))
        );
        assert_eq!(s.allocation_requested_bytes, Count::Known(120));
        assert_eq!(s.allocation_success_bytes, Count::Known(24));
        assert_eq!(s.realloc_requested_bytes, Count::Known(128));
        assert_eq!(
            (s.realloc_successes, s.realloc_failures),
            (Count::Known(0), Count::Known(1))
        );
        assert_eq!(s.live_requested_bytes, Count::Known(24));
        assert_eq!(s.peak_recorded_live_requested_bytes, Count::Known(24));
    }

    #[test]
    fn realloc_growth_equal_size_and_shrink_count_one_event_each() {
        let a = ManagedAlloc::new();
        a.allocated(16, AllocationKind::Ordinary, success());
        a.reallocated(16, 40, success());
        a.reallocated(40, 40, success());
        a.reallocated(40, 8, success());
        a.deallocated(8);
        let s = a.snapshot();
        assert_eq!(s.realloc_successes, Count::Known(3));
        assert_eq!(s.realloc_requested_bytes, Count::Known(88));
        assert_eq!(s.realloc_grown_bytes, Count::Known(24));
        assert_eq!(s.realloc_shrunk_bytes, Count::Known(32));
        assert_eq!(s.alloc_successes, Count::Known(1));
        assert_eq!(
            (s.dealloc_calls, s.deallocated_bytes),
            (Count::Known(1), Count::Known(8))
        );
        assert_eq!(s.live_requested_bytes, Count::Known(0));
        assert_eq!(s.peak_recorded_live_requested_bytes, Count::Known(40));
    }

    #[test]
    fn representation_exhaustion_and_underflow_are_sticky_unknown() {
        let c = Counter(AtomicUsize::new(usize::MAX - 1));
        c.add(2);
        assert_eq!(c.sample(), Count::Unknown);
        c.remove(usize::MAX);
        assert_eq!(c.sample(), Count::Unknown);
        let c = Counter(AtomicUsize::new(usize::MAX - 1));
        c.add(1);
        assert_eq!(c.sample(), Count::Unknown);
        let a = ManagedAlloc::new();
        a.deallocated(1); // Pure arithmetic underflow, never an invalid dealloc.
        assert_eq!(a.snapshot().peak_recorded_live_requested_bytes, Count::Unknown);
        a.allocated(1, AllocationKind::Ordinary, success());
        assert_eq!(a.snapshot().live_requested_bytes, Count::Unknown);
        assert_eq!(a.snapshot().peak_recorded_live_requested_bytes, Count::Unknown);
        a.deallocated(1);
        assert_eq!(a.snapshot().live_requested_bytes, Count::Unknown);
    }

    #[test]
    fn valid_system_alloc_and_zeroed_blocks_are_released() {
        let a = ManagedAlloc::new();
        let layout = Layout::new::<[u128; 2]>();
        for zeroed in [false, true] {
            // SAFETY: Nonzero checked layout. The nonnull result is only read
            // after zeroed allocation or initialization, then deallocated once.
            let (nonnull, initialized) = unsafe {
                let p = if zeroed {
                    a.alloc_zeroed(layout)
                } else {
                    a.alloc(layout)
                };
                if p.is_null() {
                    (false, false)
                } else {
                    if !zeroed {
                        p.write_bytes(0, layout.size());
                    }
                    let zero = std::slice::from_raw_parts(p, layout.size())
                        .iter()
                        .all(|b| *b == 0);
                    a.dealloc(p, layout);
                    (true, zero)
                }
            };
            assert!(nonnull && initialized);
        }
        let s = a.snapshot();
        assert_eq!(s.alloc_successes, Count::Known(1));
        assert_eq!(s.zeroed_successes, Count::Known(1));
        assert_eq!(s.dealloc_calls, Count::Known(2));
        assert_eq!(s.live_requested_bytes, Count::Known(0));
    }

    #[test]
    fn valid_system_realloc_uses_only_returned_pointer_and_matching_layout() {
        let a = ManagedAlloc::new();
        let mut layout = Layout::new::<[u128; 2]>();
        // SAFETY: Valid nonzero layout, checked for failure before accessing it.
        let mut p = unsafe { a.alloc_zeroed(layout) };
        assert!(!p.is_null());
        let mut resized = true;
        let mut preserved = true;
        for next in [Layout::new::<[u128; 4]>(), Layout::new::<[u128; 1]>()] {
            // SAFETY: Live pointer and matching layout; nonzero new size with
            // the same alignment. Failure retains p, success replaces it.
            let new = unsafe { a.realloc(p, layout, next.size()) };
            if new.is_null() {
                resized = false;
                break;
            }
            let retained = layout.size().min(next.size());
            p = new;
            layout = next;
            // SAFETY: Retained prefix came from the initialized previous block.
            preserved &= unsafe { std::slice::from_raw_parts(p, retained) }
                .iter()
                .all(|b| *b == 0);
        }
        // SAFETY: On failure p/layout still name the old block; otherwise both
        // name the latest successful realloc. No invalid/old pointer is used.
        unsafe { a.dealloc(p, layout) };
        assert!(resized && preserved);
        let s = a.snapshot();
        assert_eq!(s.realloc_successes, Count::Known(2));
        assert_eq!(s.realloc_failures, Count::Known(0));
        assert_eq!(s.live_requested_bytes, Count::Known(0));
        assert_eq!(
            s.peak_recorded_live_requested_bytes,
            Count::Known(Layout::new::<[u128; 4]>().size())
        );
    }
}
