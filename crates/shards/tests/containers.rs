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
    Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served, shards, shards_net, shards_vm,
    shardsd,
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
        &["serve", "7000", "3"],
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
    // Eight MiB each way at once: more than either side's windows hold.
    let big: Vec<u8> = (0u32..8 << 20).map(|i| (i.wrapping_mul(7) % 251) as u8).collect();
    let (from, echoed) = exchange(SocketAddr::from(([127, 0, 0, 1], n)), big.clone());
    assert_eq!(from, "from 172.17.0.1\n");
    assert!(echoed == big, "{} bytes came back of {}", echoed.len(), big.len());
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
