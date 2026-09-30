//! What writing an EROFS image allocates (audit D12), counted by an allocator that sees
//! only the thread that asks, in a binary of its own: allocations, reallocations, and
//! the peak of requested live bytes, over the writer alone, into a sink that keeps
//! nothing, for the audit's trees of 10,000 files.
//!
//!     cargo test --release -p shards-image --test allocations -- --nocapture
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_possible_truncation)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;

use shards_image::erofs::{self, DataRef, Kind, Meta, Node, Source, Tree};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static REALLOCS: Cell<usize> = const { Cell::new(0) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(allocs: usize, reallocs: usize, grew: isize) {
    if COUNTING.with(Cell::get) {
        ALLOCS.with(|c| c.set(c.get() + allocs));
        REALLOCS.with(|c| c.set(c.get() + reallocs));
        let live = LIVE.with(|c| {
            c.set(c.get() + grew);
            c.get()
        });
        PEAK.with(|p| p.set(p.get().max(live)));
    }
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(1, 0, layout.size() as isize);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(0, 0, -(layout.size() as isize));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(0, 1, new_size as isize - layout.size() as isize);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: Counting = Counting;

/// Every file's bytes are zeros.
struct Zeros;

impl Source for Zeros {
    fn read_at(&mut self, _: DataRef, _: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }
}

/// Keeps nothing: counts what it is given, and hashes it (FNV-1a), so writers can be
/// compared byte for byte.
struct Sink(u64, u64);

impl io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        for &b in buf {
            self.1 = (self.1 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn tree(files: usize, size: u64, xattr: usize) -> Tree {
    let meta = |mode| Meta {
        mode,
        ..Meta::default()
    };
    let mut tree = Tree::new(meta(0o755));
    let dir = tree
        .insert(
            Tree::ROOT,
            b"files",
            Node {
                kind: Kind::Dir(BTreeMap::new()),
                meta: meta(0o755),
            },
        )
        .unwrap();
    for i in 0..files {
        let mut m = meta(0o644);
        if xattr > 0 {
            m.xattrs.insert(b"user.big".to_vec(), vec![7; xattr]);
        }
        let data = DataRef { source: 0, offset: 0 };
        let node = Node {
            kind: Kind::File { size, data },
            meta: m,
        };
        tree.insert(dir, format!("file-{i:05}").as_bytes(), node).unwrap();
    }
    tree
}

/// Allocations, reallocations and peak requested live bytes of writing `tree`.
fn count(tree: &Tree) -> (usize, usize, isize, u64, u64) {
    let mut sink = Sink(0, 0xcbf2_9ce4_8422_2325);
    for c in [&ALLOCS, &REALLOCS] {
        c.with(|c| c.set(0));
    }
    LIVE.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    erofs::write(tree, &mut Zeros, &mut sink).unwrap();
    COUNTING.with(|c| c.set(false));
    (
        ALLOCS.with(Cell::get),
        REALLOCS.with(Cell::get),
        PEAK.with(Cell::get),
        sink.0,
        sink.1,
    )
}

#[test]
fn what_writing_ten_thousand_files_allocates() {
    // The images' hashes are those the writer made before it streamed its metadata
    // (PM M60): the same bytes.
    for (what, size, xattr, image) in [
        ("empty files", 0, 0, 0xb5f1_fb8e_e1d7_ecdb),
        ("512-byte inline files", 512, 0, 0x7bbe_af2d_65df_07d5),
        ("4,095-byte plain files", 4095, 0, 0x1f6a_1376_0474_7707),
        ("4,096-byte plain files", 4096, 0, 0x1f40_7cae_04c4_6da3),
        (
            "empty files with 1,024-byte xattrs",
            0,
            1024,
            0x03a2_23ab_06ad_2181,
        ),
    ] {
        let t = tree(10_000, size, xattr);
        let (allocs, reallocs, peak, bytes, hash) = count(&t);
        assert_eq!(hash, image, "{what}: other bytes");
        // Per image, not per inode, but for one small vector per inode with xattrs; the
        // peak is the 1 MiB file buffer and the inodes' layout, not their records.
        assert!(
            allocs <= 50 + if xattr > 0 { 10_000 } else { 0 },
            "{what}: {allocs} allocations"
        );
        assert!(reallocs <= 50, "{what}: {reallocs} reallocations");
        assert!(peak < 5 << 20, "{what}: a peak of {peak} bytes");
        println!(
            "{what:36} allocations {allocs:6} | reallocations {reallocs:6} | peak {peak:10} bytes | image {bytes} bytes, {hash:016x}"
        );
    }
}

/// What a tree retains of its history (audit D11): 10,000 files with a 1,024-byte xattr
/// each, inserted once, then over themselves four times, as layers replacing them would;
/// the requested live bytes before and after compacting, beside one generation's.
#[test]
fn a_compacted_tree_holds_the_image_not_its_history() {
    let live = || LIVE.with(Cell::get);
    let build = |generations: usize| {
        let mut t = tree(10_000, 0, 1024);
        let dir = t.child(Tree::ROOT, b"files").unwrap();
        for _ in 1..generations {
            for i in 0..10_000 {
                let mut m = Meta {
                    mode: 0o644,
                    ..Meta::default()
                };
                m.xattrs.insert(b"user.big".to_vec(), vec![7; 1024]);
                let data = DataRef { source: 0, offset: 0 };
                t.insert(
                    dir,
                    format!("file-{i:05}").as_bytes(),
                    Node {
                        kind: Kind::File { size: 0, data },
                        meta: m,
                    },
                )
                .unwrap();
            }
        }
        t
    };
    LIVE.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let one = build(1);
    let one_live = live();
    drop(one);
    let base = live();
    let mut five = build(5);
    let before = live() - base;
    five.compact();
    let after = live() - base;
    COUNTING.with(|c| c.set(false));
    println!("one generation {one_live} bytes; five: {before} before compacting, {after} after");
    assert!(
        after <= one_live + one_live / 10,
        "{after} bytes kept of {one_live}"
    );
    assert!(before > 4 * one_live, "the history was not there to drop");
}
