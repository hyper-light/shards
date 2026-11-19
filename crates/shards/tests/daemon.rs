//! The daemon, end to end (crates/shards/src/daemon.rs): `shards run vm` through the daemon
//! its first run starts, served from pools of warm VMs of the image's template. Real VMs;
//! the image comes from a loopback registry (tests/common, `served`), and its entrypoint
//! is the test guest, so a command is one of its modes. Needs vsock; its template and pool
//! tests need snapshots too, and print SKIP where the daemon keeps none (`cannot_pool`).
//! Tests of what Windows serves with a later step are Unix's until then, each naming it;
//! tests of a Unix mechanism Windows has no counterpart of say so.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    Patience, TempDir, Wait, WaitKind, alive, cannot_pool, cannot_run_vms, commands_with, guest_init,
    home_with, interrupt, interruptible, kernel, kill_now, left_of, processes_with, registry_of, rootfs_dir,
    run_shards_env, run_shards_env_in, served, served_variant, shards, test_image, test_image_with,
};
#[cfg(windows)]
use common::{files_under, users};

/// A home with the image pulled, the guest recorded, and, where this host takes snapshots,
/// the image's template saved by a first run, whose daemon now serves it.
#[track_caller]
fn home(name: &str, image: &str) -> TempDir {
    let home = recorded(name);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let first = run_shards_env(&["run"], &[image, "exit", "0"], &env);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    home
}

/// A home whose runs boot the test kernel and init, recorded as its guest, so that they
/// save and restore templates.
#[track_caller]
fn recorded(name: &str) -> TempDir {
    let home = TempDir::new(name);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    home
}

/// An image's first run is served by the VM that saved its template, which goes on
/// recording what the run touches. Its command may clean the data cache over the image's
/// pages before it reads them, as a guest kernel does over a page it is about to execute
/// from the image (served with DAX): the host takes the fault for a write to its
/// read-only device memory, which it was not, and gives the page back, as later runs
/// restored from the template do too. Before, the VM ended.
#[test]
fn runs_may_clean_the_cache_over_their_image() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = recorded("daemon-clean-cache");
    let env = [("SHARDS_HOME", home.as_os_str())];
    for which in ["first", "restored"] {
        let run = run_shards_env(&["run"], &[&image, "clean-cache", "/bin/testguest"], &env);
        assert_eq!(run.status, Some(0), "{which}: {}", run.stderr);
        let pages: u64 = run
            .stdout
            .strip_prefix("cleaned ")
            .and_then(|n| n.trim_end().parse().ok())
            .unwrap_or_else(|| panic!("{which}: {:?}", run.stdout));
        assert!(pages > 1, "{which}: {pages} pages");
    }
}

/// `shards run ARGS` in `home`, its stdout piped and its stdin closed.
fn spawn_run(home: &Path, args: &[&str]) -> Child {
    common::command()
        .arg("run")
        .args(args)
        .env("SHARDS_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits for a child, a client of `home`'s daemon, failing the test rather than hanging it:
/// for as long as it, its daemon and microVMs progress ([`Patience::client`], its kind its
/// caller's call site): once its daemon has died or stopped, its own end is all that goes on
/// (containers.rs' `exit`).
#[track_caller]
fn wait(child: &mut Child, home: &Path) -> Option<i32> {
    let mut patience = Patience::client(child.id(), home);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            patience.done();
            return status.code();
        }
        if !patience.holds() {
            let _ = child.kill();
            panic!("shards run did not exit, it, its daemon and microVMs no longer progressing");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until `check` holds, for as long as `home`'s daemon or its microVMs progress
/// ([`Patience`], its kind its caller's call site): a loaded 4-CPU runner, each look at its
/// processes a PowerShell's on Windows, took a pool past 5 s to settle (CI 40908704180), and
/// an arm64 Windows host nested under QEMU's TCG saved a template in 20 s and filled its
/// pool after it (D209).
#[track_caller]
fn eventually(home: &Path, what: &str, mut check: impl FnMut() -> bool) {
    let mut patience = Patience::of(home);
    while !check() {
        assert!(patience.holds(), "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
}

/// Waits until daemon process `daemon` of `home` has exited, followed by its pid ([`Patience::
/// client`]): its record, by which a home's patience finds its daemon, goes as the daemon
/// finishes (`Daemon::finish`), before its last work and its exit, and that patience then
/// finds none of its party running (KVM batch 22, refused 3 ms in; KVM batch 26,
/// `stop_ends_runs_and_waiting_vms` refused 3.8 ms in, the daemon still ending).
#[track_caller]
fn exits(home: &Path, daemon: i32, what: &str) {
    let mut patience = Patience::client(daemon.unsigned_abs(), home);
    while alive(daemon) {
        assert!(patience.holds(), "{what}");
        std::thread::sleep(patience.next_look());
    }
    patience.done();
}

/// Waits until no process's command line names `named`, each one's end waited for while it
/// goes on (`common::Wait` on its process, its kind its caller's call site), those that
/// outlive it named: a home's VM processes once its daemon has gone. A [`Patience`] of the
/// home follows what descends from its daemon, and so none of them once it has gone, their
/// party gone at its first look (CI 43968984258, x86_64 musl: `a_dead_daemons_waiting_vms_end`
/// refused 3.0 ms and `a_restored_microvm_ends_with_its_home` 1.8 ms in, "its members now:
/// none followed"). Listed again once those listed have ended, until none is, so that a
/// process the first look missed is waited for too: on Windows a last look once found what
/// no wait had followed, and the look after it disagreed (CI 44189693085: windows-2025's
/// `a_dead_daemons_waiting_vms_end` named VM process 9424, and windows-2022's
/// `a_killed_daemons_joined_microvms_end_with_it` failed naming none, `[]`).
#[track_caller]
fn all_end(named: &str, what: &str) {
    let kind = WaitKind::at(std::panic::Location::caller());
    loop {
        let left = commands_with(named);
        if left.is_empty() {
            return;
        }
        for (vm, _) in left {
            let mut ending = Wait::begin(kind, vm, None, "a VM process's end, its daemon gone");
            while alive(i32::try_from(vm).unwrap()) {
                if let Err(stall) = ending.look(false) {
                    panic!("{what}: {:?}: {}", commands_with(named), ending.refused(&stall));
                }
                std::thread::sleep(ending.next_look());
            }
            ending.answered();
        }
    }
}

fn daemon_pid(home: &Path) -> Option<i32> {
    std::fs::read_to_string(home.join("daemon.pid"))
        .ok()?
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .parse()
        .ok()
}

/// The files under `dir` whose names `named` takes that process `pid` holds open: of those
/// it lists open, from /proc on Linux and lsof elsewhere; on Windows, which lists no
/// process's files, of the files under `dir`, those the file query says the process
/// uses (FileProcessIdsUsingFileInformation: a file open or mapped).
fn held_under(pid: i32, dir: &Path, named: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let named = |p: &Path| p.file_name().and_then(|n| n.to_str()).is_some_and(&named);
    #[cfg(windows)]
    {
        files_under(dir)
            .into_iter()
            .filter(|f| named(f) && users(f).iter().any(|&u| i64::from(u) == i64::from(pid)))
            .collect()
    }
    #[cfg(not(windows))]
    {
        open_files(pid)
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.starts_with(dir) && named(p))
            .collect()
    }
}

#[cfg(windows)]
#[test]
fn windows_sees_a_files_users_until_its_last_mapping_is_closed() {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
        UnmapViewOfFile,
    };
    struct View(MEMORY_MAPPED_VIEW_ADDRESS);
    impl Drop for View {
        fn drop(&mut self) {
            // SAFETY: this view's mapping, released once before its section handle.
            unsafe { UnmapViewOfFile(self.0) };
        }
    }
    let home = TempDir::new("file-users");
    let path = home.join("memory");
    std::fs::File::create(&path).unwrap().set_len(65536).unwrap();
    let pid = std::process::id();
    assert!(!users(&path).contains(&pid), "closed file");
    let file = std::fs::File::open(&path).unwrap();
    assert!(users(&path).contains(&pid), "open file");
    // SAFETY: the open file's read-only section; no name or security attributes.
    let section = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        )
    };
    assert!(!section.is_null(), "{}", std::io::Error::last_os_error());
    // SAFETY: the new section handle, now owned until this test ends.
    let section = unsafe { OwnedHandle::from_raw_handle(section) };
    // SAFETY: a read-only view of the whole section; no requested address.
    let view = View(unsafe { MapViewOfFile(section.as_raw_handle(), FILE_MAP_READ, 0, 0, 0) });
    assert!(!view.0.Value.is_null(), "{}", std::io::Error::last_os_error());
    drop(file);
    assert!(users(&path).contains(&pid), "mapped with its file handle closed");
    drop(view);
    drop(section);
    assert!(!users(&path).contains(&pid), "unmapped and closed");
}

/// The files process `pid` holds open, by path: from /proc on Linux, from lsof elsewhere.
#[cfg(not(windows))]
fn open_files(pid: i32) -> Vec<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
    }
    let out = Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-Fn", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(str::to_owned)
        .collect()
}

/// Whether no daemon holds `home`'s lock: std's, which is flock(2) on Unix and LockFileEx
/// on Windows, as the daemon takes it, released as it closes.
fn home_is_free(home: &Path) -> bool {
    let lock = std::fs::File::open(home.join("daemon.lock")).unwrap();
    lock.try_lock().is_ok()
}

