//! Controlled allocation attribution only; warming is a negative control, never a remedy.
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static WATCH: AtomicBool = AtomicBool::new(false);
static BLOCK: AtomicUsize = AtomicUsize::new(usize::MAX);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static LAST: AtomicUsize = AtomicUsize::new(0);
struct Observed;
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCH.load(Ordering::Relaxed) {
            CALLS.fetch_add(1, Ordering::Relaxed);
            LAST.store(layout.size(), Ordering::Relaxed);
            if layout.size() == BLOCK.load(Ordering::Relaxed) {
                return std::ptr::null_mut();
            }
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Observed = Observed;

fn main() {
    assert!(cfg!(target_os = "macos"), "this attributes the Darwin primitive only");
    let arguments: Vec<_> = std::env::args().collect();
    let mode = arguments.get(1).expect("observe, cold or warm-control");
    let requested: usize = arguments.get(2).expect("observed allocation size").parse().unwrap();
    assert!(matches!(mode.as_str(), "observe" | "cold" | "warm-control"));
    // Initialize reporting before constructing the distinct mutex under observation.
    let _ = writeln!(std::io::stderr(), "mode={mode} injected_allocation_bytes={requested}");
    let mutex = Mutex::new(7usize);
    if mode == "warm-control" {
        drop(mutex.lock().unwrap());
    }
    if mode != "observe" {
        BLOCK.store(requested, Ordering::Relaxed);
    }
    WATCH.store(true, Ordering::Relaxed);
    let held = mutex.lock().unwrap();
    assert_eq!(*held, 7);
    drop(held);
    WATCH.store(false, Ordering::Relaxed);
    println!(
        "allocations={} last_bytes={}",
        CALLS.load(Ordering::Relaxed),
        LAST.load(Ordering::Relaxed)
    );
}
