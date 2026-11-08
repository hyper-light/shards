//! What growing a process's descriptor (or handle) table costs while other threads share it
//! (PM M470's question on every host): Linux's `expand_fdtable` waits an RCU grace period
//! when the table is shared, which warm.rs's `reserve_descriptors` takes off a run's
//! hand-over. This times each duplicate of one descriptor, from HELD open to HELD + EXTRA,
//! in a process whose THREADS other threads sit blocked, as a warm VM's do, and prints one
//! line per process: the slowest duplicate, where it fell, and the median.
//!
//! Build: rustc -O --edition 2021 fdgrow.rs -o fdgrow
//! Run:   ./fdgrow ROUNDS [HELD] [EXTRA] [THREADS]   (each round its own child process)

use std::io::Write as _;
use std::time::Instant;

fn dup_cost(held: usize, extra: usize, threads: usize) -> Result<(), String> {
    let base = std::fs::File::open(std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let stop_rx = std::sync::Mutex::new(stop_rx);
    std::thread::scope(|s| -> Result<(), String> {
        for i in 0..threads {
            let rx = &stop_rx;
            std::thread::Builder::new()
                .name(format!("idle-{i}"))
                .spawn_scoped(s, move || {
                    let _ = rx.lock().map(|r| r.recv());
                })
                .map_err(|e| e.to_string())?;
        }
        let mut kept = Vec::with_capacity(held + extra);
        for _ in 0..held {
            kept.push(base.try_clone().map_err(|e| e.to_string())?);
        }
        let mut costs = Vec::with_capacity(extra);
        for _ in 0..extra {
            let t = Instant::now();
            kept.push(base.try_clone().map_err(|e| e.to_string())?);
            costs.push(t.elapsed().as_nanos());
        }
        let (worst_at, worst) = costs
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(_, c)| c)
            .ok_or("no duplicates")?;
        let mut sorted = costs.clone();
        sorted.sort_unstable();
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        let _ = writeln!(
            std::io::stdout(),
            "worst_ns {worst} at_open {} median_ns {median}",
            held + worst_at + 1
        );
        drop(stop_tx);
        Ok(())
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    if args.get(1).map(String::as_str) == Some("child") {
        if let Err(e) = dup_cost(num(2, 64), num(3, 256), num(4, 8)) {
            let _ = writeln!(std::io::stderr(), "fdgrow: {e}");
            std::process::exit(1);
        }
        return;
    }
    let rounds = num(1, 100);
    let (held, extra, threads) = (num(2, 64), num(3, 256), num(4, 8));
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "fdgrow: {e}");
            std::process::exit(1);
        }
    };
    let mut worst = Vec::with_capacity(rounds);
    let mut medians = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let out = std::process::Command::new(&exe)
            .args(["child", &held.to_string(), &extra.to_string(), &threads.to_string()])
            .output();
        let Ok(out) = out else { continue };
        let text = String::from_utf8_lossy(&out.stdout);
        let words: Vec<&str> = text.split_whitespace().collect();
        if let (Some(w), Some(m)) = (words.get(1), words.get(5)) {
            if let (Ok(w), Ok(m)) = (w.parse::<u128>(), m.parse::<u128>()) {
                worst.push(w);
                medians.push(m);
            }
        }
        let _ = std::io::stdout().write_all(&out.stdout);
    }
    worst.sort_unstable();
    medians.sort_unstable();
    let q = |v: &[u128], p: usize| v.get((v.len().saturating_sub(1)) * p / 100).copied().unwrap_or(0);
    let _ = writeln!(
        std::io::stdout(),
        "SUMMARY n {} held {held} extra {extra} threads {threads} | slowest dup per process ns: p50 {} p90 {} p99 {} max {} | median dup ns: p50 {}",
        worst.len(),
        q(&worst, 50),
        q(&worst, 90),
        q(&worst, 99),
        worst.last().copied().unwrap_or(0),
        q(&medians, 50)
    );
}