/// Each thread of process `pid`: its name, state and what it waits in (/proc/PID/task/*/
/// stat and wchan), on one line; elsewhere `ps -M`'s; on Windows, which shows them by no
/// tool of its own, none.
fn threads_of(pid: i32) -> String {
    if cfg!(windows) {
        return format!("process {pid}'s threads are not listed on Windows");
    }
    if !cfg!(target_os = "linux") {
        let ps = Command::new("ps").args(["-M", "-p", &pid.to_string()]).output();
        return ps
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
    }
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return "gone".into();
    };
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    tasks
        .flatten()
        .map(|t| {
            let state = read(t.path().join("stat"))
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
                .unwrap_or_default();
            format!(
                "{}/{state}/{}",
                read(t.path().join("comm")).trim(),
                read(t.path().join("wchan")).trim()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A client that must wait for its daemon to start ends of SIGTERM at once, as the Docker
/// CLI ends before its microVM is made: with 128 and the signal's number, nothing said
/// (docker/cli cmd/docker/docker.go, notifyContext). Its daemon cannot start here, the
/// home's lock held; then it is ended by its pid, which its log's lines begin with, before
/// anything is asserted, so that a failing test leaves no process behind. Windows sends a
/// console process no SIGTERM: there its interrupt ([`interrupt`]) ends it so, with 128 and
/// SIGINT's number.
#[test]
fn a_client_waiting_for_its_daemon_ends_on_sigterm() {
    // Named short: a socket is bound in it by its whole path (sockaddr_un).
    let home = TempDir::new("client-waits");
    let lock = std::fs::File::create(home.join("daemon.lock")).unwrap();
    // flock(2) on Unix, LockFileEx on Windows, as the daemon takes it: released as it closes.
    lock.try_lock().unwrap();
    let mut client = interruptible(
        common::command()
            .args(["run", "--pull", "never", "nope"])
            .env("SHARDS_HOME", &*home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .spawn()
    .unwrap();
    // Waiting for its daemon, which it started once its signals were taken: the daemon
    // has said something before it waits for the lock. Waited for while the client and its
    // daemon go on (`common::Wait`, its kind its call site).
    let log = home.join("daemon.log");
    let kind = WaitKind::at(std::panic::Location::caller());
    let mut started = Wait::begin(kind, client.id(), Some(&home), "its daemon's first word");
    while !std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
        assert!(
            client.try_wait().unwrap().is_none(),
            "the client ended before its daemon said anything"
        );
        if let Err(stall) = started.look(false) {
            panic!("its daemon said nothing: {}", started.refused(&stall));
        }
        std::thread::sleep(started.next_look());
    }
    started.answered();
    let signalled = Instant::now();
    #[cfg(unix)]
    // SAFETY: kill(2) of our own child, not yet waited for.
    unsafe {
        libc::kill(client.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(windows)]
    interrupt(&client);
    let ended = 128 + if cfg!(windows) { 2 } else { 15 };
    // Its end: waited for while it and its daemon go on, which a client waiting on its
    // daemon past its signal does not, both idle on the lock; how soon it came, told.
    let kind = WaitKind::at(std::panic::Location::caller());
    let mut ending = Wait::begin(kind, client.id(), Some(&home), "its end on its signal");
    let status = loop {
        if let Some(status) = client.try_wait().unwrap() {
            ending.answered();
            common::measured(
                "a client's end on its signal as it waits for its daemon",
                &[signalled.elapsed()],
            );
            break Some(status);
        }
        if ending.look(false).is_err() {
            let _ = client.kill();
            let _ = client.wait();
            break None;
        }
        std::thread::sleep(ending.next_look());
    };
    let mut said = String::new();
    let _ = client.stderr.take().unwrap().read_to_string(&mut said);
    // Its daemon, named by its log's lines (`shards daemon PID: ...`).
    let daemon = std::fs::read_to_string(home.join("daemon.log"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("shards daemon ")?
                .split_once(':')?
                .0
                .parse::<i32>()
                .ok()
        });
    // Its daemon ended.
    let looked = daemon.inspect(|&pid| kill_now(pid)).is_some();
    let status = status.expect("the client waited on past its signal");
    assert_eq!((status.code(), said.as_str()), (Some(ended), ""), "{status:?}");
    assert!(looked, "the client's daemon was not found in its log");
}

#[test]
fn parallel_runs_keep_their_own_stdio() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-parallel", &image);
    let runs: Vec<(usize, Child)> = (0..8)
        .map(|i| {
            let who = format!("WHO={i}");
            (i, spawn_run(&home, &["--pull", "never", "-e", &who, &image]))
        })
        .collect();
    for (i, mut child) in runs {
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        let status = wait(&mut child, &home);
        // A run its VM's host refused to enter, as D261's gate takes it for the host's, skips
        // the test, said so (CI 44189693085, x86_64 musl: run 4's vCPU 2, 0x7).
        if status != Some(0) && common::run_host_refused(&err, &home) {
            return;
        }
        assert_eq!(status, Some(0), "run {i}: {err}");
        let whos: Vec<&str> = out.lines().filter(|l| l.starts_with("env WHO=")).collect();
        assert_eq!(
            whos,
            [format!("env WHO={i}")],
            "run {i} saw another's stdio:\n{out}"
        );
    }
}

/// What runs cost this host as its daemon serves them (PM M357): runs one after another,
/// each client to its end; a detached run's start, and its microVM's CPU to it; 1, 2, 4
/// and 8 at once (`parallel_runs_keep_their_own_stdio` starts 8), round after round, with
/// the runs that failed counted; and an idle run's microVM's own CPU and its daemon's, over
/// ten seconds. Where the daemon boots every run cold (Windows until the parity plan's step
/// 4), what cold boots cost. Each result a `COSTS` line: n, p50, p90, p99 and max.
#[test]
#[ignore = "a measurement: SHARDS_COSTS_N=20 cargo test --profile test-release -p shards --test daemon -- --ignored --exact runs_cost_this_host --nocapture"]
fn runs_cost_this_host() {
    if cannot_run_vms() {
        return;
    }
    let n: usize = std::env::var("SHARDS_COSTS_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let (image, _) = served();
    let home = home("daemon-costs", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let say = |what: &str, times: Vec<Duration>| common::measured(&format!("COSTS {what}"), &times);
    let alone: Vec<Duration> = (0..n)
        .map(|_| {
            let began = Instant::now();
            let run = run_shards_env(&["run"], &["--rm", "--pull", "never", &image, "exit", "0"], &env);
            assert_eq!(run.status, Some(0), "{}", run.stderr);
            began.elapsed()
        })
        .collect();
    say("alone", alone);
    let (mut started, mut cpu) = (Vec::new(), Vec::new());
    for i in 0..n {
        let name = format!("boot{i}");
        let began = Instant::now();
        let run = run_shards_env(
            &["run"],
            &["-d", "--name", &name, "--pull", "never", &image, "sleep"],
            &env,
        );
        assert_eq!(run.status, Some(0), "{}", run.stderr);
        started.push(began.elapsed());
        let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", &name], &env);
        let vm: i32 = pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"));
        cpu.push(cpu_time(vm));
        let gone = run_shards_env(&["rm"], &["-f", &name], &env);
        assert_eq!(gone.status, Some(0), "{}", gone.stderr);
    }
    say("a detached run started", started);
    say("its microVM's CPU to its start", cpu);
    for at_once in [1, 2, 4, 8] {
        let (mut each, mut rounds, mut failed) = (Vec::new(), Vec::new(), 0);
        for _ in 0..(n / at_once).max(1) {
            let began = Instant::now();
            let mut runs: Vec<Option<Child>> = (0..at_once)
                .map(|_| {
                    Some(
                        common::command()
                            .args(["run", "--rm", "--pull", "never", &image, "exit", "0"])
                            .env("SHARDS_HOME", &*home)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .unwrap(),
                    )
                })
                .collect();
            // Waited on while its daemon and microVMs go on ([`Patience`]).
            let mut patience = Patience::of(&home);
            while runs.iter().any(Option::is_some) {
                assert!(
                    patience.holds(),
                    "{at_once} runs at once stopped going on, {:?} in",
                    began.elapsed()
                );
                for slot in &mut runs {
                    if let Some(run) = slot
                        && let Some(status) = run.try_wait().unwrap()
                    {
                        if status.success() {
                            each.push(began.elapsed());
                        } else {
                            failed += 1;
                        }
                        *slot = None;
                    }
                }
                std::thread::sleep(patience.next_look());
            }
            patience.done();
            rounds.push(began.elapsed());
        }
        println!("COSTS {at_once} at once: {failed} failed");
        if !each.is_empty() {
            say(&format!("{at_once} at once, each"), each);
        }
        say(&format!("{at_once} at once, the round"), rounds);
    }
    let idle = run_shards_env(
        &["run"],
        &["-d", "--name", "idle", "--pull", "never", &image, "sleep"],
        &env,
    );
    assert_eq!(idle.status, Some(0), "{}", idle.stderr);
    let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", "idle"], &env);
    let vm: i32 = pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"));
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // Settled first: its boot's own CPU is the cold boots' above.
    std::thread::sleep(Duration::from_secs(5));
    let (vm_before, daemon_before) = (cpu_time(vm), cpu_time(daemon));
    std::thread::sleep(Duration::from_secs(10));
    let (vm_spent, daemon_spent) = (
        cpu_time(vm).saturating_sub(vm_before),
        cpu_time(daemon).saturating_sub(daemon_before),
    );
    println!("COSTS idle over 10 s: its microVM {vm_spent:?} of CPU, its daemon {daemon_spent:?}");
    let killed = run_shards_env(&["rm"], &["-f", "idle"], &env);
    assert_eq!(killed.status, Some(0), "{}", killed.stderr);
}

/// Signals reach the command, even when the client was started ignoring them, as a
/// script's `shards run ... &` is: `docker run`'s signal proxy takes them all the same.
#[test]
fn signals_reach_a_warm_run() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-signals", &image);
    for ignored in [false, true] {
        // A Windows console starts no process with its break ignored, as a shell's `cmd &`
        // starts its command ignoring SIGINT and SIGQUIT.
        if ignored && cfg!(windows) {
            continue;
        }
        let mut run = common::command();
        run.args(["run", "--pull", "never", &image, "trap", "INT"])
            .env("SHARDS_HOME", &*home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        interruptible(&mut run);
        #[cfg(unix)]
        if ignored {
            // As a non-interactive shell starts `cmd &` (POSIX.1-2024, XCU 2.9.3.1).
            // SAFETY: signal(2) only, between fork and exec.
            unsafe {
                run.pre_exec(|| {
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                    libc::signal(libc::SIGQUIT, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let mut child = run.spawn().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        assert_eq!(line, "ready\n");
        interrupt(&child);
        // Within a run's time, or while its daemon or microVM progress: a signal lost
        // leaves its command waiting, and a read of its output would wait with it (2 hours
        // in budget E2E 40170179004, cancelled).
        let (read, rest) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = String::new();
            let _ = out.read_to_string(&mut rest);
            let _ = read.send(rest);
        });
        let mut patience = Patience::of(&home);
        let rest = loop {
            match rest.recv_timeout(patience.next_look()) {
                Ok(rest) => {
                    patience.done();
                    break rest;
                }
                Err(_) if patience.holds() => {}
                Err(_) => {
                    let _ = child.kill();
                    panic!("the run outlived its SIGINT and its progress; ignored at start: {ignored}");
                }
            }
        };
        assert_eq!(wait(&mut child, &home), Some(0), "ignored at start: {ignored}");
        assert_eq!(rest, "got 2\n", "ignored at start: {ignored}");
    }
}

#[test]
fn errors_reach_the_client() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-errors", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let missing = run_shards_env(&["run"], &["--pull", "never", "no/such:image"], &env);
    assert_eq!(missing.status, Some(125), "{}", missing.stderr);
    assert!(missing.stderr.contains("No such image"), "{}", missing.stderr);
    let not_found = run_shards_env(
        &["run"],
        &["--pull", "never", "--entrypoint", "/bin/nonexistent", &image],
        &env,
    );
    assert_eq!(not_found.status, Some(127), "{}", not_found.stderr);
}

/// A stopped microVM's next run, served by a microVM its image's template restored, goes
/// on over the file its layer was kept in (D190), where the home's restarts say going over
/// is the quicker (`common::restarts_cross_at`): the template's own layer let go of, the
/// microVM's file under the device, what its run before made there, and the file the
/// microVM's still, the same one, once it stops again.
#[test]
fn a_restored_microvm_goes_on_over_a_stopped_containers_kept_file() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-over", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    common::restarts_cross_at(&home, 4 << 20);
    let first = run_shards_env(
        &["run"],
        &[
            "--pull",
            "never",
            "--name",
            "over",
            "-u",
            "0",
            &image,
            "fs",
            "write:/kept=one",
            // Past where the home's restarts cross.
            "fill:8388608:/held",
            "mkdir:/made",
        ],
        &env,
    );
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    let id = run_shards_env(&["inspect"], &["-f", "{{.Id}}", "over"], &env);
    let dir = home.join("containers").join(id.stdout.trim());
    let kept = dir.join("layer.ext4");
    let settled = || {
        eventually(&home, "no layer kept", || {
            kept.exists() && !dir.join("layer.new").exists()
        })
    };
    settled();
    let same = common::file_identity(&kept);
    for n in 0..2 {
        let again = run_shards_env(&["start"], &["-a", "over"], &env);
        assert_eq!(again.status, Some(1), "run {n}: {}", again.stderr);
        assert!(
            again.stderr.contains("mkdir:/made: File exists"),
            "run {n}: {}",
            again.stderr
        );
        settled();
        assert_eq!(
            common::file_identity(&kept),
            same,
            "run {n}: another file in its place"
        );
    }
}

/// A warm VM's run starts without growing its process's descriptor table: Linux grows a
/// table its threads share only after an RCU grace period (fs/file.c, `expand_fdtable`),
/// which held a run's hand-over 8 to 15 ms on a 32-CPU host whose warm VMs' 64
/// descriptors filled theirs (PM M470). Each warm VM makes room for its run's first.
#[cfg(target_os = "linux")]
#[test]
fn a_warm_vms_run_starts_without_growing_its_descriptor_table() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-fd-room", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    let table = |pid: u32| -> Option<u64> {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("FDSize:"))?
            .trim()
            .parse()
            .ok()
    };
    // What a VM's thread named `name` waits in (wchan), if it has one.
    let waits = |pid: u32, name: &str| -> Option<String> {
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .ok()?
            .flatten()
            .find(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == name))
            .and_then(|t| std::fs::read_to_string(t.path().join("wchan")).ok())
    };
    // The pool's VMs waiting for a run, their room made: each one's workload thread in its
    // read of the daemon's socket, which the kernel names by the function it sleeps in:
    // unix_stream_data_wait up to 6.x, unix_stream_read_generic on 7.2 (adacachy's
    // 7.2.9-cachyos), both of af_unix's stream read.
    let mut idle: Vec<(u32, u64)> = Vec::new();
    eventually(&home, "a warm VM waited for its run", || {
        idle = commands_with(&templates)
            .into_iter()
            .filter(|(pid, args)| {
                args.contains(" restore ")
                    && waits(*pid, "workload").is_some_and(|w| w.starts_with("unix_stream_"))
            })
            .filter_map(|(pid, _)| Some((pid, table(pid)?)))
            .collect();
        !idle.is_empty()
    });
    let mut sleeper = spawn_run(&home, &["--pull", "never", &image, "sleep"]);
    let mut line = String::new();
    BufReader::new(sleeper.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    // The run's VM: the one relaying its client's signals.
    let served: Vec<&(u32, u64)> = idle
        .iter()
        .filter(|(pid, _)| waits(*pid, "client-signals").is_some_and(|w| w.starts_with("unix_stream_")))
        .collect();
    assert_eq!(served.len(), 1, "the run's VM among the pool's {idle:?}");
    for &&(pid, before) in &served {
        assert_eq!(
            table(pid),
            Some(before),
            "VM {pid}'s descriptor table grew as its run started"
        );
    }
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

/// `stop` ends the runs in progress as it shuts down (D26): SIGTERM to the command, whose
/// client gets its status. The pool's waiting VMs end too. The run is under --init, so that
/// its command, which does not handle SIGTERM, is not PID 1, which would not hear it and
/// would be killed at the stop timeout instead (D115).
#[test]
fn stop_ends_runs_and_waiting_vms() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stop", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    let mut sleeper = spawn_run(&home, &["--pull", "never", "--init", &image, "sleep"]);
    let mut out = BufReader::new(sleeper.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    let daemon = daemon_pid(&home).expect("a daemon pid");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    // It has let go of its home; the process ends as the kernel finishes its exit, and
    // init, which adopted it, reaps it (kill(2) finds a zombie too).
    assert!(home_is_free(&home), "stop returned before the daemon exited");
    exits(&home, daemon, "the daemon outlived its stop");
    assert_eq!(
        wait(&mut sleeper, &home),
        Some(128 + 15),
        "the run was not stopped by SIGTERM"
    );
    all_end(&templates, "VMs outlived the daemon's stop");
}

/// `daemon stop` stops each run as `stop vm` would (D26): with its own stop signal, and
/// SIGKILL once its own stop timeout is up.
#[test]
fn a_stop_gives_each_run_its_own_signal_and_time() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stop-own", &image);
    let started = |args: &[&str]| {
        let mut run = spawn_run(&home, args);
        let mut line = String::new();
        BufReader::new(run.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "ready\n");
        run
    };
    let mut caught = started(&["--pull", "never", "--stop-signal", "USR1", &image, "trap", "USR1"]);
    let mut ignoring = started(&[
        "--pull",
        "never",
        "--stop-signal",
        "USR1",
        "--stop-timeout",
        "1",
        &image,
        "ignore",
        "USR1",
    ]);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let t0 = Instant::now();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    let took = t0.elapsed();
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    assert_eq!(
        wait(&mut caught, &home),
        Some(0),
        "its own signal, caught: {}",
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    let mut said = String::new();
    std::io::Read::read_to_string(caught.stdout.as_mut().unwrap(), &mut said).unwrap();
    // Linux numbers SIGUSR1 10.
    assert_eq!(said, "got 10\n");
    assert_eq!(
        wait(&mut ignoring, &home),
        Some(128 + 9),
        "killed once its own time was up: {}",
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    // Its own stop timeout waited out (`--stop-timeout 1`); how long past it the stop
    // took is no later event's to order, and is told (D209).
    assert!(took >= Duration::from_secs(1), "{took:?}");
    common::measured(
        "a daemon's stop of runs with their own stop signal and a 1 s stop timeout",
        &[took],
    );
}

/// A stop that finds its daemon stopping already, at another stop's word, returns once that
/// daemon has let go of its home, as the first does (D185): before, it took the RESTART it
/// was answered, or the slot it found none of, for the daemon's exit, and returned at once,
/// while the daemon still ended its run and held the home's lock.
#[test]
fn a_stop_while_another_stops_its_daemon_waits_for_its_home_let_go_of() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stop-twice", &image);
    // Its stop signal ignored, the run ends at its stop timeout's SIGKILL: the stop's span.
    let mut ignoring = spawn_run(
        &home,
        &[
            "--pull",
            "never",
            "--stop-signal",
            "USR1",
            "--stop-timeout",
            "5",
            &image,
            "ignore",
            "USR1",
        ],
    );
    let mut line = String::new();
    BufReader::new(ignoring.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let log = home.join("daemon.log");
    std::thread::scope(|scope| {
        let first = scope.spawn(|| run_shards_env(&["daemon"], &["stop"], &env));
        // The second asks once the first has its daemon stepping aside, as its log says.
        eventually(&home, "the first stop taken", || {
            std::fs::read_to_string(&log)
                .is_ok_and(|said| said.contains("stepping aside: a client asked it to stop"))
        });
        let second = run_shards_env(&["daemon"], &["stop"], &env);
        assert_eq!(second.status, Some(0), "{}", second.stderr);
        assert!(
            home_is_free(&home),
            "the second stop returned while its daemon held its home"
        );
        let first = first.join().unwrap();
        assert_eq!(first.status, Some(0), "{}", first.stderr);
        assert!(
            home_is_free(&home),
            "the first stop returned while its daemon held its home"
        );
    });
    assert_eq!(
        wait(&mut ignoring, &home),
        Some(128 + 9),
        "killed once its stop timeout was up: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// A daemon ending its runs is waited for as long as their stop timeouts make it: a run
/// asked for meanwhile is served by the next daemon once it is gone.
#[test]
fn a_daemon_ending_its_runs_is_waited_for() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stop-waited", &image);
    // It takes 35 s to end: past the 20 s a daemon waits for another's lock otherwise,
    // and the 30 s a client waits for a daemon it started.
    let mut slow = spawn_run(
        &home,
        &[
            "--pull",
            "never",
            "--stop-timeout",
            "35",
            &image,
            "ignore",
            "TERM",
        ],
    );
    let mut line = String::new();
    BufReader::new(slow.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    let before = daemon_pid(&home).expect("a daemon pid");
    let mut stopping = common::command()
        .args(["daemon", "stop"])
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Ending its runs, and no longer offering slots: a run asked for before then is one it
    // serves itself.
    eventually(&home, "the daemon did not begin to stop", || {
        home.join("daemon.stopping").exists() && !shards_ipc::slots::served(&home)
    });
    let env = [("SHARDS_HOME", home.as_os_str())];
    let t0 = Instant::now();
    let next = run_shards_env(&["run"], &["--pull", "never", &image, "exit", "0"], &env);
    assert_eq!(next.status, Some(0), "{}", next.stderr);
    assert!(t0.elapsed() >= Duration::from_secs(30), "{:?}", t0.elapsed());
    assert_eq!(wait(&mut slow, &home), Some(128 + 9));
    assert_eq!(wait(&mut stopping, &home), Some(0));
    assert!(!alive(before), "the old daemon still lives");
    assert_ne!(daemon_pid(&home), Some(before));
}

/// A daemon that dies leaves no VM waiting for a run it will never send.
#[test]
fn a_dead_daemons_waiting_vms_end() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-killed", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    eventually(&home, "the pool did not fill", || {
        !processes_with(&templates).is_empty()
    });
    let daemon = daemon_pid(&home).expect("a daemon pid");
    kill_now(daemon);
    all_end(&templates, "warm VMs outlived their daemon");
}

/// A client from another build replaces the daemon, and its run is served, its stdin whole:
/// none of it went to the daemon that stepped aside (HEARD, D185). Another file is another
/// build: the daemon knows `shards` by its file's identity.
#[test]
fn a_rebuilt_binary_replaces_the_daemon() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-rebuilt", &image);
    let before = daemon_pid(&home).expect("a daemon pid");
    let bin = TempDir::new("daemon-rebuilt-bin");
    let other = bin.join(format!("shards{}", std::env::consts::EXE_SUFFIX));
    // Copied by a process of its own: a copy this process wrote is open for writing in each
    // child another test's thread forks meanwhile, until that child execs, and running it
    // fails with ETXTBSY until then (x86_64 CI, 2026-10-11).
    shards_testdir::program_copy(shards(), &other).unwrap();
    let mut run = Command::new(&other)
        .args(["run", "-i", "--pull", "never", &image, "cat"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_LOCAL_STORE", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let said = "given before the old daemon stepped aside\n";
    run.stdin.take().unwrap().write_all(said.as_bytes()).unwrap();
    // Its output read as it comes, and the client ended past the deadline: a stdin lost
    // leaves `cat` waiting for ever.
    let read = |mut from: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut all = String::new();
            let _ = from.read_to_string(&mut all);
            all
        })
    };
    let (out, err) = (
        read(Box::new(run.stdout.take().unwrap())),
        read(Box::new(run.stderr.take().unwrap())),
    );
    let mut patience = Patience::of(&home);
    let status = loop {
        if let Some(status) = run.try_wait().unwrap() {
            patience.done();
            break Some(status);
        }
        if !patience.holds() {
            let _ = run.kill();
            let _ = run.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    assert_eq!(
        (status.and_then(|s| s.code()), out.as_str()),
        (Some(0), said),
        "{err}"
    );
    let after = daemon_pid(&home).expect("a daemon pid");
    assert_ne!(before, after, "the old daemon still serves");
    exits(&home, before, "the old daemon did not exit");
}

/// The event that a daemon's idle time of a second (`SHARDS_DAEMON_IDLE=1`) has run, which
/// no fixed sleep past it stands for (D209): a daemon of this build, started now in a home
/// of its own with that idle time and nothing to hold it, waited for until it exits. A
/// daemon whose idle time began before this one's would have exited by then, were nothing
/// holding it.
#[track_caller]
fn an_idle_time_ran() {
    let home = TempDir::new("daemon-sibling");
    let env: [(&str, &OsStr); 2] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_DAEMON_IDLE", "1".as_ref()),
    ];
    let started = run_shards_env(&["daemon"], &["--detached"], &env);
    assert_eq!(started.status, Some(0), "{}", started.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    exits(
        &home,
        daemon,
        "a daemon with nothing to hold it did not exit idle",
    );
}

#[test]
fn idle_daemons_exit() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = TempDir::new("daemon-idle");
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 4] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_DAEMON_IDLE", "1".as_ref()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    let run = run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    exits(&home, daemon, "the idle daemon did not exit");
    assert!(daemon_pid(&home).is_none(), "the daemon left its pid file");
}

/// A stream the Engine API answers ends with its client: an events stream whose client
/// leaves while nothing happens ends then, not at the next event, which may never come, and
/// the daemon, kept while the stream was open, goes idle after it. On Windows the stream is
/// the named pipe the home's `engine.pipe` names (D170).
#[test]
fn an_engine_stream_ends_with_its_client() {
    let home = TempDir::new("engine-gone");
    let env: [(&str, &OsStr); 2] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_DAEMON_IDLE", "1".as_ref()),
    ];
    let started = run_shards_env(&["daemon"], &["--detached"], &env);
    assert_eq!(started.status, Some(0), "{}", started.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    {
        #[cfg(unix)]
        let mut events = UnixStream::connect(home.join("engine.sock")).unwrap();
        #[cfg(windows)]
        let mut events = {
            let name = std::fs::read_to_string(home.join("engine.pipe")).unwrap();
            shards_ipc::Stream::connect(name.trim()).unwrap()
        };
        events
            .write_all(b"GET /events HTTP/1.1\r\nHost: api\r\n\r\n")
            .unwrap();
        // Its answer's head, read as its daemon goes on (`common::judged`); refused, the
        // daemon is ended, which ends the read.
        let mut head = [0u8; 15];
        let read = common::judged(
            common::Party::own(Some(&home)),
            "the events stream's head",
            || kill_now(daemon),
            || events.read_exact(&mut head),
        );
        assert!(matches!(read, Ok(Ok(()))), "{read:?}");
        assert_eq!(&head, b"HTTP/1.1 200 OK");
        // Past its idle time, as another daemon's idle exit begun after it shows, with the
        // stream open: it stays.
        an_idle_time_ran();
        assert!(alive(daemon), "the daemon exited while a stream was open");
    }
    exits(&home, daemon, "the daemon kept a stream whose client had gone");
}

/// A daemon's idle time runs from its last run's end, which nothing else need follow: the
/// run's own end starts the clock, long after its client left the daemon. Its home holds
/// the image's template already, so the run changes nothing there the daemon watches.
#[test]
fn a_daemon_idles_from_its_last_runs_end() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-idle-end", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let mut client = common::command()
        .args(["run", "-i", "--pull", "never", &image, "cat"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_DAEMON_IDLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = client.stdin.take().unwrap();
    stdin.write_all(b"running\n").unwrap();
    let mut out = BufReader::new(client.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "running\n");
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // Past its idle time, as another daemon's idle exit begun after its run's start shows,
    // but with a run: it stays.
    an_idle_time_ran();
    assert!(alive(daemon), "the daemon exited while a run went on");
    drop(stdin);
    assert_eq!(wait(&mut client, &home), Some(0));
    exits(
        &home,
        daemon,
        "the daemon did not exit once idle after its last run",
    );
}

/// With -i the command reads the client's stdin, and that stdin ends when the client
/// goes, as `docker run -i`'s does (StdinOnce): `cat` then ends, and its VM with it.
#[test]
fn interactive_stdin_ends_with_its_client() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stdin", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    // A warm VM to serve the run, and the template's processes as the run is asked for:
    // a refill is one of them new since, other than the run's own (as its microVM says,
    // below). No window of quiet stands for the pool's settling (D209): a VM of a run
    // before that goes later is among those as it was asked for, never a refill.
    eventually(&home, "no warm VM", || !processes_with(&templates).is_empty());
    let waiting = processes_with(&templates);
    let mut client = common::command()
        .args(["run", "-i", "--pull", "never", "--name", "reader", &image, "cat"])
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = client.stdin.take().unwrap();
    stdin.write_all(b"through the client\n").unwrap();
    let mut out = BufReader::new(client.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "through the client\n");
    // The VM serving it, as its microVM says, rather than the one of those waiting that
    // went: a run's VM tells its end before it goes, and one that took a loaded runner past
    // the pool's settling was counted as waiting, and its going as this one's.
    let env = [("SHARDS_HOME", home.as_os_str())];
    let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", "reader"], &env);
    let vm: u32 = pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"));
    // The pool refills while the run goes on: a VM of the template's new since it settled.
    let refilled = std::cell::Cell::new(None);
    eventually(&home, "the pool did not refill", || {
        refilled.set(
            processes_with(&templates)
                .into_iter()
                .find(|p| *p != vm && !waiting.contains(p)),
        );
        refilled.get().is_some()
    });
    client.kill().unwrap();
    client.wait().unwrap();
    // The VM that served it ends; the one refilled for the next run stays.
    eventually(&home, "the run outlived its client's stdin", || {
        !processes_with(&templates).contains(&vm)
    });
    let refilled = refilled.get().unwrap();
    assert!(
        alive(refilled as i32),
        "the VM refilled, {refilled}, went with the run's"
    );
}

/// `SHARDS_TIMING` reaches a warm run's client, which prints the line the VM measured.
#[test]
fn timing_reaches_the_client() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-timing", &image);
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_TIMING", "1".as_ref())];
    let run = run_shards_env(&["run"], &["--pull", "never", &image, "exit", "0"], &env);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let (request, answered) = (run.request_us(), run.answered_us());
    assert!(
        matches!((request, answered), (Some(q), Some(a)) if q > 0 && a >= q),
        "{}",
        run.stderr
    );
}

/// With -i on a terminal the client is the one reading it, so the terminal's job control
/// applies: in the background, the run stops for input as any reader does. The test
/// binary runs itself as the shell: it leads a session on a new pseudo-terminal, keeps
/// the foreground, and starts the client in a group of its own, then reports how the
/// client stopped. Job control is Unix's: a Windows console has no background group, nor a
/// stop for its input (SIGTTIN).
#[cfg(unix)]
#[test]
fn a_background_run_stops_to_read_its_terminal() {
    if let Some(args) = std::env::var_os("SHARDS_TEST_SHELL") {
        return shell(&args);
    }
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-tty", &image);
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_background_run_stops_to_read_its_terminal",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SHARDS_TEST_SHELL", format!("{}\n{image}", shards().display()))
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_LOCAL_STORE", "none")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    // libtest's own line may precede it on the same line.
    let stop = said
        .lines()
        .find_map(|l| l.split_once("client stopped by ").map(|(_, sig)| sig));
    assert_eq!(
        stop,
        Some(libc::SIGTTIN.to_string().as_str()),
        "{said}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The shell of `a_background_run_stops_to_read_its_terminal`: `args` is the shards
/// binary and the image, a line each.
#[cfg(unix)]
fn shell(args: &OsStr) {
    use std::os::unix::process::CommandExt;
    let args = args.to_string_lossy();
    let (bin, image) = args.split_once('\n').unwrap();
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty(3) fills both descriptors; setsid(2) and ioctl(2) make this process
    // a session leader whose controlling terminal is the new one, its group in the
    // foreground.
    unsafe {
        let (name, termp, winp) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(libc::openpty(&mut master, &mut slave, name, termp, winp), 0);
        assert!(libc::setsid() > 0);
        assert_eq!(libc::ioctl(slave, libc::TIOCSCTTY as _, 0), 0);
    }
    let mut command = Command::new(bin);
    command
        .args(["run", "-i", "--pull", "never", image, "cat"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: runs in the child between fork and exec, calling only setpgid(2), dup2(2)
    // and close(2), which are async-signal-safe: the client leads a background group of
    // this session, with the terminal as its stdin.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) < 0 || libc::dup2(slave, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(slave);
            libc::close(master);
            Ok(())
        });
    }
    let mut client = command.spawn().unwrap();
    let pid = client.id() as libc::pid_t;
    // Its stop or end, waited for while it goes on (`common::Wait`, its kind its call site).
    let kind = WaitKind::at(std::panic::Location::caller());
    let mut stopping = Wait::begin(kind, client.id(), None, "its stop for its terminal's input");
    let mut status = 0;
    loop {
        // SAFETY: waitpid(2) for our own child, stops included.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == pid {
            stopping.answered();
            break;
        }
        if stopping.look(false).is_err() {
            break;
        }
        std::thread::sleep(stopping.next_look());
    }
    let report = if libc::WIFSTOPPED(status) {
        format!("client stopped by {}", libc::WSTOPSIG(status))
    } else {
        format!("client did not stop: status {status}")
    };
    let _ = writeln!(std::io::stdout(), "{report}");
    client.kill().unwrap();
    client.wait().unwrap();
}

/// Starts `home`'s daemon with `shards ps`, which needs no VM; its pid.
fn start_daemon(home: &Path) -> i32 {
    let mut cmd = common::command();
    cmd.arg("ps");
    let out = cmd
        .env("SHARDS_HOME", home)
        .env("SHARDS_LOCAL_STORE", "none")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    daemon_pid(home).expect("a daemon pid")
}

/// Starts `home`'s daemon with `shards daemon --detached`, under a limit of `limit` open
/// descriptors, which the daemon inherits: started, it has served no request, so it has
/// learned no request's rest (`slots::Seen`, PM M545); its pid.
#[cfg(unix)]
fn start_idle_daemon(home: &Path, limit: u32) -> i32 {
    let mut cmd = common::command();
    cmd.args(["daemon", "--detached"]);
    descriptor_limit(&mut cmd, u64::from(limit), true);
    let out = cmd
        .env("SHARDS_HOME", home)
        .env("SHARDS_LOCAL_STORE", "none")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    daemon_pid(home).expect("a daemon pid")
}

/// Has `cmd` start its program under a limit of `limit` open descriptors, the soft limit and,
/// where `hard`, the hard one too, as a shell's `ulimit -n` sets both and `ulimit -S -n` the
/// soft alone: with no shell, which a host need not have.
#[cfg(unix)]
fn descriptor_limit(cmd: &mut Command, limit: u64, hard: bool) {
    // SAFETY: getrlimit(2) and setrlimit(2) are async-signal-safe, and the closure touches
    // nothing but its own copies of the limit.
    unsafe {
        cmd.pre_exec(move || {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            lim.rlim_cur = limit as libc::rlim_t;
            if hard {
                lim.rlim_max = limit as libc::rlim_t;
            }
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Raises this process's soft limit on open descriptors as far as it goes, up to macOS's
/// OPEN_MAX: a test holding hundreds of connections needs more than the 256 macOS starts
/// a process with. Windows has none to raise ([`a_daemon_raises_its_descriptor_limit`]).
#[cfg(windows)]
fn more_descriptors() {}

#[cfg(unix)]
fn more_descriptors() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) and setrlimit(2) on locals.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        lim.rlim_cur = lim.rlim_max.min(10240);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
    }
}

/// A client of a slot of its daemon's (D185) that says nothing past its first byte, which
/// names the streams it opened (none): its `say`, held, and its `answer`, where the daemon
/// tells it what it tells it.
struct Silent {
    _say: shards_ipc::slots::Line,
    answer: shards_ipc::slots::Line,
}

/// What a look for a slot of a daemon's found ([`try_slot`]).
enum Taken {
    /// A slot, taken.
    Slot(Silent),
    /// None offered as it looked.
    Busy,
    /// No daemon offers any.
    Gone,
}

/// A slot of `home`'s daemon taken by hand, as a client takes one (shards_ipc::slots), its
/// first byte written and nothing after: patient, as a client is, while none is offered and
/// its daemon and microVMs go on ([`Patience`], its kind its caller's call site); `None`
/// where no daemon offers any, or its wait is refused.
#[track_caller]
fn silent_slot(home: &Path) -> Option<Silent> {
    let mut patience = Patience::of(home);
    loop {
        match try_slot(home) {
            Taken::Slot(slot) => {
                patience.done();
                return Some(slot);
            }
            Taken::Gone => return None,
            Taken::Busy if !patience.holds() => return None,
            Taken::Busy => std::thread::sleep(patience.next_look()),
        }
    }
}

/// One look for a slot of `home`'s daemon, taken as [`silent_slot`] takes one, waiting for
/// none. On Windows a slot is an instance of the pipe the list's file names, overlapped, as a
/// client opens it, which says its number first.
#[cfg(windows)]
fn try_slot(home: &Path) -> Taken {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
    let list = home.join("requests").join("free");
    // No list: no daemon.
    let Ok(name) = std::fs::read_to_string(&list) else {
        return Taken::Gone;
    };
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(name.trim());
    let pipe = match opened {
        Ok(file) => shards_ipc::Pipe::from_handle(file.into()),
        // Every instance taken, or the list made again.
        Err(e)
            if e.raw_os_error() == i32::try_from(ERROR_PIPE_BUSY).ok()
                || e.kind() == std::io::ErrorKind::NotFound =>
        {
            return Taken::Busy;
        }
        Err(e) => panic!("{}: {e}", name.trim()),
    };
    let mut record = [0u8; 16];
    if (&pipe).read_exact(&mut record).is_err() {
        return Taken::Gone;
    }
    std::str::from_utf8(&record)
        .unwrap()
        .trim_end()
        .parse::<u64>()
        .unwrap();
    (&pipe).write_all(&[0]).unwrap();
    Taken::Slot(Silent {
        _say: pipe.try_clone().unwrap(),
        answer: pipe,
    })
}

#[cfg(unix)]
fn try_slot(home: &Path) -> Taken {
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = home.join("requests");
    let Ok(mut free) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dir.join("free"))
    else {
        return Taken::Gone;
    };
    // A slot removed since it was offered, its record left on the list, is passed over for
    // the next, as a client passes it (`shards_ipc::slots::take`): one whose thread ended
    // before its client came, as a daemon out of descriptors may (KVM, ENOENT on its `say`).
    let removed = |e: &std::io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENXIO));
    loop {
        let mut record = [0u8; 16];
        match free.read(&mut record) {
            Ok(16) => {}
            // No writer: no daemon.
            Ok(0) => return Taken::Gone,
            Ok(n) => panic!("a record of {n} bytes on free"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Taken::Busy,
            Err(e) => panic!("free: {e}"),
        }
        let n: u64 = std::str::from_utf8(&record).unwrap().trim_end().parse().unwrap();
        // Opened as a client opens them (`shards_ipc::slots` `open_slot`), neither waiting:
        // `answer` first, which a read's open takes with no writer, then `say`, which a
        // write's open takes only while its daemon reads it (ENXIO), so that a daemon gone
        // after it offered the slot is passed over, never waited for. Opened blocking, a test
        // waited 17 min in open(2) of `answer` for a daemon that had died as it wrote slot
        // 10's record (silent_slot, at b3df6c4470).
        let open = |name: &str, write: bool| {
            std::fs::OpenOptions::new()
                .read(!write)
                .write(write)
                .custom_flags(libc::O_NONBLOCK)
                .open(dir.join(format!("{n}.{name}")))
        };
        let answer = match open("answer", false) {
            Ok(answer) => blocking(answer),
            Err(e) if removed(&e) => continue,
            Err(e) => panic!("slot {n}'s answer: {e}"),
        };
        let mut say = match open("say", true) {
            Ok(say) => blocking(say),
            Err(e) if removed(&e) => continue,
            Err(e) => panic!("slot {n}'s say: {e}"),
        };
        say.write_all(&[0]).unwrap();
        return Taken::Slot(Silent { _say: say, answer });
    }
}

/// `file` with O_NONBLOCK cleared, as [`try_slot`] reads and writes its slot.
#[cfg(unix)]
fn blocking(file: std::fs::File) -> std::fs::File {
    use std::os::fd::AsRawFd as _;
    // SAFETY: fcntl(2) of a descriptor `file` owns, reading then setting its status flags.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: as above.
    let set = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
    file
}

/// A slot whose daemon has gone since it offered it is passed over, never waited for: its
/// record still on `free`, which another holds open to write, and its `answer` and `say`
/// there with no daemon at their other ends. Do: such a home, made by hand. Expect a look
/// to find no slot while `free` has a writer, and no daemon once it has none. Red: the
/// slot's FIFOs opened blocking, as at b3df6c4470, where the look waits in open(2) of
/// `answer` for ever (17 min, its sample in open under `try_slot` under `silent_slot`).
#[cfg(unix)]
#[test]
fn a_slot_whose_daemon_has_gone_is_passed_over() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let home = TempDir::new("slot-gone");
    let dir = home.join("requests");
    std::fs::create_dir(&dir).unwrap();
    let fifo = |name: &str| {
        let path = std::ffi::CString::new(dir.join(name).to_str().unwrap()).unwrap();
        // SAFETY: mkfifo(3) of a NUL-terminated path in the test's own directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    };
    for name in ["free", "10.answer", "10.say"] {
        fifo(name);
    }
    // The daemon's end of `free` as it wrote slot 10's record, held as it held it.
    let free = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(dir.join("free"))
        .unwrap();
    (&free).write_all(format!("{:015}\n", 10).as_bytes()).unwrap();
    assert!(
        matches!(try_slot(&home), Taken::Busy),
        "a slot taken with no daemon"
    );
    drop(free);
    assert!(matches!(try_slot(&home), Taken::Gone), "a daemon found with none");
}

/// How many bytes wait in the pipe `out` reads: FIONREAD on Unix, PeekNamedPipe on Windows.
fn waiting_in(out: &std::process::ChildStdout) -> usize {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD of the pipe's end this test holds, into an int on this stack.
        let asked = unsafe { libc::ioctl(out.as_raw_fd(), libc::FIONREAD, &mut waiting) };
        assert_eq!(asked, 0);
        usize::try_from(waiting).unwrap()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let mut waiting = 0u32;
        // SAFETY: a peek of no bytes at the pipe's end this test holds, its count on this stack.
        let asked = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                out.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut waiting,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(asked, 0, "{}", std::io::Error::last_os_error());
        usize::try_from(waiting).unwrap()
    }
}

