//! Where `Float::shortest` turns from Go's way (x's exact decimal and its neighbours',
//! whole) to their leading digits only (`leading`): both ways timed for 64-bit floats
//! whose exact decimal has D digits, D from 50 to 2,000 a tenth more each step and on to
//! 20,000 a quarter more, each sign of exponent, a float drawn afresh for each sample and the two ways interleaved,
//! their order alternating. crates/rego/src/number.rs takes it by #[path]. Run it built
//! for release, unclamped (on a Mac, through macos-sched/launchd-run.sh):
//!
//!   cargo test -p shards-rego --release --lib crossover -- --ignored --nocapture
//!
//! It prints, at each D, each way's n, p50, p90, p99 and max in ns, and Go's way's p50 over
//! the leading digits' (`-` where those do not apply: the decimal too short); then, for
//! each sign, the least D/read (`digits_read`) past which the leading digits' p50 is the
//! lower at every D measured, and the greater of the two.

use std::hint::black_box;
use std::time::Instant;

use super::tests::Rng;
use super::*;

fn pct(v: &mut [u128], q: f64) -> u128 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn row(v: &mut [u128]) -> String {
    let n = v.len();
    format!(
        "{n} {} {} {} {}",
        pct(v, 0.5),
        pct(v, 0.9),
        pct(v, 0.99),
        pct(v, 1.0)
    )
}

#[test]
#[ignore = "a measurement: run it built for release, with --ignored"]
fn crossover() {
    let mut r = Rng(7);
    let read = digits_read(64);
    let samples = 400;
    let warm = 20;
    println!(
        "{} {}: sign D D/read exact(n p50 p90 p99 max) leading(n p50 p90 p99 max) exact/leading",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let mut most: Option<f64> = None;
    for sign in [1i64, -1] {
        let mut wins = Vec::new();
        let mut want = 50.0f64;
        while want <= 20_000.0 {
            // x = mant × 2^exp, mant of 64 bits: its decimal has (64 + exp)·log10 2 digits
            // for exp ≥ 0, and 64·log10 2 + |exp|·log10 5 for exp < 0.
            let size = if sign > 0 {
                want / LOG10_2 - 64.0
            } else {
                (want - 64.0 * LOG10_2) / LOG10_5
            };
            let exp = sign * size.max(1.0) as i64;
            let (mut exact, mut led) = (Vec::new(), Vec::new());
            let mut applies = true;
            for i in 0..samples + warm {
                let x = Float {
                    prec: 64,
                    neg: false,
                    mant: BigUint::from(r.next() | 1 << 63),
                    exp,
                };
                let (m, e) = x.halves();
                let go = || {
                    let t = Instant::now();
                    let d = black_box(x.shortest_exact(&m, e));
                    (t.elapsed().as_nanos(), d)
                };
                let lead = || {
                    let t = Instant::now();
                    let d = black_box(shortest_leading(&m, e, read));
                    (t.elapsed().as_nanos(), d)
                };
                let ((tg, dg), (tl, dl)) = if i % 2 == 0 {
                    let g = go();
                    (g, lead())
                } else {
                    let l = lead();
                    (go(), l)
                };
                match dl {
                    Some(dl) => assert_eq!((dl.mant, dl.exp, dl.sig), (dg.mant, dg.exp, dg.sig)),
                    None => applies = false,
                }
                if i >= warm {
                    exact.push(tg);
                    led.push(tl);
                }
            }
            let digits = decimal_len(&(BigUint::from(1u64 << 63) << 1u32), exp - 1);
            let ratio = digits / read as f64;
            if applies {
                let (pe, pl) = (pct(&mut exact, 0.5), pct(&mut led, 0.5));
                wins.push((ratio, pl < pe));
                println!(
                    "{sign:+} {digits} {ratio:.2} {} {} {:.2}",
                    row(&mut exact),
                    row(&mut led),
                    pe as f64 / pl as f64
                );
            } else {
                println!("{sign:+} {digits} {ratio:.2} {} - -", row(&mut exact));
            }
            want *= if want < 2_000.0 { 1.1 } else { 1.25 };
        }
        // The least ratio past which the leading digits are the cheaper at every D.
        let from = wins
            .iter()
            .rposition(|(_, cheaper)| !cheaper)
            .map_or(wins.first(), |i| wins.get(i + 1))
            .map(|(ratio, _)| *ratio);
        println!("{sign:+} leading digits cheaper at every D from D/read {from:?}");
        most = match (most, from) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    println!("both signs: from D/read {most:?}");
}
