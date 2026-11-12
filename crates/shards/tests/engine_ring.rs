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
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use common::{Patience, Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(600);

fn shards_in(home: &Path, args: &[&str]) -> Run {
    shards_with(home, args, &[])
}

/// `shards` in `home`, `more` in its environment besides: the daemon a first command starts
/// keeps them (`SHARDS_LOG=debug` has its VM processes say each ring they attach and let go,
/// in its log; `SHARDS_PRESSURE` names the pipe its host's word comes through).
fn shards_with(home: &Path, args: &[&str], more: &[(&str, &OsStr)]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let mut env: Vec<(&str, &OsStr)> = vec![
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    env.extend_from_slice(more);
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
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A `--use-api-socket` run named `prov`, `run` among its options, its engine not yet
/// asked anything.
fn prov(home: &Path, image: &str, run: &[&str]) {
    let mut args = vec!["run", "-d", "--name", "prov", "--use-api-socket"];
    args.extend_from_slice(run);
    args.extend([image, "sleep"]);
    let ran = shards_in(home, &args);
    assert_eq!(ran.status, Some(0), "{ran}");
}

/// Waits until `prov`'s engine answers.
fn answering(home: &Path) {
    let mut patience = Patience::of(home, TIMEOUT);
    loop {
        let out = shards_in(
            home,
            &["exec", "prov", "/bin/testguest", "api", "GET", "/_ping", ""],
        );
        if out.status == Some(0) && out.stdout == "api HTTP/1.1 200 OK\nOK" {
            return;
        }
        assert!(patience.holds(), "its engine never answered: {out}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `prov`'s engine answering, and a joiner of it, `emitter`, that wrote `bytes` of the
/// pattern to its log and ended.
fn emitter(home: &Path, image: &str, bytes: u64) {
    answering(home);
    let api = |method: &str, path: &str, body: &str| {
        let out = shards_in(
            home,
            &["exec", "prov", "/bin/testguest", "api", method, path, body],
        );
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
    line.rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no time in {line:?}"))
}

/// Bytes of a log and of a stdin, each larger than many rings, cross whole through a
/// microVM's own engine, each checked byte for byte in the microVM; small requests on one
/// kept connection come back; one connection after another is served (each `api-bulk` is a
/// connection of its own, its slot made as it starts and freed as it ends). The device's
/// log says whether a ring was attached (SHARDS_LOG=debug: "an engine ring attached").
#[test]
fn a_microvms_engine_carries_bulk_whole_both_ways() {
    let Some((home, image)) = home("eng-ring") else {
        return;
    };
    let bytes = env_u64("ENGINE_RING_BYTES", 64 << 20);
    prov(&home, &image, &[]);
    emitter(&home, &image, bytes);
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
    prov(&home, &image, &[]);
    emitter(&home, &image, bytes);
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
    println!(
        "engine-ring: {rounds} rounds of {bytes} bytes; load {}; {}",
        load.trim(),
        host.lines().next().unwrap_or_default()
    );
    println!("| arm | n | p50 ms | p90 | p99 | max | MiB/s at p50 |");
    println!("|---|---|---|---|---|---|---|");
    for (name, samples) in [
        ("engine to command (log)", &mut logs),
        ("command to engine (stdin)", &mut stdin),
    ] {
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

/// The device's word of its engine rings, in its daemon's log (`SHARDS_LOG=debug`, D230):
/// rings attached and the bytes they pinned, rings let go and the bytes they unpinned, and
/// how many of those the relay asked to go (`Asked`: its idle slot taken back by the room,
/// a give-back, a swap or its stream's end).
#[derive(Debug, Default, PartialEq, Eq)]
struct Rings {
    attached: u64,
    pinned: u64,
    let_go: u64,
    unpinned: u64,
    asked: u64,
}

fn rings(home: &Path) -> Rings {
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let bytes = |line: &str, before: &str| -> u64 {
        line.split(before)
            .next()
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let mut r = Rings::default();
    for line in log.lines() {
        if line.contains("an engine ring attached, ") {
            r.attached += 1;
            r.pinned += bytes(line, " bytes pinned");
        } else if line.contains("an engine ring let go, ") {
            r.let_go += 1;
            r.unpinned += bytes(line, " bytes unpinned");
            r.asked += u64::from(line.contains("(Asked)"));
        }
    }
    r
}

/// The guest's shared memory, in kB, as `name`'s kernel says it (`Shmem` of /proc/meminfo):
/// the engine relay's slots are memory files, counted there while they live.
fn shmem(home: &Path, name: &str) -> u64 {
    let out = shards_in(home, &["exec", name, "/bin/testguest", "meminfo", "Shmem"]);
    assert_eq!(out.status, Some(0), "{out}");
    out.stdout
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no Shmem in {:?}", out.stdout))
}

/// Waits until `ready` holds of the device's word.
fn await_rings(home: &Path, what: &str, ready: impl Fn(&Rings) -> bool) {
    let mut patience = Patience::of(home, TIMEOUT);
    while !ready(&rings(home)) {
        assert!(patience.holds(), "{what}: {:?}", rings(home));
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// vCPU counts a microVM is run with: one, two and every CPU of the host (D230's V > 1).
fn vcpus() -> Vec<usize> {
    let many = std::thread::available_parallelism().map_or(4, usize::from);
    let mut v = vec![1, 2, many];
    v.dedup();
    v
}

/// The least a slot pins, in the kB /proc/meminfo counts: its control page and two rings of
/// `MIN_RING` (D230 section 16). Any slot made and kept moves a guest's `Shmem` by at least
/// this; less is no slot.
fn least_slot_kb() -> u64 {
    shards_abi::ring::slot_len(shards_abi::ring::MIN_RING) / 1024
}

/// An engine connection held open and idle in `prov` (the engine's events, which say
/// nothing until something happens), its ring attached; it ends as `prov` is removed.
fn held_open(home: &Path) -> std::thread::JoinHandle<Run> {
    answering(home);
    let before = rings(home).attached;
    let held = std::thread::spawn({
        let home = home.to_path_buf();
        move || {
            shards_in(
                &home,
                &["exec", "prov", "/bin/testguest", "api", "GET", "/events", ""],
            )
        }
    });
    await_rings(home, "the held connection's ring", |r| {
        r.attached > before && r.attached > r.let_go
    });
    held
}

/// An idle microVM pins nothing, and each engine stream's pins go as it ends (D230 section
/// 16): on microVMs of one vCPU, two and the host's every CPU, one whose engine was asked
/// nothing holds less than a slot of shared memory more than the same run without the
/// engine's socket (no relay at all), where its slots would be memory files; then as many
/// engine streams at once as it has vCPUs, and twice that, each byte-checked; once they end,
/// every ring the device attached it has let go, bytes for bytes, and the guest's shared
/// memory is within a slot of where it was before them.
#[test]
fn an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let least = least_slot_kb();
    for cpus in vcpus() {
        let home = TempDir::new(&format!("eng-pins-{cpus}"));
        let debug: [(&str, &OsStr); 1] = [("SHARDS_LOG", "debug".as_ref())];
        let pulled = shards_with(&home, &["pull", "-q", &image], &debug);
        assert_eq!(pulled.status, Some(0), "{pulled}");
        let n = cpus.to_string();
        let bare = shards_in(
            &home,
            &["run", "-d", "--name", "bare", "--cpus", &n, &image, "sleep"],
        );
        assert_eq!(bare.status, Some(0), "{bare}");
        prov(&home, &image, &["--cpus", &n]);
        let (without, idle) = (shmem(&home, "bare"), shmem(&home, "prov"));
        assert!(
            idle < without + least,
            "{cpus} vCPUs: an idle engine's microVM holds {idle} kB of shared memory, {without} kB without its socket"
        );
        assert_eq!(rings(&home).attached, 0, "{cpus} vCPUs: a ring before any stream");
        emitter(&home, &image, 4 << 20);
        await_rings(&home, "the setup's rings let go", |r| {
            r.attached > 0 && r.let_go == r.attached
        });
        let before = shmem(&home, "prov");
        let logs = (4u64 << 20).to_string();
        for streams in [cpus, 2 * cpus] {
            std::thread::scope(|s| {
                let runs: Vec<_> = (0..streams)
                    .map(|i| {
                        let (home, logs) = (&home, &logs);
                        s.spawn(move || {
                            let out = if i % 2 == 0 {
                                bulk(home, &["logs", "emitter", logs])
                            } else {
                                bulk(home, &["ping", "50"])
                            };
                            assert!(
                                out.starts_with(&format!("bulk logs {logs} ")) || out.starts_with("ping 50 "),
                                "{out}"
                            );
                        })
                    })
                    .collect();
                for r in runs {
                    r.join().unwrap();
                }
            });
            await_rings(&home, "every stream's ring let go", |r| r.let_go == r.attached);
            let r = rings(&home);
            assert_eq!(r.unpinned, r.pinned, "{cpus} vCPUs, {streams} streams: {r:?}");
        }
        let after = shmem(&home, "prov");
        assert!(
            after < before + least,
            "{cpus} vCPUs: {after} kB of shared memory after the streams, {before} kB before"
        );
        let removed = shards_in(&home, &["rm", "-f", "prov", "bare"]);
        assert_eq!(removed.status, Some(0), "{removed}");
    }
}

/// A give-back lets an idle slot go (D188's `RECLAIM`, D230): an engine connection held
/// open, idle, keeps its slot until the daemon hears its host is short of memory
/// (`SHARDS_PRESSURE`); the relay then asks the device to let it go, and the connection
/// goes on over plain vsock.
#[cfg(unix)]
#[test]
fn a_give_back_lets_an_idle_slot_go() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("eng-giveback");
    let pipe = home.join("pressure");
    let c = std::ffi::CString::new(pipe.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: mkfifo(3) of a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let more: [(&str, &OsStr); 2] = [
        ("SHARDS_LOG", "debug".as_ref()),
        ("SHARDS_PRESSURE", pipe.as_os_str()),
    ];
    let pulled = shards_with(&home, &["pull", "-q", &image], &more);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    prov(&home, &image, &[]);
    let held = held_open(&home);
    let before = rings(&home);
    // The host's word: short of memory (each line one, as containers.rs says it).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&pipe)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    await_rings(&home, "the idle slot given back", |r| {
        r.asked > before.asked && r.let_go == r.attached
    });
    let removed = shards_in(&home, &["rm", "-f", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    let _ = held.join();
}

/// A shrinking room lets an idle slot go, and a stream past the room goes over plain
/// vsock (D188, D230): under a slice of the user's manager, an engine connection held open
/// and idle keeps its slot while the budget has room; with the budget brought down to what
/// the slice holds, the relay lets it go, and a new stream crosses whole with no ring.
#[cfg(target_os = "linux")]
#[test]
fn a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain() {
    use std::process::Command;
    if cannot_run_vms() {
        return;
    }
    // SAFETY: getuid(2) takes nothing and cannot fail.
    let uid = unsafe { libc::getuid() };
    let bus = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{uid}").into())
        .join("bus");
    if !bus.exists() {
        eprintln!("SKIP: no user's systemd manager here: {} is not", bus.display());
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("eng-room");
    let slice = format!("shardsring{}.slice", std::process::id());
    struct Stopped<'a>(&'a str);
    impl Drop for Stopped<'_> {
        fn drop(&mut self) {
            for verb in ["revert", "stop"] {
                let _ = Command::new("systemctl").args(["--user", verb, self.0]).status();
            }
        }
    }
    let _stopped = Stopped(&slice);
    let budget = |high: &str| {
        let set = Command::new("systemctl")
            .args(["--user", "set-property", "--runtime", &slice])
            .arg(format!("MemoryHigh={high}"))
            .arg("MemorySwapMax=0")
            .output()
            .unwrap();
        assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    };
    let debug: [(&str, &OsStr); 1] = [("SHARDS_LOG", "debug".as_ref())];
    let pulled = shards_with(&home, &["pull", "-q", &image], &debug);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // The slice made by the run, with no budget yet: room for a slot.
    prov(&home, &image, &["--cgroup-parent", &slice]);
    let held = held_open(&home);
    let before = rings(&home);
    // The budget brought down to what the slice holds: no room past it.
    let cgroup = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{slice}");
    let holds = std::fs::read_to_string(format!("{cgroup}/memory.current")).unwrap();
    budget(holds.trim());
    await_rings(&home, "the idle slot let go for the room", |r| {
        r.asked > before.asked && r.let_go == r.attached
    });
    // A new stream past the room: no ring attached, its bytes whole all the same.
    let attached = rings(&home).attached;
    let ping = bulk(&home, &["ping", "20"]);
    assert!(ping.starts_with("ping 20 "), "{ping}");
    assert_eq!(rings(&home).attached, attached, "a ring attached past the room");
    let removed = shards_in(&home, &["rm", "-f", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    let _ = held.join();
}
