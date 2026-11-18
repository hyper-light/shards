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

/// Names process `pid` `home`'s daemon, as a daemon names itself (`shards_ipc::Named`,
/// D185): its ID and its start.
fn name_daemon(home: &std::path::Path, pid: u32) {
    let start = shards_ipc::started(pid).unwrap();
    std::fs::write(home.join("daemon.pid"), format!("{pid} {start}\n")).unwrap();
}

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
/// milliseconds, then its end; `go MS`, the same once a byte (its go) or its end comes on
/// its stdin; `spins MS`, the same once it has said "spinning" on its stdout; `refuse`, its end at once, status 1; `answer PORT MS`, connected to loopback's
/// PORT, busy for MS
/// milliseconds, then its answer written there and its end; `fetch PORT`, a request
/// written to loopback's PORT, its answer read to the connection's end; `idle`, nothing at all until
/// its stdin closes; `rests`, the same once it has said "resting" on its stdout, its start
/// over; `pulse N MS`, N rounds of a moment's CPU then MS milliseconds of none,
/// saying nothing, as a party that works between waits on a peer outside it; `falls MS`, a
/// give-back ([`falls`]); `leaves MS`, a stand-in `pulse 1 MS` started on its stdout and
/// stderr, "holder PID" said there, and its own end, the holder left holding them; `stages`
/// and `spinner`, a machine's waits in a process of their own ([`stages`], [`spinner`]);
/// `serving`, a wait on a server thread of its own process ([`serving`]); `own-vms`, waits
/// on VM processes of its own ([`own_vms`]); `starved`, two threads spinning on one CPU until
/// its stdin closes, one of them kept from it, "kept" said on its stdout once it is
/// ([`starved`]).
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
    } else if let Some(ms) = asked.strip_prefix("go ").and_then(|ms| ms.parse().ok()) {
        let _ = std::io::stdin().read(&mut [0u8; 1]);
        busy(ms);
    } else if let Some(ms) = asked.strip_prefix("spins ").and_then(|ms| ms.parse().ok()) {
        let mut out = std::io::stdout();
        out.write_all(b"spinning\n").unwrap();
        out.flush().unwrap();
        busy(ms);
    } else if asked == "refuse" {
        std::process::exit(1);
    } else if let Some((port, ms)) = answer {
        let mut to = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        busy(ms);
        to.write_all(ANSWER).unwrap();
    } else if let Some(port) = asked.strip_prefix("fetch ").and_then(|p| p.parse::<u16>().ok()) {
        let mut from = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        from.write_all(b"GET\n").unwrap();
        let mut got = Vec::new();
        from.read_to_end(&mut got).unwrap();
        assert_eq!(got, ANSWER);
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
    } else if asked == "rests" {
        let mut out = std::io::stdout();
        out.write_all(b"resting\n").unwrap();
        out.flush().unwrap();
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    } else if let Some(ms) = asked.strip_prefix("falls ").and_then(|ms| ms.parse().ok()) {
        falls(ms, busy);
    } else if asked == "lists" {
        lists();
    } else if asked == "stages" {
        stages();
    } else if asked == "spinner" {
        spinner();
    } else if asked == "serving" {
        serving();
    } else if asked == "own-vms" {
        own_vms();
    } else if asked == "vm-home" {
        vm_home();
    } else if asked == "starved" {
        starved();
    } else if let Some(bytes) = asked.strip_prefix("durable ").and_then(|b| b.parse::<u64>().ok()) {
        durable(bytes);
    } else if let Some(ms) = asked.strip_prefix("leaves ") {
        #[allow(
            clippy::zombie_processes,
            reason = "left holding this stand-in's streams past its end, as its test asks, which ends it"
        )]
        let holder = stand_in_doing(&format!("pulse 1 {ms}"))
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut out = std::io::stdout();
        out.write_all(format!("holder {}\n", holder.id()).as_bytes())
            .unwrap();
        out.flush().unwrap();
    }
}

/// A stand-in's durable write of `bytes` (D286): declared in the home `SHARDS_HOME` names, as
/// every process of a home declares there; written, said "written" on its stdout, then made
/// durable at once (`shards_durable::data`) as a byte (its go) or its end comes on its stdin,
/// and said "durable".
fn durable(bytes: u64) {
    use std::io::Read as _;
    let home = std::path::PathBuf::from(std::env::var_os("SHARDS_HOME").unwrap());
    shards_durable::install(&home).unwrap();
    let mut file = std::fs::File::create(home.join("durable-write")).unwrap();
    let chunk = vec![0x5a_u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        let n = left.min(chunk.len() as u64);
        file.write_all(&chunk[..n as usize]).unwrap();
        left -= n;
    }
    let mut out = std::io::stdout();
    out.write_all(b"written\n").unwrap();
    out.flush().unwrap();
    let _ = std::io::stdin().read(&mut [0u8; 1]);
    shards_durable::data(&file).unwrap();
    out.write_all(b"durable\n").unwrap();
    out.flush().unwrap();
}

