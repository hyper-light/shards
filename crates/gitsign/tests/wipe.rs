//! A private key block's armor is a secret key's, and what reads it leaves nothing of it
//! in memory it frees: an allocator that sees only the thread that asks (its own binary,
//! so no other test's allocator is this) looks in every heap block freed while the block
//! is read, its body grown past many reallocations, for the key's bytes and its base64.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use base64::Engine as _;
use shards_gitsign::armor;

/// The secret's bytes, which each 48 bytes of the body end with.
const MARK: &[u8; 24] = b"\x8fshards-secret-mark--\x00\x01\x7e";

struct Watching;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static FOUND: Cell<usize> = const { Cell::new(0) };
    static NEEDLES: Cell<Option<&'static [&'static [u8]]>> = const { Cell::new(None) };
}

// SAFETY: every call goes to the system allocator unchanged; a block is only read before
// it is freed, while it is still the caller's. realloc is GlobalAlloc's own (alloc, copy,
// dealloc), so a block a reallocation leaves behind is looked in too.
unsafe impl GlobalAlloc for Watching {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCHING.with(Cell::get) {
            // SAFETY: the block is the caller's to free, so its bytes are readable.
            let block = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            if let Some(needles) = NEEDLES.with(Cell::get)
                && needles.iter().any(|n| block.windows(n.len()).any(|w| w == *n))
            {
                FOUND.with(|c| c.set(c.get() + 1));
            }
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static WATCHING_ALLOCATOR: Watching = Watching;

/// What blocks `f` frees that hold a needle.
fn watched(needles: &'static [&'static [u8]], f: impl FnOnce()) -> usize {
    NEEDLES.with(|c| c.set(Some(needles)));
    FOUND.with(|c| c.set(0));
    WATCHING.with(|c| c.set(true));
    f();
    WATCHING.with(|c| c.set(false));
    FOUND.with(Cell::get)
}

/// A private key block of 1000 times MARK twice (48 bytes of body), in lines of 64
/// symbols; of 88, where what a read leaves of a line grows from 24 symbols to 36 and 52
/// (armor.rs keeps it); and of widths from 3 to 96. Each read whole at once, and again
/// with a last line that holds a carriage return, which makes the body be read in Go's
/// chunks from its start. Its body grows through io.ReadAll's chunks and many
/// reallocations of its own; nothing freed holds the mark, or any eight symbols of its
/// base64.
#[test]
fn a_private_key_blocks_body_leaves_nothing_behind() {
    let b64 = base64::engine::general_purpose::STANDARD;
    let stream = b64.encode([&MARK[..], &MARK[..]].concat()).repeat(1000);
    let mut needles: Vec<&'static [u8]> = vec![&MARK[..]];
    for at in 0..64 {
        needles.push(Box::leak(
            stream.as_bytes()[at..at + 8].to_vec().into_boxed_slice(),
        ));
    }
    let needles: &'static [&'static [u8]] = needles.leak();
    let widths: [&[usize]; 3] = [&[64], &[88], &[7, 93, 31, 64, 12, 88, 45, 96, 3, 77]];
    for (widths, last) in widths.iter().flat_map(|w| [(w, ""), (w, "QUJD\rQUJD\n")]) {
        let mut armored = String::from("-----BEGIN PGP PRIVATE KEY BLOCK-----\n\n");
        let mut at = 0;
        for &w in widths.iter().cycle() {
            if at == stream.len() {
                break;
            }
            let end = (at + w).min(stream.len());
            armored += stream.get(at..end).unwrap();
            armored.push('\n');
            at = end;
        }
        armored += last;
        armored += "-----END PGP PRIVATE KEY BLOCK-----\n";
        let mut read = 0;
        let found = watched(needles, || {
            let block = armor::decode(armored.as_bytes()).unwrap();
            let body = block.read_body().0.unwrap();
            read = body.len();
            assert!(body.windows(MARK.len()).any(|w| w == MARK));
        });
        let want = if last.is_empty() { 48_000 } else { 48_006 };
        assert_eq!(read, want, "{widths:?} {last:?}");
        assert_eq!(
            found, 0,
            "{widths:?} {last:?}: heap blocks freed holding the secret"
        );
    }
}
