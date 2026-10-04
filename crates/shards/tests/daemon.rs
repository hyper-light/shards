//! The daemon, end to end (crates/shards/src/daemon.rs): `shards run` through the daemon
//! its first run starts, served from pools of warm VMs of the image's template. Real VMs;
//! the image comes from a loopback registry (tests/common, `served`), and its entrypoint
//! is the test guest, so a command is one of its modes. Needs vsock and snapshots.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, registry_of, rootfs_dir, run_shards_env,
    run_shards_env_in, served, served_variant, shards, shards_vm, shardsd, test_image, test_image_with,
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A home with the image pulled, the guest recorded, and the image's template saved by a
/// first run, whose daemon now serves it.
fn home(name: &str, image: &str) -> TempDir {
    let home = recorded(name);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let first = run_shards_env(&["run"], &[image, "exit", "0"], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    home
}

/// A home whose runs boot the test kernel and init, recorded as its guest, so that they
/// save and restore templates.
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
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
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
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = recorded("daemon-clean-cache");
    let env = [("SHARDS_HOME", home.as_os_str())];
    for which in ["first", "restored"] {
        let run = run_shards_env(
            &["run"],
            &[&image, "clean-cache", "/bin/testguest"],
            &env,
            TIMEOUT,
        );
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
    Command::new(shards())
        .arg("run")
        .args(args)
        .env("SHARDS_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits for a child, failing the test rather than hanging it.
fn wait(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("shards run did not exit within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The pids of the processes whose command line mentions `needle`: from /proc on Linux,
/// whose `ps` may be busybox's, which takes no `-o args`; from `ps` elsewhere.
fn processes_with(needle: &str) -> Vec<u32> {
    if cfg!(target_os = "linux") {
        return std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
                let args = String::from_utf8_lossy(&cmdline).replace('\0', " ");
                args.contains(needle).then_some(pid)
            })
            .collect();
    }
    let out = Command::new("ps").args(["-axo", "pid=,args="]).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(needle))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Waits until `check` holds, for up to five seconds.
fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn daemon_pid(home: &Path) -> Option<i32> {
    std::fs::read_to_string(home.join("daemon.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether no daemon holds `home`'s lock.
fn home_is_free(home: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let lock = std::fs::File::open(home.join("daemon.lock")).unwrap();
    // SAFETY: flock(2) on a descriptor we own, released as it closes.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn parallel_runs_keep_their_own_stdio() {
    if cannot_run_vms() || cannot_snapshot() {
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
        assert_eq!(wait(&mut child), Some(0), "run {i}: {err}");
        let whos: Vec<&str> = out.lines().filter(|l| l.starts_with("env WHO=")).collect();
        assert_eq!(
            whos,
            [format!("env WHO={i}")],
            "run {i} saw another's stdio:\n{out}"
        );
    }
}

/// Signals reach the command, even when the client was started ignoring them, as a
/// script's `shards run ... &` is: `docker run`'s signal proxy takes them all the same.
#[test]
fn signals_reach_a_warm_run() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-signals", &image);
    for ignored in [false, true] {
        let mut run = Command::new(shards());
        run.args(["run", "--pull", "never", &image, "trap", "INT"])
            .env("SHARDS_HOME", &*home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        // SAFETY: kill(2) of our own child.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
        let mut rest = String::new();
        out.read_to_string(&mut rest).unwrap();
        assert_eq!(wait(&mut child), Some(0), "ignored at start: {ignored}");
        assert_eq!(rest, "got 2\n", "ignored at start: {ignored}");
    }
}

#[test]
fn errors_reach_the_client() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-errors", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let missing = run_shards_env(&["run"], &["--pull", "never", "no/such:image"], &env, TIMEOUT);
    assert_eq!(missing.status, Some(125), "{}", missing.stderr);
    assert!(missing.stderr.contains("No such image"), "{}", missing.stderr);
    let not_found = run_shards_env(
        &["run"],
        &["--pull", "never", "--entrypoint", "/bin/nonexistent", &image],
        &env,
        TIMEOUT,
    );
    assert_eq!(not_found.status, Some(127), "{}", not_found.stderr);
}

/// `stop` ends the runs in progress as dockerd ends its containers when it shuts down:
/// SIGTERM to the command, whose client gets its status. The pool's waiting VMs end too.
#[test]
fn stop_ends_runs_and_waiting_vms() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stop", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    let mut sleeper = spawn_run(&home, &["--pull", "never", &image, "sleep"]);
    let mut out = BufReader::new(sleeper.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    let daemon = daemon_pid(&home).expect("a daemon pid");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    // It has let go of its home; the process ends as the kernel finishes its exit, and
    // init, which adopted it, reaps it (kill(2) finds a zombie too).
    assert!(home_is_free(&home), "stop returned before the daemon exited");
    eventually("the daemon outlived its stop", || !alive(daemon));
    assert_eq!(
        wait(&mut sleeper),
        Some(128 + 15),
        "the run was not stopped by SIGTERM"
    );
    eventually("VMs outlived the daemon's stop", || {
        processes_with(&templates).is_empty()
    });
}

/// `daemon stop` stops each run as dockerd stops each container as it shuts down: with
/// its own stop signal, and SIGKILL once its own stop timeout is up.
#[test]
fn a_stop_gives_each_run_its_own_signal_and_time() {
    if cannot_run_vms() || cannot_snapshot() {
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
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    let took = t0.elapsed();
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    assert_eq!(
        wait(&mut caught),
        Some(0),
        "its own signal, caught: {}",
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    let mut said = String::new();
    std::io::Read::read_to_string(caught.stdout.as_mut().unwrap(), &mut said).unwrap();
    // Linux numbers SIGUSR1 10.
    assert_eq!(said, "got 10\n");
    assert_eq!(
        wait(&mut ignoring),
        Some(128 + 9),
        "killed once its own time was up: {}",
        std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
    );
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(5),
        "{took:?}"
    );
}

/// A daemon ending its runs is waited for as long as their stop timeouts make it: a run
/// asked for meanwhile is served by the next daemon once it is gone.
#[test]
fn a_daemon_ending_its_runs_is_waited_for() {
    if cannot_run_vms() || cannot_snapshot() {
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
    let mut stopping = Command::new(shards())
        .args(["daemon", "stop"])
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Ending its runs, and no longer listening: a run asked for before then is one it
    // serves itself.
    eventually("the daemon did not begin to stop", || {
        home.join("daemon.stopping").exists() && !home.join("daemon.sock").exists()
    });
    let env = [("SHARDS_HOME", home.as_os_str())];
    let t0 = Instant::now();
    let next = run_shards_env(&["run"], &["--pull", "never", &image, "exit", "0"], &env, TIMEOUT);
    assert_eq!(next.status, Some(0), "{}", next.stderr);
    assert!(t0.elapsed() >= Duration::from_secs(30), "{:?}", t0.elapsed());
    assert_eq!(wait(&mut slow), Some(128 + 9));
    assert_eq!(wait(&mut stopping), Some(0));
    assert!(!alive(before), "the old daemon still lives");
    assert_ne!(daemon_pid(&home), Some(before));
}

/// A daemon that dies leaves no VM waiting for a run it will never send.
#[test]
fn a_dead_daemons_waiting_vms_end() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-killed", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    eventually("the pool did not fill", || !processes_with(&templates).is_empty());
    let daemon = daemon_pid(&home).expect("a daemon pid");
    // SAFETY: kill(2) of the daemon this test's run started.
    unsafe { libc::kill(daemon, libc::SIGKILL) };
    eventually("warm VMs outlived their daemon", || {
        processes_with(&templates).is_empty()
    });
}

/// A client from another build replaces the daemon, and its run is served. Another file
/// is another build: the daemon knows `shardsd` by its file's identity.
#[test]
fn a_rebuilt_binary_replaces_the_daemon() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-rebuilt", &image);
    let before = daemon_pid(&home).expect("a daemon pid");
    let bin = TempDir::new("daemon-rebuilt-bin");
    let other = bin.join("shards");
    std::fs::copy(shards(), &other).unwrap();
    std::fs::copy(shardsd(), bin.join("shardsd")).unwrap();
    std::fs::copy(shards_vm(), bin.join("shards-vm")).unwrap();
    std::fs::copy(common::shards_net(), bin.join("shards-net")).unwrap();
    let out = Command::new(&other)
        .args(["run", "--pull", "never", &image, "exit", "0"])
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = daemon_pid(&home).expect("a daemon pid");
    assert_ne!(before, after, "the old daemon still serves");
    eventually("the old daemon did not exit", || !alive(before));
}

#[test]
fn idle_daemons_exit() {
    if cannot_run_vms() || cannot_snapshot() {
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
    let run = run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env, TIMEOUT);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    eventually("the idle daemon did not exit", || !alive(daemon));
    assert!(daemon_pid(&home).is_none(), "the daemon left its pid file");
}

/// A daemon's idle time runs from its last run's end, which nothing else need follow: the
/// run's own end starts the clock, long after its client left the daemon. Its home holds
/// the image's template already, so the run changes nothing there the daemon watches.
#[test]
fn a_daemon_idles_from_its_last_runs_end() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-idle-end", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    let mut client = Command::new(shards())
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
    // Past its idle time, but with a run: it stays.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(alive(daemon), "the daemon exited while a run went on");
    drop(stdin);
    assert_eq!(wait(&mut client), Some(0));
    eventually("the daemon did not exit once idle after its last run", || {
        !alive(daemon)
    });
}

/// With -i the command reads the client's stdin, and that stdin ends when the client
/// goes, as `docker run -i`'s does (StdinOnce): `cat` then ends, and its VM with it.
#[test]
fn interactive_stdin_ends_with_its_client() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-stdin", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    // The warm VMs one of which serves the run, once the VMs of the runs before have
    // gone: the same for 300 ms.
    let mut waiting = processes_with(&templates);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut since = Instant::now();
    while waiting.is_empty() || since.elapsed() < Duration::from_millis(300) {
        assert!(Instant::now() < deadline, "the pool did not settle: {waiting:?}");
        std::thread::sleep(Duration::from_millis(20));
        let now = processes_with(&templates);
        if now != waiting {
            (waiting, since) = (now, Instant::now());
        }
    }
    let mut client = Command::new(shards())
        .args(["run", "-i", "--pull", "never", &image, "cat"])
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
    // The pool refills while the run goes on: one waiting, besides the one serving.
    eventually("the pool did not refill", || {
        processes_with(&templates).len() > waiting.len()
    });
    client.kill().unwrap();
    client.wait().unwrap();
    // The VM that served it ends; those still waiting stay.
    eventually("the run outlived its client's stdin", || {
        let now = processes_with(&templates);
        waiting.iter().filter(|pid| now.contains(pid)).count() == waiting.len() - 1
    });
}

/// `SHARDS_TIMING` reaches a warm run's client, which prints the line the VM measured.
#[test]
fn timing_reaches_the_client() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-timing", &image);
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_TIMING", "1".as_ref())];
    let run = run_shards_env(&["run"], &["--pull", "never", &image, "exit", "0"], &env, TIMEOUT);
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
/// client stopped.
#[test]
fn a_background_run_stops_to_read_its_terminal() {
    if let Some(args) = std::env::var_os("SHARDS_TEST_SHELL") {
        return shell(&args);
    }
    if cannot_run_vms() || cannot_snapshot() {
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
    let deadline = Instant::now() + TIMEOUT;
    let mut status = 0;
    loop {
        // SAFETY: waitpid(2) for our own child, stops included.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == pid || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
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

/// Starts `home`'s daemon with `shards ps`, which needs no VM, under a limit of `limit`
/// open descriptors if one is given, which the daemon inherits; its pid.
fn start_daemon(home: &Path, limit: Option<u32>) -> i32 {
    let mut cmd = match limit {
        Some(n) => {
            let mut sh = Command::new("/bin/sh");
            sh.arg("-c")
                .arg(format!("ulimit -n {n} && exec \"$0\" ps"))
                .arg(shards());
            sh
        }
        None => {
            let mut plain = Command::new(shards());
            plain.arg("ps");
            plain
        }
    };
    let out = cmd
        .env("SHARDS_HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    daemon_pid(home).expect("a daemon pid")
}

/// Raises this process's soft limit on open descriptors as far as it goes, up to macOS's
/// OPEN_MAX: a test holding hundreds of connections needs more than the 256 macOS starts
/// a process with.
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

/// A connection to the daemon at `sock`, patient as the client is: macOS refuses
/// connections past a listener's backlog (at most kern.ipc.somaxconn, 128), where Linux
/// makes them wait.
fn connect_patiently(sock: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(sock) {
            Ok(conn) => return conn,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("{}: {e}", sock.display()),
        }
    }
}

/// The CPU time process `pid` has used: from /proc on Linux, from `ps` elsewhere.
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
/// raises its own: macOS starts processes with 256 (audit A07). No VM needed.
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
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg("ulimit -S -n 256 && exec \"$0\" ps")
        .arg(shards())
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
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A client that connects and says nothing holds up no shutdown: the daemon ends it as it
/// stops. The audit's reproduction (A07): before, `daemon stop` waited until the client
/// closed. No VM needed.
#[test]
fn a_client_that_says_nothing_holds_up_no_stop() {
    let home = TempDir::new("daemon-silent");
    let daemon = start_daemon(&home, None);
    let silent = UnixStream::connect(home.join("daemon.sock")).unwrap();
    let t0 = Instant::now();
    let env = [("SHARDS_HOME", home.as_os_str())];
    // Short of the daemon's 10 s deadline for a request, which would end it too.
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, Duration::from_secs(8));
    assert_eq!(stopped.status, Some(0), "{stopped}");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    eventually("the daemon outlived its stop", || !alive(daemon));
    let mut byte = [0u8; 1];
    assert_eq!(
        (&silent).read(&mut byte).unwrap_or(0),
        0,
        "the client was not let go"
    );
}

/// Past its cap of 256 clients in hand, the daemon leaves connections in its backlog, and
/// takes the next as soon as one in hand leaves (audit A07). No VM needed.
#[test]
fn clients_past_the_cap_wait_their_turn() {
    more_descriptors();
    let home = TempDir::new("daemon-cap");
    let daemon = start_daemon(&home, None);
    // Its soft limit on descriptors is raised to its hard limit, where Linux shows it.
    if let Ok(limits) = std::fs::read_to_string(format!("/proc/{daemon}/limits")) {
        let line = limits.lines().find(|l| l.starts_with("Max open files")).unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields[3], fields[4], "{line}");
    }
    let sock = home.join("daemon.sock");
    let mut silent: Vec<UnixStream> = (0..256).map(|_| connect_patiently(&sock)).collect();
    let mut listing = Command::new(shards())
        .arg("ps")
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Well within the 10 s the silent ones have to send their requests.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        listing.try_wait().unwrap().is_none(),
        "a client past the cap was taken"
    );
    let t0 = Instant::now();
    drop(silent.pop());
    assert_eq!(wait(&mut listing), Some(0));
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    drop(silent);
    let env = [("SHARDS_HOME", home.as_os_str())];
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A home removed once, as `rm -rf` or a test's directory removes it, goes with its daemon
/// whatever order its names go in. One that takes the socket first, which the daemon then
/// listens at again, and the rest after, the lock among them, fails to remove the
/// directory, not empty: the daemon, seeing its lock gone, exits and removes what it made.
/// Before, it listened on in a home holding nothing but its socket. No VM needed.
#[test]
fn a_home_removed_once_is_gone_with_its_daemon() {
    let home = TempDir::new("daemon-home-once");
    let daemon = start_daemon(&home, None);
    let path = home.to_path_buf();
    let sock = path.join("daemon.sock");
    std::fs::remove_file(&sock).unwrap();
    eventually("the daemon did not listen again", || sock.exists());
    // Every other name but the lock, then the directory, which the socket keeps: the
    // removal has failed. Then the lock, which a removal in another order takes before
    // it fails: the daemon, seeing it gone, exits and finishes the removal.
    let lock = path.join("daemon.lock");
    for entry in std::fs::read_dir(&path).unwrap() {
        let name = entry.unwrap().path();
        if name == sock || name == lock {
            continue;
        }
        // The daemon may be writing in it as it goes, a spare container: until it is gone.
        if name.is_dir() {
            eventually("a directory of the home could not be removed", || {
                std::fs::remove_dir_all(&name).is_ok() || !name.exists()
            });
        } else {
            std::fs::remove_file(&name).unwrap();
        }
    }
    assert!(
        std::fs::remove_dir(&path).is_err(),
        "removed with the socket in it"
    );
    std::fs::remove_file(&lock).unwrap();
    eventually("the daemon outlived its home", || !alive(daemon));
    eventually("what was left of the home was left", || !path.exists());
}

/// A daemon out of descriptors leaves connections in its backlog and waits for room,
/// rather than spinning on a listener that stays readable (Linux), or dropping them one
/// by one as its accepts fail (macOS), and serves again once clients leave (audit A07).
/// No VM needed.
#[test]
fn a_daemon_out_of_descriptors_waits_for_room() {
    let home = TempDir::new("daemon-starved");
    let daemon = start_daemon(&home, Some(48));
    let sock = home.join("daemon.sock");
    let log = home.join("daemon.log");
    let silent: Vec<UnixStream> = (0..64).map(|_| connect_patiently(&sock)).collect();
    // The daemon's own words: EMFILE's text is the C library's, and musl's differs.
    eventually("the daemon never ran out of descriptors", || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("clients wait until the daemon has room")
    });
    let before = cpu_time(daemon);
    std::thread::sleep(Duration::from_secs(1));
    let spent = cpu_time(daemon).saturating_sub(before);
    assert!(
        spent < Duration::from_millis(300),
        "the daemon spun: {spent:?} of CPU in 1 s"
    );
    // Held by the daemon, or waiting in its backlog, a connection has nothing to read; one
    // dropped reads its end. macOS drops the one whose accept found no descriptor.
    let dropped = silent
        .iter()
        .filter(|conn| {
            let mut pfd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(*conn),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) on one valid pollfd, without waiting.
            unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
        })
        .count();
    assert!(dropped <= 1, "{dropped} waiting clients were dropped");
    // A collection due meanwhile waits: its files would take what clients wait for.
    let left = home.join("images").join("ingest").join("left");
    std::fs::create_dir_all(left.parent().unwrap()).unwrap();
    std::fs::write(&left, b"left behind").unwrap();
    let due = home.join("images").join("collect-due");
    std::fs::write(&due, b"").unwrap();
    eventually("the collection due was not seen", || !due.exists());
    std::thread::sleep(Duration::from_millis(500));
    assert!(left.exists(), "collected while out of descriptors");
    drop(silent);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let listed = run_shards_env(&["ps"], &[] as &[&str], &env, TIMEOUT);
    assert_eq!(listed.status, Some(0), "{listed}");
    eventually("not collected once the daemon had room", || !left.exists());
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A daemon told to stop cancels what its runs are downloading: here a pull from a
/// registry that accepts connections and never answers, which would hold the stop for
/// 30 s a try. The run is refused as the daemon shuts down (audit A07). No VM needed.
#[test]
fn a_stop_cancels_the_downloads_of_runs_being_prepared() {
    use std::net::TcpListener;
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
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
    let run = Command::new(shards())
        .args(["run", "--pull", "always", &image, "exit", "0"])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    eventually("the pull never reached the registry", || {
        !accepted.lock().unwrap().is_empty()
    });
    let t0 = Instant::now();
    let env = [("SHARDS_HOME", home.as_os_str())];
    let stopped = run_shards_env(&["daemon"], &["stop"], &env, Duration::from_secs(20));
    assert_eq!(stopped.status, Some(0), "{stopped}");
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let out = run.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{said}");
    assert!(said.contains("the daemon is shutting down"), "{said}");
}

/// A home with the guest recorded, whose daemon starts with `settings`.
fn home_with(name: &str) -> TempDir {
    let home = TempDir::new(name);
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel().as_os_str(),
        "--init".as_ref(),
        guest_init().as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &[("SHARDS_HOME", home.as_os_str())], TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    home
}

/// A pool of 0 keeps no VM warm, and every run restores its own: the second run of an
/// image, from its template, is served at once, not after the 60 s a pool that never
/// fills would take (audit A14, whose reproduction this is).
#[test]
fn a_pool_of_none_serves_every_run() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-pool-none");
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_POOL", "0".as_ref())];
    let templates = home.join("templates").to_string_lossy().into_owned();
    for i in 0..3 {
        let t0 = Instant::now();
        let run = run_shards_env(&["run"], &[&image, "exit", "7"], &env, TIMEOUT);
        assert_eq!(run.status, Some(7), "run {i}: {}", run.stderr);
        if i > 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "run {i}: {:?}",
                t0.elapsed()
            );
        }
    }
    eventually("a VM was kept warm", || processes_with(&templates).is_empty());
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
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
        (
            &[("SHARDS_WARM_MAX", "100000")][..],
            "SHARDS_WARM_MAX: 100000 is more than",
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
        let run = run_shards_env(&["run"], &[&image, "exit", "0"], &env, TIMEOUT);
        assert_ne!(run.status, Some(0), "{settings:?}");
        assert!(run.stderr.contains(said), "{settings:?}: {}", run.stderr);
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "{settings:?}: {:?}",
            t0.elapsed()
        );
        assert!(
            daemon_pid(&home).is_none_or(|pid| !alive(pid)),
            "{settings:?}: it serves"
        );
    }
}

