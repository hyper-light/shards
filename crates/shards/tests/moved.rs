//! MicroVMs moved to another machine as they run (D176 part 6), end to end on real
//! microVMs: `shards move vm NAME CONTEXT` of a home's microVM to another home whose daemon
//! listens, reached through a context (D180), as another machine's is. On this machine, two
//! homes stand for two machines: nothing of the one is the other's but what the move sends
//! (its directory, snapshot, volumes and the files its snapshot restores against) and what
//! the other machine pulls itself (its image, by the digest it was made of). A move between
//! two machines is the harness's (docs/research/measurements/migrate).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
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
    let started = shards_in(&there, &["run", "daemon", "--detached", "--listen", "127.0.0.1:0"]);
    assert_eq!(started.status, Some(0), "{started}");
    let listening = Listening(there.to_path_buf());
    let invite = there.join("here.invite");
    let invited = shards_in(
        &there,
        &["invite", "client", "--name", "here", "--output", invite.to_str().unwrap()],
    );
    assert_eq!(invited.status, Some(0), "{invited}");
    let joined = shards_in(
        &here,
        &["create", "context", "lab", "--invite-file", invite.to_str().unwrap()],
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
            "run", "-d", "--pull", "never", "--name", "counter", "-u", "0", "-v", "kept:/kept", &image,
            "appender", "/kept/counts", "20",
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
    assert_eq!(counts_there, wanted, "each count once, in order: {}", said_there());
    let ps = shards_in(&there, &["ps", "--filter", "name=counter", "--format", "{{.Status}}"]);
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
        &["run", "-d", "--pull", "never", "--name", "counter", local, "counter", "20"],
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
    until("counting there", &mut || counts(&log_there()).len() >= n + 10, &log_there);
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
        &["run", "-d", "--pull", "never", "--name", "counter", &image, "counter", "20"],
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
