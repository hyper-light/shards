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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Patience, Run, TempDir, Told, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use shards_api::{Found, Judge, Record, Seen, Stall};

fn shards_in(home: &Path, args: &[&str]) -> Run {
    shards_with(home, args, &[])
}

/// `shards` in `home`, `more` in its environment besides: the daemon a first command starts
/// keeps them (`SHARDS_LOG=debug` has its VM processes say each ring they attach and let go,
/// in its log; `SHARDS_PRESSURE` names the pipe its host's word comes through).
#[track_caller]
fn shards_with(home: &Path, args: &[&str], more: &[(&str, &OsStr)]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let mut env: Vec<(&str, &OsStr)> = vec![
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    env.extend_from_slice(more);
    run_shards_env(&[], args, &env)
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
#[track_caller]
fn answering(home: &Path) {
    let mut patience = Patience::of(home);
    loop {
        let out = shards_in(
            home,
            &["exec", "prov", "/bin/testguest", "api", "GET", "/_ping", ""],
        );
        if out.status == Some(0) && out.stdout == "api HTTP/1.1 200 OK\nOK" {
            patience.done();
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
#[track_caller]
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
    rings_of(&std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default())
}

/// The device's word of its rings in a daemon's log `log`.
fn rings_of(log: &str) -> Rings {
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

/// The work whose slots a settle waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Work {
    /// The setup's: the engine calls that made `emitter`, a connection at a time, and its
    /// wait.
    Setup,
    /// The streams': as many at once as the microVM has vCPUs, then twice that.
    Streams,
}

impl std::fmt::Display for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Work::Setup => "the setup's slots",
            Work::Streams => "every stream's slot",
        })
    }
}

/// A settle's kind: its work at its microVM's vCPU count. The release it waits for is as
/// large as the slots its work leaves the relay to free once the device has let go of every
/// ring, and as quick as the vCPUs that run the relay's streams free them, and the work and
/// the microVM's size set both. So one kind's answers say nothing of another's: at
/// dc7f45bc6 on HVF (ROOT's run) the streams' slots of 1 and 2 vCPUs were freed within as
/// long as their streams took, and 21 pages of 18 vCPUs' were still pinned 207 ms into their
/// settle, where their streams had taken 177 ms.
type SettleKind = (Work, usize);

/// What this test has seen of its settles: each kind's record apart (D215's records,
/// shared), in its memory alone, none until that kind's first answer (the person's ruling
/// of 2026-11-13: no record seeds).
#[derive(Default)]
struct Settles {
    records: HashMap<SettleKind, Record>,
}

impl Settles {
    /// A settle of `kind` beginning now, which waits for its guest's pins to come to `want`;
    /// `work` how long the work it follows took, to the reading of the device's word that it
    /// let go of every ring, for its report alone. That time was its bound before (a2c887eeb):
    /// a seed of no answer of the kind's own, which refused 18 vCPUs on HVF at dc7f45bc6.
    fn begin(&mut self, kind: SettleKind, want: u64, work: Duration) -> Settle<'_> {
        let record: &Record = self.records.entry(kind).or_default();
        Settle {
            judge: Judge::begin(record),
            record,
            kind,
            want,
            work,
            began: Instant::now(),
            lowest: u64::MAX,
            judged: None,
        }
    }
}

/// A settle under way: it looks at its guest's pinned pages until they come to its count,
/// judged by its kind's record (D215's judge, shared), as the person ruled on 2026-11-13: the
/// cold start, and every wait after a run-out, follows the party's liveness and progress;
/// after that the bound is the slowest answer seen; no x2.
/// - Of a kind not yet seen, it follows its microVM's liveness alone: each look is an exec its
///   microVM answers, and one it does not answer fails the test. Its progress is told as it
///   comes ([`settled`]).
/// - Of a kind seen, its judge looks once each W* past the look before, W* the slowest settle
///   of its kind seen, and refuses it where no slot was freed since ([`Stalled`]). Past W*
///   with slots still freed it goes on, and its answer raises W*.
/// - Its gap: a slot never freed while its microVM lives is refused by no settle of a kind
///   not yet seen. Its looks go on, paced, with what it has come to told.
/// - A look reads no hold of its host's on the microVM (shards-api reads that within its own
///   followers), so a host that held the relay's vCPUs for a whole W* would refuse one of a
///   kind seen.
struct Settle<'a> {
    judge: Judge<'a>,
    record: &'a Record,
    kind: SettleKind,
    want: u64,
    work: Duration,
    began: Instant,
    /// The fewest pages a look has found pinned, and the fewest as its judge last looked:
    /// none before its first look, which is its baseline.
    lowest: u64,
    judged: Option<u64>,
}