/// The CPU time process `pid` has used, on Windows: its kernel and user times
/// (GetProcessTimes), in 100 ns.
#[cfg(windows)]
fn cpu_time(pid: i32) -> Duration {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let (mut made, mut ended, mut kernel, mut user) = Default::default();
    // SAFETY: a process's handle opened, read and closed here; four FILETIMEs on this stack.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, u32::try_from(pid).unwrap());
        assert!(!process.is_null(), "process {pid} could not be opened");
        let read = GetProcessTimes(process, &mut made, &mut ended, &mut kernel, &mut user);
        CloseHandle(process);
        assert_ne!(
            read,
            0,
            "process {pid}'s times: {}",
            std::io::Error::last_os_error()
        );
    }
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

/// The CPU time process `pid` has used: from /proc on Linux, from `ps` elsewhere.
#[cfg(unix)]
fn cpu_time(pid: i32) -> Duration {
    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // After the command's closing parenthesis come the state (field 3) and the rest:
        // utime and stime are fields 14 and 15, in clock ticks.
        let fields: Vec<u64> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .skip(1)
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        // SAFETY: sysconf(3) takes no pointers.
        let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap();
        return Duration::from_millis((fields[10] + fields[11]) * 1000 / hz);
    }
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    // [[hours:]minutes:]seconds.hundredths
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds = text.trim().split(':').fold(0.0, |total: f64, part| {
        total * 60.0 + part.parse::<f64>().unwrap()
    });
    Duration::from_secs_f64(seconds)
}

