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

/// A home's daemon's words, a wait's party's (`Said`): each whole line its log gains, its
/// heartbeat of a microVM among them, which says only that its microVM ran, as the party's
/// own look at that microVM's process reads (D209: no rate a run answered at bounds what
/// counts as progress). Do: say a heartbeat; then a microVM's start said slowly; then a run's
/// own line, in two parts. Expect: each whole line said, half a line not.
#[test]
fn a_daemons_whole_lines_are_its_words() {
    let home = TempDir::new("said");
    say(&home, "shards daemon 70625: serving it\n");
    let mut said = Said::of(&home);
    assert!(!said.said(), "what it said before it was watched taken for new");
    say(&home, HEARTBEAT);
    assert!(said.said(), "a heartbeat not taken for its words");
    say(&home, STARTING);
    assert!(said.said(), "a start said slowly not taken for its words");
    say(&home, "shards daemon 70625: container 212eb2365f93: its command ");
    assert!(!said.said(), "half a line taken for its words");
    say(&home, "ended (status 7)\n");
    assert!(said.said(), "a run's own line not taken for its words");
    assert!(!said.said(), "a line taken twice");
}

/// The stand-in a test's wait follows here: this test binary run again for this test alone,
/// doing what `SHARDS_STAND_IN` says: `exit`, at once; `busy MS`, its CPU busy for MS
/// milliseconds, then its end; `answer PORT MS`, connected to loopback's PORT, busy for MS
/// milliseconds, then its answer written there and its end; `idle`, nothing at all until
/// its stdin closes.
#[test]
#[ignore = "a process of its own for the harness's stand-ins, which run it"]
fn stand_in() {
    use std::io::Read as _;
    let asked = std::env::var("SHARDS_STAND_IN").unwrap_or_default();
    let busy = |ms| {
        let until = Instant::now() + Duration::from_millis(ms);
        let mut spun = 0u64;
        while Instant::now() < until {
            spun = std::hint::black_box(spun.wrapping_add(1));
        }
    };
    let answer = asked
        .strip_prefix("answer ")
        .and_then(|a| a.split_once(' '))
        .and_then(|(port, ms)| Some((port.parse::<u16>().ok()?, ms.parse().ok()?)));
    if let Some(ms) = asked.strip_prefix("busy ").and_then(|ms| ms.parse().ok()) {
        busy(ms);
    } else if let Some((port, ms)) = answer {
        let mut to = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        busy(ms);
        to.write_all(ANSWER).unwrap();
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

/// A test's wait on a state its home's daemon comes to (`Patience`) goes on while its party
/// works, as a command's wait does (the person's ruling of 2026-11-13, D209): its caller
/// gives it no bound. daemon.rs' and containers.rs' waits on pools and runs were cut at
/// their tests' `TIMEOUT` however their daemon worked, as run_command's were before W6's
/// slice 2: red so at 3e9723c64, cut 0.1 s into a second's work at a bound of 100 ms. Do: a
/// stand-in busy for a second, named its home's daemon; wait for its end, twice, from one
/// call site. Expect: each wait holds until it ends, and its end, told `done`, teaches the
/// call site's kind its answer.
#[test]
fn a_patient_wait_goes_on_while_its_party_works() {
    let home = TempDir::new("patient");
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        let mut daemon = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(home.join("daemon.pid"), format!("{}\n", daemon.id())).unwrap();
        let began = Instant::now();
        let mut patience = Patience::of(&home);
        while daemon.try_wait().unwrap().is_none() {
            if !patience.holds() {
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!(
                    "a daemon busy for {busy:?} did not end within its wait: cut {:?} in, round {round}",
                    began.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
        // The second round's kind is the first's, which its end taught.
        assert!(
            round == 0
                || matches!(patience.seen(), shards_api::Seen::Learned { slowest, .. } if slowest >= busy),
            "its kind learned no answer of a second's work: {:?}",
            patience.seen()
        );
        patience.done();
    }
}

/// A test's wait on a guest (`Heard`: every `Vm` wait, `within`, run.rs' and restore.rs'
/// waits on their VM process) goes on while its party works, as a command's wait does (the
/// person's ruling of 2026-11-13, D209): its caller gives it no bound, and past its cold start
/// it is judged each W* of its call site's kind. Red at 70eba153d, as
/// `red_a_guest_wait_goes_on_while_its_party_works` there: `Heard::running` on a stand-in
/// busy for a second, past a caller's bound of 100 ms, cut 0.1 s in, as `Ran` judged a VM
/// process only a `PROGRESS_SLICE` after it first found it, and its stand-in no VM process
/// at all. Do: a stand-in busy for a second; wait for its end, twice, from one call site.
/// Expect: each wait holds until it ends, and the first's end teaches the second's kind.
#[test]
fn a_guest_wait_goes_on_while_its_party_works() {
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        let mut party = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let began = Instant::now();
        let mut heard = common::Heard::running(party.id());
        while party.try_wait().unwrap().is_none() {
            if let Err(refused) = heard.look(true) {
                let _ = party.kill();
                let _ = party.wait();
                panic!(
                    "a party busy for {busy:?} cut {:?} into its wait, round {round}: {refused}",
                    began.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        heard.answered();
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
    }
}

/// [`common::Heard::streaming`] at one call site: one kind for every wait made through it.
fn streaming(vm: u32) -> common::Heard<'static> {
    common::Heard::streaming(vm, None)
}

/// A guest of more vCPUs than its host has CPUs runs them for ever, its workload stopped or
/// not: past its first line its CPU is its liveness alone (`Cpu::Liveness`), and a wait on its
/// words is refused at its judge's look once a whole W* passed with none, however its VM
/// process spins (the parity plan's step 10: a 64-vCPU storm waited on 2 h 16 min). Its
/// host's stop of it is still its host's hold (ROOT, 2026-11-13). Do: teach the kind its W*
/// by a stand-in's own end; follow a stand-in that spins and says nothing; then one stopped
/// (SIGSTOP; on Windows made with its thread suspended) through two of its judge's looks.
/// Expect: the spinning one refused while it spins; the stopped one not refused.
#[test]
fn a_streaming_guests_cpu_is_its_liveness_and_its_stop_its_hosts_hold() {
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = streaming(quick.id());
    while quick.try_wait().unwrap().is_none() {
        taught.look(true).unwrap();
    }
    taught.answered();
    let mut spinning = stand_in_doing("busy 600000")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut heard = streaming(spinning.id());
    let refused = loop {
        assert!(
            spinning.try_wait().unwrap().is_none(),
            "a spinning guest that said nothing was never refused"
        );
        if heard.look(true).is_err() {
            break true;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = spinning.kill();
    let _ = spinning.wait();
    assert!(refused);
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
    let shards_api::Seen::Learned { slowest, .. } = taught.seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let mut heard = streaming(stopped.id());
    let began = Instant::now();
    // Its judge's first two looks: W* past its start, and W* past that.
    while began.elapsed() <= slowest.saturating_mul(2) {
        let judged = heard.look(true);
        assert!(judged.is_ok(), "{judged:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = stopped.kill();
    let _ = stopped.wait();
}

/// A call that blocks until its answer or its run's end (`common::judged`: shards-api's calls
/// given no bound, `Duration::MAX`, in api_image.rs, member_exec.rs and nesting.rs, and their
/// reads of a run's channels) goes on while what this test's process started works, as a
/// command's wait does (the person's ruling of 2026-11-13, D209): its caller gives it no
/// bound, and past its cold start it is judged each W* of its call site's kind. Red at 95b653fb2 as
/// `red_a_judged_call_goes_on_while_its_party_works`: api_image.rs' `read_within`, a channel
/// read at its test's `TIMEOUT`, that bound scaled to 100 ms, cut a call on a stand-in busy
/// for a second 0.1 s in. Do: a stand-in busy for a second, its end waited for through
/// `judged`, twice from one call site. Expect: each wait holds until it ends, and the first's
/// end teaches the second's kind.
#[test]
fn a_judged_call_goes_on_while_its_party_works() {
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        let mut party = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let began = Instant::now();
        let ended = judged_end(&mut party);
        assert!(
            ended.is_ok(),
            "a party busy for {busy:?} cut {:?} into its wait, round {round}: {ended:?}",
            began.elapsed()
        );
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
    }
}

/// `party`'s end through [`common::judged`] at one call site: one kind for every wait made
/// through it. Nothing ends a busy stand-in early: refused, it is waited for to its end.
fn judged_end(party: &mut std::process::Child) -> Result<std::process::ExitStatus, String> {
    common::judged(
        common::Party::started(None),
        "a busy stand-in",
        || {},
        || party.wait().unwrap(),
    )
}

/// What an `answer` stand-in writes.
const ANSWER: &[u8] = b"answered";

/// The old shape of a read of a stream its party answers on, at its test's `TIMEOUT`
/// (warm.rs' daemon link and its client's connection, daemon.rs' engine stream,
/// containers.rs' reads of published ports): a caller's bound on each read, here scaled to
/// 100 ms, against a party that works for a second before it answers. Do: a stand-in
/// connected to this test, busy for a second, then its answer; read it at that bound.
/// Expect: its answer.
#[test]
fn red_a_judged_read_goes_on_while_its_party_works() {
    use std::io::Read as _;
    let busy = Duration::from_secs(1);
    let bound = Duration::from_millis(100);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut party = stand_in_doing(&format!("answer {port} {}", busy.as_millis()))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    let began = Instant::now();
    conn.set_read_timeout(Some(bound)).unwrap();
    let mut got = [0u8; ANSWER.len()];
    let read = conn.read_exact(&mut got);
    let _ = party.kill();
    let _ = party.wait();
    assert!(
        read.is_ok(),
        "a party busy for {busy:?} cut {:?} into its read: {read:?}",
        began.elapsed()
    );
}