/// A party in one long durable write is its host's while its write's volume works (D286,
/// D215): a stand-in that wrote a GiB makes it durable at once, under a judged wait whose kind
/// learned, from a write of nothing, a quick answer. Its flush waits in its kernel, on Windows
/// as `Executive` (PM M606), as a read of a pipe no one writes does, so that no thread state
/// tells it: its declaration does, its volume's own counters the evidence. Before, Windows
/// refused such a wait past W* (CI 44282442232, windows-2025: context's and snapshot's
/// flushes, 0.19 to 1.82 s past W*s of 65 to 220 ms); Linux and macOS held it by their
/// threads' `D` and uninterruptible states and their summed disks. Each round from one call
/// site, the closure's, so that the second is judged by what the first taught.
#[test]
fn a_party_in_a_long_durable_write_is_held_while_its_volume_works() {
    use std::io::BufRead as _;
    #[track_caller]
    fn round(home: &std::path::Path, bytes: u64, forget: bool) -> Result<(), String> {
        if forget {
            let site = std::panic::Location::caller();
            let _ = std::fs::remove_file(common::WaitKind::kept_at(site, "", ""));
        }
        let mut writer = stand_in_doing(&format!("durable {bytes}"))
            .env("SHARDS_HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(writer.stdout.take().unwrap());
        let mut said = String::new();
        // Written first, outside the wait: its writing is its CPU.
        while !said.contains("written") {
            said.clear();
            assert_ne!(
                out.read_line(&mut said).unwrap(),
                0,
                "the stand-in ended unwritten"
            );
        }
        let mut go = writer.stdin.take().unwrap();
        let party = common::Party::client(writer.id(), Some(home));
        let judged = common::judged(
            party,
            "a durable write",
            || {},
            move || {
                go.write_all(b"g").unwrap();
                let mut said = String::new();
                while !said.contains("durable") {
                    said.clear();
                    assert_ne!(
                        out.read_line(&mut said).unwrap(),
                        0,
                        "the stand-in ended undurable"
                    );
                }
            },
        );
        writer.wait().unwrap();
        judged
    }
    let home = TempDir::new("durable-held");
    let round = |bytes: u64, forget: bool| round(&home, bytes, forget);
    round(0, true).unwrap();
    round(1 << 30, false).unwrap();
}

/// A stand-in's listing of every process ([`common::processes`], what each look at a wait's
/// party reads), and what processes it started cost meanwhile, as the children it reaped
/// count it (getrusage(2) RUSAGE_CHILDREN: their page faults and CPU): said on stdout as
/// `listed LISTED FAULTS CPU_US`.
#[cfg(unix)]
fn lists() {
    let reaped = || {
        // SAFETY: rusage is plain C data, for which all zeroes is a value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage(2) into a local.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut usage) };
        assert_eq!(rc, 0);
        // time_t and suseconds_t are i64 on Linux, suseconds_t i32 on macOS.
        #[allow(clippy::useless_conversion)]
        let us = |t: libc::timeval| i64::from(t.tv_sec) * 1_000_000 + i64::from(t.tv_usec);
        (
            usage.ru_minflt + usage.ru_majflt,
            us(usage.ru_utime) + us(usage.ru_stime),
        )
    };
    let before = reaped();
    let listed = common::processes();
    let after = reaped();
    let mut out = std::io::stdout();
    out.write_all(
        format!(
            "listed {} {} {}\n",
            listed.len(),
            after.0 - before.0,
            after.1 - before.1
        )
        .as_bytes(),
    )
    .unwrap();
}

#[cfg(not(unix))]
fn lists() {}

/// A look at a wait's party lists the system's processes without starting one (D209): red at
/// 5bec7398b, where each look spawned `ps`, 16 to 25 ms of it on adacachy2 (1,200 processes)
/// against a learned W* of 8 ms, which crowded `Following`'s reads out. Do: in a stand-in of
/// its own, alone, list the processes and count what the children it reaped meanwhile cost.
/// Expect: this host's processes listed, the stand-in among them, and no child's cost.
#[cfg(unix)]
#[test]
fn a_party_look_lists_processes_without_starting_one() {
    let out = stand_in_doing("lists").output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    let counts: Vec<i64> = said
        .lines()
        .find_map(|line| line.split("listed ").nth(1))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let [listed, faults, cpu] = counts[..] else {
        panic!("the stand-in said {said:?}");
    };
    assert!(listed > 1, "{listed} processes listed");
    assert_eq!(
        (faults, cpu),
        (0, 0),
        "a listing started a process: its faults and CPU (µs)"
    );
}

/// A refusal's account changes nothing of what it describes (ROOT, 2026-11-18): it asks no
/// home that no daemon serves, so it starts no daemon there, nor through one a microVM whose
/// policy restarts it. CI 44227612442's x86_64 musl accounts of two commands refused in homes
/// with no daemon each started one there (daemons 21259 and 22143). Red:
/// lanes/redgreen/own-38f-account.patch, every home asked. Do: take a refusal's account
/// ([`common::stacks_of`]) and its guests' processes ([`common::guests_processes`]) of an empty
/// home, then of one whose pid file names a process that is not the one it names (this one,
/// started at another time). Expect: each says no daemon runs there and none was asked, and the
/// home keeps what it held: no daemon named in it, nothing made.
#[test]
fn an_account_asks_no_home_no_daemon_serves() {
    let home = TempDir::new("unasked");
    let none = format!("no daemon running in {}: none asked", home.display());
    let held = || {
        let mut names: Vec<_> = std::fs::read_dir(&*home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let stale = format!("{} {}\n", std::process::id(), u64::MAX);
    for record in [None, Some(stale.as_str())] {
        if let Some(record) = record {
            std::fs::write(home.join(shards_ipc::PID), record).unwrap();
        }
        let before = held();
        let account = common::stacks_of(&[], Some(&*home));
        let guests = common::guests_processes(&home);
        assert!(account.contains(&none), "{account}");
        assert!(guests.contains(&none), "{guests}");
        assert_eq!(held(), before, "{account}");
        assert_eq!(
            std::fs::read_to_string(home.join(shards_ipc::PID))
                .ok()
                .as_deref(),
            record,
            "{account}"
        );
    }
}

/// The memory a give-back stand-in frees, in regions of this many bytes, this many.
const REGION: usize = 32 << 20;
const REGIONS: usize = 4;

/// A stand-in guest's give-back: [`REGIONS`] regions of [`REGION`] bytes mapped and touched,
/// "mapped" said on stdout; once stdin gives a byte, each unmapped in turn, `ms` of busy CPU
/// apart, the first after twice that (its free, then its guest's wait before it reports, as
/// D151's reports come), "freed" said once the first has gone, as a guest's flush answers once
/// its host has its blocks back; then nothing until stdin closes.
fn falls(ms: u64, busy: impl Fn(u64)) {
    use std::io::Read as _;
    let regions: Vec<Region> = (0..REGIONS).map(|_| Region::touched()).collect();
    let mut out = std::io::stdout();
    out.write_all(b"mapped\n").unwrap();
    out.flush().unwrap();
    let mut stdin = std::io::stdin();
    let _ = stdin.read(&mut [0u8; 1]);
    busy(ms);
    for (i, region) in regions.into_iter().enumerate() {
        busy(ms);
        drop(region);
        if i == 0 {
            out.write_all(b"freed\n").unwrap();
            out.flush().unwrap();
        }
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

/// A command's refusal teaches its kind nothing: only a served answer (status 0) says its
/// words' work was done (ROOT's batch 22, KVM, f385b8dac: containers.rs'
/// `a_logs_history_is_what_its_user_set` ran `shards run --rm --pull never IMAGE exit 0` from
/// one closure twice, refused in 5.4 ms for a settings file that did not read, then served,
/// a microVM's boot, which the refusal's W* of 5.4 ms refused 300 ms in). Do: from one call
/// site, its kind's kept record removed, run one command line twice: the stand-in refusing
/// at once, then working a moment three times between 200 ms of nothing, as a client waits
/// on its daemon. Expect: the refusal leaves its kind cold; the served run waited on to its
/// end, status 0, its answer learned and kept as served.
#[test]
fn a_commands_refusal_teaches_its_kind_nothing() {
    #[track_caller]
    fn judged(cmd: std::process::Command, forget: bool) -> (common::Run, shards_api::Seen, String) {
        let site = std::panic::Location::caller();
        let (words, line) = (common::command_words(&cmd), common::keyed_line(&cmd));
        let kept = common::WaitKind::kept_at(site, &words, &line);
        if forget {
            let _ = std::fs::remove_file(&kept);
        }
        let run = common::run_command(cmd, &["stand-in".to_string()]);
        let seen = common::WaitKind::command(site, &words, &line).record().seen();
        (run, seen, std::fs::read_to_string(&kept).unwrap_or_default())
    }
    let run = |doing: &str, forget: bool| {
        let mut cmd = stand_in_doing("pulse 3 200");
        cmd.env("SHARDS_STAND_IN", doing);
        judged(cmd, forget)
    };
    let (refused, after_refusal, _) = run("refuse", true);
    let (served, seen, kept) = run("pulse 3 200", false);
    assert_eq!(refused.status, Some(1), "{refused}");
    assert_eq!(after_refusal, shards_api::Seen::Cold, "a refusal taught its kind");
    assert_eq!(served.status, Some(0), "{served}");
    let shards_api::Seen::Learned { slowest, .. } = seen else {
        panic!("a served answer taught its kind nothing");
    };
    assert!(slowest >= Duration::from_millis(600), "W* {slowest:?}");
    assert!(kept.starts_with("served "), "kept as {kept:?}");
}

/// A command asleep for good but ticking, as b24's guest's VM process ran a little in every
/// window, is refused once it has run past W* below R* and done more than any answer took
/// (C*), and a busy one past W* goes on (D215): HVF batch 24 at 1f71dea19, build.rs's
/// `shards run --rm sshed:1 stat /s1/noise.bin` waited 62 minutes, "its last progress process
/// 59939 ran 751µs ago" at every look, as any CPU at all was progress. Red so: the sleeper
/// served at its own end, 95 s in on HVF. Many kept records then held R* 0 ns, taught by answers
/// reaped before any look read their CPU, a floor that refuses nothing: such a record is read
/// as none, and only answers read through their end teach R* and C*. Do: from one call site,
/// its kind's kept record one of R* 0 ns without a rating, teach it three stand-ins busy
/// 50 ms; then one that works 1 ms in each 11 ms 2,000 times, within every W*; then one busy
/// 200 ms. Expect: the second refused, its account its rate against R* and C*; the third
/// served.
#[test]
fn a_command_asleep_but_ticking_is_refused_and_a_busy_one_goes_on() {
    #[track_caller]
    fn judged(
        cmd: std::process::Command,
        unrated: bool,
    ) -> (Result<common::Run, common::Ended>, std::path::PathBuf) {
        let (words, line) = (common::command_words(&cmd), common::keyed_line(&cmd));
        let kept = common::WaitKind::kept_at(std::panic::Location::caller(), &words, &line);
        if unrated {
            std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
            std::fs::write(&kept, "served 50000000 0 50000000\n").unwrap();
        }
        (
            common::run_until(cmd, &["stand-in".to_string()], Duration::from_secs(120)),
            kept,
        )
    }
    // One call site, one command line: one kind, whatever each stand-in does.
    let run = |doing: &str, unrated: bool| {
        let mut cmd = stand_in_doing("pulse 1 1");
        cmd.env("SHARDS_STAND_IN", doing);
        judged(cmd, unrated)
    };
    let mut unrated = true;
    for _ in 0..3 {
        let (taught, _) = run("busy 50", std::mem::take(&mut unrated));
        assert!(
            taught.as_ref().is_ok_and(|run| run.status == Some(0)),
            "teaching: {:?}",
            taught.err().map(|e| e.said)
        );
    }
    let start = Instant::now();
    let (asleep, kept) = run("pulse 2000 10", false);
    let said = match asleep {
        Ok(run) => panic!(
            "a ticking sleeper was not refused, {:?} in, its kind kept as {:?}: {run}",
            start.elapsed(),
            std::fs::read_to_string(&kept).unwrap_or_default()
        ),
        Err(ended) => ended.said,
    };
    assert!(
        said.contains("ran too slowly to be answering") && said.contains("C* "),
        "{said}"
    );
    let (busy, _) = run("busy 200", false);
    assert!(
        busy.as_ref().is_ok_and(|run| run.status == Some(0)),
        "the busy one: {:?}",
        busy.err().map(|e| e.said)
    );
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
    let kind = cold_kind();
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = common::Wait::begin(kind, quick.id(), None, "a quick stand-in");
    assert!(
        !taught.judged(),
        "its teaching wait judged by a record kept before it"
    );
    taught.absent();
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

/// A command waiting on this test process's own registry is held while the registry works
/// for it (D209 Part 8): its serving threads are the test process's, which no party's
/// members by parent include, so a pull or build from `common::served()`'s registry waited
/// on work no party counted. Red so (a mutation that leaves the serving threads uncounted):
/// refused once its client sat idle a whole W*. Do: teach a kind its W* by a quick
/// stand-in's end; then judge a stand-in that asks a server thread of this process, followed
/// as the registry's are (`common::Server`), which works 3 W* of CPU before it answers; all
/// in a stand-in, whose process nothing else runs in, so that its server's work is no other
/// test's progress. Expect: never refused; the asking stand-in's own end, with the answer.
#[test]
fn a_wait_on_this_processs_server_goes_on_while_it_serves() {
    let run = common::run_command(
        stand_in_doing("serving"),
        &["stand-in".to_string(), "serving".to_string()],
    );
    assert_eq!(run.status, Some(0), "{run}");
}

/// [`a_wait_on_this_processs_server_goes_on_while_it_serves`]'s stand-in.
fn serving() {
    let kind = cold_kind();
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = common::Wait::begin(kind, quick.id(), None, "a quick stand-in");
    taught.absent();
    while quick.try_wait().unwrap().is_none() {
        taught.look(false).unwrap();
    }
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = kind.record().seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let work = slowest.saturating_mul(3);
    let server = std::thread::spawn(move || {
        use std::io::Read as _;
        let (mut asked, _) = listener.accept().unwrap();
        let _serving = common::Server::begin();
        asked.read_exact(&mut [0u8; 4]).unwrap();
        let began = Instant::now();
        let mut spun = 0u64;
        while began.elapsed() < work {
            spun = std::hint::black_box(spun.wrapping_add(1));
        }
        asked.write_all(ANSWER).unwrap();
    });
    let mut client = stand_in_doing(&format!("fetch {port}"))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut wait = common::Wait::begin(kind, client.id(), None, "a stand-in asking this process");
    let began = Instant::now();
    let status = loop {
        if let Some(status) = client.try_wait().unwrap() {
            break status;
        }
        if let Err(stall) = wait.look(false) {
            let _ = client.kill();
            panic!(
                "refused {:?} into its server's {work:?} of work: {}",
                began.elapsed(),
                wait.refused(&stall)
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    server.join().unwrap();
    assert!(status.success(), "{status}");
}

/// A run's stop grace, which its home's daemon keeps (`shards_ipc::GRACE`), is the daemon's
/// own deadline: a wait on it is held until then, though nothing of its party runs, and judged
/// again past it (D209). HVF at 0ce6cdc66 (adas-mac-studio.local, kiln r2081): managed_output's
/// `stop daemon` refused 259 ms into its wait, "no progress for 45.3 ms" past a W* of 36.0
/// ms, as its daemon waited out the 10 s grace of a `sleep` that ignores SIGTERM as its PID
/// namespace's init, its log's next line 10.02 s after "stepping aside", the run `Exited
/// (137)`. Do as [`refused_beside_a_deadline`] says: the deadline a run's grace, 20 W* away;
/// a wait on no delivery. Expect: no refusal within the grace; a refusal once it is past.
#[test]
fn a_runs_stop_grace_is_its_daemons_and_not_refused() {
    let (at, until, said) =
        refused_beside_a_deadline(shards_ipc::GRACE, None, None, |w| w.saturating_mul(20));
    assert!(at >= until, "refused within its grace: {said}");
}

/// Where a run's log goes, as its options name it: inert, nothing listens there.
const LOG_SERVER: &str = "syslog-address=tcp://127.0.0.1:514";

/// A run's log's next try, which its home's daemon keeps while its shipper sleeps before it,
/// naming where the log goes (`shards_ipc::RETRY`), is the daemon's own deadline for a wait
/// on that delivery (D209 Part 10). HVF at 6f5755ae1 (adas-mac-studio.local-2, the
/// shards-green lane): containers.rs' `a_runs_log_is_sent_to_syslog_as_its_options_say`
/// refused 763 ms into its wait for a server that comes up only after its run, "no progress
/// for 399 ms" past a W* of 241 ms, its daemon asleep in its shipper's first pause (ship.rs
/// `RETRY_FIRST`). Do as [`refused_beside_a_deadline`] says: the deadline a log's next try to
/// a server, 20 W* away; a wait on the delivery to that server. Expect: no refusal before the
/// deadline; a refusal once it is past.
#[test]
fn a_logs_next_try_holds_a_wait_on_its_delivery() {
    let (at, until, said) =
        refused_beside_a_deadline(shards_ipc::RETRY, Some(LOG_SERVER), Some(LOG_SERVER), |w| {
            w.saturating_mul(20)
        });
    assert!(at >= until, "refused before its log's next try: {said}");
}

/// A run's log's next try holds up that log's delivery alone, so it holds no other wait on
/// its daemon (ROOT's ruling of 2026-11-16, D209 Part 10): one is judged on its own evidence,
/// as before 03c0b8603, and a stall elsewhere in a daemon whose shipper retries for ever is
/// still refused. Do as [`refused_beside_a_deadline`] says: the deadline a log's next try to a
/// server, as far away as a refusal past a deadline is waited for ([`refusal_bound`]); a wait
/// on no delivery, then one on the delivery to another server. Expect: each refused before
/// the deadline.
#[test]
fn a_logs_next_try_holds_no_other_wait_on_its_daemon() {
    let (at, until, said) =
        refused_beside_a_deadline(shards_ipc::RETRY, Some(LOG_SERVER), None, refusal_bound);
    assert!(
        at < until,
        "a wait on no delivery held by a log's next try: {said}"
    );
    let elsewhere = "syslog-address=tcp://127.0.0.1:601";
    let (at, until, said) = refused_beside_a_deadline(
        shards_ipc::RETRY,
        Some(LOG_SERVER),
        Some(elsewhere),
        refusal_bound,
    );
    assert!(
        at < until,
        "a wait on another delivery held by a log's next try: {said}"
    );
}

/// How long past a deadline a wait not yet refused is taken for one never refused: past its
/// deadline, a whole W* still is refused well within this.
fn refusal_bound(slowest: Duration) -> Duration {
    slowest.saturating_mul(100).max(Duration::from_secs(10))
}

/// Do: teach a kind of its caller's its W* by a quick stand-in's end; name a sleeping
/// stand-in its home's daemon, and a run of that home holding the deadline `name`, `away(W*)`
/// from now, of `of` where it names what it holds up; wait on a sleeping client, a wait on the
/// delivery to `reads` where it names one, until it is refused. Its refusal's time, the
/// deadline, and what the refusal said; a failure where it is never refused, though
/// [`refusal_bound`] past the deadline.
#[track_caller]
fn refused_beside_a_deadline(
    name: &str,
    of: Option<&str>,
    reads: Option<&str>,
    away: fn(Duration) -> Duration,
) -> (std::time::SystemTime, std::time::SystemTime, String) {
    let kind = cold_kind();
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = common::Wait::begin(kind, quick.id(), None, "a quick stand-in");
    taught.absent();
    while quick.try_wait().unwrap().is_none() {
        taught.look(false).unwrap();
    }
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = kind.record().seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let home = TempDir::new("grace");
    let spawn = || {
        stand_in_doing("pulse 1 600000")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let (mut daemon, mut client) = (spawn(), spawn());
    name_daemon(&home, daemon.id());
    let run = home.join("containers").join("graced");
    std::fs::create_dir_all(&run).unwrap();
    let until = std::time::SystemTime::now() + away(slowest);
    std::fs::write(run.join(name), shards_ipc::deadline_text(until, of).unwrap()).unwrap();
    let mut wait = common::Wait::begin(
        kind,
        client.id(),
        Some(&home),
        "a wait beside its daemon's deadline",
    );
    if let Some(to) = reads {
        wait = wait.delivery_to(to);
    }
    let refused = loop {
        if let Err(stall) = wait.look(false) {
            break Some((std::time::SystemTime::now(), wait.refused(&stall)));
        }
        if std::time::SystemTime::now() > until + refusal_bound(slowest) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    for party in [&mut daemon, &mut client] {
        let _ = party.kill();
        let _ = party.wait();
    }
    let Some((at, said)) = refused else {
        panic!("never refused past its {name}: {}", wait.account());
    };
    (at, until, said)
}

/// A test's wait on a state its home's daemon comes to (`Patience`) goes on while its party
/// works, as a command's wait does (the person's ruling of 2026-11-13, D209): its caller
/// gives it no bound. daemon.rs' and containers.rs' waits on pools and runs were cut at
/// their tests' `TIMEOUT` however their daemon worked, as run_command's were before W6's
/// slice 2: red so at 3e9723c64, cut 0.1 s into a second's work at a bound of 100 ms. Do: a
/// stand-in named its home's daemon, busy for a second from its go, sent once a look of its
/// wait has found it running; wait for its end, twice, from one call site. Expect: each wait
/// holds until it ends, and its end, told `done`, teaches the call site's kind its answer.
#[test]
fn a_patient_wait_goes_on_while_its_party_works() {
    let home = TempDir::new("patient");
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        // Its busy second begins on its go, its stdin's end, sent once a look of its wait
        // found its end missing, so that its end is an answer its wait waited for, timed
        // from its wait's begin, and never less than its work: a busy second begun as it
        // ran, before its spawn returned, taught 993.99 ms under load (adacachy2 at
        // 9dd326609), as runs-11's health test's answer did (490ddff78); and an end found
        // before any look of its wait teaches nothing, as a drain's ends and a stream's answer
        // did on a loaded KVM host (ROOT's run at 53366451ac).
        let mut daemon = stand_in_doing(&format!("go {}", busy.as_millis()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        name_daemon(&home, daemon.id());
        let mut patience = Patience::of(&home);
        let began = Instant::now();
        while daemon.try_wait().unwrap().is_none() {
            if !patience.holds() {
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!(
                    "a daemon busy for {busy:?} did not end within its wait: cut {:?} in, round {round}",
                    began.elapsed()
                );
            }
            drop(daemon.stdin.take());
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

/// A home's patience follows the daemon it found as it began after the daemon's record has
/// gone, and gives it up only once it has gone too, at as many looks in a row that find none
/// of its party as refuse a still one (`LOOKS_TO_REFUSE`; ROOT, 2026-11-18, #36). A test's
/// removal of its home takes the daemon's record before the daemon has ended, and on Windows
/// stops at what the home's microVMs still hold: daemon.rs' `a_restored_microvm_ends_with_its_home`
/// made its patience before its removal, which first looked after it, and was refused 57 ms
/// in, "this test's own VM processes, no daemon yet", its restored microVM still ending
/// (jupiter, bare-metal WHP, at 183fe264e3). Red so (lanes/redgreen/cold-wait.patch): refused
/// at its first look. Do: a stand-in, idle, named its home's daemon; a cold patience of the
/// home; the record removed; as many looks as refuse a gone party, and one more, while the
/// stand-in lives; then its end, and looks until its patience gives up. Expect: held while it
/// lives; given up after, at no fewer looks than refuse a gone party.
#[test]
fn a_homes_patience_follows_its_daemon_past_its_record() {
    let home = TempDir::new("unrecorded");
    let mut daemon = stand_in_doing("idle")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    name_daemon(&home, daemon.id());
    let mut patience = cold_patience(&home);
    std::fs::remove_file(home.join("daemon.pid")).unwrap();
    // Each look a due one: its pace waited out before it.
    for look in 0..=shards_api::LOOKS_TO_REFUSE {
        std::thread::sleep(patience.next_look());
        if !patience.holds() {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("given up at look {look} with its daemon alive, its record gone");
        }
    }
    drop(daemon.stdin.take());
    daemon.wait().unwrap();
    let mut looks = 0;
    loop {
        std::thread::sleep(patience.next_look());
        if !patience.holds() {
            break;
        }
        looks += 1;
    }
    assert!(
        looks >= shards_api::LOOKS_TO_REFUSE,
        "given up {looks} looks past its daemon's end, before {} found its party gone",
        shards_api::LOOKS_TO_REFUSE
    );
}

/// A test's wait on a guest (`Heard`: every `Vm` wait, `within`, run.rs' and restore.rs'
/// waits on their VM process) goes on while its party works, as a command's wait does (the
/// person's ruling of 2026-11-13, D209): its caller gives it no bound, and past its cold start
/// it is judged each W* of its call site's kind. Red at 70eba153d, as
/// `red_a_guest_wait_goes_on_while_its_party_works` there: `Heard::running` on a stand-in
/// busy for a second, past a caller's bound of 100 ms, cut 0.1 s in, as `Ran` judged a VM
/// process only a `PROGRESS_SLICE` after it first found it, and its stand-in no VM process
/// at all. Do: a stand-in busy for a second from its go, sent once a look of its wait has
/// found it running (as [`a_patient_wait_goes_on_while_its_party_works`]'); wait for its end,
/// twice, from one call site. Expect: each wait holds until it ends, and the first's end
/// teaches the second's kind.
#[test]
fn a_guest_wait_goes_on_while_its_party_works() {
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        let began = Instant::now();
        let mut party = stand_in_doing(&format!("go {}", busy.as_millis()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut heard = common::Heard::running(party.id(), None);
        assert!(
            round == 0
                || matches!(heard.seen(), shards_api::Seen::Learned { slowest, .. } if slowest >= busy),
            "its kind learned no answer of a second's work: {:?}",
            heard.seen()
        );
        while party.try_wait().unwrap().is_none() {
            if let Err(refused) = heard.look(true) {
                let _ = party.kill();
                let _ = party.wait();
                panic!(
                    "a party busy for {busy:?} cut {:?} into its wait, round {round}: {refused}",
                    began.elapsed()
                );
            }
            drop(party.stdin.take());
            std::thread::sleep(Duration::from_millis(2));
        }
        heard.answered();
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
    }
}

/// [`common::Heard::streaming`] at one call site: one kind for every wait made through it.
fn streaming(vm: u32) -> common::Heard<'static> {
    cold_heard(vm)
}

/// [`common::Heard::streaming`] at its caller's call site, its kind made cold as
/// [`cold_kind`]'s is.
#[track_caller]
fn cold_heard(vm: u32) -> common::Heard<'static> {
    let site = std::panic::Location::caller();
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, common::PROGRESS, ""));
    common::Heard::streaming(vm, None, None)
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
    taught.absent();
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

/// A guest whose CPU is its liveness alone (`Cpu::Liveness`, past its first line) is held, not
/// refused, while its host keeps a thread of its VM process from every CPU
/// (`shards_api::Watch::starved`), on Linux and Windows alike: a vCPU waiting for a CPU the
/// guest's other vCPUs hold is its host's hold (ROOT's batch 26), and Windows read no thread
/// of it before (own-38h, ROOT's ruling of 2026-11-18). Do: teach the kind its W* by a
/// stand-in's own end; follow a stand-in two threads of which spin pinned to one CPU, one of
/// them in its OS's idle class ([`starved`]), from its word that the other keeps it from that
/// CPU, through `LOOKS_TO_REFUSE` of its judge's windows, looking at its own pace. Expect: no
/// look refused; with Windows' read removed (lanes/redgreen/own-38h-starved-windows.patch),
/// refused there. Followed from its spawn, before its threads were in their places, it was
/// refused while it started, its process running and none of its threads kept, three windows
/// of a W* of 593 µs being less than its start on a loaded KVM host (ROOT's run at p/batch2
/// c7e44c7185, glibc: "its host not holding it", refused 2.05 ms in).
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_streaming_guest_whose_thread_its_host_keeps_from_any_cpu_is_held() {
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = streaming(quick.id());
    taught.absent();
    while quick.try_wait().unwrap().is_none() {
        taught.look(true).unwrap();
    }
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = taught.seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let mut kept = stand_in_doing("starved")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Its word comes among libtest's own lines ("test stand_in ... kept"); its stdout is held
    // open until its end, so that what libtest says after it finds a reader.
    let mut out = std::io::BufReader::new(kept.stdout.take().unwrap());
    let mut said = String::new();
    while !said.contains("kept") {
        said.clear();
        let read = std::io::BufRead::read_line(&mut out, &mut said).unwrap();
        assert_ne!(read, 0, "the stand-in ended with none of its threads kept");
    }
    let mut heard = streaming(kept.id());
    let began = Instant::now();
    while began.elapsed() <= slowest.saturating_mul(shards_api::LOOKS_TO_REFUSE) {
        let judged = heard.look(true);
        assert!(judged.is_ok(), "{judged:?}");
        std::thread::sleep(heard.next_look());
    }
    drop(kept.stdin.take());
    kept.wait().unwrap();
}

/// Says each of `script`'s lines into `said`, each whole, as a console's lines come: a guest's
/// console as [`common::guest_says`] reads one, `reads` telling of each of its reads
/// ([`cold_lines`]). Each line comes its pause in milliseconds after the read that found the
/// line before (the first, after its waiter's first read), so that each is found by a read of
/// its own and each pause is a silence its waiter saw, however long its waiter's looks take.
/// A clock of its own did not do that on Windows: a machine's look reads the system's whole
/// thread listing three times there (`Party::machine`'s threads beside its waiter, and its
/// account's runnable threads and thread states; 13.1 to 13.8 ms a read on jupiter, PM M606),
/// and a wait's first look comes before its first read, so a guest whose lines came 20, 40 and
/// 60 ms in was first read 74.8 ms in, every line by that read, and its pass taught nothing
/// (ROOT's run on jupiter at 07d4396a0c). Ends at its script's end, or once its waiter reads
/// no more.
fn console_says(
    said: &std::sync::Mutex<Vec<u8>>,
    reads: std::sync::mpsc::Receiver<()>,
    script: &[(u64, &str)],
) {
    for (ms, line) in script {
        if reads.recv().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(*ms));
        let mut said = said.lock().unwrap();
        said.extend_from_slice(format!("{line}\n").as_bytes());
        // Told under the same lock as a read takes: none told by now found this line.
        while reads.try_recv().is_ok() {}
    }
}

/// [`common::guest_says`] at its caller's call site, its two kinds made cold as
/// [`cold_kind`]'s is, waiting for `done` from a guest of more vCPUs than any host has CPUs
/// (its CPU its liveness alone), each of its reads told by `told` to the guest that says
/// `said` ([`console_says`]); and those kinds, its first line's and its next lines'. `told` goes
/// with its reads, so that its guest hears them end as its wait does.
#[track_caller]
fn cold_lines(
    said: &std::sync::Mutex<Vec<u8>>,
    told: std::sync::mpsc::Sender<()>,
    ended: &std::sync::mpsc::Receiver<()>,
) -> (common::Saying<()>, common::Lines, [&'static common::WaitKind; 2]) {
    let site = std::panic::Location::caller();
    let kinds = [common::FIRST_LINE, common::NEXT_LINE].map(|words| {
        let _ = std::fs::remove_file(common::WaitKind::kept_at(site, words, ""));
        common::WaitKind::of(site, words, "")
    });
    let read = move || {
        let said = said.lock().unwrap();
        let _ = told.send(());
        said.clone()
    };
    let (saying, lines) = common::guest_says(read, ended, "done", "", u32::MAX, None);
    (saying, lines, kinds)
}

/// A wait for a guest's line (`common::guest_says`: boot.rs' boot of its host's most vCPUs,
/// read from its kernel's printk ring and then its console, D261) goes on while its guest says
/// lines it had not said, and its pass teaches its kinds what its waits took: the time to its
/// first line, and the slowest gap between two lines after it, not the last. Do: cold, a guest
/// that says a line 50 ms after its waiter's first read, another 200 ms after the read that
/// found it and the one awaited 50 ms after the read that found that ([`console_says`]).
/// Expect: said; its first line's kind taught the time to its first line, and its next lines'
/// the slowest gap between its lines, each as its reads first found them.
#[test]
fn a_guests_lines_are_its_progress_and_its_pass_teaches_their_slowest_gap() {
    let (_end, ended) = std::sync::mpsc::channel::<()>();
    let said = std::sync::Mutex::new(Vec::new());
    let (told, reads) = std::sync::mpsc::channel();
    let (saying, lines, [first, next]) = std::thread::scope(|s| {
        s.spawn(|| console_says(&said, reads, &[(50, "one"), (200, "two"), (50, "done")]));
        cold_lines(&said, told, &ended)
    });
    assert!(
        matches!(&saying, common::Saying::Said { line, ended: None } if line == "done"),
        "{saying:?}\n{}",
        lines.timed()
    );
    let times: Vec<Duration> = lines.seen().iter().map(|(at, _)| *at).collect();
    let gap = times.windows(2).map(|w| w[1].saturating_sub(w[0])).max();
    let learned = |kind: &common::WaitKind| match kind.record().seen() {
        shards_api::Seen::Learned { slowest, .. } => Some(slowest),
        shards_api::Seen::Cold => None,
    };
    assert_eq!(learned(first), times.first().copied(), "{}", lines.timed());
    assert_eq!(learned(next), gap, "{}", lines.timed());
}

/// A guest of more vCPUs than its host has CPUs runs them for ever, its workload stopped or
/// not: a wait for its next line is refused once it has said nothing new for longer than the
/// slowest gap between lines its passes saw on this host, however this process's threads
/// beside its waiter spin (`Cpu::Liveness`), unless its host keeps one of them from any CPU
/// (its hold, as below); what it said comes back with the refusal, each
/// line with its time, and its stall teaches nothing (D261: boot.rs' 255-vCPU boot on a nested
/// WHP runner, followed by its liveness alone to its step's 122 minutes). Do: a pass whose
/// lines come 300 ms after the read that found the line before ([`console_says`]); then,
/// through the same call site, a guest that says two lines, 10 ms after such reads, and then
/// nothing, a thread spinning beside its wait. Expect: the pass said; the second refused
/// within [`refusal_bound`] of the pass's slowest gap, both its lines among what it said; its
/// kinds' records as the pass left them.
#[test]
fn a_guest_silent_past_its_slowest_gap_is_refused_and_its_stall_teaches_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_end, ended) = std::sync::mpsc::channel::<()>();
    let guest = |script: &[(u64, &str)]| {
        let said = std::sync::Mutex::new(Vec::new());
        let (told, reads) = std::sync::mpsc::channel();
        let spinning = AtomicBool::new(true);
        std::thread::scope(|s| {
            s.spawn(|| console_says(&said, reads, script));
            s.spawn(|| {
                while spinning.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            let heard = cold_lines(&said, told, &ended);
            spinning.store(false, Ordering::Relaxed);
            heard
        })
    };
    let (saying, lines, kinds) = guest(&[(300, "one"), (300, "two"), (300, "done")]);
    assert!(
        matches!(&saying, common::Saying::Said { line, .. } if line == "done"),
        "{saying:?}\n{}",
        lines.timed()
    );
    let taught = kinds.map(|kind| kind.record().seen());
    let shards_api::Seen::Learned { slowest: gap, .. } = taught[1] else {
        panic!("its pass taught its next lines' kind nothing\n{}", lines.timed());
    };
    let began = Instant::now();
    let (saying, lines, kinds) = guest(&[(10, "one"), (10, "two")]);
    let refused = began.elapsed();
    assert!(
        matches!(saying, common::Saying::Refused(_)),
        "{saying:?}\n{}",
        lines.timed()
    );
    assert!(refused < refusal_bound(gap), "refused {refused:?} in");
    assert!(
        lines.text().contains("one") && lines.text().contains("two"),
        "{saying:?}\n{}",
        lines.timed()
    );
    assert_eq!(kinds.map(|kind| kind.record().seen()), taught);
}

/// A guest of more vCPUs than its host has CPUs, silent past its slowest gap while its host
/// keeps a thread beside its waiter from any CPU, is held, not refused: under `Cpu::Liveness`
/// a thread runnable as a look reads it and unrun since the look before
/// (`shards_api::starved`) is its host's hold, of a machine's party as of a VM process's (D209
/// Part 5, D261; CI 44268931283: a 16-vCPU boot on a 4-CPU runner refused 140 ms into a silent
/// gap at W* 70 ms, its threads unread). Do (Linux, Windows): a pass each of whose lines comes
/// 20 ms after the read that found the line before ([`console_says`]; by its own clock, every
/// line of it was found by one read on Windows and the pass taught nothing: ROOT's run on
/// jupiter at 07d4396a0c); then, through the same call site, a guest that says a line and then
/// nothing for `LOOKS_TO_REFUSE` of the pass's slowest gaps from the read that found it, while
/// two threads beside its waiter spin pinned to one CPU, one of them in its OS's idle class
/// ([`idle_policy`]): runnable, and kept from that CPU by the other. Expect: the second said;
/// refused at its judge's second due look, two gaps in, with the hold removed
/// (lanes/redgreen/own-38h-vcpu.patch), or with Windows' reading of those threads removed
/// (lanes/redgreen/own-38j-beside-windows-reading.patch, on Windows).
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_wide_guest_silent_while_its_host_holds_a_thread_beside_its_waiter_is_held() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_end, ended) = std::sync::mpsc::channel::<()>();
    let cpu = first_cpu();
    let guest = |script: &[(u64, &str)], kept: bool| {
        let said = std::sync::Mutex::new(Vec::new());
        let (told, reads) = std::sync::mpsc::channel();
        let spinning = AtomicBool::new(true);
        std::thread::scope(|s| {
            s.spawn(|| console_says(&said, reads, script));
            for idle in [false, true].into_iter().filter(|_| kept) {
                let spinning = &spinning;
                s.spawn(move || {
                    pin_to(cpu);
                    if idle {
                        idle_policy();
                    }
                    while spinning.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                });
            }
            let heard = cold_lines(&said, told, &ended);
            spinning.store(false, Ordering::Relaxed);
            heard
        })
    };
    let (saying, lines, kinds) = guest(&[(20, "one"), (20, "two"), (20, "done")], false);
    assert!(
        matches!(&saying, common::Saying::Said { line, .. } if line == "done"),
        "{saying:?}\n{}",
        lines.timed()
    );
    let shards_api::Seen::Learned { slowest: gap, .. } = kinds[1].record().seen() else {
        panic!("its pass taught its next lines' kind nothing\n{}", lines.timed());
    };
    let silent = gap.saturating_mul(shards_api::LOOKS_TO_REFUSE);
    let silent = u64::try_from(silent.as_millis()).unwrap().saturating_add(1);
    let (saying, lines, _) = guest(&[(0, "one"), (silent, "done")], true);
    assert!(
        matches!(&saying, common::Saying::Said { line, .. } if line == "done"),
        "{saying:?}\n{}",
        lines.timed()
    );
}

/// A stand-in's two threads spinning pinned to one CPU until its stdin closes, one of them in
/// its OS's idle class ([`idle_policy`]): the other keeps it from that CPU, so that it is
/// runnable and almost never run, as its host keeps a vCPU of a guest wider than itself. It
/// says "kept" on its stdout once that is so: the idle one placed first, pinned and in its
/// class, while nothing of its own keeps it from running to say so, then the other, pinned.
/// Never while either is not: a thread that fails to place itself has the other stop, and
/// its failure ends the stand-in.
#[cfg(any(target_os = "linux", windows))]
fn starved() {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    let cpu = first_cpu();
    let spinning = AtomicBool::new(true);
    std::thread::scope(|s| {
        let placed = [true, false].into_iter().all(|idle| {
            let (ready, readied) = std::sync::mpsc::channel();
            let spinning = &spinning;
            s.spawn(move || {
                pin_to(cpu);
                if idle {
                    idle_policy();
                }
                let _ = ready.send(());
                while spinning.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            readied.recv().is_ok()
        });
        if placed {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"kept\n");
            let _ = out.flush();
            let _ = std::io::stdin().read(&mut [0u8; 1]);
        }
        spinning.store(false, Ordering::Relaxed);
    });
}

#[cfg(not(any(target_os = "linux", windows)))]
fn starved() {
    panic!("a starved stand-in is made on Linux and Windows alone");
}

/// The first CPU this process may run on (sched_getaffinity(2)).
#[cfg(target_os = "linux")]
fn first_cpu() -> usize {
    // SAFETY: a cpu_set_t, plain data, for sched_getaffinity to fill.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: sched_getaffinity(2) of this thread into a local set of its size.
    let got = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut set) };
    assert_eq!(got, 0);
    let cpus = usize::try_from(libc::CPU_SETSIZE).unwrap();
    // SAFETY: CPU_ISSET reads a bit of a set filled above, below CPU_SETSIZE.
    (0..cpus)
        .find(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .unwrap()
}

/// Pins the calling thread to `cpu` alone (sched_setaffinity(2) of the calling thread).
#[cfg(target_os = "linux")]
fn pin_to(cpu: usize) {
    // SAFETY: a cpu_set_t, plain data; CPU_SET sets a bit below CPU_SETSIZE in it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: sched_setaffinity(2) of the calling thread (0) to a set of its size.
    let got = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set) };
    assert_eq!(got, 0);
}

/// Puts the calling thread under SCHED_IDLE, the least weight a thread may have, which needs
/// no privilege (sched(7), since Linux 2.6.39): beside a thread of the default policy on its
/// CPU it is runnable, and almost never run. The raw call, as musl's wrapper refuses every
/// policy (ENOSYS: POSIX's policy is the process's, Linux's the thread's).
#[cfg(target_os = "linux")]
fn idle_policy() {
    // SAFETY: a sched_param, plain data, its priority 0 as SCHED_IDLE requires.
    let param: libc::sched_param = unsafe { std::mem::zeroed() };
    // SAFETY: sched_setscheduler(2) of the calling thread (0), reading `param`.
    let got = unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            0,
            libc::SCHED_IDLE,
            &raw const param,
        )
    };
    assert_eq!(got, 0);
}

/// The first CPU this process may run on (GetProcessAffinityMask), as its affinity mask's
/// lowest bit.
#[cfg(windows)]
fn first_cpu() -> usize {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: this process's pseudo-handle, which needs no closing, and two masks to write.
    let got = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) };
    assert_ne!(got, 0, "{}", std::io::Error::last_os_error());
    assert_ne!(process, 0, "no CPU this process may run on");
    usize::try_from(process.trailing_zeros()).unwrap()
}

/// Pins the calling thread to `cpu` alone (SetThreadAffinityMask).
#[cfg(windows)]
fn pin_to(cpu: usize) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    let mask = 1usize.checked_shl(u32::try_from(cpu).unwrap()).unwrap();
    // SAFETY: this thread's pseudo-handle, which needs no closing, and a mask of a CPU its
    // process may run on.
    let was = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
    assert_ne!(was, 0, "{}", std::io::Error::last_os_error());
}

/// Puts the calling thread at THREAD_PRIORITY_IDLE, the least of its process's class
/// (SetThreadPriority). Windows runs the ready thread of the highest priority and preempts a
/// lower one for it, round-robin only among equals (Microsoft Learn, "Scheduling Priorities":
/// base priority 1 in every class but realtime's, a THREAD_PRIORITY_NORMAL thread's 8 in
/// NORMAL_PRIORITY_CLASS), so beside a spinner of the default priority pinned to its CPU it is
/// ready and unrun. Its documented boosts are a foreground window's, a window's input's and a
/// satisfied wait's ("Priority Boosts"), and a lock owner's to its waiter's priority
/// (AutoBoost, "Priority Inversion"): a spinner never waits and holds no lock. What remains is
/// the balance set manager's: once a second it raises a thread ready for about 4 s to
/// priority 15 for a quantum, then back to its base (Windows Internals 7th ed., Part 1, ch. 4,
/// "Boosts for CPU starvation"; Solomon, Russinovich and Polze's Windows Operating System
/// Internals curriculum, unit 4.5, "CPU Starvation Avoidance"). So it runs once in 4 s at
/// most: a look that sees it run makes one still window at most, which refuses nothing alone
/// (`shards_api::LOOKS_TO_REFUSE`).
#[cfg(windows)]
fn idle_policy() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_IDLE};
    // SAFETY: this thread's pseudo-handle, which needs no closing, and a priority Windows
    // names.
    let set = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE) };
    assert_ne!(set, 0, "{}", std::io::Error::last_os_error());
}

