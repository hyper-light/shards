//! The test harness's own judgments (`common`), held to stand-ins: no `shards`, no microVM.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{Patience, Said, TempDir};

/// A daemon's heartbeat, as it says it each `READY_STALL` while a microVM a client waits on
/// runs (daemon.rs `log_progress`, D209): the nesting probe's VM on HVF at fafe35efc, whose
/// test waited on it to its own bound and past.
const HEARTBEAT: &str = "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 304 ms (+110 ms in 10s), 23390 page faults (+1342)\n";
const HEARTBEAT_AGAIN: &str = "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 385 ms (+80 ms in 10s), 23391 page faults (+1)\n";

/// A microVM's start said slowly (daemon.rs `ready`), as the daemon says it each
/// `READY_STALL` until the microVM is ready or it gives up on it.
const STARTING: &str = "shards daemon 7268: VM 7282: not ready in 10s: its CPU time 3265 ms (+3265 ms in 10s), 2101 page faults (+2101)\n";

fn say(home: &TempDir, what: &str) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("daemon.log"))
        .unwrap()
        .write_all(what.as_bytes())
        .unwrap();
}

/// What a test's waits on a home's daemon take for progress past their bounds (`Said`): each
/// line of its log but its heartbeat, which it says again each `READY_STALL` whatever its
/// microVM runs. Do: say the heartbeat, then say it again; then a microVM's start said slowly;
/// then a run's own line, in two parts. Expect: no progress for the heartbeat, however often
/// it comes; progress for the start (liveness governs a cold start, which the daemon gives up
/// on itself) and for the run's line once it is whole.
#[test]
fn a_daemons_heartbeat_is_no_progress_and_its_other_lines_are() {
    let home = TempDir::new("said");
    say(&home, "shards daemon 70625: serving it\n");
    // Said reads the log once a second.
    let next_read = || std::thread::sleep(Duration::from_secs(1));
    let mut beating = Said::of(&home);
    say(&home, HEARTBEAT);
    assert!(!beating.lately(), "the heartbeat taken for progress");
    next_read();
    say(&home, HEARTBEAT_AGAIN);
    assert!(!beating.lately(), "the heartbeat, said again, taken for progress");
    let mut starting = Said::of(&home);
    say(&home, STARTING);
    assert!(starting.lately(), "a start said slowly not taken for progress");
    let mut running = Said::of(&home);
    say(&home, "shards daemon 70625: container 212eb2365f93: its command ");
    assert!(!running.lately(), "half a line taken for progress");
    next_read();
    say(&home, "ended (status 7)\n");
    assert!(running.lately(), "a run's own line not taken for progress");
}

/// A wait past its bound goes on only while its subject progresses: a stand-in daemon that
/// says nothing but its heartbeat, again before each look, holds no wait past its bound
/// (Linux CI: a nesting test whose runc hung waited on it to the job's 56-minute limit;
/// ROOT's R03 ruling, 2026-11-13). Its wait may take its bound, a second for Said's next read
/// and its own look after.
#[test]
fn a_wait_whose_daemon_says_only_its_heartbeat_ends_at_its_bound() {
    let home = TempDir::new("said-wait");
    say(&home, "shards daemon 70625: serving it\n");
    let bound = Duration::from_millis(500);
    let most = bound + Duration::from_secs(2);
    let began = Instant::now();
    let mut patience = Patience::of(&home, bound);
    for again in [false, true].into_iter().cycle() {
        // The daemon says its heartbeat between each two looks, as often as they come.
        say(&home, if again { HEARTBEAT_AGAIN } else { HEARTBEAT });
        if !patience.holds() || began.elapsed() > most {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let waited = began.elapsed();
    assert!(
        waited <= most,
        "a heartbeat alone held a wait of {bound:?} for {waited:?}"
    );
}
