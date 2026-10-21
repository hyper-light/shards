//! How long gzipping a layer takes, apart from reading it: each FILE read into memory, then
//! gzipped N times at LEVEL in pieces of 32 KiB, as an export writes a layer. Prints, per
//! file, n, p50, p90, p99 and max, the rate at p50, and the output's SHA-256 (the same for
//! every build that makes Go's bytes):
//! `cargo run --release -p shards-flate --example speed -- LEVEL N FILE...`.

use std::io::Write as _;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let level = args
        .next()
        .and_then(|l| l.parse().ok())
        .unwrap_or(shards_flate::DEFAULT_COMPRESSION);
    let n: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(5).max(1);
    for path in args {
        let input = std::fs::read(&path)?;
        let mut times: Vec<Duration> = Vec::with_capacity(n);
        let mut digest = String::new();
        for _ in 0..n {
            let at = Instant::now();
            let mut w = shards_flate::GzipWriter::new(Vec::with_capacity(input.len() / 8), level)?;
            for piece in input.chunks(32 * 1024) {
                w.write_all(piece)?;
            }
            let out = w.finish()?;
            times.push(at.elapsed());
            digest = Sha256::digest(&out).iter().map(|b| format!("{b:02x}")).collect();
        }
        times.sort();
        let at = |q: f64| {
            let i = ((times.len() as f64 * q) as usize).min(times.len() - 1);
            times.get(i).copied().unwrap_or_default()
        };
        let p50 = at(0.5);
        let _ = writeln!(
            std::io::stdout(),
            "{path}: n={n} p50={:.3}s p90={:.3}s p99={:.3}s max={:.3}s {:.0} MB/s sha256={digest}",
            p50.as_secs_f64(),
            at(0.9).as_secs_f64(),
            at(0.99).as_secs_f64(),
            times.last().copied().unwrap_or_default().as_secs_f64(),
            input.len() as f64 / p50.as_secs_f64().max(1e-9) / 1e6,
        );
    }
    Ok(())
}
