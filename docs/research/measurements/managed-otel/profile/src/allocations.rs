use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

static ON: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static BASE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn acquired(size: usize) {
    let live = LIVE.fetch_add(size, Relaxed) + size;
    if ON.load(Relaxed) {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size, Relaxed);
        PEAK.fetch_max(live, Relaxed);
    }
}

// SAFETY: every native allocation operation is forwarded with its original
// pointer/layout; counters neither allocate nor unwind nor access the allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplies a valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            acquired(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller supplies a valid allocation layout.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            acquired(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        if ON.load(Relaxed) {
            FREES.fetch_add(1, Relaxed);
        }
        // SAFETY: caller owns this live allocation with this exact layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller supplies live pointer/layout and valid size.
        let next = unsafe { System.realloc(ptr, layout, new_size) };
        if !next.is_null() {
            let live = if new_size >= layout.size() {
                LIVE.fetch_add(new_size - layout.size(), Relaxed) + new_size - layout.size()
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed) - (layout.size() - new_size)
            };
            if ON.load(Relaxed) {
                REALLOCS.fetch_add(1, Relaxed);
                REALLOC_BYTES.fetch_add(new_size, Relaxed);
                PEAK.fetch_max(live, Relaxed);
            }
        }
        next
    }
}

pub(super) fn start() {
    let live = LIVE.load(Relaxed);
    BASE.store(live, Relaxed);
    PEAK.store(live, Relaxed);
    ON.store(true, Relaxed);
}

pub(super) fn end() -> serde_json::Value {
    ON.store(false, Relaxed);
    let base = BASE.load(Relaxed);
    serde_json::json!({
        "alloc_calls": CALLS.load(Relaxed), "alloc_bytes": BYTES.load(Relaxed),
        "realloc_calls": REALLOCS.load(Relaxed), "realloc_new_bytes": REALLOC_BYTES.load(Relaxed),
        "dealloc_calls": FREES.load(Relaxed), "peak_live_delta_bytes": PEAK.load(Relaxed)-base,
        "live_before": base, "live_after": LIVE.load(Relaxed),
        "note": "observed System GlobalAlloc calls for this optimized binary, not a language guarantee or native/C/kernel allocation census"
    })
}
