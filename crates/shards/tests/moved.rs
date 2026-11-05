//! MicroVMs moved to another machine as they run (D176 part 6), end to end on real
//! microVMs: `shards move vm NAME CONTEXT` of a home's microVM to another home whose daemon
//! listens, reached through a context (D180), as another machine's is. On this machine, two
//! homes stand for two machines: nothing of the one is the other's but what the move sends
//! (its directory, snapshot, volumes and the files its snapshot restores against) and what
//! the other machine pulls itself (its image, by the digest it was made of). Between two
//! machines, `a_microvm_moves_to_another_host_saved_and_live` moves microVMs to the machine
//! whose invitation `SHARDS_MOVE_INVITE` names, and the harness measures moves there
//! (docs/research/measurements/migrate/two-hosts.sh).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Run, TempDir, cannot_pool, cannot_run_vms, guest_init, kernel, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(120);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// A home whose daemon listens for another machine's clients, stopped as this drops.
struct Listening(PathBuf);

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = shards_in(&self.0, &["daemon", "stop"]);
    }
}

/// Two homes, two machines: `there`, whose daemon listens on loopback, and `here`, joined to
/// it as context `lab`; the test image pulled here alone.
fn machines(name: &str) -> Option<(TempDir, TempDir, Listening, String)> {
    if cannot_run_vms() || cannot_pool() {
        return None;
    }
    let (image, _) = served();
    let here = TempDir::new(&format!("{name}-h"));
    let there = TempDir::new(&format!("{name}-t"));
    let started = shards_in(
        &there,
        &["run", "daemon", "--detached", "--listen", "127.0.0.1:0"],
    );
    assert_eq!(started.status, Some(0), "{started}");
    let listening = Listening(there.to_path_buf());
    let invite = there.join("here.invite");
    let invited = shards_in(
        &there,
        &[
            "invite",
            "client",
            "--name",
            "here",
            "--output",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    let joined = shards_in(
        &here,
        &[
            "create",
            "context",
            "lab",
            "--invite-file",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let pulled = shards_in(&here, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    Some((here, there, listening, image))
}

fn logs(home: &Path, name: &str) -> String {
    let logs = shards_in(home, &["logs", name]);
    assert_eq!(logs.status, Some(0), "{logs}");
    logs.stdout
}

fn counts(logs: &str) -> Vec<u64> {
    logs.lines().filter_map(|l| l.parse().ok()).collect()
}

fn until(what: &str, done: &mut dyn FnMut() -> bool, said: &dyn Fn() -> String) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}: {}", said());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The end of a home's daemon log, for a failing test to say.
fn daemon_log(home: &Path) -> String {
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// A running microVM moved to another machine goes on there where it was: its counter
/// counts on from the count it had, each count once, its log moved with it; its image found
/// there by its digest, pulled from the registry it came from; and here it is no more. Its
/// volume moves with it, a copy of it there, which its restored server serves by its paths:
/// the guest appends on to the file it holds open, from the directory it works in.
#[test]
fn a_running_microvm_moves_to_another_machine_and_goes_on_there() {
    let Some((here, there, _listening, image)) = machines("moved") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            "counter",
            "-u",
            "0",
            "-v",
            "kept:/kept",
            &image,
            "appender",
            "/kept/counts",
            "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let file_here = here.join("volumes/kept/_data/counts");
    let written = |file: &Path| -> Vec<u64> {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    };
    let said_here = || daemon_log(&here);
    until("appending", &mut || written(&file_here).len() >= 10, &said_here);
    let moved = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    // Gone from here, its volume's copy here as it was at the move.
    let gone = shards_in(&here, &["inspect", "counter"]);
    assert_eq!(gone.status, Some(1), "{gone}");
    let at_move = written(&file_here);
    // Going on there, appending to its volume's copy there.
    let file_there = there.join("volumes/kept/_data/counts");
    let said_there = || format!("{}\n{}", daemon_log(&there), logs(&there, "counter"));
    until(
        "appending there",
        &mut || written(&file_there).len() >= at_move.len() + 10,
        &said_there,
    );
    let counts_there = written(&file_there);
    let wanted: Vec<u64> = (1..=counts_there.len() as u64).collect();
    assert_eq!(
        counts_there,
        wanted,
        "each count once, in order: {}",
        said_there()
    );
    let ps = shards_in(
        &there,
        &["ps", "--filter", "name=counter", "--format", "{{.Status}}"],
    );
    assert!(ps.stdout.starts_with("Up "), "{ps}");
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));
}

/// A microVM's output goes on in its log there from where it was, each count once: its log
/// moved with it. An image no registry the other machine reaches holds goes there too,
/// saved here and loaded there, by its digest.
#[test]
fn a_moved_microvms_log_and_image_go_with_it() {
    let Some((here, there, _listening, image)) = machines("moved-log") else {
        return;
    };
    // A name no registry holds: the other machine cannot pull it.
    let local = "127.0.0.1:1/only/here:v1";
    assert_eq!(shards_in(&here, &["tag", &image, local]).status, Some(0));
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", local, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "counter");
    until("counting", &mut || counts(&log_here()).len() >= 10, &log_here);
    let moved = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    let log_there = || logs(&there, "counter");
    let n = counts(&log_there()).len();
    until(
        "counting there",
        &mut || counts(&log_there()).len() >= n + 10,
        &log_there,
    );
    let all = counts(&log_there());
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, from here on to there"
    );
    // Its image is there now, by its name here.
    let inspected = shards_in(&there, &["image", "inspect", "-f", "{{.Id}}", local]);
    let id_here = shards_in(&here, &["image", "inspect", "-f", "{{.Id}}", local]);
    assert_eq!(inspected.stdout, id_here.stdout, "{inspected}");
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));
}

