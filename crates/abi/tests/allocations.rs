//! What encoding and decoding a spec allocates (audit D10), counted by an allocator that
//! sees only the thread that asks: its own binary, so no other test's allocator is this.
#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use shards_abi::run::{Size, Spec};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static REALLOCS: Cell<usize> = const { Cell::new(0) };
}

fn bump(counter: &'static std::thread::LocalKey<Cell<usize>>) {
    if COUNTING.with(Cell::get) {
        counter.with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(&ALLOCS);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(&REALLOCS);
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: Counting = Counting;

/// `f`'s result, and the allocations and reallocations it made on this thread.
fn counted<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOCS.with(|c| c.set(0));
    REALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(Cell::get), REALLOCS.with(Cell::get))
}

/// The audit's launch: three arguments and 64 environment entries, some 4 KiB.
fn launch() -> Spec {
    Spec {
        argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"exec \"$@\"".to_vec()],
        env: (0..64)
            .map(|i| format!("VARIABLE_{i:02}={}", "v".repeat(40)).into_bytes())
            .collect(),
        cwd: b"/work".to_vec(),
        user: b"1000:1000".to_vec(),
        hostname: b"0123456789ab".to_vec(),
        tty: Some(Size { rows: 24, cols: 80 }),
        resolv: None,
        stdin: true,
        builtin: 0,
        hosts: Vec::new(),
        domainname: Vec::new(),
    }
}

#[test]
fn a_spec_is_measured_without_encoding_and_encoded_in_one_allocation() {
    let spec = launch();
    let (len, allocs, _) = counted(|| spec.encoded_len());
    assert_eq!(allocs, 0, "measuring allocates");
    let (bytes, allocs, reallocs) = counted(|| spec.encode());
    assert_eq!((allocs, reallocs), (1, 0), "encoding grows its buffer");
    assert_eq!(len, Some(bytes.len()));
    assert_eq!(bytes.capacity(), bytes.len());

    // Appended to a payload's head, it grows that once.
    let mut payload = vec![7u8; 17];
    let ((), allocs, reallocs) = counted(|| spec.encode_into(&mut payload));
    assert_eq!(allocs + reallocs, 1);
    assert_eq!(payload.get(17..), Some(&bytes[..]));

    // Every field counts toward the length.
    for spec in [
        Spec::default(),
        Spec {
            tty: None,
            ..launch()
        },
    ] {
        assert_eq!(spec.encoded_len(), Some(spec.encode().len()));
    }
}

#[test]
fn a_spec_decodes_with_one_allocation_per_list_and_string() {
    let spec = launch();
    let bytes = spec.encode();
    let (decoded, allocs, reallocs) = counted(|| Spec::decode(&bytes));
    assert_eq!(decoded.as_ref(), Some(&spec));
    // Two lists, their 67 strings, and cwd, user and hostname.
    assert_eq!((allocs, reallocs), (2 + 3 + 64 + 3, 0));
}