/// A guest's machine that ends ends a wait for its line, what its guest said by then read
/// first (boot.rs: a vCPU's failed entry, D261's A/B; a guest that resets once it has said
/// it). Do: a guest that says a line and then its machine ends; one that says the line
/// awaited and then ends at once. Expect: the first ended, its line among what it said; the
/// second said, whether its wait saw its line or its end first.
#[test]
fn a_guests_end_ends_its_wait_and_what_it_said_by_then_is_read() {
    let ends = |script: &[(u64, &str)]| {
        let (end, ended) = std::sync::mpsc::channel::<()>();
        let said = std::sync::Mutex::new(Vec::new());
        let (told, reads) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                console_says(&said, reads, script);
                let _ = end.send(());
            });
            cold_lines(&said, told, &ended)
        })
    };
    let (saying, lines, _) = ends(&[(10, "one")]);
    assert!(
        matches!(saying, common::Saying::Ended(Some(()))) && lines.text().contains("one"),
        "{saying:?}\n{}",
        lines.timed()
    );
    let (saying, lines, _) = ends(&[(10, "done")]);
    assert!(
        matches!(&saying, common::Saying::Said { line, .. } if line == "done"),
        "{saying:?}\n{}",
        lines.timed()
    );
}

/// What [`cold_ends`] gives back: the ends that came, each its client, or those and what its
/// refusal said.
type Drained = Result<Vec<u32>, (Vec<u32>, String)>;

