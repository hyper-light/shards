use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::AtomicBool;

thread_local! {
    static REFUSE: Cell<bool> = const { Cell::new(false) };
    static REPORTED: Cell<bool> = const { Cell::new(false) };
}
static TRACE: AtomicBool = AtomicBool::new(false);
struct Refusing;
#[global_allocator]
static ALLOCATOR: Refusing = Refusing;

unsafe extern "C" {
    fn backtrace(frames: *mut *mut libc::c_void, count: libc::c_int) -> libc::c_int;
    fn backtrace_symbols_fd(frames: *const *mut libc::c_void, count: libc::c_int, fd: libc::c_int);
}

fn refused(size: usize) -> bool {
    if !REFUSE.get() {
        return false;
    }
    if !REPORTED.replace(true) {
        let mut digits = [0u8; usize::MAX.ilog10() as usize + 1];
        let mut remaining = size;
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        // SAFETY: constant and stack byte slices are valid for these writes.
        // Best-effort diagnostics never change the null allocation result.
        unsafe {
            let prefix = b"COLD_REFUSED_BYTES ";
            let _ = libc::write(2, prefix.as_ptr().cast(), prefix.len());
            let _ = libc::write(2, digits[at..].as_ptr().cast(), digits.len() - at);
            let _ = libc::write(2, b"\n".as_ptr().cast(), 1);
            if TRACE.load(Ordering::Relaxed) {
                // Explicit diagnostic frame envelope only, matching the retained
                // attribution experiment. C unwinding may incur its own costs.
                let mut frames = [std::ptr::null_mut(); 32];
                let count = backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
                if count > 0 {
                    backtrace_symbols_fd(frames.as_ptr(), count, 2);
                }
            }
        }
    }
    true
}

// SAFETY: non-refused requests and every deallocation use the same System
// allocator and original layout. Refusal is thread-local and returns null.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refused(layout.size()) {
            std::ptr::null_mut()
        } else {
            // SAFETY: caller supplies GlobalAlloc's valid layout.
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refused(layout.size()) {
            std::ptr::null_mut()
        } else {
            // SAFETY: caller supplies GlobalAlloc's valid layout.
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refused(size) {
            std::ptr::null_mut()
        } else {
            // SAFETY: original allocation/layout and new size come from caller.
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every successful allocation above came from System.
        unsafe { System.dealloc(ptr, layout) }
    }
}

