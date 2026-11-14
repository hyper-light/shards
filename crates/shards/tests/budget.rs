//! FUSE reads under a memory budget, timed without the daemon suite beside them (D188).
//! Cargo runs test binaries one after another, so these sequential phases compare the
//! budget's effect without other daemon tests competing for the host between them.

#![cfg(target_os = "linux")]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::{
    Patience, TempDir, cannot_pool, cannot_run_vms, home_with, processes_with, run_shards_env, served,
};
use shards_vmm::devices::virtio::fs::headroom::reads_room;

/// The file every phase reads, a gigabyte, and its reader's block, a MiB (as large as a
/// share's largest request).
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// A microVM held to a budget reads through FUSE at the pace reads held to none keep
/// (D188): under a slice of the user's manager whose `MemoryHigh` its idle microVMs leave
/// 128 MiB of, with no swap, runs restored from its template read a gigabyte of a volume
/// through FUSE (no DAX window), each held within the room the budget leaves it, so that
/// its own kernel drops its page cache. Its guest's page cache had filled the slice, and its
/// host took back the only memory it could, the guest's own RAM, its template's file pages:
/// 400 to 600 s a read on CI's AMD runners, 3 to 5 s with none (PM M308, M309). A budget set
/// as a run goes holds it too; and one short of what a run alone in the slice holds holds it
/// within the least room a run goes on in, which reads as fast. Each read is followed by the
/// marks its guest prints as it reads and judged by the reads held to none before it
/// ([`follow`], D215's shape: no fixed bound, no multiple of one): past the slowest of them,
/// a read goes on while each window as long reads what their slowest MiB keeps over it, and
/// is refused, typed, at the first that does not. The first read, with nothing seen before
/// it, follows its microVM's liveness alone.
// Linux's user systemd manager and cgroup budget; other hosts exercise shards' groups.
#[cfg(unix)]
#[test]
fn a_microvm_held_to_a_budget_reads_through_fuse_at_the_pace_of_reads_held_to_none() {
    if !cfg!(target_os = "linux") || cannot_run_vms() || cannot_pool() {
        return;
    }
    let bus = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        // SAFETY: getuid(2) takes nothing and cannot fail.
        .unwrap_or_else(|| std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
        .join("bus");
    if !bus.exists() {
        assert!(
            !std::env::var_os("SHARDS_REQUIRE_USER_MANAGER").is_some_and(|v| v == "1"),
            "no user's systemd manager's bus at {}",
            bus.display()
        );
        eprintln!("SKIP: no user's systemd manager here: {} is not", bus.display());
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-budget-fuse");
    let vol = TempDir::new("budget-fuse-vol");
    std::fs::File::create(vol.join("big"))
        .unwrap()
        .set_len(GIB)
        .unwrap();
    let env: [(&str, &OsStr); 2] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_SHARE_WINDOW_MIB", "0".as_ref()),
    ];
    let slice = format!("shardsbudget{}.slice", std::process::id());
    /// The slice's budget taken off and the slice stopped, however the test ends.
    struct Stopped<'a>(&'a str);
    impl Drop for Stopped<'_> {
        fn drop(&mut self) {
            for verb in ["revert", "stop"] {
                let _ = Command::new("systemctl").args(["--user", verb, self.0]).status();
            }
        }
    }
    let _stopped = Stopped(&slice);
    let said = || std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let cgroup = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/{slice}",
        // SAFETY: getuid(2) takes nothing and cannot fail.
        uid = unsafe { libc::getuid() }
    ));
    // What the slice holds but its page cache no process maps, which its kernel takes back
    // first: the files' own, the host's of the volume's reads among it.
    let slice_holds = || -> u64 {
        let current: u64 = std::fs::read_to_string(cgroup.join("memory.current"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let stat = std::fs::read_to_string(cgroup.join("memory.stat")).unwrap();
        let of = |key: &str| -> u64 {
            stat.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
                .unwrap_or(0)
        };
        current.saturating_sub(
            of("file")
                .saturating_sub(of("file_mapped"))
                .saturating_sub(of("shmem")),
        )
    };
    let budget = |high: &str| {
        let set = Command::new("systemctl")
            .args(["--user", "set-property", "--runtime", &slice])
            .arg(format!("MemoryHigh={high}"))
            .arg("MemorySwapMax=0")
            .output()
            .unwrap();
        assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));
    };
    let mount = format!("{}:/vol", vol.display());
    // Each read followed and judged by the record of the reads held to none so far
    // ([`follow`]): refused, or ended without the whole gigabyte, it fails the test, with
    // the slice's state and the daemon's log. Its milliseconds, as its guest says them.
    let judged = |what: &str, args: &[&str], record: &Record, env: &[(&str, &OsStr)]| -> Followed {
        let read = follow(args, env, &home, record, &cgroup).unwrap_or_else(|stalled| {
            panic!(
                "{what}: refused, {stalled}\n{}\nthe daemon's log:\n{}",
                slice_state(&cgroup),
                said()
            )
        });
        assert!(
            read.status == Some(0) && read.took().is_some(),
            "{what}: exit {:?}, no whole read in {:?}\n{}\nthe daemon's log:\n{}",
            read.status,
            read.stdout,
            read.stderr,
            said()
        );
        read
    };
    let run_read: [&str; 9] = [
        "run",
        "--rm",
        "--cgroup-parent",
        &slice,
        "-v",
        &mount,
        &image,
        "read-marked",
        "/vol/big",
    ];
    // The image's template saved under the slice, and its pool warm there.
    let first = run_shards_env(
        &["run"],
        &[
            "--rm",
            "--cgroup-parent",
            &slice,
            "-v",
            &mount,
            &image,
            "exit",
            "0",
        ],
        &env,
    );
    assert_eq!(first.status, Some(0), "{first}\nthe daemon's log:\n{}", said());
    let templates = home.join("templates").to_string_lossy().into_owned();
    let mut patience = Patience::of(&home);
    while !processes_with(&templates).into_iter().any(|pid| {
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .is_ok_and(|c| c.contains(&format!("/{slice}/")))
    }) {
        assert!(
            patience.holds(),
            "no warm VM in {slice}\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    // The reads held to none, which make the record every later read is judged by: the
    // first, with none before it, follows its microVM's liveness alone.
    let mut record = Record::default();
    let mut unbudgeted = Vec::new();
    for _ in 0..3 {
        let read = judged("a read under no budget", &run_read, &record, &env);
        record = record.with(&read);
        unbudgeted.push(read.took().unwrap_or_default());
    }
    // A budget its idle microVMs leave 128 MiB of: short of the gigabyte a read caches in
    // its guest, as one takes with none.
    let held = slice_holds() + (128 << 20);
    budget(&held.to_string());
    let budgeted: Vec<u64> = (0..3)
        .map(|_| {
            judged("a read under the budget", &run_read, &record, &env)
                .took()
                .unwrap_or_default()
        })
        .collect();
    // Held, as its first word said: within its budget's room, or within the least room a
    // run goes on in where its read had filled that room before the word came, which comes
    // once its run has begun (budget E2E 40193342168: short by 1 to 56 MiB at each read's
    // first word, its reads within bounds, PM M333).
    assert!(
        said().contains("held within its budget") || said().contains("its microVM's budget is short by"),
        "no microVM said to be held within its budget\nthe daemon's log:\n{}",
        said()
    );
    // A run begun under no budget, held to one set as it goes.
    budget("infinity");
    let started = run_shards_env(
        &["run"],
        &[
            "-d",
            "--name",
            "held",
            "--cgroup-parent",
            &slice,
            "-v",
            &mount,
            &image,
            "sleep",
        ],
        &env,
    );
    assert_eq!(
        started.status,
        Some(0),
        "{started}\nthe daemon's log:\n{}",
        said()
    );
    budget(&held.to_string());
    // Three reads, each in the run begun under no budget, each judged as every phase's is.
    let late: Vec<u64> = (0..3)
        .map(|_| {
            judged(
                "a read under a budget set as its run went",
                &["exec", "held", "/bin/testguest", "read-marked", "/vol/big"],
                &record,
                &env,
            )
            .took()
            .unwrap_or_default()
        })
        .collect();
    // Its VM process, which `rm -f` leaves ending once its run has: until it has, the
    // slice still holds its guest's page cache of the read, as its own memory.
    let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", "held"], &env);
    let pid: u32 = pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"));
    // Its state and start (proc_pid_stat(5) fields 3 and 22), past its name's parentheses.
    let stat_of_pid = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                let fields: Vec<String> = s
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                Some((fields.first()?.clone(), fields.get(19)?.clone()))
            })
    };
    let (_, started) = stat_of_pid().unwrap_or_else(|| panic!("held's VM process {pid} is not running"));
    let _ = run_shards_env(&["rm"], &["-f", "held"], &env);
    let mut patience = Patience::of(&home);
    while let Some((state, start)) = stat_of_pid()
        && !matches!(state.as_str(), "Z" | "X")
        && start == started
    {
        assert!(
            patience.holds(),
            "held's VM process {pid} still runs\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    // A run held within the least room a run goes on in for its whole reads, not at what it
    // uses, which stalled a read of 1 MiB blocks at 140 KB/s (PM M309): a run of its own
    // alone in the slice, its daemon keeping no warm microVM there (a budget short of the
    // slice ended them for room, and the room came back: no run of 9452af32's CI was held
    // short, PM M314), its guest's way to the share warmed by a small file's read. Then a
    // budget short of what the slice holds and the inactive page cache its kernel keeps
    // beside the files it maps (memory.stat's `file_mapped`, as many as the mapped under a
    // GiB of them, mm/vmscan.c `inactive_is_low`) and the room its host's own reads need
    // below it (its kernel's charge ahead on each of its CPUs, a share's request and its
    // read-ahead: `reads_room`, which the daemon leaves out of every room it tells), by a
    // mebibyte, as the daemon counts a budget's room (daemon/budget.rs), read once the
    // slice's statistics have caught up with its counter, which a microVM just ended leaves
    // them behind (they counted 1.1 GB of one a second after it ended, PM M309): no more
    // anonymous and file memory than it holds.
    let _ = run_shards_env(&["daemon"], &["stop"], &env);
    let pool_of_none: [(&str, &OsStr); 3] = [env[0], env[1], ("SHARDS_POOL", "0".as_ref())];
    budget("infinity");
    std::fs::write(vol.join("small"), vec![1u8; 1 << 20]).unwrap();
    let started = run_shards_env(
        &["run"],
        &[
            "-d",
            "--name",
            "short",
            "--cgroup-parent",
            &slice,
            "-v",
            &mount,
            &image,
            "sleep",
        ],
        &pool_of_none,
    );
    assert_eq!(
        started.status,
        Some(0),
        "{started}\nthe daemon's log:\n{}",
        said()
    );
    let warmed = run_shards_env(
        &["exec"],
        &["short", "/bin/testguest", "read", "/vol/small"],
        &pool_of_none,
    );
    assert_eq!(warmed.status, Some(0), "{warmed}\nthe daemon's log:\n{}", said());
    let stat_of = |stat: &str, key: &str| -> u64 {
        stat.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or(0)
    };
    let short_now = || {
        let mut patience = Patience::of(&home);
        let (current, stat) = loop {
            let current: u64 = std::fs::read_to_string(cgroup.join("memory.current"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let stat = std::fs::read_to_string(cgroup.join("memory.stat")).unwrap();
            if stat_of(&stat, "anon") + stat_of(&stat, "file") <= current {
                patience.done();
                break (current, stat);
            }
            assert!(
                patience.holds(),
                "{slice}'s statistics still count more than it holds ({current} bytes):\n{stat}"
            );
            std::thread::sleep(patience.next_look());
        };
        let mapped = stat_of(&stat, "file_mapped");
        let unmapped = stat_of(&stat, "file")
            .saturating_sub(mapped)
            .saturating_sub(stat_of(&stat, "shmem"));
        let gb = (mapped * 2) >> 30;
        let ratio = if gb == 0 { 1 } else { (gb * 10).isqrt() };
        (current.saturating_sub(unmapped) + mapped / ratio + reads_room()).saturating_sub(1 << 20)
    };
    let id = started.stdout.trim().to_string();
    let held_least = || {
        said()
            .lines()
            .any(|l| l.contains(&format!("container {id}:")) && l.contains("the least room a run goes on in"))
    };
    // Set again from the slice as it is then, while the daemon finds room under it: the
    // kernel takes back more of the slice as its budget falls than its statistics said it
    // keeps beside what its processes map, and the daemon counts the room the slice has
    // after (22 MiB under 229 MiB in budget E2E 40148135044's h, 1 of 608 runs). A budget
    // that leaves none is said at once. Set again once the daemon has answered the budget
    // before (a budget set anew is always said: its microVM held within it, short of it, or
    // with room again, daemon/budget.rs), or once the slice has moved so that the budget it
    // leaves is another; waited for while the daemon and its microVMs go on ([`Patience`]).
    let (mut asked, mut short): (Vec<u64>, Option<u64>) = (Vec::new(), None);
    let answered = |since: usize| {
        said()
            .get(since..)
            .is_some_and(|l| l.contains(&format!("container {id}: its microVM")))
    };
    let mut since = 0;
    let mut patience = Patience::of(&home);
    while !held_least() {
        assert!(
            patience.holds(),
            "no microVM said to be held within the least room a run goes on in, under budgets of {asked:?} MiB\nthe daemon's log:\n{}",
            said()
        );
        let now = short_now();
        if short != Some(now) || answered(since) {
            since = said().len();
            budget(&now.to_string());
            asked.push(now >> 20);
            short = Some(now);
        }
        std::thread::sleep(patience.next_look());
    }
    patience.done();
    let short = short.unwrap_or_default();
    let held_short: Vec<u64> = (0..3)
        .map(|_| {
            judged(
                "a read held within the least room",
                &["exec", "short", "/bin/testguest", "read-marked", "/vol/big"],
                &record,
                &pool_of_none,
            )
            .took()
            .unwrap_or_default()
        })
        .collect();
    let _ = run_shards_env(&["rm"], &["-f", "short"], &pool_of_none);
    // Each phase's reads, for a run's record (.github/workflows/budget-e2e.yml).
    eprintln!(
        "reads in ms: under none {unbudgeted:?}; under {} MiB {budgeted:?}; under it set as its run went {late:?}; held within the least room under {} MiB {held_short:?}; judged by {record}",
        held >> 20,
        short >> 20
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// What the reads held to none showed, by which every later read is judged (D215's shape:
/// the slowest answer seen, and none before the first, whose read follows its microVM's
/// liveness alone): the longest one took from its guest's first mark to its last, the
/// longest between two of its marks a MiB apart (its reader's block), and the longest from
/// its command's start to its first mark. Learned from this run's reads held to none alone,
/// never seeded: the test asks what a budget does to a read, so a budget's reads never
/// raise the bound they are judged by. A read under a budget once took 2.06 times its time
/// under none beside the whole suite (CI 41365265628), and one held within the least room
/// on a 32-CPU host 28 minutes, reading all along (adacachy2, 237f3ebe7): no multiple of a
/// read's time tells a slow read from one that has stopped, and progress alone, with no
/// least, takes almost none for some.
#[derive(Debug, Clone, Copy, Default)]
struct Record {
    read: Option<Duration>,
    block: Option<Duration>,
    start: Option<Duration>,
}

impl Record {
    /// The record with `read`'s times in it, each the slowest seen.
    fn with(self, read: &Followed) -> Record {
        Record {
            read: self.read.max(read.read()),
            block: self.block.max(read.block()),
            start: self.start.max(read.start()),
        }
    }

    /// What a read is judged by past the slowest read seen: a window as long as that read,
    /// and the bytes it must read over each, what the slowest MiB seen keeps over as long.
    /// None until a read with two marks has been seen.
    fn judging(&self) -> Option<(Duration, u64)> {
        let (window, block) = (self.read?, self.block?);
        let least = window.as_nanos().saturating_mul(u128::from(MIB)) / block.as_nanos().max(1);
        Some((window, u64::try_from(least).unwrap_or(u64::MAX)))
    }
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the reads held to none: the slowest {:?} from its first mark to its last, the slowest MiB {:?}, the slowest start {:?}",
            self.read, self.block, self.start
        )
    }
}

/// A read followed to its end: when its command began, each of its guest's marks as it
/// came (when, and how far it had read), what it wrote and its exit status.
struct Followed {
    began: Instant,
    marks: Vec<(Instant, u64)>,
    stdout: String,
    stderr: String,
    status: Option<i32>,
}

impl Followed {
    /// Its milliseconds, as its guest says them of the whole gigabyte.
    fn took(&self) -> Option<u64> {
        let said = format!("read /vol/big {GIB} in ");
        self.stdout
            .lines()
            .find_map(|l| l.strip_prefix(said.as_str())?.strip_suffix(" ms")?.parse().ok())
    }

    /// From its command's start to its first mark.
    fn start(&self) -> Option<Duration> {
        Some(self.marks.first()?.0.saturating_duration_since(self.began))
    }

    /// From its first mark to its last.
    fn read(&self) -> Option<Duration> {
        let (first, last) = (self.marks.first()?, self.marks.last()?);
        Some(last.0.saturating_duration_since(first.0))
    }

    /// The longest between two of its marks.
    fn block(&self) -> Option<Duration> {
        self.marks
            .windows(2)
            .map(|w| w[1].0.saturating_duration_since(w[0].0))
            .max()
    }
}

/// A read refused ([`follow`]).
#[derive(Debug)]
enum Stalled {
    /// No mark past the slowest start seen, its daemon and microVMs showing no progress for
    /// as long as [`Patience`] waits on them, and its slice held by its host within none of
    /// it ([`stalled`]).
    Unstarted { waited: Duration, record: Record },
    /// Past the slowest read seen from its first mark, a window as long over which it read
    /// less than the slowest MiB seen keeps over it: how far it had read and how long after
    /// its first mark, and what it read over the window and what it had to.
    Slow {
        read: u64,
        after: Duration,
        window: Duration,
        moved: u64,
        least: u64,
        record: Record,
    },
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stalled::Unstarted { waited, record } => write!(
                f,
                "no mark {waited:?} after its command began, past the slowest start seen, its daemon and microVMs showing nothing and its slice held by nothing; {record}"
            ),
            Stalled::Slow {
                read,
                after,
                window,
                moved,
                least,
                record,
            } => write!(
                f,
                "{read} bytes read {after:?} after its first mark, {moved} of them over the last {window:?}, where the slowest MiB seen keeps {least}; {record}"
            ),
        }
    }
}

/// What a followed read's command did: a line it wrote (when it came), or its end.
enum Heard {
    Line(Instant, String),
    Stderr(String),
    /// Its stdout or stderr closed.
    Closed,
    Exited(Option<i32>),
}

/// Where a followed read is (R04): its command begun and no mark yet, past the slowest start
/// seen looked at once each as long; reading from its first mark, past the slowest read seen
/// looked at once each window as long, each look with how far it had read by then; or its
/// whole read said, its command's end awaited.
enum Phase {
    Starting {
        looked: Option<Instant>,
    },
    Reading {
        first: Instant,
        looked: Option<(Instant, u64)>,
    },
    Read,
}

/// Runs `shards ARGS`, a `read-marked` read of the gigabyte, following its guest's marks as
/// they come, judged by `record` ([`Record`]). Before its first mark it is waited on for the
/// slowest start seen, and past it for as long as its daemon or a microVM shows progress
/// ([`Patience`], judged as D215's judge judges a wait, its first mark its answer) or the
/// slice `cgroup` has been held by its host since the look before ([`stalled`]); from its first mark, for the slowest read seen, and past that looked at
/// once each window as long, each having to read what the slowest MiB seen keeps over it
/// ([`Record::judging`]); with nothing seen yet, until it ends. Each look past the slowest
/// read seen says the slice `cgroup`'s state on stderr. Refused, its command is ended and
/// waited for.
fn follow(
    args: &[&str],
    env: &[(&str, &OsStr)],
    home: &Path,
    record: &Record,
    cgroup: &Path,
) -> Result<Followed, Stalled> {
    let mut cmd = common::command();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let began = Instant::now();
    let mut child = common::spawn(&mut cmd).unwrap();
    let pid = child.id();
    let (tell, heard) = mpsc::channel();
    // Read on threads of their own, as `run_command` reads: a pipe another process still
    // holds never holds up a read whose command has said its whole read and ended.
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let to = tell.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if to.send(Heard::Line(Instant::now(), line)).is_err() {
                return;
            }
        }
        let _ = to.send(Heard::Closed);
    });
    let to = tell.clone();
    std::thread::spawn(move || {
        let mut said = String::new();
        let _ = BufReader::new(err).read_to_string(&mut said);
        let _ = to.send(Heard::Stderr(said));
        let _ = to.send(Heard::Closed);
    });
    std::thread::scope(|s| {
        s.spawn(move || {
            let status = child.wait().ok().and_then(|st| st.code());
            let _ = tell.send(Heard::Exited(status));
        });
        let judged = judge(&heard, began, Patience::of(home), record, cgroup);
        if judged.is_err() {
            // Killed, not asked to end: a client asked waits on its exec, which a refused
            // read's microVM may never end. The slice's stop takes the microVM (`Stopped`).
            // SAFETY: kill(2) of this test's child, its read refused; its waiter reaps it.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        judged
    })
}

