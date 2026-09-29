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

use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served, shards};

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
        assert_eq!(shards_in(&home, &["wait", name]).stdout, "127\n", "{name}");
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
            &["run", "-p", "80:80", "alpine"],
            1,
            "\"--publish\" is not supported by shards yet\n",
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
