//! Dedicated Linux unit-test allocator refusal; never linked into production.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! { static LEFT: Cell<Option<usize>> = const { Cell::new(None) }; }
struct Refused;
#[global_allocator]
static ALLOCATOR: Refused = Refused;
fn refuse() -> bool {
    LEFT.try_with(|left| match left.get() {
        None => false,
        Some(0) => true,
        Some(n) => {
            left.set(Some(n - 1));
            false
        }
    })
    .unwrap_or(false)
}
// SAFETY: ordinary operations forward System's exact allocator contract;
// injected alloc/realloc failures return null without logging or heap use.
unsafe impl GlobalAlloc for Refused {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refuse() {
            std::ptr::null_mut()
        } else {
            unsafe { System.realloc(at, layout, size) }
        }
    }
    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        unsafe { System.dealloc(at, layout) }
    }
}
pub(crate) fn after<T>(successful: usize, run: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            LEFT.with(|left| left.set(None));
        }
    }
    LEFT.with(|left| {
        assert!(left.get().is_none());
        left.set(Some(successful));
    });
    let restore = Restore;
    let result = run();
    drop(restore);
    result
}