/// A move the other machine cannot take leaves the microVM here as it was, running on:
/// another microVM there has its name.
#[test]
fn a_move_refused_leaves_the_microvm_running_here() {
    let Some((here, there, _listening, image)) = machines("unmoved") else {
        return;
    };
    let taken = shards_in(&there, &["create", "--name", "counter", &image, "counter", "20"]);
    assert_eq!(taken.status, Some(0), "{taken}");
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&here, "counter");
    until("counting", &mut || counts(&log()).len() >= 5, &log);
    let refused = shards_in(&here, &["move", "vm", "counter", "lab"]);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(
        refused.stderr.contains("a microVM named counter is here already")
            && refused.stderr.contains("counter goes on here"),
        "{refused}"
    );
    let n = counts(&log()).len();
    until("counting on here", &mut || counts(&log()).len() >= n + 5, &log);
    let all = counts(&log());
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>());
    assert_eq!(shards_in(&here, &["rm", "-f", "counter"]).status, Some(0));
}

/// `shards move vm --live NAME lab` with what it reports of its rounds and its downtime
/// (`SHARDS_MOVE_TIMES`): the run, and the downtime in µs where it said one.
fn move_live(here: &Path, name: &str) -> (Run, Option<u64>) {
    move_live_to(here, name, "lab")
}

/// [`move_live`], to context `to`.
fn move_live_to(here: &Path, name: &str, to: &str) -> (Run, Option<u64>) {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_MOVE_TIMES", OsStr::new("1")),
    ];
    let moved = run_shards_env(&[], &["move", "vm", "--live", name, to], &env, TIMEOUT);
    let downtime = moved.stderr.lines().find_map(|l| {
        l.strip_prefix("move: downtime ")?
            .strip_suffix(" us")?
            .parse()
            .ok()
    });
    (moved, downtime)
}

/// A running microVM moved live goes on there where it was, as it ran on here while its
/// memory was copied: its counter counts on, each count once, through rounds that sent what
/// it wrote as it ran; held here only for its last save, the time it ran nowhere said. A
/// move the other machine cannot take leaves it running here, its last save let go.
#[test]
fn a_running_microvm_moves_live_and_goes_on_there() {
    let Some((here, there, _listening, image)) = machines("live") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "counter");
    until("counting", &mut || counts(&log_here()).len() >= 10, &log_here);
    let (moved, downtime) = move_live(&here, "counter");
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "counter\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    assert!(moved.stderr.contains("move: round 0: "), "{moved}");
    let downtime = downtime.unwrap_or_else(|| panic!("no downtime said: {moved}"));
    eprintln!("downtime {downtime} us");
    let gone = shards_in(&here, &["inspect", "counter"]);
    assert_eq!(gone.status, Some(1), "{gone}");
    let log_there = || logs(&there, "counter");
    let n = counts(&log_there()).len();
    until(
        "counting there",
        &mut || counts(&log_there()).len() >= n + 10,
        &log_there,
    );
    let all = counts(&log_there());
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, from here on to there"
    );
    assert_eq!(shards_in(&there, &["rm", "-f", "counter"]).status, Some(0));

    // Refused there: it runs on here, as it was.
    let taken = shards_in(&there, &["create", "--name", "busy", &image, "counter", "20"]);
    assert_eq!(taken.status, Some(0), "{taken}");
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", "busy", &image, "counter", "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log = || logs(&here, "busy");
    until("counting", &mut || counts(&log()).len() >= 5, &log);
    let (refused, _) = move_live(&here, "busy");
    assert_eq!(refused.status, Some(1), "{refused}");
    assert!(refused.stderr.contains("busy goes on here"), "{refused}");
    let n = counts(&log()).len();
    until("counting on here", &mut || counts(&log()).len() >= n + 5, &log);
    assert_eq!(shards_in(&here, &["rm", "-f", "busy"]).status, Some(0));
}

