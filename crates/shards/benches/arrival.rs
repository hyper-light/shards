//! Open-loop arrival benchmark: `shards run` requests arriving at a rate, whether or not
//! the ones before them are done, as users' requests do. A closed loop, which sends the
//! next request only once the last has answered, slows with the system it measures and
//! hides its queueing (Schroeder, Wierman and Harchol-Balter, "Open Versus Closed: A
//! Cautionary Tale", NSDI 2006).
//!
//! The image bench's image and guest (benches/image.rs), its template saved and its
//! working set recorded first. Then for each rate in `--rates` (per second; default
//! 2,5,10,20,40), for `--secs` seconds (10): arrivals a Poisson process of that rate,
//! from a seeded generator, each `shards run --rm --pull never IMAGE exit 0` spawned at
//! its time on a thread of its own. The rates run in order, one daemon throughout, so its
//! pools size themselves to the demand as it grows (D26).
//!
//! - `rate_R`: each request's latency, from the time it was due, not the time it was
//!   sent, so a harness running late cannot hide a queue (coordinated omission): its
//!   client's exit, the command having run.
//! - `rate_R_sent`, `rate_R_failed`: the requests sent, and those that failed; and
//!   `rate_R_done`: the rate they were answered at, per second, over the step's span to
//!   its last answer.
//!
//! `cargo bench -p shards --bench arrival [-- --rates 2,5,10 --secs 10]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[cfg(unix)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(unix)]
mod support;

#[cfg(not(unix))]
fn main() {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "SKIP: the arrival benchmark needs a Unix host");
}

#[cfg(unix)]
fn main() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use support::{report, run_env, stats};

    const UNPREFETCHED: usize = if shards_vmm::vm::RESTORES_RECORD { 3 } else { 2 };
    let rates: Vec<f64> = support::option("--rates")
        .unwrap_or_else(|| "2,5,10,20,40".into())
        .split(',')
        .map(|r| r.parse().expect("--rates R,R,..."))
        .collect();
    let secs: f64 = support::option("--secs").map_or(10.0, |v| v.parse().expect("--secs S"));
    if common::cannot_run_vms() || !shards_vmm::vm::SNAPSHOTS {
        return;
    }
    let home = common::workspace().join(format!("target/bench/arrival-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (image, _) = common::served();
    let (kernel, init) = (
        common::kernel().to_str().unwrap(),
        common::guest_init().to_str().unwrap(),
    );
    run_env(&strings(&["pull", "-q", &image]), false, &env);
    run_env(
        &strings(&["guest", "use", "--kernel", kernel, "--init", init]),
        false,
        &env,
    );
    let run_args = strings(&["run", "--rm", "--pull", "never", &image, "exit", "0"]);
    // The template, and the runs before its working set was recorded.
    for _ in 0..=UNPREFETCHED {
        run_env(&run_args, false, &env);
    }

    // xorshift64*, seeded: every run of the bench offers the same arrivals.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut uniform = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        // A float in (0, 1]: never zero, whose logarithm is not finite.
        ((state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    };
    let mut rows = Vec::new();
    for &rate in &rates {
        let start = Instant::now();
        let mut due = 0.0f64;
        let mut requests = Vec::new();
        loop {
            // Exponential gaps: a Poisson process of `rate` per second.
            due += -uniform().ln() / rate;
            if due >= secs {
                break;
            }
            let at = start + Duration::from_secs_f64(due);
            if let Some(wait) = at.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let mut command = Command::new(common::shards());
            command
                .args(&run_args)
                .env("SHARDS_HOME", &home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            requests.push(std::thread::spawn(move || {
                let ok = command.status().is_ok_and(|s| s.success());
                let done = Instant::now();
                (ok, done.duration_since(at).as_secs_f64() * 1e6, done)
            }));
        }
        let n = requests.len();
        let (mut latency, mut failed, mut last) = (Vec::with_capacity(n), 0usize, start);
        for r in requests {
            let (ok, us, done) = r.join().unwrap();
            last = last.max(done);
            if ok {
                latency.push(us);
            } else {
                failed += 1;
            }
        }
        let done_rate = (n - failed) as f64 / last.duration_since(start).as_secs_f64();
        let name = format!("rate_{rate}");
        rows.push(stats(&name, "us", latency));
        rows.push(stats(&format!("{name}_sent"), "requests", vec![n as f64]));
        rows.push(stats(&format!("{name}_failed"), "requests", vec![failed as f64]));
        rows.push(stats(&format!("{name}_done"), "per_s", vec![done_rate]));
    }
    run_env(&strings(&["daemon", "stop"]), false, &env);
    let _ = std::fs::remove_dir_all(&home);
    let rates_text: Vec<String> = rates.iter().map(f64::to_string).collect();
    report(
        "arrival",
        &[
            ("rates", rates_text.join(",")),
            ("secs", secs.to_string()),
            ("kernel", common::kernel_artifact().name.to_string()),
        ],
        &rows,
    );
}
