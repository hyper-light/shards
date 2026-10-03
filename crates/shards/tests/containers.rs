//! Containers, end to end: what `shards run` and `run -d` leave behind, and `shards ps`,
//! `wait`, `logs`, `rm`, `stop` and `kill` (crates/shards/src/daemon/commands.rs), as
//! `docker` has them. Real VMs, booted from SHARDS_KERNEL and SHARDS_INIT, so no snapshots
//! are needed; the image comes from a loopback registry (tests/common, `served`), and its
//! entrypoint is the test guest.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    Run, TempDir, cannot_run_vms, guest_init, kernel, registry, run_shards_env, served, sha256_digest,
    shards, shards_net, shards_vm, shardsd, test_index,
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// `shards run` of the test image, with `options` before the image and `command` after it.
fn run_args(image: &str, options: &[&str], command: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--pull", "never"].map(String::from).to_vec();
    args.extend(options.iter().map(|s| (*s).to_string()));
    args.push(image.to_string());
    args.extend(command.iter().map(|s| (*s).to_string()));
    args
}

fn run_in(home: &Path, image: &str, options: &[&str], command: &[&str]) -> Run {
    let args = run_args(image, options, command);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    shards_in(home, &args)
}

/// A run left going: its client, with the command's first line read.
fn start(home: &Path, image: &str, options: &[&str], command: &[&str]) -> Child {
    let mut child = Command::new(shards())
        .args(run_args(image, options, command))
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    child
}

/// Waits for a child, failing the test rather than hanging it.
fn exit(child: &mut Child) -> Option<i32> {
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

/// A home with the test image pulled, and the image's name.
fn home(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let pulled = shards_in(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    Some((home, image))
}

/// More published sockets than one message carries (shards_ipc::MAX_FDS, 8): five
/// ports on every address are ten listeners, each handed to the VM's network process, and
/// each reaching the guest.
#[test]
fn many_published_ports_all_reach_the_guest() {
    use std::io::{Read as _, Write as _};
    let Some((home, image)) = home("containers-publish-many") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &[
            "--name", "many", "-p", "7000", "-p", "7000", "-p", "7000", "-p", "7000", "-p", "7000",
        ],
        &["serve", "7000", "5"],
    );
    let listed = shards_in(&home, &["port", "many", "7000"]);
    assert_eq!(listed.stdout.lines().count(), 10, "{listed}");
    let ports: Vec<u16> = listed
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("0.0.0.0:"))
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(ports.len(), 5, "{listed}");
    for port in ports {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(TIMEOUT)).unwrap();
        c.write_all(b"hello").unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        assert_eq!(got, "from 172.17.0.1\nhello", "port {port}");
    }
    assert_eq!(exit(&mut run), Some(0));
}

#[test]
fn a_run_leaves_its_container_until_rm() {
    let Some((home, image)) = home("containers-rm") else {
        return;
    };
    let run = run_in(&home, &image, &["--name", "web"], &["exit", "3"]);
    assert_eq!(run.status, Some(3), "{}", run.stderr);
    let waited = shards_in(&home, &["wait", "web"]);
    assert_eq!(
        (waited.status, waited.stdout.as_str()),
        (Some(0), "3\n"),
        "{}",
        waited.stderr
    );
    let removed = shards_in(&home, &["rm", "web"]);
    assert_eq!(
        (removed.status, removed.stdout.as_str()),
        (Some(0), "web\n"),
        "{}",
        removed.stderr
    );
    let again = shards_in(&home, &["rm", "web"]);
    assert_eq!(again.status, Some(1));
    assert_eq!(
        again.stderr,
        "Error response from daemon: No such container: web\n"
    );
    // A name is free again once its container is gone.
    let reused = run_in(&home, &image, &["--name", "web"], &["exit", "0"]);
    assert_eq!(reused.status, Some(0), "{}", reused.stderr);
}

#[test]
fn names_are_unique_and_rm_leaves_nothing() {
    let Some((home, image)) = home("containers-names") else {
        return;
    };
    assert_eq!(
        run_in(&home, &image, &["--name", "one"], &["exit", "0"]).status,
        Some(0)
    );
    let taken = run_in(&home, &image, &["--name", "one"], &["exit", "0"]);
    assert_eq!(taken.status, Some(125));
    assert!(
        taken
            .stderr
            .contains("Conflict. The container name \"/one\" is already in use"),
        "{}",
        taken.stderr
    );
    let invalid = run_in(&home, &image, &["--name", "-x"], &["exit", "0"]);
    assert_eq!(invalid.status, Some(125), "{}", invalid.stderr);
    let removed = run_in(&home, &image, &["--rm", "--name", "gone"], &["exit", "0"]);
    assert_eq!(removed.status, Some(0), "{}", removed.stderr);
    let waited = shards_in(&home, &["wait", "gone"]);
    assert_eq!(
        waited.status,
        Some(1),
        "--rm left its container: {}",
        waited.stdout
    );
}

#[test]
fn stop_and_kill_signal_the_command() {
    let Some((home, image)) = home("containers-stop") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "sleeper"], &["sleep"]);
    let stopped = shards_in(&home, &["stop", "-t", "5", "sleeper"]);
    assert_eq!(
        (stopped.status, stopped.stdout.as_str()),
        (Some(0), "sleeper\n"),
        "{}",
        stopped.stderr
    );
    assert_eq!(exit(&mut sleeper), Some(128 + 15));
    let waited = shards_in(&home, &["wait", "sleeper"]);
    assert_eq!(waited.stdout, "143\n");

    let mut trapper = start(&home, &image, &["--name", "trapper"], &["trap", "USR1"]);
    let killed = shards_in(&home, &["kill", "-s", "USR1", "trapper"]);
    assert_eq!(
        (killed.status, killed.stdout.as_str()),
        (Some(0), "trapper\n"),
        "{}",
        killed.stderr
    );
    assert_eq!(exit(&mut trapper), Some(0));
    let not_running = shards_in(&home, &["kill", "trapper"]);
    assert_eq!(not_running.status, Some(1));
    assert!(
        not_running.stderr.contains("is not running"),
        "{}",
        not_running.stderr
    );
}

#[test]
fn rm_refuses_a_running_container_unless_forced() {
    let Some((home, image)) = home("containers-force") else {
        return;
    };
    let mut sleeper = start(&home, &image, &["--name", "busy"], &["sleep"]);
    let refused = shards_in(&home, &["rm", "busy"]);
    assert_eq!(refused.status, Some(1));
    assert!(
        refused.stderr.contains("container is running"),
        "{}",
        refused.stderr
    );
    let forced = shards_in(&home, &["rm", "-f", "busy"]);
    assert_eq!(
        (forced.status, forced.stdout.as_str()),
        (Some(0), "busy\n"),
        "{}",
        forced.stderr
    );
    assert_eq!(exit(&mut sleeper), Some(128 + 9));
    assert_eq!(shards_in(&home, &["wait", "busy"]).status, Some(1));
}

#[test]
fn ps_lists_containers_as_docker_ps_does() {
    let Some((home, image)) = home("containers-ps") else {
        return;
    };
    assert_eq!(
        run_in(&home, &image, &["--name", "done"], &["exit", "0"]).status,
        Some(0)
    );
    let mut sleeper = start(&home, &image, &["--name", "up"], &["sleep"]);
    let header = "CONTAINER ID   IMAGE";
    let running = shards_in(&home, &["ps"]);
    assert_eq!(running.status, Some(0), "{}", running.stderr);
    let lines: Vec<&str> = running.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", running.stdout);
    assert!(lines[0].starts_with(header), "{}", running.stdout);
    assert!(lines[0].ends_with("PORTS     NAMES"), "{}", running.stdout);
    assert!(
        lines[1].contains("\"/bin/testguest sleep\""),
        "{}",
        running.stdout
    );
    assert!(lines[1].contains(" Up "), "{}", running.stdout);
    assert!(lines[1].ends_with(" up"), "{}", running.stdout);
    // Every column starts where its header does.
    for name in ["IMAGE", "COMMAND", "CREATED", "STATUS"] {
        let at = lines[0].find(name).unwrap();
        assert_eq!(lines[1].as_bytes()[at - 1], b' ', "{name}:\n{}", running.stdout);
        assert_ne!(lines[1].as_bytes()[at], b' ', "{name}:\n{}", running.stdout);
    }
    let all = shards_in(&home, &["ps", "-a"]);
    let lines: Vec<&str> = all.stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{}", all.stdout);
    assert!(
        lines[1].ends_with(" up") && lines[2].ends_with(" done"),
        "newest first:\n{}",
        all.stdout
    );
    assert!(lines[2].contains(" Exited (0) "), "{}", all.stdout);
    assert!(lines[2].contains(" ago "), "{}", all.stdout);
    let quiet = shards_in(&home, &["ps", "-aq"]);
    let ids: Vec<&str> = quiet.stdout.lines().collect();
    assert_eq!(ids.len(), 2, "{}", quiet.stdout);
    assert!(
        ids.iter()
            .all(|id| id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    assert!(lines[1].starts_with(ids[0]), "{}\n{}", quiet.stdout, all.stdout);
    let stopped = shards_in(&home, &["stop", "-t", "1", "up"]);
    assert_eq!(stopped.status, Some(0), "{}", stopped.stderr);
    assert_eq!(exit(&mut sleeper), Some(128 + 15));
}

#[test]
fn logs_keep_what_a_container_wrote() {
    let Some((home, image)) = home("containers-logs") else {
        return;
    };
    let reported = run_in(&home, &image, &["--name", "reporter"], &["report"]);
    assert_eq!(reported.status, Some(0), "{}", reported.stderr);
    let logs = shards_in(&home, &["logs", "reporter"]);
    assert_eq!(logs.status, Some(0), "{}", logs.stderr);
    assert_eq!(logs.stdout, reported.stdout);
    let last = shards_in(&home, &["logs", "--tail", "1", "reporter"]);
    assert_eq!(last.stdout.lines().count(), 1, "{}", last.stdout);
    assert!(reported.stdout.ends_with(&last.stdout), "{}", last.stdout);
    let stamped = shards_in(&home, &["logs", "-t", "reporter"]);
    for line in stamped.stdout.lines() {
        let (at, rest) = line.split_once(' ').unwrap();
        assert_eq!(at.len(), "2026-09-29T09:12:34.000000005Z".len(), "{line}");
        assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T', "{line}");
        assert!(reported.stdout.lines().any(|l| l == rest), "{line}");
    }

    // --since and --until, read as the Docker client reads them (gotime.rs).
    let recent = shards_in(&home, &["logs", "--since", "1h", "reporter"]);
    assert_eq!(recent.stdout, reported.stdout, "{recent}");
    for window in [
        &["--since", "2999-01-01T00:00:00Z"][..],
        &["--until", "2000-01-01"],
        &["--since", "-1h"],
    ] {
        let args: Vec<&str> = ["logs"]
            .iter()
            .chain(window)
            .chain(&["reporter"])
            .copied()
            .collect();
        let none = shards_in(&home, &args);
        assert_eq!(
            (none.status, none.stdout.as_str()),
            (Some(0), ""),
            "{window:?}: {none}"
        );
    }
    for (window, said) in [
        (
            "x",
            "invalid value for \"since\": failed to parse value as time or duration: \"x\"\n",
        ),
        (
            "2013-13-01",
            "invalid value for \"since\": parsing time \"2013-13-01\": month out of range\n",
        ),
        (
            "1.+5",
            "Error response from daemon: invalid value for \"since\": invalid timestamp \"1.+5\": invalid nanoseconds: invalid character '+' at position 0\n",
        ),
    ] {
        let bad = shards_in(&home, &["logs", "--since", window, "reporter"]);
        assert_eq!((bad.status, bad.stderr.as_str()), (Some(1), said), "{window}");
    }
    // The container is found first, as the CLI inspects it first.
    let missing = shards_in(&home, &["logs", "--since", "x", "nosuch"]);
    assert_eq!(
        missing.stderr,
        "Error response from daemon: No such container: nosuch\n"
    );

    let errs = run_in(&home, &image, &["--name", "errs"], &["stderr", "to stderr"]);
    assert_eq!(errs.status, Some(0), "{}", errs.stderr);
    let logs = shards_in(&home, &["logs", "errs"]);
    assert_eq!((logs.stdout.as_str(), logs.stderr.as_str()), ("", "to stderr"));

    let mut sleeper = start(&home, &image, &["--name", "follow"], &["sleep"]);
    let follower = Command::new(shards())
        .args(["logs", "-f", "follow"])
        .env("SHARDS_HOME", &*home)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(shards_in(&home, &["stop", "follow"]).status, Some(0));
    assert_eq!(exit(&mut sleeper), Some(128 + 15));
    let followed = follower.wait_with_output().unwrap();
    assert_eq!(followed.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&followed.stdout), "ready\n");
}

/// `text` without the timing lines SHARDS_TIMING adds.
fn untimed(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("shards-timing "))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// Checks `check` until it holds, failing the test after TIMEOUT.
fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn detached_runs_print_their_id_and_go_on() {
    let Some((home, image)) = home("containers-detached") else {
        return;
    };
    let run = run_in(&home, &image, &["-d", "--name", "bg"], &["sleep"]);
    assert_eq!(run.status, Some(0), "{run}");
    let id = run.stdout.trim_end().to_string();
    assert_eq!(run.stdout, format!("{id}\n"), "the ID alone, on stdout");
    assert!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "{id}"
    );
    assert_eq!(untimed(&run.stderr), "", "{run}");
    // It runs on without its client: ps lists it, and its output is kept.
    let listed = shards_in(&home, &["ps", "-q", "--no-trunc"]);
    assert_eq!(listed.stdout, format!("{id}\n"), "{listed}");
    eventually("the detached run's output never reached its log", || {
        shards_in(&home, &["logs", "bg"]).stdout == "ready\n"
    });
    // Its host is named for its ID, as Docker names a container's.
    let reported = run_in(&home, &image, &["-d", "--name", "who"], &["report"]);
    assert_eq!(reported.status, Some(0), "{reported}");
    let short = reported.stdout.get(..12).unwrap();
    assert_eq!(shards_in(&home, &["wait", "who"]).stdout, "0\n");
    let report = shards_in(&home, &["logs", "who"]).stdout;
    assert!(report.contains(&format!("hostname {short}\n")), "{report}");
    assert!(report.contains(&format!("env HOSTNAME={short}\n")), "{report}");
    // Stopped, it exits as a SIGTERM ends it, and stays until removed.
    let stopped = shards_in(&home, &["stop", "-t", "5", "bg"]);
    assert_eq!(
        (stopped.status, stopped.stdout.as_str()),
        (Some(0), "bg\n"),
        "{stopped}"
    );
    assert_eq!(shards_in(&home, &["wait", id.as_str()]).stdout, "143\n");
    let all = shards_in(&home, &["ps", "-a"]);
    let bg = all
        .stdout
        .lines()
        .find(|l| l.ends_with(" bg"))
        .unwrap_or_default();
    assert!(bg.contains(" Exited (143) "), "{all}");
    // With --rm it goes once it ends.
    let gone = run_in(&home, &image, &["-d", "--rm", "--name", "gone"], &["exit", "0"]);
    assert_eq!(gone.status, Some(0), "{gone}");
    eventually("a --rm container stayed", || {
        shards_in(&home, &["wait", "gone"]).status == Some(1)
    });
}