/// [`common::requests_end`] at its caller's call site, its kind made cold as [`cold_lines`]'
/// are: waiting for the ends `next` reads, each its own client's, of requests whose clients
/// are `clients`, with `stretches` seen before it, in a home no daemon serves; and its kind,
/// and where this host keeps its record.
#[track_caller]
fn cold_ends(
    next: impl FnMut(Duration) -> Result<u32, std::sync::mpsc::RecvTimeoutError>,
    clients: &[u32],
    stretches: &[Duration],
) -> (Drained, &'static common::WaitKind, std::path::PathBuf) {
    let site = std::panic::Location::caller();
    let kept = common::WaitKind::kept_at(site, common::NEXT_END, "");
    let _ = std::fs::remove_file(&kept);
    let kind = common::WaitKind::awaiting(site, common::NEXT_END);
    let home = TempDir::new("ends");
    let came = common::requests_end(next, clients.len(), clients, |&c| Some(c), stretches, &home);
    (came, kind, kept)
}

/// Sends each of `ends`' clients on `end`, its pause in milliseconds after a read of its
/// waiter's ([`cold_ends`]) found it missing, `reads` telling of each read, once it has
/// returned, whether it took an end: the first's after a read that took none, each other's
/// after one past the read that took the end before, so that each end teaches the wait that
/// began as that one was taken, and each pause is a silence that wait saw from its start,
/// however long its looks take. A clock of its own did not do that: a drain's wait teaches an
/// end only where a read of it timed out and its look found its end missing first, and with
/// ends 30, 120 and 30 ms apart from its start each came before its wait's first look and its
/// pause had ended, under load, and its pass taught nothing (ROOT's run on a loaded KVM host
/// at 53366451ac, glibc). Ends at its script's end, or once its waiter reads no more.
fn ends_come(
    end: &std::sync::mpsc::Sender<u32>,
    reads: std::sync::mpsc::Receiver<bool>,
    ends: &[(u32, u64)],
) {
    for (n, &(client, ms)) in ends.iter().enumerate() {
        let mut told = reads.iter();
        if (n > 0 && !told.any(|took| took)) || !told.any(|took| !took) {
            return;
        }
        std::thread::sleep(Duration::from_millis(ms));
        let _ = end.send(client);
    }
}