/// A daemon raises its soft limit on open descriptors to its hard limit, as Go's runtime
/// raises its own: macOS starts processes with 256 (audit A07). No VM needed. A limit on a
/// process's descriptors is Unix's (RLIMIT_NOFILE): Windows' quota of handles, 2^24 a
/// process (Microsoft, "Kernel Objects"), is none a process raises or a shell lowers.
#[cfg(unix)]
#[test]
fn a_daemon_raises_its_descriptor_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a local.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    if lim.rlim_max <= 256 {
        eprintln!(
            "SKIP: a hard limit of {} descriptors leaves nothing to raise",
            lim.rlim_max
        );
        return;
    }
    let home = TempDir::new("daemon-limit");
    let mut cmd = common::command();
    descriptor_limit(&mut cmd, 256, false);
    let out = cmd
        .arg("ps")
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap();
    let limit: u64 = log
        .lines()
        .find_map(|l| l.split(", with up to ").nth(1))
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no descriptor limit in the daemon's log:\n{log}"));
    assert!(limit > 256, "the daemon kept a limit of {limit} descriptors");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A client that takes a slot and says nothing holds up no shutdown: the daemon ends it as
/// it stops. The audit's reproduction (A07): before, `daemon stop` waited until the client
/// closed. No VM needed.
#[test]
fn a_client_that_says_nothing_holds_up_no_stop() {
    let home = TempDir::new("daemon-silent");
    let daemon = start_daemon(&home);
    let silent = silent_slot(&home).expect("no slot was offered");
    let t0 = Instant::now();
    let env = [("SHARDS_HOME", home.as_os_str())];
    // Its daemon waits on a silent client as long as it lives (PM M545): only the stop
    // ends it, and a stop held on it would hold the command, judged as it goes on. How
    // long the stop took is told (D209).
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    common::measured("a daemon's stop with a silent client in hand", &[t0.elapsed()]);
    exits(&home, daemon, "the daemon outlived its stop");
    // Let go: told, as it asked nothing yet, to ask the next daemon (RESTART), and shut.
    let heard = || shards_ipc::slots::read_frame(&silent.answer).map(|f| f.map(|f| f.kind));
    let mut got = heard();
    if matches!(got, Ok(Some(k)) if k == shards_ipc::kind::RESTART) {
        got = heard();
    }
    assert!(matches!(got, Ok(None)), "the client was not let go: {got:?}");
}

