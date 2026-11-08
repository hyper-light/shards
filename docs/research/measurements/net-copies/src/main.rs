//! What virtio-net's copies cost through guest memory's guard (D29), against the memcpy
//! it made before (audit V10, docs/research/platform-measurements.md M137).
//!
//! `copy` cases copy SIZE bytes between guest memory and host memory, `in` as a push takes
//! a guest frame and `out` as a delivery writes one: `plain` by memcpy, `access` by an
//! `Access`'s copies, the access held across the batch.
//!
//! `push` and `deliver` cases go through a real frame ring, a frame at a time, as the
//! device does, each frame with the one take of guest memory the queue's work on it makes
//! (its descriptor read, or its used entry written): `plain` as the device did before,
//! copying by memcpy outside that take; `access` as it does now, copying in it.
//!
//! `take` times taking and letting go of guest memory alone.
//!
//! `hot` copies one place in guest memory again and again; `spread` walks 256 MiB of it,
//! more than the caches hold. A sample is a batch's mean, a batch about 20 µs; the two arms
//! alternate a sample at a time, which goes first alternating too.
//!
//!     cargo run --release -- [N]
use std::sync::atomic::Ordering;
use std::time::Instant;

use shards_netring::Region;
use shards_vmm::memory::GuestMemory;

const BASE: u64 = 0x8000_0000;
const SPAN: usize = 256 << 20;
const LARGEST: usize = 65536;
/// Where the cases' descriptor and used ring lie: past the frames.
const RING: u64 = BASE + (SPAN + LARGEST) as u64;
const BATCH_NS: f64 = 20_000.0;
const PLAIN: usize = 0;
const ACCESS: usize = 1;

fn pct(xs: &[f64], p: usize) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    v[(p * v.len() / 100).min(v.len() - 1)]
}

