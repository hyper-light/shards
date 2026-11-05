//! FUSE reads under a memory budget, timed without the daemon suite beside them (D188).
//! Cargo runs test binaries one after another, so these sequential phases compare the
//! budget's effect without other daemon tests competing for the host between them.

#![cfg(target_os = "linux")]
#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{TempDir, cannot_pool, cannot_run_vms, home_with, processes_with, run_shards_env, served};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A microVM held to a budget reads through FUSE within twice the time one held to none
/// takes (D188): under a slice of the user's manager whose `MemoryHigh` its idle microVMs
/// leave 128 MiB of, with no swap, runs restored from its template read a gigabyte of a
/// volume through FUSE (no DAX window), each held within the room the budget leaves it, so
/// that its own kernel drops its page cache. Its guest's page cache had filled the slice,
/// and its host took back the only memory it could, the guest's own RAM, its template's
/// file pages: 400 to 600 s a read on CI's AMD runners, 3 to 5 s with none (PM M308, M309).
/// A budget set as a run goes holds it too; and one short of what a run alone in the
/// slice holds holds it within the least room a run goes on in, which reads as fast.
// Linux's user systemd manager and cgroup budget; other hosts exercise shards' groups.
#[cfg(unix)]
#[test]
fn a_microvm_held_to_a_budget_reads_through_fuse_within_twice_its_time_without_one() {
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
    let home = home_with("daemon-budget-fuse", TIMEOUT);
    let vol = TempDir::new("budget-fuse-vol");
    std::fs::File::create(vol.join("big"))
        .unwrap()
        .set_len(1 << 30)
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
    // Each read's milliseconds, as the test guest says them.
    let took = |what: &str, out: &str| -> u64 {
        out.lines()
            .find_map(|l| {
                l.strip_prefix("read /vol/big 1073741824 in ")?
                    .strip_suffix(" ms")
            })
            .and_then(|ms| ms.parse().ok())
            .unwrap_or_else(|| panic!("{what}: no whole read in {out:?}\nthe daemon's log:\n{}", said()))
    };
    let read = |what: &str| -> u64 {
        let run = run_shards_env(
            &["run"],
            &[
                "--rm",
                "--cgroup-parent",
                &slice,
                "-v",
                &mount,
                &image,
                "read",
                "/vol/big",
            ],
            &env,
            Duration::from_secs(600),
        );
        assert_eq!(
            run.status,
            Some(0),
            "{what}: {run}\nthe daemon's log:\n{}",
            said()
        );
        took(what, &run.stdout)
    };
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
        TIMEOUT,
    );
    assert_eq!(first.status, Some(0), "{first}\nthe daemon's log:\n{}", said());
    let templates = home.join("templates").to_string_lossy().into_owned();
    let deadline = Instant::now() + TIMEOUT;
    while !processes_with(&templates).into_iter().any(|pid| {
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .is_ok_and(|c| c.contains(&format!("/{slice}/")))
    }) {
        assert!(
            Instant::now() < deadline,
            "no warm VM in {slice}\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let median = |mut xs: Vec<u64>| {
        xs.sort_unstable();
        xs[xs.len() / 2]
    };
    let unbudgeted = median((0..3).map(|_| read("a read under no budget")).collect());
    // A budget its idle microVMs leave 128 MiB of: short of the gigabyte a read caches in
    // its guest, as one takes with none.
    let held = slice_holds() + (128 << 20);
    budget(&held.to_string());
    let budgeted: Vec<u64> = (0..3).map(|_| read("a read under the budget")).collect();
    assert!(
        median(budgeted.clone()) <= 2 * unbudgeted,
        "reads under a budget of {} MiB took {budgeted:?} ms, under none {unbudgeted} ms\nthe daemon's log:\n{}",
        held >> 20,
        said()
    );
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
        TIMEOUT,
    );
    assert_eq!(
        started.status,
        Some(0),
        "{started}\nthe daemon's log:\n{}",
        said()
    );
    budget(&held.to_string());
    // Three reads, each in the run begun under no budget, their median held to the bound
    // as each phase's is: one read alone, beside the whole suite on a CI runner, once took
    // 2.09 times (CI 40088873246).
    let late: Vec<u64> = (0..3)
        .map(|_| {
            let exec = run_shards_env(
                &["exec"],
                &["held", "/bin/testguest", "read", "/vol/big"],
                &env,
                Duration::from_secs(600),
            );
            assert_eq!(exec.status, Some(0), "{exec}\nthe daemon's log:\n{}", said());
            took("a read under a budget set as its run went", &exec.stdout)
        })
        .collect();
    assert!(
        median(late.clone()) <= 2 * unbudgeted,
        "reads under a budget set as their run went took {late:?} ms, under none {unbudgeted} ms\nthe daemon's log:\n{}",
        said()
    );
    // Its VM process, which `rm -f` leaves ending once its run has: until it has, the
    // slice still holds its guest's page cache of the read, as its own memory.
    let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", "held"], &env, TIMEOUT);
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
    let _ = run_shards_env(&["rm"], &["-f", "held"], &env, TIMEOUT);
    let deadline = Instant::now() + TIMEOUT;
    while let Some((state, start)) = stat_of_pid()
        && !matches!(state.as_str(), "Z" | "X")
        && start == started
    {
        assert!(
            Instant::now() < deadline,
            "held's VM process {pid} still runs\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // A run held within the least room a run goes on in for its whole reads, not at what it
    // uses, which stalled a read of 1 MiB blocks at 140 KB/s (PM M309): a run of its own
    // alone in the slice, its daemon keeping no warm microVM there (a budget short of the
    // slice ended them for room, and the room came back: no run of 9452af32's CI was held
    // short, PM M314), its guest's way to the share warmed by a small file's read. Then a
    // budget short of what the slice holds and the inactive page cache its kernel keeps
    // beside the files it maps (memory.stat's `file_mapped`, as many as the mapped under a
    // GiB of them, mm/vmscan.c `inactive_is_low`), by a mebibyte, as the daemon counts a
    // budget's room (daemon/budget.rs), read once the slice's statistics have caught up
    // with its counter, which a microVM just ended leaves them behind (they counted 1.1 GB
    // of one a second after it ended, PM M309): no more anonymous and file memory than it
    // holds.
    let _ = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
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
        TIMEOUT,
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
        TIMEOUT,
    );
    assert_eq!(warmed.status, Some(0), "{warmed}\nthe daemon's log:\n{}", said());
    let stat_of = |stat: &str, key: &str| -> u64 {
        stat.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or(0)
    };
    let short_now = || {
        let deadline = Instant::now() + TIMEOUT;
        let (current, stat) = loop {
            let current: u64 = std::fs::read_to_string(cgroup.join("memory.current"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let stat = std::fs::read_to_string(cgroup.join("memory.stat")).unwrap();
            if stat_of(&stat, "anon") + stat_of(&stat, "file") <= current {
                break (current, stat);
            }
            assert!(
                Instant::now() < deadline,
                "{slice}'s statistics still count more than it holds ({current} bytes):\n{stat}"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let mapped = stat_of(&stat, "file_mapped");
        let unmapped = stat_of(&stat, "file")
            .saturating_sub(mapped)
            .saturating_sub(stat_of(&stat, "shmem"));
        let gb = (mapped * 2) >> 30;
        let ratio = if gb == 0 { 1 } else { (gb * 10).isqrt() };
        (current.saturating_sub(unmapped) + mapped / ratio).saturating_sub(1 << 20)
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
    // that leaves none is said at once.
    let (mut asked, mut short): (Vec<u64>, u64) = (Vec::new(), 0);
    let deadline = Instant::now() + TIMEOUT;
    while !held_least() {
        assert!(
            Instant::now() < deadline,
            "no microVM said to be held within the least room a run goes on in, under budgets of {asked:?} MiB\nthe daemon's log:\n{}",
            said()
        );
        short = short_now();
        budget(&short.to_string());
        asked.push(short >> 20);
        let set = Instant::now();
        while !held_least() && set.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let held_short: Vec<u64> = (0..3)
        .map(|_| {
            let exec = watching(&cgroup, "a read held within the least room", || {
                run_shards_env(
                    &["exec"],
                    &["short", "/bin/testguest", "read", "/vol/big"],
                    &pool_of_none,
                    Duration::from_secs(600),
                )
            });
            assert_eq!(exec.status, Some(0), "{exec}\nthe daemon's log:\n{}", said());
            took("a read held within the least room", &exec.stdout)
        })
        .collect();
    assert!(
        median(held_short.clone()) <= 2 * unbudgeted,
        "reads under a budget of {} MiB, held within the least room, took {held_short:?} ms, under none {unbudgeted} ms\nthe daemon's log:\n{}",
        short >> 20,
        said()
    );
    let _ = run_shards_env(&["rm"], &["-f", "short"], &pool_of_none, TIMEOUT);
    // Each phase's reads, for a run's record (.github/workflows/budget-e2e.yml).
    eprintln!(
        "reads in ms: under none {unbudgeted} (the median); under {} MiB {budgeted:?}; under it set as its run went {late:?}; held within the least room under {} MiB {held_short:?}",
        held >> 20,
        short >> 20
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// Runs `f`, and while it runs past a minute, every 30 s, says on stderr what the slice
/// `cgroup` and the processes in it are doing ([`slice_state`]): a read held short takes
/// seconds, and one that stalls (40079524601's (d): 600 s, its guest answering nothing)
/// leaves in CI's log where its time went.
#[cfg(unix)]
fn watching<T>(cgroup: &Path, what: &str, f: impl FnOnce() -> T) -> T {
    /// Set as `f` returns or unwinds, which ends the watch.
    struct Done<'a>(&'a AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let start = Instant::now();
            let mut next = Duration::from_secs(60);
            while !done.load(Ordering::Acquire) {
                if start.elapsed() >= next {
                    let _ = writeln!(
                        std::io::stderr(),
                        "{what}, {} s in: {}",
                        start.elapsed().as_secs(),
                        slice_state(cgroup)
                    );
                    next += Duration::from_secs(30);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let _done = Done(&done);
        f()
    })
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