/// A drain's wait for its requests' ends (`common::requests_end`: benches/arrival.rs, D209
/// Part 11) goes on while they end, however slowly, each end's client leaving its party, and
/// its pass teaches its kind the slowest wait for a next end and keeps it on this host. Do:
/// cold, three idle clients, whose ends come 30, 120 and 30 ms after a read of its waiter's
/// found each missing ([`ends_come`]). Expect: all three, in order; its kind taught a wait no
/// shorter than the longest pause, which the wait for that end spans from its start, and
/// kept.
#[test]
fn a_drains_ends_are_its_progress_and_its_pass_teaches_their_slowest_gap() {
    let idle: Vec<common::Reaped> = (0..3)
        .map(|_| {
            common::Reaped(
                stand_in_doing("idle")
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    let clients: Vec<u32> = idle.iter().map(|client| client.id()).collect();
    let ends: Vec<(u32, u64)> = clients.iter().copied().zip([30, 120, 30]).collect();
    let (end, ended) = std::sync::mpsc::channel();
    let (told, reads) = std::sync::mpsc::channel();
    let (came, kind, kept) = std::thread::scope(|s| {
        s.spawn(|| ends_come(&end, reads, &ends));
        // Its reads tell `ends_come` of themselves, and their telling ends as the drain does.
        let next = move |within| {
            let read = ended.recv_timeout(within);
            let _ = told.send(read.is_ok());
            read
        };
        cold_ends(next, &clients, &[])
    });
    drop(idle);
    let came = came.unwrap_or_else(|(came, refused)| panic!("refused with {came:?}: {refused}"));
    assert_eq!(came, clients);
    let shards_api::Seen::Learned { slowest, .. } = kind.record().seen() else {
        panic!("its pass taught its kind nothing");
    };
    let longest = ends
        .iter()
        .map(|&(_, ms)| Duration::from_millis(ms))
        .max()
        .unwrap();
    assert!(slowest >= longest, "it taught {slowest:?}");
    assert!(kept.exists(), "its pass kept nothing at {}", kept.display());
}

/// A drain whose requests stop ending is refused once none has ended for longer than the
/// slowest stretch its kind has seen, however its party runs (`Cpu::Liveness`: a pool's warm
/// microVMs and their daemon run for ever), and keeps nothing (D209 Part 11: benches/arrival.rs
/// in CI 44007524337, its drain followed by its party's CPU to its job's 30-minute limit). Do:
/// cold, a stretch of 50 ms its caller saw; three clients busy throughout, the ends of two
/// coming 10 ms apart and then none, its channel open. Expect: refused within
/// [`refusal_bound`] of 50 ms, those two ends what came; nothing kept.
#[test]
fn a_drain_whose_requests_stop_ending_is_refused_and_keeps_nothing() {
    let busy: Vec<common::Reaped> = (0..3)
        .map(|_| common::Reaped(stand_in_doing("busy 600000").spawn().unwrap()))
        .collect();
    let clients: Vec<u32> = busy.iter().map(|client| client.id()).collect();
    let (end, ended) = std::sync::mpsc::channel();
    let began = Instant::now();
    let (came, _, kept) = std::thread::scope(|s| {
        s.spawn(|| {
            for &client in &clients[..2] {
                std::thread::sleep(Duration::from_millis(10));
                let _ = end.send(client);
            }
        });
        let next = |within| ended.recv_timeout(within);
        cold_ends(next, &clients, &[Duration::from_millis(50)])
    });
    let took = began.elapsed();
    drop(busy);
    let Err((came, refused)) = came else {
        panic!("a drain whose requests stopped ending was not refused");
    };
    assert_eq!(came, clients[..2], "{refused}");
    assert!(
        took < refusal_bound(Duration::from_millis(50)),
        "refused {took:?} in: {refused}"
    );
    assert!(!kept.exists(), "its refusal kept {}", kept.display());
    drop(end);
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

/// A judged call's own work, on its own thread of this process, is its party's progress
/// (ROOT's batch 22, f385b8dac: api_image.rs' `refused` closures boot through
/// `common::booted`, whose shards-api client checks a spec on the call's thread; a refusal
/// answered in 206 µs on HVF, and one of 28.6 ms on KVM, both at 0 ns of the started
/// processes' CPU, taught their call site a W* that refused the next call 206 µs and 31 ms
/// in as it worked). Do: a stand-in idle throughout, the party's one live process, its calls
/// begun once it says it rests; from one call site, its kind's kept record removed, a call
/// that answers at once, then one busy for 200 ms on its own thread, then one idle there
/// until its wait is refused, the refusal ending it, or until its wait's due looks have found
/// its party still `shards_api::LOOKS_TO_REFUSE` + 1 times since it last ran, none refusing
/// it, past which it answers. Expect: the first two answer; the idle one is refused, and one
/// that answers fails the test with those looks and their times. It counts looks and sets
/// itself no span of time, which a host holding the looks late would cut short: a resting
/// process on Windows runs its default
/// thread pool's workers when its system decides ("the system controls the lifetime of
/// thread pool threads", Microsoft's Thread Pools; the loader's own use of the pool, Raymond
/// Chen, "Why does my single-threaded program have multiple threads?", 2019-11-15), and on
/// CI's arm64 Windows runners one of the stand-in's three ran once, about 840 ms into a call
/// asleep for 1 s, at the due look its refusal had come to (run 44138098584; trace runs
/// 44151128928 and 44163626733, PM M618). Such a run postpones a refusal by a window; none
/// prevents it.
#[test]
fn a_judged_calls_own_work_is_its_progress() {
    #[track_caller]
    fn work(party: u32, ms: u64, idle: bool, forget: bool) -> (Result<(), String>, String) {
        if forget {
            let site = std::panic::Location::caller();
            let _ = std::fs::remove_file(common::WaitKind::kept_at(site, "", ""));
        }
        let party = common::Party::client(party, None);
        if idle {
            // Idle, it waits for its refusal, whose end of its wait closes the channel of its
            // judge's looks; told of more than LOOKS_TO_REFUSE due looks that found its party
            // still since it last ran, none refusing it, it answers. A count of the judge's own
            // looks, which a host holding them late cannot cut short; it keeps no clock, the CI
            // step's own limit its last bound.
            return common::judged_watching(party, "a call's own work", |looks| {
                while let Ok(still) = looks.recv() {
                    if still.len() > shards_api::LOOKS_TO_REFUSE as usize {
                        return;
                    }
                }
            });
        }
        common::judged_accounted(
            party,
            "a call's own work",
            || {},
            move || {
                let until = Instant::now() + Duration::from_millis(ms);
                let mut spun = 0u64;
                while Instant::now() < until {
                    spun = std::hint::black_box(spun.wrapping_add(1));
                }
            },
        )
    }
    let mut idle = stand_in_doing("rests")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Idle once it says so: before, its start is its threads running, or runnable on CPUs
    // the tests beside it keep busy, which is its party's progress or its host's hold. Begun
    // as it was spawned, a sleeper past the busy call's W* went unrefused on CI's arm64
    // Windows runner (run 44007524337), no account kept. Its word comes among libtest's own
    // lines ("test stand_in ... resting").
    let mut out = std::io::BufReader::new(idle.stdout.take().unwrap());
    let mut said = String::new();
    while !said.contains("resting") {
        said.clear();
        let read = std::io::BufRead::read_line(&mut out, &mut said).unwrap();
        assert_ne!(read, 0, "the stand-in ended without resting");
    }
    // Its party the idle stand-in alone, not every process this binary started: the tests
    // beside it run their own (4 of 4 whole binaries on HVF, run with libtest's threads,
    // "a call idle on its own thread not refused" while they worked).
    let party = idle.id();
    // At rest, too, before its calls begin: no thread of its process runnable. A process's
    // start may leave threads of its OS's beside its own (on Windows its loader's workers),
    // which a host whose CPUs the tests beside it keep busy can hold runnable on no CPU after
    // its own thread says it rests: its host's hold, which holds a wait on it unrefused. CI's
    // arm64 Windows runner left the sleeper unrefused in runs 44007524337, 44076016255 and
    // 44077430171, no account kept: its wait's account below names its members' runnable
    // threads should it be again (`shards_api::runnable_threads`, read on Windows too).
    let at_rest = || shards_api::runnable_threads(party).is_none_or(|(runnable, _)| runnable == 0);
    common::judged(
        common::Party::client(party, None),
        "its stand-in at rest",
        || {},
        || {
            while !at_rest() {
                std::thread::sleep(Duration::from_millis(1));
            }
        },
    )
    .unwrap();
    let call = |ms: u64, still: bool, forget: bool| work(party, ms, still, forget);
    let (quick, _) = call(0, false, true);
    let (busy, _) = call(200, false, false);
    let (idle_call, idle_account) = call(0, true, false);
    drop(idle.stdin.take());
    idle.wait().unwrap();
    assert!(quick.is_ok(), "{quick:?}");
    assert!(busy.is_ok(), "{busy:?}");
    // Refused, and its account names what its member was doing. Answered, its judge's looks
    // outlasted their rule: its account names the due looks that found its party still.
    let Err(said) = idle_call else {
        panic!(
            "a call idle on its own thread not refused through {} due looks that found its party still; its wait as it answered: {idle_account}",
            shards_api::LOOKS_TO_REFUSE + 1
        );
    };
    // Its member listed, none of its threads runnable: Linux says it sleeping (S), macOS live
    // (SRUN, which XNU keeps for every live process), both by its threads.
    let member = said
        .split_once(&format!("its members now: {party} "))
        .map(|(_, member)| member.split(';').next().unwrap_or_default());
    assert!(member.is_some_and(|member| member.contains(", 0 of ")), "{said}");
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
        // Begun before its spawn: the stand-in's busy clock starts as its connect returns,
        // which can be before accept returns here (CI run 43575280954, Alpine: "it answered
        // 999.714051ms in"), as a_judged_call_goes_on_while_its_party_works' can be before
        // its spawn returns.
        let began = Instant::now();
        let mut party = stand_in_doing(&format!("answer {port} {}", busy.as_millis()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let (mut conn, _) = listener.accept().unwrap();
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
/// in. Do: a stand-in busy for a second from its go, sent once a look of its wait has found it
/// running (as [`a_patient_wait_goes_on_while_its_party_works`]'), asked after until it has
/// ended, twice from one call site. Expect: each wait holds until it ends, and the first's end
/// teaches the second's kind.
#[test]
fn a_retry_goes_on_while_what_this_test_started_works() {
    let busy = Duration::from_secs(1);
    for round in 0..2 {
        let began = Instant::now();
        let mut anchor = stand_in_doing(&format!("go {}", busy.as_millis()))
            .stdin(std::process::Stdio::piped())
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
            drop(anchor.stdin.take());
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
/// CPU apart, the first 400 ms in, followed from before it frees and settled once it says it
/// freed the first, as a caller settles once its give-back has begun ([`common::Falling`]);
/// twice from one call site. Expect: each round's end held what was left once all four went,
/// and the second round's kind learned a give-back that long.
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
        assert!(
            lines.any(|l| l.unwrap().ends_with("freed")),
            "the stand-in freed nothing"
        );
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

/// A cold settle ends by what its looks found (`Falling::cold_over`, D209 Part 4): with
/// nothing fallen, at `LOOKS_TO_REFUSE` looks in a row that found its process still, and not
/// one before; with a fall, once none has come for as long as from its start to its last
/// fall, counted from the last look since that found its host holding the process, its still
/// looks no end of it. Red: lanes/redgreen/own-35g-falling-ends.patch, a settle with nothing
/// fallen never over, as at a5b586919a, whose cold-memory test waited 54 minutes on macOS.
#[test]
fn a_cold_settle_ends_by_what_its_looks_found() {
    let began = Instant::now();
    let at = |ms| began + Duration::from_millis(ms);
    let refuse = shards_api::LOOKS_TO_REFUSE;
    assert!(!common::Falling::cold_over(began, at(1), None, refuse - 1));
    assert!(
        common::Falling::cold_over(began, at(1), None, refuse),
        "a cold settle with nothing fallen not over at {refuse} still looks"
    );
    let fell = Some((at(400), at(400)));
    assert!(!common::Falling::cold_over(began, at(799), fell, refuse));
    assert!(common::Falling::cold_over(began, at(800), fell, 0));
    let held = Some((at(400), at(700)));
    assert!(!common::Falling::cold_over(began, at(1099), held, refuse));
    assert!(common::Falling::cold_over(began, at(1100), held, 0));
}

/// A give-back with nothing to fall is refused with its account, never followed for as long
/// as its process lives (ROOT's ruling, 2026-11-18, D209 Part 4: an idle party whose memory
/// is not falling is refused, as D215's judge refuses a still one). Do: a stand-in at rest,
/// freeing nothing, followed and settled twice from one call site, its kind made cold first.
/// Expect: each settle over with nothing fallen, and its kind taught nothing, so the second
/// is cold too.
#[test]
fn a_give_back_with_nothing_fallen_ends_and_teaches_nothing() {
    /// [`common::Falling::begin`] at its caller's call site, its kind made cold: what this host
    /// kept of it removed before the kind's first wait in this process.
    #[track_caller]
    fn cold_falling(pid: u32) -> common::Falling {
        let site = std::panic::Location::caller();
        let _ = std::fs::remove_file(common::WaitKind::kept_at(site, "", ""));
        common::Falling::begin(pid)
    }
    for round in 0..2 {
        let mut stand_in = stand_in_doing("rests")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stand_in.stdout.take().unwrap()));
        assert!(
            lines.any(|l| l.unwrap().ends_with("resting")),
            "the stand-in said nothing"
        );
        let held = shards_vmm::platform::process_usage(stand_in.id())
            .unwrap()
            .footprint;
        let falling = cold_falling(stand_in.id());
        assert_eq!(
            falling.seen(),
            shards_api::Seen::Cold,
            "round {round}: its kind learned of nothing fallen"
        );
        let ended = falling.settled();
        let _ = stand_in.kill();
        let _ = stand_in.wait();
        assert!(
            ended.saturating_add(common::REPORTED) > held,
            "round {round}: {held} bytes held, {ended} as it settled"
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

/// A wait's looks that its waiter's own polls made late are no hold of its host's (D209
/// Part 8): on adas-mac-studio-3.local, busy, the test above was "not refused 177-197 ms in
/// (W* 59.554375ms)", each `ps` poll and each look slower than when W* was taught, every
/// look so late past its due that the judge took the time for its host's hold of it, and
/// its party's window saw only the looks' own durations. Red so: a look that saw a whole
/// W* of its own polling past its first goes on. Do: start a daemon that runs no microVM;
/// teach a wait's kind its W* by a state found at its first look; then wait for a state that
/// never comes, polling between looks with a stand-in of this test's own busy a whole window
/// (W*, or its CPU counter's tick where that is longer: the judge refuses over no less than
/// its looks resolve). Expect: refused at the look after the first poll, which says so at the
/// next.
#[test]
fn a_wait_is_refused_though_its_own_polls_outlast_w_star() {
    let home = TempDir::new("outlast");
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_LOCAL_STORE", std::ffi::OsStr::new("none")),
    ];
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
    // The `ps` that started it took a slot, which its daemon replaces after it has gone.
    common::settled(&home);
    let mut taught = outlasted(&home);
    let _ = taught.holds();
    taught.done();
    let shards_api::Seen::Learned { slowest, .. } = outlasted(&home).seen() else {
        panic!("its kind learned nothing of an answer");
    };
    // Each poll outlasts the window its judge refuses over, as its looks so far say it: W*,
    // or a CPU counter's tick where that is longer (macOS' 10 ms against a W* of 1.4 ms on
    // HVF, where polls of W* and 1 ms were refused only as a poll's start happened to outlast
    // the tick, 12 of 20).
    let poll = |patience: &Patience| {
        let window = patience.window().unwrap_or(slowest).max(slowest);
        let busy = format!("busy {}", window.as_millis().saturating_add(1));
        let run = common::run_command(stand_in_doing(&busy), &["stand-in".to_string(), busy.clone()]);
        assert_eq!(run.status, Some(0), "{run}");
        assert!(
            run.elapsed > window,
            "a poll of {:?} within its window {window:?}",
            run.elapsed
        );
    };
    let mut patience = outlasted(&home);
    assert!(patience.holds(), "refused at its start");
    poll(&patience);
    // Its first look past W*: a refusal there is said at the next.
    let _ = patience.holds();
    poll(&patience);
    assert!(
        !patience.holds(),
        "a wait for a state that never comes, each poll its waiter's own and longer than its \
         window, not refused at its look past its first poll: {}",
        patience.account()
    );
}

/// [`Patience::of`] at one call site: one kind for every wait made through it.
fn outlasted(home: &std::path::Path) -> Patience {
    Patience::of(home)
}

/// A home's daemon has done the work its last client left it once its threads rest, not
/// once its request slots read as offered again (`common::settled`): CI 44007524337,
/// aarch64-apple-darwin, the test above, settled, found "its daemon ran" in its wait's first
/// window and never after. Red so: settled returns while its daemon still runs. Do: a home
/// whose slots read as offered (each list's records unread, and their names), its daemon a
/// stand-in named as a daemon names itself; once it says it spins, settle the home. Expect:
/// settled only once the stand-in has ended.
#[cfg(unix)]
#[test]
fn a_daemon_is_settled_once_its_threads_rest() {
    use shards_ipc::slots::{DIR, FREE, FREE_STREAMS, OFFERED, RECORD};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let home = TempDir::new("rest");
    let dir = home.join(DIR);
    std::fs::create_dir_all(&dir).unwrap();
    // Each list a FIFO this test holds both ends of, its records unread in it.
    let lists: Vec<(std::fs::File, std::fs::File)> = [FREE, FREE_STREAMS]
        .iter()
        .map(|list| {
            let path = dir.join(list);
            let named = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: mkfifo(2) of a NUL-terminated path this owns.
            assert_eq!(unsafe { libc::mkfifo(named.as_ptr(), 0o600) }, 0, "{path:?}");
            let open = |read: bool| {
                std::fs::OpenOptions::new()
                    .read(read)
                    .write(!read)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path)
                    .unwrap()
            };
            let reader = open(true);
            let mut writer = open(false);
            writer.write_all(&[0u8; OFFERED * RECORD]).unwrap();
            (reader, writer)
        })
        .collect();
    for n in 0..2 * OFFERED {
        std::fs::write(dir.join(format!("{n}.slot")), "").unwrap();
    }
    // Long past settled's first look, which comes at once.
    let mut daemon = stand_in_doing("spins 300")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = daemon.id();
    let named = format!("{pid} {}\n", shards_ipc::started(pid).unwrap());
    std::fs::write(home.join(shards_ipc::PID), named).unwrap();
    // Its word comes among libtest's own lines ("test stand_in ... spinning").
    let mut out = std::io::BufReader::new(daemon.stdout.as_mut().unwrap());
    let mut said = String::new();
    while !said.contains("spinning") {
        said.clear();
        let read = std::io::BufRead::read_line(&mut out, &mut said).unwrap();
        assert_ne!(read, 0, "the stand-in ended without spinning");
    }
    common::settled(&home);
    let ended = daemon.try_wait().unwrap().is_some();
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(ended, "settled while its daemon still ran");
    drop(lists);
}

/// A command's wait is keyed by its call site and its whole command line, what a run mints
/// of its own made generic (ROOT's ruling (a), 2026-11-13, #77): one call site's quick
/// refused command is kept apart from its real one, as build.rs' `builds_push_what_they_build`
/// runs `build --push CTX`, refused for want of a tag, and then `build --push -t
/// localhost:PORT/team/built:1 CTX`, which `shards build` and `buildx policy` words alone
/// never told apart; and a run's own directories and ports key no new record each run.
/// Do: from one call site, run a stand-in carrying the refused command's line and then the
/// real one's, each with a context directory and a registry port of its own; then both
/// again, with new ones. Expect: the four commands' records, named as their waits name them
/// (`common::WaitKind::kept_at`), two, the second round's the first's. The records this
/// host keeps are not counted: CI keeps them from run to run, and one an earlier build
/// keyed otherwise (CI 44282442232: a stand-in's, by its binary's name with cargo's hash in
/// it, before `{self}`) is an orphan no wait reads.
#[test]
fn a_call_sites_commands_are_keyed_by_their_whole_line() {
    #[track_caller]
    fn run_at(cmd: std::process::Command) -> std::path::PathBuf {
        let (words, line) = (common::command_words(&cmd), common::keyed_line(&cmd));
        let record = common::WaitKind::kept_at(std::panic::Location::caller(), &words, &line);
        let run = common::run_command(cmd, &["stand-in".to_string()]);
        assert_eq!(run.status, Some(0), "{run}");
        record
    }
    let mut rounds = Vec::new();
    for _ in 0..2 {
        let ctx = TempDir::new("build-push-ctx");
        let (port, _) = common::writable_registry();
        let ctx = ctx.to_str().unwrap();
        let name = format!("localhost:{port}/team/built:1");
        let records: Vec<std::path::PathBuf> = [
            vec!["build", "--push", ctx],
            vec!["build", "--push", "-t", &name, ctx],
        ]
        .into_iter()
        .map(|line| run_at(stand_in_with("exit", &line)))
        .collect();
        rounds.push(records);
    }
    assert_ne!(
        rounds[0][0], rounds[0][1],
        "a refused command keyed as its real one"
    );
    assert_eq!(rounds[0], rounds[1], "a run's own directory or port in its key");
}

/// A command's key carries nothing of its build (`common::command_words`,
/// `common::keyed_line`): a stand-in's program, this test binary, is `{self}`, the directory
/// shards' build script wrote (the init it made, the helpers it carries) `{out}`, and the
/// directory of this build's `shards` `{binaries}`, never their names, which carry cargo's
/// hash of the build or `shards`' own digest and change with them (CI 44282442232: testdir's
/// windows-sys changed the test binaries' hash on Windows alone, and each Windows job then
/// found the stand-ins' records under the name before beside its own). Do: key a stand-in
/// whose arguments name this binary, an init in that directory and the tests' `shards`.
/// Expect: none of their names' hashes in its words or its line.
#[test]
fn a_commands_key_carries_nothing_of_its_build() {
    let exe = std::env::current_exe().unwrap();
    let out = std::path::Path::new(env!("OUT_DIR"));
    let init = out.join("shards-init");
    let shards = common::shards();
    let cmd = stand_in_with(
        "exit",
        &[
            exe.to_str().unwrap(),
            init.to_str().unwrap(),
            shards.to_str().unwrap(),
        ],
    );
    let (words, line) = (common::command_words(&cmd), common::keyed_line(&cmd));
    let carried: Vec<String> = [
        exe.file_stem(),
        out.parent().and_then(std::path::Path::file_name),
        shards.parent().and_then(std::path::Path::file_name),
    ]
    .into_iter()
    .map(|name| name.unwrap().to_string_lossy().into_owned())
    .filter(|name| {
        let hash = name.rsplit_once('-').map_or(name.as_str(), |(_, hash)| hash);
        words.contains(hash) || line.contains(hash)
    })
    .collect();
    assert!(carried.is_empty(), "{carried:?} in its key: {words}: {line}");
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

/// The kind of its caller's call site ([`common::WaitKind::at`]) made cold: what this host
/// kept of it removed before its first wait in this process. A test that teaches its kind
/// W* by a quick stand-in's own end means that end's W*: a record an earlier run kept judged
/// the teaching wait itself, and refused it (ROOT's whole harness binary on HVF after
/// 9dd326609: `a_stopped_process_is_its_hosts_hold_and_not_refused`'s quick stand-in, still
/// for 3.72 ms past a kept W* of 2.10 ms, "Stall { waited: 10480583, over: 3718298, ..
/// slowest: 2096084 }").
#[track_caller]
fn cold_kind() -> &'static common::WaitKind {
    let site = std::panic::Location::caller();
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, "", ""));
    common::WaitKind::at(site)
}

/// [`Patience::of`] at its caller's call site, its kind made cold: what this host kept of it
/// removed before the kind's first wait in this process.
#[track_caller]
fn cold_patience(home: &std::path::Path) -> Patience {
    let site = std::panic::Location::caller();
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, common::STATE, ""));
    Patience::of(home)
}

/// [`cold_patience`] at one call site: one kind for every wait made through it.
fn one_state(home: &std::path::Path) -> Patience {
    cold_patience(home)
}

/// A wait its caller first looks at past its kind's W* refuses nothing at that look for what
/// it did not see: its party's work since its own first look, as it began (`Wait::on`), is
/// progress (#77 waits-02; ROOT's KVM refusals at c120c8a95, "its last progress its start",
/// 18.4 ms and 31.1 ms into waits of W* 3.527 µs and 14.0 ms). Red at 9efde7252, refused at
/// that look, as a party found anew was found still and its wait's start taken for a look,
/// no look made then. Do: teach a kind its W* by a quick
/// stand-in's end; follow a busy stand-in, then a sleeping one, each first looked at 2 W*
/// past its wait's start; then follow a sleeping one look after look. Expect: neither first
/// look refuses; the sleeper looked at on is refused, once its looks have seen a whole W*.
#[test]
fn a_first_look_past_w_star_judges_nothing() {
    let kind = cold_kind();
    let mut quick = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut taught = common::Wait::begin(kind, quick.id(), None, "a quick stand-in");
    taught.absent();
    while quick.try_wait().unwrap().is_none() {
        taught.look(false).unwrap();
    }
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = kind.record().seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let spawn = |what: &str| {
        stand_in_doing(what)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    for what in ["busy 60000", "pulse 1 600000"] {
        let mut party = spawn(what);
        let mut wait = common::Wait::begin(kind, party.id(), None, what);
        std::thread::sleep(slowest.saturating_mul(2));
        let first = wait.look(false);
        let _ = party.kill();
        let _ = party.wait();
        assert!(
            first.is_ok(),
            "{what}: refused at its first look: {}",
            wait.account()
        );
    }
    let mut sleeper = spawn("pulse 1 600000");
    let mut wait = common::Wait::begin(kind, sleeper.id(), None, "a sleeping stand-in");
    let began = Instant::now();
    let refused = loop {
        if wait.look(false).is_err() {
            break true;
        }
        // Its start's own work past, it sleeps: a whole W* seen still well within this.
        if began.elapsed() > slowest.saturating_mul(100).max(Duration::from_secs(10)) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert!(
        refused,
        "a sleeper looked at on never refused: {}",
        wait.account()
    );
}

/// A wait that finds its answer already there teaches its kind nothing: it waited on no
/// party (#77 waits-03: warm.rs' `until_ended`, `Vm::wait_lines`' lines already come and
/// engine.rs' reads of what its daemon had sent taught their kinds the time of one look,
/// which then refused waits that waited, engine.rs' "its daemon made no progress" among
/// them). Red at 9efde7252, its kind learned from a state there as its wait began. Do: a
/// state its home's daemon (a busy stand-in) is in already, told `done` at once; then one
/// that comes after its wait found it missing for 50 ms; then a stream read to an end its
/// peer had sent, and one its peer sends 50 ms after its reader's first read ([`Telling`]).
/// Expect: cold after each answer already there, and learned from each that was waited for
/// (the state's W* its wait's).
#[test]
fn an_answer_already_there_teaches_nothing() {
    let home = TempDir::new("there");
    let mut daemon = stand_in_doing("busy 60000")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    name_daemon(&home, daemon.id());
    one_state(&home).done();
    let there = one_state(&home).seen();
    let waited = Duration::from_millis(50);
    let mut patience = one_state(&home);
    let began = Instant::now();
    while began.elapsed() < waited {
        assert!(patience.holds(), "{}", patience.account());
        std::thread::sleep(Duration::from_millis(1));
    }
    patience.done();
    let learned = one_state(&home).seen();
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(
        there,
        shards_api::Seen::Cold,
        "a state already there taught its kind"
    );
    assert!(
        matches!(learned, shards_api::Seen::Learned { slowest, .. } if slowest >= waited),
        "a state waited for taught {learned:?}"
    );

    let mut listener = Some(std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap());
    let port = listener.as_ref().unwrap().local_addr().unwrap().port();
    for delay in [None, Some(waited)] {
        let (sent, told) = std::sync::mpsc::channel();
        let (reading, reads) = std::sync::mpsc::channel();
        let serving = listener.take().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut to, _) = serving.accept().unwrap();
            if let Some(delay) = delay
                && reads.recv().is_ok()
            {
                std::thread::sleep(delay);
            }
            to.write_all(b"x").unwrap();
            drop(to);
            sent.send(()).unwrap();
            serving
        });
        let stream = Telling::to(port, reading);
        if delay.is_none() {
            told.recv().unwrap();
        }
        let (mut following, kind) = one_stream(&stream);
        let mut read = Vec::new();
        std::io::Read::read_to_end(&mut following, &mut read).unwrap();
        assert_eq!(read, b"x");
        let seen = kind.record().seen();
        match delay {
            None => assert_eq!(
                seen,
                shards_api::Seen::Cold,
                "a stream ended already taught its kind"
            ),
            Some(_) => assert!(
                matches!(seen, shards_api::Seen::Learned { .. }),
                "a stream waited for taught nothing"
            ),
        }
        listener = Some(peer.join().unwrap());
    }
}

/// [`common::Following::of`] at one call site, its kind made cold as [`cold_patience`]'s is,
/// with that kind.
fn one_stream(stream: &Telling) -> (common::Following<'_, Telling>, &'static common::WaitKind) {
    cold_following(stream)
}

#[track_caller]
fn cold_following(stream: &Telling) -> (common::Following<'_, Telling>, &'static common::WaitKind) {
    let site = std::panic::Location::caller();
    let file = common::WaitKind::kept_at(site, common::STREAM, "");
    // Removed before the kind's first wait in this process alone: its record is in memory
    // from then on.
    static REMOVED: std::sync::Once = std::sync::Once::new();
    REMOVED.call_once(|| {
        let _ = std::fs::remove_file(&file);
    });
    (
        common::Following::of(stream, None),
        common::WaitKind::awaiting(site, common::STREAM),
    )
}

/// A stream to a peer of this test's, each of whose reads, once it has returned, is told on
/// `told`, so that its peer answers as its reader reads ([`moved_answer`],
/// [`an_answer_already_there_teaches_nothing`]): its peer, saying nothing before its first
/// read, has that read find nothing, and a quiet it keeps from it is one its reader's wait
/// saw from its start, however long the look its wait makes before it (`Following`'s first
/// read comes after its wait's first look). A clock of the peer's own did not do that: a
/// stream answered 5 ms after its accept was first read past its answer and its end on a
/// loaded host, its wait never found its answer missing, and it taught nothing (ROOT's run
/// on a loaded KVM host at 53366451ac, glibc: "the answer before the move taught nothing").
struct Telling {
    stream: std::net::TcpStream,
    told: std::sync::mpsc::Sender<()>,
}

impl Telling {
    /// A stream to loopback's `port`, its reads told on `told`.
    fn to(port: u16, told: std::sync::mpsc::Sender<()>) -> Telling {
        Telling {
            stream: std::net::TcpStream::connect(("127.0.0.1", port)).unwrap(),
            told,
        }
    }
}

impl common::Timed for Telling {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        common::Timed::set_read_timeout(&self.stream, timeout)
    }
    fn read_now(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = common::Timed::read_now(&self.stream, buf);
        let _ = self.told.send(());
        read
    }
}

impl std::io::Read for &Telling {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = std::io::Read::read(&mut &self.stream, buf);
        let _ = self.told.send(());
        read
    }
}

/// A guest's stream (`common::Following`) reads before each look, never judging an answer
/// that has come missing: red at cf13e49e6 on KVM (adacachy2), where a look's `ps` took 16
/// to 25 ms against a learned W* of 8 ms, `pause` left no time to read between looks, and
/// vsock.rs' bad handshake, whose close came at once, was refused 128 ms later unread, its
/// VM process idle waiting on the host. Do: a stream with nothing at its first read and its
/// answer, one byte and its end, at every read after; its kind taught a W* of a nanosecond,
/// its party none (no VM process of this test's own). Expect: the byte and the end read.
#[test]
fn a_streams_answer_that_has_come_is_read_before_a_look_judges_it() {
    let stream = Arriving::default();
    let mut read = Vec::new();
    let got = std::io::Read::read_to_end(&mut quick_following(&stream), &mut read);
    assert!(got.is_ok(), "an answer that had come refused: {got:?}");
    assert_eq!(read, b"x");
}

/// A stream whose first read finds nothing, and whose reads after find `x`, then its end.
#[derive(Default)]
struct Arriving {
    reads: std::cell::Cell<u32>,
}

impl common::Timed for Arriving {
    fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
    fn read_now(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut stream = self;
        std::io::Read::read(&mut stream, buf)
    }
}

impl std::io::Read for &Arriving {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.reads.get();
        self.reads.set(n + 1);
        match (n, buf.first_mut()) {
            (0, _) => Err(std::io::ErrorKind::WouldBlock.into()),
            (1, Some(b)) => {
                *b = b'x';
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}

/// [`common::Following::of`] at its caller's site, that kind made cold and then taught an
/// answer a nanosecond long, so that every look past its first is due.
#[track_caller]
fn quick_following(stream: &Arriving) -> common::Following<'_, Arriving> {
    let site = std::panic::Location::caller();
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, common::STREAM, ""));
    common::WaitKind::awaiting(site, common::STREAM)
        .record()
        .teach(Duration::from_nanos(1), Duration::ZERO);
    common::Following::of(stream, None)
}