/// A request larger than a socket's buffer is served: here filters of 300 KB, past the 8 KiB
/// of a macOS socket pair's and the 208 KiB of Linux's. Before, the daemon wrote the request
/// to its own handling before it read it, and waited for ever. Three arguments of 100 KB:
/// Linux takes none past 128 KiB (MAX_ARG_STRLEN). No VM needed.
#[test]
fn a_request_past_a_sockets_buffer_is_served() {
    let home = TempDir::new("daemon-large");
    let env = [("SHARDS_HOME", home.as_os_str())];
    // Windows takes a command line of 32,767 characters (CreateProcessW): there its 300 KB,
    // past what a slot's line holds (the system's default, ipc's `Window::System`), go
    // as three labels of 100 KB from a file.
    if cfg!(windows) {
        let file = home.join("labels");
        let labels: String = (1..=3)
            .map(|n| format!("big{n}={}\n", "x".repeat(100_000)))
            .collect();
        std::fs::write(&file, labels).unwrap();
        let file = file.to_string_lossy().into_owned();
        let args = ["--pull", "never", "--label-file", file.as_str(), "no/such:image"];
        let made = run_shards_env(&["create"], &args, &env);
        assert_ne!(made.status, Some(0), "{made}");
        assert!(made.stderr.contains("No such image"), "{made}");
        let stopped = run_shards_env(&["daemon"], &["stop"], &env);
        assert_eq!(stopped.status, Some(0), "{stopped}");
        return;
    }
    let labels: Vec<String> = (1..=3)
        .map(|n| format!("label=big{n}={}", "x".repeat(100_000)))
        .collect();
    let mut args = vec!["-a", "-q"];
    for label in &labels {
        args.extend(["--filter", label.as_str()]);
    }
    let listed = run_shards_env(&["ps"], &args, &env);
    assert_eq!((listed.status, listed.stdout.as_str()), (Some(0), ""), "{listed}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// Past its cap of clients in hand, here 16 (`SHARDS_MAX_CLIENTS`), the daemon leaves the
/// next waiting at its slot, and takes it as soon as one in hand leaves (audit A07). No VM
/// needed.
#[test]
fn clients_past_the_cap_wait_their_turn() {
    more_descriptors();
    let home = TempDir::new("daemon-cap");
    let started = common::command()
        .arg("ps")
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_MAX_CLIENTS", "16")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // Its soft limit on descriptors is raised to its hard limit, where Linux shows it.
    if let Ok(limits) = std::fs::read_to_string(format!("/proc/{daemon}/limits")) {
        let line = limits.lines().find(|l| l.starts_with("Max open files")).unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields[3], fields[4], "{line}");
    }
    let mut silent: Vec<Silent> = (0..16)
        .map(|_| silent_slot(&home).expect("no slot was offered"))
        .collect();
    // Each counted in hand as the daemon reads its first byte, on a thread of the slot's:
    // all of them once it says it has its most, before the next asks.
    eventually(&home, "the daemon never had its most clients in hand", || {
        std::fs::read_to_string(home.join("daemon.log"))
            .unwrap_or_default()
            .contains("16 clients in hand, the most it takes at once")
    });
    let mut listing = common::command()
        .arg("ps")
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The silent ones never send their requests, and their daemon waits on each as long as
    // it lives (PM M545): the listing can be served only once one leaves. Unserved as one
    // leaves, then served; how soon after, told (D209). Nothing the daemon says marks the
    // listing waiting at its list, so how long it went unserved before is not held.
    assert!(
        listing.try_wait().unwrap().is_none(),
        "a client past the cap was taken"
    );
    let t0 = Instant::now();
    drop(silent.pop());
    assert_eq!(wait(&mut listing, &home), Some(0));
    common::measured(
        "a client past the cap served once one in hand leaves",
        &[t0.elapsed()],
    );
    drop(silent);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// D261's gate reads, beside the records, the refusals the daemons of this test process's
/// homes logged, each with its moment and whether its guest had run: a warm VM, restored, keeps
/// no records, and a burst's only witness may be in another test's home's log (CI 44449878966,
/// x86_64 musl: VM 197598 refused before its guest ran, and VM 196663, another test's, after
/// its guest ran, 178 µs later). Its moment is far from any refusal a test meets, so that no
/// other test of this process reads it as a witness. No VM needed.
#[test]
fn a_refusal_another_homes_daemon_logged_is_read_as_a_witness() {
    let home = TempDir::new("d261-logged");
    let line = "shards daemon 1: VM 196663: vCPU 3: VM entry failed (hardware reason 0x7): the \
                host's hypervisor (KVM under KVMKVMKVM) refused to enter it (after its guest ran, \
                at 1000000000.000001 s, VM process 196663)";
    std::fs::write(home.join("daemon.log"), format!("{line}\n")).unwrap();
    let found = common::refusals_logged_here();
    let witness = shards_vmm::vm::limits::Refusal {
        at_us: 1_000_000_000_000_001,
        machine: 196_663,
        entered: true,
    };
    assert!(found.contains(&witness), "{found:?}");
}

/// A home removed once, as `rm -rf` or a test's directory removes it, goes with its daemon
/// whatever order its names go in. One that takes the slots first, which the daemon then
/// offers again, and the rest after, the lock among them, fails to remove the directory,
/// not empty: the daemon, seeing its lock gone, exits and removes what it made. Before, it
/// listened on in a home holding nothing but its socket. So too what it writes as the
/// removal goes, a network's file: before, the home stayed, holding it, a test's home among
/// hundreds left. No VM needed.
#[test]
fn a_home_removed_once_is_gone_with_its_daemon() {
    let home = TempDir::new("daemon-home-once");
    let daemon = start_daemon(&home);
    let path = home.to_path_buf();
    let requests = path.join("requests");
    // Gone once, whatever the daemon makes in it as it goes: a removal of a directory
    // whose entries are being made fails not empty (rmdir(2)), and is tried again.
    eventually(&home, "the slots' directory could not be removed", || {
        let _ = std::fs::remove_dir_all(&requests);
        !requests.exists()
    });
    eventually(&home, "the daemon did not offer its slots again", || {
        shards_ipc::slots::served(&path)
    });
    // Every other name but the lock, then the directory, which the slots keep: the
    // removal has failed. Then the lock, which a removal in another order takes before
    // it fails: the daemon, seeing it gone, exits and finishes the removal.
    let lock = path.join("daemon.lock");
    for entry in std::fs::read_dir(&path).unwrap() {
        let name = entry.unwrap().path();
        if name == requests || name == lock {
            continue;
        }
        // The daemon may be writing in it as it goes, a spare microVM: until it is gone.
        if name.is_dir() {
            eventually(&home, "a directory of the home could not be removed", || {
                std::fs::remove_dir_all(&name).is_ok() || !name.exists()
            });
        } else {
            std::fs::remove_file(&name).unwrap();
        }
    }
    assert!(
        std::fs::remove_dir(&path).is_err(),
        "removed with its slots in it"
    );
    // What the daemon writes as a removal goes, past the names the removal took: its
    // predefined networks, made the first time they are asked for.
    std::fs::create_dir(path.join("networks")).unwrap();
    std::fs::write(path.join("networks").join("made-as-it-went.json"), "{}").unwrap();
    std::fs::remove_file(&lock).unwrap();
    // Its record went with the rest of the home.
    exits(&home, daemon, "the daemon outlived its home");
    eventually(&home, "what was left of the home was left", || !path.exists());
}

/// Lists removed by hand while the daemon idles, every name of the home kept: the next
/// client finds none and starts a daemon, which, finding the home's lock held, says so by
/// a name it makes and removes in the home, so that the daemon serving makes its lists
/// again (D185). The client is answered by the daemon that served before, which says it
/// made them again; the one it started steps aside. No VM needed.
#[test]
fn lists_removed_by_hand_are_made_again_for_the_next_client() {
    let home = TempDir::new("daemon-hand");
    let daemon = start_daemon(&home);
    for list in ["free", "free-streams"] {
        std::fs::remove_file(home.join("requests").join(list)).unwrap();
    }
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_LOCAL_STORE", "none".as_ref()),
    ];
    let asked = run_shards_env(&["ps"], &[] as &[&str], &env);
    assert_eq!(asked.status, Some(0), "{asked}");
    assert_eq!(daemon_pid(&home), Some(daemon), "another daemon serves");
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap();
    assert!(log.contains("was removed; offering slots there again"), "{log}");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A daemon told to stop waits for a client that takes nothing of its answer no longer
/// than the slowest take it has seen, from when that write began waiting on it (D185, PM
/// M549), says so, and exits; for one whose
/// answer is all written, not at all, and that client reads it whole once it reads, a FIFO
/// keeping what was written as its writer goes. Here two clients of `inspect` whose stdout
/// is a pipe nobody reads yet, as a pager's that waits: each has the answer's first message
/// in its hands, a volume with a label of 100 KiB, more than its pipe takes. For the one,
/// a name nothing has follows, its error a message of 120 KiB, more than the slot's FIFO
/// takes (8 KiB on macOS, 64 KiB on Linux, PM M261), whose write waits; the other's answer
/// ends there, all written. Before, the daemon waited for both for ever, its slots counted
/// until each had passed all on and its client had ended. No VM needed. Windows takes a
/// command line of 32,767 characters (CreateProcessW), which no such label nor name fits:
/// there each answer's first message is a microVM made with 100 KiB of labels from a file,
/// more than std's pipes take (64 KiB, its child pipes' quota), and for the one 4,500 names
/// nothing has follow, their errors one message of 123 KiB, more than a slot's line takes
/// (the system's default, ipc's `Window::System`, as a Linux FIFO's 64 KiB). Before any
/// take, the daemon holds such a client until it reads or ends (D185's cold start), and
/// says so: a take is a write that waited on its client, and clients reading their first
/// messages as fast as the daemon wrote them may have made it wait on none, the FIFO taking
/// the rest of each as its client read. Asked as it was, the stop held this test, which
/// read its clients only after it, until the step's limit (CI 44138098584, aarch64 musl,
/// 54 minutes). It is asked on a thread of its own, and the cut client read once the
/// daemon says it holds it: then it takes its whole answer, and the stop ends.
#[test]
fn a_stopping_daemon_waits_for_no_client_that_reads_nothing() {
    let home = TempDir::new("daemon-unread");
    start_daemon(&home);
    let env = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_LOCAL_STORE", "none".as_ref()),
    ];
    let label = format!("k={}", "v".repeat(100 << 10));
    let missing = if cfg!(windows) {
        let (image, _) = served();
        let pulled = run_shards_env(&["pull"], &[image.as_str()], &env);
        assert_eq!(pulled.status, Some(0), "{pulled}");
        let labels = home.join("labels");
        std::fs::write(&labels, format!("{label}\n")).unwrap();
        let labels = labels.to_string_lossy().into_owned();
        let args = [
            "--pull",
            "never",
            "--name",
            "big",
            "--label-file",
            labels.as_str(),
            image.as_str(),
        ];
        let made = run_shards_env(&["create"], &args, &env);
        assert_eq!(made.status, Some(0), "{made}");
        vec!["nope".to_string(); 4500]
    } else {
        let made = run_shards_env(&["volume"], &["create", "--label", &label, "big"], &env);
        assert_eq!(made.status, Some(0), "{made}");
        vec!["m".repeat(120 << 10)]
    };
    let inspect = |args: &[&str]| {
        let mut child = common::command()
            .arg("inspect")
            .args(args)
            .env("SHARDS_HOME", &*home)
            .env("SHARDS_LOCAL_STORE", "none")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        (child, out, err)
    };
    let cut: Vec<&str> = ["big"]
        .into_iter()
        .chain(missing.iter().map(String::as_str))
        .collect();
    let cut = inspect(&cut);
    let whole = inspect(&["big"]);
    // Each has the volume in its hands, and stops reading as it waits on its full pipe:
    // waited on as its daemon works on the answers (`Patience`, D209), as step 10's arm64
    // Windows host under QEMU's TCG showed a client nothing in a minute (FullReal04).
    for (who, out) in [("cut", &cut.1), ("whole", &whole.1)] {
        let mut patience = common::Patience::of(&home);
        while waiting_in(out) == 0 {
            assert!(patience.holds(), "the {who} client showed nothing");
            std::thread::sleep(Duration::from_millis(5));
        }
        patience.done();
    }
    // Each read at last: all of what is written is read.
    let read =
        |(mut child, mut out, mut err): (Child, std::process::ChildStdout, std::process::ChildStderr)| {
            let said = std::thread::spawn(move || {
                let mut text = String::new();
                err.read_to_string(&mut text).unwrap();
                text
            });
            let mut shown = String::new();
            out.read_to_string(&mut shown).unwrap();
            let status = child.wait().unwrap();
            (status.code(), shown, said.join().unwrap())
        };
    // The stop, the cut client unread (`Ok`) unless the daemon says it holds that client
    // until it reads or ends, before any take: then it is read (`Err`, what it showed).
    let held_until = "held until that client reads or ends";
    let (stopped, cut) = std::thread::scope(|scope| {
        let stopping = scope.spawn(|| run_shards_env(&["daemon"], &["stop"], &env));
        while !stopping.is_finished() {
            let log = std::fs::read_to_string(home.join("daemon.log")).unwrap();
            if log.contains(held_until) {
                let taken = read(cut);
                return (stopping.join().unwrap(), Err(taken));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        (stopping.join().unwrap(), Ok(cut))
    });
    assert_eq!(stopped.status, Some(0), "{stopped}");
    // The slot whose write waited held the daemon its answer wait, and only it: unless
    // the stop came before the daemon had the error to write, and cut it off with the rest.
    // The slots no client opened, and the one whose answer is all written, are no clients
    // it leaves. Before, it said it left two slots whose client "went no further", 3.1 ms
    // after its close, its slowest take (an HVF host at dc7f45bc6).
    let log = std::fs::read_to_string(home.join("daemon.log")).unwrap();
    let without: Vec<&str> = log.lines().filter(|l| l.contains("exiting without it")).collect();
    assert!(without.len() <= 1, "{log}");
    for line in &without {
        assert!(line.contains("took no more of its answer"), "{line}");
    }
    let held = log.lines().filter(|l| l.contains(held_until)).count();
    let (status, shown, said) = read(whole);
    assert_eq!(status, Some(0), "{said}");
    assert!(
        shown.contains(label.trim_start_matches("k=")),
        "{} bytes shown",
        shown.len()
    );
    let head = |said: &str| said.chars().take(300).collect::<String>();
    match cut {
        // The other's answer ends where its client took nothing.
        Ok(cut) => {
            assert_eq!(held, 0, "{log}");
            let (status, _, said) = read(cut);
            assert_eq!(status, Some(125), "{}", head(&said));
            if without.is_empty() {
                eprintln!("the stop came before the daemon had the error to write");
            }
        }
        // Held until it read, it took its whole answer: the error of the names nothing has.
        Err((status, _, said)) => {
            assert_eq!((held, without.len()), (1, 0), "{log}");
            assert_eq!(status, Some(1), "{}", head(&said));
            assert!(said.contains("no such object"), "{}", head(&said));
            eprintln!("no take was seen before the stop: the daemon held the cut client until it read");
        }
    }
}

/// Each thread of process `pid`, by its ID: its name, and the times it was switched to
/// (/proc/PID/task/TID/status).
#[cfg(target_os = "linux")]
fn threads_run(pid: i32) -> std::collections::BTreeMap<String, (String, u64)> {
    let mut each = std::collections::BTreeMap::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let task = task.unwrap().path();
        let tid = task.file_name().unwrap().to_string_lossy().into_owned();
        let comm = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
        let status = std::fs::read_to_string(task.join("status")).unwrap_or_default();
        let n: u64 = status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().last()?.parse::<u64>().ok())
            .sum();
        each.insert(tid, (comm.trim().to_string(), n));
    }
    each
}

/// Each thread of process `pid`, by its ID: its name (GetThreadDescription), and the
/// cycles it has run (QueryThreadCycleTime), which a thread switched to adds to: Windows
/// counts a thread's switches in no documented place.
#[cfg(windows)]
fn threads_run(pid: i32) -> std::collections::BTreeMap<String, (String, u64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let pid = u32::try_from(pid).unwrap();
    let mut each = std::collections::BTreeMap::new();
    // SAFETY: a snapshot of the system's threads, read and closed here; each of `pid`'s
    // threads opened, read and closed in turn, its description freed as documented.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        assert_ne!(
            snapshot,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = u32::try_from(size_of::<THREADENTRY32>()).unwrap();
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            let thread = if entry.th32OwnerProcessID == pid {
                OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID)
            } else {
                std::ptr::null_mut()
            };
            if !thread.is_null() {
                let mut cycles = 0u64;
                assert_ne!(
                    QueryThreadCycleTime(thread, &mut cycles),
                    0,
                    "thread {}'s cycles: {}",
                    entry.th32ThreadID,
                    std::io::Error::last_os_error()
                );
                let mut described: *mut u16 = std::ptr::null_mut();
                let mut name = String::new();
                if GetThreadDescription(thread, &mut described) >= 0 && !described.is_null() {
                    let len = (0..).take_while(|&i| *described.add(i) != 0).count();
                    name = String::from_utf16_lossy(std::slice::from_raw_parts(described, len));
                    LocalFree(described.cast());
                }
                each.insert(entry.th32ThreadID.to_string(), (name, cycles));
                CloseHandle(thread);
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    assert!(!each.is_empty(), "no thread of process {pid} could be read");
    each
}

/// An idle daemon wakes for nothing: within a while of its start, a few seconds in which
/// none of its threads is switched to, by its own wake or the kernel's ([`threads_run`]).
/// Its slots' maker woke once a second to look at its lists (PM M259); it sleeps until it
/// is told something now. No VM needed.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn an_idle_daemon_wakes_for_nothing() {
    let home = TempDir::new("daemon-idle");
    let daemon = start_daemon(&home);
    let switches = || threads_run(daemon);
    // What it does as it starts may take a while on a busy host: the first quiet window is
    // what is asked for, looked for while the daemon goes on ([`Patience`]). A daemon that
    // wakes in every window goes on, and holds its wait to its CI job's limit, its account
    // said then (D209).
    let mut patience = Patience::of(&home);
    let mut woke = Vec::new();
    while patience.holds() {
        let before = switches();
        std::thread::sleep(Duration::from_secs(3));
        woke = switches()
            .into_iter()
            .filter(|(tid, (_, n))| before.get(tid).is_none_or(|(_, was)| was != n))
            .map(|(tid, (comm, _))| format!("{comm} ({tid})"))
            .collect();
        if woke.is_empty() {
            break;
        }
    }
    assert!(
        woke.is_empty(),
        "threads that woke in every 3 s while it idled: {woke:?}"
    );
    patience.done();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// Clients waiting long, on a microVM's end (`wait`) or its output (`logs -f`), shut
/// out no other, however few the daemon serves at once: here one (review 7.9).
#[test]
fn waiters_shut_out_no_client() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-waiters", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let capped: [(&str, &OsStr); 2] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_MAX_CLIENTS", "1".as_ref()),
    ];
    let up = run_shards_env(
        &["run"],
        &["-d", "--name", "up", "--pull", "never", &image, "sleep"],
        &capped,
    );
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let mut waiting = common::command()
        .args(["wait", "up"])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut following = common::command()
        .args(["logs", "-f", "up"])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Following once it has shown the log's line, the `ready` its microVM says as it starts,
    // which its daemon sends it as it takes its request; the waiter, asked before it, has
    // no word of its own to show that it waits. Read to its end, so that it never waits on
    // its output.
    let (shown, heard) = std::sync::mpsc::channel();
    let out = BufReader::new(following.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in out.lines().map_while(Result::ok) {
            if line == "ready" {
                let _ = shown.send(());
            }
        }
    });
    let mut patience = Patience::of(&home);
    loop {
        match heard.recv_timeout(patience.next_look()) {
            Ok(()) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("the follower ended unserved"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(patience.holds(), "the follower was not served");
            }
        }
    }
    patience.done();
    let t0 = Instant::now();
    let listed = run_shards_env(&["ps"], &["-q"], &env);
    assert_eq!(listed.status, Some(0), "{}", listed.stderr);
    common::measured(
        "a client served as waiters fill the daemon's one place",
        &[t0.elapsed()],
    );
    let killed = run_shards_env(&["kill"], &["up"], &env);
    assert_eq!(killed.status, Some(0), "{}", killed.stderr);
    let mut code = String::new();
    waiting.stdout.take().unwrap().read_to_string(&mut code).unwrap();
    assert_eq!(wait(&mut waiting, &home), Some(0));
    assert_eq!(code, "137\n");
    assert_eq!(wait(&mut following, &home), Some(0));
    // Gone, it frees no room it did not hold: the next client is served.
    let after = run_shards_env(&["ps"], &["-q"], &env);
    assert_eq!(after.status, Some(0), "{}", after.stderr);
}

