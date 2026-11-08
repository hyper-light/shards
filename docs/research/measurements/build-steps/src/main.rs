//! What a build's steps cost in their snapshots (review 4.2, 4.3). An image's layers
//! applied as a base image's are, then STEPS steps as the build executor takes them
//! (crates/shards/src/build/exec.rs): the input snapshot cloned, a file put in the clone,
//! a step begun, and the links counted (`Fs::links`, as `write_layer` and a COPY count
//! them); every snapshot kept, as a build keeps them until its export. For each step:
//! the clone's and the count's µs, and the heap held, by a counting allocator.
//!
//!     build-steps STEPS LAYER.tar...

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::BufReader;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use shards_build::vfs::Fs;
use shards_image::erofs::{DataRef, Kind, Meta, Node};
use shards_image::layer;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mb() -> f64 {
    LIVE.load(Ordering::Relaxed) as f64 / 1e6
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let steps: usize = args.first().ok_or("STEPS")?.parse()?;
    let mut tree = layer::root();
    let mut entries = 0u64;
    for (i, path) in args.iter().skip(1).enumerate() {
        layer::apply(
            &mut tree,
            u32::try_from(i)?,
            BufReader::with_capacity(1 << 20, File::open(path)?),
            &mut |_| {
                entries += 1;
                Ok(())
            },
        )?;
    }
    tree.compact();
    let base = mb();
    let mut snapshots = vec![Rc::new(Fs::new(tree, (0, 0)))];
    println!("{entries} entries, {} nodes; the base snapshot {base:.1} MB", snapshots[0].tree().len());
    let (mut clones, mut counts) = (Vec::new(), Vec::new());
    for step in 1..=steps {
        let input = snapshots.last().ok_or("a snapshot")?.clone();
        let t = Instant::now();
        let mut fs = (*input).clone();
        clones.push(t.elapsed().as_secs_f64() * 1e6);
        fs.put(
            format!("step-{step}").as_bytes(),
            Node {
                kind: Kind::File { size: 0, data: DataRef { source: 0, offset: 0 } },
                meta: Meta::default(),
            },
        )
        .map_err(|e| e.to_string())?;
        fs.begin();
        let t = Instant::now();
        let links = fs.links();
        counts.push(t.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(links);
        snapshots.push(Rc::new(fs));
    }
    let held = mb() - base;
    let pct = |xs: &mut Vec<f64>, p: usize| {
        xs.sort_by(f64::total_cmp);
        xs[(p * xs.len() / 100).min(xs.len() - 1)]
    };
    println!(
        "{steps} steps: clone p50 {:.0} µs, p90 {:.0}, max {:.0}; links p50 {:.0} µs, p90 {:.0}, max {:.0}; the steps' snapshots hold {held:.1} MB, {:.2} MB a step",
        pct(&mut clones, 50), pct(&mut clones, 90), clones.iter().copied().fold(0.0, f64::max),
        pct(&mut counts, 50), pct(&mut counts, 90), counts.iter().copied().fold(0.0, f64::max),
        held / steps as f64
    );
    Ok(())
}
