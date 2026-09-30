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
    TempDir, cannot_run_vms, cannot_snapshot, guest_init, kernel, run_shards_env, run_shards_env_in, served,
    served_variant, shards, shards_vm, shardsd,
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A home with the image pulled, the guest recorded, and the image's template saved by a
/// first run, whose daemon now serves it.
fn home(name: &str, image: &str) -> TempDir {
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
    let first = run_shards_env(&["run"], &[image, "exit", "0"], &env, TIMEOUT);
    assert_eq!(first.status, Some(0), "{}", first.stderr);
    home
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

/// A daemon that dies leaves no VM waiting for a run it will never send.
#[test]
fn a_dead_daemons_waiting_vms_end() {
    if cannot_run_vms() || cannot_snapshot() {
        return;
    }
    let (image, _) = served();
    let home = home("daemon-killed", &image);
    let templates = home.join("templates").to_string_lossy().into_owned();
    eventually("the pool did not fill", || processes_with(&templates).len() >= 2);
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
    // The pool refills while the run goes on: two waiting, one serving.
    eventually("the pool did not refill", || {
        processes_with(&templates).len() == 3
    });
    client.kill().unwrap();
    client.wait().unwrap();
    eventually("the run outlived its client's stdin", || {
        processes_with(&templates).len() == 2
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
    drop(silent);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let listed = run_shards_env(&["ps"], &[] as &[&str], &env, TIMEOUT);
    assert_eq!(listed.status, Some(0), "{listed}");
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
/// for two, a second template's pool takes the first's, least recently claimed from, and
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
        ("SHARDS_POOL", "2".as_ref()),
        ("SHARDS_WARM_MAX", "2".as_ref()),
    ];
    let templates = home.join("templates");
    let warm = |template: &Path| processes_with(&template.to_string_lossy()).len();
    let run = |image: &str| {
        let run = run_shards_env(&["run"], &[image, "exit", "0"], &env, TIMEOUT);
        assert_eq!(run.status, Some(0), "{image}: {}", run.stderr);
    };
    run(&first);
    run(&first);
    eventually("the first pool did not fill", || warm(&templates) == 2);
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
    eventually("the second pool did not take the room", || {
        warm(newest) == 2 && warm(&templates) == 2
    });
    // Never more than the bound, as the first is served again.
    run(&first);
    std::thread::sleep(Duration::from_millis(500));
    assert!(warm(&templates) <= 2, "{} warm VMs", warm(&templates));
    assert_eq!(
        run_shards_env(&["daemon"], &["stop"], &env, TIMEOUT).status,
        Some(0)
    );
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
