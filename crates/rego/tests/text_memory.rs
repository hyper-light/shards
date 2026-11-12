//! What writing an integer's text holds, against what D98's number-text bound says it
//! may (`digits::need`): every allocation counted by this binary's allocator, its peak
//! over each write and the caller's copy of the text (as `Number` makes it), for integers
//! dense and sparse, of ones and of zeros, at each size where the converter's levels,
//! transforms or text change. The bound is an upper bound of the peak by construction;
//! this holds it to the allocations themselves.

use std::alloc::{GlobalAlloc, Layout, System};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use num_bigint::BigUint;
use shards_rego::digits;

struct Counting;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

fn took(n: usize) {
    let live = LIVE.fetch_add(n as u64, Ordering::SeqCst) + n as u64;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

fn gave(n: usize) {
    LIVE.fetch_sub(n as u64, Ordering::SeqCst);
}

// SAFETY: each call is System's, and only counts what System answers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, as GlobalAlloc::alloc requires.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            took(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as alloc.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            took(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: ptr came from this allocator with this layout.
        unsafe { System.dealloc(ptr, layout) };
        gave(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Counted as the new block taken before the old is given back, as a moving
        // realloc holds both.
        // SAFETY: as GlobalAlloc::realloc requires of its caller.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            took(new_size);
            gave(layout.size());
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn limbs(v: &[u64]) -> BigUint {
    BigUint::from_slice(
        &v.iter()
            .flat_map(|&x| [x as u32, (x >> 32) as u32])
            .collect::<Vec<_>>(),
    )
}

/// The peak bytes over writing x·2^shift and copying its text into an `Rc<str>`, as
/// `Number` holds it, beyond what was held before.
fn peak(x: &BigUint, shift: u64) -> (u64, usize) {
    let base = LIVE.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let mut out = String::new();
    digits::write(&mut out, false, x, shift, u64::MAX).unwrap();
    let len = out.len();
    let text: Rc<str> = out.into();
    drop(text);
    (PEAK.load(Ordering::SeqCst) - base, len)
}

#[test]
fn writing_a_number_holds_no_more_than_its_bound_says() {
    let mut r = Rng(11);
    let mut cases: Vec<(String, BigUint, u64)> = Vec::new();
    // Sizes about the powers of two (a level more), and some between.
    let mut sizes = vec![1usize, 2, 3, 5, 24, 25, 26, 63, 64, 65, 100, 1000];
    for k in [10u32, 12, 14, 16] {
        sizes.extend([(1 << k) - 1, 1 << k, (1 << k) + 1]);
    }
    sizes.extend([3 << 13, 5 << 14, 150_000]);
    for n in sizes {
        let dense: Vec<u64> = (0..n).map(|_| r.next()).collect();
        cases.push((format!("dense {n}"), limbs(&dense), 0));
        let m = BigUint::from(r.next() | 1 << 63);
        let shift = (n as u64).saturating_sub(1) * 64 + r.next() % 64;
        cases.push((format!("mantissa shifted {n}"), m.clone(), shift));
        let ones = (BigUint::from(1u32) << (n as u64 * 64)) - 1u32;
        cases.push((format!("ones {n}"), ones, 0));
        let mut mixed = dense.clone();
        for x in mixed.iter_mut().step_by(2) {
            *x = 0;
        }
        cases.push((format!("half zero {n}"), limbs(&mixed), 0));
    }
    let mut over = Vec::new();
    for (what, x, shift) in &cases {
        let bits = x.bits() + shift;
        let (held, len) = peak(x, *shift);
        let need = digits::need(bits);
        eprintln!(
            "{what}: {bits} bits, {len} digits, peak {held} B, bound {need} B ({:.2}), {:.2} a digit",
            held as f64 / need as f64,
            held as f64 / len as f64
        );
        if held > need {
            over.push(format!("{what}: held {held} past its bound {need}"));
        }
    }
    assert!(over.is_empty(), "{}", over.join("\n"));
}
