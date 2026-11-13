//! A member's typed stdin leaves nothing of itself in memory let go of (D242: no credential
//! value anywhere, and what a person types into a member is one as often as not). Memory given
//! back keeps what it held until it is reused, so each block given back while a test watches
//! is scanned for the bytes it typed, by an allocator that sees only the thread that asks, in a
//! binary of its own, as `allocations.rs` counts a stats answer's.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use shards_api::image::wire::Input;

/// What the test types: in no other data of this binary's.
const TYPED: &[u8] = b"shards-typed-input-wipe-6f1c2a9e";

struct Watched;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static FOUND: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: every call goes on to the system allocator with what it was given; a block is read
// only while it is still the caller's, before it goes back. `realloc` is the trait's own, which
// gives the old block back through `dealloc`, so a moved block's old copy is scanned too.
unsafe impl GlobalAlloc for Watched {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCHING.with(Cell::get) && layout.size() >= TYPED.len() {
            // SAFETY: `ptr` is a block of `layout.size()` bytes this allocator gave out, the
            // caller's until it goes back below.
            let block = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            if block.windows(TYPED.len()).any(|w| w == TYPED) {
                FOUND.with(|f| f.set(true));
            }
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Watched = Watched;

/// Runs `f` watching what this thread gives back: whether any block held [`TYPED`].
fn gives_back_typed(f: impl FnOnce()) -> bool {
    FOUND.with(|x| x.set(false));
    WATCHING.with(|w| w.set(true));
    f();
    WATCHING.with(|w| w.set(false));
    FOUND.with(Cell::get)
}

/// An INPUT frame, decoded and let go of. Do: decode a member's typed bytes from a payload the
/// test wipes itself, read them back, then let the input go. Expect: the bytes read back as
/// typed; `Debug` saying how many there are, never what; and no block given back holding them.
#[test]
fn an_input_frames_typed_bytes_are_wiped_as_it_drops() {
    let mut payload = zeroize::Zeroizing::new(Input::head(3).to_vec());
    payload.extend_from_slice(TYPED);
    let kept = gives_back_typed(|| {
        let input = Input::decode(&payload).unwrap();
        assert_eq!((input.member, input.bytes.0.as_slice()), (3, TYPED));
        assert_eq!(
            format!("{input:?}"),
            format!("Input {{ member: 3, bytes: Secret({} bytes) }}", TYPED.len())
        );
        drop(input);
    });
    assert!(!kept, "a member's typed bytes were given back unwiped");
}
