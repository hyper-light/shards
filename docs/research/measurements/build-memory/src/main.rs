//! Applies one uncompressed layer as `Store::rootfs` does, then writes its EROFS image
//! to nowhere, printing the heap held and the peak at each step, and the allocations it
//! made, counted by the global allocator: what the tree and the writer cost an entry,
//! apart from the process. Each `--below BASE.tar` is applied first, in order, as the
//! layers under it.
//!
//!   build-memory [--below BASE.tar]... LAYER.tar [IMAGE]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};

use shards_image::erofs::{self, DataRef, Source};
use shards_image::layer;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
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

thread_local! {
    /// When the last step ended: each step's line says how long it took.
    static CLOCK: std::cell::Cell<std::time::Instant> = std::cell::Cell::new(std::time::Instant::now());
}

/// The layers' archives, by the index each was applied with.
struct Many(Vec<File>);

impl Source for Many {
    fn read_at(&mut self, data: DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let file = self
            .0
            .get_mut(data.source as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such layer"))?;
        file.seek(SeekFrom::Start(data.offset + at))?;
        file.read_exact(buf)
    }
}

fn mb(n: usize) -> f64 {
    n as f64 / 1e6
}

fn step(name: &str, entries: u64) {
    let (live, peak) = (LIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed));
    let ms = CLOCK.with(|c| {
        let now = std::time::Instant::now();
        let ms = now.duration_since(c.replace(now)).as_secs_f64() * 1e3;
        ms
    });
    let allocs = ALLOCS.swap(0, Ordering::Relaxed);
    println!(
        "{name:<8} {ms:>7.1} ms  live {:>8.1} MB ({:>4} B/entry)  peak {:>8.1} MB ({:>4} B/entry)  allocations {allocs} ({:.1}/entry)",
        mb(live),
        live as u64 / entries.max(1),
        mb(peak),
        peak as u64 / entries.max(1),
        allocs as f64 / entries.max(1) as f64
    );
    PEAK.store(live, Ordering::Relaxed);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let usage = "usage: build-memory [--below BASE.tar]... LAYER.tar [IMAGE]";
    let mut args = std::env::args_os().skip(1).peekable();
    let mut below = Vec::new();
    while args.peek().is_some_and(|a| a == "--below") {
        args.next();
        below.push(args.next().ok_or(usage)?);
    }
    let path = args.next().ok_or(usage)?;
    // With IMAGE, the image is written there too, to compare two writers' bytes.
    let mut out: Box<dyn io::Write> = match args.next() {
        Some(image) => Box::new(io::BufWriter::new(File::create(image)?)),
        None => Box::new(io::sink()),
    };
    println!(
        "size_of Node {} Kind {} Meta {}",
        size_of::<erofs::Node>(),
        size_of::<erofs::Kind>(),
        size_of::<erofs::Meta>()
    );
    let mut tree = layer::root();
    step("start", 1);
    // The layers below, each read back by its own index, as `Store::rootfs` keeps them.
    let mut files = Vec::new();
    for (i, base) in below.iter().enumerate() {
        let mut entries = 0u64;
        layer::apply(
            &mut tree,
            u32::try_from(i)?,
            BufReader::with_capacity(1 << 20, File::open(base)?),
            &mut |_| {
                entries += 1;
                Ok(())
            },
        )?;
        step("below", entries);
        files.push(File::open(base)?);
    }
    let mut entries = 0u64;
    layer::apply(
        &mut tree,
        u32::try_from(below.len())?,
        BufReader::with_capacity(1 << 20, File::open(&path)?),
        &mut |_| {
            entries += 1;
            Ok(())
        },
    )?;
    step("apply", entries);
    files.push(File::open(&path)?);
    tree.compact();
    step("compact", entries);
    let written = erofs::write(&tree, &mut Many(files), &mut out)?;
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
