//! The test harness's own judgments (`common`), held to stand-ins and to a daemon that runs no
//! microVM: no microVM.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{Patience, Said, TempDir};

/// A daemon's heartbeat, as it says it while a microVM a client waits on runs, its looks
/// paced by its record of starts (daemon.rs `log_progress`, D209): the nesting probe's VM on HVF at fafe35efc, whose
/// test waited on it to its own bound and past.
const HEARTBEAT: &str = "shards daemon 70625: container 212eb2365f9391311256a9c21cb9db1788808fd36abb04c7d64ddae90d121858: VM 70650 runs as its client waits: its CPU time 304 ms (+110 ms in 10s), 23390 page faults (+1342)\n";

/// A microVM's start said slowly (daemon.rs `ready`), as the daemon said it at each look
/// before D215's judge (it says once now, as a start first outlasts the slowest seen).
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
/// its stdin closes; `pulse N MS`, N rounds of a moment's CPU then MS milliseconds of none,
/// saying nothing, as a party that works between waits on a peer outside it; `falls MS`, a
/// give-back ([`falls`]).
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
    } else if let Some((rounds, ms)) = asked
        .strip_prefix("pulse ")
        .and_then(|a| a.split_once(' '))
        .and_then(|(n, ms)| Some((n.parse::<u32>().ok()?, ms.parse().ok()?)))
    {
        for _ in 0..rounds {
            busy(1);
            std::thread::sleep(Duration::from_millis(ms));
        }
    } else if asked == "idle" {
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    } else if let Some(ms) = asked.strip_prefix("falls ").and_then(|ms| ms.parse().ok()) {
        falls(ms, busy);
    }
}

/// The memory a give-back stand-in frees, in regions of this many bytes, this many.
const REGION: usize = 32 << 20;
const REGIONS: usize = 4;

/// A stand-in guest's give-back: [`REGIONS`] regions of [`REGION`] bytes mapped and touched,
/// "mapped" said on stdout; once stdin gives a byte, each unmapped in turn, `ms` of busy CPU
/// apart, the first after twice that (its free, then its guest's wait before it reports, as
/// D151's reports come); then nothing until stdin closes.
fn falls(ms: u64, busy: impl Fn(u64)) {
    use std::io::Read as _;
    let regions: Vec<Region> = (0..REGIONS).map(|_| Region::touched()).collect();
    let mut out = std::io::stdout();
    out.write_all(b"mapped\n").unwrap();
    out.flush().unwrap();
    let mut stdin = std::io::stdin();
    let _ = stdin.read(&mut [0u8; 1]);
    busy(ms);
    for region in regions {
        busy(ms);
        drop(region);
    }
    let _ = stdin.read(&mut [0u8; 1]);
}

/// [`REGION`] bytes of memory of the process's own, mapped and given back to its host
/// directly, not through an allocator that may keep them.
struct Region(*mut u8);

