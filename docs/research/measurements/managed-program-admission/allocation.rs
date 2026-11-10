use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static FAIL_AT: Cell<usize> = const { Cell::new(0) };
    static ATTEMPTS: Cell<usize> = const { Cell::new(0) };
}

struct Allocator;

fn refuse() -> bool {
    FAIL_AT
        .try_with(|fail| {
            let at = fail.get();
            at != 0
                && ATTEMPTS.with(|count| {
                    let next = count.get().saturating_add(1);
                    count.set(next);
                    next == at
                })
        })
        .unwrap_or(false)
}

// SAFETY: accepted requests and all deallocations use System with the original layout;
// rejected allocations return null, as GlobalAlloc requires. Test control is thread-local.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller supplies GlobalAlloc's valid layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: as alloc, with zeroed storage requested.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller owns pointer with this layout and supplies the new size.
        unsafe { System.realloc(pointer, layout, size) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: all accepted allocations came from System with this layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

pub(crate) fn during<T>(at: usize, operation: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAIL_AT.with(|fail| fail.set(0));
        }
    }
    FAIL_AT.with(|fail| assert_eq!(fail.get(), 0));
    ATTEMPTS.with(|count| count.set(0));
    FAIL_AT.with(|fail| fail.set(at));
    let guard = Reset;
    let result = operation();
    drop(guard);
    (result, ATTEMPTS.with(Cell::get))
}
