//! What the probes share: the program each engine runs, how a sample is summarized, and
//! the process's peak memory.

/// The program each engine runs as code mode runs one: an async body awaiting the host's
/// calls, with JSON, destructuring, a loop and a closure; its value, JSON.
pub const PROGRAM: &str = r#"(async () => {
  const me = await host.whoami();
  const peers = JSON.parse(await host.peers());
  let sent = [];
  for (const { name, send } of peers) {
    if (send) sent.push(await host.send(name, `hello from ${me}`));
  }
  const total = sent.reduce((a, b) => a + b, 0);
  return JSON.stringify({ me, sent, total });
})()"#;

/// `values`, sorted: n, and the nearest-rank p50, p90, p99 and max.
pub fn summary(values: &mut [f64]) -> String {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let rank = |p: f64| values[((p * n as f64).ceil() as usize).clamp(1, n) - 1];
    format!(
        "n={n} p50={:.1} p90={:.1} p99={:.1} max={:.1}",
        rank(0.50),
        rank(0.90),
        rank(0.99),
        values[n - 1]
    )
}

/// The process's peak resident memory, in KiB (VmHWM).
pub fn peak_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
        .unwrap_or(0)
}

/// What a probe was asked: its measure, and how many.
pub fn asked() -> (String, usize) {
    let args: Vec<String> = std::env::args().collect();
    let measure = args.get(1).cloned().unwrap_or_default();
    let n = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(100);
    (measure, n)
}

/// Batches the per-call cost is measured in: their count, and the calls in each.
pub const BATCHES: usize = 50;
pub const BATCH: usize = 10_000;