/// Warm VMs kept ahead of runs are bounded all pools together (audit A13): with room
/// for one, a second template's pool takes the first's, least recently claimed from, and
/// the first is still served, on demand.
#[test]
fn warm_vms_are_bounded_across_templates() {
    if cannot_run_vms() || cannot_snapshot() {
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
        let run = run_shards_env(&["run"], &[image, "exit", "0"], &env, TIMEOUT);
        assert_eq!(run.status, Some(0), "{image}: {}", run.stderr);
    };
    run(&first);
    run(&first);
    eventually("the first pool did not fill", || warm(&templates) == 1);
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(warm(newest) == 1 && warm(&templates) == 1) {
        assert!(
            Instant::now() < deadline,
            "the second pool did not take the room: {} of {} warm, of {dirs:?}\n{}",
            warm(newest),
            warm(&templates),
            std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Never more than the bound, as the first is served again.
    run(&first);
    std::thread::sleep(Duration::from_millis(500));
    assert!(warm(&templates) <= 1, "{} warm VMs", warm(&templates));
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A pool keeps what its runs need while they come (audit A13): runs well apart keep one
/// warm VM, not the most a pool may keep; runs at once keep more, up to that most; and a
/// pool unclaimed past `SHARDS_POOL_KEEP` keeps none. The next run is served all the
/// same, and its pool keeps VMs again.
/// Each warm VM of a pool is given its template's root filesystem (`--backing`), the
/// one file a restore of it may name, so that what the VM saving the template wrote does
/// not choose what the next is given.
#[test]
fn warm_vms_are_given_their_templates_root_filesystem() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-backing");
    let env: [(&str, &OsStr); 2] = [("SHARDS_HOME", home.as_os_str()), ("SHARDS_POOL", "2".as_ref())];
    let run = run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env, TIMEOUT);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let templates = home.join("templates").to_string_lossy().into_owned();
    let warm = || -> Vec<String> {
        let out = Command::new("ps").args(["-axo", "args="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&templates) && l.contains(" restore "))
            .map(String::from)
            .collect()
    };
    eventually("the pool refilled", || !warm().is_empty());
    for vm in warm() {
        assert!(vm.contains(" --backing /") && vm.contains("/rootfs/"), "{vm}");
    }
}

#[test]
fn pools_keep_what_their_runs_need_while_they_come() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home_with("daemon-demand");
    // The keep-alive outlasts the spacing of runs well apart below, which a loaded host
    // stretches.
    const KEEP: Duration = Duration::from_secs(10);
    let keep = KEEP.as_secs().to_string();
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_POOL", "3".as_ref()),
        ("SHARDS_POOL_KEEP", keep.as_ref()),
    ];
    let templates = home.join("templates");
    let warm = || processes_with(&templates.to_string_lossy()).len();
    let settled = |what: &str, want: &dyn Fn(usize) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !want(warm()) {
            assert!(
                Instant::now() < deadline,
                "{what}: {} warm\n{}",
                warm(),
                std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let args = [image.as_str(), "exit", "0"];
    let run = || {
        let run = run_shards_env(&["run"], &args, &env, TIMEOUT);
        assert_eq!(run.status, Some(0), "{}", run.stderr);
    };
    // Runs well apart: each is served by the one VM refilled for the run before. "Apart"
    // is past the pool's refill window, SRTT + 4·RTTVAR of its refills (RFC 6298, as
    // demand.rs keeps it), which refills of at most M keep under 5·M: each run waits at
    // least that past the slowest refill seen here, which is as long as a refill or
    // longer, and at least a second.
    let mut slowest = Duration::ZERO;
    for _ in 0..4 {
        run();
        let ended = Instant::now();
        settled("a refill", &|n| n >= 1);
        slowest = slowest.max(ended.elapsed());
        let apart = (slowest * 5).max(Duration::from_secs(1));
        assert!(
            apart < KEEP,
            "refills of {slowest:?} leave no room under the keep-alive"
        );
        settled("one run at a time", &|n| n == 1);
        std::thread::sleep(apart);
    }
    // Three at once: the pool keeps more for the next burst, never past its most.
    let burst: Vec<_> = (0..3)
        .map(|_| {
            let env: Vec<(String, std::ffi::OsString)> = env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_os_string()))
                .collect();
            let image = image.clone();
            std::thread::spawn(move || {
                let env: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
                run_shards_env(&["run"], &[image.as_str(), "exit", "0"], &env, TIMEOUT).status
            })
        })
        .collect();
    for b in burst {
        assert_eq!(b.join().unwrap(), Some(0));
    }
    settled("a burst of three", &|n| n >= 2);
    std::thread::sleep(Duration::from_millis(500));
    assert!(warm() <= 3, "{} warm, past the pool's most", warm());
    // Unclaimed past its keep-alive: nothing, until the next run.
    std::thread::sleep(KEEP);
    settled("past the keep-alive", &|n| n == 0);
    run();
    settled("after the keep-alive", &|n| n >= 1);
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A collection another process leaves due, as `shards build`'s pull of a base image
/// leaves one, is taken by a daemon that nothing else wakes: its watch of `images` sees
/// the mark come.
#[test]
fn a_collection_another_process_leaves_due_is_taken() {
    let home = TempDir::new("daemon-due");
    let daemon = start_daemon(&home, None);
    // Once its client's going has woken it, and it sleeps again.
    std::thread::sleep(Duration::from_millis(300));
    let due = home.join("images").join("collect-due");
    std::fs::write(&due, b"").unwrap();
    eventually("the daemon did not take the collection due", || !due.exists());
    assert!(alive(daemon));
}