/// `n` samples of each arm, `run(arm, next, reps)` doing `reps` copies, each where `next`
/// says in guest memory; one line of their n, p50, p90, p99, max and paired difference.
fn measure(
    label: &str,
    n: usize,
    spread: bool,
    stride: usize,
    mut run: impl FnMut(usize, &mut dyn FnMut() -> usize, usize),
) {
    let mut at = 0usize;
    let mut next = || {
        if spread {
            at = (at + stride) % (SPAN - stride);
        }
        at
    };
    // Batches as long as the slower arm's.
    let per = [PLAIN, ACCESS]
        .map(|arm| {
            let start = Instant::now();
            run(arm, &mut next, 1000);
            start.elapsed().as_nanos() as f64 / 1000.0
        })
        .into_iter()
        .fold(0.0, f64::max);
    let reps = ((BATCH_NS / per.max(0.1)) as usize).clamp(1, 1 << 20);
    let mut ns = [Vec::with_capacity(n), Vec::with_capacity(n)];
    for i in 0..n {
        for arm in if i % 2 == 0 {
            [PLAIN, ACCESS]
        } else {
            [ACCESS, PLAIN]
        } {
            let t = Instant::now();
            run(arm, &mut next, reps);
            ns[arm].push(t.elapsed().as_nanos() as f64 / reps as f64);
        }
    }
    let [plain, access] = &ns;
    let diffs: Vec<f64> = plain.iter().zip(access).map(|(p, a)| a - p).collect();
    // Bootstrap medians of the paired differences, by xorshift.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut boots: Vec<f64> = (0..2000)
        .map(|_| {
            let pick: Vec<f64> = (0..diffs.len())
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    diffs[(x % diffs.len() as u64) as usize]
                })
                .collect();
            pct(&pick, 50)
        })
        .collect();
    boots.sort_by(f64::total_cmp);
    let show = |v: &[f64]| {
        let max = v.iter().copied().fold(0.0, f64::max);
        format!(
            "{} / {:.1} / {:.1} / {:.1} / {max:.1}",
            v.len(),
            pct(v, 50),
            pct(v, 90),
            pct(v, 99)
        )
    };
    println!(
        "{label}: plain {} | access {} | {:+.1} [{:+.1}, {:+.1}]",
        show(plain),
        show(access),
        pct(&diffs, 50),
        boots[50],
        boots[1949],
    );
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2000);
    let mem = GuestMemory::anonymous(&[(BASE, SPAN + 2 * LARGEST)]).expect("guest memory");
    // Every page touched first, so that no sample pays its first touch.
    let chunk = vec![0x5au8; 1 << 20];
    let a = mem.access().expect("guest memory");
    for at in (0..SPAN + 2 * LARGEST).step_by(chunk.len()) {
        let len = chunk.len().min(SPAN + 2 * LARGEST - at);
        a.write(BASE + at as u64, &chunk[..len]).expect("guest memory");
    }
    drop(a);
    let mut host = vec![0xa5u8; SPAN + LARGEST];
    let region = Region::map(shards_netring::memory().expect("ring")).expect("ring");
    let (consumer_waits, producer_rings) = shards_netring::doorbell().expect("doorbell");
    let (_producer_waits, consumer_rings) = shards_netring::doorbell().expect("doorbell");
    let mut tx = region.producer(0, producer_rings);
    let mut rx = region.consumer(0, consumer_rings, consumer_waits);
    println!("ns a copy or frame: n / p50 / p90 / p99 / max; access - plain, paired median [95%]");
    measure("take", n, false, 64, |arm, _, reps| {
        for _ in 0..reps {
            if arm == ACCESS {
                drop(mem.access());
            }
        }
    });
    for size in [64, 1514, 9000, 16384, LARGEST] {
        // A cache line's multiple apart, as a ring's frames are.
        let stride = size.next_multiple_of(64);
        for spread in [false, true] {
            let place = if spread { "spread" } else { "hot" };
            for out in [false, true] {
                let way = if out { "out" } else { "in" };
                measure(
                    &format!("{size:>6} B copy {place} {way}"),
                    n,
                    spread,
                    stride,
                    |arm, next, reps| {
                        let a = mem.access().expect("guest memory");
                        for _ in 0..reps {
                            let off = next();
                            let gpa = BASE + off as u64;
                            let h = &mut host[off..off + size];
                            if arm == ACCESS {
                                let _ = if out { a.write(gpa, h) } else { a.read(gpa, h) };
                                continue;
                            }
                            let Ok(p) = mem.host_ptr(gpa, size) else {
                                return;
                            };
                            // SAFETY: `p` is `size` bytes of guest memory, checked; `h` as many
                            // of the host's.
                            unsafe {
                                if out {
                                    std::ptr::copy_nonoverlapping(h.as_ptr(), p, size);
                                } else {
                                    std::ptr::copy_nonoverlapping(p.cast_const(), h.as_mut_ptr(), size);
                                }
                            }
                        }
                    },
                );
            }
            // Through the ring, a take of guest memory a frame, as virtio-net's push.
            measure(
                &format!("{size:>6} B push {place}"),
                n,
                spread,
                stride,
                |arm, next, reps| {
                    for _ in 0..reps {
                        let gpa = BASE + next() as u64;
                        let Ok(a) = mem.access() else {
                            return;
                        };
                        // The frame's descriptor, as the queue takes it.
                        let mut desc = [0u8; 16];
                        let _ = a.read(RING, &mut desc);
                        if a.memory().host_ptr(gpa, size).is_err() {
                            return;
                        }
                        if arm == ACCESS {
                            let _ = tx.try_push_with(size, move |dst| {
                                // SAFETY: the record's `size` bytes.
                                let _ = unsafe { a.read_raw(gpa, dst, size) };
                                drop(a);
                            });
                        } else {
                            drop(a);
                            let _ = tx.try_push_with(size, |dst| {
                                if let Ok(p) = mem.host_ptr(gpa, size) {
                                    // SAFETY: guest memory checked by host_ptr, into the record.
                                    unsafe { std::ptr::copy_nonoverlapping(p.cast_const(), dst, size) };
                                }
                            });
                        }
                        let _ = rx.pop_frame(|_, _| ());
                    }
                },
            );
            // And a delivery's: its copy and its used entry.
            let mut used = 0u16;
            measure(
                &format!("{size:>6} B deliver {place}"),
                n,
                spread,
                stride,
                |arm, next, reps| {
                    for _ in 0..reps {
                        let gpa = BASE + next() as u64;
                        let _ = tx.try_push_with(size, |_| ());
                        used = used.wrapping_add(1);
                        let give_back = |a: &shards_vmm::memory::Access<'_>| {
                            let _ = a.write_obj(RING + 64, u64::from(used));
                            let _ = a.store_u16(RING + 128, used, Ordering::Release);
                        };
                        if arm == ACCESS {
                            let _ = rx.pop_frame(|frame, n| {
                                let Ok(a) = mem.access() else {
                                    return;
                                };
                                // SAFETY: the frame's `n` bytes.
                                let _ = unsafe { a.write_raw(gpa, frame, n) };
                                give_back(&a);
                            });
                        } else {
                            let _ = rx.pop(|n, copy| {
                                if let Ok(p) = mem.host_ptr(gpa, n) {
                                    copy(0, p, n);
                                }
                            });
                            if let Ok(a) = mem.access() {
                                give_back(&a);
                            }
                        }
                    }
                },
            );
        }
    }
}