#[test]
fn a_command_that_cannot_start_says_why_as_docker_run_does() {
    let Some((home, image)) = home("containers-unstarted") else {
        return;
    };
    let said = "shards: Error response from daemon: exec: \"/nonexistent\": stat /nonexistent: no such file or directory\n\nRun 'shards run --help' for more information\n";
    let attached = run_in(
        &home,
        &image,
        &["--name", "here", "--entrypoint", "/nonexistent"],
        &[],
    );
    assert_eq!(attached.status, Some(127), "{attached}");
    assert_eq!(untimed(&attached.stderr), said, "{attached}");
    let detached = run_in(
        &home,
        &image,
        &["-d", "--name", "there", "--entrypoint", "/nonexistent"],
        &[],
    );
    assert_eq!(detached.status, Some(127), "{detached}");
    let id = detached.stdout.trim_end();
    assert_eq!(id.len(), 64, "the ID first, as the container exists: {detached}");
    assert_eq!(untimed(&detached.stderr), said, "{detached}");
    // Both stay created, keeping the code dockerd gives a command not found, and no log.
    for name in ["here", "there"] {
        assert_eq!(
            shards_in(&home, &["wait", name]).stdout,
            "127\n",
            "{name}\n--- daemon.log\n{}",
            std::fs::read_to_string(home.join("daemon.log")).unwrap_or_default()
        );
        let logs = shards_in(&home, &["logs", name]);
        assert_eq!((logs.stdout.as_str(), logs.stderr.as_str()), ("", ""), "{name}");
    }
    let all = shards_in(&home, &["ps", "-a"]);
    for line in all.stdout.lines().skip(1) {
        assert!(line.contains(" Created "), "{all}");
    }
    // With --rm, one that cannot start is gone by the time the client exits.
    let gone = run_in(
        &home,
        &image,
        &["-d", "--rm", "--name", "gone", "--entrypoint", "/nonexistent"],
        &[],
    );
    assert_eq!(gone.status, Some(127), "{gone}");
    assert_eq!(shards_in(&home, &["wait", "gone"]).status, Some(1));
}

/// `shards ARGS` in `home`, where no run is meant to boot.
fn without_vms(home: &Path, args: &[&str]) -> Run {
    run_shards_env(&[], args, &[("SHARDS_HOME", home.as_os_str())], TIMEOUT)
}

#[test]
fn usage_mistakes_are_answered_without_a_daemon() {
    let home = TempDir::new("containers-usage");
    for (args, status, stderr) in [
        (
            &["stop"][..],
            1,
            "shards: 'shards stop' requires at least 1 argument\n\nUsage:  shards stop [OPTIONS] CONTAINER [CONTAINER...]\n\nSee 'shards stop --help' for more information\n",
        ),
        (
            &["ps", "--nope"],
            125,
            "unknown flag: --nope\n\nUsage:  shards ps [OPTIONS]\n\nRun 'shards ps --help' for more information\n",
        ),
        (
            &["run", "-p", "x", "alpine"],
            125,
            "shards: invalid containerPort: x\n\nRun 'shards run --help' for more information\n",
        ),
        (
            &["run", "--pull", "sometimes", "alpine"],
            125,
            "shards: invalid pull option: 'sometimes': must be one of \"always\", \"missing\" or \"never\"\n\nRun 'shards run --help' for more information\n",
        ),
    ] {
        let r = without_vms(&home, args);
        assert_eq!((r.status, r.stderr.as_str()), (Some(status), stderr), "{args:?}");
    }
    let help = without_vms(&home, &["kill", "--help"]);
    assert_eq!(help.status, Some(0));
    assert!(
        help.stdout
            .starts_with("Usage:  shards kill [OPTIONS] CONTAINER [CONTAINER...]\n"),
        "{help}"
    );
    assert!(
        !home.join("daemon.sock").exists() && !home.join("daemon.pid").exists(),
        "no daemon was started"
    );
}

/// This build's `shards` and `shardsd` in a directory of their own, beside a `shards-vm`
/// that waits for a gate to open before it becomes this build's. Every VM their daemon
/// starts waits there, so a run stays pending, its container created, for as long as the
/// test keeps the gate shut (audit A06).
struct Gated {
    dir: TempDir,
}

impl Gated {
    fn new(name: &str) -> Gated {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new(name);
        for (from, to) in [
            (shards(), "shards"),
            (shardsd(), "shardsd"),
            (shards_net(), "shards-net"),
        ] {
            // A link where there can be one: macOS assesses a copy as a new binary.
            if std::fs::hard_link(from, dir.join(to)).is_err() {
                std::fs::copy(from, dir.join(to)).unwrap();
            }
        }
        let vm = dir.join("shards-vm");
        // It gives up once the test's directory has gone.
        let script = format!(
            "#!/bin/sh\nwhile [ ! -e '{open}' ]; do [ -d '{dir}' ] || exit 1; sleep 0.01; done\nexec '{vm}' \"$@\"\n",
            open = dir.join("open").display(),
            dir = dir.display(),
            vm = shards_vm().display()
        );
        std::fs::write(&vm, script).unwrap();
        std::fs::set_permissions(&vm, std::fs::Permissions::from_mode(0o755)).unwrap();
        Gated { dir }
    }

    fn open(&self) {
        std::fs::write(self.dir.join("open"), b"").unwrap();
    }

    /// `shards ARGS` from this directory in `home`, whose runs boot the test kernel and
    /// init, left going.
    fn start(&self, home: &Path, args: &[&str]) -> Going {
        let mut child = Command::new(self.dir.join("shards"))
            .args(args)
            .env("SHARDS_HOME", home)
            .env("SHARDS_KERNEL", kernel())
            .env("SHARDS_INIT", guest_init())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let collect = |mut from: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = from.read_to_string(&mut text);
                text
            })
        };
        let out = collect(Box::new(child.stdout.take().unwrap()));
        let err = collect(Box::new(child.stderr.take().unwrap()));
        Going { child, out, err }
    }

    /// `shards ARGS` from this directory in `home`.
    fn shards(&self, home: &Path, args: &[&str]) -> Run {
        self.start(home, args).finish()
    }
}

/// A command left going, its output collected as it comes.
struct Going {
    child: Child,
    out: std::thread::JoinHandle<String>,
    err: std::thread::JoinHandle<String>,
}

impl Going {
    fn finish(mut self) -> Run {
        let start = Instant::now();
        let status = exit(&mut self.child);
        Run {
            status,
            stdout: self.out.join().unwrap(),
            stderr: self.err.join().unwrap(),
            elapsed: start.elapsed(),
        }
    }
}

/// The ID of the one container in `home`, once it is there: created, its run pending.
fn created(gated: &Gated, home: &Path) -> String {
    let id = std::cell::RefCell::new(String::new());
    eventually("the run's container never showed", || {
        *id.borrow_mut() = gated.shards(home, &["ps", "-a", "-q", "--no-trunc"]).stdout;
        !id.borrow().is_empty()
    });
    let all = gated.shards(home, &["ps", "-a"]).stdout;
    assert!(
        all.lines().nth(1).is_some_and(|l| l.contains(" Created ")),
        "{all}"
    );
    id.into_inner().trim_end().to_string()
}

