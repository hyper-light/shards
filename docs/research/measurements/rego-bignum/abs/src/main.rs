//! `abs` of 80000000000e00029174680, read from its operands' JSON as the fuzz target reads
//! them, its result written as JSON, as the fuzz target writes it: the time, the text's
//! length and the process's peak resident memory (getrusage).
use std::time::Instant;

use shards_rego::funcs::{self, Context};
use shards_rego::value::{self, Value};

fn main() {
    let operands = value::from_json("[\r80000000000e00029174680]\t").unwrap();
    let Value::Array(args) = &operands else {
        panic!("no array");
    };
    let abs = funcs::lookup("abs").unwrap();
    let t = Instant::now();
    let v = abs(&mut Context::default(), args).unwrap().unwrap();
    let json = value::to_json(&v).unwrap();
    let secs = t.elapsed().as_secs_f64();
    // SAFETY: getrusage writes a struct rusage, here zeroed and the process's own.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above; RUSAGE_SELF and a valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let peak = if cfg!(target_os = "macos") {
        ru.ru_maxrss as f64
    } else {
        ru.ru_maxrss as f64 * 1024.0
    };
    println!(
        "abs: {secs:.6} s, {} bytes of text, peak resident {:.1} MiB",
        json.len(),
        peak / (1 << 20) as f64
    );
}