/// A microVM writing its memory as fast as its workload stamps it moves live with none of
/// it lost: every page holds its last write there, its guest checking each after every
/// sweep (testguest's `scribble`), as the rounds chased its writes and its last save held
/// what they had not.
#[test]
fn a_microvm_writing_its_memory_moves_live_with_none_of_it_lost() {
    let Some((here, there, _listening, image)) = machines("live-mem") else {
        return;
    };
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            "scribbler",
            &image,
            "scribble",
            "64",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, "scribbler");
    until("sweeping", &mut || counts(&log_here()).len() >= 2, &log_here);
    let (moved, downtime) = move_live(&here, "scribbler");
    assert_eq!(
        (moved.status, moved.stdout.as_str()),
        (Some(0), "scribbler\n"),
        "{moved}\nhere:\n{}\nthere:\n{}",
        daemon_log(&here),
        daemon_log(&there)
    );
    eprintln!("downtime {downtime:?} us: {}", moved.stderr);
    let log_there = || logs(&there, "scribbler");
    let n = counts(&log_there()).len();
    until(
        "sweeping there",
        &mut || counts(&log_there()).len() >= n + 3 || log_there().contains("BAD"),
        &log_there,
    );
    let said = log_there();
    assert!(!said.contains("BAD"), "a page lost its last write: {said}");
    let all = counts(&said);
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>(), "{said}");
    assert_eq!(shards_in(&there, &["rm", "-f", "scribbler"]).status, Some(0));
}

/// What a test moved to the other machine, removed there as it drops, the test passing or
/// failing: microVMs and volumes of names its own.
struct LeftThere<'a> {
    here: &'a Path,
    microvms: Vec<String>,
    volumes: Vec<String>,
}

impl Drop for LeftThere<'_> {
    fn drop(&mut self) {
        for name in &self.microvms {
            let _ = shards_in(self.here, &["--context", "peer", "rm", "-f", name]);
        }
        for name in &self.volumes {
            let _ = shards_in(self.here, &["--context", "peer", "volume", "rm", "-f", name]);
        }
    }
}

