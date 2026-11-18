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
//! - `rate_R_sent`, `rate_R_failed`: the requests sent, and those that failed, shards' own;
//!   `rate_R_refused_by_host`: those whose microVM's entry its host refused, as D261's gate
//!   takes a refusal for its host's ([`by_the_host`]); and `rate_R_done`: the rate they were
//!   answered at, per second, over the step's span to its last answer. Each failed request is
//!   said with why.
//!
//! A rate whose span, from its start to its drain's end, its host refused VM entries in, every
//! refusal D261's gate takes for its host's ([`span_refusals`], `limits::the_hosts_all`), is
//! not shards' measurement: its row says `"invalid":"the host refused VM entries"`, its
//! refusals are said (how many, the first and the last, the host model and a witness), and no
//! rate after it is measured on a host that has begun to refuse. A refusal the gate does not
//! take stays a failure, said with why (ROOT's ruling of 2026-11-18; CI 44245219858's rate 40
//! on EC2's nested KVM, 721 VM processes refused in 42 s).
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
    use std::io::Write as _;
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
    // Every rate's requests that failed or that their host refused: where any did, the home is
    // kept, its daemon's log in it (ci.yml's `Arrival's daemon, stopped` prints it).
    let mut failures = 0;
    // Each request's client's words, a file each, read as it ends.
    let said_dir = common::workspace().join(format!("target/bench/requests-{}", std::process::id()));
    std::fs::create_dir_all(&said_dir).unwrap();
    for (r, &rate) in rates.iter().enumerate() {
        let start = Instant::now();
        // Its span on the clock every process of its host reads, as refusals say their moments.
        let from_us = shards_vmm::vm::limits::now_us();
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
                .stdout(Stdio::null());
            let said_at = said_dir.join(format!("{rate}-{}", dues.len()));
            let (spawned, ended) = (spawned.clone(), ended.clone());
            std::thread::spawn(move || {
                // Its words in a file, not a pipe: a run hands its client's stdio to its
                // microVM, which a pipe's end would wait for after the client's.
                let child = std::fs::File::create(&said_at).and_then(|file| command.stderr(file).spawn());
                let client = child.as_ref().ok().map(std::process::Child::id);
                let _ = spawned.send(client);
                let ok = child
                    .and_then(|mut child| child.wait())
                    .is_ok_and(|s| s.success());
                let done = Instant::now();
                let said = std::fs::read_to_string(&said_at).unwrap_or_default();
                let _ = std::fs::remove_file(&said_at);
                let latency_us = done.duration_since(at).as_secs_f64() * 1e6;
                let _ = ended.send(End {
                    client,
                    ok,
                    said,
                    latency_us,
                    done,
                });
            });
            dues.push(at);
        }
        let n = dues.len();
        let clients: Vec<u32> = spawns.iter().take(n).flatten().collect();
        let came = answered(rate, &ends, &dues, &clients, &mut seen, &home);
        // The refused entries its daemon's log tells of, read once every request has ended.
        let refusals = span_refusals(&home, from_us, shards_vmm::vm::limits::now_us());
        let (mut latency, mut failed, mut refused, mut last) = (Vec::with_capacity(n), 0usize, 0usize, start);
        for end in came {
            last = last.max(end.done);
            if end.ok {
                latency.push(end.latency_us);
            } else if by_the_host(rate, &refusals, &end.said) {
                refused += 1;
            } else {
                failed += 1;
            }
        }
        let done_rate = (n - failed - refused) as f64 / last.duration_since(start).as_secs_f64();
        failures += failed + refused;
        // Its span's refusals, each taken by D261's gate: its host's, not shards' measurement.
        let invalid = match &refusals.span {
            Ok(None) => false,
            Ok(Some(all)) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "rate {rate}: invalid: its host refused VM entries in its span, every one its \
                     host's by D261's gate: {all}; host model {}",
                    shards_vmm::vm::limits::HostModel::here()
                );
                true
            }
            Err(not) => {
                for (refusal, why) in not {
                    let _ = writeln!(
                        std::io::stderr(),
                        "rate {rate}: a refused entry D261's gate does not take for its host's ({why}): {}",
                        refusal.line
                    );
                }
                failures += not.len();
                false
            }
        };
        failures += usize::from(invalid);
        let name = format!("rate_{rate}");
        let rows = [
            stats(&name, "us", latency),
            stats(&format!("{name}_sent"), "requests", vec![n as f64]),
            stats(&format!("{name}_failed"), "requests", vec![failed as f64]),
            stats(
                &format!("{name}_refused_by_host"),
                "requests",
                vec![refused as f64],
            ),
            stats(&format!("{name}_done"), "per_s", vec![done_rate]),
        ];
        let mut params = vec![
            ("rate", rate.to_string()),
            ("secs", secs.to_string()),
            ("kernel", kernel.clone()),
        ];
        if invalid {
            params.push(("invalid", "the host refused VM entries".to_string()));
        }
        report("arrival", &params, &rows);
        // No rate after it is measured on a host that has begun to refuse its entries.
        if invalid {
            let rest = rates.get(r + 1..).unwrap_or_default();
            if !rest.is_empty() {
                let _ = writeln!(
                    std::io::stderr(),
                    "rates {rest:?} not measured: the host refused VM entries at rate {rate}"
                );
            }
            break;
        }
    }
    run_env(&strings(&["daemon", "stop"]), false, &env);
    let _ = std::fs::remove_dir_all(&said_dir);
    if failures == 0 {
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// A request's end: its client (none where it could not be spawned), whether its command
/// ran, what its client said (its stderr), its latency from when it was due, in
/// microseconds, and when it ended.
#[cfg(unix)]
struct End {
    client: Option<u32>,
    ok: bool,
    said: String,
    latency_us: f64,
    done: std::time::Instant,
}

/// The refusals of entry a rate's daemon's log tells of (D261), and what D261's gate makes of
/// its span's ([`span_refusals`]).
#[cfg(unix)]
struct Refusals {
    /// Each refusal line its daemon's log holds, in the span or not.
    lines: String,
    /// Every refusal those lines and its host model's records tell of: the gate's witnesses.
    kept: Vec<shards_vmm::vm::limits::Refusal>,
    /// Its host model's burst width: the records' widest, or D261's measured.
    width_us: u64,
    /// The gate over the span's own refusals ([`shards_vmm::vm::limits::the_hosts_all`]).
    span: Result<
        Option<shards_vmm::vm::limits::HostsRefusals>,
        Vec<(shards_vmm::vm::limits::Said, shards_vmm::vm::limits::NotTheHosts)>,
    >,
}

/// The refusals of entry `home`'s daemon's log tells of (D261), and D261's gate over those of a
/// rate's span, `from_us` to `to_us` (its start to its drain's end, as refusals say their
/// moments): each with every refusal the log tells of, in the span or not, and those its host
/// model's records keep (those its refusals name, and the tests' own, `common::vcpu_limits`),
/// as its witnesses, their burst width the records' or D261's measured. Where the span holds a
/// refusal, the width is waited out from its last one first, and the log read again, so that
/// a witness said just after it is read, as the tests' gate waits (`common::host_refusal`).
#[cfg(unix)]
fn span_refusals(home: &std::path::Path, from_us: u64, to_us: u64) -> Refusals {
    use shards_vmm::vm::limits;
    let read = || -> Vec<(String, limits::Said)> {
        std::fs::read_to_string(home.join("daemon.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| limits::Said::find(line).map(|said| (line.to_string(), said)))
            .collect()
    };
    let in_span = |said: &[(String, limits::Said)]| -> Vec<limits::Said> {
        said.iter()
            .map(|(_, said)| said)
            .filter(|said| (from_us..=to_us).contains(&said.refusal.at_us))
            .cloned()
            .collect()
    };
    let records = |span: &[limits::Said]| -> Vec<limits::Limits> {
        let mut files: Vec<std::path::PathBuf> = span.iter().filter_map(|said| said.file.clone()).collect();
        files.sort();
        files.dedup();
        files
            .into_iter()
            .map(|file| limits::Limits::kept_in(file, limits::HostModel::here()))
            .chain([common::vcpu_limits()])
            .collect()
    };
    let width = |records: &[limits::Limits]| {
        records
            .iter()
            .map(limits::Limits::burst_width_us)
            .max()
            .unwrap_or_else(limits::measured_width_us)
    };
    let mut said = read();
    if let Some(last) = in_span(&said).iter().map(|said| said.refusal.at_us).max() {
        let wait = last
            .saturating_add(width(&records(&in_span(&said))))
            .saturating_sub(limits::now_us());
        std::thread::sleep(std::time::Duration::from_micros(wait));
        said = read();
    }
    let span = in_span(&said);
    let records = records(&span);
    let width_us = width(&records);
    let kept: Vec<limits::Refusal> = said
        .iter()
        .map(|(_, said)| said.refusal)
        .chain(records.iter().flat_map(limits::Limits::refusals))
        .collect();
    Refusals {
        lines: said
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        span: limits::the_hosts_all(&span, &kept, width_us),
        kept,
        width_us,
    }
}

/// Whether a failed request's microVM's entry was refused by its host, as D261's gate takes a
/// refusal for its host's: the refusal its client's words, `said`, carry, or that its daemon's
/// log tells of the VM those words name as ended (`common::run_refusal`), its witnesses and
/// burst width its rate's ([`Refusals`]), as its span is judged. Said either way: a refusal the
/// gate takes, with its witness, is counted apart from shards' failures; one it does not take,
/// and a failure that names no refused VM, stay shards', said with the client's words.
#[cfg(unix)]
fn by_the_host(rate: f64, refusals: &Refusals, said: &str) -> bool {
    use std::io::Write as _;
    let refusal = shards_vmm::vm::limits::Said::find(&common::run_refusal(said, &refusals.lines));
    let host = match refusal.map(|refusal| {
        let gate = shards_vmm::vm::limits::the_hosts(&refusal, &refusals.kept, refusals.width_us);
        (refusal, gate)
    }) {
        Some((refusal, Ok(witness))) => {
            let _ = writeln!(
                std::io::stderr(),
                "rate {rate}: refused by the host, by D261's gate ({witness}): {}",
                refusal.line
            );
            true
        }
        Some((refusal, Err(not))) => {
            let _ = writeln!(
                std::io::stderr(),
                "rate {rate}: a refused entry D261's gate does not take for its host's ({not}): {}",
                refusal.line
            );
            false
        }
        None => false,
    };
    if !host {
        let _ = writeln!(
            std::io::stderr(),
            "rate {rate}: a request failed: {}",
            said.trim()
        );
    }
    host
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
    let next = |within| ends.recv_timeout(within);
    match common::requests_end(next, rest, &outstanding, |end: &End| end.client, seen, home) {
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
    // Its daemon stopped, and the bench failed in its own words whatever that stop came to:
    // a stop that failed is said after them, never in their place. CI 44245219858's stop,
    // ended by the harness's watchdog, took the panic's place (`shards failed: status None,
    // ended by signal 9`), the requests that never ended said only above it.
    let stopped = support::run_env_unchecked(
        &["daemon".to_string(), "stop".to_string()],
        false,
        &[("SHARDS_HOME", home.as_os_str())],
    );
    if stopped.run.status == Some(0) {
        panic!("{said}");
    }
    panic!("{said}\n--- its daemon's stop failed: {}", stopped.run);
}
