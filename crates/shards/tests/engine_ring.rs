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
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Patience, Run, TempDir, Told, cannot_run_vms, guest_init, kernel, run_shards_env, served};

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

/// `METHOD PATH BODY` to `prov`'s engine from inside it: what the test guest's `api` says.
fn api(home: &Path, method: &str, path: &str, body: &str) -> String {
    let out = shards_in(
        home,
        &["exec", "prov", "/bin/testguest", "api", method, path, body],
    );
    assert_eq!(out.status, Some(0), "{out}");
    out.stdout
}

/// A joiner of `prov`, `name`, made and started through `prov`'s engine, its command `cmd`
/// (the image's entrypoint is /bin/testguest, common::test_image: its command is the mode).
fn joiner(home: &Path, image: &str, name: &str, cmd: &str) {
    let body = format!(r#"{{"Image":"{image}","Cmd":[{cmd}]}}"#);
    let made = api(home, "POST", &format!("/containers/create?name={name}"), &body);
    assert!(made.starts_with("api HTTP/1.1 201"), "{made}");
    let started = api(home, "POST", &format!("/containers/{name}/start"), "");
    assert!(started.starts_with("api HTTP/1.1 204"), "{started}");
}

/// `prov`'s engine answering, and a joiner of it, `emitter`, that wrote `bytes` of the
/// pattern to its log and ended.
fn emitter(home: &Path, image: &str, bytes: u64) {
    answering(home);
    joiner(home, image, "emitter", &format!(r#""bulk-emit","{bytes}""#));
    let waited = api(home, "POST", "/containers/emitter/wait", "");
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

/// The pages pinned in `name`'s guest now, as its kernel counts pin_user_pages(): /proc/vmstat's
/// nr_foll_pin_acquired less nr_foll_pin_released (mm/gup.c, mm/vmstat.c, Linux 5.8 on). The
/// relay's slots are pinned so (io_uring's fixed buffers, FOLL_PIN | FOLL_LONGTERM), a slot of
/// `MIN_RING` 3 pages, and unpinned so; nothing else in a test guest holds a pin past an I/O.
/// Exact, as the guest's shared memory is not: that moved by 28 to 32 kB once at times of its
/// own, in runs with the engine and without (ring-pins-v1/logs/r10, r12).
fn pinned(home: &Path, name: &str) -> u64 {
    let out = shards_in(
        home,
        &[
            "exec",
            name,
            "/bin/testguest",
            "vmstat",
            "nr_foll_pin_acquired",
            "nr_foll_pin_released",
        ],
    );
    assert_eq!(out.status, Some(0), "{out}");
    let value = |key: &str| -> u64 {
        out.stdout
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {key} in {:?}", out.stdout))
    };
    value("nr_foll_pin_acquired").saturating_sub(value("nr_foll_pin_released"))
}

/// Waits until `name`'s guest pins `want` pages again: the relay unpins a slot once it has heard
/// its device let go and passed on what the slot held, which a reading after a stream's end may
/// come before. A slot never freed keeps its pages pinned: the test fails at its own deadline,
/// `TIMEOUT`, saying `what`.
fn settled(home: &Path, name: &str, want: u64, what: &str) {
    let start = std::time::Instant::now();
    loop {
        let now = pinned(home, name);
        if now == want {
            return;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "{what}: {now} pages pinned after {TIMEOUT:?}, not {want}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// An engine connection held open and idle in `prov`, its ring attached: the wait for a
/// joiner of its, `idle`, that sleeps. A microVM's own engine serves calls on the joiners it
/// made alone, and neither `GET /events` nor calls on `prov` itself (engine/scope.rs, which
/// answers them 501 and 404 and closes); a wait writes its head at once and nothing after it
/// until its joiner ends. The connection ends as `prov.idle` is removed.
fn held_open(home: &Path, image: &str) -> std::thread::JoinHandle<Run> {
    answering(home);
    joiner(home, image, "idle", r#""sleep""#);
    // Every ring so far let go, so that the next one attached is the held connection's.
    await_rings(home, "the setup's rings let go", |r| r.let_go == r.attached);
    let before = rings(home).attached;
    let held = std::thread::spawn({
        let home = home.to_path_buf();
        move || {
            shards_in(
                &home,
                &[
                    "exec",
                    "prov",
                    "/bin/testguest",
                    "api",
                    "POST",
                    "/containers/idle/wait",
                    "",
                ],
            )
        }
    });
    await_rings(home, "the held connection's ring", |r| {
        r.attached > before && r.attached > r.let_go
    });
    held
}

/// As many engine streams at once as `prov`'s microVM has vCPUs (`cpus`), then twice that,
/// each set one test guest's `api-bulk streams` in `prov`: each stream a connection of its own
/// on a thread of its own, the even ones `logs` of `emitter` (`bytes` of it, byte-checked),
/// the odd ones 50 pings. One exec a set, not one a stream: 64 execs at once into a microVM
/// of 32 vCPUs left two that never ended, the engine not asked (ring-pins-v1/logs/r11). What
/// each set said.
fn streams(home: &Path, cpus: usize, bytes: &str) -> Vec<Run> {
    [cpus, 2 * cpus]
        .iter()
        .map(|at_once| {
            let n = at_once.to_string();
            shards_in(
                home,
                &[
                    "exec",
                    "prov",
                    "/bin/testguest",
                    "api-bulk",
                    "streams",
                    "emitter",
                    bytes,
                    &n,
                ],
            )
        })
        .collect()
}

/// An idle microVM pins nothing, and each engine stream's pins go as it ends (D230 section
/// 16): on microVMs of one vCPU, two and the host's every CPU, one whose engine was asked
/// nothing pins the pages the same run without the engine's socket (no relay at all) pins;
/// then as many engine streams at once as it has vCPUs, and twice that, each byte-checked;
/// once they end, every ring the device attached it has let go, bytes for bytes, and the
/// guest's pinned pages come back to what they were idle, exactly.
#[test]
fn an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let bytes = (4u64 << 20).to_string();
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
        let (without, idle) = (pinned(&home, "bare"), pinned(&home, "prov"));
        eprintln!("{cpus} vCPUs: {idle} pages pinned idle with the engine's socket, {without} without");
        assert_eq!(
            idle, without,
            "{cpus} vCPUs: an idle engine's microVM pins pages its run without the socket does not"
        );
        assert_eq!(rings(&home).attached, 0, "{cpus} vCPUs: a ring before any stream");
        emitter(&home, &image, 4 << 20);
        await_rings(&home, "the setup's rings let go", |r| {
            r.attached > 0 && r.let_go == r.attached
        });
        settled(
            &home,
            "prov",
            idle,
            &format!("{cpus} vCPUs: the setup's slots freed"),
        );
        for (said, at_once) in streams(&home, cpus, &bytes).iter().zip([cpus, 2 * cpus]) {
            assert_eq!(said.status, Some(0), "{said}");
            assert_eq!(said.stdout.trim(), format!("streams {at_once} {bytes}"), "{said}");
        }
        await_rings(&home, "every stream's ring let go", |r| r.let_go == r.attached);
        let r = rings(&home);
        assert_eq!(r.unpinned, r.pinned, "{cpus} vCPUs: {r:?}");
        settled(
            &home,
            "prov",
            idle,
            &format!("{cpus} vCPUs: every stream's slot freed"),
        );
        eprintln!(
            "{cpus} vCPUs: {} rings attached and let go, {} bytes pinned and unpinned; {idle} pages pinned again",
            r.attached, r.pinned
        );
        let removed = shards_in(&home, &["rm", "-f", "prov", "bare"]);
        assert_eq!(removed.status, Some(0), "{removed}");
    }
}

/// A give-back lets an idle slot go (D188's `RECLAIM`, D230), on every host: an engine
/// connection held open, idle, keeps its slot until the daemon hears its host is short of
/// memory (`SHARDS_PRESSURE`: a FIFO on Unix, a named pipe on Windows, `common::Told`); the
/// relay then asks the device to let it go, and the connection goes on over plain vsock.
#[test]
fn a_give_back_lets_an_idle_slot_go() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("eng-giveback");
    let mut told = Told::new(&home);
    let pipe = told.path.clone();
    let more: [(&str, &OsStr); 2] = [
        ("SHARDS_LOG", "debug".as_ref()),
        ("SHARDS_PRESSURE", pipe.as_os_str()),
    ];
    let pulled = shards_with(&home, &["pull", "-q", &image], &more);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    prov(&home, &image, &[]);
    let held = held_open(&home, &image);
    let before = rings(&home);
    // The host's word: short of memory.
    told.say();
    await_rings(&home, "the idle slot given back", |r| {
        r.asked > before.asked && r.let_go == r.attached
    });
    let removed = shards_in(&home, &["rm", "-f", "prov.idle", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    let _ = held.join();
}

/// The budget the shrinking-room test holds its run to, its host's own (D183): on Linux a
/// slice of the user's manager (slice 1), its cgroup's `memory.current` what it holds and
/// `MemoryHigh` its budget; on macOS and Windows a group shards keeps, its directory of the
/// home (slice 3), whose `memory.current` the daemon writes each pressure window and whose
/// `memory.high` it holds the group's microVMs to as the file changes. The slice goes as it
/// drops; a group goes with its home.
struct Budget {
    /// `--cgroup-parent`'s value.
    parent: String,
    /// Where `memory.current` is: the slice's cgroup, or the group's directory.
    dir: PathBuf,
}

impl Budget {
    /// Why not, where this Linux host has no user's manager on its bus.
    fn new(home: &Path) -> Result<Budget, String> {
        if cfg!(target_os = "linux") {
            #[cfg(unix)]
            // SAFETY: getuid(2) takes nothing and cannot fail.
            let uid = unsafe { libc::getuid() };
            #[cfg(not(unix))]
            let uid = 0;
            let bus = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| format!("/run/user/{uid}").into())
                .join("bus");
            if !bus.exists() {
                return Err(format!(
                    "no user's systemd manager here: {} is not",
                    bus.display()
                ));
            }
            let parent = format!("shardsring{}.slice", std::process::id());
            let dir = PathBuf::from(format!(
                "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{parent}"
            ));
            Ok(Budget { parent, dir })
        } else {
            let parent = format!("shardsring{}", std::process::id());
            let dir = home.join("groups").join(&parent);
            Ok(Budget { parent, dir })
        }
    }

    /// What its runs hold now, its `memory.current`: waited for where the daemon has yet
    /// to write it (a group's, each pressure window).
    fn held(&self, home: &Path) -> u64 {
        let mut patience = Patience::of(home, TIMEOUT);
        loop {
            let read = std::fs::read_to_string(self.dir.join("memory.current"))
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .filter(|&bytes| bytes > 0);
            if let Some(bytes) = read {
                return bytes;
            }
            assert!(patience.holds(), "{}: no memory.current", self.dir.display());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Holds its runs to `bytes`: the slice's `MemoryHigh` (no swap past it), or the
    /// group's `memory.high`.
    fn hold_to(&self, bytes: u64) {
        if cfg!(target_os = "linux") {
            let set = std::process::Command::new("systemctl")
                .args(["--user", "set-property", "--runtime", &self.parent])
                .arg(format!("MemoryHigh={bytes}"))
                .arg("MemorySwapMax=0")
                .output()
                .unwrap();
            assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
        } else {
            std::fs::write(self.dir.join("memory.high"), format!("{bytes}\n")).unwrap();
        }
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        if cfg!(target_os = "linux") {
            for verb in ["revert", "stop"] {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", verb, &self.parent])
                    .status();
            }
        }
    }
}

/// A shrinking room lets an idle slot go, and a stream past the room goes over plain
/// vsock (D188, D230), on every host: under its budget (`Budget`: a slice of the user's
/// manager on Linux, a group shards keeps elsewhere), an engine connection held open and idle
/// keeps its slot while the budget has room; with the budget brought down to what its run
/// holds, the relay lets it go, and a new stream crosses whole with no ring.
#[test]
fn a_shrinking_room_lets_an_idle_slot_go_and_a_stream_past_it_goes_plain() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("eng-room");
    let budget = match Budget::new(&home) {
        Ok(budget) => budget,
        Err(why) => {
            eprintln!("SKIP: {why}");
            return;
        }
    };
    let debug: [(&str, &OsStr); 1] = [("SHARDS_LOG", "debug".as_ref())];
    let pulled = shards_with(&home, &["pull", "-q", &image], &debug);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Its budget made by the run, with none set yet: room for a slot.
    prov(&home, &image, &["--cgroup-parent", &budget.parent]);
    let held = held_open(&home, &image);
    let before = rings(&home);
    // The budget brought down to what its run holds: no room past it.
    budget.hold_to(budget.held(&home));
    await_rings(&home, "the idle slot let go for the room", |r| {
        r.asked > before.asked && r.let_go == r.attached
    });
    // A new stream past the room: no ring attached, its bytes whole all the same.
    let attached = rings(&home).attached;
    let ping = bulk(&home, &["ping", "20"]);
    assert!(ping.starts_with("ping 20 "), "{ping}");
    assert_eq!(rings(&home).attached, attached, "a ring attached past the room");
    let removed = shards_in(&home, &["rm", "-f", "prov.idle", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    let _ = held.join();
}
