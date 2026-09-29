//! Prints how long, in µs, a new process's first and second 32-byte fills from AWS-LC
//! take. The first seeds AWS-LC's DRBG from its entropy source.
use std::time::Instant;

use aws_lc_rs::rand::{SecureRandom, SystemRandom};

fn main() {
    let rng = SystemRandom::new();
    let mut buf = [0u8; 32];
    let t0 = Instant::now();
    let first = rng.fill(&mut buf);
    let t1 = Instant::now();
    let second = rng.fill(&mut buf);
    let t2 = Instant::now();
    if first.is_err() || second.is_err() {
        std::process::exit(1);
    }
    println!("{} {}", (t1 - t0).as_micros(), (t2 - t1).as_micros());
}
