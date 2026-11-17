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
//! Each rate is reported as it ends, its table, its `shards-bench` line and its samples, so
//! that a run cut short still shows the rates it finished. Its requests are waited for by
//! their own progress, their ends ([`answered`], D209 Part 11): a rate whose requests stop
//! ending fails the bench, saying what its home showed, rather than holding it to its CI
//! job's limit.
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
    // The template, and the runs before its working set was recorded. Each run after the
    // template's own is a request served alone: its time is a stretch with a request
    // outstanding and none ending, as the drains' judge counts them ([`answered`]).
    let mut seen = Vec::new();
    for run in 0..=UNPREFETCHED {
        let sample = run_env(&run_args, false, &env);
        if run > 0 {
            seen.push(sample.run.elapsed);
        }
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
    let kernel = common::kernel_artifact().name.to_string();
    // Every rate's failed requests: their output goes nowhere, so where any failed the home is
    // kept, its daemon's log saying why (ci.yml's `Arrival's daemon, stopped` prints it).
    let mut failures = 0;
    for &rate in &rates {
        let start = Instant::now();
        let mut due = 0.0f64;
        // Each request's client as its thread spawns it, and each request's end.
        let (spawned, spawns) = mpsc::channel();
        let (ended, ends) = mpsc::channel();
        // When each request was due, in the order they were sent.
        let mut dues = Vec::new();
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
            dues.push(at);
        }
        let n = dues.len();
        let clients: Vec<u32> = spawns.iter().take(n).flatten().collect();
        let (mut latency, mut failed, mut last) = (Vec::with_capacity(n), 0usize, start);
        for end in answered(rate, &ends, &dues, &clients, &mut seen, &home) {
            last = last.max(end.done);
            if end.ok {
                latency.push(end.latency_us);
            } else {
                failed += 1;
            }
        }
        let done_rate = (n - failed) as f64 / last.duration_since(start).as_secs_f64();
        failures += failed;
        let name = format!("rate_{rate}");
        let rows = [
            stats(&name, "us", latency),
            stats(&format!("{name}_sent"), "requests", vec![n as f64]),
            stats(&format!("{name}_failed"), "requests", vec![failed as f64]),
            stats(&format!("{name}_done"), "per_s", vec![done_rate]),
        ];
        report(
            "arrival",
            &[
                ("rate", rate.to_string()),
                ("secs", secs.to_string()),
                ("kernel", kernel.clone()),
            ],
            &rows,
        );
    }
    run_env(&strings(&["daemon", "stop"]), false, &env);
    if failures == 0 {
        let _ = std::fs::remove_dir_all(&home);
    }
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

/// The ends of rate `rate`'s requests, due at `dues`, as `ends` brings them, `clients` their
/// clients: those that came while they arrived, then the rest, waited for by their own
/// progress, their ends ([`common::requests_end`], D209 Part 11). `seen` holds the stretches
/// with requests outstanding and none ending that this run has shown ([`stretches`]): its
/// template's runs, and each rate's arrivals, this one's added. Waited on with no judge, ten
/// CI runs' firecracker jobs ran to their 30-minute limit in this step; judged by its party's
/// CPU, which its daemon and warm microVMs spend whether or not a request ends, an eleventh
/// did. Refused, the bench fails ([`stalled`]).
#[cfg(unix)]
fn answered(
    rate: f64,
    ends: &std::sync::mpsc::Receiver<End>,
    dues: &[std::time::Instant],
    clients: &[u32],
    seen: &mut Vec<std::time::Duration>,
    home: &std::path::Path,
) -> Vec<End> {
    let mut answered: Vec<End> = ends.try_iter().collect();
    let done: Vec<std::time::Instant> = answered.iter().map(|end| end.done).collect();
    seen.extend(stretches(dues, &done));
    let outstanding: Vec<u32> = clients
        .iter()
        .copied()
        .filter(|&client| !answered.iter().any(|end| end.client == Some(client)))
        .collect();
    let rest = dues.len().saturating_sub(answered.len());
    match common::requests_end(ends, rest, &outstanding, |end: &End| end.client, seen, home) {
        Ok(came) => {
            answered.extend(came);
            answered
        }
        Err((came, refused)) => {
            answered.extend(came);
            stalled(rate, &answered, clients, home, &refused)
        }
    }
}

/// The stretches seen by requests due at `dues`, of which those that ended did at `done`: each
/// from when a request was outstanding and none had ended since, to the end that ended it. A
/// request is outstanding from when it was due; one still outstanding ends no stretch.
#[cfg(unix)]
fn stretches(dues: &[std::time::Instant], done: &[std::time::Instant]) -> Vec<std::time::Duration> {
    let mut events: Vec<(std::time::Instant, bool)> = dues
        .iter()
        .map(|&at| (at, true))
        .chain(done.iter().map(|&at| (at, false)))
        .collect();
    // At one instant, an arrival before an end.
    events.sort_by_key(|&(at, arrival)| (at, !arrival));
    let (mut outstanding, mut since, mut stretches) = (0usize, None, Vec::new());
    for (at, arrival) in events {
        if arrival {
            outstanding += 1;
            since = since.or(Some(at));
        } else if let Some(began) = since {
            stretches.push(at.duration_since(began));
            outstanding = outstanding.saturating_sub(1);
            since = (outstanding > 0).then_some(at);
        }
    }
    stretches
}

/// Rate `rate`'s requests' wait refused, `refused` its account ([`common::requests_end`]):
/// says which requests never ended and why its wait was refused, then what their home shows
/// ([`common::stacks_of`]: its runs, its microVMs' processes, its daemon's log), ends their
/// clients and stops its daemon, and fails the bench. The home is kept, its daemon's whole
/// log in it.
#[cfg(unix)]
fn stalled(rate: f64, answered: &[End], clients: &[u32], home: &std::path::Path, refused: &str) -> ! {
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
    let _ = writeln!(std::io::stderr(), "{said}\n{refused}");
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
