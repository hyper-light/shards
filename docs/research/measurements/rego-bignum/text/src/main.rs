//! A builtin on its operands' JSON, its answer as JSON: time, length, peak RSS
//! (getrusage), and the holdable the call had (u64::MAX, or the third argument; `bound`
//! names digits::need of the answer's bits as read from the first operand's float).
use std::time::Instant;

use shards_rego::digits;
use shards_rego::funcs::{self, Context};
use shards_rego::value::{self, Value};

fn peak() -> f64 {
    // SAFETY: getrusage writes a struct rusage, here zeroed and the process's own.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above; RUSAGE_SELF and a valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let max = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024.0
    }
}

/// macOS's lifetime peak physical footprint, what memory pressure and jetsam count (the
/// resident peak also counts pages the allocator keeps reusable); the resident peak
/// elsewhere.
#[cfg(target_os = "macos")]
fn footprint() -> f64 {
    // SAFETY: a rusage_info_v4 of integers, which proc_pid_rusage fills for this process.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: as above; the flavor names the struct given.
    let rc = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            (&raw mut info).cast(),
        )
    };
    if rc == 0 {
        info.ri_lifetime_max_phys_footprint as f64
    } else {
        f64::NAN
    }
}

#[cfg(not(target_os = "macos"))]
fn footprint() -> f64 {
    peak()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (name, json) = (&args[1], &args[2]);
    let operands = value::from_json(json).unwrap();
    let Value::Array(ops) = &operands else {
        panic!("no array");
    };
    let bits = match ops.first() {
        Some(Value::Number(n)) => n.to_float().map_or(0, |f| f.go_exponent().max(0) as u64),
        _ => 0,
    };
    let holdable = match args.get(3).map(String::as_str) {
        None => u64::MAX,
        Some("bound") => digits::need(bits),
        Some(h) => h.parse().unwrap(),
    };
    let builtin = funcs::lookup(name).unwrap();
    let before = peak();
    let t = Instant::now();
    let got = builtin(
        &mut Context {
            holdable,
            ..Context::default()
        },
        ops,
    );
    let answer = got.map(|v| v.map(|v| value::to_json(&v).unwrap()));
    let secs = t.elapsed().as_secs_f64();
    let mib = |b: f64| b / (1 << 20) as f64;
    match answer {
        Ok(Some(text)) => println!(
            "{name} {json}: {secs:.6} s, {} bytes, peak {:.1} MiB (start {:.1}), footprint {:.1} MiB, {bits} bits, bound {} B",
            text.len(),
            mib(peak()),
            mib(before),
            mib(footprint()),
            digits::need(bits)
        ),
        Ok(None) => println!("{name} {}: undefined, {secs:.6} s", args[2]),
        Err(e) => println!(
            "{name} {}: refused in {secs:.6} s, peak {:.1} MiB: {}",
            args[2],
            mib(peak()),
            e.message()
        ),
    }
}