/// `rm` of a container whose run has no VM yet removes it, and the run never starts: its
/// client hears that the container is gone, as `docker run` hears it of a container
/// removed before it was started, and nothing is left (audit A06, whose reproduction
/// this is: before, the command ran after its container was removed).
#[test]
fn rm_cancels_a_run_whose_vm_is_not_ready() {
    let Some((home, image)) = home("containers-cancel") else {
        return;
    };
    let gated = Gated::new("cancel-bin");
    let args = run_args(&image, &["--name", "racer"], &["exit", "7"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = gated.start(&home, &args);
    let id = created(&gated, &home);
    let waiting = gated.start(&home, &["wait", "racer"]);
    let removed = gated.shards(&home, &["rm", "racer"]);
    assert_eq!(
        (removed.status, removed.stdout.as_str()),
        (Some(0), "racer\n"),
        "{removed}"
    );
    // `wait` asked before the removal, and hears the 0 of a container that never ran, or
    // after it, and hears there is no such container; either way while every VM waits.
    let waited = waiting.finish();
    assert!(
        waited.status == Some(0) && waited.stdout == "0\n"
            || waited.status == Some(1)
                && waited.stderr == "Error response from daemon: No such container: racer\n",
        "{waited}"
    );
    gated.open();
    let run = run.finish();
    assert_eq!(run.status, Some(125), "{run}");
    assert_eq!(
        untimed(&run.stderr),
        format!(
            "shards: Error response from daemon: No such container: {id}\n\nRun 'shards run --help' for more information\n"
        ),
        "{run}"
    );
    assert_eq!(run.stdout, "", "{run}");
    assert_eq!(gated.shards(&home, &["ps", "-a", "-q"]).stdout, "");
    assert_eq!(gated.shards(&home, &["daemon", "stop"]).status, Some(0));
}

/// A container seen before its daemon died outlives it: the next daemon shows it as one
/// whose run did not start, with 255 for the status nobody saw, and `rm` removes it
/// (audit A15, whose finding this is: before, a container existed only in its daemon's
/// memory until its run started, and went with the daemon).
#[test]
fn a_container_outlives_a_daemon_that_dies() {
    let Some((home, image)) = home("containers-crash") else {
        return;
    };
    let gated = Gated::new("crash-bin");
    let args = run_args(&image, &["--name", "racer"], &["exit", "7"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = gated.start(&home, &args);
    let id = created(&gated, &home);
    let pid: i32 = std::fs::read_to_string(home.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: kill(2) of the daemon this test's run started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let run = run.finish();
    assert_ne!(run.status, Some(0), "{run}");
    gated.open();
    let listed = gated.shards(&home, &["ps", "-a", "--no-trunc"]);
    let line = listed.stdout.lines().nth(1).unwrap_or_default().to_string();
    assert!(
        line.starts_with(&id) && line.contains(" Created ") && line.ends_with(" racer"),
        "{listed}"
    );
    assert_eq!(listed.stdout.lines().count(), 2, "{listed}");
    let waited = gated.shards(&home, &["wait", "racer"]);
    assert_eq!(
        (waited.status, waited.stdout.as_str()),
        (Some(0), "255\n"),
        "{waited}"
    );
    let removed = gated.shards(&home, &["rm", "racer"]);
    assert_eq!(
        (removed.status, removed.stdout.as_str()),
        (Some(0), "racer\n"),
        "{removed}"
    );
    assert_eq!(gated.shards(&home, &["ps", "-a", "-q"]).stdout, "");
    assert_eq!(gated.shards(&home, &["daemon", "stop"]).status, Some(0));
}

/// A daemon told to stop starts no run still pending, and waits for no VM to boot for
/// one (audit A07): the run is refused at once, the daemon ends, and the run's container
/// stays, created, with the code of a start that failed.
#[test]
fn a_stopping_daemon_starts_no_pending_run() {
    let Some((home, image)) = home("containers-stop-pending") else {
        return;
    };
    let gated = Gated::new("stop-pending-bin");
    let args = run_args(&image, &["--name", "racer"], &["exit", "7"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = gated.start(&home, &args);
    let id = created(&gated, &home);
    // Every VM still waits at the gate.
    let stopping = gated.start(&home, &["daemon", "stop"]);
    let run = run.finish();
    assert_eq!(run.status, Some(125), "{run}");
    assert_eq!(
        untimed(&run.stderr),
        "shards: Error response from daemon: the daemon is shutting down\n\nRun 'shards run --help' for more information\n",
        "{run}"
    );
    let stopped = stopping.finish();
    assert_eq!(stopped.status, Some(0), "{stopped}");
    gated.open();
    // The next daemon finds the container as the last one left it.
    let listed = gated.shards(&home, &["ps", "-a", "-q", "--no-trunc"]);
    assert_eq!(listed.stdout, format!("{id}\n"), "{listed}");
    assert_eq!(gated.shards(&home, &["wait", "racer"]).stdout, "128\n");
    assert_eq!(gated.shards(&home, &["rm", "racer"]).status, Some(0));
    assert_eq!(gated.shards(&home, &["daemon", "stop"]).status, Some(0));
}

/// `shards ARGS` in `home` whose daemon keeps logs in three segments of 100,000 bytes,
/// its output as bytes.
fn retained(home: &Path) -> Command {
    let mut command = Command::new(shards());
    command
        .env("SHARDS_HOME", home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .env("SHARDS_LOG_MAX_SIZE", "100000")
        .env("SHARDS_LOG_MAX_FILE", "3");
    command
}

fn retained_output(home: &Path, args: &[&str]) -> Vec<u8> {
    let out = retained(home).args(args).stdin(Stdio::null()).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A container's log keeps only its newest output: at most SHARDS_LOG_MAX_FILE segments of
/// SHARDS_LOG_MAX_SIZE bytes, as Docker's `local` driver keeps `max-file` of `max-size`.
/// `logs` shows what is kept, `--tail` its last lines, and a follower gets every byte once
/// as segments come and go under it (audit A12).
#[test]
fn a_log_keeps_its_newest_output_within_its_retention() {
    let Some((home, image)) = home("containers-log-retention") else {
        return;
    };
    // The daemon the next command starts has the retention.
    assert_eq!(without_vms(&home, &["daemon", "stop"]).status, Some(0));
    let wrote = retained_output(
        &home,
        &[
            "run", "--name", "big", "--pull", "never", &image, "bulk", "1000000", "7",
        ],
    );
    assert_eq!(wrote.len(), 1_000_000);
    let kept = retained_output(&home, &["logs", "big"]);
    assert!(
        wrote.ends_with(&kept),
        "{} bytes kept, not the newest",
        kept.len()
    );

    // On disk: three segments, the newest, each within its size, holding what `logs` shows.
    let dir = std::fs::read_dir(home.join("containers"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|d| {
            std::fs::read_to_string(d.join("config.json")).is_ok_and(|c| c.contains("\"name\":\"big\""))
        })
        .unwrap();
    let mut seqs: Vec<u64> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            let seq = name.strip_prefix("log.")?.strip_suffix(".idx")?;
            seq.parse().ok()
        })
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs.len(), 3, "{seqs:?}");
    assert!(
        seqs[0] >= 1 && seqs[1] == seqs[0] + 1 && seqs[2] == seqs[1] + 1,
        "{seqs:?}"
    );
    assert!(!dir.join("log").exists() && !dir.join("log.idx").exists());
    let mut payload = 0u64;
    for seq in &seqs {
        let log = std::fs::metadata(dir.join(format!("log.{seq}"))).unwrap().len();
        let records = std::fs::metadata(dir.join(format!("log.{seq}.idx")))
            .unwrap()
            .len()
            / 8;
        assert!(
            log <= 100_000 || records == 1,
            "segment {seq}: {log} bytes in {records} records"
        );
        payload += log - 13 * records;
    }
    assert_eq!(payload, kept.len() as u64);

    let last = retained_output(&home, &["logs", "--tail", "1", "big"]);
    assert_eq!(last, kept.split_inclusive(|&b| b == b'\n').next_back().unwrap());

    // A follower of a container writing lines.
    let mut run = retained(&home)
        .args(["run", "-i", "--name", "fol", "--pull", "never", &image, "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Only a running container's log is followed, as dockerd follows only a running
    // container's (moby daemon/logs.go, `cLogCreated`).
    eventually("the container to run", || {
        retained_output(&home, &["ps", "-q", "--no-trunc"]).len() == 65
    });
    let mut follower = retained(&home)
        .args(["logs", "-f", "fol"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut out = follower.stdout.take().unwrap();
    let reading = {
        let received = received.clone();
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut all = Vec::new();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let n = out.read(&mut buf).unwrap();
                if n == 0 {
                    return all;
                }
                all.extend_from_slice(&buf[..n]);
                received.store(all.len(), std::sync::atomic::Ordering::Release);
            }
        })
    };
    let mut sent = Vec::new();
    {
        use std::io::Write as _;
        let mut stdin = run.stdin.take().unwrap();
        for chunk in 0..40 {
            let lines: String = (0..1000)
                .map(|line| format!("chunk {chunk} line {line}\n"))
                .collect();
            stdin.write_all(lines.as_bytes()).unwrap();
            sent.extend_from_slice(lines.as_bytes());
            let deadline = Instant::now() + TIMEOUT;
            // Each chunk is followed before the next is written: only a follower woken by
            // appends, not only by segments' coming, gets there.
            while received.load(std::sync::atomic::Ordering::Acquire) < sent.len() {
                assert!(
                    Instant::now() < deadline,
                    "the follower has {} bytes of {} at chunk {chunk}",
                    received.load(std::sync::atomic::Ordering::Acquire),
                    sent.len()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    assert_eq!(exit(&mut run), Some(0));
    let followed = reading.join().unwrap();
    assert_eq!(follower.wait().unwrap().code(), Some(0));
    assert!(sent.len() > 3 * 100_000, "the log rotated past its retention");
    assert!(
        followed == sent,
        "{} bytes followed of {}",
        followed.len(),
        sent.len()
    );
    assert_eq!(without_vms(&home, &["daemon", "stop"]).status, Some(0));
}

/// A container's log line past the largest message the daemon may send reaches the
/// terminal whole, byte for byte, through the real client, with its timestamp: before, it
/// was dropped and `logs` still said 0 (audit A08). The container is one a daemon left in
/// the home; no VM runs.
#[test]
fn logs_carry_lines_longer_than_a_message() {
    let home = TempDir::new("containers-long-line");
    let id = "c0ffee00".repeat(8);
    let dir = home.join("containers").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let record = format!(
        "{{\"id\":\"{id}\",\"name\":\"long\",\"image\":\"test\",\"command\":[\"x\"],\"created\":1,\"state\":\"exited\",\"started\":2,\"finished\":3,\"exit_code\":0,\"auto_remove\":false}}"
    );
    std::fs::write(dir.join("config.json"), record).unwrap();
    // 3 MiB and 5 bytes, then a newline, in 16 KiB records, as a workload writes them.
    let mut line: Vec<u8> = (0..(3 << 20) + 5).map(|i| b'a' + (i % 26) as u8).collect();
    line.push(b'\n');
    let at: u64 = 1_700_000_000_123_456_789;
    let mut log = Vec::new();
    for (i, piece) in line.chunks(16 << 10).enumerate() {
        log.push(1u8);
        log.extend((at + i as u64).to_be_bytes());
        log.extend(u32::try_from(piece.len()).unwrap().to_be_bytes());
        log.extend(piece);
    }
    std::fs::write(dir.join("log"), log).unwrap();
    let out = Command::new(shards())
        .args(["logs", "-t", "long"])
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
    let mut want = b"2023-11-14T22:13:20.123456789Z ".to_vec();
    want.extend(&line);
    assert!(
        out.stdout == want,
        "{} bytes, not {}",
        out.stdout.len(),
        want.len()
    );
    assert_eq!(without_vms(&home, &["daemon", "stop"]).status, Some(0));
}

/// A run is on Docker's default bridge, as `docker run` puts a container (D31), and
/// default deny holds on it (AGENTFILE_ARCH.md §3): a server on this host, at the host's
/// own address, refuses it, as nothing has granted it egress. Its own name is its address
/// in /etc/hosts, and its resolvers are the host's, none of them loopback ones.
#[test]
fn a_run_is_on_a_network_as_docker_runs_it() {
    let Some((home, image)) = home("containers-net") else {
        return;
    };
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(host) = host else {
        eprintln!("SKIP: this host has no route to give a guest an address of it");
        return;
    };
    let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let port = server.local_addr().unwrap().port();
    let to = format!("{}:{port}", host.ip());
    let ran = run_in(&home, &image, &["--rm", "-u", "root"], &["tcp", &to]);
    assert_eq!(ran.status, Some(1), "{ran}");
    assert!(ran.stdout.starts_with("tcp error Connection refused"), "{ran}");
    assert!(server.accept().is_err(), "the server was reached");
    let files = run_in(
        &home,
        &image,
        &["--rm", "-u", "root", "--hostname", "box"],
        &["stat", "/etc/hosts", "/etc/resolv.conf"],
    );
    assert_eq!(files.status, Some(0), "{files}");
    assert!(files.stdout.contains("172.17.0.2\tbox\\n"), "{files}");
    let resolv = files.stdout.split("/etc/resolv.conf").nth(1).unwrap_or_default();
    assert!(resolv.contains("nameserver "), "{files}");
    assert!(!resolv.contains("nameserver 127."), "{files}");
    // dockerd's bridge has no IPv6, and its transform drops IPv6 resolvers.
    let resolv = resolv.split("\n/").next().unwrap_or_default();
    assert!(
        !resolv
            .split("\\n")
            .any(|l| l.starts_with("nameserver") && l.contains(':')),
        "{files}"
    );
}

/// `--network none`: a loopback alone, as dockerd's `none` gives a container. The run
/// reaches nothing, and its own name is on the loopback, not a bridge's address; it still
/// has the host's resolvers, as dockerd's does.
#[test]
fn a_run_on_network_none_reaches_nothing() {
    let Some((home, image)) = home("containers-none") else {
        return;
    };
    let server = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let host = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    let Ok(host) = host else {
        eprintln!("SKIP: this host has no route to give a guest an address of it");
        return;
    };
    let to = format!("{}:{}", host.ip(), server.local_addr().unwrap().port());
    let ran = run_in(
        &home,
        &image,
        &["--rm", "-u", "root", "--network", "none"],
        &["tcp", &to],
    );
    assert_eq!(ran.status, Some(1), "{ran}");
    assert!(ran.stdout.starts_with("tcp error "), "{ran}");
    assert!(server.accept().is_err(), "a run on none reached the host");
    let files = run_in(
        &home,
        &image,
        &["--rm", "-u", "root", "--network=none", "--hostname", "box"],
        &["stat", "/etc/hosts", "/etc/resolv.conf"],
    );
    assert_eq!(files.status, Some(0), "{files}");
    assert!(files.stdout.contains("127.0.1.1\tbox\\n"), "{files}");
    assert!(!files.stdout.contains("172.17."), "{files}");
    assert!(files.stdout.contains("nameserver "), "{files}");
}

/// What dockerd says of networks a run cannot have, and when: a network that does not
/// exist fails the start, the container left created with 128 (`--rm` takes it); what
/// shards does not do yet, and what dockerd refuses outright, make no container.
#[test]
fn a_run_on_a_network_it_cannot_have_says_why_as_dockerd_does() {
    let Some((home, image)) = home("containers-badnet") else {
        return;
    };
    let help = "\n\nRun 'shards run --help' for more information\n";
    let missing = run_in(&home, &image, &["--name", "lost", "--network", "foo"], &["true"]);
    assert_eq!(
        (missing.status, missing.stderr.as_str()),
        (
            Some(125),
            format!("shards: Error response from daemon: failed to set up container networking: network foo not found{help}").as_str()
        ),
        "{missing}"
    );
    let all = shards_in(&home, &["ps", "-a"]);
    let rows: Vec<&str> = all.stdout.lines().skip(1).collect();
    assert!(
        matches!(rows.as_slice(), [row] if row.contains(" Created ") && row.ends_with(" lost")),
        "{all}"
    );
    let waited = shards_in(&home, &["wait", "lost"]);
    assert_eq!(waited.stdout, "128\n", "{waited}");
    let removed = run_in(
        &home,
        &image,
        &["--rm", "--network", "bridge", "--network", "none"],
        &["true"],
    );
    assert_eq!(removed.status, Some(125), "{removed}");
    assert!(
        removed
            .stderr
            .contains("one of the networks in private (none) mode"),
        "{removed}"
    );
    for (options, said) in [
        (
            &["--network", "host"][..],
            "shards: Error response from daemon: \"--network host\" is not supported by shards yet",
        ),
        (
            &["--network", "name=bridge,ip=172.17.0.9"][..],
            "shards: Error response from daemon: invalid config for network bridge: invalid endpoint settings:\nuser-specified IP address is supported on user-defined networks only",
        ),
        (
            &["--network", "container:x", "--hostname", "h"][..],
            "shards: Error response from daemon: conflicting options: hostname and the network mode",
        ),
        (&["--network", ""][..], "shards: no name set for network"),
    ] {
        let refused = run_in(&home, &image, options, &["true"]);
        assert_eq!(
            (refused.status, refused.stderr.as_str()),
            (Some(125), format!("{said}{help}").as_str()),
            "{refused}"
        );
    }
    let all = shards_in(&home, &["ps", "-aq"]);
    assert_eq!(all.stdout.lines().count(), 1, "{all}");
}

/// `stop` sends a container its own stop signal, `--stop-signal`'s, unless told one
/// (moby container.StopSignal): here SIGUSR1, which ends the command 128 + 10. One no
/// signal's name is refused as dockerd refuses it, before a container is made.
#[test]
fn stop_sends_a_containers_own_stop_signal() {
    let Some((home, image)) = home("containers-stopsignal") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &["--name", "usr1", "--stop-signal", "SIGUSR1"],
        &["sleep"],
    );
    let stopped = shards_in(&home, &["stop", "usr1"]);
    assert_eq!(
        (stopped.status, stopped.stdout.as_str()),
        (Some(0), "usr1\n"),
        "{stopped}"
    );
    assert_eq!(exit(&mut run), Some(138));
    assert_eq!(shards_in(&home, &["wait", "usr1"]).stdout, "138\n");
    // A signal the command ignores, and its own timeout: SIGKILL after a second, not ten.
    let mut run = start(
        &home,
        &image,
        &[
            "--name",
            "chld",
            "--stop-signal",
            "SIGCHLD",
            "--stop-timeout",
            "1",
        ],
        &["sleep"],
    );
    let began = Instant::now();
    let stopped = shards_in(&home, &["stop", "chld"]);
    let took = began.elapsed();
    assert_eq!(stopped.status, Some(0), "{stopped}");
    assert_eq!(exit(&mut run), Some(137));
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(5),
        "{took:?}"
    );
    let refused = run_in(&home, &image, &["--stop-signal", "BOGUS"], &["exit", "0"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (
            Some(125),
            "shards: Error response from daemon: invalid signal: BOGUS\n\nRun 'shards run --help' for more information\n"
        ),
        "{refused}"
    );
    assert_eq!(shards_in(&home, &["ps", "-aq"]).stdout.lines().count(), 2);
}

/// `exec` runs a command in a running container as `docker exec` does: from the run's
/// environment, user and working directory, which `-e`, `-u` and `-w` change; on its own
/// stdio, `-i` its stdin, `-t` a terminal of its own; with its own status; refused in
/// dockerd's words for what the daemon refuses, and in runc's for what cannot start.
#[test]
fn exec_runs_commands_in_a_running_container_as_docker_exec_does() {
    use std::io::Write as _;
    let Some((home, image)) = home("containers-exec") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &[
            "--name",
            "ex",
            "--hostname",
            "box",
            "-e",
            "FROMRUN=1",
            "-w",
            "/tmp",
            "-u",
            "1000",
        ],
        &["sleep"],
    );
    let exec = |args: &[&str]| shards_in(&home, &[&["exec"][..], args].concat());
    let report = exec(&["ex", "/bin/testguest", "report"]);
    assert_eq!(report.status, Some(0), "{report}");
    for line in [
        "uid 1000\n",
        "cwd /tmp\n",
        "hostname box\n",
        "env FROMRUN=1\n",
        "env HOSTNAME=box\n",
    ] {
        assert!(report.stdout.contains(line), "{line:?}\n{report}");
    }
    let changed = exec(&[
        "-e",
        "X=2",
        "-u",
        "0",
        "-w",
        "/",
        "ex",
        "/bin/testguest",
        "report",
    ]);
    for line in ["uid 0\n", "cwd /\n", "env X=2\n", "env FROMRUN=1\n"] {
        assert!(changed.stdout.contains(line), "{line:?}\n{changed}");
    }
    // Laid over the container's environment whole, as dockerd lays it: a variable of its
    // name replaced, and one given without a value (none here to take it from) unset.
    let over = exec(&["-e", "FROMRUN=2", "ex", "/bin/testguest", "report"]);
    assert_eq!(over.status, Some(0), "{over}");
    assert!(over.stdout.contains("env FROMRUN=2\n"), "{over}");
    assert!(!over.stdout.contains("env FROMRUN=1\n"), "{over}");
    assert!(std::env::var_os("FROMRUN").is_none());
    let unset = exec(&["-e", "FROMRUN", "ex", "/bin/testguest", "report"]);
    assert_eq!(unset.status, Some(0), "{unset}");
    assert!(!unset.stdout.contains("env FROMRUN"), "{unset}");
    assert_eq!(exec(&["ex", "/bin/testguest", "exit", "7"]).status, Some(7));
    let split = exec(&["ex", "/bin/testguest", "stderr", "to stderr"]);
    assert_eq!(
        (split.stdout.as_str(), split.stderr.as_str()),
        ("", "to stderr"),
        "{split}"
    );
    // -i: its stdin is this client's.
    let mut cat = Command::new(shards())
        .args(["exec", "-i", "ex", "/bin/testguest", "cat"])
        .env("SHARDS_HOME", &*home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    cat.stdin.take().unwrap().write_all(b"through exec\n").unwrap();
    let out = cat.wait_with_output().unwrap();
    assert_eq!(
        (out.status.code(), out.stdout.as_slice()),
        (Some(0), &b"through exec\n"[..])
    );
    // -t: a terminal of its own, and TERM for it.
    let tty = exec(&["-t", "ex", "/bin/testguest", "tty"]);
    for line in [
        "stdin tty true",
        "stdout tty true",
        "controlling true",
        "term xterm",
    ] {
        assert!(tty.stdout.contains(line), "{line:?}\n{tty}");
    }
    // -d: answered once it starts.
    assert_eq!(exec(&["-d", "ex", "/bin/testguest", "sleep"]).status, Some(0));
    let refused = |args: &[&str], status: i32, said: &str| {
        let r = exec(args);
        assert_eq!(
            (r.status, r.stderr.as_str()),
            (Some(status), said),
            "{args:?}\n{r}"
        );
    };
    let oci = "OCI runtime exec failed: exec failed:";
    refused(
        &["ex", "nonexistent"],
        127,
        &format!(
            "{oci} unable to start container process: exec: \"nonexistent\": executable file not found in $PATH\n"
        ),
    );
    refused(
        &["-w", "/nonexistent", "ex", "/bin/testguest", "report"],
        127,
        &format!(
            "{oci} unable to start container process: chdir to cwd (\"/nonexistent\") failed: no such file or directory\n"
        ),
    );
    refused(
        &["-w", "rel", "ex", "/bin/testguest", "report"],
        128,
        &format!("{oci} Cwd must be an absolute path\n"),
    );
    refused(
        &["nope", "/bin/testguest", "report"],
        1,
        "Error response from daemon: No such container: nope\n",
    );
    let unknown = exec(&["-u", "nobodyhere", "ex", "/bin/testguest", "report"]);
    assert_eq!(unknown.status, Some(1), "{unknown}");
    assert!(
        unknown
            .stderr
            .starts_with("Error response from daemon: unable to find user nobodyhere"),
        "{unknown}"
    );
    let stopped = shards_in(&home, &["stop", "ex"]);
    assert_eq!(stopped.status, Some(0), "{stopped}");
    let _ = exit(&mut run);
    let id = shards_in(&home, &["ps", "-aq", "--no-trunc"])
        .stdout
        .trim()
        .to_string();
    refused(
        &["ex", "/bin/testguest", "report"],
        1,
        &format!("Error response from daemon: container {id} is not running\n"),
    );
}

/// `-p`: a container's ports published on the host as dockerd publishes them. Each
/// connection reaches the guest from the bridge's gateway, as through dockerd's userland
/// proxy, and carries what either side sends whole; `ps` and `port` list the bindings as
/// docker's do; a host port taken is refused in dockerd's words, a container's as its
/// allocator's, another program's as its bind's; and a run's ports are free again as soon
/// as it has ended.
#[test]
fn published_ports_reach_the_guest_as_dockerd_publishes_them() {
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpStream};
    let Some((home, image)) = home("containers-publish") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &["--name", "web", "-p", "127.0.0.1::7001", "-p", "7000"],
        &["serve", "7000", "12"],
    );
    let shards = |args: &[&str]| shards_in(&home, args);
    let listed = shards(&["port", "web"]);
    assert_eq!(listed.status, Some(0), "{listed}");
    let host_port = |line: Option<&str>| -> u16 {
        line.and_then(|l| l.rsplit(':').next())
            .and_then(|p| p.parse().ok())
            .unwrap()
    };
    let lines: Vec<&str> = listed.stdout.lines().collect();
    let (n, m) = (
        host_port(lines.first().copied()),
        host_port(lines.get(2).copied()),
    );
    assert_eq!(
        listed.stdout,
        format!("7000/tcp -> 0.0.0.0:{n}\n7000/tcp -> [::]:{n}\n7001/tcp -> 127.0.0.1:{m}\n"),
    );
    let one = shards(&["port", "web", "7000"]);
    assert_eq!(one.stdout, format!("0.0.0.0:{n}\n[::]:{n}\n"), "{one}");
    let none = shards(&["port", "web", "7002/tcp"]);
    assert_eq!(
        (none.status, none.stderr.as_str()),
        (Some(1), "no public port '7002/tcp' published for web\n"),
        "{none}"
    );
    let ps = shards(&["ps"]);
    assert!(
        ps.stdout.contains(&format!(
            "0.0.0.0:{n}->7000/tcp, [::]:{n}->7000/tcp, 127.0.0.1:{m}->7001/tcp"
        )),
        "{ps}"
    );
    // What the guest says first, and what it sent back of `payload`.
    let exchange = |at: SocketAddr, payload: Vec<u8>| -> (String, Vec<u8>) {
        let mut c = TcpStream::connect_timeout(&at, TIMEOUT).unwrap();
        c.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut writer = c.try_clone().unwrap();
        let writing = std::thread::spawn(move || {
            writer.write_all(&payload).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        writing.join().unwrap();
        let at = got.iter().position(|&b| b == b'\n').unwrap();
        let rest = got.split_off(at + 1);
        (String::from_utf8(got).unwrap(), rest)
    };
    // Eight MiB each way at once, more than either side's windows hold, ten times: a
    // wake-up lost between the VM and its network process stalls one (the ring's `arm`).
    let big: Vec<u8> = (0u32..8 << 20).map(|i| (i.wrapping_mul(7) % 251) as u8).collect();
    for _ in 0..10 {
        let (from, echoed) = exchange(SocketAddr::from(([127, 0, 0, 1], n)), big.clone());
        assert_eq!(from, "from 172.17.0.1\n");
        assert!(echoed == big, "{} bytes came back of {}", echoed.len(), big.len());
    }
    let (from, echoed) = exchange(format!("[::1]:{n}").parse().unwrap(), b"over IPv6".to_vec());
    assert_eq!(
        (from.as_str(), echoed.as_slice()),
        ("from 172.17.0.1\n", &b"over IPv6"[..])
    );
    // Taken by a running container: dockerd's allocator's words, the container left
    // created.
    let help = "\n\nRun 'shards run --help' for more information\n";
    let late = run_in(
        &home,
        &image,
        &["--name", "late", "-p", &format!("{n}:7000")],
        &["exit", "0"],
    );
    let said = late.stderr.strip_suffix(help).unwrap_or_default();
    let (before, after) = said.split_once(" (").unwrap_or_default();
    assert_eq!(
        (late.status, before),
        (
            Some(125),
            "shards: Error response from daemon: failed to set up container networking: driver failed programming external connectivity on endpoint late"
        ),
        "{late}"
    );
    assert!(
        after.ends_with(&format!(
            "): Bind for 0.0.0.0:{n} failed: port is already allocated"
        )),
        "{late}"
    );
    let all = shards(&["ps", "-a"]);
    assert!(
        all.stdout
            .lines()
            .any(|row| row.contains(" Created ") && row.ends_with(" late")),
        "{all}"
    );
    // Taken by another program: its bind's words.
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let h = held.local_addr().unwrap().port();
    let refused = run_in(
        &home,
        &image,
        &["--rm", "-p", &format!("{h}:7000")],
        &["exit", "0"],
    );
    assert_eq!(refused.status, Some(125), "{refused}");
    assert!(
        refused.stderr.contains(&format!(
            "): failed to bind host port 0.0.0.0:{h}/tcp: address already in use{help}"
        )),
        "{refused}"
    );
    drop(held);
    let (from, echoed) = exchange(SocketAddr::from(([127, 0, 0, 1], n)), b"last".to_vec());
    assert_eq!(
        (from.as_str(), echoed.as_slice()),
        ("from 172.17.0.1\n", &b"last"[..])
    );
    assert_eq!(exit(&mut run), Some(0));
    // Ended, it lists none, and its port is free at once.
    let gone = shards(&["port", "web"]);
    assert_eq!((gone.status, gone.stdout.as_str()), (Some(0), ""), "{gone}");
    // Each run's port is free as soon as `run` returns, as `docker run`'s is: the next
    // program to bind it has it, every time.
    for _ in 0..20 {
        let again = run_in(
            &home,
            &image,
            &["--rm", "-p", &format!("{n}:7000")],
            &["exit", "0"],
        );
        assert_eq!(again.status, Some(0), "{again}");
        drop(std::net::TcpListener::bind(("0.0.0.0", n)).unwrap());
    }
    // And the daemon holds it no longer: taken by another program, it is refused at once,
    // not after the wait for a run's ports to come free.
    let held = std::net::TcpListener::bind(("0.0.0.0", n)).unwrap();
    let began = Instant::now();
    let refused = run_in(
        &home,
        &image,
        &["--rm", "-p", &format!("{n}:7000")],
        &["exit", "0"],
    );
    assert!(
        refused.stderr.contains(&format!(
            "): failed to bind host port 0.0.0.0:{n}/tcp: address already in use"
        )),
        "{refused}"
    );
    assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
    drop(held);
}

/// `-p PORT/udp`: datagrams reach the guest as dockerd's proxy carries them: each host
/// peer from a gateway port of its own, its answers back to that peer from the address
/// it asked; `port` and `ps` list the port as UDP.
#[test]
fn published_udp_ports_carry_datagrams_both_ways() {
    use std::net::UdpSocket;
    let Some((home, image)) = home("containers-publish-udp") else {
        return;
    };
    let mut run = start(
        &home,
        &image,
        &["--name", "dns", "-p", "5353/udp"],
        &["udp-echo", "5353", "5"],
    );
    let listed = shards_in(&home, &["port", "dns", "5353/udp"]);
    let n: u16 = listed
        .stdout
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap();
    assert_eq!(listed.stdout, format!("0.0.0.0:{n}\n[::]:{n}\n"), "{listed}");
    let ps = shards_in(&home, &["ps"]);
    assert!(
        ps.stdout
            .contains(&format!("0.0.0.0:{n}->5353/udp, [::]:{n}->5353/udp")),
        "{ps}"
    );
    // What the guest answers `client`'s `payload` with, and where the answer came from.
    let ask = |client: &UdpSocket, to: std::net::SocketAddr, payload: &[u8]| {
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        client.send_to(payload, to).unwrap();
        let mut buf = [0u8; 2048];
        let (len, from) = client.recv_from(&mut buf).unwrap();
        (String::from_utf8_lossy(&buf[..len]).into_owned(), from)
    };
    let v4 = std::net::SocketAddr::from(([127, 0, 0, 1], n));
    let (a, b) = (
        UdpSocket::bind("127.0.0.1:0").unwrap(),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
    );
    let (first, from) = ask(&a, v4, b"one");
    assert_eq!(from, v4);
    let (again, _) = ask(&a, v4, b"one");
    let (second, _) = ask(&b, v4, b"two");
    // Each peer comes from the gateway, on a port of its own.
    let gateway_port = |answer: &str, payload: &str| -> u16 {
        let rest = answer
            .strip_prefix("from 172.17.0.1:")
            .unwrap_or_else(|| panic!("{answer}"));
        let (port, said) = rest.split_once(' ').unwrap();
        assert_eq!(said, payload);
        port.parse().unwrap()
    };
    // Each peer comes from the gateway, on a port of its own, the same for each of its
    // datagrams.
    assert_eq!(gateway_port(&first, "one"), gateway_port(&again, "one"));
    assert_ne!(gateway_port(&first, "one"), gateway_port(&second, "two"));
    // Asked at the host's own address from its loopback, the answer comes from the
    // address asked, not the one the route back would pick.
    let host = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
        .and_then(|s| s.local_addr());
    match host {
        Ok(host) => {
            let at = std::net::SocketAddr::new(host.ip(), n);
            let (answer, from) = ask(&a, at, b"one");
            assert_eq!(from, at);
            gateway_port(&answer, "one");
        }
        Err(_) => {
            eprintln!("SKIP: this host has no address but its loopback");
            ask(&a, v4, b"one");
        }
    }
    let v6: std::net::SocketAddr = format!("[::1]:{n}").parse().unwrap();
    let c = UdpSocket::bind("[::1]:0").unwrap();
    let (third, from) = ask(&c, v6, b"three");
    assert_eq!(from, v6);
    gateway_port(&third, "three");
    assert_eq!(exit(&mut run), Some(0));
    // Ended, its port is free at once.
    drop(UdpSocket::bind(("0.0.0.0", n)).unwrap());
}

/// Bytes from a size as go-units writes it (`13.4MB`), and whether `bytes` would be
/// written so: within the rounding of three significant digits.
fn size_shows(shown: &str, bytes: u64) -> bool {
    let split = shown.find(|c: char| c.is_ascii_alphabetic()).unwrap();
    let (n, unit) = shown.split_at(split);
    let scale = match unit {
        "B" => 1.0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => panic!("{shown}"),
    };
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    (n.parse::<f64>().unwrap() * scale - bytes).abs() <= bytes * 0.005
}

/// `shards images` as `docker images` lists images (Docker 29, its containerd store): an
/// image by what its reference resolved to, an index here, its sizes those of what is
/// here of it, its platforms with `--tree`, in use while a container of it is; the table
/// with `-q`, `--no-trunc` and `--digests`; and only what a pattern matches when given.
#[test]
fn images_lists_what_was_pulled_as_docker_images_does() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (port, _) = registry(index.clone(), blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = TempDir::new("containers-images");
    let shards = |args: &[&str]| shards_in(&home, args);
    let pulled = shards(&["pull", &image]);
    let id = sha256_digest(&index);
    assert!(pulled.stdout.contains(&format!("Digest: {id}")), "{pulled}");
    let short = id.strip_prefix("sha256:").and_then(|hex| hex.get(..12)).unwrap();
    // What is here of it: its index, our manifest, config and layer; and its root
    // filesystem.
    let dir_bytes = |dir: &Path| -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum()
    };
    let content = dir_bytes(&home.join("images/blobs/sha256"));
    let rootfs: u64 = std::fs::read_dir(home.join("images/rootfs"))
        .unwrap()
        .map(|v| dir_bytes(&v.unwrap().path()))
        .sum();
    // A pull keeps our platform's attestation, as dockerd does, and collections keep it:
    // the index, our manifest, config and layer, the attestation's manifest, config and
    // statement.
    let blobs = home.join("images/blobs/sha256");
    let count = || std::fs::read_dir(&blobs).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count(), 7);
    let listed = shards(&["images"]);
    assert_eq!(listed.status, Some(0), "{listed}");
    let lines: Vec<&str> = listed.stdout.lines().collect();
    let w = image.len();
    assert_eq!(
        lines[0],
        format!(
            "{:<w$}   ID             DISK USAGE   CONTENT SIZE   EXTRA",
            "IMAGE"
        ),
        "{listed}"
    );
    let row: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(&row[..2], &[image.as_str(), short], "{listed}");
    assert!(
        size_shows(row[2], content + rootfs),
        "{} of {}",
        row[2],
        content + rootfs
    );
    assert!(size_shows(row[3], content), "{} of {content}", row[3]);
    assert_eq!(lines.len(), 2, "{listed}");
    // Its platforms: ours, here; the other, not fetched; the attestation, not listed.
    let (ours, other) = if cfg!(target_arch = "aarch64") {
        ("arm64", "amd64")
    } else {
        ("amd64", "arm64")
    };
    let tree = shards(&["images", "--tree"]);
    assert!(tree.stdout.contains(&format!("├─ linux/{ours} ")), "{tree}");
    assert!(tree.stdout.contains(&format!("└─ linux/{other} ")), "{tree}");
    assert!(!tree.stdout.contains("unknown"), "{tree}");
    // The table.
    assert_eq!(shards(&["images", "-q"]).stdout, format!("{short}\n"));
    let full = shards(&["images", "--no-trunc"]);
    assert!(full.stdout.starts_with("REPOSITORY "), "{full}");
    assert!(
        full.stdout.contains(" v1 ") && full.stdout.contains(&format!(" {id} ")),
        "{full}"
    );
    let digests = shards(&["images", "--digests"]);
    let repo = format!("127.0.0.1:{port}/test/image");
    assert!(
        digests.stdout.lines().nth(1).unwrap().starts_with(&repo),
        "{digests}"
    );
    assert!(digests.stdout.contains(&format!(" {id} ")), "{digests}");
    // In use while a container of it is.
    let ran = run_in(&home, &image, &["-d", "--name", "user"], &["sleep"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let used = shards(&["images"]);
    assert!(used.stdout.lines().nth(1).unwrap().ends_with(" U    "), "{used}");
    let tree = shards(&["images", "--tree"]);
    let mine = tree
        .stdout
        .lines()
        .find(|l| l.contains(&format!("linux/{ours}")))
        .unwrap();
    assert!(mine.ends_with(" U    "), "{tree}");
    assert_eq!(shards(&["rm", "-f", "user"]).status, Some(0));
    // The daemon has had its collection, due since the pull, by now: still all seven.
    assert_eq!(count(), 7);
    // A pattern, matched as Go's path.Match matches the familiar name, with its tag or
    // without, and never the whole one (distribution's FamiliarMatch).
    let none = shards(&["images", "nothing"]);
    assert_eq!(none.stdout.lines().count(), 1, "{none}");
    let some = shards(&["image", "ls", "127.0.0.1:*/test/*"]);
    assert_eq!(some.stdout, listed.stdout, "{some}");
    // `*` stops at `/`: no name of it is one segment.
    assert_eq!(shards(&["image", "list", "*:v1"]).stdout.lines().count(), 1);
    // On Docker Hub the familiar name is the short one (measured, Docker 29.3.1).
    assert_eq!(shards(&["tag", &image, "hubbish:1"]).status, Some(0));
    for (pattern, rows) in [
        ("hubbish", 2),
        ("hubbish:1", 2),
        ("hub*", 2),
        ("docker.io/library/hubbish", 1),
        ("docker.io/library/hubbish:1", 1),
        ("library/hubbish", 1),
    ] {
        let listed = shards(&["images", pattern]);
        assert_eq!(listed.stdout.lines().count(), rows, "{pattern}: {listed}");
    }
    assert_eq!(shards(&["rmi", "hubbish:1"]).status, Some(0));
    // Newest first: an image built from it now, before it, which records no time.
    let ctx = TempDir::new("containers-images-ctx");
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {image}\nLABEL built=yes\n")).unwrap();
    let built = shards(&["build", "-q", "-t", "built:1", ctx.to_str().unwrap()]);
    assert_eq!(built.status, Some(0), "{built}");
    let ids: Vec<String> = shards(&["images", "-q"])
        .stdout
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_eq!(ids[1], short, "{ids:?}");
}

/// `shards tag SOURCE TARGET` as `docker tag` names an image again: by name or by a prefix
/// of its ID, TARGET with `latest` if it names no tag; refused in the Docker client's and
/// dockerd's words where it cannot.
/// A name moved to another image leaves the image it named, when that was its last name,
/// as dockerd leaves it: listed by `images -a` as `<untagged>`, and found by its ID
/// (measured, Docker 29.3.1).
#[test]
fn an_image_whose_last_name_moves_stays_dangling() {
    let Some((home, image)) = home("containers-dangling") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let variant = common::served_variant(b"other");
    let pulled = shards(&["pull", "-q", &variant]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let id = shards(&["images", "-q", &variant]).stdout.trim().to_string();
    assert_eq!(shards(&["tag", &variant, "solo:1"]).status, Some(0));
    assert_eq!(shards(&["rmi", &variant]).status, Some(0));
    let moved = shards(&["tag", &image, "solo:1"]);
    assert_eq!(moved.status, Some(0), "{moved}");
    let listed = shards(&["images", "-a"]);
    assert!(
        listed
            .stdout
            .lines()
            .any(|row| row.starts_with("<untagged>") && row.contains(&id)),
        "{listed}"
    );
    let inspected = shards(&["image", "inspect", &id]);
    assert_eq!(inspected.status, Some(0), "{inspected}");
    assert!(inspected.stdout.contains("\"RepoTags\": []"), "{inspected}");
}

#[test]
fn tag_names_an_image_again_as_docker_tag_does() {
    let Some((home, image)) = home("containers-tag") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let id = shards(&["images", "-q"]).stdout.trim().to_string();
    for (args, named) in [
        (&[image.as_str(), "other:1"][..], "other:1"),
        (&[id.as_str(), "byid"], "byid:latest"),
        (
            &["other:1", "localhost:5000/team/app:v2"],
            "localhost:5000/team/app:v2",
        ),
    ] {
        let tagged = shards(&[&["tag"][..], args].concat());
        assert_eq!(
            (tagged.status, tagged.stdout.as_str(), tagged.stderr.as_str()),
            (Some(0), "", ""),
            "{args:?}"
        );
        let listed = shards(&["images", named]);
        let row = listed.stdout.lines().nth(1).unwrap_or_default();
        assert!(row.starts_with(named) && row.contains(&id), "{listed}");
    }
    let ran = run_in(&home, "other:1", &["--rm"], &["exit", "3"]);
    assert_eq!(ran.status, Some(3), "{ran}");
    let digest = format!("x@sha256:{}", "0".repeat(64));
    for (args, said) in [
        (
            &["nosuch:1", "y"][..],
            "Error response from daemon: No such image: nosuch:1",
        ),
        (
            &["dead", "y"],
            "Error response from daemon: No such image: dead:latest",
        ),
        (
            &[image.as_str(), "Bad:1"],
            "error parsing reference: \"Bad:1\" is not a valid repository/tag: invalid reference format: repository name (library/Bad) must be lowercase",
        ),
        (
            &[image.as_str(), &digest],
            "refusing to create a tag with a digest reference",
        ),
        (
            &[image.as_str(), "sha256"],
            "Error response from daemon: refusing to create an ambiguous tag using digest algorithm as name",
        ),
    ] {
        let refused = shards(&[&["tag"][..], args].concat());
        assert_eq!(
            (refused.status, refused.stderr.as_str()),
            (Some(1), format!("{said}\n").as_str()),
            "{args:?}"
        );
    }
}

/// `shards rmi` as `docker rmi` removes images (dockerd 29.3.1's words and lines): a name
/// of several untags; the last also deletes; one a container uses must be forced, and
/// forced, the image stays for its container, dangling, listed only with `images -a`; a
/// running container's image cannot be removed by ID at all; with `-f`, an image not
/// found is said and forgiven.
#[test]
fn rmi_removes_images_as_docker_rmi_does() {
    let Some((home, image)) = home("containers-rmi") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    let full = shards(&["images", "-q", "--no-trunc"]).stdout.trim().to_string();
    let id = full
        .strip_prefix("sha256:")
        .and_then(|h| h.get(..12))
        .unwrap()
        .to_string();
    assert_eq!(shards(&["tag", &image, "shp/a:1"]).status, Some(0));
    let untagged = shards(&["rmi", "shp/a:1", "shp/nosuch:1"]);
    assert_eq!(
        (
            untagged.status,
            untagged.stdout.as_str(),
            untagged.stderr.as_str()
        ),
        (
            Some(1),
            "Untagged: shp/a:1\n",
            "Error response from daemon: No such image: shp/nosuch:1\n"
        ),
        "{untagged}"
    );
    let forgiven = shards(&["rmi", "-f", "shp/nosuch:1"]);
    assert_eq!(
        (forgiven.status, forgiven.stdout.as_str()),
        (Some(0), ""),
        "{forgiven}"
    );
    // In use by a running container.
    let ran = run_in(&home, &image, &["-d", "--name", "user"], &["sleep"]);
    assert_eq!(ran.status, Some(0), "{ran}");
    let container = ran.stdout.trim().get(..12).unwrap().to_string();
    let used = shards(&["rmi", &image]);
    assert_eq!(
        (used.status, used.stderr.clone()),
        (
            Some(1),
            format!(
                "Error response from daemon: conflict: unable to delete {image} (must be forced) - container {container} is using its referenced image {id}\n"
            )
        ),
        "{used}"
    );
    let hard = shards(&["rmi", "-f", &id]);
    assert_eq!(
        hard.stderr,
        format!(
            "Error response from daemon: conflict: unable to delete {id} (cannot be forced) - image is being used by running container {container}\n"
        ),
        "{hard}"
    );
    let soft = shards(&["rmi", "-f", &image]);
    assert_eq!(
        (soft.status, soft.stdout.clone()),
        (Some(0), format!("Untagged: {image}\n")),
        "{soft}"
    );
    // Dangling now: listed with -a alone, and still its container's.
    assert_eq!(shards(&["images", "-q"]).stdout, "");
    let all = shards(&["images", "-a"]);
    let row = all.stdout.lines().nth(1).unwrap_or_default().to_string();
    assert!(
        row.starts_with("<untagged>") && row.contains(&id) && row.ends_with(" U    "),
        "{all}"
    );
    assert_eq!(shards(&["stop", "-t", "0", "user"]).status, Some(0));
    let stopped = shards(&["rmi", &id]);
    assert!(
        stopped.stderr.contains(&format!(
            "(must be forced) - image is being used by stopped container {container}"
        )),
        "{stopped}"
    );
    assert_eq!(shards(&["rm", "user"]).status, Some(0));
    let deleted = shards(&["rmi", &id]);
    assert_eq!(
        (deleted.status, deleted.stdout.clone()),
        (Some(0), format!("Deleted: {full}\n")),
        "{deleted}"
    );
    assert_eq!(shards(&["images", "-aq"]).stdout, "");
    // Its content goes with the daemon's next collection.
    let blobs = home.join("images/blobs/sha256");
    eventually("the image's blobs collected", || {
        std::fs::read_dir(&blobs).map(|d| d.count()).unwrap_or(0) == 0
    });
}

/// `shards image inspect` as dockerd's containerd store answers `docker image inspect`
/// (byte for byte against real images: scripts/images/compare-inspect): each image's
/// document in the API's order, its config as Go encodes it, its descriptor what its
/// reference resolved to, its pull's repository; one array for all, and what was not found
/// said after.
#[test]
fn image_inspect_describes_images_as_docker_does() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    // What dockerd counts: the index, and our manifest, config and layer (the blobs'
    // first three); not the attestation, never fetched, nor the root filesystem.
    let size = index.len() + blobs[..3].iter().map(Vec::len).sum::<usize>();
    let (port, _) = registry(index.clone(), blobs);
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = TempDir::new("containers-inspect");
    let shards = |args: &[&str]| shards_in(&home, args);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let tagged = shards(&["tag", &image, "other:1"]);
    assert_eq!(tagged.status, Some(0), "{tagged}");
    let id = sha256_digest(&index);
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let repo = format!("127.0.0.1:{port}/test/image");
    let shown = shards(&["image", "inspect", &image, "nosuch:1"]);
    assert_eq!(shown.status, Some(1), "{shown}");
    assert_eq!(
        shown.stderr,
        "Error response from daemon: No such image: nosuch:1\n"
    );
    let doc: serde_json::Value = serde_json::from_str(&shown.stdout).unwrap();
    let doc = &doc[0];
    assert_eq!(doc["Id"], id.as_str());
    assert_eq!(doc["RepoTags"], serde_json::json!([image, "other:1"]));
    assert_eq!(
        doc["RepoDigests"],
        serde_json::json!([format!("{repo}@{id}"), format!("other@{id}")])
    );
    assert_eq!(
        doc["Config"],
        serde_json::json!({
            "User": "app",
            "Env": ["FROM_IMAGE=yes", "PATH=/bin"],
            "Entrypoint": ["/bin/testguest"],
            "Cmd": ["report"],
            "WorkingDir": "/work"
        })
    );
    assert_eq!(
        (doc["Architecture"].as_str(), doc["Os"].as_str()),
        (Some(arch), Some("linux"))
    );
    assert_eq!(doc["Size"], size);
    assert_eq!(doc["RootFS"]["Type"], "layers");
    assert_eq!(doc["RootFS"]["Layers"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        doc["Descriptor"],
        serde_json::json!({"mediaType": "application/vnd.oci.image.index.v1+json", "digest": id, "size": index.len()})
    );
    assert_eq!(
        doc["Identity"],
        serde_json::json!({"Pull": [{"Repository": repo}]})
    );
    // The API's order, and Go's layout: four spaces, `: `.
    let keys: Vec<&str> = shown
        .stdout
        .lines()
        .filter(|l| l.starts_with("        \"") && l.contains("\": "))
        .map(|l| l.trim().split('"').nth(1).unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "Id",
            "RepoTags",
            "RepoDigests",
            "Config",
            "Architecture",
            "Os",
            "Size",
            "RootFS",
            "Metadata",
            "Descriptor",
            "Identity"
        ]
    );
    let none = shards(&["image", "inspect", "nosuch:1"]);
    assert_eq!((none.status, none.stdout.as_str()), (Some(1), "[]\n"));
}

/// The records of a USTAR archive: each name, mode and content, in order.
fn ustar(bytes: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let mut records = Vec::new();
    let mut at = 0;
    while at + 512 <= bytes.len() && bytes[at] != 0 {
        let h = &bytes[at..at + 512];
        let text = |r: std::ops::Range<usize>| {
            String::from_utf8(h[r].iter().copied().take_while(|&b| b != 0).collect()).unwrap()
        };
        let size = usize::from_str_radix(text(124..135).trim(), 8).unwrap();
        let mode = u32::from_str_radix(text(100..107).trim(), 8).unwrap();
        records.push((text(0..100), mode, bytes[at + 512..at + 512 + size].to_vec()));
        at += 512 + size.div_ceil(512) * 512;
    }
    assert_eq!(&bytes[at..], &[0u8; 1024][..], "two zero blocks end it");
    records
}

/// `shards save` as `docker save` writes images (byte for byte against dockerd 29.3.1:
/// scripts/images/compare-save): an OCI layout with what is here of each image, its
/// attestation included, Docker's manifest.json beside it; `-o` written whole, mode 0600,
/// or not at all; an image not found refused before anything is written.
#[test]
fn save_writes_images_as_docker_save_does() {
    use std::os::unix::fs::PermissionsExt as _;
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (port, _) = registry(index.clone(), blobs.clone());
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let home = TempDir::new("containers-save");
    let shards = |args: &[&str]| shards_in(&home, args);
    let pulled = shards(&["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let out = TempDir::new("containers-save-out");
    let tar = out.join("image.tar");
    let saved = shards(&["save", "-o", tar.to_str().unwrap(), &image]);
    assert_eq!(
        (saved.status, saved.stdout.as_str(), saved.stderr.as_str()),
        (Some(0), "", ""),
        "{saved}"
    );
    assert_eq!(
        std::fs::metadata(&tar).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let records = ustar(&std::fs::read(&tar).unwrap());
    let id = sha256_digest(&index);
    // The index, our manifest, config and layer, and our attestation's three: not the
    // other platform's attestation, never pulled.
    let mut want: Vec<String> = std::iter::once(&index)
        .chain(&blobs[..6])
        .map(|b| format!("blobs/sha256/{}", sha256_digest(b).trim_start_matches("sha256:")))
        .collect();
    want.sort();
    let names: Vec<&str> = records.iter().map(|(n, _, _)| n.as_str()).collect();
    let mut expected: Vec<&str> = vec!["blobs/", "blobs/sha256/"];
    expected.extend(want.iter().map(String::as_str));
    expected.extend(["index.json", "manifest.json", "oci-layout"]);
    assert_eq!(names, expected);
    for (name, mode, content) in &records {
        if let Some(hex) = name.strip_prefix("blobs/sha256/").filter(|h| !h.is_empty()) {
            assert_eq!(sha256_digest(content), format!("sha256:{hex}"), "{name}");
            assert_eq!(*mode, 0o444, "{name}");
        }
    }
    let file = |n: &str| records.iter().find(|(name, _, _)| name == n).unwrap().2.clone();
    let repo = format!("127.0.0.1:{port}/test/image");
    assert_eq!(
        String::from_utf8(file("index.json")).unwrap(),
        format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.index.v1+json","digest":"{id}","size":{},"annotations":{{"containerd.io/distribution.source.127.0.0.1:{port}":"test/image","io.containerd.image.name":"{image}","org.opencontainers.image.ref.name":"v1"}}}}]}}"#,
            index.len()
        )
    );
    let blob = |b: &Vec<u8>| format!("blobs/sha256/{}", sha256_digest(b).trim_start_matches("sha256:"));
    assert_eq!(
        String::from_utf8(file("manifest.json")).unwrap(),
        format!(
            r#"[{{"Config":"{}","RepoTags":["{repo}:v1"],"Layers":["{}"]}}]"#,
            blob(&blobs[0]),
            blob(&blobs[1])
        )
    );
    assert_eq!(file("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#);
    // Not found: refused, and no file made, nor any left beside it.
    let missing = out.join("missing.tar");
    let refused = shards(&["save", "-o", missing.to_str().unwrap(), &image, "nosuch:1"]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (Some(1), "Error response from daemon: No such image: nosuch:1\n"),
        "{refused}"
    );
    assert!(!missing.exists());
    assert_eq!(std::fs::read_dir(&*out).unwrap().count(), 1, "only image.tar");
    let bad = shards(&["save", "-o", "/nonexistent-dir/x.tar", &image]);
    assert_eq!(
        (bad.status, bad.stderr.as_str()),
        (
            Some(1),
            "failed to save image: invalid output path: stat /nonexistent-dir: no such file or directory\n"
        )
    );
    let dir = shards(&["save", "-o", out.to_str().unwrap(), &image]);
    assert_eq!(dir.stderr, "failed to save image: cannot write to a directory\n");
    // A blob that cannot be read past the archive's start: the save fails, and its file
    // is neither made nor left half written beside.
    let layer = home
        .join("images/blobs/sha256")
        .join(sha256_digest(&blobs[1]).trim_start_matches("sha256:"));
    std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o000)).unwrap();
    let broken = out.join("broken.tar");
    let failed = shards(&["save", "-o", broken.to_str().unwrap(), &image]);
    std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert_eq!(failed.status, Some(1), "{failed}");
    assert!(
        failed.stderr.starts_with("Error response from daemon: "),
        "{failed}"
    );
    assert!(!broken.exists());
    assert_eq!(std::fs::read_dir(&*out).unwrap().count(), 1, "only image.tar");
    // By ID: no name, so neither name annotation nor tag.
    let by_id = out.join("by-id.tar");
    let short = id.strip_prefix("sha256:").and_then(|h| h.get(..12)).unwrap();
    assert_eq!(
        shards(&["save", "-o", by_id.to_str().unwrap(), short]).status,
        Some(0)
    );
    let records = ustar(&std::fs::read(&by_id).unwrap());
    let file = |n: &str| {
        String::from_utf8(records.iter().find(|(name, _, _)| name == n).unwrap().2.clone()).unwrap()
    };
    assert!(
        file("index.json").ends_with(&format!(
            r#""annotations":{{"containerd.io/distribution.source.127.0.0.1:{port}":"test/image"}}}}]}}"#
        )),
        "{}",
        file("index.json")
    );
    assert!(
        file("manifest.json").contains(r#""RepoTags":null"#),
        "{}",
        file("manifest.json")
    );
    std::fs::remove_file(&by_id).unwrap();
    // A repository named alone is every tag of it, each by its name: one manifest for
    // one image, both tags on it, and an index entry for each name (measured, dockerd
    // 29.3.1); a repository with none is refused for its latest.
    for tag in ["savetest:b", "savetest:a"] {
        assert_eq!(shards(&["tag", &image, tag]).status, Some(0));
    }
    let repo_tar = out.join("repo.tar");
    let saved = shards(&["save", "-o", repo_tar.to_str().unwrap(), "savetest"]);
    assert_eq!(saved.status, Some(0), "{saved}");
    let records = ustar(&std::fs::read(&repo_tar).unwrap());
    let file = |n: &str| {
        String::from_utf8(records.iter().find(|(name, _, _)| name == n).unwrap().2.clone()).unwrap()
    };
    assert!(
        file("manifest.json").contains(r#""RepoTags":["savetest:a","savetest:b"]"#),
        "{}",
        file("manifest.json")
    );
    let index = file("index.json");
    let a = index.find(r#""io.containerd.image.name":"docker.io/library/savetest:a""#);
    let b = index.find(r#""io.containerd.image.name":"docker.io/library/savetest:b""#);
    assert!(a.is_some() && b.is_some() && a < b, "{index}");
    assert!(!index.contains(":latest"), "{index}");
    std::fs::remove_file(&repo_tar).unwrap();
    let none = shards(&["save", "-o", repo_tar.to_str().unwrap(), "nosuchrepo"]);
    assert_eq!(
        (none.status, none.stderr.as_str()),
        (
            Some(1),
            "Error response from daemon: No such image: nosuchrepo:latest\n"
        ),
        "{none}"
    );
    // To stdout, the same archive.
    let piped = Command::new(common::shards())
        .args(["save", &image])
        .env("SHARDS_HOME", &*home)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(piped.stdout, std::fs::read(&tar).unwrap());
}

/// `shards load` as `docker load` reads an archive (dockerd 29.3.1's words and records:
/// Docker's own archives load to images that inspect byte for byte as dockerd's do, by
/// hand): what `save` wrote, into another home, where it runs; gzip-compressed, from
/// stdin; Docker's older layout, manifest.json and `<id>/layer.tar`; an image with no
/// name by its ID; and a broken archive refused in Go's words.
#[test]
fn load_reads_archives_as_docker_load_does() {
    use std::io::Write as _;
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (port, _) = registry(index.clone(), blobs.clone());
    let image = format!("127.0.0.1:{port}/test/image:v1");
    let first = TempDir::new("containers-load-from");
    let saved_tar = first.join("image.tar");
    assert_eq!(shards_in(&first, &["pull", "-q", &image]).status, Some(0));
    assert_eq!(
        shards_in(&first, &["save", "-o", saved_tar.to_str().unwrap(), &image]).status,
        Some(0)
    );
    let home = TempDir::new("containers-load");
    let shards = |args: &[&str]| shards_in(&home, args);
    let loaded = shards(&["load", "-i", saved_tar.to_str().unwrap()]);
    assert_eq!(
        (loaded.status, loaded.stdout.as_str(), loaded.stderr.as_str()),
        (Some(0), format!("Loaded image: {image}\n").as_str(), ""),
        "{loaded}"
    );
    // Unpacked as it was loaded, as dockerd unpacks: its root filesystem is here.
    let rootfs = |home: &Path| -> usize {
        std::fs::read_dir(home.join("images/rootfs"))
            .into_iter()
            .flatten()
            .flat_map(|v| std::fs::read_dir(v.unwrap().path()).into_iter().flatten())
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "erofs")
            })
            .count()
    };
    assert_eq!(rootfs(&home), 1);
    // Its record describes it as the archive's index did, name and source with it.
    let shown = shards(&["image", "inspect", &image]);
    let doc: serde_json::Value = serde_json::from_str(&shown.stdout).unwrap();
    assert_eq!(doc[0]["Id"], sha256_digest(&index).as_str());
    assert_eq!(
        doc[0]["Descriptor"]["annotations"]["io.containerd.image.name"],
        image.as_str()
    );
    assert_eq!(
        doc[0]["Identity"]["Pull"][0]["Repository"],
        format!("127.0.0.1:{port}/test/image")
    );
    // Unpacked: it runs at once.
    let ran = run_in(&home, &image, &["--rm"], &["exit", "5"]);
    assert_eq!(ran.status, Some(5), "{ran}");
    // Gzip-compressed, on stdin, under another name.
    let mut gz = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let archive = std::fs::read(&saved_tar).unwrap();
    let mut feed = gz.stdin.take().unwrap();
    let feeding = std::thread::spawn(move || feed.write_all(&archive).unwrap());
    let gzipped = gz.wait_with_output().unwrap().stdout;
    feeding.join().unwrap();
    let again = TempDir::new("containers-load-gz");
    let mut load = Command::new(common::shards())
        .args(["load"])
        .env("SHARDS_HOME", &*again)
        .env("SHARDS_KERNEL", kernel())
        .env("SHARDS_INIT", guest_init())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    load.stdin.take().unwrap().write_all(&gzipped).unwrap();
    let out = load.wait_with_output().unwrap();
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned()
        ),
        (Some(0), format!("Loaded image: {image}\n"))
    );
    // Docker's older layout: a config and a layer.tar, named by manifest.json; an image
    // of its own, so its layer is unpacked here.
    let (_, own) = common::test_image_with(Some(b"legacy"));
    let (config, layer) = (&own[0], &own[1]);
    let legacy = common::tar(&[
        ("cfg.json", 0o644, 0, Some(config.as_slice())),
        ("abc", 0o755, 0, None),
        ("abc/layer.tar", 0o644, 0, Some(layer.as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["legacy:1"],"Layers":["abc/layer.tar"]}]"#),
        ),
    ]);
    let legacy_tar = home.join("legacy.tar");
    std::fs::write(&legacy_tar, legacy).unwrap();
    let old = shards(&["load", "-i", legacy_tar.to_str().unwrap()]);
    assert_eq!(
        (old.status, old.stdout.as_str()),
        (Some(0), "Loaded image: legacy:1\n"),
        "{old}"
    );
    let ran = run_in(&home, "legacy:1", &["--rm"], &["exit", "6"]);
    assert_eq!(ran.status, Some(6), "{ran}");
    // A layer the archive holds once, another directory's layer.tar a symlink to it, as
    // Docker's older save writes a layer it has written already: `../abc/layer.tar`,
    // joined to its directory and cleaned, as containerd's importer resolves it.
    let mut linked = common::tar(&[
        ("cfg.json", 0o644, 0, Some(config.as_slice())),
        ("abc", 0o755, 0, None),
        ("abc/layer.tar", 0o644, 0, Some(layer.as_slice())),
        ("aaa", 0o755, 0, None),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["linked:1"],"Layers":["aaa/layer.tar"]}]"#),
        ),
    ]);
    linked.truncate(linked.len() - 1024);
    linked.extend(common::tar_symlink("aaa/layer.tar", "../abc/layer.tar"));
    linked.resize(linked.len() + 1024, 0);
    let linked_tar = home.join("linked.tar");
    std::fs::write(&linked_tar, linked).unwrap();
    let loaded = shards(&["load", "-i", linked_tar.to_str().unwrap()]);
    assert_eq!(
        (loaded.status, loaded.stdout.as_str()),
        (Some(0), "Loaded image: linked:1\n"),
        "{loaded}"
    );
    // An image for another platform is loaded and not unpacked, and nothing more is said
    // of it, as dockerd says nothing (measured, Docker 29.3.1, an amd64 archive on arm64);
    // a run of it is refused, as no guest here runs it.
    let (_, foreign) = common::test_image_with(Some(b"foreign"));
    let host_arch = format!(
        r#""architecture":"{}""#,
        if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "amd64"
        }
    );
    let foreign_config = String::from_utf8(foreign[0].clone())
        .unwrap()
        .replace(&host_arch, r#""architecture":"s390x""#)
        .into_bytes();
    assert_ne!(
        foreign_config, foreign[0],
        "the config named the host's architecture"
    );
    let other = common::tar(&[
        ("cfg.json", 0o644, 0, Some(foreign_config.as_slice())),
        ("fff", 0o755, 0, None),
        ("fff/layer.tar", 0o644, 0, Some(foreign[1].as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["foreign:1"],"Layers":["fff/layer.tar"]}]"#),
        ),
    ]);
    let other_tar = home.join("foreign.tar");
    std::fs::write(&other_tar, other).unwrap();
    let loaded = shards(&["load", "-i", other_tar.to_str().unwrap()]);
    assert_eq!(
        (loaded.status, loaded.stdout.as_str(), loaded.stderr.as_str()),
        (Some(0), "Loaded image: foreign:1\n", ""),
        "{loaded}"
    );
    let ran = run_in(&home, "foreign:1", &["--rm"], &["exit", "0"]);
    assert_ne!(ran.status, Some(0), "{ran}");
    // Its manifest as containerd writes one for it, byte for byte: its ID is that.
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar","digest":"{}","size":{}}}]}}"#,
        sha256_digest(config),
        config.len(),
        sha256_digest(layer),
        layer.len()
    );
    let doc: serde_json::Value =
        serde_json::from_str(&shards(&["image", "inspect", "legacy:1"]).stdout).unwrap();
    assert_eq!(doc[0]["Id"], sha256_digest(manifest.as_bytes()).as_str());
    assert_eq!(
        doc[0]["Descriptor"]["mediaType"],
        "application/vnd.docker.distribution.manifest.v2+json"
    );
    assert!(doc[0].get("Identity").is_none(), "no pull, no identity");
    // A layer already here compressed, pulled as gzip: the legacy archive's copy of it,
    // uncompressed, is that gzip blob, as containerd finds it by its uncompressed digest.
    let (_, gz_blobs) = common::test_image_with(Some(b"gzipped"));
    let (gz_config, raw_layer) = (&gz_blobs[0], &gz_blobs[1]);
    let mut gzip = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut feed = gzip.stdin.take().unwrap();
    let raw = raw_layer.clone();
    let feeding = std::thread::spawn(move || feed.write_all(&raw).unwrap());
    let gz_layer = gzip.wait_with_output().unwrap().stdout;
    feeding.join().unwrap();
    let gz_manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":{}}}]}}"#,
        sha256_digest(gz_config),
        gz_config.len(),
        sha256_digest(&gz_layer),
        gz_layer.len()
    )
    .into_bytes();
    let (gz_port, _) = registry(gz_manifest, vec![gz_config.clone(), gz_layer.clone()]);
    let pulled = shards(&["pull", "-q", &format!("127.0.0.1:{gz_port}/test/image:v1")]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let reuse = common::tar(&[
        ("cfg.json", 0o644, 0, Some(gz_config.as_slice())),
        ("def", 0o755, 0, None),
        ("def/layer.tar", 0o644, 0, Some(raw_layer.as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["reused:1"],"Layers":["def/layer.tar"]}]"#),
        ),
    ]);
    let reuse_tar = home.join("reuse.tar");
    std::fs::write(&reuse_tar, reuse).unwrap();
    assert_eq!(
        shards(&["load", "-i", reuse_tar.to_str().unwrap()]).stdout,
        "Loaded image: reused:1\n"
    );
    let reused = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"{}","size":{}}}]}}"#,
        sha256_digest(gz_config),
        gz_config.len(),
        sha256_digest(&gz_layer),
        gz_layer.len()
    );
    let doc: serde_json::Value =
        serde_json::from_str(&shards(&["image", "inspect", "reused:1"]).stdout).unwrap();
    assert_eq!(doc[0]["Id"], sha256_digest(reused.as_bytes()).as_str());
    // Pulled with a Docker v2 manifest, its gzip layer is found as well.
    let (_, v2_blobs) = common::test_image_with(Some(b"docker-v2"));
    let (v2_config, v2_raw) = (&v2_blobs[0], &v2_blobs[1]);
    let mut gzip = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut feed = gzip.stdin.take().unwrap();
    let raw = v2_raw.clone();
    let feeding = std::thread::spawn(move || feed.write_all(&raw).unwrap());
    let v2_gz = gzip.wait_with_output().unwrap().stdout;
    feeding.join().unwrap();
    let v2_manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"{}","size":{}}}]}}"#,
        sha256_digest(v2_config),
        v2_config.len(),
        sha256_digest(&v2_gz),
        v2_gz.len()
    );
    let (v2_port, _) = registry(
        v2_manifest.clone().into_bytes(),
        vec![v2_config.clone(), v2_gz.clone()],
    );
    let pulled = shards(&["pull", "-q", &format!("127.0.0.1:{v2_port}/test/image:v1")]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let v2_reuse = common::tar(&[
        ("cfg.json", 0o644, 0, Some(v2_config.as_slice())),
        ("ghi", 0o755, 0, None),
        ("ghi/layer.tar", 0o644, 0, Some(v2_raw.as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["reused:2"],"Layers":["ghi/layer.tar"]}]"#),
        ),
    ]);
    let v2_tar = home.join("v2-reuse.tar");
    std::fs::write(&v2_tar, v2_reuse).unwrap();
    assert_eq!(
        shards(&["load", "-i", v2_tar.to_str().unwrap()]).stdout,
        "Loaded image: reused:2\n"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&shards(&["image", "inspect", "reused:2"]).stdout).unwrap();
    assert_eq!(doc[0]["Id"], sha256_digest(v2_manifest.as_bytes()).as_str());
    // Saved by ID: no name, so loaded by its ID.
    let by_id = home.join("by-id.tar");
    let short = sha256_digest(&index);
    let short = short.strip_prefix("sha256:").and_then(|h| h.get(..12)).unwrap();
    assert_eq!(
        shards_in(&first, &["save", "-o", by_id.to_str().unwrap(), short]).status,
        Some(0)
    );
    let unnamed = shards(&["load", "-i", by_id.to_str().unwrap()]);
    assert_eq!(
        unnamed.stdout,
        format!("Loaded image ID: {}\n", sha256_digest(&index)),
        "{unnamed}"
    );
    // Broken: refused in Go's tar reader's words.
    let broken = home.join("broken.tar");
    std::fs::write(&broken, b"garbage\n").unwrap();
    let refused = shards(&["load", "-i", broken.to_str().unwrap()]);
    assert_eq!(
        (refused.status, refused.stderr.as_str()),
        (Some(1), "unexpected EOF\n"),
        "{refused}"
    );
    let missing = shards(&["load", "-i", "/nonexistent.tar"]);
    assert_eq!(
        missing.stderr,
        "open /nonexistent.tar: no such file or directory\n"
    );
}

