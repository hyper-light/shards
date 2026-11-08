//! Public-API audit probes. Run each memory condition in a fresh process.
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs::File;
use std::hint::black_box;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use shards_image::erofs::{self, DataRef, Kind, Meta, Node, Source, Tree};
use shards_vmm::memory::GuestMemory;

static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static REALLOCS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

fn add(bytes: usize) {
    REQUESTED.fetch_add(bytes, Relaxed);
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every operation forwards the original pointer/layout to System. Counters
// use atomics and allocate nothing. The measured phases do not free older allocations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && COUNT.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            add(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded allocator contract.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() && COUNT.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            add(layout.size());
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded allocator contract; failed realloc preserves p.
        let next = unsafe { System.realloc(p, layout, new_size) };
        if !next.is_null() && COUNT.load(Relaxed) {
            REALLOCS.fetch_add(1, Relaxed);
            LIVE.fetch_sub(layout.size(), Relaxed);
            add(new_size);
        }
        next
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if COUNT.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        // SAFETY: forwarded allocator contract.
        unsafe { System.dealloc(p, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn begin(count: bool) {
    for n in [&ALLOCS, &REALLOCS, &REQUESTED, &LIVE, &PEAK] {
        n.store(0, Relaxed);
    }
    COUNT.store(count, Relaxed);
}

fn end() -> [usize; 5] {
    COUNT.store(false, Relaxed);
    [
        ALLOCS.load(Relaxed),
        REALLOCS.load(Relaxed),
        REQUESTED.load(Relaxed),
        LIVE.load(Relaxed),
        PEAK.load(Relaxed),
    ]
}

fn rusage() -> io::Result<libc::rusage> {
    let mut r = std::mem::MaybeUninit::zeroed();
    // SAFETY: output space is correctly sized and RUSAGE_SELF is valid.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getrusage initialized the output on success.
    Ok(unsafe { r.assume_init() })
}

#[cfg(target_os = "macos")]
fn resident() -> io::Result<(u64, u64)> {
    let mut r = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: the flavor matches the output's layout; we query only this process.
    if unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, r.as_mut_ptr().cast()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful proc_pid_rusage initialized r.
    let r = unsafe { r.assume_init() };
    Ok((r.ri_resident_size, r.ri_phys_footprint))
}

#[cfg(not(target_os = "macos"))]
fn resident() -> io::Result<(u64, u64)> {
    // Linux reports RSS here; no PSS/unique-memory claim is made.
    let stat = std::fs::read_to_string("/proc/self/statm")?;
    let pages: u64 = stat
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .map_err(io::Error::other)?;
    Ok((pages * shards_vmm::platform::page_size()? as u64, 0))
}

fn touch(mem: &GuestMemory, bytes: usize, stride: usize, write: bool) -> io::Result<u64> {
    let mut sum = 0;
    for at in (0..bytes).step_by(stride) {
        let mut value = [0u8];
        mem.read(at as u64, &mut value).map_err(io::Error::other)?;
        sum += u64::from(value[0]);
        if write {
            // An identical write still exercises host CoW; verify the file separately.
            mem.write(at as u64, &value).map_err(io::Error::other)?;
        }
    }
    Ok(black_box(sum))
}

fn emit_memory(
    phase: &str,
    started: Instant,
    before: libc::rusage,
    baseline: (u64, u64),
    checksum: u64,
    counts: [usize; 5],
) -> io::Result<()> {
    let ns = started.elapsed().as_nanos();
    let after = rusage()?;
    let memory = resident()?;
    println!(
        "{{\"phase\":\"{phase}\",\"ns\":{ns},\"minor_faults\":{},\"major_faults\":{},\"rss_delta\":{},\"footprint_delta\":{},\"checksum\":{checksum},\"allocs\":{},\"reallocs\":{},\"requested\":{},\"live\":{},\"peak\":{}}}",
        after.ru_minflt - before.ru_minflt,
        after.ru_majflt - before.ru_majflt,
        memory.0 as i64 - baseline.0 as i64,
        memory.1 as i64 - baseline.1 as i64,
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4]
    );
    Ok(())
}

fn memory(mode: &str, bytes: usize, file: Option<&str>, count: bool) -> io::Result<()> {
    let page = shards_vmm::platform::page_size()?;
    let ranges = [(0, bytes)];
    let input = file.map(File::open).transpose()?;
    // Warm accounting helpers before the measured phase.
    let _ = rusage()?;
    let baseline = resident()?;
    let before = rusage()?;
    begin(count);
    let started = Instant::now();
    let mem = match input.as_ref() {
        Some(f) => GuestMemory::from_file(&ranges, f)?,
        None => GuestMemory::anonymous(&ranges)?,
    };
    let counts = end();
    emit_memory("map", started, before, baseline, 0, counts)?;
    if mode == "none" {
        return Ok(());
    }
    let before = rusage()?;
    let baseline = resident()?;
    begin(count);
    let started = Instant::now();
    let stride = if mode == "sparse" { 1 << 20 } else { page };
    let checksum = touch(&mem, bytes, stride, mode != "read" && mode != "readwrite")?;
    let counts = end();
    emit_memory("first_touch", started, before, baseline, checksum, counts)?;
    let before = rusage()?;
    let baseline = resident()?;
    begin(count);
    let started = Instant::now();
    let checksum = touch(&mem, bytes, stride, mode != "read")?;
    let counts = end();
    emit_memory("repeat_touch", started, before, baseline, checksum, counts)?;
    Ok(())
}

fn save(bytes: usize, path: &str, sparse: bool, count: bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mem = GuestMemory::anonymous(&[(0, bytes)])?;
    if sparse {
        for at in (0..bytes).step_by(1 << 20) {
            mem.write(at as u64, &[1]).map_err(io::Error::other)?;
        }
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let _ = rusage()?;
    let baseline = resident()?;
    let before = rusage()?;
    begin(count);
    let started = Instant::now();
    // Single-threaded and unattached to a hypervisor: the pause contract holds.
    mem.save(&file)?;
    let ns = started.elapsed().as_nanos();
    let c = end();
    let after = rusage()?;
    let memory = resident()?;
    let metadata = file.metadata()?;
    if metadata.len() != bytes as u64 {
        return Err(io::Error::other("wrong snapshot length"));
    }
    println!(
        "{{\"phase\":\"save\",\"ns\":{ns},\"minor_faults\":{},\"major_faults\":{},\"rss_delta\":{},\"footprint_delta\":{},\"logical_bytes\":{},\"allocated_file_bytes\":{},\"allocs\":{},\"reallocs\":{},\"requested\":{},\"live\":{},\"peak\":{}}}",
        after.ru_minflt - before.ru_minflt,
        after.ru_majflt - before.ru_majflt,
        memory.0 as i64 - baseline.0 as i64,
        memory.1 as i64 - baseline.1 as i64,
        metadata.len(),
        metadata.blocks() * 512,
        c[0],
        c[1],
        c[2],
        c[3],
        c[4]
    );
    // Check sampled pages through the actual CoW restore path, outside timing.
    let restored = GuestMemory::from_file(&[(0, bytes)], &file)?;
    for at in (0..bytes).step_by(shards_vmm::platform::page_size()?) {
        let mut value = [0u8];
        restored.read(at as u64, &mut value).map_err(io::Error::other)?;
        let expected = u8::from(sparse && at.is_multiple_of(1 << 20));
        if value[0] != expected {
            return Err(io::Error::other("snapshot did not round-trip"));
        }
    }
    Ok(())
}

struct Fill;
impl Source for Fill {
    fn read_at(&mut self, _: DataRef, _: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(b'x');
        Ok(())
    }
}

struct CountingSink(u64);
impl io::Write for CountingSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += black_box(bytes).len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn image(
    files: usize,
    generations: usize,
    size: u64,
    xattr: usize,
    count: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Names are reusable inputs, excluded from the measured tree phase.
    let names: Vec<Vec<u8>> = (0..files).map(|i| format!("f{i:08}").into_bytes()).collect();
    begin(count);
    let started = Instant::now();
    let mut tree = Tree::new(Meta::default());
    for generation in 0..generations {
        for name in &names {
            let mut meta = Meta::default();
            if xattr > 0 {
                meta.xattrs = BTreeMap::from([(b"user.audit".to_vec(), vec![b'x'; xattr])]);
            }
            tree.insert(
                Tree::ROOT,
                name,
                Node {
                    meta,
                    kind: Kind::File {
                        size,
                        data: DataRef {
                            source: 0,
                            offset: generation as u64,
                        },
                    },
                },
            )?;
        }
    }
    let ns = started.elapsed().as_nanos();
    let c = end();
    println!(
        "{{\"phase\":\"tree\",\"ns\":{ns},\"files\":{files},\"generations\":{generations},\"file_size\":{size},\"xattr_size\":{xattr},\"allocs\":{},\"reallocs\":{},\"requested\":{},\"live\":{},\"peak\":{}}}",
        c[0], c[1], c[2], c[3], c[4]
    );
    // Borrow the prebuilt tree, so all measured writer allocations are new.
    let mut sink = CountingSink(0);
    let mut source = Fill;
    begin(count);
    let started = Instant::now();
    let written = erofs::write(
        &tree,
        black_box(&mut source as &mut dyn Source),
        black_box(&mut sink as &mut dyn io::Write),
    )?;
    let ns = started.elapsed().as_nanos();
    let c = end();
    if sink.0 != written.blocks * erofs::BLOCK {
        return Err("writer byte count differs from declared block count".into());
    }
    println!(
        "{{\"phase\":\"erofs\",\"ns\":{ns},\"inodes\":{},\"blocks\":{},\"allocs\":{},\"reallocs\":{},\"requested\":{},\"live\":{},\"peak\":{}}}",
        written.inodes, written.blocks, c[0], c[1], c[2], c[3], c[4]
    );
    black_box(tree);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let count = args.iter().any(|a| a == "--count");
    match args.get(1).map(String::as_str) {
        Some("memory") => memory(&args[2], args[3].parse()?,
            args.get(4).filter(|s| !s.starts_with("--")).map(String::as_str), count)?,
        Some("image") => image(args[2].parse()?, args[3].parse()?, args[4].parse()?,
            args[5].parse()?, count)?,
        Some("save") => save(args[2].parse()?, &args[3], args[4] == "sparse", count)?,
        Some("layout") => println!("{{\"touch_size\":{},\"node_size\":{},\"page_size\":{}}}",
            size_of::<shards_vmm::hv::Touch>(), size_of::<Node>(),
            shards_vmm::platform::page_size()?),
        _ => return Err("memory <read|write|sparse|none> <bytes> [file] [--count]; image <files> <generations> <size> <xattr> [--count]; layout".into()),
    }
    Ok(())
}