/// A test's own VM processes (`Party::own`: `Patience::own`, `Patience::of`, a guest's
/// stream) are those its starts named (`common::OwnVm`), by their IDs, whatever their names:
/// red at a932bc894 on adacachy2, where they were found by the name `shards-vm` and a Linux
/// VM process started through its descriptor was named `5`, so a stream's party had none and
/// its wait was refused while its VM process worked. Do: a stand-in, named as this test binary
/// is, one of this test's own VM processes, busy for 100 ms and then for a second; wait for
/// each one's end from one call site, its kind cold before the first; all in a stand-in,
/// whose process nothing else runs in: a test's own VM processes are its process's, and a
/// busy one here was every other test's party's progress (D209 Part 8: harness.rs' two poll
/// tests not refused, "its last progress process 89935 ran", that stand-in's ID, 3 of 3 runs
/// of the binary on adas-mac-studio-3.local). Expect: each wait holds until it ends, the
/// second's past the W* the first taught.
#[test]
fn a_tests_own_vm_processes_are_known_by_their_ids() {
    let run = common::run_command(
        stand_in_doing("own-vms"),
        &["stand-in".to_string(), "own-vms".to_string()],
    );
    assert_eq!(run.status, Some(0), "{run}");
}

/// [`a_tests_own_vm_processes_are_known_by_their_ids`]'s stand-in. Each VM process is busy
/// from its go, sent once a look of its wait has found it running (as
/// [`a_patient_wait_goes_on_while_its_party_works`]'), so that the first's end teaches the
/// second's kind however late that look.
fn own_vms() {
    let mut taught: Option<Duration> = None;
    for (round, busy) in [Duration::from_millis(100), Duration::from_secs(1)]
        .into_iter()
        .enumerate()
    {
        let began = Instant::now();
        let mut vm = stand_in_doing(&format!("go {}", busy.as_millis()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let own = common::OwnVm::started(vm.id());
        let mut patience = own_patience();
        if let Some(first) = taught {
            assert!(
                matches!(patience.seen(), shards_api::Seen::Learned { slowest, .. } if slowest >= first),
                "round {round}: its kind learned no answer of its first round's {first:?}: {:?}",
                patience.seen()
            );
        }
        while vm.try_wait().unwrap().is_none() {
            if !patience.holds() {
                let _ = vm.kill();
                let _ = vm.wait();
                panic!(
                    "a VM process busy for {busy:?} cut {:?} into its wait, round {round}: {}",
                    began.elapsed(),
                    patience.account()
                );
            }
            drop(vm.stdin.take());
            std::thread::sleep(Duration::from_millis(5));
        }
        patience.done();
        drop(own);
        assert!(began.elapsed() >= busy, "it ended {:?} in", began.elapsed());
        taught = Some(busy);
    }
}

/// [`Patience::own`] at its caller's site, that kind made cold as [`cold_following`]'s is.
#[track_caller]
fn own_patience() -> Patience {
    let file = common::WaitKind::kept_at(std::panic::Location::caller(), common::STATE, "");
    static REMOVED: std::sync::Once = std::sync::Once::new();
    REMOVED.call_once(|| {
        let _ = std::fs::remove_file(&file);
    });
    Patience::own()
}

/// A guest's stream (`common::Following`: `vsock_connect`'s handshake, `echo`, vsock.rs'
/// greeting) follows its party's CPU as progress until its first byte, while its peer starts
/// it, and its bytes alone after (D209), as a streaming guest's wait does its first line
/// (`Heard::streaming`). Red at a932bc894 on KVM (adacachy2, Linux x86_64): its CPU read as
/// liveness from its start, a handshake learned at 9 ms was refused 10.7 ms into the next,
/// whose guest's vsock device and kernel were at work on its answer
/// (snapshot.rs' storm: "telling the guest it may finish: refused after \"\""), and vsock.rs'
/// greeting the same at 12.7 ms. Do: a home whose daemon is a stand-in that spins; through
/// one call site, teach its kind an answer 20 ms in; then follow a peer that sends a byte at
/// once and nothing more while the party spins; then one that sends its first byte a second
/// in. Expect: the first byte's stream refused while its party spins; the late one read whole,
/// held to its byte.
#[test]
fn a_streams_wait_follows_its_partys_work_until_its_first_byte() {
    use std::io::Read as _;
    let home = TempDir::new("starts");
    let daemon = Ended(
        stand_in_doing("busy 600000")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    name_daemon(&home, daemon.0.id());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let taught = Duration::from_millis(20);
    let late = Duration::from_secs(1);
    // Each round: when its peer sends its byte, and whether it closes then or holds its end
    // until the round's read has ended.
    for (round, (delay, closes)) in [(taught, true), (Duration::ZERO, false), (late, true)]
        .into_iter()
        .enumerate()
    {
        let (end, ended) = std::sync::mpsc::channel::<()>();
        let listener = &listener;
        let (read, took, kind) = std::thread::scope(|scope| {
            let peer = scope.spawn(move || {
                let (mut to, _) = listener.accept().unwrap();
                std::thread::sleep(delay);
                to.write_all(b"x").unwrap();
                if !closes {
                    let _ = ended.recv();
                }
            });
            let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let began = Instant::now();
            let (mut following, kind) = home_stream(&stream, &home);
            // The first round teaches its kind its answer, `taught` in: a read begun later
            // than its byte found it there as it began, which teaches nothing (waits-03),
            // as one did on adacachy2 with the whole binary running ("Cold").
            if round == 0 {
                kind.record().teach(taught, Duration::ZERO);
            }
            let mut got = Vec::new();
            let read = following.read_to_end(&mut got).map(|_| got);
            drop(end);
            peer.join().unwrap();
            (read, began.elapsed(), kind)
        });
        if closes {
            assert!(
                matches!(&read, Ok(got) if got == b"x"),
                "round {round}: a stream whose first byte came {delay:?} in, its party busy, ended {took:?} in with {read:?}"
            );
        } else {
            assert!(
                read.is_err(),
                "round {round}: a stream silent past its first byte while its party spun was never refused"
            );
        }
        if round == 0 {
            let seen = kind.record().seen();
            assert!(
                matches!(seen, shards_api::Seen::Learned { slowest, .. } if slowest < late),
                "its kind learned no answer below {late:?}: {seen:?}"
            );
        }
    }
}

/// A stand-in ended as it drops, the test's end or its failure.
struct Ended(std::process::Child);

impl Drop for Ended {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// [`common::Following::of_home`] at its caller's site, that kind made cold as
/// [`cold_following`]'s is, with the kind.
#[track_caller]
fn home_stream<'a>(
    stream: &'a std::net::TcpStream,
    home: &std::path::Path,
) -> (
    common::Following<'a, std::net::TcpStream>,
    &'static common::WaitKind,
) {
    let site = std::panic::Location::caller();
    let file = common::WaitKind::kept_at(site, common::STREAM, "");
    static REMOVED: std::sync::Once = std::sync::Once::new();
    REMOVED.call_once(|| {
        let _ = std::fs::remove_file(&file);
    });
    (
        common::Following::of_home(stream, home),
        common::WaitKind::awaiting(site, common::STREAM),
    )
}

/// A guest's stream read after its microVM moved, a restored guest's first answer, is judged
/// by its own kind, never by the W* its answer before the move taught (D209 Part 7). HVF at
/// 6f3cd4428 and KVM at 8fdd66512 and 3f2705f8d: moved.rs'
/// `a_moved_microvms_published_port_answers_there` refused its read after the move 70.9 ms
/// in, 34.9 ms without a byte, by the W* of 17.7 ms its read before the move taught, both
/// read from one closure's call site. Red with both read through one closure, as moved.rs'
/// were: refused. Do: from two call sites of one helper, read a stream answered 5 ms after
/// its first read, then one answered four times the W* the first taught after its first read
/// (a loaded host's first answer is slower than its 5 ms; [`Telling`]). Expect: the second
/// read to its answer, each kind taught by its own answer alone.
#[test]
fn a_moved_guests_first_answer_is_a_kind_of_its_own() {
    let (before, warm) = moved_answer(Duration::from_millis(5));
    let shards_api::Seen::Learned { slowest: warm, .. } = warm else {
        panic!("the answer before the move taught nothing");
    };
    let quiet = warm * 4;
    let (after, restored) = moved_answer(quiet);
    assert_eq!((before.as_slice(), after.as_slice()), (&b"x"[..], &b"x"[..]));
    assert!(
        matches!(restored, shards_api::Seen::Learned { slowest, .. } if slowest >= quiet),
        "the answer after the move taught {restored:?}"
    );
}

/// What a peer's stream says once it has said nothing for `after` from its reader's first
/// read ([`Telling`]), read through [`common::Following`] as moved.rs' `ask` reads a published
/// port: its kind its caller's call site, cold before its read, and what that kind has seen
/// once read.
#[track_caller]
fn moved_answer(after: Duration) -> (Vec<u8>, shards_api::Seen) {
    let site = std::panic::Location::caller();
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, common::STREAM, ""));
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (told, reads) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut to, _) = listener.accept().unwrap();
        if reads.recv().is_ok() {
            std::thread::sleep(after);
            to.write_all(b"x").unwrap();
        }
    });
    let stream = Telling::to(port, told);
    let mut read = Vec::new();
    let got = std::io::Read::read_to_end(&mut common::Following::of(&stream, None), &mut read);
    // Its telling ends with it: a peer told of no read ends unanswered.
    drop(stream);
    peer.join().unwrap();
    if let Err(refused) = got {
        panic!("refused {after:?} into its peer's quiet: {refused}");
    }
    (
        read,
        common::WaitKind::awaiting(site, common::STREAM).record().seen(),
    )
}