impl Settle<'_> {
    /// A look found `pins` pages pinned: true where they are its count, which teaches its
    /// kind the time since it began; refused where its judge's look is due and no slot was
    /// freed since the last.
    fn looked(&mut self, pins: u64) -> Result<bool, Stalled> {
        if pins == self.want {
            // Its microVM's CPU is not read here: R*, which only a refusal reports, is none.
            self.judge.answered(Duration::ZERO);
            return Ok(true);
        }
        self.lowest = self.lowest.min(pins);
        let judged = *self.judged.get_or_insert(self.lowest);
        if self.judge.due().is_some_and(|due| Instant::now() >= due) {
            let found = if self.lowest < judged {
                Found::Progressed
            } else {
                Found::Still
            };
            self.judged = Some(self.lowest);
            self.judge.found(found).map_err(|stall| Stalled {
                now: pins,
                want: self.want,
                stall,
            })?;
        }
        Ok(false)
    }

    /// How long to wait before its next look, `look` the one it just made (an exec into its
    /// microVM): D215's `sqrt(2 c w)` of that look's cost and the wait so far, at most until
    /// its judge's next look. A few looks early and fewer as the wait grows.
    fn pause(&self, look: Duration) -> Duration {
        let left = self
            .judge
            .due()
            .map_or(Duration::MAX, |due| due.saturating_duration_since(Instant::now()));
        shards_api::pause(look, self.began.elapsed(), left)
    }

    /// What it has come to, `pins` pages pinned now.
    fn progress(&self, pins: u64) -> String {
        let (work, cpus) = self.kind;
        let seen = match self.record.seen() {
            Seen::Cold => "its kind not yet seen: followed while its microVM answers".to_string(),
            Seen::Learned { slowest, .. } => format!("the slowest settle of its kind seen {slowest:?}"),
        };
        format!(
            "{cpus} vCPUs: {work}: {pins} pages pinned, not {}, {:?} into its settle (its work took {:?}); {seen}",
            self.want,
            self.began.elapsed(),
            self.work
        )
    }
}

/// A settle refused: no slot freed over a whole W*, the slowest settle of its kind seen,
/// since its judge's look before.
#[derive(Debug, PartialEq, Eq)]
struct Stalled {
    /// Pages pinned at the look that refused it, and the count it waits for.
    now: u64,
    want: u64,
    stall: Stall,
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} pages pinned, not {}, after {:?}: none released over the last {:?}, the slowest settle of its kind seen {:?}",
            self.now,
            self.want,
            self.stall.waited(),
            self.stall.over(),
            self.stall.slowest()
        )
    }
}

/// Waits until `name`'s guest pins `want` pages again, its relay having freed the slots of
/// `kind`'s work, which took `work` to the device's word that it let go of every ring. The
/// relay frees a slot once it has heard its device let go and has passed on what the slot
/// held, which a reading after that word may come before ([`Settle`]). How long it took.
fn settled(
    settles: &mut Settles,
    home: &Path,
    name: &str,
    want: u64,
    kind: SettleKind,
    work: Duration,
) -> Result<Duration, Stalled> {
    let mut settle = settles.begin(kind, want, work);
    let mut told: Option<u64> = None;
    loop {
        let look = Instant::now();
        let now = pinned(home, name);
        if settle.looked(now)? {
            return Ok(settle.began.elapsed());
        }
        // Told as it comes, straight to stderr, which libtest does not hold back: its first
        // look short of its count, and each after it that finds a slot freed. Its home is
        // named, where a settle that goes on can be looked into as it does.
        if told.is_none_or(|t| now < t) {
            told = Some(now);
            let _ = writeln!(std::io::stderr(), "{} ({})", settle.progress(now), home.display());
        }
        std::thread::sleep(settle.pause(look.elapsed()));
    }
}