/// A microVM moved to another machine, the one whose invitation `SHARDS_MOVE_INVITE` names
/// (a file: two-hosts.sh `invite`, run there, makes one), as context `peer`; the test image
/// pulled here alone, from this machine's own loopback registry, which the other cannot
/// reach: its image goes with the move, saved here and loaded there, and so do the files its
/// snapshot restores against that the other has not alike (its kernel and init among them,
/// where that machine has none at their paths). A host whose hypervisor gives a live move
/// nothing to watch writes by (KVM on arm64, WHP) refuses one, the microVM going on here.
///
/// Saved: a counter appending to a file it holds open in a volume goes on there from its
/// count, each count once, in order, its volume and log with it, and is gone from here.
/// Live: a guest stamping 16 MiB page by page goes on sweeping there, every page holding its
/// last write, the time it ran nowhere said. 16, not 64 as between homes: it rewrites them
/// at 100 MiB/s, which a network between machines may not carry, so that its rounds end by
/// their count, and what they send then is the size of what it stamps.
#[test]
fn a_microvm_moves_to_another_host_saved_and_live() {
    let Some(invite) = std::env::var_os("SHARDS_MOVE_INVITE") else {
        eprintln!("SKIP: SHARDS_MOVE_INVITE names no other machine's invitation (two-hosts.sh invite)");
        return;
    };
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let here = TempDir::new("peer");
    let invite = PathBuf::from(invite);
    let joined = shards_in(
        &here,
        &[
            "create",
            "context",
            "peer",
            "--invite-file",
            invite.to_str().unwrap(),
        ],
    );
    assert_eq!(joined.status, Some(0), "{joined}");
    let pulled = shards_in(&here, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let id = std::process::id();
    let (counter, volume, scribbler) = (format!("cold{id}"), format!("kept{id}"), format!("live{id}"));
    let _left = LeftThere {
        here: &here,
        microvms: vec![counter.clone(), scribbler.clone()],
        volumes: vec![volume.clone()],
    };
    let logs_there = |name: &str| shards_in(&here, &["--context", "peer", "logs", name]).stdout;
    let moved_ok = |moved: &Run, name: &str| {
        assert_eq!(
            (moved.status, moved.stdout.as_str()),
            (Some(0), format!("{name}\n").as_str()),
            "{moved}\nhere:\n{}",
            daemon_log(&here)
        );
        let gone = shards_in(&here, &["inspect", name]);
        assert_eq!(gone.status, Some(1), "{gone}");
    };

    // Saved.
    let mount = format!("{volume}:/kept");
    let run = shards_in(
        &here,
        &[
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            &counter,
            "-u",
            "0",
            "-v",
            &mount,
            &image,
            "appender",
            "/kept/counts",
            "20",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, &counter);
    until("appending", &mut || counts(&log_here()).len() >= 10, &log_here);
    let (kernel, init) = (kernel(), guest_init());
    let timed: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", here.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
        ("SHARDS_MOVE_TIMES", OsStr::new("1")),
    ];
    let moved = run_shards_env(&[], &["move", "vm", &counter, "peer"], &timed, TIMEOUT);
    moved_ok(&moved, &counter);
    eprintln!("saved move: {}", moved.stderr);
    let n = counts(&logs_there(&counter)).len();
    until(
        "appending there",
        &mut || counts(&logs_there(&counter)).len() >= n + 10,
        &|| logs_there(&counter),
    );
    let all = counts(&logs_there(&counter));
    assert_eq!(
        all,
        (1..=all.len() as u64).collect::<Vec<_>>(),
        "each count once, in order, from here on to there"
    );
    let ps = shards_in(
        &here,
        &[
            "--context",
            "peer",
            "ps",
            "--filter",
            &format!("name={counter}"),
            "--format",
            "{{.Status}}",
        ],
    );
    assert!(ps.stdout.starts_with("Up "), "{ps}");

    // Live.
    let run = shards_in(
        &here,
        &[
            "run", "-d", "--pull", "never", "--name", &scribbler, &image, "scribble", "16",
        ],
    );
    assert_eq!(run.status, Some(0), "{run}");
    let log_here = || logs(&here, &scribbler);
    until("sweeping", &mut || counts(&log_here()).len() >= 2, &log_here);
    let (moved, downtime) = move_live_to(&here, &scribbler, "peer");
    let watched = cfg!(any(
        target_os = "macos",
        all(target_os = "linux", target_arch = "x86_64")
    ));
    if !watched {
        assert_eq!(moved.status, Some(1), "{moved}");
        assert!(moved.stderr.contains("goes on here"), "{moved}");
        return;
    }
    moved_ok(&moved, &scribbler);
    assert!(moved.stderr.contains("move: round 0: "), "{moved}");
    let downtime = downtime.unwrap_or_else(|| panic!("no downtime said: {moved}"));
    eprintln!("live move: downtime {downtime} us; {}", moved.stderr);
    let n = counts(&logs_there(&scribbler)).len();
    until(
        "sweeping there",
        &mut || {
            let said = logs_there(&scribbler);
            counts(&said).len() >= n + 3 || said.contains("BAD")
        },
        &|| logs_there(&scribbler),
    );
    let said = logs_there(&scribbler);
    assert!(!said.contains("BAD"), "a page lost its last write: {said}");
    let all = counts(&said);
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>(), "{said}");
}

/// The downtime of live moves, measured (PM, D176 part 7): `SHARDS_MOVES` microVMs, each
/// writing memory as it counts, moved live from here to there and back, one at a time; each
/// move's downtime, from its last save asked to its restore there said done, by this
/// machine's clock, in µs, with n, p50, p90, p99 and max. Ignored: run by the harness
/// (docs/research/measurements/migrate).
#[test]
#[ignore = "a measurement, run by docs/research/measurements/migrate"]
fn live_move_downtime() {
    let Some((here, there, _listening, image)) = machines("live-n") else {
        return;
    };
    let n: usize = std::env::var("SHARDS_MOVES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let mut downtimes = Vec::new();
    for i in 0..n {
        let name = format!("m{i}");
        let run = shards_in(
            &here,
            &[
                "run", "-d", "--pull", "never", "--name", &name, &image, "counter", "20",
            ],
        );
        assert_eq!(run.status, Some(0), "{run}");
        let log = || logs(&here, &name);
        until("counting", &mut || counts(&log()).len() >= 5, &log);
        let (moved, downtime) = move_live(&here, &name);
        assert_eq!(moved.status, Some(0), "{moved}");
        let rounds: Vec<&str> = moved
            .stderr
            .lines()
            .filter(|l| l.starts_with("move: ") && !l.starts_with("move: saved at"))
            .collect();
        eprintln!("move {i}: {downtime:?} us; {rounds:?}");
        downtimes.extend(downtime);
        assert_eq!(shards_in(&there, &["rm", "-f", &name]).status, Some(0));
    }
    downtimes.sort_unstable();
    let at = |q: f64| downtimes[((downtimes.len() as f64 - 1.0) * q).round() as usize];
    eprintln!(
        "live move downtime (us): n {} p50 {} p90 {} p99 {} max {}",
        downtimes.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        downtimes.last().copied().unwrap_or(0)
    );
}