/// A home's `daemon.pid` names its daemon by its ID and its start (D185): a wait whose home
/// names a process that has its ID and not its start follows no daemon (#77 waits-09: the
/// tests' party read the ID alone, and followed whatever process had it since). Red at
/// 9efde7252, a busy process holding its wait. Do: name a busy stand-in by its ID and a
/// start that is not its own; wait on a state of that home. Expect: its party gone, the wait
/// ended while that process is still busy.
#[test]
fn a_daemon_named_by_another_start_is_no_party() {
    let home = TempDir::new("reused");
    let mut busy = stand_in_doing("busy 60000")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = shards_ipc::started(busy.id()).unwrap();
    std::fs::write(
        home.join("daemon.pid"),
        format!("{} {}\n", busy.id(), start.wrapping_add(1)),
    )
    .unwrap();
    let mut patience = Patience::of(&home);
    let ended = loop {
        if busy.try_wait().unwrap().is_some() {
            break false;
        }
        if !patience.holds() {
            break true;
        }
        std::thread::sleep(patience.next_look().min(Duration::from_millis(5)));
    };
    let _ = busy.kill();
    let _ = busy.wait();
    assert!(
        ended,
        "a process of another start followed as its daemon: {}",
        patience.account()
    );
}

/// A process that has exited and is not reaped is no live party of a wait on a state, whose
/// waiter reaps nothing: its end is under way at one look, and it has ended at the next (#77
/// waits-09: a zombie daemon's every look said its end under way, its waits' progress for
/// ever). Red at 9efde7252, holding past 10 s. Do: name a stand-in that has exited, not
/// reaped, its home's daemon; wait on a state of that home. Expect: its party gone, the
/// wait ended.
#[cfg(unix)]
#[test]
fn an_unreaped_daemon_is_gone() {
    let home = TempDir::new("zombie");
    let mut zombie = stand_in_doing("exit")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // SAFETY: waitid(2) of a child of this test's, WNOWAIT: it is left to reap.
    let exited = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        libc::waitid(
            libc::P_PID,
            zombie.id(),
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(exited, 0, "{}", std::io::Error::last_os_error());
    name_daemon(&home, zombie.id());
    let mut patience = Patience::of(&home);
    let began = Instant::now();
    let ended = loop {
        if !patience.holds() {
            break true;
        }
        if began.elapsed() > Duration::from_secs(10) {
            break false;
        }
        std::thread::sleep(patience.next_look().min(Duration::from_millis(5)));
    };
    let _ = zombie.wait();
    assert!(ended, "an exited daemon held its wait: {}", patience.account());
}

/// A failed run's VM's refusal is read from its error, or from its daemon's log by its VM's
/// pid (`common::run_refusal`, what `common::run_host_refused` gates as boot.rs's tests do):
/// where its client heard only that its guest stopped, the VM its daemon logged as unable to
/// serve the run in those words, and that VM's refusal, not another VM's the log holds. CI
/// 44189693085, x86_64 musl, daemon.rs `pools_keep_what_their_runs_need_while_they_come`: a
/// run ended 125, "the guest stopped before the command ended", read by "VM N ended" alone and
/// found no refusal. A stand-in log, as the daemon and a warm VM's own log now write it.
#[test]
fn a_failed_runs_vm_refusal_is_read_by_its_vms_pid() {
    let refusal = |pid: u32, when: &str| {
        format!(
            "vCPU 2: VM entry failed (hardware reason 0x7): the host's hypervisor (KVM under \
             KVMKVMKVM) refused to enter it ({when}, at 1794957533.463374 s, VM process {pid})"
        )
    };
    let unserved = "the guest stopped before the command ended: failed to fill whole buffer";
    let said = format!("shards: {unserved}\n");
    let log = format!(
        "shards daemon 7: container a1: its VM 206432 could not serve its run: {unserved}\n\
         [999 120us WARN] {}\n\
         [206432 949622us WARN] {}\n\
         shards daemon 7: VM 206432 ended with Ok(1)\n",
        refusal(999, "after its guest ran"),
        refusal(206432, "after its guest ran"),
    );
    let read = common::run_refusal(&said, &log);
    assert!(
        read.contains("VM process 206432") && !read.contains("VM process 999"),
        "{read}"
    );
    // One its error carries itself: that.
    let own = format!("shards: {}\n", refusal(206432, "before its guest ran"));
    assert_eq!(common::run_refusal(&own, ""), own);
    // A 125 whose VM's refusal its daemon's log holds none of: none, its failure its test's.
    let other = log.replace("its VM 206432", "its VM 31337");
    assert_eq!(common::run_refusal(&said, &other), "");
}

/// A stream no process writes any more is taken whole however late its reader thread runs
/// (`common::taken_whole`), and one a process still holds is taken as it came, its end not
/// waited for. CI 44164945050, x86_64 musl: `shards --version` ended with status 0 in 40 ms
/// and both its streams were said held open, their reader threads yet to hand over what their
/// pipes held, as its call site's kind, cold, refused its gone party a W* (its own run) past
/// its end. Red where what came is taken alone: nothing, its reader late. The reader is let go
/// a tenth of a second after the take begins, far past what the take itself does.
#[cfg(unix)]
#[test]
fn a_stream_whose_writers_are_gone_is_taken_whole_however_late_its_reader() {
    /// A reader that reads nothing until it is let go.
    struct Late {
        pipe: std::io::PipeReader,
        go: Option<std::sync::mpsc::Receiver<()>>,
    }
    impl std::io::Read for Late {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            if let Some(go) = self.go.take() {
                let _ = go.recv();
            }
            std::io::Read::read(&mut self.pipe, b)
        }
    }
    let said = b"shards version 0.8.0\n";
    let (reader, mut writer) = std::io::pipe().unwrap();
    let probe = common::probe(&reader);
    let (go, late) = std::sync::mpsc::channel();
    let rx = common::collect(Box::new(Late {
        pipe: reader,
        go: Some(late),
    }));
    writer.write_all(said).unwrap();
    let mut got = (Vec::new(), false);
    common::taken_whole(&rx, &probe, &mut got);
    assert_eq!(
        got,
        (Vec::new(), false),
        "a stream its writer holds was waited for"
    );
    drop(writer);
    let letting = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        go.send(()).unwrap();
    });
    common::taken_whole(&rx, &probe, &mut got);
    letting.join().unwrap();
    assert_eq!(got, (said.to_vec(), true));
}

/// A command's streams another process holds open past its end are read for as long as
/// that holder makes progress, judged from its end by the record its end taught, its kind
/// cold or not (#77 waits-10: a cold wait kept past its client's end followed a silent
/// holder's liveness, and could hold its test for ever). Red at 9efde7252, waiting out the
/// holder's 30 s. Do: from a call site whose kind is cold, run a stand-in that leaves a
/// sleeping holder of its stdout and stderr and exits. Expect: its run back, status 0, its
/// stdout said held open, long before the holder's end.
#[test]
fn streams_held_past_a_commands_end_are_judged_from_its_end() {
    let hold = Duration::from_secs(30);
    let began = Instant::now();
    let run = cold_run(stand_in_doing(&format!("leaves {}", hold.as_millis())));
    let elapsed = began.elapsed();
    let holder: Option<u32> = run
        .stdout
        .lines()
        .find_map(|line| line.split("holder ").nth(1)?.trim().parse().ok());
    if let Some(holder) = holder {
        #[cfg(unix)]
        // SAFETY: kill(2) of the holder this test's stand-in started.
        unsafe {
            libc::kill(i32::try_from(holder).unwrap(), libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &holder.to_string()])
            .output();
    }
    assert_eq!(run.status, Some(0), "{run}");
    assert!(holder.is_some(), "{run}");
    assert!(
        elapsed < hold,
        "its run came back {elapsed:?} in, at its holder's end: {run}"
    );
    assert!(run.stdout.contains("still held open"), "{run}");
}

/// A command's end found by its run's first check, before any look at its party, is its
/// kind's answer all the same: its run started its client as its wait began, so that end
/// was never there before it. adacachy2 (KVM) at f385b8dac: with the whole binary running,
/// [`streams_held_past_a_commands_end_are_judged_from_its_end`] waited out its holder's 30 s
/// in 7 of 30 runs, its client's end found at that first check, its kind left cold, and its
/// held streams judged by nothing. Red at f385b8dac, waiting out the holder's 30 s. Do: from
/// a call site whose kind is cold, run a client that has ended by the time its spawn returns,
/// having forked the holder of its streams (`pulse 1 30000`) and said "holder PID". Expect:
/// its run back, status 0, its stdout said held open, long before the holder's end.
#[cfg(unix)]
#[test]
fn a_commands_end_found_before_its_first_look_teaches_its_kind() {
    use std::os::unix::process::CommandExt as _;
    let hold = Duration::from_secs(30);
    let mut cmd = stand_in_doing(&format!("pulse 1 {}", hold.as_millis()));
    // The client forks the holder, which goes on to exec, says the holder's ID on its
    // stdout (set up before this runs) and exits. Its spawn returns once every end of its
    // exec-status pipe has closed, the client's at its exit among them.
    // SAFETY: fork(2), write(2) of a stack buffer and _exit(2), each async-signal-safe.
    unsafe {
        cmd.pre_exec(|| match libc::fork() {
            -1 => Err(std::io::Error::last_os_error()),
            0 => Ok(()),
            holder => {
                let mut said = *b"holder 0000000000\n";
                let mut id = holder.unsigned_abs();
                for digit in said[7..17].iter_mut().rev() {
                    *digit = b'0' + u8::try_from(id % 10).unwrap_or(0);
                    id /= 10;
                }
                libc::write(1, said.as_ptr().cast(), said.len());
                libc::_exit(0)
            }
        });
    }
    let began = Instant::now();
    let run = cold_run(cmd);
    let elapsed = began.elapsed();
    // The client's line is one write(2) of 18 bytes to a pipe, which arrives whole (pipe(7):
    // a write of at most PIPE_BUF bytes is atomic), but not always first: the holder may exec
    // and its libtest say "test stand_in ... " before it, on its line, while the client waits
    // for a CPU (CI 44227612442, x86_64 musl: "test stand_in ... holder 0000184475").
    let holder: Option<i32> = run
        .stdout
        .lines()
        .find_map(|line| line.split_once("holder ")?.1.trim().parse().ok());
    if let Some(holder) = holder {
        // SAFETY: kill(2) of the holder this test's client forked.
        unsafe {
            libc::kill(holder, libc::SIGKILL);
        }
    }
    assert_eq!(run.status, Some(0), "{run}");
    assert!(holder.is_some(), "{run}");
    assert!(
        elapsed < hold,
        "its run came back {elapsed:?} in, at its holder's end: {run}"
    );
    assert!(run.stdout.contains("still held open"), "{run}");
}

/// [`common::run_command`] at its caller's call site, its kind made cold.
#[track_caller]
fn cold_run(cmd: std::process::Command) -> common::Run {
    let site = std::panic::Location::caller();
    let file = common::WaitKind::kept_at(site, &common::command_words(&cmd), &common::keyed_line(&cmd));
    let _ = std::fs::remove_file(file);
    common::run_command(cmd, &["stand-in".to_string()])
}

/// A test that fails, whose cleanup's command its wait refuses as it unwinds.
#[test]
#[ignore = "a process of its own, which a_cleanup_refused_as_its_test_fails_fails_it_alone runs"]
fn a_cleanup_refused_as_its_test_fails() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = cleanup("pulse 1 600000");
        }
    }
    let taught = cleanup("pulse 1 0");
    assert_eq!(taught.status, Some(0), "{taught}");
    let _cleanup = Cleanup;
    panic!("its test fails");
}

/// A cleanup's command, at one call site.
fn cleanup(what: &str) -> common::Run {
    common::run_command(stand_in_doing(what), &["stand-in".to_string(), what.to_string()])
}

/// A cleanup whose command's wait is refused as its test unwinds fails that test alone: it
/// never panics while panicking, which aborts the whole test binary (#77: one of context.rs'
/// did). Red at 9efde7252, the binary aborted (SIGABRT). Do: run a test that teaches its
/// cleanup's kind W*, then fails, its cleanup running a sleeper its wait refuses. Expect: the
/// test failed as a test fails (libtest's 101), its cleanup's refusal said.
#[test]
fn a_cleanup_refused_as_its_test_fails_fails_it_alone() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "a_cleanup_refused_as_its_test_fails",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{:?}\n{said}", out.status);
    assert!(said.contains("as its test unwound"), "{said}");
}

/// Two keeps of one kind's record merge whatever their order (waits-11): each reads what was
/// kept, merges its answer and renames it into place under the record's lock, so that no
/// other keep's merge lands between one's read and its rename. Here a kind that learned 10 ms
/// keeps while one that learned 100 ms tries to keep between its read and its rename: the
/// second is held off, keeps after it, and the record kept is 100 ms. Red with no lock: the
/// second kept 100 ms there, and the first then renamed its 10 ms over it.
#[test]
fn concurrent_keeps_never_shrink_a_kept_record() {
    let home = TempDir::new("keeps");
    let file = home.join("record");
    let kind = |ms: u64| {
        let kind = common::WaitKind::apart(file.clone());
        kind.record().teach(Duration::from_millis(ms), Duration::ZERO);
        kind
    };
    let (short, long) = (kind(10), kind(100));
    let kept_long = std::cell::Cell::new(None);
    short
        .keep_between(true, || {
            kept_long.set(Some(long.keep_between(false, || {}).unwrap()))
        })
        .unwrap();
    assert_eq!(kept_long.get(), Some(false), "a keep landed inside another's");
    long.keep();
    let kept = common::WaitKind::apart(file.clone());
    kept.keep();
    let shards_api::Seen::Learned { slowest, .. } = kept.record().seen() else {
        panic!("nothing kept");
    };
    assert_eq!(slowest, Duration::from_millis(100), "the slowest kept shrank");
}

/// [`common::Heard::console`] at its caller's call site, waiting for `awaited`, its kind made
/// cold: what this host kept of it removed before the wait begins.
#[track_caller]
fn cold_console(awaited: &str) -> common::Heard<'static> {
    let site = std::panic::Location::caller();
    let line = common::guest_text(awaited);
    let _ = std::fs::remove_file(common::WaitKind::kept_at(site, common::PROGRESS, &line));
    common::Heard::console(awaited)
}

/// [`cold_console`] at one call site: one call site for every stage waited for through it, as
/// hotplug.rs' `ask` waits for each of its steps.
fn one_stage(awaited: &str) -> common::Heard<'static> {
    cold_console(awaited)
}