/// Waits until `ready` holds of the device's word.
#[track_caller]
fn await_rings(home: &Path, what: &str, ready: impl Fn(&Rings) -> bool) {
    let mut patience = Patience::of(home);
    while !ready(&rings(home)) {
        assert!(patience.holds(), "{what}: {:?}", rings(home));
        std::thread::sleep(Duration::from_millis(50));
    }
    patience.done();
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
///
/// It returns once the device has said the ring attached, a word its log keeps
/// ([`held_attached`]). That the ring is attached still is no such word: an idle slot goes at
/// whatever asks for it (a give-back, a room told short). At f5c0c8b04 a give-back for its CI
/// runner's own memory pressure let it go 31 ms after it attached, between two of this wait's
/// looks, and the wait, looking for it attached still, waited on for a state already gone
/// (CI run 43435318858, x86_64-unknown-linux-gnu). What asks for the slot is its caller's to
/// hold to its own word.
#[track_caller]
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
    await_rings(home, "the held connection's ring", |r| held_attached(r, before));
    held
}

/// Whether the held connection's ring has attached, the `before` rings attached before it
/// each let go: the device's word that it did, which its log keeps whatever came of the ring
/// since.
fn held_attached(r: &Rings, before: u64) -> bool {
    r.attached > before
}

/// The device's word, as its caller begins to ask for the held connection's slot, that
/// nothing asked for it before: its ring is attached still. Once let go a ring stays let go,
/// so a ring found attached here was attached all along.
fn held_still(home: &Path) -> Rings {
    let r = rings(home);
    assert!(
        r.let_go < r.attached,
        "the held connection's ring let go before anything was asked of it: {r:?}"
    );
    r
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
/// guest's pinned pages come back to what they were idle, exactly ([`Settle`]: each size's
/// settles are kinds of their own).
#[test]
fn an_idle_microvm_pins_nothing_and_each_streams_pins_go_as_it_ends() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let bytes = (4u64 << 20).to_string();
    let mut settles = Settles::default();
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
        let began = Instant::now();
        emitter(&home, &image, 4 << 20);
        await_rings(&home, "the setup's rings let go", |r| {
            r.attached > 0 && r.let_go == r.attached
        });
        let (kind, work) = ((Work::Setup, cpus), began.elapsed());
        let setup = settled(&mut settles, &home, "prov", idle, kind, work)
            .unwrap_or_else(|stalled| panic!("{cpus} vCPUs: {} freed: {stalled}", kind.0));
        let began = Instant::now();
        for (said, at_once) in streams(&home, cpus, &bytes).iter().zip([cpus, 2 * cpus]) {
            assert_eq!(said.status, Some(0), "{said}");
            assert_eq!(said.stdout.trim(), format!("streams {at_once} {bytes}"), "{said}");
        }
        await_rings(&home, "every stream's ring let go", |r| r.let_go == r.attached);
        let r = rings(&home);
        assert_eq!(r.unpinned, r.pinned, "{cpus} vCPUs: {r:?}");
        let (kind, took) = ((Work::Streams, cpus), began.elapsed());
        let freed = settled(&mut settles, &home, "prov", idle, kind, took)
            .unwrap_or_else(|stalled| panic!("{cpus} vCPUs: {} freed: {stalled}", kind.0));
        eprintln!(
            "{cpus} vCPUs: {} rings attached and let go, {} bytes pinned and unpinned; {idle} pages pinned again: the setup's slots {setup:?} into their settle (it took {work:?}), the streams' {freed:?} into theirs (they took {took:?})",
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
    // Nothing has asked for its slot: its host's word comes through `told` alone, and no
    // budget holds its run.
    let before = held_still(&home);
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
/// `MemoryHigh` its budget; or, with no user's manager, as root (a guest's own hierarchy, the
/// L1 path's), a cgroup made in it as an administrator delegates one (slice 2), its own
/// `memory.high`; on macOS and Windows a group shards keeps, its directory of the home (slice
/// 3), whose `memory.current` the daemon writes each pressure window and whose `memory.high` it
/// holds the group's microVMs to as the file changes. Each goes as it drops, a group with its
/// home.
struct Budget {
    /// `--cgroup-parent`'s value.
    parent: String,
    /// Where `memory.current` and `memory.high` are: a cgroup's, or the group's directory.
    dir: PathBuf,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Slice,
    Delegated,
    Group,
}

impl Budget {
    /// Why not, where this Linux host has no user's manager on its bus and this test is not
    /// root.
    fn new(home: &Path) -> Result<Budget, String> {
        if !cfg!(target_os = "linux") {
            let parent = format!("shardsring{}", std::process::id());
            let dir = home.join("groups").join(&parent);
            return Ok(Budget {
                parent,
                dir,
                kind: Kind::Group,
            });
        }
        #[cfg(unix)]
        // SAFETY: getuid(2) takes nothing and cannot fail.
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        let bus = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| format!("/run/user/{uid}").into())
            .join("bus");
        if bus.exists() {
            let parent = format!("shardsring{}.slice", std::process::id());
            let dir = PathBuf::from(format!(
                "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{parent}"
            ));
            return Ok(Budget {
                parent,
                dir,
                kind: Kind::Slice,
            });
        }
        let root = Path::new("/sys/fs/cgroup");
        if uid != 0 || !root.join("cgroup.subtree_control").exists() {
            return Err(format!(
                "no user's systemd manager here ({} is not), and no cgroup of root's to make",
                bus.display()
            ));
        }
        // Root's own hierarchy, where the hierarchy's root may hold processes and give its
        // children the memory controller (cgroup-v2.rst, "No Internal Process Constraint").
        let name = format!("shardsring{}", std::process::id());
        let dir = root.join(&name);
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(root.join("cgroup.subtree_control"), "+memory")
            .map_err(|e| format!("its memory controller: {e}"))?;
        Ok(Budget {
            parent: format!("/{name}"),
            dir,
            kind: Kind::Delegated,
        })
    }

    /// What its runs hold now, its `memory.current`: waited for where the daemon has yet
    /// to write it (a group's, each pressure window).
    #[track_caller]
    fn held(&self, home: &Path) -> u64 {
        let mut patience = Patience::of(home);
        loop {
            let read = std::fs::read_to_string(self.dir.join("memory.current"))
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .filter(|&bytes| bytes > 0);
            if let Some(bytes) = read {
                patience.done();
                return bytes;
            }
            assert!(patience.holds(), "{}: no memory.current", self.dir.display());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Holds its runs to `bytes`: the slice's `MemoryHigh` (no swap past it), or the
    /// cgroup's or group's `memory.high`.
    fn hold_to(&self, bytes: u64) {
        if self.kind == Kind::Slice {
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
        match self.kind {
            Kind::Slice => {
                for verb in ["revert", "stop"] {
                    let _ = std::process::Command::new("systemctl")
                        .args(["--user", verb, &self.parent])
                        .status();
                }
            }
            // Its daemon's leaf in it goes as the daemon does; what is left, with the guest.
            Kind::Delegated => {
                let _ = std::fs::remove_dir(&self.dir);
            }
            Kind::Group => {}
        }
    }
}

/// A shrinking room lets an idle slot go, and a stream past the room goes over plain
/// vsock (D188, D230), on every host: under its budget (`Budget`: a slice of the user's
/// manager on Linux, or root's own cgroup where it has none, a group shards keeps elsewhere),
/// an engine connection held open and idle keeps its slot while the budget has room; with the
/// budget brought down to what its run holds, the relay lets it go, and a new stream crosses
/// whole with no ring.
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
    // Its host's word through a pipe never said: a host short of memory has every idle slot
    // given back (D188, D230), the held one among them, which its CI runner's own pressure
    // took before its room shrank at f5c0c8b04. Here the room alone asks for it, as `told`
    // alone does in the give-back test.
    let told = Told::new(&home);
    let more: [(&str, &OsStr); 2] = [
        ("SHARDS_LOG", "debug".as_ref()),
        ("SHARDS_PRESSURE", told.path.as_os_str()),
    ];
    let pulled = shards_with(&home, &["pull", "-q", &image], &more);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Its budget made by the run, with none set yet: room for a slot.
    prov(&home, &image, &["--cgroup-parent", &budget.parent]);
    let held = held_open(&home, &image);
    let before = held_still(&home);
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

/// A settle of a size not yet seen follows its microVM's liveness, whatever other kinds
/// answered (no VM: the settle fed the looks of ROOT's HVF run at dc7f45bc6). There the
/// setup's slots of 18 vCPUs and the streams' of 1 and 2 were freed as they went, and then
/// the streams' of 18 still held 21 pages 207 ms into their settle: its window, the 177 ms
/// their streams took (a2c887eeb), refused it there. Here it is looked at past both, never
/// refused, and its answer, when it comes, teaches its kind alone.
#[test]
fn a_settle_of_a_size_not_yet_seen_follows_its_liveness_whatever_other_kinds_answered() {
    let ms = Duration::from_millis;
    let mut settles = Settles::default();
    let others = [
        ((Work::Setup, 18), 3),
        ((Work::Streams, 1), 6),
        ((Work::Streams, 2), 12),
    ];
    for (kind, pins) in others {
        let mut settle = settles.begin(kind, 0, ms(60));
        assert_eq!(settle.looked(pins), Ok(false), "{kind:?}");
        std::thread::sleep(ms(5));
        assert_eq!(settle.looked(0), Ok(true), "{kind:?}");
    }
    let taught = |settles: &Settles| others.map(|(kind, _)| settles.records.get(&kind).map(Record::seen));
    let before = taught(&settles);
    let hvf = (Work::Streams, 18);
    let (refused_at, took) = (Duration::from_micros(206_907), Duration::from_micros(176_722));
    let looked = {
        let mut settle = settles.begin(hvf, 0, took);
        let mut looked = 0;
        // Its last look begins past the time HVF's was refused at, however late a pause ends.
        loop {
            let at = settle.began.elapsed();
            assert_eq!(settle.looked(21), Ok(false), "{at:?} in");
            looked += 1;
            if at > refused_at {
                break;
            }
            // A look's cost its exec's time: HVF's first answered 5.824 ms into its settle (its
            // 206.907 ms waited less the 201.082 ms its refusal looked over).
            std::thread::sleep(settle.pause(Duration::from_micros(5_824)));
        }
        assert_eq!(settle.looked(0), Ok(true));
        looked
    };
    assert!(looked > 1, "{looked} looks");
    let Seen::Learned { slowest, .. } = settles.records[&hvf].seen() else {
        panic!("its answer taught nothing");
    };
    assert!(slowest > refused_at, "{slowest:?}");
    assert_eq!(taught(&settles), before, "another kind learned of its answer");
}

/// A settle of a kind seen is refused a whole W* after its last freed slot, W* the slowest
/// settle of its kind seen (D215's judge): a slot freed within its first W* is progress, and
/// a slot never freed after it is refused at the look W* after, with how far in, the time
/// looked over and W*.
#[test]
fn a_settle_of_a_kind_seen_is_refused_a_whole_w_star_after_its_last_freed_slot() {
    let ms = Duration::from_millis;
    let kind = (Work::Streams, 4);
    let mut settles = Settles::default();
    {
        let mut first = settles.begin(kind, 0, ms(10));
        assert_eq!(first.looked(9), Ok(false));
        std::thread::sleep(ms(30));
        assert_eq!(first.looked(0), Ok(true));
    }
    let Seen::Learned { slowest, .. } = settles.records[&kind].seen() else {
        panic!("its first answer taught nothing");
    };
    let mut settle = settles.begin(kind, 0, ms(10));
    assert_eq!(settle.looked(9), Ok(false));
    let stalled = loop {
        std::thread::sleep(settle.pause(ms(1)));
        match settle.looked(6) {
            Ok(false) => {}
            Ok(true) => panic!("settled at 6 pages, not 0"),
            Err(stalled) => break stalled,
        }
    };
    assert_eq!((stalled.now, stalled.want), (6, 0));
    assert_eq!(stalled.stall.slowest(), slowest);
    assert!(stalled.stall.over() >= slowest, "{stalled}");
    assert!(stalled.stall.waited() >= slowest * 2, "{stalled}");
}

/// A settle of a kind seen goes on past W* while its slots are still freed, every W* one at
/// least, and its answer raises its kind's W* to its own time: a run-out followed by its
/// progress (the person's ruling of 2026-11-13).
#[test]
fn a_settle_of_a_kind_seen_goes_on_past_w_star_while_its_slots_are_freed() {
    let ms = Duration::from_millis;
    let kind = (Work::Setup, 2);
    let mut settles = Settles::default();
    {
        let mut first = settles.begin(kind, 0, ms(10));
        assert_eq!(first.looked(3), Ok(false));
        std::thread::sleep(ms(20));
        assert_eq!(first.looked(0), Ok(true));
    }
    let Seen::Learned { slowest, .. } = settles.records[&kind].seen() else {
        panic!("its first answer taught nothing");
    };
    let took = {
        let mut settle = settles.begin(kind, 0, ms(10));
        // A slot freed each third of W*: 9 pages to none over three W*.
        let pins = |settle: &Settle<'_>| {
            let thirds = settle.began.elapsed().as_nanos() * 3 / slowest.as_nanos().max(1);
            9u64.saturating_sub(u64::try_from(thirds).unwrap_or(u64::MAX))
        };
        while !settle
            .looked(pins(&settle))
            .unwrap_or_else(|s| panic!("refused: {s}"))
        {
            std::thread::sleep(settle.pause(ms(1)));
        }
        settle.began.elapsed()
    };
    assert!(took > slowest * 2, "{took:?} {slowest:?}");
    let Seen::Learned { slowest: now, .. } = settles.records[&kind].seen() else {
        panic!("its answers taught nothing");
    };
    assert!(now > slowest * 2, "{now:?} {slowest:?}");
}

/// The held connection's wait reads the device's word that its ring attached, which its log
/// keeps (no VM: the device's lines of CI's x86_64-unknown-linux-gnu run 43435318858 at
/// f5c0c8b04, the daemon's give-back for its runner's memory pressure among them). That give-
/// back let the ring go 31 ms after it attached, between two of the wait's looks, and the
/// wait, looking for it attached still, waited 600 s on a state already gone.
#[test]
fn the_held_connections_wait_reads_a_word_its_log_keeps() {
    let log = "\
[460995     1030851us DEBUG] vsock: port 1029: an engine ring attached, 12288 bytes pinned
[460995     1031775us DEBUG] vsock: port 1029: an engine ring let go, 12288 bytes unpinned (HostEnded)
[460995     1051771us DEBUG] vsock: port 1029: an engine ring attached, 12288 bytes pinned
[460995     1060806us DEBUG] vsock: port 1029: an engine ring let go, 12288 bytes unpinned (HostEnded)
[460995     1083997us DEBUG] vsock: port 1029: an engine ring attached, 12288 bytes pinned
[460995     1249784us DEBUG] vsock: port 1029: an engine ring let go, 12288 bytes unpinned (HostEnded)
[460995     1272652us DEBUG] vsock: port 1029: an engine ring attached, 12288 bytes pinned
[460805    1289572us DEBUG] container 114ca3cfef2e5bac44abe381fecd7314fc4935ce5661780df16074dc233a73f5: giving back what it holds cold: answered after 69.022659ms
[460995     1303913us DEBUG] vsock: port 1029: an engine ring let go, 12288 bytes unpinned (Asked)
";
    let r = rings_of(log);
    assert_eq!(
        r,
        Rings {
            attached: 4,
            pinned: 49152,
            let_go: 4,
            unpinned: 49152,
            asked: 1
        }
    );
    // The setup's three rings, each let go before the held connection's attached.
    assert!(held_attached(&r, 3));
    assert!(!held_attached(&r, 4));
}
