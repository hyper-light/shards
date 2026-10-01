//! Applies one uncompressed layer as `Store::rootfs` does, then writes its EROFS image
//! to nowhere, printing the heap held and the peak at each step, counted by the global
//! allocator: what the tree and the writer cost an entry, apart from the process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};

use shards_image::erofs::{self, DataRef, Source};
use shards_image::layer;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, size) };
        if !q.is_null() {
            // Counted as the new block allocated while the old one is still held.
            let now = LIVE.fetch_add(size, Ordering::Relaxed) + size;
            PEAK.fetch_max(now, Ordering::Relaxed);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

struct One(File);

impl Source for One {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.seek(SeekFrom::Start(data.offset + at))?;
        self.0.read_exact(buf)
    }
}

fn mb(n: usize) -> f64 {
    n as f64 / 1e6
}

fn step(name: &str, entries: u64) {
    let (live, peak) = (LIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed));
    println!(
        "{name:<8} live {:>8.1} MB ({:>4} B/entry)  peak {:>8.1} MB ({:>4} B/entry)",
        mb(live),
        live as u64 / entries.max(1),
        mb(peak),
        peak as u64 / entries.max(1)
    );
    PEAK.store(live, Ordering::Relaxed);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: build-memory LAYER.tar [IMAGE]")?;
    // With IMAGE, the image is written there too, to compare two writers' bytes.
    let mut out: Box<dyn io::Write> = match std::env::args_os().nth(2) {
        Some(image) => Box::new(io::BufWriter::new(File::create(image)?)),
        None => Box::new(io::sink()),
    };
    println!(
        "size_of Node {} Kind {} Meta {}",
        size_of::<erofs::Node>(),
        size_of::<erofs::Kind>(),
        size_of::<erofs::Meta>()
    );
    let mut entries = 0u64;
    let mut tree = layer::root();
    step("start", 1);
    layer::apply(
        &mut tree,
        0,
        BufReader::with_capacity(1 << 20, File::open(&path)?),
        &mut |_| {
            entries += 1;
            Ok(())
        },
    )?;
    step("apply", entries);
    tree.compact();
    step("compact", entries);
    let written = erofs::write(&tree, &mut One(File::open(&path)?), &mut out)?;
    out.flush()?;
    step("write", entries);
    drop(tree);
    step("dropped", entries);
    println!(
        "entries {entries} inodes {} blocks {}",
        written.inodes, written.blocks
    );
    Ok(())
}