/// [`follow`]'s judge: what `heard` brings of a read whose command began at `began`, until
/// its command has ended having said its whole read, or with both its pipes closed. Its
/// start is waited on with `patience`, its caller's: a kind of its own, which the starts its
/// caller judges teach (a kind taught other callers' starts, a play's of none, is refused at
/// once by a party idle past them).
fn judge(
    heard: &mpsc::Receiver<Heard>,
    began: Instant,
    patience: Patience,
    record: &Record,
    cgroup: &Path,
) -> Result<Followed, Stalled> {
    // Its start's wait, taught its answer by the first mark.
    let mut patience = Some(patience);
    // The slice's stall total when last looked at: grown since, its host's hold.
    let mut stalls = stalled(cgroup);
    let mut phase = Phase::Starting { looked: None };
    let (mut marks, mut stdout, mut stderr) = (Vec::new(), String::new(), String::new());
    let (mut status, mut closed) = (None::<Option<i32>>, 0);
    loop {
        if let Some(status) = status
            && (matches!(phase, Phase::Read) || closed == 2)
        {
            return Ok(Followed {
                began,
                marks,
                stdout,
                stderr,
                status,
            });
        }
        let next = match &phase {
            Phase::Starting { looked } => record.start.map(|s| looked.map_or(began + s, |t| t + s)),
            Phase::Reading { first, looked } => record
                .judging()
                .map(|(w, _)| looked.map_or(*first + w, |(t, _)| t + w)),
            Phase::Read => None,
        };
        let got = match next {
            // Its end awaited: what it says, or its pipes' close, or its exit.
            _ if status.is_some() => heard.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(at) => heard.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => heard.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(Heard::Line(at, line)) => {
                if let Some(bytes) = line.strip_prefix("at ").and_then(|b| b.parse::<u64>().ok()) {
                    marks.push((at, bytes));
                    if matches!(phase, Phase::Starting { .. }) {
                        if let Some(patience) = patience.take() {
                            patience.done();
                        }
                        phase = Phase::Reading {
                            first: at,
                            looked: None,
                        };
                    }
                    // The whole file read: its result line and its end awaited.
                    if bytes >= GIB {
                        phase = Phase::Read;
                    }
                } else if line.starts_with("read /vol/big ") {
                    phase = Phase::Read;
                }
                stdout.push_str(&line);
                stdout.push('\n');
                continue;
            }
            Ok(Heard::Stderr(said)) => {
                stderr = said;
                continue;
            }
            Ok(Heard::Closed) => {
                closed += 1;
                continue;
            }
            Ok(Heard::Exited(code)) => {
                status = Some(code);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Ok(Followed {
                    began,
                    marks,
                    stdout,
                    stderr,
                    status: status.flatten(),
                });
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        // A look is due.
        let at = Instant::now();
        match &mut phase {
            Phase::Starting { looked } => {
                let progresses = patience.as_mut().is_some_and(Patience::holds);
                let now = stalled(cgroup);
                let held = now > stalls;
                stalls = now;
                if !progresses && !held {
                    return Err(Stalled::Unstarted {
                        waited: at.saturating_duration_since(began),
                        record: *record,
                    });
                }
                *looked = Some(at);
            }
            Phase::Reading { first, looked } => {
                let Some((window, least)) = record.judging() else {
                    continue;
                };
                let read = marks.last().map_or(0, |&(_, b)| b);
                let _ = writeln!(
                    std::io::stderr(),
                    "a read {:?} after its first mark, {read} bytes read, past the slowest read seen: {}",
                    at.saturating_duration_since(*first),
                    slice_state(cgroup)
                );
                // No more than the file has left, which a read that reads it all is done with.
                if let Some((_, before)) = *looked
                    && read.saturating_sub(before) < least.min(GIB.saturating_sub(before))
                {
                    return Err(Stalled::Slow {
                        read,
                        after: at.saturating_duration_since(*first),
                        window,
                        moved: read.saturating_sub(before),
                        least: least.min(GIB.saturating_sub(before)),
                        record: *record,
                    });
                }
                *looked = Some((at, read));
            }
            Phase::Read => {}
        }
    }
}

/// A read is judged by the reads held to none ([`judge`]): past the slowest of them, a read
/// that reads less over a window as long than their slowest MiB keeps over it is refused,
/// as the read held short on adacachy2 should have been, reading all along (237f3ebe7); one
/// that keeps that pace past the bound goes on to its end; with nothing seen, a read is
/// followed to its end however it stops and starts; and one with no mark past the slowest
/// start seen, nothing of its daemon's or microVMs' showing progress, is refused. Its reads'
/// marks are played on threads of their own, each window's pace far from the bound.
#[test]
fn a_read_is_judged_by_the_pace_of_the_reads_held_to_none() {
    let home = TempDir::new("budget-judge");
    let cgroup = home.join("no-slice");
    // Its first mark `start` after now, then a MiB each `gap`, `pause` after `at` MiB; then
    // its result and end.
    let play = |start: Duration, gap: Duration, pause: (u64, Duration)| {
        let (tell, heard) = mpsc::channel();
        std::thread::spawn(move || {
            let mark = |b: u64| tell.send(Heard::Line(Instant::now(), format!("at {b}")));
            std::thread::sleep(start);
            if mark(0).is_err() {
                return;
            }
            for mib in 1..=GIB / MIB {
                std::thread::sleep(if mib == pause.0 { pause.1 } else { gap });
                if mark(mib * MIB).is_err() {
                    return;
                }
            }
            let _ = tell.send(Heard::Line(
                Instant::now(),
                format!("read /vol/big {GIB} in 1 ms"),
            ));
            let _ = tell.send(Heard::Exited(Some(0)));
        });
        heard
    };
    let seen = Record {
        read: Some(Duration::from_secs(1)),
        block: Some(Duration::from_millis(40)),
        start: Some(Duration::from_millis(100)),
    };
    // 25 MiB a window of a second, the slowest MiB 40 ms.
    assert_eq!(seen.judging(), Some((Duration::from_secs(1), 25 * MIB)));
    // A MiB each 50 ms or longer, 21 at most a window: refused at its second look, a window
    // past the bound.
    match judge(
        &play(Duration::ZERO, Duration::from_millis(50), (0, Duration::ZERO)),
        Instant::now(),
        Patience::of(&home),
        &seen,
        &cgroup,
    ) {
        Err(Stalled::Slow { moved, least, .. }) => assert!(moved < least, "{moved} of {least}"),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(read) => panic!("not refused: {} marks", read.marks.len()),
    }
    // About 400 MiB a second, 16 times what it must, past the bound for two windows: to its
    // end.
    let read = judge(
        &play(Duration::ZERO, Duration::from_micros(2_400), (0, Duration::ZERO)),
        Instant::now(),
        Patience::of(&home),
        &seen,
        &cgroup,
    )
    .unwrap_or_else(|stalled| panic!("refused: {stalled}"));
    assert_eq!((read.status, read.took()), (Some(0), Some(1)));
    assert!(
        read.read() > seen.read,
        "its read past the bound: {:?}",
        read.read()
    );
    // With nothing seen, a read with a stop of a second in it, followed to its end.
    let cold = Record::default();
    assert_eq!(cold.judging(), None);
    let read = judge(
        &play(Duration::ZERO, Duration::ZERO, (512, Duration::from_secs(1))),
        Instant::now(),
        Patience::of(&home),
        &cold,
        &cgroup,
    )
    .unwrap_or_else(|stalled| panic!("refused cold: {stalled}"));
    assert_eq!(read.marks.len(), 1025);
    assert!(read.block() >= Some(Duration::from_secs(1)));
    // No mark past the slowest start seen, no daemon in its home and no microVM under this
    // test's process showing progress: refused, the look after its patience ran out.
    let (tell, heard) = mpsc::channel::<Heard>();
    match judge(&heard, Instant::now(), Patience::of(&home), &seen, &cgroup) {
        Err(Stalled::Unstarted { waited, .. }) => assert!(waited >= Duration::from_millis(100)),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("not refused"),
    }
    drop(tell);
}

/// A start slower than the slowest seen is refused only once a whole window past the slowest
/// start its patience has seen (`Patience`'s W*) has shown no progress of its party (R03:
/// past the bound, a wait goes on while progress shows, and is refused only after a whole
/// window with none), and a host's hold of its slice counts as held. adacachy2 at 8bfa6150e and 9d2da923d refused a read under the budget 97.49 ms after
/// its command began, the third look of 32.44 ms, the slowest start seen, while its daemon's
/// warm microVMs had not been looked at once (`common::Ran`, then, judged each over a slice) and
/// its slice was at its `memory.high` (4345 `high` events). Do: a stand-in VM process under
/// this test's, running, and a start of 500 ms against a slowest seen of 30 ms; then a start
/// of as long in a slice whose tasks stall on memory, as PSI counts them, no microVM to
/// follow; then a start that never comes, the stand-in idle. Each waited on with one kind of
/// patience, this test's own, which the first start teaches. Expect: the first two followed to
/// their ends, the third refused, past the slowest start the first taught. (CI run
/// 43513425954 and ROOT's KVM run at d513d0a0b refused the first at 60 ms: its stand-in idle,
/// and every `judge` waiting with one kind, which `a_read_is_judged_by_the_pace_of_the_reads_held_to_none`'s
/// starts of none had taught a W* of microseconds.)
#[test]
fn a_slow_start_is_refused_only_past_a_whole_slice_without_progress_or_hold() {
    let home = TempDir::new("budget-start");
    let seen = Record {
        read: Some(Duration::from_secs(1)),
        block: Some(Duration::from_millis(40)),
        start: Some(Duration::from_millis(30)),
    };
    // Its first mark `start` from now, then the whole gigabyte at once, its result and end.
    let late = |start: Duration| {
        let (tell, heard) = mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(start);
            for mib in 0..=GIB / MIB {
                let _ = tell.send(Heard::Line(Instant::now(), format!("at {}", mib * MIB)));
            }
            let _ = tell.send(Heard::Line(
                Instant::now(),
                format!("read /vol/big {GIB} in 1 ms"),
            ));
            let _ = tell.send(Heard::Exited(Some(0)));
        });
        heard
    };
    let start = Duration::from_millis(500);
    // One kind for the three: a call site's.
    let patience = || Patience::of(&home);
    {
        let _vm = StandIn::running(&home);
        let read = judge(
            &late(start),
            Instant::now(),
            patience(),
            &seen,
            &home.join("no-slice"),
        )
        .unwrap_or_else(|stalled| panic!("a start of {start:?} with a VM not yet judged refused: {stalled}"));
        assert!(read.start() >= Some(start), "{:?}", read.start());
    }
    // A slice whose tasks stall on memory all along, as PSI says it (`some ... total=` µs).
    let slice = home.join("held");
    std::fs::create_dir(&slice).unwrap();
    let holding = std::sync::atomic::AtomicBool::new(true);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut total = 0u64;
            while holding.load(std::sync::atomic::Ordering::Relaxed) {
                total += 5_000;
                let pressure = format!(
                    "some avg10=0.00 avg60=0.00 avg300=0.00 total={total}\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
                );
                let staged = slice.join("pressure.new");
                std::fs::write(&staged, pressure).unwrap();
                std::fs::rename(&staged, slice.join("memory.pressure")).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let read = judge(&late(start), Instant::now(), patience(), &seen, &slice);
        holding.store(false, std::sync::atomic::Ordering::Relaxed);
        let read =
            read.unwrap_or_else(|stalled| panic!("a start of {start:?} held by its host refused: {stalled}"));
        assert!(read.start() >= Some(start), "{:?}", read.start());
    });
    let _vm = StandIn::under(&home);
    let (tell, heard) = mpsc::channel::<Heard>();
    match judge(&heard, Instant::now(), patience(), &seen, &home.join("no-slice")) {
        // Its patience's kind taught by the first start above, its first mark its answer (the
        // second's patience found no party, and its host's hold kept it).
        Err(Stalled::Unstarted { waited, .. }) => assert!(waited >= start, "{waited:?}"),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("not refused"),
    }
    drop(tell);
}

