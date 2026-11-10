//! Deterministic unit-test fault injection, never linked into production.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static FAIL_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
}

struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn refuse() -> bool {
    FAIL_AFTER
        .try_with(|left| match left.get() {
            None => false,
            Some(0) => true,
            Some(n) => {
                left.set(Some(n - 1));
                false
            }
        })
        .unwrap_or(false)
}

// SAFETY: ordinary allocations/deallocations forward the exact GlobalAlloc contract
// to System. Injected failures return null as GlobalAlloc permits; no logging/heap use.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            // SAFETY: the caller's layout satisfies GlobalAlloc's contract.
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            // SAFETY: the caller's layout satisfies GlobalAlloc's contract.
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            // SAFETY: forward the caller's pointer/layout/new-size contract unchanged.
            unsafe { System.realloc(at, layout, size) }
        }
    }
    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: forward the caller's live allocation and original layout unchanged.
        unsafe { System.dealloc(at, layout) }
    }
}

/// Every allocation at/after the selected point fails until the tested call returns.
/// Assertions happen after this guard drops, so a returning OOM is distinguishable
/// from aborting in an allocating error handler.
pub(crate) fn fail_after<T>(successful: usize, run: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            FAIL_AFTER.with(|left| left.set(None));
        }
    }
    FAIL_AFTER.with(|left| {
        assert!(left.get().is_none());
        left.set(Some(successful));
    });
    let restore = Restore;
    let result = run();
    drop(restore);
    result
}

pub(crate) fn arm(){ FAIL_AFTER.with(|left|left.set(Some(0))); }
pub(crate) fn reset(){ FAIL_AFTER.with(|left|left.set(None)); }