/// A collection due while another process holds the image store's lease, as `shards
/// build` holds it while it prepares a root filesystem, runs once the lease is let go,
/// though nothing else happens meanwhile.
#[test]
fn a_collection_waits_for_the_stores_lease() {
    let (image, _) = served();
    let home = TempDir::new("daemon-lease");
    start_daemon(&home, None);
    // A process of its own, whose collection the daemon takes.
    let env = [("SHARDS_HOME", home.as_os_str())];
    let pulled = run_shards_env(&["pull"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    let images = home.join("images");
    let due = images.join("collect-due");
    eventually("the pull's collection was not taken", || !due.exists());
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
    eventually("the collection due was not seen", || !due.exists());
    std::thread::sleep(Duration::from_millis(500));
    assert!(left.exists(), "collected under the lease");
    drop(lease);
    eventually("nothing collected once the lease was let go", || !left.exists());
}

/// What no reference needs is collected (audit A13). `shards pull` moving a tag to another
/// image leaves the old one dangling, as dockerd leaves it; once `rmi` removes it, the
/// daemon removes its blobs, root filesystem and template, ending its warm VMs, and keeps
/// the new one's, which runs from a template of its own.
#[test]
fn what_a_moved_tag_named_is_collected() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (port, _, serving) = registry_of(std::sync::Arc::new(std::sync::Mutex::new(test_image())));
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = home_with("daemon-collect");
    let env = [("SHARDS_HOME", home.as_os_str())];
    let run = || {
        let run = run_shards_env(&["run"], &["--rm", image.as_str(), "exit", "0"], &env, TIMEOUT);
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
            !name.starts_with('.') && !name.contains(".new-")
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
    eventually("no warm VM", || !processes_with(&template).is_empty());

    let old = run_shards_env(&["images"], &["-q", "--no-trunc", image.as_str()], &env, TIMEOUT);
    let old = old.stdout.trim().to_string();
    *serving.lock().unwrap() = test_image_with(Some(b"moved"));
    let pulled = run_shards_env(&["pull"], &[image.as_str()], &env, TIMEOUT);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    // Dangling, it is kept, its template too.
    let listed_now = run_shards_env(&["images"], &["-a", "-q", "--no-trunc"], &env, TIMEOUT);
    assert!(
        listed_now.stdout.lines().any(|l| l == old),
        "{}",
        listed_now.stdout
    );
    assert!(rootfs[0].exists() && templates[0].exists());
    let removed = run_shards_env(&["rmi"], &[old.as_str()], &env, TIMEOUT);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    eventually("the old image was not collected", || {
        blobs.iter().all(|b| !b.exists()) && !rootfs[0].exists() && !templates[0].exists()
    });
    eventually("the old template's VMs outlived it", || {
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
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
}

/// A daemon whose home is removed ends its runs, whose containers went with the home, and
/// exits, and never makes the home again: before, the spare container it made after the
/// removal made the home's path anew, with its own directory inside, and a run kept its
/// daemon and VM going with no container left to reach them by.
#[test]
fn a_daemon_whose_home_is_removed_exits_without_making_it_again() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-home-removed", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let up = run_shards_env(
        &["run"],
        &["-d", "--name", "up", "--pull", "never", &image, "sleep"],
        &env,
        TIMEOUT,
    );
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let daemon = daemon_pid(&home).expect("a daemon pid");
    let path = home.to_path_buf();
    let named = path.to_string_lossy().into_owned();
    // The daemon may be writing in it as it goes, a spare container or a refill: removed
    // again until it is gone.
    eventually("the home could not be removed", || {
        matches!(std::fs::remove_dir_all(&path), Ok(())) || !path.exists()
    });
    eventually("the daemon outlived its home", || !alive(daemon));
    eventually("VMs outlived their home", || processes_with(&named).is_empty());
    std::thread::sleep(Duration::from_millis(500));
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
        TIMEOUT,
    );
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert!(one.join(rel).join("daemon.pid").is_file(), "not the home named");
    assert!(!one.join(rel).join(rel).exists(), "a home nested in itself");
    let other = OsStr::new("../one/a home/with spaces");
    let listed = run_shards_env_in(&two, &["ps"], &["-a"], &with(other), TIMEOUT);
    assert_eq!(listed.status, Some(0), "{}", listed.stderr);
    assert!(
        listed.stdout.lines().any(|l| l.ends_with(" first")),
        "{}",
        listed.stdout
    );
    let stopped = run_shards_env_in(&two, &["daemon"], &["stop"], &with(other), TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    // A daemon started for a home that is there: its client has entered it first.
    let again = run_shards_env_in(&two, &["run"], &[&image, "exit", "0"], &with(other), TIMEOUT);
    assert_eq!(again.status, Some(0), "{}", again.stderr);
    assert!(!one.join(rel).join("one").exists(), "a home nested in itself");
    let stopped = run_shards_env_in(&two, &["daemon"], &["stop"], &with(other), TIMEOUT);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);

    // Longer than a socket's address (104 bytes on macOS, 108 on Linux).
    let long = base.join("l".repeat(150)).join("home");
    let run = run_shards_env(&["run"], &[&image, "exit", "0"], &with(long.as_os_str()), TIMEOUT);
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    let stopped = run_shards_env(&["daemon"], &["stop"], &with(long.as_os_str()), TIMEOUT);
    assert_eq!(stopped.status, Some(0));

    // Linux names files in bytes; macOS's filesystems in UTF-8.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = OsStr::from_bytes(b"odd-\xff-home");
        let run = run_shards_env_in(&one, &["run"], &[&image, "exit", "0"], &with(odd), TIMEOUT);
        assert_eq!(run.status, Some(0), "{}", run.stderr);
        assert!(one.join(odd).join("daemon.pid").is_file());
        let stopped = run_shards_env_in(&one, &["daemon"], &["stop"], &with(odd), TIMEOUT);
        assert_eq!(stopped.status, Some(0));

        // The XDG specification has a relative XDG_DATA_HOME ignored.
        let user = base.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let env: [(&str, &OsStr); 2] = [("HOME", user.as_os_str()), ("XDG_DATA_HOME", "relative".as_ref())];
        let listed = run_shards_env_in(&one, &["ps"], &["-a"], &env, TIMEOUT);
        assert_eq!(listed.status, Some(0), "{}", listed.stderr);
        assert!(user.join(".local/share/shards/daemon.pid").is_file());
        assert!(!one.join("relative").exists());
        let stopped = run_shards_env_in(&one, &["daemon"], &["stop"], &env, TIMEOUT);
        assert_eq!(stopped.status, Some(0));
    }
}

