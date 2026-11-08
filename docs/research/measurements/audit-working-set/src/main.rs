//! Counts Rust heap requests in the real working-set decoder. Does not create a VM.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use shards_vmm::{arch::aarch64::state::VcpuState, hv::Touch, snapshot};

struct Counted;
static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counted = Counted;

// SAFETY: all operations forward the supplied layout/pointer to System unchanged;
// counters allocate no memory, and probes keep preexisting values alive while enabled.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && ENABLED.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(layout.size(), Relaxed);
            let now = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if ENABLED.load(Relaxed) {
            DEALLOCS.fetch_add(1, Relaxed);
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        // SAFETY: forwarded allocator contract.
        unsafe { System.dealloc(p, layout) };
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let out = unsafe { System.realloc(p, layout, size) };
        if !out.is_null() && ENABLED.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(size, Relaxed);
            if size >= layout.size() {
                let now = LIVE.fetch_add(size - layout.size(), Relaxed) + size - layout.size();
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        out
    }
}

fn reset() {
    for counter in [&ALLOCS, &REALLOCS, &DEALLOCS, &REQUESTED, &LIVE, &PEAK] {
        counter.store(0, Relaxed);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("usage: audit-working-set DIRECTORY")?;
    std::fs::create_dir_all(&path)?;
    let generation = File::open(&path)?;
    println!(
        "{{\"kind\":\"layout\",\"touch_bytes\":{},\"arm_vcpu_inline_bytes\":{}}}",
        size_of::<Touch>(),
        size_of::<VcpuState>()
    );
    for n in [1, 718, 3867, 65536] {
        let pages: Vec<Touch> = (0..n)
            .map(|i| Touch {
                gpa: 0x8000_0000 + i * 16384,
                written: i % 3 == 0,
            })
            .collect();
        snapshot::write_working_set(&generation, &pages, 16384)?;
        let _ = snapshot::read_working_set(&generation, 16384, n)?;
        // Counts and timing are separate. The timing path has instrumentation disabled.
        for sample in 0..30 {
            reset();
            ENABLED.store(true, Relaxed);
            let decoded = snapshot::read_working_set(&generation, 16384, n)?;
            ENABLED.store(false, Relaxed);
            let decoded = decoded.ok_or("unexpected absent working set")?;
            assert_eq!(decoded, pages);
            println!(
                "{{\"kind\":\"allocations\",\"pages\":{n},\"sample\":{sample},\"allocs\":{},\"reallocs\":{},\"deallocs\":{},\"requested_bytes\":{},\"peak_requested_live_bytes\":{},\"retained_requested_bytes\":{}}}",
                ALLOCS.load(Relaxed),
                REALLOCS.load(Relaxed),
                DEALLOCS.load(Relaxed),
                REQUESTED.load(Relaxed),
                PEAK.load(Relaxed),
                LIVE.load(Relaxed)
            );
            drop(decoded);
        }
        for sample in 0..30 {
            let t0 = Instant::now();
            let decoded = snapshot::read_working_set(&generation, 16384, n)?;
            let us = t0.elapsed().as_secs_f64() * 1e6;
            let decoded = decoded.ok_or("unexpected absent working set")?;
            assert_eq!(decoded, pages);
            println!("{{\"kind\":\"timing\",\"pages\":{n},\"sample\":{sample},\"us\":{us}}}");
        }
    }
    for payload_bytes in [256, 1 << 20] {
        let payload = vec![0x5a; payload_bytes];
        let expected = shards_vmm::initramfs::with_init(&payload);
        for sample in 0..30 {
            reset();
            ENABLED.store(true, Relaxed);
            let archive = shards_vmm::initramfs::with_init(&payload);
            ENABLED.store(false, Relaxed);
            assert_eq!(archive, expected);
            println!(
                "{{\"kind\":\"initramfs_allocations\",\"payload_bytes\":{payload_bytes},\"sample\":{sample},\"allocs\":{},\"reallocs\":{},\"deallocs\":{},\"requested_bytes\":{},\"peak_requested_live_bytes\":{},\"retained_requested_bytes\":{},\"archive_bytes\":{},\"archive_capacity\":{}}}",
                ALLOCS.load(Relaxed),
                REALLOCS.load(Relaxed),
                DEALLOCS.load(Relaxed),
                REQUESTED.load(Relaxed),
                PEAK.load(Relaxed),
                LIVE.load(Relaxed),
                archive.len(),
                archive.capacity()
            );
        }
        for sample in 0..30 {
            let t0 = Instant::now();
            let archive = shards_vmm::initramfs::with_init(&payload);
            let us = t0.elapsed().as_secs_f64() * 1e6;
            assert_eq!(archive, expected);
            println!(
                "{{\"kind\":\"initramfs_timing\",\"payload_bytes\":{payload_bytes},\"sample\":{sample},\"us\":{us}}}"
            );
        }
    }
    std::fs::remove_file(std::path::Path::new(&path).join("working-set"))?;
    std::fs::remove_dir(&path)?;
    Ok(())
}
