//! The test harness's own judgments (`common`), held to stand-ins: no `shards`, no microVM.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{Floor, Patience, Said, TempDir};

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
/// line of its log but its heartbeat; and its heartbeat by what it did, as D215's judge
/// judges a wait (ROOT's ruling, 2026-11-13), which during a microVM's cold start, before its
/// run has answered, is liveness. Do: say the heartbeat of a microVM whose run has not
/// answered; then a microVM's start said slowly; then a run's own line, in two parts. Expect:
/// progress for each, the line once it is whole.
#[test]
fn a_cold_heartbeat_and_a_daemons_other_lines_are_progress() {
    let home = TempDir::new("said");
    say(&home, "shards daemon 70625: serving it\n");
    // Said reads the log once a second.
    let next_read = || std::thread::sleep(Duration::from_secs(1));
    let mut cold = Said::of(&home);
    say(&home, HEARTBEAT);
    assert!(cold.lately(), "a cold microVM's heartbeat not taken for liveness");
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

/// The nesting probe's microVM on HVF at fafe35efc, its heartbeats as its daemon said them
/// as its runc hung: the first, once its run had answered (its first steps said), then the
/// rest at its idle rate, its guest's timers alone.
const ANSWERED: &str = HEARTBEAT;
const IDLE: [&str; 7] = [
    HEARTBEAT_AGAIN,
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 480 ms (+94 ms in 10s), 23391 page faults (+0)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 577 ms (+96 ms in 10s), 23404 page faults (+13)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 677 ms (+100 ms in 10s), 23405 page faults (+1)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 773 ms (+96 ms in 10s), 23407 page faults (+2)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 841 ms (+67 ms in 10s), 23574 page faults (+167)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 905 ms (+63 ms in 10s), 23577 page faults (+3)\n",
];

/// The same microVM's heartbeats as it works: gigabytes allocated, as D209's TCG cases did,
/// its CPU and faults far past its idle (the daemon's own example, platform::Progress).
const WORKING: [&str; 2] = [
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 32249 ms (+21671 ms in 10s), 812345 page faults (+5120)\n",
    "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 51032 ms (+18783 ms in 10s), 1231874 page faults (+419529)\n",
];

/// A wait of `bound` on `home`'s daemon, which says `heartbeats` in turn between each two
/// looks, as often as they come, until the wait gives up or `most` has passed: how long it
/// waited.
fn waited(home: &TempDir, bound: Duration, most: Duration, heartbeats: &[&str]) -> Duration {
    let began = Instant::now();
    let mut patience = Patience::of(home, bound);
    for beat in heartbeats.iter().cycle() {
        say(home, beat);
        if !patience.holds() || began.elapsed() > most {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    began.elapsed()
}

/// Its run having answered, the nesting probe's microVM's heartbeat taught its floor (D215's
/// least rate, ROOT's ruling of 2026-11-13: an answer's own); one wait on its daemon that
/// then says only its idle heartbeats, as its runc hung, ends at its bound, and one that says
/// its heartbeats of work goes on past it. Its wait may take its bound, a second for Said's
/// next read and its own look after.
#[test]
fn a_wait_on_idle_heartbeats_ends_at_its_bound_and_one_on_work_goes_on() {
    let home = TempDir::new("said-wait");
    say(&home, "shards daemon 70625: serving it\n");
    // Its run answers, then its first heartbeat comes: the floor it teaches.
    let mut run = Said::of(&home);
    run.answered();
    say(&home, ANSWERED);
    assert!(
        run.lately(),
        "the first heartbeat of its run, its cold start's, not liveness"
    );
    let bound = Duration::from_millis(500);
    let most = bound + Duration::from_secs(2);
    let idle = waited(&home, bound, most, &IDLE);
    assert!(
        idle <= most,
        "idle heartbeats alone held a wait of {bound:?} for {idle:?}"
    );
    let working = waited(&home, bound, most, &WORKING);
    assert!(
        working > most,
        "heartbeats of work held a wait of {bound:?} only {working:?}"
    );
}

/// A VM process's slices of work as `common::Ran` looks at them each `PROGRESS_SLICE`, judged
/// by its floor as a daemon's heartbeats are (ROOT's ruling, 2026-11-13): during its cold
/// start, before its run has answered, by its liveness alone (`platform::progressed`); once
/// its run answered within a window, that window teaches its floor, past which a build step's
/// work that printed nothing counts and an idle-hung microVM's timer ticks do not.
#[test]
fn a_vm_processes_slices_count_past_its_cold_start_only_above_its_floor() {
    let (ms, secs) = (Duration::from_millis, Duration::from_secs);
    let mut floor = Floor::new(false);
    assert!(!floor.slice(ms(40), secs(10), false), "cold, and idle");
    assert!(floor.slice(ms(70), secs(10), true), "cold, and live");
    floor.answered();
    assert!(
        floor.slice(ms(3265), secs(10), true),
        "the window its run answered within"
    );
    assert!(
        !floor.slice(ms(90), secs(10), true),
        "an idle microVM's timer ticks"
    );
    assert!(
        floor.slice(ms(21671), secs(10), true),
        "a silent build step's work"
    );
}

/// The stand-in a test's wait follows here: this test binary run again for this test alone,
/// doing what `SHARDS_STAND_IN` says: `exit`, at once; `busy MS`, its CPU busy for MS
/// milliseconds, then its end; `idle`, nothing at all until its stdin closes.
#[test]
#[ignore = "a process of its own for the harness's stand-ins, which run it"]
fn stand_in() {
    use std::io::Read as _;
    let asked = std::env::var("SHARDS_STAND_IN").unwrap_or_default();
    if let Some(ms) = asked.strip_prefix("busy ").and_then(|ms| ms.parse().ok()) {
        let until = Instant::now() + Duration::from_millis(ms);
        let mut spun = 0u64;
        while Instant::now() < until {
            spun = std::hint::black_box(spun.wrapping_add(1));
        }
    } else if asked == "idle" {
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    }
}

/// [`stand_in`], doing `what`.
fn stand_in_doing(what: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["stand_in", "--exact", "--ignored", "--test-threads=1"])
        .env("SHARDS_STAND_IN", what);
    cmd
}

/// A command is waited on while it works: its caller gives its wait no bound, as a wait
/// follows its party's liveness and progress (the person's ruling of 2026-11-13).
/// api_image's and build.rs's builds on jupiter were cut at their tests' 120 s while they
/// worked under its headroom guard (WHP chain B, 8cc1f6673), as a command that is no
/// microVM's was cut at its caller's bound however it worked: red so at f370b8b60, a
/// stand-in busy for 1 s cut at a bound of 100 ms. Do: run a stand-in busy for a second.
/// Expect: its own end, status 0, past its work.
#[test]
fn a_command_is_waited_on_while_it_works() {
    let busy = Duration::from_secs(1);
    let asked = ["stand-in".to_string(), format!("busy {}", busy.as_millis())];
    let run = common::run_command(stand_in_doing(&asked[1]), &asked);
    assert_eq!(run.status, Some(0), "{run}");
    assert!(
        run.elapsed >= busy,
        "it ended {:?} in, before its work",
        run.elapsed
    );
}

/// A process its host stops or suspends is its host's hold, not its own stall: a wait whose
/// kind has an answer (W*) does not refuse it at a look past W*, as a kiln runner's
/// headroom guard suspends a test's processes to pace them (ROOT, 2026-11-13; WHP chain A,
/// 0a3dde0db: daemon.rs's six stop tests cut while the guard held their daemons and
/// microVMs). Do: teach the kind its W* by a stand-in's own end; then follow a stand-in
/// stopped (SIGSTOP; on Windows made with its thread suspended) through two of its judge's
/// looks past W*. Expect: neither refuses it.
#[test]
fn a_stopped_process_is_its_hosts_hold_and_not_refused() {
    let kind = common::WaitKind::at(std::panic::Location::caller());
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = common::Wait::begin(kind, quick.id(), None, "a quick stand-in");
    while quick.try_wait().unwrap().is_none() {
        taught.look(false).unwrap();
    }
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = kind.record().seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let mut stopped = stand_in_doing("idle");
    stopped
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(
        &mut stopped,
        windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
    );
    let mut stopped = stopped.spawn().unwrap();
    // SAFETY: kill(2) of a child of this test's.
    #[cfg(unix)]
    unsafe {
        libc::kill(i32::try_from(stopped.id()).unwrap(), libc::SIGSTOP);
    }
    let mut wait = common::Wait::begin(kind, stopped.id(), None, "a stopped stand-in");
    let began = Instant::now();
    // Its judge's first two looks: W* past its start, and W* past that.
    while began.elapsed() <= slowest.saturating_mul(2) {
        let judged = wait.look(false);
        assert!(judged.is_ok(), "{}", wait.account());
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = stopped.kill();
    let _ = stopped.wait();
}