/// Followers of a quiet container cost its daemon nothing: they wait on the log, the
/// run's end and their clients, not a timer, and all end once the container does (audit
/// A12: before, each looked for output 50 times a second).
#[test]
fn idle_followers_cost_nothing_and_end_with_their_container() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-followers", &image);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let up = run_shards_env(
        &["run"],
        &["-d", "--name", "up", "--pull", "never", &image, "sleep"],
        &env,
        TIMEOUT,
    );
    assert_eq!(up.status, Some(0), "{}", up.stderr);
    let mut followers: Vec<Child> = (0..100)
        .map(|_| {
            Command::new(shards())
                .args(["logs", "-f", "up"])
                .env("SHARDS_HOME", &*home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::sleep(Duration::from_secs(1));
    let daemon = daemon_pid(&home).expect("a daemon pid");
    let before = cpu_time(daemon);
    std::thread::sleep(Duration::from_secs(3));
    let spent = cpu_time(daemon).saturating_sub(before);
    assert!(
        spent < Duration::from_millis(100),
        "100 idle followers cost {spent:?} of CPU in 3 s"
    );
    let killed = run_shards_env(&["kill"], &["up"], &env, TIMEOUT);
    assert_eq!(killed.status, Some(0), "{}", killed.stderr);
    let deadline = Instant::now() + Duration::from_secs(5);
    for f in &mut followers {
        loop {
            if let Some(status) = f.try_wait().unwrap() {
                assert!(status.success(), "{status:?}");
                break;
            }
            assert!(Instant::now() < deadline, "a follower outlived its container");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