// A daemon is run out of descriptors by its limit, which Windows has none of to lower
// ([`a_daemon_raises_its_descriptor_limit`]).
/// A daemon out of descriptors makes no more slots and waits for room, rather than spin,
/// its clients waiting on their list, and serves again once clients leave (audit A07). Its
/// limit here is above what it holds idle, its four slots offered, which take 40
/// descriptors of their FIFOs' ends and stop pipes (shards_ipc::slots): each client that
/// begins its request then holds two more of it, its slot replaced. Started with no request
/// served ([`start_idle_daemon`]), the daemon has learned no request's rest, and waits on a
/// client that sends nothing past its first byte for as long as that client lives (PM
/// M545): the clients here hold their slots until each round lets them go. No VM needed.
#[cfg(unix)]
#[test]
fn a_daemon_out_of_descriptors_waits_for_room() {
    more_descriptors();
    let home = TempDir::new("daemon-starved");
    let daemon = start_idle_daemon(&home, 128);
    let log = home.join("daemon.log");
    let said = || std::fs::read_to_string(&log).unwrap_or_default();
    // The daemon's own words as it runs out and as it has room again: EMFILE's text is
    // the C library's, and musl's differs.
    const OUT: &str = "clients wait until the daemon has room";
    const AGAIN: &str = "offering slots again";
    // Out as of `log`: it said it ran out, and has offered no slot since.
    let out = |log: &str| {
        log.rfind(OUT)
            .is_some_and(|at| !log.get(at..).unwrap_or_default().contains(AGAIN))
    };
    assert!(
        !out(&said()),
        "out of descriptors idle; the daemon's log:\n{}",
        said()
    );
    let left = home.join("images").join("ingest").join("left");
    std::fs::create_dir_all(left.parent().unwrap()).unwrap();
    let due = home.join("images").join("collect-due");
    let mut silent = Vec::new();
    // Clients that take every slot it offers and say nothing past their first byte, until it
    // says it has no room to make the next (`out`), and no slot it offered is left: a client
    // it has yet to see the end of (the `ps` that started it), or one that takes a slot as it
    // runs out, gives it room for more first (CI 41eab0d5: 270 µs after). Waited on while it
    // goes on ([`Patience`]).
    // Where in its log it said it ran out, as judged: the window starts there, so that room
    // it found between that look and the window's start is room found (KVM: said before a
    // window read from the log's length, its collection, with room, taken for one without).
    let fill = |silent: &mut Vec<Silent>| -> usize {
        let mut patience = Patience::of(&home);
        let from = loop {
            match try_slot(&home) {
                Taken::Slot(client) => silent.push(client),
                Taken::Busy | Taken::Gone => {
                    let log = said();
                    if out(&log) {
                        break log.rfind(OUT).unwrap_or_default();
                    }
                    assert!(
                        patience.holds(),
                        "the daemon never ran out of descriptors, {} clients in; its log:\n{}",
                        silent.len(),
                        said()
                    );
                    std::thread::sleep(patience.next_look());
                }
            }
        };
        patience.done();
        from
    };
    // Its clients each take a slot it offers and say nothing past their first byte. On macOS
    // a wait past what one select(2) takes failed every slot at once, so that the daemon
    // never ran out (PM M542). Then windows until one passes with no room found: room it
    // found meanwhile was its last take's own, the stop pipe of the slot taken, which a
    // busy daemon came to after this test stopped taking slots (PM M334): that slot is
    // taken, and the window run again.
    let mut from = fill(&mut silent);
    let last_window = loop {
        // Its CPU over a second's window as it waits for room, told: what it costs, which no
        // later event orders (D209).
        let before = cpu_time(daemon);
        std::thread::sleep(Duration::from_secs(1));
        let spent = cpu_time(daemon).saturating_sub(before);
        common::measured("a daemon out of descriptors, its CPU in a second", &[spent]);
        // A collection due meanwhile waits: its files would take what clients wait for.
        std::fs::write(&left, b"left behind").unwrap();
        std::fs::write(&due, b"").unwrap();
        eventually(&home, "the collection due was not seen", || !due.exists());
        if !said().get(from..).is_some_and(|since| since.contains(AGAIN)) {
            assert!(
                left.exists(),
                "collected while out of descriptors; its log:\n{}",
                said()
            );
            break from;
        }
        eprintln!("the daemon found room as its window went; its slot taken");
        from = fill(&mut silent);
    };
    // Let go: the daemon has room, says so, and only then collects what was left, as its log
    // orders it (no window after the mark stands for its wait, D209).
    drop(silent);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let listed = run_shards_env(&["ps"], &[] as &[&str], &env);
    assert_eq!(listed.status, Some(0), "{listed}");
    eventually(&home, "not collected once the daemon had room", || !left.exists());
    let since = said().get(last_window..).unwrap_or_default().to_string();
    let (again, collected) = (since.find(AGAIN), since.find("files left in ingest/"));
    assert!(
        matches!((again, collected), (Some(a), Some(c)) if a < c),
        "the collection was not said after the daemon had room again; its log since its last window:\n{since}"
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A daemon told to stop cancels what its runs are downloading: here a pull from a
/// registry that accepts connections and never answers, which would hold the stop for
/// 30 s a try. The run is refused as the daemon shuts down (audit A07). No VM needed.
#[test]
fn a_stop_cancels_the_downloads_of_runs_being_prepared() {
    use std::net::TcpListener;
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = crate::common::minted_port(silent.local_addr().unwrap().port());
    let accepted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let accepted = accepted.clone();
        std::thread::spawn(move || {
            for conn in silent.incoming().flatten() {
                accepted.lock().unwrap().push(conn);
            }
        });
    }
    let home = TempDir::new("daemon-pulling");
    let image = format!("127.0.0.1:{port}/silent:latest");
    let run = common::command()
        .args(["run", "--pull", "always", &image, "exit", "0"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The run's client among the wait's party: before its daemon has written its pid file it
    // is all of it (CI job 132390978195 refused this wait as gone 710 µs in, the daemon's
    // first line logged and its pid file not yet written).
    let mut patience = Patience::client(run.id(), &home);
    while accepted.lock().unwrap().is_empty() {
        assert!(patience.holds(), "the pull never reached the registry");
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    let t0 = Instant::now();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    // The stop returned, the pull's connection never answered: it was cancelled, not waited
    // out. How soon, told (D209).
    common::measured("a daemon's stop with a download under way", &[t0.elapsed()]);
    let out = run.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{said}");
    assert!(said.contains("the daemon is shutting down"), "{said}");
}

/// A slice stopped under its microVMs takes no run after it with it (D183, D26): stopping
/// it, as its user may, or its manager does of its own accord (systemd-oomd's
/// `ManagedOOMMemoryPressure=kill`), ends the spawner the daemon placed there and the warm
/// microVM its pool keeps there for the slice's next run; the next runs under it start all
/// the same, and the pool is warm there again. Linux's: `--cgroup-parent` on Windows, a
/// job object, comes with the parity plan's step 12.
#[cfg(unix)]
#[test]
fn runs_under_a_slice_start_after_it_was_stopped() {
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
    let home = home_with("daemon-slice-stopped");
    let env: [(&str, &OsStr); 1] = [("SHARDS_HOME", home.as_os_str())];
    let slice = format!("shardsstopped{}.slice", std::process::id());
    /// The slice stopped, however the test ends.
    struct Stopped<'a>(&'a str);
    impl Drop for Stopped<'_> {
        fn drop(&mut self) {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", self.0])
                .status();
        }
    }
    let _stopped = Stopped(&slice);
    let said = || std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default();
    let run = |what: &str| {
        let run = run_shards_env(
            &["run"],
            &["--rm", "--cgroup-parent", &slice, &image, "exit", "0"],
            &env,
        );
        assert_eq!(
            run.status,
            Some(0),
            "{what}: {run}\nthe daemon's log:\n{}",
            said()
        );
    };
    // The image's template saved under the slice, and its pool warm there: a warm VM of it
    // made in the slice, for the slice's next run.
    let templates = home.join("templates").to_string_lossy().into_owned();
    let warm_in_slice = || {
        processes_with(&templates).into_iter().any(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .is_ok_and(|c| c.contains(&format!("/{slice}/")))
        })
    };
    run("the first run");
    let mut patience = Patience::of(&home);
    while !warm_in_slice() {
        assert!(
            patience.holds(),
            "no warm VM in {slice}\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    let stopped = Command::new("systemctl")
        .args(["--user", "stop", &slice])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    run("the first run after the slice stopped");
    run("the second run after the slice stopped");
    let mut patience = Patience::of(&home);
    while !warm_in_slice() {
        assert!(
            patience.holds(),
            "no warm VM in {slice} again\nthe daemon's log:\n{}",
            said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A pool of 0 keeps no VM warm, and every run restores its own: the second run of an
/// image, from its template, is served at once, not after the 60 s a pool that never
/// fills would take (audit A14, whose reproduction this is).
#[test]
fn a_pool_of_none_serves_every_run() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-pool-none");
    // Each run's timing line (SHARDS_TIMING): where a slow one's time went, said if it is.
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_POOL", "0".as_ref()),
        ("SHARDS_TIMING", "1".as_ref()),
    ];
    let templates = home.join("templates").to_string_lossy().into_owned();
    // Each served, its command's own status: a run waiting on a pool that never fills is
    // judged as it goes. How long each restored run took is told (D209).
    let mut restored = Vec::new();
    for i in 0..3 {
        let t0 = Instant::now();
        let run = run_shards_env(&["run"], &[&image, "exit", "7"], &env);
        assert_eq!(run.status, Some(7), "run {i}: {}", run.stderr);
        if i > 0 {
            restored.push(t0.elapsed());
        }
    }
    common::measured("a run restored from its template with a pool of none", &restored);
    eventually(&home, "a VM was kept warm", || {
        processes_with(&templates).is_empty()
    });
    // Where no VM is kept, its daemon holds its template's memory and root filesystem open
    // between its runs, so that its host keeps their pages (PM M315).
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // As the daemon names them: through links on Unix, as lsof and /proc resolve them.
    let real = if cfg!(windows) {
        home.to_path_buf()
    } else {
        std::fs::canonicalize(&*home).unwrap()
    };
    let memory = held_under(daemon, &real.join("templates"), |n| n == "memory");
    assert!(!memory.is_empty(), "its template's memory");
    let roots = held_under(daemon, &real, |n| n.ends_with(".erofs"));
    assert!(!roots.is_empty(), "its template's root filesystem");
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// Settings a daemon cannot keep stop it before it serves, and its client says which at
/// once (audit A14): a malformed count, a pool larger than all pools may keep, and a
/// fleet past its bound.
#[test]
fn a_daemon_refuses_settings_it_cannot_keep() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    // Counts beyond every possible process thread limit are refused on every host. A
    // fixed large count can be valid on a host whose own limits permit more clients.
    let too_many = u64::MAX.to_string();
    let warm_error = format!("SHARDS_WARM_MAX: {too_many} is more than");
    let clients_error = format!("SHARDS_MAX_CLIENTS: {too_many} is more than the");
    let mut took = Vec::new();
    for (settings, said) in [
        (
            &[("SHARDS_POOL", "two")][..],
            "SHARDS_POOL: \"two\" is not a count",
        ),
        (&[("SHARDS_POOL", "-1")][..], "SHARDS_POOL: \"-1\" is not a count"),
        (
            &[("SHARDS_POOL", "5"), ("SHARDS_WARM_MAX", "4")][..],
            "SHARDS_POOL: 5 is more than",
        ),
        (&[("SHARDS_WARM_MAX", too_many.as_str())][..], warm_error.as_str()),
        (
            &[("SHARDS_MAX_CLIENTS", "0")][..],
            "SHARDS_MAX_CLIENTS: the daemon serves at least one client",
        ),
        (
            &[("SHARDS_MAX_CLIENTS", too_many.as_str())][..],
            clients_error.as_str(),
        ),
        (&[("SHARDS_DAEMON_IDLE", "soon")][..], "SHARDS_DAEMON_IDLE"),
        (
            &[("SHARDS_LOG_MAX_SIZE", "20m")][..],
            "SHARDS_LOG_MAX_SIZE: \"20m\" is not a count",
        ),
        (
            &[("SHARDS_LOG_MAX_SIZE", "0")][..],
            "SHARDS_LOG_MAX_SIZE: a log keeps at least a byte",
        ),
        (
            &[("SHARDS_LOG_MAX_FILE", "0")][..],
            "SHARDS_LOG_MAX_FILE: a log keeps at least one file",
        ),
    ] {
        let home = TempDir::new("daemon-settings");
        let mut env: Vec<(&str, &OsStr)> = vec![("SHARDS_HOME", home.as_os_str())];
        env.extend(settings.iter().map(|(k, v)| (*k, v.as_ref())));
        let t0 = Instant::now();
        let run = run_shards_env(&["run"], &[&image, "exit", "0"], &env);
        assert_ne!(run.status, Some(0), "{settings:?}");
        assert!(run.stderr.contains(said), "{settings:?}: {}", run.stderr);
        took.push(t0.elapsed());
        assert!(
            daemon_pid(&home).is_none_or(|pid| !alive(pid)),
            "{settings:?}: it serves"
        );
    }
    // How soon each was said, told (D209): its daemon refused them before it served, which
    // the client waited on no more than that daemon's life.
    common::measured("a client's word of settings its daemon cannot keep", &took);
}

/// A daemon that ends before it serves is said at once, with where its log is, rather
/// than waited for (review 8.14): here one that cannot open its home's lock, which a
/// directory holds the name of.
#[test]
fn a_daemon_that_cannot_serve_is_said_at_once() {
    let home = TempDir::new("daemon-cannot-serve");
    std::fs::create_dir(home.join("daemon.lock")).unwrap();
    let t0 = Instant::now();
    let ps = run_shards_env::<&str>(&["ps"], &[], &[("SHARDS_HOME", home.as_os_str())]);
    assert_ne!(ps.status, Some(0), "{}", ps.stderr);
    assert!(
        ps.stderr
            .contains("the daemon exited with status 1 before it served; see"),
        "{}",
        ps.stderr
    );
    // Said once its daemon ended, as its words say; how soon, told (D209).
    common::measured(
        "a client's word of a daemon that ended before it served",
        &[t0.elapsed()],
    );
}

/// A run of an image held under way until [`Held::end`]: `cat`, reading what its client's
/// stdin gives it (`run -i`). Through it a test learns the run's own microVM, which is no
/// warm VM of its pool's from its claim on, and which, its run ended, takes its time to go
/// as its guest's memory is let go of (2 counted 500 ms after the run on a busy runner,
/// budget E2E 40197051533, PM M337): a test counting a pool's VMs among its template's
/// processes leaves it out.
struct Held {
    client: Child,
    stdin: Option<std::process::ChildStdin>,
    out: BufReader<std::process::ChildStdout>,
    name: String,
    /// Where its client's stderr goes, `said` in it: a file, as a run hands its client's
    /// stdio to its microVM, which a pipe's reader would wait on past its client's end
    /// (benches/arrival.rs).
    words: TempDir,
}

/// How a [`Held`] run ended: its client's status and what its client said.
struct Ended {
    status: Option<i32>,
    said: String,
}

impl Ended {
    /// Whether it failed as its VM's host refused to enter its VM, as D261's gate takes the
    /// refusal for its host's: its test then skips, said so ([`common::run_host_refused`]). CI
    /// 44379986478, x86_64 gnu: a burst's run ended 125, its VM 306660's vCPU 0 refused (0x7).
    fn host_refused(&self, home: &Path) -> bool {
        self.status != Some(0) && common::run_host_refused(&self.said, home)
    }
}

impl Held {
    /// `shards run -i --name NAME IMAGE cat`, with `env`.
    fn start(image: &str, name: &str, env: &[(&str, &OsStr)]) -> Held {
        let words = TempDir::new(&format!("held-{name}"));
        let said = std::fs::File::create(words.join("said")).unwrap();
        let mut client = common::command()
            .args(["run", "-i", "--pull", "never", "--name", name, image, "cat"])
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(said)
            .spawn()
            .unwrap();
        let stdin = client.stdin.take();
        let out = BufReader::new(client.stdout.take().unwrap());
        Held {
            client,
            stdin,
            out,
            name: name.to_string(),
            words,
        }
    }

    /// Its microVM once its command runs, as its record names it (`.State.Pid`), and its
    /// command line, as [`commands_with`] lists it among the processes that name
    /// `templates`: its own `--layer` file and MAC tell it from a VM given its process ID
    /// once it has gone.
    #[track_caller]
    fn vm(&mut self, env: &[(&str, &OsStr)], templates: &str) -> (u32, String) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(b"held\n").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        assert_eq!(line, "held\n", "{}", self.name);
        let pid = run_shards_env(&["inspect"], &["-f", "{{.State.Pid}}", self.name.as_str()], env);
        let pid: u32 = pid.stdout.trim().parse().unwrap_or_else(|_| panic!("{pid}"));
        commands_with(templates)
            .into_iter()
            .find(|(p, _)| *p == pid)
            .unwrap_or_else(|| panic!("{}'s VM {pid} is no process of {templates}", self.name))
    }

    /// Ends its command, its client's stdin closed, and waits for its client: its status and
    /// what it said.
    #[track_caller]
    fn end(mut self, home: &Path) -> Ended {
        drop(self.stdin.take());
        let status = wait(&mut self.client, home);
        let said = std::fs::read_to_string(self.words.join("said")).unwrap();
        Ended { status, said }
    }
}

/// Warm VMs kept ahead of runs are bounded all pools together (audit A13): with room
/// for one, a second template's pool takes the first's, least recently claimed from, and
/// the first is still served, on demand.
#[test]
fn warm_vms_are_bounded_across_templates() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (first, _) = served();
    let second = served_variant(b"second");
    let home = home_with("daemon-warm-max");
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_POOL", "1".as_ref()),
        ("SHARDS_WARM_MAX", "1".as_ref()),
    ];
    let templates = home.join("templates");
    let warm = |template: &Path| processes_with(&template.to_string_lossy()).len();
    let run = |image: &str| {
        let run = run_shards_env(&["run"], &[image, "exit", "0"], &env);
        assert_eq!(run.status, Some(0), "{image}: {}", run.stderr);
    };
    run(&first);
    run(&first);
    eventually(&home, "the first pool did not fill", || warm(&templates) == 1);
    run(&second);
    run(&second);
    let dirs: Vec<PathBuf> = std::fs::read_dir(&templates)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_none())
        .collect();
    assert_eq!(dirs.len(), 2, "{dirs:?}");
    let newest = dirs
        .iter()
        .max_by_key(|d| std::fs::metadata(d).unwrap().modified().unwrap())
        .unwrap();
    // As long as a run is given, or past it while the daemon or its microVMs progress, as
    // `eventually` waits.
    let mut patience = Patience::of(&home);
    while !(warm(newest) == 1 && warm(&templates) == 1) {
        assert!(
            patience.holds(),
            "the second pool did not take the room: {} of {} warm, of {dirs:?}\n{}",
            warm(newest),
            warm(&templates),
            std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    // Never more than the bound, as the first is served again: the pools' VMs are the
    // templates' processes but the run's own ([`Held`]). Its pool refills, and the warm VM
    // it took the room of, ended before the refill starts, goes as a VM process takes its
    // time to; then one at most, and no refill takes the pools past it. Counted with the
    // run's own VM, a look in the while between that warm VM's end and the refill's start
    // found the pools within their bound, and the next past it (CI windows-2025 at
    // b94c114f, as a look took 20 ms rather than a PowerShell's start).
    let first_dir = dirs
        .iter()
        .find(|d| d.as_path() != newest.as_path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let templates_named = templates.to_string_lossy().into_owned();
    let mut held = Held::start(&first, "first-again", &env);
    let ran = held.vm(&env, &templates_named);
    let ended = held.end(&home);
    if ended.host_refused(&home) {
        return;
    }
    assert_eq!(ended.status, Some(0), "the first's run, held: {}", ended.said);
    let pooled = |needle: &str| -> Vec<(u32, String)> {
        commands_with(needle)
            .into_iter()
            .filter(|vm| *vm != ran)
            .collect()
    };
    eventually(&home, "the first pool did not refill", || {
        !pooled(&first_dir).is_empty()
    });
    let mut patience = Patience::of(&home);
    while pooled(&templates_named).len() > 1 {
        assert!(
            patience.holds(),
            "the pools did not come back to their bound while they progressed: {:?}",
            pooled(&templates_named)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    // Within it from then until its daemon's stop, the later event that ends the pools:
    // watched as the stop goes, no window of the test's own standing for "never" (D209).
    let stopped = std::thread::scope(|scope| {
        let stopping = scope.spawn(|| run_shards_env(&["daemon"], &["stop"], &env));
        while !stopping.is_finished() {
            let now = pooled(&templates_named);
            assert!(now.len() <= 1, "past the bound: {now:?}, the run's own {ran:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        stopping.join().unwrap()
    });
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A pool keeps what its runs need while they come (audit A13): runs well apart keep one
/// warm VM, not the most a pool may keep; runs at once keep more, up to that most; and a
/// pool idle past `SHARDS_POOL_KEEP`, none of its runs going, keeps none. The next run is
/// served all the same, and its pool keeps VMs again.
/// Each warm VM of a pool is given its template's root filesystem (`--backing`), the
/// one file a restore of it may name, so that what the VM saving the template wrote does
/// not choose what the next is given.
#[test]
fn warm_vms_are_given_their_templates_root_filesystem() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-backing");
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_POOL", "2".as_ref())];
    let run = run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let templates = home.join("templates").to_string_lossy().into_owned();
    let warm = || -> Vec<String> {
        commands_with(&templates)
            .into_iter()
            .map(|(_, args)| args)
            .filter(|args| args.contains(" restore "))
            .collect()
    };
    eventually(&home, "the pool refilled", || !warm().is_empty());
    // An absolute path in its host's own form: `/...` on Unix, a drive's (`\\?\C:\...`) on
    // Windows.
    let rootfs = if cfg!(windows) { "\\rootfs\\" } else { "/rootfs/" };
    for vm in warm() {
        let backing = vm
            .split(" --backing ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default();
        assert!(
            Path::new(backing).is_absolute() && backing.contains(rootfs),
            "{vm}"
        );
    }
}

#[test]
fn pools_keep_what_their_runs_need_while_they_come() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-demand");
    // Its keep-alive is D26's own (SHARDS_POOL_KEEP unset) while runs come apart. Then, so
    // that the pool is seen to empty past it, its daemon starts again with one as long as the
    // window it said (below): no keep-alive of the test's own (R03). A fixed one of 15 s was
    // under a loaded host's refill window (CI 43968984258, x86_64 musl: runs 16 s apart were
    // still one burst).
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_POOL", "3".as_ref())];
    let log = home.join("daemon.log");
    // The burst window its daemon said at each refill it timed, oldest first: SRTT +
    // 4·RTTVAR of its refills from start to ready (RFC 6298, as shards_api::demand keeps
    // it), which the daemon alone measures.
    let windows = || -> Vec<Duration> {
        let said = std::fs::read_to_string(&log).unwrap_or_default();
        said.lines()
            .filter_map(|line| {
                let (_, rest) = line.split_once("; runs within ")?;
                let (us, _) = rest.split_once(" us of each other are one burst")?;
                us.parse().ok().map(Duration::from_micros)
            })
            .collect()
    };
    let templates = home.join("templates");
    let templates_named = templates.to_string_lossy().into_owned();
    // Its VMs: the template's processes, but for the runs' own VMs the burst learns
    // ([`Held`]), which leave the pool as they are claimed.
    let runs_own = std::cell::RefCell::new(Vec::<(u32, String)>::new());
    let warm = || {
        let runs_own = runs_own.borrow();
        commands_with(&templates_named)
            .into_iter()
            .filter(|vm| !runs_own.contains(vm))
            .count()
    };
    // Whether the pool comes to keep as `want` says, for as long as the daemon or its
    // microVMs go on ([`Patience`]), as a pool a slow host refills does; each look asserts
    // `always` of it.
    let comes = |mut patience: Patience, want: &dyn Fn(usize) -> bool, always: &dyn Fn(usize) -> bool| {
        loop {
            let n = warm();
            assert!(
                always(n),
                "{n} warm, past the pool's most: {:?}, the runs' own {:?}",
                commands_with(&templates_named),
                runs_own.borrow()
            );
            if want(n) {
                patience.done();
                return true;
            }
            if !patience.holds() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let settled = |patience: Patience, what: &str, want: &dyn Fn(usize) -> bool| {
        assert!(
            comes(patience, want, &|_| true),
            "{what}: {} warm: {:#?}\n{}",
            warm(),
            commands_with(&templates.to_string_lossy()),
            std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
        );
    };
    let args = [image.as_str(), "exit", "0"];
    // A run, served; or true where its VM's host refused an entry D261's gate takes for the
    // host's, which skips the test.
    let run = |env: &[(&str, &OsStr)]| -> bool {
        let run = run_shards_env(&["run"], &args, env);
        if run.status != Some(0) && common::run_host_refused(&run.stderr, &home) {
            return true;
        }
        assert_eq!(run.status, Some(0), "{}", run.stderr);
        false
    };
    // A refill, once its daemon has said it: its window then the newest said.
    let refilled = |said: usize| {
        eventually(&home, "the pool's refill said", || windows().len() > said);
        settled(Patience::of(&home), "a refill", &|n| n >= 1);
    };
    // Its first run boots its image and saves the template its pool refills from (D25).
    let said = windows().len();
    if run(&env) {
        return;
    }
    refilled(said);
    // Runs apart: each begun once the window its daemon said at the refill before it has
    // passed since the run before ended, so its claim comes more than one refill window
    // after that run's, and it is served by the one VM refilled for the run before. A pool
    // that judged two runs one burst keeps two until its keep-alive, so one look, once the
    // refill is said, tells (no window of the test's own, D209). Each run's own VM is learned
    // ([`Held`]): ended, it takes its time to go, and is no VM the pool keeps.
    for n in 0..4 {
        let window = windows().last().copied().unwrap();
        std::thread::sleep(window);
        let said = windows().len();
        let mut apart = Held::start(&image, &format!("apart-{n}"), &env);
        let vm = apart.vm(&env, &templates_named);
        runs_own.borrow_mut().push(vm);
        let ended = apart.end(&home);
        if ended.host_refused(&home) {
            return;
        }
        assert_eq!(ended.status, Some(0), "a run apart: {}", ended.said);
        refilled(said);
        assert_eq!(
            warm(),
            1,
            "runs begun {window:?} apart, the window their daemon said, were one burst to the pool:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
    // Its daemon again, its keep-alive the window it said last (whole seconds, as
    // SHARDS_POOL_KEEP takes it): by its daemon's own estimate a refill comes within it, so a
    // burst's refills come before its pool is idle past it.
    let window = windows().last().copied().unwrap();
    let keep = (window.as_secs() + u64::from(window.subsec_nanos() > 0)).to_string();
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    // Its VMs end with it: none is counted as the next daemon's pool's.
    all_end(&templates_named, "the first daemon's VMs, past its stop");
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_POOL", "3".as_ref()),
        ("SHARDS_POOL_KEEP", keep.as_ref()),
    ];
    // Three at once: the pool keeps more for the next burst, never past its most. Each
    // run held under way until all three are served, for its VM to be learned: what the
    // pool keeps is the template's processes but those, which go in their own time once
    // their runs end. Counted with them, a look as one had gone and the pool's refills had
    // not all started found the pool within its most, and the next past it (CI
    // windows-2025 at b94c114f, as a look took 20 ms rather than a PowerShell's start).
    let began = Instant::now();
    let mut burst: Vec<Held> = (0..3)
        .map(|n| Held::start(&image, &format!("burst-{n}"), &env))
        .collect();
    // How long until each was served, said where the pool keeps too few after them: a
    // loaded host's runs took 28 to 31 s (PM M346).
    let mut took = Vec::new();
    for held in &mut burst {
        let vm = held.vm(&env, &templates_named);
        took.push(began.elapsed());
        runs_own.borrow_mut().push(vm);
    }
    for held in burst {
        let ended = held.end(&home);
        if ended.host_refused(&home) {
            return;
        }
        assert_eq!(ended.status, Some(0), "a run of the burst: {}", ended.said);
    }
    settled(
        Patience::of(&home),
        &format!("a burst of three, served in {took:?}"),
        &|n| n >= 2,
    );
    // Never past its most: what the pool keeps it keeps until its keep-alive, so a pool past
    // its most stays so. Within it at every look until its keep-alive empties it, the later
    // event that ends what it keeps; unclaimed past its keep-alive, nothing, until the next
    // run.
    settled(Patience::of(&home), "the pool within its most", &|n| n <= 3);
    assert!(
        comes(Patience::of(&home), &|n| n == 0, &|n| n <= 3),
        "past the keep-alive: {} warm: {:#?}\n{}",
        warm(),
        commands_with(&templates.to_string_lossy()),
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    if run(&env) {
        return;
    }
    settled(Patience::of(&home), "after the keep-alive", &|n| n >= 1);
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A collection another process leaves due, as `shards build`'s pull of a base image
/// leaves one, is taken by a daemon that nothing else wakes: its watch of `images` sees
/// the mark come.
#[test]
fn a_collection_another_process_leaves_due_is_taken() {
    let home = TempDir::new("daemon-due");
    let daemon = start_daemon(&home);
    // Marked twice: the first may be taken as its client's going wakes it; the second, marked
    // once the first is taken and nothing else asks it anything, only its watch sees (no
    // window of the test's own stands for its sleep, D209).
    let due = home.join("images").join("collect-due");
    for mark in ["first", "second"] {
        std::fs::write(&due, b"").unwrap();
        eventually(
            &home,
            &format!("the daemon did not take the {mark} collection due"),
            || !due.exists(),
        );
    }
    assert!(alive(daemon));
}

/// A collection due while another process holds the image store's lease, as `shards
/// build` holds it while it prepares a root filesystem, runs once the lease is let go,
/// though nothing else happens meanwhile.
#[test]
fn a_collection_waits_for_the_stores_lease() {
    let (image, _) = served();
    let home = TempDir::new("daemon-lease");
    start_daemon(&home);
    // A process of its own, whose collection the daemon takes.
    let env = [("SHARDS_HOME", home.as_os_str())];
    let pulled = run_shards_env(&["pull"], &[image.as_str()], &env);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    let images = home.join("images");
    let due = images.join("collect-due");
    eventually(&home, "the pull's collection was not taken", || !due.exists());
    // Once that collection, which may still run, is done.
    let lease = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(images.join(".lease"))
        .unwrap();
    lease.lock_shared().unwrap();
    let left = images.join("ingest").join("left");
    std::fs::write(&left, b"left behind").unwrap();
    std::fs::write(&due, b"").unwrap();
    eventually(&home, "the collection due was not seen", || !due.exists());
    // Still there as the lease is let go, then collected. Nothing the daemon says marks its
    // collector waiting on the lease, so no later event orders "not collected under it"
    // past this look: how long after the lease's end it was collected is told (D209).
    assert!(left.exists(), "collected under the lease");
    let released = Instant::now();
    drop(lease);
    eventually(&home, "nothing collected once the lease was let go", || {
        !left.exists()
    });
    common::measured(
        "a collection due under the store's lease, after its end",
        &[released.elapsed()],
    );
}

/// What no reference needs is collected (audit A13). `shards pull` moving a tag to another
/// image leaves the old one dangling, kept for what may still name it by ID; once `rmi`
/// removes it, the daemon removes its blobs, root filesystem and template, ending its warm
/// VMs, and keeps the new one's, which runs from a template of its own.
#[test]
fn what_a_moved_tag_named_is_collected() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (port, _, serving) = registry_of(common::Served::new(test_image()));
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = home_with("daemon-collect");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let run = || {
        let run = run_shards_env(&["run"], &["--rm", image.as_str(), "exit", "0"], &env);
        assert_eq!(run.status, Some(0), "{}", run.stderr);
    };
    run();
    run();
    let listed = |dir: &str| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(home.join(dir))
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        paths.retain(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            // A template's origin is a record beside it (D117), not a template, its newer
            // versions too where Windows keeps them (`.origin.~N`, platform `write_in`).
            !name.starts_with('.') && !name.contains(".new-") && !name.contains(".origin")
        });
        paths.sort();
        paths
    };
    let (blobs, rootfs, templates) = (
        listed("images/blobs/sha256"),
        listed(&rootfs_dir()),
        listed("templates"),
    );
    assert_eq!(
        (rootfs.len(), templates.len()),
        (1, 1),
        "{rootfs:?} {templates:?}"
    );
    let template = templates[0].to_string_lossy().into_owned();
    eventually(&home, "no warm VM", || !processes_with(&template).is_empty());

    let old = run_shards_env(&["images"], &["-q", "--no-trunc", image.as_str()], &env);
    let old = old.stdout.trim().to_string();
    serving.set(test_image_with(Some(b"moved")));
    let pulled = run_shards_env(&["pull"], &[image.as_str()], &env);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    // Dangling, it is kept, its template too.
    let listed_now = run_shards_env(&["images"], &["-a", "-q", "--no-trunc"], &env);
    assert!(
        listed_now.stdout.lines().any(|l| l == old),
        "{}",
        listed_now.stdout
    );
    assert!(rootfs[0].exists() && templates[0].exists());
    let removed = run_shards_env(&["rmi"], &[old.as_str()], &env);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    eventually(&home, "the old image was not collected", || {
        blobs.iter().all(|b| !b.exists()) && !rootfs[0].exists() && !templates[0].exists()
    });
    eventually(&home, "the old template's VMs outlived it", || {
        processes_with(&template).is_empty()
    });
    let (now_blobs, now_rootfs) = (listed("images/blobs/sha256"), listed(&rootfs_dir()));
    assert_eq!(
        (now_blobs.len(), now_rootfs.len()),
        (blobs.len(), 1),
        "{now_blobs:?}"
    );
    run();
    run();
    assert_eq!(listed("templates").len(), 1, "the new image's template");
    assert!(
        listed(&rootfs_dir()) == now_rootfs,
        "the new image's root filesystem kept"
    );
    let stopped = run_shards_env(&["daemon"], &["stop"], &env);
    assert_eq!(stopped.status, Some(0), "{stopped}");
}

/// A daemon whose home is removed ends its runs, whose microVMs went with the home, and
/// exits, and never makes the home again: before, the spare microVM it made after the
/// removal made the home's path anew, with its own directory inside, and a run kept its
/// daemon and VM going with no microVM left to reach them by.
#[test]
fn a_daemon_whose_home_is_removed_exits_without_making_it_again() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-home-removed", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    // Under --init, so that its command, which does not handle SIGTERM, ends at once: as
    // PID 1 it would not hear it, and the daemon would end it at its stop timeout (D115).
    let up = run_shards_env(
        &["run"],
        &["-d", "--init", "--name", "up", "--pull", "never", &image, "sleep"],
        &env,
    );
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    let path = home.to_path_buf();
    let named = path.to_string_lossy().into_owned();
    // The daemon may be writing in it as it goes, a spare microVM or a refill: removed
    // again until it is gone.
    eventually(&home, "the home could not be removed", || {
        matches!(std::fs::remove_dir_all(&path), Ok(())) || !path.exists()
    });
    exits(&home, daemon, "the daemon outlived its home");
    all_end(&named, "VMs outlived their home");
    // Nothing that could make it again lives: looked at now, no window standing for "never"
    // (D209).
    let made: Vec<_> = std::fs::read_dir(&path)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    let inner: Vec<_> = made
        .iter()
        .flat_map(|p| {
            std::fs::read_dir(p)
                .map(|d| d.map(|e| e.unwrap().path()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    assert!(!path.exists(), "the home was made again: {made:?} {inner:?}");
}

/// A microVM restored from its snapshot (D176) ends with its home as a run does: before,
/// a daemon whose home was removed ended its runs and exited, and a restored microVM's VM
/// ran on, its home gone.
#[test]
fn a_restored_microvm_ends_with_its_home() {
    if cannot_run_vms() || cannot_pool() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-home-restored", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env);
    let up = shards(&[
        "run", "-d", "--init", "--name", "up", "--pull", "never", &image, "sleep",
    ]);
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let saved = shards(&["save", "vm", "up", "snap"]);
    assert_eq!(saved.status, Some(0), "{}", saved.stderr);
    let restored = shards(&["restore", "vm", "up"]);
    assert_eq!(restored.status, Some(0), "{}", restored.stderr);
    let path = home.to_path_buf();
    let named = path.to_string_lossy().into_owned();
    eventually(&home, "no VM of the restored run", || {
        !processes_with(&named).is_empty()
    });
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // Removed as `eventually` would, its failure saying what held the home: its last error,
    // and each file left with what uses it (jupiter r11 at 7254ebaeb8: windows-x86_64 failed
    // every time, saying nothing more).
    let mut patience = Patience::of(&home);
    loop {
        let removal = std::fs::remove_dir_all(&path);
        if removal.is_ok() || !path.exists() {
            break;
        }
        assert!(
            patience.holds(),
            "the home could not be removed: {removal:?}\n{}",
            left_of(&path)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    patience.done();
    // Its log went with its home: its end waited for while it and what it started go on
    // (`common::Wait` on its process, its home gone), and where its threads and its VMs'
    // wait said should it stop going on alive.
    let removed = Instant::now();
    let kind = WaitKind::at(std::panic::Location::caller());
    let mut ending = Wait::begin(kind, daemon.unsigned_abs(), None, "its end with its home");
    while alive(daemon) {
        if let Err(stall) = ending.look(false) {
            let vms: Vec<String> = processes_with(&named)
                .into_iter()
                .map(|vm| format!("VM process {vm}: {}", threads_of(vm as i32)))
                .collect();
            panic!(
                "the daemon outlived its home, {:?} after its removal: {}\ndaemon {daemon}: {}\n{}",
                removed.elapsed(),
                ending.refused(&stall),
                threads_of(daemon),
                vms.join("\n")
            );
        }
        std::thread::sleep(ending.next_look());
    }
    ending.answered();
    all_end(&named, "a restored VM outlived its home");
}

/// A daemon killed outright takes its runs with it: each VM, told how its run stops
/// (`STOP_ORDER`), stops it so once its daemon's connection closes, a run and a run
/// restored from its snapshot alike. Before, they ran on with no daemon to reach them, a
/// restored microVM of a failed test's for a day and a half.
#[test]
fn a_killed_daemons_runs_end_with_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-killed-runs", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env);
    // The restored run wherever this host's daemon keeps templates ([`common::pools`]).
    let snapshots = common::pools();
    let names: &[&str] = if snapshots { &["plain", "back"] } else { &["plain"] };
    for name in names {
        let up = shards(&[
            "run", "-d", "--init", "--name", name, "--pull", "never", &image, "sleep",
        ]);
        assert_eq!(up.status, Some(0), "{}", up.stderr);
    }
    if snapshots {
        assert_eq!(shards(&["save", "vm", "back", "snap"]).status, Some(0));
        assert_eq!(shards(&["restore", "vm", "back"]).status, Some(0));
    }
    let named = home.to_string_lossy().into_owned();
    let daemon = daemon_pid(&home).expect("a daemon pid");
    kill_now(daemon);
    all_end(&named, "runs outlived their killed daemon");
}

/// A daemon killed outright takes its microVMs' joiners (D119) with them: a microVM whose own
/// command the daemon's going stops, and one whose own command had ended already, its joiner
/// going on in it, each end once their daemon has gone. Before, a joiner held its microVM up
/// with nothing left to reach it: the stop order a VM keeps (`STOP_ORDER`) ends its own command
/// alone, and the machine's end was its guest's, which serves its joiners past that command.
#[test]
fn a_killed_daemons_joined_microvms_end_with_it() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-killed-joined", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let shards = |args: &[&str]| run_shards_env(&[], args, &env);
    for provider in ["going", "past"] {
        let up = shards(&[
            "run", "vm", "-d", "--name", provider, "--pull", "never", &image, "sleep",
        ]);
        assert_eq!(up.status, Some(0), "{}", up.stderr);
        let (joiner, network) = (format!("{provider}-joiner"), format!("container:{provider}"));
        let joined = shards(&[
            "run",
            "vm",
            "-d",
            "--name",
            &joiner,
            "--network",
            &network,
            "--pull",
            "never",
            &image,
            "sleep",
        ]);
        assert_eq!(joined.status, Some(0), "{}", joined.stderr);
    }
    // `past`'s own command ended, its joiner going on in its microVM.
    let killed = shards(&["kill", "vm", "past"]);
    assert_eq!(killed.status, Some(0), "{}", killed.stderr);
    let waited = shards(&["wait", "vm", "past"]);
    assert_eq!(waited.status, Some(0), "{}", waited.stderr);
    let named = home.to_string_lossy().into_owned();
    let daemon = daemon_pid(&home).expect("a daemon pid");
    kill_now(daemon);
    all_end(&named, "joined microVMs outlived their killed daemon");
}

/// A home named relatively is one home, wherever it is named from, however long, with
/// spaces, or on Linux in bytes that are not UTF-8: each process resolves it before it
/// makes the home its working directory, and hands the daemon it starts the resolved
/// path (audit A17, whose reproduction the first run is: before, the daemon looked for
/// its lock in the home nested in itself).
#[test]
fn a_home_named_relatively_is_one_home() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let base = TempDir::new("daemon-relative");
    let (one, two) = (base.join("one"), base.join("two"));
    std::fs::create_dir_all(&one).unwrap();
    std::fs::create_dir_all(&two).unwrap();
    fn with(home: &OsStr) -> Vec<(&'static str, &OsStr)> {
        vec![
            ("SHARDS_HOME", home),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ]
    }
    let rel = OsStr::new("a home/with spaces");
    let run = run_shards_env_in(
        &one,
        &["run"],
        &["--name", "first", &image, "exit", "0"],
        &with(rel),
    );
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert!(one.join(rel).join("daemon.pid").is_file(), "not the home named");
    assert!(!one.join(rel).join(rel).exists(), "a home nested in itself");
    let other = OsStr::new("../one/a home/with spaces");
    let listed = run_shards_env_in(&two, &["ps"], &["-a"], &with(other));
    assert_eq!(listed.status, Some(0), "{}", listed.stderr);
    assert!(
        listed.stdout.lines().any(|l| l.ends_with(" first")),
        "{}",
        listed.stdout
    );
    let stopped = run_shards_env_in(&two, &["daemon"], &["stop"], &with(other));
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    // A daemon started for a home that is there: its client has entered it first.
    let again = run_shards_env_in(&two, &["run"], &[&image, "exit", "0"], &with(other));
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    assert!(!one.join(rel).join("one").exists(), "a home nested in itself");
    let stopped = run_shards_env_in(&two, &["daemon"], &["stop"], &with(other));
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);

    // Longer than a socket's address (104 bytes on macOS, 108 on Linux).
    let long = base.join("l".repeat(150)).join("home");
    let run = run_shards_env(&["run"], &[&image, "exit", "0"], &with(long.as_os_str()));
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let stopped = run_shards_env(&["daemon"], &["stop"], &with(long.as_os_str()));
    assert_eq!(stopped.status, Some(0));

    // Linux names files in bytes; macOS's filesystems in UTF-8.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = OsStr::from_bytes(b"odd-\xff-home");
        let run = run_shards_env_in(&one, &["run"], &[&image, "exit", "0"], &with(odd));
        assert_eq!(run.status, Some(0), "{}", run.stderr);
        assert!(one.join(odd).join("daemon.pid").is_file());
        let stopped = run_shards_env_in(&one, &["daemon"], &["stop"], &with(odd));
        assert_eq!(stopped.status, Some(0));

        // The XDG specification has a relative XDG_DATA_HOME ignored.
        let user = base.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let env: [(&str, &OsStr); 2] = [("HOME", user.as_os_str()), ("XDG_DATA_HOME", "relative".as_ref())];
        let listed = run_shards_env_in(&one, &["ps"], &["-a"], &env);
        assert_eq!(listed.status, Some(0), "{}", listed.stderr);
        assert!(user.join(".local/share/shards/daemon.pid").is_file());
        assert!(!one.join("relative").exists());
        let stopped = run_shards_env_in(&one, &["daemon"], &["stop"], &env);
        assert_eq!(stopped.status, Some(0));
    }
}

/// Followers of a quiet microVM cost its daemon nothing: they wait on the log, the
/// run's end and their clients, not a timer, and all end once the microVM does (audit
/// A12: before, each looked for output 50 times a second).
#[test]
fn idle_followers_cost_nothing_and_end_with_their_container() {
    if cannot_run_vms() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-followers", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let up = run_shards_env(
        &["run"],
        &["-d", "--name", "up", "--pull", "never", &image, "sleep"],
        &env,
    );
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let mut followers: Vec<Child> = (0..100)
        .map(|_| {
            common::command()
                .args(["logs", "-f", "up"])
                .env("SHARDS_HOME", &*home)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    // Each served: it has shown the log's line, the `ready` the microVM says as it starts,
    // which its daemon sends it as it takes its request, before it follows. A quiet second
    // alone did not say so: on windows-2022 (88ff4cbc) 92 of them came to their daemon in
    // the 3 s measured after one, their requests' threads' cycles in it what each thread had
    // run in all (a `run` 2.2 M, a `slot-say` 0.1 M), and the slots' maker's 117 M what
    // making their 92 slots costs. Read to their end, so that none waits on its output.
    let (shown, heard) = std::sync::mpsc::channel();
    for follower in &mut followers {
        let out = BufReader::new(follower.stdout.take().unwrap());
        let shown = shown.clone();
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if line == "ready" {
                    let _ = shown.send(());
                }
            }
        });
    }
    drop(shown);
    // Each waited on as its daemon and their microVMs work (`Patience`, D209): a host under
    // load takes a while to start a hundred.
    let began = Instant::now();
    for n in 0..followers.len() {
        let mut patience = common::Patience::of(&home);
        loop {
            match heard.recv_timeout(patience.next_look()) {
                Ok(()) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("followers ended with {n} of 100 served")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => assert!(
                    patience.holds(),
                    "{n} of 100 followers served in {:?}",
                    began.elapsed()
                ),
            }
        }
        patience.done();
    }
    // What 100 idle followers cost their daemon, a second at a time, told with the threads
    // that ran (D209: a cost no later event orders is a measurement, not a bound): a
    // follower that looks for output on a timer (audit A12's, 50 times a second each) shows
    // in every second.
    let daemon = daemon_pid(&home).expect("a daemon pid");
    #[cfg(any(target_os = "linux", windows))]
    let threads = threads_run(daemon);
    let seconds: Vec<Duration> = (0..3)
        .map(|_| {
            let before = cpu_time(daemon);
            std::thread::sleep(Duration::from_secs(1));
            cpu_time(daemon).saturating_sub(before)
        })
        .collect();
    #[cfg(any(target_os = "linux", windows))]
    let ran = {
        let unit = if cfg!(windows) { "cycles" } else { "switches" };
        let ran: Vec<String> = threads_run(daemon)
            .into_iter()
            .filter_map(|(tid, (name, n))| {
                let was = threads.get(&tid).map_or(0, |(_, was)| *was);
                (n > was).then(|| format!("{name} ({tid}) {} {unit}", n - was))
            })
            .collect();
        ran.join(", ")
    };
    #[cfg(not(any(target_os = "linux", windows)))]
    let ran = "unread here";
    common::measured(
        &format!("a daemon's CPU a second with 100 idle followers (its threads that ran: {ran})"),
        &seconds,
    );
    let killed = run_shards_env(&["kill"], &["up"], &env);
    // A container its VM's host refused to enter had ended with it, as D261's gate takes the
    // refusal for its host's: the test skips, said so (CI 44379986478, x86_64 musl: VM
    // 201354's vCPU 3, 0x7, after its guest ran; `kill up` said it was not running).
    if killed.status != Some(0) && common::container_host_refused(up.stdout.trim(), &home) {
        return;
    }
    assert_eq!(killed.status, Some(0), "{}", killed.stderr);
    for f in &mut followers {
        let mut patience = Patience::of(&home);
        loop {
            if let Some(status) = f.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                break;
            }
            assert!(patience.holds(), "a follower outlived its container");
            std::thread::sleep(patience.next_look());
        }
        patience.done();
    }
}
