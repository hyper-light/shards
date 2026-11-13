//! What reading a run's stats from its host allocates (D241): a count of vCPUs said by a
//! hostile or broken host is held to the bytes that follow it before anything is allocated
//! for them. Counted by an allocator that sees only the thread that asks, in a binary of its
//! own, as `crates/abi/tests/allocations.rs` counts a spec's.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use shards_api::image::wire::{StatsAnswer, WireError};
use shards_api::image::{GuestTimes, HostUse, Microvm, VcpuTime};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(size: usize) {
    if COUNTING.with(Cell::get) {
        LARGEST.with(|l| l.set(l.get().max(size)));
    }
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: Counting = Counting;

/// `f`'s result, and the largest allocation it asked for on this thread.
fn largest<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.with(|l| l.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, LARGEST.with(Cell::get))
}

/// A run's stats whose host says four billion vCPUs ran its guest, in the bytes that say
/// none: refused as short, having asked for no more memory than its bytes could fill were
/// each of them a vCPU. Taking the count's bound away asks for 4 billion vCPUs' room
/// (`mutants.py`'s `hostile-count`), which a host that grants memory lazily grants, so the
/// refusal alone shows nothing of it.
#[test]
fn a_hostile_vcpu_count_allocates_nothing_its_bytes_cannot_fill() {
    let microvm = Microvm {
        host: Some(HostUse {
            cpu_ns: 1,
            resident_bytes: 2,
            footprint_bytes: 3,
        }),
        guest: Some(GuestTimes::default()),
    };
    let mut bytes = StatsAnswer::encode(1, Ok((b"cpu 10\n", Some(&microvm)))).unwrap();
    // Where it says no vCPU, their count is its last four bytes.
    let count = bytes.len() - 4;
    bytes[count..].copy_from_slice(&u32::MAX.to_be_bytes());
    let (decoded, largest) = largest(|| StatsAnswer::decode(&bytes));
    assert_eq!(decoded, Err(WireError::Short));
    let fill = bytes.len() * std::mem::size_of::<VcpuTime>();
    assert!(
        largest <= fill,
        "allocated {largest} bytes for an answer of {} bytes",
        bytes.len()
    );
}