/// A stand-in for a VM process under this test's, ended as it drops: a program run through a
/// link named as `common::vm_processes_under` finds VM processes (Linux names a process after
/// the path it ran, a link's own name; its first argument stays the program's, as busybox's
/// applets need).
struct StandIn(std::process::Child);

impl StandIn {
    /// Idle: `sleep`.
    fn under(dir: &Path) -> StandIn {
        StandIn::of(dir, "idle", "/bin/sleep", "sleep", &["600"])
    }

    /// Running all along, as a booting guest's VM process does: `sh` in a loop of its own.
    fn running(dir: &Path) -> StandIn {
        StandIn::of(dir, "running", "/bin/sh", "sh", &["-c", "while :; do :; done"])
    }

    fn of(dir: &Path, sub: &str, program: &str, arg0: &str, args: &[&str]) -> StandIn {
        use std::os::unix::process::CommandExt as _;
        let dir = dir.join(sub);
        let link = dir.join("shards-vm");
        if !link.exists() {
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(program, &link).unwrap();
        }
        StandIn(Command::new(&link).arg0(arg0).args(args).spawn().unwrap())
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How long the tasks of the slice `cgroup` have stalled on memory, in microseconds: PSI's
/// `some` total (Linux's Documentation/accounting/psi.rst), which grows while its host holds
/// any of them in reclaim, at its `memory.high`, or refaulting what it took back. None where
/// its kernel keeps none.
fn stalled(cgroup: &Path) -> Option<u64> {
    std::fs::read_to_string(cgroup.join("memory.pressure"))
        .ok()?
        .lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total=")?.parse().ok())
}

/// The slice's memory, its events and pressure, and for each process in its tree its
/// memory and each thread's state and wait channel (/proc/PID/task/TID/wchan).
#[cfg(unix)]
fn slice_state(cgroup: &Path) -> String {
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let stat = read(&cgroup.join("memory.stat"));
    let keys = [
        "anon",
        "file",
        "file_mapped",
        "shmem",
        "pgscan",
        "pgsteal",
        "pgmajfault",
        "workingset_refault_file",
    ];
    let picked: Vec<&str> = keys
        .iter()
        .filter_map(|k| stat.lines().find(|l| l.split_whitespace().next() == Some(k)))
        .collect();
    let mut out = format!(
        "current {} high {}; {}; events {}; pressure {}",
        read(&cgroup.join("memory.current")).trim(),
        read(&cgroup.join("memory.high")).trim(),
        picked.join(", "),
        read(&cgroup.join("memory.events"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        read(&cgroup.join("memory.pressure")).lines().next().unwrap_or("")
    );
    let mut dirs = vec![cgroup.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for pid in read(&dir.join("cgroup.procs")).lines() {
            let proc = PathBuf::from(format!("/proc/{pid}"));
            let status = read(&proc.join("status"));
            let field = |k: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(k))
                    .map_or(String::new(), |v| {
                        v.split_whitespace().collect::<Vec<_>>().join(" ")
                    })
            };
            out += &format!(
                "\n  {pid} {} RssAnon {} RssFile {}:",
                read(&proc.join("comm")).trim(),
                field("RssAnon:"),
                field("RssFile:")
            );
            let Ok(tasks) = std::fs::read_dir(proc.join("task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let t = task.path();
                let state = read(&t.join("stat"))
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
                    .unwrap_or_default();
                out += &format!(
                    " {}/{state}/{}",
                    read(&t.join("comm")).trim(),
                    read(&t.join("wchan")).trim()
                );
            }
        }
        if let Ok(children) = std::fs::read_dir(&dir) {
            dirs.extend(children.flatten().map(|c| c.path()).filter(|p| p.is_dir()));
        }
    }
    out
}