/// `shards push` as `docker push` uploads images (dockerd 29.3.1's lines; pushed to a
/// real registry:2 and pulled back by docker, by hand): our platform's manifest alone
/// when an index's other platforms are not here, with dockerd's note; a layer the
/// repository has, said so; every tag with `-a`; a blob mounted from another repository
/// of the registry the image came from; an image of one manifest as it is; and the
/// refusals.
#[test]
fn push_uploads_images_as_docker_push_does() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = test_index();
    let (from, _) = registry(index.clone(), blobs.clone());
    let source = format!("127.0.0.1:{from}/test/image:v1");
    let (port, repos) = common::writable_registry();
    let home = TempDir::new("containers-push");
    let shards = |args: &[&str]| shards_in(&home, args);
    assert_eq!(shards(&["pull", "-q", &source]).status, Some(0));
    let repo = format!("127.0.0.1:{port}/team/app");
    for tag in ["1", "2"] {
        assert_eq!(
            shards(&["tag", &source, &format!("{repo}:{tag}")]).status,
            Some(0)
        );
    }
    let (manifest, layer) = (&blobs[2], &blobs[1]);
    let short = sha256_digest(layer);
    let short = short.trim_start_matches("sha256:").get(..12).unwrap();
    let note = format!(
        "\n Info -> Not all multiplatform-content is present and only the available single-platform image was pushed\n         {} -> {}\n",
        sha256_digest(&index),
        sha256_digest(manifest)
    );
    let pushed = shards(&["push", &format!("{repo}:1")]);
    assert_eq!(
        (pushed.status, pushed.stdout.clone(), pushed.stderr.as_str()),
        (
            Some(0),
            format!(
                "The push refers to repository [{repo}]\n{short}: Pushed\n1: digest: {} size: {}\n{note}",
                sha256_digest(manifest),
                manifest.len()
            ),
            ""
        ),
        "{pushed}"
    );
    {
        let r = repos.lock().unwrap();
        assert_eq!(
            r.manifests["team/app"]["1"].1, *manifest,
            "the manifest, byte for byte"
        );
        let held: std::collections::BTreeSet<&String> = r.blobs["team/app"].keys().collect();
        let want: std::collections::BTreeSet<String> =
            [&blobs[0], &blobs[1]].iter().map(|b| sha256_digest(b)).collect();
        assert_eq!(
            held,
            want.iter().collect(),
            "its config and layer, not the attestation"
        );
    }
    let again = shards(&["push", &format!("{repo}:1")]);
    assert!(
        again.stdout.contains(&format!("{short}: Layer already exists\n")),
        "{again}"
    );
    // Every tag of the repository.
    let all = shards(&["push", "-a", &repo]);
    assert_eq!(all.status, Some(0), "{all}");
    assert!(
        all.stdout.contains("\n1: digest: ") && all.stdout.contains("\n2: digest: "),
        "{all}"
    );
    assert!(repos.lock().unwrap().manifests["team/app"].contains_key("2"));
    // Pulled from this registry, pushed to another repository of it: mounted.
    let other = TempDir::new("containers-push-mount");
    assert_eq!(
        shards_in(&other, &["pull", "-q", &format!("{repo}:1")]).status,
        Some(0)
    );
    let target = format!("127.0.0.1:{port}/other/app:1");
    assert_eq!(
        shards_in(&other, &["tag", &format!("{repo}:1"), &target]).status,
        Some(0)
    );
    let mounted = shards_in(&other, &["push", &target]);
    assert!(
        mounted
            .stdout
            .contains(&format!("{short}: Mounted from team/app\n")),
        "{mounted}"
    );
    assert!(
        repos
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l == "POST /v2/other/app/blobs/uploads/"),
        "a mount is asked with a POST"
    );
    // One manifest, as it is; quietly, the name alone.
    let quiet = shards_in(&other, &["push", "-q", &target]);
    assert_eq!(
        (quiet.status, quiet.stdout.as_str()),
        (Some(0), format!("{target}\n").as_str()),
        "{quiet}"
    );
    // Refusals.
    let missing = shards(&["push", &format!("{repo}:9")]);
    assert_eq!(
        (missing.status, missing.stderr.clone()),
        (Some(1), format!("tag does not exist: {repo}:9\n"))
    );
    let tagged = shards(&["push", "-a", &format!("{repo}:1")]);
    assert_eq!(tagged.stderr, "tag can't be used with --all-tags/-a\n");
}