impl Region {
    fn touched() -> Region {
        #[cfg(unix)]
        // SAFETY: an anonymous private mapping of REGION bytes, which nothing else maps.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        #[cfg(unix)]
        assert_ne!(at, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
        #[cfg(windows)]
        // SAFETY: REGION bytes reserved and committed, readable and writable, at an address
        // Windows chooses.
        let at = unsafe {
            use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
            VirtualAlloc(std::ptr::null(), REGION, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        };
        assert!(!at.is_null(), "{}", std::io::Error::last_os_error());
        let at = at.cast::<u8>();
        for page in (0..REGION).step_by(4096) {
            // SAFETY: within the REGION bytes just mapped, writable.
            unsafe { at.add(page).write_volatile(1) };
        }
        Region(at)
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the mapping `touched` made, unmapped once, as its Region drops.
        unsafe {
            libc::munmap(self.0.cast(), REGION);
        }
        #[cfg(windows)]
        // SAFETY: the allocation `touched` made, released whole once, as its Region drops.
        unsafe {
            windows_sys::Win32::System::Memory::VirtualFree(
                self.0.cast(),
                0,
                windows_sys::Win32::System::Memory::MEM_RELEASE,
            );
        }
    }
}

/// [`stand_in`], doing `what`: `what`'s first word its first argument, a filter that names
/// no test, so that it is its command's word ([`common::command_words`]) as a subcommand is.
fn stand_in_doing(what: &str) -> std::process::Command {
    stand_in_with(what, &[])
}

/// [`stand_in_doing`], `more` after its options' end (`--`): filters that name no test, so
/// that its command line carries what a test's command carries (a path, a port).
fn stand_in_with(what: &str, more: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    let word = what.split_whitespace().next().unwrap_or("exit");
    cmd.args([word, "stand_in", "--exact", "--ignored", "--test-threads=1", "--"])
        .args(more)
        .env("SHARDS_STAND_IN", what);
    cmd
}

/// A command's words, which key its wait's kind beside its call site, are its program and
/// its subcommand, never an image, a name, a path or a flag (#77). Do: take the words of
/// commands as tests run them. Expect: each its subcommand's words alone.
#[test]
fn a_commands_words_are_its_subcommands() {
    let words = |program: &str, args: &[&str]| {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        common::command_words(&cmd)
    };
    let shards = if cfg!(windows) {
        "C:\\t\\shards.exe"
    } else {
        "/t/shards"
    };
    for (args, said) in [
        ("run --rm alpine echo", "shards run"),
        ("run alpine echo hi", "shards run"),
        ("push localhost:5000/x", "shards push"),
        ("build -t x .", "shards build"),
        ("stop daemon", "shards stop daemon"),
        ("stop web", "shards stop"),
        ("run vm --kernel k", "shards run vm"),
        ("image ls", "shards image ls"),
        ("compose up -d", "shards compose up"),
        ("buildx build .", "shards buildx build"),
        ("--version", "shards"),
        ("rm -f t-1234", "shards rm"),
        ("inspect t-1234", "shards inspect"),
    ] {
        let args: Vec<&str> = args.split_whitespace().collect();
        assert_eq!(words(shards, &args), said, "{args:?}");
    }
    assert_eq!(words("git", &["clone", "https://x/y"]), "git clone");
    assert_eq!(words("sh", &["-c", "true"]), "sh");
}

/// One call site's waits on different commands are judged apart: a quick command's answer
/// never bounds a slow one's (#77, the CI triage at d24ef4cfd: build.rs' `policy_eval_answers`
/// and `builds_push_what_they_build` ran many commands from one closure, and a W* of
/// 13.4 ms and 3.49 ms, learned from quick refused commands, judged a real build and a push,
/// whose parties wait on peers outside them between their work). Do: from one call site run
/// a stand-in that exits at once, which teaches its kind W*; then one that works a moment
/// three times, each followed by 200 ms of nothing, a wait on a peer. Expect: the second
/// waited on to its end, status 0, its quiet past the first's W* no stall of its own kind's.
#[test]
fn one_call_sites_commands_are_judged_apart() {
    for what in ["exit", "pulse 3 200"] {
        let asked = ["stand-in".to_string(), what.to_string()];
        let run = common::run_command(stand_in_doing(what), &asked);
        assert_eq!(run.status, Some(0), "{run}");
    }
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
        // Begun before its spawn: the stand-in's busy clock starts as it runs, which can
        // be before spawn returns here (CI run 43530570785: "it ended 998.689681ms in").
        let began = Instant::now();
        let mut daemon = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(home.join("daemon.pid"), format!("{}\n", daemon.id())).unwrap();
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
        // Begun before its spawn: the stand-in's busy clock starts as it runs, which can
        // be before spawn returns here (CI run 43530570785: "it ended 998.689681ms in").
        let began = Instant::now();
        let mut party = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
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
        // Begun before its spawn: the stand-in's busy clock starts as it runs, which can
        // be before spawn returns here (CI run 43530570785: "it ended 998.689681ms in").
        let began = Instant::now();
        let mut party = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
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

/// A read of a stream its party answers on (warm.rs' daemon link and its client's
/// connection, through `common::judged`) goes on while its party works, as a command's wait
/// does (the person's ruling of 2026-11-13, D209): its caller gives it no bound, and past its
/// cold start it is judged each W* of its call site's kind. Red at 68770070f as
/// `red_a_judged_read_goes_on_while_its_party_works`: the old shape, a read timeout at its
/// test's `TIMEOUT` scaled to 100 ms, cut a stand-in that answers after a second's work.
/// Do: a stand-in connected to this test, busy for a second, then its answer; read it
/// through `judged`, twice from one call site. Expect: each read holds until the answer, and
/// the first's teaches the second's kind.
#[test]
fn a_judged_read_goes_on_while_its_party_works() {
    let busy = Duration::from_secs(1);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = crate::common::minted_port(listener.local_addr().unwrap().port());
    for round in 0..2 {
        let mut party = stand_in_doing(&format!("answer {port} {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (mut conn, _) = listener.accept().unwrap();
        let began = Instant::now();
        let mut got = [0u8; ANSWER.len()];
        let read = judged_read(&mut party, &mut conn, &mut got);
        let _ = party.wait();
        assert!(
            matches!(read, Ok(Ok(()))),
            "a party busy for {busy:?} cut {:?} into its read, round {round}: {read:?}",
            began.elapsed()
        );
        assert_eq!(&got, ANSWER);
        assert!(began.elapsed() >= busy, "it answered {:?} in", began.elapsed());
    }
}

/// `party`'s answer on `conn` through [`common::judged`] at one call site: one kind for every
/// read made through it. Refused, `party` is ended, which ends the read.
fn judged_read(
    party: &mut std::process::Child,
    conn: &mut std::net::TcpStream,
    got: &mut [u8],
) -> Result<std::io::Result<()>, String> {
    use std::io::Read as _;
    common::judged(
        common::Party::started(None),
        "a stand-in's answer",
        || {
            let _ = party.kill();
        },
        || conn.read_exact(got),
    )
}

/// A retry while what a test started comes up (`Patience::started`: slates_fs.rs' `slates`,
/// asked again while slates' anchor has no daemon yet; build.rs' waits on the git daemon,
/// sshd and ssh-agent it started) goes on while what this test's process started works, as a
/// command's wait does (the person's ruling of 2026-11-13, D209): its caller gives it no
/// bound, and past its cold start it is judged each W* of its call site's kind. Red at
/// f389c8b75 as `red_a_retry_goes_on_while_what_this_test_started_works`: slates_fs.rs'
/// retry, its test's `START` scaled to 100 ms, gave up on a stand-in busy for a second 0.1 s
/// in. Do: a stand-in busy for a second, asked after until it has ended, twice from one call
/// site. Expect: each wait holds until it ends, and the first's end teaches the second's kind.
#[test]
fn a_retry_goes_on_while_what_this_test_started_works() {
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        // Begun before its spawn: the stand-in's busy clock starts as it runs, which can
        // be before spawn returns here (CI run 43530570785: "it ended 998.689681ms in").
        let began = Instant::now();
        let mut anchor = stand_in_doing(&format!("busy {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut patience = Patience::started();
        while anchor.try_wait().unwrap().is_none() {
            if !patience.holds() {
                let _ = anchor.kill();
                let _ = anchor.wait();
                panic!(
                    "an anchor busy for {busy:?} given up {:?} into its retries, round {round}",
                    began.elapsed()
                );
            }
            std::thread::sleep(patience.next_look());
        }
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
        assert!(
            round == 0
                || matches!(patience.seen(), shards_api::Seen::Learned { slowest, .. } if slowest >= busy),
            "its kind learned no answer of a second's work: {:?}",
            patience.seen()
        );
        patience.done();
    }
}

/// A give-back is followed while its process's own memory falls, its CPU no progress, to its
/// end, and its kind learns how long it took (ROOT's ruling, D209 Part 4: containers.rs'
/// `what_a_microvm_holds_cold_is_given_back_at_its_hosts_word` waited for two samples 500 ms
/// apart within 4 MiB). Do: a stand-in that frees four regions one at a time, 200 ms of busy
/// CPU apart, the first 400 ms in, followed from before it frees; twice from one call site.
/// Expect: each round's end held what was left once all four went, and the second round's
/// kind learned a give-back that long.
#[test]
fn a_give_back_is_followed_while_its_memory_falls() {
    let ms = 200;
    for round in 0..2 {
        let mut stand_in = stand_in_doing(&format!("falls {ms}"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stand_in.stdout.take().unwrap()));
        // Its line follows libtest's "test stand_in ... ", which ends with no newline.
        assert!(
            lines.any(|l| l.unwrap().ends_with("mapped")),
            "the stand-in mapped nothing"
        );
        let peak = shards_vmm::platform::process_usage(stand_in.id())
            .unwrap()
            .footprint;
        let falling = common::Falling::begin(stand_in.id());
        assert!(
            round == 0
                || matches!(falling.seen(), shards_api::Seen::Learned { slowest, .. }
                    if slowest >= Duration::from_millis(ms * (REGIONS as u64 + 1))),
            "its kind learned no whole give-back: {:?}",
            falling.seen()
        );
        stand_in.stdin.as_mut().unwrap().write_all(b"g").unwrap();
        let ended = falling.settled();
        let _ = stand_in.kill();
        let _ = stand_in.wait();
        // Midway between three regions given back and four: all four went before its end.
        let region = REGION as u64;
        assert!(
            peak.saturating_sub(ended) >= 3 * region + region / 2,
            "round {round}: {peak} bytes held mapped, {ended} at its give-back's end"
        );
    }
}

/// [`Patience::of`] at one call site: one kind for every wait made through it.
fn polled(home: &std::path::Path) -> Patience {
    Patience::of(home)
}

/// Stops its home's daemon as it drops, the test's end or its failure.
struct Served<'a>(&'a [(&'a str, &'a std::ffi::OsStr)]);

impl Drop for Served<'_> {
    fn drop(&mut self) {
        let _ = common::run_shards_env(&["stop", "daemon"], &[] as &[&str], self.0);
    }
}

/// A wait's progress is its party's own, never what the waiter itself caused: a `Patience`
/// whose test polls its state with a `shards` command (saved.rs' and moved.rs' `until`,
/// build.rs' `ps` polls, containers.rs' `inspect` and `logs` polls) does not count the CPU its
/// home's daemon spends serving those polls as the daemon's progress (ROOT's ruling,
/// 2026-11-13, D209). Else a wait on a state that never comes, polled while nothing else
/// works, is never refused, and its job runs to CI's limit. Do: start a daemon that runs no
/// microVM (`shards ps`, its store none); teach a wait's kind its W* by one poll that finds
/// its state; then poll `ps -a -q` for a run that never comes, through the same kind's
/// wait, past its judge's first two looks (2 W* past its start). Expect: refused there.
#[test]
fn a_wait_is_refused_though_its_own_polls_keep_its_daemon_busy() {
    let home = TempDir::new("polled");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_LOCAL_STORE", std::ffi::OsStr::new("none")),
    ];
    let ps = || {
        let run = common::run_shards_env(&["ps"], &["-a", "-q"], &env);
        assert_eq!(run.status, Some(0), "{run}");
        run.stdout
    };
    // Started as daemon.rs' `start_daemon` starts one, its start no wait of this test's.
    let started = common::command()
        .arg("ps")
        .envs(env)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let _served = Served(&env);
    // Each wait looks once before its first poll: a look finds its daemon anew, and only the
    // looks after judge it (D215), so each poll's work falls between looks that judge it.
    let mut taught = polled(&home);
    let _ = taught.holds();
    ps();
    taught.done();
    let shards_api::Seen::Learned { slowest, .. } = polled(&home).seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let mut patience = polled(&home);
    let began = Instant::now();
    assert!(patience.holds(), "refused at its start");
    let refused = loop {
        assert_eq!(ps().trim(), "", "a run came in a home that runs none");
        let at = began.elapsed();
        if !patience.holds() {
            break true;
        }
        // Its judge's looks are due each W* past its start: a look made at 2 W* or later is
        // past its first two, and the next says a refusal there.
        if at >= slowest.saturating_mul(2) {
            break !patience.holds();
        }
    };
    assert!(
        refused,
        "a wait polled for a state that never comes, its daemon busy with its polls alone, \
         not refused {:?} in (W* {slowest:?}): {}\n--- its daemon's log\n{}",
        began.elapsed(),
        patience.account(),
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
}

/// A command's wait is keyed by its call site and its whole command line, what a run mints
/// of its own made generic (ROOT's ruling (a), 2026-11-13, #77): one call site's quick
/// refused command is kept apart from its real one, as build.rs' `builds_push_what_they_build`
/// runs `build --push CTX`, refused for want of a tag, and then `build --push -t
/// localhost:PORT/team/built:1 CTX`, which `shards build` and `buildx policy` words alone
/// never told apart; and a run's own directories and ports key no new record each run.
/// Do: from one call site, run a stand-in carrying the refused command's line and then the
/// real one's, each with a context directory and a registry port of its own; then both
/// again, with new ones. Expect: two records of the call site's on this host, the second
/// round's keyed as the first's.
#[test]
fn a_call_sites_commands_are_keyed_by_their_whole_line() {
    #[track_caller]
    fn run_at(cmd: std::process::Command) -> &'static std::panic::Location<'static> {
        let run = common::run_command(cmd, &["stand-in".to_string()]);
        assert_eq!(run.status, Some(0), "{run}");
        std::panic::Location::caller()
    }
    let mut site = None;
    for _ in 0..2 {
        let ctx = TempDir::new("build-push-ctx");
        let (port, _) = common::writable_registry();
        let ctx = ctx.to_str().unwrap();
        let name = format!("localhost:{port}/team/built:1");
        for line in [
            vec!["build", "--push", ctx],
            vec!["build", "--push", "-t", &name, ctx],
        ] {
            site = Some(run_at(stand_in_with("exit", &line)));
        }
    }
    let site = site.unwrap();
    let prefix: String = format!("{}-{}-{}-", site.file(), site.line(), site.column())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let records: Vec<String> = std::fs::read_dir(common::workspace().join("target/judge-records"))
        .unwrap()
        .flatten()
        .flat_map(|host| std::fs::read_dir(host.path()).into_iter().flatten().flatten())
        .filter_map(|record| record.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    assert_eq!(records.len(), 2, "{records:#?}");
}

/// The two call sites the CI triage at d24ef4cfd found judged by a quick refused command's
/// W* (#77) now key that command apart from their real one, and key each alike run after
/// run. Do: make build.rs' `builds_push_what_they_build` lines (its context a `TempDir`, its
/// registry `writable_registry`'s) twice over, and `policy_eval_answers`' refused and
/// resolved lines. Expect: the refused and the real line apart; the second round's lines
/// the first's.
#[test]
fn the_triaged_call_sites_key_their_refused_command_apart() {
    let shards = |args: &[&str]| {
        let mut cmd = std::process::Command::new("/t/shards");
        cmd.args(args);
        common::keyed_line(&cmd)
    };
    let mut rounds = Vec::new();
    for _ in 0..2 {
        let ctx = TempDir::new("build-push-ctx");
        let (port, _) = common::writable_registry();
        let ctx = ctx.to_str().unwrap();
        let name = format!("localhost:{port}/team/built:1");
        let refused = shards(&["build", "--push", ctx]);
        let built = shards(&["build", "--push", "-t", &name, ctx]);
        assert_ne!(refused, built);
        rounds.push((refused, built));
    }
    assert_eq!(rounds[0], rounds[1], "a run's own directory or port in its key");
    let eval = |source: &str, platform: &str| {
        shards(&[
            "buildx",
            "policy",
            "eval",
            "--print",
            "--platform",
            platform,
            source,
        ])
    };
    assert_ne!(
        eval("docker-image://alpine:3.20", "bogus/x/y/z"),
        eval("docker-image://alpine:3.20", "linux/arm64")
    );
    assert_ne!(
        shards(&["buildx", "policy", "eval", "--print", "./nope"]),
        shards(&["buildx", "policy", "eval", "docker-image://alpine:3.20"])
    );
}
