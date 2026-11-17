//! Open-loop arrival benchmark: `shards run vm` requests arriving at a rate, whether or not
//! the ones before them are done, as users' requests do. A closed loop, which sends the
//! next request only once the last has answered, slows with the system it measures and
//! hides its queueing (Schroeder, Wierman and Harchol-Balter, "Open Versus Closed: A
//! Cautionary Tale", NSDI 2006).
//!
//! The image bench's image and guest (benches/image.rs), its template saved and its
//! working set recorded first. Then for each rate in `--rates` (per second; default
//! 2,5,10,20,40), for `--secs` seconds (10): arrivals a Poisson process of that rate,
//! from a seeded generator, each `shards run vm --rm --pull never IMAGE exit 0` spawned at
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
//! Each rate's requests are waited for as the tests wait on what clients bring about
//! together ([`answered`]): a request that never ends fails the bench, saying what its home
//! showed, rather than holding it to its CI job's limit.
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
    use std::sync::mpsc;
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
        // Each request's client as its thread spawns it, and each request's end.
        let (spawned, spawns) = mpsc::channel();
        let (ended, ends) = mpsc::channel();
        let mut n = 0usize;
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
            let (spawned, ended) = (spawned.clone(), ended.clone());
            std::thread::spawn(move || {
                let child = command.spawn();
                let client = child.as_ref().ok().map(std::process::Child::id);
                let _ = spawned.send(client);
                let ok = child
                    .and_then(|mut child| child.wait())
                    .is_ok_and(|s| s.success());
                let done = Instant::now();
                let latency_us = done.duration_since(at).as_secs_f64() * 1e6;
                let _ = ended.send(End {
                    client,
                    ok,
                    latency_us,
                    done,
                });
            });
            n += 1;
        }
        let clients: Vec<u32> = spawns.iter().take(n).flatten().collect();
        let (mut latency, mut failed, mut last) = (Vec::with_capacity(n), 0usize, start);
        for end in answered(rate, &ends, n, &clients, &home) {
            last = last.max(end.done);
            if end.ok {
                latency.push(end.latency_us);
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

/// A request's end: its client (none where it could not be spawned), whether its command
/// ran, its latency from when it was due, in microseconds, and when it ended.
#[cfg(unix)]
struct End {
    client: Option<u32>,
    ok: bool,
    latency_us: f64,
    done: std::time::Instant,
}

/// The ends of rate `rate`'s `n` requests as `ends` brings them, `clients` their clients,
/// waited for as the tests wait on what clients bring about together
/// ([`common::Patience::clients`]: its party the clients, what descends from them and
/// `home`'s daemon, judged by D215's judge as D209 rules, its kind's W* the slowest of the
/// rates' waits before it on this host, no bound of the bench's own). Each request was once
/// waited on with no judge at all, and two CI runs' firecracker jobs ran to their 30-minute
/// limit in this step, their logs unkept (43782275660, 43838392544). Refused, the bench fails
/// ([`stalled`]).
#[cfg(unix)]
fn answered(
    rate: f64,
    ends: &std::sync::mpsc::Receiver<End>,
    n: usize,
    clients: &[u32],
    home: &std::path::Path,
) -> Vec<End> {
    let mut patience = common::Patience::clients(clients, home);
    let mut answered = Vec::with_capacity(n);
    while answered.len() < n {
        match ends.recv_timeout(patience.next_look()) {
            Ok(end) => answered.push(end),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if patience.holds() => {}
            Err(_) => stalled(rate, &answered, clients, home),
        }
    }
    patience.done();
    answered
}

/// Rate `rate`'s requests' wait refused, its account said ([`common::Patience::holds`]):
/// says which requests never ended, then what their home shows ([`common::stacks_of`]: its
/// runs, its microVMs' processes, its daemon's log), ends their clients and stops its daemon,
/// and fails the bench. The home is kept, its daemon's whole log in it.
#[cfg(unix)]
fn stalled(rate: f64, answered: &[End], clients: &[u32], home: &std::path::Path) -> ! {
    use std::io::Write as _;
    let waited: Vec<u32> = clients
        .iter()
        .copied()
        .filter(|&client| !answered.iter().any(|end| end.client == Some(client)))
        .collect();
    let said = format!(
        "rate {rate}: {} of its requests never ended, their clients {waited:?}, its home {}",
        waited.len(),
        home.display()
    );
    let _ = writeln!(std::io::stderr(), "{said}");
    let named: Vec<(&str, u32)> = waited
        .iter()
        .map(|&client| ("a request's client", client))
        .collect();
    let _ = writeln!(std::io::stderr(), "{}", common::stacks_of(&named, Some(home)));
    for &client in &waited {
        if let Ok(pid) = i32::try_from(client) {
            common::kill_now(pid);
        }
    }
    support::run_env(
        &["daemon".to_string(), "stop".to_string()],
        false,
        &[("SHARDS_HOME", home.as_os_str())],
    );
    panic!("{said}");
}
