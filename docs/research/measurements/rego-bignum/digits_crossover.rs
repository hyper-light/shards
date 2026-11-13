//! PM M531's crossovers for `digits` (crates/rego/src/digits.rs takes this by #[path]):
//!
//! - `schoolbook_against_transforms`: a product by schoolbook rows against one by
//!   transforms as `mul` makes it past `SCHOOL` (by pieces where the other operand is far
//!   longer), for operands of s limbs each (s × s) and of s by 64·s limbs (the shorter by
//!   a far longer, as a sparse integer's upper part meets T[k]), s from 1 to 128, then to
//!   256 four at a time;
//! - `against_num_bigint`: an integer's digits by the converter (as `write` makes them)
//!   against num-bigint 0.4.8's `to_str_radix(10)`, for random
//!   integers of n limbs, n from 1 to 2^16 a quarter more each step (400 samples a size),
//!   then four far larger, fewer samples each (their n printed).
//!
//! Each sample draws fresh operands; the two ways interleave, their order alternating, 20
//! warm-ups first. Run built for release, unclamped (on a Mac, through
//! macos-sched/launchd-run.sh):
//!
//!   cargo test -p shards-rego --release --lib digits::crossover -- --ignored --nocapture --test-threads 1
//!
//! Each line: the size, then each way's n, p50, p90, p99 and max in ns, then the ratio of
//! their p50s; last, the least size past which the second way's p50 is the lower at every
//! size measured.

use std::hint::black_box;
use std::time::Instant;

use super::tests::Rng;
use super::*;

fn pct(v: &mut [u128], q: f64) -> u128 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn row(v: &mut [u128]) -> String {
    format!(
        "{} {} {} {} {}",
        v.len(),
        pct(v, 0.5),
        pct(v, 0.9),
        pct(v, 0.99),
        pct(v, 1.0)
    )
}

/// Times two ways, interleaved, their order alternating: `samples` each after `warm`.
fn race(
    samples: usize,
    warm: usize,
    mut first: impl FnMut(usize) -> u128,
    mut second: impl FnMut(usize) -> u128,
) -> (Vec<u128>, Vec<u128>) {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for i in 0..warm + samples {
        let (x, y) = if i % 2 == 0 {
            let x = first(i);
            (x, second(i))
        } else {
            let y = second(i);
            (first(i), y)
        };
        if i >= warm {
            a.push(x);
            b.push(y);
        }
    }
    (a, b)
}

/// The least size past which `second`'s p50 was the lower at every size measured.
fn crossover(rows: &[(usize, u128, u128)]) -> Option<usize> {
    let mut from = None;
    for &(size, first, second) in rows {
        if second < first {
            from = from.or(Some(size));
        } else {
            from = None;
        }
    }
    from
}

/// The converter's digits of x, as `write` makes them.
fn convert(x: &BigUint) -> Result<String, Error> {
    let mut out = String::new();
    text(&mut out, false, &limbs_of_shifted(x, 0)?)?;
    Ok(out)
}

/// A product of two operands' limbs, as each way makes it.
type Product<'a> = dyn Fn(&[u64], &[u64]) -> Vec<u64> + 'a;

fn decimal(r: &mut Rng, n: usize) -> Vec<u64> {
    (0..n).map(|_| r.next() % BASE).collect()
}

#[test]
#[ignore = "a measurement: run it built for release, with --ignored"]
fn schoolbook_against_transforms() {
    let mut r = Rng(17);
    println!(
        "{} {}: shape s school(n p50 p90 p99 max) transforms(n p50 p90 p99 max) school/transforms",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    for (shape, times) in [("s×s", 1usize), ("s×64s", 64)] {
        let mut rows = Vec::new();
        for s in (1..=128usize).chain((132..=256).step_by(4)) {
            let pairs: Vec<(Vec<u64>, Vec<u64>)> = (0..420)
                .map(|_| (decimal(&mut r, s), decimal(&mut r, s * times)))
                .collect();
            let time = |f: &Product<'_>, i: usize| {
                let (a, b) = &pairs[i];
                let t = Instant::now();
                black_box(f(black_box(a), black_box(b)));
                t.elapsed().as_nanos()
            };
            let scratch = std::cell::RefCell::new(Scratch::default());
            let (mut a, mut b) = race(
                400,
                20,
                |i| time(&|a, b| school(a, b).unwrap(), i),
                |i| time(&|a, b| by_transforms(a, b, &mut scratch.borrow_mut()).unwrap(), i),
            );
            let (pa, pb) = (pct(&mut a, 0.5), pct(&mut b, 0.5));
            println!(
                "{shape} {s} {} {} {:.2}",
                row(&mut a),
                row(&mut b),
                pa as f64 / pb as f64
            );
            rows.push((s, pa, pb));
        }
        println!("{shape}: transforms the cheaper from s = {:?}", crossover(&rows));
    }
}

#[test]
#[ignore = "a measurement: run it built for release, with --ignored"]
fn against_num_bigint() {
    let mut r = Rng(19);
    println!(
        "{} {}: limbs num_bigint(n p50 p90 p99 max) write(n p50 p90 p99 max) num_bigint/write",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let mut sizes: Vec<usize> = Vec::new();
    let mut n = 1.0f64;
    while n <= 65536.0 {
        let k = n.round() as usize;
        if sizes.last() != Some(&k) {
            sizes.push(k);
        }
        n *= 1.25;
    }
    let mut rows = Vec::new();
    for (i, &n) in sizes
        .iter()
        .chain([131_072usize, 262_144, 524_288, 1_048_576].iter())
        .enumerate()
    {
        let large = i >= sizes.len();
        let (samples, warm) = if large { (5, 1) } else { (400, 20) };
        let xs: Vec<BigUint> = (0..samples + warm)
            .map(|_| {
                let limbs: Vec<u32> = (0..2 * n).map(|_| r.next() as u32).collect();
                BigUint::from_slice(&limbs)
            })
            .collect();
        let (mut a, mut b) = race(
            samples,
            warm,
            |i| {
                let t = Instant::now();
                black_box(xs[i].to_str_radix(10));
                t.elapsed().as_nanos()
            },
            |i| {
                let t = Instant::now();
                black_box(convert(black_box(&xs[i])).unwrap());
                t.elapsed().as_nanos()
            },
        );
        let (pa, pb) = (pct(&mut a, 0.5), pct(&mut b, 0.5));
        println!("{n} {} {} {:.2}", row(&mut a), row(&mut b), pa as f64 / pb as f64);
        if !large {
            rows.push((n, pa, pb));
        }
    }
    println!("write the cheaper from {:?} limbs", crossover(&rows));
}