/// A wait for a guest's line is keyed by what it awaits beside its call site, its numbers and
/// what the run minted said generically (`common::guest_text`): a stage's step, address,
/// size or salt says no other work, and one temporary directory's path another's. Do: key
/// hotplug.rs' steps' texts. Expect: a disk's step at another address and step the same; a
/// grow's apart; a path minted by the run said as its placeholder.
#[test]
fn a_guest_waits_text_is_keyed_without_its_numbers() {
    let disk = common::guest_text("disk 00:02.0 20743 => SHARDS-PLUG WAIT 2");
    assert_eq!(disk, "disk {n}:{n}.{n} {n} => SHARDS-PLUG WAIT {n}");
    assert_eq!(
        disk,
        common::guest_text("disk 00:03.0 20743 => SHARDS-PLUG WAIT 15")
    );
    assert_ne!(disk, common::guest_text("grow vda 4096 => SHARDS-PLUG WAIT 1"));
    let dir = TempDir::new("stage");
    let said = common::guest_text(&format!("ls {} => DONE", dir.display()));
    assert!(!said.contains(&dir.display().to_string()), "{said}");
}

/// One call site's waits for different stages of a machine's guest are judged apart: a quick
/// stage's answer never bounds a slower one's (#77, ROOT's KVM run c33 at 5ded5b857, on
/// adacachy2, Linux x86_64: hotplug.rs' `ask` waited for `SHARDS-PLUG WAIT 2`, a rescan and a
/// disk's first read, at the W* of `WAIT 1`, a grow, 1.056 ms, and refused it 1.09 ms into its
/// quiet). Red with the kind its call site's alone, refused at the second stage's first
/// judged look. Do: in a stand-in, whose process nothing else runs in, from one call site
/// wait for a grow's answer, answered at once, which teaches its kind W*; then for a disk's,
/// answered after 50 ms of nothing said and nothing beside its waiter run. Expect: the second
/// waited on to its answer, its kind cold, the stand-in's status 0.
#[test]
fn one_call_sites_guest_stages_are_judged_apart() {
    let run = common::run_command(
        stand_in_doing("stages"),
        &["stand-in".to_string(), "stages".to_string()],
    );
    assert_eq!(run.status, Some(0), "{run}");
}

/// The quiet of [`stages`]' second stage: many times the first's W*, a look at a stand-in's
/// party, well within a test's run.
const QUIET: Duration = Duration::from_millis(50);

/// [`one_call_sites_guest_stages_are_judged_apart`]'s stand-in.
fn stages() {
    for (awaited, quiet) in [
        ("grow vda 4096 => SHARDS-PLUG WAIT 1", Duration::ZERO),
        ("disk 00:02.0 20743 => SHARDS-PLUG WAIT 2", QUIET),
    ] {
        let mut heard = one_stage(awaited);
        let began = Instant::now();
        loop {
            if let Err(refused) = heard.look(false) {
                panic!("{awaited}: refused {:?} in: {refused}", began.elapsed());
            }
            if began.elapsed() >= quiet {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        heard.answered();
        assert!(
            matches!(heard.seen(), shards_api::Seen::Learned { .. }),
            "{awaited}: its answer taught nothing"
        );
    }
}

/// A machine of the test's own process progresses while its threads beside its waiter run:
/// its vCPUs, its devices, and the host's work its guest waits on (#77, ROOT's KVM run c33:
/// hotplug.rs' party its console's words alone, refused while its guest rescanned and read a
/// disk the VMM served, saying nothing). Red with `Party::machine` reading no CPU, refused at
/// the second wait's first judged look. Do: from one call site wait for one stage twice: once
/// answered at once, which teaches its kind W*; then answered once a thread of this process
/// has spun for 200 ms, its waiter looking each millisecond and its console saying nothing.
/// Expect: the second waited on to its answer.
#[test]
fn a_machines_host_work_is_its_progress() {
    let work = Duration::from_millis(200);
    for round in 0..2 {
        let mut heard = one_stage("plug 00:02.0 => SHARDS-PLUG WAIT 1");
        heard.look(false).unwrap();
        if round == 0 {
            heard.answered();
            continue;
        }
        let began = Instant::now();
        let host = std::thread::spawn(move || {
            let mut spun = 0u64;
            while began.elapsed() < work {
                spun = std::hint::black_box(spun.wrapping_add(1));
            }
        });
        while !host.is_finished() {
            if let Err(refused) = heard.look(false) {
                panic!(
                    "refused {:?} into its host's {work:?} of work: {refused}",
                    began.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        host.join().unwrap();
        heard.answered();
    }
}

/// A machine's waiter's own run is no progress of its machine's (D209: a wait's own polls are
/// none of its party's): a waiter that spins on its looks, nothing else of its process
/// running, is refused once a whole W* passed. Red with its waiter's own CPU counted, its
/// party progressing for as long as its waiter looked. Do: in a stand-in, whose process
/// nothing else runs in, teach a stage's kind W* by an answer at once; then wait for it again,
/// looking without a pause. Expect: refused within 100 W* or 10 s, whichever is longer; the
/// stand-in's status 0.
#[test]
fn a_machines_waiter_alone_is_no_progress() {
    let run = common::run_command(
        stand_in_doing("spinner"),
        &["stand-in".to_string(), "spinner".to_string()],
    );
    assert_eq!(run.status, Some(0), "{run}");
}

/// [`a_machines_waiter_alone_is_no_progress`]'s stand-in.
fn spinner() {
    let mut taught = one_stage("remove 00:02.0 => SHARDS-PLUG WAIT 3");
    taught.look(false).unwrap();
    taught.answered();
    let shards_api::Seen::Learned { slowest, .. } = taught.seen() else {
        panic!("its kind learned nothing of an answer");
    };
    let mut heard = one_stage("remove 00:02.0 => SHARDS-PLUG WAIT 3");
    let began = Instant::now();
    while heard.look(false).is_ok() {
        assert!(
            began.elapsed() <= slowest.saturating_mul(100).max(Duration::from_secs(10)),
            "a waiter spinning alone never refused (W* {slowest:?})"
        );
    }
}

/// A command its home sends through a context to another home's daemon is served by that
/// daemon, which joins its party (ROOT's word, 2026-11-18, D215): context.rs's
/// `a_context_runs_commands_on_its_machine_whole` was refused on CI's x86_64 Windows (run
/// 44189693085) with its party its client alone, "no daemon yet" in its own home, while the
/// machine's daemon served it. The context is found as shards chooses it, and its daemon as
/// the home beside it that listens on its record's address. Do: a context record naming one
/// home's address, a third home listening elsewhere. Expect: the listening home found by
/// `--context`, by `SHARDS_CONTEXT` and by the home's current context; none for `default`.
/// Red where no serving home is found.
#[test]
fn a_contexts_command_is_served_by_the_home_that_listens_on_its_address() {
    let laptop = TempDir::new("ctx-serves");
    let machine = TempDir::new("ctx-served");
    let elsewhere = TempDir::new("ctx-elsewhere");
    // Ports below every OS's ephemeral range: no daemon of a test beside this one binds them.
    std::fs::write(machine.join("listening"), "127.0.0.1:4711\n").unwrap();
    std::fs::write(elsewhere.join("listening"), "127.0.0.1:4712\n").unwrap();
    std::fs::create_dir_all(laptop.join("contexts")).unwrap();
    std::fs::write(
        laptop.join("contexts").join("gpu"),
        format!(
            "shards context 1\nname gpu\naddress 127.0.0.1:4711\ndaemon {}\ndescription \n",
            "ab".repeat(32)
        ),
    )
    .unwrap();
    let home: &std::path::Path = &laptop;
    let served = |cmd: &std::process::Command| common::serving_home(cmd, Some(home));
    let machine = machine.to_path_buf();
    let mut by_flag = std::process::Command::new("shards");
    by_flag.args(["--context", "gpu", "ps"]);
    assert_eq!(served(&by_flag), Some(machine.clone()), "by --context");
    let mut by_env = std::process::Command::new("shards");
    by_env.arg("images").env("SHARDS_CONTEXT", "gpu");
    assert_eq!(served(&by_env), Some(machine.clone()), "by SHARDS_CONTEXT");
    let mut here = std::process::Command::new("shards");
    here.arg("images").env("SHARDS_CONTEXT", "default");
    assert_eq!(served(&here), None, "the default context runs here");
    std::fs::write(laptop.join("context"), "gpu\n").unwrap();
    let mut current = std::process::Command::new("shards");
    current
        .arg("ps")
        .env_remove("SHARDS_CONTEXT")
        .env_remove("DOCKER_CONTEXT");
    assert_eq!(served(&current), Some(machine), "by its home's current context");
}

/// A context's own command runs here whatever the context, and no other home's daemon serves
/// it but the join of `create context NAME --invite-file FILE`, which the daemon at its
/// invitation's address serves, so that daemon joins its party (D215; CI 44347282760,
/// windows-2025: context.rs's `a_build_through_a_context_is_of_the_clients_context_kept_in_step`
/// refused its `create context`, its party its client alone, "no daemon yet" in its own home,
/// while the machine's daemon served the join). Do: a laptop home whose current context
/// names one listening home, an invitation naming another, and one naming an address no home
/// listens on. Expect: the invited home for `create context` and `context create`, its flags
/// in either order, its file named from the command's directory; none for the invitation to
/// no home, nor for the context's other own commands, a context chosen; the chosen context's
/// home for any other command. Red where the join's daemon is not found
/// (`docs/research/measurements/harness-party-members-v1/red-invite.patch`), or where a
/// context's own command is sent through the chosen context (`red-kept-here.patch` there).
#[test]
fn a_join_is_served_by_the_home_its_invitation_names() {
    let laptop = TempDir::new("join-serves");
    let current = TempDir::new("join-current");
    let inviting = TempDir::new("join-inviting");
    // Ports below every OS's ephemeral range, and none the test beside this one names: no
    // daemon of a test binds them.
    std::fs::write(current.join("listening"), "127.0.0.1:4713\n").unwrap();
    std::fs::write(inviting.join("listening"), "127.0.0.1:4714\n").unwrap();
    std::fs::create_dir_all(laptop.join("contexts")).unwrap();
    std::fs::write(
        laptop.join("contexts").join("gpu"),
        format!(
            "shards context 1\nname gpu\naddress 127.0.0.1:4713\ndaemon {}\ndescription \n",
            "ab".repeat(32)
        ),
    )
    .unwrap();
    std::fs::write(laptop.join("context"), "gpu\n").unwrap();
    let invitation = |address: &str| {
        format!(
            "shards invitation 1\nname laptop\naddress {address}\ndaemon {}\nsecret {}\n",
            "ab".repeat(32),
            "cd".repeat(32)
        )
    };
    std::fs::write(inviting.join("laptop.invite"), invitation("127.0.0.1:4714")).unwrap();
    std::fs::write(inviting.join("nowhere.invite"), invitation("127.0.0.1:4715")).unwrap();
    let home: &std::path::Path = &laptop;
    let served = |cmd: &std::process::Command| common::serving_home(cmd, Some(home));
    let shards = |words: &[&str]| {
        let mut cmd = std::process::Command::new("shards");
        cmd.args(words)
            .env_remove("SHARDS_CONTEXT")
            .env_remove("DOCKER_CONTEXT");
        cmd
    };
    let invite = inviting.join("laptop.invite");
    let invite = invite.to_str().unwrap();
    let invited = Some(inviting.to_path_buf());
    let create = shards(&["create", "context", "lab", "--invite-file", invite]);
    assert_eq!(served(&create), invited, "create context");
    let docker = shards(&[
        "context",
        "create",
        "lab",
        "--description",
        "a lab",
        "--invite-file",
        invite,
    ]);
    assert_eq!(
        served(&docker),
        invited,
        "context create, its flags in another order"
    );
    let mut relative = shards(&["create", "context", "lab", "--invite-file", "laptop.invite"]);
    relative.current_dir(&*inviting);
    assert_eq!(served(&relative), invited, "its file named from its directory");
    let nowhere = inviting.join("nowhere.invite");
    let nowhere = shards(&[
        "create",
        "context",
        "lab",
        "--invite-file",
        nowhere.to_str().unwrap(),
    ]);
    assert_eq!(
        served(&nowhere),
        None,
        "an invitation to an address no home listens on"
    );
    for own in [
        &["use", "context", "gpu"][..],
        &["ls", "contexts"],
        &["context", "show"],
        &["invite", "client", "x"],
    ] {
        assert_eq!(served(&shards(own)), None, "{own:?} runs here");
    }
    let chosen = shards(&["ps"]);
    assert_eq!(
        served(&chosen),
        Some(current.to_path_buf()),
        "in the chosen context"
    );
}

/// A wait on a guest's progress through a client of a home's daemon (`Vm::spawn_env` naming
/// `SHARDS_HOME`: `exec`, `run vm`) follows that home's daemon and what descends from it, its
/// microVMs among them, as `run_command`'s wait on a command does (D215; CI 44347282760,
/// windows-2025: plug.rs's `a_host_edit_wakes_a_watch_in_the_guest` refused its `exec`'s wait,
/// its party its client alone, waiting on its pipes, while its home's daemon and microVM
/// served it). Do: in a stand-in, whose own VM processes (`common::OwnVm`, every `Vm`'s) no
/// other test's party counts, a client naming a home and one naming none. Expect: the first's
/// waits, running and streamed, say that home in their party; the second's name no home. Red
/// where a `Vm`'s waits drop its home
/// (`docs/research/measurements/harness-party-members-v1/red-vm-home.patch`).
#[test]
fn a_guests_wait_through_a_homes_client_follows_its_daemon() {
    let run = common::run_command(
        stand_in_doing("vm-home"),
        &["stand-in".to_string(), "vm-home".to_string()],
    );
    assert_eq!(run.status, Some(0), "{run}");
}

/// [`a_guests_wait_through_a_homes_client_follows_its_daemon`]'s stand-in.
fn vm_home() {
    let home = TempDir::new("vm-home");
    let named = format!("no daemon yet in {}", home.display());
    let mut client = common::Vm::spawn_env(&["--help"], &[("SHARDS_HOME", home.to_str().unwrap())]);
    assert_eq!(client.home(), Some(&*home), "its command's home");
    let running = client.running().account();
    assert!(running.contains(&named), "running: {running}");
    let streamed = client.streaming(None).account();
    assert!(streamed.contains(&named), "streamed: {streamed}");
    let mut alone = common::Vm::spawn(&["--help"]);
    assert_eq!(alone.home(), None, "a command that names no home");
    let own = alone.running().account();
    assert!(!own.contains("no daemon yet in"), "no home: {own}");
    client.kill();
    alone.kill();
}

/// A party follows its daemon's VM processes, and what descends from them, as members of
/// their own, whose run is its progress, and the daemon's other descendants (its spawner, its
/// network processes) as its helpers, whose run is the daemon's, on every host alike (D215;
/// ROOT's ruling, 2026-11-18). On Windows a daemon was followed with its whole tree: its VM
/// processes' run counted as the daemon's, which a window in which the test polled its daemon
/// does not count, so a polling test never saw its microVMs' work (CI 44227612442,
/// windows-2025: build.rs `an_images_healthcheck_probes_its_microvm` refused while three
/// health probes ran). A stand-in listing of the system's processes, no process of its own.
#[test]
fn a_partys_vm_processes_are_members_of_their_own() {
    let vm = if cfg!(windows) {
        "shards-vm.exe"
    } else {
        "shards-vm"
    };
    let listed: Vec<(u32, u32, String)> = [
        (100, 1, "shards"),
        (101, 100, "shards"),
        (102, 100, vm),
        (103, 100, "shards-net"),
        (104, 102, "child"),
        (200, 1, "shards"),
        (201, 200, vm),
        (300, 1, "other"),
        (301, 300, vm),
    ]
    .into_iter()
    .map(|(pid, ppid, name)| (pid, ppid, name.to_string()))
    .collect();
    let tree = common::party_tree(&listed, vec![200, 100], &[100]);
    let mut members: Vec<u32> = tree.iter().map(|(pid, _)| *pid).collect();
    members.sort_unstable();
    assert_eq!(members, [100, 101, 102, 103, 104, 200, 201], "{tree:?}");
    let mut helpers: Vec<u32> = tree
        .iter()
        .filter(|(_, helps)| *helps)
        .map(|(pid, _)| *pid)
        .collect();
    helpers.sort_unstable();
    assert_eq!(helpers, [101, 103], "{tree:?}");
}
