//! The engine ring (D230) on real microVMs: a `--use-api-socket` run's command moves bulk
//! bytes through its own microVM's engine, each way, whole, and small requests still come
//! back; and the measurement harness that times them (`engine_ring_measurement`, ignored:
//! the coordinator's to run, on each host, against the build before the ring).
//!
//! Engine to command: a joiner's log (the daemon keeps it on the host, so that reading it
//! back crosses the engine path alone). Command to engine: a joiner's stdin through an
//! attach, which then crosses the joiner's own exec channel too, so its rate is the lower
//! of the two (docs/research/engine-ring-channel-decision-v1.md section 14).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use common::{Patience, Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(600);

fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// A home whose daemon runs, the test image pulled into it.
fn home(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let pulled = shards_in(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    Some((home, image))
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A `--use-api-socket` run named `prov`, its engine answering, and a joiner of it,
/// `emitter`, that wrote `bytes` of the pattern to its log and ended.
fn provider(home: &Path, image: &str, bytes: u64) {
    let shards = |args: &[&str]| shards_in(home, args);
    let ran = shards(&["run", "-d", "--name", "prov", "--use-api-socket", image, "sleep"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let mut patience = Patience::of(home, TIMEOUT);
    loop {
        let out = shards(&["exec", "prov", "/bin/testguest", "api", "GET", "/_ping", ""]);
        if out.status == Some(0) && out.stdout == "api HTTP/1.1 200 OK\nOK" {
            break;
        }
        assert!(patience.holds(), "its engine never answered: {out}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let api = |method: &str, path: &str, body: &str| {
        let out = shards(&["exec", "prov", "/bin/testguest", "api", method, path, body]);
        assert_eq!(out.status, Some(0), "{out}");
        out.stdout
    };
    let body = format!(r#"{{"Image":"{image}","Cmd":["/bin/testguest","bulk-emit","{bytes}"]}}"#);
    let made = api("POST", "/containers/create?name=emitter", &body);
    assert!(made.starts_with("api HTTP/1.1 201"), "{made}");
    let started = api("POST", "/containers/emitter/start", "");
    assert!(started.starts_with("api HTTP/1.1 204"), "{started}");
    let waited = api("POST", "/containers/emitter/wait", "");
    assert!(waited.contains("\"StatusCode\":0"), "{waited}");
}

/// `api-bulk ARGS` in `prov`: its one line, `bulk WHAT N NANOS` or `ping ...`.
fn bulk(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["exec", "prov", "/bin/testguest", "api-bulk"];
    all.extend_from_slice(args);
    let out = shards_in(home, &all);
    assert_eq!(out.status, Some(0), "{out}");
    out.stdout.trim().to_string()
}

fn nanos(line: &str) -> u64 {
    line.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no time in {line:?}"))
}

/// Bytes of a log and of a stdin, each larger than many rings, cross whole through a
/// microVM's own engine, each checked byte for byte in the microVM; small requests on one
/// kept connection come back; ring connections more than its relay has slots for are
/// served too (each `api-bulk` is a connection of its own, one after another, and a
/// connection's slot comes back to its relay). The device's log says whether a ring was
/// attached (SHARDS_LOG=debug: "an engine ring attached").
#[test]
fn a_microvms_engine_carries_bulk_whole_both_ways() {
    let Some((home, image)) = home("eng-ring") else {
        return;
    };
    let bytes = env_u64("ENGINE_RING_BYTES", 64 << 20);
    provider(&home, &image, bytes);
    let n = bytes.to_string();
    for _ in 0..3 {
        let logs = bulk(&home, &["logs", "emitter", &n]);
        assert!(logs.starts_with(&format!("bulk logs {bytes} ")), "{logs}");
        let stdin = bulk(&home, &["stdin", &image, &n]);
        assert!(stdin.starts_with(&format!("bulk stdin {bytes} ")), "{stdin}");
    }
    let ping = bulk(&home, &["ping", "200"]);
    assert!(ping.starts_with("ping 200 "), "{ping}");
    let removed = shards_in(&home, &["rm", "-f", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// The measurement (D230, section 14 of the decision): `ENGINE_RING_ROUNDS` rounds
/// (default 11) of each arm, their order rotated each round, `ENGINE_RING_BYTES` (default
/// 256 MiB) a bulk arm: the engine's log to the command, the command's stdin to a joiner,
/// 64-byte-class pings back to back and paced past a sleep's wakeup. Prints n, p50, p90,
/// p99 and max, MiB/s at p50, with the host, its load and the revision. Run it on the
/// build before the ring and on this one, interleaved by build-ab
/// (docs/research/measurements/engine-ring-v1/README.md).
#[test]
#[ignore = "a measurement: the coordinator's to run"]
fn engine_ring_measurement() {
    let Some((home, image)) = home("eng-ring-m") else {
        return;
    };
    let rounds = env_u64("ENGINE_RING_ROUNDS", 11) as usize;
    let bytes = env_u64("ENGINE_RING_BYTES", 256 << 20);
    provider(&home, &image, bytes);
    let n = bytes.to_string();
    let mut logs = Vec::new();
    let mut stdin = Vec::new();
    let mut pings = Vec::new();
    for round in 0..rounds {
        for arm in 0..4usize {
            match (arm + round) % 4 {
                0 => logs.push(nanos(&bulk(&home, &["logs", "emitter", &n]))),
                1 => stdin.push(nanos(&bulk(&home, &["stdin", &image, &n]))),
                2 => pings.push(("back to back", bulk(&home, &["ping", "1000"]))),
                _ => pings.push(("paced 2 ms", bulk(&home, &["ping", "200", "2000"]))),
            }
        }
    }
    let host = shards_in(&home, &["version"]).stdout;
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_else(|_| "unknown".into());
    println!("engine-ring: {rounds} rounds of {bytes} bytes; load {}; {}", load.trim(), host.lines().next().unwrap_or_default());
    println!("| arm | n | p50 ms | p90 | p99 | max | MiB/s at p50 |");
    println!("|---|---|---|---|---|---|---|");
    for (name, samples) in [("engine to command (log)", &mut logs), ("command to engine (stdin)", &mut stdin)] {
        samples.sort_unstable();
        let at = |q: usize| samples[(samples.len() - 1) * q / 100] as f64 / 1e6;
        println!(
            "| {name} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} |",
            samples.len(),
            at(50),
            at(90),
            at(99),
            at(100),
            bytes as f64 / (1 << 20) as f64 / (at(50) / 1e3)
        );
    }
    println!("| pings | per round: ping M P50 P90 P99 MAX (ns) |");
    for (name, line) in &pings {
        println!("| {name} | {line} |");
    }
}
